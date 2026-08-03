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
    nvars: usize,          // size of the register file
    body: Box<[Generator]>, // loop levels: [0] outermost … [n-1] innermost
    head: Project,          // how to build each output row
}
```

A query is a **nested loop**, and the order of `body` *is* the loop nesting. Each level is
a `Generator`:

```rust
struct Generator {
    access: Access,         // which predicate, and where to start scanning
    binds: Box<[Address]>,  // registers this level fills from the matched row
    residuals: Box<[Residual]>, // extra filters checked during the scan
}
```

- **`Access`** = a `predicate_id` plus a **`SeekKey`**. The seek key builds the scan prefix:
  either constant `Bytes`, or a `RegisterField { address, field_idx }` **splice** — bytes
  copied from a variable bound by an *earlier* loop level. Splicing is how a join narrows
  the inner scan to rows matching the outer row.
- **`binds`** = the registers (`Address`es) this generator fills when a row matches. Note
  several variables can bind to the *same* row — see the row-slot model below.
- **`residuals`** = filters applied to each scanned row *before* it counts as a match.

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
- `Record(fields)` — a record built from sub-projections (sorted `(Symbol, Project)`).

Projection is **lazy and at the escape boundary**: fields are decoded here, at the read
site, not when the row was bound. That is [I5](invariants.md#i5).

---

## The register file and the row-slot model ([I5](invariants.md#i5))

`MachineState` is the register file: `Box<[Option<Register>]>`, indexed by `Address`. A
**`Register`** holds a *whole row*:

```rust
struct Register { fact_id: FactId, bytes: ByteView }  // bytes = predicate_id ++ key
```

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

---

## The frame stack

Execution state is a stack of frames, one per loop level:

```rust
struct StackFrame {
    scan: Option<Scan>,           // the live cursor into `keys`, or None if closed
    current: Option<Register>,     // the row this level is currently sitting on
    field_offsets: Box<[FieldOffsets]>, // per-variable field-offset cache
}
```

- **`scan`** is the fjall (or `MemStore`) iterator for this level's range. It is `None`
  when the level is closed — opening it fresh on descent is what makes byte-resume possible
  ([chapter 5](05-resume.md)).
- **`current`** is the level's cursor position, saved into the resume `Cursor` on suspend.
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
> *Guard:* `exec::scan_is_alloc_free_per_row` — an allocation-counting global allocator
> asserts zero allocations across a multi-row scan step (excluding escape boundaries).

---

## The `enumerate` driver ([I7](invariants.md#i7))

`enumerate` is the whole machine. It walks a `depth` cursor up and down the frame stack:

```
loop:
  if depth == body.len():           # past the innermost loop → a full row is bound
      hand Row to the consumer (step)
      on Continue: depth -= 1       # backtrack to find the next row
      on Suspend:  return Suspended{ cursor }   # (chapter 5)

  else:
      frame = stack[depth]
      if frame.scan is None: frame.open(...)     # descend: open this level fresh
      match frame.next():                        # pull the next matching row
        Some(row): bind row into registers; frame.current = row; depth += 1   # go deeper
        None:      frame.scan = None             # exhausted: close and back up
                   if depth == 0: return Done
                   depth -= 1
```

`frame.next()` is the scan step: pull rows from the cursor, apply the generator's residuals
([I6](invariants.md#i6) — key fields only), and return the first match. It also polls the
**cancellation token every `CANCELLATION_STRIDE` (~4096) rows** — cooperative,
synchronous cancellation (see [chapter 5](05-resume.md)).

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
