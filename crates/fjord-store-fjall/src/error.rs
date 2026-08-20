//! What the **lifecycle** can refuse — the faults that are about a database as
//! an artifact rather than about facts.
//!
//! Separate from [`StoreError`] on purpose: a sidecar path, a held root lock, a
//! name that matches several instances and a database that is Complete are all
//! statements about how *this* backend keeps a database on a filesystem. The
//! seam cannot name them without naming an implementation, and a second backend
//! would have to either satisfy them or leave them dead.
//!
//! [`CatalogError::Store`] carries the seam's error, so a read fault raised
//! underneath a lifecycle call still bubbles through one `?`.

use fjord_store::error::StoreError;
use thiserror::Error;

/// A fault in creating, opening, listing, sealing or deleting a database.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// fjall itself failed.
    ///
    /// Named here, where naming it is free: this crate *is* the fjall backend.
    /// What crosses the seam is [`StoreError::Backend`], which carries the same
    /// error boxed and unnamed.
    #[error("fjall: {0}")]
    Backend(#[from] fjall::Error),

    /// A fault the seam defines — raised under a lifecycle call, and passed on
    /// rather than reworded.
    #[error("{0}")]
    Store(#[from] StoreError),

    /// A fault in a database's sidecar — missing, unreadable, malformed, or written
    /// by a sidecar format this build does not know.
    ///
    /// Carries the path because the sidecar is a file a person can go and look at,
    /// which is most of the reason it is a readable document at all.
    #[error("{path}: {detail}", path = path.display())]
    Meta {
        path: std::path::PathBuf,
        detail: String,
    },

    /// A store root already owned by another process (`ops-I1`).
    ///
    /// Not a lock to wait on: the design refuses a lock fight outright, because the
    /// alternative to failing here is two servers writing one directory.
    #[error("the store root {root} is held by another process", root = root.display())]
    RootHeld { root: std::path::PathBuf },

    /// A database the store root does not hold.
    #[error("no database named `{0}` in this store root")]
    NoSuchDatabase(String),

    /// A name that cannot be a directory, or could escape the store root.
    #[error("`{name}` is not a usable database name: {detail}")]
    BadDatabaseName { name: String, detail: &'static str },

    /// A name that holds several instances, where the caller named none and the
    /// operation must not guess.
    ///
    /// Which operations must not guess is [`Intent`](crate::catalog::Intent)'s
    /// business: a read ranks the candidates and takes the best, because reading the
    /// second-best answers oddly and is recoverable. A write or a delete refuses,
    /// because picking wrong there is neither.
    #[error(
        "`{name}` has {count} instances; name one with `{name}@<instance>` ({shown})",
        count = instances.len(),
        shown = instances.join(", "),
    )]
    AmbiguousDatabase {
        name: String,
        instances: Vec<String>,
    },

    /// An instance — or an instance prefix — that names nothing under this database.
    #[error("`{name}` has no instance matching `{instance}`")]
    NoSuchInstance { name: String, instance: String },

    /// A schema that cannot be written down and read back as itself.
    ///
    /// A database embeds its schema as source and is served from that copy
    /// ([I13](../../../website/content/invariants.md#i13)), so a schema that does not survive the
    /// round trip is one no database could be opened with. Refused at `create`, where
    /// nothing has been written yet — the alternative is an artifact whose predicates
    /// come back at different positions, which reads every stored row through the wrong
    /// type and reports nothing.
    #[error("the schema for `{name}` cannot be embedded: {detail}")]
    UnwritableSchema { name: String, detail: String },

    /// A write asked of a database that is not [`Writable`](crate::meta::Status::Writable).
    ///
    /// `ops-I2`: once Complete, immutability is structural — no writable handle
    /// exists — rather than defended per write.
    #[error("`{name}` is {status} and cannot be written to")]
    NotWritable {
        name: String,
        status: crate::meta::Status,
    },

    /// A seal asked of a database holding no facts.
    ///
    /// A silently-empty sealed artifact is the classic CI failure that looks like
    /// success — the build "succeeded" and shipped nothing — so making one takes
    /// saying so.
    #[error("`{0}` holds no facts; sealing an empty database takes --allow-zero-facts")]
    EmptyDatabase(String),
}
