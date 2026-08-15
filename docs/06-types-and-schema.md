# 6. Types & schema

> [Aperture design book](../README.md) · [← 5. Suspend & resume](05-resume.md) · **Chapter 6** · [7. Compilation →](07-compilation.md)

Every fact is typed, and the types come from a **schema**. This chapter covers the type
model, how names are interned, unions and their frozen discriminants, and **schema
identity** — the canonical form and fingerprint that make a DB self-describing and let
compatibility be a set-containment check. Code: `crates/aperture-schema/src/schema.rs`.

The one-way-door invariants here ([I10](invariants.md#i10), [I13](invariants.md#i13)) join
the [codec's I3](02-tuple-codec.md): together they freeze the on-disk type world the moment
data exists.

---

## The type model

A predicate's shape is a `Predicate`:

```rust
struct Predicate {
    name: Spur,               // interned name
    key: PredicateTy,          // indexed, identifies the fact
    value: Option<PredicateTy>, // fetched on demand (chapter 3)
}
```

and a type is a `PredicateTy`:

```rust
enum PredicateTy {
    Int,                              // → integer markers (chapter 2)
    Str,                              // → MARK_STRING
    Fact(PredicateId),                // a reference to a fact → MARK_FACT_REF
    Record(Arc<[(Spur, PredicateTy)]>), // → MARK_RECORD … MARK_TERM
    // Union { … }  — designed-for, not yet present (see below)
}
```

Each variant maps to a codec marker family from [chapter 2](02-tuple-codec.md), which is
why the type system and the codec must agree: a `Fact`-typed field encodes with
`MARK_FACT_REF` and carries a `PredicateId` naming *which* predicate it points at.

**Four constructors, and that is the whole model** — no arrays, sets, unions, or a byte/nat
distinction. The gap to Glean is smaller than Glean's surface syntax suggests. Glean's
*runtime* type model is **eight** constructors — byte, nat, string, array, tuple, sum, set,
predicate-ref (`glean/hs/Glean/RTS/Types.hs:185-194`) — and `bool`, `maybe`, `enum`, tuples
and named types are **sugar, lowered before storage**, so they cost its codec nothing.
Measured against that runtime model, Aperture is **three** constructors short (array, sum, and
the byte/nat pair), not eight behind a surface list — and one constructor **wider**: focus's
`Int` is a signed `i64` with its own negative marker band
([chapter 2](02-tuple-codec.md#the-marker-table)), where Glean has no signed integer at all.

The codec reserves marker bands for the missing three, so the room is physically there; what is
*not* settled is whether they are wanted. **`set T`** — an array's wire shape, sorted and
deduplicated so it is canonical — is separately deferrable: seven field declarations in Glean's
entire schema corpus use one, against several hundred arrays, and Glean treats array↔set as a
*compatible* change.

**The reserved band is not the whole cost of an array, though.** A length-prefixed array cannot
be prefix-matched — Glean says so outright, *"MatchArrayPrefix doesn't actually look at a prefix
because arrays encode their length at the front"*
(`glean/db/Glean/Query/Reorder.hs:794-796`) — so you can seek on a whole array and never into
one, and an array anywhere but the last key field **permanently closes the seek prefix** for
every field after it ([chapter 7](07-compilation.md#how-sargeability-actually-decides-phase-4-as-built)).
That is a one-way door in the encoding, not only in how schemas are written. The decision itself
— arrays, one fact per element, or both — is [open decisions](open-decisions.md); the full
comparison is [the ledger](glean-comparison.md).

### Records are sorted slices, everywhere

Record fields are a **sorted `[(Symbol, PredicateTy)]`** — in the schema, `Arc`-shared
(`Arc<[(Spur, PredicateTy)]>`) because schema types are frozen and shared across every
query. Never a `HashMap`. The reasons ([conventions](conventions.md)):

- **deterministic order** — the same record always encodes to the same bytes, which
  order-preservation ([I1](invariants.md#i1)) and schema fingerprinting both require;
- **one allocation**, and a linear scan beats hashing at the tiny arities records have.

This must hold in *all three* tree representations ([chapter 7](07-compilation.md)) and on
disk — it's a codec-level fact, not a convenience.

---

## Symbols and two-tier interning

Names are interned to `Spur`s (via `lasso`) so runtime code compares small integers, not
strings. Interning is **two-tier**, reflecting the two lifetimes names have:

```rust
enum Symbol { Schema(Spur), Local(Spur) }
```

- **`SchemaInterner`** — schema names (predicates, fields). Backed by a frozen
  `Arc<RodeoReader>`: read-only and lock-free, shared across every concurrent query. Schema
  names live as long as the DB.
- **`LocalInterner`** — query-local names (variables), a per-query `Rodeo`.

`get_or_intern` resolves **schema-first**: any name that exists in the schema canonicalises
to the same `Symbol::Schema`, so a query-local name **cannot shadow** a schema name, and
field resolution compares `Spur`s rather than strings. Resolve symbols to `&str`/`Arc<str>`
at plan-build time so the executor is interner-free ([conventions](conventions.md)).

---

## Unions and stable discriminants ([I10](invariants.md#i10))

Unions ("sum types" / tagged alternatives) are **designed-for but not yet in
`PredicateTy`**. They land with the schema DSL ([`PLAN.md`](../PLAN.md) Phase 8) rather than
with the rest of the deferred query surface (Phase 6b), because a union cannot be *declared*
until schemas are parsed — and the freeze below is a reason to want the declaration first, not
a reason to rush the type in. When they land, the load-bearing rule is:

> **I10 — union alternative discriminants are stable and append-only.** Like protobuf field
> numbers: each alternative has an explicit discriminant, assigned once, never reused, new
> alternatives appended. Frozen the moment union-typed data is written.
>
> *Guard:* `schema::discriminants_append_only` — a schema edit that renumbers or reuses a
> discriminant is rejected at load.

Why it's a one-way door: a union value is stored tagged by its discriminant. If
discriminants were derived from, say, sorted alternative names, adding an alternative would
**silently renumber** existing ones and misinterpret every stored union value. Explicit,
append-only tags are the only safe scheme **without a query-time transform layer** — and Glean
is the counterexample that earns the qualifier. Glean has **no discriminant syntax at all**: a
sum's tag is its *position* in the declared list (`glean/hs/Glean/RTS/Types.hs:119-120`;
the grammar offers nowhere to write one, `glean/angle/Glean/Angle/Parser.y:306-309`), encoded as
a selector nat then payload, so inserting an alternative mid-list **does** renumber. Glean buys
stability at read time instead: alternatives are matched **by name** and the selector rewritten,
with a synthetic `unknown` tag for one the target schema no longer has
(`glean/db/Glean/Query/Transform.hs:551-579`).

Aperture declines that layer — [I13](invariants.md#i13) freezes the schema at create, so there
is no second schema to project from and nowhere for a transform to live. **I10 is therefore a
consequence of that choice, not an idea inherited from Glean**: the protobuf-style explicit tag
is what remains once query-time projection is off the table, and given the freeze it is the
right call. Recorded as a divergence in [the ledger](glean-comparison.md). Get it right *before*
writing any union facts — after that it's an on-disk migration
([I3](02-tuple-codec.md) territory).

**What I10 does not yet say is what a decoder does with a tag it has never seen.** Append-only
tags do not make that impossible: a fact file can outlive the schema that wrote it, and a
retired alternative's tag is still on disk. Glean has a defined answer, the synthetic `unknown`;
Aperture has none, and per errors-not-panics ([conventions](conventions.md)) it must surface as
an `ApertureError` rather than a panic or a mis-decode. Decide it with the discriminant encoding
in Phase 8.

### How unions are used (mostly) needs no new machine

Selecting an alternative — `x.alt?` — lowers to a **match against the bound value**: a
`ResidualOp::DiscriminantEq(n)` check plus a payload bind. That's a residual and a field
bind, **not** a new generator or operator ([chapter 7](07-compilation.md),
[executor](04-executor.md)). Unions-as-data is genuinely additive; only the discriminant
freeze is a hard constraint.

---

## Schema identity

A DB must be **self-describing** and comparisons must be **filesystem-independent** — two
schemas are "the same" if they *mean* the same thing, regardless of which file declared a
predicate or in what order. This is achieved with a canonical form and a fingerprint.

### Canonical form → fingerprint

- **Canonical form:** resolve every name to its fully-qualified form; strip comments,
  whitespace, file provenance, and the order the predicates happened to be declared in. The
  result is independent of source layout by construction.
- **Fingerprint:** a hash over the canonical form — one **per predicate**, plus one for the
  **whole schema**. Because it's computed over the canonical form, file layout and
  declaration order never affect it.

This is a [tier-2 metamorphic property](testing.md): two source orderings of the same schema
produce **identical** fingerprints. Read "ordering" precisely — *declaration* order and file
layout are not identity-bearing, but a record's **field order is**, because it is encoding order
and it decides the seek prefix ([chapter 7](07-compilation.md#how-sargeability-actually-decides-phase-4-as-built)).
Permuting a predicate's fields is a semantic change and must move the fingerprint; Glean agrees,
hashing field names *and* order into a definition's fingerprint
(`glean/db/Glean/Database/Schema/ComputeIds.hs:264-274`) and handling a field reorder as a
transform rather than as identity.

**Here Aperture and Glean converge, and it is worth stating as shared ground rather than
coincidence.** Glean's `SchemaId` is `hashBinary (sort (toList nameEnv))` — a hash of the sorted
qualified-name → definition-id environment
(`glean/db/Glean/Database/Schema/ComputeIds.hs:307,340-342`) — structurally the same object as
the identity map below, and Glean has the same two granularities, one per definition and one for
the schema set.

**One difference constrains the canonical form.** Glean's per-predicate hash is a **Merkle**
hash: a reference inside a type is a `PredicateId` that *carries* the referent's hash, so
changing a predicate changes the fingerprint of everything transitively referencing it. Aperture's
`PredicateId` is a **position** in the schema, not a hash (`crates/aperture-schema/src/schema.rs`) — so the
canonical form must not spell a `Fact`-typed field as its id: a position would make the
fingerprint depend on declaration order, the very thing it exists to be independent of, and a
bare name would not propagate the referent's change. Spell it as the referent's
**fully-qualified name plus its own fingerprint** and the propagation comes for free — at the
price of needing Glean's two-pass cycle hash the moment two predicates reference each other.

### Compatibility = subset containment

Schema identity is the map `qualified_name → predicate_fingerprint`. In P0, compatibility
is deliberately collapsed to set containment:

```
compatible(old → new)  ⇔  old_map ⊆ new_map
```

so **the only compatible change is adding a new predicate.** Any in-place modification of a
predicate's key or value is *Breaking* until schema evolution exists — because values are
queryable and positionally encoded, so a field change shifts stored bytes. No field-level
diffing is needed in P0. (Richer evolution is deferred with the seam kept — see
[Operations](aperture-cli-design.md) §11 and [open decisions](open-decisions.md).)

**Do not model that seam on Glean's `evolves`, which is not the mechanism its name suggests.**
Glean's dominant path is *edit the schema in place*: two instances of a predicate sharing a
name but differing in hash are related automatically, and queries against the instance without
facts are routed through a per-predicate transform — `calcAutoEvolutions`,
`glean/db/Glean/Database/Schema.hs:640-657`, which Glean's own design note calls *version-less
schema migration*. `evolves` is the **manual** path, and it carries traps: it takes effect only
if there are **no facts of the old schema** in the DB and is silently ignored otherwise
(`glean/website/docs/schema/changing.md:167-170`), transforms are built only for a read-only DB,
and the transitive compatibility check is O(n²) and therefore **disabled by default**
(`Schema.hs:1054-1062`). Nor is `schema all.N` the version axis it reads as — it is the
**name-resolution scope**, which version an unversioned name means
(`glean/website/docs/schema/all.md:22-46`); per-schema `.N` is "a somewhat legacy feature" in
Glean's docs and `inherit` is deprecated.

### The schema is embedded and frozen ([I13](invariants.md#i13))

> **I13 — the DB's schema is embedded and frozen at create.** The canonical schema +
> fingerprint are embedded in the DB at `create` and immutable for its lifetime (no in-place
> `evolves` in P0). Every ingest is validated against the embedded schema by subset
> containment; the DB carries its own schema.
>
> *Guard:* `schema::ingest_rejects_incompatible_schema` (subset containment enforced at
> ingest) + `schema::fingerprint_is_order_independent` (tier-2 metamorphic).

Embedding the schema is what lets ingest validate a fact file's producing-schema
fingerprint cheaply (a handshake compares fingerprints before any bytes flow), and what
makes a Complete DB a portable, self-contained artifact ([ops-I4](invariants.md#ops-i4),
reproducibility).

**The freeze is Glean's own fallback promoted to a rule.** When a change is incompatible, Glean's
documented escape hatch is to bump the version and treat the new schema as entirely separate, or
to *produce two databases* and point clients at the one they want
(`glean/website/docs/schema/changing.md:80-117`) — which is precisely Aperture's default
workflow: a fresh, sealed artifact per run. Subset containment is a strictly stronger and simpler
rule than Glean's `canEvolve`, and it cannot fail the way a compatibility check that ships
disabled can. Three ways it can still bite, each cheaper to decide in **Phase 8** than after:

- **Adding an *optional* field is Breaking here.** Under subset containment the only migration is
  a new predicate name and a rewrite of every query — where Glean makes field addition routine
  with a per-type default-value table (`changing.md:64-77`; "defaultable" is anything that is not
  a predicate reference, since a reference has no default). If Aperture ever wants that, the two
  things to fix while the DSL is being written are a per-type default rule and the guarantee that
  a record's trailing fields are skippable — which [I2](invariants.md#i2)'s self-delimiting
  encoding already gives.
- **A client can be versioned even when the DB is not.** Glean's transform exists mostly because
  a client compiled against one schema queries a DB written with another — its `schema_id`
  travels on every query, and a batch's must match the DB's. I13 makes the DB self-describing; it
  says nothing about a *reader* older than the DB it opens, because that mismatch is between a
  query and a DB rather than between two schemas on disk. Lockstep rebuild of the reader is the
  invariant's boundary condition, and it should be written down as one.
- **The fingerprint algorithm is an unversioned dependency of every fact file.** Ingest compares
  fingerprints as a handshake, so changing *how* a fingerprint is computed would silently reject
  every artifact already produced and take [ops-I4](invariants.md#ops-i4) with it. Glean hit
  exactly this and now **persists** the externally-visible `SchemaId` in the DB so that "the
  SchemaIds that were previously computed remain unchanged" even if its internal fingerprinting
  changes (`glean/if/internal.thrift:24-33`). Version the algorithm in the `APERTURE_META`
  sidecar ([Operations](aperture-cli-design.md)) and treat the *stored* fingerprint as
  authoritative — this is the load-bearing one for Phase 8.

> **Schema *syntax* and import/`mod`-tree resolution** (the schema DSL, Go-style import
> edges, `schema_path` roots, redeclaration errors) are a front-end concern covered in
> [Operations §7](aperture-cli-design.md) and built in **Phase 8** ([`PLAN.md`](../PLAN.md)).
> This chapter is the *type model and identity*; that chapter is *how schemas are written and
> resolved*.

---

## Invariants owned by this chapter

| # | Statement | Guard test |
|---|-----------|------------|
| [I10](invariants.md#i10) | Union discriminants are stable and append-only. | `schema::discriminants_append_only` (pending unions) |
| [I13](invariants.md#i13) | The DB's schema is embedded and frozen at create. | `schema::ingest_rejects_incompatible_schema` + `fingerprint_is_order_independent` (pending schema) |

Related: [I3](02-tuple-codec.md) (frozen markers) — the codec-side counterpart of the same
"frozen once data exists" principle.

---

> **Reading path:** [← 5. Suspend & resume](05-resume.md) · **6. Types & schema** · [7. Compilation →](07-compilation.md)
