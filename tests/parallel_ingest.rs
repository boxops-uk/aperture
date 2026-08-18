//! **A database does not depend on how many threads wrote it, or in what order.**
//!
//! [Phase 12d](../PLAN.md)'s second acceptance criterion, and the one that decides whether
//! the [striped merge frontier](../crates/aperture-store/src/store.rs) is *correct* rather
//! than merely uncontended. `concurrent_interning_of_one_key_creates_one_fact` proves the
//! narrow case — every thread reaching for one key gets one fact. This proves the wide
//! one: run a whole nested corpus three ways and the artifact is the same artifact.
//!
//! # Why identity is the assertion and not fact ids
//!
//! [`ops-I4`](../docs/aperture-cli-design.md) says a database built twice from identical
//! inputs is identical, and it means identical *by content hash* — a multiset over each
//! fact's logical form, no physical `FactId` anywhere in it. That distinction was
//! load-bearing enough to reverse a design decision four documents had recorded as forced
//! (["parallel writes", open decisions](../docs/open-decisions.md)), and until this file it
//! had never been exercised against anything but a single-threaded, in-order ingest. An
//! invariant nothing has tried to break is a hope.
//!
//! # The reversed arm is what makes the parallel arm mean something
//!
//! A parallel run *probably* assigns ids in a different order, but "probably" is not a
//! test: eight threads might interleave neatly and prove nothing. Reversing the emission
//! order makes different ids **certain** and deterministic — the last fact is created
//! first — so the reversed arm is what demonstrates that identity ignores id assignment,
//! and the parallel arm then only has to agree with it.

use std::sync::Arc;

use aperture_cli::{code_index, workload::Corpus};
use aperture_ingest::intern_fact;
use aperture_schema::{fingerprint, schema::Schema};
use aperture_store::{identity, store::FjallDb};
use aperture_wire::WireFact;

/// Small enough to run three times in a test, deep enough that every level of nesting is
/// exercised: a reference names a declaration names a module names a file.
fn corpus() -> Corpus {
    Corpus {
        files: 12,
        modules_per_file: 2,
        decls_per_module: 4,
        refs_per_decl: 3,
    }
}

/// A fresh database with its trees already built, and the facts to put in it.
fn scratch(schema: &Schema) -> (tempfile::TempDir, FjallDb) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let db = FjallDb::open(dir.path()).expect("a database");

    let declared = (0..schema.len())
        .map(|n| aperture_schema::schema::PredicateId(u32::try_from(n).expect("a schema id fits")))
        .filter(|id| !schema.is_virtual(*id));
    db.create_predicates(declared).expect("the trees");

    (dir, db)
}

/// Every top-level fact the corpus emits, flattened out of its per-predicate blocks.
fn facts(schema: &Schema) -> Vec<WireFact> {
    corpus()
        .emit(schema)
        .into_iter()
        .flat_map(|emission| emission.facts)
        .collect()
}

/// **No fact was created twice**, checked from outside the store.
///
/// A double-create is the exact corruption the frontier prevents, and it is invisible to a
/// row count: both writers use the same key, so the second `keys` row overwrites the first
/// and the tree still holds one row — while the loser's `entities` row sits there with
/// nothing pointing at it. What gives it away is the **allocator**. Ids are handed out per
/// predicate from 1 with no reuse ([I11](../docs/invariants.md#i11)), so if every id issued
/// still has a `keys` row naming it, the highest sequence in a predicate equals its number
/// of rows. A stranded entity leaves that gap.
///
/// Only sound because nothing here fails: a *failed* write legitimately consumes a
/// sequence and I11 permits the hole. In this test every intern succeeds.
fn assert_no_id_was_created_twice(db: &FjallDb) {
    use aperture_store::fact_store::FactStore;
    let reader = db.reader();

    for predicate in db.predicate_ids() {
        let mut rows = 0u64;
        let mut highest = 0u64;
        for row in reader
            .scan(&predicate.0.to_be_bytes(), None)
            .expect("a scan")
        {
            let (_, id) = row.expect("a row");
            rows += 1;
            highest = highest.max(id.sequence());
            assert!(
                reader.point(id).expect("a read").is_some(),
                "predicate {} has a `keys` row naming {id:?} with no entity",
                predicate.0
            );
        }

        assert_eq!(
            highest,
            rows,
            "predicate {} handed out {highest} ids for {rows} rows: {} sequence(s) were              consumed by a fact nothing can reach",
            predicate.0,
            highest - rows,
        );
    }
}

fn identity_of(db: &FjallDb, schema: &Schema) -> identity::Identity {
    identity::compute(db, schema, fingerprint::identity(schema).schema()).expect("an identity")
}

/// Ids in creation order, so two arms can be shown to have assigned them differently.
fn ids(db: &FjallDb) -> Vec<u64> {
    use aperture_store::fact_store::FactStore;
    let reader = db.reader();
    let mut out = Vec::new();
    for predicate in db.predicate_ids() {
        for row in reader
            .scan(&predicate.0.to_be_bytes(), None)
            .expect("a scan")
        {
            out.push(row.expect("a row").1.raw());
        }
    }
    out
}

#[test]
fn writer_count_and_write_order_do_not_change_the_database() {
    let schema = code_index::schema();
    let facts = facts(&schema);
    let expected = corpus().facts();

    // --- one thread, in the order a walk reaches things ---
    let (_forward_dir, forward) = scratch(&schema);
    for fact in &facts {
        intern_fact(&forward, &schema, fact).expect("it ingests");
    }
    let forward_identity = identity_of(&forward, &schema);
    assert_eq!(
        forward_identity.facts, expected,
        "the corpus states {expected} facts"
    );

    // --- one thread, backwards: the last fact is created first ---
    let (_backward_dir, backward) = scratch(&schema);
    for fact in facts.iter().rev() {
        intern_fact(&backward, &schema, fact).expect("it ingests");
    }

    assert_eq!(
        identity_of(&backward, &schema),
        forward_identity,
        "reversing the emission order must not change the artifact"
    );
    assert_ne!(
        ids(&backward),
        ids(&forward),
        "the reversed arm must actually have assigned different ids, or it is proving \
         nothing about whether identity depends on them"
    );

    // --- eight threads over the same facts ---
    //
    // Every thread walks the *whole* list rather than a slice of it, so each fact is
    // offered eight times and the frontier has to decide every key under contention.
    // Slicing would hand most keys to exactly one thread and race only where the
    // subgraphs happen to overlap.
    const THREADS: usize = 8;
    let (_parallel_dir, parallel) = scratch(&schema);
    let parallel = Arc::new(parallel);
    let shared = Arc::new(facts);

    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let (db, schema, facts) = (Arc::clone(&parallel), schema.clone(), Arc::clone(&shared));
            scope.spawn(move || {
                for fact in facts.iter() {
                    intern_fact(&*db, &schema, fact).expect("it ingests");
                }
            });
        }
    });

    assert_eq!(
        identity_of(&parallel, &schema),
        forward_identity,
        "eight writers must produce the database one writer produces"
    );

    // Contention, not just concurrency: eight passes over the same facts must have
    // created each one exactly once, which is `ops-I5`'s dedup holding under a race.
    assert_eq!(
        ids(&parallel).len() as u64,
        expected,
        "eight threads offering every fact eight times wrote {expected} facts once each"
    );
    assert_no_id_was_created_twice(&parallel);

    // And the serial arms, so a failure here is attributable to the race rather than to
    // interning in general.
    assert_no_id_was_created_twice(&forward);
    assert_no_id_was_created_twice(&backward);
}

/// **Committing once per block produces the same database as committing per fact** —
/// [12f](../PLAN.md).
///
/// The flag `serve --commit-per-block` trades a durability property, and it must trade
/// *only* that: same facts, same dedup, same content hash. What differs is when the bytes
/// become durable and — because the reservation claims ids in chunks — which numbers they
/// were given, which is precisely the thing `ops-I4` does not look at.
///
/// Deliberately compared against the *forward* arm above rather than against itself, so
/// this is a claim about the two write paths agreeing rather than about staging being
/// self-consistent.
#[test]
fn committing_once_per_block_writes_the_same_database() {
    let schema = code_index::schema();
    let facts = facts(&schema);

    let (_per_fact_dir, per_fact) = scratch(&schema);
    for fact in &facts {
        intern_fact(&per_fact, &schema, fact).expect("it ingests");
    }

    let (_staged_dir, db) = scratch(&schema);
    let staged = db.staged();
    for fact in &facts {
        intern_fact(&staged, &schema, fact).expect("it ingests");
    }
    assert_eq!(
        staged.pending() as u64,
        corpus().facts(),
        "every fact is staged before any of them is durable"
    );
    staged.commit().expect("the block commits");

    assert_eq!(
        identity_of(&db, &schema),
        identity_of(&per_fact, &schema),
        "one commit per block must not change the artifact"
    );
    assert_no_id_was_created_twice(&db);
}
