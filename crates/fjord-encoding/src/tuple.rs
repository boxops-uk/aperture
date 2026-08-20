use std::borrow::Cow;
use std::cmp::Ordering;

use serde::{Serialize, Serializer, ser::SerializeMap};

use crate::error::StoreCodecError;
use fjord_schema::{
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

/// A **tagged alternative** — the marker, the discriminant, one payload, a
/// terminator.
///
/// Appended after [`MARK_FACT_REF`], which is what [I3] permits and the only thing
/// it permits: the table above it does not move. Highest in the table, so a union
/// sorts after every other type, and within a union by discriminant then payload —
/// so a key's alternatives **cluster**, and matching one is a prefix of the key
/// order rather than a filter over all of it.
///
/// A union is encoded as a *group*, like a record: it carries a terminator and
/// escapes a null payload, even though its arity of one would make the terminator
/// redundant. That is a deliberate byte: it keeps "is a group" a single concept —
/// terminated, null-escaping, depth-counted — so [`skip`] needs no notion of a
/// value still owed, and `nested_field_span` walks a payload with the machinery it
/// walks a record field with. See [phase 8.6 D-a].
///
/// [I3]: ../../../website/content/invariants.md#i3
/// [phase 8.6 D-a]: ../../../website/content/storage.md
pub const MARK_UNION: u8 = 0x52;

/// The encoded width of a fact-typed field: the marker, then a fixed-width id.
///
/// Fixed-width rather than the integer codec's variable width, so a reference sorts
/// as a band of its own after every integer ([I1]) and can be compared without a
/// decode.
///
/// [I1]: ../../../website/content/invariants.md#i1
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
/// is what keeps the hot loop allocation-free ([I9](../../../website/content/invariants.md#i9)).
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

/// The encoding of an unsigned integer, on the stack: the marker, then the
/// magnitude's significant bytes.
///
/// The **single definition** of the unsigned encoding — [`put_u64`] and
/// [`UnionTag`] both go through it, so the bytes a seek prefix is built from and
/// the bytes a stored value carries cannot drift apart.
#[inline]
fn u64_bytes(val: u64) -> ([u8; 1 + size_of::<u64>()], usize) {
    let mut out = [0u8; 1 + size_of::<u64>()];

    if val == 0 {
        out[0] = MARK_INT_ZERO;
        return (out, 1);
    }

    let width = int_width(val);
    // 1..=8, as in `put_i64`.
    out[0] = MARK_INT_ZERO + width as u8;
    out[1..=width].copy_from_slice(&val.to_be_bytes()[8 - width..]);

    (out, 1 + width)
}

#[inline]
pub fn put_u64(out: &mut Vec<u8>, val: u64) {
    let (bytes, len) = u64_bytes(val);
    out.extend_from_slice(&bytes[..len]);
}

/// The longest a union's tag can be: the marker, plus an unsigned discriminant's
/// marker and its four magnitude bytes.
pub const UNION_TAG_MAX_LEN: usize = 1 + 1 + size_of::<u32>();

/// A union's **tag** — `MARK_UNION` and the discriminant — on the stack.
///
/// The single statement of what every value of one alternative begins with, and the
/// reason a select is a *prefix* rather than a filter: a seek splices these bytes to
/// narrow a scan to one alternative, and the executor's residual compares against
/// them without allocating, which is what keeps the hot loop allocation-free
/// ([I9](../../../website/content/invariants.md#i9)). Same shape, and the same job, as
/// [`fact_ref_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnionTag {
    bytes: [u8; UNION_TAG_MAX_LEN],
    len: usize,
}

impl UnionTag {
    #[must_use]
    pub fn new(disc: u32) -> UnionTag {
        let mut bytes = [0u8; UNION_TAG_MAX_LEN];
        bytes[0] = MARK_UNION;

        let (disc_bytes, disc_len) = u64_bytes(u64::from(disc));
        bytes[1..=disc_len].copy_from_slice(&disc_bytes[..disc_len]);

        UnionTag {
            bytes,
            len: 1 + disc_len,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
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

            // **A union is a group**, so it needs nothing here that a record does
            // not: read the tag, then let the existing depth counter and terminator
            // logic close it. That is the whole payoff of the terminator — a payload
            // owed but unterminated would mean every `record_depth == 0` return
            // above becoming "return only if nothing is still owed", in the one
            // function whose failure mode is a silently wrong field offset.
            //
            // The discriminant is consumed positionally rather than scanned, so a
            // tag whose bytes contain a `0x00` cannot be read as a terminator, and
            // an unknown tag is still **skippable**: I2 does not depend on knowing
            // what an alternative means.
            MARK_UNION => {
                if record_depth == MAX_RECORD_DEPTH {
                    return Err(StoreCodecError::BadRecord);
                }

                let (_, disc_len) = get_u64(
                    bytes
                        .get(after_mark..)
                        .ok_or(StoreCodecError::UnexpectedEof)?,
                )?;

                i = checked_advance(bytes, after_mark, disc_len)?;
                record_depth += 1;
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
/// `fjord_store::fact::encode`, which resolves names against the
/// schema and hands back a value already in this order.
///
/// [chapter 3]: ../../../website/content/storage.md#a-stored-key-is-flat
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
/// [chapter 3]: ../../../website/content/storage.md#a-stored-key-is-flat
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
/// refuse (`FjordError::ReferenceCrossesPredicate`, raised in the executor —
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

        (
            PredicateTy::Union(alts),
            Value::Union {
                disc,
                value: payload,
                ..
            },
        ) => {
            let tag = u64::from(*disc);

            // **By discriminant, never by name.** The tag is the identity — it is
            // what the bytes carry and what the order is over — and a name is not
            // checked here for the same reason a record's field names are not: this
            // encoder is positional, and a caller holding names owes it a pass
            // through `fjord_store::fact::encode` first.
            let alt = alts
                .iter()
                .find(|alt| alt.disc == *disc)
                .ok_or(StoreCodecError::UnknownDiscriminant { tag })?;

            enc.union(*disc, |enc| encode_typed_at(enc, &alt.ty, payload))
                .map_err(|err| match err {
                    // A shape mismatch under a payload *is* a payload that does not
                    // match the alternative, and the tag is the actionable half of
                    // saying so. A deeper union's own refusal keeps its own tag.
                    StoreCodecError::BadRecord => StoreCodecError::BadUnion { tag },
                    other => other,
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

    /// A **union**: the tag, then one payload, then the terminator every group
    /// carries.
    ///
    /// Counted against the same depth bound a record is, and null-escaping inside it
    /// for the same reason: within a group a bare `0x00` is the terminator, and a
    /// payload that could be one has to be told apart from it.
    pub fn union<R>(
        &mut self,
        disc: u32,
        f: impl FnOnce(&mut TupleEncoder<'_>) -> Result<R, StoreCodecError>,
    ) -> Result<R, StoreCodecError> {
        if self.record_depth == MAX_RECORD_DEPTH {
            return Err(StoreCodecError::BadRecord);
        }

        self.out.extend_from_slice(UnionTag::new(disc).as_bytes());

        self.record_depth += 1;
        let result = f(self);
        self.record_depth -= 1;

        let result = result?;

        self.out.push(MARK_TERM);

        Ok(result)
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
        // *detectably* not a fact ([I11](../../../website/content/invariants.md#i11)), and a
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

    /// A **union**: the callback is handed the discriminant and the decoder
    /// positioned at the payload, and the terminator is consumed after it.
    ///
    /// The discriminant is passed rather than resolved here because only the caller
    /// holds the type that says which alternative it names — and answering that is
    /// where [`UnknownDiscriminant`](StoreCodecError::UnknownDiscriminant) comes
    /// from.
    pub fn union<R, E>(
        &mut self,
        f: impl FnOnce(&mut TupleDecoder<'a>, u64) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<StoreCodecError>,
    {
        let mark = self.peek_mark().map_err(E::from)?;

        if mark != MARK_UNION {
            return Err(E::from(StoreCodecError::UnexpectedMark(mark)));
        }

        let (disc, disc_len) = get_u64(
            self.bytes
                .get(self.pos + 1..)
                .ok_or(StoreCodecError::UnexpectedEof)
                .map_err(E::from)?,
        )
        .map_err(E::from)?;

        if self.record_depth == MAX_RECORD_DEPTH {
            return Err(E::from(StoreCodecError::BadRecord));
        }

        self.pos += 1 + disc_len;

        let old_depth = self.record_depth;
        self.record_depth += 1;

        let result = (|| {
            let value = f(self, disc)?;
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
/// sites (projection), never at bind time. See `website/content/testing.md`.
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
/// ([I2](../../../website/content/invariants.md#i2)). A *nested* record inside a field keeps its
/// wrapper, because there it is one value among others and has to be skippable as
/// one. So [`decode_typed`] reads a field or a value, and this reads a whole key;
/// handing a record-keyed predicate's key to `decode_typed` looks for a
/// `MARK_RECORD` that was never written.
///
/// [chapter 3]: ../../website/content/storage.md
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

        PredicateTy::Union(alts) => dec.union(|dec, tag| {
            let alt = alts
                .iter()
                .find(|alt| u64::from(alt.disc) == tag)
                .ok_or(StoreCodecError::UnknownDiscriminant { tag })?;

            let value = decode_typed_at(interner, dec, &alt.ty)?;

            let symbol = Symbol::Schema(alt.name);
            let name = interner
                .try_resolve(symbol)
                .ok_or(StoreCodecError::UnknownSymbol(symbol))?
                .to_owned();

            Ok(Value::Union {
                disc: alt.disc,
                alt: name,
                value: Box::new(value),
            })
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
    /// One alternative of a union: its **discriminant**, its name, and its payload.
    ///
    /// The discriminant is the identity — it is what the bytes hold and what the
    /// order is taken over — and `alt` is the name that discriminant is declared
    /// with, carried for the same reason a record's field names are: a `Value` is
    /// serialised without its type ([`Serialize`]), so a union with no name in it
    /// renders as a number. It is filled from the schema on decode and **not**
    /// checked on encode, exactly as a record's names are not: the discriminant
    /// locates the alternative, the name is what a reader sees.
    Union {
        disc: u32,
        alt: String,
        value: Box<Value>,
    },
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
                Value::Union { .. } => MARK_UNION,
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
            // Discriminant then payload, which is the encoded order. The *name* is
            // deliberately not compared: it is determined by the discriminant, so
            // for any value the schema could have produced it would decide nothing
            // — and for one built by hand with a name that disagrees, ordering by
            // it would disagree with the bytes.
            (
                Union {
                    disc: a_disc,
                    value: a_value,
                    ..
                },
                Union {
                    disc: b_disc,
                    value: b_value,
                    ..
                },
            ) => a_disc.cmp(b_disc).then_with(|| a_value.cmp(b_value)),
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
            // `{"alt": payload}` — a union renders as the one-field object it is,
            // which is also how it is written in a query and on the way in.
            Value::Union { alt, value, .. } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(alt, value)?;
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
/// [`website/content/testing.md`](../../../website/content/testing.md).
#[cfg(any(test, feature = "proptest"))]
pub mod proptest {
    use super::*;
    use ::proptest::prelude::*;
    use fjord_schema::{
        id::{MAX_FACT_SEQUENCE, MAX_TAGGABLE_PREDICATE},
        schema::{Alternative, PredicateId, PredicateTy, SchemaInterner},
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
        /// Alternatives as `(name, discriminant, payload)`, in **declaration
        /// order** — which the generator deliberately draws out of discriminant
        /// order, because a tag that is not a position is the whole of I10 and a
        /// generator that only ever declares them in order would never notice a
        /// reader that assumed otherwise.
        Union(Vec<(String, u32, TySpec)>),
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

            TySpec::Union(alts) => {
                let alts = alts
                    .iter()
                    .map(|(name, disc, alt_ty)| Alternative {
                        name: rodeo.get_or_intern(name),
                        disc: *disc,
                        ty: materialize_ty_spec(alt_ty, rodeo),
                    })
                    .collect::<Vec<_>>();

                PredicateTy::Union(Arc::from(alts.into_boxed_slice()))
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

            // **Discriminant first, and only then the payload** — stated here
            // independently of the encoder, which is what makes the ordering
            // property a check rather than a tautology. Two different alternatives
            // never compare their payloads at all: they are values of different
            // types, and the tag is what separates them.
            (
                PredicateTy::Union(alts),
                Value::Union {
                    disc: a_disc,
                    value: a_value,
                    ..
                },
                Value::Union {
                    disc: b_disc,
                    value: b_value,
                    ..
                },
            ) => {
                if a_disc != b_disc {
                    return a_disc.cmp(b_disc);
                }

                let alt = alts
                    .iter()
                    .find(|alt| alt.disc == *a_disc)
                    .expect("a generated union value names a declared alternative");

                cmp_typed(&alt.ty, a_value, b_value)
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
                // `inner` is a `BoxedStrategy`, which is not `Clone`, and both arms
                // need it — a record's fields and a union's alternatives are the same
                // recursion. `Rc`, not `Arc`: a strategy is not `Send`, and a generator
                // runs on one thread.
                let inner = std::rc::Rc::new(inner);

                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..=4).prop_map(|children| {
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
                    }),
                    union_of(inner, false),
                ]
            },
        )
    }

    /// The discriminants a generated union declares, given how many alternatives it
    /// has and which table to use.
    ///
    /// Two tables, both drawn: the **canonical** one, where a tag happens to equal
    /// its position, and a **scrambled** one, where it does not and where the
    /// declaration is not in tag order. The second is the one that matters — every
    /// reader that treats a discriminant as an index passes against the first — and
    /// it carries the two edges a tag has: `0`, and a number far outside any
    /// plausible count of alternatives.
    fn discriminants(count: usize, scrambled: bool) -> Vec<u32> {
        let table: [u32; 4] = if scrambled {
            [3, 0, 40_000, 7]
        } else {
            [0, 1, 2, 3]
        };

        table[..count].to_vec()
    }

    fn alternative_name(i: usize) -> String {
        format!("alt_{i}")
    }

    /// A pair of union values over one union type, each **independently** choosing
    /// its alternative.
    ///
    /// Independent choice is the point: a pair sharing an alternative compares
    /// payloads, a pair that does not compares tags, and a generator that only did
    /// one of those would leave half of I1's union case unexercised.
    fn union_of(
        inner: impl Strategy<Value = TypedPairSpec> + 'static,
        distinct: bool,
    ) -> impl Strategy<Value = TypedPairSpec> {
        // A pair that must land on *different* alternatives needs at least two to
        // choose between. Stated as a bound on the generator rather than as a
        // `prop_assume`, because assuming it away rejects every single-alternative
        // draw and every coincidence of the two picks — enough of the space that
        // proptest gives up before the law has been tested much at all.
        let least = if distinct { 2 } else { 1 };

        (
            prop::collection::vec(inner, least..=4),
            any::<bool>(),
            0usize..4,
            0usize..4,
        )
            .prop_map(move |(children, scrambled, pick_a, pick_b)| {
                let discs = discriminants(children.len(), scrambled);

                let alts = children
                    .iter()
                    .enumerate()
                    .map(|(i, child)| (alternative_name(i), discs[i], child.ty.clone()))
                    .collect::<Vec<_>>();

                let i = pick_a % children.len();
                let j = if distinct {
                    (i + 1 + pick_b % (children.len() - 1)) % children.len()
                } else {
                    pick_b % children.len()
                };

                TypedPairSpec {
                    ty: TySpec::Union(alts),
                    a: Value::Union {
                        disc: discs[i],
                        alt: alternative_name(i),
                        value: Box::new(children[i].a.clone()),
                    },
                    b: Value::Union {
                        disc: discs[j],
                        alt: alternative_name(j),
                        value: Box::new(children[j].b.clone()),
                    },
                }
            })
    }

    /// A pair whose **top node is a union**, for the laws that are about unions
    /// rather than about values in general.
    pub fn arb_union_pair() -> impl Strategy<Value = TypedPairSpec> {
        union_of(arb_typed_pair().boxed(), false)
    }

    /// A pair of union values that are **certainly of different alternatives** — for
    /// the law that says the tag decides the order whatever the payloads are.
    pub fn arb_distinct_alternative_pair() -> impl Strategy<Value = TypedPairSpec> {
        union_of(arb_typed_pair().boxed(), true)
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
    use std::sync::Arc;

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
        assert_eq!(MARK_UNION, 0x52);
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
            MARK_UNION,
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

        // A union: the marker, the discriminant through the unsigned encoding, the
        // payload, the terminator. Discriminant 0 is one byte, as `= 0` will be the
        // common tag in any hand-written schema.
        let union_enc = |disc: u32, payload: i64| {
            let mut b = Vec::new();
            TupleEncoder::new(&mut b)
                .union(disc, |enc| {
                    enc.put_i64(payload);
                    Ok(())
                })
                .unwrap();
            b
        };
        assert_eq!(union_enc(0, 0), [0x52, 0x48, 0x48, 0x00]);
        assert_eq!(union_enc(1, 0), [0x52, 0x49, 0x01, 0x48, 0x00]);
        assert_eq!(
            union_enc(256, 1),
            [0x52, 0x4A, 0x01, 0x00, 0x49, 0x01, 0x00]
        );
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
        use fjord_schema::schema::{PredicateId, SchemaInterner};
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
    /// `FjordError::ReferenceCrossesPredicate`,
    /// raised in the executor, layers away from the write that was wrong — or
    /// never at all, for a query that only reads the field back.
    #[test]
    fn a_typed_fact_ref_must_name_the_declared_predicate() {
        use fjord_schema::schema::{PredicateId, SchemaInterner};
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
    /// `fjord_store::store::decode_fact_id` already enforces it for a stored
    /// `keys` row; this is the same rule at the other decoder, the one that reads
    /// a reference embedded **in a key**.
    ///
    /// [I11]: ../../../website/content/invariants.md#i11
    #[test]
    fn a_fact_ref_of_the_reserved_sequence_is_rejected() {
        use fjord_schema::schema::{PredicateId, SchemaInterner};
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
        use fjord_schema::schema::SchemaInterner;
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

        /// **I2 at full generality**: skip is told nothing but the bytes.
        ///
        /// The existing skip properties cover one scalar family at a time and the
        /// record cases are hand-built. This one hands `skip` whatever the value
        /// generator produced — records, references, and now unions, nested — and
        /// asserts it walks exactly one value and stops. It is the property a
        /// mis-skip of a new constructor fails, and a mis-skip is not visible as an
        /// error: it is a field offset one byte out, and a silently wrong answer.
        #[test]
        fn skip_walks_any_typed_value(spec in arb_typed_value()) {
            let fixture = materialize_value_fixture(spec);
            let bytes = encode_typed_for_test(&fixture.ty, &fixture.value).unwrap();

            let end = skip(&bytes, 0, false)?;

            prop_assert_eq!(
                end,
                bytes.len(),
                "skip did not consume exactly one value\nty: {:#?}\nvalue: {:#?}\nbytes: {:02x?}",
                fixture.ty,
                fixture.value,
                bytes,
            );
        }

        /// **The law a select rests on**: every value of one alternative begins with
        /// that alternative's tag.
        ///
        /// This is what makes matching an alternative a *prefix* of the key order —
        /// a seek that narrows, and a residual that compares borrowed bytes — rather
        /// than a decode per row. If it ever fails, `DiscriminantEq` silently matches
        /// nothing.
        #[test]
        fn the_tag_is_a_byte_prefix_of_every_value_of_that_alternative(spec in arb_union_pair()) {
            let fixture = materialize_pair_fixture(spec);

            for value in [&fixture.a, &fixture.b] {
                let Value::Union { disc, .. } = value else {
                    unreachable!("arb_union_pair produces unions");
                };

                let bytes = encode_typed_for_test(&fixture.ty, value).unwrap();
                let tag = UnionTag::new(*disc);

                prop_assert!(
                    bytes.starts_with(tag.as_bytes()),
                    "a union value does not begin with its tag\ntag: {:02x?}\nbytes: {:02x?}",
                    tag.as_bytes(),
                    bytes,
                );
            }
        }

        /// **The tag dominates the payload.** Two values of different alternatives
        /// order by discriminant whatever their payloads are — which is what
        /// "alternatives cluster in a key" means, and is stated here without going
        /// through `cmp_typed`, so an oracle that agreed with the encoder for the
        /// wrong reason would not save it.
        #[test]
        fn a_discriminant_orders_before_any_payload(spec in arb_distinct_alternative_pair()) {
            let fixture = materialize_pair_fixture(spec);

            let (Value::Union { disc: a_disc, .. }, Value::Union { disc: b_disc, .. }) =
                (&fixture.a, &fixture.b)
            else {
                unreachable!("arb_union_pair produces unions");
            };

            prop_assert_ne!(a_disc, b_disc, "the generator guarantees distinct alternatives");

            let encoded_a = encode_typed_for_test(&fixture.ty, &fixture.a).unwrap();
            let encoded_b = encode_typed_for_test(&fixture.ty, &fixture.b).unwrap();

            prop_assert_eq!(
                a_disc.cmp(b_disc),
                encoded_a.cmp(&encoded_b),
                "tag order and byte order disagree\na: {:#?}\nb: {:#?}\nencoded_a: {:02x?}\nencoded_b: {:02x?}",
                fixture.a,
                fixture.b,
                encoded_a,
                encoded_b,
            );
        }

        /// The stack-built tag and the stored bytes are the same bytes.
        ///
        /// [`UnionTag`] exists so the hot path allocates nothing, and it reaches the
        /// same encoding through [`u64_bytes`] rather than restating it — this is the
        /// guard that keeps that true, since a second statement of an encoding is
        /// exactly how a codec drifts.
        #[test]
        fn a_union_tag_is_the_marker_and_the_unsigned_discriminant(disc in any::<u32>()) {
            let mut expected = vec![MARK_UNION];
            put_u64(&mut expected, u64::from(disc));

            let tag = UnionTag::new(disc);
            prop_assert_eq!(tag.as_bytes(), &expected[..]);
        }
    }

    /// The tests module's own union helpers: a type and an interner that can
    /// resolve its names.
    fn union_fixture(alts: &[(&str, u32, PredicateTy)]) -> (LocalInterner, PredicateTy) {
        use fjord_schema::schema::{Alternative, SchemaInterner};
        use lasso::Rodeo;

        let mut rodeo = Rodeo::new();
        let alts = alts
            .iter()
            .map(|(name, disc, ty)| Alternative {
                name: rodeo.get_or_intern(name),
                disc: *disc,
                ty: ty.clone(),
            })
            .collect::<Vec<_>>();

        let interner = LocalInterner::new(SchemaInterner::new(rodeo.into_reader()));

        (
            interner,
            PredicateTy::Union(Arc::from(alts.into_boxed_slice())),
        )
    }

    fn union_value(disc: u32, alt: &str, value: Value) -> Value {
        Value::Union {
            disc,
            alt: alt.to_owned(),
            value: Box::new(value),
        }
    }

    /// A union of **one** alternative — the degenerate case
    /// [`website/content/testing.md`](../../../website/content/testing.md) names, which no random draw
    /// reliably produces and which is the shape `maybe`'s sugar will lean on.
    #[test]
    fn a_single_alternative_union_round_trips() {
        let (interner, ty) = union_fixture(&[("only", 0, PredicateTy::Str)]);
        let value = union_value(0, "only", Value::Str("x".to_owned()));

        let bytes = encode_typed(&ty, &value).unwrap();

        assert_eq!(skip(&bytes, 0, false).unwrap(), bytes.len());
        assert_eq!(decode_typed(&interner, &bytes, &ty).unwrap(), value);
    }

    /// A union is a **group**, so a null payload is escaped inside it exactly as a
    /// null element of a record is — which is what lets `skip` tell a payload from
    /// the terminator that follows it with no state of its own.
    #[test]
    fn a_null_payload_is_escaped_inside_the_union_group() {
        let (interner, ty) = union_fixture(&[("nothing", 0, PredicateTy::Record(Arc::from([])))]);

        // The payload here is the *empty record*, which is what an alternative with
        // no declared type lowers to. The null case is reached through a value
        // encoder that can write one, so it is asserted on the bytes directly.
        let mut out = Vec::new();
        TupleEncoder::new(&mut out)
            .union(0, |enc| {
                enc.put_null();
                Ok(())
            })
            .unwrap();

        assert_eq!(
            out,
            [MARK_UNION, MARK_INT_ZERO, MARK_NULL, MARK_ESCAPE, MARK_TERM]
        );
        assert_eq!(skip(&out, 0, false).unwrap(), out.len());

        // And the declared shape still round-trips beside it.
        let value = union_value(0, "nothing", Value::Record(Box::from([])));
        let bytes = encode_typed(&ty, &value).unwrap();
        assert_eq!(decode_typed(&interner, &bytes, &ty).unwrap(), value);
    }

    /// **D-c.** A tag no alternative declares is a refusal, not a mis-decode of
    /// whatever alternative happens to sit nearby. A fact file outlives the schema
    /// that wrote it, so this is a data condition rather than an impossibility.
    #[test]
    fn an_undeclared_discriminant_is_refused_rather_than_misread() {
        let (_, wrote) = union_fixture(&[("a", 7, PredicateTy::Str)]);
        let (reads_with, reads) = union_fixture(&[("a", 3, PredicateTy::Str)]);

        let bytes = encode_typed(&wrote, &union_value(7, "a", Value::Str("x".to_owned()))).unwrap();

        // Skippable without the schema even so — I2 does not depend on the tag
        // being known, which is what lets a row be walked past a field a reader
        // cannot interpret.
        assert_eq!(skip(&bytes, 0, false).unwrap(), bytes.len());

        let err = decode_typed(&reads_with, &bytes, &reads).unwrap_err();
        assert!(
            matches!(err, StoreCodecError::UnknownDiscriminant { tag: 7 }),
            "expected UnknownDiscriminant, got {err:?}"
        );
    }

    /// A payload of the wrong shape for the alternative its tag names is refused at
    /// encode — the union's `BadRecord`.
    #[test]
    fn a_payload_that_does_not_match_its_alternative_is_refused() {
        let (_, ty) = union_fixture(&[("text", 1, PredicateTy::Str)]);

        let err = encode_typed(&ty, &union_value(1, "text", Value::Int(3))).unwrap_err();
        assert!(
            matches!(err, StoreCodecError::BadUnion { tag: 1 }),
            "expected BadUnion, got {err:?}"
        );

        let err =
            encode_typed(&ty, &union_value(9, "text", Value::Str("x".to_owned()))).unwrap_err();
        assert!(
            matches!(err, StoreCodecError::UnknownDiscriminant { tag: 9 }),
            "expected UnknownDiscriminant, got {err:?}"
        );
    }

    /// Nesting past the depth bound is an error, not a panic or a stack overflow —
    /// the union's half of `test_skip_bad_record`, since a union counts against the
    /// same bound a record does.
    #[test]
    fn a_union_nested_past_the_depth_bound_is_an_error_not_a_panic() {
        let depth = MAX_RECORD_DEPTH + 1;

        let mut bytes = Vec::new();
        for _ in 0..depth {
            bytes.extend_from_slice(&[MARK_UNION, MARK_INT_ZERO]);
        }
        bytes.push(MARK_INT_ZERO);
        bytes.extend(std::iter::repeat_n(MARK_TERM, depth));

        assert!(matches!(
            skip(&bytes, 0, false),
            Err(StoreCodecError::BadRecord)
        ));
    }

    /// A union sorts **after every other type**, at the `Value` level as in the
    /// bytes. Stated at both ends because `Value::Ord` is what a client compares
    /// rows with and the bytes are what the store sorts by; the two agreeing is the
    /// property, not either one alone.
    #[test]
    fn a_union_sorts_after_every_other_value() {
        let union = union_value(0, "a", Value::Int(0));

        for smaller in [
            Value::Null,
            Value::Str(String::new()),
            Value::Record(Box::from([])),
            Value::Int(i64::MAX),
            Value::FactRef(FactId::from_raw(u64::MAX)),
        ] {
            assert!(smaller < union, "{smaller:?} should sort before a union");
        }
    }
}
