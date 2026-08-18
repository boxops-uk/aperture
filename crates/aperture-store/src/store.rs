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
//!   [`point`](crate::fact_store::FactStore::point) is handed a bare id, and the
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
//!   unchanged, since the engine's `Cursor` is bytes-only and
//!   re-seeks by exactly these bytes.
//!
//! `FjallDb` is the long-lived handle and owns the id allocator
//! ([I11](../../docs/invariants.md#i11)); [`FjallDb::reader`] hands the executor
//! the `(handle, snapshot)` pair it consumes.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use byteview::ByteView;

use crate::lookup_cache::{Hit, LookupCache};
use fjall::{Database, Keyspace, KeyspaceCreateOptions, Readable, Snapshot};

use crate::{
    error::{FormatError, StoreError},
    fact::{self, Fact},
    fact_store::{Entity, FactStore},
    format::{FORMAT_KEY, FormatVersion, META_KEYSPACE},
};
use aperture_schema::{
    id::{FactId, FactIdError, MAX_FACT_SEQUENCE, MAX_TAGGABLE_PREDICATE},
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

/// A fact already in the store, as [`FjallDb::put`] needs to see it: the id it
/// was given, and its value bytes to compare a re-offered fact against.
struct StoredFact {
    id: FactId,
    value: Vec<u8>,
}

/// What [`FjallDb::intern`](FjallStore::intern) answers: the fact's id, and whether
/// this call is what put it there.
///
/// `created` is not bookkeeping. Interning is how a nested reference becomes an id
/// ([chapter 3](../../docs/03-storage-model.md#interning-a-nested-fact)), and a
/// target named under a thousand parents is one row — so the count a write stream
/// reports back, and the dedup `ops-I5` promises, are both this flag summed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interned {
    pub id: FactId,
    pub created: bool,
}

/// A predicate's trees plus its own id allocator
/// ([I11](../../docs/invariants.md#i11)).
struct Predicate {
    trees: Trees,
    /// The next sequence to hand out.
    next_sequence: AtomicU64,
    /// The highest sequence this predicate has **durably claimed the right to use**
    /// — see [`FjallDb::allocate`].
    reserved_through: AtomicU64,
    /// Held only while extending the claim, so the durable write happens once per
    /// [`RESERVATION_CHUNK`] rather than once per racing writer.
    reserving: Mutex<()>,
}

/// The long-lived database handle: the fjall environment, the per-predicate
/// keyspace handles, and the fact-id allocators.
pub struct FjallDb {
    db: Database,
    /// The `meta` keyspace, held open: the format stamp is read from it once, and the
    /// id reservations are written to it for the life of the database.
    meta: Keyspace,
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
    /// **The striped merge frontier**: per-key exclusion, and the cache behind the
    /// same lock — see [`FjallDb::intern`] and
    /// [chapter 3](../../docs/03-storage-model.md#the-other-half-of-the-bijection--one-key-one-fact).
    ///
    /// One mutex per stripe, held across a whole resolve-or-create. The cache lives
    /// *inside* it rather than beside it because the two want exactly the same
    /// critical section: what `(predicate, key)` already names, and the right to
    /// decide that it names nothing yet.
    shards: Box<[Mutex<LookupCache>]>,
    /// What the interning path actually read — see [`InternReads`].
    intern_reads: InternReads,
    /// How many writers are in it at once — see [`InFlight`].
    in_flight: InFlight,
}

/// Live LSM point reads the interning path has done, per tree.
///
/// **The number [Phase 12c](../../../PLAN.md) is judged by, so it is counted rather
/// than argued.** A cache that is not hitting is indistinguishable from one that is
/// absent unless something says how many reads survived it, and the two trees are
/// counted apart because they are two separate claims: the cache removes `keys`
/// reads, and the key-only fast path removes `entities` reads. One relaxed increment
/// per read, against a read that touches an LSM — the measurement is free at this
/// scale and a guard that cannot be written is not.
#[derive(Default)]
struct InternReads {
    /// `keys` lookups — one per resolve the cache did not answer.
    keys: AtomicU64,
    /// `entities` lookups — one per *found* fact whose predicate declares a value
    /// side. Key-only predicates never reach this.
    entities: AtomicU64,
}

/// Bytes the lookup cache may hold per generation, so **at most twice this is
/// resident** (`old` is a whole generation).
///
/// **Quoted in bytes because an entry count bounds nothing here.** These keys are
/// encoded fact keys — a path, a nested reference, a name — so the same count costs
/// several times as much on one corpus as on another, and a count-based ceiling is a
/// ceiling on the wrong quantity.
///
/// Sized against the hot parent set of a large code index: the 25M-fact
/// `dotnet/runtime` index nests ~957k distinct files, modules and declarations, whose
/// keys and overhead come to roughly 100 MB by
/// [`cost`](crate::lookup_cache::cost)'s reckoning. 128 MiB per generation holds that
/// set without rotating a live parent out, and bounds the cache at ~256 MiB — well
/// under the *"almost exactly as much as the facts themselves"* that Glean's own
/// post-mortem reports for its equivalent maps.
///
/// [Phase 12d](../../../PLAN.md) stripes interning, and this budget is then **divided**
/// across the stripes rather than paid per stripe.
const LOOKUP_CACHE_BYTES: usize = 128 << 20;

/// How many threads are inside interning, and the most there have ever been at once.
///
/// **A gauge rather than a timing test.** [Phase 12e](../../../PLAN.md) retires the server's
/// per-database writer, and the claim that makes is "two write streams now proceed
/// together" — which a stopwatch can only argue for and a high-water mark can settle. Two
/// relaxed atomics and a `fetch_max` per intern, against an intern that measures over a
/// microsecond.
#[derive(Default)]
struct InFlight {
    now: AtomicU64,
    peak: AtomicU64,
}

/// Counts one thread in, and out again however the call leaves.
struct Interning<'db>(&'db InFlight);

impl<'db> Interning<'db> {
    fn enter(gauge: &'db InFlight) -> Self {
        let now = gauge.now.fetch_add(1, Ordering::Relaxed) + 1;
        gauge.peak.fetch_max(now, Ordering::Relaxed);
        Interning(gauge)
    }
}

impl Drop for Interning<'_> {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::Relaxed);
    }
}

/// **Debug-only proof that a frontier critical section is never entered inside another.**
///
/// The striping needs no lock ordering, and the reason is a fact about interning rather
/// than a discipline anyone has to keep: *a parent's key has no bytes until its children
/// have ids*, so a worker interns leaf-then-parent and every critical section has closed
/// before the next opens ([chapter 3](../../docs/03-storage-model.md#interning-a-nested-fact)).
/// The whole no-deadlock argument is that sentence.
///
/// So the sentence is checked. A future change that resolved a child *inside*
/// `resolve_or_create` would deadlock against itself the moment both keys hashed to one
/// stripe — intermittently, in production, on a non-reentrant mutex, which is about the
/// worst way to learn it. In a debug build it panics instead, at the call that did it.
///
/// Nothing in release: `SHARDS` mutexes and a thread-local are not worth a counter that
/// only ever proves what the code already says.
#[cfg(debug_assertions)]
struct NotNested;

#[cfg(debug_assertions)]
thread_local! {
    static IN_FRONTIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(debug_assertions)]
impl NotNested {
    fn enter() -> Self {
        IN_FRONTIER.with(|held| {
            assert!(
                !held.replace(true),
                "a merge frontier critical section was entered inside another. Interning \
                 is bottom-up precisely so this cannot happen — a child is resolved \
                 before its parent's key exists, never during. Whatever now resolves one \
                 inside `resolve_or_create` has to stop, or the striping needs a lock \
                 ordering it was designed not to need."
            );
        });
        NotNested
    }
}

#[cfg(debug_assertions)]
impl Drop for NotNested {
    fn drop(&mut self) {
        IN_FRONTIER.with(|held| held.set(false));
    }
}

#[cfg(not(debug_assertions))]
struct NotNested;

#[cfg(not(debug_assertions))]
impl NotNested {
    fn enter() -> Self {
        NotNested
    }
}

/// Sequences claimed by one durable reservation write.
///
/// The trade is gap size against write count. A crash abandons whatever is left of the
/// current claim, so 1,024 leaves at most that many holes per predicate — invisible, since
/// [I11](../../docs/invariants.md#i11) asks for unique and never-reused, not dense — and
/// costs one small write per thousand facts, which is nothing beside the fact writes
/// themselves.
const RESERVATION_CHUNK: u64 = 1024;

/// Where a predicate's claim is written in the `meta` keyspace: `seq/` ++ id (BE).
fn reservation_key(predicate: PredicateId) -> Vec<u8> {
    let mut key = b"seq/".to_vec();
    key.extend_from_slice(&predicate.0.to_be_bytes());
    key
}

/// Stripes in the merge frontier. **Must be a power of two** — [`shard_of`] masks.
///
/// Sized from both directions. Upwards: enough that writers rarely collide, and with W
/// writers the chance that two are in the same stripe is about W/64 — negligible at the
/// 8–16 a machine has. Downwards: each stripe holds a 64th of
/// [`LOOKUP_CACHE_BYTES`], and a 64th of a large index's hot parent set (~957k entries,
/// ~100 MB) is ~1.5 MB against a 2 MiB slice — so the whole hot set stays resident and no
/// stripe rotates a live parent out. Raising this trades the second property for the
/// first; both are load-bearing, so change it with the arithmetic in front of you.
const SHARDS: usize = 64;

/// Which stripe a key belongs to.
///
/// **Any deterministic hash does.** A stripe is a lock, not a location: nothing on disk
/// depends on this, no cursor encodes it, and two processes never share one
/// (`ops-I1`). So the only requirement is that one process agrees with itself, and FNV-1a
/// is three lines.
///
/// It hashes the **whole** `predicate ++ key`, which costs about a nanosecond a byte on
/// top of an intern that measures 1.3–2.2 µs — call it 5%, paid to make the write path
/// parallel at all. Hashing a bounded prefix would be cheaper and wrong: these keys are
/// paths and nested records, so they share prefixes by construction and a prefix hash
/// would pile a directory's files into one stripe. If the 5% ever matters,
/// `examples/ingest.rs` can now price it, which is why that came first.
fn shard_of(index_key: &[u8]) -> usize {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in index_key {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    (hash as usize) & (SHARDS - 1)
}

/// What a block has staged but not yet committed.
struct Staging {
    batch: fjall::OwnedWriteBatch,
    /// `index key → (id, value)` for facts created in this batch. The trees cannot
    /// answer for them yet, and the stripe cache is allowed to forget them.
    pending: std::collections::HashMap<Vec<u8>, (FactId, Vec<u8>)>,
}

/// **One commit for a whole block** — [12f](../../PLAN.md), and a deliberate trade.
///
/// A fact normally becomes durable before its id is returned, which is what lets the
/// allocator resume from the last `entities` key and what keeps everything on disk in
/// dependency order. Committing per block breaks both, and buys the largest single term
/// in the write path: committing is 41% of interning
/// ([findings §13](../../bench/FINDINGS.md)), one fjall batch per fact through one global
/// journal mutex.
///
/// # What it costs, exactly
///
/// An id is handed out while its bytes are still here. Another writer can resolve that
/// key, take the id, and commit **first** — so a crash can truncate the journal after the
/// reference and before the fact it names. What survives is a reference to a fact that
/// was never written.
///
/// Two things bound that, and the second is the one that had to be built for this:
///
/// - `finish` walks every reference to compute identity, so a dangling one raises
///   `DanglingFactId` and the database cannot be sealed. It is never shipped.
/// - The id it named is **never reissued**, because [`FjallDb::allocate`] claims ranges
///   durably ahead of use. Without that the allocator would resume below the lost id and
///   hand it to a different fact, and the reference would then resolve — to the wrong
///   target, silently, through a seal that cannot tell the difference.
///
/// So the honest statement of the trade is: *a crash during ingest may cost the index,
/// never its correctness.* That is why this is a server flag rather than the default, and
/// why the flag is off unless somebody asks for it.
pub struct Staged<'db> {
    db: &'db FjallDb,
    staging: std::cell::RefCell<Staging>,
}

impl Staged<'_> {
    /// Resolve-or-create, with any created fact staged rather than committed.
    ///
    /// # Errors
    ///
    /// As [`FjallDb::intern`](FjallDb::intern).
    pub fn intern(
        &self,
        predicate: PredicateId,
        key_fields: &[u8],
        value: &[u8],
        keyed_only: bool,
    ) -> Result<Interned, StoreError> {
        let mut staging = self.staging.borrow_mut();
        self.db
            .intern_into(predicate, key_fields, value, keyed_only, Some(&mut staging))
    }

    /// Commit everything staged, in one batch.
    ///
    /// **Call this even when the block failed.** Ids from the part that succeeded may
    /// already have been handed to another writer, and throwing the batch away would
    /// strand every one of them. A partly-written block is what a per-fact ingest leaves
    /// behind too, and `ops-I5`'s idempotence is what makes re-sending it safe.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if fjall could not commit.
    pub fn commit(self) -> Result<(), StoreError> {
        self.staging
            .into_inner()
            .batch
            .commit()
            .map_err(StoreError::Backend)
    }

    /// Facts staged and not yet committed.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.staging.borrow().pending.len()
    }
}

/// Both of a fact's rows, into one batch — the `keys` index entry and the `entities`
/// record, which [I12](../../docs/invariants.md#i12) requires to travel together.
fn stage_rows(
    batch: &mut fjall::OwnedWriteBatch,
    handle: &Predicate,
    fact_id: FactId,
    index_key: Vec<u8>,
    key_fields: &[u8],
    value: &[u8],
) {
    let mut entity = Vec::with_capacity(KEY_LEN_LEN + key_fields.len() + value.len());
    entity.extend_from_slice(&(key_fields.len() as u32).to_be_bytes());
    entity.extend_from_slice(key_fields);
    entity.extend_from_slice(value);

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
}

/// The `keys` row a fact is indexed under: `predicate_id (BE) ++ encoded_key`.
fn index_key_for(predicate: PredicateId, key_fields: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(PREDICATE_ID_SIZE + key_fields.len());
    key.extend_from_slice(&predicate.0.to_be_bytes());
    key.extend_from_slice(key_fields);
    key
}

impl FjallDb {
    /// Open (creating if absent) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
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

        let meta = db
            .keyspace(META_KEYSPACE, KeyspaceCreateOptions::default)
            .map_err(StoreError::Backend)?;

        let mut predicates = BTreeMap::new();
        for id in ids {
            let predicate = PredicateId(id);
            predicates.insert(id, Arc::new(Self::open_predicate(&db, &meta, predicate)?));
        }

        Ok(Self {
            db,
            meta,
            predicates: RwLock::new(Arc::new(predicates)),
            shards: (0..SHARDS)
                .map(|_| Mutex::new(LookupCache::new(LOOKUP_CACHE_BYTES / SHARDS)))
                .collect(),
            intern_reads: InternReads::default(),
            in_flight: InFlight::default(),
        })
    }

    /// Check the [format stamp](crate::format), or write it if this
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
    fn stamp_or_check_format(db: &Database, holds_facts: bool) -> Result<(), StoreError> {
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

    /// Make every write durable — `fsync`, not merely handed to the OS.
    ///
    /// What `finish` calls before it computes anything: an identity describing bytes
    /// that a power loss could still take back would be a claim about a database that
    /// might not exist (`ops-I3`).
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the flush fails.
    pub fn persist(&self) -> Result<(), StoreError> {
        self.db
            .persist(fjall::PersistMode::SyncAll)
            .map_err(StoreError::Backend)
    }

    /// Merge every tree down to as few tables as the backend will make, discarding
    /// everything superseded on the way.
    ///
    /// **What `finish` calls, and the only caller there should be.** A write leaves the
    /// tree in whatever shape the write order produced; nothing reclaims that during
    /// ingestion, and a database that is still being written may be written again in a
    /// moment, so paying to merge it would be paying twice. Sealing is the one point
    /// where the shape is final: `Complete` is immutable forever, and the artifact is
    /// then copied per reader process ([operations §5]).
    ///
    /// It is worth doing because the cost lands on *reads*, per page, forever. A resume
    /// replays one seek per plan level per 256-row chunk, and an unmerged tree was
    /// measured seeking at up to 180× a merged one on an 18M-fact index — 790 µs
    /// against 4.7 µs — with the whole store also halving on disk (`bench/FINDINGS.md`).
    ///
    /// **Not free, and it is the caller's second of two costs.** 23 s on that index,
    /// single threaded, on top of the identity walk. Both are paid once, by the
    /// operation whose whole job is to say "this is finished".
    ///
    /// The predicate map is cloned rather than held: merging is long, and a reader
    /// asking which predicates exist should not wait behind it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if a merge fails. A failure leaves the database
    /// readable and unsealed — the trees are merged one at a time and each merge is
    /// atomic, so what has happened is that some are tidier than others.
    ///
    /// [operations §5]: ../../docs/aperture-cli-design.md
    pub fn compact(&self) -> Result<(), StoreError> {
        let predicates = Arc::clone(&self.predicates.read().expect("predicate map lock"));

        for predicate in predicates.values() {
            predicate
                .trees
                .keys
                .major_compact()
                .map_err(StoreError::Backend)?;
            predicate
                .trees
                .entities
                .major_compact()
                .map_err(StoreError::Backend)?;
        }

        Ok(())
    }

    /// The predicates this database has trees for, in id order.
    ///
    /// Recovered from the keyspace listing at open, so it answers what is *on disk*
    /// rather than what a schema says should be — which is the question `describe`
    /// asks, and the one that distinguishes a database created from a schema from one
    /// that has never been written to.
    #[must_use]
    pub fn predicate_ids(&self) -> Vec<PredicateId> {
        self.predicates
            .read()
            .expect("predicate map lock is poisoned")
            .keys()
            .map(|id| PredicateId(*id))
            .collect()
    }

    /// Create the trees for `predicates` now rather than on first write.
    ///
    /// Keyspace creation is ~30 ms apiece, so lazy creation puts that latency
    /// inside an ingest at an unpredictable point. A DB created from a schema
    /// knows every predicate up front and should pay the bill once, here.
    pub fn create_predicates(
        &self,
        predicates: impl IntoIterator<Item = PredicateId>,
    ) -> Result<(), StoreError> {
        for predicate in predicates {
            self.predicate(predicate)?;
        }
        Ok(())
    }

    /// Open both of a predicate's trees and recover its allocator.
    fn open_predicate(
        db: &Database,
        meta: &Keyspace,
        predicate: PredicateId,
    ) -> Result<Predicate, StoreError> {
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

        // **The higher of what is stored and what was claimed**, and it needs both.
        // The stored half is what an older database has, and what a database written
        // entirely by committed-per-fact writes needs — there may be no claim at all.
        // The claimed half is what makes an id safe to hand out *before* its fact is
        // durable, which is the whole of 12f.
        let high_water = Self::recover_high_water(&trees, predicate)?;
        let reserved = Self::recover_reservation(meta, predicate)?;
        let resume = high_water.max(reserved);

        Ok(Predicate {
            trees,
            next_sequence: AtomicU64::new(resume + 1),
            reserved_through: AtomicU64::new(resume),
            reserving: Mutex::new(()),
        })
    }

    /// The highest sequence this predicate has durably claimed, or 0 if it never has.
    fn recover_reservation(meta: &Keyspace, predicate: PredicateId) -> Result<u64, StoreError> {
        let Some(bytes) = meta
            .get(reservation_key(predicate))
            .map_err(StoreError::Backend)?
        else {
            return Ok(0);
        };

        let claimed: [u8; 8] = bytes.as_ref().try_into().map_err(|_| StoreError::Meta {
            path: std::path::PathBuf::from(format!("predicate {}", predicate.0)),
            detail: "the id reservation is not eight bytes".to_owned(),
        })?;

        Ok(u64::from_be_bytes(claimed))
    }

    /// The predicate's highest allocated sequence, or 0 if it holds no facts.
    ///
    /// An `entities` key **is** a fact id, big-endian, so the tree's last key is
    /// the high-water mark ([I11](../../docs/invariants.md#i11)).
    ///
    /// **On its own this is not enough, and the reason is worth keeping.** Deriving the
    /// allocator from the data cannot disagree with what is stored — but only while every
    /// id handed out is *already* stored, which is true exactly when a fact is committed
    /// before its id is returned. [12f](../../PLAN.md) breaks that premise on purpose: a
    /// staged write hands out an id whose bytes are still in an uncommitted batch, another
    /// writer may reference it and commit first, and a crash then loses the higher entity
    /// while keeping the reference. Resuming from the data would reissue that id to a
    /// different fact — a reference that resolves to the **wrong** fact, which `finish`
    /// cannot catch because it only checks that references resolve. Hence
    /// [`recover_reservation`](Self::recover_reservation).
    fn recover_high_water(trees: &Trees, predicate: PredicateId) -> Result<u64, StoreError> {
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
            });
        }

        Ok(fact_id.sequence())
    }

    /// The handles for `predicate`, creating both trees on first write.
    fn predicate(&self, predicate: PredicateId) -> Result<Arc<Predicate>, StoreError> {
        if predicate.0 > MAX_TAGGABLE_PREDICATE {
            // Rejected before the trees exist: a predicate whose id cannot be
            // tagged into a `FactId` can never have a fact written to it, so
            // failing at create is better than failing mid-ingest.
            return Err(FactIdError::PredicateIdTooWide {
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

        let handle = Arc::new(Self::open_predicate(&self.db, &self.meta, predicate)?);
        Arc::make_mut(&mut predicates).insert(predicate.0, Arc::clone(&handle));
        Ok(handle)
    }

    /// Write a **well-typed value** as a fact, checked against the schema.
    ///
    /// The way to write a fact by hand: name the predicate and its fields, and let
    /// [`fact`](crate::fact) resolve them — a field the predicate does not
    /// declare, one left out, one of the wrong shape, or a value side that should not
    /// be there is an error rather than bytes nobody can read back. See that module for
    /// why naming the fields is the point.
    ///
    /// The returned id is what a *reference* to this fact is, so the next fact that
    /// points at it takes this value.
    ///
    /// # A key is written once, and this is where that is enforced
    ///
    /// [`put_fact`](Self::put_fact) leaves the write-once contract to its caller,
    /// because it is the primitive bulk ingest is built on and the check is a point
    /// lookup per fact. **`put` is not that caller.** It is the documented way to
    /// write a fact by hand, it already pays schema resolution and a full encode per
    /// fact, and inheriting a bulk primitive's contract is how a duplicate key came
    /// to silently strand a fact in release builds. So it pays the lookup, and the
    /// semantics are the ones the merge frontier already commits to
    /// ([operations §5](../../docs/aperture-cli-design.md)):
    ///
    /// - a **byte-identical** fact dedups — the id already assigned comes back, and
    ///   nothing is written;
    /// - a **same-key, different-value** fact is rejected
    ///   ([`StoreError::KeyAlreadyWritten`]).
    ///
    /// Never last-writer-wins, which is what the unchecked path silently did.
    ///
    /// This is a check, not a lock: two threads writing the *same key* at once can
    /// both miss it and both write. What rules that out is
    /// [ops-I1](../../docs/aperture-cli-design.md)'s single writer per DB, not this
    /// lookup — which is here for the sequential mistake, the one that actually
    /// happens.
    ///
    /// # Errors
    ///
    /// [`StoreError::Fact`] if the value does not fit the schema,
    /// [`StoreError::KeyAlreadyWritten`] if the key holds a different fact, and
    /// whatever [`put_fact`](Self::put_fact) reports otherwise.
    pub fn put<F: Fact>(&self, schema: &Schema, fact: &F) -> Result<FactId, StoreError> {
        let (predicate, key, value) = fact::encode(schema, fact)?;

        // The schema decides whether there is a value side, exactly as it does on
        // the ingest path — not the emptiness of the encoded bytes.
        let keyed_only = schema
            .get(predicate)
            .is_some_and(|declared| declared.predicate().value.is_none());

        Ok(self.intern(predicate, &key, &value, keyed_only)?.id)
    }

    /// **Resolve or create**: the id of the fact under `(predicate, key_fields)`,
    /// writing it first if it is not there.
    ///
    /// The rule [`put`](Self::put) documents, lifted out to be reached from bytes as
    /// well as from a typed value — which is what
    /// [interning a nested reference](../../docs/03-storage-model.md#interning-a-nested-fact)
    /// needs, since a fact arriving on the wire is bytes by the time its target has
    /// to be looked up. There is one implementation of the rule rather than two,
    /// which matters because the rule is `ops-I5`'s and drifting halves of it would
    /// be two different databases depending on how a fact was written.
    ///
    /// - a **byte-identical** fact dedups: the id already assigned comes back with
    ///   [`created`](Interned::created) false, and nothing is written;
    /// - a **same-key, different-value** fact is rejected
    ///   ([`StoreError::KeyAlreadyWritten`]).
    ///
    /// Never last-writer-wins and never first-writer-wins — either is
    /// order-dependent, which [ops-I4](../../docs/aperture-cli-design.md) forbids.
    ///
    /// This is a check, not a lock: see [`put`](Self::put) for why
    /// [ops-I1](../../docs/aperture-cli-design.md)'s single writer per DB is what
    /// rules out the concurrent case.
    ///
    /// # Errors
    ///
    /// [`StoreError::KeyAlreadyWritten`] on a conflict, and whatever
    /// [`put_fact`](Self::put_fact) reports otherwise.
    pub fn intern(
        &self,
        predicate: PredicateId,
        key_fields: &[u8],
        value: &[u8],
        keyed_only: bool,
    ) -> Result<Interned, StoreError> {
        self.intern_into(predicate, key_fields, value, keyed_only, None)
    }

    /// [`intern`](Self::intern), with the created fact's bytes going either straight to
    /// the trees or into a block's batch.
    fn intern_into(
        &self,
        predicate: PredicateId,
        key_fields: &[u8],
        value: &[u8],
        keyed_only: bool,
        staging: Option<&mut Staging>,
    ) -> Result<Interned, StoreError> {
        let _in_flight = Interning::enter(&self.in_flight);
        let index_key = index_key_for(predicate, key_fields);

        // **This block's own creations, before anything else.** Their bytes are in an
        // uncommitted batch, so the trees cannot answer for them — and the stripe cache
        // could, but only until it rotates. Keeping them here rather than relying on the
        // cache is what stops the cache becoming load-bearing for correctness: an
        // eviction must cost a point read, never a duplicate key.
        if let Some((id, stored)) = staging
            .as_deref()
            .and_then(|staged| staged.pending.get(&index_key))
        {
            return if stored.as_slice() == value {
                Ok(Interned {
                    id: *id,
                    created: false,
                })
            } else {
                Err(StoreError::KeyAlreadyWritten {
                    predicate,
                    existing: *id,
                })
            };
        }

        // **The critical section, and it is the whole of the operation.** Look, then
        // read, then write, holding one stripe throughout — because this is a
        // read-modify-write and anything less lets two writers both find the key absent
        // and both create it, stranding one entity under a `keys` row the other
        // overwrote ([I12](../../docs/invariants.md#i12)).
        //
        // Per *key* rather than per database, which is the difference between a
        // mechanism and a bottleneck: the exclusion is exactly as wide as the thing being
        // decided.
        let _not_nested = NotNested::enter();
        let mut shard = self.shards[shard_of(&index_key)]
            .lock()
            .expect("a merge frontier stripe is poisoned");

        // The cache first, because a nested index asks for the same parents in
        // bursts. A hit is authoritative: a fact never changes once written, so an
        // entry cannot go stale (`ops-I2`). The comparison happens *inside* the cache
        // so that a hit hands back an id and never a row — a row would clone its
        // value on every interned reference, which is the cost this exists to remove.
        match shard.lookup(&index_key, value) {
            Some(Hit::Agrees(id)) => {
                return Ok(Interned { id, created: false });
            }
            Some(Hit::Conflicts(existing)) => {
                return Err(StoreError::KeyAlreadyWritten {
                    predicate,
                    existing,
                });
            }
            None => {}
        }

        if let Some(existing) = self.fact_at(predicate, &index_key, keyed_only)? {
            shard.insert(&index_key, existing.id, &existing.value);

            return if existing.value == value {
                Ok(Interned {
                    id: existing.id,
                    created: false,
                })
            } else {
                Err(StoreError::KeyAlreadyWritten {
                    predicate,
                    existing: existing.id,
                })
            };
        }

        let id = match staging {
            None => self.put_fact(predicate, key_fields, value)?,
            Some(staged) => {
                let (id, _) = self.stage_fact(&mut staged.batch, predicate, key_fields, value)?;
                staged
                    .pending
                    .insert(index_key.clone(), (id, value.to_vec()));
                id
            }
        };

        // Published to the shared stripe either way, because the frontier's one-key-one-
        // fact rule depends on the *next* resolver seeing this — including one on another
        // thread. That is what makes a staged id observable before it is durable, and
        // therefore why [`FjallDb::allocate`] claims its range up front.
        shard.insert(&index_key, id, value);

        Ok(Interned { id, created: true })
    }

    /// Writers inside interning right now, and the most there have ever been at once.
    ///
    /// The peak is what says the write path is actually parallel: it reaches 1 however
    /// many streams a serialised server has, and only exceeds it if two of them were
    /// genuinely interning together.
    pub fn intern_concurrency(&self) -> (u64, u64) {
        (
            self.in_flight.now.load(Ordering::Relaxed),
            self.in_flight.peak.load(Ordering::Relaxed),
        )
    }

    /// Allocate an id and put a fact's two rows into `batch`, without committing.
    ///
    /// The staging half of [`put_fact`](Self::put_fact): everything except the commit,
    /// so a block can pay for one.
    fn stage_fact(
        &self,
        batch: &mut fjall::OwnedWriteBatch,
        predicate: PredicateId,
        key_fields: &[u8],
        value: &[u8],
    ) -> Result<(FactId, Vec<u8>), StoreError> {
        let handle = self.predicate(predicate)?;
        let fact_id = FactId::new(predicate, self.allocate(&handle, predicate)?)?;
        let index_key = index_key_for(predicate, key_fields);

        stage_rows(
            batch,
            &handle,
            fact_id,
            index_key.clone(),
            key_fields,
            value,
        );
        Ok((fact_id, index_key))
    }

    /// The next sequence for `predicate`, **durably claimed before it is handed out**.
    ///
    /// The counter is the only source of sequences, so uniqueness needs no coordination
    /// between writers: `fetch_add` is the whole of it. A sequence consumed by a write
    /// that then fails is not handed out again — [I11](../../docs/invariants.md#i11) asks
    /// for unique and never-reused, not dense.
    ///
    /// **What the claim adds is survival across a crash.** The allocator used to resume
    /// from the last `entities` key, which is exact as long as every id handed out is
    /// already stored. A staged write breaks that: it returns an id whose bytes sit in an
    /// uncommitted batch, another writer can reference it and commit *first*, and a crash
    /// that loses the batch would leave the reference behind while the allocator resumed
    /// below it — reissuing that id to a different fact, which turns the reference into
    /// one that resolves to the **wrong** target. `finish` cannot catch that: it checks
    /// that references resolve, not that they resolve to what they meant.
    ///
    /// So a range is written to `meta` *before* any id in it is returned. The write needs
    /// no `fsync`: it goes through the same journal as the fact batches and is ordered
    /// before them, so a truncation that loses a batch cannot also have lost the claim
    /// that preceded it. What a crash costs is the unused tail of the current chunk,
    /// which is a hole in the sequence and nothing more.
    fn allocate(&self, handle: &Predicate, predicate: PredicateId) -> Result<u64, StoreError> {
        let sequence = handle.next_sequence.fetch_add(1, Ordering::Relaxed);

        if sequence <= handle.reserved_through.load(Ordering::Acquire) {
            return Ok(sequence);
        }

        // Extending is rare — once per `RESERVATION_CHUNK` — so one lock for it costs
        // nothing, and it keeps a burst of writers from each writing a claim.
        let _extending = handle
            .reserving
            .lock()
            .expect("an id reservation lock is poisoned");

        // Re-read under the lock: whoever held it before us may have covered this.
        let reserved = handle.reserved_through.load(Ordering::Acquire);
        if sequence <= reserved {
            return Ok(sequence);
        }

        // Cover this sequence and the chunk beyond it, so a writer that jumped far
        // ahead of the claim (many threads allocating at once) still needs one write.
        let claim = sequence
            .max(reserved)
            .saturating_add(RESERVATION_CHUNK)
            .min(MAX_FACT_SEQUENCE);

        let mut batch = self.db.batch();
        batch.insert(&self.meta, reservation_key(predicate), claim.to_be_bytes());
        batch.commit().map_err(StoreError::Backend)?;

        handle.reserved_through.store(claim, Ordering::Release);
        Ok(sequence)
    }

    /// Live point reads the interning path has done since open, as
    /// `(keys, entities)`.
    ///
    /// The two claims 12c makes, separately checkable: `keys` should be one per
    /// *distinct* key an ingest touches rather than one per reference to it, and
    /// `entities` should be zero for a key-only predicate however often it is found.
    pub fn intern_read_counters(&self) -> (u64, u64) {
        (
            self.intern_reads.keys.load(Ordering::Relaxed),
            self.intern_reads.entities.load(Ordering::Relaxed),
        )
    }

    /// Hits and misses on the ingest lookup cache since open, summed over the stripes.
    ///
    /// Not a consistent snapshot: the stripes are read one after another and a writer may
    /// move a later one while an earlier one is being read. That is the right trade for a
    /// report — taking every stripe's lock at once to make the sum atomic would let a
    /// statistics call stall the write path it is reporting on.
    pub fn lookup_counters(&self) -> (u64, u64) {
        let mut totals = (0, 0);
        for shard in &self.shards {
            let (hits, misses) = shard
                .lock()
                .expect("a merge frontier stripe is poisoned")
                .counters();
            totals.0 += hits;
            totals.1 += misses;
        }
        totals
    }

    /// The fact already stored under `(predicate, key_fields)`, if there is one:
    /// its id and its value bytes, which is what [`put`](Self::put) compares
    /// against to tell a duplicate from a conflict.
    ///
    /// Reads the trees live rather than through a snapshot — a writer wants what is
    /// there *now*, not a repeatable read.
    fn fact_at(
        &self,
        predicate: PredicateId,
        index_key: &[u8],
        keyed_only: bool,
    ) -> Result<Option<StoredFact>, StoreError> {
        let handle = self.predicate(predicate)?;

        self.intern_reads.keys.fetch_add(1, Ordering::Relaxed);
        let Some(id) = handle
            .trees
            .keys
            .get(index_key)
            .map_err(StoreError::Backend)?
        else {
            return Ok(None);
        };

        let id = decode_fact_id(&id)?;

        // **A key-only predicate needs no second read.** Its declared value type has
        // no inhabitant but the empty one, so the stored value is empty by
        // construction and the comparison the caller is about to make is already
        // decided. 22 of the 27 predicates in a code index are key-only, and this is
        // one of the two live point reads per interned reference.
        //
        // What this gives up is that the `entities` read doubled as an I12 check —
        // a `keys` row whose `entities` row is missing. That check is kept by the
        // tests rather than paid for on every reference at ingest.
        if keyed_only {
            return Ok(Some(StoredFact {
                id,
                value: Vec::new(),
            }));
        }

        // A `keys` row without its `entities` row is a broken I12, not a fact
        // this key does not have — reported rather than read as "free to write".
        self.intern_reads.entities.fetch_add(1, Ordering::Relaxed);
        let entity = handle
            .trees
            .entities
            .get(id.raw().to_be_bytes())
            .map_err(StoreError::Backend)?
            .ok_or(StoreError::DanglingFactId(id))?;

        // The row is `[key_len u32 BE][key][value]`; only the value is wanted, the
        // key being `key_fields` by construction.
        let framing: [u8; KEY_LEN_LEN] = entity
            .get(..KEY_LEN_LEN)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(StoreError::TruncatedEntity(id))?;
        let key_end = KEY_LEN_LEN + u32::from_be_bytes(framing) as usize;
        let value = entity
            .get(key_end..)
            .ok_or(StoreError::TruncatedEntity(id))?
            .to_vec();

        Ok(Some(StoredFact { id, value }))
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
    ) -> Result<FactId, StoreError> {
        let handle = self.predicate(predicate)?;

        let sequence = self.allocate(&handle, predicate)?;
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

        let mut batch = self.db.batch();
        stage_rows(&mut batch, &handle, fact_id, index_key, key_fields, value);
        batch.commit().map_err(StoreError::Backend)?;

        Ok(fact_id)
    }

    /// A [`Staged`] writer over this database: one batch for a whole block.
    ///
    /// See [`Staged`] for what it buys and what it costs.
    #[must_use]
    pub fn staged(&self) -> Staged<'_> {
        Staged {
            db: self,
            staging: std::cell::RefCell::new(Staging {
                batch: self.db.batch(),
                pending: std::collections::HashMap::new(),
            }),
        }
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

    /// How many tables each tree is spread across, keys and entities alike.
    ///
    /// The witness for [`compact`](Self::compact): a merged tree is one table, and
    /// nothing else this crate exposes can tell one table from five.
    #[cfg(any(test, feature = "proptest"))]
    #[must_use]
    pub fn table_counts(&self) -> Vec<usize> {
        let predicates = Arc::clone(&self.predicates.read().expect("predicate map lock"));

        predicates
            .values()
            .flat_map(|predicate| {
                [
                    predicate.trees.keys.table_count(),
                    predicate.trees.entities.table_count(),
                ]
            })
            .collect()
    }

    /// Turn whatever is in memory into a table, per tree.
    ///
    /// **A test fixture, and it exists to make a guard non-vacuous.** At the sizes a
    /// test writes, every fact lives in one memtable and no tree ever has two tables
    /// to merge — so a compaction guard would pass having compacted nothing. This is
    /// how a test states "there were three tables here" as a fact rather than a hope.
    #[cfg(any(test, feature = "proptest"))]
    pub fn flush_to_tables(&self) -> Result<(), StoreError> {
        let predicates = Arc::clone(&self.predicates.read().expect("predicate map lock"));

        for predicate in predicates.values() {
            predicate
                .trees
                .keys
                .rotate_memtable_and_wait()
                .map_err(StoreError::Backend)?;
            predicate
                .trees
                .entities
                .rotate_memtable_and_wait()
                .map_err(StoreError::Backend)?;
        }

        Ok(())
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
    type Item = Result<(ByteView, FactId), StoreError>;

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
pub fn predicate_of(lo: &[u8]) -> Result<u32, StoreError> {
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
        return Err(FactIdError::FactIdSequence {
            sequence: 0,
            max: MAX_FACT_SEQUENCE,
        }
        .into());
    }

    Ok(id)
}

/// `keys` row → `(row bytes, fact id)`.
///
/// The key becomes a `ByteView` by refcount move, never a copy — the register
/// holds the whole row ([I5](../../docs/invariants.md#i5)) and the hot loop
/// allocates nothing per row ([I9](../../docs/invariants.md#i9)).
fn row_to_item(row: fjall::Guard) -> Result<(ByteView, FactId), StoreError> {
    let (key, value) = row.into_inner().map_err(StoreError::Backend)?;
    let fact_id = decode_fact_id(&value)?;
    Ok((ByteView::from(key), fact_id))
}

impl FactStore for FjallStore {
    type Scan = FjallScan;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Result<FjallScan, StoreError> {
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

    fn point(&self, id: FactId) -> Result<Option<Entity>, StoreError> {
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
            return Err(StoreError::TruncatedEntity(id));
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

    use super::*;
    use crate::{
        fixtures::{
            FrozenStore, assert_scan_stays_in_predicate, assert_short_bound_is_rejected, i64_field,
        },
        mem_store::MemStore,
    };
    use aperture_encoding::tuple::strinc;

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
            Some(StoreError::ShortScanBound { .. })
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
            Err(StoreError::Id(FactIdError::PredicateIdTooWide { .. }))
        ));
        assert!(matches!(
            db.create_predicates([too_wide]),
            Err(StoreError::Id(FactIdError::PredicateIdTooWide { .. }))
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
                Err(StoreError::Format(FormatError::Unreadable { .. }))
            ),
            "a database from another format must be refused, not read",
        );

        // Refuse: the same database with the stamp removed — which is exactly what
        // every database written before this invariant existed looks like.
        remove_stamp(dir.path());

        assert!(
            matches!(
                FjallDb::open(dir.path()),
                Err(StoreError::Format(FormatError::Unstamped))
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
                Err(StoreError::Format(
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

        // **A restart must never hand out an id twice.** That is the invariant; where
        // exactly the counter resumes is not, and this used to assert `9` — the sequence
        // straight after the last row. Since 12f the allocator resumes past the durable
        // *claim* instead, which is what makes an id safe to hand out before its fact is
        // committed, and the gap it leaves is a hole this invariant permits. The exact
        // resumption point is pinned by
        // `a_reopened_allocator_resumes_past_what_was_claimed_not_past_what_was_written`;
        // what belongs here is that nothing already issued comes back.
        let db = FjallDb::open(dir.path()).expect("reopen");
        for predicate in predicates {
            let id = db.put_fact(predicate, &[99], &[]).expect("put");
            assert!(
                id.sequence() > 8,
                "a reopened allocator must resume above everything it had issued, got {}",
                id.sequence()
            );
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
        // allocator must resume above what those threads wrote — above the *claim* they
        // were drawn from, since 12f, which is a hole rather than a collision.
        assert_bijection(&db);
        drop(db);
        let db = FjallDb::open(dir.path()).expect("reopen");
        let after = db
            .put_fact(predicate, &[0xff], &[])
            .expect("put")
            .sequence();
        assert!(
            after > 100,
            "a reopened allocator must resume above the 100 sequences four threads took,              got {after}"
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
            Err(StoreError::Id(FactIdError::FactIdSequence {
                sequence: 0,
                ..
            }))
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
                StoreError::Id(FactIdError::FactIdSequence { sequence: 0, .. })
            ),
            "got {fault:?}"
        );
    }

    /// A key is written once, at the **primitive** ([`FjallDb::put_fact`]), where
    /// the contract is the caller's and the check costs a lookup per fact that
    /// bulk ingest will not pay. Writing it twice overwrites the `keys` row and
    /// strands the first fact's entity — invisible to every query, and undetectable
    /// without a bijection check. So it is a debug assertion, and this is the
    /// control proving it is armed.
    ///
    /// [`FjallDb::put`] is the other half of the rule and does not rely on this:
    /// see `put_is_write_once_and_says_so_in_release`.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a key is written once")]
    fn writing_a_key_twice_is_caught_in_debug() {
        let dir = TempDir::new().expect("tempdir");
        let db = FjallDb::open(dir.path()).expect("open");

        db.put_fact(PredicateId(0), &[1, 2], &[]).expect("put");
        let _ = db.put_fact(PredicateId(0), &[1, 2], &[]);
    }

    /// **`put` enforces write-once, in every build.**
    ///
    /// `put_fact` is the bulk primitive and leaves the contract to its caller;
    /// `put` is the documented way to write a fact by hand, and inheriting a bulk
    /// primitive's contract is what made a duplicate key silently orphan a fact in
    /// release. `put` already pays schema resolution and a full encode per fact,
    /// so one point lookup is noise beside what it is already doing.
    ///
    /// The semantics are the merge frontier's, so the two write paths agree
    /// (operations §5): a byte-identical fact **dedups** to the id already there,
    /// and a same-key-different-value fact is **rejected**. Never last-writer-wins
    /// — that is the one outcome an immutable store cannot have, and it is what
    /// unchecked `put_fact` gave.
    ///
    /// Deliberately not `#[cfg(debug_assertions)]`: release is where this was
    /// broken.
    #[test]
    fn put_is_write_once_and_says_so_in_release() {
        use crate::{
            fact::{Fact as _, ToValue, record},
            fixture,
        };
        use aperture_encoding::tuple::Value;

        struct Foo(&'static str);
        impl crate::fact::Fact for Foo {
            const PREDICATE: &'static str = "test.Foo";
            fn key(&self) -> Value {
                record([("id", 1.to_value()), ("name", "ann".to_value())])
            }
            fn value(&self) -> Option<Value> {
                Some(self.0.to_value())
            }
        }
        let _ = Foo::PREDICATE;

        let dir = TempDir::new().expect("tempdir");
        let db = FjallDb::open(dir.path()).expect("open");
        let schema = fixture::schema();

        let first = db.put(&schema, &Foo("one")).expect("the first write");

        // Byte-identical: the id already assigned, and no second fact.
        let again = db
            .put(&schema, &Foo("one"))
            .expect("an identical fact dedups rather than failing");
        assert_eq!(first, again, "an identical fact must not get a second id");

        // Same key, different value: rejected, and nothing written.
        let conflict = db
            .put(&schema, &Foo("two"))
            .expect_err("same key, different value");
        assert!(
            matches!(
                conflict,
                StoreError::KeyAlreadyWritten { existing, .. }
                    if existing == first
            ),
            "got {conflict:?}"
        );

        // One fact, and the two column families still agree about it — which is
        // the property (I12) the orphaned row broke.
        assert_eq!(assert_bijection(&db), 1);
    }

    /// **A sequence that was claimed is never handed out again, even though nothing was
    /// written with it** — [12f-1](../../PLAN.md), and the thing that makes it safe to
    /// return an id before its fact is durable.
    ///
    /// The allocator used to resume from the last `entities` key, which is exact for as
    /// long as every id handed out is already stored. Staged writes break that premise on
    /// purpose, and the failure it opens is not the obvious one: a crash loses a batch,
    /// the allocator resumes *below* an id another writer already referenced and
    /// committed, and that id is then reissued to a different fact. The reference now
    /// resolves — to the wrong target. `finish` cannot catch it, because it checks that
    /// references resolve and not that they resolve to what they meant.
    ///
    /// So this asserts the property that closes it: after a reopen, the next id is past
    /// the whole claimed range, not past the last row. One fact is written here and 1,024
    /// sequences are burned, which is exactly the trade — a hole in the sequence, which
    /// [I11](../../docs/invariants.md#i11) permits, bought against an id meaning two
    /// different things, which it does not.
    #[test]
    fn a_reopened_allocator_resumes_past_what_was_claimed_not_past_what_was_written() {
        let dir = TempDir::new().expect("tempdir");
        let predicate = PredicateId(0);

        let first = {
            let db = FjallDb::open(dir.path()).expect("open");
            db.put_fact(predicate, &[1], &[]).expect("put")
        };
        assert_eq!(first.sequence(), 1, "a fresh predicate starts at 1");

        let db = FjallDb::open(dir.path()).expect("reopen");
        let second = db.put_fact(predicate, &[2], &[]).expect("put");

        assert_eq!(
            second.sequence(),
            RESERVATION_CHUNK + 2,
            "the reopened allocator must skip the whole claim, not resume above the one \
             row that was written"
        );

        // And the claim keeps growing rather than being rewritten from the data.
        let third = db.put_fact(predicate, &[3], &[]).expect("put");
        assert_eq!(third.sequence(), RESERVATION_CHUNK + 3);
    }

    /// One durable write per chunk, not one per fact — otherwise the reservation costs
    /// more than the commits it was introduced to remove.
    #[test]
    fn a_claim_covers_a_chunk_of_ids_rather_than_one() {
        let dir = TempDir::new().expect("tempdir");
        let db = FjallDb::open(dir.path()).expect("open");
        let predicate = PredicateId(0);

        for k in 0..8u8 {
            db.put_fact(predicate, &[k], &[]).expect("put");
        }

        // Eight facts, one claim: reopening resumes past that single chunk.
        drop(db);
        let db = FjallDb::open(dir.path()).expect("reopen");
        assert_eq!(
            db.put_fact(predicate, &[9], &[]).expect("put").sequence(),
            RESERVATION_CHUNK + 2,
            "eight facts must have taken one chunk, not eight"
        );
    }

    /// **[I12](../../docs/invariants.md#i12)'s other half, mechanically: one key names
    /// one fact, however many threads reach for it at once.**
    ///
    /// The guard this invariant could not have. Until [Phase 12d](../../PLAN.md) the
    /// write-once rule was held by *there being one thread* — a property no test can
    /// observe, and the tell that a rule is resting on circumstance rather than a
    /// mechanism. `resolve_or_create` is a read-modify-write: two workers reaching for the
    /// same nested target both find the key absent, both allocate, both write, and one
    /// entity is stranded under a `keys` row the other overwrote. Silent corruption — a
    /// fact no query can reach, and nothing anywhere says so.
    ///
    /// Three assertions, because the failure has three faces and a fix could deliver any
    /// one without the others: every thread must be handed the **same id** for a key
    /// (`ops-I5`'s dedup), exactly one **create** may be reported across all of them, and
    /// the two column families must be in **exact bijection** afterwards (I12 itself).
    ///
    /// Many keys rather than one, and every thread racing every key, so the interleaving
    /// that matters is reached repeatedly rather than hoped for once.
    #[test]
    fn concurrent_interning_of_one_key_creates_one_fact() {
        const THREADS: usize = 8;
        const KEYS: u8 = 64;

        let dir = TempDir::new().expect("tempdir");
        let db = FjallDb::open(dir.path()).expect("open");
        let predicate = PredicateId(0);

        // Built before the race: creating a predicate's trees is ~30 ms and takes a
        // different lock, so leaving it to the first writer would measure that instead.
        db.create_predicates([predicate]).expect("the trees");

        let outcomes: Vec<Vec<Interned>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let db = &db;
                    scope.spawn(move || {
                        (0..KEYS)
                            .map(|k| {
                                db.intern(predicate, &[k], &[], true)
                                    .expect("a well-formed intern")
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a writer thread"))
                .collect()
        });

        for k in 0..usize::from(KEYS) {
            let first = outcomes[0][k].id;
            for (thread, got) in outcomes.iter().enumerate() {
                assert_eq!(
                    got[k].id, first,
                    "thread {thread} was handed a different id for key {k}: one key, one fact"
                );
            }
        }

        let created = outcomes
            .iter()
            .flatten()
            .filter(|interned| interned.created)
            .count();
        assert_eq!(
            created,
            usize::from(KEYS),
            "exactly one writer per key may report a create; the rest must dedup"
        );

        assert_eq!(
            assert_bijection(&db),
            usize::from(KEYS),
            "one fact per key, and both column families agreeing about every one"
        );
    }

    /// **Dedup and reject still work against a cold cache**, which is the branch the
    /// [lookup cache](crate::lookup_cache) took out of every other test's reach.
    ///
    /// The test above now proves the *cached* answers: both of its second writes hit
    /// an entry the first write left behind, so `fact_at`'s own agree and disagree
    /// arms — a live `keys` read, then the comparison — stopped being exercised
    /// anywhere the moment the cache went in. A reopen is what makes them reachable
    /// again: a fresh `FjallDb` has an empty cache and the facts are already on disk.
    /// Without this, the cache could answer everything correctly and the path under it
    /// could be wrong, and the suite would be green.
    #[test]
    fn a_reopened_store_dedups_and_rejects_from_the_trees() {
        use crate::{
            fact::{ToValue, record},
            fixture,
        };
        use aperture_encoding::tuple::Value;

        struct Foo(&'static str);
        impl crate::fact::Fact for Foo {
            const PREDICATE: &'static str = "test.Foo";
            fn key(&self) -> Value {
                record([("id", 7.to_value()), ("name", "cold".to_value())])
            }
            fn value(&self) -> Option<Value> {
                Some(self.0.to_value())
            }
        }

        let dir = TempDir::new().expect("tempdir");
        let schema = fixture::schema();

        let first = {
            let db = FjallDb::open(dir.path()).expect("open");
            db.put(&schema, &Foo("one")).expect("the first write")
        };

        // A different handle, so nothing is remembered: every answer below has to
        // come from the trees.
        let db = FjallDb::open(dir.path()).expect("reopen");

        assert_eq!(
            db.put(&schema, &Foo("one"))
                .expect("an identical fact dedups rather than failing"),
            first,
            "a cold cache must find the fact on disk, not write a second one"
        );

        let conflict = db
            .put(&schema, &Foo("two"))
            .expect_err("same key, different value");
        assert!(
            matches!(
                conflict,
                StoreError::KeyAlreadyWritten { existing, .. }
                    if existing == first
            ),
            "got {conflict:?}"
        );

        // And the miss that populated the cache did not itself write anything.
        assert_eq!(assert_bijection(&db), 1);
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
            Err(FactIdError::FactIdSequence { .. })
        ));
        assert!(
            matches!(
                FactId::new(PredicateId(1), 0),
                Err(FactIdError::FactIdSequence { .. })
            ),
            "sequence 0 is reserved so that FactId(0) is never a fact"
        );
    }

    /// Name of the child test that crashes mid-write for
    /// [`no_half_present_facts`], and the variable carrying it the DB path.
    /// The child's own test path, which is what `--exact` matches on. It moved
    /// when this module became its own crate (`focus::store::tests::…` →
    /// `store::tests::…`), and a stale path is a *passing* child rather than an
    /// error — which the parent then reads as "the crash never happened".
    const CRASH_CHILD: &str = "store::tests::crashing_writer_child_process";
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
