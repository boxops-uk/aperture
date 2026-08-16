# Measuring Aperture — the method, and the target

> [Aperture design book](../README.md) · the performance counterpart to
> [`testing.md`](testing.md). That file says how a *claim* is held to evidence; this one
> says how a *number* is, which is a different discipline with a different failure mode.
> The register of what has actually been measured is [`bench/FINDINGS.md`](../bench/FINDINGS.md).

---

## 1. The target

**Everything below is a proposal, and it is the first one this repository has had.**
[Operations §1](aperture-cli-design.md) admits the hole in as many words — *"Aperture
states no target corpus size, no churn rate and no freshness budget anywhere, so `ops-I9`
is ultimately a requirements question this repo cannot settle on its own"* — and a
measurement with no target is a number with nothing to be good or bad against. So this is
written down to be argued with. It is derived from what has been measured rather than from
what anyone needs, and the day somebody states a real requirement, this section is the
thing it replaces.

| | Target | Where the number comes from |
|---|---|---|
| **Corpus** | 20M facts, one repository, ~30k files | `dotnet/runtime`'s `src/` tree is 18.2M facts across 22 predicates, 728 MB sealed ([findings](../bench/FINDINGS.md)) |
| **Population** | 1,000 concurrent users per instance | An order of magnitude below where the code-search mix saturates (~6,000 at a 3 s think time), so the target has headroom rather than being a ceiling restated |
| **Mix** | the code-search workload: prefix search, paged to 50 | `examples/codesearch.rs`. It is the traffic a product actually sends; the generic mix is not |
| **Interactive latency** | p50 < 25 ms, p99 < 250 ms | Measured p50 is 3.1 ms at 2,048 users; the target leaves an 8× margin for hardware that is not this box |
| **Freshness** | a full re-index, not an append | `ops-I9`: a Complete database is immutable, so "freshness" is how often a new one is built, and that is an indexing-throughput question rather than a serving one |

**What is deliberately *not* targeted**, because the measurements say it cannot be:

- **Unbounded queries.** A whole-predicate scan of `src.Line` is 8.6M rows and always
  will be; the generic mix's ~67 q/s is what that costs. Bounding a result is the
  client's job, and `--limit`, `\more` and a paged UI are how.
- **Write throughput under concurrency.** One writer per database, held across an ingest
  (`ops-I1`, `ops-I5`), so adding writers adds queueing by design. Indexing is a
  build-time cost measured in hours, not a serving metric.
- **Anything absolute on this box.** See §4.

---

## 2. The ladder

Each rung is a narrower surface than the one above it, and the point of the arrangement is
**attribution**: a regression at the top is only actionable if you can say which rung it
appeared at.

```
S0  corpus       a real .NET index at a dialed size, plus a synthetic control
 │
S1  executor     in-process, real FjallDb, no compile, no wire, no tokio
S2  compile      Compilation::plan alone — the per-query floor
S3  store        fjall scan/point — the floor under S1
 │
S4  session      server machinery, in-process socket, one connection
S5  round trip   loadgen --connections 1 — the latency budget
 │
S6  population   N overlapping users, mixed workload, think time
S7  soak         hours at a sustained rate; leaks, drift, disconnect storms
```

| Instrument | Rung | What it isolates |
|---|---|---|
| `examples/engine.rs` | S1–S3 | the engine with everything else taken away |
| `examples/breakdown.rs` | S4 | the fixed per-query cost, by subtraction |
| `examples/loadgen.rs` | S5 | one connection, the whole round trip |
| `examples/soak.rs` | S6–S7 | a mixed population, and steady state over hours |
| `examples/codesearch.rs` | S6 | the product's own traffic, rather than a generic mix |

`src/workload.rs` is S0's other half: **one statement of the queries**, so a number from
one rung can be compared with a number from another. Before it, `loadgen` sought a key
computed as `files / 2` — which exists in the corpus it seeded itself and in no real index
at all, so pointing it at a checkout measured a miss and called it a seek.

Sharing the catalogue put a demand on the **synthetic** corpus that the real one already
met: `loadgen` seeded files, modules and declarations, so six of the catalogue's workloads
answered nothing and reported a throughput anyway. It seeds every predicate the catalogue
asks about now — including `src.Line`, which is the one that is large without being about
a symbol. The synthetic corpus is worth keeping beside the real one for the two things a
checkout cannot give: an exactly known row count, and a size nobody has a repository for.

---

## 3. What makes a number reportable

Four rules, and each of them exists because a measurement broke one.

**A workload states what it answers.** Every instrument runs each workload once
*unmeasured* to fix its row count and its per-step examined counts, then aborts any timed
run that fails to reproduce both. This is [`testing.md`](testing.md)'s rule about vacuous
passes applied to a measurement: a throughput figure for a query that did something other
than what you think it did is worse than no figure.

**Examined beside produced, always.** `iter::Profile` counts rows examined per plan step,
and a row/s figure without that ratio next to it cannot distinguish a fast query from a
query that is not doing the work. The 56,274-examined-per-row join in
[findings §2](../bench/FINDINGS.md) looked like an ordinary join and read the whole
predicate per outer row.

**A control that must cost something, and one that must not.** The catalogue leads with
`X where X = 42` — a plan with no steps, exactly one row, exactly zero rows examined. An
instrument reporting work for it is broken; an instrument reporting *no* work for a real
query is broken too, and only having both ends pinned catches either.

**The generator's own CPU, beside the server's.** The load generator is N OS threads on
the same cores as the server, so a flat achieved rate means nothing until it can be
attributed to one of them. `soak` reports it.

---

## 4. Absolutes stay on the box that produced them

Every result carries a **host fingerprint** — cores, RAM, kernel, rustc, git SHA — and
baselines live per host. What travels between machines is:

- **shapes** — ns/row against predicate size; is a seek O(log n) and a scan O(n)
- **ratios** — examined against produced; the row encoder against the framing above it
- **fairness** — a cheap query's p99 under a mixed population, against its isolated p50

What does not travel is any number with a unit of time in it. This box is 8 cores /
32 GB / 185 GB free; the numbers in `FINDINGS.md` are its, and are reported as ratios
wherever a ratio will do the job.

**Release builds only.** `scripts/bench.sh` enforces it by construction, because a debug
number is not a slow number — it is a number about a different program.

---

## 5. Turning a number into a guard

Nothing here is a test, and [`testing.md`](testing.md) is blunt about what that means: *an
NFR with no mechanical guard is an aspiration, not an acceptance criterion.* The numbers in
`FINDINGS.md` will decay unless they are pinned. Two kinds, and only one belongs in
`cargo test`:

**Machine-independent, so guardable exactly** — rows examined per workload; allocations
per row; `point()` calls per key-only query; frames per row; resume-replay rows per chunk.
These are counts, they do not flake, and the codebase already has the idiom for them
([I5](invariants.md#i5), [I6](invariants.md#i6), [I9](invariants.md#i9)).

**Machine-dependent, so guardable only as a budget** — everything timed. These want
`bench/baselines/<host>.json` and a `--check` mode with a stated tolerance, run from a
script and never from `cargo test`, because a timing assertion in a test suite is a test
that fails on a busy machine and teaches everyone to re-run it.

---

> [← Testing methodology](testing.md) · [Index](../README.md) · [Operations →](aperture-cli-design.md)
