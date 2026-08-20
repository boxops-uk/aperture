//! **Fjord DB** — an embedded, immutable fact database — from a client's side of the
//! socket.
//!
//! This crate is a façade and holds no logic. It exists so that getting started is
//! `cargo add fjord-db` rather than three crates chosen correctly: the client
//! ([`fjord_client`]), the type model and schema language ([`fjord_schema`]), and the
//! transport codec and message vocabulary ([`fjord_wire`]). Those three are published
//! and documented in their own right; nothing here shadows them, and reaching for one
//! directly is not a worse thing to do.
//!
//! What is **not** here is the rest of the implementation. The storage codec, the store,
//! the query engine, the write funnel and the server are internal crates and are not
//! published, because a package is what it takes to talk to a database and read rows
//! back — not the shape of what is answering.
//!
//! # A database has a schema, and the client must have it too
//!
//! Nothing in the protocol describes the data model: the value codec sends no field
//! names, no type markers and no record arities, because both ends already have them.
//! So the first thing a program needs is a [`Schema`].
//!
//! A **reader** can ask the server for the one the database was created against, which is
//! the only way to be right about it — a schema is frozen into the database at create
//! ([I13]) and the server serves each database from its own embedded copy:
//!
//! ```no_run
//! use fjord_db::{Connection, Mode, Schema};
//! # fn main() -> Result<(), fjord_db::ClientError> {
//! use std::{path::Path, sync::Arc};
//!
//! // No claim to make, so none is made.
//! let mut connection = Connection::connect(
//!     Path::new("/tmp/fjord.sock"),
//!     "code",
//!     Arc::new(Schema::empty()),
//!     Mode::ReadOnly,
//!     false,
//! )?;
//!
//! let schema = Arc::new(connection.served_schema()?);
//!
//! let mut rows = connection.query("F where src.File F")?;
//! for row in connection.take(&mut rows, 20)? {
//!     println!("{row:?}");
//! }
//! # let _ = schema;
//! # Ok(())
//! # }
//! ```
//!
//! A **producer** has to state the schema itself and assert it, because it is about to
//! encode facts against it — and a disagreement discovered at the handshake is a refused
//! connection rather than a database full of rows nobody can read back. Read one from a
//! `.sigla` file with [`read_schema`]:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let source = std::fs::read_to_string("schemas/code.sigla")?;
//! let schema = fjord_db::read_schema("code.sigla", &source)?;
//! # let _ = schema;
//! # Ok(())
//! # }
//! ```
//!
//! # A reference is the whole target fact
//!
//! On the way in, a reference is the fact it names, nested inline to any depth — not an
//! id. A producer therefore keeps no book of what it has already written: it emits what
//! it holds where it stands, and the write path *interns* each nested fact into a
//! [`FactId`], creating it or finding what that key already names. Sending the same facts
//! twice writes nothing, which is what makes retrying after a dropped connection safe.
//!
//! Stored, a reference is an id and nothing else. Reading one back is the same shape by
//! asking: [`Connection::fetch`] answers *what fact does this id name* with the target's
//! key, and [`Expander`] walks that recursively — how deep to expand is a display
//! decision, so it is the client's, and the server does one point read per id.
//!
//! [I13]: https://github.com/boxops-uk/fjord/blob/main/website/content/invariants.md#i13
//! [`Schema`]: fjord_schema::schema::Schema
//! [`FactId`]: fjord_schema::id::FactId

/// The client: connections, queries, paging, writing, expansion, addresses.
pub use fjord_client as client;
/// The type model, the schema language, and schema identity.
pub use fjord_schema as schema;
/// The transport codec and the protocol's message vocabulary.
pub use fjord_wire as wire;

/// The string interner `Schema` is built over.
///
/// Re-exported because it is in [`fjord_schema`]'s public API —
/// `Schema::new` takes a `lasso::RodeoReader` — so a program that builds a schema by hand
/// needs the *same* version of it. Taking it from here is what makes that automatic
/// instead of a semver trap. Most programs want [`read_schema`] and never touch this.
pub use fjord_schema::lasso;

pub use fjord_client::{
    Address, ClientError, Connection, DEFAULT_PORT, Endpoint, Expander, FULL_DEPTH, Hello, Rows,
    Sealed, Written,
};
pub use fjord_schema::{id::FactId, schema::Schema};
pub use fjord_wire::{
    Desc, ErrorCode, Mode, ProfileStep, QueryProfile, WireFact, WireRef, WireValue,
};

/// Read a schema from `.sigla` source.
///
/// `name` is what diagnostics call the file. The error is the rendered diagnostics, so it
/// says which line rather than that something was wrong.
///
/// **Imports are not followed here.** This lowers one block of source; a schema that spans
/// files is resolved by [`fjord_schema::syntax::resolve`], which needs to know the
/// directories to search and is therefore the caller's decision.
///
/// # Errors
///
/// The diagnostics, rendered, if the source does not parse or does not lower.
pub fn read_schema(name: &str, source: &str) -> Result<Schema, String> {
    fjord_schema::syntax::read(name, source)
}

/// **The README, compiled.**
///
/// `cfg(doctest)` so it costs an ordinary build nothing and appears in no documentation:
/// what it buys is that the examples on the crate's front page are run by `cargo test`
/// like any other, rather than being prose that compiled once when it was written.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct Readme;
