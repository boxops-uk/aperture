use thiserror::Error;

use crate::focus::{
    format::FormatVersion,
    id::{FactId, FactIdError},
    plan::{Address, PlanFingerprint},
    schema::{PredicateId, Symbol},
};

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApertureError {
    #[error("decode error: {0}")]
    Decode(#[from] StoreCodecError),

    #[error("{0} was read before anything was bound to it")]
    UseBeforeBind(Address),

    #[error("{address} holds {held} where the plan wanted {wanted}")]
    SlotKindMismatch {
        address: Address,
        wanted: &'static str,
        held: &'static str,
    },

    #[error("{0} is not a register in this plan")]
    AddressOutOfBounds(Address),

    #[error("advance of closed frame")]
    AdvanceAfterClose,

    /// A resume cursor naming more levels than the plan has. A
    /// [`Cursor`](crate::focus::iter::Cursor) is bytes-only and rebuilt from the
    /// wire, so a cursor that does not match the plan it is resumed against is
    /// untrusted input, not an impossibility.
    #[error("resume cursor names {cursor} level(s) but the plan has {plan}")]
    CursorPlanMismatch { cursor: usize, plan: usize },

    /// A resume cursor built by a **different build** of the engine.
    ///
    /// Checked before anything is read out of the cursor, because what the version
    /// governs is how to read it: a cursor whose layout this build does not know is
    /// not a cursor it can look inside to find a better diagnostic.
    #[error("resume cursor is version {cursor}; this build reads version {executor}")]
    CursorVersion { cursor: u16, executor: u16 },

    /// A resume cursor built from a **different plan** — the hole the level count
    /// leaves open ([chapter 5](../../docs/05-resume.md)).
    ///
    /// A cursor's entries are paired with the plan's levels *by order*, so two
    /// plans of the same shape over overlapping predicates would accept each
    /// other's cursors and answer from the wrong rows, with only the per-level
    /// `fact_id` check between that and a wrong answer — and that check passes
    /// whenever the saved key exists in the other plan's scan too.
    #[error("resume cursor was built from a different plan ({cursor:?}, not {plan:?})")]
    CursorPlan {
        cursor: PlanFingerprint,
        plan: PlanFingerprint,
    },

    /// A resume cursor naming an alternative the level it is replayed against
    /// does not have — the same untrusted-input case as
    /// [`CursorPlanMismatch`](Self::CursorPlanMismatch), one level down. The
    /// level count matching does not make the sources match, since two plans of
    /// the same shape can disagree about how many alternatives a level has.
    #[error("resume cursor names source {index} of a level with {sources}")]
    CursorSourceOutOfRange { index: usize, sources: usize },

    #[error("resume key not found")]
    BadResumeKey,

    /// A plan stepping *into* a key field that is not a record. The field's own
    /// marker says what it is, so this is a plan disagreeing with the schema the
    /// row was written under — reported rather than read as bytes that happen to
    /// sit there.
    #[error("a plan reads nested field {step} of a key field that is not a record")]
    NotARecord { step: usize },

    /// A plan naming a nested field the record does not have: its terminator came
    /// first. A [`FieldPath`](crate::focus::plan::FieldPath) is checked against the
    /// schema when the plan is built, so this is a malformed plan, not a query
    /// answering nothing.
    #[error("a plan reads nested field {step} of a record with fewer fields than that")]
    NestedFieldOutOfRange { step: usize },

    /// A stored reference naming a **different predicate** than the field it sits
    /// in is declared to reference.
    ///
    /// Reported rather than followed, because the row it names would be read
    /// against the declared predicate's key layout: every path in the fetching
    /// level's residuals, and every projection off the register it binds, was
    /// compiled from that layout. Following it anyway decodes another type's bytes
    /// at those offsets and answers with whatever is there.
    #[error(
        "a reference declared to name {expected:?} names {found:?}, whose key has a different shape"
    )]
    ReferenceCrossesPredicate {
        expected: PredicateId,
        found: PredicateId,
    },

    #[error("operation cancelled")]
    Cancelled,

    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

/// A database this build cannot read, decided from its
/// [format stamp](crate::focus::format) before a single row is touched
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

    /// A database this build cannot read — the [format stamp](crate::focus::format)
    /// checked at open, before a row is touched.
    #[error("{0}")]
    Format(#[from] FormatError),

    /// A value that does not fit the schema it is being written under, raised on
    /// the way in by [`fact`](crate::focus::fact).
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

#[derive(Debug, Error)]
pub enum StoreCodecError {
    #[error("unexpected end of input")]
    UnexpectedEof,

    /// A [`Symbol`] the interner cannot resolve, hit while turning stored bytes
    /// into a [`Value`](crate::focus::tuple::Value).
    ///
    /// A codec fault rather than an engine one: symbols are interned per query
    /// and a stored record's field names are read back through that interner, so
    /// the failure belongs to the decode that needed the name, not to whatever
    /// asked for the row.
    #[error("unknown symbol: {0:?}")]
    UnknownSymbol(Symbol),

    #[error("unexpected mark: {0:#x}")]
    UnexpectedMark(u8),

    #[error("unexpected terminator")]
    UnexpectedTerminator,

    #[error("{0}")]
    BadString(#[from] std::str::Utf8Error),

    #[error("bad integer")]
    BadInteger,

    #[error("bad record")]
    BadRecord,

    /// A fact reference naming a **different predicate** than the field it sits in
    /// is declared to reference, caught at the typed codec boundary — the only one
    /// holding both the declared type and the id whose tag answers it.
    ///
    /// The read-path counterpart is
    /// [`ApertureError::ReferenceCrossesPredicate`], raised when a query *follows*
    /// such a reference. Both exist because they catch it at different moments: this
    /// one before the bytes are written, that one for bytes some other writer
    /// produced.
    #[error("a reference declared to name predicate {expected} names predicate {found}")]
    FactRefPredicate { expected: u32, found: u32 },

    /// A fact reference whose sequence is 0, which is reserved so that zeroed or
    /// truncated bytes are detectably not a fact ([I11]). The stored-row decoder
    /// enforces the same rule as [`StoreError::FactIdSequence`]; this is it at the
    /// tuple codec, which is what reads a reference embedded in a key.
    ///
    /// [I11]: ../../docs/invariants.md#i11
    #[error("fact-id sequence 0 is reserved, so these bytes are not a fact reference")]
    ReservedFactId,

    #[error("integer overflow")]
    Overflow,

    #[error("integer underflow")]
    Underflow,
}
