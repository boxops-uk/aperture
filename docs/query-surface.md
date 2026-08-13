# The remaining query surface — an architecture note

> **Status: a proposal, not design of record.** It argues one architecture for everything focus
> still parses and cannot run — disjunction, negation, `never`, subqueries — plus the deferred
> items that would otherwise arrive one machine change at a time (`Access::Fetch`, primitives,
> comparisons, union select). Sequencing stays the maintainer's ([`PLAN.md`](../PLAN.md) Phase
> 6b). Where it recommends against something already written down, it says so.

The question it answers is not "can each of these be built" — each can — but **what shape does
the executor end up in if they all are**. The executor is 989 lines of implementation today
([`iter.rs`](../src/focus/iter.rs)); `enumerate` is one loop with two arms. The failure mode to
avoid is the one where each feature adds an arm to that loop, an arm to the `Cursor`, and an
[I4](invariants.md#i4) obligation of its own — six features later the machine is unreviewable
and the resume proof is a case analysis nobody can hold in their head.

---

## 1. The constraint that decides everything

Three systems, three answers to *what does a paused query hold*:

| | what survives between pages |
|---|---|
| PostgreSQL | the **executor tree itself** — the plan's operator nodes, live, server-side |
| Glean | the **whole VM** — bytecode, PC, every register, every output buffer, ABI-locked |
| Aperture | **positions** — one detached row per open level, and nothing else ([chapter 5](05-resume.md)) |

Aperture's is the strongest promise and the most expensive to keep, and chapter 4 already prices
it: *"in Aperture each such feature must extend the `Cursor` and re-prove I4 … the recurring
price of a token this small"*. That sentence is the whole architecture problem, and the way out
of paying it six times is to notice that **it is not owed six times**.

> **The suspendability frontier.** A construct costs cursor work if and only if it can be
> *mid-flight when a row is handed to the consumer*. `step` is called at a full row, so anything
> that finishes within the evaluation of one row — a filter, a deterministic bind, a bounded
> probe — is never caught in the middle by a suspend, and contributes **nothing** to the token.

Classify the surface by that line rather than by how big each feature feels, and the result is
lopsided:

| construct | rows it adds | mid-flight at a suspend? | cursor delta |
|---|---|---|---|
| `never` | 0 | no | none |
| `!S` (negation) | 0 or 1 | **no** — bounded, early-exit | none |
| subquery, existential | 0 or 1 | no | none |
| subquery, generating | — | inlined at flatten | none |
| `X = Y`, both bound | 0 or 1 | no | none |
| comparisons (`<`, `>`) | 0 or 1 | no | none |
| union select (`.alt?`) | 0 or 1, plus a bind | no | none |
| `Access::Fetch` (through a reference) | exactly 1, deterministic | no | none |
| primitives (arithmetic, strings) | exactly 1 | no | none |
| **`\|` (disjunction)** | **N** | **yes** | **a source index** |

**Only disjunction touches the resume token.** Everything else is a filter, a deterministic
bind, or a compile-time rewrite. That is the finding this note is built on: the phase reads like
four features each needing machine work, and it is one feature needing machine work and three
needing a compiler that classifies them correctly.

---

## 2. The vocabulary this implies

Three step kinds, and no more — ever:

```rust
enum Step {
    Level(Level),          // 0..N rows: a loop level
    Test(Test),            // 0..1 rows, binds nothing
    Derive(DerivedBind),   // exactly 1 row, a pure computation
}

struct Level {
    sources: Box<[Source]>,   // 0 = never · 1 = today's scan · N = disjunction
    binds: Box<[Address]>,
}

enum Source {
    Seek { access: Access, residuals: Box<[Residual]> },
    Fetch { of: Address, residuals: Box<[Residual]> },   // through a reference
    Group(Box<[Step]>),                                   // a branch containing joins
}

enum Test {
    NotExists(Box<[Step]>),      // negation
    Exists(Box<[Step]>),         // an existential subquery
    Compare { .. },              // X = Y both bound; <, >, …
    Any(Box<[Test]>),            // a disjunction whose branches all only filter
}
```

Four things about this are the argument, not the notation:

**`never`, a scan and a disjunction are one node at N = 0, 1, N.** Not three constructs sharing
a driver — the same construct, counted. `never` needs no arm, no diagnostic and no special case
in `enumerate`: a level with no sources is exhausted the moment it is entered, which is what the
empty relation *is*. Today's plans are the N = 1 case and stay byte-identical.

**Residuals move onto the source, not the level.** They are field paths into the row, and two
branches of a disjunction are two different key layouts. This is a one-line change now and an
impossible one after a `Residual` slice on `Level` has shipped in a plan format.

**A branch is a body, not a `Plan`.** `Group(Box<[Step]>)` shares the enclosing register file and
has no head of its own — the same shape as Glean's `FlatStatementGroup`, and the reason a
sub-plan needs no second `nvars`, no second interner and no second projection path.

**`Test` reuses the frame `Derive` already has.** Both are 0-or-1-row steps whose entire state is
the one bit that distinguishes arriving from above from arriving from below
(`derived_produced`). A `Test` step costs `enumerate` one arm shaped exactly like the arm it
already has.

> **The architectural rule to adopt, because it is checkable.** A new construct may add a
> `Source`, a `Test`, a `ResidualOp` or a `Computed` arm. **It may not add a `Step`.** Those four
> are additive in the sense the conventions mean — one match arm, no new control flow, no cursor
> consequence. A `Step` is a case in `enumerate` *and* a case in the cursor *and* an I4
> obligation, and "additive" has never been true of one. A test asserting `Step` has exactly
> three variants is the cheapest guard in the project.

---

## 3. What this costs I5, which is where the real tension is

**A variable bound inside a branch cannot be a lazy row, and this is the one place disjunction
collides with an invariant.** [I5](invariants.md#i5) says a register holds the whole row and the
*field* lives in the plan (`RegisterField { address, path }`). That works because exactly one
generator binds a register, so exactly one path is right. Under a disjunction it is false:

```
test.Foo {name = N} | test.Bar {name = N}
```

`N` is a string reached at a different path in each branch, and possibly at a different depth.
A `Project::RegisterField` naming one path decodes the wrong bytes whenever the other branch
produced the row — silently, which is this project's characteristic bug.

Three ways out, and only one is any good:

1. **Dispatch on the active source at every field read** — the level records a path per source,
   and every read consults which branch is live. This puts a branch on the hot path of the thing
   [I9](invariants.md#i9) exists to keep flat, for a feature most rows never touch. No.
2. **Restrict branches to one predicate and one path** (Glean's PLAN-B shape only). Cheap, flat,
   and a real language restriction — `test.Foo N | test.Bar N` is an ordinary thing to write.
3. **A branch *exports* its shared variables as values.** At the end of each branch, the
   variables the disjunction binds are materialised into `Slot::Value`. Every branch then agrees
   about what the register holds, because typecheck already forces the branches to agree about
   its type.

**(3), and the machinery for it already exists.** `Slot::Value` and the recompute-on-resume rule
are exactly Phase 6's derived-bind work, which chapter 7 records as having no producer in the
language yet and guesses will be "a primitive or a subquery". It is neither: **the first real
producer of a value slot is disjunction.**

The rule that falls out is small enough to state in one line, and it keeps the conjunctive hot
path exactly as it is:

> A variable a disjunction binds stays a **row slot** if every branch binds it to a whole row of
> the same predicate (which typing already guarantees when it is predicate-typed); otherwise each
> branch **materialises** it into a value slot. Conjunctive plans are unaffected, so I5 is
> narrowed rather than broken, and the decode cost is bounded by what a query actually shares
> across branches.

Glean pays nothing here because it binds eagerly — `matchPat` decodes or memcpy's every bound
field into a per-variable buffer before the inner loop runs (chapter 4). I5 is the divergence
that buys the allocation-free scan loop, and **this is the bill for it**, arriving exactly where
a lazy row model must be told which row it is looking at. Worth writing into I5 as a recorded
exception rather than discovering it in the middle of 6b-a.

---

## 4. The cursor, generalised once

```rust
struct Cursor(Vec<Entry>);
struct Entry { source: u32, position: Position }
enum Position { Row(Register), Branch(Cursor) }   // Branch only for Source::Group
```

One entry per **open level**, as today; the pairing rule survives verbatim (entries pair with
`Level` steps in order), and today's cursors are the `source: 0, Position::Row` case. `Test`,
`Derive` and `Fetch` contribute nothing, and the reason generalises [I14](invariants.md#i14) into
a rule worth naming:

> **The recompute rule.** In an immutable DB a store read is a pure function of its inputs, so
> anything whose result is determined by the bindings and the frozen base may be **recomputed on
> restore instead of saved**. Derived binds (I14) are the special case; a `Fetch` by fact id, a
> filter's verdict and a branch's exported values are the rest.

Two consequences for sequencing. Resume's forward walk gains one rule — **skip `Test` steps** —
and that is sound rather than a shortcut: a test binds nothing, so replaying it can only turn a
row that was already accepted into a spurious failure. And the **wire encoding should be settled
in this phase**, not after it. Chapter 5 records the token as having no version tag, no checksum
and no plan identity beyond a level count; adding the source index is the last shape change this
note foresees, so it is the moment to spend the two fields Glean spends and stop editing the
format.

---

## 5. Ordering — the immovability problem is smaller than the plan records

[`PLAN.md`](../PLAN.md) Phase 6b flags a risk that `StmtDeps` "cannot say *this one may not move
above that one*", and reads it as needing a new kind of constraint. Glean's own rule, verbatim
from `Note [Reordering negations]`:

> *"To ensure consistent semantics regardless of the order of statements in the source query we
> always move negated subqueries after the binding of all variables from the parent scope that it
> uses."*

**That is a reads-edge, not an immovability tag.** Give a negation `reads` = its free non-local
variables and `captures` = ∅, and the frontier cannot place it before those variables are bound —
because that is the only thing the frontier does. `!(A X); B X` is forced to run as `B X; !(A X)`
by the existing algorithm, which is Phase 6b's stated acceptance criterion, met by defining one
function rather than by adding a mechanism.

**Completeness survives nesting, provided group reads stay structural.** The reason greedy is
complete today is that `reads` is a property of a statement and not of an order, and `bound` only
grows. For a group, define `reads` = the union of its branches' free non-locals and `captures` =
the **intersection** of what its branches bind; both are order-independent, so the monotonicity
argument goes through by induction on nesting, with the branch ordered by a recursive application
of the same frontier seeded by the outer bound set. Glean's reorderer needs a give-up branch
(`iterate [] bad = … -- we already tried the bad list, so the first one should throw`) — but the
comparison ledger's reading of *why* is worth narrowing: the nesting defeats it because of when
it computes what a group needs, and because it also synthesises generators for unbound variables
(`maybeBindUnboundPredicate`), not because a nested group is intrinsically unorderable.

**So: do not add an immovability tag for negation.** The recommendation is to compute group reads
structurally, recurse the frontier, and re-prove completeness against `antichains()` on nested
graphs — the property already exists and wants a generator that draws groups. `Placement` (landed
this week) then has no consumer; keep it only if a second one appears, and delete it if none does
rather than leaving a tag the algorithm never reads.

---

## 6. Scope — take the intersection, don't reject the difference

Phase 6b-b currently says every branch must bind the same variable set, rejected with a
diagnostic otherwise. Glean instead takes the intersection:

```haskell
FlatDisjunction (s:ss) -> foldr (IntSet.intersection . scopeVars) (scopeVars s) ss
FlatNegation{}         -> mempty
```

**Recommend the intersection**, for three reasons. It is compositional — a branch may bind
whatever locals it likes, and only what *all* branches bind escapes. It needs no new diagnostic:
a head reading a variable only one branch binds finds it unbound, and `reject/unbound-variable`
already says exactly the right thing at exactly the right place. And it is the rule that makes a
disjunction usable as a filter, which is the case §7 turns into a `Test`. The `FlatNegation ->
mempty` line is the same rule for negation: a negated group binds nothing outward, which is what
makes it a test rather than a level.

---

## 7. The classification flatten has to do

Most of this note's savings are a compiler decision, and they should be written as one function
with the table in it:

- **A disjunction whose every branch only filters** (no branch captures anything that escapes) →
  `Test::Any`, not a level. `X.kind = "a" | X.kind = "b"` then costs no register, no level and
  no cursor entry. This is the common shape and it should never reach the machine as a level.
- **A disjunction whose branches are single generators** → `Level` with N `Source::Seek`s. Flat,
  one integer of cursor, no nested machinery. This is Glean's PLAN-B shape
  (`cxx1.Name ("foo".. | "bar"..)`), and flatten should *normalise* into it even after groups
  exist, so the common case never pays for the general one.
- **A disjunction whose branches contain joins** → `Source::Group`. The general case, and the
  only one that makes the cursor recursive.
- **A subquery in a generating position** → inlined into the enclosing statement list. Phase 2
  made group and subquery one grammar rule precisely so this is flatten-local; confirm it before
  budgeting anything else, as 6b's own decision list says.
- **A subquery whose bindings must not escape** → `Test::Exists`.
- **`never`** → a `Level` with zero sources, or dropped outright as a branch of a disjunction.

---

## 8. Negation, and why the literature's hard part does not apply

The classical difficulty with negation in Datalog is **stratification**: negation over a
recursively-defined relation has no meaning until you fix an evaluation order (Apt–Blair–Walker),
and the well-founded and stable-model semantics exist for programs that cannot be stratified at
all. None of that arrives here, and the reason is worth stating so a later reader does not import
machinery for a problem this design does not have:

**focus has no recursion, and the base is immutable and complete.** Every negation is therefore
evaluated against a relation that is already total, which is the definition of being trivially
stratified. What survives from the literature is the much older and smaller obligation —
**range restriction**: every variable in a negated literal must be bound positively elsewhere,
or the negation quantifies over something infinite. That is §5's reads-edge for the bound ones,
and a rejection for the rest:

> A variable occurring **only** inside a negation is `reject/unbound-variable`. `!(Q _)` already
> spells "no `Q` at all", so the wildcard reading is available and unambiguous, and the two
> readings of `!(Q X)` are indistinguishable at a glance — which is the argument for refusing it
> rather than picking one. (Glean picks the other road in one case, synthesising a generator for
> an unbound predicate-typed variable; that is a road, and it is not this one.)

Stratification does return, under its own name, in exactly one place: **stored derivation**
([Phase 8b](../PLAN.md)). A derived predicate defined with a negation over another derived
predicate must be derived *after* it, and the plan's "topological sort of the derivation graph
with concurrency inside each stratum" is that condition — the word for what it computes is a
stratification, and calling it that is what will stop someone later rediscovering the ban Glean
imposes for incrementality reasons the immutable DB does not share.

The evaluation shape is settled by the frontier and confirmed by the reference: Glean compiles
`CgNegation` as `singleResult (…) (jump fail)` — run the sub-query until the first result, then
reject the row. An anti-semijoin with early exit. It produces no bindings and cannot be suspended
inside, which is why it costs the cursor nothing.

---

## 9. Why not the alternatives

**A `Step` arm per construct.** The default path, and the one the concern in the brief names. It
fails on multiplication rather than on any single step: six features is six `enumerate` arms, six
`Cursor` cases and six I4 re-proofs, and the resume argument stops being "entries pair with levels
in order" and becomes a case analysis. The frontier in §1 is what makes it unnecessary — most of
those features are not levels.

**A flat body with successor tables** (branches as contiguous ranges, `next_on_ascend` /
`next_on_exhaust` precomputed by the compiler). Genuinely tempting: `depth` stays a `usize`, the
cursor stays flat, and the compiler holds the complexity. It is also a bytecode VM with the
opcodes filed off — a plan whose meaning lives in its edges rather than its nesting — and it
reintroduces the property that made a VM the wrong answer here (a plan you cannot read locally),
without the thing a VM buys (a continuation that saves itself wholesale). Declining bytecode was
decided on token size; declining this is decided on reviewability.

**DNF expansion.** Exponential, already rejected in chapter 7, and worth keeping rejected: Glean
distributes an alternation only as far as the nearest enclosing statement for the same reason.

**A Volcano operator tree of iterators.** The standard answer, and precisely what
[I7](invariants.md#i7) forbids — a suspended iterator pins a snapshot, which is
[I8](invariants.md#i8). Worth noting that §2's design *is* an operator tree; the difference is
that its nodes are inert data walked by one driver, rather than objects holding their own control
state. That is the whole content of "defunctionalised", and it is why `Source::Group` does not
breach I7 while a `Box<dyn Iterator>` would.

---

## 10. A staging that keeps each step provable

Each stage ends green and re-proves only what it changed:

1. **`Test` as a step kind** — negation, `X = Y` with both bound, and the comparison operators
   that have wanted a home since Phase 4. No cursor change; resume gains the skip rule. Retires
   `nyi/negation` and half of `nyi/bind-unification`.
2. **`Level { sources }` at N = 0 and 1** — a pure refactor: `never` arrives, today's plans
   compile to N = 1, and every existing battery must stay green untouched. This is the diff that
   should be boring, and doing it alone is what makes the next one legible.
3. **N sources, all `Seek`** — disjunction proper, the export rule of §3, the source index in the
   cursor, I4 re-proved with the census extended to draw a disjunctive plan *and* a cut taken
   mid-branch. The single largest step, and the only one that touches the token.
4. **`Source::Group`** — branches with joins; the recursive cursor entry and the inductive I4
   proof. Deferrable behind a diagnostic if the corpus says nobody writes one.
5. **`Source::Fetch`** — reaching a fact through a reference, which by then is a source arm and
   not a project of its own. Retires `nyi/fact-field`; no cursor change, by the recompute rule.

`Test` first is deliberate: it is the stage with no cursor consequence, it retires two diagnostics
on its own, and it forces the reads-edge work in §5 while the resume token is still untouched.

---

## 11. What this note does not settle

- **Whether `Source::Group` is needed in P0 at all.** A corpus question, and the honest way to
  answer it is to write the queries the example index invites and see whether any branch needs a
  join.
- **Aggregation.** Glean has `FlatAllStatement` (`X = all (P where S)`); focus has no syntax for
  it. It is the one construct here that *cannot* be made suspend-free — it materialises, which
  breaks I9's per-row allocation claim and has no bounded position to save. If it is ever wanted
  it needs its own decision, not an arm.
- **If-then-else.** Angle has `FlatConditional`; the desugaring `(C; T) | (!C; E)` costs a second
  evaluation of `C` and needs no machinery. Recommend the desugaring if it is ever asked for.
- **How a cost model prices a level with N sources.** The tier ranking (point < prefix < scan) is
  per source, and a level needs one number. Max is the safe reading; it is a guess until there is
  a cost model to put it in.
- **Whether `Placement` survives.** §5 argues it has no consumer if negation is a reads-edge.

---

> Related: [chapter 4](04-executor.md) (the machine this changes), [chapter 5](05-resume.md) (the
> token), [chapter 7](07-compilation.md) (flatten and reorder), [`PLAN.md`](../PLAN.md) Phase 6b
> (the sequencing this proposes to amend), [Glean comparison](glean-comparison.md).
