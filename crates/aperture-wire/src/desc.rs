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
//! [`PredicateTy`](aperture_schema::schema::PredicateTy) and rows are then encoded
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

use aperture_schema::schema::{LocalInterner, PredicateId, PredicateTy, Schema, Symbol};

use crate::{error::WireError, varint};

const TAG_INT: u64 = 0;
const TAG_STR: u64 = 1;
const TAG_FACT: u64 = 2;
const TAG_RECORD: u64 = 3;

/// A row's shape, with names a peer can read.
///
/// The same four cases as [`PredicateTy`], which is not a coincidence — a well-typed
/// head resolves to one of them — but with field names as `String` rather than as
/// interned symbols, because the interner is ours and not the peer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desc {
    Int,
    Str,
    Fact(PredicateId),
    /// Fields in order. A row's values follow this order and carry no names.
    Record(Box<[(String, Desc)]>),
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
        })
    }

    /// Back to a schema type, interning the field names.
    ///
    /// What a client does with a descriptor it received: rows are then decoded by the
    /// ordinary value codec, so there is one decoder rather than a second one for
    /// rows.
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
            let (count, used) = varint::get_u64(&bytes[at..])?;
            at += used;

            // A count sizes an allocation, and it came from a peer.
            let count = usize::try_from(count).map_err(|_| WireError::LengthOutOfRange {
                declared: count,
                available: bytes.len(),
            })?;
            if count > bytes.len() {
                return Err(WireError::LengthOutOfRange {
                    declared: count as u64,
                    available: bytes.len(),
                });
            }

            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                let (len, used) = varint::get_u64(&bytes[at..])?;
                at += used;

                let len = usize::try_from(len).map_err(|_| WireError::LengthOutOfRange {
                    declared: len,
                    available: bytes.len() - at,
                })?;
                if at + len > bytes.len() {
                    return Err(WireError::LengthOutOfRange {
                        declared: len as u64,
                        available: bytes.len() - at,
                    });
                }

                let name = std::str::from_utf8(&bytes[at..at + len])
                    .map_err(|_| WireError::BadString)?
                    .to_owned();
                at += len;

                let (field, used) = decode_desc(&bytes[at..])?;
                at += used;

                fields.push((name, field));
            }

            Desc::Record(fields.into())
        }
        other => return Err(WireError::UnknownRefForm(other)),
    };

    Ok((desc, at))
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
            ::proptest::collection::vec(
                (
                    ::proptest::sample::select(vec!["a", "name", "line", "of", ""]),
                    inner,
                ),
                0..4,
            )
            .prop_map(|fields| {
                Desc::Record(
                    fields
                        .into_iter()
                        .map(|(n, d)| (n.to_owned(), d))
                        .collect::<Vec<_>>()
                        .into(),
                )
            })
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
