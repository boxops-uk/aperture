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
//!    │  to_ty     interned back into the *compilation's* interner
//!    ▼
//!   PredicateTy   what the value codec drives on
//!    │  to_wire   + the row's stored Value
//!    ▼
//!   WireValue  ──encode_value──▶  bytes
//! ```
//!
//! **One interner runs the whole chain**, and it has to be the compilation's: a
//! `Plan`'s projections hold symbols minted there, so a row decodes against it, and
//! the row type is interned back into it so the match below is between names that came
//! from one place. A second interner built from the same schema agrees about schema
//! names and disagrees about every head field name.
//!
//! The temptation is to write a fourth encoder straight from `Desc` and a stored
//! `Value`, which is about twenty-five lines and would be a second definition of the
//! wire format. Going the long way round keeps one.

use aperture_encoding::tuple::{Value, decode_key};
use aperture_engine::syntax::Ty;
use aperture_schema::{
    id::FactId,
    schema::{LocalInterner, PredicateTy, Schema},
};
use aperture_store::fact_store::FactStore;

use aperture_wire::{Desc, WireRef, WireValue, protocol::Found};

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
/// # Record fields are matched positionally, and by name would be *wrong*
///
/// The first version matched by name, on the reasoning that relying on order would
/// make a silent mis-projection out of a change to either side. It could not work,
/// and the reason is worth keeping: a `PredicateTy::Record` holds a bare `Spur`, so
/// [`Desc::to_ty`](aperture_wire::Desc::to_ty) has to discard which **tier** of the
/// two-tier interner a name came from. Resolving one afterwards is a guess, and a
/// wrong guess does not fail — it resolves to a *different string*, because a local
/// `Spur` and a schema `Spur` of the same number are different names.
///
/// A query head can also name fields the schema never declares (`{decl = …}`), so
/// there is no schema symbol to hold in the first place.
///
/// Positional is not a weaker check here, it is the only correct one: both the
/// descriptor and the row come from the *same* head type, walked in the same order.
/// And the names are not lost — they are in the [`Desc`], which is what the client
/// receives. Nothing downstream reads a record name from the `PredicateTy`, including
/// `encode_value`, which zips fields positionally too.
///
/// # Errors
///
/// [`ServerError::Unprojectable`] if the row does not fit the type its own head
/// produced, which is a bug rather than a bad query.
pub fn to_wire(ty: &PredicateTy, value: &Value) -> Result<WireValue, ServerError> {
    Ok(match (ty, value) {
        (PredicateTy::Int, Value::Int(n)) => WireValue::Int(*n),
        (PredicateTy::Str, Value::Str(s)) => WireValue::Str(s.clone()),

        // Outbound, a reference is always an id: the row was read from storage, where
        // a reference already is one. The union branch is still written, because the
        // client decodes rows with the same value decoder it encodes facts with.
        (PredicateTy::Fact(_), Value::FactRef(id)) => WireValue::Ref(WireRef::Id(*id)),

        (PredicateTy::Record(field_tys), Value::Record(fields)) => {
            if field_tys.len() != fields.len() {
                return Err(ServerError::Unprojectable(
                    "a row with a different number of fields than its head declared",
                ));
            }

            let mut out = Vec::with_capacity(field_tys.len());

            for ((_, field_ty), (_, field)) in field_tys.iter().zip(fields.iter()) {
                out.push(to_wire(field_ty, field)?);
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

/// **The fact an id names**, as its key — or which kind of nothing was there.
///
/// What answers [`kinds::FETCH`](aperture_wire::kinds::FETCH), and the same chain the
/// rest of this module is, one step longer at the front: a fetch starts from *stored
/// bytes* rather than from a row the executor already decoded.
///
/// ```text
///   FactId ──point──▶ Entity.key ──decode_key──▶ Value ──to_wire──▶ WireValue
/// ```
///
/// [`decode_key`] rather than `decode_typed`, and the distinction is the layout: a
/// stored key is its fields back to back with no record wrapper, so handing a
/// record-keyed predicate's key to the field decoder looks for a `MARK_RECORD` that was
/// never written.
///
/// # It reads the store as it is now, and for a stored fact that is not a race
///
/// A query holds an immutable snapshot and releases it at every suspend
/// ([I8](../../../docs/invariants.md#i8)), so an id resolved after the fact is read
/// under a *later* view of the store than the row that carried it. Nothing follows from
/// that for a stored fact: one is immutable once written and an id is never reused
/// ([I11](../../../docs/invariants.md#i11)), so an id that was in a row is in every
/// later state, naming the same fact. A reader that took its own snapshot per batch would
/// be buying consistency that immutability already gave it.
///
/// **A virtual predicate is where that argument stops**, which is what
/// [`Found::Unstored`] exists to say. Its rows are materialised per query, so an id is a
/// position in the listing that produced it: a database created or removed in between can
/// move it or take it away. Ordinary, and not corruption — the distinction the caller
/// needs, and one only this side can draw.
///
/// # Errors
///
/// [`ServerError::Unprojectable`] for an id naming a predicate this schema does not
/// declare, [`ServerError::Store`] for a read that fails, or
/// [`ServerError::Execution`] for stored bytes that do not decode as the key their own
/// predicate declares, which is corruption rather than a bad request.
pub fn key_of<S: FactStore>(
    store: &S,
    schema: &Schema,
    interner: &LocalInterner,
    id: FactId,
) -> Result<Found, ServerError> {
    let predicate = id.predicate();

    let declared = schema
        .get(predicate)
        .ok_or(ServerError::Unprojectable(
            "an id naming a predicate this schema does not declare",
        ))?
        .predicate();

    // **A virtual predicate is not a special case here, and asking whether it is one
    // would be the mistake.** `Catalogued` answers both halves of the seam for the
    // catalogue's keyspace — a scan from its rows, and `point` by finding the id among
    // them — so a reference into one resolves through exactly this call. What decides the
    // answer is which store the caller wrapped, which is the caller's business; refusing
    // by schema flag here made an ordinary query (`X where X = aperture.db.List _`) fail
    // for a reason that was not true.
    let Some(entity) = store.point(id)? else {
        // **Which kind of absence**, since they mean opposite things and only this side
        // knows which. A stored fact cannot dangle (I11, I12), so a missing one is
        // corruption and the client should say so; a virtual predicate's rows are a view
        // materialised per query, so one that has gone means a database was created or
        // removed since — ordinary, and not something to alarm anybody about.
        return Ok(if schema.is_virtual(predicate) {
            Found::Unstored
        } else {
            Found::Missing
        });
    };

    let key = decode_key(interner, &entity.key, &declared.key)
        .map_err(|error| ServerError::Execution(error.to_string()))?;

    Ok(Found::Key(to_wire(&declared.key, &key)?))
}
