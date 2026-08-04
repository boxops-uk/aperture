use thiserror::Error;

use crate::focus::{iter::Address, plan::FactId, schema::Symbol};

#[derive(Debug, Error)]
pub enum ApertureError {
    #[error("decode error: {0}")]
    DecodeError(#[from] StoreCodecError),

    #[error("variable at address 0x{0:016x} used before it was bound")]
    UseBeforeBind(Address),

    #[error("address 0x{0:016x} out of bounds")]
    AddressOutOfBounds(Address),

    #[error("advance of closed frame")]
    AdvanceAfterClose,

    #[error("resume key not found")]
    BadResumeKey,

    #[error("dangling fact id {0:?}: key present but no entity in the `entities` column family")]
    DanglingFactId(FactId),

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
pub enum StoreError {
    #[error("fjall: {0}")]
    Backend(#[from] fjall::Error),

    #[error("`keys` row value is {len} bytes, not an {expected}-byte fact id")]
    FactIdWidth { len: usize, expected: usize },

    #[error("`entities` row for {0:?} is truncated")]
    TruncatedEntity(FactId),

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
        expected: crate::focus::schema::PredicateId,
        found: FactId,
    },
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
