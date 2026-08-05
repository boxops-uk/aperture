# 7. Compilation

> [Aperture design book](../README.md) · [← 6. Types & schema](06-types-and-schema.md) · **Chapter 7** · [8. Operations →](aperture-cli-design.md)

This is the **front end**: how focus text becomes the [`Plan` IR](04-executor.md) the
executor runs. It's the other half of the system from chapters 3–5, and the two halves meet
only at the `Plan`. Covered here: the three tree representations and why each exists, the
`lex → parse → typecheck → flatten → reorder` pipeline, sargeability, the safety-vs-ordering
distinction, and **derived facts** — the one place a feature is allowed to change the core
machine.

> **Status.** `lex → parse → lower → typecheck` is live in `src/focus/` (`grammar.llw`,
> `lexer.rs`, `cst.rs`, `parse.rs`, `lower.rs`, `syntax.rs`, `ty.rs`) as of
> [`PLAN.md`](../PLAN.md) Phase 2, with the boxed ergonomic AST (representation 3 below) not
> yet built because nothing needs it. **Flatten and reorder are not built** — `src/lens/hoist.rs`
> is their reference. The compilation driver below is Phase 3; flatten is Phase 4. This chapter
> is the *design*.

---

## Three tree representations

A query is represented three ways as it moves through the pipeline. This looks like
over-engineering; each earns its place by doing a job the others can't.

1. **CST façade** (`CstKind`/`CstNode`) — an untyped, **lossless**, grammar-shaped concrete
   syntax tree with spans and source text. Its job is *fidelity*: perfect error messages
   (every node knows its span), and round-tripping back to text. Permissive — it happily
   represents constructs that are meaningless, to be rejected later with good diagnostics.

2. **`SyntaxTree` store** — a **struct-of-arrays, `NodeId`-indexed** typed tree, in the
   *recursion-schemes* style: one functor per phase (`ExprKind<NodeId>` before flatten →
   `GroundKind<NodeId>` after), with a single generic `map`/`reduce`. This is what the
   phases actually run on. Its properties:
   - **`NodeId` gives stable cross-phase identity**, so typecheck writes a *side table*
     (`Vec<Ty>` indexed by `NodeId`) instead of mutating the tree. Annotate-without-mutate.
   - **flatten is append-and-reindex** into a new store — a clean functor transform, not an
     in-place rewrite.
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
  exponential blow-up). The one bounded exception is Glean's "PLAN-B": distribute an `|`
  only *within a single seek's pattern*. This needs a per-branch discriminant on the
  [`Cursor`](05-resume.md) — keep that token extensible.
- **Union select lowers to a residual**, not a generator: `x.alt?` becomes
  `ResidualOp::DiscriminantEq(n)` + a payload bind ([chapter 6](06-types-and-schema.md)).
- **Sargeability** decides, per key field, whether it becomes a **seek** (narrow the scan),
  a **splice** (bytes from an earlier-bound register), or a **residual** (filter during the
  scan). This is *order-dependent*: a field being *captured* (bound for the first time)
  can't seek — it's an output, not an input; a field bound by an earlier level becomes a
  splice; a constant becomes a seek prefix or an `EqConst` residual.

### reorder — identity now, real later

`reorder` chooses the loop order. **In P0 it is the identity function** — and that's
*correct*, not a stub, because of the safety/ordering split below. Its interface, though, is
built for the real algorithm and for [derived facts](#derived-facts): it takes a
**dependency DAG**. The eventual algorithm (build later, not now):

> **Kahn's topological sort** over the dependency graph, layered into **antichains** of
> independently-orderable statements, with a **selectivity heuristic** within each antichain
> (point-matches before prefix-matches before full scans, à la Glean's `Reorder`).
> Negations/conditionals move after their non-locals are bound.

---

## Safety vs ordering — why reorder can be identity

A subtle but load-bearing distinction:

- **Correctness needs only a *safety* check, not a sort.** Every variable used in a
  seek/residual/head must be **captured** in *some* generator's key pattern. Because capture
  happens at first occurrence, "bound before use" holds automatically in *any* linear order.
  So flatten just verifies **range-restriction** (reject queries with an un-captured
  variable — a clear compile error) and any order runs correctly.
- **Ordering is purely a *performance* choice** (selectivity). That's why P0 can ship
  `reorder = identity`: it's slower, never wrong.

**Topological sort becomes *required* only with derived binds** — they consume variables and
can't capture them, so they impose hard ordering edges (and a cycle is a compile error).
That's the next section, and it's why the reorder interface takes a DAG from day one.

---

## The compilation driver

The phases don't thread their own state; they run through one **compilation context** that
carries the shared plumbing:

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

---

<a id="derived-facts"></a>
## Derived facts — the one deliberate machine change

Most new features are additive (a new `ResidualOp` arm, a new `Access` kind). **Derived
facts are not** — they change the core machine, and [conventions](conventions.md) forbid
doing that casually. This is one of the two sanctioned exceptions (the other is the
[`FactRef` marker](02-tuple-codec.md)), done deliberately with its own invariant and its own
resume battery.

A **derived predicate** — `predicate P : … = KEY where <query>` — computes facts from a
query instead of storing them. Supporting it requires:

- **`Register` becomes a `Slot` sum type** — a fact variant (a stored row, today's
  `Register`) *and* a value variant (a computed binding). A **derived bind** `Z = f(bound
  vars)` materialises a computed value into a value-slot where its inputs are live.
- **A derived bind is *not* a loop level.** `enumerate` doesn't iterate it, and the
  [`Cursor`](05-resume.md) doesn't store it — it is **recomputed on restore**. So resume
  must, after re-binding the fact-slots, recompute the value-slots.
- **The new invariant (added when this lands):** *derived binds are pure functions of the
  generator (fact) bindings.* That purity is exactly what lets resume save only generator
  positions and recompute the rest — so the type for derived binds must structurally forbid
  iteration and hidden state. Its guard is a resume battery ([tier-3](testing.md)) over the
  interruption schedule, on top of [I4](invariants.md#i4)'s.

Derived binds impose the hard topological ordering the reorder interface was built for — *the*
case that makes topo-sort necessary; cycles are compile errors (recursion is out of scope).
Mechanism mirrors Glean (`DerivedFactGenerator`, `Derive when`, the `captureKey` trick,
`DerivedAndStored`). Full sequencing in [`PLAN.md`](../PLAN.md) "Phase 6".

---

## Invariants relevant to this chapter

The front end *enforces* invariants owned elsewhere rather than owning many itself:

- [I10](invariants.md#i10) (union discriminant stability) is checked at typecheck/schema-load.
- The **derived-bind purity** invariant will be added here (and to the
  [registry](invariants.md)) when Phase 6 lands.
- Record-field ordering (a [convention](conventions.md)) must be preserved across all three
  tree layers.

---

> **Reading path:** [← 6. Types & schema](06-types-and-schema.md) · **7. Compilation** · [8. Operations →](aperture-cli-design.md)
