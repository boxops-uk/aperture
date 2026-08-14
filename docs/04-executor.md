# 4. The executor (the VM)

> [Aperture design book](../README.md) · [← 3. The storage model](03-storage-model.md) · **Chapter 4** · [5. Suspend & resume →](05-resume.md)

The executor is a **pull-based virtual machine** that runs a `Plan` as a nested loop, one
row at a time. This chapter covers the plan IR it consumes, the register file that holds
bound rows, the `enumerate` driver, and the four invariants that make the hot path fast and
the machine suspendable. Suspend/resume itself is [chapter 5](05-resume.md).

Code: `src/focus/plan.rs` (types) and `src/focus/iter.rs` (the machine).

---

## The plan IR

A **`Plan`** (`plan.rs`) is:

```rust
struct Plan {
    nvars: usize,       // size of the register file
    body: Box<[Step]>,  // ordered steps: [0] outermost … [n-1] innermost
    head: Project,      // how to build each output row
}

enum Step {
    Level(Level),         // a loop level
    Derive(DerivedBind),  // a value to compute — not a loop level
    Test(Test),           // a filter to pass — not a loop level either
}
```

A query is a **nested loop**, and the order of `body` *is* the loop nesting. It is one
ordered sequence because [`reorder`](07-compilation.md) produces one order; holding levels and
computations in separate collections joined by an index would be two sources of truth for one
ordering, with nothing to say which wins.

Glean compiles to the same shape — one `seek`/`next` level per fact generator, statement order
becoming loop nesting (`glean/db/Glean/Query/Codegen.hs:994-1042`) — but it *emits* that
nesting as bytecode, where Aperture emits an ordered `[Step]` for a driver to walk.
Convergence, not lineage; the [comparison ledger](glean-comparison.md) is where the two are
told apart.

> **`body.len()` is not the number of loops.** It counts *steps*; `Plan::levels()` counts scan
> steps, and `Plan::level(n)` reaches the `n`th. The distinction is load-bearing in exactly one
> place — a [`Cursor`](05-resume.md) holds one row per **level**, and resume pairs its entries
> with scan steps by order. The two were the same number before derive steps existed, and the
> cursor's length check said `>` rather than `!=` as a result, which let a short cursor
> half-replay a plan and carry on from the wrong place.

A `Step::Derive` is chapter 7's [derived bind](07-compilation.md#derived-facts) and is
**one row**: the machine computes its value descending and reports exhausted ascending, which
costs one bit of frame state because arriving at a step from below and from above must differ
and the loop carries no direction. It contributes no cursor entry — see
[I14](invariants.md#i14).

A **`Step::Test`** is the third kind and the same shape: one row, and the row it produces is the
one already standing.

```rust
enum Test {
    Absent(Box<[Source]>),  // the row survives iff *no* source produces a row
}
```

`Absent` is **negation**, and the sources are the negated statement's alternatives — so the count
means what it means for a level: one is `!test.Bar {…}`, several is a negated disjunction, and
*none* is the negation of the empty relation, which every row passes. Each source is drained only
to its **first** row: the question is whether a witness exists, not how many, so a negation over a
predicate holding a million matching rows reads one of them.

What makes it cheap to add is that neither of its two outcomes is new to the machine. Passing is
ascending with the registers untouched; failing is the same backtrack an exhausted level does. So
negation needed no new direction, no new frame kind, and no reshaping of the loop —
[I7](invariants.md#i7) holds by construction rather than by care. The frame's one bit of state is
shared with a derive step, because a frame is one step and a step is one kind.

The probe opens each source, asks for one row, and **closes it again before returning**, so a
negation holds no iterator between probes and a suspend at any depth has nothing of its to release
([I8](invariants.md#i8)). It reads `keys` and fetches no value, which is why
[I6](invariants.md#i6) is untouched; and because it runs once per row the level above it
*produces* rather than once per row a scan *examines*, its cost is the same shape a
[fetch](#fetching-through-a-reference) pays.

> **A test is not a level, and the arithmetic depends on it.** A register address counts levels,
> not steps: a derive and a test bind no row. `Plan::levels()` is what the cursor is checked
> against, and flatten counts the same way when it assigns addresses.

Each loop level is a `Level`, and its rows come from a list of **sources**:

```rust
struct Level {
    sources: Box<[Source]>,  // alternatives, tried in order and concatenated
    binds: Box<[Address]>,   // registers this level fills from the matched row
}

enum Source {
    Seek  { access: Access, residuals: Box<[Residual]> },
    Fetch { reference: Address, path: FieldPath,        // the fact a reference names
            predicate_id: PredicateId, residuals: Box<[Residual]> },
}
```

**The count is the construct.** Zero sources is the **empty relation** — the level is
exhausted the moment it is entered, which is what `never` means. One is an ordinary scan,
which is every level focus compiles today. Many is a **disjunction**, one branch per source.
They are one node rather than three because `enumerate`'s job is identical in all three —
open a source, drain it, move to the next, back up when there is no next — so `never` needs
no arm of its own and no case in the driver. The two ways a level can end are one arm each:
"no source left to open" covers the drained level and the level that never had a source
alike.

**Residuals belong to the source, not to the level**, because a residual is a `FieldPath`
into a row and two sources are two key layouts: a path that names a field of one names
different bytes, or none, in the other. `binds` stays on the level, because every
alternative binds the same variables — which is what lets a register mean one thing whichever
branch filled it. Where a branch would have to bind a *different* shape, the branch has to
export a value instead, and that rule is [the query-surface note](query-surface.md)'s.

- **`Access`** = a `predicate_id` plus a **`SeekKey`**. The seek key builds the scan prefix:
  either constant `Bytes`, or a `RegisterField { address, field_idx }` **splice** — bytes
  copied from a variable bound by an *earlier* loop level. Splicing is how a join narrows
  the inner scan to rows matching the outer row — the same join mechanism Glean uses, whose
  `withPrefix`/`buildPrefix` assembles the inner seek prefix out of an outer level's bound
  bytes, reading the bound variable's output buffer directly when it *is* the whole prefix
  (`glean/db/Glean/Query/Codegen.hs:1110-1152`).
- **`binds`** = the registers (`Address`es) this generator fills when a row matches. Note
  several variables can bind to the *same* row — see the row-slot model below.
- **`residuals`** = filters applied to each scanned row *before* it counts as a match.

### Fetching through a reference

A `Source::Fetch` is the level whose rows are **one row**: the fact a reference names.
`reference` is a register bound at an outer level, `path` a fact-typed field of its key — so
the id is already in hand, and this is the point read that
[`SeekKeyPart::RegisterFactId`](#the-two-halves-of-a-reference) does not need.

It is a *source* and not a step, and that is the whole reason `enumerate` is unchanged by it.
A point read is a relation of at most one row, and the machine's job over such a relation is
a scan's exactly: open it, drain it, move on, back up when there is no next.

The register it binds is **`predicate_id ++ key`** — byte for byte the row a scan of that fact
would have produced. That uniformity is what keeps everything downstream ignorant of where a
row came from: a seek splices a fetched field, a residual compares against one, the head
projects one, and the [cursor](05-resume.md) saves one. `entities` stores the key beside the
value without the predicate tag, so putting the tag back is one allocation, once per opening
of the level — the same footing as the seek prefix's, and not per row.

Two checks, both about reading the right bytes rather than about the query:

- **The declared predicate is checked against the stored id.** Not redundant with the tag the
  id already carries: every residual path on this source, and every projection off the
  register it binds, was compiled against the *declared* key layout, so a reference naming
  another predicate would decode a different type's bytes at those offsets and answer with
  whatever was there. Refused (`ReferenceCrossesPredicate`), which is the same family of fault
  as splicing a key where an id belongs.
- **A reference naming no fact is reported**, not skipped. Both column families are written
  together ([I12](invariants.md#i12)) and ids are never reused ([I11](invariants.md#i11)), so
  there is no legitimate way to reach one; dropping the row would answer short in silence.

#### The two halves of a reference

Following one and reading through one are different plans, and the split is deliberate:

| | what it does | what it costs |
|---|---|---|
| `SeekKeyPart::RegisterFactId` / `ResidualOp::EqRegisterFactId` | **follows** a reference: compares the id a field holds against a bound row's identity | nothing beyond the scan it narrows — the id is in the register ([I6](invariants.md#i6)) |
| `Source::Fetch` | **reads through** a reference: binds the fact the id names | one `entities` point read per row of the level above it |

That is also why a fetch does not breach I6. What I6 forbids is a value lookup *per row the
scan examines*, which is what a value pattern would cost; a fetch is a level of its own, so it
is opened only once an outer row has already survived every residual on it — and what it reads
out of `entities` is the row's **identity**, which is what that column family is for.

### Residuals

A `Residual` is `{ field_idx, op }`, checked against a key field:

- `EqConst(bytes)` — the field equals a constant.
- `Prefix(bytes)` — the field starts with a prefix.
- `EqRegisterField { address, field_idx }` — the field equals a field of an
  already-bound register (a cross-loop equality that couldn't be expressed as a seek).

Residuals are evaluated against the **key** (`keys` CF) only — never the value. This is
[I6](invariants.md#i6). New comparison operators (`<`, `<=`, …) will arrive as new
`ResidualOp` arms without touching the machine (see [open decisions](open-decisions.md) and
[scope](../PLAN.md)).

### Projection (the head)

`Project` says how to turn the bound registers into an output `Value`:

- `Lit(value)` — a constant.
- `RegisterField { address, field_idx, ty }` — decode one key field of a bound row.
- `FactRef(address)` — the bound row's `FactId` as a fact reference.
- `Value { address, ty }` — **fetch and decode the fact's value** from `entities` (the one
  place the value is read).
- `Computed(address)` — a derived bind's output, read out of its value slot. Already a `Value`,
  so nothing is decoded. Distinct from `Lit` on purpose: folding a constant bind into a `Lit`
  is what the compiler does, and collapsing the two here would leave the recompute path with
  no coverage ([chapter 7](07-compilation.md#folding-a-constant-bind)).
- `Record(fields)` — a record built from sub-projections (sorted `(Symbol, Project)`).

Projection is **lazy and at the escape boundary**: fields are decoded here, at the read
site, not when the row was bound. That is [I5](invariants.md#i5).

---

## The register file and the row-slot model ([I5](invariants.md#i5))

`MachineState` is the register file: `Box<[Option<Slot>]>`, indexed by `Address`. A **`Slot`**
holds either a stored row or a computed value, and a **`Register`** is the row case:

```rust
enum Slot {
    Fact(Register),  // a stored row — what I5 below is about
    Value(Value),    // a derived bind's output (I14)
}

struct Register { fact_id: FactId, bytes: ByteView }  // bytes = predicate_id ++ key
```

The two kinds are separated *at the type level* rather than unified behind "some bytes",
because splicing a value where a fact id belongs — or the reverse — compares two different
encodings and quietly matches nothing. That is the same silent shape as the
[`FactRef` marker](02-tuple-codec.md) split, so reading a slot names the kind it wants
(`MachineState::fact` / `::value`) and a mismatch is a reported error, not a panic: flatten
cannot emit one, since it knows which addresses a derive step writes, but a plan arriving off
the wire can.

The critical design decision — [invariant I5](invariants.md#i5):

> **I5 — a register holds the *whole* row, not a field.** The *field* a variable denotes
> lives in the **plan** (`RegisterField { address, field_idx }`), not the register. So a
> generator binding N variables is **N `ByteView` refcount bumps to the same row** — no
> per-field decode at bind time. Decode lazily at read/projection sites only.
>
> *Guard:* `exec::bind_is_refcount_not_decode` — a decode-counting probe asserts binding N
> vars triggers zero field decodes.

Why: at bind time you don't yet know which fields will be read (a row might be bound and
then discarded when an inner loop finds no match). Decoding eagerly would do work that's
usually thrown away and would allocate per field. Holding the whole row and decoding on
demand keeps bind O(1) and the hot path allocation-free. `ByteView` clones are **refcount
bumps, not copies**, so "the whole row" is cheap to share across registers.

**Glean binds eagerly, which is I5's rationale measured in a real system.** Its compiled
`matchPat` decodes a word field straight into a machine register and **memcpy's** every
non-word field into a per-variable buffer, and it does both *before* the inner loop runs
(`glean/db/Glean/Query/Codegen.hs:1029-1034,1543-1557`) — so an outer row the inner loop goes
on to discard has already been copied field by field. That is precisely the "bound then
thrown away" waste the paragraph above cites, and it is not hypothetical. One point the other
way: because the walk is compiled, Glean binds only the variables the query actually reads
(`findOutputs`, `glean/db/Glean/Query/Codegen.hs:132-170`) rather than the whole row to every
variable.

---

## The frame stack

```rust
struct StackFrame {
    rows: Option<Rows>,           // the open source's remaining rows, or None if closed
    source: usize,                // which of the level's sources is being drained
    current: Option<Register>,     // the row this level is currently sitting on
    field_offsets: Box<[FieldOffsets]>, // per-variable field-offset cache
    derived_produced: bool,        // a Derive step's whole state (unused by scans)
}

enum Rows {
    Scan(Scan),                        // a live cursor into `keys`
    Fetched(Option<(ByteView, FactId)>), // the one row a reference names
}
```

Execution state is a stack of frames, **one per step**, not one per level: a derive step needs a
frame too, though all it uses is the last field.

- **`rows`** is what the open source has left to hand out. It is `None`
  when the level is closed — opening it fresh on descent is what makes byte-resume possible
  ([chapter 5](05-resume.md)).

  Two shapes, one iterator, because `next` — the loop that ticks the deadline and checks
  residuals — is written once for both. A `Seek` source opens a fjall (or `MemStore`) iterator
  over its range. A [`Fetch`](#fetching-through-a-reference) source does its point read *at
  open* and holds the single row it found, so draining it is the same `None` a scan gives at
  its end. Anything else added later that is "a relation the machine walks" belongs here, not
  in `enumerate`.
- **`source`** is which alternative is being drained. It only moves forward while the level
  is open, and is reset when the level closes — a level re-entered from an outer level's next
  row produces all of its alternatives again, rather than resuming where the last pass through
  it happened to stop.
- **`current`** is the level's cursor position, saved into the resume `Cursor` on suspend,
  along with `source`: which alternative produced a row is not recoverable from the row.
- **`field_offsets`** caches where each field starts within a bound row's key, so repeated
  field access on the same row doesn't re-walk from the front.

### Field-offset cache ([I9](invariants.md#i9))

`FieldOffsets` is an **inline `ArrayVec<[usize; 16]>`** — fixed capacity, on the stack, no
heap. It memoises the end offset of each field as `skip` walks them. The first access to
field `k` walks fields `0..=k` (using the [self-delimiting](02-tuple-codec.md) `skip`) and
caches the boundaries; later accesses are a lookup. Fields **beyond** the 16-slot cache are
walked on demand and *not* cached — the cache never heap-spills. This is part of:

> **I9 — the hot path is allocation-free per row.** Reused scratch buffers; `ByteView`
> clones are refcount bumps; field-offset caches are inline and never spill. Copy out only
> at escape boundaries — suspend (detach `ByteView` → owned bytes) and string/bytes
> projection.
>
> *Guard:* `exec::scan_is_alloc_free_per_row` — the `allocation-counter` dev-dependency's
> global allocator (thread-local counters, so it survives the parallel harness) asserts that
> scanning N and 2N rows allocates the same **count and bytes**, excluding escape boundaries.
> Bytes matter on their own: a single buffer sized by the row count is one allocation either
> way. A positive control in the test proves the allocator is actually linked, so a broken
> dev-dependency can't make the guard pass vacuously.

The cache itself is a cost of **interpreting** rather than compiling, and worth naming as one:
a compiler emits a single straight-line forward pass with exactly the `skip`s that query needs
and never re-walks a row, which is why Glean has no equivalent structure at all
(`glean/db/Glean/Query/Codegen.hs:1470-1585`). Aperture pays a 16-word inline memo instead of a
code generator.

**Within a scan level Aperture allocates nothing and Glean allocates per row; per descent the
two are even.** Glean's `ResetOutput` move-assigns a fresh `binary::Output` over the old one,
freeing the old buffer (`glean/rts/bytecode/subroutine.cpp:97-99`,
`glean/rts/binary.h:315-321,493-497`), and the buffer carries a 23-byte small-string
optimisation — so binding a non-word field *larger* than that is a `free` plus a `malloc` on
every row. The caveat this chapter owns in exchange: `exec::scan_is_alloc_free_per_row` runs a
**single-level** plan. Opening a level allocates on both sides — `StackFrame::open` builds a
fresh prefix `Vec`, a `strinc` upper bound and a new store scan — so a **join** allocates once
per outer row, and no guard covers that. A descent is a level transition rather than a row, so
I9 stands as written; but the guard is what makes it evidence instead of assertion, and it is
not evidence about joins.

---

## The `enumerate` driver ([I7](invariants.md#i7))

`enumerate` is the whole machine. It walks a `depth` cursor up and down the frame stack:

```
loop:
  if depth == body.len():           # past the innermost step → a full row is bound
      hand Row to the consumer (step)
      on Continue: if body is empty: return Done    # nothing to back into
                   depth -= 1       # backtrack to find the next row
      on Suspend:  if levels() == 0: return Done    # see below
                   depth -= 1
                   return Suspended{ cursor }   # (chapter 5)

  else match body[depth]:
      Level(level):
        frame = stack[depth]
        if level.sources[frame.source] is None:  # every alternative drained — or
            frame.close()                        #   there were none at all
            if depth == 0: return Done
            depth -= 1
            continue
        if frame.scan is None: frame.open(source) # descend: open this source fresh
        match frame.next():                      # pull the next matching row
          Some(row): bind row into registers; frame.current = row; depth += 1
          None:      frame.scan = None           # this alternative is drained;
                     frame.source += 1           #   the next round opens the next

      Derive(bind):                              # a one-row generator
        if not produced: compute into the value slot; produced = true; depth += 1
        else:            produced = false        # exhausted, on the way back up
                         if depth == 0: return Done
                         depth -= 1

      Test(Absent(sources)):                     # a filter: the row already standing
        if produced:     produced = false        # exhausted, on the way back up
                         if depth == 0: return Done
                         depth -= 1
        elif no source yields a row:             # open, take one, close — per source
                         produced = true; depth += 1        # pass, registers untouched
        elif depth == 0: return Done             # a whole query, and a witness ends it
        else:            depth -= 1              # drop the row, exactly as exhaustion does
```

**A plan with no levels is the unit relation: exactly one row.** Every step is a derived bind
and a derived bind is one value, so there is nothing to iterate — a query whose every binding
[folded](07-compilation.md#folding-a-constant-bind) has no steps at all. Two consequences are
written into the loop above. Backing out of the head cannot decrement past zero, and a
*suspend request* reports `Done` rather than handing back a cursor: the cursor would be empty,
an empty cursor means "start from the beginning", and so resuming would re-emit the row.
Reporting `Done` is not a half-answer — the run genuinely is complete, which is what a resume
would have discovered one round-trip later.

`frame.next()` is the scan step: pull rows from the cursor, apply the generator's residuals
([I6](invariants.md#i6) — key fields only), and return the first match. It also polls the
**cancellation token every `CANCELLATION_STRIDE` (~4096) rows** — cooperative,
synchronous cancellation (see [chapter 5](05-resume.md)).

Backtracking is the `depth -= 1` above. Glean spells the identical move as a backward `jump` to
the enclosing level's loop label, its inner levels inlined into the outer ones by a CPS-shaped
code generator (`glean/db/Glean/Query/Codegen.hs:994-1042`) — the same control flow, held as
data here and emitted as code there, which is what the next section is about.

### Why a state machine and not recursion — [I7](invariants.md#i7)

The obvious way to write a nested loop is native recursion (or nested iterators, or
`concatMap`). Aperture deliberately does **not**:

> **I7 — the executor is a defunctionalised state machine, on purpose.** The `enumerate`
> driver + the frame stack are the explicit reification of recursive `concatMap`, chosen so
> that **execution can suspend to bytes** ([I4](invariants.md#i4)). Native recursion /
> closures / coroutines cannot: a suspended closure pins live iterators and a snapshot.
> **Do not "simplify" `enumerate` back into recursion.**
>
> *Guard:* structural — enforced by the [resume battery](05-resume.md) (byte-resume is
> impossible under a recursive rewrite) plus code review.

"Defunctionalised `concatMap`" means: the thing a recursive implementation would keep on
the *call stack* (the continuation — where each nested loop had got to) is instead an
explicit **data structure** (the frame stack, each frame's `current`). Because it's data,
it can be serialised to a handful of bytes and rebuilt later. That is the entire point, and
it's why chapters 4 and 5 are two halves of one idea.

**Declining a bytecode VM is the neighbouring decision, and it turns on what a resume token
costs — not on what a machine can do.** I7's "closures cannot" is a claim about closures, and it
holds. A VM is the other alternative, and a VM suspends and resumes exactly: Glean's query VM is
a flat register machine with **no call stack** at all — "we don't have a stack (yet)"
(`glean/db/Glean/Query/Codegen.hs:573-576`) — where disjunction and if-then-else return through
a return address parked in an ordinary register plus `jumpReg`
(`glean/db/Glean/Query/Codegen.hs:455-483,577-593`), over a frame that is a
`uint64_t[inputs+locals]` placement-new'd on the C++ stack
(`glean/rts/bytecode/subroutine.h:88-99`). What a VM cannot make small is its **continuation**.
Glean's carries the entire bytecode program, the PC, the literal table, every local register
word, every `binary::Output` buffer, and a second `traverse` subroutine
(`glean/rts/bytecode/subroutine.cpp:370-381`, `glean/rts/query.cpp:427-436`); because the code
travels inside the token, the token is **version-locked to the bytecode ABI**
(`lowestSupportedVersion == version == 15`,
`glean/bytecode/def/Glean/Bytecode/Generate/Instruction.hs:86-96`), so any bytecode change
invalidates every in-flight continuation. Aperture's [`Cursor`](05-resume.md) is one detached
row per open level — roughly two orders of magnitude smaller, plan-shaped rather than
code-shaped, and therefore able to survive an engine change. The decision stands unchanged; the
reason is token size and token stability, not impossibility.

Two consequences worth keeping straight. Glean does keep an explicit **iterator** stack
(`std::vector<Iter>`, `glean/rts/query.cpp:259,262-302`); only its *control* state lives in the
PC, so the two designs differ in **what** they reify, not in whether they reify anything. And
the store is behind a seam on both sides — Glean reaches `seek`/`next` through **syscall
function-pointer registers** rather than opcodes
(`glean/bytecode/Glean/Bytecode/SysCalls.hs`), which is the exact analogue of the `FactStore`
trait.

**The small token has a recurring price, and it is paid one construct at a time.** Because a VM
saves its whole activation wholesale, disjunction and conditionals need *zero* new continuation
state — a return address is just another register it was saving anyway. In Aperture each such
feature must **extend** the `Cursor` and re-prove [I4](invariants.md#i4)
([`PLAN.md`](../PLAN.md) Phase 6b). Resume state that is small and stable is bought with
per-feature resume work; that is the honest counterweight to a token measured in tens of bytes,
and the trade is argued in full in the [comparison ledger](glean-comparison.md).

---

## The consumer seam (iteratee)

`enumerate` doesn't decide what happens to a row — it hands each finished row to a `step`
callback and obeys the answer:

```rust
enumerate(init, step, cancel) -> Iteratee<A>
// step(acc, Row) -> Stream::Continue(acc) | Stream::Suspend(acc)
// returns          Iteratee::Done(acc)    | Iteratee::Suspended(acc, Cursor)
```

The executor is the **enumerator** (producer); the `step` is the **iteratee** (consumer —
projection, serialisation, backpressure). A `Row` is a borrowed, one-step-lived view; the
consumer must copy anything it keeps. This seam is exactly what the wire protocol's
portals, backpressure, and cancellation are built on ([Operations](aperture-cli-design.md))
— chapter 5 covers `Stream::Suspend`/`Iteratee::Suspended`.

---

## Invariants owned by this chapter

| # | Statement | Guard test |
|---|-----------|------------|
| [I5](invariants.md#i5) | A register holds the whole row; fields decode lazily. | `exec::bind_is_refcount_not_decode` |
| [I6](invariants.md#i6) | Values never enter the scan hot loop. | `exec::no_value_fetch_in_scan` |
| [I7](invariants.md#i7) | The executor is a defunctionalised state machine. | structural + the resume battery |
| [I9](invariants.md#i9) | The hot path is allocation-free per row. | `exec::scan_is_alloc_free_per_row` |

---

> **Reading path:** [← 3. The storage model](03-storage-model.md) · **4. The executor** · [5. Suspend & resume →](05-resume.md)
