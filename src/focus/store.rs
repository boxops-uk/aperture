//! The store layer — the fjall [`FactStore`].
//!
//! The layout is [chapter 3](../../docs/03-storage-model.md): a `keys` row is
//! `predicate_id (4B BE) ++ encoded_key → fact_id (8B BE)`, an `entities` row is
//! `fact_id (8B BE) → [key_len u32 BE][encoded_key][value]`, and the two halves of
//! a fact are written in one batch ([I12](../../docs/invariants.md#i12)).
//!
//! Three implementation decisions this module makes; chapter 3 records the
//! reasoning and the measurements behind the first two.
//!
//! - **Both column families are split per predicate** — `keys.<id>` and
//!   `entities.<id>`. Per-predicate trees are what
//!   [operations §9](../../docs/aperture-cli-design.md) asks for: independent
//!   bulk-ingest trees, prefix-disjointness aligned with physical isolation, an
//!   O(1) wholesale drop when a derived predicate is recomputed, and per-predicate
//!   size/cardinality for free. Splitting `entities` too is what the snowflake
//!   [`FactId`] buys:
//!   [`point`](crate::focus::plan::FactStore::point) is handed a bare id, and the
//!   id's tag names the tree, so identity lookup stays one lookup. Were `entities`
//!   shared, dropping a derived predicate's `keys` tree would strand its values as
//!   unreclaimable garbage.
//! - **A predicate's trees are created on first write**, and
//!   [`FjallDb::create_predicates`] exists so a caller that knows its schema can
//!   pay that cost up front instead — keyspace creation is ~30 ms apiece
//!   (directory create plus fsyncs), which is not a cost to incur at an arbitrary
//!   point inside an ingest.
//! - **The predicate-id prefix stays on the stored `keys` row** even though the
//!   per-predicate tree makes it redundant. It costs 4 highly-compressible bytes
//!   and buys byte-identical rows across this store and
//!   `MemStore` (`src/focus/mem_store.rs`) — which is what lets the
//!   resume battery ([I4](../../docs/invariants.md#i4)) transfer to fjall
//!   unchanged, since a [`Cursor`](crate::focus::iter::Cursor) is bytes-only and
//!   re-seeks by exactly these bytes.
//!
//! `FjallDb` is the long-lived handle and owns the id allocator
//! ([I11](../../docs/invariants.md#i11)); [`FjallDb::reader`] hands the executor
//! the `(handle, snapshot)` pair it consumes.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use byteview::ByteView;
use fjall::{Database, Keyspace, KeyspaceCreateOptions, Readable, Snapshot};

use crate::focus::{
    error::{ApertureError, StoreError},
    plan::{Entity, FactId, FactStore, MAX_TAGGABLE_PREDICATE},
    schema::{PREDICATE_ID_SIZE, PredicateId},
};

/// Width of a stored `FactId`, as a `keys` value and as an `entities` key.
const FACT_ID_LEN: usize = 8;
/// Width of the `key_len` field framing an `entities` row.
const KEY_LEN_LEN: usize = 4;

/// Prefix of the per-predicate index keyspaces (`keys.7` indexes predicate 7).
const KEYS_KEYSPACE_PREFIX: &str = "keys.";
/// Prefix of the per-predicate identity keyspaces (`entities.7`).
const ENTITIES_KEYSPACE_PREFIX: &str = "entities.";

/// One predicate's two trees. Cheap to clone — fjall handles are `Arc`-backed.
#[derive(Clone)]
struct Trees {
    keys: Keyspace,
    entities: Keyspace,
}

/// A predicate's trees plus its own id allocator
/// ([I11](../../docs/invariants.md#i11)).
struct Predicate {
    trees: Trees,
    /// The next sequence to hand out. Recovered at open from what is actually
    /// stored, so a restart cannot reissue an id.
    next_sequence: AtomicU64,
}

/// The long-lived database handle: the fjall environment, the per-predicate
/// keyspace handles, and the fact-id allocators.
pub struct FjallDb {
    db: Database,
    /// `predicate → handles`, materialised at open for what is on disk and
    /// extended on first write to a predicate.
    predicates: RwLock<BTreeMap<u32, Arc<Predicate>>>,
}

impl FjallDb {
    /// Open (creating if absent) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ApertureError> {
        let db = Database::builder(path)
            .open()
            .map_err(StoreError::Backend)?;

        // Recover the per-predicate handles a previous session created. Reads
        // route through this map, so a predicate missing from it reads as "no
        // such predicate" — after a reopen that would silently hide facts.
        let mut ids = BTreeSet::new();
        for name in db.list_keyspace_names() {
            let id = name
                .strip_prefix(KEYS_KEYSPACE_PREFIX)
                .or_else(|| name.strip_prefix(ENTITIES_KEYSPACE_PREFIX));
            if let Some(Ok(id)) = id.map(str::parse::<u32>) {
                ids.insert(id);
            }
        }

        let mut predicates = BTreeMap::new();
        for id in ids {
            let predicate = PredicateId(id);
            predicates.insert(id, Arc::new(Self::open_predicate(&db, predicate)?));
        }

        Ok(Self {
            db,
            predicates: RwLock::new(predicates),
        })
    }

    /// Create the trees for `predicates` now rather than on first write.
    ///
    /// Keyspace creation is ~30 ms apiece, so lazy creation puts that latency
    /// inside an ingest at an unpredictable point. A DB created from a schema
    /// knows every predicate up front and should pay the bill once, here.
    pub fn create_predicates(
        &self,
        predicates: impl IntoIterator<Item = PredicateId>,
    ) -> Result<(), ApertureError> {
        for predicate in predicates {
            self.predicate(predicate)?;
        }
        Ok(())
    }

    /// Open both of a predicate's trees and recover its allocator.
    fn open_predicate(db: &Database, predicate: PredicateId) -> Result<Predicate, ApertureError> {
        let trees = Trees {
            keys: db
                .keyspace(
                    &format!("{KEYS_KEYSPACE_PREFIX}{}", predicate.0),
                    KeyspaceCreateOptions::default,
                )
                .map_err(StoreError::Backend)?,
            entities: db
                .keyspace(
                    &format!("{ENTITIES_KEYSPACE_PREFIX}{}", predicate.0),
                    KeyspaceCreateOptions::default,
                )
                .map_err(StoreError::Backend)?,
        };

        let high_water = Self::recover_high_water(&trees, predicate)?;

        Ok(Predicate {
            trees,
            next_sequence: AtomicU64::new(high_water + 1),
        })
    }

    /// The predicate's highest allocated sequence, or 0 if it holds no facts.
    ///
    /// An `entities` key **is** a fact id, big-endian, so the tree's last key is
    /// the high-water mark ([I11](../../docs/invariants.md#i11)). Deriving it from
    /// the data rather than keeping a counter in a sidecar means the allocator
    /// cannot disagree with what is stored — including after a crash, where a
    /// separately-persisted counter could be stale and hand out a live id twice.
    fn recover_high_water(trees: &Trees, predicate: PredicateId) -> Result<u64, ApertureError> {
        let Some(row) = trees.entities.last_key_value() else {
            return Ok(0);
        };

        // Key only: an entity's value can be large and is not wanted here.
        let key = row.key().map_err(StoreError::Backend)?;
        let fact_id = decode_fact_id(&key)?;

        if fact_id.predicate() != predicate {
            return Err(StoreError::FactIdPredicateMismatch {
                expected: predicate,
                found: fact_id,
            }
            .into());
        }

        Ok(fact_id.sequence())
    }

    /// The handles for `predicate`, creating both trees on first write.
    fn predicate(&self, predicate: PredicateId) -> Result<Arc<Predicate>, ApertureError> {
        if predicate.0 > MAX_TAGGABLE_PREDICATE {
            // Rejected before the trees exist: a predicate whose id cannot be
            // tagged into a `FactId` can never have a fact written to it, so
            // failing at create is better than failing mid-ingest.
            return Err(StoreError::PredicateIdTooWide {
                predicate: predicate.0,
                max: MAX_TAGGABLE_PREDICATE,
            }
            .into());
        }

        if let Some(handle) = self
            .predicates
            .read()
            .expect("predicate map lock is poisoned")
            .get(&predicate.0)
        {
            return Ok(Arc::clone(handle));
        }

        let mut predicates = self
            .predicates
            .write()
            .expect("predicate map lock is poisoned");

        // A racing writer may have created it between the two locks.
        if let Some(handle) = predicates.get(&predicate.0) {
            return Ok(Arc::clone(handle));
        }

        let handle = Arc::new(Self::open_predicate(&self.db, predicate)?);
        predicates.insert(predicate.0, Arc::clone(&handle));
        Ok(handle)
    }

    /// Write one fact, allocating its id, with both column families in a single
    /// batch ([I11](../../docs/invariants.md#i11),
    /// [I12](../../docs/invariants.md#i12)).
    ///
    /// This is the single-fact seeding primitive; Phase 7's bulk path allocates
    /// blocks of sequences and writes through the same layout.
    pub fn put_fact(
        &self,
        predicate: PredicateId,
        key_fields: &[u8],
        value: &[u8],
    ) -> Result<FactId, ApertureError> {
        let handle = self.predicate(predicate)?;

        // The counter is the only source of sequences, so uniqueness needs no
        // coordination between writers. A sequence consumed by a write that then
        // fails is *not* handed out again: I11 requires ids to be unique and never
        // reused, not dense, and reissuing one is exactly how a saved cursor could
        // come to name a different fact.
        let sequence = handle.next_sequence.fetch_add(1, Ordering::Relaxed);
        let fact_id = FactId::new(predicate, sequence)?;

        let mut index_key = Vec::with_capacity(PREDICATE_ID_SIZE + key_fields.len());
        index_key.extend_from_slice(&predicate.0.to_be_bytes());
        index_key.extend_from_slice(key_fields);

        let mut entity = Vec::with_capacity(KEY_LEN_LEN + key_fields.len() + value.len());
        entity.extend_from_slice(&(key_fields.len() as u32).to_be_bytes());
        entity.extend_from_slice(key_fields);
        entity.extend_from_slice(value);

        let mut batch = self.db.batch();
        batch.insert(
            &handle.trees.keys,
            index_key,
            fact_id.0.to_be_bytes().to_vec(),
        );
        batch.insert(
            &handle.trees.entities,
            fact_id.0.to_be_bytes().to_vec(),
            entity,
        );
        batch.commit().map_err(StoreError::Backend)?;

        Ok(fact_id)
    }

    /// A read view for one query: an immutable snapshot plus the keyspace handles
    /// ([I8](../../docs/invariants.md#i8) — the executor consumes this and is
    /// dropped at suspend, so nothing is pinned across an idle portal).
    pub fn reader(&self) -> FjallStore {
        FjallStore {
            snapshot: self.db.snapshot(),
            predicates: self
                .predicates
                .read()
                .expect("predicate map lock is poisoned")
                .iter()
                .map(|(id, handle)| (*id, handle.trees.clone()))
                .collect(),
        }
    }
}

/// The per-query `FactStore`: one snapshot, one set of keyspace handles.
pub struct FjallStore {
    snapshot: Snapshot,
    predicates: BTreeMap<u32, Trees>,
}

/// A scan over one predicate's `keys` tree.
pub enum FjallScan {
    /// Rows from the predicate's tree, in key order.
    Rows(fjall::Iter),
    /// The predicate has no tree in this DB: no facts, not an error.
    Empty,
    /// The bounds were malformed; yields the fault once, then ends.
    Failed(Option<ApertureError>),
}

impl Iterator for FjallScan {
    type Item = Result<(ByteView, FactId), ApertureError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::Failed(fault) => fault.take().map(Err),
            Self::Rows(rows) => Some(row_to_item(rows.next()?)),
        }
    }
}

/// Decode a stored 8-byte big-endian fact id.
fn decode_fact_id(bytes: &[u8]) -> Result<FactId, StoreError> {
    let bytes: [u8; FACT_ID_LEN] = bytes.try_into().map_err(|_| StoreError::FactIdWidth {
        len: bytes.len(),
        expected: FACT_ID_LEN,
    })?;
    Ok(FactId(u64::from_be_bytes(bytes)))
}

/// `keys` row → `(row bytes, fact id)`.
///
/// The key becomes a `ByteView` by refcount move, never a copy — the register
/// holds the whole row ([I5](../../docs/invariants.md#i5)) and the hot loop
/// allocates nothing per row ([I9](../../docs/invariants.md#i9)).
fn row_to_item(row: fjall::Guard) -> Result<(ByteView, FactId), ApertureError> {
    let (key, value) = row.into_inner().map_err(StoreError::Backend)?;
    let fact_id = decode_fact_id(&value)?;
    Ok((ByteView::from(key), fact_id))
}

impl FactStore for FjallStore {
    type Scan = FjallScan;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> FjallScan {
        // The bound's first four bytes name the predicate, which selects the tree.
        // `hi` cannot be used for this: it is typically `strinc(lo)`, whose carry
        // can name the *next* predicate (`strinc([0,0,0,0]) == [0,0,0,1]`).
        let Some(prefix) = lo.get(..PREDICATE_ID_SIZE) else {
            return FjallScan::Failed(Some(
                StoreError::ShortScanBound {
                    len: lo.len(),
                    expected: PREDICATE_ID_SIZE,
                }
                .into(),
            ));
        };
        let predicate = u32::from_be_bytes(prefix.try_into().expect("checked four bytes above"));

        let Some(trees) = self.predicates.get(&predicate) else {
            return FjallScan::Empty;
        };

        FjallScan::Rows(match hi {
            Some(hi) => self.snapshot.range(&trees.keys, lo..hi),
            None => self.snapshot.range(&trees.keys, lo..),
        })
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, ApertureError> {
        // The id's tag names the tree, so identity lookup is one point read even
        // though `entities` is split per predicate.
        let Some(trees) = self.predicates.get(&id.predicate().0) else {
            return Ok(None);
        };

        let Some(row) = self
            .snapshot
            .get(&trees.entities, id.0.to_be_bytes())
            .map_err(StoreError::Backend)?
        else {
            return Ok(None);
        };

        let row = ByteView::from(row);
        let framing: [u8; KEY_LEN_LEN] = row
            .get(..KEY_LEN_LEN)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(StoreError::TruncatedEntity(id))?;
        let key_end = KEY_LEN_LEN + u32::from_be_bytes(framing) as usize;
        if key_end > row.len() {
            return Err(StoreError::TruncatedEntity(id).into());
        }

        // Both halves are refcount views on the fetched row, not copies.
        Ok(Some(Entity {
            key: row.slice(KEY_LEN_LEN..key_end),
            value: row.slice(key_end..),
        }))
    }
}

/// Phase-1 guards still pending: [I8](../../docs/invariants.md#i8), snapshot
/// release at suspend, which needs the executor change in PLAN 1c.
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
    // `Executor::enumerate` takes `&mut self` and hands back `Iteratee::Suspended`
    // while the caller keeps the executor *and* its live scans, so this is an API
    // change (suspend consumes the executor, or clears its frames), not only a
    // test to write.
    //
    // Untestable on `MemStore`, whose scan copies rows out and pins nothing —
    // this is why fjall is pulled forward to Phase 1.
    #[test]
    #[ignore = "I8 — pending Phase 1 (needs the drop probe + suspend ownership change, PLAN 1c)"]
    fn snapshot_released_at_suspend() {
        unimplemented!(
            "Phase 1 (task 1c): assert no fjall snapshot survives a suspend, cancel or unwind"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use proptest::prelude::*;
    use tempfile::TempDir;

    use super::*;
    use crate::focus::{
        fixtures::assert_scan_stays_in_predicate, mem_store::MemStore, plan::MAX_FACT_SEQUENCE,
        tuple::strinc,
    };

    /// One fact as drawn: predicate, key bytes, value bytes.
    type FactDraw = (u32, Vec<u8>, Vec<u8>);

    /// A seeded pair of stores plus the ids that were written, in write order.
    struct Seeded {
        db: FjallDb,
        mem: MemStore,
        ids: Vec<FactId>,
        /// Held for the lifetime of the DB; dropping it removes the directory.
        _dir: TempDir,
    }

    /// Predicates are drawn from a small set so scans collide, and keys from a
    /// small alphabet so partial-key bounds land *inside* a key rather than always
    /// past its end. The store treats both as opaque bytes, so the codec's own
    /// strategies would only narrow the input.
    fn arb_facts() -> impl Strategy<Value = Vec<FactDraw>> {
        prop::collection::vec(
            (
                0..3u32,
                prop::collection::vec(0..4u8, 0..4),
                prop::collection::vec(any::<u8>(), 0..3),
            ),
            0..12,
        )
    }

    /// A scan bound: a predicate (possibly one with no facts) and a partial key.
    fn arb_bound() -> impl Strategy<Value = (u32, Vec<u8>)> {
        (0..4u32, prop::collection::vec(0..4u8, 0..3))
    }

    /// Seed the same facts into both stores over the deduplicated, sorted draw —
    /// mirroring `PlanAndStore::build_store`, so a rebuild is identical and the two
    /// stores are comparable row for row.
    ///
    /// Ids come from the real allocator and are mirrored into the model by
    /// sequence, and the two are asserted equal: the seeding path therefore also
    /// pins that `put_fact` numbers facts per predicate, in call order.
    fn seed(facts: &[FactDraw]) -> Seeded {
        let dir = TempDir::new().expect("tempdir");
        let db = FjallDb::open(dir.path()).expect("open");
        let mut mem = MemStore::new();

        let mut sorted: Vec<_> = facts.to_vec();
        sorted.sort();
        sorted.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        let mut next = BTreeMap::<u32, u64>::new();
        let mut ids = Vec::new();

        for (predicate, key, value) in &sorted {
            let sequence = {
                let next = next.entry(*predicate).or_insert(1);
                let sequence = *next;
                *next += 1;
                sequence
            };

            let predicate = PredicateId(*predicate);
            let id = db.put_fact(predicate, key, value).expect("put");
            assert_eq!(
                id,
                FactId::new(predicate, sequence).expect("model fact id"),
                "the allocator diverged from the model's per-predicate sequence"
            );

            mem.insert_valued(predicate, key.clone(), sequence, value.clone());
            ids.push(id);
        }

        Seeded {
            db,
            mem,
            ids,
            _dir: dir,
        }
    }

    fn scan_rows<S: FactStore>(store: &S, lo: &[u8], hi: Option<&[u8]>) -> Vec<(Vec<u8>, u64)> {
        store
            .scan(lo, hi)
            .map(|row| {
                let (key, id) = row.expect("scan row");
                (key.to_vec(), id.0)
            })
            .collect()
    }

    fn bound_bytes(predicate: u32, partial_key: &[u8]) -> Vec<u8> {
        let mut bytes = predicate.to_be_bytes().to_vec();
        bytes.extend_from_slice(partial_key);
        bytes
    }

    /// The predicates a DB holds trees for. Taken as a snapshot so no helper walks
    /// the store while holding the map's lock.
    fn predicates_of(db: &FjallDb) -> Vec<(u32, Trees)> {
        db.predicates
            .read()
            .expect("predicate map lock is poisoned")
            .iter()
            .map(|(id, handle)| (*id, handle.trees.clone()))
            .collect()
    }

    /// Every fact in the DB, as `(fact id, entity key bytes)`, read out of the
    /// `keys` trees — one entry per index row, so a duplicate id shows up as a
    /// repeated entry rather than being silently merged.
    fn keys_rows(db: &FjallDb) -> Vec<(FactId, Vec<u8>)> {
        let reader = db.reader();
        let mut rows = Vec::new();

        for (predicate, _) in predicates_of(db) {
            let lo = bound_bytes(predicate, &[]);
            let hi = strinc(&lo);
            for row in reader.scan(&lo, hi.as_deref()) {
                let (key, id) = row.expect("keys row");
                rows.push((id, key[PREDICATE_ID_SIZE..].to_vec()));
            }
        }

        rows
    }

    /// Every fact id present in the `entities` trees, with the tree it was found
    /// in — a fact filed under a predicate its tag does not name is unreachable by
    /// `point`, which routes on the tag alone.
    fn entity_ids(db: &FjallDb) -> Vec<FactId> {
        let mut ids = Vec::new();

        for (predicate, trees) in predicates_of(db) {
            for row in trees.entities.iter() {
                let key = row.key().expect("entities key");
                let id = decode_fact_id(&key).expect("entities key is a fact id");
                assert_eq!(
                    id.predicate().0,
                    predicate,
                    "{id:?} is stored in predicate {predicate}'s tree but tagged for another"
                );
                ids.push(id);
            }
        }

        ids
    }

    /// [I12](../../docs/invariants.md#i12) in its observable form: the two column
    /// families are in exact bijection, and every `keys` row's key bytes match the
    /// ones stored in its entity. Returns the number of facts checked.
    ///
    /// Both directions matter and fail differently: a `keys` row with no entity
    /// surfaces as `DanglingFactId` the moment a query projects the value, while an
    /// entity with no `keys` row is invisible to every query — silent, and
    /// undetectable without exactly this check.
    fn assert_bijection(db: &FjallDb) -> usize {
        let keys = keys_rows(db);
        let mut entities = entity_ids(db);
        let reader = db.reader();

        let mut ids: Vec<FactId> = keys.iter().map(|(id, _)| *id).collect();
        ids.sort();
        let unique: BTreeSet<FactId> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "a fact id indexes two keys");

        entities.sort();
        assert_eq!(
            ids, entities,
            "`keys` and `entities` disagree about which facts exist"
        );

        for (id, key) in &keys {
            let entity = reader
                .point(*id)
                .expect("point")
                .unwrap_or_else(|| panic!("{id:?} has a keys row but no entity"));
            assert_eq!(
                entity.key.to_vec(),
                *key,
                "{id:?}: the entity's key bytes differ from the indexed key"
            );
        }

        keys.len()
    }

    proptest! {
        // Each case opens a real fjall database (worker threads, on-disk trees),
        // so cases are orders of magnitude more expensive than the in-memory
        // batteries — enough to be a differential oracle, not 1024 of them.
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// The fjall store and `MemStore` are the same map. Every green executor
        /// battery is written against `MemStore`, so byte-identical scan output is
        /// what carries those batteries over to fjall (PLAN 1d).
        #[test]
        fn fjall_scan_matches_memstore(facts in arb_facts(), bound in arb_bound()) {
            let seeded = seed(&facts);
            let reader = seeded.db.reader();

            let (predicate, partial_key) = bound;
            let lo = bound_bytes(predicate, &partial_key);
            let hi = strinc(&lo);

            prop_assert_eq!(
                scan_rows(&reader, &lo, hi.as_deref()),
                scan_rows(&seeded.mem, &lo, hi.as_deref()),
                "bounded scan diverges from the MemStore model"
            );
            prop_assert_eq!(
                scan_rows(&reader, &lo, None),
                scan_rows(&seeded.mem, &lo, None),
                "unbounded scan diverges from the MemStore model"
            );

            // The scan contract itself, asserted on each store *directly* rather
            // than inferred from the two agreeing: impls that leaked into the next
            // predicate identically would satisfy the differential and both still
            // be wrong.
            for hi in [hi.as_deref(), None] {
                assert_scan_stays_in_predicate(&reader, &lo, hi).expect("fjall scan");
                assert_scan_stays_in_predicate(&seeded.mem, &lo, hi).expect("mem scan");
            }
        }

        /// `point` agrees with the model on present ids (both halves, byte for
        /// byte) and on absent ones.
        #[test]
        fn fjall_point_matches_memstore(facts in arb_facts()) {
            let seeded = seed(&facts);
            let reader = seeded.db.reader();

            // Every id written, plus one past each predicate's last sequence and
            // one in a predicate with no facts at all — so the absent case is
            // covered even when the draw is empty.
            let absent = (0..4u32).map(|predicate| {
                let used = seeded
                    .ids
                    .iter()
                    .filter(|id| id.predicate().0 == predicate)
                    .count() as u64;
                FactId::new(PredicateId(predicate), used + 1).expect("absent id")
            });

            for id in seeded.ids.iter().copied().chain(absent) {
                let got = reader.point(id).expect("point");
                let want = seeded.mem.point(id).expect("point");

                match (got, want) {
                    (None, None) => {}
                    (Some(got), Some(want)) => {
                        prop_assert_eq!(got.key.to_vec(), want.key.to_vec(), "entity key differs");
                        prop_assert_eq!(got.value.to_vec(), want.value.to_vec(), "entity value differs");
                    }
                    (got, want) => prop_assert!(
                        false,
                        "presence differs for {:?}: {:?} vs model {:?}",
                        id,
                        got.is_some(),
                        want.is_some()
                    ),
                }
            }
        }

        /// [I12](../../docs/invariants.md#i12) over generated writes: the two
        /// column families are in bijection after every seeding run.
        #[test]
        fn no_half_present_facts_after_writes(facts in arb_facts()) {
            let seeded = seed(&facts);
            prop_assert_eq!(assert_bijection(&seeded.db), seeded.ids.len());
        }
    }

    /// Predicate isolation, at the byte boundary that makes it fragile: the upper
    /// bound of predicate 0's prefix scan is `strinc([0,0,0,0]) == [0,0,0,1]`,
    /// which *is* predicate 1's prefix. A single shared tree would need the bound
    /// to be exact; routing by the low bound's predicate makes it structural.
    ///
    /// Checked on **both** stores: this is a `FactStore` contract, and `MemStore`
    /// is the oracle every executor battery runs against, so a leak there is as
    /// damaging as a leak in the real store. The `hi = None` case is the one that
    /// was actually broken — `MemStore` ranged to the end of its single map.
    #[test]
    fn scan_does_not_leak_across_predicates() {
        let facts = vec![
            (0, vec![7u8], vec![]),
            (1, vec![], vec![]),
            (1, vec![7u8], vec![]),
        ];
        let seeded = seed(&facts);
        let reader = seeded.db.reader();

        let lo = bound_bytes(0, &[]);
        let hi = strinc(&lo).expect("prefix has a successor");
        assert_eq!(hi, bound_bytes(1, &[]), "the carry must reach predicate 1");

        let want = vec![(
            bound_bytes(0, &[7]),
            FactId::new(PredicateId(0), 1).expect("id").0,
        )];
        for hi in [Some(hi.as_slice()), None] {
            assert_eq!(
                scan_rows(&reader, &lo, hi),
                want,
                "predicate 0's fjall scan (hi {hi:?}) saw another predicate's facts"
            );
            assert_eq!(
                scan_rows(&seeded.mem, &lo, hi),
                want,
                "predicate 0's MemStore scan (hi {hi:?}) saw another predicate's facts"
            );
        }
    }

    /// A scan with no upper bound must still stop at the end of its predicate.
    ///
    /// The trait permits `hi = None` and `MemStore`'s bug lived exactly there. The
    /// executor derives `hi` from `strinc`, which is `None` only for an all-`0xFF`
    /// prefix — unreachable now that the fact-id tag caps a predicate id at
    /// `0x00FF_FFFF`, whose first byte is `0x00`. So this is the store holding up
    /// its end of the contract rather than a case the executor can produce, and it
    /// stays guarded because the trait is what other implementations are written
    /// against.
    #[test]
    fn unbounded_scan_stops_at_the_predicate_boundary() {
        let last = MAX_TAGGABLE_PREDICATE;
        let facts = vec![
            (last - 1, vec![1u8], vec![]),
            (last, vec![1u8], vec![]),
            (last, vec![2u8], vec![]),
        ];
        let seeded = seed(&facts);
        let reader = seeded.db.reader();

        let lo = bound_bytes(last, &[]);
        let want = vec![
            (
                bound_bytes(last, &[1]),
                FactId::new(PredicateId(last), 1).expect("id").0,
            ),
            (
                bound_bytes(last, &[2]),
                FactId::new(PredicateId(last), 2).expect("id").0,
            ),
        ];
        assert_eq!(scan_rows(&reader, &lo, None), want);
        assert_eq!(scan_rows(&seeded.mem, &lo, None), want);

        let neighbour = bound_bytes(last - 1, &[]);
        assert_scan_stays_in_predicate(&reader, &neighbour, None).expect("fjall scan");
        assert_scan_stays_in_predicate(&seeded.mem, &neighbour, None).expect("mem scan");
    }

    /// A predicate with no tree reads as empty rather than failing — and a bound
    /// too short to name a predicate is a surfaced error, not a panic.
    #[test]
    fn absent_predicate_is_empty_and_short_bound_is_an_error() {
        let seeded = seed(&[(0, vec![1u8], vec![])]);
        let reader = seeded.db.reader();

        let lo = bound_bytes(9, &[]);
        assert!(scan_rows(&reader, &lo, None).is_empty());

        let mut short = reader.scan(&[0, 0], None);
        assert!(matches!(
            short.next(),
            Some(Err(ApertureError::Store(StoreError::ShortScanBound { .. })))
        ));
        assert!(short.next().is_none(), "the fault is yielded once");
    }

    /// A predicate id too wide for the fact-id tag is rejected before any tree is
    /// created, rather than at the first write.
    #[test]
    fn untaggable_predicate_is_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let db = FjallDb::open(dir.path()).expect("open");
        let too_wide = PredicateId(MAX_TAGGABLE_PREDICATE + 1);

        assert!(matches!(
            db.put_fact(too_wide, &[1], &[]),
            Err(ApertureError::Store(StoreError::PredicateIdTooWide { .. }))
        ));
        assert!(matches!(
            db.create_predicates([too_wide]),
            Err(ApertureError::Store(StoreError::PredicateIdTooWide { .. }))
        ));
        assert_eq!(
            db.predicates
                .read()
                .expect("predicate map lock is poisoned")
                .len(),
            0,
            "a rejected predicate must not leave trees behind"
        );
    }

    /// Facts survive a close and reopen, and the reopened handle recovers the
    /// per-predicate keyspaces — without that recovery a reader built after a
    /// reopen would route every scan to "no such predicate" and read empty.
    #[test]
    fn reopen_recovers_predicates() {
        let dir = TempDir::new().expect("tempdir");
        let key = vec![3u8, 4];
        let predicate = PredicateId(5);

        let written = {
            let db = FjallDb::open(dir.path()).expect("open");
            db.put_fact(predicate, &key, &[9]).expect("put")
        };

        let db = FjallDb::open(dir.path()).expect("reopen");
        let reader = db.reader();
        let lo = bound_bytes(predicate.0, &[]);

        assert_eq!(
            scan_rows(&reader, &lo, strinc(&lo).as_deref()),
            vec![(bound_bytes(predicate.0, &key), written.0)],
            "reopened DB lost predicate 5's rows"
        );
        let entity = reader.point(written).expect("point").expect("present");
        assert_eq!(entity.key.to_vec(), key);
        assert_eq!(entity.value.to_vec(), vec![9]);
    }

    /// [I11](../../docs/invariants.md#i11) — a `FactId` is stable, unique, and
    /// never reused within a DB.
    ///
    /// Uniqueness across predicates is structural (the tag partitions the space),
    /// so what needs guarding is the sequence: monotonic within a predicate,
    /// resumed *above* the high-water mark after a reopen, and collision-free
    /// under concurrent writers — uniqueness has to come from the counter, not from
    /// callers serialising themselves.
    #[test]
    fn factid_unique_monotonic() {
        let dir = TempDir::new().expect("tempdir");
        let predicates = [
            PredicateId(0),
            PredicateId(7),
            PredicateId(MAX_TAGGABLE_PREDICATE),
        ];
        let mut seen = BTreeSet::new();

        {
            let db = FjallDb::open(dir.path()).expect("open");
            for predicate in predicates {
                for k in 0..8u8 {
                    let id = db.put_fact(predicate, &[k], &[]).expect("put");
                    assert_eq!(id.predicate(), predicate, "id is tagged for its predicate");
                    assert_eq!(id.sequence(), u64::from(k) + 1, "sequence is monotonic");
                    assert!(seen.insert(id), "{id:?} was issued twice");
                }
            }
        }

        // A restart must never hand out an id twice, and the high-water mark is
        // recovered from the data rather than from a counter that could be stale.
        let db = FjallDb::open(dir.path()).expect("reopen");
        for predicate in predicates {
            let id = db.put_fact(predicate, &[99], &[]).expect("put");
            assert_eq!(id.sequence(), 9, "the counter resumed below the high water");
            assert!(seen.insert(id), "{id:?} was reissued after a reopen");
        }

        // Concurrent writers to one predicate: 4 threads × 25 facts must be
        // exactly the sequences 1..=100, each issued once.
        let predicate = PredicateId(3);
        let ids: Vec<FactId> = thread::scope(|scope| {
            let handles: Vec<_> = (0..4u8)
                .map(|thread| {
                    let db = &db;
                    scope.spawn(move || {
                        (0..25u8)
                            .map(|k| db.put_fact(predicate, &[thread, k], &[]).expect("put"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("writer thread"))
                .collect()
        });

        let sequences: BTreeSet<u64> = ids.iter().map(|id| id.sequence()).collect();
        assert_eq!(ids.len(), 100);
        assert_eq!(
            sequences,
            (1..=100).collect::<BTreeSet<_>>(),
            "concurrent writers collided or skipped sequences"
        );

        // The bijection must survive the concurrent run too, and the reopened
        // allocator must resume above what those threads wrote.
        assert_bijection(&db);
        drop(db);
        let db = FjallDb::open(dir.path()).expect("reopen");
        assert_eq!(
            db.put_fact(predicate, &[0xff], &[])
                .expect("put")
                .sequence(),
            101
        );
    }

    /// The sequence space is finite and must fail closed: a predicate that runs out
    /// errors rather than wrapping into another predicate's tag.
    #[test]
    fn exhausted_sequence_space_is_an_error() {
        assert!(matches!(
            FactId::new(PredicateId(1), MAX_FACT_SEQUENCE + 1),
            Err(StoreError::FactIdSequence { .. })
        ));
        assert!(
            matches!(
                FactId::new(PredicateId(1), 0),
                Err(StoreError::FactIdSequence { .. })
            ),
            "sequence 0 is reserved so that FactId(0) is never a fact"
        );
    }

    /// Name of the child test that crashes mid-write for
    /// [`no_half_present_facts`], and the variable carrying it the DB path.
    const CRASH_CHILD: &str = "focus::store::tests::crashing_writer_child_process";
    const CRASH_DIR_VAR: &str = "APERTURE_I12_CRASH_DIR";

    /// [I12](../../docs/invariants.md#i12) — a fact is never half-present, **including
    /// across a crash**.
    ///
    /// `no_half_present_facts_after_writes` covers the bijection under ordinary
    /// writes; the failure this guards is the one that only a torn write produces.
    /// fjall's write batch is one journal entry, so the honest test is to kill a
    /// process mid-stream and check what recovery yields: a batch that was being
    /// written when the process died must come back whole or not at all, never as a
    /// key without its entity.
    ///
    /// The cut point is deliberately not controlled — the child is aborted by a
    /// watchdog thread while it writes, so successive runs cut in different places.
    /// The property holds wherever it lands.
    #[test]
    fn no_half_present_facts() {
        let dir = TempDir::new().expect("tempdir");

        let status =
            std::process::Command::new(std::env::current_exe().expect("path to this test binary"))
                .args(["--exact", CRASH_CHILD, "--ignored", "--nocapture"])
                .env(CRASH_DIR_VAR, dir.path())
                .status()
                .expect("spawn the crashing writer");
        assert!(
            !status.success(),
            "the child was supposed to abort mid-write, not exit cleanly"
        );

        // Recovery replays the journal; anything torn must be dropped whole.
        let db = FjallDb::open(dir.path()).expect("reopen after a crash");
        let recovered = assert_bijection(&db);
        assert!(
            recovered > 0,
            "the child crashed before writing anything — the case is vacuous"
        );

        // The allocator recovers above everything that survived, so a post-crash
        // write cannot collide with a recovered fact ([I11]).
        let ids: BTreeSet<FactId> = keys_rows(&db).into_iter().map(|(id, _)| id).collect();
        for predicate in db
            .predicates
            .read()
            .expect("predicate map lock is poisoned")
            .keys()
            .copied()
            .collect::<Vec<_>>()
        {
            let id = db
                .put_fact(PredicateId(predicate), &[0xff, 0xff], &[])
                .expect("put after recovery");
            assert!(!ids.contains(&id), "{id:?} was reissued after a crash");
        }
        assert_bijection(&db);
    }

    /// Not a guard: the crashing half of [`no_half_present_facts`], run as a child
    /// process. Writes facts in a loop while a watchdog aborts the process, so the
    /// kill lands at an arbitrary point — including inside a batch commit.
    #[test]
    #[ignore = "not a guard — child process of store::tests::no_half_present_facts"]
    fn crashing_writer_child_process() {
        let Ok(dir) = std::env::var(CRASH_DIR_VAR) else {
            panic!("{CRASH_DIR_VAR} is unset: this test is only run as a child process");
        };

        let db = FjallDb::open(dir).expect("open");

        thread::spawn(|| {
            thread::sleep(std::time::Duration::from_millis(150));
            std::process::abort();
        });

        for k in 0..u32::MAX {
            let predicate = PredicateId(k % 4);
            db.put_fact(predicate, &k.to_be_bytes(), &[7; 48])
                .expect("put");
        }
    }
}
