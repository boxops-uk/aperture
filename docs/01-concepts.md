# 1. Concepts

> [Aperture design book](../README.md) · **Chapter 1** · Next: [2. The tuple codec →](02-tuple-codec.md)

This chapter is the mental model. It introduces every core term at a shallow depth so the
later chapters can go deep on one thing at a time. Nothing here is load-bearing on its
own — it's the map, not the territory. Terms in **bold** have full entries in the
[glossary](glossary.md).

---

## What Aperture is

Aperture is an **embedded, immutable fact database**. Two names to keep straight:

- **Aperture DB** — the database/product.
- **focus** — its query and schema *language* (and the `src/focus/` module that implements
  the engine and the language). When you read "a focus query," it's a query written in
  focus and run by Aperture.

It is **inspired by [Glean](https://glean.software/), not a clone.** Where we diverge
deliberately, the chapters say so — and
[**what we take, what we changed, and what we have not answered**](glean-comparison.md) is
one table, kept honest about which is which.

### Immutable, by design

A DB moves through a lifecycle — `Writable → Complete` — and once **Complete** it is
frozen forever: no updates, no deletes, no in-place schema change. This is not a
limitation bolted on; it is the keystone. Because a Complete DB never changes:

- a query's view of the world is a stable **snapshot** for free;
- a suspended query can be resumed from a few saved **bytes** rather than a pinned
  iterator (see [chapter 5](05-resume.md));
- ingestion can be massively parallel, because facts with different keys can never
  interfere (see [chapter 3](03-storage-model.md) and [Operations](aperture-cli-design.md)).

Immutability is the assumption almost every invariant leans on.

---

## The data model

### Facts, predicates, FactIds

A **fact** is a typed record. Every fact belongs to a **predicate** — the analogue of a
table or a relation — which fixes the fact's *type*. Every fact has a unique **`FactId`**
(a `u64`), its identity within the DB.

A predicate's type (`PredicateTy`) has two parts:

- a **key** — the part that identifies the fact and is *indexed* for querying;
- an optional **value** — extra data carried by the fact, fetched only when you ask for it.

Both key and value are typed: an integer, a string, a record (a sorted set of named
fields), a reference to another fact, or (later) a union. That is a **deliberately narrow**
type model next to Glean's — no arrays, enums, booleans or optionals — and what that costs is
[accounted for here](glean-comparison.md). Types are covered in
[chapter 6](06-types-and-schema.md).

> **Why split key from value?** Queries seek and filter on keys without ever touching
> values, so the value can live in a separate place and stay out of the hot path. This is
> [invariant I6](invariants.md#i6), and it shapes the whole storage and execution design.

### Predicates are the unit of storage

Facts are grouped by predicate on disk, and a **predicate id** is the prefix of every one
of its keys. A query over a predicate is therefore a **prefix scan** over sorted bytes —
which only works because the codec is order-preserving. That's the bridge to the next
chapter.

---

## The two halves of the system

Aperture has a clean seam in the middle: a **front end** that compiles focus text into a
plan, and a **back end** (the executor + storage) that runs plans. They meet at one data
structure — the **`Plan` IR** — and otherwise evolve independently.

```
   focus text
      │
      ▼   FRONT END  (chapter 7)
  lex → parse → typecheck → flatten → reorder
      │
      ▼
   Plan IR  ◄──── the fixed contract between the halves
      │
      ▼   BACK END  (chapters 3–5)
  executor (enumerate) ── scans ──▶ storage (fjall)
      │
      ▼
  projected rows ──▶ consumer (REPL / wire)
```

### The compilation pipeline (front end)

`lex → parse → typecheck → flatten → reorder → plan`

- **lex / parse** produce a lossless, untyped **CST façade** — grammar-shaped, with spans
  and text.
- **typecheck / flatten / reorder** operate on a typed **`SyntaxTree` store** (a
  struct-of-arrays, `NodeId`-indexed tree) and produce the `Plan`.
- **flatten** is the crux: it turns a query's statements into a flat, ordered list of loop
  levels (**generators**) and decides, per key field, whether it seeks, splices or filters.
  **reorder** then chooses the loop order — the identity for now, and *correct*, because
  ordering is a performance choice and safety is the only thing correctness needs (see
  [chapter 7](07-compilation.md)).

Chapter 7 describes three tree representations and why each would earn its place; **two are
built** (façade → typed store), and the boxed ergonomic AST is not, because nothing has
needed it.

### The `Plan` IR (the contract)

A **`Plan`** is:

- an ordered list of **generators** — generator 0 is the outermost loop, the last is the
  innermost;
- a **head** projection — how to build each output row from the bound variables.

A query is literally a **nested loop**: the generator order *is* the loop nesting. Each
generator says "scan predicate P from this seek key, bind these variables, and keep only
rows passing these residual checks." This is all [chapter 4](04-executor.md).

### The executor (back end)

The executor is a **pull-based virtual machine**. Its driver, `enumerate`, walks the
nested loop one row at a time: descend into the next loop level, pull a matching row, bind
variables, recurse; on exhaustion, back up a level. Crucially it is written as an explicit
**state machine over a stack of frames**, not as native recursion — because that is what
lets a query **suspend to bytes and resume exactly** ([chapter 5](05-resume.md)).

---

## Storage in one picture

Two sorted key–value stores (fjall "column families"):

- **`keys`**: `predicate_id ++ encoded_key → fact_id` — the index. Prefix scans over this
  *are* predicate queries. The scan hot loop touches only this.
- **`entities`**: `fact_id → encoded_key + value` — point lookup by identity, for when a
  query actually needs a fact's value.

The two are the two halves of one fact and are always written **together, atomically**
([I12](invariants.md#i12)). Full detail in [chapter 3](03-storage-model.md).

---

## Where the code lives

Knowing which module is real saves hours:

- **`src/focus/`** — **the live engine and language.** All new work lands here:
  - the codec (`tuple.rs`) and the error taxonomy (`error.rs`);
  - the plan IR and the `FactStore` trait (`plan.rs`), the fjall store behind it
    (`store.rs`), and the in-memory test store (`mem_store.rs`);
  - the executor and resume (`iter.rs`);
  - the schema and interners (`schema.rs`);
  - the front end, all the way to a plan — `grammar.llw`, `lexer.rs`, `parser.rs`, `cst.rs`,
    `parse.rs`, `lower.rs`, `syntax.rs`, `ty.rs`, `flatten.rs`, `reorder.rs`, driven by
    `compile.rs` — plus `print.rs`, which renders a tree back to focus source and is what
    makes the front end round-trippable ([chapter 7](07-compilation.md));
  - `fixture.rs` — **the one fixture database**: the schema, the facts and the example queries
    that the corpus, the test batteries and the shell all share. Not test-gated, because the
    shell is not a test: it is what makes a corpus entry something a person can type at the
    prompt.
  - test support: `fixtures.rs` (shared store contracts and hand-built plans) and `corpus.rs`
    (the language surface as data — the acceptance gate for the grammar, which now runs each
    supported entry against a real store and compares its rows).
- **`src/main.rs`** — the `aperture` binary: an interactive **focus shell** that lexes,
  parses, lowers, typechecks, **compiles and runs** what you type against a real store seeded
  from `fixture.rs`, with `:plan` to show the plan without running it. Useful for seeing the
  whole system behave; not a place to put logic — the plan renderer it needed went into
  `print.rs`.
- **`src/focus.rs`** — the module list, and then a **graveyard of commented-out prototype
  code** (~20 live lines out of ~1,250). Kept only for the transport-codec sketch. Don't add
  code here.

---

## The rest of the book

With this map in hand, read the chapters in order. Each one takes a single box in the
diagram above and explains not just *what* it does but *why it must be that way* — because
in this project the "why" is what stops a plausible refactor from silently breaking
correctness. Those load-bearing whys are captured as numbered **invariants**; every
chapter states the ones it owns, and the [registry](invariants.md) lists them all.

---

> **Reading path:** [Index](../README.md) · **1. Concepts** · [2. The tuple codec →](02-tuple-codec.md)
