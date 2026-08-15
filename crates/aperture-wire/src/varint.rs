//! LEB128 varints, and zigzag for signed values.
//!
//! The single primitive the transport codec is built out of, and the clearest place
//! to see what "optimise for transmission, not storage" actually buys. The storage
//! codec encodes an integer as a **marker byte carrying the width** followed by a
//! big-endian minimal magnitude, with negatives ones'-complemented, because
//! [I1](../../../docs/invariants.md#i1) requires `memcmp` to *be* semantic order and
//! [I2](../../../docs/invariants.md#i2) requires a value to be skippable without a
//! schema. Both cost bytes, and neither buys anything on a socket: nothing memcmps a
//! frame, and the reader has the schema.
//!
//! So integers are LEB128 over zigzag, which is what Protocol Buffers' `sint64` and
//! Avro's `long` both use, for the same reason both are transmission formats. Seven
//! payload bits per byte, a continuation bit on top; zigzag maps small negatives to
//! small unsigned values (`0, -1, 1, -2` → `0, 1, 2, 3`) so that `-1` costs one byte
//! instead of ten.
//!
//! ```text
//!            storage (order-preserving)      transport (this)
//!      0     MARK_INT_ZERO            1 B    00                  1 B
//!      1     MARK_INT_POS_MIN 01      2 B    02                  1 B
//!     -1     MARK_INT_NEG_MAX FE      2 B    01                  1 B
//!    300     MARK_INT_POS+1 01 2C     3 B    D8 04               2 B
//!  i64::MIN  MARK_INT_NEG_MIN 00×8    9 B    FF FF … 01         10 B
//! ```
//!
//! The trade is visible in the last row and is the right one: a varint is longer
//! than a fixed width at the extremes and shorter everywhere the data actually
//! lives. Line numbers, column numbers, arities, lengths and ids are all small.
//!
//! # Minimality is enforced, and it is not decoration
//!
//! LEB128 admits padding — `0x80 0x00` and `0x00` both decode to zero — so the
//! decoder **rejects any non-minimal encoding**, exactly as the storage codec's
//! canonicalising decoder does. One value, one byte string, both directions.
//!
//! That matters here for a different reason than it does in storage. In storage a
//! second encoding would break order-preservation, which is stated over encodings.
//! Here it would break *identity of a block*: a fact file's block carries a
//! [CRC32](../../../docs/aperture-cli-design.md) and the same encoding is used on
//! the wire and on disk, so "the same facts" has to mean "the same bytes" for a
//! checksum to be worth computing or a block to be comparable at all.

use crate::error::WireError;

/// The most bytes a `u64` varint can occupy: ⌈64/7⌉.
pub const MAX_LEN: usize = 10;

/// Zigzag: map a signed value onto an unsigned one that is small when the signed
/// value is near zero, in either direction.
#[must_use]
#[inline]
pub const fn zigzag(value: i64) -> u64 {
    // Arithmetic shift, so the second operand is all-ones for a negative value and
    // all-zeros for a non-negative one.
    ((value << 1) ^ (value >> 63)) as u64
}

#[must_use]
#[inline]
pub const fn unzigzag(bits: u64) -> i64 {
    ((bits >> 1) as i64) ^ -((bits & 1) as i64)
}

/// Append `value` as a LEB128 varint.
#[inline]
pub fn put_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[inline]
pub fn put_i64(out: &mut Vec<u8>, value: i64) {
    put_u64(out, zigzag(value));
}

/// Read a LEB128 varint, returning it and how many bytes it took.
///
/// Rejects a non-minimal encoding and one that overflows 64 bits. Both are faults
/// rather than best-effort reads: see the module docs for what a second encoding of
/// one value would cost.
pub fn get_u64(bytes: &[u8]) -> Result<(u64, usize), WireError> {
    let mut value: u64 = 0;
    let mut shift = 0;

    for (index, &byte) in bytes.iter().take(MAX_LEN).enumerate() {
        let payload = u64::from(byte & 0x7F);

        // The tenth byte carries only one usable bit, so anything above it would be
        // shifted off the top rather than stored.
        if shift == 63 && payload > 1 {
            return Err(WireError::VarintOverflow);
        }

        value |= payload << shift;

        if byte & 0x80 == 0 {
            // A continuation byte whose payload is zero contributes nothing, so the
            // encoding had a shorter equivalent. Index 0 is exempt: a single `0x00`
            // *is* the minimal encoding of zero.
            if index > 0 && payload == 0 {
                return Err(WireError::VarintNotMinimal);
            }
            return Ok((value, index + 1));
        }

        shift += 7;
    }

    if bytes.len() >= MAX_LEN {
        Err(WireError::VarintOverflow)
    } else {
        Err(WireError::UnexpectedEof)
    }
}

#[inline]
pub fn get_i64(bytes: &[u8]) -> Result<(i64, usize), WireError> {
    let (bits, used) = get_u64(bytes)?;
    Ok((unzigzag(bits), used))
}

/// How many bytes `value` will occupy — without encoding it.
///
/// Used to size a buffer and to state the codec's length properties as arithmetic
/// rather than as a measurement of the encoder against itself.
#[must_use]
pub const fn len_u64(value: u64) -> usize {
    // 7 payload bits per byte, and zero still needs one.
    let bits = 64 - value.leading_zeros() as usize;
    if bits == 0 { 1 } else { bits.div_ceil(7) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn zigzag_maps_small_magnitudes_to_small_values() {
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(-2), 3);
        assert_eq!(zigzag(i64::MIN), u64::MAX);
    }

    /// The claim the module docs make in a table, as arithmetic: everything a code
    /// index actually stores in an integer field — a line, a column, an arity — is
    /// one byte, and a negative of the same size is too.
    #[test]
    fn a_small_integer_costs_one_byte() {
        for value in -64..64i64 {
            let mut out = vec![];
            put_i64(&mut out, value);
            assert_eq!(out.len(), 1, "{value} should be one byte, got {out:?}");
        }

        // And the boundary is where seven payload bits run out, not somewhere else.
        for value in [64i64, -65] {
            let mut out = vec![];
            put_i64(&mut out, value);
            assert_eq!(out.len(), 2, "{value}");
        }
    }

    /// A shorter equivalent is refused rather than normalised, so that one value has
    /// exactly one encoding. Every padded form of zero, and a padded form of a value
    /// that genuinely needs two bytes.
    #[test]
    fn a_non_minimal_varint_is_refused() {
        for padded in [
            vec![0x80, 0x00],
            vec![0x80, 0x80, 0x00],
            vec![0x81, 0x80, 0x00],
        ] {
            assert_eq!(
                get_u64(&padded),
                Err(WireError::VarintNotMinimal),
                "{padded:02x?}"
            );
        }
    }

    /// Ten bytes is the most a `u64` can need, and the tenth carries one bit. A
    /// continuation past it, or a tenth byte claiming more, is an overflow rather
    /// than a wrapped value.
    #[test]
    fn an_overlong_varint_is_refused() {
        assert_eq!(get_u64(&[0xFF; 11]), Err(WireError::VarintOverflow));
        assert_eq!(
            get_u64(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02]),
            Err(WireError::VarintOverflow)
        );

        // u64::MAX itself is exactly ten bytes and must still read.
        let mut out = vec![];
        put_u64(&mut out, u64::MAX);
        assert_eq!(out.len(), MAX_LEN);
        assert_eq!(get_u64(&out), Ok((u64::MAX, MAX_LEN)));
    }

    #[test]
    fn a_truncated_varint_is_eof_not_a_value() {
        assert_eq!(get_u64(&[]), Err(WireError::UnexpectedEof));
        assert_eq!(get_u64(&[0x80]), Err(WireError::UnexpectedEof));
    }

    proptest! {
        #[test]
        fn a_varint_round_trips(value in any::<u64>()) {
            let mut out = vec![];
            put_u64(&mut out, value);
            prop_assert_eq!(get_u64(&out), Ok((value, out.len())));
        }

        #[test]
        fn zigzag_round_trips(value in any::<i64>()) {
            prop_assert_eq!(unzigzag(zigzag(value)), value);

            let mut out = vec![];
            put_i64(&mut out, value);
            prop_assert_eq!(get_i64(&out), Ok((value, out.len())));
        }

        /// `len_u64` is a *prediction*, so it has to agree with the encoder without
        /// running it — that is what lets the codec state its sizes as arithmetic.
        #[test]
        fn the_predicted_length_is_the_encoded_length(value in any::<u64>()) {
            let mut out = vec![];
            put_u64(&mut out, value);
            prop_assert_eq!(len_u64(value), out.len());
        }

        /// **Encoding is canonical**: whatever the decoder accepts, re-encoding
        /// reproduces byte for byte. With minimality enforced this is the bijection
        /// a block checksum rests on.
        #[test]
        fn decoding_and_re_encoding_is_the_identity(bytes in prop::collection::vec(any::<u8>(), 1..12)) {
            if let Ok((value, used)) = get_u64(&bytes) {
                let mut out = vec![];
                put_u64(&mut out, value);
                prop_assert_eq!(&out[..], &bytes[..used]);
            }
        }
    }
}
