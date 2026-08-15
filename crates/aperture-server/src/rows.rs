//! Turning a query result into rows on the wire.
//!
//! Three small conversions, and the point of splitting them that way is that **no new
//! encoder appears here**. A row goes out through exactly the codec a fact's key
//! comes in through:
//!
//! ```text
//!   Ty            the head's inferred type      (engine)
//!    │  desc_of
//!    ▼
//!   Desc          names resolved, sent once     (wire)
//!    │  to_ty
//!    ▼
//!   PredicateTy   what the value codec drives on
//!    │  to_wire   + the row's stored Value
//!    ▼
//!   WireValue  ──encode_value──▶  bytes
//! ```
//!
//! The temptation is to write a fourth encoder straight from `Desc` and a stored
//! `Value`, which is about twenty-five lines and would be a second definition of the
//! wire format. Going the long way round keeps one.

use aperture_encoding::tuple::Value;
use aperture_engine::syntax::Ty;
use aperture_schema::schema::{LocalInterner, PredicateTy, Schema};
use aperture_wire::{Desc, WireRef, WireValue};

use crate::error::ServerError;

/// The head's type as a descriptor, with its record field names resolved.
///
/// # Errors
///
/// [`ServerError::Unprojectable`] for a head whose type is still a variable or an
/// error. Typecheck rejects both before a plan exists, so reaching one here means the
/// front end let something through — reported rather than guessed at.
pub fn desc_of(ty: &Ty, interner: &LocalInterner) -> Result<Desc, ServerError> {
    Ok(match ty {
        Ty::Int => Desc::Int,
        Ty::String => Desc::Str,
        Ty::Fact(id) => Desc::Fact(*id),
        Ty::Record(fields) => Desc::Record(
            fields
                .iter()
                .map(|(name, field)| {
                    let name = interner
                        .try_resolve(*name)
                        .ok_or(ServerError::Unprojectable(
                            "a head field whose name this query's interner cannot resolve",
                        ))?
                        .to_owned();
                    Ok((name, desc_of(field, interner)?))
                })
                .collect::<Result<Vec<_>, ServerError>>()?
                .into(),
        ),
        Ty::Var(_) => {
            return Err(ServerError::Unprojectable(
                "a head whose type is still undetermined",
            ));
        }
        Ty::Error => {
            return Err(ServerError::Unprojectable("a head whose type is an error"));
        }
    })
}

/// A stored row value as a wire value, against the type the descriptor named.
///
/// Record fields are matched **by name** rather than by position. Both sides come
/// from the same head so the orders ought to agree, and relying on that would make a
/// silent mis-projection out of a change to either — the row would encode fine and
/// decode as the wrong fields.
///
/// # Errors
///
/// [`ServerError::Unprojectable`] if the row does not fit the type its own head
/// produced, which is a bug rather than a bad query.
pub fn to_wire(
    ty: &PredicateTy,
    value: &Value,
    interner: &LocalInterner,
) -> Result<WireValue, ServerError> {
    Ok(match (ty, value) {
        (PredicateTy::Int, Value::Int(n)) => WireValue::Int(*n),
        (PredicateTy::Str, Value::Str(s)) => WireValue::Str(s.clone()),

        // Outbound, a reference is always an id: the row was read from storage, where
        // a reference already is one. The union branch is still written, because the
        // client decodes rows with the same value decoder it encodes facts with.
        (PredicateTy::Fact(_), Value::FactRef(id)) => WireValue::Ref(WireRef::Id(*id)),

        (PredicateTy::Record(field_tys), Value::Record(fields)) => {
            let mut out = Vec::with_capacity(field_tys.len());

            for (name, field_ty) in field_tys.iter() {
                let name = interner
                    .try_resolve(aperture_schema::schema::Symbol::Local(*name))
                    .or_else(|| {
                        interner.try_resolve(aperture_schema::schema::Symbol::Schema(*name))
                    })
                    .ok_or(ServerError::Unprojectable("an unresolvable field name"))?;

                let field = fields
                    .iter()
                    .find(|(field_name, _)| field_name == name)
                    .map(|(_, value)| value)
                    .ok_or(ServerError::Unprojectable(
                        "a row missing a field its own head declared",
                    ))?;

                out.push(to_wire(field_ty, field, interner)?);
            }

            WireValue::Record(out.into())
        }

        _ => {
            return Err(ServerError::Unprojectable(
                "a row that does not fit the type its head produced",
            ));
        }
    })
}

/// The descriptor, and the type rows will be encoded against.
///
/// The interner comes back with it because [`Desc::to_ty`] interns the field names
/// into it, and [`to_wire`] has to resolve the very same symbols.
///
/// # Errors
///
/// Whatever [`desc_of`] reports.
pub fn row_shape(
    schema: &Schema,
    ty: &Ty,
    interner: &LocalInterner,
) -> Result<(Desc, PredicateTy, LocalInterner), ServerError> {
    let desc = desc_of(ty, interner)?;
    let mut fresh = LocalInterner::new(schema.interner().clone());
    let ty = desc.to_ty(&mut fresh);
    Ok((desc, ty, fresh))
}
