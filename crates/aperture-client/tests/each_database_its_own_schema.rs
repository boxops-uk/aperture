//! **One store root, two schemas** — [I13](../../../docs/invariants.md#i13) made real.
//!
//! Until 8.4 a server was handed one schema and served every database with it, which
//! was true enough while the schema was compiled into the binary. Once `create` takes a
//! schema file, "the schema" stops existing: a root holds artifacts built at different
//! times from different declarations, and each one has to be read back from the copy it
//! embedded.
//!
//! What is checked here is the consequence a person would notice: the same query is
//! answered by one database and refused by the other, because the predicate it names is
//! only in one of them — and the handshake fingerprints differ, so a client asserting
//! the wrong schema is told before it writes anything.

use std::{path::PathBuf, sync::Arc, thread};

use aperture_client::{ClientError, Connection, ErrorCode, Mode, WireFact, WireValue};
use aperture_schema::{fingerprint, schema::PredicateId, syntax};
use aperture_server::{Registry, registry::Schemas, server::Listener};
use aperture_store::catalog::Catalog;

/// A schema with one predicate a query can name.
const LOGS: &str = "schema log { predicate Line : string }";

/// A different schema, whose one predicate is spelled differently. Deliberately *not*
/// a superset: the point is that neither database can answer the other's question.
const NOTES: &str = "schema note { predicate Note : string }";

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
    catalog.create("logs", &schema(LOGS)).expect("a database");
    catalog.create("notes", &schema(NOTES)).expect("a database");

    // The server's own schema is neither of them — it is what a session bound to *no*
    // database sees, and a fallback for a database that embedded no copy.
    let (registry, listing) =
        Registry::open(catalog, Schemas::new("", schema(LOGS))).expect("a registry");

    assert!(listing.problems.is_empty(), "{:?}", listing.problems);
    assert_eq!(registry.len(), 2, "both are served");

    let listener = Listener::bind(&socket).expect("a socket");
    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    Serving { _dir: dir, socket }
}

fn connect(serving: &Serving, database: &str, source: &str, assert: bool) -> Connection {
    Connection::connect(
        &serving.socket,
        database,
        Arc::new(schema(source)),
        Mode::ReadWrite,
        assert,
    )
    .expect("a connection")
}

/// The whole claim in one test: each database answers through the schema **it** was
/// created against, and neither can be asked the other's question.
#[test]
fn a_query_is_compiled_against_the_database_it_is_asked_of() {
    let serving = start();

    let mut logs = connect(&serving, "logs", LOGS, true);
    logs.write(
        PredicateId(0),
        &[WireFact {
            predicate: PredicateId(0),
            key: WireValue::Str("a line".to_owned()),
            value: None,
        }],
    )
    .expect("it writes");

    let mut rows = logs.query("L where log.Line L").expect("a result");
    let page = logs.take(&mut rows, 10).expect("a page");
    assert_eq!(page.len(), 1, "the database that declares it answers");

    // The same text, asked of the database that does not declare `log.Line`. It is a
    // *compile* error naming the predicate, which is what says the query was compiled
    // against this database's schema rather than the server's.
    let mut notes = connect(&serving, "notes", NOTES, true);

    let refused = notes
        .query("L where log.Line L")
        .expect_err("no such predicate");
    match refused {
        ClientError::Server { code, ref message } => {
            assert_eq!(code, ErrorCode::BadQuery);
            assert!(
                message.contains("log.Line"),
                "the refusal should name it: {message}"
            );
        }
        other => panic!("expected the server to refuse, got {other:?}"),
    }

    // And its own question is answered.
    let mut rows = notes.query("N where note.Note N").expect("a result");
    assert!(notes.take(&mut rows, 10).expect("a page").is_empty());
}

/// **A copy that disagrees with the sidecar leaves the database unserved.**
///
/// The two are written together at create and neither can move afterwards, so a
/// disagreement means one of them was edited. Serving it anyway would read stored rows
/// through whichever of the two won, and report nothing; refusing makes it a problem in
/// the listing, which is where a person can see it.
#[test]
fn a_copy_that_disagrees_with_the_sidecar_leaves_the_database_unserved() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    let entry = catalog.create("logs", &schema(LOGS)).expect("a database");

    // A field added by hand: a plausible edit, and one that changes what every stored
    // row means.
    std::fs::write(
        entry.path.join("schema").join("schema.aps"),
        "schema log { predicate Line : { text : string, level : int } }",
    )
    .expect("it writes");

    let (registry, listing) =
        Registry::open(catalog, Schemas::new("", schema(LOGS))).expect("a registry");

    assert!(registry.find("logs").is_none(), "it must not be served");
    assert_eq!(listing.entries.len(), 1, "`list` still shows it (ops-I7)");
    assert_eq!(listing.problems.len(), 1, "and says what is wrong with it");
    assert!(
        listing.problems[0].to_string().contains("edited"),
        "{}",
        listing.problems[0]
    );
}

/// **A database from before the copy was load-bearing is still served**, with the
/// server's own schema and no fingerprint check.
///
/// Not laxity: such an artifact predates *both* halves of the comparison — it embedded
/// no source to read, and the number in its sidecar came from an algorithm that has
/// since been retired, so checking it would reject every database on the machine to no
/// purpose. It is served exactly as every database was before 8.4: with the schema the
/// server was started with.
#[test]
fn a_database_with_no_embedded_copy_is_served_with_the_servers_own() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    let entry = catalog.create("logs", &schema(LOGS)).expect("a database");

    std::fs::remove_dir_all(entry.path.join("schema")).expect("it goes");

    let (registry, listing) =
        Registry::open(catalog, Schemas::new("", schema(LOGS))).expect("a registry");

    assert!(listing.problems.is_empty(), "{:?}", listing.problems);

    let served = registry.find("logs").expect("it is served");
    assert_eq!(served.fingerprint, fingerprint::of(&schema(LOGS)));
}

/// A client asserting the wrong schema is refused at the handshake, before anything is
/// written — the cheap early mismatch the fingerprint exists for, now per database.
#[test]
fn the_handshake_compares_against_the_database_not_the_server() {
    let serving = start();

    assert_eq!(
        connect(&serving, "logs", LOGS, true)
            .hello()
            .schema_fingerprint,
        fingerprint::of(&schema(LOGS))
    );
    assert_eq!(
        connect(&serving, "notes", NOTES, true)
            .hello()
            .schema_fingerprint,
        fingerprint::of(&schema(NOTES))
    );

    // The server's own schema is `LOGS`, so asserting it against `notes` would have
    // passed before 8.4 — this is the case that says the comparison moved.
    let refused = Connection::connect(
        &serving.socket,
        "notes",
        Arc::new(schema(LOGS)),
        Mode::ReadWrite,
        true,
    )
    .expect_err("the schemas differ");

    assert_eq!(
        refused.code(),
        Some(ErrorCode::SchemaMismatch),
        "expected a schema mismatch, got {refused:?}"
    );
}
