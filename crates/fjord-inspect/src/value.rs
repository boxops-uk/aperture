//! A decoded value, as a page shows it.
//!
//! **One thing this does that the codec's own `Serialize` cannot**: a fact
//! reference reads as the fact it names. `Value`'s serialiser writes a
//! `FactRef` as the `u64` it is, which is right for a wire and unreadable in a
//! panel — `1099511627778` is a predicate tag and a sequence packed together,
//! and what a reader wants is `code.File#2`.
//!
//! Not a second codec: nothing here decodes bytes. It renders an *already
//! decoded* `Value`, which is a presentation choice and belongs on the
//! presentation side, in the same way the type renderer does.

use fjord_encoding::tuple::Value;
use fjord_schema::schema::Schema;

/// `value` as JSON, with every reference named.
#[must_use]
pub fn json(value: &Value, schema: &Schema) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Int(int) => serde_json::Value::from(*int),
        Value::Str(text) => serde_json::Value::from(text.clone()),

        // The whole reason this function exists.
        Value::FactRef(id) => serde_json::Value::from(fact(id, schema)),

        Value::Record(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), json(value, schema)))
                .collect(),
        ),

        // A union renders as the one-field object it is written as, which is
        // also how a query matches one.
        Value::Union { alt, value, .. } => {
            serde_json::Value::Object([(alt.clone(), json(value, schema))].into_iter().collect())
        }
    }
}

/// A fact's identity as a reader writes it: `code.File#2`.
#[must_use]
pub fn fact(id: &fjord_schema::id::FactId, schema: &Schema) -> String {
    let name = schema
        .get(id.predicate())
        .and_then(|predicate| predicate.name())
        .unwrap_or(crate::lowered::UNRESOLVED)
        .to_owned();

    format!("{name}#{}", id.sequence())
}
