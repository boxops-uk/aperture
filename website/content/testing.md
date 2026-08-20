---
title: Testing method
description: Property-first, generator-first, model-based — the three tiers, the mechanical guards for non-functional rules, and the corpus that keeps the language honest.
---

The method here is worth reading even if you never touch the code, because it is the reason to
believe the rest of these docs. Nearly every bug in this project's history — codec off-by-ones, a
residual short-circuit, resume duplicating a row — was **invisible to inspection** and caught only
by a generated case.

Three habits carry it:

- **Write the property first**, watch it fail, then fill in the implementation. "It compiles" is
  not done.
- **Every invariant owns a guard test, written up front** — the property statement *is* the spec.
- **Non-functional criteria are tested, not asserted.** No per-row allocation, no value fetch in
  the scan loop, no snapshot held across a suspend: each has a mechanical guard.

## The coverage ledger

```bash
cargo test                       # the green suite
cargo test -- --ignored --list   # the ledger
```

A guard whose subsystem does not exist yet is `#[ignore]`d with the invariant in the message — a
real test body asserting the rule, just not yet runnable. So the ignored list *is* the list of
specified-but-not-live invariants, and work that touches an invariant is finished only when its
guard is un-ignored and green. The list is currently **empty**: every guard is live.

A test ignored for any other reason must say so, so reading the ledger stays unambiguous. There
are four: three child processes that the crash guards spawn and abort, and a printer that emits
the union corpus's fingerprint for the C# client to carry — none of them a test at all.

## The three tiers

The tier tells you which generator to build.

| Tier | Shape | Needs | Example here |
|---|---|---|---|
| **1. Round-trip** | `decode(encode(x)) == x` | A generator for the *semantic* type, covering edges | Every codec value type; `parse ∘ print == id` on trees |
| **2. Metamorphic** | Two runs related to each other | **Pair generation** and an **independent oracle** you trust — not the code under test | `memcmp` order vs a hand-written comparator; a fingerprint invariant under file layout |
| **3. Model-based** | Real system vs an obviously-correct slow model | The model, **written first** — it doubles as a permanent oracle | Resume equals an uninterrupted run; a flattened plan versus nested loops |

Two refinements the project learned the hard way:

**A tier-2 property can say what a tier-3 one structurally cannot.** A model is a second reading of
the same specification, so if the model and the engine share a wrong idea of what `!` means, they
agree. Relating a query's *own* spellings uses no model at all: a negation and its assertion
partition the rows, a disjunction is the concatenation of its branches, `A | B` answers as
`B | A`, and `!(A | B)` as `!A; !B`.

**Write both halves of such a law.** The version that only said "the two halves cover everything"
passed happily against a negation that never filtered anything — which a mutation check is how they
found out.

## Generating well-formed inputs

To test the executor you must generate **valid** `(plan, store)` pairs: a plan whose generators
name predicates that exist, whose variables are bound before use, whose splices reference
already-bound registers. A random plan is almost always invalid and tests only the error path.

- **Generate schema-first, valid by construction.** Draw a small schema → draw conforming facts →
  draw a query valid against that schema, introducing variables in dependency order. Every case is
  meaningful, and it shrinks to a *minimal valid* counterexample. The generator is the type checker
  in reverse.
- **Reject sampling is permitted only for flat, mostly-valid domains.** Past a couple of
  constraints it wastes draws and shrinks badly.
- **The interruption-schedule generator is the tier-3 technique**, and it generalises: generate a
  store, generate a query, generate a *schedule of where to suspend*, and assert the result is
  invariant under the schedule. That is what caught the resume-duplicate bug.

:::note Generators are a co-owned artifact
A strategy that degenerates leaves a property green and vacuous, so the generators' own
**populations are asserted** — median size, every construct reached, statements/joins/constants
per query, rows produced. A census over the resume battery was written first and failed on five of
the six shapes it claimed to reach.
:::

## Mechanical guards for non-functional rules

Exactly the properties that silently regress under a plausible refactor:

| Invariant | Guard mechanism |
|---|---|
| [I5](invariants.html#i5) — lazy field decode | A decode-counting probe: binding N variables must cause **zero** field decodes |
| [I6](invariants.html#i6) — no value in the scan | A store spy that fails if a point read happens during a key-only query |
| [I8](invariants.html#i8) — snapshot released | A drop probe over the store handle *and* every scan it opened, cross-checked against the engine's own open-snapshot count, at all four stops |
| [I9](invariants.html#i9) — allocation-free hot path | A counting global allocator: N versus 2N rows must match on allocation **count and bytes** |

Two more in the same idiom guard *cost* rather than an invariant: projecting k fields of one row
must cost k skips rather than k(k+1)/2, and type-checking a deep type must be linear rather than
quadratic in allocations. Both are exact counts rather than ratios with a threshold to argue about.

**An NFR with no mechanical guard is an aspiration, not an acceptance criterion.**

## The corpus: the language surface as data

`fjord_engine::corpus` is the whole sigla surface as a table, each snippet classified:

| Classification | Means |
|---|---|
| `Supported(rows)` | Parses, typechecks, compiles, **and returns exactly these rows** against the shared fixture database |
| `Diagnosed(code)` | Parses, then draws exactly this diagnostic code |
| `ParseError` | Not sigla at all; a parse diagnostic is correct |

Three gates run over it: every entry parses as classified, every entry draws exactly the codes it
claims, and **every supported entry runs against a real database and returns the rows it records**.

Two details are the whole value of it:

- **The rows live inside the classification**, not beside it — so a construct cannot be marked
  supported without saying what it answers. `Supported` used to mean "produces a plan", and a plan
  that seeks the wrong prefix is still a plan.
- **Codes, not wording, are asserted**, so diagnostics can be reworded without churning the
  corpus. A construct deferred to a later phase must be reported **by name** — never as a parse
  error and never as a panic. That is the acceptance artifact for "permissive grammar, narrow
  later".

The corpus runs against the same fixture database the shell serves, which is what makes
"every shell example is a supported entry" a test — and it caught a shell advertising two queries
the compiler had no plan for.

## Required batteries

| Subsystem | Gate |
|---|---|
| Codec | Round-trip, order-preservation against an independent comparator, skip-exactness — over nested values. Order-preservation gates *any* codec change |
| Executor | Resume equals an uninterrupted run at **every** cut point, for one-, two- and three-level plans, from schema-first pairs — on both stores |
| Ingestion | Encoder round-trip, order-independence under chunk shuffling, and deterministic rejection of same-key-different-value |
| Schema | Fingerprint order-independence, and incompatible-schema rejection at ingest |
| Front end | The corpus, its three gates, plus no-panic properties over generated token soup |
| Front end, tier 1 | `parse ∘ print == id` on generated **trees**, and *a node's span is where its text was printed* |
| Front end, tier 3 | A generated query run against a slow nested-loop **model**, in **every permutation of the body** |

Two of those deserve a note.

**`parse ∘ print == id` is what stops the corpus being the whole specification.** The corpus says
which syntax is acceptable; the round-trip says the front end is faithful across all of it. Only
that direction is claimed — printing then parsing normalises whitespace, redundant parentheses and
escape choices — and the comparison uses a rendering deliberately distinct from the printer's, so
the property cannot be circular.

**Spans are checked separately, because the tree comparison is blind to them.** Every span could be
off by a byte, or name a sibling, while a tree comparison stayed green — and spans are what every
diagnostic points with. The property found an access chain spanning only its field name, so a type
error on `X.a.b` underlined `b` where one on `test.Foo X` underlined the whole application.

**The permutation property is what makes the reorderer's completeness an argued claim.** Running
every permutation of a query's body and requiring the rows not to change is the acceptance
criterion for "a written order that reads before it binds is fixed rather than refused" — and the
independent feasibility answer (an antichain decomposition) is kept deliberately *off* the
production path, as the witness rather than the implementation.

## Where the fixtures live, and why the split matters

- `fjord_store::fixtures` holds everything store-shaped — the probes, the model stores, the
  scan-contract assertions — because a probe has to be **the same** store type as the store it
  wraps.
- `fjord_engine::fixtures` holds the plan runners and re-exports the rest, so a battery has one
  place to import from.
- `fjord_store::fixture` (singular) is the shared **database**: one schema and one set of facts,
  so a plan shape asserted in one place and an answer asserted in another are about the same rows.

One rule follows from the crate split: a test in a lower crate that needs to run a query belongs in
that crate's `tests/` directory, not its `src/`. A unit test reaching back through the engine
compiles a second copy of its own crate, and the two store types are then **different types**.

## Regression examples pin, properties explore

Specific past bugs and named edge cases live beside the properties as ordinary tests. The division
of labour is deliberate: a property searches the space, an example nails the one case somebody
already got wrong.
