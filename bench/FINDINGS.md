# Findings — Phase 10, rungs S1 through S7

> [Aperture design book](../README.md) · the register [`docs/phase-10-capacity.md`](../docs/phase-10-capacity.md)
> asks for. One entry per thing measured: what was measured, the number, and what it costs
> to act on.
>
> **The phase was scoped as measurement only, and three findings were taken out of that
> scope** — because leaving them would have made every later number a measurement of a bug
> rather than of the system, or would have frozen an accident into somebody's index.
> Sealing now merges the storage it seals (§1), the two key orders that decided whether a
> join seeks are declared rather than inherited from alphabetical order (§2, and the
> find-references blocker in §11), and a connection dying mid-answer no longer strands the
> stream answering it (§10). Each carries guard tests and is marked ✅ where it appears.
> Everything else here is measurement.

**The corpus.** `dotnet/runtime` at `c99188c2f97`, its whole `src/` tree indexed by
`clients/dotnet/Aperture.Indexer --syntax-only --jobs 8`: 32,710 files, **18,176,899
facts** across 22 predicates, 1.8 GB on disk, 4,613 s to build. This is the first
database in the project's history large enough for a scaling question to mean anything,
and every number below is from it.

**The instrument.** `cargo run --release --example engine -- --store <instance>` —
S1 (`--layer executor`), S2 (`--layer compile`), S3 (`--layer store`). In-process, no
tokio, no wire, no server. Each workload is run once unmeasured to fix its row count and
per-step examined counts, and every timed run must reproduce both or the run aborts.

**The host.** 8 cores, 32 GB, kernel 6.8.0-1061-aws, release build. Note this **corrects
[§5 of the phase plan](../docs/phase-10-capacity.md)**, which says 4 cores / 15 GB / 5.8 GB
free disk; the box now has 8 / 32 / 185 GB. The disk constraint that shaped the plan is
gone. Ratios travel, absolutes do not.

---

## 1. Paging costs one seek per page — and on a freshly-ingested database that seek can cost 600 µs

**Hypothesis F7, answered, and it turned out to be two findings.**

The server computes at most `CHUNK_ROWS = 256` rows per turn, then suspends to a
bytes-only cursor and resumes ([chapter 5](../docs/05-resume.md)). Nobody had measured
what that costs, and it is paid on every query. Running the same plan straight through
against the same plan suspended every 256 rows, both bounded to 100,000 rows:

| Workload | straight | paged | overhead | per page | · snapshot | · resume |
|---|---|---|---|---|---|---|
| scan files | 13.85 ms | 14.24 ms | +3% | 3.1 µs | 0.1 µs | 4.9 µs |
| scan modules | 14.88 ms | 14.79 ms | +0% | — | 0.1 µs | 4.2 µs |
| scan lines | 38.31 ms | 42.07 ms | +10% | 9.6 µs | 0.1 µs | 9.5 µs |
| scan refs | 46.35 ms | 110.36 ms | **+138%** | 164.1 µs | 0.2 µs | 161.8 µs |
| **scan decls** | 42.56 ms | 352.99 ms | **+729%** | 796.0 µs | 0.3 µs | **790.3 µs** |

The three dotted columns take a page apart, and they settle where the cost is: **the
snapshot is free** (0.1–0.3 µs, so [I8](../docs/invariants.md#i8)'s release-at-suspend
discipline costs nothing) and **all of it is `Executor::resume`** replaying one seek per
level.

But a resume seek should not cost 790 µs, and a 200× spread between predicates in one
database is not a property of the executor. S3 measures the same seek with the machine
taken away — open a scan at a key sampled from the middle of the keyspace, take one row:

| Keyspace | rows | tables | seek, as ingested | seek, after `major_compact` |
|---|---|---|---|---|
| `src.File` | 32,710 | 1 | 3.6 µs | 3.0 µs |
| `src.Module` | 36,192 | 1 | 2.8 µs | 3.0 µs |
| **`src.Decl`** | 888,177 | 1 | **646.6 µs** | **4.7 µs** |
| `src.SearchByName` | 888,177 | 1 | 4.3 µs | 8.8 µs |
| `src.Ref` | 4,879,151 | 5 | 160.1 µs | 10.4 µs |
| `src.Line` | 8,583,810 | 4 | 7.0 µs | 3.4 µs |

`src.Decl` and `src.SearchByName` hold **exactly the same number of rows** in the same
database, each in a single table, and one seeks 150× slower than the other. Ruled out by
measurement, not by argument:

- **not row count** — the two slow ones are 888k and 4.9M; the fastest is 8.6M
- **not table count / LSM overlap** — `src.Decl` is one table, and so is `src.SearchByName`
- **not position** — seek cost is flat across the keyspace (627 µs at row 0, 698 µs at row 393,504)
- **not stale versions visible to iteration** — `approximate_len` equals the distinct row count
- **not key width** — `src.File` has the widest keys (88 B) and is the second fastest
- **not leading-byte cardinality** — a synthetic control with the same row count and key
  length, varying only how many rows share a leading prefix (1 → 5,000), moves the seek
  by 3.1 → 3.3 µs. Nothing.

What does settle it: **copy the real keys and values, unchanged, into a fresh keyspace,
sort, flush and compact** — same data, differently built.

```
keys.2 (src.Decl): 888,177 rows, key bytes min 17 avg 53 max 5907
  as ingested : tables=1 disk=50,639,866   seek 623.4 µs
  rewritten   : tables=1 disk=33,962,226   seek   3.4 µs      ← 183×
keys.4 (src.Ref) : 4,879,151 rows
  as ingested : tables=5 disk=159,824,868  seek 156.1 µs
  rewritten   : tables=2 disk= 82,628,952  seek   3.4 µs      ←  46×
```

So it is **the state ingestion leaves the LSM in**, not the data and not the engine. A
major compaction of the whole index confirms it end to end:

- **22.9 s** to compact all 44 keyspaces, single threaded
- **1,593,452,874 → 728,344,678 bytes** — the store is 46% of its ingested size
- and F7 collapses to a **uniform +7% to +20%, 4–12 µs per page**, which is the real
  answer to what paging costs

**Nothing in the tree compacted.** `grep -rn major_compact crates/ src/` was empty, and
`Catalog::finish` — the one operation that declares a database immutable forever — wrote
an identity hash and a sidecar and never touched the LSM. So the artifact that
[operations §5](../docs/aperture-cli-design.md) says gets copied per reader process was
shipped in the shape a random write order left it.

### Fixed: sealing now merges

`FjallDb::compact` merges every tree, and `seal` calls it after the durability sync and
before the identity walk — before the walk so the fingerprint is computed over the tree
that ships, before the sidecar so the byte count it records is the artifact's rather than
the ingest's. Two guards in `aperture-store/tests/finish.rs`:
`sealing_merges_every_tree_into_one_table` (which flushes three batches first and asserts
it had something to merge, because at test sizes it otherwise would not) and
`merging_does_not_change_the_identity`, which is `ops-I4` — compaction rewrites the bytes
and must not move the fingerprint.

Measured through the real command, on the 18M-fact index:

```
$ aperture --data-dir … finish code
sealing code — merging trees, then computing identity
sealed code: 18176899 facts, 853606008 bytes, identity 0xa0a07894b275e6a0
        4m16s
```

- **1.7 GB → 853 MB**, and that is now the `bytes` the sidecar reports, so `describe` says
  how big the artifact is rather than how big the ingest was.
- **Every seek 4.2–4.6 µs**, measured on the sealed database: `src.Decl` 646.6 → 4.2 µs,
  `src.Ref` 160.1 → 4.3 µs, `src.Line` 7.0 → 3.3 µs.
- Of the 4m16s, the merge is ~23 s measured separately; the rest is the identity walk,
  which `finish` always did.

**And the merge is not a cost — it is a saving.** The same `finish` on an identical copy
with the merge removed ran for **30 minutes of CPU without completing**, against 4m16s
complete. The reason is the identity walk itself: it expands every reference to the base
fact it names, which is one point read per reference, and point reads are exactly what an
unmerged tree makes expensive. So merging first makes the walk that follows cheaper by more
than the merge costs. That figure is a lower bound — the run was stopped, not finished —
but the direction is not in doubt, and the *old* `finish` was the slow one.

`finish` was quick on small databases and is now minutes on large ones, so it prints a
line to stderr before it starts rather than explaining afterwards. `ops-I3` is unchanged
and the crash guard still passes: the sidecar flip is still the last durable act, and a
crash during the merge leaves the pre-merge tree and a Writable database — the same answer,
and the same re-runnable command, as a crash during the sync.

**Still open: a Writable database is never merged.** A long-lived one that is queried while
it grows — which is what the server's `create`/write-stream path produces — pays the
unmerged seek cost on every page, forever, because it never reaches `finish`. Nothing here
addresses that, and it should not be addressed by compacting on a timer without measuring
what a merge costs a concurrent writer first.

**Watch this one, because it is the finding that changes other numbers.** Every capacity
figure taken against a freshly-ingested database — including S6's population sweep, when
it is run — is measuring an unmerged LSM unless it is sealed first, and the effect is not
uniform across predicates, so it distorts a workload *mix* rather than scaling it. `finish`
before measuring is now the whole of the fix, and every rung above S3 should say which side
of it the numbers came from.

---

## 2. A join's cost is decided by key field order — and that order is the schema's to choose

Not on the hypothesis list, and the largest effect measured anywhere in this phase.

A predicate's seekable prefix is its key's leading fields, and the physical key order is
**the order the schema declares them in**. So:

- `src.Line` is declared `{file, line}` → the key leads with the reference
- `src.Decl` is declared `{line, module, name}` → the key leads with the **line number**

The same join shape over the two, from the S1 catalogue:

| Workload | rows | examined | examined/row | per row | plan |
|---|---|---|---|---|---|
| `join on a leading field` | 8,583,810 | 8,616,520 | 1.004 | 393 ns | `src.File* → src.Line` |
| `join on a middle field` | 2,000 (capped) | 112,548,829 | **56,274** | 26.0 ms | `src.Module* → src.Decl*` |

```
L where F = src.File _;   src.Line {file = F, line = L}      -- seeks
D where M = src.Module _; src.Decl {module = M, name = D}    -- rescans src.Decl per module
```

The second cannot narrow, so it reads all 888,177 declarations once per module — 32
billion rows to completion, which is why the instrument caps it. `--profile` already tells
you (`full scan` on the inner step); nothing warns you when you write it.

This is also the most plausible cause of finding 1's *distribution*: a predicate whose key
leads with a low-cardinality int is written in an order uncorrelated with its key order, so
its tree takes the most garbage. The two slow seeks were `src.Decl` (leads with `line`) and
`src.Ref` (leads with `at.col`).

### The order is declared, not derived — checked three ways

It is tempting to read this as alphabetical, because in `src/code_index.rs` every record's
fields *are* in alphabetical order. They are not in that order because anything sorts them:

1. **`flatten`** walks the schema's own `PredicateTy::Record` slice by index
   (`FieldPath::field(idx)`) and looks each query field up **by name**
   (`flatten.rs:2341`). Nothing sorts; the slice's order is the key's order. `ty.rs:695`
   says as much in a comment — *"both sides are sorted, but the schema's order is Phase
   8's to guarantee, not this pass's to assume."*
2. **A test already pins it.** `aperture-store`'s `the_encoding_order_is_the_declared_order`
   (`fact.rs:538`) builds a predicate declaring `z` before `a` precisely because *"the
   schemas here happen to be sorted by name, and nothing in the codec depends on it"*.
3. **Directly.** Two schemas over the same two fields, differing only in declaration order,
   compiled against the same four queries:

   ```text
   declared {apple, zebra}      test.Rev {apple = "a", zebra = X}  → seek[apple = "a", zebra = _]
                                test.Rev {zebra = 1,   apple = X}  → scan
   declared {zebra, apple}      test.Rev {apple = "a", zebra = X}  → scan
                                test.Rev {zebra = 1,   apple = X}  → seek[zebra = 1, apple = _]
   ```

What makes the built-in schema alphabetical is a **convention it imposes on itself**:
`code_index.rs`'s `every_record_lists_its_fields_in_sorted_order` asserts it, and the
file's own commentary states the rule as if the machine required it — *"A record's fields
are sorted by name and that order is the key order, so the only way to choose what a
predicate narrows on is to choose what its fields are called."* That last clause is not
true: the order can be chosen directly, and only that test forbids it here.

**Costed fix, and it is much cheaper than a design change.** Declare `src.Decl` as
`{module, name, line}` — one line in `code_index.rs` — and retire or invert the sorted-order
test, whose stated purpose (a swapped field list silently answering a different question)
is served better by asserting the *intended* order per predicate than by asserting sorted.
[I3](../docs/invariants.md#i3)/[I1](../docs/invariants.md#i1) freeze what is already
written, so this is a re-index rather than a migration — cheap now, at one index; not cheap
later. Worth settling before [Phase 8](../PLAN.md)'s schema DSL fixes how a key is written
down, since the DSL will have to say whether declaration order is load-bearing.

### Fixed: the order is declared per predicate, and the guard says so  ✅

`src.Decl` is `{module, name, line}` and `src.Ref` is `{to, file, at}`, with `at` itself
`{line, col}`. The sorted-order test is replaced by `KEY_ORDER`, a table of every stored
key as its **flat** field path list, asserted against the schema — so a swap still fails,
and the guard can no longer enforce an accident. It also refuses a record value side,
which nothing has yet and which would need the same decision.

The claim that this is one line was optimistic by four files: the key order is stated
independently by the C# indexer, the C# demo and the Rust golden test, and a `WireFact`'s
key is **positional**, so every fact builder moved with the declaration. That the two
clients then produced byte-identical blocks again, from schemas neither one shares, is the
check that the reorder is consistent rather than merely compiling.

**One stale comment made this harder to see, and may have caused it.** `src.SearchByName`
exists, per its own comment, because *"a declaration's key begins with its module, so
`src.Decl {name = "encode"..}` reaches the name only after the scan has opened"*. The key
no longer begins with `module` — `line` was added ahead of it, alphabetically — so the
predicate that exists to work around `src.Decl`'s key order is documented against a key
order `src.Decl` no longer has.

---

## 3. The executor's own floor is ~330 ns/row, and it is flat in database size

The scaling curve, which [the phase plan](../docs/phase-10-capacity.md) calls the one
result that could invalidate the target outright. It does not.

| Workload | rows | per row | rows/s |
|---|---|---|---|
| scan files | 32,710 | 401 ns | 2,495,088 |
| scan modules | 36,192 | 385 ns | 2,597,765 |
| scan decls | 888,177 | 432 ns | 2,313,953 |
| scan refs | 4,879,151 | 434 ns | 2,306,705 |
| scan lines | 8,583,810 | 360 ns | 2,779,286 |

**262× the rows, and ns/row moves by less than 20% — with no trend.** Underneath it, S3's
raw `FactStore::scan` is 233–426 ns/row over the same range, so the executor adds roughly
25–50% to what fjall charges, and cross-rung agreement holds (S3 rows/s > S1 rows/s
everywhere).

Two more from the same table:

- **`X where X = 42` costs 327 ns** — a plan with no steps, one row, zero rows examined.
  That is the executor's fixed cost, against `loadgen`'s ~200 µs end-to-end floor: **the
  engine is under 0.2% of what a query costs over a socket**, which is the same conclusion
  `breakdown` reached from above and is now bracketed from below.
- **A `Source::Fetch` costs ~2.6 µs/row** — `scan decls` at 432 ns against the same scan
  plus a fetch at 3.0 µs. S3 prices a bare point read at 1.7–2.1 µs, so the fetch is
  essentially the point read and the machine adds little. Projecting the fetched fact's
  *reference* field costs exactly what projecting its *string* costs (3.0 vs 3.0 µs), so
  the fetch is the whole price.

---

## 4. ~~A fact's value cannot be read by a query~~ — **wrong, and withdrawn**

Found while writing the catalogue, and it bounds what any of these numbers can cover.

> `src.Line` is `{ file, line } -> string` and holds 8,583,810 line texts — 133 MB, the
> largest predicate in the index. No focus query can read one. There is no `->` in the
> grammar, and every spelling tried is a parse error; a query binds *key fields* only,
> and a bare `X = src.Line _` binds the fact reference.
>
> The immediate consequence for this phase: **F5 (a chunk has no byte budget) cannot be
> exercised from the query side at all.** The widest row the catalogue can build is three
> narrow fields off a nested key, which is nowhere near the 64 MiB `MAX_PAYLOAD` that F5
> is about. F5 stays open, and needs either a value-reading query or a synthetic corpus
> of very wide keys.

**`.value` reads the value side, and always has.** It compiles to `Project::Value`, in
process and over the wire:

```
focus> :plan L.value where L = src.Line _
  r0 <- src.Line scan
  head r0.value

$ aperture query code '{n = D.name, k = D.value} where D = src.Decl _' --limit 5
K      N
def    key_of
class  CodecError
…
```

The corpus said so before this finding was written —
`"X.value where X = test.Foo _"` is `Supported`, annotated *"`.value` is the fact's value
side — Project::Value"*. What is deferred is **matching** on a value
([I6](../docs/invariants.md#i6)), not reading one. The `->` spellings tried here are
indeed parse errors; the mistake was generalising from them without trying the field
access, and no plan was printed to check the conclusion against.

Two consequences. **F5 is not blocked** — a query over `src.Line`'s value side builds
rows as wide as the corpus has text, which is the wide-row generator this finding said
did not exist. And serving a file's source text out of the database is a seek plus one
value read per row, which is what makes a code-search file view possible at all
([phase 11](../docs/phase-11-code-search.md)).

*Kept rather than deleted, struck through: a findings file that quietly edits its
mistakes is one nobody can calibrate against.*

---

## 5. Compilation is 4–14 µs, and linear in the query's size

S2, against F3 — *no plan cache; every query is parsed, typechecked, flattened and
reordered afresh.* Over the S1 catalogue that costs **4.4 µs** (`X where X = 42`, no
steps) to **13.6 µs** (the widest projection); the three-level join compiles in 11.4 µs.

Against generated queries of k conjuncts over one predicate:

| conjuncts | 1 | 2 | 4 | 8 | 16 | 32 |
|---|---|---|---|---|---|---|
| compile | 5.0 µs | 7.2 µs | 12.4 µs | 23.4 µs | 43.8 µs | 83.3 µs |

**Linear** — ~2.5 µs of fixed cost plus ~2.5 µs per conjunct, with no term that grows
faster. It is also independent of corpus size, as it must be: the compiler never touches
the store.

So F3 is confirmed as *true* and reclassified as *small*: against the ~211 µs per-query
floor the phase plan records for `loadgen`, a plan cache would buy back **2–7%**. Real,
measurable, and the smallest term in the budget — not worth building against finding 1.

---

## 6. A population of readers: capacity is ~67 q/s for this mix, and it does not collapse

S6, against the **sealed** 18.2M-fact index (finding 1 matters: an unmerged database is a
different measurement), 8 cores, generator on the same box. Each client is a connection and
a thread drawing from the standard lopsided mix — 80% point lookup, 15% small scan, 4% full
scan, 1% whole-database join — with no think time, so this is the saturation curve.

| clients | point p50 | point p99 | small scan p50 | full scan p50 | join p50 | achieved | CPU (gen / server) |
|---|---|---|---|---|---|---|---|
| 1 | 193 µs | 342 µs | 1.8 ms | 70.9 ms | 5.20 s | 17 q/s | 23% (5 / 17) |
| 8 | 938 µs | 3.7 ms | 4.7 ms | 184 ms | 11.4 s | 86 q/s | 86% (21 / 65) |
| 32 | 2.6 ms | 19.7 ms | 19.4 ms | 819 ms | 43.0 s | 69 q/s | 97% (22 / 75) |
| 128 | 3.0 ms | 18.0 ms | 81.2 ms | 4.23 s | 124.5 s | 68 q/s | 99% (23 / 76) |
| 256 | 6.7 ms | 29.7 ms | 158.8 ms | 8.54 s | 178.4 s | 67 q/s | 99% (23 / 76) |
| 512 | 18.3 ms | 64.6 ms | 305.9 ms | 15.65 s | 231.6 s | 69 q/s | 99% (23 / 76) |
| 1024 | 42.7 ms | 100.4 ms | 571.3 ms | 28.84 s | 315.3 s | 66 q/s | 99% (23 / 76) |

**Zero errors at every level.** Four things worth taking from it:

- **Throughput plateaus at ~67 q/s from 32 clients to 1024 and stays there.** A 32× increase
  in offered load moves achieved throughput by 3%. It saturates; it does not collapse.
- **The cheap query stays cheap.** Point-lookup p99 goes 19.7 ms → 100.4 ms across that same
  32× range, and p99/p50 stays between 2.3 and 3.5 — it tracks p50 rather than detaching
  from it, which is the difference between a queue that is fair and one that is starving
  somebody. At 1024 clients a point lookup answers in 42.7 ms while the join it is sharing
  the server with takes 315 s: **a factor of 7,400**, and the small query is on the right
  side of it. That is the chunked resumable executor doing what I4/I8 built it for, now at
  18M facts rather than 240k.
- **The expensive class queues linearly and fairly** — the join's p50 rises in step with the
  client count (43 s at 32, 315 s at 1024), which is what no-starvation looks like from the
  other side.
- **The bottleneck is the server, not the instrument.** At saturation the generator burns
  23% of the machine and the server 76%, steady across the whole sweep — so the plateau is
  the server's, with roughly a 30% correction available if the load ever moved off-box.

### What that is in users

Capacity is a **rate**, and users are a rate divided by how often each of them asks. Two
measurements pin the arithmetic. At one client the machine spends 0.108 core-seconds per
query; at saturation, 0.118 — the same number from both ends, so this mix costs about
**0.11 core-seconds a query end to end**, ~0.09 of it server-side.

Per class, unloaded, the service times above give the shape:

| class | share of queries | service time | share of the CPU |
|---|---|---|---|
| point lookup | 80% | 193 µs | 0.3% |
| small scan | 15% | 1.8 ms | 0.5% |
| full scan | 4% | 70.9 ms | 5.1% |
| **join, whole db** | **1%** | **5.20 s** | **94%** |

**The mix decides capacity, by two orders of magnitude.** Drop the 1% join and the same
eight cores serve roughly 1,900 q/s instead of 67. So a bound on concurrent users is only
meaningful with the mix beside it:

| mix | capacity | users at 10 s think | users at 30 s think |
|---|---|---|---|
| as measured (1% whole-db join) | ~67 q/s | ~670 | ~2,000 |
| no whole-db join (4% full scans) | ~1,900 q/s | ~19,000 | ~57,000 |

Checked against a think-time sweep rather than left as arithmetic — 512, 1024 and 2048
users each thinking 10 s between queries:

| users | offered | achieved | point p50 | point p99 | CPU |
|---|---|---|---|---|---|
| 512 | 51 q/s | 41 q/s (81%) | 1.7 ms | 12.3 ms | 49% |
| 1024 | 102 q/s | 61 q/s (59%) | 2.6 ms | 16.5 ms | 83% |
| 2048 | 205 q/s | 64 q/s (31%) | 4.4 ms | 20.7 ms | 92% |

2048 connected users are **cheap** — 20.7 ms at the p99 a person actually notices, on a box
that is not full. What they cannot do is all ask at once.

One methodological trap, recorded because it will mislead somebody: **achieved-below-offered
is not saturation here.** At 512 users the server was at 49% CPU and still "missed" 19% of
the offered rate, because a user waiting 100 s inside a join is not issuing anything. When a
class's service time exceeds the think time, offered load is fiction and the honest reading
is the CPU column beside it.

## 7. F1 is real, and what it bounds is how long a *connection* lives — not how many users are served

The hypothesis: `read_loop`'s `streams: HashMap<u32, StreamHandle>` has no removal path, and
the client never reuses a stream id, so a connection issuing many queries accumulates parked
tasks. Nothing in the repo could measure it — `soak`'s mix means a client manages ~1,700
queries before a run ends, which turns out to be under the noise.

So: **one connection, 200,000 point lookups, the same key every time.**

```
                            RSS         threads
  before                    215 MB      13
  15 s in  (~80k queries)   501 MB      15
  30 s in (~160k queries)   781 MB      16
  45 s in (200k, finished)  911 MB      16
  60 s in (idle, open)      911 MB      13     <- stops the moment queries stop
  after close               904 MB      13     <- and is not returned
  round 2 on a NEW conn     1,412 MB    13     <- another 200k adds another 508 MB
```

**~3.5 kB retained per query**, growth strictly proportional to queries issued and flat the
instant they stop. It cannot be the block cache: the query is a point lookup for *one key*,
repeated, so there is nothing new to cache.

**It is retained for the life of the connection, and no longer.** Two independent checks,
because the first reading — RSS not falling on close — is exactly what a leak *and* an
allocator holding freed pages both look like.

*Successive long connections asymptote.* Three 200k-query connections in turn, each opened
after the last had closed:

| | RSS | added |
|---|---|---|
| start | 243 MB | |
| round 1 | 892 MB | +649 MB |
| round 2 | 1,306 MB | +421 MB |
| round 3 | 1,341 MB | **+35 MB** |

A leak adds the same every round. This reuses what the previous connection freed — the
process keeps a high-water mark rather than climbing.

*And a realistic population never gets near it.* S7 below: 145,582 queries over 50 minutes
across connections that live one segment each, and RSS moved 237 → 245 MB. **58 bytes per
query**, against 3,500 on one long-lived connection.

So the exposure is **the busiest single connection**, and what it sets is a high-water mark:

| client shape | queries per connection | retained while open |
|---|---|---|
| `aperture query`, one per process | 1 | nothing |
| `soak`'s users, reconnecting | ~45 | ~160 kB |
| a pooled connection held open at 67 q/s for an hour | 240k | **~840 MB** |

A connection pool is exactly the shape that hits the bottom row, and it is the one to size
RAM for. What this is *not* is a reason to restart on a schedule: the ceiling is per
connection and it comes back.

**Costed fix.** Remove the stream from `read_loop`'s map when its task completes, and let the
client reuse ids. Small, and squarely a correctness fix — 3.5 kB × queries-on-one-connection
is a real ceiling even if it is a bounded one. What is missing to *verify* one is the phase
plan's own task 10f: a `live stream tasks` counter would turn "RSS grew" into "N tasks are
live", and would have made this a five-minute test rather than the afternoon it took.

**F2 is refuted.** A cancel landing mid-chunk on the most stride-tripping query available —
56,274 rows examined per row produced — returns a clean end (256 rows drained), sends no
error frame, and leaves the connection usable for the next query. Tested through the client
API and through `aperture query --limit`; both are clean.

## 8. Steady state holds: fifty minutes, 145,582 queries, no drift

S7. A sub-knee rate on purpose — 256 users thinking 5 s, offering 51 q/s against a measured
capacity of ~67 — run in twelve four-minute segments so drift shows up as one segment
differing from the last rather than as an average over an hour.

| segment | 1 | 4 | 8 | 12 |
|---|---|---|---|---|
| point lookup p50 | 768 µs | 742 µs | 728 µs | **710 µs** |
| point lookup p99 | 6.2 ms | 6.2 ms | 5.5 ms | **5.6 ms** |
| small scan p50 | 4.4 ms | 4.5 ms | 4.3 ms | 4.3 ms |
| full scan p50 | 186.5 ms | 192.6 ms | 188.8 ms | 186.0 ms |
| achieved | 48 q/s | 48 q/s | 48 q/s | 48 q/s |
| server RSS | 237 MB | 243 MB | 243 MB | 245 MB |
| server threads | 68 | 76 | 76 | 79 |

Nothing moves. Latency at every percentile is flat or slightly better at the end than the
start, throughput is identical to the query across all twelve segments, and zero errors.
The two things that do creep are worth naming: **RSS +8 MB** (finding 7's per-connection
accumulation, at the rate short-lived connections produce) and **threads 68 → 79**, which is
the blocking pool settling rather than growing without bound — it moved 8 in the first
quarter of the run and 3 in the remaining three quarters.

**A paused-reader population costs latency and no throughput.** The same segment run again
with 400 extra clients that ask for everything and then stop reading:

| | baseline | +400 stalled |
|---|---|---|
| point lookup p50 / p99 | 710 µs / 5.6 ms | 812 µs / 6.3 ms |
| small scan p50 | 4.3 ms | 5.0 ms |
| full scan p50 | 186.0 ms | 212.0 ms |
| achieved | 48 q/s | 48 q/s |
| errors | 0 | 0 |

**+10–14% latency, 0% throughput.** Bounded per-stream queues plus the fair writer mean a
stream that will not drain is a stream that waits, holding neither a worker nor a blocking
thread — which is what the design says and what this measures.

What this does *not* cover, and would need a longer run: fjall compaction under a write
load, since this database is `Complete` and nothing compacts. The **disconnect storm** the
phase plan also asks for is finding 10, and it did not go so quietly.

## 9. F4 answered: encoding a row costs 1.5×, and the wire above it costs another 3.6×

The prediction was that per-row framing dominates above ~100k row/s. Splitting the wire
path in two — the executor alone, then the executor plus exactly what the server does per
row (`to_value`, `rows::to_wire`, `encode_value` into a fresh buffer, `session.rs:863`) —
puts most of the cost above the encoder rather than in it:

| workload | executor alone | + project & encode | tax | over the wire (1 client) |
|---|---|---|---|---|
| scan files | 2,476,335 row/s | 1,637,046 row/s | 1.5× | 461,000 row/s |
| scan decls | 2,275,416 row/s | 1,515,723 row/s | 1.5× | |
| scan lines | 2,759,872 row/s | 2,121,751 row/s | 1.3× | |
| project record | 2,293,536 row/s | 1,093,153 row/s | **2.1×** | |
| wide row | 2,110,274 row/s | 1,032,222 row/s | **2.0×** | |

So of the ~5.4× between the executor and the socket, **1.5× is the row encoding and 3.6× is
everything above it** — one `DATA_ROW` frame each, the outbound mutex, the `Notify`, the
socket, and the client's own decode. F4's mechanism is confirmed as significant but
misattributed: the encoder is not the dominant term, the framing and transport around it
are.

The two workloads that cost twice rather than 1.5× are the two that **build a record per
row**. Projection shape, not row count, is what makes a row expensive to hand out.

## 10. A client that disconnects mid-result leaked, permanently — the one that mattered  ✅ fixed

Not on the hypothesis list. Found by building S7's disconnect storm: clients that connect,
ask for a whole predicate, read two rows and vanish while the server is still producing.

```
530,717 connect-query-vanish cycles in 110 s   RSS  1,057 MB →  8,247 MB
515,926 more, on the same server, in 60 s      RSS  8,257 MB → 14,989 MB
```

**Linear, not asymptotic** — the second storm added the same per cycle as the first, which
is what separates a leak from a high-water mark. ~13.4 kB per abandoned query, never
returned. At the 8,400 cycles/s a single 16-thread client sustains, that is **65 MB/s**: a
32 GB server is dead in about eight minutes, and no privilege is required to do it.

Threads and file descriptors both return to their idle values afterwards, so nothing is
holding a task or a socket. It is memory alone.

### Three arms, because "abandoned" and "large" and "churning" are three different claims

Each from a freshly started server, 60 s, same 16 threads, same box:

| arm | cycles | RSS after | per cycle |
|---|---|---|---|
| point lookup, drained to the end | 1,309,617 | 129 MB | 8 bytes |
| **32,710-row result, drained to the end** | 2,556 | 143 MB | high-water: **flat at 149 MB** when run for 5× longer |
| **32,710-row result, abandoned after 2 rows** | 502,564 | **6,879 MB** | **13.4 kB, linear** |

So it is not connection churn — 1.3M connect-query-close cycles cost 10 MB. It is not result
size — the *same* 32,710-row query, read to the end, plateaus at 149 MB and stays there
through a nine-minute run. It is **abandonment**: ending the connection while the server
still has rows to send.

This is also the most common client behaviour there is. A `Ctrl-C` at the shell, a crashed
consumer, a proxy timing out, `--limit` on a page the user never scrolls — all of them are
this. It is worth saying that the *graceful* cancel path is clean (finding 7's F2 result):
`connection.cancel` ends the stream properly and leaks nothing. What leaks is the socket
disappearing without one.

### Fixed — and the obvious fix was the wrong one

Instrumenting the server settled where the memory was: **live stream tasks, climbing without
bound.** 383,121 abandoned connections left **113,336 tasks alive** while live *sessions* sat
at 14 — so connections were tearing down correctly and their tasks were not.

The obvious cause is the one F1 named: `StreamHandle` holds a `CancellationToken`, and
dropping a token does not cancel it, so nothing tells an in-flight query that its client has
gone. Adding `Drop for StreamHandle { self.cancel.cancel() }` is right and is kept — a doomed
query otherwise runs to completion, queueing a blocking-pool job per chunk for a result
nobody will read. **It also fixed nothing:** 106,215 tasks still stuck.

A counter per await site said why. Every stuck task was parked in the same place:

```
diag: conns 122951 · streams 37783 · sessions 0 || recv 0 prepare 0 desc 0 chunk 0 send 37783
```

**All of them in `Outbound::send`, waiting for queue room.** The writer task is the only
thing that ever frees a slot. When the client vanishes, the socket write fails, the writer
ends *on the spot* — and `close()` notified `work` (which the writer watches) but never
`room` (which producers watch). Every stream still answering parked on a queue nobody would
ever drain, holding its chunk, its plan and a database handle for the life of the process.

Two lines of fix, in `outbound.rs` and `session.rs`:

- **`close()` wakes the waiters** — `room.notify_waiters()` alongside the existing `work`
  notification. `notify_waiters` deliberately stores no permit; a producer arriving later
  finds `closed` under the lock instead.
- **`send()` registers its wait under the lock**, via `Notified::enable()`. A `Notified` does
  not register until first polled, so creating it after releasing the lock left a window in
  which a `close` could wake every waiter and miss this one.
- **The writer announces its own death** — the pump calls `close()` when `outbound_run`
  returns, rather than leaving it until `read_loop` notices, which covers a half-closed peer.

Measured, same probe, same box:

| | before | after |
|---|---|---|
| 3 storms, 781,399 abandoned queries | +7 GB each, linear | **119 → 141 → 144 → 144 MB** |
| live stream tasks afterwards | 113,336 | **0** |
| per abandoned query | 13.4 kB, permanent | ~58 bytes, asymptotic |

**Guard tests, at two levels.** In `outbound.rs`,
`closing_releases_producers_waiting_for_room` fills a stream's queue, asserts the producer
is genuinely parked, closes, and requires it to be released *and* refused — with a timeout,
because the regression is a hang rather than a failure. Reverting `notify_waiters` makes it
fail in 5 s. A second test covers the arrive-after-close path, which `notify_waiters` cannot
wake by design.

Above that, `tests/a_vanished_client.rs` asserts the thing a *server* owes rather than the
mechanism: sixteen clients ask for everything, read two frames, and drop the socket; live
stream tasks must return to zero and the server must still answer in full. Two things it
learned the hard way, both recorded in the file:

- **One client is not enough.** The defect needs the producer to fill its queue before the
  reader notices EOF, and with a single client that lands the wrong way about 40% of the
  time — the first version of this test passed against the unfixed server. Sixteen makes a
  clean sweep about a one-in-two-million event; against the unfixed server three runs
  stranded 7, 13 and 11 of 16.
- **The client has to read before it vanishes.** A socket dropped the instant after the
  query is written takes the query with it: the server never reads the frame, never starts,
  and there is no in-flight work to strand. An earlier version did exactly that and passed
  while measuring nothing — the counters said `queries_started = 0`.

**What did not change**: 100 paused readers still block rather than erroring (a stalled
client's connection is alive, so its producer waits, re-checks `closed`, and waits again),
throughput on both workloads is unchanged, and the whole suite is green at 608.

## 11. The code-search workload: ~6,000 q/s, and the product's own traffic is nothing like `soak`'s

`examples/codesearch.rs`, written against a stated product: search by name, results paged to
50–100, and **no search term means no query** — so the UI never asks this database for an
unbounded scan. Every query is a prefix seek into `src.SearchByName`, whose key leads with
`name` for exactly this reason. Terms are sampled from the corpus and truncated, so a
type-ahead burst is short prefixes matching thousands and a considered search is a long one
matching a few.

| class | weight | what it is |
|---|---|---|
| typeahead | 70% | 2–4 char prefix, one query per keystroke |
| considered search | 20% | 5–12 char prefix |
| search + locations | 8% | the same, rendering where each hit is — a fetch per row |
| open symbol | 2% | clicking a hit: exact name |

### Capacity

Saturation, page 50, no think time, against the sealed 18.2M-fact index:

| in flight | achieved | typeahead p50 | typeahead p99 | CPU (generator / server) |
|---|---|---|---|---|
| 8 | 3,580 q/s | 2.3 ms | 5.5 ms | 88% (33 / 54) |
| 32 | 4,676 q/s | 6.7 ms | 20.8 ms | 98% (41 / 57) |
| 128 | 6,094 q/s | 24.5 ms | 47.7 ms | 98% (42 / 57) |

**~6,100 q/s, against 67 q/s for `soak`'s generic mix — ninety times more.** All of that
difference is the one class `soak` has and a search UI does not: a query with no bound on
what it reads.

The generator is now a co-equal consumer — 42% of the box against the server's 57% — so this
understates the server. Per query the *server* spends **0.75 ms of CPU**, which is
**~10,700 q/s** if the load came from another machine. Treat 6,000 as measured and 10,000 as
the thing to confirm with a second host.

### In users

| users | think | achieved | typeahead p50 | p99 | CPU |
|---|---|---|---|---|---|
| 512 | 3 s | 170 q/s | 2.0 ms | 45.9 ms | 6% |
| 2,048 | 3 s | 674 q/s | 3.1 ms | 145.7 ms | 16% |

2,048 users at a 3-second think time is **16% of this box**, at a p50 of 3.1 ms. Saturation
would be around **13,000–14,000 users at 3 s**, or ~45,000 at 10 s. For tail latency rather
than throughput, half of that is the number to plan around: **~6,000 users per instance at
3 s think**, which leaves headroom for the p99 to stay tens of milliseconds.

One caveat on those tails: at 2,048 users the generator is 2,048 OS threads on 8 shared
cores, and a p99 of 146 ms at 16% CPU is mostly its own scheduling. The p50 is the trustworthy
column at high user counts.

### Page size is a dial, not a free choice

The obvious guess — that a page below `CHUNK_ROWS = 256` is free, since the executor computes
a whole chunk anyway — is **wrong**, and worth knowing before picking 50 or 100:

| page | rows returned | typeahead p50 | achieved |
|---|---|---|---|
| 50 | 48 | 6.1 ms | 5,118 q/s |
| 100 | 93 | 7.1 ms | 4,489 q/s |
| 256 | 226 | 10.5 ms | 3,274 q/s |
| 500 | 417 | 14.4 ms | 2,508 q/s |

Roughly linear in rows *delivered*, because the dominant per-row cost is not the executor's —
it is the framing, socket and client decode of finding 9, and cancelling stops paying it.
**Page 100 costs 12% of throughput against page 50**; page 256 costs 36%. Bigger pages are
cheaper per row (245k row/s at 50, 1.05M at 500) and dearer per query.

### Two things this workload does *not* suffer

- **No leak.** Paging cancels *gracefully*, which is finding 10's clean path. Across ~900,000
  paged queries the server went 756 → 792 → 797 MB — a high-water mark, asymptotic, not the
  13.4 kB-per-query bleed an abandoned result causes. The disconnect leak still matters for a
  web tier (a browser that goes away mid-request), but normal paging does not trigger it.
- **No head-of-line problem.** Without a whole-database query in the mix there is nothing for
  a cheap query to queue behind, which is why p50 barely moves until the box is full.

### The blocker: find-references cannot be served

The second thing anyone wants from code search, and it is not answerable:

```
{f = R.file, l = R.at.line} where src.SearchByName {name = "OCTET_LENGTH", to = D};
                                  R = src.Ref {to = D}

  src.SearchByName        1
  src.Ref           4,879,151   full scan          →  2.21 s, for ONE declaration
```

`src.Ref`'s key is `{at, file, to}` — `at` first, `to` last — so a lookup *by target* cannot
seek and reads every reference in the database. For a common name like `Parse`, which many
declarations share, it is that scan once per declaration: the query did not return three rows
in five minutes.

This is finding 2 landing on the product's most valuable query. The fix is the same one and
it is cheap **now**: declare `src.Ref`'s key `{to, file, at}` so the target leads. That is one
line in `code_index.rs` plus a re-index — and it is a different, worse conversation once
somebody's index is the one in production.

**Done** — see finding 2's fix. `src.Ref` leads with its target, so the negation form of
the same question (`!src.Ref {to = D}`, "what does nothing use") compiles to
`seek[to = r0#, file = _, at = _]` where it read the whole predicate before. The number
above is not re-measured: it needs a re-index of `dotnet/runtime`, which is hours and must
not run while anything else is being measured. What is checked is the plan, which is where
the 4.9M-row scan came from.

---

## 12. Ingest is 5.2k facts/s, and the write path was never the reason — three quarters of the work was re-reading, and half the wall clock was the producer waiting

The one number [glean-capabilities §2.3](../docs/glean-capabilities.md) said "nothing in
`bench/FINDINGS.md` yet attributes". Attributed here. **Not an S-rung measurement** — there is no
write-path instrument yet — but read off counters the indexer already reports, on a 25M-fact
`dotnet/runtime` index: a larger run than §1's `--syntax-only` 18.2M-fact one, reaching the build
and declaration layers as well as the source layer.

| | |
|---|---|
| Facts created | **25.0M** |
| Facts *interned* to create them | **94.9M** — so **73.6% of the work was re-reading something already present** |
| Point reads per intern | **2** (`keys` for the id, `entities` to compare the value) |
| Wall clock | **4,828 s** ⇒ **5.2k facts/s** |
| Inside the blocking `Write` call | **2,255 s** — 47% of the run |
| Per block (`--batch 4000`) | **368 ms** |

Derived from those two timings rather than measured separately: ~6,100 blocks, ~24.5M top-level
facts, ~92 µs per top-level fact, ~24 µs per intern, ~12 µs per point read.

**Two mechanisms, and the bigger one is not in the database.** The `Write` call happened while the
walk held `Indexer._gate`, so during those 2,255 s the other seven walker threads were blocked:
the write path was serialised **with** the producer rather than alongside it. That is a pipelining
defect in the client and it is worth about 1.9× on its own (`4,828 → ~2,600 s`, `5.2k → ~9.7k
facts/s`) by handing full blocks to a writer thread behind a bounded queue.

The second mechanism is the 73.6%. It is genuine work, not waiting, and it is what a
[lookup cache](../crates/aperture-store/src/lookup_cache.rs) and skipping the `entities` read on
key-only predicates (22 of 27) remove.

**What the arithmetic then says, and it is the finding that mattered.** After pipelining, the two
sides are nearly balanced — ~2,255 s of server intern against ~2,573 s of walk. So the single
writer is *not* today's binding constraint, and cutting its work further buys headroom rather than
wall clock. **The reason to make it parallel anyway is not throughput** — it is that the write-once
half of [I12](../docs/invariants.md#i12) was being held by there being one thread rather than by a
mechanism, and that only becomes visible when you go looking for the throughput. See
[Phase 12](../PLAN.md#phase-12--parallel-ingestion-the-striped-merge-frontier).

Two things to fix in how this was measured, before it is measured again:

- **`Writing` stops being comparable to wall clock** once the writer overlaps the walk. The number
  that answers "is the writer the ceiling?" is then `Queueing` — time producers spent blocked on a
  full queue — and it did not exist when this was taken.
- **A hit-rate counter is not optional.** A cache that is not hitting is indistinguishable from
  one that is absent, except in a line that prints hits and misses.

---

## 13. The write rung: committing is 41% of interning, and the cache is worth 23% of a resolve pass

The instrument finding 12 said was missing. `cargo run --release --example ingest` writes a
**synthetic** corpus shaped like the source layer of the built-in schema — a reference nests
a declaration nests a module nests a file — in process, no tokio, no wire, no server. Four
fanouts set the interns-per-fact ratio, which is the quantity interning's cost is decided
by; the default sits at 4.63 against the real index's 3.8.

40 files × 2 modules × 20 decls × 5 refs = 9,720 facts, 45,000 interns, best of 2:

| layer | ms | facts/s | interns/s | reads/fact | cache |
|---|---|---|---|---|---|
| `commit` | 30.2 | 321,968 | — | 0.00 | — |
| `create` | 58.2 | 166,914 | 772,750 | 1.00 | 78.4% |
| `dedup:warm` | 26.5 | — | 1,698,325 | 0.00 | 100.0% |
| `dedup:cold` | 34.4 | — | 1,309,655 | 0.22 | 78.4% |
| `block:create` | 76.7 | 126,792 | 587,001 | 1.00 | 78.4% |

The rows are there to be subtracted:

- **Committing is 23.9 ms of `create`'s 58.2 ms — 41%.** `create` resolves and writes;
  `dedup:cold` resolves the identical corpus against a cold cache and writes *nothing*. The
  difference is 9,720 `put_fact` calls, each its own fjall batch through one global journal
  mutex. This is the number [12f](../PLAN.md) exists for, and it is larger than expected.
- **The cache is worth 7.9 ms of a 34.4 ms resolve pass — 23%.** `dedup:cold` starts with an
  empty cache and reads the LSM once per distinct key; `dedup:warm` answers all 45,000 from
  memory. Both do zero commits, so nothing else differs.
- **Block decode is 18.4 ms per pass, ~32% on top of `create`.** The transport codec is not
  free at these rates, and it is the one term the read ladder already had an opinion about
  (finding 9's 1.5× row encoder, in the other direction).
- **`reads/fact` is 1.00 on both create rows**, which is
  [12c's guard](../crates/aperture-ingest/tests/against_a_real_store.rs) priced rather than
  counted: one live `keys` read per distinct key across four levels of nesting, where 4.63
  interns per fact would otherwise mean 4.63 reads.

Two independent estimates of commit cost agree: the `commit` floor writes 9,720 scalar keys
in 30.2 ms, and `create − dedup:cold` puts the same 9,720 commits at 23.9 ms. Different
key shapes, ~25% apart, same order — which is the cross-check that says neither row is
measuring something else.

**After the striped merge frontier (12d), on the identical command.** `create` 58.2 → 59.0 ms,
`dedup:warm` 26.5 → 27.5 ms, `dedup:cold` 34.4 → 34.8 ms, `commit` 30.2 → 29.8 ms. So per-key
exclusion costs **1–4% single-threaded**, and the largest share lands on `dedup:warm` — the row that
is nothing but hash-and-look-up, with no LSM read and no commit under it. That is the signature the
theory predicts, since what the frontier adds to a single writer is exactly one FNV pass over the
key. `commit` is unchanged, as it must be: `put_fact` is below the funnel and takes no stripe. The
run-to-run spread here is ~1 ms, so read these as "a few percent", not as three significant figures.

**With `--per-block` (12f), on the same command.** `create` 61.1 → **48.4 ms**, 159,026 →
**200,861 facts/s** — a 21% cut, and the *committing* term halves rather than vanishing (25.8 →
12.7 ms). Staging is not free: the rows are still built and a pending map is still filled; what goes
away is 9,720 journal-mutex acquisitions collapsing into one. So the honest headline for the flag is
**~20% off a create pass**, not the 41% that "committing is 41% of interning" invites — the 41% is
what committing costs, not what batching removes.

One reading artefact worth knowing: the `cache` column shows **0.0%** on a `--per-block` create.
Nothing regressed. A staged block answers its own repeats from the batch's pending map, which is
consulted *before* the stripe cache, so the cache is never asked and its hit counter never moves.
The two modes' cache columns are not comparable, and the dedup rows — which stage nothing — are.

**What this is not.** It is not finding 12 re-measured: that needs the 25M-fact
`dotnet/runtime` index rebuilt, which is hours and must not run while anything else is
being measured. It is a *baseline on a synthetic corpus*, and its value is that the terms
are now separable — a facts/s number can be attributed to committing, resolving, reading or
decoding instead of to "the write path". Also absent: the wire and the server. The rung
stops below them deliberately, and the layer that adds them is what finding 12's 47%-inside-
`Write` figure belongs to.

---

## What is still open

- **F6** — the reader head-of-line blocking on a ≥3-block ingest. The only hypothesis left
  untouched, and the only one that needs a *write* path: every database measured here is
  `Complete`. It wants a writable database, a slow funnel and a client that sends three
  blocks without waiting.
- **F4's last 3.6×** is split no further. Finding 9 separates the row encoder from
  everything above it, but "everything above it" is still one number covering the frame, the
  outbound mutex, the socket and the client's decode. Splitting *that* is what S4 proper —
  `breakdown.rs` extended to a data query — would do.
- **F8** (no admission control) is *observed* rather than measured: 2048 connections were
  accepted without complaint and nothing was ever refused, which is the predicted behaviour.
  Latency, never rejection.
- **F5** — blocked on finding 4, as above.
- **The scaling curve across *corpus sizes*** rather than across predicates. What is
  published here is one 18M-fact database whose predicates span 142 → 8.58M rows, which is
  a scan-size curve on real data at fixed database size. The 10k → 10M curve the phase
  plan asks for needs `index-repo.sh --max-files` bands, and each band is hours of
  indexing that must not run while anything is being measured.
- **No baseline file.** `bench/baselines/<host>.json` and the `--json` flag are not built;
  these numbers live in this document and are reproduced by re-running the instrument.
- **The write rung stops below the wire.** Finding 13 built it and it separates committing from
  resolving from decoding — but in process only. The server, the framing and the per-stream
  queueing are not in it, so finding 12's "47% of the run inside the blocking `Write` call" is
  still attributed to a call rather than to a layer. That wants the same treatment S4/S5 gave
  the read path, over a write stream.
- **Finding 13 is a synthetic corpus.** The ratio is dialled to bracket the real one, not taken
  from it. Re-measuring finding 12 through the rung needs a `dotnet/runtime` re-index — hours,
  and it must not run while anything else is being measured.
