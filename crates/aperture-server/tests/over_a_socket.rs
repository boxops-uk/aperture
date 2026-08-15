//! **Phase 7a's criterion, as a test**: facts are writable over a socket and queried
//! back on the same connection.
//!
//! Over a real `UnixListener` and a real `FjallDb`, because the criterion says
//! *socket* — an in-process call proves the frame handling and not the thing that was
//! promised. The client here is deliberately hand-rolled from `aperture-wire` alone,
//! which is the same position the .NET client is in: if this needs something the wire
//! crate does not expose, so does every other client.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::Arc,
    thread,
};

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_server::{
    Database, ErrorCode, Mode, Startup,
    protocol::{self, kinds},
    server::Listener,
};
use aperture_store::store::FjallDb;
use aperture_wire::{
    Desc, FrameHeader, FrameKind, StreamId, WireFact, WireRef, WireValue, decode_desc,
    encode_block, encode_frame, frame, value::decode_value,
};
use lasso::Rodeo;

const FILE: PredicateId = PredicateId(0);
const DECL: PredicateId = PredicateId(1);
/// A predicate **with a value side**, so two facts can share a key and disagree —
/// which is the only way to provoke a conflict, and so the only way to check that a
/// conflict reaches a client under its own code.
const BLOB: PredicateId = PredicateId(2);

fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let (file, decl, blob) = (
        rodeo.get_or_intern("src.File"),
        rodeo.get_or_intern("src.Decl"),
        rodeo.get_or_intern("src.Blob"),
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
            Predicate {
                name: blob,
                key: PredicateTy::Str,
                value: Some(PredicateTy::Int),
            },
        ]),
    )
}

fn blob(path: &str, contents: i64) -> WireFact {
    WireFact {
        predicate: BLOB,
        key: WireValue::Str(path.to_owned()),
        value: Some(WireValue::Int(contents)),
    }
}

/// A running server, and how to reach it.
struct Serving {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    fingerprint: u64,
}

fn start() -> Serving {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");
    let store = dir.path().join("db");

    let db = FjallDb::open(&store).expect("a database");
    let schema = schema();
    let fingerprint = protocol::provisional_fingerprint(&schema);
    let database = Arc::new(Database::new("code", db, schema));

    let listener = Listener::bind(&socket).expect("a socket");

    thread::spawn(move || {
        let _ = listener.run(vec![database]);
    });

    // The listener is bound before `run` is called, so the socket is already
    // accepting by the time `bind` returned — no readiness poll needed here. A
    // separate process would use `announce`, which is what that exists for.
    Serving {
        _dir: dir,
        socket,
        fingerprint,
    }
}

/// A minimal client: frames in, frames out.
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
        let mut head = [0u8; frame::HEADER_LEN];
        self.stream.read_exact(&mut head).expect("a frame header");
        let header = frame::decode_header(&head).expect("a header");

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
        });

        self.send(kinds::STARTUP, StreamId(0), &startup);
        self.recv()
    }
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
                // Nested: the client holds no ids at all, which is the point.
                WireValue::Ref(WireRef::Nested(Box::new(file(path)))),
                WireValue::Int(line),
                WireValue::Str(name.to_owned()),
            ]
            .into(),
        ),
        value: None,
    }
}

/// **The criterion.** Handshake, write a block of facts holding no ids, query them
/// back — one connection, start to finish.
#[test]
fn facts_are_writable_over_a_socket_and_queried_back_on_the_same_connection() {
    let serving = start();
    let mut client = Client::connect(&serving);

    // ---- handshake
    let (header, payload) = client.hello(serving.fingerprint, Mode::ReadWrite);
    assert_eq!(header.kind, kinds::READY);
    let ready = protocol::decode_ready(&payload).expect("a ready frame");
    assert_eq!(ready.version, protocol::VERSION);
    assert_eq!(ready.schema_fingerprint, serving.fingerprint);
    assert_eq!(ready.predicates, 3);

    // ---- write stream
    let write = StreamId(1);
    client.send(kinds::OPEN_WRITE, write, &[]);
    let (header, _) = client.recv();
    assert_eq!(header.kind, FrameKind::COPY_IN_RESPONSE);
    assert_eq!(header.stream, write);

    let facts = vec![
        decl("store/keys.py", 12, "key_of"),
        decl("store/keys.py", 48, "key_prefix"),
        decl("store/codec.py", 7, "encode_key"),
    ];

    let mut block = vec![];
    encode_block(&mut block, &schema(), DECL, &facts).expect("a block");
    client.send(FrameKind::COPY_DATA, write, &block);
    client.send(FrameKind::COPY_DONE, write, &[]);

    let (header, payload) = client.recv();
    assert_eq!(header.kind, kinds::COMPLETE);
    let (created, deduped) = protocol::decode_complete(&payload).expect("counts");

    // Three declarations and two files: `store/keys.py` is named twice and written
    // once. Interning, over a socket.
    assert_eq!((created, deduped), (5, 1));

    // ---- query stream, on the *same* connection
    let read = StreamId(2);
    client.send(kinds::QUERY, read, b"F where src.File F");

    let (header, payload) = client.recv();
    assert_eq!(header.kind, FrameKind::ROW_DESCRIPTION);
    assert_eq!(header.stream, read);
    let (desc, _) = decode_desc(&payload).expect("a descriptor");
    assert_eq!(desc, Desc::Str, "the head is a bare string");

    let mut paths = vec![];
    loop {
        let (header, payload) = client.recv();

        if header.kind == kinds::COMPLETE {
            let (rows, _) = protocol::decode_complete(&payload).expect("counts");
            assert_eq!(rows as usize, paths.len());
            break;
        }

        assert_eq!(header.kind, FrameKind::DATA_ROW);
        let (value, _) =
            decode_value(&payload, &schema(), &PredicateTy::Str).expect("a row decodes");

        match value {
            WireValue::Str(path) => paths.push(path),
            other => panic!("expected a string row, got {other:?}"),
        }
    }

    paths.sort();
    assert_eq!(paths, vec!["store/codec.py", "store/keys.py"]);
}

/// A record head comes back as a record descriptor, and the rows follow it
/// positionally — the case that makes a descriptor necessary at all, since no
/// predicate declares this shape.
#[test]
fn a_record_head_describes_itself_and_its_rows_follow() {
    let serving = start();
    let mut client = Client::connect(&serving);
    client.hello(0, Mode::ReadWrite);

    let write = StreamId(1);
    client.send(kinds::OPEN_WRITE, write, &[]);
    client.recv();

    let mut block = vec![];
    encode_block(&mut block, &schema(), DECL, &[decl("a.py", 3, "f")]).expect("a block");
    client.send(FrameKind::COPY_DATA, write, &block);
    client.send(FrameKind::COPY_DONE, write, &[]);
    client.recv();

    client.send(
        kinds::QUERY,
        StreamId(2),
        b"{at = D.line, what = D.name} where D = src.Decl _",
    );

    let (header, payload) = client.recv();
    assert_eq!(header.kind, FrameKind::ROW_DESCRIPTION);
    let (desc, _) = decode_desc(&payload).expect("a descriptor");

    let Desc::Record(fields) = &desc else {
        panic!("expected a record descriptor, got {desc:?}");
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].0, "at");
    assert_eq!(fields[1].0, "what");

    let (header, payload) = client.recv();
    assert_eq!(header.kind, FrameKind::DATA_ROW);

    // A client decodes rows with the same value decoder it encodes facts with — the
    // descriptor converts to a type and everything downstream is the ordinary codec.
    let mut interner = aperture_schema::schema::LocalInterner::new(schema().interner().clone());
    let ty = desc.to_ty(&mut interner);
    let (row, _) = decode_value(&payload, &schema(), &ty).expect("a row decodes");

    assert_eq!(
        row,
        WireValue::Record(vec![WireValue::Int(3), WireValue::Str("f".to_owned())].into())
    );
}

/// **A wrong schema fingerprint is refused before any data flows** — the cheap early
/// mismatch detection §6 is after, and the reason the handshake carries one.
#[test]
fn a_schema_mismatch_is_refused_at_the_handshake() {
    let serving = start();
    let mut client = Client::connect(&serving);

    let (header, payload) = client.hello(serving.fingerprint ^ 0xFF, Mode::ReadWrite);

    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, message) = protocol::decode_error(&payload).expect("an error frame");
    assert_eq!(code, ErrorCode::SchemaMismatch);
    assert!(message.contains("schema mismatch"), "{message}");
}

/// Zero means "do not check", which is what a reader or a client written against
/// whatever the server has will send.
#[test]
fn a_zero_fingerprint_skips_the_check() {
    let serving = start();
    let mut client = Client::connect(&serving);

    let (header, _) = client.hello(0, Mode::ReadOnly);
    assert_eq!(header.kind, kinds::READY);
}

/// A read-only session cannot open a write stream, and the refusal is `ops-I6`'s:
/// the mode is declared once at startup and resolved there, not argued about per
/// frame.
#[test]
fn a_read_only_session_cannot_write() {
    let serving = start();
    let mut client = Client::connect(&serving);
    client.hello(0, Mode::ReadOnly);

    client.send(kinds::OPEN_WRITE, StreamId(1), &[]);
    let (header, payload) = client.recv();

    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, _) = protocol::decode_error(&payload).expect("an error frame");
    assert_eq!(code, ErrorCode::ModeRefused);
}

/// **A stream-level fault fails its stream and the connection survives** — which is
/// what makes multiplexing worth having at all, and is checked by using the
/// connection afterwards.
#[test]
fn a_failed_stream_leaves_the_connection_usable() {
    let serving = start();
    let mut client = Client::connect(&serving);
    client.hello(0, Mode::ReadWrite);

    // A query that does not compile.
    client.send(kinds::QUERY, StreamId(1), b"this is not focus");
    let (header, payload) = client.recv();
    assert_eq!(header.kind, FrameKind::ERROR);
    assert_eq!(header.stream, StreamId(1), "the error names its own stream");
    let (code, _) = protocol::decode_error(&payload).expect("an error frame");
    assert_eq!(code, ErrorCode::BadQuery);

    // The connection still works, on a different stream.
    client.send(kinds::QUERY, StreamId(2), b"F where src.File F");
    let (header, _) = client.recv();
    assert_eq!(header.kind, FrameKind::ROW_DESCRIPTION);
    assert_eq!(header.stream, StreamId(2));
}

/// Fact blocks on a stream that was never opened for writing are a protocol fault
/// rather than an implicit open — see [`session`](aperture_server::session) for why
/// an implicit open would be worse than an error.
#[test]
fn copy_data_on_an_unopened_stream_is_refused() {
    let serving = start();
    let mut client = Client::connect(&serving);
    client.hello(0, Mode::ReadWrite);

    let mut block = vec![];
    encode_block(&mut block, &schema(), FILE, &[file("a.py")]).expect("a block");
    client.send(FrameKind::COPY_DATA, StreamId(9), &block);

    let (header, payload) = client.recv();
    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, message) = protocol::decode_error(&payload).expect("an error frame");
    assert_eq!(code, ErrorCode::Protocol);
    assert!(message.contains("never opened"), "{message}");
}

/// **A block whose bytes do not decode fails its stream, and the connection lives.**
///
/// Worth telling apart from a frame-level fault, and the codes do: the *frame* was
/// well-formed — its length delimited the payload correctly — so nothing about the
/// connection is in doubt. Only the payload was rubbish, which is `BadFacts` and
/// survivable, where a frame that would not parse is `Protocol` and is not.
#[test]
fn a_malformed_block_fails_its_stream_and_the_connection_lives() {
    let serving = start();
    let mut client = Client::connect(&serving);
    client.hello(0, Mode::ReadWrite);

    let write = StreamId(1);
    client.send(kinds::OPEN_WRITE, write, &[]);
    client.recv();

    client.send(FrameKind::COPY_DATA, write, b"not a block");
    let (header, payload) = client.recv();

    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, _) = protocol::decode_error(&payload).expect("an error frame");
    assert_eq!(code, ErrorCode::BadFacts);

    // Still usable afterwards.
    client.send(kinds::QUERY, StreamId(2), b"F where src.File F");
    let (header, _) = client.recv();
    assert_eq!(header.kind, FrameKind::ROW_DESCRIPTION);
}

/// **A conflict reaches the client under its own code**, so a producer can tell "you
/// contradicted yourself" from "your bytes were malformed" without reading English.
#[test]
fn a_conflict_has_its_own_code() {
    let serving = start();
    let mut client = Client::connect(&serving);
    client.hello(0, Mode::ReadWrite);

    let mut send_blob = |stream: StreamId, contents: i64| {
        client.send(kinds::OPEN_WRITE, stream, &[]);
        client.recv();

        let mut block = vec![];
        encode_block(&mut block, &schema(), BLOB, &[blob("same.py", contents)]).expect("a block");
        client.send(FrameKind::COPY_DATA, stream, &block);
        client.send(FrameKind::COPY_DONE, stream, &[]);
        client.recv()
    };

    let (header, _) = send_blob(StreamId(1), 1);
    assert_eq!(header.kind, kinds::COMPLETE, "the first blob lands");

    // The same key, a different value side: ops-I5's same-key-different-value, over a
    // socket.
    let (header, payload) = send_blob(StreamId(2), 2);
    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, _) = protocol::decode_error(&payload).expect("an error frame");
    assert_eq!(code, ErrorCode::Conflict);
}
