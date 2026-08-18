# Glossary

> [Aperture design book](../README.md) · reference doc

Every term of art in one place, with a pointer to the chapter that goes deep. Alphabetical.

---

**Access** — a `Generator`'s target: a `predicate_id` plus a `SeekKey`. [ch4](04-executor.md).

**antichain** — a layer of statements with no ordering dependency between them. `Deps::antichains`
answers *whether* a valid order exists, and is the independent witness the reorder completeness
property is checked against — it is **not** on the reorder path, which is a greedy runnable
frontier. Neither is it Glean's algorithm: `Reorder.hs` has no antichains and no topological sort.
[ch7](07-compilation.md).

**Aperture DB** — the database / product. Immutable: built once, sealed, then read-only.
[ch1](01-concepts.md).

**BadResumeKey** — the error raised when a resumed level's re-read row doesn't have the
saved `fact_id`; the integrity check that makes a bytes-only cursor safe. [ch5](05-resume.md).

**boxed AST** — the third tree representation (`Query`/`Pattern`/`PatternKind`): the
ergonomic, human-facing shape. [ch7](07-compilation.md).

**ByteView** — a cheaply-cloneable byte buffer (clone = refcount bump, not copy); how rows
are shared across registers without allocation. [ch4](04-executor.md), [conventions](conventions.md).

**canonical form (schema)** — the order-independent, fully-qualified, comment/whitespace-
stripped form of a schema; the thing a fingerprint is computed over. [ch6](06-types-and-schema.md).

**column family** — one of the two sorted key–value maps: `keys` (the index) and `entities`
(identity → key+value). [ch3](03-storage-model.md).

**Complete** — the sealed, immutable lifecycle state; every open-for-write is then refused
([ops-I2](invariants.md#ops-i2)). Opposite: **Writable**. [Operations](aperture-cli-design.md).

**corpus** — `aperture_engine::corpus`, the focus language surface as *data*: each snippet classified
`Supported` / `Diagnosed(code)` / `ParseError`, and the acceptance gate for permissive-early.
[testing](testing.md).

**CST façade** — the first tree: an untyped, lossless, grammar-shaped concrete syntax tree
with spans and text. [ch7](07-compilation.md).

**Cursor** — the resume token: **bytes only**, one saved (detached) row per active loop
level; pins no iterator and no snapshot. [ch5](05-resume.md).

**defunctionalised state machine** — writing the recursive `concatMap` of a nested loop as
an explicit frame stack + `enumerate` driver, so it can suspend to bytes ([I7](invariants.md#i7)).
[ch4](04-executor.md).

**derived bind** — a plan step `Z = f(bound vars)` that computes a value into a value-slot;
not a loop level, recomputed on resume; must be a pure function of the fact bindings.
[ch7](07-compilation.md#derived-facts).

**derived fact / derived predicate** — a predicate whose facts are computed by a query
(`… = KEY where <query>`) rather than stored. **Two different features share the name:**
*stored* derivation writes the facts at build time and the executor never knows the difference
(PLAN Phase 8b, gated on the schema DSL); *dynamic* derivation computes a value while a query
runs, and is what the `Register→Slot` machine change was for (Phase 6). Our two are not Glean's
three deriving modes — [the ledger](glean-comparison.md) maps them.
[ch7](07-compilation.md#derived-facts).

**discriminant** — the explicit, append-only tag identifying a union alternative
([I10](invariants.md#i10)). [ch6](06-types-and-schema.md).

**entities** — the column family `fact_id → key+value`; point lookup by identity; read only
at projection/navigation. [ch3](03-storage-model.md).

**enumerate** — the executor's driver loop: descend, pull a matching row, bind, recurse;
backtrack on exhaustion; suspend on `Stream::Suspend`. [ch4](04-executor.md).

**fact** — a typed record, the unit of data; belongs to a predicate; has a `FactId`.
[ch1](01-concepts.md).

**FactId** — a `u64` identifying a fact within a DB: a **snowflake** — predicate id in the
high 24 bits, per-predicate sequence in the low 40. Unique, stable, never reused
([I11](invariants.md#i11)); a physical id, not cross-DB identity. The tag is what lets
`entities` be split per predicate and `point()` still be one lookup.
[ch3](03-storage-model.md).

**FactRef** — a fact-reference value; encoded with its own `MARK_FACT_REF` (0x51). Typed
`PredicateTy::Fact(PredicateId)`. [ch2](02-tuple-codec.md), [ch6](06-types-and-schema.md).

**FactStore** — the storage trait (`scan`, `point`); implemented by fjall (product) and
`MemStore` (tests). [ch3](03-storage-model.md).

**fingerprint (schema)** — a hash over the canonical form; per-predicate and whole-schema;
identity/compatibility are compared by fingerprint. [ch6](06-types-and-schema.md).

**FieldOffsets** — the inline `ArrayVec<[usize;16]>` cache of field boundaries within a
row's key; never heap-spills ([I9](invariants.md#i9)). [ch4](04-executor.md).

**fjall** — the LSM key–value store backing Aperture. [ch3](03-storage-model.md).

**FieldPath** — how a plan names a key field: a top-level field, plus one step per record it is
nested inside. Flat is the fast path the field-offset cache serves; a stored key is *not* one
field, so a whole record key has no path ([ch3](03-storage-model.md#a-stored-key-is-flat)).
[ch7](07-compilation.md).

**flatten** — the compiler phase that lowers a typed query to the flat `[Step]` + `head`
plan: collect statements, check range restriction, reorder, then run sargeability.
[ch7](07-compilation.md).

**folding** — substituting a variable bound to a constant (`X = 42`, or a record of constants) at
every use, rather than giving it a register and a plan step. A folded bind reaches a key field as
the literal written in place would, and takes no space in the machine.
[ch7](07-compilation.md#folding-a-constant-bind).

**focus** — Aperture's query and schema *language* (and the `crates/aperture-engine/` module implementing
the engine + language). [ch1](01-concepts.md).

**Generator** — one loop level in a plan: `{ access, binds, residuals }`. [ch4](04-executor.md).

**head** — the plan's output projection (`Project`), applied to the bound registers to build
each result row. [ch4](04-executor.md).

**iteratee** — the consumer side of the executor seam: the `step` callback that receives each
`Row` and returns `Continue`/`Suspend`. The executor is the enumerator (producer).
[ch4](04-executor.md).

**k-way merge** — the ingestion step that *was* to merge per-worker sorted runs, deduping and
rejecting at the frontier. **Not built, and no longer on the path**: a key holding a nested
reference has no bytes to sort until interning has run, and Phase 12 made interning-as-you-decode
correct under many writers instead. It survives as an optimisation, not a plan
([Operations §5](aperture-cli-design.md)). The **merge frontier** the term pointed at is now a
real thing and a different one — see below.

**keys** — the column family `predicate_id ++ encoded_key → fact_id`; the index; prefix scans
over it *are* predicate queries; the only CF the scan hot loop touches. [ch3](03-storage-model.md).

**keyspace-per-predicate** — each predicate gets its own pair of fjall trees, `keys.<id>` and
`entities.<id>` (predicate id also stays the `keys` prefix); gives physical isolation,
fearless parallel ingest, and an O(1) wholesale drop. Costs ~30 ms per tree to create.
[ch3](03-storage-model.md).

**MachineState** — the register file: `Box<[Option<Slot>]>`, indexed by `Address`. Reads name
the kind they want (`fact` / `value`); a mismatch is a reported error. [ch4](04-executor.md).

**marker** — the leading byte of an encoded value; determines sort position and skip shape;
frozen once data exists ([I3](invariants.md#i3)). [ch2](02-tuple-codec.md).

**MemStore** — the in-memory `FactStore` for tests only; its scan pins no snapshot (so it
can't exercise [I8](invariants.md#i8)). [ch3](03-storage-model.md), [testing](testing.md).

**NodeId** — stable, cross-phase identity of a node in the `SyntaxTree` store; lets typecheck
annotate via side tables without mutating the tree. [ch7](07-compilation.md).

**one-write-funnel** — every writer passes the same validate→intern→dedup→reject pipeline
([ops-I5](invariants.md#ops-i5)). One *pipeline*, not one thread: it says there is no path around
the rules, never that one core applies them. [Operations](aperture-cli-design.md).

**merge frontier** — where a key's identity is decided: resolve-or-create, dedup, reject. Since
Phase 12 it is **striped** — one lock per `hash(predicate ++ key)`, so the exclusion is exactly as
wide as the thing being decided and a database takes as many writers as it has streams.
[I12](invariants.md#i12), [ch3](03-storage-model.md#the-other-half-of-the-bijection--one-key-one-fact).

**order-preserving** — `memcmp(encode(a), encode(b)) == cmp(a, b)`; the codec's defining
property ([I1](invariants.md#i1)). [ch2](02-tuple-codec.md).

**Plan** — the IR the executor consumes: `{ nvars, body: [Step], head }`. The fixed
contract between front end and back end. `body.len()` counts **steps**; `Plan::levels()` counts
loop levels, and a `Cursor` holds one row per level. [ch4](04-executor.md#the-plan-ir).

**point** — the `FactStore` operation that looks up a fact's `entities` row by `FactId`; must
not be called during a key-only scan ([I6](invariants.md#i6)). [ch3](03-storage-model.md).

**predicate** — a relation/table analogue; fixes a fact's type; its id is the key prefix.
[ch1](01-concepts.md), [ch3](03-storage-model.md).

**PredicateTy** — a type: `Int | Str | Fact(PredicateId) | Record(sorted fields)` (union
later). [ch6](06-types-and-schema.md).

**Project** — a projection node: `Lit | RegisterField | FactRef | Value | Computed | Record`.
[ch4](04-executor.md).

**range restriction** — the safety check flatten enforces: every used variable is captured in
some generator's key pattern; makes bind-before-use automatic in any order. [ch7](07-compilation.md).

**Register** — a bound row: `{ fact_id, bytes }` (the whole row, not a field —
[I5](invariants.md#i5)). The fact case of a [`Slot`](#). [ch4](04-executor.md).

**reorder** — the compiler phase choosing loop order: the greedy *runnable frontier*, complete
because the constraint is monotone, so a written order that reads before it binds is fixed
rather than refused. Selectivity within the safe orders is not built. [ch7](07-compilation.md).

**residual** — a filter applied to a scanned row during the scan: `EqConst | Prefix |
EqRegisterField` (key fields only, [I6](invariants.md#i6)). [ch4](04-executor.md).

**resume** — reconstructing executor state from a `Cursor` to continue a suspended query
exactly ([I4](invariants.md#i4)). [ch5](05-resume.md).

**Row** — the borrowed, one-step-lived view of a fully-bound result handed to the iteratee.
[ch4](04-executor.md).

**sargeable** — a key field that can narrow the scan (a seek or splice) rather than being
filtered afterward; sargeability is order-dependent. [ch7](07-compilation.md).

**SchemaInterner / LocalInterner** — the two-tier name interning: frozen `Arc`-shared schema
names vs per-query local names; schema-first resolution. [ch6](06-types-and-schema.md).

**seek / SeekKey** — the scan start position for a level; built from constant `Bytes` and/or
register-field **splices**. [ch4](04-executor.md).

**side table** — an auxiliary array indexed by `NodeId` (e.g. `Vec<Ty>`) holding a phase's
annotations without mutating the tree. [ch7](07-compilation.md).

**skip** — advance past one encoded value using only its marker (schema-free), landing
exactly at the next value ([I2](invariants.md#i2)). [ch2](02-tuple-codec.md).

**Slot** — what a register holds: `Fact(Register)`, a stored row, or `Value`, a derived bind's
computed output. Kept apart at the type level because splicing one where the other belongs
compares two encodings and quietly matches nothing.
[ch4](04-executor.md#the-register-file-and-the-row-slot-model-i5).

**snapshot** — a query's consistent read view; trivial for an immutable DB, but a fjall
iterator pins one, so it's dropped at suspend ([I8](invariants.md#i8)). [ch5](05-resume.md).

**splice** — bytes copied from an earlier-bound register into a seek key, narrowing an inner
scan to rows matching the outer row (how a join works). [ch4](04-executor.md).

**StackFrame** — one loop level's execution state: `{ scan, current, field_offsets }`.
[ch4](04-executor.md).

**Step** — one position in a plan's body: `Scan(Generator)`, a loop level, or
`Derive(DerivedBind)`, a value to compute. One ordered sequence, because `reorder` produces one
order. A derive step is a *one-row* generator and contributes nothing to a `Cursor`
([I14](invariants.md#i14)). [ch4](04-executor.md#the-plan-ir).

**strinc** — the prefix-successor: the smallest byte string greater than all strings with a
given prefix; the exclusive upper bound of a prefix scan. [ch3](03-storage-model.md).

**Stream / Iteratee** — the seam types: the consumer returns `Stream::Continue|Suspend`;
`enumerate` returns `Iteratee::Done|Suspended{cursor}`. [ch4](04-executor.md), [ch5](05-resume.md).

**suspend** — a voluntary, resumable yield (`Stream::Suspend`) producing a `Cursor`; distinct
from cancel and terminal unwind. [ch5](05-resume.md).

**sync marker** — a reserved, structurally-illegal byte sequence marking block boundaries in
a fact file, so parallel ingest can split without parsing from byte zero (candidate, then
header-validated). [Operations](aperture-cli-design.md).

**SyntaxTree store** — the second tree: a struct-of-arrays, `NodeId`-indexed typed tree the
compiler phases run on. [ch7](07-compilation.md).

**transport / wire codec** — the post-yield, framed binary format for rows leaving the
executor; not order-preserving; separate from the storage codec. [ch3](03-storage-model.md),
[Operations](aperture-cli-design.md).

**tuple codec** — the storage codec: order-preserving, self-delimiting; encodes both keys and
values. [ch2](02-tuple-codec.md).

**Writable** — the mutable lifecycle state before `finish`; ingestion happens here. Becomes
**Complete**. [Operations](aperture-cli-design.md).

---

> [← Capabilities, efficiency & cost](glean-capabilities.md) · [Index](../README.md)
