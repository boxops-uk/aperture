# Fjord

**Fjord DB stores facts and answers questions about them.** A fact is a typed record — *this
file declares this function at this line*; *this reference points at that declaration* — and a
question is a sentence in **sigla**, its typed, Datalog-flavoured query language:

```sigla
N where F = code.File "src/lib.rs"; code.Decl {file = F, name = N, line = _}
```

*The name of every declaration in `src/lib.rs`.* That is a join through a fact reference, and
over the sample database in the docs it answers three rows.

It is **embedded**: a database is a directory, and reading one is a library call rather than a
service. It is **immutable**: built once against a schema, sealed, and thereafter only read —
the single decision that makes the rest tractable, because snapshots become trivial, resume
tokens become plain bytes, and parallel ingestion becomes fearless. Queries compile to a
nested-loop plan run by a suspendable virtual machine, so a page of results held for an hour
costs what one held for a millisecond does.

Underneath: facts in an LSM ([fjall](https://github.com/fjall-rs/fjall)) under an
order-preserving codec, and a language that is a faithful subset of Glean's Angle at the core
and its own thing past that ([what is inherited and what is not](docs/glean.md)).

## Try it without installing anything

The design book **runs the engine**. It is compiled to WebAssembly and shipped inside the page,
so an example that shows what the lexer does is asking the lexer — and every one of them is
editable:

- **[The workbench](https://boxops-uk.github.io/fjord/playground)** — one query and every view
  of it at once: tokens, parse tree, types, the plan, and the executor stepping across real
  rows while the registers fill.
- **[Getting started](https://boxops-uk.github.io/fjord/getting-started)** — the same thing on
  your own machine, from a download to an answered query, in eight steps.

## Install

Take the binary. Linux x86_64, carrying SLSA provenance that names the workflow which built it:

```bash
curl -LO https://github.com/boxops-uk/fjord/releases/latest/download/fjord
chmod +x fjord
gh attestation verify ./fjord --repo boxops-uk/fjord   # optional, and worth it once
./fjord --help
```

`fjord` needs **glibc 2.34 or newer** — Ubuntu 22.04, Debian 12, RHEL 9 and later. The
`fjord-x86_64-linux-musl` build beside it is the same code linked statically, with no floor at
all, for an older distro, Alpine, or a `scratch` container. `fjord-viewer` — the code-search
site, built on nothing but the client — ships both ways too.

**Linux x86_64 only.** The store root's lock is POSIX `flock` and the default transport is a
Unix socket, so Windows is out of scope rather than untested.

To talk to a database from your own program:

```bash
cargo add fjord-db                              # Rust
dotnet add package Boxops.Fjord.Client          # .NET
```

`fjord-db` is a façade over the three crates that do the work — `fjord-client`, `fjord-schema`
and `fjord-wire` — so one dependency is the whole of getting started. The storage layer, the
query engine and the server are internal crates and are not published: a package is what it
takes to talk to a database and read rows back, not the shape of what is answering.

## The design book

**The book is the design of record** — the architecture, both languages, the wire protocol,
operations, and every invariant with the guard test that pins it:

**<https://boxops-uk.github.io/fjord/>**

The fastest route to *what must I not break here* is the [invariant
registry](https://boxops-uk.github.io/fjord/invariants). Every release also carries the book as
an attested `fjord-docs-site.tar.gz`, servable from the root of any static host.

## Where it stands

> **Status: `0.1.0`, a pre-release.** Built and guarded: the storage codec and the fjall store,
> a suspendable executor that resumes exactly, the sigla front end end to end — text to `Plan`
> to rows, joins *through fact references* included — the schema language, union types and
> schema identity, the wire protocol with a second implementation in another language, parallel
> ingestion, a server, a client, the command-line tool, and a code-search site built on nothing
> but the client. The engine compiles to WebAssembly, and the book above is that build.
>
> **Not built:** authentication, stored derivation, ingestion from files, arrays and sets,
> per-predicate statistics. [`CHANGELOG.md`](CHANGELOG.md) is the full inventory — including,
> deliberately, what each release does not contain — and [`PLAN.md`](PLAN.md) is the roadmap.
>
> `0.x` is a pre-release series: the on-disk format is not promised stable across its minor
> versions, and what *is* promised inside one is that nothing already written is renumbered.

## Working on it

Start with [`AGENTS.md`](AGENTS.md) — the **working contract**: the conventions, the traps, the
testing method, and where every other kind of truth lives. Read it before changing anything.

```bash
cargo build
cargo test                          # the green suite
cargo test -- --ignored --list      # the invariant coverage ledger
cargo +1.97.1 clippy --all-targets --workspace -- -D warnings
cargo +1.97.1 fmt --all
python3 website/build.py --strict   # the book builds clean, as CI requires
(cd web && npm run smoke)           # the book's own engine, driven in a browser
```

`default-members` is every crate, so the first two mean *everything* without `--workspace`. The
`+1.97.1` matches CI's lint gate, which is pinned so that a clippy release cannot redden a
branch nobody has touched; the suite itself runs on `stable`.

The book's pages are Markdown in [`website/content/`](website/content/), rendered by two
independent renderers — the generator in [`website/`](website/README.md), and the interactive
site in [`web/`](web/README.md), which is the one that publishes — with a check that compares
them page for page.

### Two invariant namespaces (don't conflate them)

- **Engine invariants `I1`–`I15`** — codec, executor/resume, storage, identity, format, and
  derived-bind purity. Indexed in the
  [registry](https://boxops-uk.github.io/fjord/invariants).
- **Operational invariants `ops-I1`–`ops-I10`** — lifecycle, single-process ownership,
  reproducibility, the one-write-funnel. Explained in
  [Operations](https://boxops-uk.github.io/fjord/operations). Always written `ops-Ix` so they
  are never mistaken for the engine `Ix`.

### Also worth knowing

- [`docs/glean.md`](docs/glean.md) — where every idea came from, and what each system can be
  asked to do, spends, and charges. **Read it before proposing a feature Glean has.**
- [`bench/FINDINGS.md`](bench/FINDINGS.md) — the measurement register, one entry per thing
  measured; [`bench/glean-read-path.md`](bench/glean-read-path.md) is the comparison still to
  run.
