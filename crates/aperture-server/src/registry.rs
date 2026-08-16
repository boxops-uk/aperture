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

use aperture_schema::schema::Schema;
use aperture_store::{
    catalog::{Catalog, Finished, Listing},
    store::FjallDb,
};

use aperture_wire::protocol::{self, Control, ControlOp, ControlReply};

use crate::{blocking, error::ServerError, session::Database, stats::ServerStats};

/// The store root, the databases open under it, and the schema they share.
pub struct Registry {
    catalog: Catalog,
    /// One built-in schema for every database — since Phase 8.4 *parsed* from
    /// `schemas/code.aps` rather than written in Rust, but still one schema the whole
    /// root shares. What is left to do is per-database: this becomes the schema each
    /// database was *created* against, read from its own embedded copy — which is
    /// [I13](../../../docs/invariants.md#i13), and is why nothing below assumes the
    /// schema and the registry have the same lifetime.
    schema: Arc<Schema>,
    fingerprint: u64,
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
    /// # Errors
    ///
    /// [`ServerError::Store`] only if the root itself cannot be read.
    pub fn open(catalog: Catalog, schema: Schema) -> Result<(Registry, Listing), ServerError> {
        let schema = Arc::new(schema);
        let fingerprint = protocol::provisional_fingerprint(&schema);

        let mut listing = catalog.list()?;
        let mut open = BTreeMap::new();

        for entry in &listing.entries {
            match FjallDb::open(&entry.path) {
                Ok(db) => {
                    open.insert(
                        entry.name().to_owned(),
                        Arc::new(Database::new(
                            entry.name(),
                            db,
                            Arc::clone(&schema),
                            entry.status(),
                        )),
                    );
                }
                Err(problem) => listing.problems.push(problem),
            }
        }

        Ok((
            Registry {
                catalog,
                schema,
                fingerprint,
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
        self.fingerprint
    }

    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
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
            ControlOp::Create => self.create(&request.database).await,
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
    async fn create(&self, name: &str) -> Result<ControlReply, ServerError> {
        let catalog = self.catalog.clone();
        let schema = Arc::clone(&self.schema);
        let fingerprint = self.fingerprint;
        let wanted = name.to_owned();

        let (entry, db) = blocking::run(move || {
            let entry = catalog.create(&wanted, &schema, fingerprint)?;
            let db = FjallDb::open(&entry.path)?;
            Ok((entry, db))
        })
        .await?;

        let database = Arc::new(Database::new(
            entry.name(),
            db,
            Arc::clone(&self.schema),
            entry.status(),
        ));

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
            // A database the root holds but this server never opened. There is no
            // handle to pass, so the offline path is not merely allowed here — it is
            // the only correct one.
            let catalog = self.catalog.clone();
            let schema = Arc::clone(&self.schema);
            let wanted = name.to_owned();

            let sealed =
                blocking::run(move || Ok(catalog.finish(&wanted, &schema, allow_zero_facts)?))
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
        let schema = Arc::clone(&self.schema);
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
