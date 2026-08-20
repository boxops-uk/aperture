//! What a decode can refuse.
//!
//! One type, in the crate that owns the bytes: every variant is either "these
//! bytes are not what the marker says" or "the schema and the bytes disagree".
//! Layers above wrap it — a store surfaces it as a corrupt row, the engine as a
//! decode error — but neither can add to it, which is the point of it living
//! here.

use fjord_schema::schema::Symbol;
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
    /// [`FjordError::ReferenceCrossesPredicate`], raised when a query *follows*
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
    /// [I11]: ../../website/content/invariants.md#i11
    #[error("fact-id sequence 0 is reserved, so these bytes are not a fact reference")]
    ReservedFactId,

    /// A stored union value tagged with a discriminant **no alternative declares**.
    ///
    /// The case [I10](../../website/content/invariants.md#i10) left open: append-only tags do
    /// not make this impossible, because a fact file outlives the schema that wrote
    /// it and a retired alternative's tag is still on disk. Glean answers it with a
    /// synthetic `unknown` alternative, which it can because it projects between
    /// schemas at query time; [I13](../../website/content/invariants.md#i13) leaves nowhere for
    /// such a projection to live, so the honest answer here is a refusal — and per
    /// errors-not-panics it is this variant rather than a mis-decode of whatever
    /// alternative happened to sit at that tag.
    #[error("no alternative of this union is declared with discriminant {tag}")]
    UnknownDiscriminant { tag: u64 },

    /// A union value whose payload does not match the alternative its discriminant
    /// names — the union's answer to [`BadRecord`](StoreCodecError::BadRecord).
    #[error("a union payload does not match the alternative discriminant {tag} names")]
    BadUnion { tag: u64 },

    #[error("integer overflow")]
    Overflow,

    #[error("integer underflow")]
    Underflow,
}
