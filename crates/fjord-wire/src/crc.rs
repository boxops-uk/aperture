//! CRC-32 (IEEE 802.3), for a block's integrity check.
//!
//! Hand-rolled rather than pulled in, for the reason the rest of the codec is: it is
//! forty lines and a table, and the alternative is a dependency in the path every
//! byte of every ingest passes through. The polynomial is the ubiquitous one —
//! `0xEDB88320` reflected — so a block's checksum is the same number zlib, PNG, gzip
//! and Avro would compute, which matters the day someone checks a file with another
//! tool.
//!
//! It is a **corruption** check and not a security one. A peer that wants to send a
//! block that lies can recompute the checksum; what this catches is a flipped bit, a
//! truncated write, and — the case it is really here for — a
//! [resynchronisation](crate::block) candidate that turned out to be data rather
//! than a block header.

/// The reflected IEEE polynomial.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// Byte-at-a-time table, built once at first use.
///
/// `LazyLock` rather than a `const` table so the polynomial stays visible as the
/// definition rather than as 256 magic numbers no reader can check.
static TABLE: std::sync::LazyLock<[u32; 256]> = std::sync::LazyLock::new(|| {
    let mut table = [0u32; 256];

    for (index, entry) in table.iter_mut().enumerate() {
        let mut value = index as u32;
        for _ in 0..8 {
            value = if value & 1 == 1 {
                (value >> 1) ^ POLYNOMIAL
            } else {
                value >> 1
            };
        }
        *entry = value;
    }

    table
});

/// The CRC-32 of `bytes`.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    update(0xFFFF_FFFF, bytes) ^ 0xFFFF_FFFF
}

/// Fold more bytes into a running CRC, so a block's checksum can cover its header
/// and its payload without joining the two into one buffer.
#[must_use]
pub fn update(mut crc: u32, bytes: &[u8]) -> u32 {
    let table = &*TABLE;

    for &byte in bytes {
        crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }

    crc
}

/// Start a running CRC. Pair with [`update`] and [`finish`].
#[must_use]
pub const fn start() -> u32 {
    0xFFFF_FFFF
}

#[must_use]
pub const fn finish(crc: u32) -> u32 {
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The check value every CRC-32 implementation publishes.** `"123456789"`
    /// hashes to `0xCBF43926` under IEEE 802.3, which is what says this is *that*
    /// CRC-32 and not a plausible-looking variant with the wrong polynomial,
    /// reflection or initial value — the four ways a hand-rolled CRC goes subtly
    /// wrong and still looks like it works.
    #[test]
    fn the_standard_check_vector_matches() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// The empty input is zero, and a single zero byte is not — the degenerate pair
    /// that catches an implementation that forgot to invert.
    #[test]
    fn the_edges_are_the_published_ones() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(&[0x00]), 0xD202_EF8D);
    }

    /// The streaming form has to agree with the one-shot form, or a block's checksum
    /// would depend on whether its header and payload were folded together or
    /// joined first.
    #[test]
    fn folding_in_pieces_equals_hashing_the_whole() {
        let whole = b"the quick brown fox jumps over the lazy dog";

        for split in 0..whole.len() {
            let (head, tail) = whole.split_at(split);
            let piecewise = finish(update(update(start(), head), tail));
            assert_eq!(piecewise, crc32(whole), "split at {split}");
        }
    }

    /// A single flipped bit changes it — the whole point, stated as a test rather
    /// than assumed of the polynomial.
    #[test]
    fn a_flipped_bit_changes_the_checksum() {
        let clean = b"src.Decl { line = 12 }".to_vec();
        let base = crc32(&clean);

        for index in 0..clean.len() {
            for bit in 0..8 {
                let mut corrupt = clean.clone();
                corrupt[index] ^= 1 << bit;
                assert_ne!(crc32(&corrupt), base, "byte {index} bit {bit}");
            }
        }
    }
}
