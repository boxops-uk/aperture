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
    error::{ApertureError, FormatError, StoreError},
    fact::{self, Fact},
    format::{FORMAT_KEY, FormatVersion, META_KEYSPACE},
    plan::{Entity, FactId, FactStore, MAX_FACT_SEQUENCE, MAX_TAGGABLE_PREDICATE},
    schema::{PREDICATE_ID_SIZE, PredicateId, Schema},
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
    ///
    /// Behind an `Arc` as well as the lock, so [`FjallDb::reader`] shares the map
    /// instead of copying it: opening a query used to clone every predicate's
    /// handles, on the one path that happens per query. Writes are
    /// copy-on-write ([`Arc::make_mut`]), which costs a copy only when a predicate
    /// is created — already the expensive operation, at ~30 ms a keyspace pair.
    /// Readers keep whichever map they were handed, which is the snapshot
    /// semantics a query wants anyway.
    predicates: RwLock<Arc<BTreeMap<u32, Arc<Predicate>>>>,
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

        // Before a single row is read: does this build understand what wrote them?
        // A fresh directory is stamped here, which is what makes this also the
        // *create* path.
        Self::stamp_or_check_format(&db, !ids.is_empty())?;

        let mut predicates = BTreeMap::new();
        for id in ids {
            let predicate = PredicateId(id);
            predicates.insert(id, Arc::new(Self::open_predicate(&db, predicate)?));
        }

        Ok(Self {
            db,
            predicates: RwLock::new(Arc::new(predicates)),
        })
    }

    /// Check the [format stamp](crate::focus::format), or write it if this
    /// database is new ([I15](../../docs/invariants.md#i15)).
    ///
    /// `holds_facts` is what separates the two cases, and it is asked of the
    /// keyspace listing rather than of the stamp: an *unstamped* database with
    /// predicate trees in it was written by something else — an older build, or not
    /// Aperture at all — and stamping it would be this build certifying bytes it has
    /// never read. An unstamped *empty* directory is a create, and gets the stamp.
    ///
    /// Runs before any predicate tree is opened, because a version this build
    /// cannot read is a reason not to touch the rows at all.
    fn stamp_or_check_format(db: &Database, holds_facts: bool) -> Result<(), ApertureError> {
        let meta = db
            .keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)
            .map_err(StoreError::Backend)?;

        if let Some(stamp) = meta.get(FORMAT_KEY).map_err(StoreError::Backend)? {
            FormatVersion::decode(&stamp)?.check_readable()?;
            return Ok(());
        }

        if holds_facts {
            return Err(FormatError::Unstamped.into());
        }

        let mut batch = db.batch();
        batch.insert(
            &meta,
            FORMAT_KEY,
            FormatVersion::CURRENT.encode().as_slice(),
        );
        batch.commit().map_err(StoreError::Backend)?;

        Ok(())
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

        // The read guard is bound and dropped explicitly rather than left as a
        // temporary in an `if let` scrutinee: a temporary there lives to the end
        // of the `if let`, so taking the write lock below would be sound only
        // because Rust 2024 shortened that scope. Stated this way it does not
        // depend on the edition.
        {
            let predicates = self
                .predicates
                .read()
                .expect("predicate map lock is poisoned");

            if let Some(handle) = predicates.get(&predicate.0) {
                return Ok(Arc::clone(handle));
            }
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
        Arc::make_mut(&mut predicates).insert(predicate.0, Arc::clone(&handle));
        Ok(handle)
    }

    /// Write a **well-typed value** as a fact, checked against the schema.
    ///
    /// The way to write a fact by hand: name the predicate and its fields, and let
    /// [`fact`](crate::focus::fact) resolve them — a field the predicate does not
    /// declare, one left out, one of the wrong shape, or a value side that should not
    /// be there is an error rather than bytes nobody can read back. See that module for
    /// why naming the fields is the point.
    ///
    /// The returned id is what a *reference* to this fact is, so the next fact that
    /// points at it takes this value.
    ///
    /// # Errors
    ///
    /// [`ApertureError::Fact`] if the value does not fit the schema, and whatever
    /// [`put_fact`](Self::put_fact) reports otherwise.
    pub fn put<F: Fact>(&self, schema: &Schema, fact: &F) -> Result<FactId, ApertureError> {
        let (predicate, key, value) = fact::encode(schema, fact)?;
        self.put_fact(predicate, &key, &value)
    }

    /// Write one fact from **encoded bytes**, allocating its id, with both column
    /// families in a single batch ([I11](../../docs/invariants.md#i11),
    /// [I12](../../docs/invariants.md#i12)).
    ///
    /// The primitive under [`put`](Self::put), and the one Phase 7's bulk path will
    /// build on — it allocates blocks of sequences and writes through the same layout.
    /// A caller holding a fact rather than bytes wants `put`, which cannot get a
    /// record's field order wrong.
    ///
    /// # A key is written once
    ///
    /// A `keys` row maps a key to exactly *one* fact, so writing the same
    /// `(predicate, key_fields)` twice overwrites the index row and strands the
    /// first fact's `entities` row — a fact no query can reach, and one that no
    /// bijection check can attribute to anything. **Not writing a key twice is
    /// the caller's contract**, which an immutable fact database has no reason to
    /// break.
    ///
    /// It is not enforced on the write path: the check is a point lookup per
    /// fact, and this is the primitive Phase 7's bulk ingest is built on. So it
    /// is asserted in debug builds — where the whole suite, including the
    /// generated store batteries, exercises it — and costs nothing in release.
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

        // The write-once contract, checked where it is free to check (see above).
        #[cfg(debug_assertions)]
        {
            let already_written = handle
                .trees
                .keys
                .contains_key(&index_key)
                .map_err(StoreError::Backend)?;

            assert!(
                !already_written,
                "predicate {} already holds a fact keyed {:02x?}. Writing it again \
                 would overwrite the `keys` row and strand the first fact's entity; \
                 a key is written once.",
                predicate.0, key_fields,
            );
        }

        let mut entity = Vec::with_capacity(KEY_LEN_LEN + key_fields.len() + value.len());
        entity.extend_from_slice(&(key_fields.len() as u32).to_be_bytes());
        entity.extend_from_slice(key_fields);
        entity.extend_from_slice(value);

        let mut batch = self.db.batch();
        batch.insert(
            &handle.trees.keys,
            index_key,
            fact_id.raw().to_be_bytes().to_vec(),
        );
        batch.insert(
            &handle.trees.entities,
            fact_id.raw().to_be_bytes().to_vec(),
            entity,
        );
        batch.commit().map_err(StoreError::Backend)?;

        Ok(fact_id)
    }

    /// How many read snapshots fjall currently considers open.
    ///
    /// fjall's snapshot tracker is the only thing that knows, and this is what the
    /// [I8](../../docs/invariants.md#i8) guard asserts against: a scan or store
    /// handle that outlives its query shows up here as a snapshot that is still
    /// pinning LSM blocks and a superseded generation. Exposed because "the
    /// executor released it" is only believable if the storage engine agrees.
    ///
    /// # This is the one place that reaches into fjall
    ///
    /// There is no supported API for it. `Database::snapshot` is the only public
    /// snapshot method; the count lives on `SnapshotTracker`, reached through
    /// `DatabaseInner::supervisor`, which is `#[doc(hidden)] pub` — reachable, with
    /// no stability promise — and fjall itself calls `open_snapshots` only from its
    /// own unit tests.
    ///
    /// So it is confined to test builds. An ordinary build of this crate, and every
    /// consumer of it, depends on fjall's public surface alone; only the guard
    /// depends on more, and an upgrade that moves the field breaks the *test* build,
    /// loudly, in the one place that knows why.
    ///
    /// If it disappears, the fix is a documented accessor upstream rather than a
    /// different witness. I8 deliberately has two: `DropProbe` says *which object*
    /// survived, and this says whether the engine agrees. Nothing else can answer
    /// the second question without inferring it from disk usage or compaction
    /// behaviour, which would be a guess dressed as a guard.
    #[cfg(any(test, feature = "proptest"))]
    #[must_use]
    pub fn open_snapshots(&self) -> usize {
        self.db.supervisor.snapshot_tracker.open_snapshots()
    }

    /// A read view for one query: an immutable snapshot plus the keyspace handles
    /// ([I8](../../docs/invariants.md#i8) — `Executor::enumerate` consumes this and
    /// drops it on every exit path, so nothing is pinned across an idle portal).
    pub fn reader(&self) -> FjallStore {
        let predicates = self
            .predicates
            .read()
            .expect("predicate map lock is poisoned");

        FjallStore {
            snapshot: self.db.snapshot(),
            predicates: Arc::clone(&predicates),
        }
    }
}

/// The per-query `FactStore`: one snapshot, one set of keyspace handles.
pub struct FjallStore {
    snapshot: Snapshot,
    predicates: Arc<BTreeMap<u32, Arc<Predicate>>>,
}

/// A scan over one predicate's `keys` tree.
pub enum FjallScan {
    /// Rows from the predicate's tree, in key order.
    Rows(fjall::Iter),
    /// The predicate has no tree in this DB: no facts, not an error.
    Empty,
}

impl Iterator for FjallScan {
    type Item = Result<(ByteView, FactId), ApertureError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::Rows(rows) => Some(row_to_item(rows.next()?)),
        }
    }
}

/// The predicate a scan bound names — its first four bytes.
///
/// Shared by every [`FactStore`], so the contract for a malformed bound is one
/// behaviour rather than one per implementation.
pub(crate) fn predicate_of(lo: &[u8]) -> Result<u32, StoreError> {
    let prefix = lo
        .get(..PREDICATE_ID_SIZE)
        .ok_or(StoreError::ShortScanBound {
            len: lo.len(),
            expected: PREDICATE_ID_SIZE,
        })?;

    Ok(u32::from_be_bytes(
        prefix.try_into().expect("checked four bytes above"),
    ))
}

/// Decode a stored 8-byte big-endian fact id.
///
/// This is the one place stored bytes become a [`FactId`], which is where the
/// reserved sequence has to be enforced: sequence 0 exists precisely so that
/// zeroed or truncated bytes are *detectably* not a fact
/// ([I11](../../docs/invariants.md#i11)), and a property nothing checks is only
/// an intention. Unchecked, a corrupt row's `FactId(0)` travels on and surfaces
/// as a dangling reference at projection — several layers from the row that is
/// actually wrong.
fn decode_fact_id(bytes: &[u8]) -> Result<FactId, StoreError> {
    let bytes: [u8; FACT_ID_LEN] = bytes.try_into().map_err(|_| StoreError::FactIdWidth {
        len: bytes.len(),
        expected: FACT_ID_LEN,
    })?;

    let id = FactId::from_raw(u64::from_be_bytes(bytes));

    if id.sequence() == 0 {
        return Err(StoreError::FactIdSequence {
            sequence: 0,
            max: MAX_FACT_SEQUENCE,
        });
    }

    Ok(id)
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

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Result<FjallScan, ApertureError> {
        // The bound's first four bytes name the predicate, which selects the tree.
        // `hi` cannot be used for this: it is typically `strinc(lo)`, whose carry
        // can name the *next* predicate (`strinc([0,0,0,0]) == [0,0,0,1]`).
        let prefix = predicate_of(lo)?;

        let Some(handle) = self.predicates.get(&prefix) else {
            // No tree for this predicate: no facts, which is not a fault.
            return Ok(FjallScan::Empty);
        };

        Ok(FjallScan::Rows(match hi {
            Some(hi) => self.snapshot.range(&handle.trees.keys, lo..hi),
            None => self.snapshot.range(&handle.trees.keys, lo..),
        }))
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, ApertureError> {
        // The id's tag names the tree, so identity lookup is one point read even
        // though `entities` is split per predicate.
        let Some(handle) = self.predicates.get(&id.predicate().0) else {
            return Ok(None);
        };

        let Some(row) = self
            .snapshot
            .get(&handle.trees.entities, id.raw().to_be_bytes())
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

#[cfg(test)]
mod tests {
    use std::thread;

    use proptest::prelude::*;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::focus::{
        fixtures::{
            DropProbe, FrozenStore, assert_scan_stays_in_predicate, assert_short_bound_is_rejected,
            collect_rows, i64_field, interner_with,
        },
        iter::{Address, CANCELLATION_STRIDE, Executor, Iteratee, Stream},
        mem_store::MemStore,
        plan::{
            Access, FieldPath, Level, MAX_FACT_SEQUENCE, Plan, Project, Residual, ResidualOp,
            SeekKey, SeekKeyPart, Step,
        },
        schema::PredicateTy,
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
            .expect("open scan")
            .map(|row| {
                let (key, id) = row.expect("scan row");
                (key.to_vec(), id.raw())
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
            for row in reader.scan(&lo, hi.as_deref()).expect("open scan") {
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
            FactId::new(PredicateId(0), 1).expect("id").raw(),
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
                FactId::new(PredicateId(last), 1).expect("id").raw(),
            ),
            (
                bound_bytes(last, &[2]),
                FactId::new(PredicateId(last), 2).expect("id").raw(),
            ),
        ];
        assert_eq!(scan_rows(&reader, &lo, None), want);
        assert_eq!(scan_rows(&seeded.mem, &lo, None), want);

        let neighbour = bound_bytes(last - 1, &[]);
        assert_scan_stays_in_predicate(&reader, &neighbour, None).expect("fjall scan");
        assert_scan_stays_in_predicate(&seeded.mem, &neighbour, None).expect("mem scan");
    }

    /// **Every** `FactStore` rejects a bound too short to name a predicate, the
    /// same way and at the same moment.
    ///
    /// This is what making `scan` fallible bought. While it returned the iterator
    /// directly there was nowhere to report a malformed bound, so the case went
    /// unspecified and the implementations diverged: fjall yielded the fault as a
    /// first row, while `MemStore` and `FrozenStore` read "no predicate to bound
    /// to" as "no bound" and scanned straight on — returning rows from *two*
    /// predicates, which is the leak `assert_scan_stays_in_predicate` exists to
    /// forbid. Nothing caught it, because no valid bound is ever short.
    #[test]
    fn every_store_rejects_a_bound_too_short_to_name_a_predicate() {
        // Two predicates, so a store that fails to bound has somewhere to leak to.
        let short: &[u8] = &[0, 0];
        let seeded = seed(&[(0, vec![1u8], vec![]), (1, vec![1u8], vec![])]);

        assert_short_bound_is_rejected(&seeded.db.reader(), short);
        assert_short_bound_is_rejected(&seeded.mem, short);
        assert_short_bound_is_rejected(
            &FrozenStore::from_facts([
                (PredicateId(0), i64_field(1), 1),
                (PredicateId(1), i64_field(1), 1),
            ]),
            short,
        );
    }

    /// A predicate with no tree reads as empty rather than failing — and a bound
    /// too short to name a predicate is a surfaced error, not a panic.
    #[test]
    fn absent_predicate_is_empty_and_short_bound_is_an_error() {
        let seeded = seed(&[(0, vec![1u8], vec![])]);
        let reader = seeded.db.reader();

        let lo = bound_bytes(9, &[]);
        assert!(scan_rows(&reader, &lo, None).is_empty());

        // Reported by `scan` itself: opening is what failed, not a row.
        assert!(matches!(
            reader.scan(&[0, 0], None).err(),
            Some(ApertureError::Store(StoreError::ShortScanBound { .. }))
        ));
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
            vec![(bound_bytes(predicate.0, &key), written.raw())],
            "reopened DB lost predicate 5's rows"
        );
        let entity = reader.point(written).expect("point").expect("present");
        assert_eq!(entity.key.to_vec(), key);
        assert_eq!(entity.value.to_vec(), vec![9]);
    }

    /// [I15](../../docs/invariants.md#i15) — a database says which encoding wrote
    /// it, and a build that does not understand the answer refuses it.
    ///
    /// The three cases are the whole rule: a new directory is **stamped**, a
    /// stamped one is **checked**, and one holding facts without a stamp is
    /// **refused** rather than adopted. The last is the case the invariant exists
    /// for — every database written before stamping existed is that shape, and
    /// silently adopting one would be this build certifying bytes it has never
    /// read.
    #[test]
    fn a_database_says_which_format_wrote_it() {
        let dir = TempDir::new().expect("tempdir");
        let predicate = PredicateId(1);

        // Create: a fresh directory is stamped, and the stamp survives a reopen
        // rather than being rewritten each time.
        {
            let db = FjallDb::open(dir.path()).expect("create");
            assert_eq!(read_stamp(&db), Some(FormatVersion::CURRENT));
            db.put_fact(predicate, &[1], &[]).expect("put");
        }

        let db = FjallDb::open(dir.path()).expect("reopen");
        assert_eq!(read_stamp(&db), Some(FormatVersion::CURRENT));
        drop(db);

        // Check: a version this build does not implement is refused before a row
        // is read. Bumping only the codec half is the sharper case — the storage
        // layout is untouched, so nothing about the *rows* looks wrong.
        write_stamp(
            dir.path(),
            &FormatVersion {
                codec: FormatVersion::CURRENT.codec + 1,
                ..FormatVersion::CURRENT
            }
            .encode(),
        );

        assert!(
            matches!(
                FjallDb::open(dir.path()),
                Err(ApertureError::Format(FormatError::Unreadable { .. }))
            ),
            "a database from another format must be refused, not read",
        );

        // Refuse: the same database with the stamp removed — which is exactly what
        // every database written before this invariant existed looks like.
        remove_stamp(dir.path());

        assert!(
            matches!(
                FjallDb::open(dir.path()),
                Err(ApertureError::Format(FormatError::Unstamped))
            ),
            "an unstamped database holding facts must be refused, not stamped",
        );
    }

    /// A stamp that is present but **corrupt** is a refusal too, and a distinct
    /// one: the metadata is bytes on disk like any other and gets no more trust
    /// than a row does (conventions: errors, not panics, on data paths).
    #[test]
    fn a_corrupt_format_stamp_is_reported() {
        let dir = TempDir::new().expect("tempdir");

        FjallDb::open(dir.path()).expect("create");
        write_stamp(dir.path(), b"not a stamp");

        assert!(
            matches!(
                FjallDb::open(dir.path()),
                Err(ApertureError::Format(
                    FormatError::BadMagic { .. } | FormatError::Truncated { .. }
                ))
            ),
            "a corrupt stamp must be reported, not decoded into a version",
        );
    }

    /// The stamp as stored, or `None` for a database carrying none.
    fn read_stamp(db: &FjallDb) -> Option<FormatVersion> {
        let meta = db
            .db
            .keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)
            .expect("meta keyspace");

        meta.get(FORMAT_KEY)
            .expect("read the stamp")
            .map(|bytes| FormatVersion::decode(&bytes).expect("decode the stamp"))
    }

    /// Overwrite the stamp of the database at `path`, which must be closed.
    ///
    /// Written through a bare fjall handle rather than through [`FjallDb`], since
    /// what it is producing is a database this build cannot open.
    fn write_stamp(path: &std::path::Path, bytes: &[u8]) {
        let db = Database::builder(path).open().expect("open raw");
        let meta = db
            .keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)
            .expect("meta keyspace");
        let mut batch = db.batch();
        batch.insert(&meta, FORMAT_KEY, bytes);
        batch.commit().expect("write the stamp");
    }

    /// Remove the stamp from the database at `path`, which must be closed —
    /// turning it into a pre-stamp database.
    fn remove_stamp(path: &std::path::Path) {
        let db = Database::builder(path).open().expect("open raw");
        let meta = db
            .keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)
            .expect("meta keyspace");
        let mut batch = db.batch();
        batch.remove(&meta, FORMAT_KEY);
        batch.commit().expect("remove the stamp");
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

    /// [I11](../../docs/invariants.md#i11) — sequence 0 is reserved so that
    /// zeroed or corrupt bytes are *detectably* not a fact. That only holds if
    /// the decode boundary enforces it, so it is checked both as a unit and
    /// end to end, on a row written behind the store's back.
    #[test]
    fn a_zeroed_fact_id_is_rejected_at_decode() {
        assert!(matches!(
            decode_fact_id(&[0u8; FACT_ID_LEN]),
            Err(StoreError::FactIdSequence { sequence: 0, .. })
        ));

        // A corrupt `keys` row surfaces on the scan that reads it, rather than
        // handing `FactId(0)` to the executor to fail as a dangling reference at
        // projection — several layers from the row that is actually wrong.
        let seeded = seed(&[(0, vec![1u8], vec![])]);
        let trees = predicates_of(&seeded.db)
            .into_iter()
            .find(|(id, _)| *id == 0)
            .expect("predicate 0's trees")
            .1;

        let mut batch = seeded.db.db.batch();
        batch.insert(&trees.keys, bound_bytes(0, &[2]), vec![0u8; FACT_ID_LEN]);
        batch.commit().expect("write a corrupt keys row");

        let reader = seeded.db.reader();
        let lo = bound_bytes(0, &[]);
        let fault = reader
            .scan(&lo, strinc(&lo).as_deref())
            .expect("open scan")
            .find_map(Result::err)
            .expect("the corrupt row must surface");

        assert!(
            matches!(
                fault,
                ApertureError::Store(StoreError::FactIdSequence { sequence: 0, .. })
            ),
            "got {fault:?}"
        );
    }

    /// A key is written once ([`FjallDb::put_fact`]). Writing it twice would
    /// overwrite the `keys` row and strand the first fact's entity — invisible to
    /// every query, and undetectable without a bijection check. Enforcing that on
    /// the write path costs a lookup per fact, so it is a debug assertion; this
    /// is the control proving it is armed.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a key is written once")]
    fn writing_a_key_twice_is_caught_in_debug() {
        let dir = TempDir::new().expect("tempdir");
        let db = FjallDb::open(dir.path()).expect("open");

        db.put_fact(PredicateId(0), &[1, 2], &[]).expect("put");
        let _ = db.put_fact(PredicateId(0), &[1, 2], &[]);
    }

    /// [`FjallDb::reader`] costs the same whatever the schema's size.
    ///
    /// Opening a reader happens once per query, and it used to copy the whole
    /// predicate map — a heap allocation plus every predicate's handles. The map is
    /// shared behind an `Arc` now, so a DB with four times the predicates must cost
    /// a reader exactly the same: one allocation, for the snapshot.
    ///
    /// Measured rather than asserted, as every non-functional claim here is.
    #[test]
    fn opening_a_reader_does_not_scale_with_the_predicate_count() {
        // The counting allocator is only linked because `allocation-counter` is a
        // dev-dependency. If that breaks, `measure` reports zeroes and the equality
        // below holds vacuously — so prove the probe sees a known allocation.
        let control = allocation_counter::measure(|| {
            std::hint::black_box(Vec::<u8>::with_capacity(4096));
        });
        assert!(
            control.count_total > 0,
            "counting allocator is not installed; this guard would pass vacuously: {control:?}"
        );

        let reader_allocations = |predicates: u32| {
            let dir = TempDir::new().expect("tempdir");
            let db = FjallDb::open(dir.path()).expect("open");
            db.create_predicates((0..predicates).map(PredicateId))
                .expect("create predicate trees");

            let mut seen = 0;
            let info = allocation_counter::measure(|| {
                let reader = db.reader();
                seen = reader.predicates.len();
                std::hint::black_box(&reader);
            });

            // Without this the guard would also pass for a reader that saw nothing.
            assert_eq!(
                seen, predicates as usize,
                "the reader must see every predicate"
            );
            info.count_total
        };

        let few = reader_allocations(4);
        let many = reader_allocations(16);

        assert!(few > 0, "opening a reader allocated nothing at all");
        assert_eq!(
            few, many,
            "opening a reader scales with the schema: {few} allocations for 4 \
             predicates against {many} for 16"
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

    /// How many predicates the crashing writer spreads its facts across, and how
    /// many facts it commits *before* arming the watchdog.
    ///
    /// The prefix exists so the crash case can never be vacuous. A keyspace pair
    /// costs ~30 ms to create ([chapter 3]) and `put_fact` creates one lazily on
    /// first use, so on a busy disk four predicates' worth of setup can outlast the
    /// watchdog: the child then dies before a single fact is durable and the parent
    /// fails its own non-vacuity check, having learned nothing about I12.
    ///
    /// [chapter 3]: ../../docs/03-storage-model.md
    const CRASH_PREDICATES: u32 = 4;
    const CRASH_COMMITTED_PREFIX: u32 = 8;

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
            recovered >= CRASH_COMMITTED_PREFIX as usize,
            "recovered {recovered} facts, fewer than the {CRASH_COMMITTED_PREFIX} the child \
             committed before arming its watchdog — the crash case is vacuous"
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

    /// [I8](../../docs/invariants.md#i8) — an immutable snapshot per query,
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

        // Create the trees and commit a prefix before arming the watchdog, so the
        // kill always lands in the streaming phase — which is where the interesting
        // case is (inside a batch commit) — and never in keyspace creation. See
        // `CRASH_COMMITTED_PREFIX`.
        db.create_predicates((0..CRASH_PREDICATES).map(PredicateId))
            .expect("create predicate trees");

        for k in 0..CRASH_COMMITTED_PREFIX {
            db.put_fact(
                PredicateId(k % CRASH_PREDICATES),
                &k.to_be_bytes(),
                &[7; 48],
            )
            .expect("put");
        }

        thread::spawn(|| {
            thread::sleep(std::time::Duration::from_millis(150));
            std::process::abort();
        });

        for k in CRASH_COMMITTED_PREFIX..u32::MAX {
            db.put_fact(
                PredicateId(k % CRASH_PREDICATES),
                &k.to_be_bytes(),
                &[7; 48],
            )
            .expect("put");
        }
    }
}
