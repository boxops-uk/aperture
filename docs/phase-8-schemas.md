# Phase 8 — Schemas: the DSL, imports, and identity

> [Aperture design book](../README.md) · **proposed phase plan**, not yet folded into
> [`PLAN.md`](../PLAN.md). Written on the per-phase template so it can be moved there whole
> once accepted — the same standing [`phase-10-capacity.md`](phase-10-capacity.md) had.
>
> **This file is decisions first and syntax second, on purpose.** Phase 8 fixes what can be
> written down for the life of every database anyone builds, and four of the seven decisions
> below are cheaper to make now by a wide margin. The grammar is the easy half.
>
> **D1 and D3 were checked against Glean's source rather than against memory**, at `main`:
> `glean/db/Glean/Database/Schema/ComputeIds.hs`, `Schema.hs`, `Schema/Types.hs`,
> `if/glean.thrift`, `rts/fact.h`. That reading **reversed D1's recommendation** and roughly
> halved D3's cost. Where this file quotes Glean it quotes those files; where it diverges it
> says so. (Note that `code/glean` in this repo is a different thing entirely — an early
> Rust query-language prototype, the ancestor of `focus`, with no schema system in it.)

**Goal.** Parse schemas, so predicate and type definitions stop being hardcoded Rust — a
separate schema DSL feeding the same type model the query compiler already uses — and give a
schema an identity that a database can embed, a fact file can name, and two ends of a wire
can compare.

**Depends on:** Phase 7a (ingest validates against a real schema) and Phase 2 (the
permissive-early grammar discipline, and `lelwel`).

**Design of record:** [chapter 6](06-types-and-schema.md) for the type model, canonical form,
fingerprints, subset containment and the freeze; [operations §7](aperture-cli-design.md) and
§5's `schema check` / `fingerprint` / `diff` for syntax, imports and the commands. **Read
those; this file does not restate them.**

**Invariants in scope:**
- *makes green:* [I13](invariants.md#i13) — `schema::ingest_rejects_incompatible_schema` and
  `schema::fingerprint_is_order_independent`; [I10](invariants.md#i10) —
  `schema::discriminants_append_only`, once unions are represented. These three are the last
  pending entries in the coverage ledger, and their bodies are already written as the
  specification (`crates/aperture-schema/src/schema.rs`).
- *upholds:* [I3](invariants.md#i3) (a union marker is **appended**, never renumbered),
  [I11](invariants.md#i11) (a predicate id must fit the 24-bit fact-id tag — and see D1,
  which is a larger claim than width), [I1](invariants.md#i1) (a union's discriminant is part
  of an order-preserving key).

---

## 1. Seven decisions, and the first one reframes the phase

### D1 — What assigns a `PredicateId`, and what keeps it still — **settled**

**The one to settle before anything else is written, and it was not on the phase's task
list.** It is not a syntax question: it falls out of three requirements that are each already
agreed, and it is the decision the rest of the identity work is shaped by.

A `PredicateId` is today a **position** in `Schema::predicates`. It is also the 24-bit tag
inside every [`FactId`](03-storage-model.md), the name of both keyspaces (`keys.<id>`,
`entities.<id>`), and the predicate field in every block header on the wire and in every fact
file. It is on disk, in bulk, forever.

Three requirements:

1. **Reproducibility** (`ops-I4`) — the same schema must produce the same database.
2. **Identity is layout-independent** ([I13](invariants.md#i13)'s guard) — two source
   orderings of one schema must have the *same fingerprint*.
3. **Adding a predicate is compatible** (subset containment) — a superset schema must leave
   every existing predicate's id exactly where it was, or every `FactId` already written now
   names a different predicate.

| Scheme | Fails |
|---|---|
| Declaration order, id a function of the schema *text* | (2) — reformatting moves ids, so two schemas with one fingerprint disagree about what a stored id means |
| Sorted by qualified name, recomputed per schema | (3) — adding `a.Aardvark` renumbers everything after it |
| Hash of the name, truncated to 24 bits | collisions: ~3% at a thousand predicates, and a collision is unresolvable without renaming something |

**An earlier draft of this file concluded from that table that no implicit scheme can work,
and recommended explicit ids written by hand. That was wrong, and reading Glean is what
showed it.** The error was assuming the physical id has to be a function of the schema
*text*. It does not — it can be a function of the **database**.

#### What Glean does (read at `main`, quoted below)

Glean splits the two jobs completely:

- **Identity is a content hash.** A `PredicateId` is `PredicateId ref hash`, where the hash is
  `hashBinary (showRef ref, rmLocType keyTy, rmLocType valTy, fmap rmLocQuery drv)` — name and
  version, key and value types with locations stripped, and the derivation
  (`ComputeIds.hs`, `fingerprintDef`). Field names and order ride inside the types. No number
  appears anywhere in it.
- **The physical tag is per-database and persisted.** A `Pid` is a small integer
  (`newtype Pid`), paired with the hash by `PidRef`. It is assigned by *sorting* and
  enumerating — `zip (sort $ fst <$> toList (hashedPreds stored)) [lowestPid..]` — and then
  **written into the stored schema**, `storedSchema_predicateIds = Map.fromList [(fromPid $
  predicatePid p, predicateRef p) | …]`, and read back on open. A database keeps its Pids for
  life.
- **Adding a predicate appends.** `nextPid = maybe lowestPid succ $ maximumMay (map snd
  storedPids)`, then `addedPids = zip ids [nextPid ..]`. The sort decides the *initial*
  assignment only; after that the stored map is authoritative and new predicates go on the
  end. Existing Pids never move.
- **Serialized facts do carry the Pid** (`rts/fact.h`: `serialize(binary::Output&, Pid type,
  Clause)`), and a `Batch` carries a `SchemaId` so the receiver can tell which schema the
  numbers belong to. Glean therefore has Aperture's portability problem too, and answers it
  by naming the schema rather than by making the number global.

#### Settled: Glean's split

**Taken.** No ids in the DSL. Identity is the content-hash map, with no number in
it — which is what makes requirement (2) hold by construction. The physical id is assigned at
`create` by sorted qualified name, **persisted in the database's embedded schema**, and
**append-only** when predicates are added — which is what makes (3) hold. Sorted assignment is
deterministic, so (1) holds.

One rule has to come with it, and it is the part Aperture must add rather than copy: **ingest
must check the id map agrees for the predicates present, not merely that the fingerprints are
subset-contained.** A block header carries a raw `predicate u32`, so a fact file written
against a database whose map assigned `src.Ref = 4` is nonsense against one that assigned it
`5` — and two databases created independently from *different* schemas can be
subset-compatible and still disagree, because each sorted its own predicate set. The map is
embedded already, so the check is a lookup; what it is not is free to forget.

The cost, accepted knowingly: a fact file is portable to a database **whose schema it was
written against**, not to any database that happens to declare those predicates. The rejected
alternative — explicit ids written by hand in the DSL — would have bought that stronger
portability at the price of every schema carrying numbers a person maintains. If fact files
ever need to travel between independently-created databases, this is the decision to reopen,
and the id-map check is where the failure would announce itself.

### D2 — The fingerprint algorithm is a cross-language, unversioned dependency

Chapter 6 calls this "the load-bearing one for Phase 8", and it is worse than that chapter
says, because the .NET client implements the fingerprint too
(`clients/dotnet/Aperture.Client/Schema.cs` computes `provisional_fingerprint` and its own
doc already anticipates being replaced). Changing *how* a fingerprint is computed silently
rejects every artifact and every client already built.

**Recommendation, three parts:**

- **Version the algorithm** in `APERTURE_META`, and treat the **stored** fingerprint as
  authoritative — Glean's answer after it hit exactly this (`glean/if/internal.thrift:24-33`).
- **Specify the canonical form as a byte string**, not as "a hash over the schema". A second
  implementation needs something it can produce character by character; anything vaguer means
  the C# client is guessing.
- **Keep the provisional fingerprint alive for one release** so a client can claim either,
  and retire it when the golden is regenerated. The handshake already carries a "do not
  check" zero, so the mechanism exists.

### D3 — How the canonical form spells a `Fact`-typed field — **settled**

Chapter 6 settles the shape and leaves the consequence: a reference must be spelled as the
referent's **fully-qualified name plus its own fingerprint**, because a position would make
identity depend on declaration order and a bare name would not propagate a change in the
referent.

**The consequence to plan for is cycles.** Two predicates that reference each other have no
well-founded hash under that rule.

**Glean's scheme is concrete enough to transcribe**, which makes this cheaper than the earlier
draft assumed. `computeIds` finds strongly-connected components with `stronglyConnComp`, and
for a cyclic group: map every reference in the group to `hash0`, compute each member's
individual hash against that, take `cycleHash = hashBinary hashes` over the group, then give
each member `hashBinary (individualHash, cycleHash)`. Every member of a cycle is thereby
distinct, and the whole group's hash changes if any of them does.

**Settled: implement it.** It is perhaps fifty lines plus an SCC pass, it is a
known-good algorithm rather than one to invent, and refusing cycles is a permanent limit on
what a schema can say for a saving that has just got much smaller. The cost that remains is
D2's: every client that computes a fingerprint has to reproduce this exactly, so the canonical
form must specify the SCC ordering (members sorted by qualified name) as tightly as everything
else.

### D4 — Multiplicity: arrays, or one fact per element — **settled**

The standing [open decision](open-decisions.md#multiplicity--arrays-or-one-fact-per-element),
which says in terms: *decide before the schema DSL fixes what can be written*. Its shape is
already constrained three ways — prefer the value side, forbid or diagnose an array in a
leading key field, and it couples to Phase 8b, because Glean's array story works only because
`stored` derivation exists to explode one into a seekable index.

**Settled: no `[T]` in Phase 8**, and said in the DSL by diagnosing it rather than by having no
syntax — a `nyi/array` code, per permissive-early, so the message names the decision
instead of reading as a parse error. The codec's reserved band means adding it later is not a
one-way door in the encoding; writing every schema without it is the expensive thing to undo,
and 8b is what would make it pay.

### D5 — Unions: three sub-decisions, all frozen the moment one fact is written

1. **Discriminant syntax.** Explicit and append-only ([I10](invariants.md#i10)). Same shape as
   D1, and the same reason.
2. **The unknown tag.** Chapter 6 flags that I10 does not say what a decoder does with a
   discriminant it has never seen, and that a fact file outlives the schema that wrote it.
   Per errors-not-panics it must be an `ApertureError`. **Recommendation:** decode failure by
   name (`UnknownDiscriminant { predicate, tag }`), not a synthetic `unknown` alternative —
   Glean needs one because it projects between schemas at query time, and
   [I13](invariants.md#i13) means Aperture never has two schemas to project between.
3. **The marker.** `0x52`, appended — `MARK_FACT_REF` is `0x51` and the table stops there.
   Appending is what [I3](invariants.md#i3) permits; the golden marker test is edited
   deliberately. Note for [I1](invariants.md#i1): a union sorts *after* every other type, and
   within a union by discriminant then payload — so alternatives cluster in a key, which is a
   seek property worth writing down before somebody depends on the opposite.

### D6 — Optional fields are Breaking, and whether to keep the seam open

Under subset containment, adding a field to an existing predicate is Breaking; the only
migration is a new predicate name and a rewrite of every query. Glean makes field addition
routine with a per-type default table. Chapter 6 names the two things to fix *while the DSL is
being written* if that is ever wanted: a per-type default rule, and the guarantee that a
record's trailing fields are skippable — which [I2](invariants.md#i2) already gives.

**Recommendation:** stay with subset containment, and write the two prerequisites into the
DSL's notes rather than the DSL. This is a decision to record, not to build.

### D7 — A reader older than the database it opens

[I13](invariants.md#i13) makes a *database* self-describing and says nothing about a client
compiled against a different schema. The current answer is lockstep rebuild, and chapter 6
says it "should be written down as one". **Recommendation:** write it into the invariant's
boundary conditions — one paragraph, no code.

---

## 2. The grammar

Following Phase 2 exactly: a `lelwel` grammar, **permissive-early**, every deferred construct
drawing a specific `nyi/…` diagnostic rather than a parse error, and an executable corpus so
the audit table cannot drift from what the compiler does.

```
schema src;                                  -- namespace, open across files
import lang.rust;                            -- explicit Go-style edge

predicate File = 0 : string
predicate Module = 1 : { file : File, name : string }
predicate Decl = 2 : { module : Module, name : string, line : int } -> string
type Position = { line : int, col : int }    -- a named record, no id, not a predicate
```

Points the grammar has to make legible rather than merely accept:

- **Field order is the key order.** Chapter 6 is explicit that the DSL "inherits" this and
  that "declaration order is load-bearing and has to read as such". A record is written in the
  order it is stored, and the manual should say what that costs — this is the phase that
  follows `bench/FINDINGS.md` §2, where alphabetical habit cost 56,274 rows examined per row
  produced.
- **The value side is `->`**, and is the thing a query reads but never matches on
  ([I6](invariants.md#i6)).
- **`type` versus `predicate`**: a named record is sugar with no identity of its own, expanded
  before canonicalisation. Worth having early — it is what stops `Position` being written out
  four times — and it costs the type model nothing.

Deferred with a code, not a parse error: `[T]` (D4), `set T`, `nat`/`byte`, `evolves`, and —
until 8d — `union`.

---

## 3. Imports and resolution

Operations §7 settles this; the build is a transcription:

- every file opens with a namespace declaration; namespaces are **open across files**;
- imports are **explicit edges** with concatenation semantics — transitive closure, dedup by
  file identity, union the blocks. **Cycles are harmless by construction**, and the real error
  is genuine redeclaration: two different definitions of one fully-qualified name, as against
  the same file reached twice;
- roots come from `schema_path`, first-match-wins — a new config key, and `config.rs` has none
  today;
- transitive visibility is accepted and documented rather than fought.

---

## 4. Sequence

Each step ends green, and the order is chosen so the riskiest thing — identity — is settled
before anything depends on it, and unions come last because they are the widest blast radius.

- **8a — Decisions.** ✅ D1, D3 and D4 settled above and recorded in
  [`open-decisions.md`](open-decisions.md) — multiplicity has moved out of "still open", and
  predicate ids are recorded there as a decision that was never an open *question*, only an
  unexamined assumption. D2 and D5 are recommendations this file carries into 8c and 8f; D6
  and D7 are one paragraph each to write into [chapter 6](06-types-and-schema.md).
- **8b — Lexer, grammar, corpus.** Parse the surface, including what is deferred, each with
  its code. *Done when* the schema corpus parses as classified, the way
  `aperture_engine::corpus` gates the query one.
- **8c — Lower to `Schema`, and identity.** The canonical form as a specified byte string,
  per-predicate and whole-schema fingerprints, D1's id validation, D3's cycle answer. *Done
  when* `fingerprint_is_order_independent` is un-ignored and green, **with its negative
  control**: a field permutation must move the fingerprint.
- **8d — Load a database from a schema file.** `create` takes a schema path; the embedded copy
  becomes the canonical form; `code_index` is deleted. *Done when* a parsed schema runs a query
  end to end, and when `ingest_rejects_incompatible_schema` is green.
- **8e — `schema check` / `fingerprint` / `diff`.** The three commands §5 specifies, `diff`
  answering `Identical | Compatible (n added) | Breaking` with per-predicate reasons.
- **8f — Unions.** `PredicateTy::Union`, marker `0x52`, the discriminant freeze, `X.alt?`
  lowering to `DiscriminantEq` plus a payload bind. *Done when* `discriminants_append_only` is
  green and the `nyi/union-select` corpus entry is reclassified `Supported` with its rows and
  the code retired from `Code::ALL`.

---

## 5. What this deletes, and what has to move with it

- **`src/code_index.rs` goes.** It is 22 predicates of hardcoded Rust and its own module doc
  says it is deleted rather than ported. Its `KEY_ORDER` guard becomes the schema file itself;
  its `CATALOGUE`/`with_catalogue` split has to survive, because a virtual predicate is a
  property of the *server* and must stay out of the fingerprint, the embedded copy, and the
  keyspaces.
- **Both .NET schema statements and the golden.** `Aperture.Indexer/CodeIndex.cs` and
  `Aperture.Demo/Program.cs` state the schema independently *on purpose* — that is what makes
  the byte-identical golden meaningful. They do not have to parse the DSL, but they do have to
  compute D2's fingerprint, so the canonical form has to be portable. Budget this as real
  work, not a follow-up.
- **`schema_doc`** is explicitly provisional and safe to replace; nothing reads it to make a
  decision.
- **The blast radius of `PredicateTy::Union`** is **29 files** that name the enum's variants —
  about half of them tests, but the other half is the codec, the wire value encoder,
  `flatten`, `iter`, `intern`, `desc` and `print`, each of which has to answer what a union
  means before it compiles again. That is the reason unions are 8f and not 8c: everything
  before them stays a schema-crate change, and 8f is the one step that reaches the machine.

---

## 6. The one-way doors, in one list

| | Frozen the moment | Recovered by |
|---|---|---|
| A database's predicate id map (D1) | that database is created | nothing, for that database — it is the tag in every `FactId` it holds. A *new* database is free to assign afresh, which is the whole reason the map belongs to the artifact rather than to the schema text |
| Union discriminants (D5) | the first union fact is written | an on-disk migration |
| The union marker (D5) | the same moment | I3 forbids renumbering |
| The fingerprint algorithm (D2) | the first artifact ships | a version field, if there is one |
| No arrays (D4) | every schema written before it | rewriting schemas and re-indexing |
| Reference cycles (D3) | the first cyclic schema | a two-pass hash, if refused early |

---

> [← 6. Types & schema](06-types-and-schema.md) · [Index](../README.md) ·
> [Operations →](aperture-cli-design.md)
