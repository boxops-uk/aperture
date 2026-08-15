//! Hand-built fixtures shared by the executor batteries — key-field encoders, an
//! interner builder, and a plan runner.
//!
//! Test machinery, not a product backend. Lives in a support module so tests
//! import these rather than redefining helpers inline (see `docs/testing.md`).

// The store-shaped half of this toolbox — the probes, the model stores and the
// value helpers — lives in `aperture-store`, because a probe has to be the same
// `FactStore` as the store it is probing. Re-exported here so a battery has one
// place to import from.
pub use aperture_store::fixtures::*;

use std::collections::BTreeSet;

use tokio_util::sync::CancellationToken;

use crate::focus::{
    error::ApertureError,
    iter::{Cursor, Executor, Iteratee, Stream},
    plan::Plan,
};
use aperture_encoding::tuple::Value;
use aperture_schema::schema::LocalInterner;
use aperture_store::fact_store::FactStore;

/// Run `plan` to completion against `store`, collecting every projected row.
///
/// This is the "run to completion, collect rows" reference model the resume
/// battery checks suspend/resume against ([I4]).
///
/// [I4]: ../../../docs/invariants.md
pub fn collect_rows<S: FactStore>(
    store: S,
    plan: Plan,
    interner: &LocalInterner,
) -> Result<Vec<Value>, ApertureError> {
    let cancel = CancellationToken::new();
    let ex = Executor::new(store, plan);

    let out = ex.enumerate(
        Vec::new(),
        |mut acc, mut row| {
            acc.push(row.to_value(interner)?);
            Ok(Stream::Continue(acc))
        },
        &cancel,
    )?;

    Ok(match out {
        Iteratee::Done(rows) | Iteratee::Suspended(rows, _) => rows,
    })
}

/// Drive `plan` to completion **without projecting**, returning the row count.
///
/// The NFR guards that must not trigger a read site (I5 lazy-decode, I9
/// alloc-free) use this instead of [`collect_rows`], whose projection step would
/// decode and allocate at the escape boundary.
pub fn count_rows<S: FactStore>(store: S, plan: Plan) -> Result<usize, ApertureError> {
    let cancel = CancellationToken::new();
    let ex = Executor::new(store, plan);

    let out = ex.enumerate(0usize, |n, _row| Ok(Stream::Continue(n + 1)), &cancel)?;

    Ok(match out {
        Iteratee::Done(n) | Iteratee::Suspended(n, _) => n,
    })
}

/// A resume must make progress, so the round-trip count is bounded by the row
/// count. This cap turns a non-advancing resume into a test failure rather than a
/// hang.
const MAX_SUSPENDS: usize = 4096;

/// Run `plan` against `store`, **suspending after every row index in `schedule`**
/// (1-based, counted across the whole run), rebuilding the executor from a
/// bytes-only [`Cursor`] at each resume.
///
/// `mk` must return an equivalent `(store, plan)` pair on every call: the
/// executor consumes both, and a resume is handed a *fresh* pair plus the cursor
/// — which is exactly what the wire path does when an idle portal wakes up. The
/// cursor carries no iterator and no snapshot, so nothing else crosses the gap.
///
/// Returns the projected rows and the number of suspend/resume round-trips
/// actually taken, so a test can assert its schedule wasn't vacuous.
///
/// This is the system-under-test half of the [I4] battery; [`collect_rows`] is
/// the model.
///
/// [I4]: ../../../docs/invariants.md
pub fn run_with_suspends<S: FactStore>(
    mut mk: impl FnMut() -> (S, Plan),
    interner: &LocalInterner,
    schedule: &BTreeSet<usize>,
) -> Result<(Vec<Value>, usize), ApertureError> {
    let cancel = CancellationToken::new();

    let mut rows = Vec::new();
    let mut emitted = 0usize;
    let mut suspends = 0usize;
    let mut cursor: Option<Cursor> = None;

    loop {
        let (store, plan) = mk();

        let ex = match cursor.take() {
            None => Executor::new(store, plan),
            Some(cursor) => Executor::resume(store, plan, cursor)?,
        };

        let out = ex.enumerate(
            (rows, emitted),
            |(mut rows, n), mut row| {
                rows.push(row.to_value(interner)?);
                let n = n + 1;

                if schedule.contains(&n) {
                    Ok(Stream::Suspend((rows, n)))
                } else {
                    Ok(Stream::Continue((rows, n)))
                }
            },
            &cancel,
        )?;

        match out {
            Iteratee::Done((rows, _)) => return Ok((rows, suspends)),
            Iteratee::Suspended((emitted_rows, n), suspended_at) => {
                rows = emitted_rows;
                emitted = n;
                cursor = Some(suspended_at);
                suspends += 1;

                assert!(
                    suspends <= MAX_SUSPENDS,
                    "resume made no progress: {suspends} round-trips for {} row(s)",
                    rows.len()
                );
            }
        }
    }
}
