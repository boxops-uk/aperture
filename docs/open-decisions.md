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
element**. This was framed as a choice between the two spellings, and that framing was wrong:
**Glean does both, deliberately, for the same data** — the indexer writes one compact
array-bearing fact per container, and a `stored` derived predicate explodes it with `[..]` into
one fact per element to get the seekable index. Arrays are the dominant representation in its
shipped schemas (several hundred array-typed fields, ~150 uses of `[..]`, `stored` derivation
used pervasively), and its docs call adding an index that way "common practice". The evidence and
its citations are [in the ledger](glean-comparison.md); this file records the decision.

**Still open** — and still a call about **how every schema is written**, not just about what a
type can hold. What the comparison settled is the *shape* of any answer, three constraints deep:

- **Prefer the value side.** [I6](invariants.md#i6) keeps values out of the scan loop, so an
  array on the *value* side of a key→value predicate costs a scan nothing — it is decoded at
  projection or not at all. This is a place Aperture **beats** Glean rather than copying it:
  Glean recommends key→value for large values and then barely uses it (one array-valued
  predicate in its whole corpus).
- **Forbid or diagnose an array in a leading key field.** A length-prefixed array cannot be
  prefix-matched — Glean says so outright, "MatchArrayPrefix doesn't actually look at a prefix
  because arrays encode their length at the front"
  (`glean/db/Glean/Query/Reorder.hs:794-796`) — so an array early in a key silently closes the
  seek prefix for every field after it. A **fully determined** array can still join the prefix;
  a partial one cannot. Silent is the problem: `:plan` shows the shape, but there is no cost
  model to say it is the wrong one.
- **It couples to [Phase 8b](../PLAN.md).** Glean's array story works *because* `stored`
  derivation exists to build the exploded index. Arrays without it ship the storage win and none
  of the query mitigation, and schema authors route around the gap — Glean's own schemas carry
  two admissions of exactly that ("this is an example of where efficient set membership would be
  useful"; "very hard at the moment to build set or list facts dynamically").

One warning to carry either way, because it lands on
[`ops-I4`](invariants.md#ops-i4) reproducibility: **order-free data
in an array is non-deterministic** — the writer picks an order the data does not have. Glean's
answer is `set T`, an array kept sorted and deduplicated. That is a separate and deferrable
question (7 uses in all of Glean, which also treats array↔set as a *compatible* change), but it
is the reason not to reach for `[T]` for a bag.

The codec reserves marker bands, so adding `[T]` later is not a one-way door in the *encoding*.
Writing every schema without it is the thing that gets expensive to undo. **Decide before the
schema DSL fixes what can be written** ([`PLAN.md`](../PLAN.md) Phase 8), and record the answer
here either way. Related, and easier: `bool` and `maybe T` are sugar over a union once unions
land; `nat`/`byte` are a range question, not a shape one.

### Primitives in the query language

Angle's primitive surface is **narrower than "arithmetic, string operations, comparisons" sounds**:
exactly 15 `prim.*` ops — arithmetic is `+` on nat *only*, string functions are `toLower` and
`reverse` *only*, comparisons are nat-only plus a generic `!=`, and the rest are container and
byte-span helpers ([the ledger](glean-comparison.md#primitives-expressions-and-aggregation) cites
them). That makes this decision **smaller than it looks**. What Angle has that focus has no answer
to at all is
**if-then-else** and **element iteration over an array or set** (`X[..]`) — and the second is the
multiplicity decision above, not this one. focus has string prefix matching and nothing else.

**Order comparisons are not "deferred with a seam", and this file said they were.** There is no
pending `ResidualOp` arm — all four are live (`src/focus/plan.rs`) — and there is no lexer token
for `<`, `>` or `+` (`src/focus/lexer.rs`), so `X < 3` is a **parse error**, not a diagnosed
deferral. That is the one deliberate exception to *permissive grammar, narrow later*
([conventions](conventions.md)) and to `focus::corpus`'s claim to parse the full intended surface,
and it is recorded here because "deferred with a seam" hid it.

Arithmetic, string functions and conditionals are in neither place: not built, not deferred,
not ruled out. The seam that would carry them **does exist**, once they lex: Phase 6 built derived
binds (a pure function of the fact bindings, [I14](invariants.md#i14)), and a primitive would be an
arm of `Computed` — Glean's `PrimCall` is the same shape, a one-row generator. So the engine half
is additive whenever it is wanted, and the grammar half is a token and a production — the open
question is whether P0 wants any of it, since a query language with no arithmetic pushes that work
onto the caller.

Worth knowing when deciding: a primitive would be the **first thing in the language to produce a
`Step::Derive` at all**. A constant bind folds instead, so the machinery is currently exercised
only by hand-built plans — which means the first primitive is also the first real test of it.

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

### `pattern = pattern` unification — scope settled, and one case was never unification

The grammar permits `pattern = pattern` (permissive-early, [chapter 7](07-compilation.md)).
The boundary this doc left open is now **decided**, by the shape of the left-hand side and —
for a variable already mentioned — the shape of the right:

| LHS | RHS | outcome |
|---|---|---|
| a variable not yet bound, or `_` | anything bindable | **implemented** — the bind introduces it |
| a variable already mentioned | a **fact pattern** | **implemented** — a row bind, an *ordering* question |
| a variable already mentioned | a **constant** (literal, or record of them) | **implemented** — a *substitution*, folded at every use |
| a record of variables/wildcards | a **constant** | **implemented** — a *destructuring*, folded piece by piece |
| a variable already mentioned | anything else | `nyi/bind-unification` |
| a literal or string prefix | — | `reject/bind-lhs` — a literal can never be a target |
| a generator, an access, a record with a literal leaf | — | `nyi/bind-unification` |

**Rows two to four are the correction**, and the through-line is that none of them was ever
unification. Unification means *two things compared at runtime*; each of these has one thing
already determined, so the answer is where in the plan it comes from:

- `test.Ref {of = P}; P = test.Foo {id = 1}` is one variable named twice by statements written
  in the order that reads before it binds. It needs **reordering**, and `focus::reorder` does it
  (the runnable frontier, greedily).
- `test.Foo {id = N}; N = 1` says what `N` *is*. It needs **substitution**, and the constant
  fold already did it — in the other order. The fold is collected from the whole body before any
  statement is lowered, so it was order-free all along; only the typecheck gate was not.
- `{a = X, b = Y} = {a = 1, b = 2}` is those two binds written as one. It needs the same
  substitution, applied per field.

In every case both spellings compile to the **same plan**, which is what the paired corpus
entries pin. What is left for `nyi/bind-unification` is exactly the set with two runtime values
and nothing to substitute: `X = Y` with both bound, `X = Y.name`, `X = "a"..`, and
generator-against-generator.

Three consequences worth keeping in view:

- **Typecheck no longer decides which statement binds a variable.** Deciding it there is
  deciding it in source order — the one order the query might not have used. Typecheck checks
  that the types agree and stops.
- **Claiming one variable twice is still unification, and flatten owns it.** Two rows (`X =
  test.Foo {id = 1}; X = test.Foo {id = 2}` — *these two facts are the same fact*) or two
  constants (`Y = 1; Y = 2`). Only flatten knows whether a variable is already a row or a
  constant rather than a capture, and it decides from the whole statement list so that this too
  is order-free. Typecheck used to catch both incidentally; that they now have an owner is the
  load-bearing half of the change — flatten's query generator assumes the row half, and
  `lookup` walks bindings in reverse, so the constant half would have silently kept the *last*.
- **A literal leaf on the left of a destructuring is refused, and that is not conservatism.**
  `{a = 1} = {a = 2}` typechecks (both sides are `int`) and binds nothing, so accepting it would
  emit no constraint and mean `true` where it means the empty relation. `Ast::is_destructurable`
  is the gate and a named guard test is the proof. A *wildcard* leaf is fine by the same
  reasoning read backwards: it binds nothing, but it also cannot fail.

The three cases this doc originally listed as "reject for now" — `var = var` with both bound,
`generator = generator`, `record = record` — are unaffected, and each still has a corpus entry
pinning it. LHS-structural-vs-generator by pattern-pushing is *not* implemented and lands with
the rest of unification.

Still deferred, not open: the feature itself. What Glean does here is worth knowing — it does
**both**, in two passes: `Glean/Query/Opt.hs` ("Note [Query Simplification]") unifies every
`P = Q` and applies the substitution, and `Glean/Query/Reorder.hs` orders statements so
variables are bound before use. Only the second has an equivalent here. Note also that Opt's
"Dealing with Choice" section concludes unification must be **branch-local** — a variable
visible outside a branch must not be unified at all — which is a rule to import alongside it if
unification ever lands after disjunction (Phase 6b).

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
order comparisons (a *new* `ResidualOp` arm — plus a token, since they do not lex today; see
above), cross-fact navigation (`Access::Fetch`), `evolves`, cross-DB queries. The two that are *not* additive — derived facts and (now-resolved) the
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
