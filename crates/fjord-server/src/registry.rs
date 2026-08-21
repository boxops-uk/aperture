//! **What the server owns**: a store root, and every database under it.
//!
//! Until 9d the server was handed a `Vec<Arc<Database>>` opened once at startup, and
//! that was the whole reason lifecycle commands had to be refused while it ran: a
//! `create` cannot add to a list nobody holds, and a `remove` cannot delete a directory
//! this process has open. The registry is the mutable form of that list, plus the
//! [`Catalog`] the CLI's offline path already uses — which is what makes
//! [operations §5](../../../website/content/operations.md)'s "two front doors, one
//! implementation" true rather than aspirational. Everything below delegates the actual
//! work to `fjord-store`; what lives here is *when* it is safe to do it.
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

use fjord_schema::{
    fingerprint::{self, Identity},
    schema::{PredicateId, Schema},
    syntax,
};
use fjord_store_fjall::{
    catalog::{Catalog, Entry, Finished, Intent, Listing, Selector},
    error::CatalogError,
    schema_doc,
    store::FjallDb,
};

use fjord_wire::protocol::{Control, ControlOp, ControlReply};

use crate::{
    blocking,
    error::ServerError,
    session::Database,
    stats::{InterningCounters, ServerStats},
};

/// How a database's schema is arrived at.
///
/// **A schema belongs to a database, not to a server** ([I13](../../../website/content/invariants.md#i13)):
/// each one embedded its own at create, and this is what reads it back. Exactly one piece
/// is the server's rather than the database's — the **virtual** predicates,
/// `fjord.db.List` and `fjord.db.Interning`, which the server answers out of the root it
/// owns and no artifact holds.
///
/// **There is no fallback, deliberately.** Serving a database that embeds no copy with
/// any schema the server holds is a guess, and a silent one: if that schema has moved
/// since the database was written, its rows decode as something else — the loud version
/// a decode error, the quiet version a query answering zero rows. So a database with no
/// embedded copy is **listed and not served**, which is already how a copy this
/// server cannot *read* is treated, and is the same refusal
/// [I15](../../../website/content/invariants.md#i15) makes of a database carrying no format stamp.
///
/// The virtual half is carried as *source* rather than as a `Schema`, because composing
/// two schemas means composing two interners, and the language already has an operator
/// for it: concatenation. Reserved names sort last
/// ([`RESERVED_NAMESPACE`](fjord_schema::syntax::lower::RESERVED_NAMESPACE)), so
/// appending them moves no stored id.
pub struct Schemas {
    virtual_source: String,
    /// The virtual predicates alone, composed once.
    ///
    /// What a session naming **no database** is served with, and what its handshake
    /// identity is computed over. A control session therefore agrees with the server
    /// about the catalogue and about nothing else — which is all a lifecycle request can
    /// honestly agree about, since it names a database that may not exist yet.
    catalogue: Arc<Schema>,
}

impl Default for Schemas {
    /// This server's own: the catalogue, and nothing else.
    fn default() -> Schemas {
        Schemas::new(crate::catalogue::SOURCE)
    }
}

impl Schemas {
    /// `virtual_source` is appended to every database's own schema.
    ///
    /// Almost every caller wants [`Schemas::default`], which passes
    /// [`catalogue::SOURCE`](crate::catalogue::SOURCE). This form is for a battery that
    /// wants a server answering no virtual predicates at all: `""` is how a test says
    /// "no catalogue", which is a different thing from a catalogue it cannot read.
    ///
    /// # Panics
    ///
    /// If `virtual_source` does not parse and lower. On every real path it is
    /// compiled-in text, so this is a build error wearing a runtime hat — and a server
    /// that started with a catalogue it could not read would answer `fjord.db.List` with
    /// nothing and never say why.
    #[must_use]
    pub fn new(virtual_source: impl Into<String>) -> Schemas {
        let virtual_source = virtual_source.into();

        let catalogue = syntax::read("the catalogue", &virtual_source)
            .map(|schema| with_virtuals_marked(&schema))
            .unwrap_or_else(|error| panic!("the catalogue does not lower: {error}"));

        Schemas {
            virtual_source,
            catalogue: Arc::new(catalogue),
        }
    }

    /// The virtual predicates alone — what a session naming no database sees.
    #[must_use]
    pub fn catalogue(&self) -> &Arc<Schema> {
        &self.catalogue
    }

    /// The schema to serve the database at `path` with.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Meta`] if the copy is **absent**, unreadable, does not lower, or is
    /// not the schema the sidecar says this database was created against. Each leaves the
    /// database **unserved** rather than served through a schema it does not hold: a
    /// schema that disagrees reads stored rows through the wrong types and reports
    /// nothing.
    pub fn of(&self, path: &std::path::Path, recorded: u64) -> Result<Arc<Schema>, CatalogError> {
        let fault = |detail: String| CatalogError::Meta {
            path: path
                .join(schema_doc::SCHEMA_DIR)
                .join(schema_doc::SCHEMA_FILE),
            detail,
        };

        // **No copy is a refusal, not a fallback.** A database that embeds no schema
        // predates one being kept; there is nothing here that can describe its rows, and
        // the only other candidate — a schema this build happens to carry — would be a
        // guess whose failure mode is a query answering nothing.
        let Some(source) = schema_doc::source(path)? else {
            return Err(fault(
                "it embeds no schema copy, so nothing here can describe its rows — \
                 re-index it against the schema it was built from"
                    .to_owned(),
            ));
        };

        let composed = format!("{source}\n{}", self.virtual_source);
        let schema = syntax::recover(schema_doc::SCHEMA_FILE, &composed).map_err(fault)?;
        let served = with_virtuals_marked(&schema);

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
    /// Takes the resolved [`Entry`] rather than a name, so that a caller which has
    /// already chosen among a name's instances cannot re-resolve and drift onto
    /// another one.
    ///
    /// # Errors
    ///
    /// Whatever [`of`](Schemas::of) reports.
    pub fn of_entry(&self, entry: &Entry) -> Result<Arc<Schema>, CatalogError> {
        self.of(&entry.path, entry.meta.schema_fingerprint)
    }
}

/// Mark every predicate in the reserved namespace **virtual**.
///
/// Virtual by **namespace**, not by name: the reserved namespace is what makes "the
/// server answers this one" a property of the schema text rather than a list kept
/// somewhere else and forgotten when a second one is added.
///
/// Shared by [`Schemas::new`] and [`Schemas::of`] so a catalogue served on its own is
/// marked the same way as one appended to a database's schema. Marking it in one place
/// and not the other is how a catalogue predicate acquires keyspaces.
fn with_virtuals_marked(schema: &Schema) -> Schema {
    schema
        .clone()
        .with_virtual((0..schema.len()).filter_map(|index| {
            let id = PredicateId(index as u32);
            schema
                .get(id)?
                .name()?
                .starts_with(syntax::lower::RESERVED_NAMESPACE)
                .then_some(id)
        }))
}

/// The store root, and the databases open under it.
pub struct Registry {
    catalog: Catalog,
    /// How each database's schema is arrived at, and what a session bound to *no*
    /// database sees.
    schemas: Schemas,
    identity: Identity,
    /// **Keyed by instance id, not by name**, because a name holds several and a map
    /// keyed by name would silently serve one of them and drop the rest.
    ///
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
    /// Whether a write stream commits once per block rather than once per fact.
    ///
    /// **Off unless asked for, and the asking is operational rather than structural.** It
    /// is a property of *this run of the server*, not of the artifact — two databases with
    /// identical content must not differ in their metadata because of how fast somebody
    /// wanted to write them — so it lives here and not in the sidecar. What it trades is
    /// on [`Staged`](fjord_store_fjall::store::Staged): a crash during ingest may cost the
    /// index and can never cost its correctness.
    block_commits: bool,
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
    /// [`ServerError::Catalog`] only if the root itself cannot be read.
    pub fn open(catalog: Catalog, schemas: Schemas) -> Result<(Registry, Listing), ServerError> {
        // Over the catalogue alone, which is all this server has of its own. A session
        // naming no database is handshaking about lifecycle, not about data.
        let identity = fingerprint::identity(&schemas.catalogue);

        let mut listing = catalog.list()?;
        let mut open = BTreeMap::new();

        for entry in &listing.entries {
            let opened = FjallDb::open(&entry.path)
                .map_err(CatalogError::from)
                .and_then(|db| {
                    let schema = schemas.of(&entry.path, entry.meta.schema_fingerprint)?;
                    Ok(Database::new(
                        entry.name(),
                        &entry.meta.instance,
                        db,
                        schema,
                        entry.status(),
                    ))
                });

            match opened {
                Ok(database) => {
                    open.insert(entry.meta.instance.clone(), Arc::new(database));
                }
                Err(problem) => listing.problems.push(problem),
            }
        }

        Ok((
            Registry {
                block_commits: false,
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
    /// Commit once per block on every write stream this registry serves.
    ///
    /// Consuming rather than a setter: it is a startup choice, and a server that changed
    /// its durability behaviour halfway through a stream would be answering two different
    /// questions in one ingest.
    pub fn with_block_commits(mut self, on: bool) -> Registry {
        self.block_commits = on;
        self
    }

    /// Whether write streams commit once per block.
    #[must_use]
    pub fn block_commits(&self) -> bool {
        self.block_commits
    }

    #[must_use]
    pub fn stats(&self) -> &Arc<ServerStats> {
        &self.stats
    }

    /// What every open database's interning path has done since this server opened it.
    ///
    /// **One row per database this server holds open**, in instance order, which is what
    /// `fjord.db.Interning` answers with. A database the root lists but this server
    /// could not open has no counters to report and no row; a sealed one is open, never
    /// interns, and reads zero — which is a true statement about it rather than a gap.
    ///
    /// Reading it takes each stripe's lock in turn, per database, so it belongs on a
    /// query about the counters and nowhere near the path that increments them. That is
    /// why nothing calls this unless a plan names the predicate.
    #[must_use]
    pub fn interning(&self) -> Vec<InterningCounters> {
        self.read()
            .values()
            .map(|database| {
                let (hits, misses) = database.db.lookup_counters();
                let (keys, entities) = database.db.intern_read_counters();

                InterningCounters {
                    name: database.name.clone(),
                    instance: database.instance.clone(),
                    hits,
                    misses,
                    keys,
                    entities,
                }
            })
            .collect()
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

    /// The schema a session that names **no database** sees: the catalogue, which is all
    /// this server holds that is not some database's. A session bound to a database sees
    /// that database's own.
    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schemas.catalogue
    }

    /// The database `address` names — `name`, or `name@instance`.
    ///
    /// **Resolved through the catalog rather than over the open map**, which costs a
    /// walk of the root's sidecars per bind and buys two things. The rule for what an
    /// unqualified name means lives in exactly one place ([`Intent`]) instead of being
    /// restated here over a different element type. And the answer comes from the
    /// authoritative state on disk, so a database created a moment ago by the offline
    /// path is bindable without this server having noticed it.
    ///
    /// # Errors
    ///
    /// Whatever resolution reports — an unknown name, an unknown instance, or an
    /// ambiguity the caller must settle — and [`ServerError::UnknownDatabase`] for one
    /// the root holds but this server could not open.
    pub fn bind(&self, address: &str) -> Result<Arc<Database>, ServerError> {
        let selector = Selector::parse(address)?;
        let entry = self.catalog.resolve(&selector, Intent::Read)?;

        self.by_instance(&entry.meta.instance)
            .ok_or_else(|| ServerError::UnknownDatabase(address.to_owned()))
    }

    /// The database with this exact instance id, if this server opened it.
    #[must_use]
    pub fn by_instance(&self, instance: &str) -> Option<Arc<Database>> {
        self.read().get(instance).map(Arc::clone)
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
    /// `source` is the schema to create it against, already resolved by the caller, and
    /// it is **required**. It is also **lowered here rather than trusted**: the text
    /// arrived over a socket, and a database created from a schema nothing read is a
    /// database nothing can serve.
    ///
    /// An empty `source` must not mean "this server's own" — that is the write half
    /// of the guess [`Schemas`] refuses to read: a database whose embedded schema is
    /// whatever binary happened to be listening, so the same command against two
    /// builds produces two different artifacts. `create` requires a schema
    /// ([operations](../../../website/content/operations.md)).
    ///
    /// # Errors
    ///
    /// [`ServerError::Protocol`] if `source` is empty or does not lower.
    async fn create(&self, name: &str, source: &str) -> Result<ControlReply, ServerError> {
        let catalog = self.catalog.clone();

        if source.trim().is_empty() {
            return Err(ServerError::Protocol(
                "create needs a schema: pass one with `--schema <file>`".to_owned(),
            ));
        }

        let schema = Arc::new(
            syntax::read("the schema this client sent", source).map_err(ServerError::Protocol)?,
        );

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

        let database = Arc::new(Database::new(
            entry.name(),
            &entry.meta.instance,
            db,
            schema,
            entry.status(),
        ));

        self.write().insert(entry.meta.instance.clone(), database);

        Ok(ControlReply::Created {
            instance: entry.meta.instance,
        })
    }

    /// Seal a database, and stop taking writes for it.
    async fn finish(
        &self,
        address: &str,
        allow_zero_facts: bool,
    ) -> Result<ControlReply, ServerError> {
        // **Resolved once, and everything below addresses the instance it chose.**
        // Re-resolving would let a `create` arriving in between move the answer, and a
        // seal is the one operation where landing on the wrong instance is unrecoverable.
        // `Intent::Write` prefers the writable one and falls back to the sole one, which
        // is what keeps a re-run after a crash able to report "already sealed".
        let selector = Selector::parse(address)?;
        let entry = self.catalog.resolve(&selector, Intent::Write)?;
        let exact = entry.selector();

        let Some(database) = self.by_instance(&entry.meta.instance) else {
            // A database the root holds but this server never opened — one whose store
            // or whose schema copy could not be read at startup. There is no handle to
            // pass, so the offline path is not merely allowed here; it is the only
            // correct one, and it reads that database's own schema rather than this
            // server's, since the content fingerprint is over the facts *it* holds.
            let catalog = self.catalog.clone();

            // Read for its **check** rather than for its result: `Catalog::finish` reads
            // the embedded copy itself now, and this is what still refuses a database
            // whose copy disagrees with the fingerprint its sidecar records. Sealing that
            // would record an `ops-I4` identity over content described by a schema one of
            // whose two statements has been edited.
            self.schemas.of_entry(&entry)?;

            let sealed =
                blocking::run(move || Ok(catalog.finish(&exact, allow_zero_facts)?)).await?;

            return Ok(finished(&sealed));
        };

        // **The seal takes the barrier exclusively**, and that is what makes `ops-I2`
        // exact rather than nearly. A block whose session established while the database
        // was still Writable either takes the barrier before the seal — and the seal
        // waits behind it — or takes it after, and finds the database no longer
        // writable. There is no third order. Since 12e writers hold that barrier
        // *shared*, so "waits behind it" means waiting for every in-flight block rather
        // than for the one that happened to be running.
        let _sealing = database.sealing.write().await;

        let catalog = self.catalog.clone();
        let schema = Arc::clone(&database.schema);
        let held = Arc::clone(&database);

        let sealed = blocking::run(move || {
            Ok(catalog.finish_held(&exact, held.db.as_ref(), &schema, allow_zero_facts)?)
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
    async fn remove(&self, address: &str) -> Result<ControlReply, ServerError> {
        // `Intent::Sole` rather than `Read`: a delete must not rank and commit, so
        // `rm code` where `code` holds three instances is a question, not a guess.
        let selector = Selector::parse(address)?;
        let entry = self.catalog.resolve(&selector, Intent::Sole)?;
        let instance = entry.meta.instance.clone();

        {
            let mut open = self.write();

            if let Some(database) = open.remove(&instance) {
                match Arc::try_unwrap(database) {
                    // The last reference, so the fjall handle closes right here —
                    // before anything deletes the directory it is holding.
                    Ok(database) => drop(database),

                    // A session still has it. Put it back: a query that is running is
                    // not a reason to hand a client a half-deleted database, and the
                    // caller can ask again once the session has gone. Reported by the
                    // address the caller used, since that is what they can act on.
                    Err(shared) => {
                        open.insert(instance, shared);
                        return Err(ServerError::InUse(address.to_owned()));
                    }
                }
            }
        }

        let catalog = self.catalog.clone();
        let exact = entry.selector();

        blocking::run(move || Ok(catalog.remove(&exact)?)).await?;

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

#[cfg(test)]
mod tests {
    use fjord_schema::schema::{Predicate, PredicateTy};
    use lasso::Rodeo;

    use super::*;

    /// One stored predicate, stated in Rust so this file does not depend on a schema
    /// file to have a database to compose against.
    fn stored() -> Schema {
        let mut rodeo = Rodeo::new();
        let file = rodeo.get_or_intern("src.File");
        Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            }]),
        )
    }

    /// A database created against [`stored`], and the schema a server serves it with.
    fn served() -> (tempfile::TempDir, Schema, Arc<Schema>) {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
        let entry = catalog.create("code", &stored()).expect("a database");

        let schemas = Schemas::default();
        let served = schemas
            .of(&entry.path, entry.meta.schema_fingerprint)
            .expect("it is servable");

        (dir, stored(), served)
    }

    /// **The property the whole arrangement rests on**: appending a virtual predicate
    /// does not change what a client has to agree with.
    ///
    /// If this ever fails, every .NET client stops connecting until it declares a
    /// predicate it can never write to — which is the outcome the virtual/stored split
    /// exists to avoid, and the reason identity skips virtuals rather than the server
    /// keeping two schemas and hoping they stay in step.
    #[test]
    fn the_catalogue_does_not_change_the_handshake() {
        let (_dir, stored, served) = served();

        assert_eq!(
            fingerprint::of(&stored),
            fingerprint::of(&served),
            "a virtual predicate must be invisible to the handshake"
        );
    }

    /// Restating the stored schema must not move an id, because an id is a position and
    /// is the tag in every `FactId` already written.
    #[test]
    fn appending_the_catalogue_moves_no_stored_id() {
        let (_dir, stored, served) = served();

        let appended = syntax::read("the catalogue", crate::catalogue::SOURCE)
            .expect("the catalogue lowers")
            .len();

        assert_eq!(
            served.len(),
            stored.len() + appended,
            "the catalogue's predicates appended, nothing else"
        );

        for index in 0..stored.len() {
            let id = PredicateId(index as u32);
            assert_eq!(
                served.get(id).and_then(|p| p.name()),
                stored.get(id).and_then(|p| p.name()),
                "predicate {index} moved"
            );
        }
    }

    /// **Exactly the catalogue's predicates are virtual, and nothing else is.**
    ///
    /// Stated as a set rather than as one id: the failure this guards is a predicate
    /// added to `catalogue.sigla` and left *stored*, which gives it keyspaces at `create`,
    /// puts it in `ops-I4`'s identity, and moves the fingerprint every client agrees
    /// with. That is what happened the first time one was added.
    #[test]
    fn exactly_the_catalogues_predicates_are_virtual() {
        let (_dir, stored, served) = served();

        let declared =
            syntax::read("the catalogue", crate::catalogue::SOURCE).expect("the catalogue lowers");

        let mut expected: Vec<PredicateId> = (0..declared.len())
            .filter_map(|index| declared.get(PredicateId(index as u32))?.name())
            .filter_map(|name| served.find_position(name).map(|(id, _)| id))
            .collect();
        expected.sort_unstable();

        assert!(!expected.is_empty(), "the catalogue declares something");
        assert_eq!(served.virtuals(), expected.as_slice());
        assert!(
            served
                .find_position(crate::catalogue::PREDICATE)
                .is_some_and(|(id, _)| served.is_virtual(id)),
            "the listing is one of them"
        );
        assert!(stored.virtuals().is_empty(), "the stored schema has none");
    }

    /// A server's own schema is the catalogue and nothing else — which is what a session
    /// naming no database sees, and what its handshake identity is over.
    #[test]
    fn a_servers_own_schema_is_the_catalogue_alone() {
        let schemas = Schemas::default();
        let catalogue = schemas.catalogue();

        assert!(!catalogue.is_empty(), "it declares the virtual predicates");
        assert_eq!(
            catalogue.virtuals().len(),
            catalogue.len(),
            "and every one of them is virtual: a server stores nothing of its own"
        );
    }
}
