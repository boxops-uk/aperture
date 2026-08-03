# Aperture

Aperture is an embedded, immutable **fact database** with a typed, Datalog-flavoured
query language (Angle-inspired). Facts are typed records identified by a `FactId`,
grouped by predicate, stored in an LSM (fjall) and queried by compiling queries to a
nested-loop plan executed by a suspendable pull-based VM.

It is **inspired by Glean, not a clone.** Where we diverge deliberately, it's noted below.

> This file is the persistent contract for working on Aperture. It is loaded every
> session. Keep it tight. Deep design rationale lives in `docs/` — link, don't inline.

---

## 0. How to work on this project (read first)

**Test-driven, property-first, verification mandatory.** Reasoning is not evidence.
Nearly every correctness bug this project has hit (codec off-by-ones, residual
short-circuits, resume duplicating a row) was invisible to inspection and caught only
by running a test. The default is **TDD with property-based tests** (`proptest`) — see
§9 for the full methodology, which is not optional boilerplate but a core practice here.

- **Write the property (and the strategy signature) first**, watch it fail, then fill
  the impl and the generator. The property statement _is_ the spec — name the invariant
  before writing the code.
- Every change ends in a **passing test that proves the specific thing works.** "It
  compiles" is not done. Prefer the smallest change that closes with a green test; if a
  task can't be closed with a test, it's mis-scoped — split it.
- Favour **generated cases over hand-picked examples** wherever a property exists.
  Reserve example tests for specific regressions and named edge cases; let proptest
  explore the rest. The correctness-critical subsystems (codec ordering, resume) are
  property-tested adversarially, not happy-path.

**Keep diffs reviewable in one sitting.** The dominant failure mode of agentic work
here is a large mostly-correct diff whose 10% wrong part is expensive to find. Small,
self-contained, test-backed changes keep human review a real gate.

**Respect the invariants in §3 absolutely.** They look like implementation details;
several are load-bearing for correctness or are frozen on disk. If a change seems to
require violating one, stop and flag it — do not "simplify" past it.

**Non-functional acceptance criteria count.** "No per-row heap allocation on the scan
path", "values never fetched in the scan loop", "resume pins no snapshot" are part of
_done_, not nice-to-haves. State them in task acceptance criteria or they won't be met.

---

## 1. Build / test / run

```
cargo build
cargo test                      # all tests
cargo test <name>               # a single test / module
cargo clippy --all-targets -- -D warnings
cargo fmt
```

- `fjall` is the storage backend. The `Store`/`FactStore` trait is the seam — an
  in-memory `BTreeMap` impl exists **for tests only** (it is _not_ a product backend).
  Write and test logic against the trait; the fjall impl is a thin implementation of it.
- <!-- TODO: fill in any indexer/fixture-loading commands, feature flags, and the
     exact way to run the executor against a sample DB, once they exist. -->

---

## 2. Architecture map

**Compilation pipeline:** `lex → parse → typecheck → flatten → reorder → plan → execute`

- **lex/parse** produce an untyped CST (the _façade_, `CstKind`/`CstNode`): lossless,
  grammar-shaped, spans + text.
- **typecheck/flatten/reorder** operate on the **`SyntaxTree` store** — a
  struct-of-arrays, `NodeId`-indexed typed tree (recursion-schemes style: a functor per
  phase, `ExprKind<NodeId>` pre-flatten → `GroundKind<NodeId>` post-flatten, one generic
  `reduce`/`map`). `NodeId` gives stable cross-phase identity so typecheck writes a
  _side table_ (`Vec<Ty>` by `NodeId`) without touching the tree, and flatten is
  append-and-reindex into a new store.
- There is also a **boxed, ergonomic lowered AST** (`Query`/`Pattern`/`PatternKind`).
  This is the human-facing shape; store form is what the phases run on. (Keep the two
  representation choices consistent — e.g. record fields are a **sorted
  `Box<[(Symbol, T)]>` everywhere**, not `HashMap`.)

**Storage — two column families:**

- `keys`: `pred_id (4B BE) ++ tuple_encoded_key  →  fact_id (8B BE)`
  — sorted; prefix scans over this _are_ predicate queries (half-open
  `[prefix, strinc(prefix))`; `strinc` is the prefix-successor — increment the last
  non-`0xFF` byte, drop trailing `0xFF`s; empty/all-`0xFF` ⇒ unbounded upper). Scan hot
  loop touches only this CF.
- `entities`: `fact_id (8B BE)  →  [key_len u32 BE][full stored key][value bytes]`
  — point lookup by identity; self-describing (carries its own key).

**Executor (the VM):**

- `Plan` = ordered `[Generator]` (generator 0 = outer loop … n-1 = inner) + a `head`
  projection. A query is a **nested loop**; the ordering _is_ the loop nesting.
- `Generator` = `{ access (predicate + seek key), binds (Vec<VarId>), residuals }`.
- `MachineState` = the register file: `Box<[Option<Register>]>`. A `Register` is
  `{ fact_id, bytes: ByteView }` — the **whole row** (see §3 row-slot).
- `StackFrame` = one loop level: `{ scan cursor, current row, field_offsets cache }`.
- `enumerate()` = the driver: descend (open frame fresh) → pull matching row → bind → recurse;
  on exhaustion close + backtrack. This is **defunctionalised `concatMap`**: the frame
  stack is the reified continuation (see §3 resume).
- `Cursor` = the byte-only resume token. `Row` = a borrowed, one-step-lived projection
  view handed to the consumer.
- **Iteratee seam:** `enumerate(init, step, cancel) -> Outcome`, with per-row
  `Feed::More/Halt` and `Outcome::Done/Suspended{resume}`. The executor is the
  _enumerator_ (producer); projection+serialisation is the _iteratee_ (consumer); a
  bounded channel between the blocking executor thread and the async writer is the
  backpressure seam.

---

## 3. Invariants — DO NOT BREAK

These are the crown jewels of the design. Each has a _why_; the why is what stops a
plausible-looking refactor from silently breaking correctness.

**I1 — Key encoding is order-preserving.** For all values,
`memcmp(encode(a), encode(b)) == semantic_compare(a, b)`. The entire storage model
(prefix scans = predicate queries) rests on this. **Never** change the codec without
re-running the order-preservation property test. Integer band: variable-width,
minimal-magnitude, marker encodes width; negatives use ones'-complement of the
magnitude; wider negatives get _smaller_ markers. The decoder is a _canonicalising
validator_: it recomputes the width from the decoded magnitude and rejects any
non-minimal encoding, and rejects out-of-range values (`i64`/`u64` share the positive
band). One value ⇒ exactly one legal byte string; that bijection is what order-preservation
rests on.

**I2 — Encoding is self-delimiting; `skip` needs no schema.** The marker byte alone
determines how to advance past a value (three skip-shape bands: fixed-width /
terminator-walk / width-in-marker). Records are `MARK_RECORD <elems> MARK_TERM` with a
bare null _element_ escaped as `0x00 0xFF` (distinguishing it from the terminator);
`skip` walks record interiors with `nested=true`. Schema-free skip is what lets the scan
hot loop and field walks work without type info. Self-delimiting cuts both ways: `skip`
lands exactly at the next value's start, and a full decode consumes _exactly_ to
end-of-input (trailing bytes are an error). Record nesting is bounded
(`MAX_RECORD_DEPTH`), so hostile/corrupt depth surfaces as a `BadRecord` error, never a
stack overflow.

**I3 — The marker table is frozen on disk.** Marker _values and their relative order_
are semantic (a marker is the MSB of a value's sort key). Once any data is written they
cannot change without a migration. Reserved bands exist for future types; new types go
in a reserved slot in the correct skip-family band, never by renumbering.

**I4 — Resume == uninterrupted run.** Suspending and resuming must reproduce the exact
row sequence of an uninterrupted run — **no duplicates, no skips**, including across
join cross-product boundaries. The `Cursor` is **bytes only** (one saved key per active
level); it holds **no live iterators and pins no LSM snapshot**. Resume re-opens each
level `Included(saved_key)`, consumes the saved row to re-bind, then advances; the re-read row's `fact_id` must equal
the saved one (else `BadResumeKey`) — the integrity check that makes a snapshot-free,
bytes-only cursor safe against a shifted/rebuilt store. Test at
_every_ cut point (all suspend schedules), for 1-, 2-, and 3-level plans.

**I5 — Row-slot / register model.** A register holds the _whole_ row (`fact_id` + full
key bytes); the _field_ a variable denotes lives in the **plan** (`SlotField{var,idx}`),
not the register. So a generator binding N variables is N `ByteView` refcount bumps to
the same row — no per-field decode at bind time. **Never** decode fields eagerly at
bind; decode lazily at read/projection sites only.

**I6 — Values never enter the scan hot loop.** Residuals on _key_ fields are checked
during the scan against the `keys` CF only. A fact's value lives in `entities` and is
fetched only when explicitly projected (`Project::Value`) or navigated. Value patterns,
when added, are a _distinct residual class_ evaluated against the fetched value buffer —
never pushed into the scan.

**I7 — The executor is a defunctionalised state machine, on purpose.** The `enumerate`
driver + `StackFrame` stack is the explicit reification of recursive `concatMap`, chosen
_so that execution can suspend to bytes_ (I4). Native recursion / closures / coroutines
cannot do this — a suspended closure pins iterators and a snapshot. **Do not "simplify"
`enumerate` back into recursion.**

**I8 — Immutable snapshot per query.** Resume correctness (I4) assumes a saved key still
resolves to the same fact. fjall iterators pin a read snapshot; **drop the executor at
suspend** to release it. A held `Iter`/`Slice` keeps LSM blocks (and a whole
generation of superseded data) alive — never stash one across an idle portal.

**I9 — Hot-path is allocation-free per row.** Reused scratch buffers; `ByteView` clones
are refcount bumps not copies; field-offset caches are inline `ArrayVec<[usize; 16]>`
(fixed capacity, deep fields walked on-demand past the cache — never heap-spill).
**Copy out only at escape boundaries:** suspend (detach `ByteView` → owned bytes so the
`Cursor` pins nothing), and string/bytes projection into the output buffer.

**I10 — Union alternative discriminants are stable and append-only.** Like protobuf
field numbers: each union alternative has an explicit discriminant, assigned once,
never reused, new alternatives appended. Frozen the moment union-typed data is written.
This is a one-way door — get it right before writing any union facts.

---

## 4. Conventions

- **Errors, not panics, on data paths.** Corrupt stored bytes must surface as a
  `StoreError`/`StoreCodecError` variant, never `unwrap`/`panic` — a bad byte shouldn't
  take down a connection. `unwrap` is fine only where an invariant makes it truly
  impossible, with a comment saying why.
- **Ownership types signal sharing:** `Box<[T]>` for owned-once inner structure;
  `Arc<T>` only at genuine sharing boundaries (a `Plan` shared across a portal/executor);
  `Arc<str>` / `Arc<[u8]>` for content deduplicated across many owners (interned names,
  cached encoded constants). Don't reach for `Arc` by reflex.
- **Record fields are sorted `Box<[(Symbol, T)]>`** (deterministic order, one alloc,
  linear scan beats hashing at tiny arities). Not `HashMap`.
- **Permissive grammar, narrow later.** The grammar/parser stay uniform and permissive;
  meaningless constructs (wildcard in head, non-variable bind LHS, `.value` shadowing)
  are rejected at **typecheck/flatten** with clear diagnostics. Don't contort the
  grammar to encode semantic restrictions.
- **Symbols are interned** (`lasso`/`Spur`); resolve to `&str`/`Arc<str>` at plan-build
  time so runtime code is interner-free. Interning is **two-tier**: schema names live in a
  frozen, `Arc`-shared `SchemaInterner` (read-only, lock-free across queries); query-local
  names in a per-query `Rodeo`. `get_or_intern` resolves **schema-first**, so any schema
  name canonicalises to the same `Symbol::Schema` (locals can't shadow it) and field
  resolution compares `Spur`s, not strings.
- Follow existing prose/formatting/style in the codebase; match neighbouring code.

---

## 5. Scope & phase awareness

Build in dependency + de-risking order: the invariant-critical, catastrophic-if-wrong
pieces first (with heavy test batteries and careful review); the additive
"another-arm-on-an-enum" pieces later (lighter oversight).

**P0 (foundation, mostly designed/prototyped):**

- Codec: `Int`, `Str`, records; order-preservation + round-trip + skip-exactness
  proptests.
- Store trait + in-memory test impl.
- Executor core: plan types, register file, frame, `enumerate`, resume — resume-at-every-cut
  as the gate.
- Projection to `Value` (the "expensive but useful" decoded type; wire codec is
  separate and deferred).
- Compiler front end: lexer, parser, typecheck, flatten — targeting the plan types the
  executor already consumes.
- fjall `Store` impl behind the trait; then the same resume battery against fjall.

**Deferred but explicitly designed-for (additive; must not reshape the machine):**

- **fjall wiring** replacing the in-memory store.
- **Wire protocol / connection layer** — the iteratee `Outcome` + byte `Cursor` are
  exactly its primitives (portals, backpressure, cancellation).
- **Cross-fact navigation** (`X.parent.name`) — a `Fetch` generator: a degenerate
  loop level yielding 0-or-1 rows. Same machine, new access kind.
- **Order comparisons** (`<`, `<=`, …) — new `ResidualOp` arms.
- **Unions as data** — `Ty::Union`/`Value::Union`, a `decode_typed` arm, and a
  `ResidualOp::DiscriminantEq(n)` + payload-bind for **alternative selection** (this is
  most of union usage and needs _no_ new operator — it's a residual + field bind).
- **Disjunction (`|`)** — a real `FlatDisjunction` **node** (union-of-streams). Glean
  keeps it as a node (branches share the enclosing environment), and DNF-distributes
  only _within a single seek's pattern_ (bounded), never across sibling conjuncts. Copy
  that: **never DNF-expand `|` across the surrounding conjunction.** Needs a per-branch
  discriminant added to the `Cursor` — keep the token type extensible to that.
- **Negation (`!`)** and **subqueries** (hoisting + derived binds) — non-conjunctive /
  interior-node constructs; separate design pass each.

**Cancellation:** cooperative, synchronous — poll a cancel flag (`CancellationToken` /
`Arc<AtomicBool>`) every ~4096 scanned rows inside the scan loop. Do **not** make the
executor `async` (its work is blocking CPU/IO; async would only colour the codebase).
Terminal unwind (cancel/deadline/rows-scanned cap) is distinct from `Feed::Halt`
(voluntary, resumable yield).

**Divergences from Glean (deliberate):** arbitrary head projection (Glean emits facts;
we project an arbitrary `Value`/record); our specific storage codec; native Rust
executor instead of a bytecode VM (we implement the abstract machine the VM denotes).

---

## 6. Anti-patterns (things that look reasonable and are wrong here)

- Materialising a full result set (defeats streaming/backpressure). Pull one row at a
  time; halt on backpressure.
- Decoding fields eagerly at bind time (breaks I5/I9). Decode lazily at read sites.
- Fetching a value inside the scan loop (breaks I6).
- Holding an iterator / `Slice` across a suspended portal (breaks I8 — pins a snapshot).
- Rewriting the `enumerate` driver as native recursion (breaks I7 — no more byte-resume).
- Renumbering markers or union discriminants after data exists (breaks I3/I10 — on-disk
  migration).
- DNF-expanding disjunction across sibling conjuncts (exponential blow-up; use the node).
- Reshaping the core machine to add a feature that was designed to be additive — if it
  needs a machine change, that's a signal to stop and reconsider the design.
- `HashMap` for record fields (non-deterministic order).
- `unwrap`/`panic` on decoded data.

---

## 7. Open decisions (not yet settled — don't treat as invariant)

- **`FactRef` marker.** Decision leaning: give `FactRef` its **own marker** (fixed-width
  band) so value decode is self-describing without the schema, and the byte-level
  `Int`/`Fact` distinction is enforced. The earlier "share the integer encoding for
  byte-uniform join splices" rationale was found to be overstated (splices work with a
  distinct marker too). Until implemented, fact-typed fields encode via `put_u64` and
  **decode requires the schema type.** Resolve before unions/decoding tooling harden.
- **Wire (transport) codec vs storage codec — settled.** Storage uses **one tuple codec
  for both keys and values** (values are tuple-encoded, not a separate value format) —
  chosen so queries can eventually _match on values_; `Project::Value` is then
  decode-not-copy-through. It is FoundationDB-_inspired_ but not FDB-compatible; call it
  the **tuple codec**, not "FDB". A distinct **transport/wire codec** applies only to rows
  _after_ they leave the executor (post-yield) and carries none of the storage constraints
  (order-preservation, self-delimiting, join-uniform). `Value` is the intermediate "decode
  to a struct" comfort type in the meantime.
- **`pattern = pattern` unification.** Grammar permits it; flatten currently should
  implement only the easy half (LHS var/wildcard; LHS-structural-vs-generator by
  pattern-pushing) and **reject** var=var-both-bound, generator=generator, and
  anonymous-record=anonymous-record with clear diagnostics (those are the future-feature
  to-do list).

---

## 8. Testing methodology (property-based, generator-first)

Property tests are the primary test form. The point is not "call `proptest!`" — it's a
**maintained library of composable generators** that mirrors the type tree, so that a
strategy for a complex type is _built from_ the strategies of its parts.

### Generators are a first-class, co-owned artifact

- Every domain type owns a **canonical strategy**. Add the type → add its strategy in
  the same change (same discipline as deriving `Debug`).
- Strategies live in a **`proptest` support module per domain area** — e.g.
  `codec::proptest`, `plan::proptest`, `exec::proptest` — gated behind
  `cfg(any(test, feature = "proptest"))`, exporting **named strategy functions**
  (`arb_value()`, `arb_predicate_ty()`, `arb_plan_and_store()`). Tests **import**
  strategies; they do not define generators inline.
- **Compose, don't hand-roll.** Build strategies from combinators
  (`prop_map`, `prop_flat_map`, `prop_oneof`, `prop_recursive`) — never imperative `Rng`
  sampling. This is what makes proptest's **shrinking** work: a compositional generator
  yields a _minimal, readable_ counterexample; a hand-sampled one yields a 400-byte blob
  you can't minimize. Recursive types (`Value`, `PredicateTy`, patterns) use
  `prop_recursive` with an explicit depth/size bound (also stops runaway nesting).
- Inject known edge cases into generators explicitly (`prop_oneof![Just(i64::MIN), …,
any::<i64>()]`): `i64::MIN`, empty string, embedded-null string, empty record,
  max-nesting, single-alternative union. Don't rely on random draws to hit them.
- **Shared fixtures are machinery too, not per-test boilerplate.** Test doubles like the
  in-memory `MemStore` live in one support module (`focus::mem_store`) that tests import —
  don't redefine a store / schema / interner inline per test.
- **Comment the property, not the history.** A test comment states the invariant the test
  pins ("a residual on a key field filters on the field value"), not the bug that
  motivated it or what the code "previously" did.

### The three property tiers (they tell you which generator to build)

1. **Round-trip / involution** — `decode(encode(x)) == x`. Needs a generator for the
   _semantic_ type, covering the full domain incl. edges. First and highest-value tier.
   _(codec: every value type.)_
2. **Metamorphic / relational** — e.g. `memcmp(encode(a), encode(b)) == cmp(a, b)`;
   `skip` lands exactly at the next value's start. Needs pair-generation and an
   **independent oracle** (a hand-written comparator you trust, _not_ the code under
   test). _(codec ordering, skip exactness.)_
3. **Model-based / differential** — the deep tier. Define an obviously-correct **model**
   (slow reference), run the real system, assert output-equality. Write the model
   **first**; it doubles as a permanent oracle. _(executor: resume == uninterrupted run,
   where the model is "run to completion, collect rows" and the generated input includes
   an **interruption schedule** — see below.)_

### Generating well-formed inputs (the executor's hard case)

To test the executor you must generate **valid** `(plan, store)` pairs — a plan whose
generators reference predicates that exist, whose variables are bound before use, whose
seek splices reference already-bound registers. A random `Plan` is almost always invalid
and tests only the error path.

- **Generate schema-first, valid-by-construction.** Draw a small schema (predicates +
  key types) → draw conforming facts (the store) → draw a query valid against that schema
  (introduce variables in dependency order, only splice bound ones). Every case is
  meaningful and shrinks to a _minimal valid_ counterexample. **Use this for anything
  with well-formedness constraints** (plans, typed patterns) — the generator encodes the
  well-formedness rules (it's the type checker in reverse).
- **Reject-sampling (`prop_filter`) is permitted only for flat, mostly-valid domains**
  (a single key's bytes). Past a couple of constraints it wastes draws and shrinks
  badly — do not use it for plans.
- The **interruption-schedule generator** (where to suspend) is the tier-3 technique and
  generalizes: generate store → generate query → generate schedule → assert the result
  is invariant under the schedule. This is what caught the resume-duplicate bug.

### Required property batteries (acceptance gates)

- **Codec:** round-trip (tier 1) + order-preservation vs independent comparator (tier 2)
  - skip-exactness (tier 2), over nested values. Order-preservation is the gate for _any_
    codec change (I1/I2/I3).
- **Executor:** resume == uninterrupted run (tier 3) at **every** cut point, for 1-/2-/3-
  level plans, generated from schema-first `(plan, store)` pairs (I4).
- Regression examples (specific past bugs, named edge cases) live alongside the
  properties as ordinary `#[test]`s — properties explore, examples pin.

---

## 9. Pointers

- `docs/design/` — full design rationale (the "why" behind every invariant above).
- <!-- TODO: link the phase plan (PLAN.md / tracker), the schema format doc, and any
     ADRs as they're written. -->
