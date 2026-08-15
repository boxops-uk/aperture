use std::borrow::Cow;
use std::cmp::Ordering;

use serde::{Serialize, Serializer, ser::SerializeMap};

use crate::focus::error::StoreCodecError;
use aperture_schema::{
    id::FactId,
    schema::{LocalInterner, PredicateId, PredicateTy, Symbol},
};

pub const MARK_NULL: u8 = 0x00;

pub const MARK_STRING: u8 = 0x21;
pub const MARK_RECORD: u8 = 0x22;

pub const MARK_INT_NEG_MIN: u8 = 0x40;
pub const MARK_INT_NEG_MAX: u8 = 0x47;
pub const MARK_INT_ZERO: u8 = 0x48;
pub const MARK_INT_POS_MIN: u8 = 0x49;
pub const MARK_INT_POS_MAX: u8 = 0x50;

pub const MARK_FACT_REF: u8 = 0x51;

/// The encoded width of a fact-typed field: the marker, then a fixed-width id.
///
/// Fixed-width rather than the integer codec's variable width, so a reference sorts
/// as a band of its own after every integer ([I1]) and can be compared without a
/// decode.
///
/// [I1]: ../../../docs/invariants.md#i1
pub const FACT_REF_FIELD_LEN: usize = 1 + size_of::<u64>();

pub const MARK_TERM: u8 = 0x00;
pub const MARK_ESCAPE: u8 = 0xFF;

pub const NULL: u8 = 0x00;

/// How many big-endian bytes a magnitude needs: 0 for zero, else 1..=8.
///
/// The bound is what licenses the narrowing casts on this value elsewhere — a
/// width always fits a `u8`, and `8 - width` never underflows.
#[inline]
pub fn int_width(mag: u64) -> usize {
    8 - (mag.leading_zeros() / 8) as usize
}

/// A fact reference as a key field, on the stack.
///
/// The single definition of the encoding — [`TupleEncoder::put_fact_id`] writes these
/// bytes, and the executor's residual compares against them without allocating, which
/// is what keeps the hot loop allocation-free ([I9](../../../docs/invariants.md#i9)).
#[must_use]
pub fn fact_ref_bytes(id: FactId) -> [u8; FACT_REF_FIELD_LEN] {
    let mut out = [0u8; FACT_REF_FIELD_LEN];
    out[0] = MARK_FACT_REF;
    out[1..].copy_from_slice(&id.raw().to_be_bytes());
    out
}

pub fn put_i64(out: &mut Vec<u8>, val: i64) {
    if val == 0 {
        out.push(MARK_INT_ZERO);
        return;
    }

    let mag = val.unsigned_abs();
    let width = int_width(mag);

    // `width` is 1..=8 for a non-zero magnitude ([`int_width`]), so the cast
    // cannot truncate and the mark stays inside its band: 0x49..=0x50 going up
    // from MARK_INT_ZERO, 0x40..=0x47 going down.
    let width_byte = width as u8;
    let mark = if val > 0 {
        MARK_INT_ZERO + width_byte
    } else {
        MARK_INT_ZERO - width_byte
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

            // Checked against `i64::MAX` just above, so the sign cannot flip.
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

            // `i64::MIN` is the one magnitude that does not fit an `i64`, so it is
            // named rather than negated; every smaller one does fit, which is what
            // makes the cast below safe.
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
    // 1..=8, as in `put_i64`.
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

fn get_escaped(bytes: &[u8]) -> Result<(Cow<'_, [u8]>, usize), StoreCodecError> {
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

pub fn get_str(bytes: &[u8]) -> Result<(Cow<'_, str>, usize), StoreCodecError> {
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

pub fn skip(
    bytes: &[u8],
    start: usize,
    require_escape_null: bool,
) -> Result<usize, StoreCodecError> {
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
                if require_escape_null || record_depth > 0 {
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

            MARK_FACT_REF => {
                i = checked_advance(bytes, after_mark, 8)?;

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

/// Encode `value` against its declared type, positionally: a record's fields are
/// written in **declared order** — the schema's, which is what the read path walks —
/// and the value's are taken in the order they are in.
///
/// **Not the encoder for a fact's key.** A record encoded here keeps its wrapper, which
/// is right for a *value* and for a record nested inside a key field, and wrong for the
/// key itself: a stored key is its top-level fields back to back with no wrapper
/// ([chapter 3]), so a key written through this one is a key no query can find — the
/// seek builds the flat form and the two never meet. [`encode_key`] is the one that
/// knows the difference.
///
/// It checks arity but *not* field names, because a tuple has none — so a caller
/// holding a record whose fields might be in any order owes it a pass through
/// [`fact::encode`](crate::focus::fact::encode), which resolves names against the
/// schema and hands back a value already in this order.
///
/// [chapter 3]: ../../../docs/03-storage-model.md#a-stored-key-is-flat
pub fn encode_typed(ty: &PredicateTy, value: &Value) -> Result<Vec<u8>, StoreCodecError> {
    let mut out = Vec::new();
    encode_typed_at(&mut TupleEncoder::new(&mut out), ty, value)?;
    Ok(out)
}

/// Encode a fact's **key**, which is flat.
///
/// A stored key is the key type's top-level fields back to back with no wrapper of
/// their own, while a record *inside* a field keeps its wrapper — there it is one value
/// among others and has to be skippable as one ([chapter 3]). So a record key is not
/// [`encode_typed`] of a record: that writes a marker and a terminator the read path
/// does not expect, and every field index lands one byte late.
///
/// A **scalar** key is one field and needs none of this — the same asymmetry a query
/// meets as `nyi/whole-key`.
///
/// [chapter 3]: ../../../docs/03-storage-model.md#a-stored-key-is-flat
pub fn encode_key(ty: &PredicateTy, value: &Value) -> Result<Vec<u8>, StoreCodecError> {
    let (PredicateTy::Record(field_tys), Value::Record(fields)) = (ty, value) else {
        return encode_typed(ty, value);
    };

    if field_tys.len() != fields.len() {
        return Err(StoreCodecError::BadRecord);
    }

    let mut out = Vec::new();
    let mut enc = TupleEncoder::new(&mut out);

    for ((_, field_ty), (_, field_value)) in field_tys.iter().zip(fields.iter()) {
        encode_typed_at(&mut enc, field_ty, field_value)?;
    }

    Ok(out)
}

/// A fact reference against the field it sits in: it must name the **declared**
/// predicate, and it must not be the reserved id.
///
/// The predicate a reference names is inside the id itself — a [`FactId`] is a
/// snowflake, tagged with its owning predicate — so this costs a shift and a
/// compare, and the typed codec is the only boundary that holds both halves.
///
/// Why it has to be checked *here*, rather than left to the read path: a
/// wrong-predicate reference is not corrupt in any way the bytes reveal. It
/// encodes, decodes, sorts and projects as a well-formed reference. The only
/// thing that ever notices is a query that **follows** it, which reads the row on
/// the other end against the *declared* predicate's key layout and so must
/// refuse (`ApertureError::ReferenceCrossesPredicate`, raised in the executor —
/// named rather than linked, because the codec sits below it).
/// A query that merely reads the field back never notices at all.
fn checked_fact_ref(predicate: PredicateId, id: FactId) -> Result<(), StoreCodecError> {
    if id.sequence() == 0 {
        return Err(StoreCodecError::ReservedFactId);
    }

    if id.predicate() != predicate {
        return Err(StoreCodecError::FactRefPredicate {
            expected: predicate.0,
            found: id.predicate().0,
        });
    }

    Ok(())
}

/// [`encode_typed`] into an encoder already in progress — a field of a record.
pub fn encode_typed_at(
    enc: &mut TupleEncoder<'_>,
    ty: &PredicateTy,
    value: &Value,
) -> Result<(), StoreCodecError> {
    match (ty, value) {
        (PredicateTy::Int, Value::Int(i)) => {
            enc.put_i64(*i);
            Ok(())
        }

        (PredicateTy::Str, Value::Str(s)) => {
            enc.put_str(s);
            Ok(())
        }

        (PredicateTy::Fact(predicate), Value::FactRef(id)) => {
            checked_fact_ref(*predicate, *id)?;
            enc.put_fact_id(*id);
            Ok(())
        }

        (PredicateTy::Record(field_tys), Value::Record(field_values)) => {
            if field_tys.len() != field_values.len() {
                return Err(StoreCodecError::BadRecord);
            }

            enc.record(|enc| {
                for ((_, field_ty), (_, field_value)) in field_tys.iter().zip(field_values.iter()) {
                    encode_typed_at(enc, field_ty, field_value)?;
                }

                Ok(())
            })
        }

        _ => Err(StoreCodecError::BadRecord),
    }
}

pub trait TupleEncode {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError>;
}

pub trait TupleDecode<'a>: Sized {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError>;
}

pub fn encode_tuple<T: TupleEncode + ?Sized>(value: &T) -> Result<Vec<u8>, StoreCodecError> {
    let mut out = Vec::new();
    let mut enc = TupleEncoder::new(&mut out);
    value.tuple_encode(&mut enc)?;
    Ok(out)
}

pub fn decode_tuple<'a, T>(bytes: &'a [u8]) -> Result<T, StoreCodecError>
where
    T: TupleDecode<'a>,
{
    let mut dec = TupleDecoder::new(bytes);
    let value = T::tuple_decode(&mut dec)?;

    if let Some(&mark) = dec.bytes.get(dec.pos) {
        return Err(StoreCodecError::UnexpectedMark(mark));
    }

    Ok(value)
}

pub struct TupleEncoder<'a> {
    out: &'a mut Vec<u8>,
    record_depth: usize,
}

impl<'a> TupleEncoder<'a> {
    pub fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            record_depth: 0,
        }
    }

    /// Writing a scalar cannot fail — the sink is a `Vec` and every encoding is
    /// total — so these return nothing. [`record`](Self::record) is the one
    /// fallible operation, because nesting past [`MAX_RECORD_DEPTH`] is a fault
    /// the encoding itself cannot express. A `Result` on the rest only put a `?`
    /// at every call site and left a reader wondering which of them could fail.
    pub fn put_null(&mut self) {
        self.out.push(MARK_NULL);

        // Inside a record a bare NULL *is* the terminator, so a null value has to
        // be escaped to be distinguishable from the end of the record.
        if self.record_depth > 0 {
            self.out.push(MARK_ESCAPE);
        }
    }

    pub fn put_i64(&mut self, val: i64) {
        put_i64(self.out, val);
    }

    pub fn put_u64(&mut self, val: u64) {
        put_u64(self.out, val);
    }

    pub fn put_str(&mut self, val: &str) {
        put_str(self.out, val);
    }

    pub fn put_fact_id(&mut self, id: FactId) {
        self.out.extend_from_slice(&fact_ref_bytes(id));
    }

    pub fn record<R>(
        &mut self,
        f: impl FnOnce(&mut TupleEncoder<'_>) -> Result<R, StoreCodecError>,
    ) -> Result<R, StoreCodecError> {
        if self.record_depth == MAX_RECORD_DEPTH {
            return Err(StoreCodecError::BadRecord);
        }

        self.out.push(MARK_RECORD);

        self.record_depth += 1;
        let result = f(self);
        self.record_depth -= 1;

        let result = result?;

        self.out.push(MARK_TERM);

        Ok(result)
    }
}

pub struct TupleDecoder<'a> {
    bytes: &'a [u8],
    pos: usize,
    record_depth: usize,
}

impl<'a> TupleDecoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            record_depth: 0,
        }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }

    fn peek_mark(&self) -> Result<u8, StoreCodecError> {
        self.bytes
            .get(self.pos)
            .copied()
            .ok_or(StoreCodecError::UnexpectedEof)
    }

    fn next_is_record_end(&self) -> Result<bool, StoreCodecError> {
        if self.record_depth == 0 {
            return Ok(false);
        }

        let mark = self.peek_mark()?;

        Ok(mark == MARK_TERM && self.bytes.get(self.pos + 1) != Some(&MARK_ESCAPE))
    }

    fn next_is_null_value(&self) -> Result<bool, StoreCodecError> {
        let mark = self.peek_mark()?;

        if mark != MARK_NULL {
            return Ok(false);
        }

        if self.record_depth > 0 {
            Ok(self.bytes.get(self.pos + 1) == Some(&MARK_ESCAPE))
        } else {
            Ok(true)
        }
    }

    pub fn take_null(&mut self) -> Result<(), StoreCodecError> {
        let mark = self.peek_mark()?;

        if mark != MARK_NULL {
            return Err(StoreCodecError::UnexpectedMark(mark));
        }

        if self.record_depth > 0 {
            if self.bytes.get(self.pos + 1) != Some(&MARK_ESCAPE) {
                return Err(StoreCodecError::UnexpectedTerminator);
            }

            self.pos += 2;
        } else {
            self.pos += 1;
        }

        Ok(())
    }

    pub fn take_i64(&mut self) -> Result<i64, StoreCodecError> {
        let (val, consumed) = get_i64(&self.bytes[self.pos..])?;
        self.pos += consumed;
        Ok(val)
    }

    pub fn take_u64(&mut self) -> Result<u64, StoreCodecError> {
        let (val, consumed) = get_u64(&self.bytes[self.pos..])?;
        self.pos += consumed;
        Ok(val)
    }

    pub fn take_str(&mut self) -> Result<Cow<'a, str>, StoreCodecError> {
        let (val, consumed) = get_str(&self.bytes[self.pos..])?;
        self.pos += consumed;
        Ok(val)
    }

    pub fn take_fact_id(&mut self) -> Result<FactId, StoreCodecError> {
        let mark = self.peek_mark()?;

        if mark != MARK_FACT_REF {
            return Err(StoreCodecError::UnexpectedMark(mark));
        }

        let start = self.pos + 1;
        let end = checked_advance(self.bytes, start, 8)?;

        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.bytes[start..end]);

        self.pos = end;

        let id = FactId::from_raw(u64::from_be_bytes(buf));

        // Sequence 0 is reserved so that zeroed or truncated bytes are
        // *detectably* not a fact ([I11](../../docs/invariants.md#i11)), and a
        // property nothing checks is only an intention. The stored-`keys`-row
        // decoder (`store::decode_fact_id`) already enforces it; this is the same
        // rule at the decoder that reads a reference embedded **in a key**, which
        // is the only other way stored bytes become a `FactId`.
        if id.sequence() == 0 {
            return Err(StoreCodecError::ReservedFactId);
        }

        Ok(id)
    }

    pub fn record<R, E>(
        &mut self,
        f: impl FnOnce(&mut TupleDecoder<'a>) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<StoreCodecError>,
    {
        let mark = self.peek_mark().map_err(E::from)?;

        if mark != MARK_RECORD {
            return Err(E::from(StoreCodecError::UnexpectedMark(mark)));
        }

        self.pos += 1;

        if self.record_depth == MAX_RECORD_DEPTH {
            return Err(E::from(StoreCodecError::BadRecord));
        }

        let old_depth = self.record_depth;
        self.record_depth += 1;

        let result = (|| {
            let value = f(self)?;
            self.expect_record_end().map_err(E::from)?;
            Ok(value)
        })();

        self.record_depth = old_depth;

        result
    }

    pub fn expect_record_end(&mut self) -> Result<(), StoreCodecError> {
        if self.record_depth == 0 {
            return Err(StoreCodecError::BadRecord);
        }

        if self.next_is_record_end()? {
            self.pos += 1;
            Ok(())
        } else {
            let mark = self.peek_mark()?;
            Err(StoreCodecError::UnexpectedMark(mark))
        }
    }

    pub fn is_record_end(&self) -> Result<bool, StoreCodecError> {
        self.next_is_record_end()
    }
}

impl TupleEncode for i64 {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_i64(*self);
        Ok(())
    }
}

impl<'a> TupleDecode<'a> for i64 {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        dec.take_i64()
    }
}

impl TupleEncode for u64 {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_u64(*self);
        Ok(())
    }
}

impl<'a> TupleDecode<'a> for u64 {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        dec.take_u64()
    }
}

impl TupleEncode for FactId {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_fact_id(*self);
        Ok(())
    }
}

impl<'a> TupleDecode<'a> for FactId {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        dec.take_fact_id()
    }
}

impl TupleEncode for str {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(self);
        Ok(())
    }
}

impl TupleEncode for String {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(self);
        Ok(())
    }
}

impl TupleEncode for &str {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(self);
        Ok(())
    }
}

impl<'a> TupleDecode<'a> for Cow<'a, str> {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        dec.take_str()
    }
}

impl<'a> TupleDecode<'a> for String {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        Ok(dec.take_str()?.into_owned())
    }
}

impl<T> TupleEncode for Option<T>
where
    T: TupleEncode,
{
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        match self {
            Some(value) => value.tuple_encode(enc),
            None => {
                enc.put_null();
                Ok(())
            }
        }
    }
}

impl<'a, T> TupleDecode<'a> for Option<T>
where
    T: TupleDecode<'a>,
{
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        if dec.next_is_null_value()? {
            dec.take_null()?;
            Ok(None)
        } else {
            Ok(Some(T::tuple_decode(dec)?))
        }
    }
}

/// Decode-counting probe for the I5 guard (`exec::bind_is_refcount_not_decode`).
///
/// Every typed field decode bumps a thread-local counter; the guard asserts that
/// binding variables triggers zero decodes — decoding happens only at read
/// sites (projection), never at bind time. See `docs/testing.md`.
#[cfg(any(test, feature = "proptest"))]
pub mod decode_probe {
    use std::cell::Cell;

    thread_local! {
        static DECODES: Cell<u64> = const { Cell::new(0) };
    }

    /// Reset the decode counter to zero.
    pub fn reset() {
        DECODES.with(|c| c.set(0));
    }

    /// Number of typed field decodes since the last [`reset`].
    pub fn count() -> u64 {
        DECODES.with(Cell::get)
    }

    pub(crate) fn bump() {
        DECODES.with(|c| c.set(c.get() + 1));
    }
}

pub fn decode_typed(
    interner: &LocalInterner,
    bytes: &[u8],
    ty: &PredicateTy,
) -> Result<Value, StoreCodecError> {
    let mut dec = TupleDecoder::new(bytes);

    let value = decode_typed_at(interner, &mut dec, ty)?;

    if !dec.remaining().is_empty() {
        let mark = dec
            .remaining()
            .first()
            .copied()
            .ok_or(StoreCodecError::UnexpectedEof)?;

        return Err(StoreCodecError::UnexpectedMark(mark));
    }

    Ok(value)
}

/// Decode a **stored key** — which is the key type's top-level fields back to
/// back, with no record wrapper of its own ([chapter 3]).
///
/// That asymmetry is the layout, not an accident: a key is stored flat so a seek
/// can extend a prefix by whole fields and the executor can reach field *k* by
/// skipping the *k* before it, which is what the field-offset cache holds
/// ([I2](../../docs/invariants.md#i2)). A *nested* record inside a field keeps its
/// wrapper, because there it is one value among others and has to be skippable as
/// one. So [`decode_typed`] reads a field or a value, and this reads a whole key;
/// handing a record-keyed predicate's key to `decode_typed` looks for a
/// `MARK_RECORD` that was never written.
///
/// [chapter 3]: ../../docs/03-storage-model.md
pub fn decode_key(
    interner: &LocalInterner,
    bytes: &[u8],
    ty: &PredicateTy,
) -> Result<Value, StoreCodecError> {
    let mut dec = TupleDecoder::new(bytes);

    let value = match ty {
        PredicateTy::Record(fields) => {
            let mut out: Vec<(String, Value)> = Vec::with_capacity(fields.len());

            for (name, field_ty) in fields.iter() {
                let value = decode_typed_at(interner, &mut dec, field_ty)?;

                let symbol = Symbol::Schema(*name);
                let field_name = interner
                    .try_resolve(symbol)
                    .ok_or(StoreCodecError::UnknownSymbol(symbol))?
                    .to_owned();

                out.push((field_name, value));
            }

            Value::Record(out.into_boxed_slice())
        }

        scalar => decode_typed_at(interner, &mut dec, scalar)?,
    };

    // As for a field: a key that decoded "successfully" while leaving bytes unread
    // is a key of a different shape than the schema says.
    if !dec.remaining().is_empty() {
        let mark = dec
            .remaining()
            .first()
            .copied()
            .ok_or(StoreCodecError::UnexpectedEof)?;

        return Err(StoreCodecError::UnexpectedMark(mark));
    }

    Ok(value)
}

pub fn decode_typed_at(
    interner: &LocalInterner,
    dec: &mut TupleDecoder<'_>,
    ty: &PredicateTy,
) -> Result<Value, StoreCodecError> {
    // I5 probe: this is the single funnel for typed field/value decoding.
    #[cfg(any(test, feature = "proptest"))]
    decode_probe::bump();

    match ty {
        PredicateTy::Int => {
            let i = dec.take_i64()?;
            Ok(Value::Int(i))
        }

        PredicateTy::Str => {
            let s = dec.take_str()?;
            Ok(Value::Str(s.into_owned()))
        }

        PredicateTy::Fact(predicate) => {
            // A fact reference is encoded with its own marker (MARK_FACT_REF),
            // consistently with `skip` and the `FactId` codec — not the integer
            // codec.
            //
            // `take_fact_id` has already rejected the reserved sequence; what is
            // left is whether the id names the predicate this field is declared
            // to reference, which only this boundary knows.
            let id = dec.take_fact_id()?;
            checked_fact_ref(*predicate, id)?;
            Ok(Value::FactRef(id))
        }

        PredicateTy::Record(fields) => dec.record(|dec| {
            let mut out: Vec<(String, Value)> = Vec::with_capacity(fields.len());

            for (name, field_ty) in fields.iter() {
                if dec.is_record_end()? {
                    return Err(StoreCodecError::BadRecord);
                }

                let value = decode_typed_at(interner, dec, field_ty)?;

                let symbol = Symbol::Schema(*name);
                let field_name = interner
                    .try_resolve(symbol)
                    .ok_or(StoreCodecError::UnknownSymbol(symbol))?
                    .to_owned();

                out.push((field_name, value));
            }

            if !dec.is_record_end()? {
                return Err(StoreCodecError::BadRecord);
            }

            Ok(Value::Record(out.into_boxed_slice()))
        }),
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Int(i64),
    Str(String),
    FactRef(FactId),
    Record(Box<[(String, Value)]>),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        use Value::*;

        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => MARK_NULL,
                Value::Str(_) => MARK_STRING,
                Value::Record(_) => MARK_RECORD,
                Value::Int(_) => MARK_INT_NEG_MIN,
                Value::FactRef(_) => MARK_FACT_REF,
            }
        }

        let ra = rank(self);
        let rb = rank(other);

        if ra != rb {
            return ra.cmp(&rb);
        }

        match (self, other) {
            (Int(a), Int(b)) => a.cmp(b),
            (Str(a), Str(b)) => a.cmp(b),
            (FactRef(a), FactRef(b)) => a.raw().cmp(&b.raw()),
            (Record(a), Record(b)) => a.as_ref().cmp(b.as_ref()),
            (Null, Null) => Ordering::Equal,
            _ => unreachable!("equal rank for different Value variants"),
        }
    }
}

impl Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Value::Null => serializer.serialize_none(),
            Value::Int(n) => serializer.serialize_i64(*n),
            Value::Str(s) => serializer.serialize_str(s),
            Value::FactRef(id) => serializer.serialize_u64(id.raw()),
            Value::Record(fields) => {
                let mut map = serializer.serialize_map(Some(fields.len()))?;

                for (key, value) in fields.iter() {
                    map.serialize_entry(key, value)?;
                }

                map.end()
            }
        }
    }
}

/// Composable `proptest` strategies and oracles for the tuple codec.
///
/// Named `arb_*` strategies mirror the value/type tree so other domains'
/// generators (e.g. the schema-first `(plan, store)` generator) can build on
/// them, and the independent oracles (`cmp_typed`, `encode_typed_for_test`) are
/// shared test machinery rather than per-test boilerplate. See
/// [`docs/testing.md`](../../../docs/testing.md).
#[cfg(any(test, feature = "proptest"))]
pub mod proptest {
    use super::*;
    use ::proptest::prelude::*;
    use aperture_schema::{
        id::{MAX_FACT_SEQUENCE, MAX_TAGGABLE_PREDICATE},
        schema::{PredicateId, PredicateTy, SchemaInterner},
    };
    use lasso::Rodeo;
    use std::{cmp::Ordering, sync::Arc};

    /// A codec type, parallel to [`PredicateTy`] but interner-free so it shrinks
    /// cleanly — field names are materialised (interned) only when a fixture is
    /// built.
    #[derive(Debug, Clone)]
    pub enum TySpec {
        Int,
        Str,
        Fact(PredicateId),
        Record(Vec<(String, TySpec)>),
    }

    #[derive(Debug, Clone)]
    pub struct TypedValueSpec {
        pub ty: TySpec,
        pub value: Value,
    }

    #[derive(Debug, Clone)]
    pub struct TypedPairSpec {
        pub ty: TySpec,
        pub a: Value,
        pub b: Value,
    }

    /// A materialised [`TypedValueSpec`]: the interner that resolves the record
    /// field names, plus the realised [`PredicateTy`] and value.
    pub struct TypedValueFixture {
        pub interner: LocalInterner,
        pub ty: PredicateTy,
        pub value: Value,
    }

    pub struct TypedPairFixture {
        pub interner: LocalInterner,
        pub ty: PredicateTy,
        pub a: Value,
        pub b: Value,
    }

    pub fn materialize_ty_spec(ty: &TySpec, rodeo: &mut Rodeo) -> PredicateTy {
        match ty {
            TySpec::Int => PredicateTy::Int,

            TySpec::Str => PredicateTy::Str,

            TySpec::Fact(id) => PredicateTy::Fact(*id),

            TySpec::Record(fields) => {
                let fields = fields
                    .iter()
                    .map(|(name, field_ty)| {
                        let spur = rodeo.get_or_intern(name);
                        let field_ty = materialize_ty_spec(field_ty, rodeo);
                        (spur, field_ty)
                    })
                    .collect::<Vec<_>>();

                PredicateTy::Record(Arc::from(fields.into_boxed_slice()))
            }
        }
    }

    pub fn materialize_value_fixture(spec: TypedValueSpec) -> TypedValueFixture {
        let mut rodeo = Rodeo::new();

        let ty = materialize_ty_spec(&spec.ty, &mut rodeo);

        let reader = rodeo.into_reader();
        let schema_interner = SchemaInterner::new(reader);
        let interner = LocalInterner::new(schema_interner);

        TypedValueFixture {
            interner,
            ty,
            value: spec.value,
        }
    }

    pub fn materialize_pair_fixture(spec: TypedPairSpec) -> TypedPairFixture {
        let mut rodeo = Rodeo::new();

        let ty = materialize_ty_spec(&spec.ty, &mut rodeo);

        let reader = rodeo.into_reader();
        let schema_interner = SchemaInterner::new(reader);
        let interner = LocalInterner::new(schema_interner);

        TypedPairFixture {
            interner,
            ty,
            a: spec.a,
            b: spec.b,
        }
    }

    /// Encode a value against its type using the storage encoder — the encoder
    /// the read path decodes; used to build stores and to drive round-trip
    /// properties.
    ///
    /// Kept as names of their own so the codec batteries read as codec batteries;
    /// both are [`encode_typed`] and [`encode_typed_at`].
    pub fn encode_typed_for_test(
        ty: &PredicateTy,
        value: &Value,
    ) -> Result<Vec<u8>, StoreCodecError> {
        encode_typed(ty, value)
    }

    pub fn encode_typed_at_for_test(
        enc: &mut TupleEncoder<'_>,
        ty: &PredicateTy,
        value: &Value,
    ) -> Result<(), StoreCodecError> {
        encode_typed_at(enc, ty, value)
    }

    /// The independent order oracle: compares two values field-by-field per the
    /// type, *not* by reusing the code under test. Order-preservation is proved
    /// by matching encoded-byte ordering against this.
    pub fn cmp_typed(ty: &PredicateTy, a: &Value, b: &Value) -> Ordering {
        match (ty, a, b) {
            (PredicateTy::Int, Value::Int(a), Value::Int(b)) => a.cmp(b),

            (PredicateTy::Str, Value::Str(a), Value::Str(b)) => a.cmp(b),

            (PredicateTy::Fact(_), Value::FactRef(a), Value::FactRef(b)) => a.raw().cmp(&b.raw()),

            (PredicateTy::Record(field_tys), Value::Record(a_fields), Value::Record(b_fields)) => {
                assert_eq!(field_tys.len(), a_fields.len());
                assert_eq!(field_tys.len(), b_fields.len());

                for (((_, field_ty), (_, a_value)), (_, b_value)) in
                    field_tys.iter().zip(a_fields.iter()).zip(b_fields.iter())
                {
                    let ord = cmp_typed(field_ty, a_value, b_value);

                    if ord != Ordering::Equal {
                        return ord;
                    }
                }

                Ordering::Equal
            }

            _ => panic!("schema/value mismatch: ty={ty:?}, a={a:?}, b={b:?}"),
        }
    }

    fn field_name(i: usize) -> String {
        format!("field_{i}")
    }

    /// A predicate tag, with both ends of the field it occupies drawn explicitly.
    fn arb_predicate_id() -> impl Strategy<Value = u32> {
        prop_oneof![
            Just(0u32),
            Just(MAX_TAGGABLE_PREDICATE),
            0u32..=MAX_TAGGABLE_PREDICATE,
        ]
    }

    /// A **valid** fact-id sequence: 1-based, since 0 is reserved, and up to the
    /// width of the field. Both edges injected, because a sequence at either end
    /// is where the tag and the sequence meet in the encoded bytes.
    fn arb_sequence() -> impl Strategy<Value = u64> {
        prop_oneof![
            Just(1u64),
            Just(MAX_FACT_SEQUENCE),
            1u64..=MAX_FACT_SEQUENCE
        ]
    }

    /// A pair of values sharing one schema, drawn together so ordering/round-trip
    /// properties can compare `a` against `b`. Injects the known integer/string
    /// edges explicitly rather than trusting random draws to hit them, and
    /// recurses into records with an explicit depth/size bound.
    pub fn arb_typed_pair() -> impl Strategy<Value = TypedPairSpec> {
        let arb_i64 = prop_oneof![
            Just(i64::MIN),
            Just(-1i64),
            Just(0i64),
            Just(1i64),
            Just(i64::MAX),
            any::<i64>(),
        ];
        let arb_str = prop_oneof![
            Just(String::new()),
            Just("\0".to_string()),
            Just("\0\0".to_string()),
            any::<String>(),
        ];

        let leaf = prop_oneof![
            (arb_i64.clone(), arb_i64).prop_map(|(a, b)| TypedPairSpec {
                ty: TySpec::Int,
                a: Value::Int(a),
                b: Value::Int(b),
            }),
            (arb_str.clone(), arb_str).prop_map(|(a, b)| TypedPairSpec {
                ty: TySpec::Str,
                a: Value::Str(a),
                b: Value::Str(b),
            }),
            // Both halves of a pair share one type, so both references are tagged
            // for the *same* predicate — which is what the schema means and, since
            // `encode_typed` now checks it, the only thing it will encode. Drawing
            // the tag and the sequence separately rather than an arbitrary `u64`
            // costs no byte coverage: a valid id ranges over the whole 64-bit space
            // except the reserved sequences, so ordering is still exercised across
            // both fields and their boundary.
            (arb_predicate_id(), arb_sequence(), arb_sequence()).prop_map(|(predicate, a, b)| {
                TypedPairSpec {
                    ty: TySpec::Fact(PredicateId(predicate)),
                    a: Value::FactRef(
                        FactId::new(PredicateId(predicate), a).expect("a drawn id is valid"),
                    ),
                    b: Value::FactRef(
                        FactId::new(PredicateId(predicate), b).expect("a drawn id is valid"),
                    ),
                }
            },),
        ];

        leaf.prop_recursive(
            5,  // max depth
            64, // max total generated nodes
            4,  // max fields per record
            |inner| {
                prop::collection::vec(inner, 0..=4).prop_map(|children| {
                    let mut field_tys = Vec::with_capacity(children.len());
                    let mut a_fields = Vec::with_capacity(children.len());
                    let mut b_fields = Vec::with_capacity(children.len());

                    for (i, child) in children.into_iter().enumerate() {
                        let name = field_name(i);

                        field_tys.push((name.clone(), child.ty));
                        a_fields.push((name.clone(), child.a));
                        b_fields.push((name, child.b));
                    }

                    TypedPairSpec {
                        ty: TySpec::Record(field_tys),
                        a: Value::Record(a_fields.into_boxed_slice()),
                        b: Value::Record(b_fields.into_boxed_slice()),
                    }
                })
            },
        )
    }

    /// A single typed value (the `a` half of a pair).
    pub fn arb_typed_value() -> impl Strategy<Value = TypedValueSpec> {
        arb_typed_pair().prop_map(|pair| TypedValueSpec {
            ty: pair.ty,
            value: pair.a,
        })
    }

    /// A bare value with its schema discarded — for consumers that only need a
    /// well-formed [`Value`].
    pub fn arb_value() -> impl Strategy<Value = Value> {
        arb_typed_value().prop_map(|spec| spec.value)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::proptest::*;
    use super::*;
    use ::proptest::prelude::*;

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

        buf.extend(std::iter::repeat_n(MARK_RECORD, depth));
        buf.extend(std::iter::repeat_n(MARK_TERM, depth));

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

    // I3 — the marker table is frozen on disk. A marker byte is the MSB of a
    // value's sort key, so its value *and* its position in the ordering are
    // semantic: renumbering one is an on-disk migration, not a refactor. This
    // golden test pins every marker's byte, the marker ordering, and
    // representative encodings — so any renumber or layout change breaks loudly
    // here instead of silently corrupting existing stores.
    #[test]
    fn marker_table_golden() {
        // The frozen table. These exact bytes live on disk.
        assert_eq!(MARK_NULL, 0x00);
        assert_eq!(MARK_STRING, 0x21);
        assert_eq!(MARK_RECORD, 0x22);
        assert_eq!(MARK_INT_NEG_MIN, 0x40);
        assert_eq!(MARK_INT_NEG_MAX, 0x47);
        assert_eq!(MARK_INT_ZERO, 0x48);
        assert_eq!(MARK_INT_POS_MIN, 0x49);
        assert_eq!(MARK_INT_POS_MAX, 0x50);
        assert_eq!(MARK_FACT_REF, 0x51);
        assert_eq!(MARK_TERM, 0x00);
        assert_eq!(MARK_ESCAPE, 0xFF);
        assert_eq!(NULL, 0x00);

        // The ordering is semantic (memcmp of markers == sort order of the
        // families): null < string < record < negatives < zero < positives <
        // fact-refs, with the negative/positive width bands contiguous.
        let ordered = [
            MARK_NULL,
            MARK_STRING,
            MARK_RECORD,
            MARK_INT_NEG_MIN,
            MARK_INT_NEG_MAX,
            MARK_INT_ZERO,
            MARK_INT_POS_MIN,
            MARK_INT_POS_MAX,
            MARK_FACT_REF,
        ];
        assert!(
            ordered.windows(2).all(|w| w[0] < w[1]),
            "marker ordering is not strictly increasing: {ordered:02x?}"
        );

        // Golden encodings — exact on-disk bytes for representative values in
        // each family, including the width-band extremes.
        let i64_enc = |v: i64| {
            let mut b = Vec::new();
            put_i64(&mut b, v);
            b
        };
        assert_eq!(i64_enc(0), [0x48]);
        assert_eq!(i64_enc(1), [0x49, 0x01]);
        assert_eq!(i64_enc(-1), [0x47, 0xFE]);
        assert_eq!(i64_enc(256), [0x4A, 0x01, 0x00]);
        assert_eq!(
            i64_enc(i64::MAX),
            [0x50, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            i64_enc(i64::MIN),
            [0x40, 0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );

        let str_enc = |s: &str| {
            let mut b = Vec::new();
            put_str(&mut b, s);
            b
        };
        assert_eq!(str_enc(""), [0x21, 0x00]);
        assert_eq!(str_enc("A"), [0x21, 0x41, 0x00]);
        assert_eq!(str_enc("\0"), [0x21, 0x00, 0xFF, 0x00]);
        assert_eq!(str_enc("a\0b"), [0x21, 0x61, 0x00, 0xFF, 0x62, 0x00]);

        // Records and fact-refs go through the encoder.
        let mut empty_rec = Vec::new();
        TupleEncoder::new(&mut empty_rec)
            .record(|_| Ok(()))
            .unwrap();
        assert_eq!(empty_rec, [0x22, 0x00]);

        let mut rec_of_zero = Vec::new();
        TupleEncoder::new(&mut rec_of_zero)
            .record(|enc| {
                enc.put_i64(0);
                Ok(())
            })
            .unwrap();
        assert_eq!(rec_of_zero, [0x22, 0x48, 0x00]);

        let mut fact_ref = Vec::new();
        TupleEncoder::new(&mut fact_ref).put_fact_id(FactId::from_raw(1));
        assert_eq!(fact_ref, [0x51, 0, 0, 0, 0, 0, 0, 0, 1]);
    }

    // A fact-typed field is encoded with the resolved fact-reference marker
    // (MARK_FACT_REF), consistently with `skip`, `put_fact_id`/`take_fact_id`,
    // and the `FactId` codec — never the integer codec. Regression for the
    // latent mismatch where `decode_typed_at` decoded `Fact` fields as a u64
    // (integer band): `skip` and `decode` disagreed, a canonically encoded fact
    // reference could not be decoded, and fact fields sorted inside the integer
    // band instead of after positive integers (breaking I1).
    #[test]
    fn fact_field_uses_fact_ref_marker_and_round_trips() {
        use aperture_schema::schema::{PredicateId, SchemaInterner};
        use lasso::Rodeo;

        let ty = PredicateTy::Fact(PredicateId(0));
        let value = Value::FactRef(FactId::from_raw(42));

        let bytes = encode_typed_for_test(&ty, &value).unwrap();

        // Canonical form: MARK_FACT_REF then 8 fixed big-endian bytes.
        assert_eq!(bytes.len(), 9);
        assert_eq!(bytes[0], MARK_FACT_REF);
        assert_eq!(&bytes[1..], &42u64.to_be_bytes());

        // `skip` consumes exactly the field...
        assert_eq!(skip(&bytes, 0, false).unwrap(), bytes.len());

        // ...and `decode_typed` round-trips it (interner unused for a fact ref).
        let interner = LocalInterner::new(SchemaInterner::new(Rodeo::new().into_reader()));
        assert_eq!(decode_typed(&interner, &bytes, &ty).unwrap(), value);
    }

    /// A fact reference carries the predicate it names in the id's own tag, so
    /// "does this reference name what the field is declared to reference" is a
    /// comparison the typed codec can make **for free** — and this is the only
    /// boundary that can make it, because it is the only one holding both the
    /// declared type and the id.
    ///
    /// Unchecked, `Fact(0)` accepts an id tagged for predicate 1: the bytes
    /// encode, decode and project as a well-typed reference to the wrong
    /// predicate. The fault surfaces only if a query later *follows* it — as
    /// `ApertureError::ReferenceCrossesPredicate`,
    /// raised in the executor, layers away from the write that was wrong — or
    /// never at all, for a query that only reads the field back.
    #[test]
    fn a_typed_fact_ref_must_name_the_declared_predicate() {
        use aperture_schema::schema::{PredicateId, SchemaInterner};
        use lasso::Rodeo;

        let ty = PredicateTy::Fact(PredicateId(0));
        let elsewhere = FactId::new(PredicateId(1), 7).expect("a valid id");

        assert!(
            matches!(
                encode_typed_for_test(&ty, &Value::FactRef(elsewhere)),
                Err(StoreCodecError::FactRefPredicate {
                    expected: 0,
                    found: 1
                })
            ),
            "encoding a reference tagged for another predicate must be rejected",
        );

        // The decode side is checked independently, because the bytes need not
        // have come from this encoder: a fact file, another DB, a corrupt row.
        let mut bytes = Vec::new();
        TupleEncoder::new(&mut bytes).put_fact_id(elsewhere);

        let interner = LocalInterner::new(SchemaInterner::new(Rodeo::new().into_reader()));
        assert!(
            matches!(
                decode_typed(&interner, &bytes, &ty),
                Err(StoreCodecError::FactRefPredicate {
                    expected: 0,
                    found: 1
                })
            ),
            "decoding a reference tagged for another predicate must be rejected",
        );
    }

    /// Sequence 0 is reserved so that zeroed or corrupt bytes are *detectably*
    /// not a fact ([I11]) — and a property nothing checks is only an intention.
    /// [`decode_fact_id`](crate::focus::store) already enforces it for a stored
    /// `keys` row; this is the same rule at the other decoder, the one that reads
    /// a reference embedded **in a key**.
    ///
    /// [I11]: ../../../docs/invariants.md#i11
    #[test]
    fn a_fact_ref_of_the_reserved_sequence_is_rejected() {
        use aperture_schema::schema::{PredicateId, SchemaInterner};
        use lasso::Rodeo;

        let ty = PredicateTy::Fact(PredicateId(0));

        assert!(
            matches!(
                encode_typed_for_test(&ty, &Value::FactRef(FactId::from_raw(0))),
                Err(StoreCodecError::ReservedFactId)
            ),
            "encoding the reserved id must be rejected",
        );

        // Eight zero bytes behind the marker — the shape a truncated or zeroed
        // row actually takes.
        let bytes = [MARK_FACT_REF, 0, 0, 0, 0, 0, 0, 0, 0];

        assert!(
            matches!(
                TupleDecoder::new(&bytes).take_fact_id(),
                Err(StoreCodecError::ReservedFactId)
            ),
            "the decoder itself must reject the reserved sequence",
        );

        let interner = LocalInterner::new(SchemaInterner::new(Rodeo::new().into_reader()));
        assert!(
            matches!(
                decode_typed(&interner, &bytes, &ty),
                Err(StoreCodecError::ReservedFactId)
            ),
            "and so must the typed decode above it",
        );
    }

    // ---- a stored key is flat, a nested record is not ----------------------

    /// A key is its top-level fields back to back; a record *inside* a field keeps
    /// its wrapper. That is the layout the whole read path assumes — a seek extends
    /// a prefix by whole fields, and field *k* is reached by skipping the *k*
    /// before it — so it is pinned here, in bytes, next to the codec it is a
    /// property of.
    #[test]
    fn a_stored_key_is_its_fields_with_no_wrapper_of_its_own() {
        use aperture_schema::schema::SchemaInterner;
        use lasso::Rodeo;
        use std::sync::Arc;

        let mut rodeo = Rodeo::new();
        let (outer, inner, id) = (
            rodeo.get_or_intern("outer"),
            rodeo.get_or_intern("inner"),
            rodeo.get_or_intern("id"),
        );
        let schema = SchemaInterner::new(rodeo.into_reader());
        let interner = LocalInterner::new(schema);

        // `{ id : int, outer : { inner : str } }` — fields sorted by name.
        let key_ty = PredicateTy::Record(Arc::from([
            (id, PredicateTy::Int),
            (
                outer,
                PredicateTy::Record(Arc::from([(inner, PredicateTy::Str)])),
            ),
        ]));

        let mut bytes = Vec::new();
        put_i64(&mut bytes, 7);
        bytes.push(MARK_RECORD);
        put_str(&mut bytes, "x");
        bytes.push(MARK_TERM);

        // The top level has no `MARK_RECORD`: the first byte is the first field's.
        assert_ne!(bytes[0], MARK_RECORD, "a stored key carries no wrapper");

        // Two top-level fields, reachable by skipping.
        let first = skip(&bytes, 0, false).unwrap();
        assert_eq!(&bytes[..first], i64_field_bytes(7).as_slice());
        assert_eq!(skip(&bytes, first, false).unwrap(), bytes.len());

        // And the whole key decodes as the record its type says it is.
        let decoded = decode_key(&interner, &bytes, &key_ty).expect("decode a stored key");
        assert_eq!(
            decoded,
            Value::Record(Box::new([
                ("id".to_owned(), Value::Int(7)),
                (
                    "outer".to_owned(),
                    Value::Record(Box::new([("inner".to_owned(), Value::Str("x".to_owned()))]))
                ),
            ]))
        );

        // A scalar key is one field, and decodes the same way either function
        // would read it.
        let mut scalar = Vec::new();
        put_str(&mut scalar, "abc");
        assert_eq!(
            decode_key(&interner, &scalar, &PredicateTy::Str).unwrap(),
            Value::Str("abc".to_owned())
        );

        // Trailing bytes are a fault, as they are for a field: a key that decodes
        // "successfully" while leaving bytes unread hides a schema mismatch.
        let mut trailing = bytes.clone();
        put_i64(&mut trailing, 1);
        assert!(decode_key(&interner, &trailing, &key_ty).is_err());
    }

    /// One encoded i64, for comparing bytes above.
    fn i64_field_bytes(v: i64) -> Vec<u8> {
        let mut out = Vec::new();
        put_i64(&mut out, v);
        out
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

        #[test]
        fn test_typed_value_roundtrip(spec in arb_typed_value()) {
            let fixture = materialize_value_fixture(spec);

            let bytes = encode_typed_for_test(&fixture.ty, &fixture.value).unwrap();

            let decoded = decode_typed(
                &fixture.interner,
                &bytes,
                &fixture.ty,
            ).unwrap();

            prop_assert_eq!(decoded, fixture.value);
        }

        #[test]
        fn test_typed_value_order_matches_encoded_order(spec in arb_typed_pair()) {
            let fixture = materialize_pair_fixture(spec);

            let encoded_a = encode_typed_for_test(&fixture.ty, &fixture.a).unwrap();
            let encoded_b = encode_typed_for_test(&fixture.ty, &fixture.b).unwrap();

            let expected = cmp_typed(&fixture.ty, &fixture.a, &fixture.b);
            let actual = encoded_a.cmp(&encoded_b);

            prop_assert_eq!(
                expected,
                actual,
                "typed ordering mismatch\n\
                ty: {:#?}\n\
                a: {:#?}\n\
                b: {:#?}\n\
                encoded_a: {:02x?}\n\
                encoded_b: {:02x?}",
                fixture.ty,
                fixture.a,
                fixture.b,
                encoded_a,
                encoded_b,
            );
        }

        #[test]
        fn test_value_ord_matches_typed_order_for_same_schema(spec in arb_typed_pair()) {
            let fixture = materialize_pair_fixture(spec);

            let expected = cmp_typed(&fixture.ty, &fixture.a, &fixture.b);
            let actual = fixture.a.cmp(&fixture.b);

            prop_assert_eq!(
                expected,
                actual,
                "Value::Ord mismatch\n\
                ty: {:#?}\n\
                a: {:#?}\n\
                b: {:#?}",
                fixture.ty,
                fixture.a,
                fixture.b,
            );
        }

        #[test]
        fn test_roundtrip_preserves_value_and_ordering(spec in arb_typed_pair()) {
            let fixture = materialize_pair_fixture(spec);

            let encoded_a = encode_typed_for_test(&fixture.ty, &fixture.a).unwrap();
            let encoded_b = encode_typed_for_test(&fixture.ty, &fixture.b).unwrap();

            let decoded_a = decode_typed(
                &fixture.interner,
                &encoded_a,
                &fixture.ty,
            ).unwrap();

            let decoded_b = decode_typed(
                &fixture.interner,
                &encoded_b,
                &fixture.ty,
            ).unwrap();

            prop_assert_eq!(&decoded_a, &fixture.a);
            prop_assert_eq!(&decoded_b, &fixture.b);

            prop_assert_eq!(
                decoded_a.cmp(&decoded_b),
                encoded_a.cmp(&encoded_b),
                "decoded ordering does not match encoded byte ordering\n\
                ty: {:#?}\n\
                decoded_a: {:#?}\n\
                decoded_b: {:#?}\n\
                encoded_a: {:02x?}\n\
                encoded_b: {:02x?}",
                fixture.ty,
                decoded_a,
                decoded_b,
                encoded_a,
                encoded_b,
            );
        }
    }
}
