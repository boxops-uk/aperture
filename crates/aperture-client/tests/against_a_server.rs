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
const DOC: PredicateId = PredicateId(2);

fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let (file, decl, doc) = (
        rodeo.get_or_intern("src.File"),
        rodeo.get_or_intern("src.Decl"),
        rodeo.get_or_intern("src.Doc"),
    );
    let (f_file, f_line, f_name, f_decl) = (
        rodeo.get_or_intern("file"),
        rodeo.get_or_intern("line"),
        rodeo.get_or_intern("name"),
        rodeo.get_or_intern("decl"),
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
            // **A key of one field, and that field a reference.** The shape the built-in
            // schema uses for an attribute *of* something — a declaration has at most
            // one doc comment and at most one type, so the declaration alone is the
            // identity and the answer is the value. It encodes as the bare reference
            // does, which is exactly why it is worth a test of its own: nothing else
            // here would notice if a record of one started framing itself.
            Predicate {
                name: doc,
                key: PredicateTy::Record(vec![(f_decl, PredicateTy::Fact(DECL))].into()),
                value: Some(PredicateTy::Str),
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

/// A doc comment for a declaration, nested three deep: doc → declaration → file.
fn doc(path: &str, line: i64, name: &str, text: &str) -> WireFact {
    WireFact {
        predicate: DOC,
        key: WireValue::Record(
            vec![WireValue::Ref(WireRef::Nested(Box::new(decl(
                path, line, name,
            ))))]
            .into(),
        ),
        value: Some(WireValue::Str(text.to_owned())),
    }
}

struct Serving {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    /// The registry the server is running, kept so a test can read its counters.
    registry: Arc<Registry>,
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
    let registry = Arc::new(registry);
    let listener = Listener::bind(&socket).expect("a socket");

    let serving = Arc::clone(&registry);
    thread::spawn(move || {
        let _ = listener.run_blocking(serving);
    });

    Serving {
        _dir: dir,
        socket,
        registry,
    }
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
    assert_eq!(connection.hello().predicates, 3);
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

/// **A key of one field, holding a reference, behind a value.**
///
/// Three things at once, and each is a place a shape can be got wrong on its own: the
/// key is a record of one — which encodes as its single field and must not start
/// framing itself — the field is a reference nested two levels deep, so interning has
/// to reach the file through the declaration before the doc's key has any bytes, and
/// the fact has a value side that the query reads without matching on
/// ([I6](../../../docs/invariants.md#i6)).
///
/// It is written here rather than only in the encoder's golden because encoding a shape
/// correctly and *storing* one are different claims.
#[test]
fn a_key_of_one_field_holds_a_reference_and_a_value() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    let written = connection
        .write(
            DOC,
            &[
                doc(
                    "store/keys.py",
                    12,
                    "key_of",
                    "The key a row is filed under.",
                ),
                doc("store/codec.py", 7, "encode_key", "Order-preserving."),
            ],
        )
        .expect("the facts are written");

    // Two docs, two declarations, two files: nothing was here before, and every one of
    // the six was named by nesting rather than by an id.
    assert_eq!((written.created, written.deduped), (6, 0));

    // The value is read, the reference is followed, and neither is matched on.
    let mut rows = connection
        .query("{name = D.name, text = T.value} where T = src.Doc {decl = D}")
        .expect("it compiles");

    let mut answers: Vec<String> = connection
        .drain(&mut rows)
        .expect("the rows arrive")
        .iter()
        .map(|row| match row {
            WireValue::Record(fields) => match (&fields[0], &fields[1]) {
                (WireValue::Str(name), WireValue::Str(text)) => format!("{name}: {text}"),
                other => panic!("expected two strings, got {other:?}"),
            },
            other => panic!("expected a record row, got {other:?}"),
        })
        .collect();

    answers.sort();

    assert_eq!(
        answers,
        [
            "encode_key: Order-preserving.",
            "key_of: The key a row is filed under.",
        ]
    );
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

/// **A profile is the outcome to a plan's intent.** What makes it worth carrying is
/// the gap between examined and produced: a residual that rejects almost everything
/// is invisible in a row count and obvious here.
#[test]
fn a_profile_reports_what_the_query_examined() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    seed(&mut connection, 500);

    // A constant bind, which folds — so `src.File F` becomes an exact key seek.
    let mut scan = connection
        .query_profiled("F where src.File F; F = \"f00042.py\"")
        .expect("it compiles");

    let rows = connection.drain(&mut scan).expect("its rows");
    let profile = scan.profile().expect("a profile arrived");

    assert_eq!(rows.len(), 1);
    assert_eq!(profile.steps.len(), 1, "one level, one step");
    assert_eq!(profile.steps[0].label, "src.File");

    // A constant bind **folds**, so this is a seek rather than a scan with a filter —
    // and the number is how you can tell without reading the plan.
    assert_eq!(profile.examined(), 1, "the index answered it");
    assert!(!profile.steps[0].full_scan);

    // ...against a scan of the same predicate, which reads all five hundred.
    let mut whole = connection
        .query_profiled("F where src.File F")
        .expect("it compiles");
    let rows = connection.drain(&mut whole).expect("its rows");
    let profile = whole.profile().expect("a profile arrived");

    assert_eq!(rows.len(), 500);
    assert_eq!(profile.examined(), 500);
    assert!(profile.steps[0].full_scan, "it read the predicate whole");
}

/// A profile survives **chunking**: the result is long enough to cross the server's
/// 256-row boundary several times, so the tally has to accumulate across real resumes
/// rather than describing the last page.
#[test]
fn a_profile_accumulates_across_the_chunks_it_took() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    seed(&mut connection, 1000);

    let mut rows = connection
        .query_profiled("F where src.File F")
        .expect("it compiles");

    // Read in pages, so the server genuinely parks and resumes between them.
    let mut seen = 0;
    loop {
        let page = connection.take(&mut rows, 37).expect("a page");
        if page.is_empty() {
            break;
        }
        seen += page.len();
    }

    assert_eq!(seen, 1000);
    assert_eq!(
        rows.profile().expect("a profile arrived").examined(),
        1000,
        "the whole run's work, not the last page's, and not the replay's"
    );
}

/// A query that did not ask for a profile does not get one — which is what makes the
/// frame additive, and is the property the .NET client depends on without knowing it.
#[test]
fn an_unprofiled_query_gets_no_profile_frame() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    seed(&mut connection, 10);

    let mut rows = connection.query("F where src.File F").expect("it compiles");
    assert_eq!(connection.drain(&mut rows).expect("its rows").len(), 10);
    assert!(rows.profile().is_none());
}

/// **A connection that has answered a thousand queries holds no more than one that has
/// answered one.**
///
/// The regression guard for `bench/FINDINGS.md` §7. A stream's task used to wait forever
/// on a channel whose only `Sender` lived in a map with no removal path, so every query
/// left a parked task behind: ~3.5 kB retained per query, for the life of the connection.
/// A pooled connection is exactly the shape that reaches it, and a web tier is a pool by
/// construction.
///
/// Two claims, and they are different halves of the same fix. The server's is that the
/// task **ends** — `streams_live` is the gauge that was already counting them and that
/// nothing was allowed to decrement. The client's is that it stops **inventing** ids, or
/// the server's map grows with the query count even once every task in it is dead.
///
/// Polled rather than slept on: the task ends immediately once it may, so a fixed sleep
/// would be either flaky or slow.
#[test]
fn a_long_lived_connection_does_not_accumulate_streams() {
    let serving = start();

    let mut writer = serving.open(Mode::ReadWrite);
    seed(&mut writer, 4);
    drop(writer);

    let mut connection = serving.open(Mode::ReadOnly);

    const QUERIES: usize = 200;
    for _ in 0..QUERIES {
        let mut rows = connection.query("F where src.File F").expect("a query");
        let all = connection.drain(&mut rows).expect("the rows");
        assert_eq!(all.len(), 4, "the query is the same every time");
    }

    // The client's half: four concurrent streams were never open, so four ids were never
    // needed. One is enough, and the writer above used one of its own before it closed.
    assert!(
        connection.stream_ids_issued() <= 2,
        "the client invented {} stream ids for {QUERIES} sequential queries — it is not \
         recycling them",
        connection.stream_ids_issued()
    );

    // The server's half. The connection is still open, which is the whole point: this is
    // not "they go when you hang up", it is "they go when the work is done".
    let stats = Arc::clone(serving.registry.stats());
    let settled = within(std::time::Duration::from_secs(5), || {
        stats.streams_live() == 0
    });

    assert!(
        settled,
        "{} stream tasks are still live after {QUERIES} finished queries on an open \
         connection",
        stats.streams_live()
    );

    // The control: the gauge is capable of being non-zero, so a zero above is the tasks
    // ending rather than the counter never having counted.
    assert!(
        stats.queries_completed() >= QUERIES as u64,
        "the queries did not run"
    );

    // And the connection still works, which is what says the streams ended rather than
    // broke.
    let mut rows = connection.query("F where src.File F").expect("a query");
    assert_eq!(connection.drain(&mut rows).expect("the rows").len(), 4);
}

/// A **write** stream spans frames by definition, and must not be ended between them.
///
/// The rule that ends a finished stream is "`handle` returned and this is not a write in
/// progress". Getting that wrong in the other direction would end the stream at
/// `OPEN_WRITE` and lose every block after it — which no other test here would notice,
/// because they all write in one call.
#[test]
fn a_write_stream_survives_between_its_frames() {
    let serving = start();
    let mut connection = serving.open(Mode::ReadWrite);

    // Two blocks on one stream, so the second arrives after `copy_data` has already
    // returned once.
    let first: Vec<WireFact> = (0..3).map(|n| file(&format!("a{n}.py"))).collect();
    let second: Vec<WireFact> = (0..3).map(|n| file(&format!("b{n}.py"))).collect();

    let written = connection
        .write_blocks(&[(FILE, &first), (FILE, &second)])
        .expect("both blocks are written");

    assert_eq!(written.created, 6, "both blocks landed");

    let mut rows = connection.query("F where src.File F").expect("a query");
    assert_eq!(connection.drain(&mut rows).expect("the rows").len(), 6);
}

/// Wait for `f` to hold, or give up.
fn within(limit: std::time::Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    f()
}
