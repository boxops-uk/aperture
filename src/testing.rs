//! A server on a socket, in this process, for the tests that need one.
//!
//! **Why the tool tests a server in-process at all.** `tests/over_a_server.rs` drives
//! the real binary against a real `aperture serve`, which is the right shape for the
//! lifecycle — it proves the frames crossed a socket between two processes. It is the
//! wrong shape for anything that needs *facts in a database*, because the tool has no
//! command that writes any (that is Phase 7b), and the wrong shape for anything that
//! needs to look at a value the binary does not print.
//!
//! So this stands the same server up behind the same socket, writes facts through the
//! ordinary client, and hands back a path. Nothing here is a shortcut around the wire:
//! every fact below is encoded, framed and interned exactly as a `.NET` producer's are.

use std::{path::PathBuf, sync::Arc, thread};

use aperture_client::{Connection, Mode};
use aperture_server::{Registry, server::Listener};
use aperture_store::catalog::Catalog;
use aperture_wire::{WireFact, WireRef, WireValue};

use crate::code_index;

/// A running server, and the scratch directory it lives in.
pub struct Serving {
    /// Kept for its `Drop`: the directory outlives every use and goes at the end.
    _dir: tempfile::TempDir,
    pub socket: PathBuf,
}

/// A server holding one database, `code`, with `files` files in it.
///
/// The thread is deliberately not joined: the listener runs until the process ends,
/// which for a test binary is the right lifetime and saves every caller a shutdown
/// dance for something that owns nothing but a socket.
pub fn serving(files: usize) -> Serving {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");

    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog
        .create("code", &code_index::schema())
        .expect("a database");

    // The **served** schema, as `serve` builds it: the stored predicates plus the
    // catalogue. Created with the stored one, because a virtual predicate is not part
    // of the artifact — which is the arrangement these tests exist to exercise.
    let (registry, _listing) =
        Registry::open(catalog, code_index::with_catalogue()).expect("a registry");
    let listener = Listener::bind(&socket).expect("a socket");

    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    let serving = Serving {
        _dir: dir,
        socket: socket.clone(),
    };

    if files > 0 {
        seed(&serving, files);
    }

    serving
}

/// A file, by index — the same key the seeder writes, so a test can name one.
fn file_fact(n: usize) -> WireFact {
    WireFact {
        predicate: code_index::id("src.File"),
        // Zero-padded, so the order rows come back in is the order they were written —
        // which is what lets a paging test compare sequences rather than sets.
        key: WireValue::Str(format!("f{n:05}.py")),
        value: None,
    }
}

/// A module in that file, nesting it — so something in this corpus holds a **reference**.
fn module_fact(n: usize) -> WireFact {
    WireFact {
        predicate: code_index::id("src.Module"),
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(file_fact(n)))),
            WireValue::Str(format!("m{n:05}")),
        ])),
        value: None,
    }
}

/// A declaration in that module, nesting it — two levels deep, so a query can read
/// *through* a reference and make the store answer a point read.
fn decl_fact(n: usize) -> WireFact {
    WireFact {
        predicate: code_index::id("src.Decl"),
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(module_fact(n)))),
            WireValue::Str(format!("d{n:05}")),
            WireValue::Int(n as i64),
        ])),
        value: Some(WireValue::Str("def".to_owned())),
    }
}

/// `files` files, each with a module and a declaration, written over the wire like
/// anything else.
///
/// **The nesting is what makes the corpus worth having.** A file is a bare string, so a
/// query over files alone never asks the store for a point read — and a fetch through a
/// reference is exactly the path a wrapped store has to get right and can most easily
/// get wrong. One module and one declaration per file gives every test a `D.module.name`
/// to reach for, at the cost of two more facts per file.
fn seed(serving: &Serving, files: usize) {
    let mut writer = Connection::connect(
        &serving.socket,
        "code",
        Arc::new(code_index::schema()),
        Mode::ReadWrite,
        true,
    )
    .expect("a writer");

    let facts: Vec<WireFact> = (0..files).map(file_fact).collect();

    writer
        .write(code_index::id("src.File"), &facts)
        .expect("the files are written");

    // One block per predicate, because a block is a run of one predicate's facts. The
    // modules and declarations nest what came before them, so the server interns rather
    // than creating — which is the write path a real producer takes.
    let modules: Vec<WireFact> = (0..files).map(module_fact).collect();
    writer
        .write(code_index::id("src.Module"), &modules)
        .expect("the modules are written");

    let decls: Vec<WireFact> = (0..files).map(decl_fact).collect();
    writer
        .write(code_index::id("src.Decl"), &decls)
        .expect("the declarations are written");
}

/// A server holding one database, `code`, listening on **both** doors.
///
/// Returns the TCP address alongside the socket, so a test can ask the same question
/// through each and compare the answers — which is the only claim `--listen-tcp` makes:
/// the same protocol, over a different pipe.
pub fn serving_on_tcp(files: usize) -> (Serving, String) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");

    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog
        .create("code", &code_index::schema())
        .expect("a database");

    let (registry, _listing) =
        Registry::open(catalog, code_index::with_catalogue()).expect("a registry");
    let registry = Arc::new(registry);

    // **A port the OS chose, taken and released.** `serve_on` takes an address rather
    // than a bound listener, so there is no way to ask it what port 0 became; binding
    // here first is how the test learns a free one. The window between drop and re-bind
    // is a race in principle and has never been one in practice, and the alternative —
    // a fixed port — fails whenever anything else on the machine wants it.
    let address = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("its address").to_string()
    };

    let listener = aperture_server::Listener::bind(&socket).expect("a socket");

    {
        let address = address.clone();
        let socket = socket.clone();
        thread::spawn(move || {
            drop(listener);
            let _ = aperture_server::server::serve_on(&socket, Some(&address), None, registry);
        });
    }

    // The listener is bound inside the thread, so wait for the door to open rather than
    // racing it.
    for _ in 0..200 {
        if std::net::TcpStream::connect(&address).is_ok() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }

    let serving = Serving {
        _dir: dir,
        socket: socket.clone(),
    };

    if files > 0 {
        seed(&serving, files);
    }

    (serving, address)
}

/// Make another database on this server, over the wire.
///
/// **So a listing can have more than one row in it**, which is the difference between a
/// test that exercises a range bound and one that cannot: with a single database, the
/// lower bound of any seek excludes everything the upper bound would have, and a broken
/// upper bound is invisible.
pub fn create_database(serving: &Serving, name: &str) {
    let mut control = Connection::control(&serving.socket, Arc::new(code_index::schema()))
        .expect("a control session");

    control.create(name).expect("the database is created");
}
