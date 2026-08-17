# Aperture — build plan

The living phase tree for taking *Aperture DB* and **focus** from prototype to production.
This file owns the **sequence**; the *design* is ground truth in the [design book](README.md)
and the [invariant registry](docs/invariants.md); the *working rules* live in
[`CLAUDE.md`](CLAUDE.md). Each phase references the design rather than restating it.

**Definition of done, everywhere:** a task ends in a **green test** (prefer a property —
[`docs/testing.md`](docs/testing.md)), and **every invariant a phase lists as "makes green"
must have its [guard](docs/invariants.md) un-ignored and passing** before the phase is done.

**How to use this tree.** Phase order and dependencies are owned by the maintainer. When
picking up a phase, decompose its next unstarted node into task-sized leaves *at pickup*
(early decomposition is always wrong), each ending green, ordered by dependency and
de-risking. Keep diffs small and reviewable. Phases 0–4 are decomposed to task granularity
as a template; later phases are coarser and get decomposed when reached.

**Terminology.** "**Phase N**" is a build step in this plan. "**P0**" (used in the design
docs) is the *product-scope milestone* — the first shippable feature set — and is a
different thing; don't conflate them.

**Per-phase template.** Each phase below has: **Goal · Depends on · Design of record
(pointers — do not restate) · Invariants in scope · Tasks · Acceptance (checklist).**

---

## Current state, honestly

The engine spine exists in `crates/aperture-engine/`:

- **Codec** (`tuple.rs`) — heavily property-tested: order-preservation, round-trip, and
  skip are covered, and the golden marker table now pins the on-disk values
  ([I1](docs/invariants.md#i1), [I2](docs/invariants.md#i2), [I3](docs/invariants.md#i3)
  green). Named `arb_*` strategies live in `tuple::proptest`.
- **Executor + resume** (`iter.rs`, `plan.rs`) — implemented and guarded: a happy-path
  battery over hand-built plans, mechanical NFR guards, and the tier-3 resume battery
  (`exec::resume_equals_uninterrupted`) over schema-first `(plan, store)` pairs from
  `plan::proptest`, **run against both `MemStore` and fjall** (the fjall arm is also
  differential — the two stores must agree row for row and id for id).
  [I4](docs/invariants.md#i4)–[I9](docs/invariants.md#i9) green. `enumerate` consumes the
  executor, so releasing the snapshot at every stop is structural
  ([I8](docs/invariants.md#i8)).
- **Front end** — `lex → parse → lower → typecheck` is live in `crates/aperture-engine/` (Phase 2 done):
  the full intended surface parses, lowers to the `SyntaxTree` store, and typechecks, with
  every construct deferred to a later phase drawing one specific diagnostic naming it. Three
  acceptance artifacts: `aperture_engine::corpus` (the audit table as data, with a parse gate and a
  diagnostic-code gate over it), **`parse ∘ print == id` on generated trees** (`aperture_engine::print`
  renders a tree back to focus source, so the corpus is worked examples rather than the whole
  specification), and **a node's span is where the printer emitted it** — the half spans are
  checkable at all, since a tree comparison is blind to them.
- **Flatten → reorder** (`flatten.rs`, `reorder.rs`, Phase 4 done) — `Compilation::plan`
  produces a runnable `Plan`, so the two halves of the system meet. Sargeability builds seeks,
  splices and residuals over the chosen order; `reorder` emits the **greedy runnable frontier**
  over a graph of *variables* (not edges between statements — which statement captures a shared
  variable depends on the order, so edges would forbid correct orders), which makes it
  load-bearing for **acceptance** and not only for speed: a written order that reads a variable
  the statement after it binds has a perfectly good plan, and this is what stops it being
  refused. Greedy is complete because the constraint is monotone, and `Deps::antichains` is kept
  **off** that path — it is the feasibility answer and the independent witness the completeness
  property is checked against. The headline gate is tier-3: a generated `(query, store)` pair run
  against a nested-loop model, in **every** permutation of the body. What flatten defers has a
  code and a corpus entry each
  ([chapter 7](docs/07-compilation.md#what-flatten-defers-and-why)). Phase 5 added **following
  a fact reference** (a fact-id splice and compare) and **hoisting a nested generator**, which
  retired the last of `src/lens/`.
- **The compilation driver** (`aperture_engine::compile`, Phase 3 done) — one `Compilation` carrying
  the source, schema, interner, diagnostics sink and the trees the phases produce. A phase
  reports by pushing into the sink and cannot return diagnostics; codes are a `Code` enum
  rather than strings; rendering sorts into source order while the sink keeps arrival order.
- **A focus shell** (`src/main.rs`, Phase 5 done) — reads a query, highlights it from the
  compiler's own lexer, **compiles it through the driver and runs it** against a real `FjallDb`
  seeded from `aperture_store::fixture`; `:plan` shows the plan without running it and `:facts` scans a
  predicate. Its database is the corpus's, so anything the corpus classifies `Supported` is
  typeable at the prompt and returns the rows recorded there.
- **Store** (`store.rs`) — the fjall store is complete and guarded (Phase 1 done): a pair of
  keyspaces per predicate (`keys.<id>`, `entities.<id>`), `scan`/`point`, and an atomic
  `put_fact` over a snowflake [`FactId`](docs/03-storage-model.md#factid-allocation-i11) with
  a per-predicate allocator recovered from the data. Held to `MemStore` as a differential
  oracle. [I8](docs/invariants.md#i8), [I11](docs/invariants.md#i11),
  [I12](docs/invariants.md#i12) green — the I12 crash case aborts a child process mid-write,
  and the I8 guard cross-checks a drop probe against fjall's own open-snapshot count.
- **One fixture database** (`aperture_store::fixture`) — the schema, the facts and the example queries
  the corpus, the batteries and the shell all share, deliberately not test-gated. Before it
  there were two databases and a corpus entry was not something a person could run.
- **Writing a fact by hand** (`aperture_store::fact`) — `FjallDb::put(&schema, &fact)` takes a
  **well-typed value** whose key fields are *named* and resolved against the schema, because
  `put_fact` takes bytes and three of its preconditions fail **silently**: a stored key is
  flat, field order is the schema's declaration order, and only the schema says whether a
  predicate has a value side at all. Each one writes a fact that is simply never found. The id
  `put` returns **is** what a reference to that fact is, so referential integrity follows from
  write order rather than a check ([I11](docs/invariants.md#i11)). Deliberately *not* bulk
  ingestion (a `Value` per fact) and *not* a `serde` derive — both recorded in
  [chapter 3](docs/03-storage-model.md#writing-a-fact-by-hand). This is the seam Phase 7's
  fact-file path builds **beside**, not on.
- **Dynamic derivation** (`iter.rs`, `plan.rs`, `flatten.rs`, Phase 6 done) — a register holds a
  `Slot` (a stored row **or** a computed value), and a plan's body is a sequence of `Step`s (a
  scan to iterate, or a value to compute) rather than a list of levels. `body.len()` counts
  steps; `Plan::levels()` counts loops, and the difference is load-bearing — a `Cursor` holds one
  row per *level*, and a derive step is recomputed on restore instead of saved
  ([I14](docs/invariants.md#i14)). A **constant bind folds** rather than becoming a step, so
  `X where X = 42` compiles to no steps at all and a plan with no levels is the unit relation.
  Nothing in focus lowers a derive step yet: the machinery is exercised by hand-built plans, and
  its first producer will be a primitive or a subquery.
- **The deferred query surface** (`flatten.rs`, `ty.rs`, `iter.rs`, `plan.rs`, Phase 6b) — the four
  constructs that parsed and typechecked-as-deferred now compile: **disjunction** is a level with a
  source per branch, **`never`** is a level with none, a **subquery** inlines, and **negation** is
  a `Step::Test` — a filter that binds nothing, takes no cursor entry, and whose variables are
  *reads*, which is the whole of the placement rule Datalog states separately. The machine grew one
  step kind and no control flow. What each construct still refuses is narrower than the code that
  reports it suggests, and each refusal has a corpus entry.
- **Unbuilt:** bulk ingestion, schema parsing, **stored** derivation (gated on the schema DSL —
  [Phase 8b](#phase-8b--stored-derivation)), the wire protocol, and the operational layer.
  `schema.rs` holds Phase 8's guards, written up front and `#[ignore]`d — the only pending
  entries left in the coverage ledger.

Module map: [chapter 1](docs/01-concepts.md). Nothing here contradicts the design docs.

---

## Dependency graph

```
0  guard matrix & harness ─┬─▶ 1  fjall store ✅ (I8, I11, I12 green; resume battery re-run on fjall)
                           │
                           └─▶ 2  grammar ✅ ─▶ 3  driver ✅ ─▶ 4  flatten/reorder ✅ ─┬─▶ 5  REPL ✅  (→ remote-only later)
                                                                                    ├─▶ 6  dynamic derivation ✅ (machine change; I14 green)
                                                                                    │      └─▶ 6b  deferred query surface ✅ (`|`, never, `!`, subquery)
                                                                                    └─▶ 7a wire ingestion ✅ ─▶ 9  operations ─▶ 8  schema (+ union types) ─▶ 8b  stored derivation
                                                                                                             └─▶ 7b  file ingestion (deferred past 9)

Cross-edges:  6 also depends on the resume battery (0) + fjall (1).   7 depends on 1 (store + atomic put_fact).
              8b (stored derivation) is gated on **8**: a derived predicate cannot be built before it can be
              declared. It needs 7 to write through and 6 to run the query, and shares ops-I8's lifecycle with 9.
              It needs nothing from 6's machine change — a stored derived predicate is facts, scanned like any other.
              6b depends on 6 for the **Cursor**: it is `Vec<Register>` today and `Vec<Slot>` after 6, and
              disjunction adds a per-branch discriminant to the same token — edit it once, re-prove I4 once.
              Nothing in 7–9 depends on 6b, so its position *before* 7 is a choice, not a constraint.
              **9 was resequenced ahead of 7b and 8** — see the ordering principle. It depended on
              "7–8"; with the schema hardcoded, 8 stops being a dependency and only 7a is needed.
Gates:        Codec I1–I3 green.  Executor I4–I7, I9 green on MemStore.  I8/I11/I12 at Phase 1.  I10/I13 at Phase 8.
              FactRef marker — resolved (own marker 0x51, already in the codec); no longer gates ingestion.
              Union types gate on the schema DSL (8), not on 6b: a union cannot be declared before it can be written down.
```

The **`Plan` IR is the fixed point** everything aims at ([chapter 4](docs/04-executor.md)):
the front end (2–4) produces it, the executor (already built) consumes it, and the two
halves progress independently while it's stable.

---

## Ordering principle

Front-load the pieces where a subtle bug is catastrophic and hard to detect later (codec
ordering, resume): codec has heavy batteries; resume gets them in Phase 0, then is
re-validated against a *real snapshotting store* in Phase 1 — the only place
[I8](docs/invariants.md#i8) is testable. Then build the front end (2–4) up to the `Plan` IR
the executor already consumes, so the halves meet in the middle. Then run it by hand (REPL,
5). Do the one invariant-critical **machine change** (derived facts, 6) while the machine is
still small and the resume battery is fresh — and take the rest of the **cursor-format work
with it** (6b), because disjunction extends the same token derived binds do and
[I4](docs/invariants.md#i4) is expensive to re-establish twice. Then make it writable
(ingestion, 7), remove the last hardcoded piece (schema, 8) — which is what finally unlocks
**stored** derivation (8b), since a derived predicate cannot be built before it can be declared —
and harden (operations, 9).

**Amended after 7a: operations moved ahead of file ingestion and the schema DSL.** The original
order put 9 last because it "depends on 7–8", and that reading was too strong. What 9 needs from 8
is *a schema*, not a schema *language* — and a hardcoded one satisfies it, at the price of one
function that Phase 8 deletes. What it needs from 7 is a way in, which 7a is. Against that, three
things argued for pulling it forward:

- **The lifecycle is what makes anything else usable.** Every phase so far produced a library. A
  database that cannot be created, sealed, listed or removed is not a tool, and no amount of
  further engine work changes that.
- **The async runtime is cheapest now.** The server is small; every later feature makes the port
  dearer, and the sync↔async bridge is what chunked results, fair interleaving and cancellation
  all wait on.
- **I4 and I8 have no interactive exerciser.** The bytes-only cursor and the whole resume battery
  — the most heavily tested machinery in this project — are exercised only by tests. `\more` in a
  wire shell is the first thing that holds a cursor across a round trip, and it is in 9.

7b (file ingestion) is deferred past 9 outright: it is a throughput feature, and nothing in the
lifecycle, the CLI or the runtime needs it.

---

## Phase 0 — Invariant guard matrix + test harness

**Goal.** Stand up the **full [invariant](docs/invariants.md) coverage ledger** and the
shared test machinery every later acceptance gate depends on, and close the I3 gap.

**Depends on:** nothing (foundation).

**Design of record:** [testing methodology](docs/testing.md) (tiers, generator-first,
schema-first `(plan, store)` generation, the coverage-ledger discipline); the invariants
themselves in the [registry](docs/invariants.md).

**Invariants in scope:**
- *makes green:* [I3](docs/invariants.md#i3) (`codec::marker_table_golden` — the missing
  guard), [I5](docs/invariants.md#i5), [I6](docs/invariants.md#i6),
  [I7](docs/invariants.md#i7), [I9](docs/invariants.md#i9) (on `MemStore`), and
  [I4](docs/invariants.md#i4) (on `MemStore`; fjall in Phase 1).
- *writes-but-ignores (pending later phases):* [I8](docs/invariants.md#i8),
  [I11](docs/invariants.md#i11), [I12](docs/invariants.md#i12) (Phase 1);
  [I10](docs/invariants.md#i10), [I13](docs/invariants.md#i13) (Phase 8).
- *upholds:* [I1](docs/invariants.md#i1), [I2](docs/invariants.md#i2).

**Tasks (each ends green):**
- **0a. Shared test machinery.** Promote `aperture_store::mem_store` (started) + a schema/fixture
  builder into support modules tests import. Add the **NFR guard machinery**: an
  `allocation-counter` dev-dependency (I9), a `FactStore` spy that fails on unexpected `point()`
  (I6), a decode-counter probe (I5). ([testing](docs/testing.md#nfr-guards-are-mechanical-not-eyeballed).)
- **0b. Executor happy-path battery** over hand-built plans (1-/2-/3-level joins, seeks,
  key-field residuals, record/scalar heads); model = "run to completion, collect rows."
- **0c. Schema-first `(plan, store)` generator + resume battery** — valid-by-construction
  generator + interruption-schedule generator; **resume == uninterrupted run** at every cut
  point for 1-/2-/3-level plans. The headline gate ([I4](docs/invariants.md#i4)).
- **0d. Codec support-module restructure + the I3 golden test.** Inline generators → named
  `arb_*` strategies; **write `codec::marker_table_golden`** pinning every marker's value.
- **0e. Write the deferred guards as ignored-pending** — I8/I11/I12 (pending Phase 1),
  I10/I13 (pending Phase 8): real test bodies, `#[ignore = "Ixx — pending Phase N"]`.
- **0f. Fold in discovered latent fixes** with regression tests. Found and fixed: the residual
  walked the unstripped key; `Project::Value` decoded the register instead of the entity; the
  fact-ref field used the wrong marker; and `StackFrame::open` built its seek prefix from
  field offsets cached against the *previous* outer row (stale spans ⇒ wrong join results —
  caught by the 0c generator, pinned by
  `exec::seek_splice_rereads_field_when_outer_row_width_changes`).

**Acceptance:**
- [x] The §3 guard matrix exists; `cargo test -- --ignored --list` shows every pending guard tagged with its phase.
- [x] I3 golden marker test written and green; I1–I2 still green.
- [x] On-`MemStore` executor guards green: I4 (every cut point, 1-/2-/3-level), I5, I6, I7, I9.
- [x] Codec `arb_*` strategies are importable by other modules.

---

## Phase 1 — fjall store & the snapshot/identity guards

**Goal.** Implement the fjall `FactStore` behind the trait, plus the minimal atomic
`put_fact` to *seed* a store for tests, and flip the guards that are structurally untestable
on `MemStore` to green.

**Depends on:** Phase 0 (the resume battery + schema-first `(plan, store)` generator it
re-runs).

**Design of record:** [storage model](docs/03-storage-model.md) (two CFs, keyspace-per-
predicate, FactId allocation, atomic write); [resume](docs/05-resume.md) (snapshot release);
[operations §9/§10](docs/aperture-cli-design.md) (on-disk layout; the executor consumes a
`(handle, snapshot)` — no connection assumption).

**Why here, not later.** The fjall impl and seeding `put_fact` depend only on the
`FactStore` trait and the tuple codec — both exist. They do **not** need the front end: the
resume/I8 battery runs on generated `(plan, store)` pairs (0c) populated via `put_fact`. So
the soundest place is immediately after the harness that consumes it. This `put_fact` is the
single-fact seeding primitive — the bulk pipeline (Phase 7) builds on the same primitives.

**Invariants in scope:**
- *makes green:* [I8](docs/invariants.md#i8) (`i8_snapshot::snapshot_released_at_suspend`, drop-probe),
  [I11](docs/invariants.md#i11) (`store::factid_unique_monotonic`),
  [I12](docs/invariants.md#i12) (`store::no_half_present_facts`), and
  [I4](docs/invariants.md#i4) **re-run against fjall**.
- *upholds:* I1–I3.

**Tasks:**
- **1a.** ✅ fjall `FactStore` impl: a keyspace pair per predicate; `scan` (prefix range) +
  `point`; differential oracle against `MemStore`. Two decisions taken here and recorded in
  [chapter 3](docs/03-storage-model.md#one-keyspace-per-predicate--for-both-column-families):
  `entities` is split per predicate too, and predicate trees are creatable up front
  (`create_predicates`) because a keyspace costs ~30 ms to create.
- **1b.** ✅ Atomic `put_fact`: snowflake `FactId` (predicate tag + per-predicate sequence)
  with the high-water mark recovered from the last `entities` key
  ([I11](docs/invariants.md#i11)); both CFs in one write batch
  ([I12](docs/invariants.md#i12)); both guards un-ignored and green.
- **1c.** ✅ Snapshot discipline: `enumerate` now takes `self` **by value**, so done,
  suspend, cancel and error unwind all drop the frame stack and the store handle — I8 is a
  property of the signature rather than a caller discipline. Guard cross-checks a drop probe
  (store handle + every scan) against fjall's own open-snapshot count, with a mid-run
  positive control.
- **1d.** ✅ The Phase 0c resume battery generalised over `FactStore` and re-run against
  fjall, plus a differential arm: the same spec must give identical rows and ids on fjall and
  `MemStore` — which is what licenses every other executor battery to be written against
  `MemStore` alone.

**Acceptance:**
- [x] Resume == uninterrupted run holds against the *real* store (not just `MemStore`), and
      the two agree row for row.
- [x] I8, I11, I12 guards un-ignored and green.
- [x] Facts are never half-present (bijection over generated writes, and across a crash);
      fact-ids are unique, monotonic per predicate, and never reused across a reopen (tested).
- [x] A held iterator does not survive a suspend — and cannot be held, by construction.

---

## Phase 2 — focus grammar: permissive-early, catch-in-compilation

**Goal.** Bring the focus grammar in `crates/aperture-engine/` up to the full intended feature surface,
deferring "not-yet-implemented" to later phases via clear diagnostics — so no grammar
reshape is needed as features land.

**Depends on:** nothing engine-side (parallel with Phase 1). Used `src/lens/` as the
reference to **re-implement into `focus`** (against the `Plan` IR), deleting it file-by-file;
the last of it went with hoisting in Phase 5.

**Design of record:** [chapter 7](docs/07-compilation.md) (three tree layers; permissive-
early principle). **Phase-specific — grammar/lexer resolutions, as settled** (build detail,
not in the book; each pinned by a test in `parse.rs` or `lexer.rs`):

- `QId` qualified names — a leading-lowercase segment plus an uppercase final segment
  disambiguates `test.Foo` from `E.from` with no parser lookahead.
- **Dot binds tighter than application** — `test.Foo X.name` == `test.Foo (X.name)`.
- **`|` is looser than application** — `test.Foo A | test.Bar B` is a disjunction of two
  applications, so `fact_pattern: QId branch` (not `pattern`). Forced: with `pattern` the
  grammar is an LL(1) conflict on `|`, since a fact's key would put `|` in its own follow
  set. A disjunction *inside* a key is `test.Foo (A | B)`.
- **Disjunction is flat** — `pattern: branch ('|' branch)*`, N branches under one node, which
  is the shape `FlatDisjunction` wants. Deliberately *not* lelwel's Pratt left-recursion,
  which would give a right-leaning binary tree for flatten to undo.
- **Group and subquery are one rule** — `'(' pattern ('where' stmt_list | ) ')'`. Factoring
  the optional `where` keeps it LL(1) with no ordered choice or backtracking, and makes a
  subquery the same shape as a query, so lowering reuses the query algebra.
- **Negation prefixes a statement**, not a pattern — the level [chapter
  7](docs/07-compilation.md) reorders at. Consequence: `(!A) | B` is not expressible; moving
  `!` into `branch` is the change if that is wanted.
- `..` vs `.` by maximal munch; the `never` keyword.
- **`Nat` is lexed permissively and validated in lowering** (`lexer::parse_nat`) — so `1__0`
  is one token with a diagnostic pointing at the number, not two tokens with a parse error
  pointing between them. The sign is applied after the magnitude (`signed_literal`), because
  `i64::MIN`'s magnitude is one past `i64::MAX`.
- **A fact pattern's key stays mandatory** — a whole-predicate scan is `test.Foo _`.
- **`.value` is a reserved access name**, lowering to `FieldRef::Value`.
- **Diagnostic codes** are `nyi/…` (deferred), `reject/…` (meaningless) and `lit/…`
  (malformed literal), so tests assert on identity rather than wording. Phase 9 owns the
  single error taxonomy and may replace them with an enum.

**Invariants in scope:** *upholds:* [I10](docs/invariants.md#i10) (typecheck enforces stable
discriminants at schema-load). No engine invariant made green here.

**Tasks:**
- **2a.** ✅ Audit the `focus` grammar/lexer vs the target feature list. The table is
  **executable** — `aperture_engine::corpus` holds it as data (37 entries, since grown), each classified
  `Supported` / `Diagnosed(code)` / `ParseError`, so it cannot drift from what the compiler
  does. Running it before touching the grammar gave the audit empirically: 6 entries did not
  parse, and they were exactly the six constructs 2c adds.
- **2b.** ✅ Lexer: token boundaries pinned (`E.from` ≠ qualname, `a.B.c`, `..` munch,
  keywords) and the literal decoders added — `parse_nat`, `signed_literal`, `unescape_str`,
  each reporting by code. **Prerequisite discovered:** nothing in the grammar was testable,
  because `focus` had no parse entry point and no CST façade; those landed first
  (`aperture_engine::cst`, `aperture_engine::parse`).
- **2c.** ✅ Grammar: parens (group + subquery), `never`, union select, flat disjunction,
  statement negation. Resolutions above.
- **2d.** ✅ Façade → `SyntaxTree` store lowering (`aperture_engine::lower`), with sorted-slice record
  fields and a duplicate-field rejection. The boxed ergonomic AST (representation 3) is **not**
  built — nothing needs it yet — so `lens/query.rs` survives.
- **2e.** ✅ Typecheck (`aperture_engine::ty`, re-implemented from `lens/ty.rs`) against `PredicateTy`,
  emitting one specific diagnostic per deferred construct. No `Ty::Never`: `never` reports as
  not-yet-implemented, so a type for it would be speculative.

**Acceptance:**
- [x] The target-feature corpus parses in `focus` (incl. constructs deferred to later phases) —
      `corpus::every_entry_parses_as_classified`.
- [x] The implemented subset typechecks; every deferred construct yields a specific, tested
      diagnostic — never a parse error or panic —
      `corpus::every_entry_is_diagnosed_as_classified`, which compares the *whole set* of codes
      so a deferred construct cannot also report a type error about itself.
- [x] Subsumed `lens` files deleted (7 of 12; `hoist.rs` + `query.rs` and their dependencies
      remain as the Phase 4 reference).

**Two extras the phase paid for, recorded because they are one-way doors of their own:**
`parse` bounds nesting *before* parsing (the generated parser is recursive descent and
`pattern` is mutually recursive through both records and application, so deep input would
overflow the stack — a panic on a data path); and a record *pattern* may name a subset of a
key's fields, an omitted field being a wildcard, while two record *types* must still agree
exactly ([chapter 7](docs/07-compilation.md)).

---

## Phase 3 — Compilation driver: shared context

**Goal.** A single compilation context carrying the shared plumbing (a pooled diagnostics
sink, the interners, the schema, the `SyntaxTree` store + side tables) so later phases are
passes over shared state, not bolted-on functions. **Not** a `salsa`-style incremental engine.

**Depends on:** Phase 2 (typed trees).

**Design of record:** [chapter 7 — "The compilation driver"](docs/07-compilation.md#the-compilation-driver)
(one keep-going diagnostics sink via `codespan-reporting`; two-tier interners; `NodeId` side
tables; explicitly no memoization).

**Invariants in scope:** none made green (plumbing). *upholds:* the record-field-ordering
convention across tree layers.

**Smaller than it was written, because Phase 2 delivered four of its pieces:** keep-going
diagnostics in all three phases, single-lex (`aea2003a6`), severity-aware `has_errors`
(`ef70c8c6d` — added precisely so a pooled sink carrying a warning would not read as a failed
parse), and a side table of *resolved* types. What was left is the sink, the context, and the
rendering.

**Tasks:**
- **3a.** ✅ `aperture_engine::diag` — the sink and the code taxonomy. `Code` is an enum (20 variants,
  `as_str` rendering exactly the strings Phase 2 used, `kind` deriving the prefix); the
  `Diagnostic` alias moves out of `parser.rs`, which is generated-parser glue; `Diagnostics`
  reports with either span type and filters `has_errors` by severity.
- **3b.** ✅ Phases take the sink and cannot return diagnostics — `parse → Option<Cst>`,
  `lower → Ast`, `check → Typed`. *Done:* the corpus gates pass **unchanged**, which is what
  proves the signature change altered no behaviour.
- **3c.** ✅ `aperture_engine::compile::Compilation` — source, schema, interner, sink, tree and side
  tables in one context; `check()` sequences the phases; rendering lives here. The CST is
  deliberately not stored (it borrows the source; storing it buys a self-referential struct).
- **3d.** ✅ `plan()` as the Phase 4 seam: type-checks, then reported `nyi/flatten`
  (superseded — Phase 4 replaced that report with the flatten pass itself, and the code is
  gone from the taxonomy).
- **3e.** ✅ `src/main.rs` compiles only through the driver.

**Acceptance:**
- [x] No front-end function returns diagnostics — the sink is the only path. Structural: the
      signatures are the check, so there is nothing to test and nothing to remember.
- [x] One context carries diagnostics + interning + the typed store through the pipeline, and
      is the only thing the shell calls.
- [x] `plan(q)` sequences the phases (then reporting `nyi/flatten`; now flattening), and a
      query that does not typecheck has no plan — flatten is not run over it, so the only
      diagnostic is the one the user can act on.
- [x] Multi-error diagnostics render via `codespan-reporting`, **in source order**, tested
      against a buffer — `compile::one_sink_holds_every_phase_and_rendering_sorts_it` and the
      shell's own `a_line_wrong_twice_prints_both_faults_in_source_order`.
- [x] Determinism and composed-no-panic properties green over generated input; the ledger is
      still 4 entries, since this phase makes no invariant green and adds no pending guard.

**What the phase discovered:** a sink has **two orders**. Diagnostics arrive in phase order —
lowering's before typecheck's, whatever part of the query each is about — which is right for a
log and wrong for a reader. Rendering sorts by where a diagnostic points; the sink does not,
because that arrival order is what lets a caller ask what one phase reported. Both are pinned
by one test on the same two diagnostics.

---

## Phase 4 — Flatten → reorder

**Goal.** Lower the typed query to the flat `Plan`: flatten nested generators into an ordered
generator list, run sargeability to build seeks/residuals, and choose the loop order — identity
at the outset, the greedy runnable frontier as built, structured either way so a cost model drops
in without reshaping.

**Depends on:** Phase 3 (the driver + context).

**Design of record:** [chapter 7](docs/07-compilation.md) covers all the settled design —
flatten, disjunction-stays-a-node (never DNF across conjuncts), union-select →
`DiscriminantEq` residual, sargeability's order-dependence, the safety-vs-ordering split, and
[reorder — the runnable frontier](docs/07-compilation.md#reorder--the-runnable-frontier): why
greedy is complete, why a layering is the wrong shape for *choosing* an order, and what is
actually outstanding — a **cost model**, not a topological sort. (Glean's `Reorder` has neither a
topological sort nor antichains; the only `topSort` in its pipeline is over derived-predicate
dependencies in `glean/db/Glean/Query/Prune.hs:85`.) **Read it before this phase.**

**Invariants in scope:**
- *makes green:* the end-to-end property **"flattened plan run == expected rows"** (tier-3,
  schema-first), run **to completion and resumed at every scheduled cut point, on `MemStore`
  and on fjall** — so [I4](docs/invariants.md#i4) is guarded over the plan shapes *flatten*
  emits, and [I5](docs/invariants.md#i5)–[I9](docs/invariants.md#i9) via the produced plans.
- *upholds:* I1–I9.

**Phase-specific decisions, as settled:**

- **Intra-row repeated variables (`Edge{from=X,to=X}`): rejected**, as `nyi/repeated-variable`
  — deferred rather than meaningless, since the pattern means something ordinary and `EqField`
  is additive when something wants it ([open decisions](docs/open-decisions.md)). Repeated
  *reads* of an outer-level variable are supported, and make the seek a point match.
- **`FieldPath`** (not a flat `FieldIdx`) in the plan types: a top-level field plus a step per
  nested record, with the flat case the fast path the field-offset cache serves.
- **A stored key is flat** — its top-level fields back to back, no wrapper of its own. Forced
  here, because flatten is the first thing that has to *choose*; the codec chapter and
  `plan::proptest` already assumed it and the demo shell's seeding did not
  ([chapter 3](docs/03-storage-model.md#a-stored-key-is-flat)).
- **Flatten reads types from the schema, not from typecheck's side table**: a plan needs
  declared `PredicateTy`, walked along the path it will read at run time, so a projection
  cannot disagree with the bytes it decodes. Phase 6 is the first thing that needs the table.
- **No hoisting.** A fact pattern away from the top level of a statement is
  `nyi/nested-generator`. *(Superseded: Phase 5 hoists, and the code is gone.)*

**Tasks:**
- **4a.** ✅ Flatten the implemented subset (scans, joins, scalar/record heads, nested captures)
  to `Plan`; range-restriction safety over the *chosen* order, so it also checks whatever
  `reorder` returned. text→plan→run for the corpus and for hand-written worked examples.
- **4b.** ✅ Sargeability over the chosen order (seek · splice · residual · capture), with the
  decision table in [chapter 7](docs/07-compilation.md#how-sargeability-actually-decides-phase-4-as-built).
  A string prefix seeks in the leading field and filters elsewhere; a fully-input key becomes a
  point match.
- **4c.** ✅ Reorder as the **greedy runnable frontier**, over a graph of **variables** rather
  than edges between statements — because which statement captures a shared variable depends on
  the order, so edges would forbid correct orders. It is load-bearing for **acceptance**: a
  written order that reads a variable the next statement binds is made legal rather than refused,
  and a source order that already works is returned unchanged. Greedy is *complete* because
  `reads` is structural and `bound` only grows, so emitting anything runnable can never strand
  anything else — property-tested against `Deps::antichains`, which is kept **off** the reorder
  path as the exact feasibility answer and the independent witness. What is left is a **cost
  model**, and the `// TODO: selectivity` seam says what it needs: extend `StmtDeps` with each
  statement's key-prefix shape, then `min_by_key` over the frontier (plus moving
  negations/conditionals after their parent-scope binds — Phase 6b). A layering cannot be that
  cost model: a layer index is only a lower bound on position, so sorting inside a layer can never
  defer a cheap-looking scan past the selective statement that would have bound its key.
- **4d.** ✅ Intra-row repeats decided and implemented: rejected, tested both ways (the repeat,
  and the repeated *read* that is supported).
- **4e.** ✅ The tier-3 battery re-run **through the interruption schedule**, and against fjall.
  Not scope creep — the phase claims I4 "via the produced plans", and `plan::proptest` draws
  none of the shapes flatten emits: it only ever seeks by one whole spliced field from an empty
  prefix, with at most one flat-path residual per level and no `Project::Value`. So resume and
  the store differential had never seen a constant seek prefix, a several-part composite, a
  `ResidualOp::Prefix`, a nested `FieldPath`, two residuals on one level, or a point read at
  projection. A **census** (`the_generator_reaches_every_plan_shape`) asserts the battery
  reaches all six — it failed on five of them, and string prefixes, nested record keys,
  three-field keys, row binds and values were added to the generator until it passed.

**Acceptance:**
- [x] `plan(q)` produces a runnable `Plan` for the corpus, safety-checked (non-range-restricted
      queries rejected with a clear error). The corpus gate now runs the whole driver, so
      `Supported` means *produces a plan*, and every construct flatten defers has an entry
      naming it.
- [x] Reorder is the greedy runnable frontier over the variable graph: completeness
      property-tested against `Deps::antichains`, and a source order that already works compiles
      to exactly the plan it compiled to before.
- [x] "flattened plan run == expected rows" holds over generated `(query, store)` pairs
      (tier-3), against a nested-loop model — and holds in **every permutation** of the body,
      which is the reorderability claim made executable.
- [x] Intra-row repeats are rejected — tested, alongside the repeated read that is not.
- [x] I4 holds over **compiled** plans — resume == the query's meaning at every scheduled cut
      point, on `MemStore` and on fjall — with a census asserting the battery reaches the plan
      shapes the executor's own generator never draws.

**What the phase discovered:** the codebase had **two stored-key layouts** — flat (the codec
chapter, `plan::proptest`, the offset cache) and record-wrapped (the demo shell's seeding) —
and both "worked", because the executor never learns which convention wrote a row. Only a
*plan* has to choose, and choosing wrong reads the wrong bytes with no error. That is the
shape of bug this project's testing discipline exists for, and it was invisible until two
halves of the system had to agree.

---

## Phase 5 — REPL: experiment by executing ✅

**Goal.** A simple interactive REPL to *run* queries end-to-end (parse → compile → plan →
execute → project), for testing and demo. First moment the whole pipeline is exercised by a
human — invaluable for integration gaps unit tests miss.

**Depends on:** Phase 4 (a runnable `Plan` from text) + Phase 1 (a store to run against).

**Design of record:** the iteratee/portal seam it drives is [chapter 5](docs/05-resume.md);
the remote-first product shell is [operations §5](docs/aperture-cli-design.md).

**Early now, remote-first later.** The Phase 5 REPL runs in-process against a fixture store
for fast iteration. The *product* shell is remote-first — always speaks the wire protocol,
the permanent wire exerciser. Treat Phase 5 as a scaffold: reuse its executor-driving and
diagnostic rendering, but expect the interactive front to be re-pointed at the wire client in
Phase 9; don't build in state a wire shell can't reproduce.

**Invariants in scope:** none made green; *upholds* the full engine set by exercising it.

**Tasks:**
- **5a.** ✅ The `plan(query)` call at the prompt, running it via `enumerate`, printing
  projected `Value`s with references resolved to the facts they name; `:plan` shows the plan
  without running it. The loop, line editing, highlighting and codespan rendering already
  existed from Phase 2.
- **5b.** ✅ **One fixture database** (`aperture_store::fixture`): the schema, the facts and the example
  queries the corpus, the batteries and the shell share. Not test-gated, because the shell is
  not a test.
- **5c.** ✅ Rendering a `Plan` for a person, in `aperture_engine::print` beside the two renderings of a
  tree it already owned — fields named from the schema, since `of = r0#` is the answer to "did
  it follow the reference?" and `1 = r0#` is not.
- **5d.** ✅ Both halves of ["reaching a fact through a
  reference"](#reaching-a-fact-through-a-reference--three-sizes-listed-apart--phase-5) that a demo needs
  — the fact-id splice/compare and hoisting — see the scope note below.

**The scope decision this phase faced, and how it went.** Phase 4 left the shell advertising
two examples that typechecked and had no plan, because every join through a reference was
`nyi/fact-field` and the idiomatic spelling of one was `nyi/nested-generator`. The options were
to narrow the examples or to pull in the reference items; **all three items landed**, which is
what turned the demo from a database you can enumerate into one you can query. The three sizes
were estimated in that table and held: #1 was ~40 lines of IR plus executor, #2 was one relaxed
check plus the diagnostic its trap needed, #3 was flatten-local. What it cost beyond the
estimate was *test* work rather than implementation: the census would not go green on unit
tests, so the query generator had to learn fact-typed fields.

**What "runnable" turned out to mean.** `Supported` in the corpus meant "produces a plan", and
the module doc claimed it meant "runs" — a different claim, since a plan that seeks the wrong
prefix is still a plan. It now carries **the rows the entry answers with**, checked against a
real `FjallDb`, and a `Supported` entry cannot be added without saying what it returns.

**Acceptance:**
- [x] Typing a focus query returns rows (or a well-rendered diagnostic) against a fixture store, end-to-end, through the real compiler and executor.
- [x] Diagnostics from typecheck/flatten render nicely (source spans).
- [x] Every `Supported` corpus entry runs against a real store and returns its recorded rows.
- [x] Every example the shell offers is a corpus entry that runs.

---

## Phase 6 — Dynamic derivation: the machine change ✅

**Goal.** The **register-and-step machine change** derived facts need: promote `Register` to a
`Slot` sum type, make a plan's body a sequence of steps rather than a list of levels, and make
resume recompute what it does not save. One of the **two sanctioned machine changes**, done here
— while the machine is still small and the resume battery is fresh — rather than after ingestion
and schema pile onto the current register shape.

**The split this phase discovered, and it moved half the work out.** "Derived facts" was one
name for two features:

- **Stored derivation** (`predicate P : … = KEY where <query>`, written as facts) needs *nothing
  from the executor* — at query time `P` is facts in a keyspace, scanned like any other
  predicate, and the deriver is a program that runs a query and calls `put`. It cannot be built
  before a derived predicate can be **declared**, which is the schema DSL. Moved to
  [Phase 8b](#phase-8b--stored-derivation).
- **Dynamic derivation** — a value computed while a query runs — is the machine change, and is
  this phase.

**Depends on:** Phase 4 (flatten + the graph-taking reorder interface) and the resume battery
(Phases 0/1).

**Design of record:** [chapter 7 — "Derived facts"](docs/07-compilation.md#derived-facts) (the
two kinds; the `Slot` sum type; derived binds as not-a-loop-level, recomputed on resume;
[folding](docs/07-compilation.md#folding-a-constant-bind)), [chapter
4](docs/04-executor.md#the-plan-ir) (the `Step` sequence, levels vs steps, the unit relation)
and [chapter 5](docs/05-resume.md) (one cursor entry per *level*).

**Invariants in scope:**
- *adds & makes green:* **[I14](docs/invariants.md#i14)** — *a derived bind is a pure function
  of the fact bindings* — guarded by a tier-3 resume battery at every cut point, with the derive
  step both above and below a scan.
- *upholds:* [I4](docs/invariants.md#i4)–[I9](docs/invariants.md#i9) (the `Register→Slot` and
  `body → [Step]` changes must regress none of them).

**Decisions taken here, as settled:**

- **A plan's body is one ordered sequence of steps** (`Step::Scan | Step::Derive`), not levels
  plus a side-table of computations. `reorder` produces one order; two collections joined by an
  index would be two sources of truth for it, with nothing to say which wins. The cost is that
  `body.len()` stops meaning "number of loops" — that is `Plan::levels()` now — and the one place
  it mattered was the cursor's length check, which said `>` while the two counts were the same
  number and so let a short cursor half-replay a plan.
- **A derive step is a one-row generator**: compute descending, exhausted ascending, one bit of
  frame state. So "a derived bind is not a loop level" stops being free and becomes maintained —
  derive frames hold no row and contribute nothing to the cursor.
- **A plan with no levels is the unit relation — exactly one row** — and reports `Done` when
  asked to suspend, because its cursor would be empty and an empty cursor restarts a run.
  `EmptyPlan` is gone; it existed to refuse exactly this shape.
- **A constant bind folds; it does not become a derive step.** `X = 42`, and a record of
  constants to any depth, is substituted at every use. A step holding a compile-time constant
  would be a level to walk and a value to recompute in order to arrive back at the literal.

**Tasks:**
- **6a.** ✅ `Register` → `Slot` (fact | computed value), with `SlotKindMismatch` making a
  wrong-kind read say so — the same silent shape as the `FactRef` marker trap. Behavioural
  guards added *first* to the four seams the refactor lands on, which had none: the two
  `MachineState::get` faults were covered only by a test asserting how they *render*, and
  resume's fact-id integrity check had no test at all.
- **6b.** ✅ `body: Box<[Step]>`, `Plan::levels()` / `::level(n)`, the derive arm of `enumerate`,
  and resume as one forward walk — *re-bind the fact-slots, recompute the value-slots*.
- **6c.** ✅ [I14](docs/invariants.md#i14) and its guard. **Mutation-checked, and it had to be**:
  the first version of the guard passed with resume's recompute deleted, because the derive sat
  *below* the scan and `enumerate` re-entered it from beneath on the way back up, recomputing it
  itself. Only a derive *above* a scan observes the fault.
- **6d.** ✅ Folding, and the empty-body relaxation it needs. The trap it walks past — `constant`
  writes the wrapped record form, while a stored key is flat — is pinned by a test, because both
  reasons it is safe are invisible from the fold's own code.

**Acceptance:**
- [x] The `Register→Slot` and `body → [Step]` changes leave all prior engine guards green
      (297 tests; the I4 battery re-run on `MemStore` and fjall either side of both).
- [x] [I14](docs/invariants.md#i14) + guard added to the registry and chapter 7, green at every
      cut point in both step orders.
- [x] Resume is correct over plans containing a derive step, and over plans containing folds.
- [x] A constant bind answers at the prompt — `X where X = 42`, `X where X = {name = "foo", y =
      24}` — and narrows a seek where the written literal would.

**What is left unlowered, deliberately.** Nothing in focus produces a `Step::Derive`: a constant
folds, and anything else is a value that differs per row. The nearest candidate, `Y = X.name`,
would most likely become another *substitution* (an alias for a field of `X`'s register) rather
than a value slot — so the first real producer is a **primitive**
([open decision](docs/open-decisions.md)) or a **subquery**
([Phase 6b](#phase-6b--the-deferred-query-surface-)). The machinery is built ahead of them on
purpose, because its resume behaviour is the expensive thing to get wrong later; it is exercised
by hand-built plans, and I14 records that scope rather than implying pressure the language does
not yet apply.

---

## Phase 6b — The deferred query surface ✅

**Goal.** Implement the four constructs that **parse and typecheck-as-deferred today but have
no engine behind them**: disjunction (`|`), the empty pattern (`never`), statement negation
(`!`) and a subquery as a pattern. Each is a `nyi/` code `ty.rs` reports *before flatten ever
runs* (`ty.rs:269`, `:256`, `:147`, `:276`), so each is a query a person would naturally write
that can only be answered by rephrasing it. **Union types are deliberately not here** — they
moved to Phase 8, for the reasons below.

**Why this is a phase and not a bullet.** These sat in [deferred
features](#deferred-features-additive--must-not-reshape-the-machine) under the heading
"additive — must not reshape the machine". That claim is *true and was never a claim about
size*: three of the four need a new plan operator or frame kind, and disjunction changes the
`Cursor` format. Left in a bullet list they had no acceptance criteria and no invariant
accounting, which is the one thing this plan exists to prevent.

**Depends on:** Phase 6. Not semantically — negation and subqueries would compile against
today's machine — but structurally: `Cursor` is `Vec<Register>` (`iter.rs:559`), Phase 6
rewrites it to `Vec<Slot>`, and disjunction adds a per-branch discriminant to the same token.
Doing 6b first means editing the resume token twice and re-establishing
[I4](docs/invariants.md#i4) twice, which is the most expensive battery here.

**Design of record:** [chapter 7 — flatten](docs/07-compilation.md) (disjunction survives as a
`FlatDisjunction` union-of-streams node, **never DNF-expanded across sibling conjuncts** —
that's exponential blow-up; the one bounded exception is Glean's "PLAN-B", distributing an `|`
only *within a single seek's pattern*) and [chapter 5 — the seam to the
wire](docs/05-resume.md) ("later features extend the cursor without reshaping it: disjunction
adds a per-branch discriminant to the token" — this phase is what that sentence was kept for).
Negation prefixing a *statement* rather than a pattern is Phase 2's grammar resolution, with
the recorded consequence that `(!A) | B` is inexpressible unless `!` moves into `branch`.

**Invariants in scope:**
- *upholds, and must re-prove:* [I4](docs/invariants.md#i4) — the cursor grows a per-branch
  discriminant, so resume == uninterrupted run has to be re-established over plans *containing*
  a disjunction, including a cut point taken mid-branch. [I7](docs/invariants.md#i7) — a
  union-of-streams operator is a new **frame kind**, never a recursive call.
- *at risk, to be settled here:* [I6](docs/invariants.md#i6) — see the negation decision below.
- *makes green:* no new invariant, unless the negation decision adds one.

**Decisions this phase had to settle at pickup, and how each went:**

- **Does negation read the store inside the row loop — and is that I6?** ✅ **Settled: its own
  step, and I6 is untouched.** A negation is a `Step::Test`, not a probe inside the scan loop and
  not a semijoin frame: it runs once per row the level above it *produces*, which is the same
  shape `Source::Fetch` pays, and it reads `keys` and fetches no value — which is what I6 is
  about. Guarded rather than argued (`exec::a_negation_probe_fetches_no_value`). The step also
  closes each probe before returning, so [I8](docs/invariants.md#i8) stays structural.
- **Does `never` get a type?** ✅ Settled in 6b-a: a fresh type variable, which is what "the
  identity of `|`" means with no subtyping. No `Ty::Never` constructor.
- **Is a subquery inlined?** ✅ **Yes, and it needed no operator.** Its statements become the
  enclosing query's and its head is the value the bind names, which is what Phase 2's
  one-rule-for-group-and-subquery bought. Two narrow cases keep `nyi/subquery`: a subquery that
  rebinds an outer name (typecheck scoped them apart, so inlining would conflate two variables),
  and negation inside one.

**The risk `Deps` was thought unable to express — answered, and it needed no mechanism.** The
worry was that Glean tags every statement `Ordered` or `Floating`
(`glean/db/Glean/Query/Flatten/Types.hs:70-77`) and forces a negated subquery *after* every
parent-scope variable it uses, semantically rather than heuristically (`Note [Reordering
negations]`, `glean/db/Glean/Query/Reorder.hs:547-573`) — while `StmtDeps` records only captures
and reads and so cannot say "this one may not move above that one".

It does not need to. Give a negation `reads` = the variables it names and `captures` = nothing,
and the frontier already refuses to run it before those are bound, because that is the only thing
the frontier does. `!(A X); B X` is therefore *forced* to run as `B X; !(A X)`, and completeness
survives untouched: `reads` is still structural and `bound` still only grows. So `Placement`
gained no consumer here after all — what keeps it is `preserves_written_order`, which is a
different and narrower claim ([the query-surface note](docs/query-surface.md) §5 predicted both).
What remains open is only the **nested group** — a negated subquery, where a group's reads depend
on how its own branches are ordered — and that is exactly the shape still reported as
`nyi/negation`.

**Architecture note, written before pickup, and it held:**
[`docs/query-surface.md`](docs/query-surface.md) argued one shape for this whole phase on the
finding that **only disjunction touches the resume token** — negation, `never`, subqueries,
`Source::Fetch`, primitives and comparisons are all filters, deterministic binds, or compile-time
rewrites, and a construct costs cursor work only if it can be mid-flight when a row is handed out.
Both recommendations that amended this phase were taken: negation's placement is a **reads-edge**
rather than an immovability tag, and branch scope is the **intersection** of what the branches
bind rather than 6b-b's rejection. Its status blocks record what it predicted and what it got
wrong; the one thing it did not foresee is that `Test` would house negation *alone*, since the
comparisons it was also meant to carry turned out to be residuals.

**The machine half of 6b-a is done** (`plan.rs`, `iter.rs`), ahead of the language, the same way
Phase 6 built derived binds ahead of a producer: a level's rows come from a list of `Source`s,
so zero is the empty relation, one is a scan and many is a disjunction; a cursor entry carries
the alternative that produced it; and [I4](docs/invariants.md#i4) is re-proved over generated
multi-source plans with a census asserting a cut is taken while a later alternative is live.
Nothing in focus lowers one yet — `nyi/disjunction` is still reported — so what is left of 6b-a
is `flatten` and `ty.rs`, plus `never` decided alongside them.

**Tasks (coarse — decompose at pickup, per the rule at the top of this file):**
- **6b-a. Disjunction.** ✅ — the cursor discriminant, flatten lowering `|` to a multi-source
  level, `ty.rs` giving `|` a type, and `never` decided alongside it (a fresh type variable,
  which is what "the identity of `|`" means with no subtyping; no `Ty::Never` constructor).
  What is left is an alternation *inside* a pattern, which keeps `nyi/disjunction`. No DNF expansion across conjuncts. The classification the note asks for — a disjunction
  whose branches only *filter* becomes a test rather than a level, and single-generator branches
  normalise to `Source::Seek` — is flatten's, and is what keeps the common case off the token.
- **6b-b. Range-restriction safety across branches.** ✅ — as the **intersection** the note
  recommends rather than the rejection this task first described: a variable only one branch
  binds does not escape the statement, and a later read of it is `reject/unbound-variable` where
  a person can act on it. What *is* rejected outright is a variable two branches bind in
  **different places** — a register holds one row and the plan holds one path into it, so
  reaching it at another field would decode the wrong bytes for half the rows, silently.
- **6b-c. The I4 battery over disjunctive plans**, and the **census** extended. ✅ — the census
  asserts a disjunctive plan is drawn and a cut taken mid-branch, and it has since grown the
  same claim for negation: a test is reached, its probe seeks by a bound register and filters by
  one, and it is drawn *above* a scan.
- **6b-d. Negation.** ✅ — a `Step::Test(Test::Absent(sources))`: no register, no cursor entry,
  and neither of its outcomes new to the machine (pass is a derive's ascent, fail is an exhausted
  level's backtrack). Its variables are reads, which *is* the placement rule. Three refusals
  rather than one: a negated **subquery** (a level inside a test), a **generator inside its key**
  (hoisting it out would answer differently when it matches nothing), and a variable **only** the
  negation names — `reject/unbound-variable`, because the existential reading is already
  spellable as `_` and the two are indistinguishable at a glance.
- **6b-e. Subquery.** ✅ — inlined, per the decision above.
- **6b-f. Corpus reclassification.** Mostly done: negation, disjunction, `never`, subqueries and
  the constructs behind them are `Supported(rows)` and run against a real `FjallDb`. **No code
  was retired**, and that is the honest outcome rather than a miss — each of `nyi/disjunction`,
  `nyi/negation` and `nyi/subquery` now covers a *narrower* construct than it did (an alternation
  inside a pattern; a negated group or a generator in a negation's key; a subquery rebinding an
  outer name), and each keeps a corpus entry naming it. `nyi/union-select` is Phase 8's.

**Acceptance:**
- [x] Disjunction compiles to one level per statement and runs — `conjoined_disjunctions_do_not_multiply`
      is the DNF test: three two-branch disjunctions are 8 clauses expanded and 3 levels of 2
      sources here, so the plan is **linear** in the branches written.
- [x] [I4](docs/invariants.md#i4) holds over plans containing a disjunction, at every scheduled
      cut point, on `MemStore` **and** fjall, with the census asserting the battery reaches a
      disjunctive plan and a cut taken mid-branch — over hand-built plans (`plan::proptest`) and
      now over **compiled** ones too, the query generator having learned to draw `A | B`.
- [x] The **algebraic laws** that need no model, added once the metamorphic scaffolding for
      negation existed: a negation and its assertion partition the rows, a disjunction is the
      concatenation of its branches, `A | B` answers as `B | A`, and `!(A | B)` as `!A; !B`.
      Double negation and distributivity are *not* on that list, and the reasons are recorded
      where the laws are: `!!S` does not parse and could not be an identity if it did, and `|`
      joins patterns rather than statement lists.
- [x] Branch scope is the **intersection**, and a variable two branches bind in different places
      is rejected with a diagnostic naming it — never a run-time error.
- [x] `never`, negation and subqueries each either run, or draw a **narrowed** diagnostic
      naming what is still missing — never a parse error and never a panic.
- [x] Negation's placement is *forced*, not merely likely: `!(A X); B X` compiles to the **same
      plan** as `B X; !(A X)` — asserted as one plan rather than as one set of rows, since equal
      rows would also hold if the negation ran first and happened to match nothing.
- [x] Every reclassified corpus entry returns its recorded rows against a real `FjallDb`. No
      `nyi/` code was retired: each survivor names a narrower construct and keeps an entry.
- [x] All prior engine guards green — the cursor format changed, so this is the claim that
      matters (414 tests).

**What is left of the phase, deliberately:** an alternation *inside* a pattern
(`nyi/disjunction`), a negated **group** (`nyi/negation`) — which is `Source::Group`, the note's
stage 4 — and a subquery that rebinds an outer name (`nyi/subquery`). Each is a narrowed
diagnostic with a corpus entry, and none of them is on the path to
[Phase 7](#phase-7--transport-codec--fact-writing-ingestion).

---

## Phase 7 — Transport codec + fact writing (ingestion)

**Goal.** Write facts programmatically and from files so the DB isn't hardcoded — a
`Db`/ingestion path that encodes and stores facts, with efficient *parallel* ingestion.

**Depends on:** Phase 1 (the store + atomic `put_fact` + FactId allocator).

**Sequenced wire-first, in two parts.** The **write stream over a socket** (§6) is 7a and the
**file pipeline** (§5, §8) is 7b, in that order, and the reason is not preference. A parallel
file ingest has to answer *where interning happens* — workers encode keys and sort in step 2,
but a key holding a reference has no bytes until ids are assigned in step 3. A write stream has
no such conflict: one writer, one ordered stream, interning as it arrives. So 7a builds and
tests the interning primitive on the path that does not need the hard answer, and 7b asks it
holding a working implementation. It also puts the **primary ingestion API** first, which is
what a client actually programs against.

**Design of record:** the write stream, the wire fact encoding and what a reference is on the
way in are [operations §6](docs/aperture-cli-design.md#6-wire-protocol--the-write-stream); the
parallel decode→sort→k-way-merge→bulk-`ingest()` pipeline and the fact-file format + sync
markers are [operations §5 & §8](docs/aperture-cli-design.md) (`ops-I5` one-write-funnel,
`ops-I4` reproducibility). Interning is
[chapter 3](docs/03-storage-model.md#interning-a-nested-fact); the storage-vs-transport codec
split — which runs **both ways** — is
[chapter 3](docs/03-storage-model.md#storage-codec-vs-transport-codec). **Read those; don't
restate them.**

**The gate is closed.** A reference on the way in is
[the target fact written inline](docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline),
interned at ingest, or a `FactId` a producer already holds. Settled because the alternative
answers all put a map from every entity to its assigned id inside the *indexer*; nesting means a
producer emits what it has in hand where it stands. Both file format and wire format inherit it,
so there is one fact encoding rather than two.

**What already exists, and what this phase must *not* do to it.** `aperture_store::fact` +
`FjallDb::put` are the **single-fact** seam ([chapter
3](docs/03-storage-model.md#writing-a-fact-by-hand)): a well-typed value, key fields resolved
against the schema by name. It materialises a `Value` per fact, which is right for a deriver
writing thousands and wrong for a loader writing millions — so the fact-file path wants a
**streaming** form that never builds the value, built *beside* this one rather than replacing
it. The schema resolution it does (declared field order, unknown/missing field, stray value
side) is the part the streaming path still owes, by a different mechanism.

**Invariants in scope:**
- *strengthens (at scale):* [I11](docs/invariants.md#i11), [I12](docs/invariants.md#i12).
- *upholds (relies on):* [ops-I4](docs/invariants.md#ops-i4) (reproducibility ⇒ conflict
  handling is order-independent), [I13](docs/invariants.md#i13) (validate ingest against the
  embedded schema — against the one built-in schema, per-database copies being Phase 8.4's
  remaining half).
- **Note:** the [`FactRef` marker](docs/open-decisions.md) is already resolved (own marker
  `0x51`), so writing fact-typed *bytes* is unblocked — no pre-work there.

**What interning buys `ops-I4`.** "Hash the canonical schema and base facts" could not be taken
literally while a base fact contained a physical id — two reproducible builds would hash
differently for no semantic reason. With references sent as nested facts, a DB has a canonical
*logical* form (expand every reference to its target's key, recursively) and the hash is over
that. Reproducibility no longer needs the strictly stronger promise that ingest assign identical
physical ids.

**Phase-specific rules:** the encoder must agree **byte-for-byte** with the read-path decoder
(the round-trip property is the guard); dedup byte-identical facts silently and **reject
same-key-different-value** deterministically (`--on-conflict=reject` default; any override must be
commutative, so no pick-one rule of either polarity).

**What this phase is actually claiming against Glean, stated so it isn't overclaimed.** The reject
rule is Glean's own default (`glean/rts/define.cpp:91-102`), not a divergence; what Glean cannot do
is *hold* it — it disables the rule on three paths, including its offline merge, with an in-source
admission that it may be "silently picking one of the two facts"
(`glean/write/Glean/Write/SendAndRebaseQueue.hs:408-426`). So the claim here is **"one funnel is
what makes rejecting affordable"**, and the acceptance criteria below are what make it true. The
genuine format divergence is *splittability*: a Glean binary `Batch` is one opaque sequential blob
with no sync marker, header, CRC or footer index (`glean/if/glean.thrift:159-181`), so it cannot be
split and Glean parallelises across batches; this pipeline splits **one file** at validated sync
markers (`ops-I5`, operations §8).

**Tasks — 7a, the write stream:** the transport codec (a wire fact: predicate, key, optional
value, references id-or-nested) and its round-trip property; the frame layer and PG-shaped
startup; the server and its socket, with the per-DB single writer task (`ops-I1`); **interning**
— resolve-or-create a nested fact, bottom-up — and the write stream that funnels through it
(`ops-I5`); a query stream, so ingest-then-query closes end to end.

**Tasks — 7b, the file pipeline:** `Db` + per-predicate partition handles; the fact-file format
+ sync-marker chunk splitter; the parallel decode→encode→sort→k-way-merge→bulk-`ingest()`
pipeline, which is where *where interning happens* gets answered — a per-chunk pre-pass or a
stratum boundary in the merge. *Done per task:* ingest-then-query returns the ingested facts.

**Acceptance — 7a:**
- [x] Facts are writable over a socket and queried back on the same connection. Twice over: a Rust client (`aperture-server/tests/over_a_socket.rs`) and a **C# one** (`clients/dotnet`), the second because a client in this repository can agree with the server by accident.
- [x] Transport encoder/decoder round-trip property green (tier-1), nested references included.
- [x] A nested reference interns to the same `FactId` a second occurrence of that target resolves to — one row, however many parents name it.
- [x] A nested fact disagreeing with a stored one under the same key is rejected by name (`ops-I5`), and the connection's other streams survive.
- [x] Interning is bottom-up and total on any well-typed nested value: no order in which a parent is written before the child its key holds. **Narrowed by the property that proved it:** total up to *self-consistency*. A fact naming one target twice with two different value sides is well-typed and contradictory, and is refused as an ordinary conflict — the criterion as written was too strong. The census proves both outcomes are reached, so the weakened property is not vacuous.

**7a is done.** The transport codec and its framing (`aperture-wire` — `varint`, `value`, `crc`,
`block`, `desc`, `frame`), the write funnel (`aperture-ingest` — `FactSink`, `intern_fact`,
`intern_block`), the protocol and socket (`aperture-server`), a binary to run it
(`src/bin/aperture-serve.rs`), and a C# client that proves the protocol is implementable from
outside (`clients/dotnet`).

**What 7a deliberately left, each named as deferred in [operations §5](docs/aperture-cli-design.md)
rather than discovered later:** fair interleaving between streams (frames carry a stream id and
the server honours it, but a long query still delays a short one behind it), chunked incremental
results (the executor already suspends; the loop that resumes it between chunks is missing),
in-band cancellation, per-stream flow control, and TCP (`ops-I10` is default-closed — the opt-in
flag is not wired). None is on 7b's path.

**Acceptance — 7b:**
- [ ] Facts are writable from files in parallel, and queried back.
- [ ] Ingest is order-independent: shuffling input chunks yields the same DB *or* the same deterministic rejection (tier-2 metamorphic).
- [ ] Same-key-different-value is deterministically rejected regardless of chunking/worker interleaving.
- [ ] One fact encoding, not two: a block is byte-identical on the wire and in a file.

---

## Phase 8 — Schema parsing (new grammar)

**Goal.** Parse schemas so predicate/type definitions aren't hardcoded — a separate schema
DSL feeding the same type model the query compiler uses. **Union types land here**, not in
[Phase 6b](#phase-6b--the-deferred-query-surface-), for three reasons that all point the same
way: `PredicateTy` (`schema.rs:23`) has four variants and no `Union`; there is no way to
*declare* one until this phase, because the schema is hardcoded Rust; and
[I10](docs/invariants.md#i10) freezes discriminants **the moment union data is written** —
chapter 6's "get it right *before* writing any union facts — after that it's an on-disk
migration". Implementing union select early would mean taking this phase's hardest one-way door
with no syntax for anyone to walk through it.

**Depends on:** Phase 7 (ingest validates against a real, parsed schema) + Phase 2 (the
permissive-then-narrow grammar discipline it reuses).

**Design of record:** the type model + identity (canonical form, per-predicate + whole-schema
fingerprints, subset-containment compatibility, embed-and-freeze) are
[chapter 6](docs/06-types-and-schema.md); the schema *syntax*, Go-style import/`mod`-tree
resolution, `schema_path` roots, and redeclaration errors are
[operations §7](docs/aperture-cli-design.md).

**Invariants in scope:**
- *makes green:* [I13](docs/invariants.md#i13) (`schema::ingest_rejects_incompatible_schema`
  + `schema::fingerprint_is_order_independent`); [I10](docs/invariants.md#i10)
  (`schema::discriminants_append_only`) when unions are represented.
- *upholds:* [I3](docs/invariants.md#i3) (reject schema changes that would violate on-disk marker ordering);
  [I11](docs/invariants.md#i11) — **a predicate id must fit the 24-bit fact-id tag**; the
  schema loader is where that is validated (the store rejects it today, but only at the point
  it would create the predicate's trees).

**Tasks:** schema lexer/parser; lower to the schema/type model; canonical form +
fingerprints; validate the freeze-invariants (stable discriminants, marker ordering); wire
the query compiler to load schema from parsed input instead of hardcoded fixtures.
**Union types, as their own thread:** a `PredicateTy::Union` variant with explicit
append-only discriminants; the declaration syntax for them; **a new codec marker** — the table
stops at `MARK_FACT_REF = 0x51`, so a union takes `0x52`, *appended* (which
[I3](docs/invariants.md#i3) permits) with the golden marker test edited deliberately, never
renumbered; and `X.alt?` lowering to a `ResidualOp::DiscriminantEq(n)` **plus a payload bind**
([chapter 6](docs/06-types-and-schema.md)). The payload bind is why this waits for Phase 6
rather than only for the DSL: a union payload is not a fact row, so it needs the `Slot` value
variant.

**Acceptance:**
- [ ] Parse a schema file and run a query against it end-to-end (test).
- [ ] Fingerprint order-independence green (tier-2: two source orderings → identical fingerprint).
- [ ] Ingest rejects a fact file whose schema fingerprint isn't subset-compatible (I13).
- [ ] Invariant-violating schema edits (renumbered discriminant, reordered marker) are rejected at load (I10/I3, tested).
- [ ] A union declares, ingests, round-trips through the codec, and `X.alt?` selects an
      alternative and binds its payload — the `nyi/union-select` corpus entry reclassified to
      `Supported` with its rows, and the code retired from `Code::ALL`.

---

## Phase 8b — Stored derivation

**Goal.** `predicate P : … = KEY where <query>` as **facts written at build time**:
`DerivedAndStored`. The half of "derived facts" that never reaches the executor — at query time
`P` is facts in a keyspace, scanned like any other predicate — so this is a *writer* and a
*lifecycle phase*, not a machine change.

**Depends on:** Phase 8, and that is the gating dependency: **a derived predicate cannot be
built before it can be declared**, and declaring one is the schema DSL. Also Phase 7 (a write
path to put facts through) and Phase 6 (the query it derives from must compile and run). This is
why it is not in [Phase 6](#phase-6--dynamic-derivation-the-machine-change-): nothing about it
needed the machine change, and everything about it needs a schema.

**Design of record:** [ops-I8](docs/invariants.md#ops-i8) (create → ingest base → derive →
finish; derivers read the frozen base via a *sealed snapshot*, write only derived predicates;
prefix-disjointness makes read/write disjointness structural; embarrassingly parallel, no
stratification in P0), [chapter 7](docs/07-compilation.md#two-kinds-and-only-one-of-them-is-the-executors-business)
(the two kinds), and [operations §9](docs/aperture-cli-design.md) (per-predicate trees, which
make dropping a re-derived predicate an O(1) tree delete rather than range tombstones).

**Invariants in scope:**
- *makes enforceable & tested:* [ops-I8](docs/invariants.md#ops-i8).
- *upholds:* [ops-I4](docs/invariants.md#ops-i4) — identity is `hash(canonical schema, base
  facts)`, so **derived facts are implied by identity, never part of it**; re-deriving must be
  reproducible. [I11](docs/invariants.md#i11)/[I12](docs/invariants.md#i12) at derivation scale.
  [I14](docs/invariants.md#i14) is *not* in scope: a stored deriver runs an ordinary query.

**Two things Glean's implementation settles for us — both cheap to write down now and expensive to
discover after schemas exist:**

- **Negation in a stored derivation is legal here, and that is a capability we get for free.**
  Glean forbids it — *"use of negation is not allowed in a stored predicate"*
  (`glean/db/Glean/Database/Schema.hs:451-462`), and if-then-else with it — for one reason:
  *"facts derived based on the absence of other facts could be invalidated when new facts are
  added to an incremental database. The work required to identify which fact to invalidate is
  close to that of re-deriving those predicates all over again"* (`:791-801`), propagated
  transitively through the derivation graph by a topological sort. The ban is a cost of
  incrementality, not of derivation, so **an immutable DB does not need it**: nothing can be added
  to a Complete DB, so nothing can invalidate a derived fact (`ops-I2`). Record it as a decision
  rather than leaving it implicit — and record it as a **one-way door**: if `ops-I9` ever reopens,
  every stored derivation containing a `!` becomes unsound, and because focus negates a
  *statement* rather than a pattern the transitive analysis would be at a different granularity
  than Glean's.
- **Write the query's *results*, not the body's output.** Glean's
  `Note [Writing derived facts]` (`glean/db/Glean/Query/UserQuery.hs:1074-1119`) records the trap:
  the statement that produces a derived fact is an ordinary statement, so the reorderer may place
  it **above** a later filter, and the fact set accumulated while the query runs then contains
  facts that are not true — the *results* are still correct, which is why the fix is to write the
  results and filter the fact set by them, never to dump what the body produced. `reorder` has
  exactly that freedom, so a deriver implemented as "run the plan and put what the body produces"
  is wrong in the same way. It is the same family of bug as the
  [I14](docs/invariants.md#i14) guard's lesson — only a derive step placed *above* a scan observes
  the fault — and as the `Source::Fetch` ordering hazard
  ([chapter 7](docs/07-compilation.md#what-flatten-defers-and-why)), which is dormant only
  because no fact is derived yet.

**Tasks (coarse — decompose at pickup):** a `derivation` on the schema's `Predicate` (there is
none today, and `focus`'s grammar has no `predicate` keyword — both are Phase 8's to add); the
derive phase in the lifecycle, reading a sealed snapshot and writing through the one write funnel
(`ops-I5`); `DerivedAndStored` vs derive-on-demand as a schema-level distinction; re-derivation
(**not simply a tree drop** — see below); derived-on-derived via sealed rounds — for which the
shape to copy exists: a
per-predicate completion list in the sidecar (Glean's `metaCompletePredicates`,
`glean/if/internal.thrift:74-80`, appended at `glean/db/Glean/Query/Derive.hs:242-251`) plus a
topological sort of the derivation graph with concurrency inside each stratum
(`glean/tools/gleancli/GleanCLI/Derive.hs:86-132`), which computes the round boundaries from the
schema instead of asking the operator to declare them.

**Blocked until one decision is made: re-derivation cannot be *only* a tree drop.**
[Open decision](docs/open-decisions.md#re-derivation-and-what-happens-to-the-high-water-mark).
Dropping a predicate's two trees is O(1) and is what the physical layout was chosen for — but
the allocator's high-water mark is recovered from the last key of the very `entities` tree being
deleted, so the next write to that predicate starts at sequence 1 and reuses ids, against
[I11](docs/invariants.md#i11). Meanwhile any dependent predicate already written still holds
references to the ids that were dropped. Whatever rule is chosen (a fresh DB per build, or
in-place on a Writable DB with the dependent subtree dropped alongside), it has to be decided
before this phase writes a derived fact, because the failure mode is a silently wrong answer
rather than an error.

**Acceptance:**
- [ ] A schema declaring a derived predicate parses, derives, and the derived facts are queryable
      exactly as base facts are — indistinguishable to the executor, which is the claim.
- [ ] `ops-I8` enforced and tested: a deriver cannot read its own writes or another deriver's,
      and cannot write a predicate it does not own (structural via prefix-disjointness).
- [ ] Deriving twice from the same base gives the same facts (`ops-I4`); re-deriving one
      predicate drops and rebuilds only its trees.
- [ ] **Only the query's results are written.** A derivation whose body would produce a fact the
      head's filters exclude writes nothing — tested with a plan the reorderer is free to schedule
      the derive above the filter in, so the test fails if the deriver dumps the body's output.
- [ ] A stored derivation containing `!` derives and is queryable — the capability the immutable
      DB grants, pinned so a later reader does not "restore" Glean's ban by analogy.
- [ ] Ingest is refused after `derive` in a way the lifecycle defines, not by accident.

---

## Phase 9 — Operations: a usable tool

**Goal.** Stop being a set of libraries. A person can create a database, put facts in it over the
wire, query it interactively, seal it, and throw it away — with the lifecycle, the runtime and the
CLI the design has specified all along.

**Depends on:** Phase 7a (a writable, queryable DB over a socket). **Not Phase 8** — see the
[ordering principle](#ordering-principle) for why a hardcoded schema satisfies what 9 needs from
it, and what pulling 9 forward buys.

**Design of record:** [`docs/aperture-cli-design.md`](docs/aperture-cli-design.md) **in full** —
CLI tree (§4), per-command requirements (§5), operational invariants `ops-I1`–`ops-I10` (§1), wire
protocol (§6), on-disk layout (§9), workspace structure (§10). Note §10 puts lifecycle, sidecar
read/write and store-root enumeration in **`aperture-store`**, not a crate of their own.

**Invariants in scope:** *makes enforceable & tested:* `ops-I1`–`ops-I10`. *upholds under a real
connection layer:* [I8](docs/invariants.md#i8), now exercised through portals rather than only by
the Phase 1 guard. *first interactive exerciser for:* [I4](docs/invariants.md#i4).

### What "usable if incomplete" means

```
aperture create mydb                      # Writable DB, built-in schema stamped in
aperture serve                            # owns the store root
aperture list / describe mydb             # sidecar scan; works while the server holds them
aperture query mydb 'X where src.File X'  # over the wire, streamed, Ctrl-C cancels
aperture shell mydb                       # REPL over the wire; \more holds the cursor
aperture finish mydb                      # seal: Writable → Complete, immutable
aperture db rm mydb
```

Facts arrive over the wire — the C# client under `clients/dotnet` already does this, and
`aperture-client` is its Rust twin.

**Deliberately outside it**, so the phase has an edge: fact files (7b), schema parsing and the
`schema` subcommands (8), `db backup`/`restore`/`verify`, shell completions, derivation (8b),
cross-DB (`ops-I9`), authentication (`ops-I10` stays default-closed), provenance/properties, and
per-predicate stats. Each is already listed in [operations §11](docs/aperture-cli-design.md) with
the seam that keeps it cheap.

### 9a — The database as an artifact ✅

`aperture-store` grows the lifecycle §10 assigns it.

- `APERTURE_META` sidecar, **versioned**, written temp → fsync → rename. Fields exactly §9's:
  name, instance, status, format version, schema fingerprint, content fingerprint (at finish),
  counts, size, created_at. **No `externally_modified` and no provenance** — `ops-I6` and §5 say
  so, and the versioned format is what makes them later additions rather than migrations.
- `Status` = `Writable | Complete | Broken`.
- Layout `<root>/<name>/<instance>/`, instance a provisional ULID.
- **The catalog is the filesystem** (`ops-I7`): enumeration walks the root and reads sidecars,
  never opening fjall — which is what lets `list` work while a server holds every DB.
- Store-root lock for `ops-I1`. fjall's own lock gives per-DB detection; the *root* is what
  `serve` owns.
- `create` materialises every predicate's keyspaces up front (§9: a keyspace costs ~30 ms) and
  embeds the canonical schema under `schema/` beside the sidecar.
- **One built-in schema.** It is currently written twice — `src/main.rs` and
  `src/bin/aperture-serve.rs` — and becomes one definition in the CLI package's lib target. The
  catalog never sees it: `create` takes `&Schema` and records its fingerprint, which is the
  down payment on [I13](docs/invariants.md#i13) that Phase 8 completes.

*Acceptance:* **done.** A killed process mid-create leaves either nothing or a Writable DB
(a child process aborted by a watchdog at six delays, with a census proving the kill lands
*inside* a create rather than always after it); `list` reads sidecars while a `FjallDb` handle
is held open; a second holder of the root is refused by name. 21 tests.

### 9b — `finish`, and identity that means something ✅

- `ops-I3` ordering: flush + `SyncAll` → compute identity → record → flip status **as the last
  durable act**. Tested by injecting a failure at each step; a crash mid-finish leaves Writable
  and the command re-runs.
- **Content fingerprint = `hash(canonical schema, base facts)`, computed for real.** Per
  predicate in `keys` order, with references expanded to their target's *logical* key,
  recursively, so no physical `FactId` enters the hash. This is exactly what
  [the nested-reference decision](docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline)
  made well-defined, and it is what `ops-I4` has been asserting since before it was computable.
  Recording a placeholder instead would put a lie in the artifact.
- Refuse to seal a DB with no facts unless `--allow-zero-facts`.
- After finish, every write-mode open is refused at establishment, forever (`ops-I2`).

*Acceptance:* **done.** The same facts written in a *different order* produce the same
fingerprint — with a non-vacuity check that the two databases really did assign ids differently;
different facts, a different value side and a renamed *referenced* fact each change it; finishing
twice is a no-op with a notice; an empty database will not seal without the flag; and a killed
`finish` leaves a Writable database the command re-runs on, never a Complete one without an
identity. 10 tests.

**One design point worth carrying forward.** §5 reads "record it in the sidecar → atomically flip
status to Complete as the final durable act", which sounds like two writes. It is **one**: two
would leave a window where a database is Writable *and* carries a content fingerprint that another
write would immediately invalidate. One rename means a crash leaves the old sidecar exactly as it
was — Writable, no identity, re-runnable — and the sidecar write is still the final durable act,
which is what `ops-I3` actually requires.

### 9c — The CLI, and the lifecycle commands ✅

`aperture-cli` (the root package, renamed). **First usable checkpoint.**

- clap tree per §4; every global arg `#[arg(global = true)]`.
- §2 address resolution: bare name → local socket, `aperture://host:port/db` → TCP, `--embedded
  <path>` → in-process. **Never a silent fallback** from connect to open (`ops-I1`); a missing
  server is a psql-style actionable error.
- Config layering per §3 (figment: defaults → file → `APERTURE_` env → flags, every field
  `Option<T>` so an unset flag does not clobber a lower layer).
- Commands: `create`, `list`, `describe`, `finish`, `db rm`, `serve`.
- Output rendering is **client-side** (`--format table|json|raw`); the server never produces JSON.

*Acceptance:* **done.** `create → list → describe → finish → list → db rm` end to end against the
real binary, with `list` showing the status change; `--format json`; a held root refusing every
lifecycle command by name while `list` and `describe` keep working (`ops-I1` with no silent
fallback, `ops-I7` needing no lock). 5 tests, no `assert_cmd` — `CARGO_BIN_EXE_aperture` is set
for integration tests, so the dependency buys nothing.

**Two things this step did not do**, both deliberately: figment's config *file* layer (defaults →
env → flags is the same shape with one layer missing, so adding the file is an insertion), and
routing lifecycle commands through a running server — which is 9d's, and until then a held root
is refused with a message that says so rather than opened anyway.

### 9d — The async runtime ✅

Two steps, because one would be unreviewable.

**9d-i — port to tokio, behaviour unchanged. ✅** The mechanical half: listener and sessions
became tasks. The design point that is *not* mechanical: **fjall is synchronous and the executor
is CPU-bound**, so neither belongs on the reactor. Every call that touches a store — ingesting a
block, compiling and running a query — is behind `spawn_blocking`, and what stayed on the reactor
is framing and scheduling. The engine no longer crosses back either: a query returns *encoded
bytes*, so the async side knows nothing of a `Plan`, an `Executor` or a `Value`.

That cut is what the rest of 9d is built on — once the engine is off the reactor, the reactor is
free to interleave streams, flush in chunks and notice a cancel, none of which is possible while a
query owns the thread that would have to do them. All nine socket tests pass unchanged, driven by
a deliberately **synchronous** client: a client written against the wire format should need
nothing of the server's runtime, and that is where it is checked.

**9d-ii — what the runtime was for. ✅**
- Per-connection reader task, and a **writer task doing round-robin over bounded per-stream
  queues** (§5). Without it one chatty stream starves the socket even when the executor has
  capacity — which is true of the server today, and said so in its module docs.
- **Per-DB single writer task** (`ops-I1`, `ops-I5`): serialised writes made structural rather
  than promised. fjall's non-transactional path loses updates on concurrent read-modify-write,
  and a Writable DB is single-server-owned anyway, so serialisation is free.
- **Chunked `DataRow`s** off the executor's `Suspended`: a large result never buffers and never
  monopolises the connection.
- **Per-stream `Cancel`** in band, mapped to the `CancellationToken` the executor already takes.
- **Lifecycle over the wire.** §5's rule is that every DB-taking command works against any
  address, with the server implementing it on the same core code as the embedded path — so
  `create`/`finish`/`drop` become control messages rather than requiring the server be stopped,
  and the server grows a **registry**: the store root and the databases open under it, in a map it
  can change. Two front doors, one implementation.

*Acceptance:* **done.** A long query and a short one on one connection: the short one completes
after **0** of the long one's 4000 rows. A cancel stops a stream inside a chunk and leaves the
connection answering on another. A thousand-row result crosses four chunks — three real resumes
through the bytes-only cursor — and comes back as a thousand *distinct* rows, which a resume that
dropped or repeated one would not. And the whole command sequence — create, write, query, seal,
delete — runs against a server that is up throughout, both over a socket (6 tests) and through the
real binary (3). The .NET client passes unchanged, which is what says the new frames are additive.

**Three of the five needed a way in, not five, and that is `ops-I7` rather than a shortcut.**
`list` and `describe` read sidecars and never open fjall, so they already worked while a server
held every database under the root — they were never the commands being refused. Only `create`,
`finish` and `remove` mutate, and those became control frames on an ordinary stream: fair
queueing, per-stream errors and task isolation all come free, and a `create` costing tens of
milliseconds per keyspace does not stall the connection's reader. §5's *remote* `list` is the
virtual predicate `aperture.db.List`, which is 9f's and is the normal query machinery.

**How a command decides, in one place.** `commands::route` — a server listening on the derived
socket takes the command; nothing listening means this process does the work under the root lock.
That is §2's resolution rather than a fallback, and the ordering is the whole of it: the forbidden
thing is to try the server, fail, and open the directory *anyway*, because a server might be
holding it. Here nothing is opened until the socket has said none is. The lock stays the authority
— a root held by something that is not listening is refused by name, with both halves in the
message, which is the situation that actually needs one.

**Two hazards the server has and the offline path does not**, each answered where it arises. A
second handle on a store this process already holds: `Catalog::finish_held` takes the handle the
caller has, and a store test pins that a database sealed through it is the same artifact — same
identity — as one sealed by opening the directory, or `ops-I4` would depend on which door a build
came through. And a database pulled out from under a session: `remove` takes it out of the map
first, then deletes it only if the registry turns out to hold the last reference; otherwise the
entry goes back and the request is refused by name, as psql does and for the same reason.

**An `ops-I2` hole found on the way and closed.** The handshake never checked status — a Complete
database accepted a read-write session, because the server had no idea what status anything was.
It refuses at establishment now, and the interesting half is the session established *before* a
seal: that one is caught inside the per-database writer lock, where the seal happens, so a block
either takes the lock before it and the seal waits, or takes it after and finds the database no
longer writable. There is no third order, and breaking either check fails exactly one test.

**One bug worth carrying forward, found by the .NET demo and not by the Rust tests.** Rows were
matched to their type *by name*, and that cannot work: a `PredicateTy::Record` holds a bare
`Spur`, so `Desc::to_ty` has to discard which **tier** of the two-tier interner a name came from,
and a local `Spur` and a schema `Spur` of the same number are *different names* — so resolving one
afterwards does not fail, it silently answers with the wrong string. It only surfaced once the
compilation's own interner was reused (which is itself required, since a `Plan`'s projections hold
symbols minted there). Matching is positional now, which is not a weaker check but the only
correct one: descriptor and row come from the same head type walked in the same order, and
`encode_value` zips positionally too. The names are not lost — they live in the `Desc` the client
receives.

### 9e — `aperture-client` ✅

The Rust twin of the C# client: connect, handshake, session modes, stream multiplexing, the write
stream, and a query stream **that holds its cursor**. Used by the CLI, the shell, and any Rust
deriver.

*Acceptance:* **done.** The Rust and C# clients produce byte-identical blocks for the same facts —
a fixed corpus the .NET side writes out as hex (`./clients/dotnet/emit-golden.sh`) and the Rust
side encodes independently and compares, plus the schema fingerprint alongside it so a divergence
says *which* of the two things disagreed. The corpus is chosen for what it reaches: a value side,
two levels of nesting, a record inside a key, two references to two predicates, and integers on
both sides of the varint's one-byte boundary. 10 client tests.

**One edge had to be turned round first.** The message vocabulary lived in `aperture-server`, so a
Rust client would have had to depend on the server — fjall, the engine and a runtime, to send a
handshake — or keep a second copy of the message formats. The second is worse than it sounds: the
.NET client exists to *detect* drift between two implementations of this protocol, and a second
Rust copy would be drift we caused ourselves. `protocol` is in `aperture-wire` now, which is where
[operations §10](docs/aperture-cli-design.md) always put it.

**A result is a bookmark, not an iterator**, and that is the design decision worth carrying into
9f. `Rows` holds no borrow of the connection, so several results can be open at once — which is
what the stream id, the server's per-stream tasks and a shell that holds one result at `\more`
while running another query all need. `Connection::take(&mut rows, n)` reads *n* rows and stops;
nothing is buffered at either end, because the place is kept by the *stream* staying open. Between
pages the server is parked on a full outbound queue holding a bytes-only cursor with its snapshot
already released ([I8](docs/invariants.md#i8)), so a pause of a millisecond and a pause of an hour
cost it the same thing. A test already checks that a paged read of a thousand rows, in pages of 37,
concatenates to exactly an uninterrupted run — [I4](docs/invariants.md#i4) from a client, ahead of
the shell that will make it interactive.

Frames for a stream nobody is currently reading are **parked, not dropped**. Since 9d-ii the server
interleaves, so frames arrive in whatever order the work finishes; a client that assumed its own
order would silently discard another stream's answers. Breaking the parking fails the
two-results-open test and nothing else.

### 9f — `query`, `shell`, and the cursor ✅

- `query`: streams incrementally, `--timeout`, Ctrl-C → a per-stream Cancel rather than a
  connection teardown.
- `shell`: **remote-first, always over the wire**, so the wire format has a permanent exerciser.
  `\l` (via `aperture.db.List`), `\d [pred]` with prefix fallback, `\c`, `\timing`, `\cancel`,
  `\q`, readline history.
- **`\more` holds the cursor and resumes it.** The highest-value item in operations §5 by a
  distance: the Phase 5 REPL discards the resume token at both call sites, so
  [I4](docs/invariants.md#i4) and [I8](docs/invariants.md#i8) — a bytes-only cursor and the most
  heavily tested machinery in this project — have **no interactive exerciser at all**. Paired with
  a truncation footer that names the knob.
- **`\profile`** — facts searched per predicate with a full-scan flag: the *outcome* to `:plan`'s
  *intent*. The executor already counts rows examined for cancellation; this surfaces it.
  **Built ahead of the shell**, because it is the instrument performance work needs and the shell
  is not: `Executor::enumerate_profiled` hands back the counter the cancellation stride was already
  keeping, per step, and the server pairs it with the plan's names and a full-scan flag. Reached
  today through `aperture query --profile`; `\profile` is the same thing with a prompt in front of
  it. The counter is per **step of the plan's body**, not per predicate, because that is what the
  machine counts and what a disjunction, a fetch and a negation each need a slot of; the server is
  where positions become names, since a client holds a query's text and never its plan.
- `aperture.db.List` as a **virtual predicate** through the normal query machinery — no bespoke
  control message for enumeration.
- **TCP opt-in**: `--listen-tcp host:port`, default-closed (`ops-I10`), operator responsible for
  the gateway in front of it.

*Acceptance:* **done.** `\more` returns the next page and the concatenation equals an
uninterrupted run — [I4](docs/invariants.md#i4), interactively, for the first time. A
thousand rows over four server chunks, read forty at a time, compared against the same query
taken in one go, *in order*: a count would pass for a resume that dropped one row and
repeated another. Mutation-checked by dropping one row per page.

**Two shells, not one re-pointed.** This step's plan said 9f would re-point the Phase 5 REPL
at the wire client. It added a second one instead, and the reason is worth keeping: `:plan`
and `:type` need a compiler in the same process as the question, and a client holds a query's
text and never its plan. `aperture shell <db>` is the wire shell; `aperture shell` is still
the embedded demo over its own scratch database.

**`aperture.db.List` is a virtual predicate, and it is answered at the `FactStore` seam.**
The obvious home for one is the executor — a `Source::Virtual` beside `Seek` and `Fetch` —
and that is the wrong place: `FactStore` is already the answer to "where do rows come from",
and the executor is generic over it, so answering a predicate from memory is a different
answer to the same question rather than a new question. `Catalogued` wraps the store,
encodes the listing through `aperture_store::fact::encode` so every row is `predicate_id ++
key` byte for byte, and sorts it — after which nothing above it can tell the difference. The
plan IR gains no variant, the cursor gains no case, `enumerate` is untouched, and I4 needs no
re-proving, because the resume battery is already written over an arbitrary `FactStore`. What
it costs is that `:plan` shows a scan of predicate 22 and says nothing about where its rows
live.

Virtuality is a property of the **server**, not of the database, and three things follow that
nothing had to be told twice: it is skipped by the handshake fingerprint, skipped by the
schema copy embedded at create, and given no keyspaces. So a client that has never heard of
`aperture.db.List` still connects — the .NET clients were not touched — and no artifact
claims to hold a kind of fact nothing can write.

**`--listen-tcp` needed both halves.** The server binding a port is untestable while the
client speaks only Unix sockets, so `Connection` grew a `Transport` enum and the CLI grew
§2's `aperture://host:port/db` form, resolved in the one place `query` and `shell` share. The
test asks one server the same question through both doors. `ops-I10` stays default-closed:
no config-file entry, no environment variable, and the startup banner says so when a port is
open.

**Acceptance — the phase:**
- [x] The command sequence at the top of this phase works, over the wire, against a running server.
      *The lifecycle half since 9d; `query` and `shell` complete it in 9f.*
- [ ] `ops-I1`–`ops-I10` enforced and tested end-to-end (`assert_cmd`/`trycmd`).
      *`ops-I1`–`ops-I7` are, over a socket and through the binary. `ops-I10` is now tested at
      the only thing it claims — a port opens **only** when `--listen-tcp` names one, and the
      same question answers identically through either door. `ops-I8` (the hoisted finalization
      phase) and `ops-I9` (cross-DB) are unbuilt, and are the reason this box is not ticked.*
- [x] A long query does not delay a short one on the same connection.
- [x] `\more` is an interactive [I4](docs/invariants.md#i4) exerciser, and the pages concatenate to an uninterrupted run.
- [x] The same inputs built twice produce the same content fingerprint (`ops-I4`) — including
      when one of them was sealed through a handle the server already held.
- [x] The workspace matches [operations §10](docs/aperture-cli-design.md) (see the cross-cutting note),
      including the dependency direction it states: `client → wire`, and nothing depends on the server.

**What is deliberately still missing afterwards**, so "incomplete" is a statement rather than a
discovery: telemetry and tracing spans, one end-to-end error taxonomy, `db verify`/`backup`/
`restore`, and everything in the "outside it" list above. None blocks daily use; all are listed
here rather than found later.

---

## Phase 10 — Capacity: measure it

**Goal.** Find out whether Aperture holds up for a few hundred to ~1000 concurrent users
issuing overlapping queries of mixed complexity — by building a ladder of measurement
surfaces from the executor upward, recording what each costs, and writing the findings
down. **Measurement, not optimisation.**

**Depends on:** Phase 9a–9e for the server and client. S1–S3 depend on nothing further.

**Design of record:** [`docs/performance.md`](docs/performance.md) — the method, and the
**target** every number is reported against. The phase's own working notes, including the
eight hypotheses read out of the code before any of it was measured, are
[`docs/phase-10-capacity.md`](docs/phase-10-capacity.md); the register of what was
measured is [`bench/FINDINGS.md`](../bench/FINDINGS.md). **Read those; don't restate them.**

**Invariants in scope:** *makes green:* none. *upholds:* all of them — measurement adds no
behaviour on any data path. The production edits this phase made are counters, a feature
gate, two bug fixes and two key-order declarations, each carrying its own guards.

**Why it is a phase at all.** Performance was a non-topic in this file: no phase, no
target, no acceptance criteria, no cost model, and one sentence deferring file ingestion as
"a throughput feature". Meanwhile a real apparatus had grown with no home — a load
generator, a cost breakdown, a soak — none of it mentioned in `PLAN.md` or `CLAUDE.md`. The
gap that mattered was not the instruments but the **target**: a number with nothing to be
good or bad against is a number nobody can act on.

**Tasks:**
- **10a. Write the capacity target down.** ✅ [`docs/performance.md`](docs/performance.md) §1
  — corpus size, population, mix, latency objective, and what is deliberately *not*
  targeted. Stated as a proposal derived from measurement rather than from a requirement,
  because [operations §1](docs/aperture-cli-design.md) is right that this repo cannot settle
  a requirements question on its own; it is written to be argued with and replaced.
- **10b. `docs/performance.md`** ✅ — the method beside [`docs/testing.md`](docs/testing.md):
  the ladder, what each rung isolates, the four rules that make a number reportable, host
  fingerprinting, and which findings are guardable exactly and which only as a budget.
- **10c. S0** ✅ — `src/workload.rs`: the catalogue and the pivot sampling, stated once.
  `engine`, `loadgen` and `soak` had three of each, which is how `loadgen` came to seek a
  key computed as `files / 2` — real only in a corpus it seeded itself. Pivots are
  **sampled**, so the same catalogue runs against somebody's checkout.
- **10d. S1–S3** ✅ — `examples/engine.rs`, three layers, run against an 18.2M-fact index.
- **10e. S4** — `examples/breakdown.rs` extended to a data query. **Not done**: finding 9
  separates the row encoder from everything above it, and "everything above it" is still one
  number covering the frame, the outbound mutex, the socket and the client's decode.
- **10f. Server counters** ✅ — `ServerStats`, relaxed atomics on the `Registry`, gauges held
  by a `Drop` guard. **Deliberately counted and not exposed**: an exporter is a separate
  decision with an operational cost, and a `/metrics` port is the shape `ops-I10` refuses.
- **10g. S5–S6** ✅ — the population sweep to 2048 clients: capacity plateaus and does not
  collapse, zero errors, and the cheap query stays on the right side of the expensive one by
  a factor of 7,400.
- **10h. S7** ✅ — fifty minutes, 145,582 queries, no drift at any percentile; `FINDINGS.md`
  ranked and costed.

**Acceptance:**
- [x] A capacity target is written down, and every later number is reported against it.
- [x] Each rung has an instrument that runs from a clean directory; a second run reproduces
      it within noise.
- [x] **Every instrument is self-checking** — a workload's row count and per-step examined
      counts are fixed by an unmeasured probe, and a timed run that fails to reproduce them
      aborts with the discrepancy rather than printing a rate.
- [x] **Vacuous-pass controls**: the zero-data baseline examines exactly 0 rows and still
      costs something; a full scan reports `full_scan = true`.
- [x] Cross-rung agreement: S1 row/s > S4 row/s > S5 row/s, with the differences accounted
      for.
- [ ] The scaling curve published for 10k → 10M facts on real data. **Not done** — what is
      published is one 18M-fact database whose predicates span 142 → 8.58M rows, which is a
      scan-size curve at fixed database size. The size bands are hours of indexing each.
- [x] Cross-connection fairness answered with a number, at N = 1 … 2048.
- [ ] F1–F8 each carry a verdict. **Seven of eight**: F1, F3, F4, F7 confirmed with numbers,
      F2 and F5 refuted, F8 observed. **F6** — the reader head-of-line blocking on a ≥3-block
      ingest — is untouched, and is the only one needing a *write* path: every database
      measured here is `Complete`.
- [ ] `bench/baselines/<host>.json` and a `--check` mode. **Not built**; the numbers live in
      `FINDINGS.md` and are reproduced by re-running the instrument.
- [x] `cargo test`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo fmt
      --all` green; the coverage ledger unchanged in content — this phase adds no guard and
      retires none.
- [x] Release only.

**Two findings were taken out of "measurement only", and both because leaving them would
have made every later number a measurement of a bug.** Sealing now merges every tree —
an unmerged one was seeking at up to 180× a merged one, and the artifact halves on disk —
and a client that vanished mid-answer no longer strands the stream answering it, which was
2.3 GB that never came back. A third change is not a bug fix but a one-way door taken while
it was still cheap: the two key field orders that decide whether a join seeks are now
**declared** rather than inherited from alphabetical naming, which is a re-index today and a
migration once somebody's index is in production.

---

## Cross-cutting — workspace extraction

The design's target layout ([operations §10](docs/aperture-cli-design.md)) is a Cargo
**workspace** (`aperture-schema` / `-encoding` / `-store` / `-engine` / `-ingest` / `-wire` /
`-client` / `-server` / `-cli`).

**The first four are done**, ahead of Phase 7 and on purpose: ingestion is the first thing that
needs a real store/encoding boundary, and extracting it afterwards would mean moving ingestion
too. `-ingest` starts as a new crate with a clean edge; `-wire`/`-client`/`-server`/`-cli` are
Phase 9's, and the root package stays the shell until it grows a command tree. Each extraction's
"green test" was: everything compiles, clippy is clean, and all 420 tests pass — held at every
step.

**It was billed as mechanical and two-thirds of it was, but the third that was not is the part
worth reading.** Two edges pointed the wrong way and had to be *designed* out before any file
moved: the umbrella error was returned by the codec and the store themselves, and `plan.rs`
held both the query plan and the storage seam. And the split found a coupling no module
boundary can show you — four store tests reached into the engine, which across a crate edge
compiles a **second copy** of the store, making the `FactStore` under test a different type
from the one the engine links. Three did not need the engine at all; the fourth is now an
integration test, and the rule is written down in [testing](docs/testing.md).

---

## Deferred features (additive — must not reshape the machine)

Not on the critical path; each is additive: order comparisons (`ResidualOp`
arms); `pattern = pattern` full unification (easy half in Phase 4, reject the three hard
cases); `evolves`; cross-DB query. Detail and kept seams:
[`CLAUDE.md` scope](CLAUDE.md#scope-phases--open-decisions),
[open decisions](docs/open-decisions.md), [operations §11](docs/aperture-cli-design.md). The
two *non-additive* constructs — derived facts (Phase 6) and the now-resolved `FactRef` marker
— are handled as deliberate changes above.

**Five items left this list and took phases**, because "additive" was a claim about the
*machine* and never one about size: disjunction, `never`, negation and subqueries are
[Phase 6b](#phase-6b--the-deferred-query-surface-), and unions-as-data is
[Phase 8](#phase-8--schema-parsing-new-grammar). What they had in common as bullets was no
acceptance criteria and no invariant accounting — while one of them changes the resume token
and one of them freezes bytes on disk.

### Reaching a fact through a reference — three sizes, listed apart ✅ (Phase 5)

Phase 4 deferred everything to do with a fact-typed field under two codes, which made three
very differently-sized pieces of work look like one — and left a schema built around
references, which is what a fact database is for, unqueryable past a whole-row scan. All three
landed in Phase 5; the sizes are kept because they held, and because the fourth piece below is
still open and is estimated against them.

| # | Work | Size | Unblocked |
|---|---|---|---|
| 1 | **Fact-id splice / compare** — `SeekKeyPart::RegisterFactId(Address)` + `ResidualOp::EqRegisterFactId`, encoding `MARK_FACT_REF ++ id` off `Register::fact_id`. **No store read**, so [I6](docs/invariants.md#i6) stays structural. | small (~40 lines: `plan.rs`, `iter.rs`, `flatten.rs`) — held | `P = test.Foo {id = 1}; test.Ref {of = P}` — every join through a reference |
| 2 | **Capture-and-project a reference** — the rule narrowed from "a fact-typed field is deferred" to "**may be captured, projected and matched, never navigated**". No IR change: `Project::RegisterField{ty: Fact}` already decoded to `Value::FactRef`. | one relaxed check plus the diagnostic the trap below needs — held | projecting *which* facts a relation points at |
| 3 | **Hoist a nested generator** — `test.Ref {of = test.Foo {id = 1}}` becomes its own loop level, bound to a name the query did not write. Flatten-local. Needed #1 to be worth anything. | medium, flatten-only — held | the idiomatic nested-pattern spelling, which is how one writes this query |

**The trap #2 walked into, as predicted.** `flatten::resolve`'s `.value`-on-a-non-row arm
declined *quietly*, which was correct only while `collect` reported `nyi/fact-field` before a
variable could be bound to a fact-typed field. Allowing the capture made the arm reachable and
`plan()` would have returned `None` with an empty sink — breaking its documented promise that a
refusal always has a reason. The `debug_assert` in `flatten_ordered` is what would have caught
it; the diagnostic was written as part of #2 rather than after it, and both arms of "reading
through a reference" are now corpus entries.

**What was not predicted** is that the census made this test work rather than implementation
work. A new plan shape has to be *reached by the generator*, or the resume battery says nothing
about it — so the `(query, store)` generator grew fact-typed fields, and reaching the residual
form reliably took a deliberate draw rather than a chance one (2 in 300 left to chance).

**The fourth piece — reading *through* a reference — landed in Phase 6b**, and it was the
largest of the four as estimated, but not in the way the estimate said. It needed no new slot
kind: `Source::Fetch` is a source arm, the register it binds is `predicate_id ++ key` (byte for
byte the row a scan would have produced), and `dereference` substitutes the fetched row for the
reference between resolving a base and reading a field out of it — so `Slot` gained nothing and
`field_slot` got shorter. What it did need was a new **frame** shape: the frame's `scan` became
`Rows`, a scan or a fetched row, which is the one place a point read and a range differ to the
machine.

Everything in the table above is a compare against an id already in a register, which is why
none of it needed a lookup; this is the one that does, and it is a lookup per row the level
above it *produces* rather than per row a scan examines — which is why
[I6](docs/invariants.md#i6) holds with its "or navigated" clause rather than despite it.

`nyi/fact-field` now means one narrow thing: a reference held in a fact's **value**.

---

## Phase 11 — A code-search site, and what it took ✅

**Goal.** Build Glean's code-navigation demo on Aperture — browse a repository, open a
file, click a symbol, land on its definition — plus the three things that demo implies
and Glass actually serves: search, find-references, a symbol panel. Then fix what
building it turned out to need.

**Design of record:** [`docs/phase-11-code-search.md`](docs/phase-11-code-search.md),
which is the gap analysis it started from *and* the record of what came of it. The
analysis is kept as written rather than edited to match the outcome.

**What landed, in the order it landed:**

- **The stream leak's mechanism**, which `bench/FINDINGS.md` §7 costed and named a
  connection pool as the shape that hits. A stream's task now ends when its work does,
  the reader sweeps dead handles, and the client recycles ids.
- **Five key orders in the schema** — `src.FileXRef`, `src.DeclSpan`,
  `src.SearchByLowerName`, `src.DerivesFrom`, `src.AttributeOf` — plus `at.length` on a
  reference. Three of them are the same data keyed a second way, which is what a stored
  derivation would materialise and is why five comments now carry the same apology.
- **Order comparisons**, as a byte compare over the order-preserving encoding
  ([I1](docs/invariants.md#i1)) rather than a decode. `X < 3` was a **lex** error before
  this — the one place the grammar broke its own permissive-early rule.
- **Arithmetic**, which is the first thing in focus to lower a `Step::Derive` at all.
- **A cursor the client can hold**, so paging stops needing the connection — which
  nothing could work around, since "everything after key K" is not expressible.
- **`QUERY_COUNT`**: the same plan with a different accumulator, so a search UI can say
  how many results there are without receiving them.
- **[`aperture-viewer`](crates/aperture-viewer)**, over `aperture-client` and nothing
  below it.

**Not built, and each for a stated reason:** stored derivation (Phase 8b, gated on
re-derivation vs [I11](docs/invariants.md#i11)); a general `ORDER BY` (materialisation
or a reverse-scan change to the machine, and nothing wants it — ranking is a judgement,
and the viewer makes it over a bounded window); request batching (a protocol feature
with its own design questions, off the path).

**Invariants in scope:** *upholds* [I1](docs/invariants.md#i1) (a comparison **is** the
order-preservation property, used somewhere other than a seek for the first time),
[I4](docs/invariants.md#i4) (a cursor now crosses the wire, so the plan-fingerprint
check is what stands between a caller and a plausible wrong answer),
[I6](docs/invariants.md#i6)/[I9](docs/invariants.md#i9) (every new residual is a
borrowed span, no decode and no allocation), [I14](docs/invariants.md#i14) (a derived
bind is still a pure function of the fact bindings, which is what lets resume recompute
it).

---

## Related prior design work

- **[The design book](README.md)** — the engine design of record (codec, storage, executor,
  resume, types, compilation) with every invariant and its rationale.
- **[`docs/aperture-cli-design.md`](docs/aperture-cli-design.md)** — the operational design
  of record: lifecycle, `ops-I*` invariants, ingestion pipeline, fact-file format, schema
  resolution/identity, wire protocol, on-disk + workspace layout. **Primary reference for
  Phases 7–9.**
- **VM / ISA design** (external note) — a fixed-width 64-bit ISA from rewriting Glean's C++
  query VM. Relevant *only if* Aperture ever moves to a bytecode VM (currently a deliberate
  divergence — we implement the abstract machine directly). Not needed for any current phase.
- **`src/lens/`** — the disconnected first-attempt front end, which was the reference for
  re-implementing parse/typecheck/lower/flatten into `focus` (Phases 2–5). **Deleted**, its last
  file retired by hoisting. Recoverable from git history if a later phase wants to look.
