# Testing methodology

> [Aperture design book](../README.md) · reference doc

Property tests are the **primary** test form here, not a supplement. Reasoning is not
evidence: nearly every correctness bug this project has hit (codec off-by-ones, a residual
short-circuit, resume duplicating a row) was invisible to inspection and caught only by
running a generated case. This doc is the method — read it before adding tests to any
[invariant-critical subsystem](invariants.md).

The point is not "call `proptest!`." It's a **maintained library of composable generators**
that mirrors the type tree, so a strategy for a complex type is *built from* the strategies
of its parts.

---

## The invariant coverage ledger

Every [invariant](invariants.md) names a **guard test**. Those guards are the acceptance
backbone:

- **Write the guard up front** — the property statement *is* the invariant's spec. Write it
  before or alongside the feature, and watch it fail first.
- A guard whose subsystem doesn't exist yet is `#[ignore = "Ixx — pending Phase N"]`. It's a
  real test body asserting the invariant, just not yet runnable.
- **`cargo test -- --ignored --list` is the ledger.** It shows exactly which invariants are
  specified-but-not-yet-live. A phase is *done* only when the invariants it touches have
  their guards un-ignored and green.
- A test that is `#[ignore]`d for any *other* reason must say so in its ignore message, so
  reading the ledger stays unambiguous. There is one today:
  `store::tests::crashing_writer_child_process` is ignored because it is not a test at all —
  it is the child process [I12](invariants.md#i12)'s crash guard spawns and aborts.

### NFR guards are mechanical, not eyeballed

Non-functional invariants ([I5](invariants.md#i5), [I6](invariants.md#i6),
[I8](invariants.md#i8), [I9](invariants.md#i9)) are exactly the properties that silently
regress under a plausible refactor. They get **mechanical** guards — machinery in a support
module, not per-test boilerplate:

| Invariant | Guard mechanism |
|-----------|-----------------|
| I5 — lazy field decode | a decode-counting probe: binding N vars ⇒ 0 field decodes |
| I6 — no value in scan | a `FactStore` spy that fails if `point()` is called during a key-only query |
| I8 — snapshot released | a drop probe over the store handle *and* every scan it opened, cross-checked against fjall's own open-snapshot count, at all four stops (done/suspend/cancel/unwind) |
| I9 — alloc-free hot path | the `allocation-counter` dev-dependency; N vs 2N rows must match on alloc count *and* bytes |

An NFR with no mechanical guard is an aspiration, not an acceptance criterion. Two more in the
same idiom, guarding cost rather than an invariant: a **skip counter** (projecting k fields of
one row must cost k skips, not k(k+1)/2 — `exec::projection_walks_each_field_once`) and an
**allocation count per `check`** that must stay linear in the size of the type, not quadratic
(`ty::checking_a_deep_type_is_linear_not_quadratic`). Both are exact counts rather than ratios
with a threshold to argue about.

### The front end reports into one sink, and it has two orders

Diagnostics from every phase land in one `Diagnostics` sink per compilation
([chapter 7](07-compilation.md#the-compilation-driver)), which changes what a test asks. "What
did *this phase* report" is no longer "the `Vec` it returned" — nothing returns one — but the
tail added while it ran (`Diagnostics::since`). That is why the sink keeps **arrival** order,
and why **rendering** sorts into source order instead: a test slices a log, a reader reads a
query. One test pins both, on the same two diagnostics in opposite orders, so a change that
collapsed the distinction fails it whichever way it went.

Two properties cover the composed pipeline, and one is deliberately documented by what it does
*not* catch. **Compiling never panics** on arbitrary input, *including through rendering* —
each phase has that property alone, and the driver is where a phase can be handed something
impossible by the one before it. **Compiling is deterministic** — the same source twice gives
the same tree and the same rendered text — which catches a `HashMap` iteration order or a clock
reaching the output, but *cannot* catch a dependence on interning order, since two runs of one
source intern identically. `print`'s round-trip, which re-parses with a fresh interner, is that
guard; collapsing every name in the canonical form to a constant leaves determinism green and
fails eight of the printer's tests.

### Trait contracts are asserted per implementation, not differentially

Where a trait has more than one implementation, its contract lives in `focus::fixtures` as an
assertion each store is put through directly — `assert_scan_stays_in_predicate` and
`assert_short_bound_is_rejected` ([chapter 3](03-storage-model.md#the-scan-contract)). A
differential between two stores is not a substitute: two implementations that break the
contract the same way agree with each other perfectly. Both of these exist because that
happened — a leak `MemStore` and `FrozenStore` shared, which the differential could never see.

---

## Generators are a first-class, co-owned artifact

- Every domain type owns a **canonical strategy**. Add the type → add its strategy in the
  same change (the same discipline as deriving `Debug`).
- Strategies live in a **`proptest` support module per domain area** — today
  `tuple::proptest` (values and typed pairs), `plan::proptest` (whole `(plan, store)` pairs,
  which is what the executor batteries generate) and `syntax::proptest` (query trees, for the
  front-end round trip) — gated behind `cfg(any(test, feature = "proptest"))`, exporting
  **named strategy functions** (`arb_value()`, `arb_typed_pair()`, `arb_plan_and_store()`,
  `arb_interruption_schedule()`, `arb_query_spec()`). Tests **import** strategies; they don't
  define generators inline.
- **Compose, don't hand-roll.** Build strategies from combinators (`prop_map`,
  `prop_flat_map`, `prop_oneof`, `prop_recursive`) — never imperative `Rng` sampling. This
  is what makes proptest's **shrinking** work: a compositional generator yields a *minimal,
  readable* counterexample; a hand-sampled one yields a 400-byte blob you can't minimise.
  Recursive types (`Value`, `PredicateTy`, patterns) use `prop_recursive` with an explicit
  depth/size bound.
- **Inject known edge cases explicitly** (`prop_oneof![Just(i64::MIN), …, any::<i64>()]`):
  `i64::MIN`, empty string, embedded-null string, empty record, max-nesting,
  single-alternative union. Don't rely on random draws to hit them.
- **Shared fixtures are machinery too.** The in-memory `MemStore` (`focus::mem_store`) and
  schema/fixture builders live in support modules tests import — never redefined inline.
- **Comment the property, not the history.** A test comment states the invariant it pins ("a
  residual on a key field filters on the field value"), not the bug that motivated it.

---

## The three property tiers

The tier tells you which generator to build.

1. **Round-trip / involution** — `decode(encode(x)) == x`. Needs a generator for the
   *semantic* type, covering the full domain including edges. First and highest-value tier.
   *(codec: every value type.)*
2. **Metamorphic / relational** — `memcmp(encode(a), encode(b)) == cmp(a, b)`; `skip` lands
   exactly at the next value's start; a schema fingerprint is invariant under file layout.
   Needs **pair-generation** and an **independent oracle** — a hand-written comparator you
   trust, *not* the code under test. *(codec ordering/skip; schema identity.)*
   **A tier-2 property can say what a tier-3 one structurally cannot**, and negation is the
   worked example: the model is a second reading of the same specification, so if the model and
   the engine share a wrong idea of what `!` means, they agree. Running one query three ways —
   `!S`, `S`, and neither — and relating the three answers uses no model at all
   (`flatten::a_negation_and_its_assertion_partition_the_rows`). Write **both** halves of such a
   law: the version that only said "the two halves cover everything" passed happily against a
   negation that never filtered anything, which the mutation check is how we found out.
3. **Model-based / differential** — the deep tier. Define an obviously-correct **model** (a
   slow reference), run the real system, assert output-equality. Write the model **first**;
   it doubles as a permanent oracle. *(executor: resume == uninterrupted run, the model
   being "run to completion, collect rows," the input including an interruption schedule;
   ingestion: order-independence under chunk shuffling.)*

---

## Generating well-formed inputs (the executor's hard case)

To test the executor you must generate **valid** `(plan, store)` pairs — a plan whose
generators reference predicates that exist, whose variables are bound before use, whose seek
splices reference already-bound registers. A random `Plan` is almost always invalid and
tests only the error path.

- **Generate schema-first, valid-by-construction.** Draw a small schema (predicates + key
  types) → draw conforming facts (the store) → draw a query valid against that schema
  (introduce variables in dependency order; only splice bound ones). Every case is
  meaningful and shrinks to a *minimal valid* counterexample. Use this for anything with
  well-formedness constraints (plans, typed patterns) — the generator **is the type checker
  in reverse.**
- **Reject-sampling (`prop_filter`) is permitted only for flat, mostly-valid domains** (a
  single key's bytes). Past a couple of constraints it wastes draws and shrinks badly — not
  for plans.
- The **interruption-schedule generator** (where to suspend) is the tier-3 technique and
  generalises: generate store → generate query → generate schedule → assert the result is
  invariant under the schedule. This is what caught the resume-duplicate bug.

---

## Required property batteries (acceptance gates)

- **Codec:** round-trip (tier 1) + order-preservation vs independent comparator (tier 2) +
  skip-exactness (tier 2), over nested values. Order-preservation gates *any* codec change
  ([I1](invariants.md#i1)/[I2](invariants.md#i2)/[I3](invariants.md#i3)).
- **Executor:** resume == uninterrupted run (tier 3) at **every** cut point, for 1-/2-/3-
  level plans, from schema-first `(plan, store)` pairs ([I4](invariants.md#i4)) — against
  both `MemStore` and fjall ([I8](invariants.md#i8) needs the latter).
- **Ingestion:** encoder/decoder round-trip (tier 1) + order-independence under chunk
  shuffling (tier 2) + same-key-different-value deterministic rejection
  ([I11](invariants.md#i11)/[I12](invariants.md#i12)/[ops-I4](invariants.md#ops-i4)).
- **Schema:** fingerprint order-independence (tier 2) + incompatible-schema rejection at
  ingest ([I13](invariants.md#i13)).
- **Front end:** the **target-feature corpus** (`focus::corpus`) — the language surface as
  *data*, each snippet classified `Supported(rows)` / `Diagnosed(code)` / `ParseError`, with
  three gates over it: every entry parses as classified, every entry draws exactly the
  diagnostic codes it claims, and **every supported entry runs against a real `FjallDb` and
  returns the rows it records**. This is the acceptance artifact for
  "[permissive grammar, narrow later](07-compilation.md)": a construct deferred to a later
  phase must be reported *by name*, never as a parse error or a panic. Diagnostics carry
  codes (`nyi/…`, `reject/…`, `lit/…`) precisely so the gate asserts identity rather than
  wording. The gates accumulate rather than failing on the first entry, so one run lists
  everything outstanding — which is how the Phase 2 audit was taken in the first place.
  Parse and lowering additionally have no-panic properties over generated token soup, because
  a tree with holes in it is the ordinary input to lowering, not an edge case.

  The rows live *in the classification* rather than beside it, so a construct cannot be marked
  supported without saying what it answers. That distinction is the whole of what Phase 5 added
  here: `Supported` had meant "produces a plan", and a plan that seeks the wrong prefix or
  projects the wrong path is still a plan. The database is `focus::fixture`, shared with the
  shell — which is what makes `every_shell_example_is_a_supported_entry` possible, and what
  caught a shell advertising two queries the compiler had no plan for.
- **Front end, tier 1:** **`parse ∘ print == id` on trees** — generate a tree
  (`syntax::proptest`), print it as focus source (`focus::print`), parse and lower the text, and
  the tree must come back structurally identical. This is what stops the corpus being the *whole*
  specification of the surface: the corpus says which syntax is acceptable, the round-trip says
  the front end is faithful across all of it. Only that direction is claimed — `print ∘ parse` is
  not the identity on *text*, since whitespace, redundant parens and the choice of string escapes
  are normalised away — so the comparison is between canonical forms of trees, in a rendering
  deliberately distinct from the printer's so the property cannot be circular. The generator's own
  population is asserted (median size, every construct reached), because a strategy that
  degenerates leaves the property green and vacuous.
- **Front end, tier 1:** **a node's span is where its text was printed.** The printer records
  the range it emitted each node at, and parsing and lowering that text must give back exactly
  those ranges ([chapter 7](07-compilation.md)). This is the half of the front end the tree
  round-trip is blind to: spans carry no structure, so every one of them could be off by a byte
  or name a sibling while the tree comparison stayed green — and spans are what every diagnostic
  points with. It found the access chain spanning only its field name, so a type error on
  `X.a.b` underlined `b` where one on `test.Foo X` underlined the application.
- **Front end, tier 3:** **"the flattened plan runs to the rows the query means"**
  (`flatten::proptest`) — generate a schema, conforming facts, and a *query in focus text*
  valid against them; compile it through the real driver, run it, and compare against a
  **model** that reads the query as slow nested loops. The model is the oracle, written first,
  and deliberately shares nothing with the compiler's idea of how to go fast. The same battery
  carries the **reorderability** claim: run every permutation of the body and the rows must not
  change — which is what makes `reorder = identity` an argued choice rather than a shortcut
  ([chapter 7](07-compilation.md)). Its population is asserted too (statements, joins,
  constants, wildcards, rows produced), for the same reason the printer's is.
- **The same battery carries [I4](invariants.md#i4) over *compiled* plans**, on `MemStore` and
  on fjall: the query is also run with a suspend at every scheduled row and compared against
  the model. This is not redundant with the executor's own resume battery, and the reason is a
  generator-coverage argument worth remembering: `plan::proptest` draws plans that seek by one
  whole spliced field from an empty prefix, with at most one flat-path residual per level and
  no value projection — while flatten emits constant seek prefixes, several-part composites,
  `ResidualOp::Prefix`, nested field paths, several residuals per level and `Project::Value`. A
  **census** asserts the battery reaches each of those shapes, because otherwise the extra arm
  would be a slower way of testing what was already covered. Written first, it failed on five
  of the six.
- **Regression examples** (specific past bugs, named edge cases) live alongside the
  properties as ordinary `#[test]`s — properties explore, examples pin.

---

> [← Invariants](invariants.md) · [Index](../README.md) · [Conventions →](conventions.md)
