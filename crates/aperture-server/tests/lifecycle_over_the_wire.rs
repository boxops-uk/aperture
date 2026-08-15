//! **9d's last piece, as a test**: a database is created, written, sealed and removed
//! *against a running server*, rather than by stopping it first.
//!
//! That was the whole of what was left. `ops-I1` gives one process the store root, so
//! before this the honest interim was for the CLI to refuse every lifecycle command
//! while a server held it — which made "usable" and "serving" mutually exclusive.
//!
//! `list` and `describe` are not here, and their absence is the design rather than a
//! gap: `ops-I7` reads sidecars and never opens fjall, so both already worked while a
//! server held every database. Only the three that *mutate* needed a way in.
//!
//! The client is hand-rolled from `aperture-wire` and `protocol` alone, as everywhere
//! else on this seam: if a lifecycle client needs something those two do not expose,
//! so does every non-Rust one.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_server::{
    Control, ControlOp, ControlReply, ErrorCode, Mode, Registry, Startup,
    protocol::{self, kinds},
    server::Listener,
};
use aperture_store::catalog::Catalog;
use aperture_wire::{
    FrameHeader, FrameKind, StreamId, WireFact, WireValue, encode_block, encode_frame, frame,
};
use lasso::Rodeo;

const FILE: PredicateId = PredicateId(0);

/// One predicate, so a fact count is a fact count: `src.File : string`.
///
/// Nesting is exercised to death in `over_a_socket.rs`; what is being counted here is
/// *when* a write is allowed, and a key that interned two facts would make every
/// assertion below a subtraction.
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

fn block(paths: &[&str]) -> Vec<u8> {
    let facts: Vec<WireFact> = paths.iter().map(|path| file(path)).collect();
    let mut out = vec![];
    encode_block(&mut out, &schema(), FILE, &facts).expect("a block");
    out
}

/// A running server over an **empty** store root, since the point is what it can be
/// told to put in one.
struct Serving {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    root: PathBuf,
    fingerprint: u64,
}

impl Serving {
    /// A catalog over the same root, for checking what the server did to the disk.
    ///
    /// Takes no lock, and that is `ops-I7`: reading the catalog while a server owns
    /// every database under it is the one thing that must always work.
    fn catalog(&self) -> Catalog {
        Catalog::open(&self.root).expect("a store root")
    }
}

fn start() -> Serving {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let socket = dir.path().join("aperture.sock");
    let root = dir.path().join("store");

    let schema = schema();
    let fingerprint = protocol::provisional_fingerprint(&schema);

    let catalog = Catalog::open(&root).expect("a store root");
    let (registry, _listing) = Registry::open(catalog, schema).expect("a registry");

    let listener = Listener::bind(&socket).expect("a socket");
    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    Serving {
        _dir: dir,
        socket,
        root,
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

    /// Open a session bound to `database`, or — for the empty string — to none at all.
    fn hello(serving: &Serving, database: &str, mode: Mode) -> (Client, FrameHeader, Vec<u8>) {
        let mut client = Client::connect(serving);

        let startup = protocol::encode_startup(&Startup {
            version: protocol::VERSION,
            database: database.to_owned(),
            mode,
            schema_fingerprint: serving.fingerprint,
        });

        client.send(kinds::STARTUP, StreamId(0), &startup);
        let (header, payload) = client.recv();
        (client, header, payload)
    }

    /// A **control session**: bound to no database, which is the only session a
    /// `create` could be sent on.
    fn control_session(serving: &Serving, mode: Mode) -> Client {
        let (client, header, _) = Client::hello(serving, "", mode);
        assert_eq!(header.kind, kinds::READY, "a control session establishes");
        client
    }

    fn control(&mut self, op: ControlOp, database: &str, allow_zero_facts: bool) -> ControlReply {
        let (header, payload) = self.control_raw(op, database, allow_zero_facts);
        assert_eq!(
            header.kind,
            kinds::CONTROL_REPLY,
            "expected a reply, got {:?}",
            protocol::decode_error(&payload)
        );
        protocol::decode_control_reply(&payload).expect("a control reply")
    }

    fn control_raw(
        &mut self,
        op: ControlOp,
        database: &str,
        allow_zero_facts: bool,
    ) -> (FrameHeader, Vec<u8>) {
        let request = protocol::encode_control(&Control {
            op,
            database: database.to_owned(),
            allow_zero_facts,
        });

        self.send(kinds::CONTROL, StreamId(1), &request);
        self.recv()
    }

    /// Write one block on a fresh write stream, and report what came back.
    fn write_block(&mut self, stream: StreamId, paths: &[&str]) -> (FrameHeader, Vec<u8>) {
        self.send(kinds::OPEN_WRITE, stream, &[]);
        let (header, payload) = self.recv();

        if header.kind != FrameKind::COPY_IN_RESPONSE {
            return (header, payload);
        }

        self.send(FrameKind::COPY_DATA, stream, &block(paths));
        self.send(FrameKind::COPY_DONE, stream, &[]);
        self.recv()
    }

    /// Run a query and count the rows it answered with.
    fn count(&mut self, stream: StreamId, source: &str) -> u64 {
        self.send(kinds::QUERY, stream, source.as_bytes());

        let (header, payload) = self.recv();
        assert_eq!(
            header.kind,
            FrameKind::ROW_DESCRIPTION,
            "{:?}",
            protocol::decode_error(&payload)
        );

        let mut rows = 0;
        loop {
            let (header, payload) = self.recv();
            match header.kind {
                FrameKind::DATA_ROW => rows += 1,
                kinds::COMPLETE => {
                    let (sent, _) = protocol::decode_complete(&payload).expect("a complete");
                    assert_eq!(sent, rows, "the count and the rows agree");
                    return rows;
                }
                other => panic!("unexpected frame `{other}` during a query"),
            }
        }
    }
}

fn error_of(payload: &[u8]) -> (ErrorCode, String) {
    protocol::decode_error(payload).expect("an error frame")
}

/// Poll until `attempt` succeeds, or give up loudly.
///
/// Needed exactly once, for a condition that is genuinely asynchronous: a session
/// releases its database when its task ends, and a client closing a socket does not
/// get to say when that happens.
fn eventually(what: &str, mut attempt: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        if attempt() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }

    panic!("{what} never happened");
}

/// **The criterion.** A database's whole life, over one socket, against a server that
/// is running throughout: create it, write to it, query it, seal it, and delete it.
#[test]
fn a_database_lives_and_dies_against_a_running_server() {
    let serving = start();
    let mut control = Client::control_session(&serving, Mode::ReadWrite);

    // ---- create
    let ControlReply::Created { instance } = control.control(ControlOp::Create, "code", false)
    else {
        panic!("expected a created reply");
    };
    assert!(!instance.is_empty(), "it was given a provisional instance");

    // On the disk, and visible to a reader that never opens fjall (`ops-I7`) — which
    // is how `list` and `describe` see a database this server just made.
    let entry = serving.catalog().get("code").expect("it is on the disk");
    assert_eq!(entry.meta.instance, instance);
    assert!(entry.status().is_writable());

    // ---- and immediately usable, without restarting anything
    let (mut writer, header, _) = Client::hello(&serving, "code", Mode::ReadWrite);
    assert_eq!(header.kind, kinds::READY, "the new database is served");

    let (header, payload) = writer.write_block(StreamId(1), &["a.py", "b.py", "c.py"]);
    assert_eq!(header.kind, kinds::COMPLETE);
    assert_eq!(
        protocol::decode_complete(&payload).expect("a complete"),
        (3, 0)
    );

    assert_eq!(writer.count(StreamId(2), "X where src.File X"), 3);

    // ---- seal
    let ControlReply::Finished {
        fingerprint,
        facts,
        bytes,
        already_complete,
    } = control.control(ControlOp::Finish, "code", false)
    else {
        panic!("expected a finished reply");
    };

    assert_eq!(facts, 3);
    assert!(fingerprint != 0, "an identity was computed, not stubbed");
    assert!(bytes > 0);
    assert!(!already_complete);

    let entry = serving.catalog().get("code").expect("it is on the disk");
    assert_eq!(entry.meta.content_fingerprint, Some(fingerprint));
    assert!(!entry.status().is_writable(), "the sidecar flipped");

    // ---- `ops-I2`: no writable session exists for it, ever again
    let (_refused, header, payload) = Client::hello(&serving, "code", Mode::ReadWrite);
    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, message) = error_of(&payload);
    assert_eq!(code, ErrorCode::ModeRefused);
    assert!(message.contains("code"), "{message}");

    // ...while reading it goes on working, which is what sealing is *for*.
    let (mut reader, header, _) = Client::hello(&serving, "code", Mode::ReadOnly);
    assert_eq!(header.kind, kinds::READY);
    assert_eq!(reader.count(StreamId(1), "X where src.File X"), 3);
    drop(reader);
    drop(writer);

    // ---- remove
    eventually("the sessions let go of `code`", || {
        matches!(
            control.control_raw(ControlOp::Remove, "code", false).0.kind,
            kinds::CONTROL_REPLY
        )
    });

    assert!(
        serving
            .catalog()
            .find("code")
            .expect("the root reads")
            .is_none(),
        "it is gone from the disk"
    );

    let (_gone, header, payload) = Client::hello(&serving, "code", Mode::ReadOnly);
    assert_eq!(header.kind, FrameKind::ERROR);
    assert_eq!(error_of(&payload).0, ErrorCode::UnknownDatabase);
}

/// **`ops-I2` reaches a session that was already open.** A write session established
/// while the database was Writable does not get to keep writing across a seal — which
/// is the case the establishment check alone cannot answer, and the reason the seal
/// happens inside the per-database writer lock.
#[test]
fn a_seal_stops_a_write_session_that_was_already_established() {
    let serving = start();
    let mut control = Client::control_session(&serving, Mode::ReadWrite);
    control.control(ControlOp::Create, "code", false);

    // Established *before* the seal, and kept open across it.
    let (mut writer, header, _) = Client::hello(&serving, "code", Mode::ReadWrite);
    assert_eq!(header.kind, kinds::READY);

    let (header, _) = writer.write_block(StreamId(1), &["a.py", "b.py"]);
    assert_eq!(header.kind, kinds::COMPLETE, "the first block lands");

    let reply = control.control(ControlOp::Finish, "code", false);
    assert!(
        matches!(reply, ControlReply::Finished { facts: 2, .. }),
        "{reply:?}"
    );

    // The same connection, the same session, a second write stream — refused.
    let (header, payload) = writer.write_block(StreamId(2), &["c.py"]);
    assert_eq!(header.kind, FrameKind::ERROR);
    assert_eq!(error_of(&payload).0, ErrorCode::ModeRefused);

    // ...and the refusal was a refusal, not a partial write: the sealed database holds
    // what it held when it was sealed, and its recorded count still describes it.
    let (mut reader, _, _) = Client::hello(&serving, "code", Mode::ReadOnly);
    assert_eq!(reader.count(StreamId(1), "X where src.File X"), 2);
}

/// **`ops-I6` is about the whole session, not about facts.** A read-only session does
/// not get to create, seal or delete a database by asking on a different frame kind.
#[test]
fn a_read_only_session_cannot_change_the_lifecycle() {
    let serving = start();

    // Made by a session that may, so there is something to try to destroy.
    let mut allowed = Client::control_session(&serving, Mode::ReadWrite);
    allowed.control(ControlOp::Create, "code", false);

    let mut reader = Client::control_session(&serving, Mode::ReadOnly);

    for op in [ControlOp::Create, ControlOp::Finish, ControlOp::Remove] {
        let name = if op == ControlOp::Create {
            "other"
        } else {
            "code"
        };
        let (header, payload) = reader.control_raw(op, name, true);

        assert_eq!(header.kind, FrameKind::ERROR, "{op:?} should be refused");
        assert_eq!(error_of(&payload).0, ErrorCode::ModeRefused);
    }

    // Nothing happened: no second database, and the first is untouched.
    assert!(
        serving
            .catalog()
            .find("other")
            .expect("the root reads")
            .is_none()
    );
    assert!(
        serving
            .catalog()
            .get("code")
            .expect("still there")
            .status()
            .is_writable()
    );
}

/// A database a session still holds is **refused by name**, not pulled out from under
/// it. `remove` closes the store, and a query running against a closed store is a
/// fault the client did not cause.
#[test]
fn removing_a_database_a_session_holds_is_refused() {
    let serving = start();
    let mut control = Client::control_session(&serving, Mode::ReadWrite);
    control.control(ControlOp::Create, "code", false);

    let (mut holder, header, _) = Client::hello(&serving, "code", Mode::ReadOnly);
    assert_eq!(header.kind, kinds::READY);

    let (header, payload) = control.control_raw(ControlOp::Remove, "code", false);
    assert_eq!(header.kind, FrameKind::ERROR);

    let (code, message) = error_of(&payload);
    assert_eq!(code, ErrorCode::InUse);
    assert!(message.contains("code"), "{message}");

    // Refused, and *nothing else*: the holder's session is still serving.
    assert_eq!(holder.count(StreamId(1), "X where src.File X"), 0);
    assert!(
        serving
            .catalog()
            .find("code")
            .expect("the root reads")
            .is_some()
    );

    drop(holder);

    // The refusal was contention, not a state: it ends when the session does.
    eventually("the session lets go of `code`", || {
        matches!(
            control.control_raw(ControlOp::Remove, "code", false).0.kind,
            kinds::CONTROL_REPLY
        )
    });

    assert!(
        serving
            .catalog()
            .find("code")
            .expect("the root reads")
            .is_none()
    );
}

/// A control session is bound to no database, and says so rather than guessing at one.
#[test]
fn a_control_session_has_no_database_to_query() {
    let serving = start();
    let mut control = Client::control_session(&serving, Mode::ReadWrite);
    control.control(ControlOp::Create, "code", false);

    control.send(kinds::QUERY, StreamId(2), b"X where src.File X");
    let (header, payload) = control.recv();

    assert_eq!(header.kind, FrameKind::ERROR);
    assert_eq!(error_of(&payload).0, ErrorCode::UnknownDatabase);

    // Naming one that does not exist is the other half of the same rule: a session
    // binds a database or it binds none, and never something almost right.
    let (_client, header, payload) = Client::hello(&serving, "nope", Mode::ReadOnly);
    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, message) = error_of(&payload);
    assert_eq!(code, ErrorCode::UnknownDatabase);
    assert!(message.contains("nope"), "{message}");
}

/// A lifecycle request the store declines comes back as a **refusal**, with the reason
/// in it — not as `Internal`, which would send someone to the server's logs to read a
/// message already in their hand.
#[test]
fn a_declined_request_says_why() {
    let serving = start();
    let mut control = Client::control_session(&serving, Mode::ReadWrite);
    control.control(ControlOp::Create, "code", false);

    let (header, payload) = control.control_raw(ControlOp::Create, "code", false);
    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, message) = error_of(&payload);
    assert_eq!(code, ErrorCode::Refused);
    assert!(message.contains("already exists"), "{message}");

    // An empty database will not seal without being told to, over the wire exactly as
    // it will not offline — a silently-empty sealed artifact is the same CI failure
    // whichever door it came through.
    let (header, payload) = control.control_raw(ControlOp::Finish, "code", false);
    assert_eq!(header.kind, FrameKind::ERROR);
    let (code, message) = error_of(&payload);
    assert_eq!(code, ErrorCode::Refused);
    assert!(message.contains("--allow-zero-facts"), "{message}");

    // ...and does when it is.
    let reply = control.control(ControlOp::Finish, "code", true);
    assert!(
        matches!(reply, ControlReply::Finished { facts: 0, .. }),
        "{reply:?}"
    );

    // Sealing again is the same no-op it is offline, with the notice a client needs to
    // tell "I sealed it" from "it was already sealed".
    let reply = control.control(ControlOp::Finish, "code", true);
    assert!(
        matches!(
            reply,
            ControlReply::Finished {
                already_complete: true,
                ..
            }
        ),
        "{reply:?}"
    );

    let (header, payload) = control.control_raw(ControlOp::Remove, "nope", false);
    assert_eq!(header.kind, FrameKind::ERROR);
    assert_eq!(error_of(&payload).0, ErrorCode::UnknownDatabase);
}
