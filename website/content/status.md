---
title: Status & roadmap
description: What is built and guarded, what is not built, what is deliberately deferred, and the one decision still open — read this before assuming a feature exists.
---

Fjord is being taken from prototype to production. This page is the honest inventory. Where
something is built, the repository has a test that says so; where it is not, it is listed as not
built rather than described as if it were.

## Built and guarded

| Area | State |
|---|---|
| **Storage codec** | Order-preserving, self-delimiting, golden-pinned marker table. Heavily property-tested |
| **Storage layer** | A pair of trees per predicate, atomic two-map writes, snowflake ids recovered from the data, format stamp, snapshots released at suspend |
| **Executor** | Three step kinds, one driver, lazy field decode, allocation-free scan, in-band cancellation |
| **Suspend & resume** | A bytes-only cursor with a version, a plan fingerprint and a per-level integrity check — proven against an interruption-schedule generator on both stores |
| **sigla front end** | lex → parse → typecheck → flatten → reorder → `Plan`, round-trippable, span-checked, corpus-gated |
| **The language** | Generators, joins, records, field access, `.value`, constants and folding, aliases, constraints, denials, four comparisons, integer arithmetic, negation, disjunction, `never`, subqueries, references both ways |
| **Schema DSL** | Files, namespaces, imports, canonical form, per-predicate and whole-schema fingerprints, subset-containment compatibility, `schema check` / `fingerprint` / `diff` |
| **Embedded schema** | A database is created against a schema file, embeds it, and is **served from that copy** |
| **Wire protocol** | Frames, streams, handshake with a per-predicate schema claim, four query kinds, paging, profiling, counting, fetch, control frames, cancellation, a fair writer |
| **Ingestion over the wire** | Write streams, blocks, interning of nested references, dedup and deterministic conflict rejection |
| **Parallel ingestion** | Per-key exclusion striped 64 ways; many writers per database, with correctness proven by a racing guard and by wire-level counts |
| **Server** | Unix socket by default, TCP opt-in, per-connection reader task, per-stream tasks, blocking pool, virtual `fjord.db.List` |
| **Client** | Rust client crate — connect, query, page, count, profile, fetch, expand, write, lifecycle |
| **CLI** | `serve`, `create`, `finish`, `list`, `describe`, `query`, `shell`, `schema …`, `db rm` |
| **Shell** | The wire REPL: compiles locally against the schema the server serves, real cursor paging, expansion, profiling |
| **Second implementation** | A C# client, its demo producer, a real Roslyn/MSBuild indexer, and a byte-for-byte golden against the Rust encoder |
| **Viewer** | A code-search site: browse, file view with cross-references, prefix search, symbol pages |
| **Measurement** | Six instruments across seven rungs, and a findings register |

## Not built

| Missing | What it means for you | Gating |
|---|---|---|
| **Ingestion from files** | Facts arrive over the wire from a producer. The file format, block encoding and splitting rule are all defined and shared with the wire path; the pipeline is not wired to a command | Was gated on parallel ingestion, which is now done |
| **Union types** | The schema DSL parses a sum and names `nyi/union`. No `maybe`, `enum` or union-typed field yet | Discriminant encoding, and the freeze that comes with it |
| **Stored derivation** | A derived predicate cannot be *declared*. Derived data is written by hand — which is what four predicates in the sample schema are | The schema DSL (done) plus the re-derivation decision below |
| **Arrays and sets** | A one-to-many is one fact per element. Marker bands are reserved | An open design question, not a missing implementation |
| **`fjord write`, `db backup/restore/verify`, `completions`** | Named in the CLI design, absent from the binary. A Complete database is a directory, so `tar` is the backup | — |
| **Per-predicate statistics** | Nothing feeds a selectivity heuristic, which is why the reorderer does not have one | `finish` is the natural place to record them |
| **Per-stream flow control** | Bounded queues and per-connection backpressure in the meantime | — |
| **A resumable deadline** | A timeout unwinds terminally instead of handing back a cursor | The token cannot represent a mid-descent position |
| **Authentication** | None, by design. The transport is the trust boundary | — |

## The one open decision

**Re-derivation, and what happens to the high-water mark.** It gates stored derivation, and it is
cheapest to answer before that phase writes anything down.

Two things the design states are both true and, together, inconsistent:

- a predicate can be **dropped and replaced wholesale in O(1)** by deleting its two trees, which
  is named as what re-deriving a derived predicate needs;
- [I11](invariants.html#i11): a fact id is **never reused** within a database.

The mechanism is what connects them. The allocator's high-water mark is recovered *from the data* —
the last key in a predicate's identity tree — precisely so no counter can go stale. Delete that
tree and the evidence goes with it: the next write is sequence 1 again, and old ids come back
naming different rows, so any dependent predicate still holding references points at whatever took
their place. Silently.

Two coherent answers, and they are not the same size:

- **Re-derivation produces a new database.** Matches the immutable-artifact philosophy and needs no
  new machinery. It also means a one-predicate fix rebuilds everything.
- **In place, but bounded.** Legal only on a Writable database, and only for a predicate nothing
  already-written references — which in practice means dropping its dependent subtree with it. The
  derivation graph is already topologically sorted for stratified derivation, so the dependency
  information exists; what is still needed is for the high-water mark to survive the drop.

Anything more permissive — re-deriving under live readers — needs persistent generation metadata,
dependent invalidation, and generation-aware cursors and references. That is a great deal more than
"an O(1) tree delete", and the phrase should not be read as promising it.

## Decisions that are settled (so they are not re-litigated)

The project keeps a record of these, because a reversal that is not written down once gets
re-argued forever.

| Decision | Where it landed |
|---|---|
| **Parallel writes to a Writable database** | **Yes**, behind a striped merge frontier. The chain that derived "one writer" from reproducibility was wrong: the hash is a multiset over logical forms, and what needed serialising was the key-to-fact bijection — which now has a mechanism |
| **Per-block commits** | A server flag, **off by default**, gated on a durable id claim. It trades exactly one thing: a crash during ingest may cost the index, never its correctness |
| **What a reference is on the way in** | The **target fact, written inline** — so a producer keeps no book. Stored, it is an id and nothing else |
| **Multiplicity** | **One fact per element** for now, diagnosed by name. An array cannot be prefix-matched, so it is an encoding one-way door as well as a type |
| **Primitives** | Comparisons and arithmetic are **in the language**. Arithmetic is the first thing in sigla to lower a derive step |
| **Intra-row repeated variables** | **Rejected** by name, rather than adding a residual operator nothing else uses |
| **`pattern = pattern` unification** | Scope settled — and most of what was filed as unification turned out not to be it. Binding a row a field already named is an *ordering* question; `X = Y` with both bound is a residual; `X = "a"..` is a *constraint* |
| **Cancellation counts rows examined** | Settled, in the executor |
| **Storage codec vs transport codec** | Two codecs, siblings, sharing no bytes |
| **Schema compatibility** | Subset containment: the only compatible change is adding a predicate |
| **An on-disk format version** | Built — two numbers in database metadata, checked at open |
| **A client never computes a fingerprint** | It **carries** the number and is refused by name if it is stale |
| **Predicate ids** | They belong to the **database**, not to the schema text — which is why a fact block names its predicate rather than numbering it |
| **The `FactRef` marker** | Its own marker, not shared with the integer encoding |

## Two rules about what may change

These are the guardrails that keep the machine reviewable, and they are worth knowing before
proposing a feature.

**A new construct may add a source, a test, a residual operator or a computed-value arm. It may
not add a `Step`.** Those four are additive in the sense that matters: one match arm, no new
control flow, no cursor consequence. A step is a case in the driver *and* a case in the cursor
*and* an obligation to re-prove that resume is exact.

**Additive is not the same as small.** Disjunction, `never`, negation and subqueries were all
"additive" and still needed a phase each, because disjunction extends the resume token. Union types
are additive and freeze their discriminants on disk the moment one is written. Both got acceptance
criteria rather than a bullet.

## Where the roadmap lives

`PLAN.md` in the repository is the living phase tree: what each phase is, what it depends on, and
its current state. The numbers are historical labels rather than positions — the tree is
deliberately out of chronological order, and renumbering was considered and rejected because
hundreds of references across the repository point at these numbers.

| Phase | What | State |
|---|---|---|
| 0–5 | Guards and harness · store · grammar · driver · flatten/reorder · REPL | done |
| 6, 6b | Dynamic derivation · the deferred query surface | done |
| 7a | Wire ingestion: write stream, interning | done |
| 7b | File ingestion | **open** |
| 8 (8.1–8.5) | Schema parsing, identity, imports, embedding | done |
| 8.6 | Union types | **open** |
| 8b | Stored derivation | **open**, gated on the decision above |
| 9 (a–f) | Operations: lifecycle, runtime, client, CLI, shell | done |
| 10 | Capacity: measure it | done |
| 11 | The code-search site, and what it took | done |
| 12 | Parallel ingestion: the striped merge frontier | done |
| 13 | Fjord against Glean, on one corpus | part done — the write paths are measured and within 8%; the read-path comparison is planned and not yet run |
