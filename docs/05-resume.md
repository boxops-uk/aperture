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
> *Guard:* `store::snapshot_released_at_suspend` — a drop probe over the store handle and
> every scan it opened, plus fjall's own open-snapshot count, assert nothing survives a
> suspend, a completed run, a cancellation, or an error unwind. **Untestable on `MemStore`**
> (its scan pins nothing) ⇒ needs the fjall store (why fjall is pulled forward to Phase 1 in
> [`PLAN.md`](../PLAN.md)).

**"Drop the executor" is enforced by the signature, not by a rule.** `Executor::enumerate`
takes `self` **by value**, so a run cannot outlive its own return: done, suspend, cancel and
error unwind all consume the executor and with it the frame stack and the store handle. There
is no shape of caller — however it stores or reuses the executor — that can park a live
iterator across an idle portal. To continue, rebuild with `Executor::resume(store, plan,
cursor)` against a *fresh* snapshot; that is exactly what the wire path does when a portal
wakes up.

The alternative is a rule kept by hand, and the reference implementation shows what that looks
like: Glean's cross-suspend safety rests on a comment — "Don't keep any pointers in local
registers across `Suspend`, because they won't be valid when we resume"
(`glean/db/Glean/Query/Codegen.hs:120-129`) — with a live workaround for it in the array
generator, which re-reads its output buffer every iteration because the previous one may have
suspended (`glean/db/Glean/Query/Codegen.hs:879-887`). A rule like that is broken by whoever
does not read the note. A signature cannot be.

I4 and I8 are a pair: I8 says "don't keep anything alive across a suspend," which *forces*
the cursor to be pure bytes, and I4 says "and yet reproduce the run exactly." The tension
between them is the whole design problem.

**Read I8 as two claims, because only the first is unusual.** The first is a **per-query
immutable view**: `FjallDb::reader` takes exactly one `fjall::Snapshot` and every scan of that
query reads through it, so a run cannot half-see a write that landed under it. Glean has no
equivalent — `GetSnapshot` appears nowhere in `glean/rocksdb/`, and each `seek` opens a fresh
iterator at whatever superversion is current — so Aperture's intra-page isolation is the
*stronger* of the two. The second claim, releasing it at suspend, Glean also satisfies, by the
trivial route of having nothing to release. So the goal I8 exists for stands exactly as
argued — **an idle portal must pin no LSM generation** — but Aperture has to arrange it
deliberately *because* it holds a real snapshot while running, not because anyone else fails
to.

---

## The `Cursor` — bytes, nothing else

On suspend, the executor builds a `Cursor` from the frame stack:

```rust
struct Cursor(Vec<Register>);   // one detached Register per loop *level*
```

One entry per **level**, not per plan step: a [`Step::Derive`](04-executor.md#the-plan-ir) holds
no row and contributes nothing, because it is recomputed on restore instead
([I14](invariants.md#i14)). A suspend only ever happens at a full row, so the cursor names
*every* level — which is what makes resume's replay-by-order sound, and why its length check is
`!= plan.levels()` rather than a bound.

For each level it saves that level's `current` row — but **detached**: `ByteView`
bytes are copied to owned memory (`to_detached`) so the cursor references no shared buffer,
no iterator, no snapshot ([I9](invariants.md#i9)'s "copy out only at escape boundaries").
The saved `Register` is `{ fact_id, key bytes }` — enough to find the row again and to
prove it's the same one.

That's the entire token. No open cursors, no plan pointer beyond what the caller re-supplies,
no snapshot — nothing in it that a socket could not carry.

**A resume token is client-held on both sides of the comparison.** Glean's continuation is
opaque bytes handed back to the caller and passed in again
(`glean/if/glean.thrift:397-406`), self-contained down to the compiled program —
`restartCompiled` takes no compiled query, because the program comes out of the blob
(`glean/hs/Glean/RTS/Foreign/Query.hsc:136-174`) — and so resumable in a *different process*.
Neither system keeps a server-side cursor object alive between pages. What differs is what the
token **weighs**, itemised in
[chapter 4](04-executor.md#why-a-state-machine-and-not-recursion--i7), and what it **proves**,
which is the rest of this chapter.

**The wire form is a seam, not a fact yet.** `Cursor` is an in-process `Vec<Register>`: no
encoder, **no version tag, no checksum**. The level-count check above is also the *only* plan
identity it carries — `CursorPlanMismatch` catches a wrong length, not a wrong plan — and
entries are then paired with scan steps by order, so two same-shaped plans over overlapping
predicates accept each other's cursors, with the `fact_id` check below the only thing between
that and a wrong answer. Glean closes both holes with two fields, which is the measure of what
closing them costs: its continuation carries a version plus an FNV-1 checksum over the blob and
the return type (`glean/db/Glean/Query/UserQuery.hs:1258-1283`). A version field and a plan
fingerprint are the cheap part to copy when the encoder lands, and the transport-codec sketch
kept in `src/focus.rs` is where it goes.

---

## How resume reconstructs the run

`Executor::resume(store, plan, cursor)` rebuilds machine state so the next `enumerate` call
continues exactly where the last one stopped:

One forward walk over the plan's steps — **re-bind the fact-slots, recompute the
value-slots.** At a scan step, consuming the next saved row in order:

1. **Re-open the level's scan** starting `Included(saved_key)` — i.e. seek straight to the
   row the level was sitting on.
2. **Pull one row and re-bind** it into the registers, restoring the variable bindings the
   deeper levels depend on.
3. **Integrity check:** the re-read row's `fact_id` **must equal** the saved `fact_id`. If
   not → `BadResumeKey`.

At a derive step, recompute its value into its slot. Nothing is consumed from the cursor, and
nothing needs to be: purity ([I14](invariants.md#i14)) is what makes recomputing equivalent to
having saved it.

Then set `depth` to the innermost step and hand back to `enumerate`, which — because
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

**The *direction* of that check is where the safety lives.** Aperture saves `(key, fact_id)`
and verifies the row re-read at the key. Glean runs it the other way — save the id, then
`factById(id)` → key → `seek(…, restart)` (`glean/rts/query.cpp:710-735`) — which is a
self-consistency check that cannot fail once the id resolves at all. Its token carries no `Repo`
field either (`glean/if/glean.thrift:397-406`), so a continuation replayed against a *different*
DB of the same schema finds a valid fact of the right type at that id and **silently resumes at
the wrong row**: skipped rows and duplicated rows, no error. Key→id catches that case; id→key
cannot see it.

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

**The reference implementation does not hold this property** — which is the strongest available
argument for I4 owning a mechanical guard rather than a review rule. Glean's per-query
result-dedup set lives in the stack-local query executor and is **not** part of the
continuation (`glean/rts/query.cpp:181-189,240,427-436`), so a query whose rows can yield the
same fact id twice is deduplicated *within* a page and not across one: the paged run and the
uninterrupted run differ observably, in results, in production. A property tested at every cut
point is what catches that class of thing before it ships; nothing about it is visible to
inspection.

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

**Cancellation converges with Glean; suspend depth does not.** Glean polls in the same place
and counts the same thing — a timeout/interrupt check at the top of `next`, sampled every 100th
call (`glean/rts/query.cpp:26-28,304-319`) — i.e. rows **examined**, independently reaching the
conclusion [open decisions](open-decisions.md) records for Aperture. But Glean's cancellation
is **global only**: `interruptRunningQueries` aborts every query started before the interrupt.
A per-query `CancellationToken` is a facility Aperture has and Glean does not.

The gap runs the other way on suspend *depth*. Glean carries a second suspend site *inside*
the generator loop, so a `max_time_ms` timeout hands back a **resumable** continuation even
with zero results (`glean/db/Glean/Query/Codegen.hs:1017-1024`). Aperture cannot: a deadline
or a rows-scanned cap unwinds terminally (the table above), and a `Cursor` of one saved row
per level has no analogue of Glean's per-iterator `first` bit — the flag saying whether the
position it names has already been consumed — so it cannot *represent* a mid-descent position
at all. A resumable time slice is that bit plus the [I4](invariants.md#i4) proof that resuming
from it is exact: deferred, with the seam in the right place rather than done.

---

## The seam to the wire

`Iteratee::Suspended { cursor }` + the byte `Cursor` are precisely the primitives the
connection layer needs: a **portal** is a suspended query holding a cursor; **backpressure**
is the consumer returning `Suspend` when its output queue is full; **cross-suspend safety**
is I8 (nothing pinned while idle). The wire protocol chapter builds portals, chunked result
streaming, and per-stream cancellation directly on this — see
[Operations](aperture-cli-design.md).

Later features extend the cursor without reshaping it: **disjunction** (`|`) adds a
per-branch discriminant to the token; keep the `Cursor` type extensible to that
([`PLAN.md`](../PLAN.md) Phase 6b owns it, sequenced immediately after the `Register → Slot`
promotion of Phase 6 so the token — and [I4](invariants.md#i4) with it — is settled once
rather than twice). That work has no counterpart in a register machine, where a branch's return
address is just another register the continuation was saving anyway: it is the recurring price of
a token this small ([chapter 4](04-executor.md#why-a-state-machine-and-not-recursion--i7)).

---

## Invariants owned by this chapter

| # | Statement | Guard test |
|---|-----------|------------|
| [I4](invariants.md#i4) | Resume reproduces an uninterrupted run exactly. | `exec::resume_equals_uninterrupted` (tier-3, every cut point) |
| [I8](invariants.md#i8) | Immutable snapshot per query; released at suspend. | `store::snapshot_released_at_suspend` (needs fjall) |

---

> **Reading path:** [← 4. The executor](04-executor.md) · **5. Suspend & resume** · [6. Types & schema →](06-types-and-schema.md)
