# Invariant registry

> [Aperture design book](../README.md) · reference doc

Every invariant in one place. **Engine invariants `I1`–`I14`** are the codec / executor /
storage / identity rules explained in chapters 2–6. **Operational invariants
`ops-I1`–`ops-I10`** are the lifecycle / connection rules explained in
[Operations](aperture-cli-design.md). The two namespaces are separate — always write
`ops-Ix` for the operational ones.

Each invariant names a **guard test**: the test that pins it. Guards are written *up front*
(the property statement is the spec); one whose subsystem doesn't exist yet is
`#[ignore = "Ixx — pending Phase N"]`, and `cargo test -- --ignored --list` is the live
coverage ledger. A phase is done only when the invariants it touches are un-ignored and
green. See [testing](testing.md).

> **Reading a guard name.** The prefix is the *subsystem* as this book names it, not a Rust
> path: `codec::` is `crates/aperture-encoding/src/tuple.rs`, `exec::` is
> `crates/aperture-engine/src/iter.rs`, `store::` is `crates/aperture-store/src/store.rs`, and
> `schema::` is `crates/aperture-schema/src/schema.rs`. The part after `::` is the test
> function — every one of them is greppable, and the real module path is
> `<file>::tests::<name>` within its crate. One guard is an integration test and names its
> file instead: `i8_snapshot::` is `crates/aperture-store/tests/i8_snapshot.rs`.

---

## Engine invariants — quick table

| ID | Statement | Guard | Where | Status |
|----|-----------|-------|-------|--------|
| [I1](#i1) | Key encoding is order-preserving. | `codec::test_typed_value_order_matches_encoded_order` + round-trip | [ch2](02-tuple-codec.md) | ✅ green |
| [I2](#i2) | Encoding is self-delimiting; `skip` needs no schema. | the `codec::test_skip_*` family | [ch2](02-tuple-codec.md) | ✅ green |
| [I3](#i3) | The marker table is frozen on disk. | `codec::marker_table_golden` | [ch2](02-tuple-codec.md) | ✅ green |
| [I4](#i4) | Resume == uninterrupted run. | `exec::resume_equals_uninterrupted` + `…_on_fjall` | [ch5](05-resume.md) | ✅ green on `MemStore` **and** fjall |
| [I5](#i5) | Register holds the whole row; fields decode lazily. | `exec::bind_is_refcount_not_decode` | [ch4](04-executor.md) | ✅ green |
| [I6](#i6) | Values never enter the scan hot loop. | `exec::no_value_fetch_in_scan` | [ch3](03-storage-model.md)/[ch4](04-executor.md) | ✅ green |
| [I7](#i7) | The executor is a defunctionalised state machine. | structural + resume battery | [ch4](04-executor.md) | ✅ green — resume battery in place |
| [I8](#i8) | Immutable snapshot per query; released at suspend. | `i8_snapshot::snapshot_released_at_suspend` | [ch5](05-resume.md) | ✅ green |
| [I9](#i9) | Hot path is allocation-free per row. | `exec::scan_is_alloc_free_per_row` | [ch4](04-executor.md) | ✅ green |
| [I10](#i10) | Union discriminants are stable and append-only. | `schema::discriminants_append_only` | [ch6](06-types-and-schema.md) | Phase 8 (with unions) |
| [I11](#i11) | `FactId` is stable, unique, never reused within a DB. | `store::factid_unique_monotonic` + `exhausted_sequence_space_is_an_error` | [ch3](03-storage-model.md) | ✅ green |
| [I12](#i12) | A fact is written to both column families atomically. | `store::no_half_present_facts_after_writes` + `no_half_present_facts` (crash) | [ch3](03-storage-model.md) | ✅ green |
| [I13](#i13) | The DB's schema is embedded and frozen at create. | `schema::ingest_rejects_incompatible_schema` + `fingerprint_is_order_independent` | [ch6](06-types-and-schema.md) | Phase 8 |
| [I14](#i14) | A derived bind is a pure function of the fact bindings. | `iter::a_derive_is_recomputed_across_every_cut_point` | [ch7](07-compilation.md) | ✅ green (hand-built plans) |
| [I15](#i15) | A DB says which format wrote it; an unreadable one is refused. | `store::a_database_says_which_format_wrote_it` + `a_corrupt_format_stamp_is_reported` | [ch3](03-storage-model.md) | ✅ green |

---

## Engine invariants — detail

<a id="i1"></a>
### I1 — Key encoding is order-preserving
`memcmp(encode(a), encode(b)) == semantic_compare(a, b)`. What that buys, precisely: a
**value-range** scan (`X > 3` as a bounded seek, not a filter) and rows in semantic order with no
sort. An exact-*prefix* scan — a predicate query, or leading key fields fixed to constants — needs
only a self-delimiting **canonical** encoding, no ordering: Glean's fact keys are LEB128 varints
that mis-order, and it serves prefix seeks on them. So I1 is a **divergence whose divergent half
is unspent** — `ResidualOp` has no ordering arm and `<`/`>` are not lexer tokens, so no query can
ask for a range yet. Kept because it is nearly free to hold and impossible to retrofit: the marker
table freezes the moment data exists ([I3](#i3)), and [I15](#i15)'s stamp versions the *next*
codec rather than migrating the one holding today's rows.
What *is* spent today is the weaker store-level half — a scan yields rows in lexicographic key
order, which resume re-seeks against ([I4](#i4)) — and that is a **commitment**: it forecloses the
truncated `keys` row Glean adopted when it dropped ordered iteration, and there is no key-size
budget here to bound it. **The gate for any codec change.** *Why & how:*
[chapter 2](02-tuple-codec.md#property-1--order-preserving-i1) and
[chapter 3](03-storage-model.md#the-order-a-scan-is-promised-in).
*Guard:* `codec::test_typed_value_order_matches_encoded_order` (tier-2: encoded-byte order vs
`cmp_typed`, an independent comparator that walks the type rather than reusing the code under
test) + `test_roundtrip_preserves_value_and_ordering` + the scalar properties
`test_{i64,u64,str}_preserves_order`.

<a id="i2"></a>
### I2 — Encoding is self-delimiting; `skip` needs no schema
The marker byte alone says how to advance past a value (three skip families); a full decode
consumes exactly to end-of-input; record nesting is bounded (`MAX_RECORD_DEPTH`) → errors, never
stack overflow. **A divergence, and not for the reason it looks like.** That the scan hot loop can
walk fields with no type info is downstream of [I7](#i7), not a property encodings must have:
Glean's fact encoding is untagged and positional and its skip is per-predicate codegen, against
which a tag is pure overhead — a byte per field and a branch per field. What I2 buys is that the
bytes can be walked **without the schema** (golden-byte tests, dumping a row of unknown predicate,
diagnosing a corrupt key), and the byte-level `Int`/`Fact` distinction `MARK_FACT_REF` enforces.
*Why & how:*
[chapter 2](02-tuple-codec.md#property-2--self-delimiting-i2). *Guard:* the
`codec::test_skip_*` family — `test_skip_{string,i64,u64}` are the exactness properties (skip
lands exactly where decode ends), the rest are the record/terminator/escape edge cases — plus
`decode_typed` rejecting trailing bytes and `MAX_RECORD_DEPTH` erroring rather than
overflowing.

<a id="i3"></a>
### I3 — The marker table is frozen on disk
Marker values and their order are semantic (a marker is the MSB of a value's sort key).
Once data exists they can't change without migration; new types go in reserved bands.
**I3 must hold forever for every database already written**, and what changed is only that a
*future* encoding can now be a different one: a migration presupposes detection, and until
[I15](#i15) nothing a reader was handed said which encoding wrote it. Glean versions both its DB
binary representation (readable/writable sets negotiated at open) and its bytecode ABI; the
[format stamp](03-storage-model.md#the-format-stamp-i15) is that escape hatch, and it makes
nothing migratable — a database stamped `codec 1` is bound by this invariant exactly as it was,
so renumbering a marker under that stamp is as wrong as it ever was. Two further things
the table freezes: every marker a value can *begin* with stays below `MARK_ESCAPE` (otherwise
string ordering inverts across a record boundary), and for a *container* type the reserved band is
not the whole decision — length-prefix versus terminator decides whether an array can be
prefix-matched at all, which no later renumbering can undo.
*Why & how:* [chapter 2](02-tuple-codec.md#property-3--frozen-on-disk-i3). *Guard:*
`codec::marker_table_golden` (golden bytes — breaks loudly on renumber).

<a id="i4"></a>
### I4 — Resume == uninterrupted run
Suspend+resume reproduces the exact row sequence — no duplicates, no skips, across join
boundaries. The `Cursor` is bytes only; pins no iterator and no snapshot. Resume re-opens
each level at the saved key **in the alternative that saved it**, re-binds, and checks the
re-read `fact_id` matches (else `BadResumeKey`). *Why & how:* [chapter 5](05-resume.md).
*Guard:* `exec::resume_equals_uninterrupted` (tier-3, every cut point, 1-/2-/3-level,
MemStore + fjall), with `exec::the_battery_reaches_a_cut_inside_a_later_source` asserting the
battery draws a multi-source level and cuts inside one. Over **compiled** plans it is
`flatten::resume_of_a_compiled_plan_equals_the_query`, which draws its loop order rather than
taking the identity: where a step that binds nothing *sits* is a property of the order, and the
[census](testing.md) asserts the battery reaches a negation placed above a scan — the placement
Phase 6 found to be the only one that observes a restore fault at all. Plus
`…_of_a_negated_plan_…`, the same experiment with the draw **forced**: the general battery draws
a negation in about one case in fifteen, which says the shape is reached and not that it is
covered.

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
the fetched buffer. **Navigated** is now real and not a promissory note:
[`Source::Fetch`](04-executor.md#fetching-through-a-reference) reads `entities` for the fact a
reference names — its *identity*, which is what that CF is for — and it is a level, so it is
opened once per row the level above it **produces**, never once per row a scan examines. That
distinction is the whole of I6: a lookup inside the filter loop is what it forbids. **An Aperture strengthening, not an adopted idea:** Glean has the same
key-only/key-value split at its iterator, but a non-wild value pattern marks the seek as needing a
value and it then fetches one for every row the scan *examines* — a second store lookup per row,
which is exactly what I6 forbids. Cheap to hold here partly because value patterns are deferred
(`nyi/value-match`), so the query that tempts the fetch cannot be written yet. *Why & how:*
[chapter 3](03-storage-model.md#why-two-not-one). *Guard:*
`exec::no_value_fetch_in_scan` (store spy fails on unexpected `point()`), plus
`exec::a_fetch_reads_entities_once_per_row_it_is_opened_for`, which counts point reads against
rows *produced* rather than examined, plus `exec::a_negation_probe_fetches_no_value` — the third
way the machine can read the store, and the same rule: a probe asks whether a **key** exists, and
runs once per row the level above it produces.

<a id="i7"></a>
### I7 — The executor is a defunctionalised state machine
`enumerate` + the frame stack are the explicit reification of recursive `concatMap`, chosen
so execution can suspend to bytes (I4). Native recursion/closures/coroutines can't — they
pin iterators and a snapshot. **Don't rewrite `enumerate` as recursion.** A compiled bytecode
machine is the *other* road to a serialisable continuation — Glean's VM has no call stack at all —
so what decides it here is continuation **size** and not version-locking a compiled form.
*Why & how:*
[chapter 4](04-executor.md#why-a-state-machine-and-not-recursion--i7). *Guard:* structural —
the resume battery is impossible to pass under a recursive rewrite; plus review. **Negation was
the test of that**, and it passed without a new frame kind: a test's two outcomes are the
machine's two existing directions — pass is the ascent a derived bind makes, fail is the
backtrack an exhausted level makes — so the arm added a step kind and no control flow.

<a id="i8"></a>
### I8 — Immutable snapshot per query; released at suspend
fjall iterators pin a read snapshot; drop the executor at suspend to release it. A held
`Iter`/`Slice` keeps LSM blocks and a whole superseded generation alive. **Structural, not a
discipline:** `Executor::enumerate` takes `self` by value, so every exit path — done,
suspend, cancel, error unwind — drops the frame stack and the store handle, and no shape of
caller can park a live iterator across a suspend. *Why & how:*
[chapter 5](05-resume.md#the-two-invariants-at-stake). *Guard:*
`i8_snapshot::snapshot_released_at_suspend` — an **integration** test of `aperture-store`
(`crates/aperture-store/tests/`), because it is the one store guard that has to run a query and
a unit test reaching back through the engine would compile a second copy of the store
([testing](testing.md)). All four stops, against two independent witnesses: a
drop probe over the store handle and every scan it opened, and fjall's own open-snapshot
count (`FjallDb::open_snapshots`), with a mid-run positive control so it cannot pass
vacuously. **Untestable on `MemStore`**, whose scan pins nothing; needs fjall.

<a id="i9"></a>
### I9 — Hot path is allocation-free per row
Reused scratch buffers; `ByteView` clones are refcount bumps; field-offset caches are inline
`ArrayVec<[usize;16]>` that never heap-spill. Copy out only at escape boundaries (suspend,
string/bytes projection). *Why & how:* [chapter 4](04-executor.md#field-offset-cache-i9).
*Guard:* `exec::scan_is_alloc_free_per_row` (`allocation-counter`, a dev-dependency; counts
and bytes both compared).

<a id="i10"></a>
### I10 — Union alternative discriminants are stable and append-only
Explicit, assigned-once, never-reused, append-only discriminants (protobuf-style). Frozen
the moment union data is written; a discriminant *derived* from the declaration renumbers the
moment an alternative is inserted. **A divergence.** Glean has no discriminant syntax at all — an
alternative's discriminant is its **position** in the list, so an insert renumbers, and stability
comes from a query-time transform that remaps alternatives **by name** and answers with a synthetic
`unknown` for one that no longer exists. Explicit discriminants are therefore not "the only safe
scheme"; they are the only safe scheme *without* that transform layer, which [I13](#i13) declines
by freezing the schema instead. *Why & how:*
[chapter 6](06-types-and-schema.md#unions-and-stable-discriminants-i10). *Guard:*
`schema::discriminants_append_only` (renumber/reuse rejected at load) — which also owes the one
case the invariant needs and Aperture has not settled: what a decoder does with a tag no schema
declares. Glean has a defined answer there; this design does not yet.

<a id="i11"></a>
### I11 — `FactId` is stable, unique, never reused within a DB
Assigned once as a **snowflake** — predicate id in the high 24 bits, a per-predicate sequence
in the low 40 — so uniqueness across predicates is structural and each predicate allocates
independently. Monotonic within a predicate, never reused (no deletion), stable for the DB's
lifetime; sequence 0 is reserved, so `FactId(0)` is never a fact. The high-water mark is
recovered from the last `entities` key rather than a sidecar counter, which cannot go stale
across a crash — Glean's persisted `NEXT_ID` cannot go stale either, but it can go *missing*, and
its error for that is "corrupt database". The scan→point map and resume's integrity check depend on
it. It is a *physical* row id, **not** cross-DB identity (that's the content hash,
[ops-I4](#ops-i4)). Constrains the schema: a predicate id must fit 24 bits. What the tag trades
away is **density across predicates**: Glean's ids are documented as *dense*, and five mechanisms
spend that density — substitution vectors indexed by `id − base`, fact sets indexed by
`id − starting_id`, Elias-Fano ownership sets, the fact→owner interval map, and the `id < mid`
stacking test — so stacking is one consumer of five. Within a predicate the sequence stays dense,
so each of those survives as a per-predicate instance keyed by the tag; only a fact set *spanning*
predicates degrades. Against that, Glean has **no concurrent writer at all** at the storage layer
and buys parallelism back with the whole rebase/substitution subsystem, whose *allocation* half
per-predicate counters delete — not its reference-relocation half, which no id scheme deletes.
What Aperture does instead is not relocate: a producer sends
[the target fact, not an id](open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline),
and ingest [interns](03-storage-model.md#interning-a-nested-fact) it into one.
**One live tension, unresolved:** "never reused" and Phase 8b's O(1) re-derivation by *tree
drop* cannot both hold, because the high-water mark is recovered from the very tree being
dropped — [open decision](open-decisions.md#re-derivation-and-what-happens-to-the-high-water-mark).
*Why & how:* [chapter 3](03-storage-model.md#factid-allocation-i11). *Guards:*
`store::factid_unique_monotonic` + `store::exhausted_sequence_space_is_an_error` +
`store::untaggable_predicate_is_rejected`. The reserved sequence is guarded where stored bytes
become an id, which is two places and not one:
`store::a_zeroed_fact_id_is_rejected_at_decode` for a `keys` row, and
`codec::a_fact_ref_of_the_reserved_sequence_is_rejected` for a reference embedded **in a key** —
a property nothing checks is only an intention. `codec::a_typed_fact_ref_must_name_the_declared_predicate`
and `fact::a_reference_must_name_the_declared_predicate` are the other half of what the tag makes
checkable: the id says which predicate it belongs to, so a reference into the wrong one is a
compare rather than a lookup, and it is caught before the bytes exist.

<a id="i12"></a>
### I12 — A fact is written to both column families atomically
`keys` and `entities` are written in one fjall batch — a fact is never half-present. A
dangling half is silent corruption at projection; a dangling entity is invisible to every
query. **Adopted, not diverged:** Glean commits its two families in one wider batch, carrying its
id counter and per-predicate stats rows along with them.
**Atomicity is not the whole of the bijection.** A batch is all-or-nothing, but writing the
*same key twice* overwrites the `keys` row and strands the first fact's entity — an orphan the
batch is innocent of. `FjallDb::put` refuses that in every build (identical fact ⇒ the id
already assigned; same key, different value ⇒ `KeyAlreadyWritten`); `put_fact` is the bulk
primitive and still leaves it to its caller, checked by a debug assertion, because Phase 7's
merge frontier establishes it more cheaply upstream.
*Why & how:* [chapter 3](03-storage-model.md#the-atomic-two-cf-write-i12). *Guards:*
`store::no_half_present_facts_after_writes` (the two CFs in exact bijection over generated
writes) + `store::no_half_present_facts` (a child process aborted mid-write; the bijection
must survive recovery) + `store::put_is_write_once_and_says_so_in_release` and
`store::writing_a_key_twice_is_caught_in_debug` (the two halves of the write-once rule).

<a id="i13"></a>
### I13 — The DB's schema is embedded and frozen at create
Canonical schema + fingerprint embedded at `create`, immutable for the DB's lifetime (no
`evolves` in P0); every ingest validated by subset containment; the DB is self-describing.
*Why & how:* [chapter 6](06-types-and-schema.md#the-schema-is-embedded-and-frozen-i13).
*Guards:* `schema::ingest_rejects_incompatible_schema` + `schema::fingerprint_is_order_independent`
(pending schema). The second one's specification is **predicate order free, field order
significant**: two source orderings of the same predicates — spread across files differently,
declared in a different sequence — must share a fingerprint, and permuting the *fields* of a
predicate must **change** it. Field order is encoding order (`aperture_store::fact` resolves a fact's named
fields into declared order before any bytes exist) and it decides the seek prefix, so a field
permutation is a semantic change; a guard that certified it as identity would certify two DBs with
one fingerprint and incompatible bytes. Glean agrees: field order sits inside its `fingerprintDef`,
and a field reorder is a *transform*, not identity.

<a id="i14"></a>
### I14 — A derived bind is a pure function of the fact bindings
A [`Step::Derive`](07-compilation.md#derived-facts)'s value is determined entirely by the
fact slots bound when it is computed: no iteration, no store read, no state the
[`Cursor`](05-resume.md) does not carry. That purity is exactly what lets the cursor save
**only** generator positions — a derive step contributes no cursor entry and is *recomputed*
on restore, so an impure one would resume to a different value than the uninterrupted run
had, and the row sequences would diverge at the cut point. It is why `Computed`'s arms must
stay expressions over already-bound slots; adding an arm that reads anything else breaks
[I4](#i4) rather than only this. *Why & how:*
[chapter 7](07-compilation.md#derived-facts). *Guard:*
`iter::a_derive_is_recomputed_across_every_cut_point` — resume == uninterrupted at every cut
point, with the derive step **both above and below** a scan. The order is the test: below a
scan the machine re-enters the derive from beneath and recomputes it anyway, so only the
*above* case observes a resume that failed to recompute (mutation-checked).

> **Scope, honestly.** The guard drives hand-built plans, because nothing in focus lowers a
> derive step yet — a constant bind is [folded](07-compilation.md#folding-a-constant-bind)
> instead, and the first real producer will be a primitive or a subquery. So this invariant is
> currently held by construction rather than by pressure from the language, and the guard is
> what will catch the first producer getting it wrong.

<a id="i15"></a>
### I15 — A database says which format wrote it, and an unreadable one is refused
Every DB carries a **format stamp** — `codec` and `storage` version numbers in its metadata
keyspace, written once when the DB is created and checked at every open before a row is read
([chapter 3](03-storage-model.md#the-format-stamp-i15)). A build reads exactly the versions it
writes; anything else, including a database holding facts with **no** stamp, is refused.

This is the invariant [I3](#i3) was waiting for. I3 has to hold *forever* only because nothing
said which encoding wrote a DB, and a migration presupposes detection — so the marker table
could never be renumbered even in a new format, because no reader could tell the two apart.
That is now a decision rather than a fact: I3 still binds every DB stamped `codec 1`, and a
future codec is a different number rather than an impossibility. Nothing may be migrated yet
and nothing here promises a migration; what exists is the discriminator that makes one
possible, taken early because every unwritten feature — arrays, unions, an embedded schema —
lands more encoding behind the door while it is missing.

**Two numbers, because two things are frozen and they move separately.** `codec` covers the
marker table and per-type encodings ([chapter 2](02-tuple-codec.md)); `storage` covers row
framing, keyspace naming and the `FactId` split ([chapter 3](03-storage-model.md)). One number
would refuse a DB over a change that cannot affect it, and could not say which half a reader
failed to understand.

**The rule is equality, and the refusals are the point.** "Readable up to N" is the plausible
refinement — the marker table is append-only, so a newer reader *could* read older bytes — but
it is a promise about every past encoding, and it costs nothing to add once there is a past
encoding to make it about. An *unstamped* database holding facts is refused rather than
stamped: adopting one would be this build certifying bytes it has never read, which is exactly
the silent misread the stamp exists to prevent.

*Guards:* `store::a_database_says_which_format_wrote_it` — a fresh directory is stamped, the
stamp survives a reopen, a bumped version is refused, and a stamped DB with its stamp removed
is refused. Plus `store::a_corrupt_format_stamp_is_reported`: the stamp is bytes on disk like
any other and gets no more trust than a row does. Both mutation-checked.

> **What it does not cover.** The stamp says what is *on disk*. A resume
> [`Cursor`](05-resume.md) is in flight rather than on disk and carries its own version, for
> the same reason and on a separate counter — the two move independently, and a cursor is
> checked against the build that reads it rather than against a database.

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
descriptive, never identity. Conflict handling is order-independent (strict reject — neither
first- nor last-writer-wins; Glean's default is the same reject, and the paths where it
disables that rule land on *first*-writer-wins). This is why [I11](#i11) fact-ids are *not*
cross-DB identity.

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
