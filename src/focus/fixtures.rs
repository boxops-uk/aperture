//! Hand-built fixtures shared by the executor batteries — key-field encoders, an
//! interner builder, and a plan runner.
//!
//! Test machinery, not a product backend. Lives in a support module so tests
//! import these rather than redefining helpers inline (see `docs/testing.md`).

use lasso::Rodeo;
use tokio_util::sync::CancellationToken;

use crate::focus::{
    error::ApertureError,
    iter::{Executor, Iteratee, Stream},
    plan::{FactStore, Plan},
    schema::{LocalInterner, SchemaInterner},
    tuple::{Value, put_i64, put_str},
};

/// Encode a single i64 key field.
pub fn i64_field(v: i64) -> Vec<u8> {
    let mut b = Vec::new();
    put_i64(&mut b, v);
    b
}

/// Encode a single string key field.
pub fn str_field(s: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, s);
    b
}

/// Concatenate encoded fields into one composite key. The tuple codec is
/// self-delimiting ([I2]), so a multi-field key is just its fields back-to-back.
///
/// [I2]: ../../../docs/invariants.md
pub fn compose(fields: &[&[u8]]) -> Vec<u8> {
    fields.concat()
}

/// A `LocalInterner` whose schema tier holds `names`, so `Project::Record` field
/// symbols (looked up with `LocalInterner::get`) resolve during projection.
pub fn interner_with(names: &[&str]) -> LocalInterner {
    let mut rodeo = Rodeo::new();
    for name in names {
        rodeo.get_or_intern(*name);
    }
    LocalInterner::new(SchemaInterner::new(rodeo.into_reader()))
}

/// Run `plan` to completion against `store`, collecting every projected row.
///
/// This is the "run to completion, collect rows" reference model the resume
/// battery checks suspend/resume against ([I4]).
///
/// [I4]: ../../../docs/invariants.md
pub fn collect_rows<S: FactStore>(
    store: S,
    plan: Plan,
    interner: &LocalInterner,
) -> Result<Vec<Value>, ApertureError> {
    let cancel = CancellationToken::new();
    let mut ex = Executor::new(store, plan);

    let out = ex.enumerate(
        Vec::new(),
        |mut acc, row| {
            acc.push(row.to_value(interner)?);
            Ok(Stream::Continue(acc))
        },
        &cancel,
    )?;

    Ok(match out {
        Iteratee::Done(rows) | Iteratee::Suspended(rows, _) => rows,
    })
}
