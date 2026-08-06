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
  splices and residuals over the chosen order; `reorder` is a verified identity taking a
  graph over *variables* (not edges between statements — which statement captures a shared
  variable depends on the order, so edges would forbid correct orders). The headline gate is
  tier-3: a generated `(query, store)` pair run against a nested-loop model, in **every**
  permutation of the body. What flatten defers has a code and a corpus entry each
  ([chapter 7](docs/07-compilation.md#what-flatten-defers-and-why)). `src/lens/hoist.rs`
  survives as the reference for the one piece deferred with a seam — hoisting a nested
  generator — along with `query.rs` (the boxed AST, which nothing needs yet) and the three
  files those depend on; the other seven `lens` files are deleted.
- **The compilation driver** (`focus::compile`, Phase 3 done) — one `Compilation` carrying
  the source, schema, interner, diagnostics sink and the trees the phases produce. A phase
  reports by pushing into the sink and cannot return diagnostics; codes are a `Code` enum
  rather than strings; rendering sorts into source order while the sink keeps arrival order.
- **A focus shell** (`src/main.rs`) — reads a query, highlights it from the compiler's own
  lexer, and reports what the front end makes of it against a real `FjallDb` seeded at
  startup; `:facts` runs a hand-built plan through the executor. It compiles only through the
  driver, and stops at a type: calling `plan()` at the prompt is Phase 5's task. Phase 5
  scaffold, landed early — see that phase.
- **Store** (`store.rs`) — the fjall store is complete and guarded (Phase 1 done): a pair of
  keyspaces per predicate (`keys.<id>`, `entities.<id>`), `scan`/`point`, and an atomic
  `put_fact` over a snowflake [`FactId`](docs/03-storage-model.md#factid-allocation-i11) with
  a per-predicate allocator recovered from the data. Held to `MemStore` as a differential
  oracle. [I8](docs/invariants.md#i8), [I11](docs/invariants.md#i11),
  [I12](docs/invariants.md#i12) green — the I12 crash case aborts a child process mid-write,
  and the I8 guard cross-checks a drop probe against fjall's own open-snapshot count.
- **Unbuilt:** ingestion, schema parsing, the wire protocol, and the operational layer.
  `schema.rs` holds Phase 8's guards, written up front and `#[ignore]`d — the only pending
  entries left in the coverage ledger.

Module map: [chapter 1](docs/01-concepts.md). Nothing here contradicts the design docs.

---

## Dependency graph

```
0  guard matrix & harness ─┬─▶ 1  fjall store ✅ (I8, I11, I12 green; resume battery re-run on fjall)
                           │
                           └─▶ 2  grammar ✅ ─▶ 3  driver ✅ ─▶ 4  flatten/reorder ✅ ─┬─▶ 5  REPL  (→ remote-only later)
                                                                                    ├─▶ 6  derived facts  (deliberate machine change; own resume battery)
                                                                                    └─▶ 7  ingestion ─▶ 8  schema ─▶ 9  operations

Cross-edges:  6 also depends on the resume battery (0) + fjall (1).   7 depends on 1 (store + atomic put_fact).
              Derived-*and-stored* persistence (part of 6) integrates operationally with 7 + 9 (ops-I8 phased derivation).
Gates:        Codec I1–I3 green.  Executor I4–I7, I9 green on MemStore.  I8/I11/I12 at Phase 1.  I10/I13 at Phase 8.
              FactRef marker — resolved (own marker 0x51, already in the codec); no longer gates ingestion.
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
still small and the resume battery is fresh. Then make it writable (ingestion, 7), remove
the last hardcoded piece (schema, 8), and harden (operations, 9).

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

**Depends on:** nothing engine-side (parallel with Phase 1). Uses `src/lens/` as the
reference to **re-implement into `focus`** (against the `Plan` IR), then delete file-by-file.

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
generator list, run sargeability to build seeks/residuals, and pass through a reorder step
that is **initially identity** — structured so the real algorithm drops in without reshaping.

**Depends on:** Phase 3 (the driver + context).

**Design of record:** [chapter 7](docs/07-compilation.md) covers all the settled design —
flatten, disjunction-stays-a-node (never DNF across conjuncts), union-select →
`DiscriminantEq` residual, sargeability's order-dependence, the safety-vs-ordering split (why
identity reorder is *correct*, not a stub), and the future Kahn + antichain + selectivity
reorder (whose interface is built here). **Read it before this phase.**

**Invariants in scope:**
- *makes green:* the end-to-end property **"flattened plan run == expected rows"** (tier-3,
  schema-first) — this exercises [I4](docs/invariants.md#i4)–[I9](docs/invariants.md#i9) via
  the produced plans.
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
  `nyi/nested-generator`; `src/lens/hoist.rs` stays as its reference.

**Tasks:**
- **4a.** ✅ Flatten the implemented subset (scans, joins, scalar/record heads, nested captures)
  to `Plan`; range-restriction safety over the *chosen* order, so it also checks whatever
  `reorder` returned. text→plan→run for the corpus and for hand-written worked examples.
- **4b.** ✅ Sargeability over the chosen order (seek · splice · residual · capture), with the
  decision table in [chapter 7](docs/07-compilation.md#how-sargeability-actually-decides-phase-4-as-built).
  A string prefix seeks in the leading field and filters elsewhere; a fully-input key becomes a
  point match.
- **4c.** ✅ Reorder as a verified identity, taking a graph over **variables** rather than edges
  between statements — because which statement captures a shared variable depends on the order,
  so edges would forbid correct orders. `Deps::respects` / `Deps::antichains` are what the real
  algorithm sorts within; the `// TODO: Kahn + antichain + selectivity` seam is in place.
- **4d.** ✅ Intra-row repeats decided and implemented: rejected, tested both ways (the repeat,
  and the repeated *read* that is supported).

**Acceptance:**
- [x] `plan(q)` produces a runnable `Plan` for the corpus, safety-checked (non-range-restricted
      queries rejected with a clear error). The corpus gate now runs the whole driver, so
      `Supported` means *produces a plan*, and every construct flatten defers has an entry
      naming it.
- [x] Reorder is a verified identity with the real (graph-taking) interface in place.
- [x] "flattened plan run == expected rows" holds over generated `(query, store)` pairs
      (tier-3), against a nested-loop model — and holds in **every permutation** of the body,
      which is the reorderability claim made executable.
- [x] Intra-row repeats are rejected — tested, alongside the repeated read that is not.

**What the phase discovered:** the codebase had **two stored-key layouts** — flat (the codec
chapter, `plan::proptest`, the offset cache) and record-wrapped (the demo shell's seeding) —
and both "worked", because the executor never learns which convention wrote a row. Only a
*plan* has to choose, and choosing wrong reads the wrong bytes with no error. That is the
shape of bug this project's testing discipline exists for, and it was invisible until two
halves of the system had to agree.

---

## Phase 5 — REPL: experiment by executing

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

**Tasks:** REPL loop + line editing + diagnostic rendering (reuse `codespan-reporting`); seed
a store with fixtures; run compiled plans via `enumerate` and print projected `Value`s;
`:commands` (show flattened plan, show diagnostics). *Done per task:* an integration test
drives a query string through the REPL path and asserts the printed rows.

**Part of this already exists, from Phase 2** (`src/main.rs`): the loop, line editing, live
highlighting from the compiler's own lexer, codespan diagnostic rendering, a real `FjallDb`
seeded at startup, and `:schema` / `:facts` — the latter driving a hand-built plan through
`enumerate` and printing projected `Value`s, with fact references resolved to the facts they
name. What is missing is the compile step in the middle: it stops at a type. Flatten exists as
of Phase 4, so Phase 5 owes the `plan(query)` call, running the plan, and the `:commands` that
show one — not the shell around it. It earned its place early by finding a double-reported
lexer diagnostic that only a live front end makes obvious; treat it as the scaffold this phase
already says it is.

**One thing to decide at pickup, found by Phase 4:** the demo schema is built around fact
references (`demo.Knows {from : demo.Person}`), and both `:help` examples are nested
generators over fact-typed fields — exactly what flatten defers as `nyi/nested-generator` and
`nyi/fact-field`. They typecheck, so the shell's advice is honest today, but it will not
*run*. Either the examples change to the runnable subset (a join through a scalar field), or
Phase 5 pulls in the fact-id splice and `Access::Fetch` — which is a `Plan` IR change, and so
a decision rather than a detail.

**Acceptance:**
- [ ] Typing a focus query returns rows (or a well-rendered diagnostic) against a fixture store, end-to-end, through the real compiler and executor.
- [ ] Diagnostics from typecheck/flatten render nicely (source spans).

---

## Phase 6 — Derived facts (a deliberate machine change)

**Goal.** Support derived predicates — `predicate P : … = KEY where <query>` — that compute
facts from a query, plus `DerivedAndStored`. This is one of the **two sanctioned machine
changes** (it promotes `Register` to a `Slot` sum type and touches resume), done here — while
the machine is still small and the resume battery is fresh — rather than after ingestion and
schema pile onto the current register shape.

**Depends on:** Phase 4 (flatten + the graph-taking reorder interface) and the resume battery
(Phases 0/1). *Derived-and-stored persistence* additionally needs Phase 7 (a store to write
to) and integrates operationally in Phase 9 (ops-I8 phased derivation).

**Design of record:** [chapter 7 — "Derived facts"](docs/07-compilation.md#derived-facts)
(the `Slot` sum type; derived binds as not-a-loop-level, recomputed on resume; the hard
topo-ordering they impose; the Glean mechanism) and [ops-I8](docs/invariants.md#ops-i8)
(phased derivation, sealed snapshots).

**Invariants in scope:**
- *adds & makes green:* a **new invariant** — *derived binds are pure functions of the fact
  bindings* — with its own tier-3 resume battery. **Add it to the
  [registry](docs/invariants.md) and [chapter 7](docs/07-compilation.md) when this lands.**
- *upholds:* [I4](docs/invariants.md#i4), [I5](docs/invariants.md#i5) (the `Register→Slot`
  change must not regress the resume battery or the row-slot model).

**Tasks:** promote `Register` → `Slot` (fact | computed-value); derived-bind plan step +
flatten lowering (the topo-ordering case); recompute value-slots on resume; the new purity
type/guard; `DerivedAndStored` gating (persistence deferred to integrate with Phase 7/9).

**Acceptance:**
- [ ] Derived predicates compile (flatten → plan with derived-bind steps) and execute (recomputed correctly) against a fixture/fjall store.
- [ ] Resume is correct under the interruption-schedule generator — recompute-on-restore property green (tier-3), on top of Phase 0c's battery.
- [ ] The `Register→Slot` change leaves all prior engine guards green.
- [ ] The new purity invariant + guard are added to the registry and chapter 7.

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

**Invariants in scope:**
- *strengthens (at scale):* [I11](docs/invariants.md#i11), [I12](docs/invariants.md#i12).
- *upholds (relies on):* [ops-I4](docs/invariants.md#ops-i4) (reproducibility ⇒ conflict
  handling is order-independent), [I13](docs/invariants.md#i13) (validate ingest against the
  embedded schema — against the still-hardcoded schema until Phase 8).
- **Note:** the [`FactRef` marker](docs/open-decisions.md) is already resolved (own marker
  `0x51`), so writing fact-typed fields is unblocked — no pre-work here.

**Phase-specific rules:** the encoder must agree **byte-for-byte** with the read-path decoder
(the round-trip property is the guard); dedup byte-identical facts silently and **reject
same-key-different-value** deterministically (`--on-conflict=reject` default; any override
commutative, never LWW).

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
DSL feeding the same type model the query compiler uses.

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

**Acceptance:**
- [ ] Parse a schema file and run a query against it end-to-end (test).
- [ ] Fingerprint order-independence green (tier-2: two source orderings → identical fingerprint).
- [ ] Ingest rejects a fact file whose schema fingerprint isn't subset-compatible (I13).
- [ ] Invariant-violating schema edits (renumbered discriminant, reordered marker) are rejected at load (I10/I3, tested).

---

## Phase 9 — Operations / production-ready

**Goal.** The hardening pass: telemetry, cohesive error handling, the full database/lifecycle
+ connection layer, and the workspace restructure.

**Depends on:** Phases 7–8 (a writable, schema-validated, queryable DB).

**Design of record:** [`docs/aperture-cli-design.md`](docs/aperture-cli-design.md) **in
full** — CLI tree (§4), per-command requirements (§5), operational invariants
`ops-I1`–`ops-I10` (§1), wire protocol (§6), on-disk layout (§9), workspace structure (§10).

**Invariants in scope:** *makes enforceable & tested:* `ops-I1`–`ops-I10`. *upholds under the
real connection layer:* [I8](docs/invariants.md#i8) (drop-at-suspend, already guarded at
Phase 1) now exercised through portals.

**Scope (coarse — decompose when reached):**
- **DB / lifecycle** (`ops-I2`/`ops-I3`/`ops-I4`): `Writable → Complete` (+ `Broken`);
  `create` embeds canonical schema + fingerprint (I13); ingest refused on Complete; `finish`
  seals (flush + `SyncAll` → content-hash identity → atomic sidecar flip as the last durable
  act); filesystem is the catalog (`ops-I7`); phased derivation via sealed snapshots (`ops-I8`).
- **Connection layer / wire protocol** (if not landed in Phase 7): PSQL-inspired framed
  binary protocol with stream multiplexing, chunked `DataRow`s with a fair per-connection
  writer, per-stream `Cancel`, the bounded-channel sync↔async bridge with byte `Cursor`
  portals ([chapter 5](docs/05-resume.md)), default-closed bind (`ops-I10`). The remote-first
  `shell` (re-pointing the Phase 5 REPL) lands here.
- **Telemetry:** query profiling (facts-searched-per-predicate), metrics, tracing spans.
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

Not on the critical path; each is additive: cross-fact navigation (`Access::Fetch`); order
comparisons (`ResidualOp` arms); unions-as-data then the `FlatDisjunction` union-of-streams
operator (per-branch discriminant on `Cursor`); negation/subqueries; `pattern = pattern` full
unification (easy half in Phase 4, reject the three hard cases); `evolves`; cross-DB query.
Detail and kept seams: [`CLAUDE.md` scope](CLAUDE.md#scope-phases--open-decisions),
[open decisions](docs/open-decisions.md), [operations §11](docs/aperture-cli-design.md). The
two *non-additive* constructs — derived facts (Phase 6) and the now-resolved `FactRef` marker
— are handled as deliberate changes above.

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
- **`src/lens/`** — the disconnected first-attempt front end; reference for re-implementing
  parse/typecheck/lower into `focus` (Phases 2–4), then delete.
