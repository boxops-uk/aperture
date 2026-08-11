# 6. Types & schema

> [Aperture design book](../README.md) · [← 5. Suspend & resume](05-resume.md) · **Chapter 6** · [7. Compilation →](07-compilation.md)

Every fact is typed, and the types come from a **schema**. This chapter covers the type
model, how names are interned, unions and their frozen discriminants, and **schema
identity** — the canonical form and fingerprint that make a DB self-describing and let
compatibility be a set-containment check. Code: `src/focus/schema.rs`.

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

**Four constructors, and that is the whole model** — no arrays, enums, booleans, optionals, or
a byte/nat distinction, all of which Glean's type system has. The codec has reserved marker
bands for them ([chapter 2](02-tuple-codec.md#the-marker-table)), so the room is physically
there; what is *not* settled is whether they are wanted, and multiplicity is the one that
changes how schemas are written rather than just what they can hold. See
[the Glean comparison](glean-comparison.md#the-type-model-is-narrower-than-gleans).

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
append-only tags are the only safe scheme. Get it right *before* writing any union facts —
after that it's an on-disk migration ([I3](02-tuple-codec.md) territory).

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
  whitespace, file provenance, and declaration order. The result is order-independent by
  construction.
- **Fingerprint:** a hash over the canonical form — one **per predicate**, plus one for the
  **whole schema**. Because it's computed over the canonical form, file layout and
  declaration order never affect it.

This is a [tier-2 metamorphic property](testing.md): two source orderings of the same schema
produce **identical** fingerprints.

### Compatibility = subset containment

Schema identity is the map `qualified_name → predicate_fingerprint`. In P0, compatibility
is deliberately collapsed to set containment:

```
compatible(old → new)  ⇔  old_map ⊆ new_map
```

so **the only compatible change is adding a new predicate.** Any in-place modification of a
predicate's key or value is *Breaking* until schema evolution (`evolves`) exists — because
values are queryable and positionally encoded, so a field change shifts stored bytes. No
field-level diffing is needed in P0. (Richer evolution is deferred with the seam kept — see
[Operations](aperture-cli-design.md) §11 and [open decisions](open-decisions.md).)

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
