# Aperture — working contract

**Aperture** (the product: *Aperture DB*) is an embedded, immutable **fact database**;
**focus** is its typed, Datalog-flavoured query and schema language (the `aperture-engine`
crate). This file is the **working contract** loaded every session — how to work here, the
invariants by number, and where to read the rest. It is deliberately tight.

**The design is a documented book — read it, don't reinvent it.** Start at
[`README.md`](README.md) and follow the chapters. Everything below points into it; the
*why* behind every rule lives there, not here.

- Design book & reading path: [`README.md`](README.md)
- Every invariant (statement · why · guard · status): [`docs/invariants.md`](docs/invariants.md)
- Testing method & the coverage ledger: [`docs/testing.md`](docs/testing.md)
- Conventions & anti-patterns: [`docs/conventions.md`](docs/conventions.md)
- What's unsettled: [`docs/open-decisions.md`](docs/open-decisions.md)
- Where we diverge from Glean, and what we have not decided: [`docs/glean-comparison.md`](docs/glean-comparison.md)
- The build sequence: [`PLAN.md`](PLAN.md)

**Module map — a workspace, bottom to top.** Each crate depends only on the ones above it in
this list, and the compiler is what enforces that now; there is no edge pointing back.

| Crate | Holds |
|---|---|
| `aperture-schema` | the type model (`schema`) and the physical row id (`id`) — depends on nothing |
| `aperture-encoding` | the order-preserving storage tuple codec (`tuple`) and `StoreCodecError` |
| `aperture-wire` | the **transport** codec and its framing — `varint`, the schema-driven `value`/fact encoding, `crc`, `block` (a run of one predicate's facts behind a sync marker), `frame` (`[kind][stream][length]`). A sibling of `aperture-encoding`, not a layer on it: it depends on `aperture-schema` alone and shares no bytes with the storage codec |
| `aperture-store` | the `FactStore` seam, the fjall backend, the in-memory model, `fact`, the format stamp, the errors the storage layer raises — and the **lifecycle**: `catalog` (the store root, `ops-I1`'s lock, `ops-I7`'s filesystem-as-catalog), `meta` (the `APERTURE_META` sidecar), `schema_doc` (the embedded schema copy), `ulid` |
| `aperture-ingest` | the **write funnel** (`ops-I5`): `FactSink` (the write seam, as `FactStore` is the read seam), and `intern` — a `WireFact` in, a `FactId` out, nested references resolved bottom-up. Sits above `store` and `wire` because it is the crossing between them, and neither should know the other |
| `aperture-engine` | **focus** and the machine: lex → parse → typecheck → flatten → reorder → `Plan`, and the executor — all new query work lands here |
| `aperture-server` | the wire **protocol** over a Unix socket: the message vocabulary (`protocol`), one connection's life (`session`), rows out without a fourth encoder (`rows`), the listener (`server`) |
| root `aperture` | a lib (`code_index` — the built-in schema, hardcoded until Phase 8, **one definition** shared by both binaries) plus the shell (`src/main.rs`) and `src/bin/aperture-serve.rs`; becomes `aperture-cli` when it grows a command tree |

**A non-Rust client is part of the test surface.** `clients/dotnet` is a C#
implementation of the wire protocol plus a console producer that writes a nested code
index into a real database and queries it back — `./clients/dotnet/run-demo.sh`. It
exists to answer what the Rust tests cannot: whether the protocol is implementable from
outside, by something that shares no constants, no enums and no unwritten assumptions.
It has already earned that twice.

`src/main.rs` compiles and runs what you type against a real store seeded with a **code index**
(files → modules → declarations → references), written through the fact API; `:plan` shows the
plan. The index is a real one — `example/` holds a small Python corpus, the `ast`-based indexer
that reads it and the JSON it emits, which the shell compiles in and writes as facts at startup
([`example/README.md`](example/README.md)). Regenerate with `python3 example/index.py`. Keep
logic out of it — the plan renderer it needed lives in `aperture_engine::print`.
**`aperture_store::fact` is how a fact is written by hand**: a well-typed value whose key
fields are named, resolved against the schema (`FjallDb::put`), because `put_fact` takes bytes
and three of its preconditions fail silently — see
[chapter 3](docs/03-storage-model.md#writing-a-fact-by-hand). `aperture_store::fixture` is the
fixture database the corpus and the batteries share.
`aperture-engine/src/lib.rs` is the module list plus a commented-out graveyard (~20 live lines;
only the transport-codec sketch is worth keeping). See [chapter 1](docs/01-concepts.md).

**Test support spans two crates, and the split is load-bearing.** `aperture_store::fixtures`
holds everything store-shaped — the probes, the model stores, the scan-contract assertions,
the value helpers — because a probe has to be *the same* `FactStore` as the store it wraps;
`aperture_engine::fixtures` holds the plan runners and re-exports the rest, so a battery still
has one place to import from. A test in a lower crate that needs to run a query belongs in that
crate's `tests/` directory, not its `src/`: a unit test reaching back through the engine
compiles a second copy of its own crate, and the two `FactStore`s are then different types
(`aperture-store/tests/i8_snapshot.rs` is the one such guard).

---

## How to work here (read first)

- **Test-driven, property-first, verification mandatory.** Reasoning is not evidence —
  nearly every bug here (codec off-by-ones, a residual short-circuit, resume duplicating a
  row) was invisible to inspection and caught only by a generated case. Write the property
  first, watch it fail, then fill the impl. "It compiles" is not done. Full method:
  [`docs/testing.md`](docs/testing.md).
- **Every invariant owns a guard test, written up front** — even red / `#[ignore]`-pending
  ones. `cargo test -- --ignored --list` is the coverage ledger; a phase is done only when
  the invariants it touches are un-ignored and green.
- **Non-functional criteria are part of *done*, and are *tested*, not asserted** — no
  per-row allocation, no value fetch in the scan loop, no snapshot held across suspend each
  have a mechanical guard ([I5](docs/invariants.md#i5)/[I6](docs/invariants.md#i6)/[I8](docs/invariants.md#i8)/[I9](docs/invariants.md#i9)).
- **Keep diffs reviewable in one sitting.** The dominant failure mode here is a large,
  mostly-correct diff whose 10%-wrong part is expensive to find.
- **Respect the invariants absolutely.** Several look like implementation detail but are
  load-bearing or frozen on disk. If a change seems to require breaking one, stop and flag
  it — don't "simplify" past it.

---

## Build / test

```
cargo build
cargo test                          # the green suite
cargo test -- --ignored --list      # the invariant coverage ledger (guards not yet live)
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all
```

`default-members` is the whole workspace, so the first two mean *everything* without
`--workspace`. That is deliberate: the coverage ledger silently narrowing to one package as
crates are extracted would be a ledger that stops counting.

`fjall` is the storage backend; the `FactStore` trait (`aperture_store::fact_store`) is the
seam — its own module, so neither implementation can be mistaken for the definition — with an
in-memory `MemStore` (`aperture_store::mem_store`) **for tests only**. The focus grammar is a
`lelwel` grammar (`crates/aperture-engine/src/grammar.llw`, compiled by that crate's
`build.rs`).

---

## Architecture, in one breath

`lex → parse → typecheck → flatten → reorder` compiles focus text to a **`Plan` IR** (the
fixed contract — an ordered `[Step]`, a scan to iterate or a value to compute); the executor
runs the plan as a **nested loop** (`enumerate` over a frame
stack) against two sorted column families (`keys` = index, `entities` = identity), and can
**suspend to a bytes-only `Cursor` and resume exactly**. Deep dives:
[storage](docs/03-storage-model.md) · [executor](docs/04-executor.md) ·
[resume](docs/05-resume.md) · [codec](docs/02-tuple-codec.md) ·
[types/schema](docs/06-types-and-schema.md) · [compilation](docs/07-compilation.md) ·
[operations](docs/aperture-cli-design.md).

---

## Invariants — DO NOT BREAK

**Full statement, rationale, and guard test for each: [`docs/invariants.md`](docs/invariants.md).**
Know these by number — they are the guardrails every change is checked against.

| # | In one line | Chapter |
|---|-------------|---------|
| [I1](docs/invariants.md#i1)  | Key encoding is order-preserving. | [2](docs/02-tuple-codec.md) |
| [I2](docs/invariants.md#i2)  | Encoding is self-delimiting; `skip` needs no schema. | [2](docs/02-tuple-codec.md) |
| [I3](docs/invariants.md#i3)  | The marker table is frozen on disk. | [2](docs/02-tuple-codec.md) |
| [I4](docs/invariants.md#i4)  | Resume == uninterrupted run (bytes-only cursor). | [5](docs/05-resume.md) |
| [I5](docs/invariants.md#i5)  | A register holds the whole row; fields decode lazily. | [4](docs/04-executor.md) |
| [I6](docs/invariants.md#i6)  | Values never enter the scan hot loop. | [3](docs/03-storage-model.md)/[4](docs/04-executor.md) |
| [I7](docs/invariants.md#i7)  | The executor is a defunctionalised state machine. | [4](docs/04-executor.md) |
| [I8](docs/invariants.md#i8)  | Immutable snapshot per query; released at suspend. | [5](docs/05-resume.md) |
| [I9](docs/invariants.md#i9)  | Hot path is allocation-free per row. | [4](docs/04-executor.md) |
| [I10](docs/invariants.md#i10) | Union discriminants are stable and append-only. | [6](docs/06-types-and-schema.md) |
| [I11](docs/invariants.md#i11) | `FactId` is stable, unique, never reused within a DB. | [3](docs/03-storage-model.md) |
| [I12](docs/invariants.md#i12) | Both column families are written atomically. | [3](docs/03-storage-model.md) |
| [I13](docs/invariants.md#i13) | The DB's schema is embedded and frozen at create. | [6](docs/06-types-and-schema.md) |
| [I14](docs/invariants.md#i14) | A derived bind is a pure function of the fact bindings. | [7](docs/07-compilation.md) |
| [I15](docs/invariants.md#i15) | A DB says which format wrote it; an unreadable one is refused. | [3](docs/03-storage-model.md) |

**Operational invariants `ops-I1`–`ops-I10`** (lifecycle, single-writer, reproducibility,
one-write-funnel) are a **separate namespace** — always written `ops-Ix` — and live in
[`docs/aperture-cli-design.md §1`](docs/aperture-cli-design.md), summarised in the
[registry](docs/invariants.md#operational-invariants-ops-i1ops-i10).

---

## Conventions (essentials — full list in [`docs/conventions.md`](docs/conventions.md))

- **Errors, not panics, on data paths.** Corrupt bytes surface as an `ApertureError` /
  `StoreCodecError` variant, never `unwrap`/`panic` (unwrap only where an invariant makes it
  impossible, with a comment).
- **Record fields are sorted `[(Symbol, T)]` slices everywhere** (`Box<[…]>` owned,
  `Arc<[…]>` shared) — never `HashMap`. Deterministic order is a codec requirement.
- **Ownership signals sharing:** `Box<[T]>` owned-once; `Arc` only at genuine sharing
  boundaries; `ByteView` clones are refcount bumps.
- **Symbols interned; runtime is interner-free** (two-tier `SchemaInterner` + per-query
  `Rodeo`, schema-first resolution).
- **Permissive grammar, narrow later** — reject meaningless constructs at typecheck/flatten
  with clear diagnostics, not in the grammar.

**Anti-patterns** (each breaks a specific invariant — see
[`docs/conventions.md`](docs/conventions.md#anti-patterns-look-reasonable-are-wrong-here)):
materialising a full result set; eager field decode at bind (I5/I9); value fetch in the
scan loop (I6); holding an iterator across a suspend (I8); rewriting `enumerate` as
recursion (I7); writing one column family without the other (I12); renumbering markers or
discriminants after data exists (I3/I10); DNF-expanding disjunction across conjuncts;
reshaping the machine for an "additive" feature; `HashMap` record fields; `unwrap` on
decoded data.

---

## Scope, phases & open decisions

- **Build order and current state:** [`PLAN.md`](PLAN.md). Both sanctioned machine changes are
  **done**: the **`FactRef` marker** (its own marker `0x51` in the codec) and **dynamic
  derivation** ([Phase 6](PLAN.md) — a register holds a `Slot`, a plan's body is a sequence of
  `Step`s, and a derive step is recomputed on resume rather than saved,
  [I14](docs/invariants.md#i14)). Everything else deferred is additive and must not reshape the
  machine. **Stored** derivation — a derived predicate written as facts — is
  [Phase 8b](PLAN.md), gated on the schema DSL: it needs nothing from the machine change, and
  cannot be built before a derived predicate can be *declared*.
- **A constant bind folds.** `X = 42` — and a record of constants to any depth — is substituted
  at every use, taking no register and no step; a plan whose every bind folded has no steps and
  means exactly one row. Nothing in focus lowers a `Step::Derive` yet, so that machinery is
  exercised by hand-built plans; its first producer will be a primitive or a subquery. Do not
  "simplify" it away — its resume behaviour is the expensive thing to get wrong later
  ([chapter 7](docs/07-compilation.md#folding-a-constant-bind)).
- **Additive is not the same as small.** The constructs that parsed and typechecked-as-deferred
  and now compile — `|`, `never`, `!`, subqueries — were **[Phase 6b](PLAN.md)**, and
  **union types** are [Phase 8](PLAN.md) (a union cannot be declared before schemas parse, and
  [I10](docs/invariants.md#i10) freezes its discriminants on disk once one is written). Neither
  reshapes the machine, but disjunction extends the resume `Cursor`, so both carry acceptance
  criteria rather than a bullet. **A negation is a `Step::Test`** — the third step kind, a filter
  that binds nothing, takes no cursor entry, and is re-decided on restore rather than replayed;
  its variables are `reads`, which is the whole of the rule that a negation runs after whatever
  binds them, so `reorder` needed no new kind of constraint. Do not add one. What still draws
  `nyi/negation` is a negated **group** and a generator inside a negation's key — the second is a
  refusal, not a gap: hoisting it out answers differently when it matches nothing. Phase 6b is
  sequenced *after* Phase 6 on purpose: both touch the
  resume token, so edit it once and re-prove [I4](docs/invariants.md#i4) once. (Phase 6 left the
  cursor's *entries* a `Vec<Register>` in the end — only *fact* slots are ever saved, since a
  derive step is recomputed — but it did change what a cursor entry is counted against.) The
  token now also carries a **layout version and a plan fingerprint**, checked before any entry is
  read: entries are paired with levels by order, so without them two same-shaped plans over
  overlapping predicates accept each other's cursors and answer short, silently
  ([chapter 5](docs/05-resume.md)). Interned names are deliberately outside the fingerprint —
  a `Symbol` is per-query, so hashing one would fail a legitimate resume.
- **Unsettled decisions:** [`docs/open-decisions.md`](docs/open-decisions.md) — three. Two from
  comparing the design against Glean: **multiplicity** (arrays *and* one fact per element — Glean
  writes both, deliberately; decide before the Phase 8 schema DSL fixes how schemas are written)
  and **primitives** (arithmetic, string functions, conditionals: not built, not even lexed, not
  ruled out). One that an external audit found asserted but never decided, **gating a phase and
  cheapest to answer before it**: **re-derivation vs I11** (the high-water mark is recovered from
  the `entities` tree, so Phase 8b's O(1) tree drop restarts sequences at 1 and reuses ids that
  dependent predicates still reference). The audit's other two are settled: an **on-disk format
  version** is now [I15](docs/invariants.md#i15), built — a twelve-byte stamp in a `meta`
  keyspace, `codec` and `storage` versioned separately, checked at open, with an unstamped DB
  holding facts **refused** rather than adopted; it makes nothing migratable (I3 still binds
  every DB stamped `codec 1`), it makes a future codec a different number rather than an
  impossibility. And **what a reference is on the way in** is the target fact, nested — see
  below.
- **A reference a producer sends is the whole target fact, not an id** — nested inline to any
  depth, and **interned** at ingest into a `FactId`
  ([chapter 3](docs/03-storage-model.md#interning-a-nested-fact),
  [operations §6](docs/aperture-cli-design.md#6-wire-protocol--the-write-stream)). A producer
  holding an id may send it, so the inbound form is id-or-nested; **stored, a reference is a
  `FactId` and nothing else** — this is transport, never disk. Settled because every id-based
  answer puts a map from each entity to its assigned identity inside the *indexer*, plus an
  emission order respecting it; nesting lets a producer emit what it holds where it stands, and
  makes the write side the same spelling as the read side (`Knows { from = Person { id = 1 } }`
  has compiled since Phase 5). Interning is resolve-or-create, **bottom-up** — a parent's key has
  no bytes until its children have ids — and it is total because a reference *in a key* cannot
  be cyclic. It is not a new rule anywhere: "already there" is `ops-I5`'s silent dedup and
  "disagrees" is its same-key-different-value reject. This is why **Phase 7 is wire-first**
  ([PLAN.md](PLAN.md)): a write stream interns as it arrives, so the file pipeline's
  encode-and-sort-before-ids conflict never takes shape, and 7b answers *where* interning goes
  holding a built primitive.
  Everything the file was originally opened for has settled: intra-row repeated variables
  are **rejected**
  (`nyi/repeated-variable`, Phase 4), `pattern = pattern` is settled — the gate is the **left
  side's shape alone**, and most of what was filed as unification turned out not to be it
  (binding a row a field already named is an **ordering** question `reorder` answers; `X = Y`
  with both bound is a residual on the level that binds later; `X = "a"..` is a **constraint**
  that narrows the level binding `X`) — and cancellation counting rows examined is settled in
  the executor. What is left of `nyi/bind-unification` is a left side that is not a target at
  all — `gen = gen`, `Y.name = X` — which is **pattern-pushing**, not binding.
- **What flatten defers** — a value in no register, matching on a value, an alternation *inside*
  a pattern, an intra-row repeat — each has a code and a corpus entry
  ([chapter 7](docs/07-compilation.md#what-flatten-defers-and-why)). **Both halves of reaching a
  fact through a reference now work**, and they stay distinct in the IR because they are
  different plans: *following* one is a compare against an id already in a register
  (`SeekKeyPart::RegisterFactId`, no store read), *reading through* one is a
  [`Source::Fetch`](docs/04-executor.md#fetching-through-a-reference) level — one point read per
  row of the level above it, binding `predicate_id ++ key` so the fetched fact is an ordinary
  register from there on. The danger the split guards is that a register also holds its own
  row's key bytes; splicing those where an id belongs compares a key against an id and quietly
  matches nothing. `nyi/fact-field` is now only a reference held in a fact's *value*.
  **`Slot` is the single substitution**, and `flatten::resolve` the only function that answers
  *where does this expression's value live* — for a key field, the head, an alias's right side,
  and a record's pieces alike, with a constant as an ordinary arm rather than a parallel path;
  `dereference` sits inside it and answers a reference with the row it names, so a fetch added
  no `Slot` arm. So `Y = X.file` is an **alias**: no register, no step, the same plan as the
  read it names. `nyi/value-bind` now means only *this value is in no register and would have to
  be built*.
- **A bind means one of four things, and flatten decides which** — a row bind (a level), a
  constant fold, an alias, or a **constraint**
  ([chapter 7](docs/07-compilation.md#what-a-bind-can-mean)). Only the first takes a register and
  none is a `Step`. Typecheck checks the left side's shape and the types, and nothing else: it
  used to ask whether a variable had been *mentioned above*, which decided in source order — the
  one order the query might not have used. A constraint (`X = "a"..`) is the one worth knowing by
  shape: it is collected from the whole body before an order is chosen, exactly as the constant
  fold is, and applied by whichever level **captures** the variable, so `test.Name X; X = "a"..`
  is the same range seek `test.Name "a"..` is. Applying it afterwards as a residual would answer
  the same rows and read the whole predicate to find them — do not "simplify" it into
  `apply_compares`.
- **A denial is `!=`, and it is never a seek.** `X != "a".."` is a fifth statement
  (`QueryStmt::Deny`, its own token) and the negative of the constraint alone
  ([chapter 7](docs/07-compilation.md#denying-a-value)). Where it goes is the constraint's story
  unchanged — collected from the whole body, keyed by variable, a pure read that binds nothing —
  but a prefix is *one* run of the key order and its negation is the two runs either side, so it
  filters however it is written. Do not look for a sargeable form: the two polarities are held in
  separate collections precisely so a capture cannot be handed one to narrow itself by. `!` and
  `!=` stay different syntax for different questions — `!` says no such row exists and takes a
  `Step::Test`; `!=` says this row's field does not look like that and takes a residual
  (`NotEqConst`/`NotPrefix`). A new `ResidualOp` needs a **distinct fingerprint tag** or two
  plans differing only in polarity accept each other's resume cursors.
