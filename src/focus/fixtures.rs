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
    tuple::{Value, put_i64, put_str, strinc},
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
    let ex = Executor::new(store, plan);

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

/// Assert the [`FactStore`] scan contract: **every row a scan yields lies inside
/// the predicate named by `lo`'s prefix.**
///
/// This is a contract on the trait, not a property of one backend, so every impl
/// is held to it — including for `hi = None`, which the trait permits and which a
/// store must therefore clamp itself. A scan that walks past the predicate binds a
/// *different* predicate's row into the register, and the join above it silently
/// produces wrong results rather than failing.
///
/// [`FjallStore`](crate::focus::store::FjallStore) satisfies this structurally —
/// one keyspace per predicate, so there is nothing else in the tree to walk into.
/// [`MemStore`](super::mem_store::MemStore) holds every predicate in one
/// map and has to clamp explicitly; it did not, and an unbounded scan walked on
/// into the next predicate's rows. That bug is why this assertion exists.
///
/// [I1]: ../../../docs/invariants.md
pub fn assert_scan_stays_in_predicate<S: FactStore>(
    store: &S,
    lo: &[u8],
    hi: Option<&[u8]>,
) -> Result<(), ApertureError> {
    let predicate = lo
        .get(..PREDICATE_ID_SIZE)
        .expect("a scan bound names a predicate in its first four bytes");

    for row in store.scan(lo, hi) {
        let (key, fact_id) = row?;
        assert!(
            key.starts_with(predicate),
            "scan from {lo:?} (hi {hi:?}) yielded {key:?} (fact {fact_id:?}), \
             which is outside predicate {predicate:?}"
        );
    }

    Ok(())
}

/// A `FactStore` wrapper counting how many things it has handed out that are
/// **still alive** — itself, plus every scan it opened — for the I8 guard
/// (`store::snapshot_released_at_suspend`).
///
/// Both have to be counted, because either alone keeps a fjall read snapshot
/// pinned: the store handle owns the `Snapshot`, and every `Iter` fjall hands out
/// holds its own clone of the snapshot nonce. A guard that watched only the
/// iterators would pass while the whole snapshot stayed open.
///
/// The count reaching zero is the *localising* half of the guard — it says which
/// object survived. The authoritative half is fjall's own open-snapshot count
/// (`FjallDb::open_snapshots`).
pub struct DropProbe<S: FactStore> {
    inner: S,
    live: Arc<AtomicUsize>,
}

impl<S: FactStore> DropProbe<S> {
    /// Wrap `inner`, returning the probe and a handle to its live-object count.
    /// The count starts at 1: the store handle itself.
    pub fn new(inner: S) -> (Self, Arc<AtomicUsize>) {
        let live = Arc::new(AtomicUsize::new(1));
        (
            Self {
                inner,
                live: Arc::clone(&live),
            },
            live,
        )
    }
}

impl<S: FactStore> Drop for DropProbe<S> {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct ProbedScan<I> {
    inner: I,
    live: Arc<AtomicUsize>,
}

impl<I> Drop for ProbedScan<I> {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<I: Iterator> Iterator for ProbedScan<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        self.inner.next()
    }
}

impl<S: FactStore> FactStore for DropProbe<S> {
    type Scan = ProbedScan<S::Scan>;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Self::Scan {
        self.live.fetch_add(1, Ordering::SeqCst);
        ProbedScan {
            inner: self.inner.scan(lo, hi),
            live: Arc::clone(&self.live),
        }
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, ApertureError> {
        self.inner.point(id)
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
        Self::from_facts(
            facts
                .into_iter()
                .map(move |(key_fields, sequence)| (predicate_id, key_fields, sequence)),
        )
    }

    /// Build from key-only facts spread across predicates.
    ///
    /// The multi-predicate case is what makes the scan contract testable here at
    /// all — a store holding one predicate cannot leak out of it.
    ///
    /// `sequence` is the fact's number *within its predicate*, not a raw
    /// [`FactId`], for the same reason [`MemStore::insert_valued`] takes one: the
    /// real store composes a snowflake id from the two, so a fixture that took
    /// whole ids could hold a fact tagged for a different predicate — or, as this
    /// one did, sequence 0, which [I11] reserves precisely so that no valid id is
    /// `FactId(0)`.
    ///
    /// [I11]: ../../../docs/invariants.md
    /// [`MemStore::insert_valued`]: super::mem_store::MemStore::insert_valued
    pub fn from_facts(facts: impl IntoIterator<Item = (PredicateId, Vec<u8>, u64)>) -> Self {
        let mut rows: Vec<FrozenFact> = facts
            .into_iter()
            .map(|(predicate_id, key_fields, sequence)| {
                let mut full = predicate_id.0.to_be_bytes().to_vec();
                full.extend_from_slice(&key_fields);
                FrozenFact {
                    key: ByteView::from(full),
                    fact_id: FactId::new(predicate_id, sequence).expect("test fixture fact id"),
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
        // A scan never crosses out of the predicate named by `lo`'s prefix — the
        // trait's contract, which [`assert_scan_stays_in_predicate`] holds every
        // impl to. Like `MemStore`, this store keeps every predicate in one
        // sorted run and so has to clamp explicitly; the real store gets it
        // structurally from one keyspace per predicate.
        //
        // A fixture that shipped beside that assertion while breaking it is worse
        // than one that never claimed to satisfy it, because it reads as evidence.
        let predicate_end = lo.get(..PREDICATE_ID_SIZE).and_then(strinc);
        let upper = match (hi, predicate_end.as_deref()) {
            (Some(hi), Some(predicate_end)) => Some(hi.min(predicate_end)),
            (hi, predicate_end) => hi.or(predicate_end),
        };

        let start = self.rows.partition_point(|f| f.key.as_ref() < lo);
        let end = match upper {
            Some(upper) => self.rows.partition_point(|f| f.key.as_ref() < upper),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// [`FrozenStore`] is held to the same scan contract as every other
    /// `FactStore`, including the `hi = None` case the trait permits — the case
    /// `MemStore` was once wrong about, and this store was silently wrong about
    /// too until it gained a second predicate to leak into.
    #[test]
    fn frozen_store_scan_stays_in_its_predicate() {
        let (first, second) = (PredicateId(0), PredicateId(1));

        let store = FrozenStore::from_facts([
            (first, i64_field(1), 1),
            (first, i64_field(2), 2),
            (second, i64_field(1), 1),
            (second, i64_field(2), 2),
        ]);

        for predicate in [first, second] {
            let lo = predicate.0.to_be_bytes().to_vec();
            let hi = strinc(&lo);

            for hi in [hi.as_deref(), None] {
                assert_scan_stays_in_predicate(&store, &lo, hi).expect("frozen scan");
            }

            // ...and it yields the predicate's rows rather than none of them.
            let rows = store.scan(&lo, None).count();
            assert_eq!(rows, 2, "predicate {} lost its rows", predicate.0);
        }
    }
}
