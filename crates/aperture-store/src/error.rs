//! What the storage layer can refuse — three types, one umbrella.
//!
//! [`StoreError`] is the umbrella the engine sees; [`FormatError`] and
//! [`FactError`] are arms of it, kept separate because they are raised at
//! different moments. A format stamp is checked once at open, before a row is
//! touched; a fact is checked on the way in, against the schema it is being
//! written under.
//!
//! Everything here is a *data* condition rather than an impossibility: the read
//! path decodes bytes it did not produce in this process, so corruption surfaces
//! as a typed error and never as a panic
//! ([conventions](../../../docs/conventions.md)).

use aperture_encoding::error::StoreCodecError;
use aperture_schema::{
    id::{FactId, FactIdError},
    schema::PredicateId,
};
use thiserror::Error;

use crate::format::FormatVersion;

/// A database this build cannot read, decided from its
/// [format stamp](crate::format) before a single row is touched
/// ([I15](../../docs/invariants.md#i15)).
///
/// Every variant is a *refusal*, and that is the point of the type: without a
/// stamp the alternative is not an error but a silent misread, since bytes written
/// under another encoding decode into plausible-looking values.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FormatError {
    /// A database holding facts but carrying no stamp: written before stamping
    /// existed, or by something that is not Aperture.
    ///
    /// Refused rather than stamped with the current version, which would be a
    /// build asserting that data it has never read was written by itself.
    #[error(
        "this database holds facts but carries no format stamp, so nothing says \
         which encoding wrote it"
    )]
    Unstamped,

    /// A stamp naming a format this build does not implement.
    #[error("this database is {found}; this build reads {current}")]
    Unreadable {
        found: FormatVersion,
        current: FormatVersion,
    },

    #[error("the format stamp is {len} bytes, not {expected}")]
    Truncated { len: usize, expected: usize },

    #[error("the format stamp does not begin with the Aperture magic (found {found:?})")]
    BadMagic { found: [u8; 8] },
}

/// Faults raised by the storage backend itself, or by rows on disk that don't
/// match the [layout](../../docs/03-storage-model.md) the store wrote.
///
/// Corruption surfaces here as a typed error rather than a panic: the read path
/// decodes bytes it did not produce in this process (a reopened DB, a file copied
/// between machines), so a malformed row is a data condition, not an
/// impossibility.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("fjall: {0}")]
    Backend(#[from] fjall::Error),

    /// A database this build cannot read — the [format stamp](crate::format)
    /// checked at open, before a row is touched.
    #[error("{0}")]
    Format(#[from] FormatError),

    /// A value that does not fit the schema it is being written under, raised on
    /// the way in by [`fact`](crate::fact).
    #[error("cannot write this fact: {0}")]
    Fact(#[from] FactError),

    /// A `keys` row naming an id with no row behind it in `entities`. The
    /// [scan → point mapping](../../docs/03-storage-model.md) is a total function
    /// only while every id resolves, so a gap is a fault in the store rather than
    /// a query answering nothing.
    #[error("dangling fact id {0:?}: key present but no entity in the `entities` column family")]
    DanglingFactId(FactId),

    #[error("`keys` row value is {len} bytes, not an {expected}-byte fact id")]
    FactIdWidth { len: usize, expected: usize },

    #[error("`entities` row for {0:?} is truncated")]
    TruncatedEntity(FactId),

    /// A `keys` row too short to carry the predicate-id prefix every row begins
    /// with. A register holds the whole row and strips that prefix to reach the
    /// key fields, so a shorter row has no key to read.
    #[error("`keys` row is {len} bytes, shorter than the {expected}-byte predicate prefix")]
    ShortKeyRow { len: usize, expected: usize },

    #[error("scan bound is {len} bytes, shorter than the {expected}-byte predicate prefix")]
    ShortScanBound { len: usize, expected: usize },

    /// An id that could not be minted — [`FactIdError`], raised without a store
    /// in reach and surfaced here when a store is the one that hit it.
    #[error("{0}")]
    Id(#[from] FactIdError),

    /// A fact written under one predicate with an id tagged for another. The id
    /// routes `point()`, so accepting it would file the fact where no query looks.
    #[error("fact id {found:?} is tagged predicate {} but the fact is {expected:?}", found.predicate().0)]
    FactIdPredicateMismatch {
        expected: PredicateId,
        found: FactId,
    },

    /// A second, *differing* fact offered for a key that already holds one.
    ///
    /// A key maps to exactly one fact, so the alternative to refusing is to
    /// overwrite the `keys` row and strand the first fact's entity — a fact no
    /// query can reach, and one no bijection check can attribute to anything
    /// ([I12](../../docs/invariants.md#i12)). Last-writer-wins is the one outcome
    /// an immutable store cannot have.
    ///
    /// A byte-identical fact is *not* this: it dedups to the id already there,
    /// which is the merge frontier's rule for the same situation
    /// ([operations §5](../../docs/aperture-cli-design.md)).
    #[error(
        "{predicate:?} already holds a different fact keyed the same way, as {existing:?}; \
         a key is written once"
    )]
    KeyAlreadyWritten {
        predicate: PredicateId,
        existing: FactId,
    },

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
    /// ([I13](../../../docs/invariants.md#i13)), so a schema that does not survive the
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

    /// Stored bytes that do not decode.
    ///
    /// Distinct from [`FactError::Codec`], which is the same fault on the way *in*:
    /// this is bytes already on our own disk failing to be what they claim, which is
    /// corruption rather than a caller's mistake.
    #[error("stored bytes do not decode: {0}")]
    Corrupt(#[from] StoreCodecError),
}

/// A fault in a **write**: a fact that does not fit the schema it is being written
/// under. Distinct from [`StoreCodecError`], which is bytes that do not decode — this
/// is a well-formed value in the wrong shape, caught before any bytes exist.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FactError {
    #[error("no predicate called `{0}`")]
    UnknownPredicate(String),

    #[error("`{predicate}` declares no field called `{field}`")]
    UnknownField { predicate: String, field: String },

    #[error("`{predicate}` declares a field `{field}` that this fact does not set")]
    MissingField { predicate: String, field: String },

    #[error("`{predicate}` expects {expected} here, but this fact offers {got}")]
    TypeMismatch {
        predicate: String,
        expected: String,
        got: String,
    },

    #[error("`{0}` has no value side, but this fact offers one")]
    UnexpectedValue(String),

    #[error("`{0}` declares a value side, but this fact offers none")]
    MissingValue(String),

    #[error("{0}")]
    Codec(#[from] StoreCodecError),
}
