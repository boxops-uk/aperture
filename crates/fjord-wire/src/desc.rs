//! **Row descriptors** — the outbound direction's answer to "where does the type
//! come from".
//!
//! A fact going *in* takes its shape from the schema: the block names a predicate,
//! and the predicate's key type says everything ([`value`](crate::value)). A row
//! coming *out* cannot, because a query row is shaped by the query's **head** —
//! `{a = X, b = Y.name}` is a record no predicate declares. So the server sends a
//! descriptor once per query stream and then rows positionally against it, which is
//! PostgreSQL's `RowDescription` before its `DataRow`s and the reason §6 borrows that
//! shape.
//!
//! The descriptor *is* a type, which is what keeps this from being a second codec:
//! [`Desc`] converts to a
//! [`fjord_schema::schema::PredicateTy`] and rows are then encoded
//! and decoded by exactly the machinery a fact's key is. The only thing a descriptor
//! adds is that it carries its record field **names** as text — a `PredicateTy` holds
//! interned symbols, and a peer has no interner.
//!
//! ```text
//!   T  [descriptor]        once, when the stream opens
//!   D  [row]               per row, positional against it
//!   D  [row]
//!   C  [complete]
//! ```

use fjord_schema::schema::{Alternative, LocalInterner, PredicateId, PredicateTy, Schema, Symbol};

use crate::{error::WireError, varint};

const TAG_INT: u64 = 0;
const TAG_STR: u64 = 1;
const TAG_FACT: u64 = 2;
const TAG_RECORD: u64 = 3;
/// **Appended**, which is what keeps an older peer honest: it meets a tag it has no
/// case for and reports [`WireError::UnknownRefForm`] rather than mis-reading the
/// bytes that follow. Renumbering any tag above would do the opposite.
const TAG_UNION: u64 = 4;

/// A row's shape, with names a peer can read.
///
/// The same cases as [`PredicateTy`], which is not a coincidence — a well-typed head
/// resolves to one of them — but with field and alternative names as `String` rather
/// than as interned symbols, because the interner is ours and not the peer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desc {
    Int,
    Str,
    Fact(PredicateId),
    /// Fields in order. A row's values follow this order and carry no names.
    Record(Box<[(String, Desc)]>),
    /// Alternatives, each with its **name and discriminant**. A row carries only the
    /// tag, so this is what lets a peer print `{alt = …}` rather than `{4 = …}` — and
    /// it is why a descriptor carries the number as well as the name: the row is
    /// matched by tag, and the two must not be re-derived from position at either
    /// end.
    Union(Box<[(String, u32, Desc)]>),
}

impl Desc {
    /// The descriptor for a schema type, resolving its field names.
    ///
    /// # Errors
    ///
    /// [`WireError::TypeMismatch`] if a record field's name is not in the schema's
    /// interner, which would mean a type built against a different schema.
    pub fn of(schema: &Schema, ty: &PredicateTy) -> Result<Desc, WireError> {
        Ok(match ty {
            PredicateTy::Int => Desc::Int,
            PredicateTy::Str => Desc::Str,
            PredicateTy::Fact(id) => Desc::Fact(*id),
            PredicateTy::Record(fields) => Desc::Record(
                fields
                    .iter()
                    .map(|(name, field)| {
                        let name = schema
                            .interner()
                            .resolve(*name)
                            .ok_or(WireError::TypeMismatch(
                                "a record field name this schema cannot resolve",
                            ))?
                            .to_owned();
                        Ok((name, Desc::of(schema, field)?))
                    })
                    .collect::<Result<Vec<_>, WireError>>()?
                    .into(),
            ),
            PredicateTy::Union(alts) => Desc::Union(
                alts.iter()
                    .map(|alt| {
                        let name = schema
                            .interner()
                            .resolve(alt.name)
                            .ok_or(WireError::TypeMismatch(
                                "an alternative name this schema cannot resolve",
                            ))?
                            .to_owned();
                        Ok((name, alt.disc, Desc::of(schema, &alt.ty)?))
                    })
                    .collect::<Result<Vec<_>, WireError>>()?
                    .into(),
            ),
        })
    }

    /// Back to a schema type, for the value codec to drive on.
    ///
    /// **The record field names in the result are placeholders**, and nothing may
    /// resolve them. `PredicateTy::Record` holds a bare `Spur`, so the *tier* a name
    /// came from — schema or per-query local — cannot be carried, and a local `Spur`
    /// and a schema `Spur` of the same number are different names. Resolving one
    /// afterwards does not fail; it silently answers with the wrong string.
    ///
    /// That costs nothing, because the encoding is **positional**: `encode_value`
    /// zips a record's fields against its type's and never looks at a name. The names
    /// live in the [`Desc`] itself, which is what a peer receives and reads.
    #[must_use]
    pub fn to_ty(&self, interner: &mut LocalInterner) -> PredicateTy {
        match self {
            Desc::Int => PredicateTy::Int,
            Desc::Str => PredicateTy::Str,
            Desc::Fact(id) => PredicateTy::Fact(*id),
            Desc::Record(fields) => PredicateTy::Record(
                fields
                    .iter()
                    .map(|(name, field)| {
                        let symbol = match interner.get_or_intern(name) {
                            Symbol::Schema(spur) | Symbol::Local(spur) => spur,
                        };
                        (symbol, field.to_ty(interner))
                    })
                    .collect(),
            ),
            // Unlike a record's, an alternative's name is **not** a placeholder: a
            // decoded union value carries it, so this is the name a peer sees. The
            // discriminant is what the codec matches on either way.
            Desc::Union(alts) => PredicateTy::Union(
                alts.iter()
                    .map(|(name, disc, alt)| {
                        let symbol = match interner.get_or_intern(name) {
                            Symbol::Schema(spur) | Symbol::Local(spur) => spur,
                        };
                        Alternative {
                            name: symbol,
                            disc: *disc,
                            ty: alt.to_ty(interner),
                        }
                    })
                    .collect(),
            ),
        }
    }
}

/// Append a descriptor.
pub fn encode_desc(out: &mut Vec<u8>, desc: &Desc) {
    match desc {
        Desc::Int => varint::put_u64(out, TAG_INT),
        Desc::Str => varint::put_u64(out, TAG_STR),
        Desc::Fact(id) => {
            varint::put_u64(out, TAG_FACT);
            varint::put_u64(out, u64::from(id.0));
        }
        Desc::Record(fields) => {
            varint::put_u64(out, TAG_RECORD);
            varint::put_u64(out, fields.len() as u64);
            for (name, field) in fields.iter() {
                varint::put_u64(out, name.len() as u64);
                out.extend_from_slice(name.as_bytes());
                encode_desc(out, field);
            }
        }
        Desc::Union(alts) => {
            varint::put_u64(out, TAG_UNION);
            varint::put_u64(out, alts.len() as u64);
            for (name, disc, alt) in alts.iter() {
                varint::put_u64(out, name.len() as u64);
                out.extend_from_slice(name.as_bytes());
                varint::put_u64(out, u64::from(*disc));
                encode_desc(out, alt);
            }
        }
    }
}

/// Read a descriptor, returning it and the bytes it took.
///
/// Unlike a value, a descriptor **is** self-describing — it has to be, since it is
/// the thing that tells the reader what everything else means. That is the one place
/// this format carries tags, and it carries them exactly once per stream rather than
/// once per field per row.
pub fn decode_desc(bytes: &[u8]) -> Result<(Desc, usize), WireError> {
    let (tag, mut at) = varint::get_u64(bytes)?;

    let desc = match tag {
        TAG_INT => Desc::Int,
        TAG_STR => Desc::Str,
        TAG_FACT => {
            let (id, used) = varint::get_u64(&bytes[at..])?;
            at += used;
            let id = u32::try_from(id).map_err(|_| WireError::UnknownPredicate(u32::MAX))?;
            Desc::Fact(PredicateId(id))
        }
        TAG_RECORD => {
            let count = take_count(bytes, &mut at)?;

            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                let name = take_name(bytes, &mut at)?;

                let (field, used) = decode_desc(&bytes[at..])?;
                at += used;

                fields.push((name, field));
            }

            Desc::Record(fields.into())
        }
        TAG_UNION => {
            let count = take_count(bytes, &mut at)?;

            let mut alts = Vec::with_capacity(count);
            for _ in 0..count {
                let name = take_name(bytes, &mut at)?;

                let (disc, used) = varint::get_u64(&bytes[at..])?;
                at += used;
                let disc = u32::try_from(disc).map_err(|_| WireError::UnknownDiscriminant(disc))?;

                let (alt, used) = decode_desc(&bytes[at..])?;
                at += used;

                alts.push((name, disc, alt));
            }

            Desc::Union(alts.into())
        }
        other => return Err(WireError::UnknownRefForm(other)),
    };

    Ok((desc, at))
}

/// A count of things to follow, checked before it sizes an allocation — it came
/// from a peer.
fn take_count(bytes: &[u8], at: &mut usize) -> Result<usize, WireError> {
    let (count, used) = varint::get_u64(&bytes[*at..])?;
    *at += used;

    let count = usize::try_from(count).map_err(|_| WireError::LengthOutOfRange {
        declared: count,
        available: bytes.len(),
    })?;

    // Each thing costs at least a byte, so a count past what is left cannot be
    // honoured whatever follows.
    if count > bytes.len() {
        return Err(WireError::LengthOutOfRange {
            declared: count as u64,
            available: bytes.len(),
        });
    }

    Ok(count)
}

/// A length-prefixed name — a record's field or a union's alternative.
fn take_name(bytes: &[u8], at: &mut usize) -> Result<String, WireError> {
    let (len, used) = varint::get_u64(&bytes[*at..])?;
    *at += used;

    let len = usize::try_from(len).map_err(|_| WireError::LengthOutOfRange {
        declared: len,
        available: bytes.len() - *at,
    })?;

    if *at + len > bytes.len() {
        return Err(WireError::LengthOutOfRange {
            declared: len as u64,
            available: bytes.len() - *at,
        });
    }

    let name = std::str::from_utf8(&bytes[*at..*at + len])
        .map_err(|_| WireError::BadString)?
        .to_owned();
    *at += len;

    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::proptest::prelude::*;

    fn arb_desc() -> impl Strategy<Value = Desc> {
        let leaf = prop_oneof![
            Just(Desc::Int),
            Just(Desc::Str),
            (0u32..64).prop_map(|id| Desc::Fact(PredicateId(id))),
        ];

        leaf.prop_recursive(3, 16, 4, |inner| {
            // `Rc`, not `Arc`: a strategy is not `Send`, and a generator runs on one thread.
            let inner = std::rc::Rc::new(inner);
            let names = || ::proptest::sample::select(vec!["a", "name", "line", "of", ""]);

            prop_oneof![
                ::proptest::collection::vec((names(), inner.clone()), 0..4).prop_map(|fields| {
                    Desc::Record(
                        fields
                            .into_iter()
                            .map(|(n, d)| (n.to_owned(), d))
                            .collect::<Vec<_>>()
                            .into(),
                    )
                }),
                // Tags drawn wide, including zero and numbers far past any count of
                // alternatives: a descriptor carries the number, so a reader that
                // rebuilt it from position would be caught by the round trip.
                ::proptest::collection::vec((names(), 0u32..100_000, inner), 1..4).prop_map(
                    |alts| {
                        Desc::Union(
                            alts.into_iter()
                                .map(|(n, disc, d)| (n.to_owned(), disc, d))
                                .collect::<Vec<_>>()
                                .into(),
                        )
                    }
                ),
            ]
        })
    }

    #[test]
    fn a_scalar_descriptor_is_one_byte() {
        let mut out = vec![];
        encode_desc(&mut out, &Desc::Int);
        assert_eq!(out.len(), 1);

        // Which is the point of sending it once per stream rather than per row: the
        // tags this format otherwise refuses are affordable exactly here.
        assert_eq!(decode_desc(&out), Ok((Desc::Int, 1)));
    }

    #[test]
    fn a_truncated_descriptor_is_refused() {
        let mut out = vec![];
        encode_desc(
            &mut out,
            &Desc::Record(vec![("name".to_owned(), Desc::Str)].into()),
        );

        for cut in 0..out.len() {
            assert!(decode_desc(&out[..cut]).is_err(), "cut to {cut}");
        }
    }

    proptest! {
        #[test]
        fn a_descriptor_round_trips(desc in arb_desc()) {
            let mut out = vec![];
            encode_desc(&mut out, &desc);
            prop_assert_eq!(decode_desc(&out), Ok((desc, out.len())));
        }
    }
}
