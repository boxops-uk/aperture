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

The engine spine exists in `src/focus/`:

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
- **Front end** — `lex → parse → lower → typecheck` is live in `src/focus/` (Phase 2 done):
  the full intended surface parses, lowers to the `SyntaxTree` store, and typechecks, with
  every construct deferred to a later phase drawing one specific diagnostic naming it. Three
  acceptance artifacts: `focus::corpus` (the audit table as data, with a parse gate and a
  diagnostic-code gate over it), **`parse ∘ print == id` on generated trees** (`focus::print`
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
- **The compilation driver** (`focus::compile`, Phase 3 done) — one `Compilation` carrying
  the source, schema, interner, diagnostics sink and the trees the phases produce. A phase
  reports by pushing into the sink and cannot return diagnostics; codes are a `Code` enum
  rather than strings; rendering sorts into source order while the sink keeps arrival order.
- **A focus shell** (`src/main.rs`, Phase 5 done) — reads a query, highlights it from the
  compiler's own lexer, **compiles it through the driver and runs it** against a real `FjallDb`
  seeded from `focus::fixture`; `:plan` shows the plan without running it and `:facts` scans a
  predicate. Its database is the corpus's, so anything the corpus classifies `Supported` is
  typeable at the prompt and returns the rows recorded there.
- **Store** (`store.rs`) — the fjall store is complete and guarded (Phase 1 done): a pair of
  keyspaces per predicate (`keys.<id>`, `entities.<id>`), `scan`/`point`, and an atomic
  `put_fact` over a snowflake [`FactId`](docs/03-storage-model.md#factid-allocation-i11) with
  a per-predicate allocator recovered from the data. Held to `MemStore` as a differential
  oracle. [I8](docs/invariants.md#i8), [I11](docs/invariants.md#i11),
  [I12](docs/invariants.md#i12) green — the I12 crash case aborts a child process mid-write,
  and the I8 guard cross-checks a drop probe against fjall's own open-snapshot count.
- **One fixture database** (`focus::fixture`) — the schema, the facts and the example queries
  the corpus, the batteries and the shell all share, deliberately not test-gated. Before it
  there were two databases and a corpus entry was not something a person could run.
- **Writing a fact by hand** (`focus::fact`) — `FjallDb::put(&schema, &fact)` takes a
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
                                                                                    │      └─▶ 6b  deferred query surface  (`|`, never, `!`, subquery)
                                                                                    └─▶ 7  ingestion ─▶ 8  schema (+ union types) ─▶ 8b  stored derivation ─▶ 9  operations

Cross-edges:  6 also depends on the resume battery (0) + fjall (1).   7 depends on 1 (store + atomic put_fact).
              8b (stored derivation) is gated on **8**: a derived predicate cannot be built before it can be
              declared. It needs 7 to write through and 6 to run the query, and shares ops-I8's lifecycle with 9.
              It needs nothing from 6's machine change — a stored derived predicate is facts, scanned like any other.
              6b depends on 6 for the **Cursor**: it is `Vec<Register>` today and `Vec<Slot>` after 6, and
              disjunction adds a per-branch discriminant to the same token — edit it once, re-prove I4 once.
              Nothing in 7–9 depends on 6b, so its position *before* 7 is a choice, not a constraint.
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
- **0a. Shared test machinery.** Promote `focus::mem_store` (started) + a schema/fixture
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
- *makes green:* [I8](docs/invariants.md#i8) (`store::snapshot_released_at_suspend`, drop-probe),
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

**Goal.** Bring the focus grammar in `src/focus/` up to the full intended feature surface,
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
  **executable** — `focus::corpus` holds it as data (37 entries, since grown), each classified
  `Supported` / `Diagnosed(code)` / `ParseError`, so it cannot drift from what the compiler
  does. Running it before touching the grammar gave the audit empirically: 6 entries did not
  parse, and they were exactly the six constructs 2c adds.
- **2b.** ✅ Lexer: token boundaries pinned (`E.from` ≠ qualname, `a.B.c`, `..` munch,
  keywords) and the literal decoders added — `parse_nat`, `signed_literal`, `unescape_str`,
  each reporting by code. **Prerequisite discovered:** nothing in the grammar was testable,
  because `focus` had no parse entry point and no CST façade; those landed first
  (`focus::cst`, `focus::parse`).
- **2c.** ✅ Grammar: parens (group + subquery), `never`, union select, flat disjunction,
  statement negation. Resolutions above.
- **2d.** ✅ Façade → `SyntaxTree` store lowering (`focus::lower`), with sorted-slice record
  fields and a duplicate-field rejection. The boxed ergonomic AST (representation 3) is **not**
  built — nothing needs it yet — so `lens/query.rs` survives.
- **2e.** ✅ Typecheck (`focus::ty`, re-implemented from `lens/ty.rs`) against `PredicateTy`,
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
- **3a.** ✅ `focus::diag` — the sink and the code taxonomy. `Code` is an enum (20 variants,
  `as_str` rendering exactly the strings Phase 2 used, `kind` deriving the prefix); the
  `Diagnostic` alias moves out of `parser.rs`, which is generated-parser glue; `Diagnostics`
  reports with either span type and filters `has_errors` by severity.
- **3b.** ✅ Phases take the sink and cannot return diagnostics — `parse → Option<Cst>`,
  `lower → Ast`, `check → Typed`. *Done:* the corpus gates pass **unchanged**, which is what
  proves the signature change altered no behaviour.
- **3c.** ✅ `focus::compile::Compilation` — source, schema, interner, sink, tree and side
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
- **5b.** ✅ **One fixture database** (`focus::fixture`): the schema, the facts and the example
  queries the corpus, the batteries and the shell share. Not test-gated, because the shell is
  not a test.
- **5c.** ✅ Rendering a `Plan` for a person, in `focus::print` beside the two renderings of a
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
([Phase 6b](#phase-6b--the-deferred-query-surface)). The machinery is built ahead of them on
purpose, because its resume behaviour is the expensive thing to get wrong later; it is exercised
by hand-built plans, and I14 records that scope rather than implying pressure the language does
not yet apply.

---

## Phase 6b — The deferred query surface

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

**Decisions this phase must settle at pickup (deliberately not pre-made):**

- **Does negation read the store inside the row loop — and is that I6?** `!test.Bar {id = X}`
  per outer row is a probe: *does this seek find nothing*. [I6](docs/invariants.md#i6) is about
  **values** — residuals on key fields are checked against the `keys` CF only — so a key-only
  probe does not breach it as written. But it is the same shape as the deferred
  `Access::Fetch`, which this plan treats as real work with a new slot kind. Decide explicitly
  between a probe in the scan loop, a semijoin frame, and deferring negation again; do **not**
  inherit "additive" as an answer.
- **Does `never` get a type?** Phase 2 declined `Ty::Never` because a type for it would be
  speculative. Disjunction is exactly what makes it non-speculative — `never` is the identity
  of `|` — so the decision belongs *inside* 6b-a, not before it. `never` implemented alone is a
  keyword with no consumer.
- **Is a subquery inlined?** Phase 2 made group and subquery **one grammar rule** so that "a
  subquery is the same shape as a query, so lowering reuses the query algebra". If a subquery in
  a generating position inlines its statements into the enclosing generator list, this is
  flatten-local and needs no operator at all. Confirm that before budgeting a nested executor.

**A risk `Deps` cannot express today, and this phase is where it becomes real.** Glean tags every
statement `Ordered` or `Floating` — "may not move" versus "may"
(`glean/db/Glean/Query/Flatten/Types.hs:70-77`) — and its negation-placement rule is **semantic**,
not a heuristic: a negated subquery is forced *after* every parent-scope variable it uses is
bound, because an unbound variable inside a negation behaves as a wildcard, so moving it changes
what the query *means* (`Note [Reordering negations]`,
`glean/db/Glean/Query/Reorder.hs:547-573`). `StmtDeps` records only what a statement can capture
and what it must read, which cannot say "this one may not move above that one" — and the frontier
is free to emit anything runnable. Safe today, because nothing in focus is order-sensitive in that
way; unsafe the moment negation and disjunction have an engine. The `// TODO` in `reorder.rs`
already names half of it ("move negations/conditionals after their non-locals are bound");
*expressing* immovability is the design question, and it must be answered before 6b-d lands, not
after. The same nesting is why Glean's `Reorder` needs a give-up branch where focus's greedy pass
does not: a nested group's reads depend on how its own branches are ordered, which is exactly
where the monotonicity argument fails — so re-prove completeness here rather than inheriting it.

**Architecture note, written before pickup:** [`docs/query-surface.md`](docs/query-surface.md)
argues one shape for this whole phase and for the deferred items behind it, on the finding that
**only disjunction touches the resume token** — negation, `never`, subqueries, `Access::Fetch`,
primitives and comparisons are all filters, deterministic binds, or compile-time rewrites, and a
construct costs cursor work only if it can be mid-flight when a row is handed out. Two of its
recommendations amend this phase and are not yet folded into the tasks below: negation's
placement wants a **reads-edge** rather than an immovability tag (Glean's own rule is "after the
binding of all parent-scope variables it uses", which is what `reads` means), and branch scope
wants the **intersection** of what the branches bind rather than 6b-b's rejection.

**The machine half of 6b-a is done** (`plan.rs`, `iter.rs`), ahead of the language, the same way
Phase 6 built derived binds ahead of a producer: a level's rows come from a list of `Source`s,
so zero is the empty relation, one is a scan and many is a disjunction; a cursor entry carries
the alternative that produced it; and [I4](docs/invariants.md#i4) is re-proved over generated
multi-source plans with a census asserting a cut is taken while a later alternative is live.
Nothing in focus lowers one yet — `nyi/disjunction` is still reported — so what is left of 6b-a
is `flatten` and `ty.rs`, plus `never` decided alongside them.

**Tasks (coarse — decompose at pickup, per the rule at the top of this file):**
- **6b-a. Disjunction.** ~~The per-branch discriminant on `Cursor`~~ ✅. Left: flatten lowers a
  `FlatDisjunction` to a multi-source level, `ty.rs` gives `|` a type, `never` decided alongside
  it. No DNF expansion across conjuncts. The classification the note asks for — a disjunction
  whose branches only *filter* becomes a test rather than a level, and single-generator branches
  normalise to `Source::Seek` — is flatten's, and is what keeps the common case off the token.
- **6b-b. Range-restriction safety across branches.** Every branch must bind the same variable
  set, or the head reads a register the taken branch never wrote. Flatten's safety check is
  over the *chosen order* today; it now also has to be over *branches*, and the failure must be
  a diagnostic, not a run-time `UseBeforeBind`.
- **6b-c. The I4 battery over disjunctive plans**, and the **census** extended. Phase 4's
  lesson stands: a plan shape the generator never draws is a shape the resume battery says
  nothing about, so `the_generator_reaches_every_plan_shape` must assert it reaches a
  disjunction and a mid-branch cut — and the draw must be deliberate, not left to chance.
- **6b-d. Negation**, per the decision above.
- **6b-e. Subquery**, per the decision above.
- **6b-f. Corpus reclassification.** The five entries at `corpus.rs:224`–`246` move from
  `Diagnosed(code)` to `Supported(rows)` — which is the real gate, since a `Supported` entry
  cannot be added without saying what it returns (Phase 5). Retire each code from `Code::ALL`
  as it goes; `every_code_is_in_all` is the reminder.

**Acceptance:**
- [ ] Disjunction compiles to a `FlatDisjunction` and runs — with a test that a query whose DNF
      expansion would be exponential produces a plan **linear** in the branches.
- [ ] [I4](docs/invariants.md#i4) holds over plans containing a disjunction, at every scheduled
      cut point, on `MemStore` **and** fjall, with the census asserting the battery reaches a
      disjunctive plan and a cut taken mid-branch.
- [ ] A branch binding a different variable set is rejected with a clear diagnostic naming the
      variable — never a run-time error.
- [ ] `never`, negation and subqueries each either run, or draw a **narrowed** diagnostic
      naming what is still missing — never a parse error and never a panic.
- [ ] Negation's placement is *forced*, not merely likely: `!(A X); B X` answers exactly as
      `B X; !(A X)` does, so no negation is ever evaluated with an unbound variable acting as a
      wildcard — the immovability question above, closed by a test rather than a comment.
- [ ] Every reclassified corpus entry returns its recorded rows against a real `FjallDb`, and no
      retired `nyi/` code is left in `Code::ALL`.
- [ ] All prior engine guards green — the cursor format changed, so this is the claim that
      matters.

---

## Phase 7 — Transport codec + fact writing (ingestion)

**Goal.** Write facts programmatically and from files so the DB isn't hardcoded — a
`Db`/ingestion path that encodes and stores facts, with efficient *parallel* ingestion.

**Depends on:** Phase 1 (the store + atomic `put_fact` + FactId allocator).

**Design of record:** the parallel decode→sort→k-way-merge→bulk-`ingest()` pipeline, the
fact-file format + sync markers, and the wire COPY path are specified in
[operations §5 & §8](docs/aperture-cli-design.md) (`ops-I5` one-write-funnel, `ops-I4`
reproducibility). The storage-vs-transport codec split is
[chapter 3](docs/03-storage-model.md#storage-codec-vs-transport-codec). **Read those; don't
restate them.**

**What already exists, and what this phase must *not* do to it.** `focus::fact` +
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
  embedded schema — against the still-hardcoded schema until Phase 8).
- **Note:** the [`FactRef` marker](docs/open-decisions.md) is already resolved (own marker
  `0x51`), so writing fact-typed fields is unblocked — no pre-work here.

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

**Tasks:** `Db` + per-predicate partition handles; the fact-file format + sync-marker chunk
splitter; the parallel decode→encode→sort→k-way-merge→bulk-`ingest()` pipeline with the
dedup/reject rule; the framed wire protocol (CopyData-style fact blocks). *Done per task:*
ingest-then-query returns the ingested facts.

**Acceptance:**
- [ ] Facts are writable programmatically and from files in parallel, and queried back.
- [ ] Encoder/decoder round-trip property green (tier-1).
- [ ] Ingest is order-independent: shuffling input chunks yields the same DB *or* the same deterministic rejection (tier-2 metamorphic).
- [ ] Same-key-different-value is deterministically rejected regardless of chunking/worker interleaving.

---

## Phase 8 — Schema parsing (new grammar)

**Goal.** Parse schemas so predicate/type definitions aren't hardcoded — a separate schema
DSL feeding the same type model the query compiler uses. **Union types land here**, not in
[Phase 6b](#phase-6b--the-deferred-query-surface), for three reasons that all point the same
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
  the fault — and as the deferred `Access::Fetch` hazard.

**Tasks (coarse — decompose at pickup):** a `derivation` on the schema's `Predicate` (there is
none today, and `focus`'s grammar has no `predicate` keyword — both are Phase 8's to add); the
derive phase in the lifecycle, reading a sealed snapshot and writing through the one write funnel
(`ops-I5`); `DerivedAndStored` vs derive-on-demand as a schema-level distinction; re-derivation
as a tree drop; derived-on-derived via sealed rounds — for which the shape to copy exists: a
per-predicate completion list in the sidecar (Glean's `metaCompletePredicates`,
`glean/if/internal.thrift:74-80`, appended at `glean/db/Glean/Query/Derive.hs:242-251`) plus a
topological sort of the derivation graph with concurrency inside each stratum
(`glean/tools/gleancli/GleanCLI/Derive.hs:86-132`), which computes the round boundaries from the
schema instead of asking the operator to declare them.

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

## Phase 9 — Operations / production-ready

**Goal.** The hardening pass: telemetry, cohesive error handling, the full database/lifecycle
+ connection layer, and the workspace restructure.

**Depends on:** Phases 7–8 (a writable, schema-validated, queryable DB).

**Design of record:** [`docs/aperture-cli-design.md`](docs/aperture-cli-design.md) **in
full** — CLI tree (§4), per-command requirements (§5), operational invariants
`ops-I1`–`ops-I10` (§1), wire protocol (§6), on-disk layout (§9), workspace structure (§10). Read
it knowing that **none of it is implemented**: where it weighs a decision against Glean it is
weighing Glean's shipped code against this design, so Glean's costs there are measured and ours are
predicted.

**Invariants in scope:** *makes enforceable & tested:* `ops-I1`–`ops-I10`. *upholds under the
real connection layer:* [I8](docs/invariants.md#i8) (drop-at-suspend, already guarded at
Phase 1) now exercised through portals.

**Scope (coarse — decompose when reached):**
- **DB / lifecycle** (`ops-I2`/`ops-I3`/`ops-I4`): `Writable → Complete` (+ `Broken`);
  `create` embeds canonical schema + fingerprint (I13); ingest refused on Complete; `finish`
  seals (flush + `SyncAll` → content-hash identity → atomic sidecar flip as the last durable
  act); filesystem is the catalog (`ops-I7`); the sealed-snapshot machinery `ops-I8` needs, shared with [Phase 8b](#phase-8b--stored-derivation).
- **Connection layer / wire protocol** (if not landed in Phase 7): PSQL-inspired framed
  binary protocol with stream multiplexing, chunked `DataRow`s with a fair per-connection
  writer, per-stream `Cancel`, the bounded-channel sync↔async bridge with byte `Cursor`
  portals ([chapter 5](docs/05-resume.md)), default-closed bind (`ops-I10`). The remote-first
  `shell` (re-pointing the Phase 5 REPL) lands here.
- **The shell holds the cursor — first, and by a distance** ([operations
  §5](docs/aperture-cli-design.md)). Phase 5's REPL discards the resume token
  (`Iteratee::Suspended(rows, _)`, `src/main.rs`), so [I4](docs/invariants.md#i4) and
  [I8](docs/invariants.md#i8) — a bytes-only cursor and an entire resume battery, the most
  heavily tested machinery in this project — have **no interactive exerciser at all**. A wire
  client *can* hold a `Cursor`; that is the whole point of a bytes-only continuation, and it is
  the one item here that pressure-tests what the project spent the most effort on. Then, in
  order: a profile view (facts searched per predicate, with a full-scan flag — the *outcome* to
  `:plan`'s *intent*), `finish --allow-zero-facts`, a prefix-matching describe, and a readiness
  signal for `serve`. Each is specified in operations §5.
- **Telemetry:** query profiling (facts-searched-per-predicate — the counter already exists for
  cancellation and is simply not surfaced), metrics, tracing spans.
- **Cohesive errors:** one taxonomy end-to-end; no panics on data paths.
- **Snapshot lifecycle & migration:** backup/restore; migration story for the frozen marker
  (I3) & discriminant (I10) tables.
- **CLI + config:** the §4 command tree; config hierarchy CLI > env > file > defaults (clap +
  figment); XDG paths.

**Acceptance:**
- [ ] Operable as a real service: observable, diagnosable, with the defined lifecycle and connection layer.
- [ ] `ops-I1`–`ops-I10` enforced and tested (end-to-end `assert_cmd`/`trycmd`).
- [ ] The resume/streaming machinery runs against fjall through the real connection layer under the same batteries.
- [ ] The workspace restructure is complete (see cross-cutting note) with all batteries green.

---

## Cross-cutting — workspace extraction

The design's target layout ([operations §10](docs/aperture-cli-design.md)) is a Cargo
**workspace** (`aperture-schema` / `-encoding` / `-store` / `-engine` / `-ingest` / `-wire` /
`-client` / `-server` / `-cli`). Today it's a single `aperture` crate with the `focus`
module. The load-bearing seam is **already honored** — the executor consumes a
`(store handle, snapshot)` and assumes no connection — so the split is a *mechanical*
extraction, not a redesign. Do it incrementally as the operational layer needs the
boundaries: `-store`/`-encoding`/`-engine` fall out naturally at Phase 7 (ingestion needs a
clean store/encoding boundary), `-wire`/`-client`/`-server`/`-cli` at Phase 9. Each extraction
step's "green test" is: everything still compiles and all invariant batteries pass. Don't do
a big-bang restructure ahead of need.

---

## Deferred features (additive — must not reshape the machine)

Not on the critical path; each is additive: cross-fact navigation (`Access::Fetch` — reading
*through* a reference, the fourth piece in the table below); order comparisons (`ResidualOp`
arms); `pattern = pattern` full unification (easy half in Phase 4, reject the three hard
cases); `evolves`; cross-DB query. Detail and kept seams:
[`CLAUDE.md` scope](CLAUDE.md#scope-phases--open-decisions),
[open decisions](docs/open-decisions.md), [operations §11](docs/aperture-cli-design.md). The
two *non-additive* constructs — derived facts (Phase 6) and the now-resolved `FactRef` marker
— are handled as deliberate changes above.

**Five items left this list and took phases**, because "additive" was a claim about the
*machine* and never one about size: disjunction, `never`, negation and subqueries are
[Phase 6b](#phase-6b--the-deferred-query-surface), and unions-as-data is
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

**Still open: reading *through* a reference** — `P.name` or `P.value` where `P` came out of a
field. `nyi/fact-field`, and the fourth and largest piece: the already-listed `Access::Fetch`,
a point read per row and a new slot kind. Everything in the table above is a compare against an
id already in a register, which is why none of it needed one.

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
