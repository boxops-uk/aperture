---
title: Fjord DB
description: A database of facts about code. Index once, seal the result, then ask it questions in a small typed query language — from a file you can copy anywhere.
---

Fjord DB stores **facts**: small typed records, like *`Crc32` is declared in `Crc32.cs`, at
line 7*. You describe the shape of your facts once, write them in, and then seal the
database — after which it never changes again. What you are left with is a directory you
can archive, copy, and serve to as many readers as you like.

You ask it questions in **sigla**, a small typed language of its own. The job it was built
for is a **code index** — files, declarations, references, the build graph: everything
behind *go to definition* and *find all references*. That is what the sample schema
describes, and what every number in this book was measured against.

<div class="cards">
  <a class="card" href="getting-started.html"><b>Getting started →</b>
    <span>Build the binaries, create a database, serve it, ask it something.</span></a>
  <a class="card" href="walkthrough.html"><b>A guided tour →</b>
    <span>A real session, end to end, with the output it actually printed.</span></a>
  <a class="card" href="concepts.html"><b>Concepts →</b>
    <span>Facts, predicates, keys and values — the model, in one page.</span></a>
  <a class="card" href="query-language.html"><b>sigla reference →</b>
    <span>Every construct the language has, with the rows each one returns.</span></a>
</div>

## The idea, in three parts

**Facts, grouped by predicate.** A *predicate* is Fjord's word for a table: `File`, `Decl`,
`Ref`. It fixes what its facts look like. Part of each fact is the **key** — the part that
is indexed, and the part queries look things up by — and a fact may carry extra data
alongside it that is only read when a query asks. Everything you can search on lives in
the key, which is why designing a schema is mostly deciding what the keys are.

**Built once, then frozen.** You create a database against a schema, write facts into it,
and then `finish` it. From that moment it is read-only: no updates, no deletes, no schema
change. The workflow is "a fresh database per build", the way you would rebuild an artifact
rather than patch it.

**Queries that can pause.** A query is compiled to a small plan and run one row at a time,
and it can stop in the middle, hand you a few bytes, and carry on from exactly there
later. That is what makes paging cheap: nothing is held open between pages, so the bytes
can go in a URL and the next page can be answered by a different process.

## Why read-only is the point

Freezing the data is not a limitation that got bolted on. It is the decision everything
else leans on.

- **Every query sees a stable world.** No locking, no versions to reconcile — there is only
  one version.
- **A paused query costs a handful of bytes**, not a held-open connection.
- **Writing goes as wide as you like.** Facts cannot conflict, so there is no rule to
  arbitrate between writers.
- **The database is just a directory.** `tar` it, copy it, serve it from ten processes.

## A schema, a query, and the rows

A schema is a file. It names your predicates and says what each one holds:

```schema
schema demo {
  predicate Person : string
  predicate Knows  : { from : Person, to : Person }
  predicate Age    : { person : Person } -> int
}
```

A query is the shape you want back, the word `where`, and what to match:

```sigla
{a = X, b = Y} where demo.Knows {from = X, to = Y}
```

Rows come back as JSON, one per line, in the shape the query asked for:

```json
{"a": "#0:1", "b": "#0:2"}
```

`#0:1` is an id, because `Knows` points *at* two `Person` facts rather than containing
them. Ask for those references to be expanded and you get the people themselves:

```json
{"a": "ada", "b": "grace"}
```

## Try it, on this page

The engine that answers a query like that is compiled to WebAssembly and running here, over
a small code index: four files, seven declarations, and the references between them.

:::demo run
N where F = code.File "src/lib.rs"; code.Decl {file = F, name = N, line = _}
:::

Every demo in this book is that engine — not a recording, and not a JavaScript imitation.
Edit the query and everything below it is recomputed by the same code the server runs.

## What is built

:::note Status
The engine, the storage layer, the language, the wire protocol, the server, the client,
the CLI, the shell and a code-search viewer are **built and guarded**. Ingestion from
**files** and **stored derivation** are not. [Status & roadmap](status.html) has the
honest list.
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

## Where to go next

- **New here?** [Getting started](getting-started.html), then the
  [guided tour](walkthrough.html).
- **Writing queries?** [sigla query language](query-language.html) and the
  [Shell reference](shell.html).
- **Designing a schema?** [Schema language](schema-language.html) — read the part about
  field order twice.
- **Wondering how it works?** [A query, step by step](query-lifecycle.html), then
  [Storage](storage.html) and [Executor & resume](executor.html).
- **Building a client?** [Wire protocol](wire-protocol.html) and
  [Clients & the viewer](clients.html).
- **Operating it?** [CLI reference](cli.html) and [Operations](operations.html).
- **Just want the binaries?** [The latest release](https://github.com/boxops-uk/fjord/releases/latest)
  carries `fjord` and `fjord-viewer` with SLSA provenance, and this site as a bundle.

:::note About these docs
This site **is** the Fjord design book — the design of record, including the invariant
registry. Where it says something is built, the repository has a test that says so; where
something is not built, it is listed as not built rather than described as if it were.
:::
