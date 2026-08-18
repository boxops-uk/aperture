use aperture_wire::{ErrorCode, WireError};
use thiserror::Error;

/// Why a request did not answer.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Wire(#[from] WireError),

    /// **The server refused, and said why.**
    ///
    /// The code is carried rather than folded into the message, because that is what
    /// it is for: a program branches on it without parsing English, and a person reads
    /// the message. A caller that only wants to print something wants
    /// [`Display`](std::fmt::Display), which is the message alone.
    #[error("{message}")]
    Server { code: ErrorCode, message: String },

    /// The peer said something well-formed that does not belong here — a data row
    /// before a descriptor, a reply to an operation nobody asked for.
    ///
    /// Distinct from [`Wire`](ClientError::Wire), which is bytes that do not decode.
    /// This is bytes that decode into the wrong thing, which usually means the two
    /// ends disagree about the conversation rather than about the format.
    #[error("protocol: {0}")]
    Protocol(String),

    /// **The server is older than the question.**
    ///
    /// It answered a frame kind it does not know, which is the framing layer working as
    /// designed: an unrecognised kind is handed up intact rather than failing the decode,
    /// so a peer *can* be told "I do not know that message". This is that answer, made
    /// into a sentence somebody can act on.
    ///
    /// Its own variant because the remedy is different in kind. Every other refusal is
    /// about the request — a bad query, a sealed database, a name already taken — and the
    /// answer is in the message. This one is about the **build on the other end**, and
    /// the answer is to restart it. A caller that can carry on without the request should
    /// (the shell turns expansion off and prints the rows), and one that cannot should
    /// fail: a script that asked for expanded rows must not silently receive ids.
    #[error("{0}")]
    Unsupported(String),
}

impl ClientError {
    /// The server's code, if this came from the server.
    #[must_use]
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            ClientError::Server { code, .. } => Some(*code),
            _ => None,
        }
    }
}
