//! The **format stamp** — what a DB says about the encoding that wrote it.
//!
//! [I3](../../docs/invariants.md#i3) freezes the marker table, and the reason it
//! had to hold *forever* rather than *until a migration* was that nothing a reader
//! is handed says which encoding wrote it: a migration presupposes detection.
//! This module is that detection ([I15](../../docs/invariants.md#i15)) — a fixed
//! block written once at create, in the DB's own metadata keyspace, checked at
//! every open.
//!
//! **Two numbers, not one**, because two different things are frozen and they
//! change for different reasons:
//!
//! - [`FormatVersion::codec`] covers the **tuple codec** — the marker table and
//!   the encoding of each type ([chapter 2](../../docs/02-tuple-codec.md)). A new
//!   type's marker moves this.
//! - [`FormatVersion::storage`] covers the **physical layout** — how a row is
//!   framed in each column family, how a keyspace is named, and the `FactId`
//!   split ([chapter 3](../../docs/03-storage-model.md)). Rows changing shape
//!   moves this.
//!
//! A codec addition does not reshape a row and a layout change does not touch the
//! markers, so one number would force a reader to refuse a DB over a change that
//! cannot affect it, and would leave the diagnostic unable to say which half it
//! failed to understand.
//!
//! **The rule is equality, deliberately.** A reader accepts exactly the versions it
//! writes. "Readable up to N" is the plausible refinement — the marker table is
//! append-only, so a newer reader *could* read older bytes — but it is a promise
//! about every past encoding, and it can be added additively once there is a past
//! encoding to make it about. Refusing is the answer that cannot be silently
//! wrong.

use std::fmt;

use crate::focus::error::FormatError;

/// The keyspace holding database-level metadata — the stamp today, the embedded
/// schema when [I13](../../docs/invariants.md#i13) lands.
///
/// Not a predicate keyspace: [`FjallDb::open`](crate::focus::store::FjallDb::open)
/// recovers predicates by the `keys.`/`entities.` prefixes, which this name does
/// not carry, so it is invisible to that walk.
pub const META_KEYSPACE: &str = "meta";

/// The stamp's key within [`META_KEYSPACE`].
pub const FORMAT_KEY: &[u8] = b"format";

/// Leading bytes of the stamp: enough to say "not an Aperture database" for a
/// directory that is something else, rather than reading two arbitrary `u16`s out
/// of it.
const MAGIC: &[u8; 8] = b"APERTURE";

/// Width of the encoded stamp: the magic, then the two versions big-endian.
pub const BLOCK_LEN: usize = MAGIC.len() + 2 * size_of::<u16>();

/// Which encoding wrote a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatVersion {
    /// The tuple codec — markers and per-type encodings.
    pub codec: u16,
    /// The physical layout — row framing, keyspace naming, the `FactId` split.
    pub storage: u16,
}

impl FormatVersion {
    /// What this build writes, and the only thing it reads.
    ///
    /// Both start at 1 rather than 0 so that a zeroed block — the shape a
    /// truncated file or an uninitialised buffer takes — is not a valid version,
    /// the same reason sequence 0 is reserved in a
    /// [`FactId`](crate::focus::id::FactId).
    pub const CURRENT: Self = Self {
        codec: 1,
        storage: 1,
    };

    /// The stamp as it is stored.
    #[must_use]
    pub fn encode(self) -> [u8; BLOCK_LEN] {
        let mut out = [0u8; BLOCK_LEN];
        out[..MAGIC.len()].copy_from_slice(MAGIC);
        out[MAGIC.len()..MAGIC.len() + 2].copy_from_slice(&self.codec.to_be_bytes());
        out[MAGIC.len() + 2..].copy_from_slice(&self.storage.to_be_bytes());
        out
    }

    /// Read a stamp back.
    ///
    /// # Errors
    ///
    /// [`FormatError::BadMagic`] if the block is not a stamp at all, and
    /// [`FormatError::Truncated`] if it is too short to hold one. Length is checked
    /// **exactly**: a longer block is a later format's, and a later format is
    /// something this build has already decided it cannot read.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() != BLOCK_LEN {
            return Err(FormatError::Truncated {
                len: bytes.len(),
                expected: BLOCK_LEN,
            });
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            let mut found = [0u8; MAGIC.len()];
            found.copy_from_slice(&bytes[..MAGIC.len()]);
            return Err(FormatError::BadMagic { found });
        }

        // Both slices are `BLOCK_LEN`-bounded above, so neither conversion can fail.
        let two = |at: usize| -> u16 {
            u16::from_be_bytes(
                bytes[at..at + 2]
                    .try_into()
                    .expect("a two-byte window of a fixed-width block"),
            )
        };

        Ok(Self {
            codec: two(MAGIC.len()),
            storage: two(MAGIC.len() + 2),
        })
    }

    /// Whether this build can read a database stamped `self`.
    ///
    /// # Errors
    ///
    /// [`FormatError::Unreadable`], naming both versions, when either number
    /// differs from [`FormatVersion::CURRENT`].
    pub fn check_readable(self) -> Result<(), FormatError> {
        if self == Self::CURRENT {
            Ok(())
        } else {
            Err(FormatError::Unreadable {
                found: self,
                current: Self::CURRENT,
            })
        }
    }
}

impl fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "codec {}, storage {}", self.codec, self.storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp round-trips, and its width is fixed — the number the length check
    /// in [`FormatVersion::decode`] is entitled to insist on.
    #[test]
    fn a_stamp_round_trips_at_a_fixed_width() {
        let stamped = FormatVersion {
            codec: 7,
            storage: 9,
        };
        let bytes = stamped.encode();

        assert_eq!(bytes.len(), BLOCK_LEN);
        assert_eq!(
            FormatVersion::decode(&bytes).expect("a stamp this build wrote"),
            stamped
        );
    }

    /// **Bytes that are not a stamp are refused, not parsed.** Twelve bytes of
    /// something else would otherwise decode as a plausible version pair and either
    /// be accepted or reported as the wrong problem.
    #[test]
    fn bytes_that_are_not_a_stamp_are_refused() {
        let mut bytes = FormatVersion::CURRENT.encode();
        bytes[0] = b'X';

        assert!(
            matches!(
                FormatVersion::decode(&bytes),
                Err(FormatError::BadMagic { .. })
            ),
            "a bad magic must be reported as one",
        );
    }

    /// A block of the wrong width is refused in **both** directions: short is
    /// truncation, long is a later format that has appended something this build
    /// does not know is there.
    #[test]
    fn a_block_of_the_wrong_width_is_refused() {
        let bytes = FormatVersion::CURRENT.encode();

        for len in [0, BLOCK_LEN - 1] {
            assert!(
                matches!(
                    FormatVersion::decode(&bytes[..len]),
                    Err(FormatError::Truncated { .. })
                ),
                "a {len}-byte block must be reported as truncated",
            );
        }

        let mut longer = bytes.to_vec();
        longer.push(0);

        assert!(
            matches!(
                FormatVersion::decode(&longer),
                Err(FormatError::Truncated { .. })
            ),
            "a longer block is a later format's, and is not read",
        );
    }

    /// **Either number differing is a refusal.** Two numbers exist so that a reader
    /// can say which half it does not understand; a check that only looked at one
    /// would read a DB whose rows are framed differently.
    #[test]
    fn either_version_differing_is_unreadable() {
        let current = FormatVersion::CURRENT;

        assert!(
            current.check_readable().is_ok(),
            "a build must read what it writes",
        );

        for other in [
            FormatVersion {
                codec: current.codec + 1,
                ..current
            },
            FormatVersion {
                storage: current.storage + 1,
                ..current
            },
        ] {
            assert!(
                matches!(other.check_readable(), Err(FormatError::Unreadable { .. })),
                "{other} must be unreadable to a build writing {current}",
            );
        }
    }

    /// A zeroed block — a truncated file, an uninitialised buffer — is not a valid
    /// version, which is why both numbers start at 1.
    #[test]
    fn a_zeroed_version_is_not_current() {
        let zeroed = FormatVersion {
            codec: 0,
            storage: 0,
        };

        assert!(zeroed.check_readable().is_err());
    }
}
