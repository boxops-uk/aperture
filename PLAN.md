# Aperture — build plan

This is the living phase tree for getting Aperture working end-to-end and then to
production. It complements `CLAUDE.md`: `CLAUDE.md` holds the invariants and working
rules; this holds the _sequence_. Each task's definition of done is a **green test**
(prefer a property — see `CLAUDE.md` §8). Every task must respect the §3 invariants.

**How to use this tree.** The phase order and dependencies below are owned by the
maintainer. When picking up a phase, the agent decomposes its next unstarted node into
task-sized leaves _at pickup_ (early decomposition is always wrong), each leaf ending in
a green test, ordered by dependency and de-risking. Keep diffs small and reviewable.
Phases 1–2 are decomposed to task granularity here as a template; later phases are
intentionally coarser and get decomposed when reached.

The spine already exists in prototype: the codec is heavily property-tested; the
executor + resume + projection are implemented but **not yet** covered by the §8 property
batteries — Phase 0 back-fills that. This plan is about turning that spine into a working
end-to-end system with a real front end and storage, then hardening it.

---

## Ordering principle

Front-load the pieces where a subtle bug is catastrophic and hard to detect later
(codec ordering, resume) — codec already has heavy test batteries; resume gets them in
Phase 0. Then build the
_front end_ (grammar → compiler → flatten/reorder) that produces the plan IR the
executor already consumes, so the two halves meet in the middle. Then make it runnable
(REPL), then make facts writable (ingestion), then remove the last hardcoded piece
(schema parsing), then harden (operations). Derived facts weave in after flatten exists.

The IR contract is the fixed point everything aims at: **the query-plan IR (`Plan`) is
the lowering target.** The grammar/compiler/flatten phases exist to produce it; the
executor already consumes it. Keep that contract stable and both halves can progress
independently.

---

## Phase 0 — Back-fill: align the spine with the test strategy

**Goal.** Make the existing spine (codec, executor, resume, projection) actually meet
`CLAUDE.md` §8 — because later phases' acceptance gates depend on that test infrastructure
existing. Today only the codec is property-tested; the executor and resume have **no**
tests, and the codec's generators are inline rather than the shared support-module shape
§8 prescribes.

**Why first.** Resume is the second catastrophic-if-wrong subsystem (I4) and is currently
unverified. Phase 3's own acceptance — "flattened plan run == expected rows over generated
`(query, store)` pairs" — _requires_ the schema-first `(plan, store)` generator and the
model-based oracle this phase builds. Build the harness before building on top of it.

**Tasks (decompose at pickup; each ends green):**

- **0a.** Shared test machinery: the in-memory `FactStore` (`focus::mem_store`) and a
  small schema/fixture builder, in support modules tests import — not redefined inline
  (promote to `cfg(any(test, feature = "proptest"))` when integration tests/benches need
  them).
- **0b.** Executor happy-path tests over hand-built plans (1-, 2-, 3-level joins, seeks,
  key-field residuals, record/scalar heads); the model is "run to completion, collect
  rows."
- **0c.** Schema-first `(plan, store)` generator (valid-by-construction, §8) + the
  interruption-schedule generator; the **resume == uninterrupted run** property at _every_
  cut point for 1-/2-/3-level plans (I4). This is the phase's acceptance gate.
- **0d.** Restructure the codec's inline generators into the §8 support-module shape
  (named `arb_*` strategies) so Phase 1+ compose from them.
- **0e.** Fold discovered latent fixes in with regression tests (residual walks the
  stripped key — done; audit `Project::Value`, which today reads key bytes and stays
  unfinished until Phase 5).

**Acceptance:** the executor/resume property batteries of §8 exist and are green; the
codec strategies are importable; the spine's "all tested" claim is true.

---

## Phase 1 — Grammar: permissive-early, catch-in-compilation

**Goal.** Update the existing grammar so it parses the _full_ intended feature surface
now, deferring "not-yet-implemented" to later compiler phases via clear diagnostics —
so no grammar reshape is needed as features land. Reference the existing
parse → façade → typecheck implementation; align it to the backend, since the plan IR
is the lowering target.

**Why first.** The grammar is the widest one-way-ish door: reshaping it after downstream
code depends on its tree shape is expensive. Getting it permissive-and-stable now lets
every later phase add _meaning_ to constructs that already _parse_.

**Design constraints (from prior work).**

- Permissive grammar, narrow later: parse union select (`.alt?`), disjunction (`|`),
  negation (`!`), `pattern = pattern`, nested records, etc.; _reject_ the unimplemented
  ones at typecheck/flatten with good errors, not at the grammar.
- Known-resolved conflicts to preserve: qualified predicate names lexed as one `QId`
  token (leading-lowercase disambiguates from `UId` access); dot binds tighter than
  fact application (`test.Foo X.name` == `test.Foo (X.name)`), so `fact_pattern` is its
  own `pattern` alternative; `..` string-prefix vs `.` access by maximal munch; `Nat`
  underscore rule (`0|[1-9][0-9]*(_[0-9]+)*` or lex-permissive-then-validate).
- Three tree layers stay: CST façade (untyped) → `SyntaxTree` store (typed, `NodeId`,
  side-tables) → boxed ergonomic AST. Record fields sorted `Box<[(Symbol,T)]>` (not
  `HashMap`) everywhere — reconcile the boxed AST to match.

**Tasks (decompose further at pickup; each ends green):**

- **1a.** Audit the existing grammar/lexer against the target feature list; produce a
  table of "parses / rejected-where / not-yet-representable." _Done:_ the table + a
  failing test per gap.
- **1b.** Lexer: land `QId`, the `Nat`-underscore rule, `..`/`.` munch, `never`.
  _Done:_ lexer unit tests (incl. `E.from` ≠ qualname, `a.B.c` boundaries, `1__0`
  rejected) pass.
- **1c.** Grammar: dot-tighter-than-application (`fact_pattern` lifted); postfix
  `.field` access chain; permissive `pattern = pattern`; nested record / union-select
  (`?`) / disjunction (`|`) _surface_ syntax. _Done:_ parse tests over a corpus incl.
  the deferred features; LL(1) build clean.
- **1d.** Façade → `SyntaxTree` store lowering aligned; boxed AST reconciled
  (sorted-slice record fields). _Done:_ round-trip/structure tests over the corpus.
- **1e.** Typecheck updated to the backend's type model (`PredicateTy` incl.
  `Fact`/`Record`; union alternatives with stable discriminants), emitting clear
  "not yet implemented" diagnostics for deferred constructs. _Done:_ typecheck accepts
  the P0 subset, rejects the rest with the intended messages (tested).

**Acceptance for the phase:** the target-feature corpus parses; the P0 subset
typechecks; every deferred construct produces a specific, tested diagnostic rather than
a parse error or a panic.

---

## Phase 2 — Compilation driver: shared context, not hand-wired passes

**Goal.** A single compilation abstraction the phases run through — carrying the shared
plumbing (a **pooled diagnostic/error collection**, the **interners**, the schema, the
`SyntaxTree` store + side-tables) — rather than each pass threading its own state. Not a
demand-driven/incremental query engine; just the common context and the driver that
sequences typecheck → flatten → plan.

**Why here.** Phase 1 gives typed trees; this gives the plumbing later phases plug into,
so flatten/reorder (Phase 3) are passes over a shared context with unified diagnostics and
interning, not bolted-on functions each reinventing error handling.

**Design constraints.**

- One **diagnostics sink** for the whole pipeline (parse/typecheck/flatten), drained once
  and rendered via `codespan-reporting`; phases accumulate into it and keep going
  (permissive-grammar-narrow-later needs multi-error reporting), not fail-fast.
- Shared **interning**: the two-tier `SchemaInterner` (frozen, shared) + per-compilation
  local `Rodeo` (`CLAUDE.md` §4) live on the context.
- The `SyntaxTree` store's stable `NodeId` + side-tables (`Vec<Ty>`-by-`NodeId`) is how
  typecheck annotates without mutating the tree — the context owns the store and the side
  tables.
- Output contract unchanged: the driver's terminal product is `plan(query) -> Plan` (the
  executor's input); everything upstream serves that.
- **Explicitly not now:** memoization / incremental recomputation / a `salsa`-style query
  engine. The context is a plain threaded struct; incrementality is a later concern and
  must not be designed-in speculatively.

**Tasks (decompose at pickup):**

- **2a.** Stand up the compilation context (diagnostics sink + interners + store/side
  tables) and thread it through the existing parse → typecheck path. _Done:_ typecheck
  runs through the context and reports multiple diagnostics in one pass (tested).
- **2b.** The driver that sequences the phases to `plan(query)` (a stub calling into
  Phase 3). _Done:_ end-to-end "text → typed → (stub) plan" for the P0 subset, all
  diagnostics surfaced through the one sink.

**Acceptance:** one context carries diagnostics + interning + the typed store through the
pipeline; asking for `plan(q)` runs typecheck → flatten in sequence over it; multi-error
diagnostics render through `codespan-reporting` (tested).

---

## Phase 3 — Flatten → reorder

**Goal.** Lower the typed query to the flat plan IR: flatten nested generators into an
ordered generator list, run sargeability to build seeks/residuals, and pass through a
reorder step. **Reorder is initially a no-op** (identity endomorphism) — but structured
so the planned algorithm drops in without reshaping.

**Design constraints (from prior work — these are settled and must be honored).**

- Flatten produces the ordered `[Generator]` + `head: Project` the executor consumes.
  Disjunction survives flattening as a **node** (`FlatDisjunction`), never DNF-expanded
  across sibling conjuncts; Glean's PLAN-B (distribute an `|` only _within_ a single
  seek's pattern, bounded) is the one place duplication is allowed. Union select
  (`.alt?`) lowers to a **match-against-bound-value** = `DiscriminantEq(n)` residual +
  payload bind, _not_ a new generator.
- **Correctness needs only a _safety_ check, not a topological sort, for generators:**
  every variable used in a seek/residual/head must be _captured_ in some generator's
  key pattern (capture-at-first-occurrence makes binding-before-use automatic in any
  linear order). Reject unsafe (non-range-restricted) queries at compile time. Ordering
  is a _performance_ choice (selectivity), which is why reorder can be identity in P0.
- **Topological sort becomes required only with derived binds** (`Z = f(X,Y)` from
  subqueries/primitives) — they consume vars, can't capture them, impose hard ordering
  edges, and a cycle is a compile error. Keep the reorder step's interface able to take
  a dependency DAG.
- Sargeability is _order-dependent_ (a captured field can't seek — it's being bound), so
  it runs over the chosen order; a captured field forces a fuller scan, a
  bound-from-earlier field becomes a splice.
- Reorder plan (note for when it's built, do **not** build now): **Kahn's-algorithm
  topological sort** over the dependency graph, layered by an **antichain** of
  independently-orderable statements, with a **selectivity heuristic** choosing within
  each antichain (lookups/point-matches before prefix-matches before scans, à la
  Glean's `Reorder`). Negations/conditionals move after their non-locals are bound.
- Watch-items flagged in prior work: intra-row repeated variables
  (`Edge{from=X,to=X}`) need a same-row `ResidualOp::EqField` (distinct from cross-slot
  `EqSlotField`) or explicit rejection — decide P0 scope. `FieldPath` (not flat
  `FieldIdx`) in plan types for nested-record access, depth-1 fast path only for now.

**Tasks (decompose at pickup):**

- **3a.** Flatten the P0 subset (scans, joins, scalar/record heads) to `Plan`; safety
  check (range-restriction) with clear rejection. _Done:_ text→plan for the P0 corpus,
  and the executor runs the produced plans to the expected rows (differential vs
  hand-built plans from prior tests).
- **3b.** Sargeability over source order (seek vs residual vs splice). _Done:_ produced
  plans use seeks where expected (tested against known-sargeable queries).
- **3c.** Reorder step as identity, taking the interface a real reorderer will use
  (dependency edges available, antichain structure representable). _Done:_ identity
  reorder is a no-op verified by plan-equality; the interface compiles with a
  `// TODO: Kahn + antichain + selectivity heuristic` seam.
- **3d.** Decide + implement intra-row repeats (`EqField`) or reject them. _Done:_
  either tested `EqField` semantics or a tested rejection diagnostic.

**Acceptance:** `plan(q)` produces a runnable `Plan` for the P0 corpus, safety-checked;
reorder is a verified identity with the real interface in place; the property
"flattened plan run == expected rows" holds over generated (query, store) pairs
(`CLAUDE.md` §8 tier-3, schema-first generation).

---

## Phase 4 — REPL: experiment by executing

**Goal.** A simple interactive REPL to _run_ queries end-to-end (not just typecheck).
Reference the existing `old_main` demo (logos lexer + rustyline + codespan-reporting for
diagnostics) — but this time compile _and execute_ against a store, printing results.

**Why here.** First moment the whole pipeline (parse → compile → plan → execute →
project) is exercised by a human. Invaluable for finding integration gaps the unit
tests miss.

**Design constraints.**

- Reuse the diagnostics stack (`codespan-reporting`) for typecheck/flatten rejections —
  the permissive-grammar-narrow-later errors should render nicely here.
- Execute against a store: for now the in-memory test `Store` seeded with fixtures
  (fjall comes in Phase 5). Print projected `Value`s.
- Drive the executor via `enumerate` to exhaustion (streaming/portal machinery not needed
  in the REPL yet, but don't foreclose it).

**Tasks (decompose at pickup):** REPL loop + line editing + diagnostic rendering;
seed an in-memory store with fixtures; run compiled plans and print rows; `:commands`
(e.g. show flattened plan, show diagnostics) mirroring Glean's debug affordances.
_Done per task:_ an integration test drives a query string through the REPL path and
asserts the printed rows.

**Acceptance:** typing a query returns rows (or a well-rendered diagnostic) against a
fixture store, end-to-end, through the real compiler and executor.

---

## Phase 5 — Transport codec + fact writing (ingestion)

**Goal.** Write facts programmatically so the DB isn't hardcoded — a `Db`/ingestion path
that encodes and stores facts, with an eye toward efficient _parallel_ ingestion.

**Design constraints (from prior work — partial; see the flag below).**

- A `Db` owns the keyspace + the two partition handles (`keys`, `entities`), `Arc`-shared.
- `put_fact(pred, key, value)`: tuple-encode the key **and** the value (one storage
  codec — see below), allocate a sequential `FactId`, write **both column families
  atomically** (a write batch), so a fact is never half-present.
- FactId allocation is a monotonic counter (an `AtomicU64` high-water mark) — this is
  the seam that must be gotten right for parallel/concurrent ingestion.
- **The encoder must agree byte-for-byte with the decoder** — the projector must read
  back exactly what ingestion wrote, for both key and value. This is a hard invariant; a
  drift here is silent corruption.
- **Storage codec vs transport codec (settled — `CLAUDE.md` §7).** Both keys and values
  are **tuple-encoded with the one storage codec** (order-preserving, self-delimiting) —
  values too, so queries can eventually _match on values_; `Project::Value` is then
  decode-not-copy-through. The **transport/wire codec is separate and applies only
  post-yield** (to rows leaving the executor): a framed binary format (PSQL-inspired),
  _not_ order-preserving, never touching stored bytes. It's FDB-_inspired_, not FDB — do
  not call the storage codec "FDB". Both stored and wire forms must be efficiently
  chunkable for file ingestion.

**The parallel-ingestion pipeline (recovered from the operations design work — this is
the design, not a placeholder).** The bulk path, used both embedded and server-side for
file ingest:

1. **Split input into chunks via a sync-marker scan** — do not serially parse from byte
   zero. The fact-file format carries sync markers so a chunk boundary can be found
   without parsing the whole prefix. (Human-authorable fact files use a one-fact-per-line
   format with `|` (space-pipe-space) field separators and bare JSON values —
   schema-guided self-delimiting parsing means the separator is only asserted at a known
   cursor position after a complete value, never scanned for inside one. `memmap2` +
   `rayon` for parallel chunk processing; `lexical` for fast numbers, `sonic-rs` for
   SIMD record JSON.)
2. **Workers in parallel:** wire/text-decode → storage-tuple-encode (order-preserving
   key) → **sort** within the worker.
3. **K-way merge across workers, per predicate.** At the merge frontier identical keys
   are colocated, so: **dedup byte-identical facts silently; reject the batch on
   same-key-different-value** (deterministic and order-independent — this is required by
   the resume/immutability invariant I4). `--on-conflict=reject` is the default; any
   override must be **commutative, never last-write-wins**.
4. **Feed the sorted, deduped, conflict-free ascending stream to fjall's bulk
   `ingest()`** — the unchecked-write fast path that needs no per-key reads _because the
   merge already established the invariants_ (sorted, unique, conflict-free).
   **One keyspace (partition) per predicate** ⇒ per-predicate ingests are independent
   trees and may overlap/parallelize freely. This is what makes concurrency "fearless":
   facts with different keys can't affect each other; only same-key-different-value
   collides, and that's rejected at the merge frontier.

- FactId allocation is a monotonic counter (`AtomicU64` high-water mark). Same-key
  dedup/conflict happens at the _merge frontier_ on the encoded key, before ingest.
- **The wire codec is the permanent exerciser:** the shell/REPL always speaks the wire
  protocol even locally, and writes are _just another stream_ on a connection (a
  deriver/tool interleaves read and write streams — no separate write sub-channel).
- Schema validation on every ingest path against the DB's **embedded** schema; a fact
  file's header fingerprint is checked for compatibility (subset containment) before
  ingest (the canonical-schema fingerprint + containment check itself lands in Phase 6 —
  until then ingestion validates against the still-hardcoded schema). Rendering to
  JSON/text is **client-side only** — the server emits binary.

**Tasks (decompose at pickup):** `Db` + per-predicate partition handles; the ingestion
encoder (tuple-encode key + value) with a **round-trip property** against
the read-path decoder; the fact-file format + sync-marker chunk splitter; the parallel
decode→encode→sort→k-way-merge→bulk-`ingest()` pipeline with the dedup/reject-on-conflict
rule; the framed wire protocol (CopyData-style fact blocks for writes). _Done per task:_
ingest-then-query returns the ingested facts; encoder/decoder round-trip property holds
(`CLAUDE.md` §8 tier-1); a **metamorphic property** that ingest is order-independent
(shuffling input chunks yields the same DB or the same deterministic rejection — tier-2).

**Acceptance:** facts can be written programmatically and from files in parallel, and
queried back; the round-trip and order-independence properties are green;
same-key-different-value is deterministically rejected regardless of chunking/worker
interleaving.

---

## Phase 6 — Schema parsing (new grammar)

**Goal.** Parse schemas so predicate/type definitions aren't hardcoded — a _separate_
grammar (schema DSL) feeding the same type model the query compiler uses.

**Design constraints.**

- New grammar, same discipline as Phase 1 (permissive-then-narrow; three tree layers if
  it earns them). Produces the `PredicateTy`/schema structures the compiler already
  consumes — the schema is the source of truth typecheck/flatten read.
- **Imports/resolution (recovered decision):** _no project file that enumerates sources._
  Resolve via a **`mod`-tree walk from a single entry file** (Go/Starlark-ish module
  resolution), not an explicit source manifest. `create` walks the `mod` tree from the
  schema root.
- **Schema identity is filesystem-independent (recovered):** two schemas are equal iff
  they're _compatible_, regardless of which file a predicate is declared in or in what
  order. Compute a **canonical form** (order-independent) and a **fingerprint** over it;
  compare/validate by fingerprint. **Embed the canonical schema + fingerprint in the DB
  at creation** — the schema travels with the data, the DB is self-describing, and the
  schema is fixed for the DB's lifetime (P0 has no in-place `evolves`).
- **Evolution/compatibility (recovered):** define compatible vs breaking changes —
  e.g. _adding a nullable field to a predicate is compatible_; compatibility is
  **subset containment** checked at ingest (a fact file's fingerprint must be compatible
  with the DB's embedded schema). Full in-place evolution is deferred; the fingerprint +
  containment check is the P0 mechanism.
- **Freeze the one-way-door invariants at schema-load** (`CLAUDE.md` §3): union
  alternative discriminants explicit/stable/append-only (I10); marker/type ordering
  frozen (I3); reject schema changes that would violate on-disk ordering.
- Predicate/field naming rules align with the query grammar (`QId` shape; field names
  lowercase; reserved words).

**Tasks (decompose at pickup):** schema lexer/parser; lower to the schema/type model;
validate the freeze-invariants (stable discriminants, append-only alternatives); wire
the query compiler to load schema from parsed input instead of hardcoded fixtures.
_Done per task:_ parse a schema file and run a query against it end-to-end (test);
invariant-violating schema edits are rejected (tested).

**Acceptance:** schemas are loaded from source; queries run against parsed schemas; the
stability invariants are enforced at load with tested rejections.

---

## Phase 7 — Operations / production-ready

**Goal.** The hardening pass: telemetry, cohesive error handling/reporting, and
database/lifecycle management.

**Scope (coarse — decompose when reached).**

- **Telemetry:** query profiling (facts-searched-per-predicate, à la Glean's `:debug`),
  metrics, tracing spans across the pipeline and the executor.
- **Cohesive errors:** one error taxonomy end-to-end (parse/typecheck/flatten/store/
  runtime), rendered consistently (codespan for source-level; structured for runtime);
  no panics on data paths (`CLAUDE.md` §4).
- **DB / lifecycle (recovered state machine):** a DB moves **Writable → Complete**.
  `create` (needs a schema; embeds canonical schema + fingerprint). `write`/ingest only
  on a Writable DB (refused on Complete). `finish` **seals** it: flush + sync everything
  → compute content identity `hash(canonical schema, base facts)` → record in a sidecar →
  atomically flip status to Complete as the final durable act; after finish, every
  write-mode open is refused forever. Crash-mid-finish leaves Writable and `finish` is
  re-runnable (idempotent-ish). DBs are **addressable by name and by fingerprint**;
  arbitrary out-of-band tool edits set an `externally_modified` flag so identity honestly
  becomes a random non-reproducible id. `query` opens read-only (embedded read-only
  requires Complete + lock-free); `shell` is remote-first (always speaks the wire, the
  permanent wire exerciser). CLI arg/config hierarchy: **CLI args > env > config file >
  defaults** (clap + a layered config crate like figment/config-rs; XDG paths via
  `directories`).
- **Snapshot lifecycle:** the immutable-snapshot-per-query invariant I8; drop-at-suspend
  to release; backup/restore; migration story for the frozen-on-disk marker &
  discriminant tables.
- **Wire protocol / connection layer** (if not landed with ingestion in Phase 5):
  PSQL-inspired framed binary protocol with **stream-level multiplexing**
  (`[stream_id:u32][len:u32][payload]`), read and write streams on one connection,
  chunked `DataRow`s with fair writer interleaving (never buffer a full result set),
  per-stream `Cancel` frames (Ctrl-C ≠ connection teardown), and the bounded-channel
  sync↔async bridge with byte `Cursor` portals.
- **fjall `Store` impl** behind the trait, with the resume battery re-run against it.
  **Pull this earlier — right after Phase 3, not here.** I8 (drop-at-suspend releases the
  read snapshot) is _untestable_ against the in-memory store (whose scan pins nothing), so
  resume/I8 correctness is only actually validated against a real snapshotting store.

**Acceptance:** operable as a real service — observable, diagnosable, with a defined
lifecycle and migration story; the resume/streaming machinery runs against the real
(fjall) store under the same property batteries.

---

## Cross-cutting — Derived facts

Woven in **after Phase 3 (flatten) exists**, since it's a flatten/compiler concern.

**Goal.** Support derived predicates — `predicate P : ... = KEY where <query>` — that
compute facts from a query rather than storing them raw, plus `DerivedAndStored`.

**Design constraints (from prior work / Glean source read).**

- A **derived bind** is a plan-step kind distinct from a generator: `Z = f(bound vars)`,
  evaluated where its inputs are live, materializing a computed value into a register.
  This requires promoting the register (`Register`, today a struct holding a fact row) to
  a **sum type** — a `Slot` enum with fact and value variants — so a slot can hold a
  non-fact binding. It is **not** a loop level — `enumerate` doesn't iterate it and the
  resume token doesn't store it; it is _recomputed on restore_ (so resume must recompute
  value-slots after rebinding the fact-slots).
- **Invariant to lock (add to `CLAUDE.md` when built):** derived binds must be **pure
  functions of the generator (fact) bindings** — that's what lets resume save only
  generator positions and recompute the rest. The type for derived binds must
  structurally forbid iteration/hidden state.
- Mechanism mirrors Glean: `DerivedFactGenerator`; `Derive when query`; the temp-pid /
  `captureKey` trick for capturing the derived key at codegen; `DerivedAndStored`
  gating. Derived binds impose the **hard topological ordering** (Phase 3 note) — this
  is _the_ case that makes topo-sort necessary; cycles are compile errors (or recursion,
  out of scope).
- Subquery flattening: a value-producing subquery becomes (its generators, hoisted) +
  (a derived bind for its head); a fact-producing subquery's head is just an alias to an
  existing `Fact` slot (no `Value` needed).

**Acceptance:** derived predicates compile (flatten → plan with derived-bind steps),
execute (recomputed correctly), and — critically — **resume correctly** (the
recompute-on-restore property holds under the interruption-schedule generator,
`CLAUDE.md` §8 tier-3).

---

## Deferred features (designed-for, additive — see `CLAUDE.md` §5)

Not on the critical path; each is additive to a machine that shouldn't reshape:
cross-fact navigation (`Access::Fetch` degenerate generator); order comparisons
(`ResidualOp` arms); unions-as-data then the `FlatDisjunction` union-of-streams operator
(with the per-branch discriminant added to `Cursor`); negation/`FlatConditional`;
`pattern = pattern` full unification (easy half in flatten, reject the three hard cases);
`FactRef` own marker; fetch memoization for repeated navigation.

---

## Related prior design work (source threads)

Much of the detail above was recovered from earlier design conversations; pull these up
if a phase needs more than this plan captures:

- **Operations / CLI / lifecycle / parallel ingestion / schema identity** — the richest
  operational source; produced an `aperture-cli-design.md`. Has the full Writable→Complete
  lifecycle, the parallel decode→sort→k-way-merge→bulk-`ingest()` pipeline, the
  `mod`-tree schema resolution, canonical-schema fingerprinting + subset-containment
  compatibility, and the framed multiplexed wire protocol. **The primary reference for
  Phases 5–7.**
- **Fact-file format** — human-authorable one-fact-per-line `|`-separated bare-JSON
  format, schema-guided self-delimiting parsing, `memmap2`/`rayon`/`lexical`/`sonic-rs`
  chunked parsing. (Reference for Phase 5's file ingest.)
- **VM / ISA design** — a fixed-width 64-bit instruction set with implicit register
  banks, from rewriting Glean's C++ query VM. Relevant _if_ Aperture ever moves from the
  native-Rust executor to a bytecode VM (currently a deliberate divergence — we implement
  the abstract machine directly; see `CLAUDE.md` §5).
- **Grammar / lexer / parser** — several threads: LL(1) conflict resolution (lelwel and
  otherwise), resilient LL parsing (`MarkOpened`/`open_before`, green trees, recovery
  sets), lexer performance, REPL syntax highlighting (`rustyline` `Highlighter`), and a
  bidirectional lexer/generator theory thread (relevant to `CLAUDE.md` §8's
  generator-first testing). References for Phases 1 and 4.
