---
title: Concepts
description: Facts, predicates, keys and values — the model in one page, with the lifecycle that makes a database an artifact.
---

Two names to keep straight:

- **Fjord DB** — the database. Embedded, immutable, fact-shaped.
- **sigla** — its query *and* schema language. "A sigla query" is a query written in sigla
  and run by Fjord.

## Facts and predicates

A **fact** is a typed record — one thing that is true. Every fact belongs to a
**predicate**, which is Fjord's word for a table: it fixes what the facts in it look like.
And every fact has a **`FactId`**, its identity inside the database.

```schema
predicate File : string
predicate Module : { file : File, name : string }
predicate Decl : { module : Module, name : string, line : int } -> string
```

Three predicates, and each one holds a different shape. A `File` fact is just a string — a
path. A `Module` fact is a record of two fields, and its first field is a **reference** to
a `File` fact rather than a copy of it. A `Decl` fact is a record of three, and the
`-> string` on the end is extra data it carries: the declaration's kind.

## Keys and values

Everything in a fact before the `->` is its **key**. That is the part that identifies the
fact, and the part that is indexed. Everything after the `->` is the **value** — extra
data, read only when a query actually asks for it.

The rule that follows is short and worth memorising:

:::note If a query needs to match on it, it belongs in the key
Queries seek and filter on keys and never touch values, so a value can live somewhere else
and stay out of the hot path — which is [invariant I6](invariants.html#i6), and shapes the
whole storage and execution design. A field you put on the value side is a field you can
read but not search.
:::

### The types you can use

Four building blocks, and that is all of them:

| Type | Written | What it is |
|---|---|---|
| `Int` | `int` | A signed 64-bit integer |
| `Str` | `string` | UTF-8 text |
| `Fact(p)` | the predicate's name | A reference to a fact of predicate `p` |
| `Record` | `{ a : t, b : u }` | An **ordered** list of named fields; records may nest |
| `Union` | `{ a : t = 0 \| b : u = 1 }` | One of several alternatives, each tagged with a number |

No arrays, no sets, no booleans, no optionals. A one-to-many is one fact per element. This
is deliberate — [Schema language](schema-language.html) has the reasoning, and
[Status](status.html) has the list of what is still an open question rather than a missing
feature.

### Every fact has an id

Ids print as `#23:60`, which reads as "predicate 23, fact 60". They are **stable, unique
and never reused** within a database ([I11](invariants.html#i11)), which is what lets one
fact refer to another by number.

They are *physical* ids, though, not a name you can rely on across databases: two
databases built from the same input agree on their content, not on their numbering. The
bit layout, and why it is shaped that way, is in
[Storage](storage.html#fact-ids-are-snowflakes-i11).

## Built once, then frozen

A database moves through a short lifecycle and then stops:

```text
   create ──▶ Writable ──▶ finish ──▶ Complete   (and Broken, for a failed one)
                  │                       │
              ingest, derive          read only, forever
```

Once it is **Complete**, a client asking to write is refused when it connects, rather than
per fact. And because a Complete database can never change:

- a query sees a stable snapshot of the world without doing anything to get one;
- a paused query resumes from a few saved **bytes** rather than a held-open iterator;
- writing parallelises freely, because facts with different keys cannot interfere;
- the database is a directory you can archive, copy and serve from as many processes as
  you like.

The workflow this implies is "a fresh sealed database per build", rather than "update the
index in place". Almost every invariant in the design leans on it.

## The two halves of the system

There is a clean seam down the middle. A **front end** turns sigla text into a plan; a
**back end** runs plans. They meet at one data structure and otherwise evolve
independently.

```text
   sigla text
      │
      ▼   FRONT END
  lex → parse → typecheck → flatten → reorder
      │
      ▼
   Plan IR  ◄──── the fixed contract between the halves
      │
      ▼   BACK END
  executor ── scans ──▶ storage
      │
      ▼
  projected rows ──▶ consumer (shell, wire, viewer)
```

**The plan** is an ordered list of steps, and the order *is* the loop nesting: the first
step is the outer loop, the next one runs inside it, and so on. Deciding that order well
is most of what makes a query fast.

**The executor** walks that nested loop one row at a time. It is written as an explicit
state machine rather than as recursion, which is the thing that lets a query stop and
resume exactly.

[A query, step by step](query-lifecycle.html) follows one query through both halves;
[Executor & resume](executor.html) is the machine in detail.

## Storage in one picture

Every fact is stored twice, in two sorted key–value maps:

| Map | Shape | Job |
|---|---|---|
| `keys` | `predicate ++ key → fact id` | The index. A query over a predicate is a scan over a stretch of this map. |
| `entities` | `fact id → key + value` | Identity. One lookup, for when a query needs a fact's value or follows a reference to its target. |

The two are halves of one fact and are always written together, atomically
([I12](invariants.html#i12)). [Storage model](storage.html) is the full picture, including
the codec and the on-disk layout.

Both halves of every fact, for a whole database, are worth seeing at once — the stored key
as bytes, the same key decoded, and the value beside it:

:::demo store
N where code.Decl {file = _, name = N, line = _}
:::

Step the run and the shaded band is the stretch of the map the scan is walking. It is a
band rather than a scattering because the keys are sorted, and the encoding is built so
that sorting the bytes sorts the values.

## References, in and out

This is the one asymmetry worth learning early.

- **Writing**, a reference may be an id *or* the whole target fact written inline, nested
  as deep as you like. Fjord looks each nested fact up, writes it if it is new, and
  substitutes its id. That is what lets a producer keep no book of what it has already
  sent.
- **Stored**, a reference is an id and nothing else.
- **Reading**, a row therefore carries a number. Asking what that number names is a
  question for the *client* — it asks the server to fetch it, and can expand references
  recursively if you want. sigla itself cannot ask, because a query names a fact by its
  key, never by its number.

## Where to go from here

- [Schema language](schema-language.html) — designing predicates, and why field order is
  the index design.
- [sigla query language](query-language.html) — every construct, with the rows it returns.
- [Invariants](invariants.html) — the rules the design is checked against, each with the
  test that pins it. Two namespaces: `I1`–`I15` for the engine, `ops-I1`–`ops-I10` for
  operations.
- [Status & roadmap](status.html) — what is built, what is not, and
  [where Fjord stands against Glean](status.html#relation-to-glean), which it is inspired
  by and is not a clone of.
