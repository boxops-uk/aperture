use aperture_schema::{id::FactId, schema::PredicateId};
use aperture_store::error::StoreError;
use aperture_wire::WireError;
use thiserror::Error;

/// Why a fact could not be ingested.
///
/// Three sources, kept apart because they are answered differently. A **wire** fault
/// is a peer sending something malformed; a **schema** fault is a peer sending
/// something well-formed that this database has no shape for; a **store** fault is
/// ours. Only the last is a reason to doubt the database.
#[derive(Debug, Error)]
pub enum IngestError {
    /// The bytes did not decode.
    #[error("{0}")]
    Wire(#[from] WireError),

    /// The store could not answer or could not write.
    #[error("{0}")]
    Store(#[from] StoreError),

    /// A predicate the database's schema does not declare.
    ///
    /// Reachable even after a fingerprint handshake, since a block names its
    /// predicate by id and an id is a *position* in a schema — so this is the check
    /// that a peer is talking about the schema it said it was.
    #[error("no predicate {0} in this database's schema")]
    UnknownPredicate(u32),

    /// A value whose shape does not fit the type the schema declares for it.
    #[error("{what} does not fit the schema: {detail}")]
    TypeMismatch {
        what: &'static str,
        detail: &'static str,
    },

    /// The same key already holds a **different** fact — `ops-I5`'s reject, and the
    /// one an interned nested fact can raise by disagreeing with a target that is
    /// already stored.
    ///
    /// Carries what is already there rather than only saying no: a producer's next
    /// question is always "different from what?".
    #[error(
        "predicate {predicate:?} already holds a different fact under this key, as {existing:?}"
    )]
    Conflict {
        predicate: PredicateId,
        existing: FactId,
    },
}

impl IngestError {
    /// Whether this is the peer's fault rather than the database's — what decides
    /// between failing the stream and taking the database out of service.
    #[must_use]
    pub fn is_peers_fault(&self) -> bool {
        match self {
            IngestError::Wire(_)
            | IngestError::UnknownPredicate(_)
            | IngestError::TypeMismatch { .. }
            | IngestError::Conflict { .. } => true,
            IngestError::Store(_) => false,
        }
    }
}
