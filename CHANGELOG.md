# Changelog

Fjord DB. Dates are the release date; `0.0.x` is a pre-release series and the on-disk format
is not yet promised to be stable across it.

## Unreleased

**The documentation is one body now, and it is tested.** The design book is the website
(`website/` — `python3 website/serve.py`), verified claim by claim against the tree; its
invariants page is the canonical registry. `AGENTS.md` is the working contract for
contributors, `PLAN.md` is a roadmap rather than a phase tree (with the auth design and the
settled-decisions record inside it), and the two Glean documents merged into `docs/glean.md`.
CI builds the site strictly and runs `scripts/check-docs.py`, which fails on a broken link, an
invariant citation the registry does not declare, a reference to a retired document, or a
build-plan phase number in code — each a way the documentation actually went stale once.

**The design book is published, with the engine running inside it.** Every push to main
deploys the interactive site — `web/`, the same pages with `fjord-engine` compiled to
WebAssembly, so a demo of the lexer *is* the lexer — to <https://boxops-uk.github.io/fjord/>
(after the tests, the drift gate, and the bundle being driven in a real browser), and every
release carries that bundle as an attested `fjord-docs-site.tar.gz` beside the binaries; the
site's getting-started page links the release downloads back. The generated copy
(`python3 website/serve.py`) is what reads with no toolchain, and the second renderer the
smoke check holds the first one to.

**Every error state is demonstrated by a test** that provokes it and asserts it at its
contract layer, fjall/OS bubbles excepted; the engine's corpus gate now covers every
diagnostic code, not only the deferrals. Comments across the workspace state the risk they
guard rather than the history of how the code got there.

## 0.0.1 — unreleased

The first published artifact. Everything below is *what is there*, and the
[gaps](#what-is-not-in-it) are as much of the release as the features.

### Install

```bash
cargo add fjord-db                              # the Rust client
dotnet add package Boxops.Fjord.Client          # the .NET client
```

Binaries — `fjord` and `fjord-viewer` — are attached to the release and carry SLSA
provenance:

```bash
gh attestation verify ./fjord --repo boxops-uk/fjord
```

**Linux x86_64.** The store root's lock is POSIX `flock` and the default transport is a Unix
socket, so Windows is out of scope rather than untested. Other Unix targets are expected to
work and are not built or tested by CI.

### What is in it

- **An immutable fact database.** A database is created against a schema, written to, sealed,
  and thereafter only read. Facts are typed records identified by a `FactId`, grouped by
  predicate, stored in an LSM.
- **sigla**, a typed Datalog-flavoured query language: generators, joins, records, field
  access, constants and folding, aliases, constraints, denials, four comparisons, integer
  arithmetic, negation, disjunction, `never`, subqueries, and references followed in both
  directions.
- **A suspendable executor.** A query suspends to a bytes-only cursor and resumes exactly,
  releasing its snapshot at every chunk boundary — so a page held for an hour costs what one
  held for a millisecond does.
- **A schema language.** Files, namespaces, imports, a canonical form, per-predicate and
  whole-schema fingerprints, subset-containment compatibility, and `schema check` /
  `fingerprint` / `diff`.
- **Union types**, with **explicit append-only discriminants** — `{ num : int = 3 | text :
  string = 0 }`. A tag is written down rather than taken from the position, because a derived
  one renumbers the moment an alternative is inserted and every value already written then
  reads as a different alternative. Written and matched as `{alt = p}`, selected as `X.alt?`,
  and where the union is a leading key field, matching an alternative is a **seek** rather than
  a filter.
- **A wire protocol**, with a second implementation in C# that shares no code with the Rust
  one and a byte-for-byte golden test between the two encoders.
- **Parallel ingestion.** Many writers per database, behind per-key exclusion striped 64 ways.
- **A server**, a **client**, a **command-line tool**, and a **code-search site** built on
  nothing but the client.

### What is not in it

Stated because a missing feature discovered by a user is worse than one written down.

| Missing | What it means for you |
|---|---|
| **Authentication** | None, by design at this stage. The transport is the trust boundary: the server binds a Unix socket, TCP is opt-in per invocation, and access control belongs to a gateway in front |
| **`maybe` and `enum`** | Both are sugar over a union, which *is* there — but each needs a naming decision that enters the schema fingerprint, so both still parse and report themselves. Write the union out |
| **Stored derivation** | A derived predicate cannot be *declared*. Derived data is written by hand, which is what four predicates in the sample schema are |
| **Ingestion from files** | Facts arrive over the wire from a producer. The file format is defined and the pipeline is not wired to a command |
| **Arrays and sets** | A one-to-many is one fact per element |
| **`fjord write`, `db backup`/`restore`/`verify`, `completions`** | Named in the design, absent from the binary. A sealed database is a directory, so `tar` is the backup |
| **Per-predicate statistics** | Nothing feeds a selectivity heuristic, which is why the reorderer has none |
| **Per-stream flow control** | Bounded queues and per-connection backpressure in the meantime |
| **A resumable deadline** | A timeout unwinds terminally rather than handing back a cursor |

Two operational facts that are easy to meet and are not in that table because they are
properties of what *is* built:

- **A `Writable` database is never merged.** Trees are compacted at `finish`, so a long-lived
  ingest-then-query workflow pays unmerged-LSM seek cost until it is sealed — up to two orders
  of magnitude on a page seek. Seal before you measure read performance.
- **The interning lookup cache is a fixed budget per open database** (~256 MiB, two
  generations of 128 MiB) with no operator dial. It is measured at its ceiling at 18.3M facts
  and untested above that.

### Notes for anyone who has been tracking `main`

- **Unions landed (8.6), and nothing else moved with them.** The marker table gained `0x52`,
  *appended* — the eleven markers below it are unchanged, so every database already written is
  read by exactly the bytes that wrote it. The wire's descriptor and value tables gained a tag
  each, also appended, so an older peer meets one and says so rather than mis-reading what
  follows. `schemas/code.sigla` is deliberately untouched: a union there would move its
  fingerprint and the constants two .NET clients carry, and that is a flag day with nothing to
  do with unions working.
- **There is no built-in schema.** `fjord create` requires `--schema <file>`; a server carries
  no data schema of its own, and a database that embeds no schema copy is listed rather than
  served. `schemas/code.sigla` is a sample rather than a default.
- **`fjord shell` requires a database.** The embedded demo, and the `example/` corpus it was
  seeded from, are gone.
- **`fjord finish` used to seal against the tool's built-in schema** regardless of what the
  database embedded, which computed the content identity over misread rows for any database
  built against another schema. It reads the embedded copy now.
- The command-line tool moved to `crates/fjord-cli`, and `Connection::control` no longer takes
  a schema.
- **The .NET namespace is `Boxops.Fjord.Client`**, matching the package id — so
  `dotnet add package Boxops.Fjord.Client` is followed by `using Boxops.Fjord.Client;` and
  there is one name rather than two. The projects and the solution are renamed to match.
