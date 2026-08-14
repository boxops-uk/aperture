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
struct Cursor {
    version: u16,           // which cursor layout these bytes are in
    plan: PlanFingerprint,  // which plan produced them
    entries: Vec<Entry>,    // one entry per open loop *level*
}
struct Entry { source: usize, row: Register }
```

One entry per **level**, not per plan step: a [`Step::Derive`](04-executor.md#the-plan-ir) holds
no row and contributes nothing, because it is recomputed on restore instead
([I14](invariants.md#i14)), and a [`Step::Test`](04-executor.md#the-plan-ir) — a negation — holds
none either, because it binds nothing and its verdict is re-decided rather than replayed. A
suspend only ever happens at a full row, so the cursor names *every* level — which is what makes
resume's replay-by-order sound, and why its length check is `!= plan.levels()` rather than a
bound.

That two of the three step kinds cost the token nothing is the [query-surface
note](query-surface.md)'s finding rather than a coincidence: a construct pays cursor work only
if it can be **mid-flight when a row is handed out**, and a filter finishes within the
evaluation of one row. Disjunction is the one construct so far that is not, which is why it and
not negation added a field.

**An entry says which of the level's [sources](04-executor.md#the-plan-ir) produced the row**,
and that is not recoverable from the row itself. Alternatives can overlap, so one fact is
reachable from more than one of them; and the sources *after* the live one have not run yet.
Resuming into the wrong alternative therefore re-emits rows and skips rows at once. A
single-source level — every level focus compiles today — says `0`, so this is the shape the
token had all along with the part that was implied now written down.

For each level it saves that level's `current` row — but **detached**: `ByteView`
bytes are copied to owned memory (`to_detached`) so the cursor references no shared buffer,
no iterator, no snapshot ([I9](invariants.md#i9)'s "copy out only at escape boundaries").
The saved `Register` is `{ fact_id, key bytes }` — enough to find the row again and to
prove it's the same one.

That's the entire token: two stamps and one detached row per level. No open cursors, no plan
pointer beyond the fingerprint and what the caller re-supplies, no snapshot — nothing in it
that a socket could not carry.

**A resume token is client-held on both sides of the comparison.** Glean's continuation is
opaque bytes handed back to the caller and passed in again
(`glean/if/glean.thrift:397-406`), self-contained down to the compiled program —
`restartCompiled` takes no compiled query, because the program comes out of the blob
(`glean/hs/Glean/RTS/Foreign/Query.hsc:136-174`) — and so resumable in a *different process*.
Neither system keeps a server-side cursor object alive between pages. What differs is what the
token **weighs**, itemised in
[chapter 4](04-executor.md#why-a-state-machine-and-not-recursion--i7), and what it **proves**,
which is the rest of this chapter.

**A cursor says which run it belongs to**, and it has to, because the entries alone cannot.
They are paired with the plan's levels *by order*: without the two stamped fields, two plans of
one shape over overlapping predicates accept each other's cursors, and the per-level `fact_id`
check is all that stands between that and a wrong answer — a check that passes whenever the
saved key exists in the other plan's scan too. The failure is a **silently short answer**, not
an error, which is why both are checked before an entry is read and before the empty-cursor
shortcut that restarts a run:

| check | catches | error |
|---|---|---|
| `version` | a cursor from a build where an entry meant something else | `CursorVersion` |
| `plan` fingerprint | a cursor from another plan | `CursorPlan` |
| entry count | a forged length, exactly | `CursorPlanMismatch` |
| `source` index | an alternative this level does not have | `CursorSourceOutOfRange` |
| `fact_id` | a saved key that is no longer the row it named | `BadResumeKey` |

Widening to narrowing, and each one earns its place: the version governs how to read the rest,
the fingerprint is a 2⁻⁶⁴ bet where the entry count is certain, and the count says *how* a
cursor is wrong where the fingerprint only says *that* it is. The order is also what keeps the
three checks below it reachable — a test that replays one plan's cursor against another now has
to re-stamp it first, which is what a forged wire cursor is.

**What the fingerprint covers.** FNV-1a over the plan's structure, written out explicitly
rather than derived — stability is the whole requirement, and `DefaultHasher` is free to change
between Rust releases. Interned names are deliberately *not* hashed: a `Symbol` is an index into
a per-query interner, so the same query compiled in another process names its head fields with
different numbers, and hashing those would fail a legitimate resume — strictly worse than the
hole it closes. The consequence, stated rather than hidden: two plans differing only in what
their head fields are *called* fingerprint the same. Neither positions a scan.

**The wire form is still a seam.** `Cursor` is in-process: the two stamps are fields and the
checks are real, but there is no encoder and **no checksum**. Glean's continuation carries a
version plus an FNV-1 checksum over the blob and the return type
(`glean/db/Glean/Query/UserQuery.hs:1258-1283`); the checksum is the half that cannot be
written before the blob exists, and the transport-codec sketch kept in `src/focus.rs` is where
it goes. The two versions are on **separate counters** on purpose: this one says what is in
flight, the [format stamp](03-storage-model.md#the-format-stamp-i15) says what is on disk, and
a cursor is checked against the build reading it rather than against a database.

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

At a **test** step — a [negation](04-executor.md#the-plan-ir) — nothing at all happens except
marking it produced. It binds no register, so there is no state to rebuild; the row it passed
was handed to the consumer before the suspend; and the base is frozen (`ops-I2`), so a second
probe could only agree with the first. Re-running it could therefore never *correct* anything
and could only fail spuriously, against a database this token cannot detect it is not looking
at. Marking it produced is not optional in the same way: without the bit the machine arrives
from below, probes, passes, and ascends into a row it has already emitted.

> **The recompute rule.** In an immutable DB a store read is a pure function of its inputs, so
> anything whose result is determined by the bindings and the frozen base may be **recomputed
> on restore, or skipped, instead of saved**. Derived binds ([I14](invariants.md#i14)) are the
> special case that named it; a fetch by fact id and a filter's verdict are the rest. What the
> cursor holds is what a *scan* cannot recompute: its position.

**A [`Fetch`](04-executor.md#fetching-through-a-reference) level takes no saved position, and
still consumes an entry.** Step 1 has nothing to do for it: the row is whichever one the
reference names, so re-binding the *outer* registers is what puts the level back where it was
— the recompute rule, arrived at from the other direction. What it keeps is step 3, and that
is not ceremony: the check is what catches a cursor replayed against a store where the
reference now names another fact, which is the one way this level can silently move. Saving an
ordinary entry rather than nothing also keeps one rule for the whole token — one entry per
level, paired by order — instead of a second rule about which levels count.

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
the wrong row**: skipped rows and duplicated rows, no error. Key→id can fail, so it can catch
that; id→key is a tautology and can never catch anything.

**But it is a detector, not a guarantee, and the difference matters.** Key→id catches a
wrong DB only when the two DBs' key→id mappings *differ at the saved key*. They often do —
and then the replay stops with `BadResumeKey` rather than answering. They need not. Ids are
allocated per predicate in write order ([I11](invariants.md#i11)), so two DBs built from the
same facts in the same order agree on a **prefix** of every predicate's mapping, and a cursor
saved inside that prefix resumes clean and then goes on to emit the *other* DB's rows. That is
not an exotic case: it is what an incremental rebuild looks like, which is the likeliest way
for a stale cursor to meet a DB it was not taken from.

So the honest statement is: key→id is strictly more than Glean has, and strictly less than DB
identity. **The thing that actually closes it is a DB fingerprint in the token** — the third
stamp, next to the version tag and the plan fingerprint the cursor now carries (above). Those
two answer "which build" and "which query"; neither answers "which database", and this is the
case that needs it. Do not read this paragraph as an argument that the stamp is unnecessary —
it is the argument that it is, because the check it would be redundant with only *usually*
fires.

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

**The disjunction half of that is now paid.** The per-branch discriminant this section was
kept for is the `source` on an entry, and [I4](invariants.md#i4) is re-established over
generated plans holding a multi-source level, with a census asserting the battery takes a cut
*while a later alternative is live* — the cut that a source index is the only defence against.
What it cost is the measure of the trade: in a register machine a branch's return address is
just another register the continuation was saving anyway, and here it was a token change, a
resume change and a battery extension ([chapter
4](04-executor.md#why-a-state-machine-and-not-recursion--i7)). The remaining extension is a
branch that *contains a join*, whose entry holds a nested cursor rather than a row
([the query-surface note](query-surface.md), [`PLAN.md`](../PLAN.md) Phase 6b).

**A cursor is untrusted, and both new ways to malform one are errors rather than panics.** An
entry naming a source the level does not have is `CursorSourceOutOfRange` — the level-count
check one level down, since two plans of the same shape can disagree about how many
alternatives a level has. A saved position outside the range of the source it is replayed into
is `BadResumeKey`, checked where a saved position becomes a scan bound so that it covers every
`FactStore` at once: unchecked, `lo > hi` is a **panic** inside the store's range, and a `lo`
below the prefix silently re-scans rows the level already emitted.

---

## Invariants owned by this chapter

| # | Statement | Guard test |
|---|-----------|------------|
| [I4](invariants.md#i4) | Resume reproduces an uninterrupted run exactly. | `exec::resume_equals_uninterrupted` (tier-3, every cut point) |
| [I8](invariants.md#i8) | Immutable snapshot per query; released at suspend. | `store::snapshot_released_at_suspend` (needs fjall) |

---

> **Reading path:** [← 4. The executor](04-executor.md) · **5. Suspend & resume** · [6. Types & schema →](06-types-and-schema.md)
