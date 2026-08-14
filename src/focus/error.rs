use thiserror::Error;

use crate::focus::{
    iter::Address,
    plan::FactId,
    schema::{PredicateId, Symbol},
};

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApertureError {
    #[error("decode error: {0}")]
    Decode(#[from] StoreCodecError),

    #[error("cannot write this fact: {0}")]
    Fact(#[from] FactError),

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

    #[error("dangling fact id {0:?}: key present but no entity in the `entities` column family")]
    DanglingFactId(FactId),

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

    #[error("unknown symbol: {0:?}")]
    UnknownSymbol(Symbol),

    #[error("store error: {0}")]
    Store(#[from] StoreError),
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

    /// A predicate id too wide for the [`FactId`] tag. Reachable only from a
    /// schema; the check lives here because the fact-id layout is what it breaks.
    #[error("predicate id {predicate} does not fit the {max}-max fact-id tag")]
    PredicateIdTooWide { predicate: u32, max: u32 },

    /// Sequence 0 is reserved, and the space per predicate is finite: a predicate
    /// that allocates past `max` needs a wider tag split, not a wrapped counter.
    #[error("fact-id sequence {sequence} is outside 1..={max}")]
    FactIdSequence { sequence: u64, max: u64 },

    /// A fact written under one predicate with an id tagged for another. The id
    /// routes `point()`, so accepting it would file the fact where no query looks.
    #[error("fact id {found:?} is tagged predicate {} but the fact is {expected:?}", found.predicate().0)]
    FactIdPredicateMismatch {
        expected: PredicateId,
        found: FactId,
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

    #[error("integer overflow")]
    Overflow,

    #[error("integer underflow")]
    Underflow,
}
