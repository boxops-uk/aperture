//! **The client** — connect, handshake, write facts, read rows.
//!
//! The Rust twin of the C# client under `clients/dotnet`, and it is worth saying what
//! *twin* means here, because it is not "shared code". The two share the wire format
//! and nothing else: no constants, no enums, no unwritten assumptions. That is the
//! whole reason the .NET client exists, and this crate does not weaken it — it makes
//! the Rust side an ordinary client rather than a privileged one, so the CLI and the
//! shell exercise the same protocol an external tool would.
//!
//! What it is made of is `aperture-wire` and a socket. It depends on **no** storage
//! engine, no query engine and no runtime, which is
//! [operations §10](../../docs/aperture-cli-design.md)'s `client → wire → encoding` and
//! its rule that nothing depends on the server.
//!
//! ```no_run
//! # use std::{path::Path, sync::Arc};
//! # use aperture_client::Connection;
//! # use aperture_wire::Mode;
//! # fn main() -> Result<(), aperture_client::ClientError> {
//! # let schema: Arc<aperture_schema::schema::Schema> = todo!();
//! let mut connection = Connection::connect(
//!     Path::new("/tmp/aperture.sock"),
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
//! # What is deliberately not here
//!
//! - **TCP.** `ops-I10` is default-closed and the server binds a Unix socket only, so
//!   there is nothing to connect to yet. [`Connection::connect`] takes a path for that
//!   reason, and gains an address form when the server gains a listener.
//! - **Reconnection, retry and timeouts.** An I/O policy belongs to the program, not to
//!   the transport: a shell wants to tell a person, a deriver wants to retry, and a
//!   client that chose for both would be wrong for one. The one error worth retrying
//!   says so by its code — [`ErrorCode::InUse`](aperture_wire::ErrorCode).
//! - **Concurrency.** Frames for other streams are parked rather than dropped, so
//!   several results can be open at once; but one thread drives the socket. A
//!   background reader is a different design and this one has no need of it yet.

pub mod connection;
pub mod error;
pub mod rows;

pub use connection::{Connection, Hello, Sealed, Written};
pub use error::ClientError;
pub use rows::Rows;

// The vocabulary a caller needs, so a consumer imports one crate rather than two for
// the ordinary cases. Anything further — the block codec, the frame layer — is
// `aperture-wire` directly, which is where it belongs.
pub use aperture_wire::{
    Desc, ErrorCode, Mode, ProfileStep, QueryProfile, WireFact, WireRef, WireValue,
};
