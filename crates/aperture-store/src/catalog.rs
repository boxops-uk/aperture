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
//! # A name is a container, and an instance is a database
//!
//! `<name>` holds one directory per **instance**, and `create` adds one rather than
//! conflicting: a database-per-CI-run needs somewhere to go. This is the
//! [Glean `Repo`](../../../docs/glean-capabilities.md) shape — a name plus a version of
//! it — with a generated [ULID](crate::ulid) where Glean takes a caller-supplied
//! revision.
//!
//! Which instance an unqualified name means is [`Intent`]'s answer and depends on what
//! the caller is about to do, because the cost of being wrong differs: a read ranks the
//! candidates and commits, a write or a delete refuses and names them. `name@instance`
//! (or any unambiguous prefix of the instance) says exactly which, and `@` is why a name
//! may not contain one.
//!
//! # Creation is all-or-nothing
//!
//! A database is built under a scratch name at the root and the finished **instance
//! directory is renamed into the name**. A rename is atomic, so a process killed at any
//! point leaves either no instance directory or a finished Writable one — never a
//! half-built one for [`list`](Catalog::list) to find and report as real. An empty
//! `<name>` directory can survive a failure and is inert: the scan looks for instance
//! ids inside it and finds none.
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

/// The character separating a database's name from an instance of it.
///
/// `@` rather than `:` or `/`: a colon collides with `host:port` in an address and a
/// slash with the path of a URI, and `@` survives inside both
/// (`aperture://host:port/code@01JQ8F`). It is also the separator Docker tags and Go
/// module versions trained everyone on, and it reads as *at this version of*.
pub const INSTANCE_SEPARATOR: char = '@';

/// Which database a caller means: a name, and optionally which instance of it.
///
/// This is the [Glean `Repo`](../../../docs/glean-capabilities.md) shape — a name plus a
/// version of it — with one deliberate difference. Glean's second component is a
/// caller-supplied hash, usually the revision indexed; ours is a generated
/// [ULID](crate::ulid), so it is opaque and orders by creation time. Both systems order
/// instances by a *recorded timestamp* rather than by the id itself, which is why the id
/// being opaque costs nothing.
///
/// An absent instance does not mean "any": it means *let the operation decide*, which is
/// [`Intent`]'s job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    name: String,
    instance: Option<String>,
}

impl Selector {
    /// `name`, with the instance left to [`Intent`].
    #[must_use]
    pub fn of(name: impl Into<String>) -> Selector {
        Selector {
            name: name.into(),
            instance: None,
        }
    }

    /// `name`, at the instance whose id starts with `instance`.
    #[must_use]
    pub fn at(name: impl Into<String>, instance: impl Into<String>) -> Selector {
        Selector {
            name: name.into(),
            instance: Some(instance.into()),
        }
    }

    /// Parse `name` or `name@instance`.
    ///
    /// # Errors
    ///
    /// [`StoreError::BadDatabaseName`] for an empty half or a second separator. The
    /// error carries the whole text rather than the piece at fault, because the whole
    /// text is what somebody typed.
    pub fn parse(text: &str) -> Result<Selector, StoreError> {
        let bad = |detail| {
            Err(StoreError::BadDatabaseName {
                name: text.to_owned(),
                detail,
            })
        };

        let mut halves = text.split(INSTANCE_SEPARATOR);
        let name = halves.next().unwrap_or_default();
        let instance = halves.next();

        if halves.next().is_some() {
            return bad("it names an instance twice");
        }
        if name.is_empty() {
            return bad("it names no database");
        }
        match instance {
            // `code@` asked for an instance and then did not name one. Treating it as
            // `code` would answer a question that was not asked.
            Some("") => bad("it ends in `@` without naming an instance"),
            Some(instance) => Ok(Selector::at(name, instance)),
            None => Ok(Selector::of(name)),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The instance id, or the prefix of one, if the caller named it.
    #[must_use]
    pub fn instance(&self) -> Option<&str> {
        self.instance.as_deref()
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        if let Some(instance) = &self.instance {
            write!(f, "{INSTANCE_SEPARATOR}{instance}")?;
        }
        Ok(())
    }
}

/// What a caller means to do, which is what decides which instance an unqualified name
/// picks out.
///
/// The distinction is about what being wrong costs. Reading the second-best instance
/// answers oddly and is recoverable, so a read **ranks** and commits. Writing to the
/// wrong half-built database, or deleting the wrong one, is not recoverable, so both
/// **refuse** and name the candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// A query, or anything descriptive. Ranked sealed-before-unsealed, then
    /// newest-first, and the best is taken — never ambiguous.
    ///
    /// Sealed outranks newer on purpose: while a CI run builds instance *n*, queries
    /// belong to the sealed instance *n-1*, not to the half-written one. The
    /// newest-first tiebreak is what keeps a root holding a single unsealed database
    /// readable by name, which is what it was before instances existed.
    Read,
    /// A write or a seal: the one [`Writable`](Status::Writable) instance.
    Write,
    /// Something destructive: exactly one instance, or a refusal naming them all.
    Sole,
}

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

    /// The selector naming *exactly* this instance.
    ///
    /// What a caller resolved an unqualified name with once and wants to keep hold of:
    /// re-resolving the same name later can land somewhere else, because a `create` or
    /// a `finish` in between changes the ranking.
    #[must_use]
    pub fn selector(&self) -> Selector {
        Selector::at(self.name(), &self.meta.instance)
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

        // **In resolution order**, which is why `candidates` can filter this without
        // sorting again: the first row `list` shows for a name is the one an unqualified
        // read of that name binds. Sorting by instance ascending — the obvious order,
        // and what this did — put the *oldest* first and quietly contradicted that.
        listing.entries.sort_by(resolution_order);

        Ok(listing)
    }

    /// Every instance of `name`, **best first** — sealed before unsealed, then newest
    /// before oldest.
    ///
    /// The order is [`Intent::Read`]'s rule, stated once here so that a caller wanting
    /// to *show* the candidates lists them in the same order resolution would pick from.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if the root cannot be read.
    pub fn candidates(&self, name: &str) -> Result<Vec<Entry>, StoreError> {
        Ok(self
            .list()?
            .entries
            .into_iter()
            .filter(|entry| entry.name() == name)
            .collect())
    }

    /// The best instance of `name`, or `None` if the root holds none.
    ///
    /// The "does this root hold a `code` at all" question, which is a different one from
    /// [`resolve`](Catalog::resolve)'s "which `code` do I mean" and is worth being able
    /// to ask without choosing an [`Intent`]. Ranked the same way, so the answer is the
    /// one an [`Intent::Read`] would have bound.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if the root cannot be read.
    pub fn find(&self, name: &str) -> Result<Option<Entry>, StoreError> {
        Ok(self.candidates(name)?.into_iter().next())
    }

    /// The one database `selector` names, under `intent`'s rule for an unqualified name.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchDatabase`] when the name holds nothing;
    /// [`StoreError::NoSuchInstance`] when a named instance matches nothing;
    /// [`StoreError::AmbiguousDatabase`] when the choice is the caller's to make;
    /// [`StoreError::NotWritable`] when [`Intent::Write`] finds only sealed instances.
    ///
    /// Note what this does *not* check: a named instance is returned whatever its
    /// status, even under [`Intent::Write`]. `finish` has to be able to tell an
    /// already-sealed database from an unwritable one, and that is a distinction only
    /// the caller holding the entry can draw.
    pub fn resolve(&self, selector: &Selector, intent: Intent) -> Result<Entry, StoreError> {
        let candidates = self.candidates(selector.name())?;
        if candidates.is_empty() {
            return Err(StoreError::NoSuchDatabase(selector.name().to_owned()));
        }

        let ambiguous = |among: &[Entry]| StoreError::AmbiguousDatabase {
            name: selector.name().to_owned(),
            instances: among
                .iter()
                .map(|entry| entry.meta.instance.clone())
                .collect(),
        };

        // Exactly one, or a refusal naming them all.
        let sole = |among: Vec<Entry>| match among.len() {
            1 => Ok(among.into_iter().next().expect("one candidate")),
            _ => Err(ambiguous(&among)),
        };

        if let Some(prefix) = selector.instance() {
            // Crockford base32 is case-insensitive by construction, so a prefix typed in
            // either case selects the same instance.
            let wanted = prefix.to_ascii_uppercase();
            let matched: Vec<Entry> = candidates
                .into_iter()
                .filter(|entry| {
                    entry
                        .meta
                        .instance
                        .to_ascii_uppercase()
                        .starts_with(&wanted)
                })
                .collect();

            return match matched.len() {
                0 => Err(StoreError::NoSuchInstance {
                    name: selector.name().to_owned(),
                    instance: prefix.to_owned(),
                }),
                1 => Ok(matched.into_iter().next().expect("one match")),
                _ => Err(ambiguous(&matched)),
            };
        }

        match intent {
            // Ranked, so there is always a best and never a question.
            Intent::Read => Ok(candidates.into_iter().next().expect("a candidate")),

            Intent::Write => {
                let writable: Vec<Entry> = candidates
                    .iter()
                    .filter(|entry| entry.status().is_writable())
                    .cloned()
                    .collect();

                match writable.len() {
                    1 => Ok(writable.into_iter().next().expect("one writable")),
                    _ if writable.len() > 1 => Err(ambiguous(&writable)),
                    // **No writable instance falls through to the sole rule rather than
                    // failing here**, and that is what keeps `finish` idempotent: a
                    // re-run after a crash finds one Complete instance and has to be
                    // able to see it in order to answer "already sealed" instead of
                    // "not writable". Whether a sealed instance is an error is the
                    // caller's question, and `open_write` and `finish` answer it
                    // differently.
                    _ => sole(candidates),
                }
            }

            Intent::Sole => sole(candidates),
        }
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
    /// [`StoreError::BadDatabaseName`], or whatever
    /// the store or the sidecar reports. On any of them nothing is left behind.
    pub fn create(&self, name: &str, schema: &Schema) -> Result<Entry, StoreError> {
        check_name(name)?;

        // **Derived here rather than passed in.** A caller handing over both a schema
        // and a number could hand over two that disagree, and the sidecar would then
        // record an identity for a schema this database does not hold — which nothing
        // downstream could detect, since a fingerprint is exactly what everything
        // downstream trusts.
        let schema_fingerprint = fingerprint::of(schema);

        // **Before anything exists on disk: can this schema be written down and read
        // back?** The copy under `schema/` is what the database will be served with, so
        // a schema that does not survive the round trip is a database that cannot be
        // opened — or worse, one opened with its predicates at different positions,
        // which reads every stored row through the wrong type. Checking here costs one
        // parse of a file that is about to be written anyway, and turns a silent
        // corruption into a refusal with nothing left behind.
        recoverable(schema).map_err(|detail| StoreError::UnwritableSchema {
            name: name.to_owned(),
            detail,
        })?;

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

        // **The name directory, which may already hold other instances.** Created here
        // rather than checked for: a name is a container, and `create` adding the second
        // instance to it is the ordinary case rather than a conflict. If something that
        // is not a directory is already at that path, this is where it is reported.
        let home = self.root.join(name);
        fs::create_dir_all(&home).map_err(|source| StoreError::Meta {
            path: home.clone(),
            detail: format!("cannot create the name directory: {source}"),
        })?;

        // Still one atomic rename, just one level deeper than it used to be: the
        // instance directory moves out of the scratch and into the name. A process
        // killed at any point leaves either no instance directory or a whole one, and
        // an empty name directory is invisible to [`list`](Catalog::list) because it
        // holds nothing that parses as an instance id.
        let destination = home.join(&instance);
        fs::rename(&built, &destination).map_err(|source| StoreError::Meta {
            path: destination.clone(),
            detail: format!("cannot move into place: {source}"),
        })?;

        // Deliberately not `scratch.keep()`: what used to be kept was the scratch
        // directory itself, because it *became* the name directory. Now the instance is
        // moved out from under it and the empty scratch is ours to remove, which is what
        // dropping it does.
        drop(scratch);

        sync_dir(&home)
            .and_then(|()| sync_dir(&self.root))
            .map_err(|source| StoreError::Meta {
                path: self.root.clone(),
                detail: format!("cannot sync the store root: {source}"),
            })?;

        Ok(Entry {
            meta,
            path: destination,
        })
    }

    /// Open `name` for writing.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotWritable`] unless the database is
    /// [`Writable`](Status::Writable) — `ops-I2` refuses at establishment, so that
    /// immutability is the absence of a handle rather than a check on every write.
    pub fn open_write(&self, selector: &Selector) -> Result<(Entry, FjallDb), StoreError> {
        let entry = self.resolve(selector, Intent::Write)?;

        if !entry.status().is_writable() {
            return Err(StoreError::NotWritable {
                name: entry.name().to_owned(),
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
    pub fn open_read(&self, selector: &Selector) -> Result<(Entry, FjallDb), StoreError> {
        let entry = self.resolve(selector, Intent::Read)?;
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
        selector: &Selector,
        schema: &Schema,
        allow_zero_facts: bool,
    ) -> Result<Finished, StoreError> {
        let entry = self.resolve(selector, Intent::Write)?;
        let name = entry.name().to_owned();

        if let Some(already) = sealable(&name, &entry)? {
            return Ok(already);
        }

        let db = FjallDb::open(&entry.path)?;
        let identity = seal(&name, &entry, &db, schema, allow_zero_facts)?;

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
        selector: &Selector,
        db: &FjallDb,
        schema: &Schema,
        allow_zero_facts: bool,
    ) -> Result<Finished, StoreError> {
        let entry = self.resolve(selector, Intent::Write)?;
        let name = entry.name().to_owned();

        if let Some(already) = sealable(&name, &entry)? {
            return Ok(already);
        }

        let identity = seal(&name, &entry, db, schema, allow_zero_facts)?;
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
    pub fn remove(&self, selector: &Selector) -> Result<(), StoreError> {
        let entry = self.resolve(selector, Intent::Sole)?;

        let trash = self
            .root
            .join(format!("{TRASH_PREFIX}{}", entry.meta.instance));

        fs::rename(&entry.path, &trash).map_err(|source| StoreError::Meta {
            path: entry.path.clone(),
            detail: format!("cannot remove: {source}"),
        })?;

        fs::remove_dir_all(&trash).map_err(|source| StoreError::Meta {
            path: trash,
            detail: format!("cannot delete: {source}"),
        })?;

        // **And the name directory, if that was the last instance under it.**
        // `remove_dir` succeeds only on an empty directory, which is exactly the
        // condition being tested for — so the failure when siblings remain is the
        // answer rather than a fault, and is deliberately dropped. Any other failure
        // is dropped too: the instance is already gone, so the operation succeeded,
        // and an empty name directory is invisible to `list`.
        let _ = fs::remove_dir(self.root.join(entry.name()));

        sync_dir(&self.root).map_err(|source| StoreError::Meta {
            path: self.root.clone(),
            detail: format!("cannot sync the store root: {source}"),
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

/// Whether `schema` survives being written down and read back at the same positions.
///
/// The check `create` makes before anything exists. Two ways to fail, and both are
/// about a schema that was **built rather than parsed**: a name the language cannot
/// spell (a predicate with no namespace, a field that is not a field name), and a
/// numbering that is not the one lowering would recover. Everything that came from a
/// `.aps` file passes by construction, which is why this reads as paranoia and is not:
/// `Schema` is a public type, and the failure it prevents is silent.
fn recoverable(schema: &Schema) -> Result<(), String> {
    let text = aperture_schema::syntax::print::print(schema);
    let back = aperture_schema::syntax::recover(crate::schema_doc::SCHEMA_FILE, &text)?;

    if aperture_schema::syntax::print::equivalent(schema, &back) {
        return Ok(());
    }

    Err(format!(
        "written back, it is a different schema — this one holds predicates that \
         cannot be spelled, or is numbered in an order lowering does not recover:\n{text}"
    ))
}

/// Whether `name` can be a database.
///
/// The rules are about the filesystem rather than about taste: a name becomes a
/// directory directly under the store root, so anything that could escape it, collide
/// with the catalog's own dot-prefixed entries, or fail to be a filename is refused.
/// The order a name's instances are both **listed** and **resolved** in.
///
/// Names ascending, so a listing reads alphabetically. Within a name: sealed before
/// unsealed before broken, then newest before oldest — which is [`Intent::Read`]'s rule,
/// and the reason it is written here rather than there is that a listing showing a
/// different order than resolution uses is a listing that misleads.
///
/// `Broken` sorts last because it is readable only if it can be, so it is the last thing
/// an unqualified name should land on.
fn resolution_order(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    let rank = |status: Status| match status {
        Status::Complete => 0,
        Status::Writable => 1,
        Status::Broken => 2,
    };

    a.name()
        .cmp(b.name())
        .then_with(|| rank(a.status()).cmp(&rank(b.status())))
        // Descending: a ULID's leading 48 bits are a millisecond timestamp, so reversing
        // the id order is reversing chronology.
        .then_with(|| b.meta.instance.cmp(&a.meta.instance))
}

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
    if name.contains(INSTANCE_SEPARATOR) {
        return bad("`@` separates a name from an instance, so a name may not contain one");
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
