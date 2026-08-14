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

Sorted lexicographically, so a **prefix scan over this map is a predicate query**. What that asks
of the codec is only that a value have exactly one encoding — the
[canonicalising decoder](02-tuple-codec.md#property-1--order-preserving-i1) — so that every
matching row carries the same leading bytes. Order-preservation ([I1](invariants.md#i1)) is what
turns a *value range* into a byte range, and that half is
[not spent yet](#the-order-a-scan-is-promised-in). To find all facts of predicate `P`, scan the
half-open byte range `[P, strinc(P))`:

- `strinc(prefix)` is the **prefix-successor** — the smallest byte string greater than
  every string with that prefix. Compute it by incrementing the last non-`0xFF` byte and
  dropping trailing `0xFF`s; an empty or all-`0xFF` prefix means "unbounded upper."
- Seeking to a *value* within the predicate just extends the prefix with more encoded key
  fields. This is how a query narrows a scan (see [sargeability](07-compilation.md)).

**The scan hot loop touches only `keys`.** Nothing else. That is a load-bearing
performance property — see [I6](invariants.md#i6) below and [chapter 4](04-executor.md).

<a id="a-stored-key-is-flat"></a>
#### A stored key is **flat**: its top-level fields, back to back

`tuple_encoded_key` above is the key type's top-level fields concatenated, with **no record
wrapper of its own** — even when the key type is a record. A record *inside* a field does
keep its `MARK_RECORD … MARK_TERM` wrapper, because there it is one value among others and
has to be skippable as one ([chapter 2](02-tuple-codec.md#records-and-the-nullterminator-subtlety)).

Three things rest on that asymmetry, which is why it is written down rather than left to the
encoder that happens to run first:

- **A seek extends a prefix by whole fields** (above). With a wrapper, every seek would carry
  a constant leading byte and no seek could stop before the terminator.
- **Field *k* costs *k* skips**, which is what the executor's field-offset cache holds. Under
  a wrapper the top level has exactly one field, and every key-field read would be a nested
  walk the cache cannot serve.
- **A key is therefore not *a* field.** A plan addresses key fields with a
  [`FieldPath`](07-compilation.md#flatten--the-crux) — a top-level field plus a step per
  record nested inside it — and there is no path that names a whole record key. Projecting one
  is a record over its fields; binding a variable to one is `nyi/whole-key`.

Settled in Phase 4, because flatten is the first thing that has to *choose*: the executor
never learns which convention wrote a row, so both encodings "work" until a plan reads a
field, and then one of them reads the wrong bytes silently. Pinned by
`codec::a_stored_key_is_its_fields_with_no_wrapper_of_its_own` and by `tuple::decode_key`,
which is how a whole key is read back (`decode_typed` reads a field or a value).

### `entities` — identity (answers "what is this fact?")

```
fact_id (8B big-endian)   →   [key_len u32 BE][full stored key][value bytes]
```

A point lookup by `FactId`. It is **self-describing**: it carries its own key length and
bytes, then the value. You read from here only when a query genuinely needs a fact's value
(a `Project::Value` in the plan, or cross-fact navigation) — never during scanning. The id
names the predicate whose tree holds the row ([snowflake `FactId`](#factid-allocation-i11)),
so this stays a single lookup even though the CF is split per predicate.

**What it costs is that the key is stored twice** — once as the `keys` row that indexes it, once
inside the `entities` row that identifies it. Glean's layout has the identical duplication and
names it as its main space defect, worst exactly where keys are large: file paths and symbol
names, which is the example corpus's entire profile. Glean's fix is to store a *truncated* key in
`keys` and re-check the full one from `entities` — available to it because it gave up ordered
iteration, and [not available here](#the-order-a-scan-is-promised-in).

### Why two, not one

The split is the physical expression of "keys are for finding, values are for fetching."
Keeping values out of `keys` means the index stays dense and the scan loop reads only what
it needs to *filter*, deferring the (potentially large) value until a row actually
survives to projection. The layout is **Glean's, down to the names** — its two fact-bearing
column families are called `keys` and `entities` and point the same two ways — and it is adopted
on purpose, not arrived at ([the comparison](glean-comparison.md) has the side-by-side). What is
*not* adopted is the discipline on top of it, [invariant I6](invariants.md#i6):

> **I6 — values never enter the scan hot loop.** Residuals on *key* fields are checked
> during the scan against `keys` only. A value is fetched from `entities` only when
> explicitly projected or navigated. Value patterns, when added, are a distinct residual
> class over the fetched value buffer — never pushed into the scan.
>
> *Guard:* `exec::no_value_fetch_in_scan` — a store spy fails if `point()` is called while
> running a key-only query.

**I6 is a strengthening, and it is ours.** Glean has the same key-only/key-value choice at its
iterator — a scan can be asked for the index row alone — but where a query *matches* on a value
it marks the seek as needing one, and then fetches the value for every row the scan **examines**:
a second store lookup per row, which its own interface documents as the expensive mode. That is
precisely what I6 forbids. Affording the stricter rule is partly luck of sequencing — value
patterns are deferred (`nyi/value-match`), so the query that tempts a fetch into the loop cannot
be written yet — and I6 is what keeps it out when it can.

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

**Glean's split is global where this one is physical.** One `keys` family and one `entities`
family hold every predicate; the predicate is an 8-byte key prefix in `keys` only, with RocksDB
told about it through a fixed-prefix transform, so prefix bloom filters do the work separate trees
do here. Its `entities` has no predicate structure at all — facts of every predicate interleave in
id order. Two things fall out of the physical split that are worth claiming, because they are
savings rather than preferences:

- **No predicate id in every `entities` row.** Glean stores one, because a bare id does not say
  which predicate it belongs to. Here the tag says it, and "what predicate is this fact?" is a
  shift rather than a fetch.
- **No `stats` family inside the write batch.** Glean maintains per-predicate counts as rows
  updated in every commit, to answer `count(predicate)` and to skip a seek into an empty one. A
  predicate's cardinality and size are properties of its own tree here, so both answers cost
  nothing on the write path.

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

### The scan contract

Because that seam is the only way in, what `scan` promises is a contract on the **trait**, not
on any one store — and both halves of it are asserted directly against every implementation
(`focus::fixtures`), never inferred from two stores agreeing, since two stores that leak
identically would satisfy a differential and both still be wrong.

- **A scan never leaves the predicate its lower bound names**
  (`assert_scan_stays_in_predicate`). fjall gets this structurally, from one tree per
  predicate; a store holding every predicate in one map has to clamp explicitly, and
  `MemStore` once didn't.
- **Opening a scan is fallible, and a bound too short to name a predicate is an error**
  (`assert_short_bound_is_rejected`). `scan` returns `Result` for exactly this: a `lo` with
  fewer than four bytes is a fault in the *call*, not in a row, and there is nowhere else to
  report it. Left unspecified, each store invented an answer — one returned an error as a
  first row, the others scanned across the predicate boundary and reported nothing.

---

## The order a scan is promised in

A scan yields rows in **lexicographic key order**, and everything above it takes that as given: a
seek narrows by extending a byte prefix, residuals are checked as rows arrive in that order, and
resume re-opens a level at the saved key bytes and expects to land on the same fact
([I4](invariants.md#i4), [chapter 5](05-resume.md)). That is a **commitment**, not an incidental
property of fjall, and it is stronger than what the design it was taken from now offers: Glean's
prefix iterator returns facts "in no specified order" — a requirement it dropped deliberately, so
that a `keys` row could hold a *truncated* key on backends with a small maximum key size, with the
full key re-checked from `entities`.

Two consequences, neither of which has an answer here yet:

- **Key truncation is foreclosed.** It is the fix for [storing the key
  twice](#entities--identity-answers-what-is-this-fact), and it needs duplicate index entries in
  no particular order. A backend that cannot hold a whole key cannot hold this `keys` family.
- **There is no key-size budget and no degradation path.** Glean caps a key explicitly and
  documents what happens above the cap; nothing here states a bound, so an over-long key is a
  question the store answers by accident.

The codec's [order-preservation](02-tuple-codec.md#property-1--order-preserving-i1) is the
stronger property and is still unspent. This is the weaker half of it, and it is what is
load-bearing today.

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
  which is exactly what fjall's ascending-only bulk `ingest()` wants. Glean, whose ids are one
  flat space behind one counter, has **no concurrent writer at all** at the storage layer — a
  batch inserted out of sequence is an error, and the code says "we do *not* support concurrent
  writes" in as many words — and buys the parallelism back with a whole rebase subsystem: a
  writer builds a local fact set and every id in it is renamed through a substitution on merge.
  The snowflake deletes that subsystem rather than reimplementing it.
- **Uniqueness across predicates is structural**, not enforced: the tag partitions the id
  space, so two predicates cannot collide however their sequences are allocated.

**Sequence 0 is reserved**, so no valid id is `FactId(0)`: a zeroed or corrupt eight bytes is
detectably not a fact, which is worth having on a path where I11 is what makes a bytes-only
resume cursor safe.

**The high-water mark is recovered from the data, not from a sidecar counter.** An `entities`
key *is* a fact id, big-endian, so the last key in a predicate's `entities` tree is its
high-water mark. A separately persisted counter could be stale after a crash and reissue a
live id; a derived one cannot disagree with what is stored. Glean persists its `NEXT_ID` in an
admin family and inside the same batch as the facts, so its counter cannot be stale either — but
it can be *missing*, and Glean's own error message for that case is "corrupt database". A mark
read back from the data is not state that can be lost.

Sequences are **unique and never reused, but not dense**: an id consumed by a write that then
fails is not handed out again.

**What the tag costs is density *across* predicates**, and it is worth naming precisely, because
Glean's interface says its ids "are supposed to be dense" and five separate mechanisms spend that
density — a substitution is a flat vector indexed by `id − base`, an in-memory fact set indexes
its facts by `id − starting_id`, ownership sets are Elias-Fano-coded (a coding for *monotone*
sequences), the fact→owner map is an interval map over consecutive ids, and stacking is the single
compare `id < mid`. Within one predicate the sequence here is monotonic from 1 with holes only
where a write failed, so every one of those structures survives as a **per-predicate instance
keyed by the tag** — the same dense-map-of-predicates shape Glean already uses elsewhere — rather
than becoming impossible. The one that genuinely degrades is a set of facts *spanning* predicates:
under a snowflake that is up to 2²⁴ far-apart runs, and bitset or monotone compaction over it buys
much less. If such a set is ever wanted, key it by predicate from the start.

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
write batch**. Glean does exactly this, in one wider batch that also carries its id counter and
its per-predicate stats rows — an adopted rule, not a divergence.

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

## Writing a fact by hand

`put_fact(predicate, key_bytes, value_bytes)` is the primitive, and it is the wrong thing to
hand a person. Three of its preconditions are invisible at the call site, and each fails
*silently* — the write succeeds and the fact is simply never found:

| what a caller has to know | what happens if they don't |
|---|---|
| a record key is **flat**; only a record *inside* a field keeps its wrapper | the seek builds the flat form and never meets the wrapped one |
| field order is the **schema's declaration order**, which a Rust struct has no reason to share | fields land in the wrong positions and decode as each other |
| only the schema says whether a predicate has a value side at all | a value written where none is expected, or missing where one is |

So `FjallDb::put(&schema, &fact)` takes a **well-typed value** instead: a type implementing
`focus::fact::Fact` names its predicate and gives its key fields *by name*, and the write
resolves those against the schema — reordering into declared order and reporting an unknown
field, a missing one, a wrong shape or a stray value side before any bytes exist. A fact whose
fields are listed in whatever order reads well still writes a findable fact, which is the whole
point.

The id it returns **is** what a reference to that fact is, so a fact pointing at another takes
the value the earlier write handed back. Referential integrity is then a consequence of write
order rather than a check: nothing can point at a fact that has not been written, because
there is no id for it yet.

Two things this is deliberately not. It is **not bulk ingestion** — it materialises a value per
fact, which is right for a deriver writing thousands and wrong for a loader writing millions;
Phase 7's fact-file path wants a streaming form. And it is **not a `serde` derive**, though it
is the layer one would sit on: serde's data model has no fact reference, and its struct fields
arrive in declaration order, so a derive would still need the schema resolution above.

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

## The format stamp ([I15](invariants.md#i15))

Every database carries, in a metadata keyspace of its own, twelve bytes that say which
encoding wrote it: the magic `APERTURE`, then two `u16` version numbers.

```
   0            8      10      12
  ┌─────────────┬───────┬───────┐
  │ "APERTURE"  │ codec │storage│
  └─────────────┴───────┴───────┘
```

It is written **once, when the DB is created** — a directory with no keyspaces in it — and
checked at every open, before a predicate tree is touched. `meta` is not a predicate keyspace
and carries neither the `keys.` nor the `entities.` prefix, so it is invisible to the walk
that [recovers predicates at open](#one-keyspace-per-predicate--for-both-column-families).
It is also where the [embedded schema](06-types-and-schema.md) goes when
[I13](invariants.md#i13) lands, which is the reason for a metadata block rather than a bare
version key.

**Why two numbers.** They freeze different things and move for different reasons: `codec` is
the marker table and the per-type encodings ([chapter 2](02-tuple-codec.md)), `storage` is
everything on this page — row framing in each column family, keyspace naming, the `FactId`
split. A new type's marker moves the first and reshapes no row; a change to the `entities`
framing moves the second and touches no marker. One number would refuse a DB over a change
that cannot affect it, and could not tell a reader which half it failed to understand.

**Three cases, and the third is the one that matters.**

| the database | what happens |
|---|---|
| a new directory | stamped with the current versions — this is the *create* path |
| stamped, versions understood | opened |
| stamped, versions not understood | refused, naming both |
| **holds facts, no stamp** | **refused** |

An unstamped database with predicate trees in it was written by something else — an older
build, or not Aperture at all. Stamping it on the way past would be this build certifying
bytes it has never read, which is precisely the silent misread the stamp exists to prevent.
Every database written before this existed is that shape, and refusing them is the honest
answer: there is nothing to migrate *to* yet, and nothing that could say what to migrate
*from*.

The rule is **equality**, not "readable up to N". The marker table is append-only, so a newer
reader could in principle read older bytes — but that is a promise about every past encoding,
and it costs nothing to make later, once there is a past encoding to make it about.

What this changes about [I3](invariants.md#i3) is worth stating precisely, because it is
easy to overread: **nothing is migratable now**. I3 still binds every database stamped
`codec 1`, and renumbering a marker under that stamp is as wrong as it ever was. What the
stamp buys is that a *future* codec is a different number rather than an impossibility. It
was taken now rather than later for one reason — arrays, unions, stored schemas and
operational metadata are all still unwritten, so today the field costs twelve bytes and a
check at open, and every one of those features would otherwise land more encoding behind a
door with no handle.

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
| [I15](invariants.md#i15) | A DB says which format wrote it; an unreadable one is refused. | `store::a_database_says_which_format_wrote_it`, `store::a_corrupt_format_stamp_is_reported` |

I6 is stated here because it's a *storage-shape* decision, and enforced in the
[executor](04-executor.md).

---

> **Reading path:** [← 2. The tuple codec](02-tuple-codec.md) · **3. The storage model** · [4. The executor →](04-executor.md)
