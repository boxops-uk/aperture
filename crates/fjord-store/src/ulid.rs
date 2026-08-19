//! A **ULID** — the provisional instance id a database carries until `finish`.
//!
//! 48 bits of millisecond timestamp then 80 bits of randomness, rendered in
//! Crockford base32: 26 characters, case-insensitive, no `I`/`L`/`O`/`U` so it
//! cannot be misread aloud or mistyped into something else valid.
//!
//! **Why not a UUID.** The property wanted here is that instances of one database
//! sort by creation time, because that is the order a person listing a store root
//! wants them in and the order a retention policy would work through. A UUIDv4 sorts
//! randomly; a ULID's leading timestamp makes lexicographic order chronological, and
//! a directory listing gets it for free.
//!
//! **Why not a dependency.** It is thirty lines and one call for entropy, against a
//! crate in the build for a format this file can state completely.
//!
//! It is *provisional* in the sense operations §5 means: content-derived identity can
//! only exist at `finish`, since it hashes the base facts. The directory keeps this
//! name afterwards — renaming under a live server is not worth it — and the content
//! fingerprint goes in the sidecar instead.

/// Crockford base32: no `I`, `L`, `O` or `U`.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Characters in a rendered ULID: 10 for the timestamp, 16 for the randomness.
pub const LEN: usize = 26;

/// A fresh ULID for `now`.
#[must_use]
pub fn new() -> String {
    let mut entropy = [0u8; 10];
    getrandom::fill(&mut entropy).expect("the system entropy source");

    encode(crate::meta::now_ms(), entropy)
}

/// The rendering, given its two halves — separated from [`new`] so it is testable
/// without a clock or an entropy source.
#[must_use]
pub fn encode(timestamp_ms: u64, entropy: [u8; 10]) -> String {
    // 48 bits of timestamp then 80 of randomness, as one 128-bit big-endian value,
    // which is what makes base32 of the whole thing sort chronologically.
    let mut bits = [0u8; 16];
    bits[..6].copy_from_slice(&timestamp_ms.to_be_bytes()[2..]);
    bits[6..].copy_from_slice(&entropy);

    let value = u128::from_be_bytes(bits);
    let mut out = [0u8; LEN];

    // 26 base-32 digits carry 130 bits, so the top digit holds only the leading two
    // — written from the least significant end, which is where the arithmetic is
    // simplest and the padding lands where it belongs.
    for index in (0..LEN).rev() {
        let digit = ((value >> ((LEN - 1 - index) * 5)) & 0x1F) as usize;
        out[index] = ALPHABET[digit];
    }

    String::from_utf8(out.to_vec()).expect("the alphabet is ASCII")
}

/// Whether `text` could be a ULID this module produced.
///
/// Used where a directory name is taken as an instance id: a store root is a
/// filesystem and anything can appear in one, so a name that is not an instance
/// should be skipped rather than read as one.
#[must_use]
pub fn is_valid(text: &str) -> bool {
    text.len() == LEN && text.bytes().all(|byte| ALPHABET.contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ulid_is_twenty_six_valid_characters() {
        let id = new();
        assert_eq!(id.len(), LEN);
        assert!(is_valid(&id), "{id}");
    }

    /// **The property the whole choice is for**: later means lexicographically
    /// greater, so a directory listing is in creation order.
    #[test]
    fn ulids_sort_by_time() {
        let entropy = [0u8; 10];
        let mut previous = encode(0, entropy);

        for timestamp in [1u64, 999, 1_000, 1_700_000_000_000, (1 << 48) - 1] {
            let next = encode(timestamp, entropy);
            assert!(next > previous, "{next} should sort after {previous}");
            previous = next;
        }
    }

    /// Randomness breaks the tie within a millisecond, so two created together are
    /// still distinct — which is what stops a fast loop colliding.
    #[test]
    fn ulids_in_the_same_millisecond_differ() {
        let a = encode(42, [0u8; 10]);
        let mut entropy = [0u8; 10];
        entropy[9] = 1;
        let b = encode(42, entropy);

        assert_ne!(a, b);
        assert_eq!(a[..10], b[..10], "the timestamp halves agree");
    }

    /// Two fresh ids differ, which is the entropy source actually being consulted
    /// rather than the encoding being deterministic.
    #[test]
    fn fresh_ulids_differ() {
        let ids: std::collections::BTreeSet<String> = (0..64).map(|_| new()).collect();
        assert_eq!(ids.len(), 64);
    }

    #[test]
    fn only_crockford_characters_are_valid() {
        assert!(
            !is_valid("0123456789ABCDEFGHJKMNPQRS"[..25].as_ref()),
            "too short"
        );
        assert!(!is_valid("0123456789ABCDEFGHJKMNPQRSTV"), "too long");
        // I, L, O and U are the ones Crockford leaves out.
        for excluded in ['I', 'L', 'O', 'U'] {
            let text: String = std::iter::repeat_n(excluded, LEN).collect();
            assert!(!is_valid(&text), "{excluded} is not in the alphabet");
        }
    }
}
