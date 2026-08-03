# Invariant registry

> [Aperture design book](../README.md) · reference doc

Every invariant in one place. **Engine invariants `I1`–`I13`** are the codec / executor /
storage / identity rules explained in chapters 2–6. **Operational invariants
`ops-I1`–`ops-I10`** are the lifecycle / connection rules explained in
[Operations](aperture-cli-design.md). The two namespaces are separate — always write
`ops-Ix` for the operational ones.

Each invariant names a **guard test**: the test that pins it. Guards are written *up front*
(the property statement is the spec); one whose subsystem doesn't exist yet is
`#[ignore = "Ixx — pending Phase N"]`, and `cargo test -- --ignored --list` is the live
coverage ledger. A phase is done only when the invariants it touches are un-ignored and
green. See [testing](testing.md).

---

## Engine invariants — quick table

| ID | Statement | Guard | Where | Status |
|----|-----------|-------|-------|--------|
| [I1](#i1) | Key encoding is order-preserving. | `codec::order_preservation` + round-trip | [ch2](02-tuple-codec.md) | ✅ green |
| [I2](#i2) | Encoding is self-delimiting; `skip` needs no schema. | `codec::skip_exactness` | [ch2](02-tuple-codec.md) | ✅ green |
| [I3](#i3) | The marker table is frozen on disk. | `codec::marker_table_golden` | [ch2](02-tuple-codec.md) | ✅ green |
| [I4](#i4) | Resume == uninterrupted run. | `exec::resume_equals_uninterrupted` | [ch5](05-resume.md) | Phase 0 (MemStore) → Phase 1 (fjall) |
| [I5](#i5) | Register holds the whole row; fields decode lazily. | `exec::bind_is_refcount_not_decode` | [ch4](04-executor.md) | ✅ green |
| [I6](#i6) | Values never enter the scan hot loop. | `exec::no_value_fetch_in_scan` | [ch3](03-storage-model.md)/[ch4](04-executor.md) | ✅ green |
| [I7](#i7) | The executor is a defunctionalised state machine. | structural + resume battery | [ch4](04-executor.md) | Phase 0 — happy-path green; resume battery pending |
| [I8](#i8) | Immutable snapshot per query; released at suspend. | `store::snapshot_released_at_suspend` | [ch5](05-resume.md) | Phase 1 (needs fjall) |
| [I9](#i9) | Hot path is allocation-free per row. | `exec::scan_is_alloc_free_per_row` | [ch4](04-executor.md) | ✅ green |
| [I10](#i10) | Union discriminants are stable and append-only. | `schema::discriminants_append_only` | [ch6](06-types-and-schema.md) | Phase 8 (with unions) |
| [I11](#i11) | `FactId` is stable, unique, never reused within a DB. | `store::factid_unique_monotonic` | [ch3](03-storage-model.md) | Phase 1 |
| [I12](#i12) | A fact is written to both column families atomically. | `store::no_half_present_facts` | [ch3](03-storage-model.md) | Phase 1 |
| [I13](#i13) | The DB's schema is embedded and frozen at create. | `schema::ingest_rejects_incompatible_schema` + `fingerprint_is_order_independent` | [ch6](06-types-and-schema.md) | Phase 8 |

---

## Engine invariants — detail

<a id="i1"></a>
### I1 — Key encoding is order-preserving
`memcmp(encode(a), encode(b)) == semantic_compare(a, b)`. Prefix scans over sorted bytes
*are* range/predicate queries — the whole storage model rests on this. **The gate for any
codec change.** *Why & how:* [chapter 2](02-tuple-codec.md#property-1--order-preserving-i1).
*Guard:* `codec::order_preservation` (tier-2 vs an independent comparator) + round-trip.

<a id="i2"></a>
### I2 — Encoding is self-delimiting; `skip` needs no schema
The marker byte alone says how to advance past a value (three skip families). Lets the scan
hot loop walk fields with no type info; a full decode consumes exactly to end-of-input.
Record nesting is bounded (`MAX_RECORD_DEPTH`) → errors, never stack overflow. *Why & how:*
[chapter 2](02-tuple-codec.md#property-2--self-delimiting-i2). *Guard:* `codec::skip_exactness`
+ trailing-bytes-rejected + max-depth-errors.

<a id="i3"></a>
### I3 — The marker table is frozen on disk
Marker values and their order are semantic (a marker is the MSB of a value's sort key).
Once data exists they can't change without migration; new types go in reserved bands.
*Why & how:* [chapter 2](02-tuple-codec.md#property-3--frozen-on-disk-i3). *Guard:*
`codec::marker_table_golden` (golden bytes — breaks loudly on renumber).

<a id="i4"></a>
### I4 — Resume == uninterrupted run
Suspend+resume reproduces the exact row sequence — no duplicates, no skips, across join
boundaries. The `Cursor` is bytes only; pins no iterator and no snapshot. Resume re-opens
each level at the saved key, re-binds, and checks the re-read `fact_id` matches (else
`BadResumeKey`). *Why & how:* [chapter 5](05-resume.md). *Guard:*
`exec::resume_equals_uninterrupted` (tier-3, every cut point, 1-/2-/3-level, MemStore + fjall).

<a id="i5"></a>
### I5 — Row-slot / register model
A register holds the *whole* row (`fact_id` + key bytes); the *field* lives in the plan
(`RegisterField`), not the register. Binding N vars = N refcount bumps, zero decodes.
Decode lazily at read sites only. *Why & how:* [chapter 4](04-executor.md#the-register-file-and-the-row-slot-model-i5).
*Guard:* `exec::bind_is_refcount_not_decode`.

<a id="i6"></a>
### I6 — Values never enter the scan hot loop
Residuals on *key* fields are checked against the `keys` CF only; a value is fetched from
`entities` only when projected/navigated. Value patterns are a distinct residual class over
the fetched buffer. *Why & how:* [chapter 3](03-storage-model.md#why-two-not-one). *Guard:*
`exec::no_value_fetch_in_scan` (store spy fails on unexpected `point()`).

<a id="i7"></a>
### I7 — The executor is a defunctionalised state machine
`enumerate` + the frame stack are the explicit reification of recursive `concatMap`, chosen
so execution can suspend to bytes (I4). Native recursion/closures/coroutines can't — they
pin iterators and a snapshot. **Don't rewrite `enumerate` as recursion.** *Why & how:*
[chapter 4](04-executor.md#why-a-state-machine-and-not-recursion--i7). *Guard:* structural —
the resume battery is impossible to pass under a recursive rewrite; plus review.

<a id="i8"></a>
### I8 — Immutable snapshot per query; released at suspend
fjall iterators pin a read snapshot; drop the executor at suspend to release it. A held
`Iter`/`Slice` keeps LSM blocks and a whole superseded generation alive. *Why & how:*
[chapter 5](05-resume.md#the-two-invariants-at-stake). *Guard:*
`store::snapshot_released_at_suspend` (drop-probe) — **untestable on `MemStore`**, needs fjall.

<a id="i9"></a>
### I9 — Hot path is allocation-free per row
Reused scratch buffers; `ByteView` clones are refcount bumps; field-offset caches are inline
`ArrayVec<[usize;16]>` that never heap-spill. Copy out only at escape boundaries (suspend,
string/bytes projection). *Why & how:* [chapter 4](04-executor.md#field-offset-cache-i9).
*Guard:* `exec::scan_is_alloc_free_per_row` (allocation-counting allocator).

<a id="i10"></a>
### I10 — Union alternative discriminants are stable and append-only
Explicit, assigned-once, never-reused, append-only discriminants (protobuf-style). Frozen
the moment union data is written; derived-from-names schemes would silently renumber. *Why &
how:* [chapter 6](06-types-and-schema.md#unions-and-stable-discriminants-i10). *Guard:*
`schema::discriminants_append_only` (renumber/reuse rejected at load).

<a id="i11"></a>
### I11 — `FactId` is stable, unique, never reused within a DB
Assigned once from a monotonic counter; unique, never reused (no deletion), stable for the
DB's lifetime. The scan→point map and resume's integrity check depend on it. It is a
*physical* row id, **not** cross-DB identity (that's the content hash, [ops-I4](#ops-i4)).
*Why & how:* [chapter 3](03-storage-model.md#factid-allocation-i11). *Guard:*
`store::factid_unique_monotonic` (pending ingestion).

<a id="i12"></a>
### I12 — A fact is written to both column families atomically
`keys` and `entities` are written in one fjall batch — a fact is never half-present. A
dangling half is silent corruption at projection. *Why & how:*
[chapter 3](03-storage-model.md#the-atomic-two-cf-write-i12). *Guard:*
`store::no_half_present_facts` (no danglers; crash-injection leaves neither — pending ingestion).

<a id="i13"></a>
### I13 — The DB's schema is embedded and frozen at create
Canonical schema + fingerprint embedded at `create`, immutable for the DB's lifetime (no
`evolves` in P0); every ingest validated by subset containment; the DB is self-describing.
*Why & how:* [chapter 6](06-types-and-schema.md#the-schema-is-embedded-and-frozen-i13).
*Guard:* `schema::ingest_rejects_incompatible_schema` + `schema::fingerprint_is_order_independent`
(pending schema).

---

## Operational invariants (`ops-I1`–`ops-I10`)

Explained in full in [Operations §1](aperture-cli-design.md). Summarised here so the whole
invariant surface is visible in one place; cite them `ops-Ix`.

<a id="ops-i1"></a>**ops-I1 — Single-process store ownership.** A fjall directory is opened
by exactly one process; a running server owns every DB under its root; no silent
connect→open fallback.

<a id="ops-i2"></a>**ops-I2 — Complete = immutable.** Lifecycle `Writable → Complete` (+
`Broken`). Once Complete, every open-for-write is refused at session establishment —
structural, not per-write. The database-level face of the immutability that engine
invariants assume.

<a id="ops-i3"></a>**ops-I3 — Finish ordering.** `finish` makes data durable
(`SyncAll`) *first*, then flips status via an atomic sidecar write as the **last** durable
action. Never observable that metadata says Complete while data isn't durable.

<a id="ops-i4"></a>**ops-I4 — Reproducibility.** A DB built twice from identical inputs is
identical; identity = `hash(canonical schema, base facts)`. Timestamps/random ids are
descriptive, never identity. Conflict handling is order-independent (strict reject, not
last-writer-wins). This is why [I11](#i11) fact-ids are *not* cross-DB identity.

<a id="ops-i5"></a>**ops-I5 — One write funnel.** Every writer (bulk ingest, wire COPY,
tools) passes the same pipeline: schema-validate → sort/merge → dedup identical → reject
same-key-different-value. Structural guarantees hold regardless of writer trust.

<a id="ops-i6"></a>**ops-I6 — Session modes.** A session declares `read-only` | `read-write`
at open, resolved once against DB status (Complete ⇒ read-only, full stop).

<a id="ops-i7"></a>**ops-I7 — Filesystem is the catalog.** No manifest of DBs; enumeration =
walk the store root + read sidecars. Any index is rebuildable and never authoritative.

<a id="ops-i8"></a>**ops-I8 — Derivation is phased.** create → ingest base → derive →
finish. Derivers read the frozen base via a *sealed snapshot*, write only derived
predicates; prefix-disjointness makes read/write disjointness structural.

<a id="ops-i9"></a>**ops-I9 — No cross-DB anything in P0.** No cross-DB queries/stacking/
ownership; don't hardcode "predicate + key fully identifies a fact, forever" in planner
layers.

<a id="ops-i10"></a>**ops-I10 — No in-DB auth; the transport is the trust boundary.** No
RBAC; authn is the transport's job (socket permissions / gateway). Safe only because binding
is default-closed (Unix socket only; TCP explicit opt-in).

---

> [Index](../README.md) · [Testing →](testing.md)
