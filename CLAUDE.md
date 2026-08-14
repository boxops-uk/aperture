# Aperture — working contract

**Aperture** (the product: *Aperture DB*) is an embedded, immutable **fact database**;
**focus** is its typed, Datalog-flavoured query and schema language (the `src/focus/`
module). This file is the **working contract** loaded every session — how to work here, the
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

**Module map.** `src/focus/` is the live engine + language — all new work lands here.
`src/main.rs` is the `aperture` shell: it compiles and runs what you type against a real store
seeded with a **code index** (files → modules → declarations → references), written through the
fact API; `:plan` shows the plan. The index is a real one — `example/` holds a small Python
corpus, the `ast`-based indexer that reads it and the JSON it emits, which the shell compiles
in and writes as facts at startup ([`example/README.md`](example/README.md)). Regenerate with
`python3 example/index.py`. Keep logic out of it — the plan renderer it needed lives in
`focus::print`. **`focus::fact` is how a fact is written by hand**: a well-typed value whose
key fields are named, resolved against the schema (`FjallDb::put`), because `put_fact` takes
bytes and three of its preconditions fail silently — see
[chapter 3](docs/03-storage-model.md#writing-a-fact-by-hand). `focus::fixture` is the fixture
database the corpus and the batteries share.
`src/focus.rs` is the module list plus a commented-out graveyard (~20 live lines; only the
transport-codec sketch is worth keeping). See [chapter 1](docs/01-concepts.md).

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
cargo clippy --all-targets -- -D warnings
cargo fmt
```

`fjall` is the storage backend; the `FactStore` trait (`focus::plan`) is the seam, with an
in-memory `MemStore` (`focus::mem_store`) **for tests only**. The focus grammar is a
`lelwel` grammar (`src/focus/grammar.llw`, compiled by `build.rs`).

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
- **Additive is not the same as small.** The constructs that parse and typecheck-as-deferred
  but have no engine — `|`, `never`, `!`, subqueries — are **[Phase 6b](PLAN.md)**, and
  **union types** are [Phase 8](PLAN.md) (a union cannot be declared before schemas parse, and
  [I10](docs/invariants.md#i10) freezes its discriminants on disk once one is written). Neither
  reshapes the machine, but disjunction extends the resume `Cursor`, so both carry acceptance
  criteria rather than a bullet. Phase 6b is sequenced *after* Phase 6 on purpose: both touch the
  resume token, so edit it once and re-prove [I4](docs/invariants.md#i4) once. (Phase 6 left
  `Cursor` a `Vec<Register>` in the end — only *fact* slots are ever saved, since a derive step is
  recomputed — but it did change what a cursor entry is counted against.)
- **Unsettled decisions:** [`docs/open-decisions.md`](docs/open-decisions.md) — two, both from
  comparing the design against Glean: **multiplicity** (arrays *and* one fact per element — Glean
  writes both, deliberately; decide before the Phase 8 schema DSL fixes how schemas are written)
  and **primitives** (arithmetic, string functions, conditionals: not built, not even lexed, not
  ruled out). Everything the file
  was originally opened for has settled: intra-row repeated variables are **rejected**
  (`nyi/repeated-variable`, Phase 4), the `pattern = pattern` *scope* is settled — split across
  typecheck and flatten, and one case of it turned out never to have been unification at all
  (binding a row a field already named is an **ordering** question, which `reorder` now answers;
  the feature itself stays deferred) — and cancellation counting rows examined is settled in
  the executor.
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
