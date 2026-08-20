---
title: Concepts
description: Facts, predicates, keys and values; the Writable → Complete lifecycle; and the two halves of the system that meet at one data structure.
---

Two names to keep straight:

- **Fjord DB** — the database. Embedded, immutable, fact-shaped.
- **sigla** — its query *and* schema language, and the crate that implements the engine
  behind it. "A sigla query" is a query written in sigla and run by Fjord.

## The data model

A **fact** is a typed record. Every fact belongs to a **predicate** — the analogue of a
table or a relation — which fixes the fact's type. Every fact has a **`FactId`**, a `u64`
that is its identity within the database.

A predicate's type has two parts:

- a **key** — the part that identifies the fact, and the part that is *indexed*;
- an optional **value** — extra data carried by the fact, read only when a query asks.

```schema
predicate File : string
predicate Module : { file : File, name : string }
predicate Decl : { module : Module, name : string, line : int } -> string
```

`File`'s key is a bare string. `Module`'s is a record of two fields, the first of which is
a **reference** to a `File` fact. `Decl` has a value side — the declaration's kind — which
is fetched on demand and can never be matched on.

:::note Why split key from value?
Queries seek and filter on keys without ever touching values, so the value can live
somewhere else and stay out of the hot path. That is [invariant I6](invariants.html#i6),
and it shapes the whole storage and execution design. The practical consequence for schema
design: **if a query needs to match on it, it belongs in the key.**
:::

### The type model is deliberately narrow

Four constructors, and that is all of it:

| Type | Written | Notes |
|---|---|---|
| `Int` | `int` | A signed `i64`, with its own negative marker band in the codec |
| `Str` | `string` | UTF-8 |
| `Fact(p)` | the predicate's name | A reference to a fact of predicate `p` |
| `Record` | `{ a : t, b : u }` | An **ordered** list of named fields; nesting allowed |
| `Union` | `{ a : t = 0 \| b : u = 1 }` | One of several alternatives, tagged by an explicit, append-only discriminant ([I10](invariants.html#i10)) |

No arrays, no sets, no booleans, no optionals. `maybe` and `enum` are sugar over a union and
wait on a naming decision, since what they desugar to enters the fingerprint. The codec
reserves marker bands for arrays, so the room is physically there; whether they are wanted
is an open question rather than a missing feature.

### Predicates are the unit of storage

Facts are grouped by predicate on disk, and a predicate id is the prefix of every one of
its keys. A query over a predicate is therefore a **prefix scan over sorted bytes** — which
works only because the key encoding is order-preserving ([I1](invariants.html#i1)).

Each predicate also gets its own pair of storage trees, which buys physical isolation
between predicates, fearless parallel ingest, and an O(1) wholesale drop.

### A `FactId` is a snowflake

```text
  63                    40 39                        0
  ┌──────────────────────┬───────────────────────────┐
  │  predicate id (24b)  │  per-predicate seq (40b)  │
  └──────────────────────┴───────────────────────────┘
```

Sequences are per predicate and 1-based, so `#23:1` is "predicate 23, first fact". The tag
is what lets the identity map be split per predicate and a point read still be one lookup.
Ids are **stable, unique and never reused within a database** ([I11](invariants.html#i11))
— and they are *physical* ids, not cross-database identity. Two databases built from the
same inputs agree on content, not on numbering.

## Immutable, by design

A database moves through a lifecycle and stops:

```text
   create ──▶ Writable ──▶ finish ──▶ Complete   (and Broken, for a failed one)
                  │                       │
              ingest, derive          read only, forever
```

Once **Complete**, every open-for-write is refused at session establishment — structurally,
not per write. Because a Complete database never changes:

- a query's view of the world is a stable snapshot for free;
- a suspended query resumes from a few saved **bytes** rather than a pinned iterator;
- ingestion parallelises, because facts with different keys cannot interfere;
- the artifact is a directory you can archive, copy and serve from N processes.

Immutability is the assumption almost every invariant leans on. The workflow it implies is
"a fresh sealed artifact per build" rather than "update the index in place".

## The two halves of the system

There is a clean seam in the middle: a **front end** that compiles sigla text into a plan,
and a **back end** that runs plans. They meet at one data structure and otherwise evolve
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
  executor (enumerate) ── scans ──▶ storage (fjall)
      │
      ▼
  projected rows ──▶ consumer (shell, wire, viewer)
```

**The front end** produces a lossless untyped tree from the grammar, then runs typecheck,
flatten and reorder over a typed, `NodeId`-indexed tree. `flatten` is the crux: it turns
statements into a flat ordered list of loop levels and decides, per key field, whether it
**seeks**, **splices** or **filters**. `reorder` then chooses the loop order greedily,
emitting the *runnable frontier* — so a query that reads a variable the next statement binds
is reordered rather than refused.

**The `Plan`** is `{ nvars, body: [Step], head }`. The body is ordered and the order *is*
the loop nesting; the head says how to build each output row from the bound registers.

**The executor** is a pull-based machine whose driver, `enumerate`, walks the nested loop
one row at a time. It is written as an explicit state machine over a stack of frames rather
than as recursion — because that is what lets a query suspend to bytes and resume exactly.

## Storage in one picture

Two sorted key–value maps:

| Map | Shape | Job |
|---|---|---|
| `keys` | `predicate_id ++ encoded_key → fact_id` | The index. Prefix scans over it *are* predicate queries. The scan hot loop touches only this. |
| `entities` | `fact_id → encoded_key + value` | Identity. A point lookup, for when a query needs a fact's value or a reference is followed to its target. |

The two are halves of one fact and are always written together, atomically
([I12](invariants.html#i12)). Detail: [Storage model](storage.html).

## Two codecs, and they are not the same

| | Storage codec | Transport codec |
|---|---|---|
| Where | On disk, in `keys` and `entities` | On the wire, in both directions |
| Property that matters | **Order-preserving** and self-delimiting | Compact |
| Ints | Marker byte, big-endian minimal magnitude | LEB128 varint over zigzag |
| Field names / types | Never present — self-describing by marker | Never sent — both peers hold the schema |
| A reference | Marker plus a fixed 8 bytes | An id, **or the whole target fact nested** |

Measured on the shapes a code index holds, the transport encoding is about 40% smaller than
the storage one. They share no bytes and neither is a layer on the other.

## Where a reference comes from and goes

This is the one asymmetry worth learning early.

- **Inbound**, a reference may be an id or **the target fact written inline**, to any
  depth. Ingest interns each nested fact bottom-up and substitutes the id. That is what
  lets a producer keep no book of what it has already sent.
- **Stored**, a reference is a `FactId` and nothing else.
- **Outbound**, a row therefore carries a number. Asking what it names is a **protocol**
  question (`fetch`), answered with the target's *key* — and the client expands it
  recursively if you ask. sigla cannot ask, because a query names a fact by its key.

## Two invariant namespaces

Do not conflate them:

- **Engine invariants `I1`–`I15`** — codec, executor, resume, storage, identity.
- **Operational invariants `ops-I1`–`ops-I10`** — lifecycle, ownership, reproducibility,
  the one write funnel. Always written `ops-Ix`.

Both are listed with their guard tests on the [Invariants](invariants.html) page. They are
not documentation of intent: each one names a test, and a phase is finished only when the
ones it touches are green.

## Relation to Glean

Fjord is **inspired by [Glean](https://glean.software/), not a clone**. The two-map
storage layout and the nested-loop execution shape are Glean's, down to the names of the
column families. The machine that runs that shape is not: Glean compiles a query to
bytecode for a VM; Fjord walks an ordered `[Step]` with one driver, because a bytecode
VM's continuation cannot be made small and a small continuation is what makes stateless
paging possible.

Four invariants that look inherited are not — order-preserving keys, a self-delimiting
encoding, values kept out of the scan loop, and stable union discriminants. Glean does the
opposite, or nothing, in each case. The repository keeps a full ledger of what was taken,
what was changed and what has not been decided (`docs/glean-comparison.md`).
