# Phase 13 — Fjord against Glean, measured

> [Fjord design book](../README.md) · the method: [`performance.md`](performance.md) ·
> what has been measured so far: [`bench/FINDINGS.md`](../bench/FINDINGS.md) · what each
> system can be *asked*: [`glean-capabilities.md`](glean-capabilities.md)

Two databases now hold **the same facts**: 18,258,385 of them, every predicate agreeing
stored-for-stored, from one Roslyn walk over 26,924 files of `dotnet/runtime`
([findings §16](../bench/FINDINGS.md)). The write paths have been measured. This is the
plan for the read paths.

**What this phase is not.** It is not a race to a headline number. Both engines answer the
same questions with the same asymptotics — a seek is a seek — so a single number would
mostly report which one this box's page cache liked on the day. What is worth having is a
**map of where the two differ and by how much**, with each difference attached to a design
decision one of them made on purpose.

---

## 1. What makes this comparable, and what does not

**Comparable, and now demonstrated:**

- **One corpus.** Same producer, same walk, same 18.26M facts, same per-predicate counts,
  both `Complete`/sealed ([§16](../bench/FINDINGS.md)).
- **One schema, field order included.** `fjbench.angle` preserves every predicate, field and
  *field order* from `code.sigla`, because on both systems the record's field order is the
  key's byte order and therefore the index design.
- **Both key orders pre-materialised on both sides.** `SearchByName`, `SearchByLowerName`,
  `FileXRef`, `DerivesFrom` and `AttributeOf` are written by the indexer into both
  databases. Glean could have *derived* them and we cannot yet
  ([Phase 8b](../PLAN.md)) — writing them on both sides is what makes the suite measure two
  **engines** rather than one engine against the other's deriver. That capability difference
  gets its own family (F16) instead of contaminating fifteen others.

**Not comparable, and treated as capability-with-a-price rather than hidden:**

| | Glean | Fjord |
|---|---|---|
| recursion | yes | no ([comparison §3](glean-comparison.md)) |
| stored derivation | `glean derive` | Phase 8b |
| aggregation | yes | comparisons and arithmetic only |
| expansion | server-side, on by default in the shell | client-side, `--expand` |

For each of those, the measurement is **what the same information need costs the caller** —
one recursive query against a client-side loop of round trips — not a head-to-head of a
feature only one has.

**Two rungs, and they must not be mixed.**

| rung | Glean | Fjord |
|---|---|---|
| **in-process** — the engine alone | `glean --db-root … query` (local backend, no Thrift) | `cargo run --release --example engine -- --store …` |
| **over the wire** — the service | `glean server` + `--service host:port` | `fjord serve` + client |

`glean query --db-root` runs the engine inside the CLI process, which is the right partner
for `examples/engine`. Comparing our socket against their local backend would be measuring
our transport against their absence of one.

---

## 2. The instruments, and why the layers line up

Glean's `UserQueryStats` (`glean/if/glean.thrift`) reports `compile_time_ns`,
`execute_time_ns`, `elapsed_ns`, `result_count` and — with `--profile` — **`facts_searched`
per predicate**. Ours reports the same shape:

| what | Glean | Fjord |
|---|---|---|
| compiling the query | `compile_time_ns` | the compile rung (S2), `:plan` |
| running it | `execute_time_ns` | the executor rung (S1) |
| end to end | `elapsed_ns` | `--timing` |
| rows out | `result_count` | rows |
| **work done** | `facts_searched` per predicate | `Profile.examined` per plan step |

**`facts_searched` against `examined` is the most valuable column in the suite**, and the
reason this is worth doing at all: it turns every timing difference into one of two
statements — *it did more work*, or *it did the same work slower*. A benchmark that reports
only milliseconds cannot tell those apart, and they have opposite fixes.

**Harness requirements**, all of them lessons already paid for:

1. **`glean script`, not one process per query.** The `glean` binary is 97 MB; process
   startup would dominate every seek. Startup is measured *once*, on purpose, as F15 —
   because it is what a person typing a query actually pays.
2. **Pivots sampled once, substituted into both.** `workload::Pivots` already samples a
   file, a directory, a search term and a declaration from the live corpus; the driver takes
   them from there and writes both spellings, so a seek on one side is not a miss on the other.
3. **p50/p95/p99 over N repetitions**, never a mean. §8 established the steady-state method.
4. **Work-done counters recorded beside every timing**, or the row is not reportable.
5. **A/B/A interleaving** across three passes, to catch drift rather than average it away.
6. **A baseline file per host** — `bench/baselines/<host>.json` — which closes the
   "no baseline file" item [findings](../bench/FINDINGS.md) has carried since Phase 10.
7. **Nothing else on the box.** §15 and §16 both turned on memory pressure: a 16 GB indexer
   starved the page cache the LSM needed. A query benchmark sharing the machine with
   anything measures the sharing.
8. **Warm and cold arms.** Warm = the suite's own second pass. Cold needs
   `drop_caches` and therefore root; if it is unavailable, say so in the row rather than
   quietly reporting warm numbers as cold.

---

## 3. The suite

Sixteen families. The first eleven reuse `workload::catalogue`'s questions — they already
have stated rationale and are what the Fjord numbers in §1–§11 were taken over — and each
is paired with its Angle spelling. **The prediction column is what makes this an experiment
rather than a table**: it is drawn from the design docs, so a run can falsify one.

| # | family | the question | what it should expose | prediction |
|---|---|---|---|---|
| F1 | point lookup | one file by exact path | the floor: both are one key probe | parity; whatever differs is process and transport, not engine |
| F2 | prefix range | every file under one directory | prefix seek on an order-preserving key — both spell it `"x"..` | parity, slope set by rows returned |
| F3 | search index | `SearchByName {name = "Parse"}` | the query a person types | parity; if not, encoding of the result |
| F4 | prefix search | `SearchByLowerName {name = "parse"..}` | range seek plus fan-out | parity |
| F5 | scan curve | full scans of File → Module → Decl → Ref → Line (26.9k → 7.5M rows) | **raw scan throughput against database size** | Glean, on residency: 2.4 GB against 886 MB for comparable facts means more of it fits. If *we* win, lazy field decode ([I5](invariants.md#i5)) beats their residency, which is the more interesting result |
| F6 | projection width | one field against three off a nested key | per-row decode cost | parity; ours should be flat in field count ([I5](invariants.md#i5)) |
| F7 | **value read** | `Decl.name` (key) against `Decl.value` (kind) | **the sharpest prediction in the suite** | a large Fjord penalty: a value is a second point read per row ([I6](invariants.md#i6)), where Glean's value is inline. If the penalty is small, the page cache is absorbing it and the trade in [capabilities §2.2](glean-capabilities.md) is cheaper than it reads |
| F8 | joins | leading-field join against trailing-field join | what a key's field order is worth | both degrade, and similarly: neither planner consults statistics ([capabilities §3.1](glean-capabilities.md)) |
| F9 | reference | *following* one (id compare) against *reading through* one (fetch) | the split our IR makes explicit | ours flat on the compare, one point read per row on the fetch; Glean's nested pattern match should behave like the compare |
| F10 | expansion | shallow ids, then 1/2/3 hops | server-side against client-side ([capabilities §2.7](glean-capabilities.md)) | Glean wins with depth (one round trip), we win shallow (no expansion work at all); the crossover is the number |
| F11 | negation | declarations with no doc comment | anti-join shape | parity; ours is a `Step::Test` re-decided on restore |
| F12 | counting | how many rows, no rows returned | `--count` against `--omit-results` | parity, and both far under returning the rows |
| F13 | paging | first page of 40, then the whole result in pages | time-to-first-row against total | ours: a bytes-only cursor with no held snapshot ([I8](invariants.md#i8)); theirs: a continuation. Expect parity on first page and a difference on resume cost |
| F14 | fairness | p99 of cheap seeks while a 7.5M-row scan runs | scheduling under mixed load | ours, on chunked interleaving (§9, §11) — if Glean runs a query to completion per request, its p99 should be the scan's duration |
| F15 | cold start | first answer from a cold process | what a CLI user pays | ours, heavily: 97 MB of Haskell binary against a 12 MB Rust one. Worth stating because it is real and because it is not the engine |
| F16 | capability price | transitive closure: `DerivesFrom*` | one recursive Glean query against our client-side loop | Glean, decisively; the number is how much a missing feature costs, and it is the strongest argument in the file for building recursion |

Two more that are not query families but belong to the same run, because they are read-path
costs nobody has priced:

- **F17 — `glean derive` against an indexer-written key order.** Building `FileXRef`
  server-side from `Ref` against having the producer write both. This is what Phase 8b buys
  and what it would cost.
- **F18 — open cost.** Time from process start to a database being answerable, on both, at
  this size. Ours opens every database under the root at startup ([`ops-I7`](fjord-cli-design.md));
  it has never been measured at 18M facts.

---

## 4. What gets written down

One row per (family, rung, system), carrying: p50/p95/p99, rows out, **work done**
(`facts_searched`/`examined`), bytes returned, and the pass it came from. Plus, for every
row, the thing [`performance.md §4`](performance.md) requires: the host, and the statement
that absolutes do not travel.

A family whose two sides disagree by less than the spread between passes is reported as
**parity**, explicitly. Half the value of this suite is the families where nothing
interesting happens, because that is what makes the three or four that do interesting.

---

## 5. Build order

1. **The driver, one family, both rungs** (F1). Everything else is a table row once the
   plumbing, the pivots and the counter parsing exist.
2. **F5 and F7** — the scan curve and the value read. The two headline predictions, and both
   are cheap once F1 runs.
3. **F2, F3, F4, F6, F8, F9, F11, F12** — the rest of the reused catalogue.
4. **F10, F13, F14** — expansion, paging, fairness. These need more harness than query text.
5. **F15, F17, F18** — process and lifecycle costs.
6. **F16 last**, because it needs a recursive Angle query and a client-side loop written
   against our own client, and it is the one family where the answer is already known and
   only the magnitude is not.
