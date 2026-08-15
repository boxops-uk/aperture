# Open decisions

> [Aperture design book](../README.md) · reference doc

What is **not yet settled** — do not treat anything here as an invariant. Below the open
items is a short record of decisions that *have* settled, kept so they aren't re-litigated,
with a pointer to where they now live.

---

## Still open

Every decision this file was *opened* for has settled — the record is below. What replaced them
came from two passes over the whole design. The first two questions are what comparing it against
Glean left ([the comparison](glean-comparison.md) is the analysis); the last two are what an
external audit of the repository found asserted but not decided. Both **gate a phase** rather
than the engine, and both are cheapest to answer before that phase writes anything down. (The
audit found a third — an on-disk format version — which is now
[settled and built](#an-on-disk-format-version--settled-two-numbers-in-db-metadata).)

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
pending `ResidualOp` arm — all four are live (`crates/aperture-engine/src/plan.rs`) — and there is no lexer token
for `<`, `>` or `+` (`crates/aperture-engine/src/lexer.rs`), so `X < 3` is a **parse error**, not a diagnosed
deferral. That is the one deliberate exception to *permissive grammar, narrow later*
([conventions](conventions.md)) and to `aperture_engine::corpus`'s claim to parse the full intended surface,
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

### What a reference *is* in a fact file

**Gates [Phase 7](../PLAN.md).** A stored reference is a final, DB-local
[`FactId`](03-storage-model.md#factid-allocation-i11). An independent producer — an indexer on
another machine, writing a file that will be ingested later — cannot know one. So a fact file's
reference is *something else*, and nothing yet says what.

The tag does not answer this. Per-predicate counters delete the global allocation bottleneck;
they do not delete **reference relocation**, which has three separate causes
([chapter 3](03-storage-model.md#factid-allocation-i11) now says so): two workers on the *same*
predicate still share its counter, dedup at the merge frontier collapses two ids into one, and —
the one specific to this design — **a reference can sit in a key**.

That last one is not a nuance, it is a contradiction in the pipeline as written.
[Operations §5](aperture-cli-design.md) has workers *encode storage tuples and sort* in step 2,
and the per-predicate merge assign ids in step 3. A key holding a reference has no bytes until
the target's id is final, so it has no sort position either: step 2 cannot finish before step 3.

The candidate answers, with what each actually costs:

- **Local ids plus substitution at ingest.** Glean's design, and it works. It is also the
  subsystem the snowflake was said to delete, so choosing this is choosing to build it.
- **Logical `(predicate, key)` references**, resolved to physical ids at ingest, with ingest
  ordered by the reference graph. Sorting still waits on resolution, but only across strata
  rather than within a batch. **A reference in a key cannot be part of a cycle** — the target
  must be identified before the referring key exists — so the stratification this needs is
  well-founded for exactly the case that forces it. Cycles are possible only through *values*,
  where a reference is `nyi/fact-field` and unbuilt.
- **References only to facts already in the target DB.** The cheapest, and it forbids a file
  from carrying a self-contained subgraph.
- **A content-derived stable id.** Removes relocation entirely and brings collision handling and
  cyclic definitions with it.

This also lands on [`ops-I4`](invariants.md#ops-i4): "hash the canonical schema and the base
facts" is underspecified while a base fact contains a physical id, since two reproducible builds
would hash differently for no semantic reason. Either the hash is over a canonical *logical*
graph, or ingest has to guarantee deterministic final ids — which is a stronger promise than
reproducibility alone.

### Re-derivation, and what happens to the high-water mark

**Gates [Phase 8b](../PLAN.md).** Two things the design states are both true and, together,
inconsistent:

- A predicate can be **dropped and replaced wholesale in O(1)** by deleting its two trees, and
  that is named as what re-deriving a derived predicate needs
  ([chapter 3](03-storage-model.md#two-column-families)).
- [I11](invariants.md#i11): a `FactId` is **never reused** within a DB.

The mechanism is what connects them. The allocator's high-water mark is
[recovered from the data](03-storage-model.md#factid-allocation-i11) — the last key in a
predicate's `entities` tree — precisely so that no sidecar counter can go stale. Delete that
tree and the evidence goes with it: the next write to the predicate is sequence 1 again, and
old ids come back naming different rows. Any dependent predicate still holding references to
the dropped ones now points at whatever took their place, silently.

Two coherent answers, and they are not the same size:

- **Re-derivation produces a new DB.** Matches the immutable-artifact philosophy and needs no
  new machinery. It also means a one-predicate fix rebuilds everything.
- **In-place, but bounded.** Legal only on a **Writable** DB (a Complete one is immutable, so
  no cursor or external reference can exist yet), and only for a predicate nothing
  already-written references — which in practice means dropping its dependent subtree with it.
  The derivation graph is already topologically sorted for stratified derivation, so the
  dependency information exists. What is still needed is that the high-water mark survive the
  drop, which the data-recovered mark cannot do on its own.

Anything more permissive than those — re-deriving under live readers — needs persistent
generation metadata, dependent invalidation, and generation-aware cursors and references. That
is a great deal more than "an O(1) tree delete", and the phrase should not be read as promising
it.

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
cancellation. The count now belongs to the run (`CancellationPoll`, `crates/aperture-engine/src/iter.rs`), and
`exec::a_matching_scan_observes_cancellation` is the guard — a scan with no residual, cancelled
mid-run, must stop. The bounded overrun a stride buys (a run shorter than the stride can finish
despite a cancelled token) is the intended trade and is documented on the constant.

Found while writing the [I8](invariants.md#i8) guard, whose cancellation arm needed a
row-rejecting residual to reach a poll — which is why that guard is built the way it is. I8 was
never affected: `enumerate` consumes the executor, so the snapshot is released on the
`Err(Cancelled)` path like any other.

### `pattern = pattern` unification — scope settled, and one case was never unification

The grammar permits `pattern = pattern` (permissive-early, [chapter 7](07-compilation.md)).
The boundary this doc left open is now **decided**, and it fell on the **shape of the left-hand
side** alone — "already mentioned" turned out not to be a property of the query at all:

| LHS | RHS | outcome |
|---|---|---|
| a variable, or `_` | a **fact pattern** | **implemented** — a row bind, an *ordering* question |
| a variable, or `_` | a **constant** (literal, or record of them) | **implemented** — a *substitution*, folded at every use |
| a variable, or `_` | anything naming a **place** (`Y`, `Y.name`, `Y.value`, a subquery) | **implemented** — an *alias*, or a **compare** where both sides are bound elsewhere |
| a variable, or `_` | a **pattern** (`"a"..`) | **implemented** — a *constraint* on the level that binds the left side |
| a record of variables/wildcards | any of the above | **implemented** — a *destructuring*, piece by piece |
| a variable, or `_` | a value in **no register** (`{a = 1, b = Y}`) | `nyi/value-bind` — it would have to be built |
| a literal or string prefix | — | `reject/bind-lhs` — a literal can never be a target |
| a generator, an access, a record with a literal leaf | — | `nyi/bind-unification` — nothing to bind; the pattern would have to be **pushed inward** |

**Rows one to five are the correction**, and the through-line is that none of them was ever
unification. Unification means *two things compared at runtime*; each of these has one thing
already determined, so the answer is where in the plan it comes from:

- `test.Ref {of = P}; P = test.Foo {id = 1}` is one variable named twice by statements written
  in the order that reads before it binds. It needs **reordering**, and `aperture_engine::reorder` does it
  (the runnable frontier, greedily).
- `test.Foo {id = N}; N = 1` says what `N` *is*. It needs **substitution**, and the constant
  fold already did it — in the other order. The fold is collected from the whole body before any
  statement is lowered, so it was order-free all along; only the typecheck gate was not.
- `{a = X, b = Y} = {a = 1, b = 2}` is those two binds written as one. It needs the same
  substitution, applied per field.
- `test.Name X; X = "a"..` says what `X` has to **look like**, not what it is. There is nothing
  to compare and nothing to substitute — a prefix denotes a range, which is why it looked like
  unification — so the answer is to **narrow where `X` already lives**, and the level that
  captures it does so. That makes it the same range scan `test.Name "a"..` is, with a name for
  the answer ([chapter 7](07-compilation.md#what-a-bind-can-mean)).

In every case both spellings compile to the **same plan**, which is what the paired corpus
entries pin. `nyi/bind-unification` no longer means "two runtime values" — `X = Y` with both
bound is built, as a residual on the level that binds later, and `X = Y.name` is an alias. It
means the **left side is not a target**: a generator or a field read, where there is no variable
to bind and the pattern would have to be pushed inward. That is a different operation from
binding, and the only part of `pattern = pattern` still without an answer.

Three consequences worth keeping in view:

- **Typecheck no longer decides which statement binds a variable — or *whether* one does.**
  Deciding it there is deciding it in source order, the one order the query might not have used,
  and the gate that asked "has this been mentioned above" was the last copy of the decision
  `reorder` took over: `F = G` compiled or was refused depending on where the statement
  mentioning `G` was written, for the same query and the same plan. Typecheck checks that the
  types agree and stops.
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

Of the three cases this doc originally listed as "reject for now", two are now built: `var = var`
with both bound is a residual on the level that binds later, and `record = record` destructures
against whatever the right side names. `generator = generator` is not, and neither is a field
read on the left — LHS-structural-vs-generator **by pattern-pushing** is what is left of the
feature, and each half has a corpus entry pinning it.

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
with `put_fact_id` on the encoder and a matching decode path (`crates/aperture-encoding/src/tuple.rs`). So a
value's bytes are self-describing without the schema, and the byte-level `Int`/`Fact`
distinction is enforced. The earlier "share the integer encoding for byte-uniform join
splices" rationale was found overstated (splices work with a distinct marker too). See
[chapter 2](02-tuple-codec.md#the-marker-table).

The engine-side effect (the [Phase 7 gate](../PLAN.md) "resolve `FactRef` before ingesting
fact-typed fields") is satisfied by the marker existing, and `CLAUDE.md` no longer lists it as
open. A fact-typed field is written end to end by the shared fixture (`aperture_store::fixture`), and as
of Phase 5 it is also **queried** end to end: `test.Ref {of = test.Foo {id = 1}}` follows the
reference by splicing the id the marker distinguishes, which is the use the distinct marker was
doubted to support.

### An on-disk format version — settled: two numbers in DB metadata

**Settled and built** as [I15](invariants.md#i15) —
[chapter 3](03-storage-model.md#the-format-stamp-i15) is where it now lives. The question was
never *whether*, only where the field lives and what it covers, and both halves are answered:
it lives in a **metadata keyspace** (`meta`, the same block the embedded schema will use when
[I13](invariants.md#i13) lands), and it is **two numbers, separately** — `codec` for the marker
table and per-type encodings, `storage` for row framing, keyspace naming and the `FactId`
split. They move for different reasons, so one number would refuse a database over a change
that cannot affect it.

The rule is **equality**: a build reads exactly what it writes, and an unstamped database
holding facts is refused rather than adopted. "Readable up to N" is the additive refinement,
deliberately not taken while there is no past encoding to make it about.

What it does *not* do is make anything migratable. [I3](invariants.md#i3) still binds every
database stamped `codec 1`; what changed is that a future codec is now a different number
rather than an impossibility, which is what a migration would need and could never have had.
Taken now because the cost was twelve bytes and a check at open, and every unwritten feature —
arrays, unions, stored schemas, operational metadata — would otherwise have landed more
encoding behind a door with no handle.

The **resume cursor** carries its own version, on a separate counter, for the same reason and
against the build rather than the database
([chapter 5](05-resume.md#the-cursor--bytes-nothing-else)).

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
above), `evolves`, cross-DB queries. The two that are *not* additive — derived facts and (now-resolved) the
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
