# 3. The storage model

> [Aperture design book](../README.md) · [← 2. The tuple codec](02-tuple-codec.md) · **Chapter 3** · [4. The executor →](04-executor.md)

This chapter is how facts live on disk: the two column families, one keyspace per predicate
for each of them, how a `FactId` is allocated (and why it is a snowflake), and why the two
halves of a fact are always written together. It builds directly on the
[order-preserving codec](02-tuple-codec.md).

Backend: **fjall**, an LSM key–value store. The `FactStore` trait (`src/focus/plan.rs`) is
the seam; an in-memory `MemStore` (`src/focus/mem_store.rs`) implements it for tests
*only*.

---

## Two column families

A fact is stored in two sorted key–value maps ("column families" in fjall terms), each
answering a different question:

### `keys` — the index (answers "which facts match?")

```
predicate_id (4B big-endian) ++ tuple_encoded_key   →   fact_id (8B big-endian)
```

Sorted lexicographically. Because the codec is order-preserving
([I1](invariants.md#i1)), a **prefix scan over this map is a predicate query**. To find all
facts of predicate `P`, scan the half-open byte range `[P, strinc(P))`:

- `strinc(prefix)` is the **prefix-successor** — the smallest byte string greater than
  every string with that prefix. Compute it by incrementing the last non-`0xFF` byte and
  dropping trailing `0xFF`s; an empty or all-`0xFF` prefix means "unbounded upper."
- Seeking to a *value* within the predicate just extends the prefix with more encoded key
  fields. This is how a query narrows a scan (see [sargeability](07-compilation.md)).

**The scan hot loop touches only `keys`.** Nothing else. That is a load-bearing
performance property — see [I6](invariants.md#i6) below and [chapter 4](04-executor.md).

### `entities` — identity (answers "what is this fact?")

```
fact_id (8B big-endian)   →   [key_len u32 BE][full stored key][value bytes]
```

A point lookup by `FactId`. It is **self-describing**: it carries its own key length and
bytes, then the value. You read from here only when a query genuinely needs a fact's value
(a `Project::Value` in the plan, or cross-fact navigation) — never during scanning. The id
names the predicate whose tree holds the row ([snowflake `FactId`](#factid-allocation-i11)),
so this stays a single lookup even though the CF is split per predicate.

### Why two, not one

The split is the physical expression of "keys are for finding, values are for fetching."
Keeping values out of `keys` means the index stays dense and the scan loop reads only what
it needs to *filter*, deferring the (potentially large) value until a row actually
survives to projection. This is [invariant I6](invariants.md#i6):

> **I6 — values never enter the scan hot loop.** Residuals on *key* fields are checked
> during the scan against `keys` only. A value is fetched from `entities` only when
> explicitly projected or navigated. Value patterns, when added, are a distinct residual
> class over the fetched value buffer — never pushed into the scan.
>
> *Guard:* `exec::no_value_fetch_in_scan` — a store spy fails if `point()` is called while
> running a key-only query.

---

## One keyspace per predicate — for *both* column families

Each predicate gets its **own pair of fjall keyspaces** (physical trees), `keys.<id>` and
`entities.<id>`, with the predicate id still carried as the key prefix in `keys`. Three
consequences, all load-bearing:

- **Physical isolation.** Facts of different predicates cannot affect each other's storage.
  This is what makes bulk ingestion embarrassingly parallel — per-predicate ingests are
  independent trees that can overlap freely (see [Operations](aperture-cli-design.md) and
  [ops-I8](invariants.md#ops-i8)). It is not merely a nicety: fjall's bulk `ingest()`
  requires *strictly ascending* keys, so a single shared tree would force one globally
  ordered serial sink, and concurrent ingestions into it would pile up overlapping L0 runs
  and serialise on the tree's flush lock.
- **Prefix-disjointness aligns with isolation.** "Predicate id is the key prefix" and "one
  tree per predicate" say the same thing at two levels, so a deriver reading predicate A
  and writing predicate B is structurally read/write-disjoint.
- **A predicate can be dropped or replaced wholesale**, in O(1), by deleting its two trees —
  which is what re-deriving a derived predicate needs. In a shared tree that would mean
  range tombstones in a store whose premise is that nothing is ever deleted. Note this is
  why `entities` is split too: were it shared, dropping a derived predicate's `keys` tree
  would strand its values as unreclaimable garbage.

Splitting `entities` is only possible because a [`FactId` is tagged with its predicate](#factid-allocation-i11):
`point()` is handed a bare id and no predicate, so an untagged id would turn identity lookup
into a search across every predicate's tree.

### What it costs

Per-keyspace overhead is real and worth stating in numbers rather than adjectives (measured
against fjall 3.1.5, release build, N keyspaces vs. one holding the same rows):

| | per-predicate | single tree |
|---|---|---|
| create a keyspace | **~30 ms** each (directory create + fsyncs) | once |
| reopen a DB | ~0.2 ms × #keyspaces (1024 → 214 ms) | 41 ms |
| fixed on-disk cost | 2 files + 2 dirs + ~2 KB per keyspace | 8 files |
| write throughput | no penalty (marginally better) | — |

Two obligations follow, and both are choices this design makes deliberately:

- **Create a predicate's trees up front, from the schema, not lazily on first write.** At
  ~30 ms a tree, lazy creation drops an fsync-bound stall into the middle of an ingest at an
  unpredictable moment. A DB is created from a known schema, so the whole bill is payable
  once at `create` (`FjallDb::create_predicates`).
- **Set `max_memtable_size` explicitly.** fjall's default is 64 MiB *per keyspace*, and the
  database-level write-buffer cap defaults to unset — so write memory would otherwise scale
  with how many predicates are being written at once, times two.

What is *shared* across all keyspaces, and therefore costs nothing extra: the journal (so a
cross-keyspace write batch is atomic — which [I12](#the-atomic-two-cf-write-i12) relies on,
since a fact's two halves live in two keyspaces either way), the block cache, the file
descriptor table, and the sequence-number/snapshot tracker (so a `Snapshot` is
cross-keyspace and consistent, making [I8](invariants.md#i8) identical under either layout).

**The seam is narrow enough to reverse.** The executor reaches the store only through
`FactStore::scan(lo, hi)`, whose bounds already carry the 4-byte predicate prefix, so
"one tree per predicate" versus "one tree, prefix-partitioned" is a change inside
`src/focus/store.rs` and nowhere else. The `FactId` layout below is the part that is *not*
reversible once data exists.

---

## FactId allocation ([I11](invariants.md#i11))

Every fact gets a `FactId` — a `u64` — assigned once at ingest. It is a **snowflake**: the
owning predicate in the high 24 bits, a per-predicate sequence in the low 40.

```
   63          40 39                                    0
  ┌──────────────┬───────────────────────────────────────┐
  │ predicate id │ sequence within that predicate        │
  │   (24 bits)  │              (40 bits)                │
  └──────────────┴───────────────────────────────────────┘
    ≤ 16.7 M predicates      ≤ 1.1 T facts per predicate
```

The split is byte-aligned on purpose: the tag is the top *three bytes* of the big-endian
encoding, so routing a lookup to a predicate's tree is a slice, not arithmetic.

Three things follow from the tag, and they are the reason for it:

- **`entities` can be split per predicate** (above) while `point()` stays one lookup from a
  bare id.
- **There is no global allocator.** Each predicate counts its own facts, so two ingest
  workers on different predicates share no counter and write disjoint, ascending id ranges —
  which is exactly what fjall's ascending-only bulk `ingest()` wants.
- **Uniqueness across predicates is structural**, not enforced: the tag partitions the id
  space, so two predicates cannot collide however their sequences are allocated.

**Sequence 0 is reserved**, so no valid id is `FactId(0)`: a zeroed or corrupt eight bytes is
detectably not a fact, which is worth having on a path where I11 is what makes a bytes-only
resume cursor safe.

**The high-water mark is recovered from the data, not from a sidecar counter.** An `entities`
key *is* a fact id, big-endian, so the last key in a predicate's `entities` tree is its
high-water mark. A separately persisted counter could be stale after a crash and reissue a
live id; a derived one cannot disagree with what is stored.

Sequences are **unique and never reused, but not dense**: an id consumed by a write that then
fails is not handed out again.

> **I11 — `FactId` is stable, unique, and never reused within a DB.** Unique within a DB
> (structurally, via the predicate tag), monotonic within a predicate, never reused (there is
> no deletion — the DB is immutable), stable for the DB's lifetime.
>
> *Guard:* `store::factid_unique_monotonic` — ids are tagged for their predicate, strictly
> increasing within it, unique under concurrent writers, and resumed *above* the high-water
> mark after a reopen or a crash. Plus `store::exhausted_sequence_space_is_an_error`: the
> space is finite and fails closed rather than wrapping into another predicate's tag.

**Consequence for the schema: a predicate id must fit 24 bits.** Phase 8's schema loader owns
that validation; until then the store rejects an untaggable predicate id at the point it
would create the predicate's trees, rather than at the first write
(`store::untaggable_predicate_is_rejected`).

Why it must hold:

- The **scan → point mapping** (`keys` → `fact_id` → `entities`) is a *total function* only
  if fact-ids are unique and stable.
- **Resume's integrity check** ([I4](invariants.md#i4)) compares a saved `fact_id` against
  the one re-read after suspend; if ids could shift or be reused, a bytes-only cursor would
  be unsafe. See [chapter 5](05-resume.md).

**`FactId` is a *physical* row id, not cross-DB identity.** Two DBs built from identical
inputs are considered identical by a *content hash* (`hash(canonical schema, base facts)`,
[ops-I4](invariants.md#ops-i4)), **not** by fact-id equality. Reproducibility comes from
the deterministic merge during ingestion, not from fact-ids matching across builds. The
per-predicate counter is the seam that makes concurrent ingestion safe.

---

## The atomic two-CF write ([I12](invariants.md#i12))

`keys` and `entities` are the two halves of one fact. They are written in a **single fjall
write batch**.

> **I12 — a fact is written to both column families atomically.** A fact is never
> half-present: never a key without its entity, never an entity without its key.
>
> *Guard:* `store::no_half_present_facts_after_writes` — over generated writes, the two
> column families are in exact bijection and every indexed key matches the key stored in its
> entity. Plus `store::no_half_present_facts`, the crash case: a child process is aborted
> mid-write at an uncontrolled point, and after recovery the bijection must still hold — a
> torn batch has to come back whole or not at all.

A half-present fact is silent corruption, and the two directions fail differently: a `keys`
entry with no `entities` row makes the scan→point lookup return nothing exactly when a query
projects that fact's value (surfacing as `DanglingFactId`), while an entity with no `keys` row
is invisible to every query — silent, and undetectable without checking both directions. The
batch is the only thing standing between "immutable, self-consistent store" and
"mostly-consistent store." Writing one CF without the other, or outside a batch, is an
[anti-pattern](conventions.md).

---

## Snapshots and immutability

A query reads a consistent **snapshot**. For a Complete (immutable) DB this is trivial —
nothing changes under you. But fjall iterators pin a read snapshot while they live, and
that snapshot keeps LSM blocks (and a whole generation of superseded data) alive. So the
rule ([I8](invariants.md#i8), detailed in [chapter 5](05-resume.md)):

> **drop the executor — and its iterators — at suspend**, so no snapshot is held across an
> idle portal.

`Executor::enumerate` takes `self` by value, so this is what the signature does rather than
what a caller must remember: every exit path consumes the executor and releases the snapshot.

This is why resume is designed to reconstruct state from **bytes** rather than keep a live
iterator around. The storage layer's job is to make that reconstruction sound: a saved key
still resolving to the same fact is exactly what [I11](invariants.md#i11) and the immutable
snapshot guarantee.

---

## Storage codec vs transport codec

Worth stating once, clearly (it recurs):

- **Storage (tuple) codec** — [chapter 2](02-tuple-codec.md). Order-preserving,
  self-delimiting. Encodes **both keys *and* values** (values are tuple-encoded too, so
  queries can eventually *match on values*, and `Project::Value` becomes decode-not-copy).
- **Transport / wire codec** — a separate, framed binary format applied only to rows
  *after* they leave the executor (post-yield). **Not** order-preserving, never touches
  stored bytes. Details in [Operations](aperture-cli-design.md).

Don't blur them: a constraint that applies to one (order-preservation, self-delimiting)
does not apply to the other.

---

## Invariants owned by this chapter

| # | Statement | Guard test |
|---|-----------|------------|
| [I6](invariants.md#i6) | Values never enter the scan hot loop. | `exec::no_value_fetch_in_scan` (store spy) |
| [I11](invariants.md#i11) | `FactId` is stable, unique, never reused within a DB. | `store::factid_unique_monotonic`, `store::exhausted_sequence_space_is_an_error` |
| [I12](invariants.md#i12) | A fact is written to both column families atomically. | `store::no_half_present_facts_after_writes`, `store::no_half_present_facts` (crash) |

I6 is stated here because it's a *storage-shape* decision, and enforced in the
[executor](04-executor.md).

---

> **Reading path:** [← 2. The tuple codec](02-tuple-codec.md) · **3. The storage model** · [4. The executor →](04-executor.md)
