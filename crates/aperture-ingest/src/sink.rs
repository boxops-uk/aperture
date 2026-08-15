//! The **write seam**: what interning is allowed to ask of a store.
//!
//! Its own module for the reason [`fact_store`](aperture_store::fact_store) is —
//! that one is the *read* seam the executor consumes, and this is the write seam the
//! funnel produces through. They are deliberately different traits over the same
//! store: reading wants `scan` and `point` against a snapshot, and writing wants one
//! operation that reading has no business offering.
//!
//! One method, because interning needs exactly one thing: **resolve or create**.
//! Splitting it into a lookup and a write would put the dedup rule and the conflict
//! rule in the caller, where every caller would have to get them right, rather than
//! in the store where `ops-I5` says they belong.

use aperture_schema::schema::PredicateId;
use aperture_store::{
    error::StoreError,
    store::{FjallDb, Interned},
};

use crate::error::IngestError;

/// A store facts can be written into.
///
/// `&self` rather than `&mut self`: the real implementation allocates ids from an
/// atomic counter and writes through a batch, so it needs no exclusive borrow — and
/// requiring one would stop a write stream sharing a database handle with the query
/// streams on the same connection.
pub trait FactSink {
    /// The id of the fact under `(predicate, key_fields)`, writing it first if it is
    /// not already there.
    ///
    /// Named for what it does rather than for the caller that wants it, which also
    /// keeps it clear of the store's inherent `intern` — a trait method sharing that
    /// name resolves back to itself inside the impl below, and recurses.
    ///
    /// # Errors
    ///
    /// [`IngestError::Conflict`] if the key is present with a different value — the
    /// same-key-different-value case `ops-I5` rejects.
    fn resolve_or_create(
        &self,
        predicate: PredicateId,
        key_fields: &[u8],
        value: &[u8],
    ) -> Result<Interned, IngestError>;
}

impl FactSink for FjallDb {
    fn resolve_or_create(
        &self,
        predicate: PredicateId,
        key_fields: &[u8],
        value: &[u8],
    ) -> Result<Interned, IngestError> {
        self.intern(predicate, key_fields, value)
            .map_err(|err| match err {
                // A conflict is the **peer's** fault, not the database's, and the
                // two are answered differently — one fails a stream, the other takes
                // a database out of service. So it is lifted out of `StoreError`
                // here rather than folded in with the backend faults.
                StoreError::KeyAlreadyWritten {
                    predicate,
                    existing,
                } => IngestError::Conflict {
                    predicate,
                    existing,
                },
                other => IngestError::Store(other),
            })
    }
}
