//! The **embedded schema copy** — `schema/` beside the sidecar.
//!
//! [Operations §9](../../../docs/aperture-cli-design.md) calls it "belt & suspenders
//! vs lost sidecar", and that is exactly its standing: the sidecar's
//! `schema_fingerprint` is authoritative, and this is a readable copy of what that
//! fingerprint is *of*. Nothing reads it to make a decision.
//!
//! It also makes a database self-describing, which is the point
//! [I13](../../../docs/invariants.md#i13) is aiming at — a client can read it to
//! learn the shape it must encode against, rather than having the schema written into
//! it by hand.
//!
//! # Provisional, and safe to be
//!
//! This is **not** the canonical form [chapter 6](../../../docs/06-types-and-schema.md)
//! specifies. That needs schema *syntax* to canonicalise, which arrives with
//! [Phase 8](../../../PLAN.md), and it is what the real fingerprint will be computed
//! over. Replacing this document later is not a migration, because nothing depends on
//! it: it is a derived artifact, rewritten by whoever creates the next database.

use std::{fs, path::Path};

use aperture_schema::schema::{PredicateId, PredicateTy, Schema};
use serde::{Deserialize, Serialize};

/// The directory holding the copy, inside a database.
pub const SCHEMA_DIR: &str = "schema";

/// The document's file name.
pub const SCHEMA_FILE: &str = "schema.json";

/// A type, with its field names resolved — a schema holds interned symbols, and a
/// reader of this file has no interner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TypeDoc {
    Int,
    Str,
    /// A reference. Both the id and the name are written: the id is what the wire
    /// carries, and the name is what a person reads.
    Fact {
        predicate: u32,
        name: String,
    },
    Record {
        fields: Vec<FieldDoc>,
    },
}

/// A record field.
///
/// The type is **nested rather than flattened**, which reads more verbosely and is
/// the only correct choice: `TypeDoc::Fact` carries a `name` of its own, so
/// flattening puts two `name` keys in one object — JSON that serialises happily and
/// then refuses to parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDoc {
    pub name: String,
    pub ty: TypeDoc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateDoc {
    /// A predicate's id **is** its position, so this is written out to make the
    /// document readable on its own rather than by counting.
    pub id: u32,
    pub name: String,
    pub key: TypeDoc,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<TypeDoc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaDoc {
    /// This document's own format version, so a reader knows what it is looking at.
    pub version: u32,
    /// Says plainly that this is not chapter 6's canonical form.
    pub provisional: bool,
    pub predicates: Vec<PredicateDoc>,
}

impl SchemaDoc {
    pub const VERSION: u32 = 1;

    /// Render `schema` as a document.
    #[must_use]
    pub fn of(schema: &Schema) -> SchemaDoc {
        let predicates = (0..schema.len())
            .filter_map(|index| {
                let id = PredicateId(index as u32);
                let predicate = schema.get(id)?;

                Some(PredicateDoc {
                    id: id.0,
                    name: predicate.name().unwrap_or("?").to_owned(),
                    key: TypeDoc::of(schema, predicate.key().ty),
                    value: predicate.value().map(|value| TypeDoc::of(schema, value.ty)),
                })
            })
            .collect();

        SchemaDoc {
            version: SchemaDoc::VERSION,
            provisional: true,
            predicates,
        }
    }
}

impl TypeDoc {
    #[must_use]
    pub fn of(schema: &Schema, ty: &PredicateTy) -> TypeDoc {
        match ty {
            PredicateTy::Int => TypeDoc::Int,
            PredicateTy::Str => TypeDoc::Str,
            PredicateTy::Fact(target) => TypeDoc::Fact {
                predicate: target.0,
                name: schema
                    .get(*target)
                    .and_then(|predicate| predicate.name())
                    .unwrap_or("?")
                    .to_owned(),
            },
            PredicateTy::Record(fields) => TypeDoc::Record {
                fields: fields
                    .iter()
                    .map(|(name, field)| FieldDoc {
                        name: schema.interner().resolve(*name).unwrap_or("?").to_owned(),
                        ty: TypeDoc::of(schema, field),
                    })
                    .collect(),
            },
        }
    }
}

/// Write the copy into `directory/schema/`.
///
/// # Errors
///
/// [`StoreError::Meta`](crate::error::StoreError::Meta) if it cannot be written.
pub fn write(directory: &Path, schema: &Schema) -> Result<(), crate::error::StoreError> {
    let dir = directory.join(SCHEMA_DIR);
    let path = dir.join(SCHEMA_FILE);

    let fail = |detail: String| crate::error::StoreError::Meta {
        path: path.clone(),
        detail,
    };

    fs::create_dir_all(&dir).map_err(|source| fail(format!("cannot create: {source}")))?;

    let mut json = serde_json::to_string_pretty(&SchemaDoc::of(schema))
        .map_err(|source| fail(format!("cannot serialise: {source}")))?;
    json.push('\n');

    fs::write(&path, json).map_err(|source| fail(format!("cannot write: {source}")))?;

    Ok(())
}

/// Read the copy back.
///
/// # Errors
///
/// [`StoreError::Meta`](crate::error::StoreError::Meta) if it is missing or malformed.
pub fn read(directory: &Path) -> Result<SchemaDoc, crate::error::StoreError> {
    let path = directory.join(SCHEMA_DIR).join(SCHEMA_FILE);

    let fail = |detail: String| crate::error::StoreError::Meta {
        path: path.clone(),
        detail,
    };

    let text =
        fs::read_to_string(&path).map_err(|source| fail(format!("cannot read: {source}")))?;

    serde_json::from_str(&text).map_err(|source| fail(format!("malformed: {source}")))
}
