//! [I8](../../../docs/invariants.md#i8) — an immutable snapshot per query,
//! released at every stop.
//!
//! An **integration** test rather than a unit test, and for a structural reason:
//! it is the one store guard that has to *run a query*, so it needs the engine,
//! and the engine depends on this crate. Reaching back through a dev-dependency
//! would compile a second copy of `aperture-store` — the store under test and
//! the store the engine links would be different types. From outside the crate
//! there is one of each, which is also the arrangement a caller sees.

use std::sync::atomic::Ordering;

use aperture::focus::{
    error::ApertureError,
    fixtures::{DropProbe, collect_rows, i64_field, interner_with},
    iter::{CANCELLATION_STRIDE, Executor, Iteratee, Stream},
    plan::{
        Access, Address, FieldPath, Level, Plan, Project, Residual, ResidualOp, SeekKey,
        SeekKeyPart, Step,
    },
};
use aperture_schema::schema::{PredicateId, PredicateTy};
use aperture_store::store::FjallDb;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// [I8](../../../docs/invariants.md#i8) — an immutable snapshot per query,
/// released at **every** stop.
///
/// A fjall scan pins a read snapshot, and a pinned snapshot keeps LSM blocks
/// and a whole superseded generation alive, so an idle portal must hold none.
/// `Executor::enumerate` takes `self` by value, which is what makes this
/// structural: done, suspend, cancel and error unwind all drop the frame stack
/// and the store handle. This asserts it for each of the four, against two
/// independent witnesses — the drop probe (which object is still alive) and
/// fjall's own open-snapshot count (whether the engine still considers the
/// snapshot open).
///
/// Untestable on `MemStore`, whose scan copies rows out and pins nothing —
/// which is why fjall is pulled forward to Phase 1.
#[test]
fn snapshot_released_at_suspend() {
    let (db, _dir, _) = snapshot_probe_db();
    let (outer, inner) = (PredicateId(0), PredicateId(1));
    let interner = interner_with(&[]);
    let cancel = CancellationToken::new();

    assert_eq!(db.open_snapshots(), 0, "a seeded DB holds no snapshot");

    // Positive control: while a run is in flight, both witnesses must see the
    // snapshot pinned. Without this the assertions below could pass vacuously.
    //
    // The two counts are independently derived and must agree exactly. fjall's
    // tracker counts live *references* to a snapshot nonce — `Snapshot` holds
    // one and each `Iter` clones it — so at a cut inside a two-level plan it
    // reads 3: the reader plus one per open scan, which is precisely what the
    // drop probe is counting.
    let (probe, live) = DropProbe::new(db.reader());
    assert_eq!(db.open_snapshots(), 1, "a reader must open a snapshot");
    assert_eq!(live.load(Ordering::SeqCst), 1);

    let mut mid_run = None;
    let out = Executor::new(probe, two_level_plan(outer, inner))
        .enumerate(
            0usize,
            |n, _row| {
                mid_run = Some((db.open_snapshots(), live.load(Ordering::SeqCst)));
                Ok(Stream::Suspend(n + 1))
            },
            &cancel,
        )
        .expect("run");

    assert!(
        matches!(out, Iteratee::Suspended(1, _)),
        "expected a suspend"
    );
    assert_eq!(
        mid_run,
        Some((3, 3)),
        "mid-run: expected the store handle plus two open scans, seen by both witnesses"
    );
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "the executor kept a scan or the store handle alive past a suspend"
    );
    assert_eq!(
        db.open_snapshots(),
        0,
        "a fjall snapshot survived a suspend — an idle portal is pinning LSM blocks"
    );

    // Running to completion releases it too.
    let (probe, live) = DropProbe::new(db.reader());
    let rows = collect_rows(probe, two_level_plan(outer, inner), &interner).expect("run");
    assert_eq!(rows.len(), 6, "the plan must produce rows to be a real run");
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(
        db.open_snapshots(),
        0,
        "a snapshot survived a completed run"
    );

    // An error unwinding out of `step` releases it: the executor is consumed on
    // that path too, so there is no "failed query still holding a snapshot".
    let (probe, live) = DropProbe::new(db.reader());
    let failed = Executor::new(probe, two_level_plan(outer, inner)).enumerate(
        (),
        |(), _row| Err(ApertureError::AdvanceAfterClose),
        &cancel,
    );
    assert!(failed.is_err(), "the step was supposed to fail");
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(
        db.open_snapshots(),
        0,
        "a snapshot survived an error unwind"
    );

    // And a cancellation, which is the one stop that needs setting up rather
    // than asking for: the token is polled every `CANCELLATION_STRIDE` rows
    // examined, so the run has to be at least that long to reach a poll.
    //
    // The shape here is the *skipped*-row half — a residual that rejects the
    // whole predicate bar one row — because that is the half where a snapshot
    // is held across the most work. The matched-row half is
    // `iter::a_matching_scan_observes_cancellation`, which is about the poll
    // interval rather than the snapshot.
    let dir = TempDir::new().expect("tempdir");
    let big = FjallDb::open(dir.path()).expect("open");
    let last = CANCELLATION_STRIDE as i64;
    for x in 0..=last {
        big.put_fact(outer, &i64_field(x), &[]).expect("put");
    }

    let filtered = Plan {
        nvars: 1,
        body: Step::levels([Level::seek(
            Access {
                predicate_id: outer,
                seek_key: SeekKey::Prefix(Box::new([])),
            },
            Box::new([Address::new(0)]), // Matches only the final key, so the run skips every other row
            // and trips the poll on the way.
            Box::new([Residual {
                path: FieldPath::field(0),
                op: ResidualOp::EqConst(i64_field(last).into_boxed_slice()),
            }]),
        )]),
        head: Project::RegisterField {
            address: Address::new(0),
            path: FieldPath::field(0),
            ty: PredicateTy::Int,
        },
    };

    let cancelled = CancellationToken::new();
    cancelled.cancel();

    let (probe, live) = DropProbe::new(big.reader());
    let stopped = Executor::new(probe, filtered).enumerate(
        0usize,
        |n, _row| Ok(Stream::Continue(n + 1)),
        &cancelled,
    );

    assert!(
        matches!(stopped, Err(ApertureError::Cancelled)),
        "expected the run to be cancelled, got {:?}",
        stopped.map(|_| "a completed run")
    );
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(
        big.open_snapshots(),
        0,
        "a snapshot survived a cancellation"
    );
}

/// A two-level plan over `outer` and `inner`: scan `outer`, seek `inner` by the
/// outer row's first field, project the outer field. Two levels means two scans
/// are open at a mid-stream cut, so the probe is watching more than one thing.
fn two_level_plan(outer: PredicateId, inner: PredicateId) -> Plan {
    Plan {
        nvars: 2,
        body: Step::levels([
            Level::seek(
                Access {
                    predicate_id: outer,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                Box::new([Address::new(0)]),
                Box::new([]),
            ),
            Level::seek(
                Access {
                    predicate_id: inner,
                    seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                    }])),
                },
                Box::new([Address::new(1)]),
                Box::new([]),
            ),
        ]),
        head: Project::RegisterField {
            address: Address::new(0),
            path: FieldPath::field(0),
            ty: PredicateTy::Int,
        },
    }
}

/// A DB whose two-level plan yields several rows: three `outer` facts, each
/// with two `inner` matches.
fn snapshot_probe_db() -> (FjallDb, TempDir, Plan) {
    let dir = TempDir::new().expect("tempdir");
    let db = FjallDb::open(dir.path()).expect("open");
    let (outer, inner) = (PredicateId(0), PredicateId(1));

    for x in 1i64..=3 {
        db.put_fact(outer, &i64_field(x), &[]).expect("put");
        for y in [10i64, 20] {
            let key = [i64_field(x), i64_field(y)].concat();
            db.put_fact(inner, &key, &[]).expect("put");
        }
    }

    let plan = two_level_plan(outer, inner);
    (db, dir, plan)
}
