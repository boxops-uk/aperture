# Phase 10 — Capacity: measure it

> [Aperture design book](../README.md) · **proposed phase plan**, not yet folded into
> [`PLAN.md`](../PLAN.md). Written on the per-phase template so it can be moved there
> whole once accepted.

> **Status — read this before the rest.** This was drafted against the tree at
> `d57f45167` and two commits landed underneath it: `ce377eb81` (committing
> `examples/breakdown.rs`) and `87d3055b2` (`examples/soak.rs` plus the whole
> `Aperture.Indexer`). **S6 is therefore largely built, not proposed.** `soak.rs`
> already does the weighted mix, per-client think time, per-class percentiles,
> offered-vs-achieved saturation reading, an error count, and `--stalled` for paused
> readers — and its header already states the generator-shares-the-machine caveat §5
> raises. Treat §3's S6 as a description of what exists plus the gaps below it, and
> S7 as `soak --seconds` extended.
>
> **S1–S3 are now built and run.** `examples/engine.rs --layer executor|compile|store`
> is the instrument, and it was run against a real index — `dotnet/runtime`'s whole
> `src/` tree, **18,176,899 facts**, built by `Aperture.Indexer`. The results, and the
> three findings they turned up, are in [`bench/FINDINGS.md`](../bench/FINDINGS.md):
> **F7 answered** (paging costs one seek per page — 4–12 µs, ~10% of a 256-row chunk),
> **F3 answered** (compile is 4–14 µs, linear in query size, 2–7% of the round-trip
> floor), and two things that were on nobody's list: **nothing ever compacted**, which cost
> up to 180× on a seek and 2× on disk — now **fixed, in `finish`**, with two guards and the
> artifact measured at 1.7 GB → 853 MB — and **a key's field order**, which the schema
> declares and `code_index` happens to declare alphabetically, decides whether a join seeks
> or rescans, at 56,274× the rows examined. F5 turns out not to be reachable from
> the query side at all, because a fact's *value* cannot be read by a query.
>
> **S4a, S6 and S7 have now been run against the sealed index too**, on 8 cores. The
> population sweep goes to 2048 clients: capacity plateaus at **~67 q/s for the standard mix
> and does not collapse**, zero errors anywhere, and the cheap query stays on the right side
> of the expensive one by a factor of 7,400. A fifty-minute soak at a sub-knee rate — 145,582
> queries — shows **no drift at any percentile**. **F1 is confirmed but bounded** (~3.5 kB a
> query, retained for a connection's life and reused after it closes), **F4 is confirmed and
> misattributed** (the row encoder is 1.5×, the framing and transport above it 3.6×), **F2 is
> refuted**, and **F8 behaves exactly as predicted**. `soak` grew sampled pivots (it computed
> them from `--files`, which only ever worked against a corpus it seeded itself) and a
> CPU-attribution line, so a flat achieved rate can be read as the server's rather than the
> generator's.
>
> **Task 10f's counters are built**, and deliberately only counted: `ServerStats` is a
> struct of relaxed atomics on the `Registry`, wired through connections, stream tasks,
> queries, chunks, rows, blocking dispatches and queue-full waits, with gauges held by a
> `Drop` guard so they are right on every exit path. There is **no exporter, no endpoint
> and no stats file** — exposing them is a separate decision with an operational cost, and
> a `/metrics` listener in particular is a second port on a server whose `ops-I10` safety
> argument rests on binding being default-closed. The durable home the design already names
> — a virtual predicate over the socket that exists — remains the right one, and any
> Prometheus or OTEL exporter belongs on the far side of it, as a *client* rather than a
> feature of the server.
>
> They earned their keep immediately: the leak's regression guard asserts on
> `streams_live`, which is the thing a server owes rather than the mechanism that broke.
> **`--features console`** adds `tokio-console` for the other half of that problem — seeing
> where a task is parked rather than counting how many there are — off by default, and
> needing `--cfg tokio_unstable` so it cannot be turned on by accident.
>
> What remains genuinely unbuilt: **S0** (the catalogue grew to seventeen workloads with
> sampled pivots, but it lives in `examples/engine.rs` rather than a shared
> `src/workload.rs`, so `loadgen` and `soak` still state their own), the **scaling curve
> across corpus *sizes*** (what is published is one 18M-fact database across predicates
> spanning 142 → 8.58M rows), and **`bench/baselines/<host>.json`**. **F6** is the one hypothesis untouched, and the only one
> that needs a *write* path: every database measured here is `Complete`.
>
> **§5's host description is stale**: this box is now 8 cores / 32 GB / 185 GB free, not
> 4 / 15 / 5.8. The disk constraint that shaped the plan is gone.

**Goal.** Find out whether Aperture DB holds up for a few hundred to ~1000 concurrent
users issuing overlapping queries of mixed complexity — by building a ladder of
measurement surfaces from the executor upward, recording what each one costs, and writing
down the findings. **Measurement only:** no mechanical guards, no fixes.

**Depends on:** Phase 9a–9e (done) for the server and client. S1–S3 depend on nothing
further and can start immediately, in parallel with 9f. S4–S7 want 9f finished, because
`--timeout`, the wire shell and `\more` are part of the surface being measured.

**Design of record:** this file, plus `docs/performance.md` (to be written as task 10b —
the durable method doc, beside [`docs/testing.md`](testing.md)).

**Invariants in scope:** *makes green:* none. *upholds:* all of them — this phase adds no
behaviour on any data path. The only production-code edits are counters and a feature gate.

---

## 1. Why this phase exists

Performance is, in the written plan, a non-topic. `PLAN.md`'s 1489 lines contain no phase,
no target, no acceptance criteria and no cost model for it; the closest is one sentence
deferring file ingestion as "a throughput feature". [Operations
§1](aperture-cli-design.md) admits the hole outright:

> Aperture states no target corpus size, no churn rate and no freshness budget anywhere,
> so `ops-I9` is ultimately a *requirements* question this repo cannot settle on its own.

Two other admissions point the same way: *"there is no cost model"*
([open decisions](open-decisions.md)), and per-predicate statistics are deferred with the
note that `reorder`'s selectivity seam already exists and has no consumer.

Meanwhile a real apparatus has grown with no home. `grep -i -E "loadgen|bench|breakdown"`
over `PLAN.md` and `CLAUDE.md` returns nothing:

| Artifact | What it does | Status |
|---|---|---|
| `examples/loadgen.rs` (568 ln) | End to end over a real socket. Seeds N files × K decls, runs 8 named workloads over C connections, reports p50/p95/p99/max, query/s, row/s, rows examined | committed |
| `examples/breakdown.rs` (509 ln) | Decomposes the ~211 µs per-query fixed cost by subtraction — transport, compiler, `spawn_blocking` hop, mpsc hop, with the residual as the signal | committed |
| `examples/soak.rs` (490 ln) | The weighted mix: N virtual users, think time, per-class percentiles, offered vs achieved, `--stalled` for paused readers | committed |
| `examples/codesearch.rs` | **The product workload.** Prefix search paged to 50–100, terms sampled from the corpus, no unbounded query — ~6,100 q/s against the generic mix's 67 | committed |
| `examples/engine.rs` | **S1–S3.** In-process against a real index: ns/row with `Profile` attribution, the paging comparison taken apart per page, the raw scan/seek/point floor under it | committed |
| `scripts/bench.sh` (71 ln) | create · serve · seed · measure, release-only by construction | committed |
| `iter::Profile` → `PROFILE` frame → `query --profile` | Rows examined per plan step, with a full-scan flag | committed, fully plumbed |
| `clients/dotnet/Aperture.Indexer` + `index-repo.sh` | Indexes a real .NET checkout over the wire; `--max-files` dials the size; reports created/deduped | committed; 18.2M facts indexed |
| [`bench/FINDINGS.md`](../bench/FINDINGS.md) | The register: what was measured, the number, what a fix would cost | S1–S3 entered |

Three things are missing, and they are this phase:

1. **No bottom of the ladder.** Everything measures the whole round trip or a synthetic
   micro-hop. Nothing measures the executor against a real store at scale, so there is no
   **scaling curve** — the one result that could invalidate the target outright.
2. **The concurrency instrument measures the wrong shape.** `loadgen` runs *one workload
   at a time*, closed loop, every connection doing the same thing, no think time. The
   question is a mixed population of overlapping users. The maximum concurrency exercised
   anywhere in the repo today is **8** (loadgen's default); in the test suite it is **3**.
3. **No target.** Written down, it is the thing every number means.

**On the scope decision.** [`docs/testing.md`](testing.md) holds that *"an NFR with no
mechanical guard is an aspiration, not an acceptance criterion"*. This phase deliberately
stops short of guards, so its numbers will decay unless a follow-on turns the deterministic
ones into tests. §8 names exactly which are guardable and how, so that work is cheap when
it is wanted. That is the concern, stated once.

---

## 2. Eight hypotheses, from reading the code

Inspection is not evidence here — that is the project's founding methodological claim. Each
of these is a *prediction* with the rung that settles it and the number that would.

| # | Hypothesis | Where it comes from | Settled by |
|---|---|---|---|
| **F1** ✅ | **Stream tasks leak, per query.** `read_loop`'s `streams: HashMap<u32, StreamHandle>` (`session.rs:316`) has no removal path anywhere in the file; the client's `claim_stream` (`client/connection.rs:528`) never reuses an id. A connection issuing 10k queries leaves 10k parked tokio tasks, each holding `Arc<Session>`, `Arc<Outbound>`, a `CancellationToken` and an `mpsc(2)` buffer, until the *connection* closes — **true, and the mechanism is as described: ~3.5 kB retained per query, growth strictly proportional to queries issued on a connection, so 200k point lookups for one key took the server from 243 MB to 892 MB. It is *bounded*, though — a third such connection added 35 MB where the first added 649, and a realistic population reconnecting between queries retains 58 bytes/query. What it sets is a high-water mark for the busiest connection, not a restart schedule ([findings §7](../bench/FINDINGS.md))** | S7 — RSS and live-task count against **queries issued**, not connections open |
| **F2** ⛔ | **A mid-chunk cancel reports `ErrorCode::Internal`, not a clean end.** `CANCELLATION_STRIDE = 4096` counts rows *examined* (`iter.rs:389`); `CHUNK_ROWS = 256` counts rows *produced*. A selective query trips the stride inside a chunk → `ApertureError::Cancelled` → `ServerError::Execution` (`session.rs:859`) → an ERROR frame, where the design says *"a cancel is an early end, not a failure"*. Under load this is the common case, and no test covers the branch — **refuted: cancelling the most stride-tripping query available (56,274 examined per row produced) returns a clean end, sends no error frame, and leaves the connection usable. Tested through the client API and through `query --limit`** | S4 / S6 — cancel the `denial` workload and read the frame kind |
| **F3** ✅ | **No plan cache.** Every query is parsed, typechecked, flattened and reordered afresh on the blocking pool (`session.rs:577`). At a ~211 µs floor on 4 cores that is a ceiling of roughly 19k q/s whatever the query does — **true, and small: 4–14 µs, 2–7% of the floor, linear in query size ([findings §5](../bench/FINDINGS.md))** | S2 — compile µs as a fraction of the floor |
| **F4** ✅ | **Per-row framing dominates above ~100k row/s.** One `DATA_ROW` frame per row: ~3 allocations, 2 outbound-mutex acquisitions and a `Notify` each (`session.rs:617`, `outbound.rs:90-122`, `rows.rs`) — **confirmed as significant but misattributed: the row *encoder* is 1.5× (2.1× where the projection builds a record), and the framing, socket and client decode above it are a further 3.6× ([findings §9](../bench/FINDINGS.md))** | S4 — row/s with framing against S1 row/s without |
| **F5** ⛔ | **A chunk has no byte budget.** `CHUNK_ROWS` is row-bounded only, so 256 wide rows materialise unbounded memory on a blocking thread (`session.rs:863`). The only byte cap in the system is `MAX_PAYLOAD` = 64 MiB, and it is per frame — **not reachable from the query side: a fact's *value* cannot be read by a query at all, so the widest row buildable is three narrow key fields ([findings §4](../bench/FINDINGS.md))** | S1 / S4 — a wide-row workload, RSS at the chunk boundary |
| **F6** | **The reader head-of-line blocks the whole connection.** `read_loop` *awaits* `handle.inbound.send(..)` on a channel of capacity **2** (`session.rs:353`); a third frame for a busy stream stalls the connection's reader — including the read that would pick up a CANCEL for a *different* stream. `write_blocks` fires every block then `COPY_DONE` without waiting (`client/connection.rs:242`) | S4 / S6 — a ≥3-block ingest against a slow funnel |
| **F7** ✅ | **Paging is not free.** Per 256 rows: two clones, a `spawn_blocking` dispatch, a **fresh fjall snapshot**, and `Executor::resume` replaying **one seek per plan level** (`iter.rs:1116`) — deliberately uncounted by `Profile`. A 1M-row query is ~3,900 of each — **true; the snapshot is free (0.1 µs) and the replayed seek is all of it: 4–12 µs a page, ~10%. On an *uncompacted* store the same seek costs up to 790 µs, +729% ([findings §1](../bench/FINDINGS.md))** | S1 — the same plan straight through vs suspended every 256 rows |
| **F8** ~ | **No admission control of any kind.** No connection cap, no query timeout, no max rows, no concurrency limiter. tokio defaults apply: **4** worker threads (this box), **512** blocking threads, an **unbounded** submission queue. 1000 in-flight queries means 512 running and the rest queued invisibly — latency, never rejection — **observed exactly so: 2048 connections accepted without complaint, nothing ever refused, zero errors, and the queue showed up as the expensive class's p50 rising from 43 s to 315 s while the cheap class stayed under 101 ms** | S6 — the latency distribution at the knee |

Two more findings from reading that need no rung, recorded so nobody re-derives them:

- **Write load does not scale with connections, by design.** One writer mutex per database
  held *across* the ingest (`session.rs:518`), and `put_fact` does one point read plus one
  fjall batch commit per fact. Adding writer connections adds queueing, and a waiting
  writer parks its connection's reader (F6). `loadgen` seeds on one connection, correctly.
- **`remove` under load is essentially always refused** — `Arc::try_unwrap` is the liveness
  test (`registry.rs:229`), so with N sessions bound there are N+1 references. Expected.

---

## 3. The ladder

Each rung is a separate measurable surface, narrow at the bottom and widening upward. Each
ends in a repeatable instrument, a baseline in `bench/baselines/<host>.json`, and its
hypotheses answered.

```
S0  corpus       real .NET indices at dialed size, plus a synthetic control
 │
S1  executor     in-process, real FjallDb, no compile, no wire, no tokio
S2  compile      Compilation::plan alone — the per-query floor
S3  store        fjall scan/point — the floor under S1
 │
S4  session      server machinery, in-process socket, one connection
S5  round trip   loadgen --connections 1 — the latency budget
 │
S6  population   N overlapping users, mixed workload, think time, sweep to 1000
S7  soak         hours at a sustained rate; leaks, drift, disconnect storms
```

---

### S0 — A corpus you can dial, and one that is real

Today the only large-store generator is `seed()` buried inside `examples/loadgen.rs:202`,
at one shape and one skew. Every rung needs the same data, at a size it can afford, with
**known** selectivity — otherwise a number does not say what it measured.

**Real data is the primary corpus, and it is already dialable.**
`clients/dotnet/index-repo.sh <checkout> [db] --max-files N` indexes a .NET checkout over
the wire into the built-in schema — six source predicates plus a line table, seven
build-layer ones and eight over the declaration graph (`src/code_index.rs`), of which
`src.Ref` and `src.Line` are the two that reach seven figures on a real checkout.
`--max-files`
is the dial, so the **scaling curve runs on real data** rather than on uniform synthetic
rows, which flatter seeks and understate cache pressure. Index enough checkouts to reach
each size band; the indexer already reports `created` / `deduped`, which is the interning
cost F-none-of-the-above but worth recording per band.

**A synthetic control stays**, extracted from `loadgen::seed` into a library module, for
two things real data cannot give: exact expected row counts for a workload, and sizes
beyond what checkouts provide.

**New library module `src/workload.rs`** (root crate, exported from `src/lib.rs` beside
`code_index` — which is `loadgen`'s own stated reason for living where it does):

```rust
pub struct Corpus { files, decls_per_file, refs_per_decl, seed, .. }
impl Corpus { pub fn facts(&self) -> impl Iterator<Item = WireFact> }

pub struct Pivots { /* seek keys, prefixes, names — sampled from whichever corpus loaded */ }
pub struct Workload { pub name: &'static str, pub focus: String,
                      pub expected_rows: Option<u64>, pub expected_examined: Option<u64> }
pub fn catalogue(pivots: &Pivots) -> Vec<Workload>
```

Two points carried from the house idiom:

- **A workload states the rows it answers with**, exactly as `aperture_engine::corpus`
  makes a `Supported` entry carry its rows. A run returning a different count did not
  measure what it thought, and says so instead of printing a throughput figure.
- **Pivots are sampled, not computed.** `loadgen` builds its seek key as `files / 2`
  (`loadgen.rs:264`), which only works for the synthetic corpus. Sampling lets the same
  catalogue run against Roslyn.

**Grow the catalogue past today's eight.** Nothing exercises `src.Ref` (seven figures on a
real checkout), `src.SearchByName` (the query a person actually types), `src.Import`
(module→module joins), a **wide row** (F5), or a **high-fanout join**.

### S1 — The executor alone, against a real store

New: `examples/engine.rs --layer executor`. In-process `FjallDb`, plan compiled once
outside the loop, `Executor::enumerate_profiled` driven directly. No tokio, no wire, no
session.

Per workload × corpus size:
- ns/row and row/s, with `Profile` attribution (examined vs produced) beside every number
- **the scaling curve** — does ns/row stay flat as the DB grows 10×? A seek must be
  O(log n), a scan O(n). This is the result that decides whether the target is reachable
- **the cost of paging (F7)** — the same plan run straight through against the same plan
  suspended every `CHUNK_ROWS` and resumed, which is what the server actually does. Nobody
  has this number and it is paid on every query
- RSS at the chunk boundary for the wide-row workload (F5)

Reuse rather than rebuild: `aperture_engine::fixtures::{collect_rows, count_rows,
run_with_suspends}` (`fixtures.rs:32/59/91`) already drive plans, and `run_with_suspends`
*is* the paging comparison. `FrozenStore` (`store/fixtures.rs:271`) is the allocation-free
control. These sit behind `feature = "proptest"`, which the root manifest already enables
for dev-dependencies — an `examples/` target links dev-deps, so no manifest change.

### S2 — Compilation, alone

`examples/engine.rs --layer compile`. `Compilation::plan` over the catalogue and over
generated queries of increasing size. Answers F3 with a fraction: how much of the fixed
cost is the compiler, and does it grow with query size or with corpus size.

**Overlaps `examples/breakdown.rs`.** Read that file first; either call into it or restrict
S2 to the scaling-with-query-size question breakdown does not ask.

### S3 — The store floor

`examples/engine.rs --layer store`. Raw `FactStore::scan` / `point` throughput underneath
S1, so an S1 regression is attributable to the engine rather than to fjall. Also LSM shape
(freshly ingested vs compacted) and predicate-count effects — a keyspace pair costs ~30 ms
to create (`store.rs:231`) and the built-in schema holds twenty-two, so `aperture create`
is a measured **1.4 s** before a fact is written.

### S4 — The session, in-process

The server's session machinery over an in-process socket, one connection, no CLI process.
Isolates the three costs S1 excluded: the `spawn_blocking` hop (≥2 per query), the per-row
frame through the outbound mutex and `Notify`, and `QUEUE_DEPTH = 32` backpressure.

**`examples/breakdown.rs` already stands up a real in-process server** — `Catalog::open` +
`Registry::open` + `Listener::bind` + a thread — and already times the hops. S4 extends it
rather than duplicating it. What it adds: row/s *with* framing against S1's row/s *without*
(F4), the chunk-boundary cost at the session layer (F7), the mid-chunk cancel path (F2),
and the `write_blocks` reader stall (F6).

**Blocked on `breakdown.rs` being committed.** Check `git status` first; if it is still
untracked, do S1–S3 meanwhile.

### S5 — One connection, the full round trip

`loadgen --connections 1`, recorded. The latency budget per workload, decomposed against S4
and S1. The existing instrument used as-is; what it needs is a `--warmup` (it has none —
only a single unmeasured compile probe) and machine-readable output.

### S6 — The population: overlapping users of mixed complexity

The actual question. A second mode in `loadgen` rather than a new binary, since the
connection machinery is shared: `--mix WEIGHTS`, `--users N`, `--think MS`, `--duration S`,
`--warmup S`, `--json PATH`.

**The model changes in three ways.** Today: one workload at a time, C connections all doing
it, closed loop, no think time. Instead: N virtual users, each on its own connection, each
drawing from a **weighted mix** of the catalogue, with a think time between queries — so
offered load decouples from N, and "1000 connected, 20 in flight" is distinguishable from
"1000 in flight". Report both.

Sweep N: 1, 8, 32, 128, 256, 512, 1000. Report **per workload**, not aggregate: p50, p95,
p99, p99.9, total q/s, and in-flight concurrency.

**The headline is cross-connection fairness, and nothing in the design provides it.**
`outbound`'s round-robin is *within* a connection (`outbound.rs:153`) — across connections
there is only the tokio scheduler and a FIFO blocking pool. So: with a mix of cheap
(`seek one file`, ~280 µs) and expensive (`follow reference`, ~56 ms) queries, does the
cheap query's p99 stay near its isolated p50? That number is what this phase is for.

Alongside it at each N: server RSS, thread count, live stream tasks (F1), fd count, and
**the generator's own CPU** (see §5).

### S7 — Soak and steady state

Hours at a sustained sub-knee rate. Watches RSS against *queries issued* (F1), fd growth,
fjall open-snapshot count, latency drift, and compaction effects. Plus a disconnect storm —
clients vanishing mid-result — and a **paused-reader population**: a client parked at page
37 holds roughly a socket buffer + 32 frames + one chunk server-side, and nobody has
measured that at scale.

---

## 4. Instrumentation to add

Measurement, not features. Three sources, increasing intrusiveness.

1. **`iter::Profile` — free, already plumbed.** A row/s figure without an
   examined-vs-produced ratio beside it is not reportable.

2. **Server-side counters — none exist.** The only signal a load test can scrape today is
   `eprintln!("connection ended: …")` (`server.rs:106`). Add a `ServerStats` of relaxed
   atomics: connections open, **live stream tasks**, queries started / completed / failed,
   chunks run, rows sent, blocking dispatches, queue-full waits, time waiting for a
   blocking slot; plus self-read RSS / thread count / fd count from `/proc/self`.

   **Exposure: `aperture serve --stats-file PATH`** — one JSON line every N seconds. No
   wire change, no protocol surface, no design-of-record edit, deletable in one commit. The
   durable alternative — an `aperture.server.Stats` virtual predicate riding 9f's
   `aperture.db.List` seam — is the right long-term home and explicitly *not* this phase.

   Also widen `FjallDb::open_snapshots()` (`store.rs:598`, today
   `cfg(any(test, feature = "proptest"))`) behind a new `metrics` feature, so a release
   server can report it.

3. **`perf`** — present on this box; `cargo-flamegraph` is not. `perf record` plus a stack
   collapser is enough for flame graphs on S1 and S6. Off by default.

---

## 5. Constraints, stated up front

- **This box is 8 cores / 32 GB RAM / 185 GB free disk.** (It was 4 / 15 / 5.8 when this
  was drafted, and the disk constraint that shaped the plan is gone.) Measured at ~88
  B/fact on real data — 18.2M facts is 1.6 GB as ingested, 728 MB compacted — so the
  size bands are affordable, but `bench.sh` should still check free space before seeding.
- **Numbers from this box are relative.** Report scaling shapes, ratios and fairness
  findings; withhold absolute capacity claims until real hardware exists. Every result file
  carries a **host fingerprint** (cores, RAM, kernel, rustc, git SHA) and baselines are
  stored per host — which makes moving to a larger machine a re-run, not a rewrite.
- **The generator competes with the server.** `aperture-client` is synchronous, so N users
  is N OS threads on the same 4 cores. Mitigations: `thread::Builder::stack_size(256 *
  1024)`, and **report the generator's own CPU** (`/proc/self/stat` utime+stime) beside
  every result, so a saturation reading can be attributed. If the generator turns out to
  saturate first, the options are an async generator speaking the wire format directly, or
  a second host — the latter needs `--listen-tcp`, which is 9f's. Measure before choosing.
- **One server process per store root**, by `flock` (`catalog.rs:145`), non-blocking and
  deliberately not a wait. There is no in-process multi-reader scaling story beyond stream
  multiplexing; horizontal scaling is a copy of a Complete DB per process
  ([operations §5](aperture-cli-design.md), "Reader scaling model").

---

## 6. Tasks

Each ends in a recorded, reproducible measurement rather than a green test.

- **10a. Write the capacity target down.** Corpus size, user count, query mix, latency
  objective. Closes the `ops-I9` gap the design admits. One page, in `docs/performance.md`.
- **10b. `docs/performance.md`** — the method as a reference doc beside
  [`docs/testing.md`](testing.md): the ladder, what each rung isolates, what a number means,
  host fingerprinting, why absolute numbers are withheld.
- **10c. S0** — `src/workload.rs`; the catalogue grown past eight; pivots sampled; corpus
  size bands built from `index-repo.sh --max-files`.
- **10d. S1–S3** — `examples/engine.rs`, three layers, baselines recorded.
- **10e. S4** — extend `examples/breakdown.rs` once committed.
- **10f. Server counters** — `ServerStats`, `--stats-file`, the `metrics` feature gate.
- **10g. S5–S6** — `loadgen` re-pointed at `src/workload.rs`, population mode, the sweep.
- **10h. S7** — the soak, and `bench/FINDINGS.md` ranked and costed.

## 7. Files

**New:** `src/workload.rs` · `examples/engine.rs` · `docs/performance.md` ·
`bench/baselines/<host>.json` · `bench/FINDINGS.md`

**Modified:** `examples/loadgen.rs` (re-point + population mode) ·
`examples/breakdown.rs` (S4, after it lands) · `scripts/bench.sh` (rung selector, disk
check, JSON path) · `crates/aperture-server/src/{server,session,outbound}.rs` (counters) ·
`crates/aperture-store/src/store.rs` (`metrics` feature) · `PLAN.md` (fold this file in;
also gives `loadgen` / `bench.sh` / `breakdown.rs` a home) · `CLAUDE.md` (one pointer)

**Untouched:** the engine, the codec, the wire protocol, the plan IR.

## 8. Acceptance

- [ ] A capacity target is written down — corpus size, user count, query mix, latency
      objective — and every later number is reported against it.
- [ ] Each rung has an instrument that runs from a clean directory and a baseline recorded
      under a host fingerprint; a second run reproduces it within noise.
- [ ] **Every instrument is self-checking.** A workload asserts its row count against
      `Workload::expected_rows` and its examined count where the corpus makes it exact; a
      mismatch aborts with the discrepancy rather than printing a throughput figure for a
      query that did something else.
- [ ] **Vacuous-pass controls**, per the house idiom (`iter.rs:5079` is the model): the
      zero-data baseline examines exactly 0 rows and still costs something; a full scan
      reports `full_scan = true`. An instrument reporting no work for a real query is
      broken, not fast.
- [ ] **Cross-rung agreement.** S1 row/s > S4 row/s > S5 row/s, and the differences account
      for the layers between them — breakdown's subtraction discipline, with the residual
      as the signal that the model is incomplete.
- [ ] The scaling curve is published for 10k → 10M facts on real data, ns/row against size.
- [ ] Cross-connection fairness is answered with a number: the cheap workload's p99 under a
      mixed population at N = 1, 8, 32, 128, 256, 512, 1000.
- [ ] F1–F8 each carry a verdict — confirmed with a number, or refuted — in
      `bench/FINDINGS.md`, ranked, with a costed fix.
- [ ] `cargo test`, `cargo clippy --all-targets --workspace -- -D warnings`, `cargo fmt
      --all` green; `cargo test -- --ignored --list` unchanged in content — this phase adds
      no guard and retires none.
- [ ] Release only. `bench.sh` already enforces it and records why.

## 9. After this phase — deliberately not built here

- **Guardable deterministically** (machine-independent exact counts, the existing idiom):
  rows examined per workload; allocations per row; `point()` calls per key-only query;
  frames per row; resume-replay rows per chunk. These belong in `cargo test` and never flake.
- **Guardable only as a budget** (machine-dependent): everything timed. Needs
  `bench/baselines/<host>.json` plus a `--check` mode with a stated tolerance, run from a
  script, never from `cargo test`.
- **Fixes**, once `FINDINGS.md` ranks them. F1 (stream-task reaping) and F2 (the cancel
  error code) look like correctness bugs rather than performance work and may not want to
  wait for a phase.

---

> [← Testing methodology](testing.md) · [Index](../README.md) · [Operations →](aperture-cli-design.md)
