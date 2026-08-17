//! The **embedded schema copy** — `schema/` beside the sidecar.
//!
//! [I13](../../../docs/invariants.md#i13): a database carries its own schema, embedded
//! at create and frozen for its lifetime. This is that copy, and since Phase 8.4 it is
//! **load-bearing** rather than belt-and-braces: it is what a server reads to learn what
//! a database holds, so a store root can hold databases built from different schemas
//! without anything having to be told which.
//!
//! # It is source, in the language `create --schema` takes
//!
//! Not JSON, which is what it was until 8.4, and not
//! [the canonical form](aperture_schema::fingerprint) either. The canonical form's job
//! is to be *hashed* — embedding it would need a second parser for a second grammar,
//! whose only reader would be this crate. Source needs no new reader at all, and it is
//! the one form a person can read, diff, and hand back to `create`.
//!
//! # Written in id order, and read back with [`syntax::recover`]
//!
//! A predicate's id is a *position*, and it is the tag in every
//! [`FactId`](aperture_schema::id::FactId) the database holds. Ordinary lowering assigns
//! ids by sorted name ([D1](../../../docs/phase-8-schemas.md)) — right for a schema
//! being declared, and wrong here, where the numbering is already frozen on disk. So the
//! copy is printed in id order and read back in declaration order, and `create` proves
//! the round trip before the database exists at all.

use std::{fs, path::Path};

use aperture_schema::{
    schema::Schema,
    syntax::{self, print},
};

/// The directory holding the copy, inside a database.
pub const SCHEMA_DIR: &str = "schema";

/// The document's file name.
pub const SCHEMA_FILE: &str = "schema.aps";

/// What is written above the schema, so the file says what it is.
const HEADER: &str = "\
# The schema this database was created against, embedded at create and frozen for its
# lifetime (I13). Written by `aperture create`; read back when the database is served.
#
# Predicates are listed in **id order**, which is the order their keyspaces are named
# in and the order every stored FactId's tag refers to. Editing this file does not
# change the database — it makes it unreadable.

";

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

    let mut text = String::from(HEADER);
    text.push_str(&print::print(schema));

    fs::write(&path, &text).map_err(|source| fail(format!("cannot write: {source}")))?;

    Ok(())
}

/// The source a database embedded, or `None` if it embedded none.
///
/// `None` is a real answer rather than a hole: a database created before 8.4 carries a
/// copy in the older format, and a server reading one is looking at an artifact that
/// predates the copy being load-bearing. What it must not do is *guess*, which is why
/// this says "there is none" instead of returning something empty.
///
/// # Errors
///
/// [`StoreError::Meta`](crate::error::StoreError::Meta) if the file is there and cannot
/// be read.
pub fn source(directory: &Path) -> Result<Option<String>, crate::error::StoreError> {
    let path = directory.join(SCHEMA_DIR).join(SCHEMA_FILE);

    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(crate::error::StoreError::Meta {
            path,
            detail: format!("cannot read: {source}"),
        }),
    }
}

/// The schema a database embedded, with the numbering it was created with.
///
/// # Errors
///
/// [`StoreError::Meta`](crate::error::StoreError::Meta) if the copy is unreadable or no
/// longer a schema — which is a corrupt artifact, not a bad query, and says so with the
/// diagnostics against the text it read.
pub fn read(directory: &Path) -> Result<Option<Schema>, crate::error::StoreError> {
    let Some(text) = source(directory)? else {
        return Ok(None);
    };

    syntax::recover(SCHEMA_FILE, &text)
        .map(Some)
        .map_err(|detail| crate::error::StoreError::Meta {
            path: directory.join(SCHEMA_DIR).join(SCHEMA_FILE),
            detail,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(source: &str) -> Schema {
        syntax::read("test", source).expect("it lowers")
    }

    /// The copy comes back as the schema that was written, at the same positions.
    #[test]
    fn a_copy_reads_back_as_the_schema_it_was_written_from() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let written = schema(
            "schema src { predicate File : string\n\
             predicate Decl : { file : File, name : string } -> string }",
        );

        write(dir.path(), &written).expect("it writes");

        let back = read(dir.path()).expect("it reads").expect("there is one");
        assert!(print::equivalent(&written, &back));
    }

    /// A database that embedded no copy says so, rather than answering with an empty
    /// schema — which would read as "this database holds nothing".
    #[test]
    fn a_database_with_no_copy_says_there_is_none() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        assert!(read(dir.path()).expect("it reads").is_none());
    }

    /// A copy somebody edited into nonsense is a corrupt artifact, and is refused with
    /// the reason rather than half-read.
    #[test]
    fn a_broken_copy_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        write(
            dir.path(),
            &schema("schema src { predicate File : string }"),
        )
        .expect("it writes");

        fs::write(
            dir.path().join(SCHEMA_DIR).join(SCHEMA_FILE),
            "schema src { predicate File : bananas }",
        )
        .expect("it writes");

        let Err(failed) = read(dir.path()) else {
            panic!("a schema this is not");
        };
        assert!(
            failed.to_string().contains("bananas"),
            "the reason should name what it could not read: {failed}"
        );
    }
}
