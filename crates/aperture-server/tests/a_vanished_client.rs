//! **A client that hangs up mid-answer must not strand the stream answering it.**
//!
//! This is the regression guard for the worst defect this server has had
//! (`bench/FINDINGS.md` §10): a connection dying while rows were still owed left its
//! stream task parked forever in [`Outbound::send`](aperture_server::outbound::Outbound),
//! waiting for queue room that only the writer frees — and the writer had died with the
//! socket. Half a million such connections took a server from 1.0 GB to 8.2 GB, linearly,
//! and nothing came back. It needs no privilege: a `Ctrl-C`, a crashed consumer, a proxy
//! timing out.
//!
//! There is a unit guard beside the mechanism in `outbound.rs`, and it is the one that
//! fails fastest. This one is here because a mechanism can be right while the thing it
//! was for stays broken: what a *server* owes is that the task ends, and the only way to
//! say that is to count live stream tasks across a real socket, with a real client, that
//! really goes away.
//!
//! It is an integration test for the same reason `over_a_socket.rs` is one — a client
//! that vanishes has to be a separate process's worth of separateness, which here means
//! a real `UnixStream` that gets dropped.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_server::{Registry, registry::Schemas, server::Listener};
use aperture_store::catalog::Catalog;
use aperture_wire::{
    FrameHeader, FrameKind, Mode, Startup, StreamId, WireFact, WireValue, encode_block,
    encode_frame, frame,
    protocol::{self, kinds},
};
use lasso::Rodeo;

const FILE: PredicateId = PredicateId(0);

/// One predicate, a bare string, because the point is the *number* of rows.
fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let file = rodeo.get_or_intern("src.File");

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![Predicate {
            name: file,
            key: PredicateTy::Str,
            value: None,
        }]),
    )
}

fn file(path: &str) -> WireFact {
    WireFact {
        predicate: FILE,
        key: WireValue::Str(path.to_owned()),
        value: None,
    }
}

/// Enough rows, and wide enough ones, that the answer cannot fit in a socket buffer.
///
/// Both halves matter and the first one alone is not enough — which the vacuity check at
/// the bottom of the test caught. A departed peer's socket accepts writes until its RST
/// is processed, so 400 narrow rows (12 kB) were written, acknowledged by the kernel and
/// gone before the server ever learned the client had left: the writer never failed, the
/// queue never filled, and the test passed having exercised nothing. At ~1 kB a row this
/// is ~600 kB against a default `SO_SNDBUF` of ~208 kB, so the writer blocks, the RST
/// lands, and the producer meets a full queue behind a dead writer — the actual defect.
const ROWS: usize = 600;
const PATH_PADDING: usize = 1000;

struct Serving {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    fingerprint: u64,
    registry: Arc<Registry>,
}

fn start() -> Serving {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");

    let schema = schema();
    let fingerprint = aperture_schema::fingerprint::of(&schema);

    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    catalog.create("code", &schema).expect("a database");

    let (registry, _listing) =
        Registry::open(catalog, Schemas::new("", schema)).expect("a registry");
    let registry = Arc::new(registry);

    let listener = Listener::bind(&socket).expect("a socket");

    // The test keeps a handle on the registry the server is running, which is what lets
    // it read the counters afterwards. Everything else is `over_a_socket.rs`'s harness.
    let serving = Arc::clone(&registry);
    thread::spawn(move || {
        let _ = listener.run_blocking(serving);
    });

    Serving {
        _dir: dir,
        socket,
        fingerprint,
        registry,
    }
}

struct Client {
    stream: UnixStream,
}

impl Client {
    fn connect(serving: &Serving) -> Client {
        Client {
            stream: UnixStream::connect(&serving.socket).expect("a connection"),
        }
    }

    fn send(&mut self, kind: FrameKind, stream: StreamId, payload: &[u8]) {
        let mut out = vec![];
        encode_frame(&mut out, kind, stream, payload).expect("a frame");
        self.stream.write_all(&out).expect("a write");
    }

    fn recv(&mut self) -> (FrameHeader, Vec<u8>) {
        let mut header = [0u8; frame::HEADER_LEN];
        self.stream.read_exact(&mut header).expect("a header");
        let header = frame::decode_header(&header).expect("a well-formed header");

        let mut payload = vec![0u8; header.length as usize];
        self.stream.read_exact(&mut payload).expect("a payload");

        (header, payload)
    }

    fn hello(&mut self, fingerprint: u64, mode: Mode) -> (FrameHeader, Vec<u8>) {
        let startup = protocol::encode_startup(&Startup {
            version: protocol::VERSION,
            database: "code".to_owned(),
            mode,
            schema_fingerprint: fingerprint,
            predicates: vec![],
        });

        self.send(kinds::STARTUP, StreamId(0), &startup);
        self.recv()
    }
}

/// Wait for `f` to hold, or give up. Polled rather than slept-on: the fix makes this
/// immediate, and a fixed sleep would either be flaky or slow.
fn within(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn seed(serving: &Serving) {
    let mut client = Client::connect(serving);
    let (header, _) = client.hello(serving.fingerprint, Mode::ReadWrite);
    assert_eq!(header.kind, kinds::READY);

    let write = StreamId(1);
    client.send(kinds::OPEN_WRITE, write, &[]);
    let (header, _) = client.recv();
    assert_eq!(header.kind, FrameKind::COPY_IN_RESPONSE);

    let facts: Vec<WireFact> = (0..ROWS)
        .map(|n| {
            let padding = "x".repeat(PATH_PADDING);
            file(&format!("src/f{n:05}/{padding}.py"))
        })
        .collect();
    let mut block = vec![];
    encode_block(&mut block, &schema(), FILE, &facts).expect("a block");

    client.send(FrameKind::COPY_DATA, write, &block);
    client.send(FrameKind::COPY_DONE, write, &[]);

    let (header, _) = client.recv();
    assert_eq!(header.kind, kinds::COMPLETE);
}

/// How many clients vanish. **Not one, and the number is the point.**
///
/// With a single client this is a coin flip, and it lands the wrong way about 40% of the
/// time — measured, by reverting the fix and running it. The defect needs the producer to
/// fill its queue *before* the reader notices EOF and closes the connection; when the
/// reader wins, the producer is refused cleanly and nothing is stranded even on a broken
/// server. A guard that passes two runs in five against the bug it exists for is not a
/// guard, so this takes sixteen bites: against the unfixed server five of eight leaked,
/// which puts a clean sweep of sixteen at about one run in two million.
const VANISHING: usize = 16;

#[test]
fn clients_that_vanish_mid_result_strand_nothing() {
    let serving = start();
    seed(&serving);

    let stats = Arc::clone(serving.registry.stats());
    assert!(
        within(Duration::from_secs(5), || stats.streams_live() == 0),
        "the seeding connection should have left nothing behind"
    );

    let failed_before = stats.queries_failed();

    for _ in 0..VANISHING {
        let mut client = Client::connect(&serving);
        let (header, _) = client.hello(serving.fingerprint, Mode::ReadOnly);
        assert_eq!(header.kind, kinds::READY);

        // Ask for everything, read just enough to know the answer is under way, and go.
        //
        // **Reading first is load-bearing**, and leaving it out made an earlier version
        // of this test pass while measuring nothing. A socket dropped the instant after
        // the query is written takes the query with it — the server never reads the
        // frame, never starts, and there is no in-flight work to strand. Which is also
        // why the real reproduction (`probe storm`) takes two rows before vanishing.
        client.send(kinds::QUERY, StreamId(2), b"F where src.File F");

        let (header, _) = client.recv();
        assert_eq!(header.kind, FrameKind::ROW_DESCRIPTION);

        let (header, _) = client.recv();
        assert_eq!(header.kind, FrameKind::DATA_ROW, "the answer is under way");
    }

    // **The property.** However a connection ended, the work it started ends too.
    assert!(
        within(Duration::from_secs(10), || stats.streams_live() == 0),
        "{} of {VANISHING} vanished clients left a stream task alive; each is parked \
         holding a chunk, a plan and a database handle, and nothing will ever wake them",
        stats.streams_live()
    );

    // **And the test was not vacuous.** The queries have to have been cut short — if they
    // *completed*, every client left after its last row went out and the run never
    // reached the state the defect lives in.
    //
    // Note what this deliberately does not assert: that a producer waited for queue room.
    // That was the first witness tried, and it is wrong on a fixed server — the producer
    // now learns the connection went from `closed` under the lock and returns instead of
    // parking, so the counter it would have bumped stays at zero precisely *because* the
    // bug is gone. A witness only the broken code can satisfy is not a witness.
    assert!(
        stats.queries_failed() >= failed_before + VANISHING as u64,
        "only {} of {VANISHING} queries were cut short, so some of this run did not \
         exercise the path the guard is for — {ROWS} rows of ~{PATH_PADDING} bytes may \
         no longer outrun the socket buffer",
        stats.queries_failed() - failed_before
    );

    // **And the server is still serving**, which is the other half of "handled".
    let mut client = Client::connect(&serving);
    let (header, _) = client.hello(serving.fingerprint, Mode::ReadOnly);
    assert_eq!(header.kind, kinds::READY);

    client.send(kinds::QUERY, StreamId(2), b"F where src.File F");
    let (header, _) = client.recv();
    assert_eq!(
        header.kind,
        FrameKind::ROW_DESCRIPTION,
        "the server still answers"
    );

    let mut rows = 0;
    loop {
        let (header, _) = client.recv();
        match header.kind {
            FrameKind::DATA_ROW => rows += 1,
            kinds::COMPLETE => break,
            other => panic!("unexpected frame {other}"),
        }
    }

    assert_eq!(rows, ROWS, "and answers in full");
}
