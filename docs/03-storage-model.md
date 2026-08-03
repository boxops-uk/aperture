# 3. The storage model

> [Aperture design book](../README.md) · [← 2. The tuple codec](02-tuple-codec.md) · **Chapter 3** · [4. The executor →](04-executor.md)

This chapter is how facts live on disk: the two column families, one keyspace per
predicate, how a `FactId` is allocated, and why the two halves of a fact are always written
together. It builds directly on the [order-preserving codec](02-tuple-codec.md).

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
(a `Project::Value` in the plan, or cross-fact navigation) — never during scanning.

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

## One keyspace per predicate

Each predicate gets its **own fjall keyspace** (physical tree), with the predicate id as
the key prefix. Two consequences, both important:

- **Physical isolation.** Facts of different predicates cannot affect each other's storage.
  This is what makes bulk ingestion embarrassingly parallel — per-predicate ingests are
  independent trees that can overlap freely (see [Operations](aperture-cli-design.md) and
  [ops-I8](invariants.md#ops-i8)).
- **Prefix-disjointness aligns with isolation.** "Predicate id is the key prefix" and "one
  tree per predicate" say the same thing at two levels, so a deriver reading predicate A
  and writing predicate B is structurally read/write-disjoint.

---

## FactId allocation ([I11](invariants.md#i11))

Every fact gets a `FactId` — a `u64` — assigned once at ingest from a **monotonic counter**
(an `AtomicU64` high-water mark).

> **I11 — `FactId` is stable, unique, and never reused within a DB.** Unique within a DB,
> never reused (there is no deletion — the DB is immutable), stable for the DB's lifetime.
>
> *Guard:* `store::factid_unique_monotonic` (pending ingestion) — ingest assigns unique,
> monotonically increasing ids; no two distinct facts share one.

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
monotonic counter is the seam that makes concurrent ingestion safe; getting it right is a
Phase-0.5/Phase-5 concern in [`PLAN.md`](../PLAN.md).

---

## The atomic two-CF write ([I12](invariants.md#i12))

`keys` and `entities` are the two halves of one fact. They are written in a **single fjall
write batch**.

> **I12 — a fact is written to both column families atomically.** A fact is never
> half-present: never a key without its entity, never an entity without its key.
>
> *Guard:* `store::no_half_present_facts` (pending ingestion) — after ingest every `keys`
> entry resolves in `entities` and vice versa; crash-injection between the two writes
> leaves neither.

A half-present fact is silent corruption: a `keys` entry with no `entities` row makes the
scan→point lookup return nothing (or garbage) exactly when a query projects that fact's
value. The batch is the only thing standing between "immutable, self-consistent store" and
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
| [I11](invariants.md#i11) | `FactId` is stable, unique, never reused within a DB. | `store::factid_unique_monotonic` (pending) |
| [I12](invariants.md#i12) | A fact is written to both column families atomically. | `store::no_half_present_facts` (pending) |

I6 is stated here because it's a *storage-shape* decision, and enforced in the
[executor](04-executor.md).

---

> **Reading path:** [← 2. The tuple codec](02-tuple-codec.md) · **3. The storage model** · [4. The executor →](04-executor.md)
