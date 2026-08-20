---
title: Glossary
description: Every term of art in one place, with a pointer to the page that goes deep.
---

## A–C

**Access** — a level's target: a predicate id plus a seek key. [Executor](executor.html)

**antichain** — a layer of statements with no ordering dependency between them. Used as the
independent *witness* the reorderer's completeness is checked against — deliberately not on the
reordering path, which is a greedy runnable frontier.

**block** — a run of one predicate's facts behind a sync marker, with a header and a CRC. The same
bytes on the wire and in a file. [Wire protocol](wire-protocol.html#blocks)

**bookmark** — what a client holds instead of a borrow of the connection: a `Rows` value, so several
results can be open at once. [Clients](clients.html)

**canonical form (schema)** — the order-independent, fully-qualified, comment-stripped form a
fingerprint is computed over. [Schema language](schema-language.html#identity-canonical-form-and-fingerprints)

**column family** — one of the two sorted maps: `keys` (the index) and `entities` (identity →
key + value). [Storage](storage.html#two-column-families)

**Complete** — the sealed, immutable lifecycle state. Every open-for-write is then refused.
Opposite: **Writable**.

**constraint** — a bind whose right side is a pattern rather than a value (`X = "a"..`): collected
from the whole body and applied by whichever level captures the variable, so it can *seek*.
[sigla](query-language.html#a-constraint-narrows-the-level-that-captures-the-variable)

**corpus** — the sigla language surface as *data*: each snippet classified `Supported(rows)`,
`Diagnosed(code)` or `ParseError`, and the acceptance gate for permissive-early.
[Testing](testing.html#the-corpus-the-language-surface-as-data)

**Cursor** — the resume token: **bytes only**, one detached row per open loop level, plus a layout
version and a plan fingerprint. Pins no iterator and no snapshot.
[Executor](executor.html#the-cursor-bytes-and-nothing-else)

## D–F

**defunctionalised state machine** — writing the recursive nested loop as an explicit frame stack
plus one driver, so it can suspend to bytes. [I7](invariants.html#i7)

**denial** — `!=`: this row's field does not look like that. A residual, never a seek — as against
`!`, which says no such row exists. [sigla](query-language.html#a-denial)

**derived bind** — a plan step computing a value from bound variables into a value slot. Not a loop
level; recomputed on resume rather than saved. [I14](invariants.html#i14)

**derived predicate** — a predicate whose facts a query could compute, stored keyed the way the
query wants to read it. **Two features share the name**: *stored* derivation writes the facts at
build time (not built), *dynamic* derivation computes a value while a query runs (built).

**discriminant** — the explicit, append-only tag identifying a union alternative.
[I10](invariants.html#i10)

**entities** — the map `fact_id → key + value`. A point lookup by identity, read at projection and
at a fetch.

**expansion** — replacing a reference in a row with the fact it names, recursively. A **client**
concern, built on the protocol's fetch. [A query, step by step](query-lifecycle.html#expanding-a-reference)

**fact** — a typed record: the unit of data. Belongs to a predicate, has a `FactId`.

**FactId** — a `u64` identifying a fact within a database: a **snowflake** — predicate id in the
high 24 bits, per-predicate sequence in the low 40. Stable, unique, never reused. A *physical* id,
not cross-database identity. [I11](invariants.html#i11)

**FactStore** — the storage seam: `scan` and `point`. Implemented by the real store and by an
in-memory one for tests — and by the wrapper that answers the virtual catalogue predicate.

**fetch** — two things, and they are related: a `Source::Fetch` level (reading *through* a
reference inside a query) and the `F`/`f` protocol frames (asking what an id names).

**FieldPath** — how a plan names a key field: a top-level field plus one step per record it is
nested inside.

**field-offset cache** — the inline, fixed-capacity memo of where each field of a row ends. Never
spills to the heap. [I9](invariants.html#i9)

**fingerprint (schema)** — a hash over the canonical form; one per predicate and one for the whole
schema. Identity and compatibility are compared by fingerprint.

**fjall** — the LSM key–value store Fjord is built on.

**Fjord DB** — the database. Immutable: built once, sealed, then read-only.

**flatten** — the compiler phase that lowers a typed query to the flat `[Step]` plus head: collect
statements, fold constants, collect constraints, check range restriction, hoist nested generators,
then decide sargeability. [A query, step by step](query-lifecycle.html#7-compilation)

**folding** — substituting a variable bound to a constant at every use, rather than giving it a
register and a step. A folded bind reaches a key field exactly as a literal in place would.

## G–P

**generator** — a fact pattern as a statement: a loop over one predicate's rows.

**head** — the plan's output projection, applied to the bound registers to build each row.

**iteratee** — the consumer side of the executor seam: the callback that receives each row and
answers `Continue` or `Suspend`.

**interning** — resolve-or-create at ingest: a nested fact's key is looked up, and it either takes
the id that is there or is written with a new one. Bottom-up, because a parent's key has no bytes
until its children have ids. [Storage](storage.html#interning-a-nested-fact)

**keys** — the map `predicate_id ++ encoded_key → fact_id`. The index; prefix scans over it *are*
predicate queries; the only map the scan hot loop touches.

**keyspace-per-predicate** — each predicate gets its own pair of physical trees. Physical
isolation, parallel ingest, and an O(1) wholesale drop.

**level** — one loop of the nested loop: a step with sources and binds. `Plan::levels()` counts
these, and a cursor holds one row per level.

**marker** — the leading byte of an encoded value: determines sort position and skip shape. Frozen
once data exists. [I3](invariants.html#i3)

**merge frontier** — where a key's identity is decided: resolve, dedup, reject. **Striped** — one
lock per `hash(predicate ++ key)` — so exclusion is exactly as wide as the thing being decided.

**one write funnel** — every writer passes the same validate → intern → dedup → reject pipeline.
One *pipeline*, not one thread (`ops-I5`).

**order-preserving** — `memcmp(encode(a), encode(b)) == compare(a, b)`: the storage codec's
defining property. [I1](invariants.html#i1)

**Plan** — the IR the executor consumes: `{ nvars, body: [Step], head }`. The fixed contract
between front end and back end.

**point** — the store operation that reads a fact's identity row by id. Must not be called during a
key-only scan. [I6](invariants.html#i6)

**predicate** — a relation, and the unit of storage. Fixes a fact's type; its id is the key prefix.

**PredicateTy** — a type: `Int`, `Str`, `Fact(p)`, `Record` of ordered fields, or `Union` of
tagged alternatives.

**Project** — a projection node: a literal, a field of a register, a reference, a value, a computed
value, or a record built out of those.

## Q–Z

**range restriction** — the safety check flatten enforces: every used variable is captured by some
generator's key pattern. Makes bind-before-use automatic in any order.

**register** — a bound row: a fact id plus the row's bytes. The fact case of a **slot**.

**reorder** — the compiler phase that chooses loop order: a greedy **runnable frontier**, complete
because the constraint is monotone. Load-bearing for *acceptance*, not just speed.

**residual** — a filter applied to a scanned row during the scan, on key fields only: equals a
constant, has this prefix, equals another register's field, does not equal, does not have this
prefix, and the order comparisons.

**resume** — reconstructing executor state from a cursor to continue a suspended query exactly.
[I4](invariants.html#i4)

**Row** — the borrowed, one-step-lived view of a fully bound result handed to the consumer.

**sargeable** — a key field that can narrow the scan (a seek or a splice) rather than being filtered
afterwards. Sargeability is **order-dependent**.

**seek / SeekKey** — the scan start position for a level, built from constant bytes and register
splices.

**sigla** — Fjord's query and schema *language*.

**slot** — what a register holds: a stored row, or a derived bind's computed value. Kept apart at the
type level, because splicing one where the other belongs quietly matches nothing.

**snapshot** — a query's consistent read view. Trivial for an immutable database, but an iterator
pins one, so it is dropped at every suspend. [I8](invariants.html#i8)

**splice** — bytes copied from an earlier-bound register into a seek key, narrowing an inner scan to
rows matching the outer row. This is how a join works.

**stratification** — the evaluation order a negation over a derived relation needs. Not a problem
for queries here (sigla has no recursion and the base is total); it returns under its own name for
stored derivation, as a topological sort of the derivation graph.

**strinc** — the prefix successor: the smallest byte string greater than every string with a given
prefix. The exclusive upper bound of a prefix scan.

**Step** — one position in a plan's body: a level, a derive or a test. Exactly three kinds, and a
test asserting that is the cheapest guard in the project.

**suspend** — a voluntary, resumable yield producing a cursor. Distinct from a cancel and from a
terminal unwind.

**sync marker** — ten `0xFF` bytes marking a block boundary. Unreachable inside a payload **by the
encoding**, which is what makes one file splittable for parallel ingest.

**transport codec** — the wire format: compact, schema-driven, not order-preserving. A sibling of the
storage codec, sharing no bytes with it.

**tuple codec** — the storage codec: order-preserving, self-delimiting, frozen on disk. Encodes both
keys and values.

**virtual predicate** — a predicate answered by whoever is running the query rather than read from a
keyspace — `fjord.db.List`. Absent from the handshake fingerprint, from a database's embedded
schema and from every artifact's keyspaces, which is why a client that has never heard of it
connects exactly as before.

**Writable** — the mutable lifecycle state before `finish`. Ingestion happens here.
