//! Writing a fact as a **well-typed Rust value** — the seam a hand-written deriver
//! emits through.
//!
//! [`TupleEncode`](crate::focus::tuple::TupleEncode) already lets a type encode itself
//! as a tuple, and that is the right primitive for the *codec*. It is the wrong one
//! for someone writing facts, for two reasons that both end in silently wrong bytes:
//!
//! - **A fact is not a tuple.** It belongs to a predicate, its key and its value are
//!   two separate encodings, and only the schema says which predicate has a value at
//!   all.
//! - **A record's field order is its schema's, not its struct's.** The encoding order
//!   is the order the *schema declares* the fields in — and schemas here list them
//!   sorted by name ([chapter 6]), which a Rust struct has no reason to. So a
//!   `struct Decl { module, name, line }` encoded field by field produces a key the
//!   read path decodes as `(line, module, name)`: no error, just a fact nobody can
//!   find. The positional encoder in [`tuple`](crate::focus::tuple) cannot catch that,
//!   because a tuple has no field names to check.
//! - **A key is flat and a value is not.** `encode_typed` is the obvious function to
//!   reach for and is wrong for a key, which is its own silent-mismatch trap; see
//!   [`encode_key`](crate::focus::tuple::encode_key).
//!
//! So a [`Fact`] names its fields and this module resolves them **against the
//! schema**: a name the predicate does not declare, a field left out, a value of the
//! wrong shape, or a value side on a predicate that has none are all errors before
//! anything is written. What reaches the encoder is already in declared order.
//!
//! ```ignore
//! struct Module { file: FactId, name: &'static str }
//!
//! impl Fact for Module {
//!     const PREDICATE: &'static str = "src.Module";
//!
//!     fn key(&self) -> Value {
//!         record([("file", self.file.to_value()), ("name", self.name.to_value())])
//!     }
//! }
//!
//! let id = db.put(&schema, &Module { file, name: "main" })?;
//! ```
//!
//! The id comes back because a reference *is* an id: the fact you write next names
//! this one by the value `put` returned, which is what makes referential integrity a
//! consequence of write order rather than a check ([I11]).
//!
//! # What this is not
//!
//! **Bulk ingestion.** Building a [`Value`] per fact costs an allocation per fact,
//! which is right for a deriver writing thousands and wrong for a loader writing
//! millions. Phase 7's fact-file path wants a streaming form that never materialises
//! the value; this one exists because the alternative for a person is to hand-encode
//! bytes in sorted-field order, and that is the mistake described above.
//!
//! **A `serde` derive.** `#[derive(Serialize)]` would remove the `impl` here, and
//! `Value` is the obvious target for one — but serde's data model has no fact
//! reference, and its struct fields arrive in declaration order, so a derive would
//! have to reorder against a schema it cannot see. The checks below are what a derive
//! would need to be layered on top of, not replaced by.
//!
//! [chapter 6]: ../../../docs/06-types-and-schema.md
//! [I11]: ../../../docs/invariants.md#i11

use crate::focus::{
    error::FactError,
    id::FactId,
    schema::{PredicateId, PredicateTy, Schema, SchemaInterner},
    tuple::{Value, encode_key, encode_typed},
};

/// A Rust value that can be written as a fact.
///
/// The predicate is a *name*, resolved against the schema at write time rather than
/// an id baked into the type: a predicate id is a position in a schema
/// ([chapter 6](../../../docs/06-types-and-schema.md)), so a type carrying one would
/// silently mean a different predicate under a schema that declared them in another
/// order.
pub trait Fact {
    /// The declared name of the predicate this is a fact of.
    const PREDICATE: &'static str;

    /// The key, with its fields **named**. Order does not matter: the schema decides
    /// the encoding order, and this is checked against it.
    fn key(&self) -> Value;

    /// The value side, for a predicate that has one. A predicate with a declared
    /// value must be given one, and a predicate without one must not.
    fn value(&self) -> Option<Value> {
        None
    }
}

/// A Rust value that can stand in a fact's key or value.
///
/// Implement it for a type that is a **nested record** in a key; the scalars and
/// references are covered here.
pub trait ToValue {
    fn to_value(&self) -> Value;
}

impl ToValue for i64 {
    fn to_value(&self) -> Value {
        Value::Int(*self)
    }
}

impl ToValue for str {
    fn to_value(&self) -> Value {
        Value::Str(self.to_owned())
    }
}

impl ToValue for String {
    fn to_value(&self) -> Value {
        Value::Str(self.clone())
    }
}

/// A reference to another fact — which is what the id a write returned *is*.
impl ToValue for FactId {
    fn to_value(&self) -> Value {
        Value::FactRef(*self)
    }
}

impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}

/// A record of named fields, in any order.
pub fn record<'a>(fields: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    Value::Record(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

/// A fact's predicate and its encoded key and value bytes, ready to write.
///
/// Separate from writing it so that a caller with its own store — a test, a differing
/// backend, a fact-file writer — reaches the same checks.
pub fn encode<F: Fact>(
    schema: &Schema,
    fact: &F,
) -> Result<(PredicateId, Vec<u8>, Vec<u8>), FactError> {
    let (id, predicate) = schema
        .find_position(F::PREDICATE)
        .ok_or_else(|| FactError::UnknownPredicate(F::PREDICATE.to_owned()))?;

    let key_ty = predicate.key().ty.clone();
    let value_ty = predicate.value().map(|value| value.ty.clone());

    let key = checked(schema.interner(), F::PREDICATE, &key_ty, &fact.key())?;
    let key = encode_key(&key_ty, &key)?;

    let value = match (value_ty, fact.value()) {
        (None, None) => Vec::new(),
        (Some(ty), Some(value)) => {
            let value = checked(schema.interner(), F::PREDICATE, &ty, &value)?;
            encode_typed(&ty, &value)?
        }
        (None, Some(_)) => {
            return Err(FactError::UnexpectedValue(F::PREDICATE.to_owned()));
        }
        (Some(_), None) => {
            return Err(FactError::MissingValue(F::PREDICATE.to_owned()));
        }
    };

    Ok((id, key, value))
}

/// `value` checked against `ty` and reordered into **declared** order.
///
/// The reordering is the point. Returning a canonical `Value` rather than encoding
/// here keeps one encoder: what comes back is what
/// [`encode_typed`](crate::focus::tuple::encode_typed) already writes positionally,
/// so the name resolution cannot drift from the bytes.
fn checked(
    interner: &SchemaInterner,
    predicate: &str,
    ty: &PredicateTy,
    value: &Value,
) -> Result<Value, FactError> {
    let mismatch = || FactError::TypeMismatch {
        predicate: predicate.to_owned(),
        expected: describe(ty),
        got: shape(value),
    };

    match (ty, value) {
        (PredicateTy::Int, Value::Int(_)) | (PredicateTy::Str, Value::Str(_)) => Ok(value.clone()),

        // A reference has to name the predicate the field *declares*, and the id
        // carries its predicate in its own tag, so this is a compare rather than a
        // lookup. Not merely a nicety: the read path reads the referenced row
        // against the declared predicate's key layout, so a reference to another
        // predicate is a fact that either errors when followed or answers with
        // another type's bytes — and neither is visible from the field itself.
        // Sequence 0 is no fact's id, here for the same reason.
        (PredicateTy::Fact(predicate), Value::FactRef(id))
            if id.predicate() == *predicate && id.sequence() != 0 =>
        {
            Ok(value.clone())
        }

        (PredicateTy::Record(field_tys), Value::Record(given)) => {
            let mut out = Vec::with_capacity(field_tys.len());
            let mut used = vec![false; given.len()];

            for (name, field_ty) in field_tys.iter() {
                let name = interner
                    .resolve(*name)
                    .ok_or_else(|| FactError::UnknownField {
                        predicate: predicate.to_owned(),
                        field: "?".to_owned(),
                    })?;

                let at = given
                    .iter()
                    .position(|(given, _)| given == name)
                    .ok_or_else(|| FactError::MissingField {
                        predicate: predicate.to_owned(),
                        field: name.to_owned(),
                    })?;

                used[at] = true;
                out.push((
                    name.to_owned(),
                    checked(interner, predicate, field_ty, &given[at].1)?,
                ));
            }

            // A name the predicate does not declare. Reported rather than ignored:
            // a misspelled field would otherwise write a fact missing the one it
            // meant to set, which is a fact nobody looks for.
            if let Some((name, _)) = given
                .iter()
                .zip(&used)
                .find(|(_, used)| !**used)
                .map(|(field, _)| field)
            {
                return Err(FactError::UnknownField {
                    predicate: predicate.to_owned(),
                    field: name.clone(),
                });
            }

            Ok(Value::Record(out.into()))
        }

        _ => Err(mismatch()),
    }
}

/// A declared type, for a diagnostic.
fn describe(ty: &PredicateTy) -> String {
    match ty {
        PredicateTy::Int => "int".to_owned(),
        PredicateTy::Str => "string".to_owned(),
        PredicateTy::Fact(predicate) => format!("a reference to predicate {}", predicate.0),
        PredicateTy::Record(fields) => format!("a record of {} field(s)", fields.len()),
    }
}

/// What was offered, in the same vocabulary.
///
/// A reference names the predicate it points at, so that the mismatch this most
/// often reports — the right *kind* of value pointing at the wrong predicate —
/// reads as two predicates rather than as "expected a reference, got a
/// reference".
fn shape(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Int(_) => "int".to_owned(),
        Value::Str(_) => "string".to_owned(),
        Value::FactRef(id) if id.sequence() == 0 => "the reserved fact id".to_owned(),
        Value::FactRef(id) => format!("a reference to predicate {}", id.predicate().0),
        Value::Record(fields) => format!("a record of {} field(s)", fields.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{fixture, tuple::decode_key};

    /// `test.Foo : { id : int, name : string } -> string` — a record key whose
    /// fields sort `id`, `name`, and a value side.
    struct Foo {
        id: i64,
        name: &'static str,
        value: &'static str,
    }

    impl Fact for Foo {
        const PREDICATE: &'static str = "test.Foo";

        fn key(&self) -> Value {
            // Deliberately **not** in declared order: that is the whole point.
            record([("name", self.name.to_value()), ("id", self.id.to_value())])
        }

        fn value(&self) -> Option<Value> {
            Some(self.value.to_value())
        }
    }

    fn foo() -> Foo {
        Foo {
            id: 1,
            name: "ann",
            value: "one",
        }
    }

    fn encoded<F: Fact>(fact: &F) -> Result<(PredicateId, Vec<u8>, Vec<u8>), FactError> {
        encode(&fixture::schema(), fact)
    }

    /// **The reason this module exists.** Key fields are encoded in the order the
    /// *schema* declares, whatever order the fact lists them in — so a struct whose
    /// fields happen to be declared the other way round still writes a findable fact.
    #[test]
    fn fields_are_encoded_in_the_schemas_order_not_the_facts() {
        let (predicate, key, value) = encoded(&foo()).expect("a well-formed fact");

        assert_eq!(predicate, PredicateId(0), "test.Foo is the first predicate");

        // The bytes are `id` then `name`, which is what the read path expects — and
        // is the reverse of the order `Foo::key` lists them in.
        let expected = fixture::facts()
            .into_iter()
            .find(|fact| fact.predicate == PredicateId(0) && fact.sequence == 1)
            .expect("the fixture's first test.Foo");

        assert_eq!(key, expected.key);
        assert_eq!(value, expected.value);
    }

    /// ...and the round trip says so in terms of values rather than bytes: what was
    /// written decodes back to what the fact meant.
    #[test]
    fn a_written_key_decodes_back_to_the_fact() {
        let schema = fixture::schema();
        let (_, key, _) = encoded(&foo()).expect("a well-formed fact");

        let interner = crate::focus::schema::LocalInterner::new(schema.interner().clone());
        let ty = schema
            .get(PredicateId(0))
            .expect("test.Foo")
            .key()
            .ty
            .clone();

        // `decode_key`, not `decode_typed`: a key is flat.
        assert_eq!(
            decode_key(&interner, &key, &ty).expect("decodes"),
            record([("id", 1.to_value()), ("name", "ann".to_value())]),
        );
    }

    /// A predicate no schema declares. The name is resolved at write time precisely
    /// so this is an error rather than a panic or a write to predicate 0.
    #[test]
    fn an_unknown_predicate_is_reported() {
        struct Nope;
        impl Fact for Nope {
            const PREDICATE: &'static str = "test.Nope";
            fn key(&self) -> Value {
                Value::Int(1)
            }
        }

        assert!(matches!(
            encoded(&Nope),
            Err(FactError::UnknownPredicate(name)) if name == "test.Nope"
        ));
    }

    /// A misspelled field would otherwise write a fact missing the one it meant to
    /// set — a fact nobody looks for, and no error anywhere.
    #[test]
    fn a_field_the_predicate_does_not_declare_is_reported() {
        struct Typo;
        impl Fact for Typo {
            const PREDICATE: &'static str = "test.Foo";
            fn key(&self) -> Value {
                record([("id", 1.to_value()), ("nmae", "ann".to_value())])
            }
            fn value(&self) -> Option<Value> {
                Some("one".to_value())
            }
        }

        assert!(matches!(
            encoded(&Typo),
            // `name` is missing before `nmae` is unknown: the walk is over the
            // *declared* fields, and either message names the mistake.
            Err(FactError::MissingField { field, .. }) if field == "name"
        ));
    }

    /// A field left out entirely. The encoding is positional, so a short record is
    /// not a record with a hole in it — it is bytes that decode as something else.
    #[test]
    fn a_missing_field_is_reported() {
        struct Partial;
        impl Fact for Partial {
            const PREDICATE: &'static str = "test.Foo";
            fn key(&self) -> Value {
                record([("id", 1.to_value())])
            }
            fn value(&self) -> Option<Value> {
                Some("one".to_value())
            }
        }

        assert!(matches!(
            encoded(&Partial),
            Err(FactError::MissingField { field, .. }) if field == "name"
        ));
    }

    /// A field of the wrong shape, including the case a hand-written deriver is most
    /// likely to reach: an integer where a **reference** belongs.
    #[test]
    fn a_field_of_the_wrong_type_is_reported() {
        struct Swapped;
        impl Fact for Swapped {
            const PREDICATE: &'static str = "test.Foo";
            fn key(&self) -> Value {
                record([("id", "ann".to_value()), ("name", 1.to_value())])
            }
            fn value(&self) -> Option<Value> {
                Some("one".to_value())
            }
        }

        assert!(matches!(
            encoded(&Swapped),
            Err(FactError::TypeMismatch { .. })
        ));

        struct NotAReference;
        impl Fact for NotAReference {
            const PREDICATE: &'static str = "test.Ref";
            fn key(&self) -> Value {
                record([("of", 1.to_value())])
            }
        }

        let error = encoded(&NotAReference).expect_err("an int is not a reference");
        assert!(
            error.to_string().contains("reference"),
            "the message should say what was expected: {error}"
        );
    }

    /// A value side is the schema's business, not the fact's: `test.Foo` has one and
    /// `test.Bar` does not, and getting either wrong is caught rather than written.
    #[test]
    fn a_value_side_must_match_the_declaration() {
        struct NoValue;
        impl Fact for NoValue {
            const PREDICATE: &'static str = "test.Foo";
            fn key(&self) -> Value {
                record([("id", 1.to_value()), ("name", "ann".to_value())])
            }
        }

        assert!(matches!(encoded(&NoValue), Err(FactError::MissingValue(_))));

        struct SpareValue;
        impl Fact for SpareValue {
            const PREDICATE: &'static str = "test.Bar";
            fn key(&self) -> Value {
                record([("id", 1.to_value())])
            }
            fn value(&self) -> Option<Value> {
                Some("nothing asked for this".to_value())
            }
        }

        assert!(matches!(
            encoded(&SpareValue),
            Err(FactError::UnexpectedValue(_))
        ));
    }

    /// A nested record is checked by name at every depth, not only the top.
    #[test]
    fn a_nested_record_is_checked_too() {
        struct Nested(i64);
        impl Fact for Nested {
            const PREDICATE: &'static str = "test.Nested";
            fn key(&self) -> Value {
                record([("outer", record([("inner", self.0.to_value())]))])
            }
        }

        let (_, key, _) = encoded(&Nested(1)).expect("a well-formed fact");
        assert_eq!(
            key,
            fixture::facts()
                .into_iter()
                .find(|fact| fact.predicate == PredicateId(4) && fact.sequence == 1)
                .expect("the fixture's first test.Nested")
                .key,
        );

        struct WrongInner(i64);
        impl Fact for WrongInner {
            const PREDICATE: &'static str = "test.Nested";
            fn key(&self) -> Value {
                record([("outer", record([("innr", self.0.to_value())]))])
            }
        }

        assert!(matches!(
            encoded(&WrongInner(1)),
            Err(FactError::MissingField { field, .. }) if field == "inner"
        ));
    }

    /// **The encoding order is the order the schema *declares*, not alphabetical
    /// order** — the schemas here happen to be sorted by name, and nothing in the
    /// codec depends on it.
    ///
    /// Worth pinning because the two are indistinguishable on every schema in the
    /// repo, so a change that started sorting would pass the whole suite. This
    /// predicate declares `z` before `a`, and if the encoder sorted, the *string*
    /// would lead and the ordering would go the other way.
    #[test]
    fn the_encoding_order_is_the_declared_order() {
        use crate::focus::schema::Predicate;
        use lasso::Rodeo;
        use std::sync::Arc;

        let mut names = Rodeo::new();
        let z = names.get_or_intern("z");
        let a = names.get_or_intern("a");
        let rev = names.get_or_intern("test.Rev");

        let schema = Schema::new(
            names.into_reader(),
            Arc::from(vec![Predicate {
                name: rev,
                key: PredicateTy::Record(Arc::from([(z, PredicateTy::Int), (a, PredicateTy::Str)])),
                value: None,
            }]),
        );

        struct Rev(i64, &'static str);
        impl Fact for Rev {
            const PREDICATE: &'static str = "test.Rev";
            fn key(&self) -> Value {
                record([("a", self.1.to_value()), ("z", self.0.to_value())])
            }
        }

        let key = |z, a| encode(&schema, &Rev(z, a)).expect("a well-formed fact").1;

        assert!(
            key(1, "zzz") < key(2, "aaa"),
            "`z` is declared first, so the integer decides the order",
        );
    }

    /// **A key written with the record wrapper is a key no query can find.**
    ///
    /// `encode_typed` is the obvious function to reach for, and for a *value* it is
    /// the right one — so this is the mistake the API exists to make impossible. It is
    /// silent: both encodings are well-formed bytes, the write succeeds, and the seek
    /// simply never meets them.
    #[test]
    fn a_key_is_flat_and_a_wrapped_one_would_never_match() {
        use crate::focus::tuple::{MARK_RECORD, encode_typed};

        let schema = fixture::schema();
        let ty = schema
            .get(PredicateId(0))
            .expect("test.Foo")
            .key()
            .ty
            .clone();

        let value = record([("id", 1.to_value()), ("name", "ann".to_value())]);
        let (_, written, _) = encoded(&foo()).expect("a well-formed fact");
        let wrapped = encode_typed(&ty, &value).expect("encodes");

        assert_ne!(written, wrapped);
        assert_eq!(wrapped[0], MARK_RECORD, "the wrapper is what differs");
        assert_eq!(
            written,
            wrapped[1..wrapped.len() - 1],
            "a flat key is exactly the wrapped one without its marker and terminator",
        );
    }

    /// A **reference is the id a write returned**, so a fact pointing at another is
    /// written by taking that value — which is the whole ergonomics this API is for.
    #[test]
    fn a_reference_is_written_as_the_id_a_write_returned() {
        struct Ref(FactId);
        impl Fact for Ref {
            const PREDICATE: &'static str = "test.Ref";
            fn key(&self) -> Value {
                record([("of", self.0.to_value())])
            }
        }

        let target = FactId::new(PredicateId(0), 1).expect("a fact id");
        let (predicate, key, _) = encoded(&Ref(target)).expect("a well-formed fact");

        assert_eq!(predicate, PredicateId(9));
        assert_eq!(key, crate::focus::tuple::fact_ref_bytes(target));
    }

    /// ...and it has to be the id of a fact of the **declared** predicate.
    ///
    /// `test.Ref` declares `of : test.Foo`, so an id the store returned for a
    /// `test.Bar` is a type error even though both are ids. The tag inside the id
    /// is what makes it checkable here, before any bytes exist — which is where
    /// this module's whole argument says a fact's mistakes belong. Left to the
    /// read path it is not a type error at all: the field reads back as a
    /// perfectly well-formed reference, and only a query that *follows* it
    /// discovers that the row on the other end has another predicate's key
    /// layout.
    #[test]
    fn a_reference_must_name_the_declared_predicate() {
        struct Ref(FactId);
        impl Fact for Ref {
            const PREDICATE: &'static str = "test.Ref";
            fn key(&self) -> Value {
                record([("of", self.0.to_value())])
            }
        }

        let elsewhere = FactId::new(PredicateId(1), 1).expect("a fact id");
        let error = encoded(&Ref(elsewhere)).expect_err("test.Bar is not test.Foo");

        assert!(
            matches!(error, FactError::TypeMismatch { .. }),
            "got {error:?}"
        );
        assert!(
            error.to_string().contains("reference to predicate 0")
                && error.to_string().contains("reference to predicate 1"),
            "the message should name both predicates: {error}"
        );

        // And the reserved sequence, which is no fact's id at all.
        assert!(
            encoded(&Ref(FactId::from_raw(0)))
                .expect_err("sequence 0 is reserved")
                .to_string()
                .contains("reserved"),
        );
    }
}
