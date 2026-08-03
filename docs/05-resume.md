# 5. Suspend & resume

> [Aperture design book](../README.md) · [← 4. The executor](04-executor.md) · **Chapter 5** · [6. Types & schema →](06-types-and-schema.md)

This is the subsystem where a subtle bug is catastrophic and invisible. A query can
**suspend** mid-stream (for backpressure, a portal going idle, a fair-scheduling yield) and
later **resume** — and the resumed run must produce *exactly* the rows an uninterrupted run
would, in the same order, with no duplicates and no skips. This chapter explains how a
resume token that is **only bytes** achieves that, and why it's safe.

It builds directly on [the executor](04-executor.md) — in particular
[I7](invariants.md#i7), the defunctionalised state machine, which is what makes any of this
possible. Code: `src/focus/iter.rs`.

---

## The two invariants at stake

> **I4 — resume == uninterrupted run.** Suspending and resuming reproduces the exact row
> sequence of an uninterrupted run — no duplicates, no skips — including across join
> cross-product boundaries. The `Cursor` is **bytes only** (one saved key per active
> level); it holds **no live iterators and pins no LSM snapshot**.
>
> *Guard:* `exec::resume_equals_uninterrupted` (tier-3, model-based) at **every** cut
> point, for 1-/2-/3-level plans, over schema-first `(plan, store)` pairs. Run against both
> `MemStore` and fjall. **The executor's headline acceptance gate.**

> **I8 — immutable snapshot per query; released at suspend.** fjall iterators pin a read
> snapshot; **drop the executor at suspend** to release it. A held `Iter`/`Slice` keeps LSM
> blocks (and a whole superseded generation) alive.
>
> *Guard:* `store::snapshot_released_at_suspend` — a drop-probe asserts no snapshot survives
> a suspend. **Untestable on `MemStore`** (its scan pins nothing) ⇒ needs the fjall store
> (why fjall is pulled forward to Phase 1 in [`PLAN.md`](../PLAN.md)).

These two are a pair: I8 says "don't keep anything alive across a suspend," which *forces*
the cursor to be pure bytes, and I4 says "and yet reproduce the run exactly." The tension
between them is the whole design problem.

---

## The `Cursor` — bytes, nothing else

On suspend, the executor builds a `Cursor` from the frame stack:

```rust
struct Cursor(Vec<Register>);   // one detached Register per *active* level
```

For each open level it saves that level's `current` row — but **detached**: `ByteView`
bytes are copied to owned memory (`to_detached`) so the cursor references no shared buffer,
no iterator, no snapshot ([I9](invariants.md#i9)'s "copy out only at escape boundaries").
The saved `Register` is `{ fact_id, key bytes }` — enough to find the row again and to
prove it's the same one.

That's the entire token. No open cursors, no plan pointer beyond what the caller re-supplies,
no snapshot. It can be written to a socket and read back an hour later.

---

## How resume reconstructs the run

`Executor::resume(store, plan, cursor)` rebuilds machine state so the next `enumerate` call
continues exactly where the last one stopped:

For each saved level, in order:

1. **Re-open the level's scan** starting `Included(saved_key)` — i.e. seek straight to the
   row the level was sitting on.
2. **Pull one row and re-bind** it into the registers, restoring the variable bindings the
   deeper levels depend on.
3. **Integrity check:** the re-read row's `fact_id` **must equal** the saved `fact_id`. If
   not → `BadResumeKey`.

Then set `depth` to the innermost saved level and hand back to `enumerate`, which — because
that innermost frame's scan is already open and positioned — calls `next()` and thereby
**advances past** the last-emitted row. Outer levels are *not* advanced; they stay pinned on
their saved rows and only advance when the inner level exhausts and the machine backtracks.
That is precisely the nested-loop semantics of an uninterrupted run, reconstructed from
bytes.

### Why the `fact_id` check is the linchpin

A bytes-only cursor is only safe if a saved key still means the same fact. In an immutable
Complete DB it always does. But the cursor might outlive a rebuild, be replayed against a
shifted store, or hit a bug — so resume **verifies** rather than trusts: re-open at the
saved key, and confirm the row found there has the saved `fact_id`. This is what makes
"snapshot-free, bytes-only" *safe* rather than merely *cheap*, and it's why
[I11](invariants.md#i11) (fact-ids are stable and never reused) is a prerequisite —
detailed in [chapter 3](03-storage-model.md).

If the saved key no longer resolves to the saved fact, that's not silent — it's
`BadResumeKey`, a real error on a data path ([conventions](conventions.md)).

---

## Why this is so heavily tested

Resume bugs are the archetypal *invisible* bug: the code compiles, happy-path queries
return the right rows, and only a specific suspend-at-exactly-this-boundary schedule
produces a duplicate or a skip. Inspection does not find these. So the guard is
**model-based (tier-3)**:

- **Model:** run the plan to completion with no interruption, collect the rows. Obviously
  correct.
- **System under test:** run the same plan but suspend according to a generated
  **interruption schedule** (suspend after row 1, or row 5, or at every cut point), resume,
  and collect.
- **Property:** the two row sequences are identical, for **every** schedule.

The inputs are generated **schema-first, valid-by-construction** (draw a schema → draw
conforming facts → draw a valid query), so every case is meaningful and shrinks to a
minimal counterexample. This exact technique caught the historical "resume duplicates a
row" bug. Full methodology in [chapter on testing](testing.md); the acceptance bar is 1-,
2-, and 3-level plans at every cut point, against both `MemStore` and fjall (I8 needs the
latter).

---

## Suspend vs cancel vs terminal unwind

Three ways a run can stop early — don't confuse them:

| Kind | Trigger | Resumable? | Snapshot |
|------|---------|-----------|----------|
| **Suspend** (`Stream::Suspend`) | consumer yields (backpressure, portal idle) | **Yes** — returns a `Cursor` | released (I8) |
| **Cancel** | `CancellationToken` tripped (polled every ~4096 rows) | No | released |
| **Terminal unwind** | deadline / rows-scanned cap | No | released |

**Suspend is voluntary and resumable**; the other two are terminal. All three must release
the snapshot (I8) — a cancelled query that leaves an iterator alive is as much a leak as a
suspended one. Cancellation is **cooperative and synchronous**: the scan loop polls a flag
every `CANCELLATION_STRIDE` rows. The executor is deliberately **not `async`** — its work
is blocking CPU/IO, and making it async would only colour the whole codebase for no benefit
(see [scope](../PLAN.md) and [conventions](conventions.md)).

---

## The seam to the wire

`Iteratee::Suspended { cursor }` + the byte `Cursor` are precisely the primitives the
connection layer needs: a **portal** is a suspended query holding a cursor; **backpressure**
is the consumer returning `Suspend` when its output queue is full; **cross-suspend safety**
is I8 (nothing pinned while idle). The wire protocol chapter builds portals, chunked result
streaming, and per-stream cancellation directly on this — see
[Operations](aperture-cli-design.md).

Later features extend the cursor without reshaping it: **disjunction** (`|`) adds a
per-branch discriminant to the token; keep the `Cursor` type extensible to that (see
[scope](../PLAN.md) and [open decisions](open-decisions.md)).

---

## Invariants owned by this chapter

| # | Statement | Guard test |
|---|-----------|------------|
| [I4](invariants.md#i4) | Resume reproduces an uninterrupted run exactly. | `exec::resume_equals_uninterrupted` (tier-3, every cut point) |
| [I8](invariants.md#i8) | Immutable snapshot per query; released at suspend. | `store::snapshot_released_at_suspend` (needs fjall) |

---

> **Reading path:** [← 4. The executor](04-executor.md) · **5. Suspend & resume** · [6. Types & schema →](06-types-and-schema.md)
