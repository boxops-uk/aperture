# Fjord

**Fjord** (the product: *Fjord DB*) is an embedded, immutable **fact database**.
**sigla** is its typed, Datalog-flavoured query and schema language — a small, faithful subset
of Glean's Angle at the core, and its own thing past that ([what is inherited and what is
not](docs/glean.md)). Facts are typed records identified by a `FactId`, grouped by predicate,
stored in an LSM (fjall) and queried by compiling sigla queries to a nested-loop plan run by a
suspendable, pull-based virtual machine.

The database is **immutable**: a DB is built once (schema → base facts → derivations),
sealed, and thereafter only read. That single decision is what makes the rest of the design
tractable — snapshots are trivial, resume tokens can be plain bytes, and parallel ingestion
is "fearless."

> **Status: `0.0.1`, a pre-release.** Built and guarded: the storage codec and the fjall
> store, a suspendable executor that resumes exactly, the sigla front end end to end — text
> to `Plan` to rows, joins *through fact references* included — the schema language, union
> types and schema identity, the wire protocol with a second implementation in another
> language, parallel ingestion, a server, a client, the command-line tool, and a code-search
> site built on nothing but the client.
>
> **Not built:** authentication, stored derivation, ingestion from files, arrays and sets,
> per-predicate statistics. The engine compiles to WebAssembly and one interactive segment
> runs on it — the rest of that site does not exist yet. [`CHANGELOG.md`](CHANGELOG.md) is the full inventory — including,
> deliberately, what each release does not contain — and [`PLAN.md`](PLAN.md) is the roadmap.

---

## The documentation

**The design book is a website**, and it is the design of record — architecture, the two
languages, the wire protocol, operations, and every invariant with the guard test that pins
it. Read it at **<https://boxops-uk.github.io/fjord/>**, or locally:

```bash
python3 website/serve.py        # build and browse at http://127.0.0.1:8000
```

The source pages are ordinary Markdown under [`website/content/`](website/content/), readable
in place; start at [Overview](website/content/index.md) and follow the nav order, or go
straight to the [invariant registry](website/content/invariants.md) — the fastest way to
check *what must I not break here*.

Beside the book:

- [`AGENTS.md`](AGENTS.md) — the **working contract**: how to work in this repository, the
  conventions, the traps, and the testing method. Read it before changing anything.
- [`PLAN.md`](PLAN.md) — the **roadmap**: what is unbuilt with its acceptance criteria, and
  the record of settled decisions.
- [`docs/glean.md`](docs/glean.md) — where every idea came from, and what each system can be
  asked to do, spends, and charges. **Read it before proposing a feature Glean has.**
- [`bench/FINDINGS.md`](bench/FINDINGS.md) — the measurement register, one entry per thing
  measured; [`bench/glean-read-path.md`](bench/glean-read-path.md) is the comparison still to
  run.

## Two invariant namespaces (don't conflate them)

- **Engine invariants `I1`–`I15`** — codec, executor/resume, storage, identity, format, and
  derived-bind purity. Indexed in the [registry](website/content/invariants.md).
- **Operational invariants `ops-I1`–`ops-I10`** — lifecycle, single-process ownership,
  reproducibility, the one-write-funnel. Explained in
  [Operations](website/content/operations.md). Always written `ops-Ix` so they are never
  mistaken for the engine `Ix`.

## Install

```bash
cargo add fjord-db                              # the Rust client
dotnet add package Boxops.Fjord.Client          # the .NET client
```

`fjord-db` is a façade over the three crates that do the work — `fjord-client`, `fjord-schema`
and `fjord-wire` — so one dependency is the whole of getting started. The storage layer, the
query engine and the server are internal crates and are not published: a package is what it
takes to talk to a database and read rows back, not the shape of what is answering.

The binaries — `fjord` and `fjord-viewer` — are attached to each release and carry SLSA
provenance naming the workflow that built them:

```bash
gh attestation verify ./fjord --repo boxops-uk/fjord
```

**Linux x86_64.** The store root's lock is POSIX `flock` and the default transport is a Unix
socket, so Windows is out of scope rather than untested.

## Build & test

```
cargo build
cargo test                          # the green suite
cargo test -- --ignored --list      # the invariant coverage ledger
cargo +1.97.1 clippy --all-targets --workspace -- -D warnings
cargo +1.97.1 fmt --all
python3 website/build.py --strict   # the design book builds clean
```

`default-members` is every crate, so the first two mean *everything* without `--workspace`.
The `+1.97.1` matches CI's lint gate, which is pinned so that a clippy release cannot redden a
branch nobody has touched; the test suite runs on `stable`.
