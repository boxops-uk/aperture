//! A fact's physical row id, and the two rules that make one valid.
//!
//! Its own module because of where it sits: the codec
//! encodes a reference to a fact, the store allocates and
//! routes by one, and the plan carries one — so the type is
//! below all three. Keeping it here is what lets each of them depend on the id
//! without depending on each other.

use serde::{Serialize, Serializer};
use thiserror::Error;

use crate::schema::PredicateId;

/// An id that cannot exist: the two rules [`FactId::new`] enforces.
///
/// Separate from `StoreError` — which wraps
/// it — because minting an id is not a storage operation. A plan builder or a
/// test composes one without a store anywhere in reach, and the fault it can hit
/// is the same one.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FactIdError {
    /// A predicate id too wide for the [`FactId`] tag. Reachable only from a
    /// schema; the check lives here because the fact-id layout is what it breaks.
    #[error("predicate id {predicate} does not fit the {max}-max fact-id tag")]
    PredicateIdTooWide { predicate: u32, max: u32 },

    /// Sequence 0 is reserved, and the space per predicate is finite: a predicate
    /// that allocates past `max` needs a wider tag split, not a wrapped counter.
    #[error("fact-id sequence {sequence} is outside 1..={max}")]
    FactIdSequence { sequence: u64, max: u64 },
}

/// Bits of a [`FactId`] holding the predicate tag — the high three bytes.
///
/// Byte-aligned on purpose: the tag is a *slice* of the big-endian encoding, not a
/// shift, so routing a `point()` to a predicate's tree costs nothing.
pub const FACT_ID_PREDICATE_BITS: u32 = 24;

/// Bits of a [`FactId`] holding the per-predicate sequence — the low five bytes.
pub const FACT_ID_SEQUENCE_BITS: u32 = u64::BITS - FACT_ID_PREDICATE_BITS;

/// Largest predicate id representable in a [`FactId`] tag (~16.7 M predicates).
pub const MAX_TAGGABLE_PREDICATE: u32 = (1 << FACT_ID_PREDICATE_BITS) - 1;

/// Largest per-predicate sequence (~1.1 T facts per predicate).
pub const MAX_FACT_SEQUENCE: u64 = (1 << FACT_ID_SEQUENCE_BITS) - 1;

/// A fact's physical row id: a **snowflake** — the owning predicate in the high
/// [`FACT_ID_PREDICATE_BITS`] bits, a per-predicate sequence in the low
/// [`FACT_ID_SEQUENCE_BITS`] ([I11], [chapter 3]).
///
/// The tag is what lets `entities` be split per predicate exactly as `keys` is:
/// `FactStore::point` is handed a bare id and no predicate, so an untagged id
/// would make identity lookup a search across every predicate's tree. Tagged, it
/// is one lookup in one tree. It also removes the global allocator: each predicate
/// counts its own facts, so two ingest workers on different predicates share no
/// counter and write disjoint, ascending id ranges.
///
/// **Sequence 0 is reserved**, so no valid id is `FactId(0)` and a zeroed or
/// corrupt eight bytes is detectably not a fact — worth having on a path where
/// [I11] is what makes a bytes-only resume cursor safe.
///
/// Uniqueness is structural rather than enforced: the tag partitions the id space,
/// so two predicates cannot collide however their sequences are allocated.
///
/// [I11]: ../../../docs/invariants.md#i11
/// [chapter 3]: ../../../docs/03-storage-model.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FactId(u64);

impl FactId {
    /// The raw eight bytes, for storing or comparing.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Wrap an id that is **already known to be valid** — decoded from a stored
    /// row that the decode boundary has checked, or handed back by a model store
    /// that got it from [`FactId::new`].
    ///
    /// The field is private so that [`FactId::new`]'s checks are the only way to
    /// *mint* an id: the tag has to fit and sequence 0 is reserved, which is what
    /// makes a zeroed eight bytes detectably not a fact
    /// ([I11](../../../docs/invariants.md#i11)). Named rather than a tuple
    /// constructor so the places that bypass those checks are greppable.
    #[must_use]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Compose an id from its predicate and sequence.
    ///
    /// # Errors
    ///
    /// [`FactIdError::PredicateIdTooWide`] if the predicate does not fit the tag,
    /// [`FactIdError::FactIdSequence`] if the sequence is 0 (reserved) or past
    /// [`MAX_FACT_SEQUENCE`].
    pub fn new(predicate: PredicateId, sequence: u64) -> Result<Self, FactIdError> {
        if predicate.0 > MAX_TAGGABLE_PREDICATE {
            return Err(FactIdError::PredicateIdTooWide {
                predicate: predicate.0,
                max: MAX_TAGGABLE_PREDICATE,
            });
        }
        if sequence == 0 || sequence > MAX_FACT_SEQUENCE {
            return Err(FactIdError::FactIdSequence {
                sequence,
                max: MAX_FACT_SEQUENCE,
            });
        }

        Ok(Self(
            (u64::from(predicate.0) << FACT_ID_SEQUENCE_BITS) | sequence,
        ))
    }

    /// The predicate that owns this fact.
    #[must_use]
    pub fn predicate(self) -> PredicateId {
        // The shift leaves 24 bits, so the narrowing cannot truncate.
        PredicateId((self.0 >> FACT_ID_SEQUENCE_BITS) as u32)
    }

    /// This fact's sequence within its predicate.
    #[must_use]
    pub fn sequence(self) -> u64 {
        self.0 & MAX_FACT_SEQUENCE
    }
}

impl Serialize for FactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}
