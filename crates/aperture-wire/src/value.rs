//! The wire value: what a fact looks like in flight.
//!
//! # Positional, because the schema is already on both ends
//!
//! [`WireValue::Record`] holds a bare slice of values with **no field names**, and
//! that is the central design decision rather than a shortcut. Both peers have the
//! schema — the handshake compares the client's expected schema fingerprint against
//! the DB's before a byte of data flows
//! ([operations §6](../../../docs/aperture-cli-design.md#6-wire-protocol--the-write-stream)),
//! and [I13](../../../docs/invariants.md#i13) freezes a DB's schema at create — so
//! the field names, their order and their types are known to the reader in advance.
//! Sending them again is sending what the reader already has.
//!
//! This is Avro's model, and Avro states the consequence plainly: *"Binary encoded
//! Avro data does not include type information or field names"*, so *"a schema must
//! always be used in order to read Avro data correctly"*, and a record is *"just the
//! concatenation of the encodings of its fields"*. Making the Rust type positional is
//! what stops that being a convention someone can break: there is no field name to
//! put in the wrong order, because there is no field name.
//!
//! **Why not tags, the way Protocol Buffers and Thrift do.** A protobuf field is a
//! tag-value pair, the tag being `(field_number << 3) | wire_type` as a varint —
//! one to two bytes per field per message. What that buys is a reader skipping
//! fields it does not know, which is schema evolution across versions that never
//! agreed. Here the peers *have* agreed, and disagreement is caught at handshake by
//! fingerprint rather than tolerated per field; P0 compatibility is subset
//! containment, so a predicate that exists at all has the shape both ends think it
//! has. Tags would be a per-fact price for a property this connection already has by
//! other means.
//!
//! **Why not a zero-copy layout, the way Cap'n Proto does.** Cap'n Proto's win is
//! O(1) access with no parse step, paid for with fixed-width fields that are larger
//! on the wire than varints. Neither half fits: every inbound fact is decoded
//! anyway — to intern its references and to re-encode it as a storage tuple
//! ([chapter 3](../../../docs/03-storage-model.md#interning-a-nested-fact)) — so
//! there is no parse to avoid, and the size is worse.
//!
//! # A reference is a union, and it is the only tag on the wire
//!
//! A `Fact`-typed field holds either a [`FactId`] the producer already has or the
//! target fact written inline
//! ([settled](../../../docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline)).
//! That choice is genuinely per-occurrence, so it is the one thing the schema cannot
//! predict and the one place a discriminator is written — a varint branch index,
//! which is exactly how Avro encodes a union. It appears only at `Fact`-typed
//! positions, and the schema says where those are.
//!
//! The nested branch carries **no predicate id**: `PredicateTy::Fact(p)` names its
//! target predicate, so the reader knows what it is about to read. A nested fact
//! costs one byte plus its own key and value, and nothing else.
//!
//! # What is not encoded, and why each absence is safe
//!
//! | not sent | because |
//! |---|---|
//! | field names | the schema has them ([I13](../../../docs/invariants.md#i13)) |
//! | field types | likewise — this is why there are no markers |
//! | record arity | the schema's field list has the count |
//! | a record terminator | a record ends when its fields do, which the schema says |
//! | a nested fact's predicate | `PredicateTy::Fact(p)` names it |
//! | a value-side presence flag | `Predicate::value` is `Some` or it is not |
//! | a null | [`PredicateTy`] has no null. PostgreSQL sends a `-1` length per column because SQL columns are nullable; this type model has nothing to spend that byte on |

use aperture_schema::{
    id::FactId,
    schema::{PredicateId, PredicateTy, Schema},
};

use crate::{error::WireError, varint};

/// The union branch for a reference the producer already holds an id for.
const REF_ID: u64 = 0;
/// The union branch for a reference sent as the target fact itself.
const REF_NESTED: u64 = 1;

/// A value in flight, typed against a [`PredicateTy`].
///
/// Deliberately **not** `aperture_encoding::tuple::Value`, and the difference is the
/// two things this type has to say that a stored value cannot: a record is
/// positional rather than named, and a reference may be a whole fact rather than an
/// id. A stored value is neither — it is named because the storage codec is
/// schema-free, and its references are always ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireValue {
    Int(i64),
    Str(String),
    Ref(WireRef),
    /// Fields **in schema order**, no names. See the module docs.
    Record(Box<[WireValue]>),
}

/// How a reference reaches us: as an id, or as the fact it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireRef {
    /// A producer that already holds the id — the shell, a deriver reading `put`'s
    /// return, an incremental writer.
    Id(FactId),
    /// The target fact itself, to be interned at ingest. What an indexer sends,
    /// because it means keeping no book of what it has already emitted.
    Nested(Box<WireFact>),
}

/// A fact in flight: its key, and its value side if the predicate declares one.
///
/// `predicate` is carried for the caller's benefit and is **not** encoded. A
/// top-level fact takes its predicate from the block header that frames it, and a
/// nested one from the `PredicateTy::Fact(p)` of the field it sits in — so writing
/// it into the fact as well would be a second source of truth for the same thing,
/// and a peer could disagree with itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireFact {
    pub predicate: PredicateId,
    pub key: WireValue,
    pub value: Option<WireValue>,
}

// ---- encoding --------------------------------------------------------------

/// Append `value`, encoded against `ty`.
///
/// The type is checked rather than assumed, and the check is not defensive
/// bookkeeping: the bytes carry no type of their own, so a value written against the
/// wrong one is not a malformed frame the peer can reject — it is a well-formed
/// frame that decodes to something else.
pub fn encode_value(
    out: &mut Vec<u8>,
    schema: &Schema,
    ty: &PredicateTy,
    value: &WireValue,
) -> Result<(), WireError> {
    match (ty, value) {
        (PredicateTy::Int, WireValue::Int(n)) => {
            varint::put_i64(out, *n);
            Ok(())
        }

        // Length-prefixed and raw. The storage codec escapes its contents and hangs
        // a terminator off the end, because a stored string has to stay
        // order-preserving and skippable without a schema; neither is true here, so
        // a blob costs its own size and a header, whatever bytes are in it.
        (PredicateTy::Str, WireValue::Str(s)) => {
            varint::put_u64(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
            Ok(())
        }

        (PredicateTy::Fact(target), WireValue::Ref(reference)) => {
            encode_ref(out, schema, *target, reference)
        }

        (PredicateTy::Record(field_tys), WireValue::Record(fields)) => {
            if field_tys.len() != fields.len() {
                return Err(WireError::TypeMismatch("record arity"));
            }

            // Concatenation, and that is the whole of it: no marker, no arity, no
            // terminator. The schema supplies all three.
            for ((_, field_ty), field) in field_tys.iter().zip(fields.iter()) {
                encode_value(out, schema, field_ty, field)?;
            }
            Ok(())
        }

        _ => Err(WireError::TypeMismatch("value does not fit this type")),
    }
}

fn encode_ref(
    out: &mut Vec<u8>,
    schema: &Schema,
    target: PredicateId,
    reference: &WireRef,
) -> Result<(), WireError> {
    match reference {
        WireRef::Id(id) => {
            // The id's own tag says which predicate it belongs to, so a reference
            // aimed at the wrong one is catchable here rather than at the far end —
            // for free, and against the exact fault a `Fact`-typed field is prone
            // to. See `decode_ref` for the same check read the other way.
            if id.predicate() != target {
                return Err(WireError::TypeMismatch(
                    "reference names a different predicate than the field declares",
                ));
            }
            varint::put_u64(out, REF_ID);
            varint::put_u64(out, id.raw());
            Ok(())
        }
        WireRef::Nested(fact) => {
            if fact.predicate != target {
                return Err(WireError::TypeMismatch(
                    "nested fact is of a different predicate than the field declares",
                ));
            }
            varint::put_u64(out, REF_NESTED);
            encode_fact(out, schema, fact)
        }
    }
}

/// Append a fact's key, and its value side if the predicate has one.
pub fn encode_fact(out: &mut Vec<u8>, schema: &Schema, fact: &WireFact) -> Result<(), WireError> {
    let declared = schema
        .get(fact.predicate)
        .ok_or(WireError::UnknownPredicate(fact.predicate.0))?;
    let predicate = declared.predicate();

    encode_value(out, schema, &predicate.key, &fact.key)?;

    match (&predicate.value, &fact.value) {
        (Some(value_ty), Some(value)) => encode_value(out, schema, value_ty, value),
        (None, None) => Ok(()),
        // No presence flag is written, so these two are unrecoverable at the far
        // end rather than merely wrong: the reader consults the schema and would
        // read the *next* fact's bytes as this one's value, or this one's value as
        // the next fact.
        (Some(_), None) => Err(WireError::TypeMismatch(
            "predicate declares a value side and the fact has none",
        )),
        (None, Some(_)) => Err(WireError::TypeMismatch(
            "predicate declares no value side and the fact has one",
        )),
    }
}

/// Encode one fact into a fresh buffer.
pub fn to_bytes(schema: &Schema, fact: &WireFact) -> Result<Vec<u8>, WireError> {
    let mut out = vec![];
    encode_fact(&mut out, schema, fact)?;
    Ok(out)
}

// ---- decoding --------------------------------------------------------------

/// Read a value of type `ty`, returning it and how many bytes it consumed.
pub fn decode_value(
    bytes: &[u8],
    schema: &Schema,
    ty: &PredicateTy,
) -> Result<(WireValue, usize), WireError> {
    match ty {
        PredicateTy::Int => {
            let (n, used) = varint::get_i64(bytes)?;
            Ok((WireValue::Int(n), used))
        }

        PredicateTy::Str => {
            let (len, used) = varint::get_u64(bytes)?;
            let rest = &bytes[used..];

            let len = usize::try_from(len).map_err(|_| WireError::LengthOutOfRange {
                declared: len,
                available: rest.len(),
            })?;

            if len > rest.len() {
                return Err(WireError::LengthOutOfRange {
                    declared: len as u64,
                    available: rest.len(),
                });
            }

            let text = std::str::from_utf8(&rest[..len]).map_err(|_| WireError::BadString)?;
            Ok((WireValue::Str(text.to_owned()), used + len))
        }

        PredicateTy::Fact(target) => {
            let (reference, used) = decode_ref(bytes, schema, *target)?;
            Ok((WireValue::Ref(reference), used))
        }

        PredicateTy::Record(field_tys) => {
            let mut fields = Vec::with_capacity(field_tys.len());
            let mut at = 0;

            for (_, field_ty) in field_tys.iter() {
                let (field, used) = decode_value(&bytes[at..], schema, field_ty)?;
                fields.push(field);
                at += used;
            }

            Ok((WireValue::Record(fields.into()), at))
        }
    }
}

fn decode_ref(
    bytes: &[u8],
    schema: &Schema,
    target: PredicateId,
) -> Result<(WireRef, usize), WireError> {
    let (form, used) = varint::get_u64(bytes)?;

    match form {
        REF_ID => {
            let (raw, id_used) = varint::get_u64(&bytes[used..])?;
            let id = FactId::from_raw(raw);

            // `from_raw` deliberately does not validate — it is for ids that have
            // already been checked — so a wire decode has to. Sequence 0 is
            // reserved, which is what makes a zeroed eight bytes detectably not a
            // fact ([I11](../../../docs/invariants.md#i11)).
            if id.sequence() == 0 {
                return Err(WireError::BadFactId(raw));
            }

            // And the tag has to be the predicate the field points at. A `Fact(p)`
            // field can only hold a reference to `p`, the id carries its own
            // predicate in its top bits, so this is a type check the snowflake makes
            // free — and it catches the one corruption a bare id is prone to,
            // an id from the right DB and the wrong tree.
            if id.predicate() != target {
                return Err(WireError::TypeMismatch(
                    "reference names a different predicate than the field declares",
                ));
            }

            Ok((WireRef::Id(id), used + id_used))
        }
        REF_NESTED => {
            let (fact, fact_used) = decode_fact(&bytes[used..], schema, target)?;
            Ok((WireRef::Nested(Box::new(fact)), used + fact_used))
        }
        // A branch index is a varint precisely so that a third form — a block-local
        // back-reference, the compaction deliberately left out of P0 — costs nothing
        // to add. Until one exists, an unknown index is a peer talking a dialect we
        // do not have, and the stream fails rather than guessing its width.
        other => Err(WireError::UnknownRefForm(other)),
    }
}

/// Read a fact of `predicate`, returning it and how many bytes it consumed.
pub fn decode_fact(
    bytes: &[u8],
    schema: &Schema,
    predicate: PredicateId,
) -> Result<(WireFact, usize), WireError> {
    let found = schema
        .get(predicate)
        .ok_or(WireError::UnknownPredicate(predicate.0))?;
    let declared = found.predicate();

    let (key, mut at) = decode_value(bytes, schema, &declared.key)?;

    let value = match &declared.value {
        Some(value_ty) => {
            let (value, used) = decode_value(&bytes[at..], schema, value_ty)?;
            at += used;
            Some(value)
        }
        None => None,
    };

    Ok((
        WireFact {
            predicate,
            key,
            value,
        },
        at,
    ))
}

/// Decode exactly one fact from `bytes`, insisting nothing is left over.
///
/// The framed entry point: a frame declares its own length, so bytes remaining
/// after a complete fact mean the peer's idea of the predicate differs from ours.
pub fn from_bytes(
    bytes: &[u8],
    schema: &Schema,
    predicate: PredicateId,
) -> Result<WireFact, WireError> {
    let (fact, used) = decode_fact(bytes, schema, predicate)?;

    if used != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - used));
    }

    Ok(fact)
}

/// Generators for a schema and a well-typed fact against it.
///
/// A schema has to be generated *with* the fact, not beside it, because the wire
/// encoding is driven by the schema at every step: what a nested reference expands
/// into is the target predicate's key type, so a fact is only well-typed relative to
/// one particular schema.
///
/// **References point strictly backwards** — predicate `i` may only name `j < i` —
/// which is not a simplification but the real constraint written down. A reference
/// in a key cannot be part of a cycle, because the target has to be fully identified
/// before the referring key has any bytes at all
/// ([chapter 3](../../../docs/03-storage-model.md#interning-a-nested-fact)). A
/// generator that drew cycles would draw facts no producer could send.
#[cfg(any(test, feature = "proptest"))]
pub mod proptest {
    use super::*;
    use ::proptest::prelude::*;
    use aperture_schema::schema::Predicate;
    use lasso::Rodeo;
    use std::sync::Arc;

    /// At most this many predicates, and so at most this deep a chain of nested
    /// references.
    const MAX_PREDICATES: usize = 4;
    const MAX_FIELDS: usize = 3;

    /// A type, interner-free so it shrinks as data rather than as interner handles.
    #[derive(Debug, Clone)]
    pub enum TySpec {
        Int,
        Str,
        /// A reference to the predicate at this index. Resolved modulo the
        /// predicates declared *before* the one holding it, so it always points
        /// backwards; a predicate with none before it gets [`TySpec::Int`] instead.
        Ref(u8),
        Record(Vec<TySpec>),
    }

    #[derive(Debug, Clone)]
    pub struct PredSpec {
        pub key: TySpec,
        pub value: Option<TySpec>,
    }

    /// A schema, and the draws that fill one of its facts.
    #[derive(Debug, Clone)]
    pub struct SchemaAndFact {
        pub predicates: Vec<PredSpec>,
        /// Which predicate the fact belongs to, taken modulo the schema's size.
        pub of: u8,
        /// The values a walk over the type consumes, cycled. A tape rather than a
        /// strategy built per type, because the type is runtime data — and it
        /// shrinks the way a reader wants: shorter tapes, smaller numbers.
        pub ints: Vec<i64>,
        pub texts: Vec<String>,
        pub picks: Vec<u8>,
    }

    fn arb_ty() -> impl Strategy<Value = TySpec> {
        let leaf = prop_oneof![
            Just(TySpec::Int),
            Just(TySpec::Str),
            (0u8..8).prop_map(TySpec::Ref),
        ];

        leaf.prop_recursive(3, 12, MAX_FIELDS as u32, |inner| {
            ::proptest::collection::vec(inner, 0..=MAX_FIELDS).prop_map(TySpec::Record)
        })
    }

    fn arb_text() -> impl Strategy<Value = String> {
        prop_oneof![
            6 => any::<String>(),
            1 => Just(String::new()),
            // The bytes a *storage* string has to escape, and this one does not —
            // the contrast is a test, so the generator has to reach it.
            1 => Just("\0".to_owned()),
            1 => Just("a\0b\0c".to_owned()),
            1 => Just("\u{1f600}".to_owned()),
        ]
    }

    pub fn arb_schema_and_fact() -> impl Strategy<Value = SchemaAndFact> {
        (
            ::proptest::collection::vec(
                (arb_ty(), ::proptest::option::of(arb_ty()))
                    .prop_map(|(key, value)| PredSpec { key, value }),
                1..=MAX_PREDICATES,
            ),
            any::<u8>(),
            ::proptest::collection::vec(any::<i64>(), 1..6),
            ::proptest::collection::vec(arb_text(), 1..6),
            ::proptest::collection::vec(any::<u8>(), 1..8),
        )
            .prop_map(|(predicates, of, ints, texts, picks)| SchemaAndFact {
                predicates,
                of,
                ints,
                texts,
                picks,
            })
    }

    impl SchemaAndFact {
        /// The predicate the drawn fact belongs to.
        #[must_use]
        pub fn predicate_id(&self) -> PredicateId {
            PredicateId((self.of as usize % self.predicates.len()) as u32)
        }

        /// Materialise the schema. Field names are `f0`, `f1`, … — ascending, so
        /// sorted-by-name is also declaration order, which is what a record's
        /// encoding order means ([chapter 6](../../../docs/06-types-and-schema.md)).
        #[must_use]
        pub fn schema(&self) -> Schema {
            let mut rodeo = Rodeo::new();
            let names: Vec<_> = (0..=MAX_FIELDS)
                .map(|f| rodeo.get_or_intern(format!("f{f}")))
                .collect();

            let predicates: Vec<Predicate> = self
                .predicates
                .iter()
                .enumerate()
                .map(|(index, spec)| Predicate {
                    name: rodeo.get_or_intern(format!("gen.P{index}")),
                    key: materialise(&spec.key, index, &names),
                    value: spec.value.as_ref().map(|ty| materialise(ty, index, &names)),
                })
                .collect();

            Schema::new(rodeo.into_reader(), Arc::from(predicates))
        }

        /// A well-typed fact of [`predicate_id`](Self::predicate_id) against
        /// [`schema`](Self::schema).
        #[must_use]
        pub fn fact(&self, schema: &Schema) -> WireFact {
            let mut tape = Tape {
                draws: self,
                int: 0,
                text: 0,
                pick: 0,
            };
            tape.fact(schema, self.predicate_id())
        }
    }

    /// `TySpec` → `PredicateTy`, resolving a reference against what is declared
    /// *before* `index`.
    fn materialise(ty: &TySpec, index: usize, names: &[lasso::Spur]) -> PredicateTy {
        match ty {
            TySpec::Int => PredicateTy::Int,
            TySpec::Str => PredicateTy::Str,
            // Nothing to point at: predicate 0 has no predecessor, so the position
            // becomes an int rather than a dangling reference.
            TySpec::Ref(_) if index == 0 => PredicateTy::Int,
            TySpec::Ref(which) => PredicateTy::Fact(PredicateId((*which as usize % index) as u32)),
            TySpec::Record(fields) => PredicateTy::Record(
                fields
                    .iter()
                    .enumerate()
                    .map(|(f, field)| (names[f], materialise(field, index, names)))
                    .collect(),
            ),
        }
    }

    struct Tape<'a> {
        draws: &'a SchemaAndFact,
        int: usize,
        text: usize,
        pick: usize,
    }

    impl Tape<'_> {
        fn next_int(&mut self) -> i64 {
            let value = self.draws.ints[self.int % self.draws.ints.len()];
            self.int += 1;
            value
        }

        fn next_text(&mut self) -> String {
            let value = self.draws.texts[self.text % self.draws.texts.len()].clone();
            self.text += 1;
            value
        }

        fn next_pick(&mut self) -> u8 {
            let value = self.draws.picks[self.pick % self.draws.picks.len()];
            self.pick += 1;
            value
        }

        fn fact(&mut self, schema: &Schema, predicate: PredicateId) -> WireFact {
            let declared = schema
                .get(predicate)
                .expect("a generated predicate")
                .predicate()
                .clone();

            WireFact {
                predicate,
                key: self.value(schema, &declared.key),
                value: declared.value.as_ref().map(|ty| self.value(schema, ty)),
            }
        }

        fn value(&mut self, schema: &Schema, ty: &PredicateTy) -> WireValue {
            match ty {
                PredicateTy::Int => WireValue::Int(self.next_int()),
                PredicateTy::Str => WireValue::Str(self.next_text()),

                // **Both branches of the union get drawn**, which is the point of
                // the pick: an id and a nested fact are two different encodings of
                // one field, and a battery that only ever drew one would say nothing
                // about the other.
                PredicateTy::Fact(target) => {
                    WireValue::Ref(if self.next_pick().is_multiple_of(2) {
                        let sequence = 1 + (self.next_int().unsigned_abs() % 1_000);
                        WireRef::Id(FactId::new(*target, sequence).expect("a generated id"))
                    } else {
                        WireRef::Nested(Box::new(self.fact(schema, *target)))
                    })
                }

                PredicateTy::Record(fields) => WireValue::Record(
                    fields
                        .iter()
                        .map(|(_, field_ty)| self.value(schema, field_ty))
                        .collect(),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{proptest::arb_schema_and_fact, *};
    use ::proptest::prelude::*;
    use aperture_schema::schema::Predicate;
    use lasso::Rodeo;
    use std::sync::Arc;

    /// A one-predicate schema: `gen.P0` with the given key type and no value side.
    fn schema_of(key: PredicateTy) -> Schema {
        let mut rodeo = Rodeo::new();
        let name = rodeo.get_or_intern("gen.P0");
        Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![Predicate {
                name,
                key,
                value: None,
            }]),
        )
    }

    fn encoded(key: PredicateTy, value: WireValue) -> Vec<u8> {
        let schema = schema_of(key);
        to_bytes(
            &schema,
            &WireFact {
                predicate: PredicateId(0),
                key: value,
                value: None,
            },
        )
        .expect("a well-typed fact")
    }

    // ---- the length properties ---------------------------------------------
    //
    // Three claims, each naming a storage-codec constraint that is *not* paid here.
    // Stated as arithmetic rather than as a golden byte count, so they say why the
    // number is what it is.

    /// **A string costs its length and a length prefix — whatever bytes are in it.**
    ///
    /// The storage codec escapes a string's contents so a terminator stays
    /// unambiguous and the encoding stays order-preserving, which makes a stored
    /// string's size depend on its *content*: every NUL costs two bytes. Nothing on
    /// the wire is compared or skipped without a schema, so there is no terminator
    /// and no escape, and a blob passes through at its own size.
    #[test]
    fn a_string_costs_its_own_length_whatever_is_in_it() {
        for text in ["", "abc", "\0", "a\0b\0c\0", "\u{1f600}"] {
            let bytes = encoded(PredicateTy::Str, WireValue::Str(text.to_owned()));

            assert_eq!(
                bytes.len(),
                varint::len_u64(text.len() as u64) + text.len(),
                "for {text:?}"
            );
        }

        // Said the other way, because it is the property that matters for a blob:
        // two strings of one length encode to one length, however different.
        let plain = encoded(PredicateTy::Str, WireValue::Str("abcd".to_owned()));
        let nasty = encoded(PredicateTy::Str, WireValue::Str("\0\0\0\0".to_owned()));
        assert_eq!(plain.len(), nasty.len());
    }

    /// **A record costs exactly its fields.** No marker, no arity, no terminator —
    /// the schema carries all three, so a record is Avro's "just the concatenation
    /// of the encodings of its fields" and the test is that literally.
    #[test]
    fn a_record_costs_exactly_its_fields_and_nothing_else() {
        let fields = vec![
            (PredicateTy::Int, WireValue::Int(12)),
            (PredicateTy::Str, WireValue::Str("key_of".to_owned())),
            (PredicateTy::Int, WireValue::Int(-3)),
        ];

        let separately: usize = fields
            .iter()
            .map(|(ty, value)| encoded(ty.clone(), value.clone()).len())
            .sum();

        let mut rodeo = Rodeo::new();
        let names: Vec<_> = (0..3)
            .map(|f| rodeo.get_or_intern(format!("f{f}")))
            .collect();
        let record_ty = PredicateTy::Record(
            names
                .iter()
                .zip(fields.iter())
                .map(|(name, (ty, _))| (*name, ty.clone()))
                .collect(),
        );

        let together = encoded(
            record_ty,
            WireValue::Record(fields.into_iter().map(|(_, v)| v).collect()),
        );

        assert_eq!(together.len(), separately);
    }

    /// **A nested reference costs one byte plus the fact.** The predicate is not
    /// repeated — `PredicateTy::Fact(p)` names it — so nesting is as cheap as the
    /// union branch that announces it.
    #[test]
    fn a_nested_reference_costs_one_byte_more_than_the_fact_it_carries() {
        let mut rodeo = Rodeo::new();
        let (target, referrer) = (
            rodeo.get_or_intern("gen.Target"),
            rodeo.get_or_intern("gen.Referrer"),
        );
        let schema = Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: target,
                    key: PredicateTy::Str,
                    value: None,
                },
                Predicate {
                    name: referrer,
                    key: PredicateTy::Fact(PredicateId(0)),
                    value: None,
                },
            ]),
        );

        let inner = WireFact {
            predicate: PredicateId(0),
            key: WireValue::Str("store/keys.py".to_owned()),
            value: None,
        };
        let inner_bytes = to_bytes(&schema, &inner).expect("a target fact");

        let nested = to_bytes(
            &schema,
            &WireFact {
                predicate: PredicateId(1),
                key: WireValue::Ref(WireRef::Nested(Box::new(inner))),
                value: None,
            },
        )
        .expect("a referring fact");

        assert_eq!(nested.len(), 1 + inner_bytes.len());
    }

    // ---- the round trip ------------------------------------------------------

    /// **A reference is checked against the predicate it names**, in both
    /// directions, and the snowflake tag is what makes that free: a `Fact(p)` field
    /// can only hold a reference to `p`, and an id carries its own predicate in its
    /// top bits.
    ///
    /// Worth a test of its own because it is the one corruption a bare id is prone
    /// to — an id from the right database and the wrong tree — which no length
    /// check and no checksum would catch.
    #[test]
    fn a_reference_to_the_wrong_predicate_is_refused_both_ways() {
        let schema = schema_of(PredicateTy::Fact(PredicateId(0)));
        let elsewhere = FactId::new(PredicateId(7), 1).expect("an id");

        let fact = WireFact {
            predicate: PredicateId(0),
            key: WireValue::Ref(WireRef::Id(elsewhere)),
            value: None,
        };

        assert!(matches!(
            to_bytes(&schema, &fact),
            Err(WireError::TypeMismatch(_))
        ));

        // And a peer that sends one anyway is refused at decode, rather than
        // producing a fact pointing into a tree that does not hold it. Branch 0 is
        // the id form, hand-written because the encoder above refuses to make it.
        let mut bytes = vec![];
        varint::put_u64(&mut bytes, 0);
        varint::put_u64(&mut bytes, elsewhere.raw());

        assert!(matches!(
            from_bytes(&bytes, &schema, PredicateId(0)),
            Err(WireError::TypeMismatch(_))
        ));
    }

    /// Sequence 0 is reserved, so a zeroed id is detectably not a fact — the
    /// property [I11](../../../docs/invariants.md#i11) keeps, checked where bytes
    /// arrive from outside.
    #[test]
    fn a_zeroed_reference_is_not_a_fact() {
        let schema = schema_of(PredicateTy::Fact(PredicateId(0)));

        let mut bytes = vec![];
        varint::put_u64(&mut bytes, 0);
        varint::put_u64(&mut bytes, 0);

        assert_eq!(
            from_bytes(&bytes, &schema, PredicateId(0)),
            Err(WireError::BadFactId(0))
        );
    }

    /// A frame declares its own length, so bytes left over mean the peer's idea of
    /// the predicate differs from ours — reported, not ignored, because ignoring
    /// them decodes the next value from the wrong offset.
    #[test]
    fn trailing_bytes_are_a_fault() {
        let schema = schema_of(PredicateTy::Int);
        let mut bytes = encoded(PredicateTy::Int, WireValue::Int(1));
        bytes.push(0x00);

        assert_eq!(
            from_bytes(&bytes, &schema, PredicateId(0)),
            Err(WireError::TrailingBytes(1))
        );
    }

    /// A truncated frame is an error rather than a short value, at every type.
    #[test]
    fn a_truncated_frame_is_refused() {
        let schema = schema_of(PredicateTy::Str);
        let full = encoded(PredicateTy::Str, WireValue::Str("abcdef".to_owned()));

        for cut in 0..full.len() {
            assert!(
                from_bytes(&full[..cut], &schema, PredicateId(0)).is_err(),
                "a frame cut to {cut} of {} bytes decoded",
                full.len()
            );
        }
    }

    /// **The census.** A round-trip property is worth exactly what the generator
    /// reaches, and the shape that matters here — a *nested* reference — is the one
    /// a lazy generator would never draw. So the shapes are counted rather than
    /// hoped for, in the discipline `flatten`'s battery already uses.
    #[test]
    fn the_generator_reaches_every_wire_shape() {
        use ::proptest::{
            strategy::{Strategy, ValueTree},
            test_runner::TestRunner,
        };

        const RUNS: usize = 400;

        #[derive(Default)]
        struct Seen {
            id_ref: bool,
            nested_ref: bool,
            nested_within_nested: bool,
            record: bool,
            empty_record: bool,
            value_side: bool,
            string_needing_escape_in_storage: bool,
        }

        fn walk(value: &WireValue, seen: &mut Seen, depth: usize) {
            match value {
                WireValue::Int(_) => {}
                WireValue::Str(text) => {
                    seen.string_needing_escape_in_storage |= text.contains('\0');
                }
                WireValue::Ref(WireRef::Id(_)) => seen.id_ref = true,
                WireValue::Ref(WireRef::Nested(fact)) => {
                    seen.nested_ref = true;
                    seen.nested_within_nested |= depth > 0;
                    walk(&fact.key, seen, depth + 1);
                    if let Some(value) = &fact.value {
                        walk(value, seen, depth + 1);
                    }
                }
                WireValue::Record(fields) => {
                    seen.record = true;
                    seen.empty_record |= fields.is_empty();
                    for field in fields.iter() {
                        walk(field, seen, depth);
                    }
                }
            }
        }

        let mut runner = TestRunner::deterministic();
        let mut seen = Seen::default();

        for _ in 0..RUNS {
            let spec = arb_schema_and_fact()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let schema = spec.schema();
            let fact = spec.fact(&schema);

            seen.value_side |= fact.value.is_some();
            walk(&fact.key, &mut seen, 0);
            if let Some(value) = &fact.value {
                walk(value, &mut seen, 0);
            }
        }

        let missing: Vec<&str> = [
            (seen.id_ref, "a reference sent as an id"),
            (seen.nested_ref, "a reference sent as a nested fact"),
            (
                seen.nested_within_nested,
                "a nested fact inside a nested fact",
            ),
            (seen.record, "a record"),
            (seen.empty_record, "an empty record"),
            (seen.value_side, "a predicate with a value side"),
            (
                seen.string_needing_escape_in_storage,
                "a string holding a NUL — what storage escapes and this does not",
            ),
        ]
        .into_iter()
        .filter_map(|(present, what)| (!present).then_some(what))
        .collect();

        assert!(
            missing.is_empty(),
            "{RUNS} generated facts never produced: {}",
            missing.join(", ")
        );
    }

    /// **The contrast, measured.** For the shapes a code index actually holds, the
    /// transport encoding is materially smaller than the storage one — which is the
    /// whole claim of this crate, so it is a test rather than a paragraph.
    ///
    /// Not a pointwise law, and the test does not pretend otherwise: a varint is
    /// *longer* than a fixed width at the extremes (`i64::MIN` is ten bytes here and
    /// nine in storage) and a length prefix passes three bytes at 16 KiB where a
    /// terminator stays at one. The claim is about the data, not about every value:
    /// line numbers, columns, arities and identifiers are small, and strings are
    /// mostly free of the bytes storage has to escape.
    #[test]
    fn transport_is_smaller_than_storage_for_the_shapes_a_code_index_holds() {
        use aperture_encoding::tuple::{Value, encode_key};

        let mut rodeo = Rodeo::new();
        let file = rodeo.get_or_intern("gen.File");
        let decl = rodeo.get_or_intern("gen.Decl");
        let names: Vec<_> = ["f0", "f1", "f2", "f3"]
            .iter()
            .map(|n| rodeo.get_or_intern(*n))
            .collect();

        // `{ file : ref, line : int, col : int, name : str }` — the example
        // indexer's declaration, field for field.
        let decl_key = PredicateTy::Record(
            vec![
                (names[0], PredicateTy::Fact(PredicateId(0))),
                (names[1], PredicateTy::Int),
                (names[2], PredicateTy::Int),
                (names[3], PredicateTy::Str),
            ]
            .into(),
        );

        let schema = Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: file,
                    key: PredicateTy::Str,
                    value: None,
                },
                Predicate {
                    name: decl,
                    key: decl_key.clone(),
                    value: None,
                },
            ]),
        );

        let target = FactId::new(PredicateId(0), 3).expect("an id");
        let rows: Vec<(i64, i64, &str)> = vec![
            (12, 4, "key_of"),
            (48, 8, "Store.put"),
            (131, 0, "decode_key"),
            (7, 4, "MARK_TERM"),
            (256, 12, "run_filters"),
        ];

        let (mut wire_total, mut store_total) = (0usize, 0usize);

        for (line, col, name) in rows {
            let wire = to_bytes(
                &schema,
                &WireFact {
                    predicate: PredicateId(1),
                    key: WireValue::Record(
                        vec![
                            WireValue::Ref(WireRef::Id(target)),
                            WireValue::Int(line),
                            WireValue::Int(col),
                            WireValue::Str(name.to_owned()),
                        ]
                        .into(),
                    ),
                    value: None,
                },
            )
            .expect("a well-typed fact");

            let stored = encode_key(
                &decl_key,
                &Value::Record(
                    vec![
                        ("f0".to_owned(), Value::FactRef(target)),
                        ("f1".to_owned(), Value::Int(line)),
                        ("f2".to_owned(), Value::Int(col)),
                        ("f3".to_owned(), Value::Str(name.to_owned())),
                    ]
                    .into(),
                ),
            )
            .expect("a well-typed key");

            wire_total += wire.len();
            store_total += stored.len();
        }

        assert!(
            wire_total * 4 <= store_total * 3,
            "transport {wire_total} B vs storage {store_total} B — expected at \
             least a quarter smaller over these rows"
        );
    }

    proptest! {
        /// **The round trip**, over generated schemas and well-typed facts, with
        /// both branches of the reference union reached.
        #[test]
        fn a_fact_round_trips(spec in arb_schema_and_fact()) {
            let schema = spec.schema();
            let fact = spec.fact(&schema);

            let bytes = to_bytes(&schema, &fact).expect("a well-typed fact encodes");
            let back = from_bytes(&bytes, &schema, fact.predicate);

            prop_assert_eq!(back, Ok(fact));
        }

        /// **Encoding is canonical**: whatever the decoder accepts re-encodes to the
        /// same bytes. A block carries a CRC32 and the same encoding is used on the
        /// wire and in a file, so "the same facts" has to mean "the same bytes" for
        /// a checksum to be worth computing.
        #[test]
        fn decoding_and_re_encoding_reproduces_the_bytes(spec in arb_schema_and_fact()) {
            let schema = spec.schema();
            let fact = spec.fact(&schema);

            let bytes = to_bytes(&schema, &fact).expect("a well-typed fact encodes");
            let back = from_bytes(&bytes, &schema, fact.predicate).expect("it decodes");
            let again = to_bytes(&schema, &back).expect("and re-encodes");

            prop_assert_eq!(again, bytes);
        }

        /// **A truncated frame never decodes**, at any cut point and any shape.
        ///
        /// The generated counterpart to the hand-written case above: a codec with no
        /// terminators and no per-field lengths relies on the schema to know where a
        /// value ends, so running off the end must be an error rather than a value
        /// built from whatever followed.
        #[test]
        fn a_truncated_fact_never_decodes(spec in arb_schema_and_fact()) {
            let schema = spec.schema();
            let fact = spec.fact(&schema);
            let bytes = to_bytes(&schema, &fact).expect("a well-typed fact encodes");

            for cut in 0..bytes.len() {
                prop_assert!(
                    from_bytes(&bytes[..cut], &schema, fact.predicate).is_err(),
                    "cut to {} of {} bytes decoded", cut, bytes.len()
                );
            }
        }
    }
}
