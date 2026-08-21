//! **The database, as a table** — every row, in the order a scan meets it,
//! as bytes *and* as a fact.
//!
//! The bytes are the point. A seek is a byte prefix and a scan is a range over
//! the same order, so `[lo, hi)` means nothing against decoded values and
//! everything against the stored keys — which is why this shows both, and why a
//! reader watching a scan can see the range as a band across rows rather than
//! as two hex strings.
//!
//! **Read through the seam**, exactly as the executor reads: `scan` over each
//! predicate's whole keyspace, then `point` for the value side. Nothing here
//! reaches into a store's internals, which is also what makes this table the
//! same rows the machine will walk, in the same order.

use fjord_encoding::tuple::decode_key;
use fjord_schema::schema::{LocalInterner, PredicateId};
use fjord_store::fact_store::FactStore;
use serde::Serialize;

use crate::value::json;

/// One stored row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowBytes {
    /// The fact's identity, as a reader writes it: `code.Decl#4`.
    pub fact: String,
    /// The whole stored key — the predicate id, then the key fields — in hex.
    ///
    /// **With the predicate prefix**, because that is what a scan bound is
    /// compared against: a range that started at the key fields would shade the
    /// wrong rows.
    pub key: String,
    /// The key, decoded against the predicate's declared type.
    pub decoded: serde_json::Value,
    /// The value side in hex, for a predicate that has one.
    pub value: Option<String>,
    /// The value side, decoded.
    pub value_decoded: Option<serde_json::Value>,
}

/// One predicate's rows, in key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredicateRows {
    pub id: u32,
    pub name: String,
    /// The declared type, as the schema language writes it.
    pub ty: String,
    pub rows: Vec<RowBytes>,
}

/// The whole database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Database {
    pub predicates: Vec<PredicateRows>,
    pub facts: usize,
}

/// Read the demo database out of a store built against `schema`.
#[must_use]
pub fn database(schema_source: &str) -> Database {
    let (schema, _) = crate::schema::compile(schema_source);
    let Some(schema) = schema else {
        return Database {
            predicates: Vec::new(),
            facts: 0,
        };
    };

    let Ok(store) = crate::demo::store(&schema) else {
        return Database {
            predicates: Vec::new(),
            facts: 0,
        };
    };

    let interner = LocalInterner::new(schema.interner().clone());
    let mut predicates = Vec::new();
    let mut facts = 0;

    for index in 0..schema.len() {
        let id = PredicateId(index as u32);
        let Some(declared) = schema.get(id) else {
            continue;
        };
        let Some(name) = declared.name() else {
            continue;
        };

        let key_ty = declared.key().ty.clone();
        let value_ty = declared.value().map(|value| value.ty.clone());

        // The predicate's whole keyspace: its id is the prefix of every one of
        // its keys, which is the same bound a query with no narrowing gets.
        let lo = id.0.to_be_bytes();
        let rows = store
            .scan(&lo, None)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let (key_bytes, fact_id) = row.ok()?;
                let entity = store.point(fact_id).ok().flatten();

                let decoded = entity
                    .as_ref()
                    .and_then(|entity| decode_key(&interner, &entity.key, &key_ty).ok())
                    .map_or(serde_json::Value::Null, |value| json(&value, &schema));

                let (value, value_decoded) = match (&value_ty, entity.as_ref()) {
                    (Some(ty), Some(entity)) if !entity.value.is_empty() => (
                        Some(hex(&entity.value)),
                        fjord_encoding::tuple::decode_typed(&interner, &entity.value, ty)
                            .ok()
                            .map(|value| json(&value, &schema)),
                    ),
                    _ => (None, None),
                };

                Some(RowBytes {
                    fact: crate::value::fact(&fact_id, &schema),
                    key: hex(&key_bytes),
                    decoded,
                    value,
                    value_decoded,
                })
            })
            .collect::<Vec<_>>();

        facts += rows.len();
        predicates.push(PredicateRows {
            id: id.0,
            name: name.to_owned(),
            ty: fjord_schema::syntax::print::signature(&schema, id).unwrap_or_default(),
            rows,
        });
    }

    Database { predicates, facts }
}

/// The same view, already JSON.
#[must_use]
pub fn database_json(schema_source: &str) -> String {
    serde_json::to_string(&database(schema_source)).expect("a database view serialises")
}

/// Bytes as a page shows them: lower-case hex, no separators.
///
/// No separators because the page compares them as strings — a scan bound is a
/// *prefix* of a key, and `"0000000104"` starts with `"00000001"` while
/// `"00 00 00 01 04"` does not start with `"00000001"`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}
