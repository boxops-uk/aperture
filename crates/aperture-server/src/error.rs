use aperture_ingest::IngestError;
use aperture_wire::WireError;
use thiserror::Error;

use aperture_wire::protocol::ErrorCode;

/// Why the server could not do what a frame asked.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Wire(#[from] WireError),

    #[error("{0}")]
    Ingest(#[from] IngestError),

    #[error("{0}")]
    Store(#[from] aperture_store::error::StoreError),

    /// The peer sent a frame that makes no sense here — a `CopyData` on a stream it
    /// never opened, a second startup, a kind the server has no handler for.
    #[error("protocol: {0}")]
    Protocol(String),

    #[error("no database named `{0}`")]
    UnknownDatabase(String),

    /// A frame that needs a database, on a session bound to none.
    #[error("this session names no database; name one at startup to query or write")]
    NoDatabase,

    /// A write asked of a Complete database (`ops-I2`).
    ///
    /// Distinct from [`ModeRefused`](ServerError::ModeRefused), which is a session
    /// that never asked to write. This one asked, and the database is sealed.
    #[error("`{0}` is complete: it takes no more writes")]
    Sealed(String),

    /// A database a session still holds, asked to be deleted.
    ///
    /// A refusal rather than a wait, and rather than pulling the store out from under
    /// a running query. It is also the one error here worth *retrying*: the condition
    /// ends when the session does.
    #[error("`{0}` is in use by an open session and cannot be removed")]
    InUse(String),

    #[error(
        "schema mismatch: the client expects {expected:#018x} and this database has {actual:#018x}"
    )]
    SchemaMismatch { expected: u64, actual: u64 },

    /// A client claiming predicates this database does not hold in that shape.
    ///
    /// Distinct from [`SchemaMismatch`](ServerError::SchemaMismatch) in what it can
    /// *say*: two whole-schema numbers that differ say nothing about which predicate
    /// they differ over, and a producer writing a subset needs exactly that. Same code
    /// on the wire — a client branches on "we disagree about the schema" either way —
    /// and a different message, because a message is what a person acts on.
    #[error(
        "schema mismatch: `{database}` does not hold {} as this client declares {them} \
         ({detail})",
        if broken.len() == 1 { "this predicate" } else { "these predicates" },
        them = broken.join(", "),
    )]
    SchemaNotContained {
        database: String,
        broken: Vec<String>,
        detail: String,
    },

    #[error("this session is read-only")]
    ModeRefused,

    /// The query did not compile. Carries the rendered diagnostics, because a
    /// compiler's own message is better than anything this layer could summarise.
    #[error("{0}")]
    BadQuery(String),

    /// A row that does not fit the type its own head produced — a fault in the
    /// server rather than in the request.
    #[error("cannot project {0}")]
    Unprojectable(&'static str),

    #[error("{0}")]
    Execution(String),
}

impl ServerError {
    /// The code a client branches on.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        use aperture_store::error::StoreError;

        match self {
            ServerError::Protocol(_) | ServerError::Wire(_) => ErrorCode::Protocol,
            ServerError::UnknownDatabase(_) | ServerError::NoDatabase => ErrorCode::UnknownDatabase,
            ServerError::SchemaMismatch { .. } | ServerError::SchemaNotContained { .. } => {
                ErrorCode::SchemaMismatch
            }
            ServerError::ModeRefused | ServerError::Sealed(_) => ErrorCode::ModeRefused,
            ServerError::InUse(_) => ErrorCode::InUse,
            ServerError::BadQuery(_) => ErrorCode::BadQuery,
            ServerError::Ingest(ingest) => match ingest {
                IngestError::Conflict { .. } => ErrorCode::Conflict,
                _ => ErrorCode::BadFacts,
            },

            // A lifecycle request the store declined is the client's answer, not the
            // server's failure — `Internal` would tell a client to look at the logs
            // for a message that is already in its hand.
            ServerError::Store(store) => match store {
                StoreError::NoSuchDatabase(_) => ErrorCode::UnknownDatabase,
                // An instance that names nothing is the same answer as a database that
                // names nothing: there is no such thing to bind to.
                StoreError::NoSuchInstance { .. } => ErrorCode::UnknownDatabase,
                StoreError::NotWritable { .. } => ErrorCode::ModeRefused,
                StoreError::RootHeld { .. } => ErrorCode::InUse,
                // Ambiguity is refused rather than guessed at, and the message already
                // lists the instances the caller may choose between.
                StoreError::AmbiguousDatabase { .. }
                | StoreError::BadDatabaseName { .. }
                | StoreError::EmptyDatabase(_) => ErrorCode::Refused,
                _ => ErrorCode::Internal,
            },

            ServerError::Io(_) | ServerError::Unprojectable(_) | ServerError::Execution(_) => {
                ErrorCode::Internal
            }
        }
    }

    /// Whether the connection can carry on after this.
    ///
    /// A stream-level fault fails its stream and leaves the connection alone —
    /// which is most of them, since most are the peer asking for something it cannot
    /// have. An I/O fault or a protocol desynchronisation is not recoverable: once
    /// the frame boundaries are in doubt, everything after them is too.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ServerError::Io(_) | ServerError::Wire(_) | ServerError::Protocol(_)
        )
    }
}
