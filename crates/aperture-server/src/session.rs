//! One connection, from handshake to close.
//!
//! # What this is, and what it is not yet
//!
//! It is the frame loop [operations §5 `serve`](../../../docs/aperture-cli-design.md)
//! describes: a PG-shaped handshake, then framed messages tagged by stream, with a
//! write stream and a query stream living on one connection at once.
//!
//! It is **not** concurrent between streams yet. Frames carry a `stream` and the
//! server honours it — two streams can be open, and their frames may arrive
//! interleaved — but the server processes each frame to completion as it arrives
//! rather than running streams on separate tasks. §5's "per-connection single writer
//! task that fairly interleaves ready streams" is what makes a long query stop
//! starving a short one, and it is a scheduling change on top of this loop rather
//! than a different loop. Phase 7a's criterion is that facts are writable over a
//! socket and queried back on the same connection; fairness is §5's, and saying so
//! here is better than implying it works.
//!
//! Deferred with it, and named in §5 as deferred: per-stream flow-control windows,
//! in-band cancellation, and chunked incremental flushing of a long result.
//!
//! # A write stream is a state, and that is the only state a connection has
//!
//! `OPEN_WRITE` puts a stream id into [`Session::writing`]; `COPY_DATA` on an id that
//! is not there is a protocol fault rather than an implicit open. That matters more
//! than it looks: an implicit open would mean a client that mistyped a stream id
//! silently started a *second* write stream, and the counts it got back would be for
//! a stream it did not think it had.

use std::{
    collections::HashMap,
    io::{BufReader, BufWriter, Read, Write},
    sync::Arc,
};

use aperture_engine::{
    compile::Compilation,
    iter::{Executor, Iteratee, Stream},
    plan::Plan,
};
use aperture_ingest::intern_block;
use aperture_schema::schema::Schema;
use aperture_store::store::FjallDb;
use aperture_wire::{
    FrameHeader, FrameKind, StreamId, encode_desc, encode_frame, frame, value::encode_value,
};
use tokio_util::sync::CancellationToken;

use crate::{
    error::ServerError,
    protocol::{self, ErrorCode, Mode, Ready, Startup, kinds},
    rows,
};

/// The database a session is bound to, and everything needed to serve it.
///
/// `Arc` because every connection shares one open store — `ops-I1`'s single-process
/// ownership means there is exactly one, and a second `FjallDb::open` on a held
/// directory is the lock fight the design refuses.
pub struct Database {
    pub name: String,
    pub db: Arc<FjallDb>,
    pub schema: Arc<Schema>,
    /// See [`protocol::provisional_fingerprint`] — replaced when Phase 8 lands.
    pub fingerprint: u64,
}

impl Database {
    #[must_use]
    pub fn new(name: impl Into<String>, db: FjallDb, schema: Schema) -> Database {
        let fingerprint = protocol::provisional_fingerprint(&schema);
        Database {
            name: name.into(),
            db: Arc::new(db),
            schema: Arc::new(schema),
            fingerprint,
        }
    }
}

/// What a write stream has accumulated so far.
#[derive(Debug, Default)]
struct Writing {
    created: u64,
    deduped: u64,
}

struct Session {
    database: Arc<Database>,
    mode: Mode,
    writing: HashMap<u32, Writing>,
}

/// Serve one connection to completion.
///
/// # Errors
///
/// Only fatal faults escape: an I/O failure, or a peer whose frames no longer parse.
/// Everything else is answered with an error frame on the stream that caused it and
/// the connection carries on.
pub fn serve<R: Read, W: Write>(
    reader: R,
    writer: W,
    databases: &[Arc<Database>],
) -> Result<(), ServerError> {
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let database = match handshake(&mut reader, &mut writer, databases) {
        Ok(session) => session,
        Err(error) => {
            // A failed handshake is answered and then the connection ends: there is
            // no session to keep, and pretending otherwise leaves a client waiting
            // for a `Ready` that is never coming.
            let _ = send_error(&mut writer, StreamId(0), &error);
            return if error.is_fatal() { Err(error) } else { Ok(()) };
        }
    };

    let mut session = database;

    loop {
        let Some((header, payload)) = read_frame(&mut reader)? else {
            return Ok(());
        };

        match dispatch(&mut session, &mut writer, &header, &payload) {
            Ok(()) => {}
            Err(error) if error.is_fatal() => {
                let _ = send_error(&mut writer, header.stream, &error);
                let _ = writer.flush();
                return Err(error);
            }
            Err(error) => {
                // A stream-level fault: report it on its own stream, forget any
                // half-finished state for that stream, and keep the connection.
                session.writing.remove(&header.stream.0);
                send_error(&mut writer, header.stream, &error)?;
                writer.flush()?;
            }
        }
    }
}

fn handshake<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    databases: &[Arc<Database>],
) -> Result<Session, ServerError> {
    let Some((header, payload)) = read_frame(reader)? else {
        return Err(ServerError::Protocol(
            "the connection closed before a startup frame".to_owned(),
        ));
    };

    if header.kind != kinds::STARTUP {
        return Err(ServerError::Protocol(format!(
            "expected a startup frame, got `{}`",
            header.kind
        )));
    }

    let startup: Startup = protocol::decode_startup(&payload)?;

    if startup.version != protocol::VERSION {
        return Err(ServerError::Protocol(format!(
            "this server speaks protocol version {}, the client speaks {}",
            protocol::VERSION,
            startup.version
        )));
    }

    let database = databases
        .iter()
        .find(|candidate| candidate.name == startup.database)
        .ok_or_else(|| ServerError::UnknownDatabase(startup.database.clone()))?;

    // Zero means "do not check" — a reader, or a client written against whatever the
    // server has. A non-zero value is a claim, and a wrong claim is refused here
    // rather than after a block of facts nobody can read back.
    if startup.schema_fingerprint != 0 && startup.schema_fingerprint != database.fingerprint {
        return Err(ServerError::SchemaMismatch {
            expected: startup.schema_fingerprint,
            actual: database.fingerprint,
        });
    }

    send(
        writer,
        kinds::READY,
        StreamId(0),
        &protocol::encode_ready(&Ready {
            version: protocol::VERSION,
            schema_fingerprint: database.fingerprint,
            predicates: database.schema.len() as u64,
        }),
    )?;
    writer.flush()?;

    Ok(Session {
        database: Arc::clone(database),
        mode: startup.mode,
        writing: HashMap::new(),
    })
}

fn dispatch<W: Write>(
    session: &mut Session,
    writer: &mut W,
    header: &FrameHeader,
    payload: &[u8],
) -> Result<(), ServerError> {
    match header.kind {
        kinds::OPEN_WRITE => open_write(session, writer, header.stream),
        FrameKind::COPY_DATA => copy_data(session, header.stream, payload),
        FrameKind::COPY_DONE => copy_done(session, writer, header.stream),
        kinds::QUERY => query(session, writer, header.stream, payload),

        kinds::STARTUP => Err(ServerError::Protocol(
            "a second startup frame on an open session".to_owned(),
        )),

        other => Err(ServerError::Protocol(format!(
            "no handler for frame kind `{other}`"
        ))),
    }
}

fn open_write<W: Write>(
    session: &mut Session,
    writer: &mut W,
    stream: StreamId,
) -> Result<(), ServerError> {
    if session.mode != Mode::ReadWrite {
        return Err(ServerError::ModeRefused);
    }

    if session.writing.contains_key(&stream.0) {
        return Err(ServerError::Protocol(format!(
            "stream {} is already a write stream",
            stream.0
        )));
    }

    session.writing.insert(stream.0, Writing::default());
    send(writer, FrameKind::COPY_IN_RESPONSE, stream, &[])?;
    writer.flush()?;
    Ok(())
}

fn copy_data(session: &mut Session, stream: StreamId, payload: &[u8]) -> Result<(), ServerError> {
    if !session.writing.contains_key(&stream.0) {
        return Err(ServerError::Protocol(format!(
            "stream {} carries fact blocks but was never opened for writing",
            stream.0
        )));
    }

    // Ingest before touching the counters, so a failed block leaves them alone. The
    // facts it wrote before failing are still written — interning is not a
    // transaction, which `aperture-ingest` records and §6 defers.
    let out = intern_block(
        session.database.db.as_ref(),
        &session.database.schema,
        payload,
    )?;

    let writing = session
        .writing
        .get_mut(&stream.0)
        .expect("checked just above");

    writing.created += out.created as u64;
    writing.deduped += out.deduped as u64;

    Ok(())
}

fn copy_done<W: Write>(
    session: &mut Session,
    writer: &mut W,
    stream: StreamId,
) -> Result<(), ServerError> {
    let writing = session.writing.remove(&stream.0).ok_or_else(|| {
        ServerError::Protocol(format!(
            "stream {} was closed for writing but never opened",
            stream.0
        ))
    })?;

    send(
        writer,
        kinds::COMPLETE,
        stream,
        &protocol::encode_complete(writing.created, writing.deduped),
    )?;
    writer.flush()?;
    Ok(())
}

fn query<W: Write>(
    session: &mut Session,
    writer: &mut W,
    stream: StreamId,
    payload: &[u8],
) -> Result<(), ServerError> {
    let source = std::str::from_utf8(payload)
        .map_err(|_| ServerError::Protocol("a query that is not UTF-8".to_owned()))?;

    let schema = Arc::clone(&session.database.schema);
    let mut compilation = Compilation::new(source, &schema);
    let plan = compilation.plan();

    if compilation.diagnostics().has_errors() {
        return Err(ServerError::BadQuery(compilation.render_to_string()));
    }

    let head = compilation
        .head_ty()
        .ok_or_else(|| ServerError::BadQuery("this query has no head type".to_owned()))?;

    let (desc, row_ty, row_interner) = rows::row_shape(&schema, head, compilation.interner())?;

    let Some(plan) = plan else {
        return Err(ServerError::BadQuery(
            "no plan, and no diagnostic saying why — that is a compiler bug".to_owned(),
        ));
    };

    // The descriptor goes first, and it goes before the rows are computed: a client
    // needs it to decode anything, and sending it early is also what lets a long
    // query show its shape before its first row.
    let mut described = vec![];
    encode_desc(&mut described, &desc);
    send(writer, FrameKind::ROW_DESCRIPTION, stream, &described)?;

    let count = send_rows(
        session,
        writer,
        stream,
        plan,
        &row_ty,
        &row_interner,
        compilation.interner(),
    )?;

    send(
        writer,
        kinds::COMPLETE,
        stream,
        &protocol::encode_complete(count, 0),
    )?;
    writer.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_rows<W: Write>(
    session: &Session,
    writer: &mut W,
    stream: StreamId,
    plan: Plan,
    row_ty: &aperture_schema::schema::PredicateTy,
    row_interner: &aperture_schema::schema::LocalInterner,
    query_interner: &aperture_schema::schema::LocalInterner,
) -> Result<u64, ServerError> {
    // Collected rather than streamed, and that is the deferral §5 names: chunked
    // incremental flushing wants the iteratee to yield between chunks, which is a
    // scheduling change rather than a codec one. The executor already supports it —
    // `enumerate` returns `Suspended` — so what is missing is the loop above, not the
    // machinery below.
    let collected = Executor::new(session.database.db.reader(), plan)
        .enumerate(
            Vec::new(),
            |mut acc, mut row| {
                acc.push(row.to_value(query_interner)?);
                Ok(Stream::Continue(acc))
            },
            &CancellationToken::new(),
        )
        .map_err(|error| ServerError::Execution(error.to_string()))?;

    let (Iteratee::Done(values) | Iteratee::Suspended(values, _)) = collected;

    let mut count = 0;
    let mut buffer = vec![];

    for value in &values {
        let wire = rows::to_wire(row_ty, value, row_interner)?;

        buffer.clear();
        encode_value(&mut buffer, &session.database.schema, row_ty, &wire)?;
        send(writer, FrameKind::DATA_ROW, stream, &buffer)?;
        count += 1;
    }

    Ok(count)
}

// ---- frame plumbing ---------------------------------------------------------

fn send<W: Write>(
    writer: &mut W,
    kind: FrameKind,
    stream: StreamId,
    payload: &[u8],
) -> Result<(), ServerError> {
    let mut out = Vec::with_capacity(frame::HEADER_LEN + payload.len());
    encode_frame(&mut out, kind, stream, payload)?;
    writer.write_all(&out)?;
    Ok(())
}

fn send_error<W: Write>(
    writer: &mut W,
    stream: StreamId,
    error: &ServerError,
) -> Result<(), ServerError> {
    let payload = protocol::encode_error(error.code(), &error.to_string());
    send(writer, FrameKind::ERROR, stream, &payload)?;
    writer.flush()?;
    Ok(())
}

/// Read one frame, or `None` at a clean end of stream.
///
/// The header comes first and alone, which is the whole reason its length is
/// fixed-width and up front: nine bytes say how many more to await, so a reader never
/// guesses and never over-reads into the next frame.
fn read_frame<R: Read>(reader: &mut R) -> Result<Option<(FrameHeader, Vec<u8>)>, ServerError> {
    let mut head = [0u8; frame::HEADER_LEN];

    if !read_full(reader, &mut head)? {
        return Ok(None);
    }

    let header = frame::decode_header(&head)?;
    let mut payload = vec![0u8; header.length as usize];

    if !read_full(reader, &mut payload)? {
        return Err(ServerError::Protocol(
            "the connection closed between a frame header and its payload".to_owned(),
        ));
    }

    Ok(Some((header, payload)))
}

/// Fill `buffer`; `false` at a **clean** end of stream, an error partway through one.
///
/// `read_exact` cannot tell "the peer hung up politely" from "the peer hung up in the
/// middle of a message", and those are different events: one ends a connection
/// normally and the other is a fault worth reporting. An empty buffer is trivially
/// filled, which is what makes a zero-length payload — `COPY_DONE`, `COPY_IN_RESPONSE`
/// — read the same way as any other.
fn read_full<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<bool, ServerError> {
    let mut filled = 0;

    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => {
                return Err(ServerError::Protocol(format!(
                    "the connection closed {filled} bytes into a {}-byte read",
                    buffer.len()
                )));
            }
            n => filled += n,
        }
    }

    Ok(true)
}

/// The error code a client sees for a given fault — exposed for tests, which is the
/// only way to check the mapping without a socket.
#[must_use]
pub fn code_of(error: &ServerError) -> ErrorCode {
    error.code()
}
