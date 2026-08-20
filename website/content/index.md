---
title: Fjord DB
description: An embedded, immutable fact database with a typed, Datalog-flavoured query language — and a query engine that can suspend to a handful of bytes and resume exactly.
---

Fjord stores **facts**: typed records, grouped by **predicate**, each with a stable
identity. You give it a schema, write facts into a database, seal the database, and from
then on only read it. Queries are written in **sigla** — a small, typed, Datalog-flavoured
language — compiled to a nested-loop plan and run by a pull-based machine that can stop
mid-result, hand you a few bytes, and pick up exactly where it left off.

The canonical use is a **code index**: files, modules, declarations, references, the build
graph, the declaration graph. That is what the sample schema describes and what every
number in these docs was measured against.

<div class="cards">
  <a class="card" href="getting-started.html"><b>Getting started →</b>
    <span>Build the binaries, create a database, serve it, ask it something.</span></a>
  <a class="card" href="walkthrough.html"><b>Walkthrough →</b>
    <span>A real session, end to end, with the output it actually printed.</span></a>
  <a class="card" href="query-language.html"><b>sigla reference →</b>
    <span>Every construct the language has, with the rows each one returns.</span></a>
  <a class="card" href="query-lifecycle.html"><b>A query, step by step →</b>
    <span>From the text you type to the rows that come back — every layer it crosses.</span></a>
</div>

## What it is, in six sentences

1. A **fact** is a typed record. It belongs to a **predicate**, which fixes its type, and
   it has a `FactId` — a `u64` that is unique within the database and never reused.
2. A predicate's type is a **key** (indexed, identifies the fact) and an optional
   **value** (extra data, read only when a query asks). Keys are what queries seek on;
   values never enter the scan loop.
3. A database is **built once and sealed**. `Writable → Complete`, and a Complete database
   is frozen forever: no updates, no deletes, no schema change.
4. The **schema travels with the data**. It is written in a small DSL, embedded in the
   database at `create`, and served back from that copy — so an artifact is
   self-describing and a client can ask what it may ask about.
5. A query compiles `lex → parse → typecheck → flatten → reorder` into a **`Plan`** — an
   ordered list of steps — and the executor runs it as a nested loop over two sorted
   key–value maps.
6. Everything is **client/server over one socket**: the CLI, the shell, the viewer and the
   .NET indexer all speak the same framed, multiplexed wire protocol.

## Why immutability is the keystone

It is not a limitation that was bolted on; it is the decision the rest of the design
leans on.

- A query's view of the world is a stable **snapshot** for free.
- A suspended query can be resumed from a few **bytes** rather than a pinned iterator —
  which is what makes stateless paging possible.
- Ingestion parallelises without a conflict rule, because facts with different keys can
  never interfere.
- An artifact is a directory you can `tar`, copy, and hand to another process.

## What is built

:::note Status
The engine spine, the storage layer, the language front end, the wire protocol, the
server, the client, the CLI, the shell and a code-search viewer are **built and
guarded** — union types included. Ingestion from **files** and **stored derivation** are
not. See [Status & roadmap](status.html) for the honest list.
:::

<p>
<span class="pill ok">codec</span>
<span class="pill ok">executor + resume</span>
<span class="pill ok">fjall store</span>
<span class="pill ok">sigla front end</span>
<span class="pill ok">schema DSL</span>
<span class="pill ok">union types</span>
<span class="pill ok">wire protocol</span>
<span class="pill ok">server + client</span>
<span class="pill ok">CLI + shell</span>
<span class="pill ok">parallel ingest</span>
<span class="pill ok">code-search viewer</span>
<span class="pill todo">file ingestion</span>
<span class="pill todo">stored derivation</span>
</p>

## A schema, a query, and the rows

A schema is a file. Field order is key order, and key order is the index design:

```schema
schema demo {
  predicate Person : string
  predicate Knows  : { from : Person, to : Person }
  predicate Age    : { person : Person } -> int
}
```

A query is a head pattern, `where`, and a list of statements:

```sigla
{a = X, b = Y} where demo.Knows {from = X, to = Y}
```

Rows come back as JSON, one per line, shaped by the query's head:

```json
{"a": "#0:1", "b": "#0:2"}
```

A reference is a fact id on the way out, because that is what a reference *is* once
stored. Ask the client to expand it and you get the fact it names — the same nested shape
a producer writes:

```json
{"a": "ada", "b": "grace"}
```

## Where to go next

- **New here?** [Getting started](getting-started.html), then the
  [Walkthrough](walkthrough.html).
- **Writing queries?** [sigla query language](query-language.html) and the
  [Shell reference](shell.html).
- **Designing a schema?** [Schema language](schema-language.html) — read the part about
  field order twice.
- **Wondering how it works?** [A query, step by step](query-lifecycle.html), then
  [Storage](storage.html) and [Executor & resume](executor.html).
- **Building a client?** [Wire protocol](wire-protocol.html) and
  [Clients & the viewer](clients.html).
- **Operating it?** [CLI reference](cli.html) and [Operations](operations.html).

:::note About these docs
This site **is** the Fjord design book — the design of record, including the invariant
registry. Where it says something is built, the repository has a test that says so; where
something is not built, it is listed as not built rather than described as if it were. The
roadmap and the record of settled decisions live in the repository's `PLAN.md`; the
measured findings in `bench/FINDINGS.md`.
:::
