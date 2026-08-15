//! What a decode can refuse.
//!
//! One type, in the crate that owns the bytes: every variant is either "these
//! bytes are not what the marker says" or "the schema and the bytes disagree".
//! Layers above wrap it — a store surfaces it as a corrupt row, the engine as a
//! decode error — but neither can add to it, which is the point of it living
//! here.

use aperture_schema::schema::Symbol;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreCodecError {
    #[error("unexpected end of input")]
    UnexpectedEof,

    /// A [`Symbol`] the interner cannot resolve, hit while turning stored bytes
    /// into a [`Value`](crate::tuple::Value).
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
