# 7. Compilation

> [Aperture design book](../README.md) · [← 6. Types & schema](06-types-and-schema.md) · **Chapter 7** · [8. Operations →](aperture-cli-design.md)

This is the **front end**: how focus text becomes the [`Plan` IR](04-executor.md) the
executor runs. It's the other half of the system from chapters 3–5, and the two halves meet
only at the `Plan`. Covered here: the three tree representations and why each exists, the
`lex → parse → typecheck → flatten → reorder` pipeline, sargeability, the safety-vs-ordering
distinction, and **derived facts** — the one place a feature is allowed to change the core
machine.

> **Status.** The whole pipeline is live in `src/focus/`: `lex → parse → lower → typecheck`
> ([`PLAN.md`](../PLAN.md) Phase 2), the compilation driver (Phase 3), and **flatten →
> reorder** (`flatten.rs`, `reorder.rs`, Phase 4) — so `Compilation::plan` produces a `Plan`
> the executor runs. Two of the three tree representations exist: the boxed ergonomic AST
> (representation 3 below) is not built because nothing needs it, and neither is a
> post-flatten `GroundKind` store, because flatten produces a plan directly and a functor
> pass that copied the tree unchanged would be a representation nothing reads. What flatten
> defers is listed [below](#what-flatten-defers-and-why).

---

## Three tree representations

A query is represented three ways as it moves through the pipeline. This looks like
over-engineering; each earns its place by doing a job the others can't.

1. **CST façade** (`CstKind`/`CstNode`) — an untyped, **lossless**, grammar-shaped concrete
   syntax tree with spans and source text. Its job is *fidelity*: perfect error messages
   (every node knows its span), and round-tripping back to text. Permissive — it happily
   represents constructs that are meaningless, to be rejected later with good diagnostics.

2. **`SyntaxTree` store** — a **struct-of-arrays, `NodeId`-indexed** typed tree, in the
   *recursion-schemes* style: a functor per phase (`ExprKind<NodeId>`) with a single generic
   `map`/`reduce`. This is what the phases actually run on. A *second* functor for the
   post-flatten shape was designed and is **not built** — flatten produces a plan directly,
   and the pass would have copied the tree unchanged. Its properties:
   - **`NodeId` gives stable cross-phase identity**, so typecheck writes a *side table*
     (`Vec<Ty>` indexed by `NodeId`) instead of mutating the tree. Annotate-without-mutate.
   - **flatten would be append-and-reindex** into a new store, were there a second functor —
     a clean transform rather than an in-place rewrite. It goes straight to a `Plan` instead;
     the property that matters (nothing mutates the tree) holds either way.
   - struct-of-arrays keeps nodes compact and cache-friendly.

3. **Boxed, ergonomic AST** (`Query`/`Pattern`/`PatternKind`) — the human-facing shape,
   for code that wants to pattern-match on a query naturally. Convenience, not the phase
   substrate.

Keep the representation choices **consistent** across all three — most importantly, record
fields are sorted `[(Symbol, T)]` slices everywhere, never `HashMap` ([chapter
6](06-types-and-schema.md), [conventions](conventions.md)).

### The typed tree prints back to source

`focus::print` renders the `SyntaxTree` store as focus text, and it is the inverse of
`parse → lower`: **`parse ∘ print` is the identity on trees.** That is not a convenience — it is
what lets the front end be *property-tested* rather than only checked against hand-written
snippets. Generate a tree, print it, parse it, compare; the corpus then says which syntax is
acceptable, and the round-trip says the pipeline is faithful across all of it
([testing](testing.md)).

The other direction does not hold, and is not claimed: printing normalises whitespace, drops
redundant parentheses, and picks its own string escapes. The comparison is therefore between
*canonical forms* of trees — a separate s-expression rendering, deliberately not focus syntax,
so the property cannot be satisfied by the printer agreeing with itself.

The whole difficulty is parentheses: printing has to re-insert exactly the ones the grammar's
three precedence levels require, which makes the printer the place where the precedence
decisions are stated executably rather than in prose.

### A node's span is the whole text it stands for

Typecheck labels a diagnostic with the node's span whatever the node is (`ty.rs`), so the rule
has to be uniform: **a node spans all of its own source text.** An application covers
`test.Foo X`, a record covers `{…}` — and a postfix chain step covers `X.a.b`, not the `b` it
was written with. A step spanning only its name would underline one identifier where every
other kind underlines the construct.

Two consequences are worth stating, because both are easy to get subtly wrong:

- **A step's start comes from the chain, not from its base node.** A parenthesised base
  *excludes* its parens (`paren_primary` lowers as a pass-through to its child, so the child
  keeps the span it was pushed with), and measuring from there would put
  `(test.Foo _).value` at `test.Foo _).value` — an underline that opens inside a paren it never
  closes.
- **Precedence parens belong to no node.** They are inserted by *printing* to preserve a shape,
  so they sit outside the span of what they wrap. A subquery's parens are the opposite case:
  they belong to its own rule, and are inside it.

The printer is what makes this testable. It knows where it emitted each node, so it can
*predict* every span, and lowering the printed text must recover exactly those ranges
([testing](testing.md)) — a generated tree has no source of its own to compare against.

---

## The pipeline

```
lex → parse → typecheck → flatten → reorder → Plan
```

### lex / parse — permissive early

The grammar is a **`lelwel` grammar** (`src/focus/grammar.llw`, compiled by `build.rs`;
`parser.rs` is the generated-parser glue). The governing principle is **permissive grammar,
narrow later**:

> Parse the **full** intended feature surface now — union select (`.alt?`), disjunction
> (`|`), negation (`!`), `pattern = pattern`, nested records — even features not yet
> implemented. Reject the unimplemented ones at **typecheck/flatten** with clear
> diagnostics, *not* at the grammar.

Why: the grammar is the widest one-way door. Reshaping it after downstream code depends on
its tree shape is expensive, so getting it permissive-and-stable once lets every later phase
add *meaning* to constructs that already parse. (The settled lexer/grammar resolutions — the
two precedence rules, flat disjunction, the group/subquery factoring, statement-level
negation, permissive `Nat` — are recorded in [`PLAN.md`](../PLAN.md) Phase 2, each with the
test that pins it.)

One consequence worth stating, since it is not a grammar property at all: the generated
parser is **recursive descent**, and `pattern` is mutually recursive with itself through both
records and fact application. Deep input would overflow the stack, which on a data path must
be an error ([conventions](conventions.md)) — so `parse` bounds nesting from the token stream
*before* parsing, at the codec's `MAX_RECORD_DEPTH`, and returns no tree when the bound is
exceeded.

### typecheck — annotate, don't mutate

Typecheck resolves names against the [schema](06-types-and-schema.md) and writes types into
the `NodeId` side table. It accepts the implemented subset and emits **specific, tested "not
yet implemented" diagnostics** for the deferred constructs the grammar allowed through. It's
also where one-way-door schema rules are enforced at load (stable discriminants,
[I10](invariants.md#i10)).

Three properties make "permissive early" actually work rather than merely parse:

- **Diagnostics carry a code** — `nyi/…` for a deferred construct, `reject/…` for a
  meaningless one, `lit/…` for a malformed literal — so the promise is *testable* by identity
  rather than by wording. The [corpus](testing.md) asserts the whole set of codes a snippet
  draws, which is what stops a deferred construct also reporting a type error about itself.
- **Errors accumulate.** A failed unification rolls its substitution back, so a mistake in one
  record field cannot poison its siblings, and checking continues — a query the grammar let
  through can be wrong several ways at once and should say so in one pass.
- **Poison propagates.** `Ty::Error` unifies with anything *and* binds an unbound variable to
  itself. Without the second half, `X = nosuch.Pred _` reports the unknown predicate and then
  reports again at every `X.field` that follows.

**A record pattern may name a subset of the fields; a record type may not.** An omitted field
in a pattern is a wildcard, so `Edge {from = 1}` means "any edge from 1" — which is exactly
what sargeability wants, since a mentioned prefix of the key becomes a seek and the rest a
scan. Unifying two record *types*, by contrast, requires the same field set: a pattern is a
partial description of a value, a type is not.

### flatten — the crux

Flatten lowers the typed, nested query into the flat `Plan`: an ordered `[Generator]` + a
`head: Project`. Three rules govern it:

- **Disjunction stays a node.** `|` survives flattening as a `FlatDisjunction`
  (union-of-streams) — it is **never DNF-expanded across sibling conjuncts** (that's
  exponential blow-up). Glean's "PLAN-B" (`glean/db/Glean/Query/Flatten.hs:398-459`) is the
  same rule seen from the other side, and it is worth stating as it actually is: it duplicates
  the *enclosing pattern* outward to the nearest enclosing statement, so
  `cxx1.Name ("foo".. | "bar"..)` becomes `(cxx1.Name "foo"..) | (cxx1.Name "bar"..)`
  (`:439-443`) — **one seek per alternative under a disjunction node**, not an alternation
  folded into a single seek. What bounds it is **scope, not frequency**: Glean does PLAN B
  *always* ("for now we do PLAN B all the time", `:453`) and PLAN A — bind a fresh variable,
  duplicate nothing — is the unbuilt alternative; the duplication stops at the nearest
  enclosing statement, which is exactly what keeps it from becoming DNF across conjuncts. N
  seeks from one written pattern is why this needs a per-branch discriminant on the
  [`Cursor`](05-resume.md) — keep that token extensible.
- **Union select lowers to a residual**, not a generator: `x.alt?` becomes
  `ResidualOp::DiscriminantEq(n)` + a payload bind ([chapter 6](06-types-and-schema.md)).
- **Sargeability** decides, per key field, whether it becomes a **seek** (narrow the scan),
  a **splice** (bytes from an earlier-bound register), or a **residual** (filter during the
  scan). This is *order-dependent*: a field being *captured* (bound for the first time)
  can't seek — it's an output, not an input; a field bound by an earlier level becomes a
  splice; a constant becomes a seek prefix or an `EqConst` residual.

### How sargeability actually decides (Phase 4, as built)

A seek is a **byte prefix of the stored key**, and a stored key is its top-level fields back
to back ([chapter 3](03-storage-model.md#a-stored-key-is-flat)). So the seek is built by
walking the key type's fields **in declared order** — which is encoding order — and it can
only be extended while every field so far is fully determined. The first field that isn't
**closes** it, and everything after that filters instead:

| the field's pattern | while the prefix is open | once it is closed |
|---|---|---|
| a constant (literal, or a record of them) | a seek component | `EqConst` residual |
| a string prefix (`"ab"..`) | a seek component, and closes the prefix *after* itself | `Prefix` residual |
| a variable bound at an outer level, or a field read of one (`Y.name`) | a **splice** — `SeekKeyPart::RegisterField` | `EqRegisterField` residual |
| a variable bound *here* (a capture) | closes it — an output cannot narrow | — |
| a wildcard, or a field the pattern omits | closes it | — |
| a record giving only *some* of its fields | closes it, and its given fields become residuals one step deeper | — |

Two consequences worth stating, because both look like details and are not:

- **A key whose every field is an input becomes a point match.** `test.Node {id = X}; test.Edge
  {from = X, to = X}` splices both fields, so the inner level seeks the single row rather than
  scanning and filtering.
- **A prefix can end a seek but never sit inside one.** The bytes after a string prefix are
  not that field's, so a constant record containing one is not a constant at all
  (`Const::Prefix` inside a record is refused, and the field falls back to residuals).

A plan addresses a field with a **`FieldPath`**: a top-level field, plus a step per record it
is nested inside. Flat is the fast path — the executor's field-offset cache holds exactly
those — and a nested step re-derives its offsets per read. `FieldPath` is why
`test.Nested {outer = {inner = X}}` can be projected at all, and why a *whole* record key
cannot: it is not one field, so there is no path that names it.

### reorder — the runnable frontier

`reorder` chooses the loop order, over a **dependency graph** (the interface [derived
facts](#derived-facts) needed too). The algorithm:

> Repeatedly emit the **frontier** — the statements whose `reads` are all bound —
> lowest-numbered first. Greedy, one pass, **no backtracking**.

No backtracking is needed because the constraint is **monotone**: `reads` is structural (fixed
whatever the order) and `bound` only grows, so a statement runnable at one step is still
runnable at the next, and emitting one can never strand another. If any valid order exists,
greedy finds it. That is a completeness claim, and it is property-tested against `antichains()`
as an independent check — the two must agree on *which* graphs are orderable.

**Completeness is Aperture's own, and Glean cannot claim it.** Glean's `Reorder` is two passes
with different jobs, and the second — the one that checks a statement's bindings can actually be
compiled — queues a statement it cannot place, tries the next, and, having got all the way
through the list, **gives up** (`glean/db/Glean/Query/Reorder.hs:575-639`). The difference is
**nested statement groups** from negation and disjunction, whose own reads depend on how their
branches are ordered — which is exactly where monotonicity fails. focus has none until Phase 6b,
so this is a claim to re-prove there rather than one to assume survives.

**A query whose written order already works is returned unchanged**, so this is not a second
way to compile anything: the same plan, plus the orders that used to be refused. Which is the
point — `reorder` is load-bearing for **acceptance**, not only for speed. `test.Ref {of = P};
P = test.Foo {id = 1}` reads `P` in the statement written before the one that captures it, and
the frontier is what makes that query legal at all rather than a refusal with a perfectly good
plan going spare.

What is *not* built is selectivity, and the **tiers** are the one part of Glean's reorderer this
design genuinely takes: point match before prefix match before full scan, applied greedily in
strict order — `chooseAll StmtFilter → chooseAll StmtPointMatch → chooseBest
StmtPrefixFactMatch → chooseBest StmtPrefixMatch → chooseBest StmtScan`
(`glean/db/Glean/Query/Reorder.hs:539-544`). Two things about that ranking travel with it. It is
priced from **key-prefix boundness, not from cardinality** — Glean walks the key pattern left to
right against "is this variable bound?", stops at the first field that is not, and reads the
answer off where it stopped; its `PredicateStats` is a request-driver and deriver concern, never
imported by `Reorder`. And it treats a field the pattern omits as a wildcard that **closes** the
prefix, which is the same rule as the table above, one side using it to build the prefix and the
other to price it.

**Nothing else about the two algorithms is shared, and the resemblance is easy to overstate.**
Glean's `Reorder` contains no topological sort and no antichains; the only topological sort in
its pipeline is over *derived-predicate* dependencies at schema load, in a different module
(`glean/db/Glean/Query/Prune.hs:85`). Its loop is transitive **lookup-chasing** from the bound
set — emit every `X = pred …`
whose `X` is already bound, add whatever that binds, repeat, because such a statement is O(1) and
unlocks more — then a greedy pass over the tiers, then back to chasing. Beyond the tiers, Glean
has and focus does not: lookup-chasing itself, any cost model at all, an `Ordered`/`Floating` tag
distinguishing statements a person put in an order from ones flattening invented, a *semantic*
rule requiring a negation's non-locals be bound before it runs, synthesis of a generator for an
otherwise-unbound variable, and a whole optimiser stage
([below](#folding-a-constant-bind)). Full ledger, both directions:
[Glean comparison](glean-comparison.md).

The blocker here is data, not structure: `StmtDeps` carries variable occurrences only, not the
shape of each statement's key prefix. When it does, the heuristic replaces "lowest-numbered" with
a `min_by_key` over the frontier — which can then weigh a statement against **what is bound at
the moment it would run**, the only point at which "point match, prefix seek or full scan" has an
answer. Note that layering with `antichains()` and sorting *within* a layer cannot express that:
a layer index is only a lower bound on position, so it can never defer a cheap-looking scan past
the selective statement that would have bound its key. `antichains()` is kept for feasibility
and diagnostics, off the choosing path.

**But the blocker is sequencing rather than information, and the comparison is what makes that
plain.** Glean extracts the seek prefix in **codegen**
(`glean/db/Glean/Query/Codegen.hs:1085-1190`), so its reorderer has no real prefix to look at and
must approximate one from the pattern tree; flatten builds seek and residuals *here*, and so
holds the **typed** pattern at the moment it orders — and still declines to read it. The
classifier that does the approximating is about sixty lines. Cheaper still, and independent of
any cost model, is **lookup-chasing**: propagating boundness transitively before consulting a
tier at all, which is the common case on the shell's own code index (files → modules →
declarations → references), where most joins are a point match through an already-bound
reference and the frontier today takes the lower index instead.

**The graph is over variables, not edges between statements**, and that turned out to be
load-bearing rather than a modelling preference. Which statement *captures* a shared variable
depends on the order chosen: in `test.Edge {from = X, to = Y}; test.Node {id = Y}` either
statement can capture `Y` — whichever runs first — and reversing them is a valid plan with a
different seek. An edge list fixes that choice *before* the order is picked, and so forbids
orders that are perfectly correct. So `Deps` records, per statement, the variables it **can
capture** (a bare variable at a key field) and the ones it can only **read** (the base of an
access chain — `Y.name` reads `Y` and can never bind it); edges fall out of an order rather
than constraining it, `respects(order)` is the one property an order must have, and
`antichains()` layers what a selectivity heuristic would be free to sort. A derived bind is
the same shape: reads it cannot satisfy itself, one capture it offers.

Whatever `reorder` returns is checked, in flatten's safety pass, against the order it
actually chose — so a future reorderer that returns an order violating the reads reports it
rather than emitting a plan that reads an unbound register.

---

## Safety vs ordering — where the split actually falls

A subtle but load-bearing distinction:

- **Correctness needs only a *safety* check, not a sort.** Every variable used in a
  seek/residual/head must be **captured** in *some* generator's key pattern, before it is read.
  Flatten verifies exactly that — **range-restriction** — over the order that was chosen, and
  any order passing it runs correctly.
- **Choosing among the orders that pass is a *performance* question** (selectivity), and that
  part is not built: `reorder` takes the first order it finds, which is slower, never wrong.
  *Finding* one, by contrast, is not optional, which is the next paragraph's point.

The line between those two moved once, and it is worth being precise about where it now is.
"Bound before use holds automatically in *any* linear order" is **false**, and used to be
stated here: it holds only where every variable is captured at its first occurrence. A
variable that can only be *read* breaks it — `test.Name Y.name` reads `Y`, and so does `of = P`
where `P` is a row bound elsewhere — and then some orders are correct and others are not. So
ordering is a performance choice *among the safe orders*, and finding a safe one at all is
`reorder`'s job rather than a property of how the query happened to be written.

That is what makes the written order not the run order: `test.Ref {of = P}; P = test.Foo {id =
1}` reads `P` in the statement written first and captures it in the statement written second,
and it compiles — to the same plan as the other spelling. Before, it was refused at typecheck
as a `pattern = pattern` ([open decisions](open-decisions.md)), which conflated an ordering
question with unification.

**A topological order becomes *required* for derived binds** too — they consume variables and
can't capture them, so they impose hard ordering edges (and a cycle is a compile error). That's
the next section, and it's the other reason the reorder interface takes a graph.

The claim is *tested*, not asserted, and from both ends. The tier-3 battery generates a
`(query, store)` pair and runs it against a model that reads the query as slow nested loops
([testing](testing.md)):

- **Handed an order**, it runs every *safe* permutation of the body. The plans differ — one
  seeks where another filters — and the rows do not.
- **Handed a rewritten source**, it runs **every** permutation, safe or not, and lets `reorder`
  choose. This is what says the written order does not matter: the unsafe ones are precisely
  those where a read precedes its bind, and they now compile rather than being refused. A
  separate census counts them, because the property would be decoration if the generator drew
  none.

The two are not the same claim, and the difference is the point: an order that is *given* is
still checked and still refused, because flatten's safety pass runs over the order that was
chosen — by `reorder` or by a caller. What changed is that being written in a bad order is no
longer the same thing as being handed one.

<a id="what-flatten-defers-and-why"></a>
### What flatten defers, and why

Everything below **parses and typechecks**, then draws one specific `nyi/…` naming it — the
permissive-early promise, now checked all the way through the driver *and past it*: the corpus
gate runs `Compilation::plan` and then runs the plan against a real store, so `Supported`
means **returns these rows**. Every code here has a corpus entry **except `nyi/whole-key`**,
which needs a schema where one predicate's whole key is another's field type — a shape the
fixture deliberately does not have, so its guard builds a two-predicate schema of its own.

| construct | code | what it needs |
|---|---|---|
| `X = {a = 1, b = Y}` — a value in **no register** | `nyi/value-bind` | a **derived bind**: the value has to be *built*, which is the `Slot` value variant ([Phase 6](#derived-facts)) |
| `X = Y` with both bound, `X = "a"..`, `gen = gen` | `nyi/bind-unification` | two values compared at runtime and nothing to substitute — a register-to-register residual ([open decisions](open-decisions.md)) |
| `X.name`, `X.value` where `X` came out of a reference field | `nyi/fact-field` | cross-fact navigation: a second lookup, which is a new `Access` kind (`Access::Fetch`) |
| `test.Name Y.value` — a value in a key position | `nyi/value-match` | a residual class over the fetched value buffer, never in the scan ([I6](invariants.md#i6)) |
| `test.Nested Y; test.Wide {outer = Y}` — a whole key matched **into a record field** | `nyi/whole-key` | flat against wrapped: the same record, not the same bytes ([chapter 3](03-storage-model.md#a-stored-key-is-flat)) |
| `Edge {from = X, to = X}` | `nyi/repeated-variable` | a same-row `EqField` residual — the [Phase 4 decision](open-decisions.md) |

**`X = Y.name` is not on this list any more, and the line it moved across is the useful one.**
A field read names a *place* — a register plus a path — so binding a name to it is the same
substitution a constant bind is: no register, no step, and the same plan as writing the read
where the name is used. What is left under `nyi/value-bind` is the case where the right side is
in no register at all and would have to be constructed. So the two bind deferrals now divide on
*where a value is*: nothing anywhere (`nyi/value-bind`) against two things each somewhere
(`nyi/bind-unification`).

That split is what makes [`Slot`](#where-a-value-lives) the single substitution. One function —
`resolve` — answers "where does this expression's value live" for every position that can
consume one: a key field, the head, an alias's right side, and a record's pieces when it
destructures. A constant is an ordinary arm of it rather than a parallel path, so
`test.Bar {id = 1}` and `Z = 1; test.Bar {id = Z}` are the same code and not merely the same
answer. Glean reaches the same place from the other end, and pays for it: it emits a statement
for every read and then removes the redundancy with a unification pass (`Opt.hs`), which costs a
per-row rebuild wherever the pass fails to fire. Substituting a *location* rather than a *term*
is what makes the pass unnecessary here ([Glean comparison](glean-comparison.md)).

A record pattern destructures against any slot for the same reason — `{inner = X} = P.outer`
names each piece of a place — which is Glean's `expandStmt` decomposition with the trivial
leaves never built rather than built and dropped. Its one limit is typecheck's: records unify
exactly, so a pattern has to name every field, and `{extra = _, inner = X}` is the spelling for
"only this piece".

**Reaching a fact through a reference** is the one that would have been dangerous to leave
implicit, and it is now split at exactly the line the danger falls on. *Following* a reference
is supported: a register holds the fact id of the row it is bound to, and
`SeekKeyPart::RegisterFactId` splices that — so a join through a reference costs no store read
and [I6](invariants.md#i6) stays structural. What is still deferred is reaching the fact the
reference *names*, its key fields or its value, which is a second lookup.

The trap the split closes is that a register also holds its own row's **key bytes**, and those
are not the referenced fact's. Splicing them where an id belongs compares two different things
and matches nothing — a silently empty answer rather than an error, which is why the operator
is defined off `Register::fact_id` and the executor's guard fixture separates a row's key from
its id on purpose (an integer field and a reference differ only in their marker byte).

**Hoisting is not deferred either.** A fact pattern denotes the facts matching it, so it is a
generator wherever it is written — in a key field, in the head, under a field read. Flatten
gives each nested one the name the query did not and appends it as a level *before* whatever
named it, innermost first. Everything downstream — the dependency graph, the safety check,
sargeability, projection — then sees an ordinary row bind, which is what keeps hoisting
flatten-local. The claim that it is a *spelling* rather than a second way to run a query is
tested by plan equality: the nested form and the two-statement form compile to the same plan,
not merely to the same rows.

Three rejections are permanent rather than deferred: `reject/unbound-variable` (range
restriction), `reject/not-a-generator` (a statement that matches nothing), and
`reject/not-projectable` (a head that is a pattern, not a value).

---

## The compilation driver

The phases don't thread their own state; they run through one **compilation context**
(`focus::compile::Compilation`) that carries the shared plumbing:

- **One diagnostics sink** for the whole pipeline (parse/typecheck/flatten), accumulating
  errors and **continuing** rather than failing fast (permissive-grammar-narrow-later needs
  multi-error reporting), drained once and rendered via `codespan-reporting`.
- **Shared interners** — the two-tier `SchemaInterner` + per-compilation `Rodeo` ([chapter
  6](06-types-and-schema.md)).
- **The `SyntaxTree` store + side tables** — owned by the context, so typecheck's
  annotations live beside the tree.

The driver's terminal product is `plan(query) -> Plan`. Explicitly **not** now: memoization,
incremental recomputation, a `salsa`-style query engine — the context is a plain threaded
struct; incrementality must not be designed-in speculatively ([`PLAN.md`](../PLAN.md) Phase 3).

Three things about the sink are load-bearing, and the third was a surprise.

**A phase reports by pushing, and cannot return diagnostics.** `parse`, `lower` and
`ty::check` take `&mut Diagnostics` and hand back only their artifact. A `Vec` handed back is
a `Vec` a caller can drop, which made "every diagnostic reaches the user" a property of each
call site rather than of the code — the same shape of problem as an executor that *could* be
parked across a suspend, and the same fix ([I8](invariants.md#i8)): make the wrong thing
unexpressible rather than forbidden.

**A code is an identity, not a string.** `diag::Code` enumerates the taxonomy — `nyi/…`
deferred to a later phase, `reject/…` meaningless, `lit/…` a malformed literal — so a typo
cannot make a test pass for the wrong reason, and `Code::kind` answers "is this something
that will work later?" without parsing the prefix back out. Phase 9 still owns the error
taxonomy end to end; this is its shape.

**The sink has two orders, and they are not the same.** Diagnostics arrive in *phase* order,
so a fault at the head of a query found by typecheck lands after one in its body found by
lowering. That is right for the sink, which is a log — it is what lets a caller ask what a
single phase reported — and wrong for a person, who reads the query top to bottom. So
**rendering sorts by where a diagnostic points and the sink does not**: presentation is a
different question from accumulation, and conflating them means one of the two answers is
wrong.

### The refusal case

`parse` returns `Option<Cst>`, and `None` means *no tree at all* — a source too long to
address, or nesting past the cap. It is not "a tree with errors in it": that is the ordinary
case, comes back as `Some`, and is what lowering's error nodes and typecheck's poison exist
to handle. Keeping the two distinct in the type is what stops a driver treating a refusal as
an empty query.

---

<a id="derived-facts"></a>
## Derived facts — the one deliberate machine change

Most new features are additive (a new `ResidualOp` arm, a new `Access` kind). **Derived
facts are not** — they change the core machine, and [conventions](conventions.md) forbid
doing that casually. This is one of the two sanctioned exceptions (the other is the
[`FactRef` marker](02-tuple-codec.md)), done deliberately with its own invariant and its own
resume battery.

### Two kinds, and only one of them is the executor's business

The word "derived" covers two features that share a name and almost nothing else:

- **Stored derivation** — `predicate P : … = KEY where <query>`, computed once when the DB is
  built and **written as facts**. At query time `P` is facts in a keyspace, scanned like any
  other predicate, so the executor needs *nothing* for it: the deriver is a program that runs a
  query and calls `put`. It is gated on being able to **declare** one, which is the schema DSL
  ([`PLAN.md`](../PLAN.md) Phase 8), and its lifecycle is [ops-I8](invariants.md#ops-i8) —
  create → ingest base → derive → finish, derivers reading a sealed snapshot of the frozen base.
- **Dynamic derivation** — a value computed *while a query runs*, from the bindings live at
  that point. This is the machine change, and the rest of this section is about it.

### The machine change, as built

- **`Register` became a [`Slot`](04-executor.md#the-register-file-and-the-row-slot-model-i5)
  sum type** — a fact variant (a stored row) and a value variant (a computed binding).
- **A plan's body became a sequence of [`Step`](04-executor.md#the-plan-ir)s** — a scan to
  iterate or a value to compute — because `reorder` produces one order, and splitting it across
  two collections joined by an index would be two sources of truth for one ordering.
- **A derived bind is *not* a loop level.** `enumerate` does not iterate it, and the
  [`Cursor`](05-resume.md) does not store it — it is **recomputed on restore**. So resume, after
  re-binding the fact-slots, recomputes the value-slots. That is one forward walk over the
  steps: a scan consumes the next cursor entry, a derive recomputes.
- **The new invariant: [I14](invariants.md#i14)** — *a derived bind is a pure function of the
  fact bindings*, which is exactly what lets the cursor save only generator positions. Its guard
  is a resume battery ([tier-3](testing.md)) at every cut point, on top of
  [I4](invariants.md#i4)'s, and the derive step must sit **above** a scan in at least one case:
  below one, the machine re-enters it from beneath on the way back up and recomputes it anyway,
  so only the above case can observe a resume that failed to.

**I14 mirrors nothing in Glean, and is worth claiming as this design's own.** Glean's on-demand
derivation is macro **inlining**: a derived predicate's defining query is expanded at the call
site and compiled as part of the caller (`glean/db/Glean/Query/Flatten.hs:264-290`), so no
derived value ever exists as a *binding* for a continuation to carry, and no purity rule is
needed to say what happens to one across a suspend. I14 exists because Aperture resumes from
**bytes** ([chapter 5](05-resume.md)) — the question it answers is one only a bytes-only cursor
asks.

Derived binds impose the hard ordering the reorder interface was built for — *the* case where a
statement consumes variables and can never capture them; cycles are compile errors. Recursion is
out of scope, which is a firmer line than Glean draws — so "both decline recursion" needs its
qualifier: Glean refuses a recursive reference by *default* but will compile one behind
`--experimental-recursion` (`glean/db/Glean/Query/Flatten.hs:296-308`), iterating the query to a
fixpoint over the facts each round newly produces
(`glean/db/Glean/Query/Codegen.hs:1412-1465`). Declining it here is still right: a fixpoint
driver is a genuine reshape of the machine, not an additive feature.

**What actually mirrors Glean here is narrow.** `DerivedFactGenerator`
(`glean/db/Glean/Query/Codegen/Types.hs:158`) is the same idea as a step producing a binding no
stored row supplied, and `DerivedAndStored` — spelled `stored` in Angle — is the same
stored-versus-dynamic split as the two kinds above, one of **three** derivation modes rather
than two (`DeriveOnDemand | DerivedAndStored | DeriveIfEmpty`,
`glean/angle/Glean/Angle/Types.hs:619`). Full sequencing in [`PLAN.md`](../PLAN.md) "Phase 6".

**One hazard is worth taking from Glean even though its mechanism is not.** `captureKey`
(`glean/db/Glean/Query/Flatten.hs:549-586`) is not a derivation mechanism at all — it rewrites
`X = pred pat` so the **client** gets the key back without a second fetch, which focus needs
nothing for, because [I5](invariants.md#i5) already puts the whole row in the register. What its
`Note [query result]` records is the trap underneath: where the key cannot be captured and a real
fetch is required, that fetch has to be emitted **last**, because the fact may be produced by a
derived-fact generator and does not exist until then — and nothing otherwise stops a later phase
moving it earlier. That is the same family as I14's "the derive step must sit above a scan" case,
and it becomes a live risk the moment `Access::Fetch` lands: **a fact read must never be ordered
above the step that produces the fact it reads**, and `reorder` is where that would go wrong.

### Folding a constant bind

A variable bound to a **constant** does not become a derived bind at all. `X = 42` — and
equally `X = {name = "foo", y = 24}`, to any depth — is *substituted at every use*: a key field
asks `constant` and a head asks `project`, each reaching the arm it would have reached had the
literal been written in place. So `Z = 1; test.Bar {id = Z}` seeks the bytes `{id = 1}` seeks,
by the same code rather than a parallel path.

**The fold does not care where the bind was written.** Constants are collected from the whole
body before any statement is lowered, so `test.Bar {id = Z}; Z = 1` reaches `emit` with the same
bindings as `Z = 1; test.Bar {id = Z}` and is the same plan. Unlike the row case this needs no
reordering at all — a constant has no level to move — and unlike the row case it was never
*about* the order: the fold was order-free from the start and only typecheck's gate was not
([open decisions](open-decisions.md)).

The same substitution covers a **record of variables on the left**: `{a = X, b = Y} = {a = 1,
b = 2}` destructures piece by piece into the two binds written out, which is exactly the sugar
it looks like. Sound only because the right side is constant — `{a = X} = {a = Y}` would need
the two compared per row — and only because a *literal* leaf on the left is refused:
`{a = 1} = {a = 2}` typechecks and binds nothing, so accepting it would emit no constraint and
mean `true` where it means the empty relation. A wildcard leaf is fine, since it binds nothing
but also cannot fail.

Both halves of "one variable, one constant" are flatten's to enforce, and the second is the
dangerous one: `lookup` walks the bindings in reverse, so `Y = 1; Y = 2` would silently keep
the *last*.

**The substitution reaches through a field read**, and has to. `A = {x = 2}` makes `A.x` the
literal `2`, so `resolve` maps a read through a folded record to the constant of the piece it
names. Stopping at the variable was a real bug, and instructively it went wrong in two
different ways with no error message in either: in the **head** `resolve` declined quietly, so
flatten returned no plan with nothing reported and the "no plan without a reason" assertion
fired; at a **key field** the constraint was dropped altogether, so the level matched every row.
The second is the worse outcome and the reason the arm that lowers a read at a key field now
reports when nothing else explained a decline — a field that narrows nothing, filters nothing
and says nothing is the one result worse than refusing the query.

A folded bind therefore occupies **no register and no step**. Introducing one would be a level
for the executor to walk and a value for a resume to recompute, both to arrive back at a
constant known at compile time. Two consequences worth stating:

- **Range restriction accepts a folded variable as bound before any level runs**, which is
  right on its own terms: `X = 42` gives `X` exactly one value, and the check exists to insist a
  variable ranges over something finite.
- **A query whose every binding folds has no steps**, which is why a plan with no levels has to
  mean the unit relation ([chapter 4](04-executor.md#the-enumerate-driver-i7)).

The trap it walks past: folding reaches `constant`, whose record arm writes the
`MARK_RECORD`-wrapped form, while [a stored key is flat](03-storage-model.md#a-stored-key-is-flat).
Wrapped is right for a record *inside a field* and wrong for a whole key, and choosing wrong
reads bytes that match nothing with no error. It is safe because `key` destructures the
top-level record itself and emits field by field, so a whole key never reaches `constant` — and
because `key` decomposes a whole key into its fields before any of this. Both halves are
invisible from the fold's own code, so both are pinned by tests.

**What is left unlowered.** Nothing in focus currently produces a `Step::Derive`: a constant
folds, and anything else — `Y = X.name`, or `{a = 1, b = Y}` with `Y` captured — is a value that
differs per row. `Y = X.name` would most likely become another *substitution* (an alias for a
field of `X`'s register) rather than a value slot, so the first real producer is likely a
**primitive** ([open decision](open-decisions.md)) or a **subquery**
([`PLAN.md`](../PLAN.md) Phase 6b). The machinery is deliberately built ahead of them, because
its resume behaviour is the expensive thing to get wrong later; it is exercised by hand-built
plans, and [I14](invariants.md#i14) records that scope honestly.

**What folding is not is an optimiser, and that is the gap that bites first.** Glean has a whole
query-simplification stage with no counterpart here (`glean/db/Glean/Query/Opt.hs`): unification
and substitution over `P = Q`, structural decomposition of `{A,B} = {C,D}` into a bind per field,
tautology **and duplicate-statement** elimination, and propagation of a statement that can never
match outward through its conjunction. Its own worked example
(`glean/db/Glean/Query/Opt.hs:53-81`) is the one that matters, because it is exactly the shape
[Phase 8b](../PLAN.md) produces: expanding a derived predicate yields
`X where StringPair {B, A}; X = {A, B}; X = {_, "a"}`, and only substitution **through a record**
turns that into `{A, "a"} where StringPair {"a", A}` — a **seek** where the unsimplified form is a
full scan. The fold above cannot do it: `{A, B} = {_, "a"}` has a variable leaf, and folding
requires constants all the way down. So a stored derived predicate will be systematically slower
than the query a person would have written by hand until something performs that substitution,
which is the failure Glean built `Opt` for.

Two things follow, and the second is a trap in advance. Aperture's constant fold is a **proper
subset** of that stage — `Opt` would fold `X = 42` as one case of general substitution, not as a
feature. And the pass Glean needs *because* it substitutes, `BindOrder.hs`, is legitimately
unnecessary here for a reason Glean states about itself: it exists only because substitution and
statement floating invalidate the bind-versus-match decisions its typechecker already made
(`glean/db/Glean/Query/BindOrder.hs:38-51`). focus decides capture-versus-read once, at collect
time, and nothing later disturbs it — which stops being true the day an `Opt`-shaped stage lands.

---

## Invariants relevant to this chapter

The front end *enforces* invariants owned elsewhere rather than owning many itself:

- [I10](invariants.md#i10) (union discriminant stability) is checked at typecheck/schema-load.
- [I14](invariants.md#i14) (**derived-bind purity**) is owned here: `Computed`'s arms are the
  structural statement of it, so adding one that reads anything but already-bound slots breaks
  [I4](invariants.md#i4) as well as I14.
- Record-field ordering (a [convention](conventions.md)) must be preserved across all three
  tree layers.

---

> **Reading path:** [← 6. Types & schema](06-types-and-schema.md) · **7. Compilation** · [8. Operations →](aperture-cli-design.md)
