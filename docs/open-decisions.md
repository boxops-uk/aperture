# Open decisions

> [Aperture design book](../README.md) · reference doc

What is **not yet settled** — do not treat anything here as an invariant. Below the open
items is a short record of decisions that *have* settled, kept so they aren't re-litigated,
with a pointer to where they now live.

---

## Still open

Every decision this file was *opened* for has settled — the record is below. What replaced them
came from two passes over the whole design. The first is what comparing it against Glean left
([the comparison](glean-comparison.md) is the analysis); the second is what an external audit of
the repository found asserted but not decided. It **gates a phase** rather than the engine, and is
cheapest to answer before that phase writes anything down. (**Multiplicity** was the third and is
now [settled](#multiplicity--settled-one-fact-per-element-for-now-diagnosed-by-name), taken with
the rest of Phase 8's one-way doors.) (The audit found two
others: an on-disk format version, now [settled and
built](#an-on-disk-format-version--settled-two-numbers-in-db-metadata), and what a reference is
in a fact file, [settled when Phase 7 was
sequenced](#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline).)

### Primitives in the query language — **settled and built** (Phase 11)

**Comparisons and arithmetic are in the language.** `<`, `<=`, `>`, `>=` are statements;
`+` and `-` are expressions binding tighter than `|` and looser than access. What follows is
kept as the record of the decision.

A comparison is a **residual** where one side is a field of a row, and a byte compare rather
than a decode: the key encoding is order-preserving ([I1](invariants.md#i1)), so the
lexicographic order of two encoded fields of one type *is* their value order. Three shapes —
against a constant, against another register's field, against another field of the same row —
and which one is used is decided by *address* rather than by syntax, the relation flipping
where the field turned out to be on the right. Where neither side is a row it is a
`Step::Test` instead, which is what a filter with no row of its own is.

Arithmetic is integers, wrapping, and it is **the first thing in focus to lower a
`Step::Derive` at all** — the machinery Phase 6 built and only hand-written plans had
exercised. `Computed` grew from one arm to four; nothing about the machine moved.

What is still absent: **if-then-else**, and a **sargeable** comparison. The second is worth
knowing about — an order comparison on a leading key field denotes one contiguous run of the
key order, unlike a denial, so unlike `NotPrefix` there *is* a seek form to look for later.
It is not built.

### Primitives in the query language — the decision as it stood

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

### Multiplicity — settled: one fact per element for now, diagnosed by name

**Decided while planning [Phase 8](phase-8-schemas.md), which is where this file said to decide
it**: *"decide before the schema DSL fixes what can be written, and record the answer here either
way."*

`PredicateTy` gains no `[T]`. The schema DSL **parses** an array type and reports `nyi/array`, so
the refusal names the decision rather than reading as a parse error — permissive-early, as
[conventions](conventions.md) has it. One fact per element stays the way a one-to-many is written.

The reasoning is the one this file already recorded, and the deciding weight was the third point:
Glean's array story works *because* `stored` derivation exists to explode an array into a seekable
index, and that is [Phase 8b](../PLAN.md). Adding arrays before it ships the storage win and none
of the query mitigation, which is exactly the position Glean's own schemas carry two written
admissions about. The codec reserves the marker band, so this is not a one-way door in the
*encoding*; what it defers is a one-way door in how every schema is written, and that is the thing
8b makes cheap rather than expensive.

Unchanged and still true: prefer the value side ([I6](invariants.md#i6) keeps it out of the scan
loop), forbid or diagnose an array in a leading key field (a length-prefixed array cannot be
prefix-matched), and `set T` is separately deferrable. `bool` and `maybe T` remain sugar over a
union once unions land.

### A client never computes a fingerprint — settled while planning Phase 8

Recorded because the first plan said the opposite, and because it is the kind of assumption
that quietly costs every future client a port.

A schema fingerprint is a hash over a canonical form ([chapter 6](06-types-and-schema.md)), and
the .NET client computes one today — so the plan budgeted "both .NET schema statements have to
compute the new fingerprint" as real work. **Glean does not ask that of a client.** Its
`glean.thrift` says at the definition: *"The `SchemaId` for the current schema can be obtained
at compile time from `schema_id` in the generated `builtin.thrift` file"*, its schema compiler
emits the constant, and it generates client bindings for seven languages from the same schema.

So: **a client carries the number rather than deriving it.** `aperture schema fingerprint`
prints it, a client holds it as a constant, and a stale one fails the handshake loudly — which
is what the assertion is for. What that constant is, precisely, is a *provenance* tag rather
than a checksum of the shapes a hand-written client implements; the byte-identical golden is
what guards those, and it is the stronger check.

**Generating the client** — Glean's answer, which makes provenance and shapes agree by
construction — is the proper end state and is deliberately not Phase 8's. It would also end the
argument the two independent statements exist to make, so the golden's role has to be rethought
in the same breath.

### Predicate ids — settled: they belong to the database, not to the schema text

**Decided while planning [Phase 8](phase-8-schemas.md)**, and worth recording here because it was
never an open question in this file — it was an unexamined assumption, and examining it changed
the answer.

A `PredicateId` is a position in the schema *and* the 24-bit tag inside every
[`FactId`](03-storage-model.md), so three agreed requirements meet on it: reproducibility
(`ops-I4`), layout-independent identity ([I13](invariants.md#i13)'s guard), and "adding a predicate
is compatible" (subset containment). No assignment that is a function of the schema **text**
satisfies all three — declaration order breaks the second, sorted-by-name breaks the third, and a
24-bit hash of the name collides at ~3% by a thousand predicates.

**The answer is that it need not be a function of the text at all.** Glean splits identity from the
physical tag: `PredicateId` is a content hash with no number in it, while `Pid` is a small integer
assigned by sorted name, **persisted in the stored schema**, and append-only afterwards
(`nextPid = max + 1`), so a database keeps its numbering for life. Aperture takes the same split —
no ids in the DSL, the map embedded in the database at create.

**And the wire carries names, so the database's numbering never leaves it.** A predicate id is
encoded in exactly one place — the block header, once per run of facts; `WireFact::predicate` is
not encoded at all, and a nested fact takes its predicate from the declared field type. Sending a
fully-qualified name there instead costs about six bytes per block and removes the whole problem:
a client never learns the database's numbering, a fact file is portable to any database whose
schema declares those names, and the id map needs no cross-checking at ingest because there is no
id to disagree about. The database's ids become purely internal, which is what makes "the id
belongs to the database" the plain answer rather than a trade.

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

### What a reference *is* on the way in — settled: the target fact, written inline

**Settled: a reference a producer sends is the target fact itself**, nested in full — its key,
and its value side if the predicate declares one — and ingest **interns** it: resolve the key
against the target predicate, take the id if it is already there, allocate the next sequence if
it is not, and substitute. A producer that already holds a [`FactId`](03-storage-model.md#factid-allocation-i11)
may send that instead, so the wire form is *id or nested fact*; stored, a reference is a
`FactId` and nothing else. This is a **transport** decision only — it changes what a producer
sends, never what is on disk. Lives in
[chapter 3](03-storage-model.md#interning-a-nested-fact) and
[Operations §6](aperture-cli-design.md#6-wire-protocol--the-write-stream).

**The reason is the producer, not the pipeline.** Every id-based answer — local ids plus a
substitution table (Glean's), logical `(predicate, key)` references, ids that must already exist
in the target — makes the *indexer* keep a map from every entity it has seen to the identity it
was assigned, and emit in an order that respects it. That is bookkeeping proportional to the
whole index, carried in the producer, for a target the producer is holding anyway. An indexer
walking a syntax tree knows the file when it reaches the declaration; nesting lets it say so
where it stands.

It also makes the two directions one spelling. `Knows { from = Person { id = 1 } }` has been how
a traversal is *written* since Phase 5 ([the comparison](glean-comparison.md#the-idiomatic-spelling-of-a-join--closed));
it is now how a fact is *sent*.

What made this look hard was a pipeline problem, and it is narrowed rather than solved:
[operations §5](aperture-cli-design.md) has parallel workers encode storage tuples **and sort**
in step 2, while ids are assigned at the step-3 merge — and a key holding a reference has no
bytes, so no sort position, until then. Interning does not remove that ordering; it names it. On
a **write stream** the ordering is free, because there is one writer consuming one stream and
interning as it goes, which is why [Phase 7](../PLAN.md) does the wire first. In the parallel
**file** pipeline it becomes a pre-pass or a stratum boundary — a Phase 7b question, asked with
the interning primitive already built and tested.

Three things fall out, and each is load-bearing rather than incidental:

- **Interning is the dedup rule already specified**, not a second one. A nested target occurring
  under a thousand parents is one row, written once; `ops-I5` already says byte-identical facts
  dedup silently at the frontier.
- **A nested fact both names and defines its target**, so a nested value that disagrees with a
  target already present is exactly the **same-key-different-value** conflict `ops-I5` rejects.
  No new rule, and the rejection stays order-independent.
- **The walk terminates and is well founded.** A nested fact is a finite tree, interned
  bottom-up. And the case that forces resolution before sorting — a reference in a *key* —
  cannot be part of a cycle: the target must be fully identified before the referring key has
  any bytes at all. Cycles are reachable only through *values*, where a reference is
  `nyi/fact-field` and unbuilt.

**It also closes the [`ops-I4`](invariants.md#ops-i4) underspecification.** "Hash the canonical
schema and the base facts" could not be taken literally while a base fact contained a physical
id — two reproducible builds would hash differently for no semantic reason. With references sent
as nested facts, a DB has a canonical *logical* form: expand every reference to its target's
key, recursively, and no physical id appears. The hash is over that, and reproducibility no
longer requires the strictly stronger promise that ingest assign identical physical ids.

**The cost, stated plainly, because it is the real one.** A reference costs the target's whole
fact on the wire rather than eight bytes, and a producer emitting the same target under a
thousand parents sends it a thousand times. That is the trade for deleting the producer's
bookkeeping, and it is the right one for P0: the wire is framed and compressible, and a producer
that *does* hold ids may send them. Block-local back-references — naming a fact by its ordinal
earlier in the same block — are the obvious compaction, and are deliberately **not** in P0: they
are a pure encoding win over a semantics that is now decided, so they can be added without
changing what a reference means.

### Storage codec vs transport (wire) codec — settled, and it runs both ways

**One storage (tuple) codec for both keys *and* values** — values are tuple-encoded too, so
queries can eventually *match on values* and `Project::Value` becomes decode-not-copy. It is
FoundationDB-*inspired*, **not** FDB-compatible (don't call it "FDB"). A **distinct
transport/wire codec**: a framed binary format, **not** order-preserving, never touching stored
bytes. Lives in [chapter 3](03-storage-model.md#storage-codec-vs-transport-codec) and
[Operations §6](aperture-cli-design.md#6-wire-protocol--the-write-stream).

**Amended when Phase 7 was sequenced:** this entry was written read-only-shaped — "applies only
to rows *after* they leave the executor (post-yield)" — because the read path was the only one
that existed. The transport codec is **bidirectional**: rows out, and facts in. The two
directions share a value encoding and differ in one thing, which is the whole of why the
amendment is worth recording rather than quietly stretching the old wording — **only the inbound
direction has a reference that is not an id**, since a fact on its way in may nest its target
([above](#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline)) and a row
on its way out has been read from storage, where a reference is a `FactId` already.

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
