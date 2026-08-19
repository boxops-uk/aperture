# Changelog

Fjord DB. Dates are the release date; `0.0.x` is a pre-release series and the on-disk format
is not yet promised to be stable across it.

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
| **Union types** | A schema parses a sum and names it unimplemented. No `maybe`, no `enum`, no union-typed field |
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
