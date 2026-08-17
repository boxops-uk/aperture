//! **[I13](../../../docs/invariants.md#i13)** — the DB's schema is embedded and frozen
//! at create, and every ingest is validated against it by **subset containment**.
//!
//! The guard was written in Phase 0 and has been `#[ignore]`d ever since, in
//! `aperture-schema`, where it could never have run: validating an ingest needs a
//! database to validate it against, a schema that was parsed rather than built, and a
//! write path. All three exist now, and none of them is in that crate — so the guard
//! lives here, over the real client and the real server, and
//! [`docs/invariants.md`](../../../docs/invariants.md) points at it by this name.
//!
//! # What containment means on the way in
//!
//! Chapter 6 settles compatibility as `old ⊆ new` over `name → predicate fingerprint`.
//! A producer is the same question asked from the other end: **everything it claims
//! must be in the database, identically**. So —
//!
//! - a producer declaring *fewer* predicates is accepted, which is the case that makes
//!   the rule worth having: an indexer that writes six of twenty-seven predicates
//!   should not have to restate the twenty-one it never touches;
//! - a *renamed* predicate is refused, because the database does not hold it;
//! - a *changed key type* and a *dropped field* are refused, because the database holds
//!   that name and not that shape — and a fact encoded against the producer's idea of it
//!   would decode as something else rather than fail.
//!
//! Each of those is a fact file's producing schema in [chapter 6](../../../docs/06-types-and-schema.md)'s
//! wording; over the write stream, the producer *is* the file's header, so the check
//! lands at the handshake and no bytes flow before it.

use std::{path::PathBuf, sync::Arc, thread};

use aperture_client::{Connection, ErrorCode, Mode, WireFact, WireValue};
use aperture_schema::syntax;
use aperture_server::{Registry, registry::Schemas, server::Listener};
use aperture_store::catalog::Catalog;

/// The database's schema: three predicates, one of them a record with two fields.
const EMBEDDED: &str = "\
schema src {
  predicate File : string
  predicate Decl : { file : File, name : string }
  predicate Doc : { decl : Decl } -> string
}";

struct Serving {
    _dir: tempfile::TempDir,
    socket: PathBuf,
}

fn schema(source: &str) -> aperture_schema::schema::Schema {
    syntax::read("test", source).expect("it lowers")
}

fn start() -> Serving {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");

    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog
        .create("code", &schema(EMBEDDED))
        .expect("a database");

    let (registry, listing) =
        Registry::open(catalog, Schemas::new("", schema(EMBEDDED))).expect("a registry");
    assert!(listing.problems.is_empty(), "{:?}", listing.problems);

    let listener = Listener::bind(&socket).expect("a socket");
    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    Serving { _dir: dir, socket }
}

/// Open a write session claiming `source` as the schema being written against.
fn producing(serving: &Serving, source: &str) -> Result<Connection, aperture_client::ClientError> {
    Connection::connect(
        &serving.socket,
        "code",
        Arc::new(schema(source)),
        Mode::ReadWrite,
        true,
    )
}

/// **The guard.** Each incompatible producing schema is refused; the compatible subset
/// is accepted, and can write.
#[test]
fn ingest_rejects_incompatible_schema() {
    let serving = start();

    // A **compatible subset**: two of the three predicates, both exactly as the
    // database declares them. Accepted, and it writes — which is the half of the
    // criterion that stops the other half from passing vacuously.
    const SUBSET: &str = "schema src { predicate File : string\n\
                          predicate Decl : { file : File, name : string } }";

    let mut producer = producing(&serving, SUBSET).expect("a producer of a subset is let in");

    // **The producer's own ids, which are not the database's.** Two predicates sort to
    // 0 and 1 here and to 0 and 1 there for different reasons, and it does not matter:
    // a block header carries the predicate's *name*. Asking by name is what a client
    // does, and writing a constant is what would silently write to the wrong one.
    let (decl, _) = schema(SUBSET).find_position("src.Decl").expect("declared");
    let (file, _) = schema(SUBSET).find_position("src.File").expect("declared");

    let written = producer
        .write(
            decl,
            &[WireFact {
                predicate: decl,
                key: WireValue::Record(
                    vec![
                        WireValue::Ref(aperture_client::WireRef::Nested(Box::new(WireFact {
                            predicate: file,
                            key: WireValue::Str("a.py".to_owned()),
                            value: None,
                        }))),
                        WireValue::Str("main".to_owned()),
                    ]
                    .into(),
                ),
                value: None,
            }],
        )
        .expect("the facts are written");

    // Two, not one: the nested `src.File` is interned on the way in, which is what a
    // producer holding no ids means (chapter 3).
    assert_eq!(written.created, 2);

    // A **renamed** predicate: the database holds nothing called `src.Declaration`.
    let renamed = producing(
        &serving,
        "schema src { predicate File : string\n\
         predicate Declaration : { file : File, name : string } }",
    )
    .expect_err("nothing here is called that");

    assert_eq!(renamed.code(), Some(ErrorCode::SchemaMismatch));
    assert!(
        renamed.to_string().contains("src.Declaration"),
        "refused by name: {renamed}"
    );

    // A **changed key type**: same name, and `name` is an int here. Every fact this
    // producer wrote would decode as something else rather than fail, which is why
    // this has to be caught before any of them arrive.
    let retyped = producing(
        &serving,
        "schema src { predicate File : string\n\
         predicate Decl : { file : File, name : int } }",
    )
    .expect_err("a string field is not an int field");

    assert_eq!(retyped.code(), Some(ErrorCode::SchemaMismatch));
    assert!(
        retyped.to_string().contains("src.Decl"),
        "refused by name: {retyped}"
    );

    // A **dropped field**: `Decl` without `name`. A shorter key encodes happily and
    // seeks somewhere else entirely.
    let dropped = producing(
        &serving,
        "schema src { predicate File : string\n\
         predicate Decl : { file : File } }",
    )
    .expect_err("a two-field key is not a one-field key");

    assert_eq!(dropped.code(), Some(ErrorCode::SchemaMismatch));

    // A **reordered** key, which is the one that looks harmless: the same fields, the
    // same types, and a different physical key order — so it decides a different index
    // and reads different bytes. Field order is inside the fingerprint precisely here.
    let reordered = producing(
        &serving,
        "schema src { predicate File : string\n\
         predicate Decl : { name : string, file : File } }",
    )
    .expect_err("field order is part of what a predicate is");

    assert_eq!(reordered.code(), Some(ErrorCode::SchemaMismatch));

    // And a producer that claims **the whole schema**, unchanged, is still the ordinary
    // case: equality answers first, and containment never runs.
    producing(&serving, EMBEDDED).expect("the whole schema, agreed exactly");
}

/// The schema a database validates against is the one it **embedded**, not the one the
/// server was started with — the other half of I13, and the half that only became
/// checkable when a store root could hold two schemas.
#[test]
fn the_schema_validated_against_is_the_embedded_one() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");

    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog
        .create("code", &schema(EMBEDDED))
        .expect("a database");

    // The server's own schema declares something else entirely. If a handshake checked
    // *that*, every claim below would come out the wrong way round.
    let server_schema = "schema other { predicate Thing : int }";
    let (registry, _listing) =
        Registry::open(catalog, Schemas::new("", schema(server_schema))).expect("a registry");

    let listener = Listener::bind(&socket).expect("a socket");
    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    let serving = Serving { _dir: dir, socket };

    producing(&serving, "schema src { predicate File : string }")
        .expect("checked against what the database embedded");

    let refused =
        producing(&serving, server_schema).expect_err("that is the server's, not this database's");
    assert_eq!(refused.code(), Some(ErrorCode::SchemaMismatch));
}
