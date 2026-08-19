---
title: Executor & resume
description: The plan IR, the register file, the frame stack, the one driver loop — and the bytes-only cursor that lets a query stop and pick up exactly where it left off.
---

The back end is small on purpose: one plan shape, one driver loop, three step kinds, and a
resume token that fits in a socket.

## The plan IR

```rust
struct Plan {
    nvars: usize,       // the size of the register file
    body: Box<[Step]>,  // ordered: [0] outermost … [n-1] innermost
    head: Project,      // how to build each output row
}

enum Step {
    Level(Level),         // 0..N rows: a loop level
    Derive(DerivedBind),  // exactly 1 row: a pure computation
    Test(Test),           // 0..1 rows, binds nothing: a filter
}

struct Level {
    sources: Box<[Source]>,  // alternatives, tried in order and concatenated
    binds: Box<[Address]>,   // registers this level fills from the matched row
}

enum Source {
    Seek  { access: Access, residuals: Box<[Residual]> },
    Fetch { reference: Address, path: FieldPath,
            predicate_id: PredicateId, residuals: Box<[Residual]> },
}
```

A query **is** a nested loop, and the order of `body` is the loop nesting. It is one ordered
sequence because `reorder` produces one order; two collections joined by an index would be two
sources of truth for one ordering with nothing to say which wins.

:::warn `body.len()` is not the number of loops
It counts *steps*. `Plan::levels()` counts loop levels, and a `Cursor` holds one row per
**level**. The two were the same number before derive steps existed, and the cursor's length
check said `>` rather than `!=` as a result — which let a short cursor half-replay a plan and
carry on from the wrong place.
:::

### The count is the construct

A level's source count is what distinguishes three language features:

| Sources | Means | Cost in the driver |
|---|---|---|
| 0 | `never` — the empty relation | Exhausted the moment it is entered |
| 1 | An ordinary scan | Every level sigla compiles today |
| N | A disjunction, one branch per source | Tried in order, rows concatenated |

They are one node rather than three because the driver's job is identical in all three — open
a source, drain it, move to the next, back up when there is no next. So `never` needed no arm
of its own.

**Residuals belong to the source, not the level**, because a residual is a path into a row and
two sources are two key layouts: a path that names a field of one names different bytes, or
none, in the other. `binds` stays on the level, because every alternative binds the same
variables.

### Access, seek, splice

An `Access` is a `predicate_id` plus a `SeekKey`, and the seek key is built from two kinds of
piece:

- **constant bytes** — a literal or a folded constant, encoded at compile time;
- **a splice** — bytes copied from a register an *earlier* level bound.

Splicing is how a join narrows the inner scan to rows matching the outer row. There is no join
operator; there is a seek key with someone else's bytes in it.

### A test is not a level

```rust
enum Test {
    Absent(Box<[Source]>),  // the row survives iff *no* source produces a row
}
```

`Absent` is negation. Each source is drained only to its **first** row — the question is
whether a witness exists, not how many — so a negation over a predicate holding a million
matching rows reads one of them. It opens each source, asks for one row, and **closes it again
before returning**, so a negation holds no iterator between probes.

Neither of its outcomes is new to the machine: passing is ascending with the registers
untouched, failing is the same backtrack an exhausted level does. That is why it needed no new
frame kind and no new direction.

:::note The architectural rule that keeps this small
A new construct may add a `Source`, a `Test`, a residual operator or a computed-value arm. **It
may not add a `Step`.** A step is a case in the driver *and* a case in the cursor *and* an
obligation to re-prove that resume is exact — and "additive" has never been true of one. A test
asserting `Step` has exactly three variants is the cheapest guard in the project.
:::

### Fetching through a reference

A `Source::Fetch` is how a query reads *through* a reference rather than following one. It
takes the reference out of a register, does one point read, and binds the fetched fact as an
ordinary register from there on — so `X.name where test.Ref {of = X}` becomes two levels, and
everything downstream treats the second one like a scan.

One point read per row of the level above it. Two reads of one reference are one fetch, because
a second level would read the same row again and could never disagree with the first.

## The register file, and the row–slot model

```rust
enum Slot {
    Fact(Register),  // a stored row
    Value(Value),    // a derived bind's output
}

struct Register { fact_id: FactId, bytes: ByteView }  // bytes = predicate_id ++ key
```

The two kinds are separated **at the type level** rather than unified behind "some bytes",
because splicing a value where a fact id belongs — or the reverse — compares two different
encodings and quietly matches nothing. Reading a slot names the kind it wants, and a mismatch
is a reported error rather than a panic: flatten cannot emit one, but a plan arriving off the
wire can.

:::invariant I5 — a register holds the whole row
The *field* a variable denotes lives in the **plan**, not the register. So a generator binding
N variables is N refcount bumps on the same row — no per-field decode at bind time. Fields
decode lazily, at read and projection sites only.

*Guard:* `exec::bind_is_refcount_not_decode` — a decode-counting probe asserts that binding N
variables triggers zero field decodes.
:::

Why it must be that way: at bind time you do not yet know which fields will be read. A row may
be bound and then discarded when an inner loop finds no match, so decoding eagerly does work
that is usually thrown away and allocates per field. Holding the whole row keeps bind O(1) and
the hot path allocation-free.

### The frame stack and the field-offset cache

One frame per level: `{ scan, current, field_offsets }`. The offsets are an **inline
fixed-capacity array** that memoises where each field ends as `skip` walks them — the first
access to field *k* walks `0..=k` and caches the boundaries, later accesses are a lookup, and
fields beyond the cache's capacity are walked on demand and not cached. It never spills to the
heap.

:::invariant I9 — the hot path is allocation-free per row
Reused scratch buffers, refcount-bump clones, inline offset caches that never spill. Copy out
only at escape boundaries: a suspend, and a string or bytes projection.

*Guard:* `exec::scan_is_alloc_free_per_row` — a counting global allocator asserts that scanning
N and 2N rows allocates the same **count and bytes**. A positive control proves the allocator is
actually linked, so a broken dev-dependency cannot make the guard pass vacuously.
:::

The honest caveat the project records with it: that guard runs a **single-level** plan. Opening
a level allocates — a fresh prefix buffer, an upper bound, a new store scan — so a *join*
allocates once per outer row. A descent is a level transition rather than a row, so I9 stands
as written, but the guard is not evidence about joins.

## The `enumerate` driver

```text
loop:
  if depth == body.len():           # past the innermost step → a full row is bound
      hand the Row to the consumer
      on Continue: if body is empty: return Done
                   depth -= 1       # backtrack to find the next row
      on Suspend:  if levels() == 0: return Done
                   depth -= 1
                   return Suspended{ cursor }

  else match body[depth]:
      Level(level):
        if this frame's source index is past the last source:
            close the frame
            if depth == 0: return Done
            depth -= 1; continue
        if the frame has no open scan: open the current source
        match frame.next():
          Some(row): bind into registers; frame.current = row; depth += 1
          None:      close this alternative; advance the source index

      Derive(bind):
        if not produced: compute into the value slot; produced = true; depth += 1
        else:            produced = false; back up a level (or Done at 0)

      Test(Absent(sources)):
        if produced:            produced = false; back up a level (or Done at 0)
        elif no source yields:  produced = true; depth += 1     # pass, registers untouched
        else:                   drop the row, exactly as exhaustion does
```

`frame.next()` is the scan step: pull rows, apply the source's residuals — **key fields only**
([I6](invariants.html#i6)) — and return the first match. It also polls the cancellation token
every ~4096 rows.

**A plan with no levels is the unit relation: exactly one row.** A query whose every bind
folded has no steps at all, so there is nothing to iterate. Two consequences are written into
the loop: backing out of the head cannot decrement past zero, and a *suspend request* there
reports `Done` rather than handing back a cursor — an empty cursor means "start from the
beginning", so resuming would re-emit the row.

### Why a state machine and not recursion

:::invariant I7 — the executor is a defunctionalised state machine, on purpose
The driver plus the frame stack are the explicit reification of a recursive `concatMap`, chosen
so that **execution can suspend to bytes**. Native recursion, closures and coroutines cannot: a
suspended closure pins live iterators and a snapshot. Do not "simplify" the driver back into
recursion.

*Guard:* structural — enforced by the resume battery (byte-resume is impossible under a
recursive rewrite) plus review.
:::

"Defunctionalised" means what a recursive implementation would keep on the *call stack* — the
continuation, where each nested loop had got to — is instead an explicit **data structure**.
Because it is data, it can be serialised to a handful of bytes and rebuilt later.

The neighbouring decision is declining a **bytecode VM**, and it turns on token size rather
than capability. A VM does suspend and resume exactly; what it cannot make small is its
continuation, which carries the program, the program counter, the literal table, every
register and every output buffer — and is therefore version-locked to the bytecode ABI, so any
change to the instruction set invalidates every in-flight continuation. The trade is honest in
both directions: a small, stable token is bought with **per-feature resume work**, paid one
construct at a time.

## The consumer seam

The executor is the enumerator; the consumer is an **iteratee** — a `step` callback that
receives each row and answers `Continue` or `Suspend`.

```text
  consumer returns   Stream::Continue | Stream::Suspend
  enumerate returns  Iteratee::Done | Iteratee::Suspended { cursor }
```

A `Row` is **borrowed and one-step-lived**: it is a view of the registers as they stand, not a
copy. Nothing materialises a result set, at any layer — the server's chunk loop, the CLI's
renderer and the viewer's pages are all consumers of this seam.

## The `Cursor` — bytes, and nothing else

```rust
struct Cursor {
    version: u16,           // which cursor layout these bytes are in
    plan: PlanFingerprint,  // which plan produced them
    entries: Vec<Entry>,    // one per open loop *level*
}
struct Entry { source: usize, row: Register }
```

That is the whole token: two stamps and one detached row per level. No open iterators, no plan
pointer beyond the fingerprint, no snapshot — nothing a socket could not carry.

- A **derive** step contributes nothing: it is recomputed on restore
  ([I14](invariants.html#i14)).
- A **test** contributes nothing: it binds nothing, and its verdict is re-decided rather than
  replayed.
- A row is saved **detached** — bytes copied to owned memory — so the cursor references no
  shared buffer and no live scan.
- An entry says **which source** produced the row, because that is not recoverable from the
  row: alternatives can overlap, and the sources after the live one have not run yet.

:::note The recompute rule
In an immutable database a store read is a pure function of its inputs, so anything determined
by the bindings and the frozen base may be **recomputed on restore instead of saved**. Derived
binds are the special case that named it; a fetch by fact id and a filter's verdict are the
rest. What the cursor holds is what a *scan* cannot recompute: its position.
:::

### The token says which run it belongs to

Entries are paired with the plan's levels **by order**, so a cursor from another plan would be
read against the wrong levels. Five checks run before anything is trusted, widening to
narrowing:

| Check | Catches | Error |
|---|---|---|
| `version` | A cursor from a build where an entry meant something else | `CursorVersion` |
| plan fingerprint | A cursor from another plan | `CursorPlan` |
| entry count | A forged length, exactly | `CursorPlanMismatch` |
| `source` index | An alternative this level does not have | `CursorSourceOutOfRange` |
| `fact_id` | A saved key that is no longer the row it named | `BadResumeKey` |

Without the first two, two same-shaped plans over overlapping predicates accept each other's
cursors and the failure is a **silently short answer** rather than an error.

The fingerprint is FNV-1a over the plan's structure, written out explicitly rather than derived
— stability is the whole requirement, and a language-default hasher is free to change between
releases. **Interned names are deliberately not hashed**: a symbol is an index into a per-query
interner, so the same query compiled in another process names its head fields with different
numbers, and hashing those would fail a legitimate resume. The stated consequence: two plans
differing only in what their head fields are *called* fingerprint the same. Neither positions a
scan.

## How resume reconstructs the run

`resume(store, plan, cursor)` does one forward walk over the plan's steps — **re-bind the fact
slots, recompute the value slots**:

1. At a **level**, consuming the next saved entry in order: re-open the scan positioned at the
   saved key, pull one row, re-bind it into the registers.
2. **Integrity check:** the re-read row's `fact_id` must equal the saved one. If not,
   `BadResumeKey`.
3. At a **derive**, recompute the value. Nothing is consumed from the cursor, and nothing needs
   to be — purity is what makes recomputing equivalent to having saved it.
4. At a **test**, nothing happens except marking it produced. The row it passed was handed to
   the consumer before the suspend, and the base is frozen, so a second probe could only agree.
   Marking it produced is not optional though: without the bit the machine arrives from below,
   probes, passes, and ascends into a row it has already emitted.

Then set `depth` to the innermost step and hand back to the driver — which, because that
innermost frame's scan is already open and positioned, calls `next()` and thereby **advances
past** the last-emitted row. Outer levels are *not* advanced; they stay pinned on their saved
rows and only move when the inner level exhausts. That is exactly the nested-loop semantics of
an uninterrupted run, reconstructed from bytes.

### Why the `fact_id` check is the linchpin

A bytes-only cursor is safe only if a saved key still means the same fact. In an immutable
sealed database it always does — but a cursor can outlive a rebuild, be replayed against a
different store, or hit a bug. So resume **verifies** rather than trusts, and the direction is
where the safety lives: saving `(key, fact_id)` and checking the row re-read at the key can
fail, where looking a key up *from* the id is a tautology that can never catch anything.

The project is explicit that this is a **detector, not a guarantee**: two databases built from
the same facts in the same order agree on a prefix of every predicate's mapping, so a cursor
saved inside that prefix would resume clean and then emit the other database's rows. What
closes it is a database fingerprint in the token — recorded as the third stamp, not yet built.

:::invariant I4 — resume equals an uninterrupted run
Resuming from a cursor produces exactly the rows, in exactly the order, that an uninterrupted
run would have.

*Guard:* `exec::resume_equals_uninterrupted` plus its fjall arm — a tier-3 model-based property
over generated `(plan, store)` pairs **and a generated interruption schedule**: suspend at every
boundary, in every combination, and compare against a run to completion. Green on both the
in-memory store and the real one, where the two must also agree row for row and id for id.
:::

:::invariant I8 — the snapshot is released at suspend
A query reads an immutable snapshot, and every stop — suspend, cancel, terminal unwind —
releases it. A paused query never holds storage open.

*Guard:* `i8_snapshot::snapshot_released_at_suspend`, which cross-checks a drop probe against
the storage engine's own count of open snapshots.
:::

## Suspend, cancel, unwind — three different things

| Kind | Trigger | Resumable? | Snapshot |
|---|---|---|---|
| **Suspend** | The consumer yields — backpressure, a page boundary | **Yes**, returns a `Cursor` | Released |
| **Cancel** | A cancellation token, polled every ~4096 rows | No | Released |
| **Terminal unwind** | A deadline or a rows-examined cap | No | Released |

Cancellation is **cooperative and synchronous**: the scan loop polls a flag. The executor is
deliberately not `async` — its work is blocking CPU and I/O, and making it async would colour
the whole codebase for nothing. The server hops onto a blocking pool instead.

One capability gap recorded rather than hidden: a *deadline* unwinds terminally rather than
handing back a resumable position, because a cursor of one row per level has no way to
represent a mid-descent position. A resumable time slice needs that extra bit plus a fresh
proof that resuming from it is exact.

## What a run examined

The executor already counts rows examined, for cancellation. `enumerate_profiled` hands that
counter back instead of throwing it away, **per step of the plan's body** — which is what gives
a fetch, a disjunction and a negation each a line of their own:

```text
STEP      EXAMINED
src.Decl  1000      full scan
src.Ref   1
1001 examined, 1 produced
```

Read it against `:plan`: the plan is the intent, this is the outcome.

## Where this shows up

| Feature | Built on |
|---|---|
| `:more`, `query --limit`, a paged web tier | `Iteratee::Suspended` and the byte cursor |
| Chunked results (256 rows) that never buffer server-side | The same, once per chunk |
| In-band per-stream cancellation | The cancellation token, polled in the scan loop |
| `--profile` | The counter cancellation already kept |
| `--count` | The same plan and executor with a different accumulator: the driver is a fold |
