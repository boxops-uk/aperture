---
title: Invariants
description: The rules this design is checked against — fifteen engine invariants and ten operational ones, each with the guard test that pins it.
---

An invariant here is not documentation of intent. Each one names a **guard test**, guards are
written *up front* (the property statement is the spec), and a phase is finished only when the
invariants it touches are un-ignored and green.

```bash
cargo test                       # the green suite
cargo test -- --ignored --list   # the coverage ledger: guards written, not yet live
```

Two namespaces, never conflated: **`I1`–`I15`** are engine rules; **`ops-I1`–`ops-I10`** are
operational ones and are always written with the prefix.

## Engine invariants

| # | Statement | Guard | Status |
|---|---|---|---|
| [I1](#i1) | Key encoding is order-preserving | `codec::test_typed_value_order_matches_encoded_order` + round-trip | green |
| [I2](#i2) | Encoding is self-delimiting; `skip` needs no schema | the `codec::test_skip_*` family | green |
| [I3](#i3) | The marker table is frozen on disk | `codec::marker_table_golden` | green |
| [I4](#i4) | Resume equals an uninterrupted run | `exec::resume_equals_uninterrupted` + the fjall arm | green on both stores |
| [I5](#i5) | A register holds the whole row; fields decode lazily | `exec::bind_is_refcount_not_decode` | green |
| [I6](#i6) | Values never enter the scan hot loop | `exec::no_value_fetch_in_scan` | green |
| [I7](#i7) | The executor is a defunctionalised state machine | structural + the resume battery | green |
| [I8](#i8) | Immutable snapshot per query, released at suspend | `i8_snapshot::snapshot_released_at_suspend` | green |
| [I9](#i9) | The hot path is allocation-free per row | `exec::scan_is_alloc_free_per_row` | green |
| [I10](#i10) | Union discriminants are stable and append-only | `schema::discriminants_append_only` | **pending** unions |
| [I11](#i11) | A `FactId` is stable, unique and never reused within a database | `store::factid_unique_monotonic` + `exhausted_sequence_space_is_an_error` | green |
| [I12](#i12) | Both maps are written atomically — and a key names exactly one fact | `store::no_half_present_facts_after_writes` + the crash case + `concurrent_interning_of_one_key_creates_one_fact` | green |
| [I13](#i13) | The database's schema is embedded and frozen at create | `i13_embedded_schema::ingest_rejects_incompatible_schema` + the fingerprint metamorphic | green |
| [I14](#i14) | A derived bind is a pure function of the fact bindings | `iter::a_derive_is_recomputed_across_every_cut_point` | green |
| [I15](#i15) | A database says which format wrote it; an unreadable one is refused | `store::a_database_says_which_format_wrote_it` + `a_corrupt_format_stamp_is_reported` | green |

Exactly one guard is `#[ignore]`d — I10's, which waits on union types existing to have
discriminants.

### I1 — Key encoding is order-preserving {#i1}

`memcmp(encode(a), encode(b)) == compare(a, b)`. What that buys, precisely: a **value-range**
scan as a bounded seek rather than a filter, and rows in semantic order with no sort. An exact
*prefix* scan needs only a canonical self-delimiting encoding and no ordering at all — so this is
a deliberate divergence whose divergent half is partly unspent, since no query lowers a range
seek yet.

It is kept because it is nearly free to hold and impossible to retrofit. What **is** spent today
is the store-level half — a scan yields rows in lexicographic key order, which resume re-seeks
against — and that is a commitment.

*The gate for any codec change.* [Storage → the tuple codec](storage.html#the-tuple-codec)

### I2 — The encoding is self-delimiting {#i2}

The marker byte alone says how to advance past a value; a full decode consumes exactly to
end-of-input; record nesting is bounded, so malformed bytes are an error rather than a stack
overflow. `skip` therefore needs no schema, which is what lets the scan hot loop walk to the
*n*th field of a row it holds no type for.

### I3 — The marker table is frozen on disk {#i3}

Marker values **and their relative order** are semantic, because a marker is the most significant
part of a value's sort key. New types take a reserved band in the right skip family; renumbering
an existing marker after data exists silently corrupts every stored key. A golden-bytes test pins
every marker so a renumber breaks loudly.

[I15](#i15) does not soften this: a database stamped `codec 1` is bound exactly as before. What
the stamp buys is that a *future* codec is a different number rather than an impossibility.

### I4 — Resume equals an uninterrupted run {#i4}

Resuming from a `Cursor` produces exactly the rows an uninterrupted run would, in exactly the
order.

*Guard:* a tier-3 model-based property over generated `(plan, store)` pairs **and a generated
interruption schedule** — suspend at every cut point, in every combination, and compare against a
run to completion. Run against the in-memory store and the real one, where the two must also agree
row for row and id for id.

[Executor → the cursor](executor.html#the-cursor-bytes-and-nothing-else)

### I5 — A register holds the whole row {#i5}

The *field* a variable denotes lives in the **plan**, not the register — so a generator binding N
variables is N refcount bumps on one row, with no per-field decode at bind time. Fields decode
lazily at read and projection sites.

Why: at bind time you do not know which fields will be read, and a row may be bound and then
discarded when an inner loop finds no match.

One recorded narrowing: a variable a **disjunction** binds cannot always stay a lazy row, because
two branches reach a value at different paths. The rule is that it stays a row slot if every
branch binds it to a whole row of the same predicate, and otherwise each branch materialises it
into a value slot. Conjunctive plans are unaffected.

### I6 — Values never enter the scan hot loop {#i6}

The hot loop touches the index map only. A value is a point read, taken when a projection asks for
it. Two consequences reach the language: **a value cannot be matched on**, and the fix for
"I need to filter on this" is to put it in the key.

### I7 — The executor is a defunctionalised state machine {#i7}

The driver plus the frame stack are the explicit reification of a recursive `concatMap`, chosen so
execution can **suspend to bytes**. Closures and coroutines cannot: a suspended closure pins live
iterators and a snapshot.

*Do not "simplify" the driver back into recursion.* The neighbouring decision — declining a
bytecode VM — turns on token size and token stability, not on capability.

### I8 — Immutable snapshot per query, released at suspend {#i8}

A query reads a snapshot, and every stop releases it: suspend, cancel, terminal unwind alike. A
paused query that leaves an iterator alive is as much a leak as a suspended one.

*Guard:* cross-checks a drop probe against the storage engine's own count of open snapshots,
because "we dropped our handle" and "the engine considers it closed" are two different claims.

### I9 — The hot path is allocation-free per row {#i9}

Reused scratch buffers; refcount-bump clones; inline field-offset caches that never spill. Copy
out only at escape boundaries — a suspend, and a string or bytes projection.

*Guard:* a counting global allocator asserts that scanning N and 2N rows allocates the same count
**and** bytes, with a positive control proving the allocator is linked. The caveat the project
records: the guard runs a single-level plan, and opening a level allocates — so a join allocates
once per outer row, and no guard covers that.

### I10 — Union discriminants are stable and append-only {#i10}

Like protobuf field numbers: each alternative has an explicit discriminant, assigned once, never
reused, new alternatives appended. Frozen the moment union-typed data is written.

Why it is a one-way door: a union value is stored tagged by its discriminant, so discriminants
derived from position or from sorted names would **silently renumber** existing ones and
misinterpret every stored value. This is why the schema DSL has syntax for writing the number
down — and why the type does not exist yet rather than existing with the tags left implicit.

### I11 — A `FactId` is stable, unique, never reused {#i11}

Assigned once at ingest; a snowflake, with the predicate in the high 24 bits and a per-predicate
sequence in the low 40. Uniqueness across predicates is structural rather than enforced.

A physical id, **not** cross-database identity. It is also the prerequisite for the resume
integrity check: a saved key that still resolves to the saved fact is only meaningful if ids do
not move.

### I12 — Both maps are written atomically, and a key names exactly one fact {#i12}

Two halves, and the second is the one that took a mechanism.

**Atomicity:** a fact is never half-present. A key with no entity makes a value projection return
nothing; an entity with no key is invisible to every query — silent, and undetectable without
checking both directions.

**Write-once:** writing the same key twice overwrites the key row and strands the first fact's
entity. Held for a long time by there being one writer thread — a property no test can observe —
and now held by **per-key exclusion striped by `hash(predicate ++ key)`**, which needs no lock
ordering because interning is bottom-up and critical sections are never nested.

*Guard:* a bijection check over generated writes, a crash test that aborts a child process
mid-write, and N threads racing to intern one key.

### I13 — The schema is embedded and frozen at create {#i13}

The canonical schema and its fingerprint are embedded at `create` and immutable for the database's
lifetime. Every ingest is validated against that copy, by **subset containment**.

What it buys: an artifact is self-describing and portable, a handshake can compare fingerprints
before any bytes flow, and a server can serve one store root's databases from *their own*
schemas rather than from whatever it was started with.

Its boundary condition, stated: it says nothing about a *reader* older than the database it opens.
That mismatch is between a query and a database rather than between two schemas on disk, and
lockstep rebuild of the reader is the answer.

### I14 — A derived bind is a pure function of the fact bindings {#i14}

Which is what makes it recomputable on resume instead of saved — the general form being the
**recompute rule**: in an immutable database a store read is a pure function of its inputs, so
anything determined by the bindings and the frozen base may be recomputed rather than saved.

*Guard:* a derive step is recomputed across **every** cut point, and the rows match an
uninterrupted run.

### I15 — A database says which format wrote it {#i15}

A twelve-byte stamp in a metadata keyspace, with `codec` and `storage` versioned separately,
checked at open. An unreadable database is **refused**, and one holding facts with no stamp at all
is refused rather than adopted.

## Operational invariants

Explained in full on the [Operations](operations.html) page.

| # | Statement |
|---|---|
| `ops-I1` | Single-**process** store ownership; no silent connect→open fallback |
| `ops-I2` | Complete = immutable; every write refused at session establishment |
| `ops-I3` | Finish ordering: durable first, status flip last |
| `ops-I4` | Reproducibility; identity is `hash(canonical schema, base facts)`; conflicts reject, order-independently |
| `ops-I5` | One write funnel — one *pipeline*, not one thread |
| `ops-I6` | Session modes declared at open, resolved once against status |
| `ops-I7` | The filesystem is the catalog |
| `ops-I8` | Derivation is phased: create → ingest → derive → finish |
| `ops-I9` | No cross-database anything in P0 |
| `ops-I10` | No in-database auth; the transport is the trust boundary |

## The anti-patterns each one forbids

Every item here looks reasonable and breaks a specific invariant. The project keeps this list
because the dominant failure mode is a large, mostly-correct change whose 10%-wrong part is
expensive to find.

| Don't | Breaks |
|---|---|
| Materialise a full result set | The streaming contract; and aggregation cannot be made suspend-free |
| Decode fields eagerly at bind | [I5](#i5), [I9](#i9) |
| Fetch a value inside the scan loop | [I6](#i6) |
| Hold an iterator across a suspend | [I8](#i8) |
| Rewrite the driver as recursion | [I7](#i7) |
| Write one map without the other | [I12](#i12) |
| Renumber markers or discriminants after data exists | [I3](#i3), [I10](#i10) |
| DNF-expand a disjunction across conjuncts | Exponential blow-up; and the plan shape the machine has |
| Reshape the machine for an "additive" feature | The rule that a construct may add a source, a test or a residual — never a `Step` |
| Use a hash map for record fields | Deterministic order is a codec requirement |
| `unwrap` on decoded data | Errors, not panics, on data paths |
| "Restore" the single writer to fix an ordering problem | [I12](#i12)'s mechanism — and a conflict rule that picks a winner breaks `ops-I4` |
