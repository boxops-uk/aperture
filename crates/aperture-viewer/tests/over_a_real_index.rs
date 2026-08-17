//! **The viewer against a real server, over a real socket.**
//!
//! Not a mock, for the reason every other integration test here is not one: what is
//! being checked is that the *queries* answer, and a mock would only check that the
//! rendering compiles. Each one of them is a shape
//! [phase 11](../../../docs/phase-11-code-search.md) argued about — the file view in
//! particular, which was a scan of the largest predicate in the index until
//! `src.FileXRef` existed.
//!
//! The corpus is written through the ordinary write stream, nested exactly as an
//! indexer's facts are, so the ids the viewer resolves are ids the server allocated.

use std::{net::SocketAddr, sync::Arc, thread, time::Duration};

use aperture_client::{Connection, Mode, WireFact, WireRef, WireValue};
use aperture_schema::schema::{PredicateId, Schema};
use aperture_server::{Registry, registry::Schemas, server::Listener};
use aperture_store::catalog::Catalog;

/// The built-in schema, parsed from the file the server reads.
fn schema() -> Schema {
    const SOURCE: &str = include_str!("../../../schemas/code.aps");

    let mut diagnostics = vec![];
    let cst = aperture_schema::syntax::parse::parse(SOURCE, &mut diagnostics).expect("it parses");

    aperture_schema::syntax::lower::lower(&cst, &mut diagnostics)
        .expect("it lowers")
        .schema
}

fn id(schema: &Schema, name: &str) -> PredicateId {
    schema
        .find_position(name)
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("no `{name}` in the schema"))
}

// ---- the corpus, as an indexer would send it -------------------------------

const FILE_A: &str = "src/a.cs";
const FILE_B: &str = "src/b.cs";

fn file(schema: &Schema, path: &str) -> WireFact {
    WireFact {
        predicate: id(schema, "src.File"),
        key: WireValue::Str(path.to_owned()),
        value: None,
    }
}

fn module(schema: &Schema, path: &str, name: &str) -> WireFact {
    WireFact {
        predicate: id(schema, "src.Module"),
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(file(schema, path)))),
            WireValue::Str(name.to_owned()),
        ])),
        value: None,
    }
}

fn decl(schema: &Schema, path: &str, name: &str, line: i64) -> WireFact {
    WireFact {
        predicate: id(schema, "src.Decl"),
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(module(schema, path, "N")))),
            WireValue::Str(name.to_owned()),
            WireValue::Int(line),
        ])),
        value: Some(WireValue::Str("class".to_owned())),
    }
}

struct Serving {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
}

fn start() -> Serving {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");
    let schema = schema();

    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog.create("code", &schema).expect("a database");

    let (registry, _listing) =
        Registry::open(catalog, Schemas::new("", schema)).expect("a registry");
    let listener = Listener::bind(&socket).expect("a socket");

    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    let serving = Serving { _dir: dir, socket };
    seed(&serving);
    serving
}

/// Two files, one declaration each, one reference from A to B's, and the line text.
///
/// Deliberately cross-file: a reference that pointed inside its own file would let a
/// viewer that ignored the target's file still look right.
fn seed(serving: &Serving) {
    let schema = schema();
    let mut writer = Connection::connect(
        &serving.socket,
        "code",
        Arc::new(schema.clone()),
        Mode::ReadWrite,
        true,
    )
    .expect("a writer");

    let files = vec![file(&schema, FILE_A), file(&schema, FILE_B)];
    writer
        .write(id(&schema, "src.File"), &files)
        .expect("the files");

    let decls = vec![
        decl(&schema, FILE_A, "Alpha", 1),
        decl(&schema, FILE_B, "Beta", 3),
    ];
    writer
        .write(id(&schema, "src.Decl"), &decls)
        .expect("the declarations");

    // The search indexes, both cases, exactly as the indexer writes them.
    for (predicate, name) in [("src.SearchByName", "Alpha"), ("src.SearchByName", "Beta")] {
        let target = if name == "Alpha" {
            decl(&schema, FILE_A, "Alpha", 1)
        } else {
            decl(&schema, FILE_B, "Beta", 3)
        };

        let fact = WireFact {
            predicate: id(&schema, predicate),
            key: WireValue::Record(Box::from([
                WireValue::Str(name.to_owned()),
                WireValue::Ref(WireRef::Nested(Box::new(target.clone()))),
            ])),
            value: None,
        };
        writer
            .write(id(&schema, predicate), &[fact])
            .expect("the search index");

        let folded = WireFact {
            predicate: id(&schema, "src.SearchByLowerName"),
            key: WireValue::Record(Box::from([
                WireValue::Str(name.to_lowercase()),
                WireValue::Ref(WireRef::Nested(Box::new(target))),
            ])),
            value: None,
        };
        writer
            .write(id(&schema, "src.SearchByLowerName"), &[folded])
            .expect("the folded index");
    }

    // `Beta` is used on line 2 of A, at column 5, and is four characters long.
    let at = WireValue::Record(Box::from([
        WireValue::Int(2),
        WireValue::Int(5),
        WireValue::Int(4),
    ]));

    let reference = WireFact {
        predicate: id(&schema, "src.Ref"),
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(decl(&schema, FILE_B, "Beta", 3)))),
            WireValue::Ref(WireRef::Nested(Box::new(file(&schema, FILE_A)))),
            at.clone(),
        ])),
        value: None,
    };
    writer
        .write(id(&schema, "src.Ref"), &[reference])
        .expect("the reference");

    let xref = WireFact {
        predicate: id(&schema, "src.FileXRef"),
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(file(&schema, FILE_A)))),
            at,
            WireValue::Ref(WireRef::Nested(Box::new(decl(&schema, FILE_B, "Beta", 3)))),
        ])),
        value: None,
    };
    writer
        .write(id(&schema, "src.FileXRef"), &[xref])
        .expect("the file-keyed reference");

    // Two lines of source for A, so the reference has something to sit on. Line 2 is
    // `    Beta x;` — `Beta` starts at column 5.
    let lines: Vec<WireFact> = [(1i64, "class Alpha {"), (2, "    Beta x;")]
        .into_iter()
        .map(|(number, text)| WireFact {
            predicate: id(&schema, "src.Line"),
            key: WireValue::Record(Box::from([
                WireValue::Ref(WireRef::Nested(Box::new(file(&schema, FILE_A)))),
                WireValue::Int(number),
            ])),
            value: Some(WireValue::Str(text.to_owned())),
        })
        .collect();

    writer
        .write(id(&schema, "src.Line"), &lines)
        .expect("the line table");
}

// ---- the viewer ------------------------------------------------------------

/// Stand the viewer up on a port and return its address.
fn serve(serving: &Serving) -> (SocketAddr, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");

    let app = aperture_viewer::App::open(&serving.socket, "code", Arc::new(schema()), 2)
        .expect("the viewer opens the database");

    let app = Arc::new(app);

    let (address, listener) = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        (listener.local_addr().expect("its address"), listener)
    });

    runtime.spawn(async move {
        let _ = axum::serve(listener, app.router()).await;
    });

    (address, runtime)
}

/// A GET, with a blocking client written here because the viewer's own dependencies
/// stop at the client and a test has no business widening them.
fn get(address: SocketAddr, path: &str) -> (u16, String) {
    use std::io::{Read, Write};

    let mut socket = std::net::TcpStream::connect(address).expect("a connection");
    socket.set_read_timeout(Some(Duration::from_secs(30))).ok();

    write!(
        socket,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .expect("a request");

    let mut raw = String::new();
    socket.read_to_string(&mut raw).expect("a response");

    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);

    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();

    (status, body)
}

/// **Every screen answers, and the file view has its links.**
///
/// One test rather than five, because standing up a server, a database and a viewer
/// is most of the cost and the claims are independent of each other.
#[test]
fn every_screen_answers() {
    let serving = start();
    let (address, _runtime) = serve(&serving);

    // The health check asks the *database*, so a 200 here says the whole chain is up.
    let (status, body) = get(address, "/health");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("files indexed 2"), "{body}");

    // The file list.
    let (status, body) = get(address, "/");
    assert_eq!(status, 200);
    assert!(body.contains(FILE_A), "{body}");
    assert!(body.contains(FILE_B), "{body}");

    // **The file view.** The source is there, escaped, and the reference on line 2 is
    // a link to the *other* file at the target's line — which is the whole demo.
    let (status, body) = get(address, "/file/src/a.cs");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("class Alpha {"), "{body}");
    assert!(
        body.contains(r#"<a href="/file/src/b.cs#L3""#),
        "the cross-reference is not linked to its target:\n{body}"
    );
    assert!(
        body.contains(">Beta</a>"),
        "the link does not cover the identifier:\n{body}"
    );

    // Case-insensitive search finds a capitalised name from a lowercase term.
    let (status, body) = get(address, "/search?q=bet");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Beta"), "{body}");
    assert!(body.contains("1 match"), "the count is wrong:\n{body}");

    // And find-references resolves the file *id* a reference carries into a path.
    let (status, body) = get(address, "/symbol/Beta");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("used 1 time"), "{body}");
    assert!(
        body.contains(r#"href="/file/src/a.cs#L2""#),
        "the use is not linked to where it is:\n{body}"
    );

    // A file with no source is a 404 rather than a blank page.
    let (status, _) = get(address, "/file/src/nothing.cs");
    assert_eq!(status, 404);

    // A symbol nobody declares says so.
    let (status, body) = get(address, "/symbol/Nothing");
    assert_eq!(status, 200);
    assert!(body.contains("no declaration by that name"), "{body}");
}

/// A path with a quote in it cannot end the focus literal it lands in.
///
/// The one place a request reaches the query language. Nothing in a real index
/// contains a quote, which is exactly why the case is worth a test: the input that
/// never happens is the one nobody notices is wrong.
#[test]
fn a_hostile_path_does_not_reach_the_query() {
    let serving = start();
    let (address, _runtime) = serve(&serving);

    let (status, body) = get(address, "/file/x%22%3B%20src.File%20_%3B%20F%20%3D%20%22");

    // Whatever it answers, it must not be a *compile* error — which is what a query
    // spliced out of shape would produce, and is how injection announces itself here.
    assert!(
        !body.contains("invalid syntax") && !body.contains("BadQuery"),
        "the path reached the parser:\n{body}"
    );
    assert!(status == 404 || status == 200, "{status}");
}

/// **No page reads a predicate whole.**
///
/// The guard for the trap that cost this viewer 58 seconds a page against the 25M-fact
/// index, and which every test here passed while it was live: with a two-file corpus
/// both spellings answer identically in microseconds, so nothing that checks *rows*
/// can see it.
///
/// What it checks instead is the **plan**, through the profile the wire already
/// carries: `ProfileStep::full_scan` says a step read a predicate whole, and that is a
/// property of the plan rather than of the corpus. So a two-file database detects it
/// exactly as a twenty-five-million-fact one does — which is the whole reason this can
/// be a unit-cost test rather than a benchmark.
///
/// # The trap it is actually about
///
/// A row bind **claims** its variable. Written
/// `src.SearchByLowerName {name = "x".., to = D}; D = src.Decl {module = M}`, the
/// second statement says what `D` *is*, so `flatten`'s `Claims` makes the first
/// statement's mention of `D` a read — and the level binding it has to run first. No
/// reordering can rescue that, because it is not an ordering question: `reorder` is
/// working as designed, and the seek becomes a residual over all 888,177 declarations.
///
/// Reading *through* the reference the seek already bound (`D.module.file`) is the same
/// answer at 2.1 ms against 30,222 ms. Both are things a person would write. This is
/// what stands between the fast one and the next edit.
#[test]
fn no_page_reads_a_predicate_whole() {
    let serving = start();

    let mut connection = Connection::connect(
        &serving.socket,
        "code",
        Arc::new(schema()),
        Mode::ReadOnly,
        false,
    )
    .expect("a reader");

    let mut scanning = vec![];

    for (name, query) in aperture_viewer::query::census() {
        let mut rows = connection.query_profiled(&query).expect(name);
        let _ = connection.drain(&mut rows).expect("its rows");

        let profile = rows
            .profile()
            .expect("a profiled query reports what it examined");

        for step in &profile.steps {
            if step.full_scan {
                scanning.push(format!(
                    "{name}: step `{}` reads its predicate whole\n      {query}",
                    step.label
                ));
            }
        }
    }

    assert!(
        scanning.is_empty(),
        "{} of the viewer's queries scan a predicate:\n    {}",
        scanning.len(),
        scanning.join("\n    ")
    );
}

/// **The one scan that is deliberate**, named so it cannot be confused for a miss.
///
/// `Paths::load` reads every `src.File` — that is what it is for, and there is no key
/// order that makes "every row" a seek. It is exempt from the guard above by not being
/// in the census, and this is the exemption written down.
///
/// It is also bounded in a way none of the page queries are: it runs **once**, at
/// startup, over the smallest predicate in the source layer (~32,000 rows on
/// `dotnet/runtime`).
#[test]
fn loading_the_file_list_is_the_one_deliberate_scan() {
    let serving = start();

    let mut connection = Connection::connect(
        &serving.socket,
        "code",
        Arc::new(schema()),
        Mode::ReadOnly,
        false,
    )
    .expect("a reader");

    let mut rows = connection
        .query_profiled("{id = X, path = P} where X = src.File P")
        .expect("the file list");
    let _ = connection.drain(&mut rows).expect("its rows");

    let profile = rows.profile().expect("a profile");

    assert!(
        profile.steps.iter().any(|step| step.full_scan),
        "the file list is expected to scan; if it seeks now, the guard above should \
         cover it and this test should go"
    );
}
