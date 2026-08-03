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

### NFR guards are mechanical, not eyeballed

Non-functional invariants ([I5](invariants.md#i5), [I6](invariants.md#i6),
[I8](invariants.md#i8), [I9](invariants.md#i9)) are exactly the properties that silently
regress under a plausible refactor. They get **mechanical** guards — machinery in a support
module, not per-test boilerplate:

| Invariant | Guard mechanism |
|-----------|-----------------|
| I5 — lazy field decode | a decode-counting probe: binding N vars ⇒ 0 field decodes |
| I6 — no value in scan | a `FactStore` spy that fails if `point()` is called during a key-only query |
| I8 — snapshot released | a drop-probe on the fjall iterator; asserts nothing survives a suspend |
| I9 — alloc-free hot path | an allocation-counting global allocator; asserts 0 allocs per scan step |

An NFR with no mechanical guard is an aspiration, not an acceptance criterion.

---

## Generators are a first-class, co-owned artifact

- Every domain type owns a **canonical strategy**. Add the type → add its strategy in the
  same change (the same discipline as deriving `Debug`).
- Strategies live in a **`proptest` support module per domain area** — `codec::proptest`,
  `plan::proptest`, `exec::proptest` — gated behind `cfg(any(test, feature = "proptest"))`,
  exporting **named strategy functions** (`arb_value()`, `arb_predicate_ty()`,
  `arb_plan_and_store()`). Tests **import** strategies; they don't define generators inline.
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
- **Regression examples** (specific past bugs, named edge cases) live alongside the
  properties as ordinary `#[test]`s — properties explore, examples pin.

---

> [← Invariants](invariants.md) · [Index](../README.md) · [Conventions →](conventions.md)
