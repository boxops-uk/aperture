//! **The client** — connect, handshake, write facts, read rows.
//!
//! The Rust twin of the C# client under `clients/dotnet`, and it is worth saying what
//! *twin* means here, because it is not "shared code". The two share the wire format
//! and nothing else: no constants, no enums, no unwritten assumptions. That is the
//! whole reason the .NET client exists, and this crate does not weaken it — it makes
//! the Rust side an ordinary client rather than a privileged one, so the CLI and the
//! shell exercise the same protocol an external tool would.
//!
//! What it is made of is `fjord-wire` and a socket. It depends on **no** storage
//! engine, no query engine and no runtime, which is
//! [operations §10](../../docs/fjord-cli-design.md)'s `client → wire → encoding` and
//! its rule that nothing depends on the server.
//!
//! ```no_run
//! # use std::{path::Path, sync::Arc};
//! # use fjord_client::Connection;
//! # use fjord_wire::Mode;
//! # fn main() -> Result<(), fjord_client::ClientError> {
//! # let schema: Arc<fjord_schema::schema::Schema> = todo!();
//! let mut connection = Connection::connect(
//!     Path::new("/tmp/fjord.sock"),
//!     "code",
//!     schema,
//!     Mode::ReadOnly,
//!     false,
//! )?;
//!
//! let mut rows = connection.query("F where src.File F")?;
//!
//! // A page. The stream stays open, and the next call carries on.
//! for row in connection.take(&mut rows, 20)? {
//!     println!("{row:?}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # The schema is the client's
//!
//! Nothing in the protocol describes it: the value codec sends no names and no types
//! because both ends already have them. The handshake asserts they agree, by
//! fingerprint, before a byte of data flows — which is what turns "we disagree about
//! the data model" from a corrupt read months later into a refusal at connect time.
//!
//! # What a page costs, and why `take` is the interesting method
//!
//! [`Connection::take`] reads *n* rows and stops, leaving the stream open. Nothing is
//! buffered here and nothing is buffered there: the server's outbound queue for that
//! stream fills, its query loop suspends holding a **bytes-only cursor**, and the
//! snapshot was already released at the chunk boundary
//! ([I8](../../docs/invariants.md#i8)). A pause of a millisecond and a pause of an hour
//! cost it the same thing. That is the property `\more` is built on
//! ([Phase 9f](../../PLAN.md)), and the reason a result is a bookmark
//! ([`Rows`]) rather than an iterator holding the socket.
//!
//! # Two pipes, one protocol
//!
//! [`Connection::connect`] takes a Unix socket path and [`Connection::connect_tcp`] an
//! authority; [`Connection::open`] takes an [`Endpoint`] and dispatches, which is the one
//! a caller holding an [`Address`] wants. The frames, the handshake and the stream
//! multiplexing are identical — only the pipe differs, which is why it is one enum inside
//! rather than a second client. The server end is default-closed (`ops-I10`) and listens
//! on TCP only when an operator passed `--listen-tcp`, so reaching one means somebody
//! opted in; nothing at this end asserts anything about who may.
//!
//! # What is deliberately not here
//!
//! - **Reconnection, retry and timeouts.** An I/O policy belongs to the program, not to
//!   the transport: a shell wants to tell a person, a deriver wants to retry, and a
//!   client that chose for both would be wrong for one. The one error worth retrying
//!   says so by its code — [`ErrorCode::InUse`](fjord_wire::ErrorCode).
//! - **Concurrency.** Frames for other streams are parked rather than dropped, so
//!   several results can be open at once; but one thread drives the socket. A
//!   background reader is a different design and this one has no need of it yet.

pub mod address;
pub mod connection;
pub mod error;
pub mod expand;
pub mod rows;

pub use address::{Address, DEFAULT_PORT, Endpoint};
pub use connection::{Connection, Hello, Sealed, Written};
pub use error::ClientError;
pub use expand::{Expander, FULL_DEPTH};
pub use rows::Rows;

// The vocabulary a caller needs, so a consumer imports one crate rather than two for
// the ordinary cases. Anything further — the block codec, the frame layer — is
// `fjord-wire` directly, which is where it belongs.
pub use fjord_wire::{
    Desc, ErrorCode, Mode, ProfileStep, QueryProfile, WireFact, WireRef, WireValue,
};

/// **The README, compiled.**
///
/// `cfg(doctest)` so it costs an ordinary build nothing and appears in no documentation:
/// what it buys is that the examples on the crate's front page are run by `cargo test`
/// like any other, rather than being prose that compiled once when it was written.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct Readme;
