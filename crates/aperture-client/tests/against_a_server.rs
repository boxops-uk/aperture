//! The client, against a real server over a real socket.
//!
//! Not a mock, and the reason is the same one the .NET demo keeps proving: a client
//! tested against our idea of the server tests the idea. What is being checked here is
//! the conversation — that a page holds its place, that two results can be open at
//! once, and that a cancel ends one stream and leaves the connection working.

use std::{path::PathBuf, sync::Arc, thread};

use aperture_client::{ClientError, Connection, ErrorCode, Mode, WireFact, WireRef, WireValue};
use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_server::{Registry, server::Listener};
use aperture_store::catalog::Catalog;
use aperture_wire::provisional_fingerprint;
use lasso::Rodeo;

const FILE: PredicateId = PredicateId(0);
const DECL: PredicateId = PredicateId(1);

fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let (file, decl) = (
        rodeo.get_or_intern("src.File"),
        rodeo.get_or_intern("src.Decl"),
    );
    let (f_file, f_line, f_name) = (
        rodeo.get_or_intern("file"),
        rodeo.get_or_intern("line"),
        rodeo.get_or_intern("name"),
    );

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![
            Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: decl,
                key: PredicateTy::Record(
                    vec![
                        (f_file, PredicateTy::Fact(FILE)),
                        (f_line, PredicateTy::Int),
                        (f_name, PredicateTy::Str),
                    ]
                    .into(),
                ),
                value: None,
            },
        ]),
    )
}

fn file(path: &str) -> WireFact {
    WireFact {
        predicate: FILE,
        key: WireValue::Str(path.to_owned()),
        value: None,
    }
}

fn decl(path: &str, line: i64, name: &str) -> WireFact {
    WireFact {
        predicate: DECL,
        key: WireValue::Record(
            vec![
                // Nested: this client holds no ids at all, which is the point.
                WireValue::Ref(WireRef::Nested(Box::new(file(path)))),
                WireValue::Int(line),
                WireValue::Str(name.to_owned()),
            ]
            .into(),
        ),
        value: None,
    }
}

struct Serving {
    _dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Serving {
    fn open(&self, mode: Mode) -> Connection {
        Connection::connect(&self.socket, "code", Arc::new(schema()), mode, true)
            .expect("a connection")
    }

    fn control(&self) -> Connection {
        Connection::control(&self.socket, Arc::new(schema())).expect("a control session")
    }
}

fn start() -> Serving {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");

    let schema = schema();
    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog
        .create("code", &schema, provisional_fingerprint(&schema))
        .expect("a database");

    let (registry, _listing) = Registry::open(catalog, schema).expect("a registry");
    let listener = Listener::bind(&socket).expect("a socket");

    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    Serving { _dir: dir, socket }
}

/// Write `count` files, so a result can be made as long as a test needs.
fn seed(connection: &mut Connection, count: usize) {
    let facts: Vec<WireFact> = (0..count).map(|n| file(&format!("f{n:05}.py"))).collect();
    let written = connection.write(FILE, &facts).expect("they are written");
    assert_eq!(written.created, count as u64);
}

fn strings(rows: &[WireValue]) -> Vec<String> {
    rows.iter()
        .map(|row| match row {
            WireValue::Str(text) => text.clone(),
            other => panic!("expected a string row, got {other:?}"),
        })
        .collect()
}

/// Handshake, write facts holding no ids, read them back — one connection, and every
/// step through the client rather than around it.
#[test]
fn facts_written_by_this_client_are_queried_back_by_it() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    assert_eq!(connection.hello().version, 1);
    assert_eq!(connection.hello().predicates, 2);
    assert_eq!(
        connection.hello().schema_fingerprint,
        provisional_fingerprint(&schema()),
        "the handshake asserted our schema and the server agreed"
    );

    let written = connection
        .write(
            DECL,
            &[
                decl("store/keys.py", 12, "key_of"),
                decl("store/keys.py", 48, "key_prefix"),
                decl("store/codec.py", 7, "encode_key"),
            ],
        )
        .expect("the facts are written");

    // Three declarations and two files: `store/keys.py` is named twice and written
    // once. Interning, and the client never learned what anything was called.
    assert_eq!((written.created, written.deduped), (5, 1));
    assert_eq!(written.seen(), 6);

    let mut rows = connection.query("F where src.File F").expect("it compiles");
    assert_eq!(rows.desc(), &aperture_client::Desc::Str);

    let mut paths = strings(&connection.drain(&mut rows).expect("the rows arrive"));
    paths.sort();

    assert_eq!(paths, ["store/codec.py", "store/keys.py"]);
    assert!(rows.finished());
    assert_eq!(rows.sent(), 2);
}

/// **The page holds its place, and the pages concatenate.**
///
/// The property `\more` is built on, checked here before there is a shell to check it
/// in. The result is long enough to cross the server's chunk boundary several times —
/// so between pages the server is parked mid-result holding a bytes-only cursor, with
/// its snapshot already released ([I8](../../../docs/invariants.md#i8)) — and the
/// concatenation of the pages must equal an uninterrupted run of the same query, which
/// is [I4](../../../docs/invariants.md#i4) seen from a client.
#[test]
fn a_paged_read_equals_an_uninterrupted_one() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    // Well past the server's 256-row chunk, and not a multiple of it or of the page
    // size below: a paging bug that only shows up on an unaligned tail is exactly the
    // kind this is for.
    seed(&mut connection, 1000);

    let query = "F where src.File F";

    let mut whole = connection.query(query).expect("it compiles");
    let uninterrupted = strings(&connection.drain(&mut whole).expect("every row"));
    assert_eq!(uninterrupted.len(), 1000);

    let mut paged = connection.query(query).expect("it compiles");
    let mut pages = vec![];

    loop {
        let page = connection.take(&mut paged, 37).expect("a page");
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 37);
        pages.push(strings(&page));
    }

    // 1000 = 27 pages of 37 and one of 1, so the last page is short — which is the
    // case a `take` that read a fixed count would hang on.
    assert_eq!(pages.len(), 28);
    assert_eq!(pages.last().map(Vec::len), Some(1));

    let concatenated: Vec<String> = pages.concat();
    assert_eq!(
        concatenated, uninterrupted,
        "the pages are the uninterrupted run, in order and without repeats"
    );

    assert!(paged.finished());
    assert_eq!(paged.sent(), 1000);
}

/// **Two results open at once.** The second query is issued while the first is parked
/// mid-result, and neither loses a row — which is only true because frames for a
/// stream nobody is reading are *parked* rather than dropped.
#[test]
fn two_results_are_open_at_once() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    seed(&mut connection, 400);

    let mut first = connection.query("F where src.File F").expect("it compiles");
    let opening = strings(&connection.take(&mut first, 10).expect("a page"));
    assert_eq!(opening.len(), 10);

    // A second query, started while the first is still open and its rows are still
    // arriving on the socket.
    // A prefix constraint, which the level binding `F` applies as a seek rather than
    // as a filter — so this is a short answer by construction, not a long one filtered.
    let mut second = connection
        .query("F where src.File F; F = \"f00042\"..")
        .expect("it compiles");
    let narrow = strings(&connection.drain(&mut second).expect("its rows"));
    assert_eq!(narrow, ["f00042.py"]);

    // ...and the first carries on exactly where it stopped.
    let rest = strings(&connection.drain(&mut first).expect("the rest"));
    assert_eq!(rest.len(), 390);
    assert_eq!(first.sent(), 400);

    let mut all = opening;
    all.extend(rest);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 400, "no row was lost or duplicated");
}

/// A cancel is an **early end, not a failure**: the stream completes with what it
/// sent, the client is not owed an error, and the connection keeps answering.
#[test]
fn a_cancel_ends_one_result_and_leaves_the_connection_working() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    seed(&mut connection, 1000);

    let mut rows = connection.query("F where src.File F").expect("it compiles");
    let page = connection.take(&mut rows, 5).expect("a page");
    assert_eq!(page.len(), 5);

    let sent = connection.cancel(&mut rows).expect("it cancels");
    assert!(sent >= 5, "the server sent at least what we read: {sent}");
    assert!(rows.finished());

    // The connection is untouched: a stream ended, not a session.
    let mut again = connection
        .query("F where src.File F; F = \"f00007\"..")
        .expect("it compiles");
    assert_eq!(
        strings(&connection.drain(&mut again).expect("its rows")),
        ["f00007.py"]
    );

    // Cancelling a finished result is a no-op rather than a second cancel on a stream
    // the server has already closed.
    assert_eq!(connection.cancel(&mut rows).expect("a no-op"), sent);
}

/// A query that does not compile fails its **stream**, carrying the compiler's own
/// diagnostics, and the connection is usable afterwards.
#[test]
fn a_bad_query_fails_its_stream_by_code() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadOnly);

    let error = connection.query("this is not focus").expect_err("it fails");

    assert_eq!(error.code(), Some(ErrorCode::BadQuery));
    assert!(
        error.to_string().contains("invalid syntax"),
        "the compiler's own words: {error}"
    );

    let mut rows = connection.query("F where src.File F").expect("it compiles");
    assert!(connection.drain(&mut rows).expect("no rows").is_empty());
}

/// The lifecycle, through the client: create a database, seal it, and find that
/// `ops-I2` refuses a write session to it afterwards.
#[test]
fn the_lifecycle_runs_through_the_client() {
    let serving = start();
    let mut control = serving.control();

    let instance = control.create("fresh").expect("it is created");
    assert!(!instance.is_empty());

    // Immediately usable, without the server being restarted.
    let mut writer = Connection::connect(
        &serving.socket,
        "fresh",
        Arc::new(schema()),
        Mode::ReadWrite,
        true,
    )
    .expect("a session on the new database");

    seed(&mut writer, 3);
    drop(writer);

    let sealed = control.finish("fresh", false).expect("it seals");
    assert_eq!(sealed.facts, 3);
    assert!(sealed.fingerprint != 0);
    assert!(!sealed.already_complete);

    // `ops-I2`, from a client's side: the refusal is at establishment.
    let refused = Connection::connect(
        &serving.socket,
        "fresh",
        Arc::new(schema()),
        Mode::ReadWrite,
        true,
    )
    .expect_err("a sealed database takes no writer");

    assert_eq!(refused.code(), Some(ErrorCode::ModeRefused));

    // ...and reading it still works.
    let mut reader = Connection::connect(
        &serving.socket,
        "fresh",
        Arc::new(schema()),
        Mode::ReadOnly,
        true,
    )
    .expect("a reader");

    let mut rows = reader.query("F where src.File F").expect("it compiles");
    assert_eq!(reader.drain(&mut rows).expect("its rows").len(), 3);
    drop(rows);
    drop(reader);

    control.remove("fresh").expect("it is removed");

    let gone = Connection::connect(
        &serving.socket,
        "fresh",
        Arc::new(schema()),
        Mode::ReadOnly,
        false,
    )
    .expect_err("it is gone");

    assert_eq!(gone.code(), Some(ErrorCode::UnknownDatabase));
}

/// **A schema that disagrees is refused at the handshake**, before a byte of data
/// flows — which is the whole reason the fingerprint is sent as a claim rather than
/// asked for as a question.
#[test]
fn a_schema_that_disagrees_is_refused_before_any_data() {
    let serving = start();

    // One predicate short of the server's: a producer built against an older schema.
    let mut rodeo = Rodeo::new();
    let file = rodeo.get_or_intern("src.File");
    let stale = Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![Predicate {
            name: file,
            key: PredicateTy::Str,
            value: None,
        }]),
    );

    let refused = Connection::connect(
        &serving.socket,
        "code",
        Arc::new(stale),
        Mode::ReadWrite,
        true,
    )
    .expect_err("the fingerprints disagree");

    assert_eq!(refused.code(), Some(ErrorCode::SchemaMismatch));

    // ...and `false` is the reader's answer: nothing is claimed, so nothing is checked.
    let mut rodeo = Rodeo::new();
    let file = rodeo.get_or_intern("src.File");
    let stale = Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![Predicate {
            name: file,
            key: PredicateTy::Str,
            value: None,
        }]),
    );

    Connection::connect(
        &serving.socket,
        "code",
        Arc::new(stale),
        Mode::ReadOnly,
        false,
    )
    .expect("a reader that asserts nothing is let in");
}

/// A bookmark from another connection is refused rather than read from, which would
/// be a wait for a frame nobody is going to send.
#[test]
fn a_bookmark_belongs_to_its_connection() {
    let serving = start();
    let mut one = serving.open(Mode::ReadWrite);
    let mut two = serving.open(Mode::ReadOnly);

    seed(&mut one, 10);

    let mut rows = one.query("F where src.File F").expect("it compiles");
    assert_eq!(one.take(&mut rows, 2).expect("a page").len(), 2);

    let wrong = two.next_row(&mut rows).expect_err("not this connection's");
    assert!(matches!(wrong, ClientError::Protocol(_)), "{wrong}");

    // The bookmark still works where it belongs.
    assert_eq!(one.drain(&mut rows).expect("the rest").len(), 8);
}
