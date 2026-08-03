//! The store layer.
//!
//! The fjall [`FactStore`](crate::focus::plan::FactStore) implementation and the
//! atomic `put_fact` seeding primitive land here in **Phase 1**
//! ([`PLAN.md`](../../PLAN.md) tasks 1a/1b). Today the module holds only that
//! phase's invariant guards, written up front as the specification of what the
//! implementation must satisfy.
//!
//! The guards below are `#[ignore]`d because their *subject* doesn't exist yet,
//! not because the properties are undecided — each states the procedure Phase 1
//! has to make pass. They sit in a `pending_phase_1` module so
//! `cargo test -- --ignored --list` (the coverage ledger) names the phase that
//! owns them. See [`docs/testing.md`](../../docs/testing.md).

/// Phase-1 invariant guards: [I8](../../docs/invariants.md#i8) snapshot release,
/// [I11](../../docs/invariants.md#i11) fact-id allocation,
/// [I12](../../docs/invariants.md#i12) the atomic two-CF write.
///
/// All three are structurally untestable before the fjall store exists: `MemStore`
/// pins no snapshot, and nothing can write a fact yet.
#[cfg(test)]
mod pending_phase_1 {
    // I8 — an immutable snapshot per query, released at suspend. A fjall `Iter`
    // pins a read snapshot, which keeps LSM blocks and a whole superseded
    // generation alive; the executor must therefore be dropped at suspend, not
    // parked.
    //
    // Procedure: wrap the fjall store so every `Scan` it hands out registers
    // itself with a drop probe. Run a query against it, suspend mid-stream
    // (`Stream::Suspend`), and assert the probe sees zero live scans once the
    // suspend returns — the bytes-only `Cursor` is all that survives. Repeat for
    // the terminal stops (cancel, deadline unwind): those must release the
    // snapshot too.
    //
    // Untestable on `MemStore`, whose scan copies rows out and pins nothing —
    // this is why fjall is pulled forward to Phase 1.
    #[test]
    #[ignore = "I8 — pending Phase 1 (needs the fjall store + drop probe, PLAN 1a/1c)"]
    fn snapshot_released_at_suspend() {
        unimplemented!(
            "Phase 1 (task 1c): assert no fjall snapshot survives a suspend, cancel or unwind"
        );
    }

    // I11 — a `FactId` is stable, unique, and never reused within a DB. The
    // scan→point mapping and resume's integrity check both rest on it.
    //
    // Procedure: put N facts through the atomic `put_fact` and collect the
    // returned ids; assert they are unique and strictly increasing. Then reopen
    // the store and put more, asserting the counter resumes *above* the previous
    // maximum — a restart must never hand out an id twice. Drive the same
    // property from several threads at once: uniqueness comes from the atomic
    // counter, not from serialisation by the caller.
    #[test]
    #[ignore = "I11 — pending Phase 1 (needs atomic put_fact, PLAN 1b)"]
    fn factid_unique_monotonic() {
        unimplemented!(
            "Phase 1 (task 1b): assert put_fact ids are unique, monotonic, and never reused across a reopen"
        );
    }

    // I12 — a fact is written to both column families atomically. A dangling half
    // is silent corruption: a key with no entity surfaces as `DanglingFactId` at
    // projection, an entity with no key is invisible to every query.
    //
    // Procedure: put facts, then walk the whole `keys` CF and assert every key
    // resolves to an entity whose key bytes match — and the converse, that every
    // entity is reachable by a scan. Then inject a failure between the two CF
    // writes and assert *neither* half is present afterwards, so the batch is all
    // or nothing rather than merely usually-ordered-correctly.
    #[test]
    #[ignore = "I12 — pending Phase 1 (needs the two-CF write batch, PLAN 1b)"]
    fn no_half_present_facts() {
        unimplemented!(
            "Phase 1 (task 1b): assert no half-present facts, including under a failure between CF writes"
        );
    }
}
