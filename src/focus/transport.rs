use std::borrow::Cow;

use crate::focus::{error::StoreCodecError, plan::FactId};

const MARK_NULL: u8 = 0x00;

const MARK_STRING: u8 = 0x21;
const MARK_RECORD: u8 = 0x22;

const MARK_INT_NEG_MIN: u8 = 0x40;
const MARK_INT_NEG_MAX: u8 = 0x47;
const MARK_INT_ZERO: u8 = 0x48;
const MARK_INT_POS_MIN: u8 = 0x49;
const MARK_INT_POS_MAX: u8 = 0x50;

const MARK_TERM: u8 = 0x00;
const MARK_ESCAPE: u8 = 0xFF;

const NULL: u8 = 0x00;

#[inline]
pub fn int_width(mag: u64) -> usize {
    8 - (mag.leading_zeros() / 8) as usize
}

pub fn put_i64(out: &mut Vec<u8>, val: i64) {
    if val == 0 {
        out.push(MARK_INT_ZERO);
        return;
    }

    let mag = val.unsigned_abs();
    let width = int_width(mag);

    let mark = if val > 0 {
        MARK_INT_ZERO + width as u8
    } else {
        MARK_INT_ZERO - width as u8
    };

    let bytes = if val > 0 {
        mag.to_be_bytes()
    } else {
        (!mag).to_be_bytes()
    };

    out.push(mark);
    out.extend_from_slice(&bytes[8 - width..]);
}

pub fn get_i64(bytes: &[u8]) -> Result<(i64, usize), StoreCodecError> {
    let mark = *bytes.first().ok_or(StoreCodecError::UnexpectedEof)?;

    if mark == MARK_INT_ZERO {
        return Ok((0, 1));
    }

    match mark {
        MARK_INT_POS_MIN..=MARK_INT_POS_MAX => {
            let width = (mark - MARK_INT_ZERO) as usize;
            let contents = bytes
                .get(1..1 + width)
                .ok_or(StoreCodecError::UnexpectedEof)?;

            let mut buf = [0u8; 8];
            buf[8 - width..].copy_from_slice(contents);

            let mag = u64::from_be_bytes(buf);

            if int_width(mag) != width {
                return Err(StoreCodecError::BadInteger);
            }

            if mag > i64::MAX as u64 {
                return Err(StoreCodecError::Overflow);
            }

            Ok((mag as i64, width + 1))
        }

        MARK_INT_NEG_MIN..=MARK_INT_NEG_MAX => {
            let width = (MARK_INT_ZERO - mark) as usize;
            let contents = bytes
                .get(1..1 + width)
                .ok_or(StoreCodecError::UnexpectedEof)?;

            let mut buf = [0u8; 8];
            buf[8 - width..].copy_from_slice(contents);

            let encoded = u64::from_be_bytes(buf);

            let mask = if width == 8 {
                u64::MAX
            } else {
                (1u64 << (width * 8)) - 1
            };

            let mag = (!encoded) & mask;

            if int_width(mag) != width {
                return Err(StoreCodecError::BadInteger);
            }

            if mag > (1u64 << 63) {
                return Err(StoreCodecError::Underflow);
            }

            let val = if mag == (1u64 << 63) {
                i64::MIN
            } else {
                -(mag as i64)
            };

            Ok((val, width + 1))
        }

        _ => Err(StoreCodecError::UnexpectedMark(mark)),
    }
}

#[inline]
pub fn put_u64(out: &mut Vec<u8>, val: u64) {
    if val == 0 {
        out.push(MARK_INT_ZERO);
        return;
    }
    let width = int_width(val);
    let be = val.to_be_bytes();
    out.push(MARK_INT_ZERO + width as u8);
    out.extend_from_slice(&be[8 - width..]);
}

pub fn get_u64(bytes: &[u8]) -> Result<(u64, usize), StoreCodecError> {
    let mark = *bytes.first().ok_or(StoreCodecError::UnexpectedEof)?;

    if mark == MARK_INT_ZERO {
        return Ok((0, 1));
    }

    match mark {
        MARK_INT_POS_MIN..=MARK_INT_POS_MAX => {
            let width = (mark - MARK_INT_ZERO) as usize;

            let contents = bytes
                .get(1..1 + width)
                .ok_or(StoreCodecError::UnexpectedEof)?;

            let mut buf = [0u8; 8];
            buf[8 - width..].copy_from_slice(contents);

            let val = u64::from_be_bytes(buf);

            if int_width(val) != width {
                return Err(StoreCodecError::BadInteger);
            }

            Ok((val, width + 1))
        }

        _ => Err(StoreCodecError::UnexpectedMark(mark)),
    }
}

fn put_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    use memchr::memchr_iter;

    out.reserve(bytes.len() + 1);

    let mut start = 0;

    for i in memchr_iter(NULL, bytes) {
        out.extend_from_slice(&bytes[start..=i]);
        out.push(MARK_ESCAPE);
        start = i + 1;
    }

    out.extend_from_slice(&bytes[start..]);
    out.push(MARK_TERM);
}

fn get_escaped<'a>(bytes: &'a [u8]) -> Result<(Cow<'a, [u8]>, usize), StoreCodecError> {
    use memchr::memchr;

    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;

    loop {
        let Some(null_idx) = memchr(NULL, &bytes[i..]) else {
            return Err(StoreCodecError::UnexpectedEof);
        };

        let abs_null = i + null_idx;

        if bytes.get(abs_null + 1) == Some(&MARK_ESCAPE) {
            if out.is_empty() {
                out.reserve(bytes.len() - abs_null);
            }
            out.extend_from_slice(&bytes[start..abs_null]);
            out.push(NULL);

            start = abs_null + 2;
            i = start;
            continue;
        }

        if out.is_empty() {
            return Ok((Cow::Borrowed(&bytes[0..abs_null]), abs_null + 1));
        }

        out.extend_from_slice(&bytes[start..abs_null]);
        return Ok((Cow::Owned(out), abs_null + 1));
    }
}

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    out.push(MARK_STRING);
    put_escaped(out, s.as_bytes());
}

pub fn get_str<'a>(bytes: &'a [u8]) -> Result<(Cow<'a, str>, usize), StoreCodecError> {
    let Some((&mark, contents)) = bytes.split_first() else {
        return Err(StoreCodecError::UnexpectedEof);
    };

    if mark != MARK_STRING {
        return Err(StoreCodecError::UnexpectedMark(mark));
    }

    let (escaped_bytes, consumed) = get_escaped(contents)?;

    match escaped_bytes {
        Cow::Borrowed(b) => {
            let s = std::str::from_utf8(b).map_err(StoreCodecError::BadString)?;
            Ok((Cow::Borrowed(s), consumed + 1))
        }
        Cow::Owned(b) => {
            let s = String::from_utf8(b).map_err(|e| StoreCodecError::BadString(e.utf8_error()))?;
            Ok((Cow::Owned(s), consumed + 1))
        }
    }
}

#[inline]
fn checked_advance(bytes: &[u8], start: usize, n: usize) -> Result<usize, StoreCodecError> {
    let end = start.checked_add(n).ok_or(StoreCodecError::UnexpectedEof)?;

    if end > bytes.len() {
        return Err(StoreCodecError::UnexpectedEof);
    }

    Ok(end)
}

fn skip_terminated(bytes: &[u8], mut start: usize) -> Result<usize, StoreCodecError> {
    use memchr::memchr;

    loop {
        let haystack = bytes.get(start..).ok_or(StoreCodecError::UnexpectedEof)?;

        let Some(rel) = memchr(MARK_TERM, haystack) else {
            return Err(StoreCodecError::UnexpectedEof);
        };

        let i = start + rel;

        if bytes.get(i + 1) == Some(&MARK_ESCAPE) {
            start = i + 2;
        } else {
            return Ok(i + 1);
        }
    }
}

const MAX_RECORD_DEPTH: usize = 256;

pub fn skip(bytes: &[u8], start: usize, nested: bool) -> Result<usize, StoreCodecError> {
    let mut i = start;
    let mut record_depth = 0usize;

    loop {
        if record_depth > 0 {
            let b = *bytes.get(i).ok_or(StoreCodecError::UnexpectedEof)?;

            if b == MARK_TERM && bytes.get(i + 1) != Some(&MARK_ESCAPE) {
                i += 1;
                record_depth -= 1;

                if record_depth == 0 {
                    return Ok(i);
                }

                continue;
            }
        }

        let mark = *bytes.get(i).ok_or(StoreCodecError::UnexpectedEof)?;
        let after_mark = i + 1;

        match mark {
            MARK_NULL => {
                if nested || record_depth > 0 {
                    if bytes.get(after_mark) != Some(&MARK_ESCAPE) {
                        return Err(StoreCodecError::UnexpectedTerminator);
                    }

                    i = after_mark + 1;
                } else {
                    i = after_mark;
                }

                if record_depth == 0 {
                    return Ok(i);
                }
            }

            MARK_STRING => {
                i = skip_terminated(bytes, after_mark)?;

                if record_depth == 0 {
                    return Ok(i);
                }
            }

            MARK_RECORD => {
                i = after_mark;

                if record_depth == MAX_RECORD_DEPTH {
                    return Err(StoreCodecError::BadRecord);
                }

                record_depth += 1;
            }

            MARK_INT_NEG_MIN..=MARK_INT_NEG_MAX => {
                let width = (MARK_INT_ZERO - mark) as usize;
                i = checked_advance(bytes, after_mark, width)?;

                if record_depth == 0 {
                    return Ok(i);
                }
            }

            MARK_INT_ZERO => {
                i = after_mark;

                if record_depth == 0 {
                    return Ok(i);
                }
            }

            MARK_INT_POS_MIN..=MARK_INT_POS_MAX => {
                let width = (mark - MARK_INT_ZERO) as usize;
                i = checked_advance(bytes, after_mark, width)?;

                if record_depth == 0 {
                    return Ok(i);
                }
            }

            other => return Err(StoreCodecError::UnexpectedMark(other)),
        }
    }
}

pub fn strinc(prefix: &[u8]) -> Option<Vec<u8>> {
    let i = prefix.iter().rposition(|&b| b != 0xFF)?;

    let mut out = prefix[..=i].to_vec();
    out[i] += 1;

    Some(out)
}

pub fn field_range(key: &[u8], idx: usize) -> Result<std::ops::Range<usize>, StoreCodecError> {
    let mut at = 0;
    for _ in 0..idx {
        at = skip(key, at, false)?;
    }
    let end = skip(key, at, false)?;
    Ok(at..end)
}

#[derive(Debug, Clone)]
pub enum OutValue {
    Int(i64),
    Str(String),
    FactId(FactId),
    Record(Box<[OutValue]>),
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_i64_rejects_positive_overflow() {
        let bytes = [
            MARK_INT_POS_MAX,
            0x80,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];

        assert!(matches!(get_i64(&bytes), Err(StoreCodecError::Overflow)));
    }

    #[test]
    fn test_i64_rejects_negative_underflow() {
        let bytes = [
            MARK_INT_NEG_MIN,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];

        assert!(matches!(get_i64(&bytes), Err(StoreCodecError::Underflow)));
    }

    #[test]
    fn test_i64_rejects_noncanonical_positive_zero() {
        let bytes = [MARK_INT_POS_MIN, 0x00];

        assert!(matches!(get_i64(&bytes), Err(StoreCodecError::BadInteger)));
    }

    #[test]
    fn test_i64_rejects_noncanonical_positive_width() {
        let bytes = [MARK_INT_ZERO + 2, 0x00, 0x01];

        assert!(matches!(get_i64(&bytes), Err(StoreCodecError::BadInteger)));
    }

    #[test]
    fn test_i64_rejects_noncanonical_negative_width() {
        let bytes = [MARK_INT_ZERO - 2, 0xff, 0xfe];

        assert!(matches!(get_i64(&bytes), Err(StoreCodecError::BadInteger)));
    }

    #[test]
    fn test_i64_min_is_valid() {
        let mut buf = Vec::new();
        put_i64(&mut buf, i64::MIN);

        let (decoded, consumed) = get_i64(&buf).unwrap();

        assert_eq!(decoded, i64::MIN);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_u64_rejects_noncanonical_zero() {
        let bytes = [MARK_INT_POS_MIN, 0x00];

        assert!(matches!(get_u64(&bytes), Err(StoreCodecError::BadInteger)));
    }

    #[test]
    fn test_u64_rejects_noncanonical_width() {
        let bytes = [MARK_INT_ZERO + 2, 0x00, 0x01];

        assert!(matches!(get_u64(&bytes), Err(StoreCodecError::BadInteger)));
    }

    #[test]
    fn test_u64_rejects_negative_mark() {
        let bytes = [MARK_INT_ZERO - 1, 0xfe];

        assert!(matches!(
            get_u64(&bytes),
            Err(StoreCodecError::UnexpectedMark(_))
        ));
    }

    #[test]
    fn test_u64_max_is_valid() {
        let mut buf = Vec::new();
        put_u64(&mut buf, u64::MAX);

        let (decoded, consumed) = get_u64(&buf).unwrap();

        assert_eq!(decoded, u64::MAX);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_skip_empty_record() {
        let buf = vec![MARK_RECORD, MARK_TERM];

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_record_with_i64() {
        let mut buf = Vec::new();

        buf.push(MARK_RECORD);
        put_i64(&mut buf, 123);
        buf.push(MARK_TERM);

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_record_with_nested_null() {
        let buf = vec![MARK_RECORD, MARK_NULL, MARK_ESCAPE, MARK_TERM];
        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_nested_records() {
        let mut buf = Vec::new();

        buf.push(MARK_RECORD);

        put_i64(&mut buf, 1);

        buf.push(MARK_RECORD);
        put_i64(&mut buf, 2);
        buf.push(MARK_TERM);

        put_i64(&mut buf, 3);

        buf.push(MARK_TERM);

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_bad_record() {
        let depth = MAX_RECORD_DEPTH + 1;
        let mut buf = Vec::new();

        for _ in 0..depth {
            buf.push(MARK_RECORD);
        }

        for _ in 0..depth {
            buf.push(MARK_TERM);
        }

        let end = skip(&buf, 0, false);

        assert!(matches!(end, Err(StoreCodecError::BadRecord)));
    }

    #[test]
    fn test_skip_nested_bare_null_is_terminator() {
        let buf = vec![MARK_RECORD, MARK_NULL];
        let end = skip(&buf, 0, false).unwrap();
        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_nested_null_requires_escape_when_called_directly() {
        let buf = vec![MARK_NULL];

        assert!(matches!(
            skip(&buf, 0, true),
            Err(StoreCodecError::UnexpectedTerminator)
        ));
    }

    #[test]
    fn test_str_empty_encoding() {
        let mut buf = Vec::new();

        put_str(&mut buf, "");

        assert_eq!(buf, vec![MARK_STRING, MARK_TERM]);

        let (decoded, consumed) = get_str(&buf).unwrap();

        assert_eq!(decoded, "");
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_str_single_nul_encoding() {
        let mut buf = Vec::new();

        put_str(&mut buf, "\0");

        assert_eq!(buf, vec![MARK_STRING, MARK_NULL, MARK_ESCAPE, MARK_TERM,]);

        let (decoded, consumed) = get_str(&buf).unwrap();

        assert_eq!(decoded, "\0");
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_str_multiple_nuls_encoding() {
        let mut buf = Vec::new();

        put_str(&mut buf, "\0\0");

        assert_eq!(
            buf,
            vec![
                MARK_STRING,
                MARK_NULL,
                MARK_ESCAPE,
                MARK_NULL,
                MARK_ESCAPE,
                MARK_TERM,
            ]
        );

        let (decoded, consumed) = get_str(&buf).unwrap();

        assert_eq!(decoded, "\0\0");
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_str_nul_in_middle_encoding() {
        let mut buf = Vec::new();

        put_str(&mut buf, "a\0b");

        assert_eq!(
            buf,
            vec![MARK_STRING, b'a', MARK_NULL, MARK_ESCAPE, b'b', MARK_TERM,]
        );

        let (decoded, consumed) = get_str(&buf).unwrap();

        assert_eq!(decoded, "a\0b");
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_str_nul_ordering_edges() {
        let cases = [
            "", "\0", "\0\0", "\0\0\0", "\0a", "\x01", "a", "a\0", "a\0b",
        ];

        for a in cases {
            for b in cases {
                let mut buf_a = Vec::new();
                let mut buf_b = Vec::new();

                put_str(&mut buf_a, a);
                put_str(&mut buf_b, b);

                assert_eq!(
                    a.cmp(b),
                    buf_a.cmp(&buf_b),
                    "ordering mismatch for {a:?} vs {b:?}: {buf_a:02x?} vs {buf_b:02x?}"
                );
            }
        }
    }

    #[test]
    fn test_skip_string_with_nul() {
        let mut buf = Vec::new();

        put_str(&mut buf, "a\0b\0c");

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_string_rejects_unterminated_escape_sequence() {
        let buf = vec![MARK_STRING, b'a', MARK_NULL, MARK_ESCAPE, b'b'];

        assert!(matches!(
            skip(&buf, 0, false),
            Err(StoreCodecError::UnexpectedEof)
        ));
    }

    #[test]
    fn test_get_str_rejects_unterminated_escape_sequence() {
        let buf = vec![MARK_STRING, b'a', MARK_NULL, MARK_ESCAPE, b'b'];

        assert!(matches!(get_str(&buf), Err(StoreCodecError::UnexpectedEof)));
    }

    #[test]
    fn test_skip_record_with_two_nested_nulls() {
        let buf = vec![
            MARK_RECORD,
            MARK_NULL,
            MARK_ESCAPE,
            MARK_NULL,
            MARK_ESCAPE,
            MARK_TERM,
        ];

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_record_with_null_string_and_nested_null() {
        let mut buf = Vec::new();

        buf.push(MARK_RECORD);

        put_str(&mut buf, "\0");
        buf.push(MARK_NULL);
        buf.push(MARK_ESCAPE);

        buf.push(MARK_TERM);

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_record_bare_null_is_terminator_not_null_value() {
        let buf = vec![MARK_RECORD, MARK_NULL];

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_direct_nested_null_requires_escape() {
        let buf = vec![MARK_NULL];

        assert!(matches!(
            skip(&buf, 0, true),
            Err(StoreCodecError::UnexpectedTerminator)
        ));
    }

    #[test]
    fn test_skip_direct_nested_null_with_escape() {
        let buf = vec![MARK_NULL, MARK_ESCAPE];

        let end = skip(&buf, 0, true).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_nested_record_containing_null() {
        let buf = vec![
            MARK_RECORD,
            MARK_RECORD,
            MARK_NULL,
            MARK_ESCAPE,
            MARK_TERM,
            MARK_TERM,
        ];

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_record_with_string_nul_then_nested_record_null() {
        let mut buf = Vec::new();

        buf.push(MARK_RECORD);

        put_str(&mut buf, "a\0b");

        buf.push(MARK_RECORD);
        buf.push(MARK_NULL);
        buf.push(MARK_ESCAPE);
        buf.push(MARK_TERM);

        buf.push(MARK_TERM);

        let end = skip(&buf, 0, false).unwrap();

        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_skip_record_rejects_unterminated_record() {
        let buf = vec![
            MARK_RECORD,
            MARK_NULL,
            MARK_ESCAPE,
            // missing record MARK_TERM
        ];

        assert!(matches!(
            skip(&buf, 0, false),
            Err(StoreCodecError::UnexpectedEof)
        ));
    }

    #[test]
    fn test_skip_nested_record_rejects_unterminated_inner_record() {
        let buf = vec![
            MARK_RECORD,
            MARK_RECORD,
            MARK_NULL,
            MARK_ESCAPE,
            // missing inner MARK_TERM
            MARK_TERM,
        ];

        assert!(matches!(
            skip(&buf, 0, false),
            Err(StoreCodecError::UnexpectedEof)
        ));
    }

    #[test]
    fn test_strinc_empty() {
        assert_eq!(strinc(b""), None);
    }

    #[test]
    fn test_strinc_all_ff() {
        assert_eq!(strinc(&[0xff]), None);
        assert_eq!(strinc(&[0xff, 0xff]), None);
        assert_eq!(strinc(&[0xff, 0xff, 0xff]), None);
    }

    #[test]
    fn test_strinc_simple_ascii() {
        assert_eq!(strinc(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(strinc(b"abz"), Some(b"ab{".to_vec()));
    }

    #[test]
    fn test_strinc_single_byte() {
        assert_eq!(strinc(&[0x00]), Some(vec![0x01]));
        assert_eq!(strinc(&[0x01]), Some(vec![0x02]));
        assert_eq!(strinc(&[0xfe]), Some(vec![0xff]));
    }

    #[test]
    fn test_strinc_trailing_ff_bytes() {
        assert_eq!(strinc(&[0x01, 0xff]), Some(vec![0x02]));
        assert_eq!(strinc(&[0x01, 0xff, 0xff]), Some(vec![0x02]));
        assert_eq!(strinc(&[0x01, 0x02, 0xff]), Some(vec![0x01, 0x03]));
        assert_eq!(strinc(&[0x01, 0x02, 0xff, 0xff]), Some(vec![0x01, 0x03]));
    }

    #[test]
    fn test_strinc_middle_increment_preserves_prefix() {
        assert_eq!(
            strinc(&[0x10, 0x20, 0x30, 0xff, 0xff]),
            Some(vec![0x10, 0x20, 0x31])
        );
    }

    #[test]
    fn test_strinc_does_not_strip_when_no_trailing_ff() {
        assert_eq!(strinc(&[0x10, 0xff, 0x20]), Some(vec![0x10, 0xff, 0x21]));
    }

    #[test]
    fn test_strinc_binary_edges() {
        assert_eq!(strinc(&[0x00, 0x00, 0xff]), Some(vec![0x00, 0x01]));

        assert_eq!(strinc(&[0x00, 0xff, 0xff]), Some(vec![0x01]));

        assert_eq!(strinc(&[0xfe, 0xff, 0xff]), Some(vec![0xff]));
    }

    proptest! {
        #[test]
        fn test_i64_roundtrip(val in any::<i64>()) {
            let mut buf = Vec::new();
            put_i64(&mut buf, val);
            let (decoded, consumed) = get_i64(&buf).unwrap();
            assert_eq!(val, decoded);
            assert_eq!(consumed, buf.len());
        }

        #[test]
        fn test_u64_roundtrip(val in any::<u64>()) {
            let mut buf = Vec::new();
            put_u64(&mut buf, val);
            let (decoded, consumed) = get_u64(&buf).unwrap();
            assert_eq!(val, decoded);
            assert_eq!(consumed, buf.len());
        }

        #[test]
        fn test_str_roundtrip(s in any::<String>()) {
            let mut buf = Vec::new();
            put_str(&mut buf, &s);
            let (decoded, consumed) = get_str(&buf).unwrap();
            assert_eq!(s, decoded);
            assert_eq!(consumed, buf.len());
        }

        #[test]
        fn test_i64_preserves_order(a in any::<i64>(), b in any::<i64>()) {
            let mut buf_a = Vec::new();
            let mut buf_b = Vec::new();
            put_i64(&mut buf_a, a);
            put_i64(&mut buf_b, b);
            assert_eq!(a.cmp(&b), buf_a.cmp(&buf_b));
        }

        #[test]
        fn test_u64_preserves_order(a in any::<u64>(), b in any::<u64>()) {
            let mut buf_a = Vec::new();
            let mut buf_b = Vec::new();
            put_u64(&mut buf_a, a);
            put_u64(&mut buf_b, b);
            assert_eq!(a.cmp(&b), buf_a.cmp(&buf_b));
        }

        #[test]
        fn test_str_preserves_order(a in any::<String>(), b in any::<String>()) {
            let mut buf_a = Vec::new();
            let mut buf_b = Vec::new();
            put_str(&mut buf_a, &a);
            put_str(&mut buf_b, &b);
            assert_eq!(a.cmp(&b), buf_a.cmp(&buf_b));
        }

        #[test]
        fn test_skip_string(s in any::<String>()) {
            let mut buf = Vec::new();
            put_str(&mut buf, &s);
            let end = skip(&buf, 0, false).unwrap();
            assert_eq!(end, buf.len());
        }

        #[test]
        fn test_skip_i64(val in any::<i64>()) {
            let mut buf = Vec::new();
            put_i64(&mut buf, val);
            let end = skip(&buf, 0, false).unwrap();
            assert_eq!(end, buf.len());
        }

        #[test]
        fn test_skip_u64(val in any::<u64>()) {
            let mut buf = Vec::new();
            put_u64(&mut buf, val);
            let end = skip(&buf, 0, false).unwrap();
            assert_eq!(end, buf.len());
        }

        #[test]
        fn test_strinc_is_strictly_greater(prefix in any::<Vec<u8>>()) {
            if let Some(next) = strinc(&prefix) {
                prop_assert!(prefix < next);
            }
        }

        #[test]
        fn test_strinc_returns_none_only_for_empty_or_all_ff(prefix in any::<Vec<u8>>()) {
            let result = strinc(&prefix);
            let should_be_none = prefix.iter().all(|&b| b == 0xff);

            prop_assert_eq!(result.is_none(), should_be_none);
        }

        #[test]
        fn test_strinc_is_prefix_upper_bound(prefix in any::<Vec<u8>>(), suffix in any::<Vec<u8>>()) {
            if let Some(next) = strinc(&prefix) {
                let mut key = prefix.clone();
                key.extend_from_slice(&suffix);

                prop_assert!(key < next);
            }
        }
    }
}
