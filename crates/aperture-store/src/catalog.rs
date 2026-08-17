//! **The store root** — databases as artifacts, and the filesystem as the catalog.
//!
//! ```text
//! <root>/
//! ├── .aperture.lock          the root's owner (ops-I1)
//! └── <name>/<instance>/      instance: a provisional ULID
//!     ├── APERTURE_META       the sidecar (ops-I7)
//!     ├── schema/             the embedded schema copy
//!     └── <fjall files>
//! ```
//!
//! # There is no manifest
//!
//! [`ops-I7`](../../../docs/aperture-cli-design.md): enumeration is a walk of the root
//! and a read of each sidecar, and **never opens fjall**. That is what lets `list`
//! work while a server holds every database under the root — the sidecars are
//! ordinary files, and the server's exclusive hold is on the fjall directories.
//!
//! Any index or cache over this must be rebuildable from a scan and never
//! authoritative. There isn't one, and this note is why there shouldn't be.
//!
//! # Creation is all-or-nothing
//!
//! A database is built under a scratch name at the root and **renamed into place**
//! once it is complete. A rename within one directory is atomic, so a process killed
//! at any point leaves either nothing under `<name>` or a finished Writable database —
//! never a half-built one for [`list`](Catalog::list) to find and report as real.
//!
//! The scratch directory is removed on every failure path by
//! [`Scratch`](crate::meta::Scratch). A hard kill can leave one behind; it starts with
//! a dot, so the scan skips it, and it is inert rather than misleading.

use std::{
    fs,
    path::{Path, PathBuf},
};

use aperture_schema::{
    fingerprint,
    schema::{PredicateId, Schema},
};

use crate::{
    error::StoreError,
    identity,
    meta::{Meta, Scratch, Status, sync_dir},
    store::FjallDb,
    ulid,
};

/// The lock file naming the root's owner.
pub const LOCK_FILE: &str = ".aperture.lock";

/// The prefix a half-built database carries while it is being built.
const SCRATCH_PREFIX: &str = ".create-";

/// The prefix a database being removed carries, between the rename and the delete.
const TRASH_PREFIX: &str = ".trash-";

/// One database found in a store root.
#[derive(Debug, Clone)]
pub struct Entry {
    pub meta: Meta,
    /// The instance directory — what to open.
    pub path: PathBuf,
}

impl Entry {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.meta.name
    }

    #[must_use]
    pub fn status(&self) -> Status {
        self.meta.status
    }
}

/// What sealing a database came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// `hash(canonical schema, base facts)` — the database's identity (`ops-I4`).
    pub fingerprint: u64,
    pub facts: u64,
    pub bytes: u64,
    /// Whether it was already sealed, in which case nothing was done.
    pub already_complete: bool,
}

/// What a scan of the root found, and what it could not make sense of.
///
/// Problems are **collected rather than raised**, and that is an operator decision:
/// one database with a malformed sidecar must not make `list` unable to show the
/// other nine. The caller reports them alongside the listing.
#[derive(Debug, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub problems: Vec<StoreError>,
}

/// A store root.
///
/// Cheap and stateless — it holds a path. Ownership of the root is
/// [`lock`](Catalog::lock)'s business, and deliberately separate: reading a listing
/// requires no ownership at all, which is the whole of `ops-I7`.
#[derive(Debug, Clone)]
pub struct Catalog {
    root: PathBuf,
}

impl Catalog {
    /// Open the store root at `root`, creating the directory if it is absent.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if the directory cannot be created or is not a directory.
    pub fn open(root: impl AsRef<Path>) -> Result<Catalog, StoreError> {
        let root = root.as_ref().to_path_buf();

        fs::create_dir_all(&root).map_err(|source| StoreError::Meta {
            path: root.clone(),
            detail: format!("cannot create the store root: {source}"),
        })?;

        if !root.is_dir() {
            return Err(StoreError::Meta {
                path: root,
                detail: "the store root is not a directory".to_owned(),
            });
        }

        Ok(Catalog { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Take exclusive ownership of the root (`ops-I1`).
    ///
    /// # Errors
    ///
    /// [`StoreError::RootHeld`] if another process holds it. Deliberately not a wait:
    /// the design refuses a lock fight, because the alternative to failing here is
    /// two servers writing one directory.
    pub fn lock(&self) -> Result<RootLock, StoreError> {
        RootLock::acquire(&self.root)
    }

    /// Every database under the root, by reading sidecars only.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] only if the root itself cannot be read; a database that
    /// cannot be understood lands in [`Listing::problems`].
    pub fn list(&self) -> Result<Listing, StoreError> {
        let mut listing = Listing::default();

        for name_entry in self.read_dir(&self.root)? {
            let name_path = name_entry.path();

            // Dot-prefixed names are ours: the lock, a scratch build, a pending
            // delete. A database can never be called one, because `check_name`
            // refuses a leading dot.
            let Some(name) = name_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.') || !name_path.is_dir() {
                continue;
            }

            for instance_entry in self.read_dir(&name_path)? {
                let path = instance_entry.path();

                let Some(instance) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };

                // A directory that is not an instance id is not a database, and a
                // store root is a filesystem — anything can appear in one.
                if !ulid::is_valid(instance) || !path.is_dir() {
                    continue;
                }

                // Absent sidecar: not a database, skip. Present but unreadable: a
                // database that is *broken*, which is worth telling someone about.
                if !path.join(crate::meta::META_FILE).exists() {
                    continue;
                }

                match Meta::read(&path) {
                    Ok(meta) => listing.entries.push(Entry { meta, path }),
                    Err(problem) => listing.problems.push(problem),
                }
            }
        }

        listing
            .entries
            .sort_by(|a, b| (a.name(), &a.meta.instance).cmp(&(b.name(), &b.meta.instance)));

        Ok(listing)
    }

    /// The database called `name`, if the root holds one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if the root cannot be read.
    pub fn find(&self, name: &str) -> Result<Option<Entry>, StoreError> {
        Ok(self
            .list()?
            .entries
            .into_iter()
            .find(|entry| entry.name() == name))
    }

    /// The database called `name`, or an error naming it.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchDatabase`] if there is none.
    pub fn get(&self, name: &str) -> Result<Entry, StoreError> {
        self.find(name)?
            .ok_or_else(|| StoreError::NoSuchDatabase(name.to_owned()))
    }

    /// Create a Writable database called `name`, against `schema`.
    ///
    /// Materialises **every** predicate's trees up front rather than on first write:
    /// a keyspace costs about 30 ms, and a database created from a schema knows all
    /// of them, so the bill is paid once here instead of at an unpredictable point
    /// inside an ingest ([chapter 3](../../../docs/03-storage-model.md)).
    ///
    /// # Errors
    ///
    /// [`StoreError::BadDatabaseName`], [`StoreError::DatabaseExists`], or whatever
    /// the store or the sidecar reports. On any of them nothing is left behind.
    pub fn create(&self, name: &str, schema: &Schema) -> Result<Entry, StoreError> {
        check_name(name)?;

        // **Derived here rather than passed in.** A caller handing over both a schema
        // and a number could hand over two that disagree, and the sidecar would then
        // record an identity for a schema this database does not hold — which nothing
        // downstream could detect, since a fingerprint is exactly what everything
        // downstream trusts.
        let schema_fingerprint = fingerprint::of(schema);

        let destination = self.root.join(name);
        if destination.exists() {
            return Err(StoreError::DatabaseExists(name.to_owned()));
        }

        let instance = ulid::new();
        let scratch = Scratch::new(self.root.join(format!("{SCRATCH_PREFIX}{instance}")));
        let built = scratch.path().join(&instance);

        fs::create_dir_all(&built).map_err(|source| StoreError::Meta {
            path: built.clone(),
            detail: format!("cannot create: {source}"),
        })?;

        // The store first, because it is the part that can fail for interesting
        // reasons — a format this build cannot read, a directory it cannot own.
        {
            let db = FjallDb::open(&built)?;

            // **Every predicate but the virtual ones.** A virtual predicate is
            // answered by whoever runs the query rather than read from a keyspace, so
            // making it a pair of trees would cost the ~30 ms a keyspace costs and
            // leave two empty ones in the artifact forever, saying that a database
            // holds a kind of fact that nothing can ever write to it.
            db.create_predicates(
                (0..schema.len())
                    .map(|index| PredicateId(index as u32))
                    .filter(|id| !schema.is_virtual(*id)),
            )?;
        }

        crate::schema_doc::write(&built, schema)?;

        let meta = Meta::new(name, &instance, schema_fingerprint);
        meta.write(&built)?;

        // Durable before the rename, so the rename cannot expose a database whose
        // contents have not reached the disk.
        sync_dir(&built)
            .and_then(|()| sync_dir(scratch.path()))
            .map_err(|source| StoreError::Meta {
                path: built.clone(),
                detail: format!("cannot sync: {source}"),
            })?;

        fs::rename(scratch.path(), &destination).map_err(|source| StoreError::Meta {
            path: destination.clone(),
            detail: format!("cannot move into place: {source}"),
        })?;
        scratch.keep();

        sync_dir(&self.root).map_err(|source| StoreError::Meta {
            path: self.root.clone(),
            detail: format!("cannot sync the store root: {source}"),
        })?;

        Ok(Entry {
            meta,
            path: destination.join(&instance),
        })
    }

    /// Open `name` for writing.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotWritable`] unless the database is
    /// [`Writable`](Status::Writable) — `ops-I2` refuses at establishment, so that
    /// immutability is the absence of a handle rather than a check on every write.
    pub fn open_write(&self, name: &str) -> Result<(Entry, FjallDb), StoreError> {
        let entry = self.get(name)?;

        if !entry.status().is_writable() {
            return Err(StoreError::NotWritable {
                name: name.to_owned(),
                status: entry.status(),
            });
        }

        let db = FjallDb::open(&entry.path)?;
        Ok((entry, db))
    }

    /// Open `name` for reading. Any status a query can be run against is allowed.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchDatabase`], or whatever opening the store reports.
    pub fn open_read(&self, name: &str) -> Result<(Entry, FjallDb), StoreError> {
        let entry = self.get(name)?;
        let db = FjallDb::open(&entry.path)?;
        Ok((entry, db))
    }

    /// Seal `name`: `Writable → Complete`.
    ///
    /// # The order is `ops-I3`, and the last step is the only one that matters
    ///
    /// 1. flush and `SyncAll` — every fact durable before anything claims they are;
    /// 2. compute the content identity (`ops-I4`);
    /// 3. one atomic sidecar write carrying the identity, the counts **and** the flip.
    ///
    /// Operations §5 describes 2 and 3 as "record it in the sidecar → atomically flip
    /// status to Complete as the final durable act", which reads like two writes.
    /// **One is the correct reading and the safer one.** Two would leave a window in
    /// which a database is Writable *and* carries a content fingerprint — a
    /// fingerprint that another write would immediately invalidate. One rename means
    /// a crash leaves the old sidecar exactly as it was: Writable, no identity,
    /// re-runnable. The sidecar write is the final durable act either way, which is
    /// what `ops-I3` actually requires.
    ///
    /// Finishing a Complete database is a no-op with [`Finished::already_complete`]
    /// set, rather than an error: a re-run after a crash cannot tell whether it is the
    /// re-run or the original, and both should succeed.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchDatabase`]; [`StoreError::NotWritable`] if it is `Broken`;
    /// [`StoreError::EmptyDatabase`] for a database with no facts unless
    /// `allow_zero_facts`; and whatever the store or the identity walk reports.
    pub fn finish(
        &self,
        name: &str,
        schema: &Schema,
        allow_zero_facts: bool,
    ) -> Result<Finished, StoreError> {
        let entry = self.get(name)?;

        if let Some(already) = sealable(name, &entry)? {
            return Ok(already);
        }

        let db = FjallDb::open(&entry.path)?;
        let identity = seal(name, &entry, &db, schema, allow_zero_facts)?;

        // Dropped before the sidecar write for the same reason the sync came first:
        // nothing should be holding a *writable* handle when the database becomes
        // immutable. The offline path can say that by closing the store; the server
        // path says it by sealing inside the per-database writer lock, which is what
        // [`finish_held`](Catalog::finish_held) is for.
        drop(db);

        record(&entry, identity)
    }

    /// Seal `name` when **this process already holds it open**.
    ///
    /// The server owns every database under its root (`ops-I1`), so the offline
    /// [`finish`](Catalog::finish)'s first act — open the directory — is the one thing
    /// it cannot do: that is a second handle on a store this process is already
    /// holding. It passes the handle it has instead, and everything after the open is
    /// the same code, in the same `ops-I3` order.
    ///
    /// The caller is responsible for the other half of `ops-I2`: no write may be in
    /// flight, and none may start afterwards. `aperture-server` gets both from the
    /// per-database writer lock, which it holds across this call and seals inside.
    ///
    /// # Errors
    ///
    /// Exactly [`finish`](Catalog::finish)'s.
    pub fn finish_held(
        &self,
        name: &str,
        db: &FjallDb,
        schema: &Schema,
        allow_zero_facts: bool,
    ) -> Result<Finished, StoreError> {
        let entry = self.get(name)?;

        if let Some(already) = sealable(name, &entry)? {
            return Ok(already);
        }

        let identity = seal(name, &entry, db, schema, allow_zero_facts)?;
        record(&entry, identity)
    }

    /// Delete `name`.
    ///
    /// Renamed out of the way first, then removed: the rename is atomic and is what
    /// makes the database disappear from a listing at one instant, rather than
    /// dissolving file by file while somebody is walking the root.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchDatabase`], or [`StoreError::Meta`] if the removal fails.
    pub fn remove(&self, name: &str) -> Result<(), StoreError> {
        let entry = self.get(name)?;

        let live = self.root.join(name);
        let trash = self
            .root
            .join(format!("{TRASH_PREFIX}{}", entry.meta.instance));

        fs::rename(&live, &trash).map_err(|source| StoreError::Meta {
            path: live,
            detail: format!("cannot remove: {source}"),
        })?;

        fs::remove_dir_all(&trash).map_err(|source| StoreError::Meta {
            path: trash,
            detail: format!("cannot delete: {source}"),
        })?;

        Ok(())
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<fs::DirEntry>, StoreError> {
        let listing = fs::read_dir(path).map_err(|source| StoreError::Meta {
            path: path.to_path_buf(),
            detail: format!("cannot list: {source}"),
        })?;

        listing
            .map(|entry| {
                entry.map_err(|source| StoreError::Meta {
                    path: path.to_path_buf(),
                    detail: format!("cannot read an entry: {source}"),
                })
            })
            .collect()
    }
}

/// The status gate both sealing paths share.
///
/// `Ok(None)` means go ahead; `Ok(Some(_))` is the already-Complete no-op.
fn sealable(name: &str, entry: &Entry) -> Result<Option<Finished>, StoreError> {
    match entry.status() {
        Status::Complete => Ok(Some(Finished {
            fingerprint: entry.meta.content_fingerprint.unwrap_or(0),
            facts: entry.meta.facts.unwrap_or(0),
            bytes: entry.meta.bytes.unwrap_or(0),
            already_complete: true,
        })),
        Status::Broken => Err(StoreError::NotWritable {
            name: name.to_owned(),
            status: Status::Broken,
        }),
        Status::Writable => Ok(None),
    }
}

/// Steps 1 and 2 of `ops-I3`: make it durable, **merge it**, then work out what it is.
///
/// Everything here reads the disk, so the caller may hand over a handle it already
/// holds — which is the whole difference between the two public paths.
fn seal(
    name: &str,
    entry: &Entry,
    db: &FjallDb,
    schema: &Schema,
    allow_zero_facts: bool,
) -> Result<identity::Identity, StoreError> {
    // Durable first. Everything after this reads what is already on the disk, so an
    // identity computed here describes bytes that survive a power loss.
    db.persist()?;

    // Then merge, and merge *here* — before the walk, so the identity is computed over
    // the tree that will actually be shipped, and before `record`, so the byte count it
    // writes down is the artifact's rather than the ingest's. What this reclaims is
    // read cost, paid per page by every future query
    // ([`FjallDb::compact`](FjallDb::compact) says how much); a `Complete` database is
    // immutable, so this is the last moment the shape can be chosen and the only one at
    // which choosing it is not premature.
    //
    // Not conditional on `allow_zero_facts`: an empty database has nothing to merge and
    // merging it costs nothing, so the check stays where it reads best.
    db.compact()?;

    let identity = identity::compute(db, schema, entry.meta.schema_fingerprint)?;

    // A silently-empty sealed artifact is the classic CI failure that looks like
    // success, so it takes a flag to make one.
    if identity.facts == 0 && !allow_zero_facts {
        return Err(StoreError::EmptyDatabase(name.to_owned()));
    }

    Ok(identity)
}

/// Step 3 of `ops-I3`: **one** atomic sidecar write, carrying the identity, the counts
/// and the flip — and the last durable act either path performs.
fn record(entry: &Entry, identity: identity::Identity) -> Result<Finished, StoreError> {
    // Measured after the sync, so it counts what is actually there.
    let bytes = identity::directory_size(&entry.path);

    let mut meta = entry.meta.clone();
    meta.status = Status::Complete;
    meta.content_fingerprint = Some(identity.fingerprint);
    meta.facts = Some(identity.facts);
    meta.bytes = Some(bytes);
    meta.write(&entry.path)?;

    Ok(Finished {
        fingerprint: identity.fingerprint,
        facts: identity.facts,
        bytes,
        already_complete: false,
    })
}

/// Whether `name` can be a database.
///
/// The rules are about the filesystem rather than about taste: a name becomes a
/// directory directly under the store root, so anything that could escape it, collide
/// with the catalog's own dot-prefixed entries, or fail to be a filename is refused.
fn check_name(name: &str) -> Result<(), StoreError> {
    let bad = |detail| {
        Err(StoreError::BadDatabaseName {
            name: name.to_owned(),
            detail,
        })
    };

    if name.is_empty() {
        return bad("it is empty");
    }
    if name.starts_with('.') {
        return bad("a leading dot is reserved for the catalog's own entries");
    }
    if name.contains(['/', '\\']) {
        return bad("it contains a path separator");
    }
    if name.contains(|c: char| c.is_control()) {
        return bad("it contains a control character");
    }
    if name.len() > 255 {
        return bad("it is longer than a filename may be");
    }

    Ok(())
}

/// Exclusive ownership of a store root, held for as long as this value lives.
///
/// `flock`, which is what makes the hold **end when the process does** — including
/// when it is killed. A lock file holding a pid would need liveness checks and would
/// still be wrong after a pid is reused; the kernel already knows who is alive.
#[derive(Debug)]
pub struct RootLock {
    file: fs::File,
    root: PathBuf,
}

impl RootLock {
    fn acquire(root: &Path) -> Result<RootLock, StoreError> {
        let path = root.join(LOCK_FILE);

        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| StoreError::Meta {
                path: path.clone(),
                detail: format!("cannot open the lock file: {source}"),
            })?;

        // SAFETY: `fd` is owned by `file` and outlives the call.
        let taken = unsafe {
            use std::os::fd::AsRawFd;
            libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
        };

        if taken != 0 {
            let error = std::io::Error::last_os_error();

            // `EWOULDBLOCK` is "somebody else has it", which is the answer this is
            // for. Anything else is a real I/O fault and should not be reported as
            // contention.
            return if error.kind() == std::io::ErrorKind::WouldBlock {
                Err(StoreError::RootHeld {
                    root: root.to_path_buf(),
                })
            } else {
                Err(StoreError::Meta {
                    path,
                    detail: format!("cannot lock: {error}"),
                })
            };
        }

        Ok(RootLock {
            file,
            root: root.to_path_buf(),
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for RootLock {
    fn drop(&mut self) {
        // SAFETY: `fd` is owned by `file` and outlives the call.
        unsafe {
            use std::os::fd::AsRawFd;
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
