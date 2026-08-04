# Open decisions

> [Aperture design book](../README.md) · reference doc

What is **not yet settled** — do not treat anything here as an invariant. Below the open
items is a short record of decisions that *have* settled, kept so they aren't re-litigated,
with a pointer to where they now live.

---

## Still open

### Cancellation is polled per *skipped* row, not per row scanned

[Chapter 5](05-resume.md#suspend-vs-cancel-vs-terminal-unwind) says the scan loop polls the
cancellation flag "every `CANCELLATION_STRIDE` rows." The implementation resets its counter on
every `StackFrame::next` call, so the poll is only reached when a *single* `next()` skips
`CANCELLATION_STRIDE` rows — i.e. only while a residual is rejecting rows. **A plan whose rows
all match never polls the token at all** and runs to completion regardless of cancellation.

Found while writing the [I8](invariants.md#i8) guard, whose cancellation arm needs a
row-rejecting residual to reach a poll — which is why that guard is built the way it is. I8
itself is unaffected: `enumerate` consumes the executor, so the snapshot is released on the
`Err(Cancelled)` path like any other.

Open: whether "every N rows" should mean rows *scanned* (move the counter into the frame, or
into `enumerate`'s loop) or rows *skipped* is genuinely the intended semantics. A streaming
query that a client has abandoned is the case that argues for the former.

### Intra-row repeated variables — `EqField` vs reject

A pattern like `Edge{from = X, to = X}` constrains two fields of the *same* row to be equal.
That needs a same-row `ResidualOp::EqField` (distinct from the cross-slot
`EqRegisterField`), **or** an explicit rejection. Decide the P0 scope in Phase 4
([`PLAN.md`](../PLAN.md) task 4d) — either tested `EqField` semantics or a tested rejection
diagnostic.

---

## Settled — recorded so they aren't reopened

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

### `FactRef` marker — resolved (own marker), needs housekeeping

**Decision: `FactRef` has its own fixed-width marker.** This is **implemented** in the
codec — `MARK_FACT_REF = 0x51` (a fixed-width band right above the positive-integer band),
with `put_fact_id` on the encoder and a matching decode path (`src/focus/tuple.rs`). So a
value's bytes are self-describing without the schema, and the byte-level `Int`/`Fact`
distinction is enforced. The earlier "share the integer encoding for byte-uniform join
splices" rationale was found overstated (splices work with a distinct marker too). See
[chapter 2](02-tuple-codec.md#the-marker-table).

> **Housekeeping:** `CLAUDE.md §7` still lists this as an open decision ("until implemented,
> fact-typed fields encode via `put_u64` and decode requires the schema type"). That text is
> **stale** — reconcile it when `CLAUDE.md` is refactored to point into this book. The
> engine-side effect (the [Phase 7 gate](../PLAN.md) "resolve `FactRef` before ingesting
> fact-typed fields") is now satisfied by the marker existing.

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
order comparisons (`ResidualOp` arms), cross-fact navigation (`Access::Fetch`), unions-as-
data then the disjunction union-of-streams operator (with a per-branch `Cursor`
discriminant), negation/subqueries, `evolves`, cross-DB queries. The two that are *not*
additive — derived facts and (now-resolved) the `FactRef` marker — are handled as deliberate
machine/codec changes ([chapter 7](07-compilation.md), [chapter 2](02-tuple-codec.md)).

---

> [← Conventions](conventions.md) · [Index](../README.md) · [Glossary →](glossary.md)
