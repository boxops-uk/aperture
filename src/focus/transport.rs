use std::borrow::Cow;
use std::cmp::Ordering;

use serde::{Serialize, Serializer, ser::SerializeMap};

use crate::focus::{
    error::{StoreCodecError, StoreError},
    plan::FactId,
    schema::{LocalInterner, PredicateTy, Symbol},
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

pub const MARK_TERM: u8 = 0x00;
pub const MARK_ESCAPE: u8 = 0xFF;

pub const NULL: u8 = 0x00;

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

    pub fn put_null(&mut self) -> Result<(), StoreCodecError> {
        if self.record_depth > 0 {
            self.out.push(MARK_NULL);
            self.out.push(MARK_ESCAPE);
        } else {
            self.out.push(MARK_NULL);
        }

        Ok(())
    }

    pub fn put_i64(&mut self, val: i64) -> Result<(), StoreCodecError> {
        put_i64(self.out, val);
        Ok(())
    }

    pub fn put_u64(&mut self, val: u64) -> Result<(), StoreCodecError> {
        put_u64(self.out, val);
        Ok(())
    }

    pub fn put_str(&mut self, val: &str) -> Result<(), StoreCodecError> {
        put_str(self.out, val);
        Ok(())
    }

    pub fn put_fact_id(&mut self, id: FactId) -> Result<(), StoreCodecError> {
        self.out.push(MARK_FACT_REF);
        self.out.extend_from_slice(&id.0.to_be_bytes());
        Ok(())
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

        Ok(FactId(u64::from_be_bytes(buf)))
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
        enc.put_i64(*self)
    }
}

impl<'a> TupleDecode<'a> for i64 {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        dec.take_i64()
    }
}

impl TupleEncode for u64 {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_u64(*self)
    }
}

impl<'a> TupleDecode<'a> for u64 {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        dec.take_u64()
    }
}

impl TupleEncode for FactId {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_fact_id(*self)
    }
}

impl<'a> TupleDecode<'a> for FactId {
    fn tuple_decode(dec: &mut TupleDecoder<'a>) -> Result<Self, StoreCodecError> {
        dec.take_fact_id()
    }
}

impl TupleEncode for str {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(self)
    }
}

impl TupleEncode for String {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(self)
    }
}

impl TupleEncode for &str {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(self)
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
            None => enc.put_null(),
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

pub fn decode_typed(
    interner: &LocalInterner,
    bytes: &[u8],
    ty: &PredicateTy,
) -> Result<Value, StoreError> {
    let mut dec = TupleDecoder::new(bytes);

    let value = decode_typed_at(interner, &mut dec, ty)?;

    if !dec.remaining().is_empty() {
        let mark = dec
            .remaining()
            .first()
            .copied()
            .ok_or(StoreCodecError::UnexpectedEof)?;

        return Err(StoreError::DecodeError(StoreCodecError::UnexpectedMark(
            mark,
        )));
    }

    Ok(value)
}

pub fn decode_record_typed<'b>(
    interner: &LocalInterner,
    dec: &mut TupleDecoder<'b>,
    fields: &[(Symbol, PredicateTy)],
) -> Result<Value, StoreError> {
    dec.record(|dec| {
        let mut out: Vec<(String, Value)> = Vec::with_capacity(fields.len());

        for (name, field_ty) in fields.iter() {
            if dec.is_record_end()? {
                return Err(StoreError::DecodeError(StoreCodecError::BadRecord));
            }

            let value = decode_typed_at(interner, dec, field_ty)?;

            let field_name = interner
                .try_resolve(*name)
                .ok_or(StoreError::UnknownSymbol(*name))?
                .to_owned();

            out.push((field_name, value));
        }

        if !dec.is_record_end()? {
            return Err(StoreError::DecodeError(StoreCodecError::BadRecord));
        }

        Ok(Value::Record(out.into_boxed_slice()))
    })
}

pub fn decode_typed_at<'b>(
    interner: &LocalInterner,
    dec: &mut TupleDecoder<'b>,
    ty: &PredicateTy,
) -> Result<Value, StoreError> {
    match ty {
        PredicateTy::Int => {
            let i = dec.take_i64()?;
            Ok(Value::Int(i))
        }

        PredicateTy::Str => {
            let s = dec.take_str()?;
            Ok(Value::Str(s.into_owned()))
        }

        PredicateTy::Fact(_) => {
            let id = dec.take_u64()?;
            Ok(Value::FactRef(FactId(id)))
        }

        PredicateTy::Record(fields) => dec.record(|dec| {
            let mut out: Vec<(String, Value)> = Vec::with_capacity(fields.len());

            for (name, field_ty) in fields.iter() {
                if dec.is_record_end()? {
                    return Err(StoreError::DecodeError(StoreCodecError::BadRecord));
                }

                let value = decode_typed_at(interner, dec, field_ty)?;

                let symbol = Symbol::Schema(*name);
                let field_name = interner
                    .try_resolve(symbol)
                    .ok_or(StoreError::UnknownSymbol(symbol))?
                    .to_owned();

                out.push((field_name, value));
            }

            if !dec.is_record_end()? {
                return Err(StoreError::DecodeError(StoreCodecError::BadRecord));
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
                Value::FactRef(_) => MARK_INT_POS_MIN,
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
            (FactRef(a), FactRef(b)) => a.0.cmp(&b.0),
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
            Value::FactRef(id) => serializer.serialize_u64(id.0),
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

#[cfg(test)]
pub(crate) mod tests {
    use crate::focus::schema::{PredicateId, SchemaInterner};

    use super::*;
    use lasso::{Rodeo, Spur};
    use proptest::prelude::*;
    use std::{cmp::Ordering, sync::Arc};

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

    #[derive(Debug, Clone)]
    enum TySpec {
        Int,
        Str,
        Fact(PredicateId),
        Record(Vec<(String, TySpec)>),
    }

    #[derive(Debug, Clone)]
    struct TypedValueSpec {
        ty: TySpec,
        value: Value,
    }

    #[derive(Debug, Clone)]
    struct TypedPairSpec {
        ty: TySpec,
        a: Value,
        b: Value,
    }

    struct TypedValueFixture {
        interner: LocalInterner,
        ty: PredicateTy,
        value: Value,
    }

    struct TypedPairFixture {
        interner: LocalInterner,
        ty: PredicateTy,
        a: Value,
        b: Value,
    }

    fn materialize_ty_spec(ty: &TySpec, rodeo: &mut Rodeo) -> PredicateTy {
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

    fn materialize_value_fixture(spec: TypedValueSpec) -> TypedValueFixture {
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

    fn materialize_pair_fixture(spec: TypedPairSpec) -> TypedPairFixture {
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

    fn encode_typed_for_test(ty: &PredicateTy, value: &Value) -> Result<Vec<u8>, StoreCodecError> {
        let mut out = Vec::new();
        let mut enc = TupleEncoder::new(&mut out);

        encode_typed_at_for_test(&mut enc, ty, value)?;

        Ok(out)
    }

    fn encode_typed_at_for_test(
        enc: &mut TupleEncoder<'_>,
        ty: &PredicateTy,
        value: &Value,
    ) -> Result<(), StoreCodecError> {
        match (ty, value) {
            (PredicateTy::Int, Value::Int(i)) => enc.put_i64(*i),

            (PredicateTy::Str, Value::Str(s)) => enc.put_str(s),

            (PredicateTy::Fact(_), Value::FactRef(id)) => enc.put_u64(id.0),

            (PredicateTy::Record(field_tys), Value::Record(field_values)) => {
                if field_tys.len() != field_values.len() {
                    return Err(StoreCodecError::BadRecord);
                }

                enc.record(|enc| {
                    for ((_, field_ty), (_, field_value)) in
                        field_tys.iter().zip(field_values.iter())
                    {
                        encode_typed_at_for_test(enc, field_ty, field_value)?;
                    }

                    Ok(())
                })
            }

            _ => Err(StoreCodecError::BadRecord),
        }
    }

    fn cmp_typed(ty: &PredicateTy, a: &Value, b: &Value) -> Ordering {
        match (ty, a, b) {
            (PredicateTy::Int, Value::Int(a), Value::Int(b)) => a.cmp(b),

            (PredicateTy::Str, Value::Str(a), Value::Str(b)) => a.cmp(b),

            (PredicateTy::Fact(_), Value::FactRef(a), Value::FactRef(b)) => a.0.cmp(&b.0),

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

    fn arb_typed_pair_spec() -> impl Strategy<Value = TypedPairSpec> {
        let leaf = prop_oneof![
            (any::<i64>(), any::<i64>()).prop_map(|(a, b)| TypedPairSpec {
                ty: TySpec::Int,
                a: Value::Int(a),
                b: Value::Int(b),
            }),
            (any::<String>(), any::<String>()).prop_map(|(a, b)| TypedPairSpec {
                ty: TySpec::Str,
                a: Value::Str(a),
                b: Value::Str(b),
            }),
            (any::<u64>(), any::<u64>()).prop_map(|(a, b)| TypedPairSpec {
                ty: TySpec::Fact(PredicateId(0)),
                a: Value::FactRef(FactId(a)),
                b: Value::FactRef(FactId(b)),
            }),
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

    fn arb_typed_value_spec() -> impl Strategy<Value = TypedValueSpec> {
        arb_typed_pair_spec().prop_map(|pair| TypedValueSpec {
            ty: pair.ty,
            value: pair.a,
        })
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
        fn test_typed_value_roundtrip(spec in arb_typed_value_spec()) {
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
        fn test_typed_value_order_matches_encoded_order(spec in arb_typed_pair_spec()) {
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
        fn test_value_ord_matches_typed_order_for_same_schema(spec in arb_typed_pair_spec()) {
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
        fn test_roundtrip_preserves_value_and_ordering(spec in arb_typed_pair_spec()) {
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
