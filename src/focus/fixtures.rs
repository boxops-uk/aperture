//! Hand-built fixtures shared by the executor batteries — key-field encoders, an
//! interner builder, and a plan runner.
//!
//! Test machinery, not a product backend. Lives in a support module so tests
//! import these rather than redefining helpers inline (see `docs/testing.md`).

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use byteview::ByteView;
use lasso::Rodeo;
use tokio_util::sync::CancellationToken;

use crate::focus::{
    error::ApertureError,
    iter::{Cursor, Executor, Iteratee, Stream},
    plan::{Entity, FactId, FactStore, Plan},
    schema::{LocalInterner, PREDICATE_ID_SIZE, PredicateId, SchemaInterner},
    tuple::{Value, put_i64, put_str},
};

/// Encode a single i64 key field.
pub fn i64_field(v: i64) -> Vec<u8> {
    let mut b = Vec::new();
    put_i64(&mut b, v);
    b
}

/// Encode a single string key field.
pub fn str_field(s: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, s);
    b
}

/// Concatenate encoded fields into one composite key. The tuple codec is
/// self-delimiting ([I2]), so a multi-field key is just its fields back-to-back.
///
/// [I2]: ../../../docs/invariants.md
pub fn compose(fields: &[&[u8]]) -> Vec<u8> {
    fields.concat()
}

/// A `LocalInterner` whose schema tier holds `names`, so `Project::Record` field
/// symbols (looked up with `LocalInterner::get`) resolve during projection.
pub fn interner_with(names: &[&str]) -> LocalInterner {
    let mut rodeo = Rodeo::new();
    for name in names {
        rodeo.get_or_intern(*name);
    }
    LocalInterner::new(SchemaInterner::new(rodeo.into_reader()))
}

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
    let mut ex = Executor::new(store, plan);

    let out = ex.enumerate(
        Vec::new(),
        |mut acc, row| {
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
    let mut ex = Executor::new(store, plan);

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

        let mut ex = match cursor.take() {
            None => Executor::new(store, plan),
            Some(cursor) => Executor::resume(store, plan, cursor)?,
        };

        let out = ex.enumerate(
            (rows, emitted),
            |(mut rows, n), row| {
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

/// A `FactStore` wrapper that counts `point()` calls, for the I6 guard
/// (`exec::no_value_fetch_in_scan`): a value must be fetched from `entities`
/// only at projection, never during a key-only scan.
pub struct PointSpy<S: FactStore> {
    inner: S,
    point_calls: Arc<AtomicUsize>,
}

impl<S: FactStore> PointSpy<S> {
    /// Wrap `inner`, returning the spy and a handle to its `point()` call count.
    pub fn new(inner: S) -> (Self, Arc<AtomicUsize>) {
        let point_calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner,
                point_calls: Arc::clone(&point_calls),
            },
            point_calls,
        )
    }
}

impl<S: FactStore> FactStore for PointSpy<S> {
    type Scan = S::Scan;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Self::Scan {
        self.inner.scan(lo, hi)
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, ApertureError> {
        self.point_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.point(id)
    }
}

/// A read-only `FactStore` whose scan is **allocation-free per row**, for the I9
/// guard (`exec::scan_is_alloc_free_per_row`). Rows are pre-materialised as
/// owned `ByteView`s behind an `Arc`, so a scan step is a refcount bump, not a
/// copy — `MemStore`'s scan clones a `Vec` per row and can't isolate the
/// executor's own per-row cost.
struct FrozenFact {
    /// Full index key: `predicate_id ++ key_fields`.
    key: ByteView,
    fact_id: FactId,
    value: ByteView,
}

pub struct FrozenStore {
    rows: Arc<[FrozenFact]>,
}

impl FrozenStore {
    /// Build from key-only facts of a single predicate; `key_fields` excludes the
    /// predicate id (prepended here). Rows are sorted by full key.
    pub fn from_keys(
        predicate_id: PredicateId,
        facts: impl IntoIterator<Item = (Vec<u8>, u64)>,
    ) -> Self {
        let mut rows: Vec<FrozenFact> = facts
            .into_iter()
            .map(|(key_fields, id)| {
                let mut full = predicate_id.0.to_be_bytes().to_vec();
                full.extend_from_slice(&key_fields);
                FrozenFact {
                    key: ByteView::from(full),
                    fact_id: FactId(id),
                    value: ByteView::from(Vec::new()),
                }
            })
            .collect();
        rows.sort_by(|a, b| a.key.as_ref().cmp(b.key.as_ref()));
        Self { rows: rows.into() }
    }
}

pub struct FrozenScan {
    rows: Arc<[FrozenFact]>,
    idx: usize,
    end: usize,
}

impl Iterator for FrozenScan {
    type Item = Result<(ByteView, FactId), ApertureError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx < self.end {
            let fact = &self.rows[self.idx];
            self.idx += 1;
            // ByteView clone is a refcount bump (or inline copy) — no heap alloc.
            Some(Ok((fact.key.clone(), fact.fact_id)))
        } else {
            None
        }
    }
}

impl FactStore for FrozenStore {
    type Scan = FrozenScan;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> FrozenScan {
        let start = self.rows.partition_point(|f| f.key.as_ref() < lo);
        let end = match hi {
            Some(hi) => self.rows.partition_point(|f| f.key.as_ref() < hi),
            None => self.rows.len(),
        };
        FrozenScan {
            rows: Arc::clone(&self.rows),
            idx: start,
            end,
        }
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, ApertureError> {
        Ok(self.rows.iter().find(|f| f.fact_id == id).map(|f| Entity {
            key: f.key.slice(PREDICATE_ID_SIZE..),
            value: f.value.clone(),
        }))
    }
}
