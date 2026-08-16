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
use aperture_wire::{WireFact, WireValue, protocol::provisional_fingerprint};

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
        .create(
            "code",
            &code_index::schema(),
            provisional_fingerprint(&code_index::schema()),
        )
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

/// `files` files, written over the wire like anything else.
fn seed(serving: &Serving, files: usize) {
    let mut writer = Connection::connect(
        &serving.socket,
        "code",
        Arc::new(code_index::schema()),
        Mode::ReadWrite,
        true,
    )
    .expect("a writer");

    let facts: Vec<WireFact> = (0..files)
        .map(|n| WireFact {
            predicate: code_index::FILE,
            // Zero-padded, so the order rows come back in is the order they were
            // written — which is what lets a paging test compare sequences rather
            // than sets.
            key: WireValue::Str(format!("f{n:05}.py")),
            value: None,
        })
        .collect();

    writer
        .write(code_index::FILE, &facts)
        .expect("the facts are written");
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
        .create(
            "code",
            &code_index::schema(),
            provisional_fingerprint(&code_index::schema()),
        )
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
