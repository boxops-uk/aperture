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
- The build sequence: [`PLAN.md`](PLAN.md)

**Module map.** `src/focus/` is the live engine + language — all new work lands here.
`src/lens/` is a superseded first attempt (not compiled) kept only as a reference to
re-implement into `focus`, then delete file-by-file. `src/focus.rs` is a commented-out
graveyard (~10 live lines; only the transport-codec sketch is worth keeping). See
[chapter 1](docs/01-concepts.md).

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
fixed contract); the executor runs the plan as a **nested loop** (`enumerate` over a frame
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

- **Build order and current state:** [`PLAN.md`](PLAN.md). Two constructs are deliberate
  machine changes (not additive) — **derived facts** and the **`FactRef` marker** — each
  with its own phase; everything else on the deferred list is additive and must not reshape
  the machine.
- **Unsettled decisions:** [`docs/open-decisions.md`](docs/open-decisions.md)
  (`pattern = pattern` scope; intra-row repeats). Note: the `FactRef` marker is **resolved**
  (own marker `0x51`, already in the codec) — earlier "open decision" framing is obsolete.
