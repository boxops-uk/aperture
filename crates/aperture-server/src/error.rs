use aperture_ingest::IngestError;
use aperture_wire::WireError;
use thiserror::Error;

use crate::protocol::ErrorCode;

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

    #[error(
        "schema mismatch: the client expects {expected:#018x} and this database has {actual:#018x}"
    )]
    SchemaMismatch { expected: u64, actual: u64 },

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
        match self {
            ServerError::Protocol(_) | ServerError::Wire(_) => ErrorCode::Protocol,
            ServerError::UnknownDatabase(_) => ErrorCode::UnknownDatabase,
            ServerError::SchemaMismatch { .. } => ErrorCode::SchemaMismatch,
            ServerError::ModeRefused => ErrorCode::ModeRefused,
            ServerError::BadQuery(_) => ErrorCode::BadQuery,
            ServerError::Ingest(ingest) => match ingest {
                IngestError::Conflict { .. } => ErrorCode::Conflict,
                _ => ErrorCode::BadFacts,
            },
            ServerError::Io(_)
            | ServerError::Store(_)
            | ServerError::Unprojectable(_)
            | ServerError::Execution(_) => ErrorCode::Internal,
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
