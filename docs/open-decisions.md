# Open decisions

> [Aperture design book](../README.md) · reference doc

What is **not yet settled** — do not treat anything here as an invariant. Below the open
items is a short record of decisions that *have* settled, kept so they aren't re-litigated,
with a pointer to where they now live.

---

## Still open

Every decision this file was *opened* for has settled — the record is below. What replaced them
came out of stepping back and comparing the whole design against Glean
([the comparison](glean-comparison.md) is the analysis; these two are the questions it left):

### Multiplicity — arrays, or one fact per element?

`PredicateTy` has no array type, so a one-to-many relationship is modelled as **one fact per
element**. That is arguably the better answer for an index — every element is independently
seekable, and nothing decodes a list to filter it — but it is a decision about **how every
schema is written**, not just about what a type can hold.

The codec reserves marker bands, so adding `[T]` later is not a one-way door in the *encoding*.
Writing schemas without it is the thing that gets expensive to undo. **Decide before the schema
DSL fixes what can be written** ([`PLAN.md`](../PLAN.md) Phase 8), and record the answer here
either way. Related, and easier: `bool` and `maybe T` are sugar over a union once unions land;
`nat`/`byte` are a range question, not a shape one.

### Primitives in the query language

Angle has `prim.*` — arithmetic, string operations, comparisons — and if-then-else. focus has
string prefix matching, and **order comparisons** are deferred-with-a-seam (`ResidualOp` arms).
Arithmetic, string functions and conditionals are in neither place: not built, not deferred,
not ruled out. The seam that would carry them is Phase 6's derived binds (a pure function of
the fact bindings), so this is additive whenever it is wanted — the open question is whether P0
wants any of it, since a query language with no arithmetic pushes that work onto the caller.

---

## Settled — recorded so they aren't reopened

### Intra-row repeated variables — **rejected in Phase 4**, by name

A pattern like `Edge{from = X, to = X}` constrains two fields of the *same* row to be equal,
which needs a same-row `ResidualOp::EqField` — distinct from the cross-level
`EqRegisterField`, because there is no outer register to compare against.

**Decided: reject it for now.** `nyi/repeated-variable`, with a corpus entry and a message
saying what it would need. Deferred rather than *meaningless*, so the code carries the `nyi/`
prefix: the pattern means something perfectly ordinary, and the reason not to support it is
that adding an operator the executor has no other use for buys a machine change for one
construct. `EqField` is additive when something wants it.

The neighbouring case that *is* supported, and is what makes the distinction worth a test:
repeated **reads** of a variable bound at an outer level (`test.Node {id = X}; test.Edge {from
= X, to = X}`) are two ordinary splices — and since every field is then an input, the seek
becomes a point match. Only a repeated *capture* is refused. Flatten detects it structurally:
the second occurrence resolves to a slot on the level currently being emitted
(`flatten::an_intra_row_repeat_is_rejected`).

### Cancellation counts rows *examined* — settled as the book already said

The question was whether "polls every `CANCELLATION_STRIDE` rows"
([chapter 5](05-resume.md#suspend-vs-cancel-vs-terminal-unwind)) meant rows *scanned* or rows
*skipped*. **Decided: examined** — matched or skipped alike, which is what the chapters said
all along; the implementation was the thing that disagreed.

It disagreed because the counter was a local inside a single `StackFrame::next` call, so it
reset on every call and the poll was only reachable while a residual was rejecting rows: **a
plan whose rows all matched never polled the token**, and ran to completion regardless of
cancellation. The count now belongs to the run (`CancellationPoll`, `src/focus/iter.rs`), and
`exec::a_matching_scan_observes_cancellation` is the guard — a scan with no residual, cancelled
mid-run, must stop. The bounded overrun a stride buys (a run shorter than the stride can finish
despite a cancelled token) is the intended trade and is documented on the constant.

Found while writing the [I8](invariants.md#i8) guard, whose cancellation arm needed a
row-rejecting residual to reach a poll — which is why that guard is built the way it is. I8 was
never affected: `enumerate` consumes the executor, so the snapshot is released on the
`Err(Cancelled)` path like any other.

### `pattern = pattern` unification — scope settled at typecheck

The grammar permits `pattern = pattern` (permissive-early, [chapter 7](07-compilation.md)).
The boundary this doc left open is now **decided and enforced in `focus::ty`**, by the shape of
the left-hand side:

| LHS | outcome |
|---|---|
| a variable not yet bound, or `_` | **implemented** — the bind introduces it |
| a literal or string prefix | `reject/bind-lhs` — a literal can never be a target |
| anything else — a bound variable, a generator, an anonymous record, an access | `nyi/bind-unification` |

So the three cases this doc listed as "reject for now" — `var = var` with both bound,
`generator = generator`, `record = record` — are all the third row, and each has a corpus entry
pinning it. LHS-structural-vs-generator by pattern-pushing is *not* implemented and lands with
the rest of unification.

Still deferred, not open: the feature itself.

### `FactRef` marker — resolved (own marker)

**Decision: `FactRef` has its own fixed-width marker.** This is **implemented** in the
codec — `MARK_FACT_REF = 0x51` (a fixed-width band right above the positive-integer band),
with `put_fact_id` on the encoder and a matching decode path (`src/focus/tuple.rs`). So a
value's bytes are self-describing without the schema, and the byte-level `Int`/`Fact`
distinction is enforced. The earlier "share the integer encoding for byte-uniform join
splices" rationale was found overstated (splices work with a distinct marker too). See
[chapter 2](02-tuple-codec.md#the-marker-table).

The engine-side effect (the [Phase 7 gate](../PLAN.md) "resolve `FactRef` before ingesting
fact-typed fields") is satisfied by the marker existing, and `CLAUDE.md` no longer lists it as
open. A fact-typed field is written end to end by the shared fixture (`focus::fixture`), and as
of Phase 5 it is also **queried** end to end: `test.Ref {of = test.Foo {id = 1}}` follows the
reference by splicing the id the marker distinguishes, which is the use the distinct marker was
doubted to support.

### Storage codec vs transport (wire) codec — settled

**One storage (tuple) codec for both keys *and* values** — values are tuple-encoded too, so
queries can eventually *match on values* and `Project::Value` becomes decode-not-copy. It is
FoundationDB-*inspired*, **not** FDB-compatible (don't call it "FDB"). A **distinct
transport/wire codec** applies only to rows *after* they leave the executor (post-yield): a
framed binary format, **not** order-preserving, never touching stored bytes. Lives in
[chapter 3](03-storage-model.md#storage-codec-vs-transport-codec) and
[Operations §6](aperture-cli-design.md).

### Schema compatibility (P0) — settled as subset containment

Compatibility is `old_map ⊆ new_map` — the only compatible change is adding a predicate;
any in-place field change is Breaking until `evolves` exists. Recorded in
[chapter 6](06-types-and-schema.md#compatibility--subset-containment) and
[Operations §7](aperture-cli-design.md). (`evolves` + field-level compatibility is deferred
with the seam kept.)

---

## Deferred, not undecided

Features that are *designed-for and additive* (a new enum arm, a new access kind) aren't
"open decisions" — they have a settled shape and a kept seam, listed in
[`PLAN.md`](../PLAN.md) "Deferred features" and [Operations §11](aperture-cli-design.md):
order comparisons (`ResidualOp` arms), cross-fact navigation (`Access::Fetch`), `evolves`,
cross-DB queries. The two that are *not* additive — derived facts and (now-resolved) the
`FactRef` marker — are handled as deliberate machine/codec changes
([chapter 7](07-compilation.md), [chapter 2](02-tuple-codec.md)).

**Additive is not the same as small**, and five features that were on that list now have
phases of their own to say so: disjunction, `never`, negation and subqueries are
[`PLAN.md`](../PLAN.md) Phase 6b, and unions-as-data is Phase 8. None of them reshapes the
machine — but disjunction extends the resume `Cursor` with a per-branch discriminant, and a
union freezes its discriminants on disk the moment one is written
([I10](invariants.md#i10)), so both need acceptance criteria rather than a bullet.

---

> [← Conventions](conventions.md) · [Index](../README.md) · [Aperture vs Glean →](glean-comparison.md)
