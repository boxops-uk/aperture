---
title: Performance
description: The measurement method, the capacity target every number is read against, and what has actually been measured — with the two findings that changed the design.
---

The project separates the **method** (how a number is held to evidence, and what target it
is read against — this page) from the **register** of what has actually been measured —
`bench/FINDINGS.md` in the repository, which is deliberately a history: a number is only
worth reading against the tree that produced it.

:::note Ratios travel, absolutes do not
Every absolute below is from one box: 8 cores, 32 GB, Linux 6.8, release build. Read the ratios
and the shapes; re-measure the absolutes on your own hardware.
:::

## The target

Written down to be argued with. It is derived from what has been measured rather than from what
anyone needs — and the day somebody states a real requirement, it is what that requirement
replaces.

| | Target | Where the number comes from |
|---|---|---|
| **Corpus** | 20 M facts, one repository, ~30 k files | `dotnet/runtime`'s `src/` tree is 18.2 M facts across 22 predicates — 1.8 GB unsealed, 728 MB once sealed |
| **Population** | 1,000 concurrent users per instance | An order of magnitude below where the code-search mix saturates, so the target has headroom |
| **Mix** | The code-search workload: prefix search, paged to 50 | The traffic a product actually sends; a generic mix is not |
| **Interactive latency** | p50 < 25 ms, p99 < 250 ms | Measured p50 is 3.1 ms at 2,048 users — an 8× margin for hardware that is not this box |
| **Freshness** | A full re-index, not an append | A Complete database is immutable, so freshness is an indexing-throughput question |

What is deliberately **not** targeted:

- **Unbounded queries.** A whole-predicate scan of the line table is 8.6 M rows and always will
  be. Bounding a result is the client's job — `--limit`, `:more`, and a paged UI.
- **Write throughput under concurrency.** Not because the design forbids concurrent writers (it
  does not — a Writable database takes many), but because indexing is a build-time cost measured
  in hours and there is no target yet — and one must not be back-filled from a projection.
- **Anything absolute on this box.**

## The ladder

Each rung is a narrower surface than the one above it, and the point of the arrangement is
**attribution**: a regression at the top is only actionable if you can say which rung it appeared
at.

```text
S0  corpus       a real .NET index at a dialed size, plus a synthetic control
 │
S1  executor     in-process, real store, no compile, no wire, no async runtime
S2  compile      the front end alone — the per-query floor
S3  store        raw scan and point — the floor under S1
 │
S4  session      server machinery, in-process socket, one connection
S5  round trip   one connection, the whole latency budget
 │
S6  population   N overlapping users, mixed workload, think time
S7  soak         hours at a sustained rate; leaks, drift, disconnect storms
```

| Instrument | Rung | Isolates |
|---|---|---|
| `examples/engine.rs` | S1–S3 | The engine with everything else taken away |
| `examples/breakdown.rs` | S4 | The fixed per-query cost, by subtraction |
| `examples/loadgen.rs` | S5 | One connection, the whole round trip |
| `examples/soak.rs` | S6–S7 | A mixed population, and steady state over hours |
| `examples/codesearch.rs` | S6 | The product's own traffic rather than a generic mix |
| `examples/ingest.rs` | write | The write path per layer: commit, resolve, decode |

One statement of the workloads is shared across rungs, so a number from one can be compared with
a number from another. That mattered: before it, the load generator sought a key computed as
`files / 2`, which exists in the corpus it seeded itself and in no real index — so pointing it at
a checkout measured a **miss** and called it a seek.

## What has been measured

### The executor's floor is ~330–430 ns per row, and flat in database size

| Workload | Rows | Per row | Rows/s |
|---|---|---|---|
| scan files | 32,710 | 401 ns | 2,495,088 |
| scan modules | 36,192 | 385 ns | 2,597,765 |
| scan decls | 888,177 | 432 ns | 2,313,953 |
| scan refs | 4,879,151 | 434 ns | 2,306,705 |
| scan lines | 8,583,810 | 360 ns | 2,779,286 |

**262× the rows, and nanoseconds per row move by less than 20%, with no trend.** Underneath it,
the raw store scan is 233–426 ns/row over the same range, so the executor adds roughly 25–50% to
what the storage engine charges.

Two more from the same run: a plan with **no steps** (`X where X = 42`, one row, zero rows
examined) costs 327 ns — which is under 0.2% of what a query costs over a socket. And a **fetch**
through a reference costs ~2.6 µs per row, against a bare point read priced at 1.7–2.1 µs: the
fetch is essentially the point read, and the machine adds little.

### Compilation is 4–14 µs, and linear

There is no plan cache: every query is parsed, typechecked, flattened and reordered afresh.

| Conjuncts | 1 | 2 | 4 | 8 | 16 | 32 |
|---|---|---|---|---|---|---|
| Compile | 5.0 µs | 7.2 µs | 12.4 µs | 23.4 µs | 43.8 µs | 83.3 µs |

~2.5 µs fixed plus ~2.5 µs per conjunct, with no term that grows faster — and independent of
corpus size, as it must be, because the compiler never touches the store. Against the ~211 µs
per-query floor over a socket, a plan cache would buy back **2–7%**: real, measurable, and the
smallest term in the budget.

### Row encoding costs 1.5×, and the wire above it another 3.6×

| Workload | Executor alone | + project & encode | Tax |
|---|---|---|---|
| scan files | 2,476,335 row/s | 1,637,046 row/s | 1.5× |
| scan decls | 2,275,416 row/s | 1,515,723 row/s | 1.5× |
| project record | 2,293,536 row/s | 1,093,153 row/s | **2.1×** |
| wide row | 2,110,274 row/s | 1,032,222 row/s | **2.0×** |

Of the ~5.4× between the executor and the socket, 1.5× is the row encoding and **3.6× is
everything above it** — one frame each, the outbound queue, the socket, and the client's own
decode. The two workloads that cost twice rather than 1.5× are the two that **build a record per
row**: projection *shape*, not row count, is what makes a row expensive to hand out.

This is why `--count` and `--format count` exist: same plan, same executor, and the part that
costs is thrown away.

### The mix decides capacity, by two orders of magnitude

A generic mixed population — 80% point lookups, 15% small scans, 4% full scans, **1%
whole-database joins**:

| Class | Share | Service time | Share of the CPU |
|---|---|---|---|
| point lookup | 80% | 193 µs | 0.3% |
| small scan | 15% | 1.8 ms | 0.5% |
| full scan | 4% | 70.9 ms | 5.1% |
| **join, whole database** | **1%** | **5.20 s** | **94%** |

Capacity is ~67 q/s with that 1% in the mix and **~1,900 q/s without it**. So a bound on
concurrent users is only meaningful with the mix beside it — and the practical lesson for anyone
building on this is that the unbounded query is the whole problem.

A think-time sweep, rather than arithmetic:

| Users | Offered | Achieved | Point p50 | Point p99 | CPU |
|---|---|---|---|---|---|
| 512 | 51 q/s | 41 q/s | 1.7 ms | 12.3 ms | 49% |
| 1,024 | 102 q/s | 61 q/s | 2.6 ms | 16.5 ms | 83% |
| 2,048 | 205 q/s | 64 q/s | 4.4 ms | 20.7 ms | 92% |

:::note A trap worth knowing
**Achieved-below-offered is not saturation.** At 512 users the server was at 49% CPU and still
"missed" 19% of the offered rate — because a user waiting inside a five-second join is not issuing
anything. When a class's service time exceeds the think time, offered load is fiction and the
honest reading is the CPU column beside it.
:::

### The product's own traffic: ~6,000 q/s

`examples/codesearch.rs` models a stated product — prefix search, paged to 50, and **no search
term means no query**, so the UI never asks for an unbounded scan.

| In flight | Achieved | Typeahead p50 | p99 | CPU (generator / server) |
|---|---|---|---|---|
| 8 | 3,580 q/s | 2.3 ms | 5.5 ms | 88% (33 / 54) |
| 32 | 4,676 q/s | 6.7 ms | 20.8 ms | 98% (41 / 57) |
| 128 | 6,094 q/s | 24.5 ms | 47.7 ms | 98% (42 / 57) |

**~6,100 q/s against 67 for the generic mix — ninety times more**, and all of the difference is
the one class a search UI does not have. The generator is a co-equal consumer here (42% of the
box against the server's 57%), so per query the *server* spends 0.75 ms of CPU — ~10,700 q/s if
the load came from another machine.

In users: 2,048 users at a 3-second think time is **16% of this box** at a p50 of 3.1 ms.

### Page size is a dial, not a free choice

The obvious guess — that a page below the 256-row chunk is free, since the executor computes a
whole chunk anyway — is **wrong**:

| Page | Rows returned | Typeahead p50 | Achieved |
|---|---|---|---|
| 50 | 48 | 6.1 ms | 5,118 q/s |
| 100 | 93 | 7.1 ms | 4,489 q/s |
| 256 | 226 | 10.5 ms | 3,274 q/s |
| 500 | 417 | 14.4 ms | 2,508 q/s |

Roughly linear in rows **delivered**, because the dominant per-row cost is the framing, socket and
client decode — and cancelling stops paying it. Page 100 costs 12% of throughput against page 50;
page 256 costs 36%. Bigger pages are cheaper per row and dearer per query.

### Steady state holds

Fifty minutes, 145,582 queries, no drift. Across ~900,000 paged queries the server's memory went
756 → 792 → 797 MB — a high-water mark, asymptotic, not a per-query bleed.

### Ingest is ~5.2 k facts/s, and three quarters of the work was re-reading

Read off the indexer's own counters on a 25 M-fact index:

| | |
|---|---|
| Facts created | 25.0 M |
| Facts *interned* to create them | **94.9 M** — so 73.6% of the work was re-reading something already present |
| Point reads per intern | 2 (the index for the id, the identity map to compare the value) |
| Wall clock | 4,828 s ⇒ **5.2 k facts/s** |

The write rung then priced the layers separately, in process:

| Layer | Facts/s | Interns/s | Reads per fact | Cache hit |
|---|---|---|---|---|
| `commit` only | 321,968 | — | 0.00 | — |
| `create` | 166,914 | 772,750 | 1.00 | 78.4% |
| `dedup:warm` | — | 1,698,325 | 0.00 | 100% |
| `dedup:cold` | — | 1,309,655 | 0.22 | 78.4% |

Read the **differences**: committing is 41% of interning (which is what `--commit-per-block`
trades), and the lookup cache is worth 23% of a resolve pass.

### Many writers is correct, and not yet faster

Four write streams against one database produce **exactly** the counts one writer does — the
wire-level version of the in-process property that writer count and write order do not change the
database. It was not faster on a small corpus, and could not have been: the writer was never the
ceiling there. The number that says to add writers is the *queueing* counter; while it is near
zero, one writer is the right answer.

## Two findings that changed the design

Most of the register is measurement. Three items were acted on, because leaving them would have
made every later number a measurement of a bug. Two are worth repeating here because they change
how you *use* Fjord.

### Field order is the largest effect measured anywhere

The same join shape over two predicates:

| Workload | Rows | Examined | Examined per row | Per row |
|---|---|---|---|---|
| Join on a **leading** field | 8,583,810 | 8,616,520 | 1.004 | 393 ns |
| Join on a **middle** field | 2,000 (capped) | 112,548,829 | **56,274** | 26.0 ms |

The second cannot narrow, so it re-reads all 888,177 declarations once per module — 32 billion
rows to completion. `--profile` tells you (`full scan` on the inner step); nothing warns you when
you write it.

The two predicates involved had been declared in alphabetical order out of habit, so the key led
with a line number and a column. They are declared deliberately now, and the sample schema says
per predicate which question its order answers. This is the reason
[Schema language](schema-language.html#field-order-is-the-index-design) puts field order first.

### Sealing merges, because an unmerged tree seeks up to 180× slower

Paging costs one seek per page, and on a freshly-ingested database that seek was measured at up to
600 µs. Ingestion leaves each tree in whatever shape the write order produced, and nothing
reclaimed it afterwards.

`finish` now performs a major compaction before it walks the facts to compute identity — so the
byte count it records is the artifact's, and the shape every future reader pays for is the final
one. The artifact also roughly halves on disk.

## How to measure your own

```bash
./scripts/bench.sh                        # create, serve, seed, measure — one command
FILES=100000 CONNS=16 ./scripts/bench.sh

cargo run --release --example engine -- --store <instance> --layer executor
cargo run --release --example codesearch -- --data-dir <root> --users 256 --page 50
cargo run --release --example ingest
```

Three rules the method insists on, and they are worth borrowing:

- **Release builds only.** A debug build of the executor is not the thing being measured, and the
  difference is not a constant factor you can divide out.
- **Every timed run must reproduce the row count and the per-step examined counts** of an
  unmeasured warm-up, or the run aborts. A throughput number for a query that answered differently
  is not a number.
- **Say what a measurement does not cover.** Every finding in the register carries its own caveat
  — the corpus size it was taken at, which rung it belongs to, and what it would take to make it
  more than a projection.
