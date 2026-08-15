use thiserror::Error;

/// What can go wrong decoding a wire value.
///
/// Every variant is a **decode** fault. Encoding a well-typed value cannot fail:
/// [`WireValue`](crate::value::WireValue) is positional and carries no names, so the
/// only way it can disagree with a [`PredicateTy`](aperture_schema::schema::PredicateTy)
/// is in its shape — which [`TypeMismatch`](Self::TypeMismatch) reports, and which the
/// encoder checks rather than assuming, because the alternative is bytes a peer
/// decodes as something else.
///
/// Named apart from `StoreCodecError` deliberately, and the two must not be merged.
/// A storage decode fault means **corrupt bytes on our own disk**; a wire decode
/// fault means **a peer sent something wrong**, which is an ordinary event on a
/// network and is answered by failing that stream rather than by doubting the
/// database.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    #[error("unexpected end of input")]
    UnexpectedEof,

    /// A varint whose continuation bits run past what a `u64` can hold.
    #[error("varint overflows 64 bits")]
    VarintOverflow,

    /// A varint with a shorter equivalent — `0x80 0x00` for zero, say.
    ///
    /// Rejected rather than accepted-and-normalised, so that one value has exactly
    /// one encoding. See [`varint`](crate::varint) for what rests on that.
    #[error("varint is not minimally encoded")]
    VarintNotMinimal,

    #[error("string is not valid UTF-8")]
    BadString,

    /// A length prefix that runs past the end of the input.
    #[error("declared length {declared} exceeds the {available} bytes remaining")]
    LengthOutOfRange { declared: u64, available: usize },

    /// A union branch index the reader has no branch for.
    #[error("unknown reference form {0}")]
    UnknownRefForm(u64),

    /// A `FactId` that is not one — sequence zero is reserved, so a zeroed or
    /// corrupt eight bytes is detectably not a fact
    /// ([I11](../../../docs/invariants.md#i11)).
    #[error("not a valid fact id: {0}")]
    BadFactId(u64),

    /// The value's shape does not match the type it was encoded or decoded against.
    ///
    /// On the decode side this is reachable only through the schema — a record
    /// arity, say — because the bytes themselves carry no type to disagree with.
    #[error("value does not match its declared type: {0}")]
    TypeMismatch(&'static str),

    /// Bytes left over after a complete value was decoded.
    ///
    /// A fault rather than a nicety: a wire frame declares its own length, so
    /// leftover bytes mean the peer's idea of the type differs from ours, and
    /// ignoring them would decode the *next* value from the wrong offset.
    #[error("{0} bytes remain after decoding")]
    TrailingBytes(usize),

    /// A predicate id the schema does not declare.
    #[error("no predicate {0} in this schema")]
    UnknownPredicate(u32),

    // ---- framing ----------------------------------------------------------
    /// A block was expected here and its sync marker is not present.
    #[error("no sync marker at this offset")]
    NoSyncMarker,

    /// The marker was there and the header behind it is not a block's.
    ///
    /// The cheap half of validating a resynchronisation candidate: four bytes,
    /// before a checksum is computed over anything.
    #[error("not a block header")]
    BadMagic,

    /// A block's checksum disagrees with its bytes — a torn write, a flipped bit,
    /// or a file cut mid-block.
    #[error("checksum mismatch: header declares {declared:#010x}, bytes give {computed:#010x}")]
    ChecksumMismatch { declared: u32, computed: u32 },

    /// A count or a length past what a block or frame may carry.
    ///
    /// Bounded because both size a read, and in a naive reader an allocation, from
    /// a number a peer chose. The storage codec never has to think about this; its
    /// bytes come from our own disk.
    #[error("{what}: {declared} exceeds the maximum of {max}")]
    BlockTooLarge {
        what: &'static str,
        declared: u64,
        max: u64,
    },
}
