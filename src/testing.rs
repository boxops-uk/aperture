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

    let schema = code_index::schema();
    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog
        .create("code", &schema, provisional_fingerprint(&schema))
        .expect("a database");

    let (registry, _listing) = Registry::open(catalog, schema).expect("a registry");
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
