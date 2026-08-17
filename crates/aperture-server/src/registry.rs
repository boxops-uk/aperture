//! **What the server owns**: a store root, and every database under it.
//!
//! Until 9d the server was handed a `Vec<Arc<Database>>` opened once at startup, and
//! that was the whole reason lifecycle commands had to be refused while it ran: a
//! `create` cannot add to a list nobody holds, and a `remove` cannot delete a directory
//! this process has open. The registry is the mutable form of that list, plus the
//! [`Catalog`] the CLI's offline path already uses — which is what makes
//! [operations §5](../../../docs/aperture-cli-design.md)'s "two front doors, one
//! implementation" true rather than aspirational. Everything below delegates the actual
//! work to `aperture-store`; what lives here is *when* it is safe to do it.
//!
//! # The two hazards, and where each is answered
//!
//! **A second handle on a store this process already holds.** `ops-I1` gives the server
//! every database under its root, so the offline `finish`'s first act — open the
//! directory — is exactly what the server must not do. It passes the handle it has:
//! [`Catalog::finish_held`].
//!
//! **A database pulled out from under a session.** `remove` closes the store, and a
//! query running against a closed store is a fault the client did not cause. So a
//! database is taken out of the map *first* — no new session can bind it — and removed
//! only if this registry turns out to hold the last reference. If a session still has
//! it, the entry goes back and the request is refused by name, which is what psql does
//! and for the same reason.

use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};

use aperture_schema::{
    fingerprint::{self, Identity},
    schema::{PredicateId, Schema},
    syntax,
};
use aperture_store::{
    catalog::{Catalog, Finished, Listing},
    error::StoreError,
    schema_doc,
    store::FjallDb,
};

use aperture_wire::protocol::{Control, ControlOp, ControlReply};

use crate::{blocking, error::ServerError, session::Database, stats::ServerStats};

/// How a database's schema is arrived at.
///
/// **A schema belongs to a database, not to a server** ([I13](../../../docs/invariants.md#i13)):
/// each one embedded its own at create, and this is what reads it back. Two pieces are
/// the server's rather than the database's, and both are here because they are the same
/// two every time:
///
/// - the **virtual** predicates — `aperture.db.List` and anything joining it — which
///   the server answers out of the root it owns and no artifact holds;
/// - a **fallback**, for a database that embedded no copy at all.
///
/// The virtual half is carried as *source* rather than as a `Schema`, because composing
/// two schemas means composing two interners, and the language already has an operator
/// for it: concatenation. Reserved names sort last
/// ([`RESERVED_NAMESPACE`](aperture_schema::syntax::lower::RESERVED_NAMESPACE)), so
/// appending them moves no stored id.
pub struct Schemas {
    virtual_source: String,
    fallback: Arc<Schema>,
}

impl Schemas {
    /// `virtual_source` is appended to every database's own schema; `fallback` is what
    /// a database with no embedded copy is served with, already composed.
    #[must_use]
    pub fn new(virtual_source: impl Into<String>, fallback: Schema) -> Schemas {
        Schemas {
            virtual_source: virtual_source.into(),
            fallback: Arc::new(fallback),
        }
    }

    /// The schema to serve the database at `path` with.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if the copy is unreadable, does not lower, or is not the
    /// schema the sidecar says this database was created against. Each leaves the
    /// database **unserved** rather than served through a schema it does not hold: a
    /// schema that disagrees reads stored rows through the wrong types and reports
    /// nothing.
    pub fn of(&self, path: &std::path::Path, recorded: u64) -> Result<Arc<Schema>, StoreError> {
        let Some(source) = schema_doc::source(path)? else {
            return Ok(Arc::clone(&self.fallback));
        };

        let fault = |detail: String| StoreError::Meta {
            path: path
                .join(schema_doc::SCHEMA_DIR)
                .join(schema_doc::SCHEMA_FILE),
            detail,
        };

        let composed = format!("{source}\n{}", self.virtual_source);
        let schema = syntax::recover(schema_doc::SCHEMA_FILE, &composed).map_err(fault)?;

        // Virtual by **namespace**, not by name: the reserved namespace is what makes
        // "the server answers this one" a property of the schema text rather than a
        // list kept somewhere else and forgotten when a second one is added.
        let served = schema
            .clone()
            .with_virtual((0..schema.len()).filter_map(|index| {
                let id = PredicateId(index as u32);
                schema
                    .get(id)?
                    .name()?
                    .starts_with(syntax::lower::RESERVED_NAMESPACE)
                    .then_some(id)
            }));

        let embedded = fingerprint::of(&served);
        if embedded != recorded {
            return Err(fault(format!(
                "the copy is {embedded:#018x} and the sidecar records {recorded:#018x} — \
                 one of the two was edited"
            )));
        }

        Ok(Arc::new(served))
    }

    /// The schema of a database named in the root but not open here.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSuchDatabase`] if the root does not hold one, or whatever
    /// [`of`](Schemas::of) reports.
    pub fn of_entry(&self, catalog: &Catalog, name: &str) -> Result<Arc<Schema>, StoreError> {
        let entry = catalog.get(name)?;
        self.of(&entry.path, entry.meta.schema_fingerprint)
    }
}

/// The store root, and the databases open under it.
pub struct Registry {
    catalog: Catalog,
    /// How each database's schema is arrived at, and what a session bound to *no*
    /// database sees.
    schemas: Schemas,
    identity: Identity,
    /// Sorted, so a listing derived from it is stable; behind a lock, so a `create`
    /// can add to it while connections are being served.
    open: RwLock<BTreeMap<String, Arc<Database>>>,
    /// This server's counters.
    ///
    /// Here because the registry is already *the* per-server shared value — every
    /// session is handed one, and there is exactly one per running server — so hanging
    /// the counters on it costs no new plumbing. It is not a claim that counting is a
    /// registry concern: [`ServerStats`] is its own module for that reason.
    stats: Arc<ServerStats>,
}

impl Registry {
    /// Open every database under `catalog`'s root.
    ///
    /// A database that cannot be opened becomes a **problem in the listing** rather
    /// than a failure to start: it still appears in `list` (`ops-I7` reads its
    /// sidecar), a handshake to it says there is no such database, and the other nine
    /// are served. A server that refuses to start because one directory is corrupt is
    /// a server that cannot be used to find out which one.
    ///
    /// A schema this server could not read is the same kind of problem as a store it
    /// could not open, and is treated the same way — the database is listed and not
    /// served, which is the only honest answer when what it holds cannot be described.
    ///
    /// # Errors
    ///
    /// [`ServerError::Store`] only if the root itself cannot be read.
    pub fn open(catalog: Catalog, schemas: Schemas) -> Result<(Registry, Listing), ServerError> {
        let identity = fingerprint::identity(&schemas.fallback);

        let mut listing = catalog.list()?;
        let mut open = BTreeMap::new();

        for entry in &listing.entries {
            let opened = FjallDb::open(&entry.path).and_then(|db| {
                let schema = schemas.of(&entry.path, entry.meta.schema_fingerprint)?;
                Ok(Database::new(entry.name(), db, schema, entry.status()))
            });

            match opened {
                Ok(database) => {
                    open.insert(entry.name().to_owned(), Arc::new(database));
                }
                Err(problem) => listing.problems.push(problem),
            }
        }

        Ok((
            Registry {
                catalog,
                schemas,
                identity,
                open: RwLock::new(open),
                stats: Arc::new(ServerStats::default()),
            },
            listing,
        ))
    }

    /// The store root this server owns.
    ///
    /// Cheap to clone — it holds a path — and read rather than mutated: enumeration
    /// needs no ownership at all, which is the whole of `ops-I7`.
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// This server's counters.
    ///
    /// Readable, and read by tests; not *reported* anywhere, which is
    /// [`stats`](crate::stats)'s own note to explain.
    #[must_use]
    pub fn stats(&self) -> &Arc<ServerStats> {
        &self.stats
    }

    /// The schema fingerprint a session that names no database handshakes against.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        self.identity.schema()
    }

    /// The identity a session that names no database is checked against — the whole
    /// number and the per-predicate map alike.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The schema a session that names **no database** sees: the fallback, which is
    /// this server's built-in one. A session bound to a database sees that database's.
    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schemas.fallback
    }

    /// The database called `name`, if this server is serving one.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<Arc<Database>> {
        self.read().get(name).map(Arc::clone)
    }

    /// How many databases are being served — what `serve` prints, and what a test
    /// checks a `create` changed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Carry out a lifecycle request.
    ///
    /// # Errors
    ///
    /// Whatever the catalog reports, or [`ServerError::InUse`] for a database a
    /// session still holds.
    pub async fn execute(&self, request: &Control) -> Result<ControlReply, ServerError> {
        match request.op {
            ControlOp::Create => self.create(&request.database, &request.schema).await,
            ControlOp::Finish => {
                self.finish(&request.database, request.allow_zero_facts)
                    .await
            }
            ControlOp::Remove => self.remove(&request.database).await,
        }
    }

    /// Create a database and start serving it.
    ///
    /// Built and opened before it is published, so a name appears in the registry only
    /// once it names something a session could actually bind — which is the same
    /// all-or-nothing rule [`Catalog::create`] follows on the disk, one level up.
    ///
    /// `source` is the schema to create it against, already resolved by the caller, or
    /// empty for this server's own. It is **lowered here rather than trusted**: the
    /// text arrived over a socket, and a database created from a schema nothing read
    /// is a database nothing can serve.
    async fn create(&self, name: &str, source: &str) -> Result<ControlReply, ServerError> {
        let catalog = self.catalog.clone();

        let schema = if source.is_empty() {
            Arc::clone(&self.schemas.fallback)
        } else {
            Arc::new(
                syntax::read("the schema this client sent", source)
                    .map_err(ServerError::Protocol)?,
            )
        };

        let wanted = name.to_owned();

        let (entry, db) = blocking::run(move || {
            let entry = catalog.create(&wanted, &schema)?;
            let db = FjallDb::open(&entry.path)?;
            Ok((entry, db))
        })
        .await?;

        // **Served from its own embedded copy, immediately.** Not from the schema it
        // was created with, which would be the same thing on the happy path and would
        // let a database be served — once, until the next restart — through a copy
        // nothing had ever read back.
        let schema = self
            .schemas
            .of(&entry.path, entry.meta.schema_fingerprint)?;

        let database = Arc::new(Database::new(entry.name(), db, schema, entry.status()));

        self.write().insert(entry.name().to_owned(), database);

        Ok(ControlReply::Created {
            instance: entry.meta.instance,
        })
    }

    /// Seal a database, and stop taking writes for it.
    async fn finish(
        &self,
        name: &str,
        allow_zero_facts: bool,
    ) -> Result<ControlReply, ServerError> {
        let Some(database) = self.find(name) else {
            // A database the root holds but this server never opened — one whose store
            // or whose schema copy could not be read at startup. There is no handle to
            // pass, so the offline path is not merely allowed here; it is the only
            // correct one, and it reads that database's own schema rather than this
            // server's, since the content fingerprint is over the facts *it* holds.
            let catalog = self.catalog.clone();
            let schemas = self.schemas.of_entry(&catalog, name)?;
            let wanted = name.to_owned();

            let sealed =
                blocking::run(move || Ok(catalog.finish(&wanted, &schemas, allow_zero_facts)?))
                    .await?;

            return Ok(finished(&sealed));
        };

        // **The seal happens inside the per-database writer lock**, and that is what
        // makes `ops-I2` exact rather than nearly. A block whose session established
        // while the database was still Writable either takes this lock before the seal
        // — and the seal waits behind it — or takes it after, and finds the database no
        // longer writable. There is no third order.
        let _writing = database.writer.lock().await;

        let catalog = self.catalog.clone();
        let schema = Arc::clone(&database.schema);
        let wanted = name.to_owned();
        let held = Arc::clone(&database);

        let sealed = blocking::run(move || {
            Ok(catalog.finish_held(&wanted, held.db.as_ref(), &schema, allow_zero_facts)?)
        })
        .await?;

        database.seal();

        Ok(finished(&sealed))
    }

    /// Stop serving a database, then delete it.
    ///
    /// The order is the whole of it, and it is the same shape as
    /// [`Catalog::remove`]'s rename-then-delete one level down: make it unreachable
    /// first, destroy it second.
    async fn remove(&self, name: &str) -> Result<ControlReply, ServerError> {
        {
            let mut open = self.write();

            if let Some(database) = open.remove(name) {
                match Arc::try_unwrap(database) {
                    // The last reference, so the fjall handle closes right here —
                    // before anything deletes the directory it is holding.
                    Ok(database) => drop(database),

                    // A session still has it. Put it back: a query that is running is
                    // not a reason to hand a client a half-deleted database, and the
                    // caller can ask again once the session has gone.
                    Err(shared) => {
                        open.insert(name.to_owned(), shared);
                        return Err(ServerError::InUse(name.to_owned()));
                    }
                }
            }
        }

        let catalog = self.catalog.clone();
        let wanted = name.to_owned();

        blocking::run(move || Ok(catalog.remove(&wanted)?)).await?;

        Ok(ControlReply::Removed)
    }

    /// A poisoned lock is recovered from rather than propagated.
    ///
    /// The map is a `BTreeMap` of `Arc`s and nothing here can leave it half-updated,
    /// so the invariant a poison flag protects does not exist — and a server that
    /// answered every later request with a panic because one task died holding this
    /// would be strictly worse than one that carries on.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<Database>>> {
        self.open.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Arc<Database>>> {
        self.open.write().unwrap_or_else(PoisonError::into_inner)
    }
}

fn finished(sealed: &Finished) -> ControlReply {
    ControlReply::Finished {
        fingerprint: sealed.fingerprint,
        facts: sealed.facts,
        bytes: sealed.bytes,
        already_complete: sealed.already_complete,
    }
}
