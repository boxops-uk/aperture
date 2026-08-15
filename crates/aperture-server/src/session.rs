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
//! interleaved — but the server awaits each frame's work before reading the next,
//! rather than running streams on separate tasks. §5's "per-connection single writer
//! task that fairly interleaves ready streams" is what makes a long query stop
//! starving a short one, and it is a scheduling change on top of this loop rather
//! than a different loop.
//!
//! Deferred with it, and named in §5 as deferred: per-stream flow-control windows,
//! in-band cancellation, and chunked incremental flushing of a long result.
//!
//! # Where the blocking work goes, and why that is the whole point of the port
//!
//! **fjall is synchronous and the executor is CPU-bound**, so neither belongs on the
//! reactor: a query that scans a million rows would stall every other connection the
//! thread happened to be driving. Every call that touches a store — ingesting a
//! block, compiling and running a query — is moved to
//! [`spawn_blocking`](tokio::task::spawn_blocking), and what stays here is framing and
//! scheduling.
//!
//! That cut is what 9d-ii builds on. Once the engine is off the reactor, the reactor
//! is free to interleave streams, flush a result in chunks, and notice a cancel — none
//! of which is possible while a query owns the thread that would have to do them.
//!
//! # A write stream is a state, and that is the only state a connection has
//!
//! `OPEN_WRITE` puts a stream id into [`Session::writing`]; `COPY_DATA` on an id that
//! is not there is a protocol fault rather than an implicit open. That matters more
//! than it looks: an implicit open would mean a client that mistyped a stream id
//! silently started a *second* write stream, and the counts it got back would be for
//! a stream it did not think it had.

use std::{collections::HashMap, sync::Arc};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use aperture_engine::{
    compile::Compilation,
    iter::{Executor, Iteratee, Stream},
};
use aperture_ingest::{Ingested, intern_block};
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
pub async fn serve<R, W>(
    reader: R,
    writer: W,
    databases: &[Arc<Database>],
) -> Result<(), ServerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let mut session = match handshake(&mut reader, &mut writer, databases).await {
        Ok(session) => session,
        Err(error) => {
            // A failed handshake is answered and then the connection ends: there is
            // no session to keep, and pretending otherwise leaves a client waiting
            // for a `Ready` that is never coming.
            let _ = send_error(&mut writer, StreamId(0), &error).await;
            return if error.is_fatal() { Err(error) } else { Ok(()) };
        }
    };

    loop {
        let Some((header, payload)) = read_frame(&mut reader).await? else {
            return Ok(());
        };

        match dispatch(&mut session, &mut writer, &header, &payload).await {
            Ok(()) => {}
            Err(error) if error.is_fatal() => {
                let _ = send_error(&mut writer, header.stream, &error).await;
                let _ = writer.flush().await;
                return Err(error);
            }
            Err(error) => {
                // A stream-level fault: report it on its own stream, forget any
                // half-finished state for that stream, and keep the connection.
                session.writing.remove(&header.stream.0);
                send_error(&mut writer, header.stream, &error).await?;
                writer.flush().await?;
            }
        }
    }
}

async fn handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    databases: &[Arc<Database>],
) -> Result<Session, ServerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Some((header, payload)) = read_frame(reader).await? else {
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
    )
    .await?;
    writer.flush().await?;

    Ok(Session {
        database: Arc::clone(database),
        mode: startup.mode,
        writing: HashMap::new(),
    })
}

async fn dispatch<W: AsyncWrite + Unpin>(
    session: &mut Session,
    writer: &mut W,
    header: &FrameHeader,
    payload: &[u8],
) -> Result<(), ServerError> {
    match header.kind {
        kinds::OPEN_WRITE => open_write(session, writer, header.stream).await,
        FrameKind::COPY_DATA => copy_data(session, header.stream, payload).await,
        FrameKind::COPY_DONE => copy_done(session, writer, header.stream).await,
        kinds::QUERY => query(session, writer, header.stream, payload).await,

        kinds::STARTUP => Err(ServerError::Protocol(
            "a second startup frame on an open session".to_owned(),
        )),

        other => Err(ServerError::Protocol(format!(
            "no handler for frame kind `{other}`"
        ))),
    }
}

async fn open_write<W: AsyncWrite + Unpin>(
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
    send(writer, FrameKind::COPY_IN_RESPONSE, stream, &[]).await?;
    writer.flush().await?;
    Ok(())
}

async fn copy_data(
    session: &mut Session,
    stream: StreamId,
    payload: &[u8],
) -> Result<(), ServerError> {
    if !session.writing.contains_key(&stream.0) {
        return Err(ServerError::Protocol(format!(
            "stream {} carries fact blocks but was never opened for writing",
            stream.0
        )));
    }

    // Off the reactor: this writes to fjall, which is synchronous, and a block of a
    // million facts would otherwise stall every connection this thread drives.
    let database = Arc::clone(&session.database);
    let block = payload.to_vec();

    let out: Ingested = blocking(move || {
        intern_block(database.db.as_ref(), &database.schema, &block).map_err(ServerError::from)
    })
    .await?;

    let writing = session
        .writing
        .get_mut(&stream.0)
        .expect("checked just above");

    writing.created += out.created as u64;
    writing.deduped += out.deduped as u64;

    Ok(())
}

async fn copy_done<W: AsyncWrite + Unpin>(
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
    )
    .await?;
    writer.flush().await?;
    Ok(())
}

/// A query's answer, computed off the reactor.
///
/// Encoded bytes rather than values, so that nothing the engine owns crosses back:
/// the async side writes frames and knows nothing about a `Plan`, an `Executor` or a
/// `Value`.
struct Answer {
    descriptor: Vec<u8>,
    rows: Vec<Vec<u8>>,
}

async fn query<W: AsyncWrite + Unpin>(
    session: &mut Session,
    writer: &mut W,
    stream: StreamId,
    payload: &[u8],
) -> Result<(), ServerError> {
    let source = std::str::from_utf8(payload)
        .map_err(|_| ServerError::Protocol("a query that is not UTF-8".to_owned()))?
        .to_owned();

    let database = Arc::clone(&session.database);
    let answer = blocking(move || run_query(&database, &source)).await?;

    // The descriptor goes first: a client needs it to decode anything, and sending it
    // before the rows is also what will let a long query show its shape before its
    // first row once results are chunked.
    send(
        writer,
        FrameKind::ROW_DESCRIPTION,
        stream,
        &answer.descriptor,
    )
    .await?;

    for row in &answer.rows {
        send(writer, FrameKind::DATA_ROW, stream, row).await?;
    }

    send(
        writer,
        kinds::COMPLETE,
        stream,
        &protocol::encode_complete(answer.rows.len() as u64, 0),
    )
    .await?;
    writer.flush().await?;
    Ok(())
}

/// Compile and run, entirely on a blocking thread.
///
/// Everything the engine touches lives inside this function, which is what keeps the
/// reactor free of it.
fn run_query(database: &Database, source: &str) -> Result<Answer, ServerError> {
    let schema = &database.schema;

    let mut compilation = Compilation::new(source, schema);
    let plan = compilation.plan();

    if compilation.diagnostics().has_errors() {
        return Err(ServerError::BadQuery(compilation.render_to_string()));
    }

    let head = compilation
        .head_ty()
        .ok_or_else(|| ServerError::BadQuery("this query has no head type".to_owned()))?;

    let (desc, row_ty, row_interner) = rows::row_shape(schema, head, compilation.interner())?;

    let Some(plan) = plan else {
        return Err(ServerError::BadQuery(
            "no plan, and no diagnostic saying why — that is a compiler bug".to_owned(),
        ));
    };

    // Collected rather than streamed, which is the deferral §5 names: the executor
    // already suspends — `enumerate` returns `Suspended` — so what is missing is the
    // loop that resumes it between chunks, not the machinery under it.
    let collected = Executor::new(database.db.reader(), plan)
        .enumerate(
            Vec::new(),
            |mut acc, mut row| {
                acc.push(row.to_value(compilation.interner())?);
                Ok(Stream::Continue(acc))
            },
            &CancellationToken::new(),
        )
        .map_err(|error| ServerError::Execution(error.to_string()))?;

    let (Iteratee::Done(values) | Iteratee::Suspended(values, _)) = collected;

    let mut descriptor = vec![];
    encode_desc(&mut descriptor, &desc);

    let mut encoded = Vec::with_capacity(values.len());
    for value in &values {
        let wire = rows::to_wire(&row_ty, value, &row_interner)?;

        let mut buffer = vec![];
        encode_value(&mut buffer, schema, &row_ty, &wire)?;
        encoded.push(buffer);
    }

    Ok(Answer {
        descriptor,
        rows: encoded,
    })
}

/// Run `work` on the blocking pool.
///
/// A panic in the work reaches here as a join error rather than unwinding the
/// connection, so a bug in one query fails that stream instead of taking the server
/// with it.
async fn blocking<T, F>(work: F) -> Result<T, ServerError>
where
    F: FnOnce() -> Result<T, ServerError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(join) => Err(ServerError::Execution(format!(
            "a blocking task did not finish: {join}"
        ))),
    }
}

// ---- frame plumbing ---------------------------------------------------------

async fn send<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: FrameKind,
    stream: StreamId,
    payload: &[u8],
) -> Result<(), ServerError> {
    let mut out = Vec::with_capacity(frame::HEADER_LEN + payload.len());
    encode_frame(&mut out, kind, stream, payload)?;
    writer.write_all(&out).await?;
    Ok(())
}

async fn send_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    stream: StreamId,
    error: &ServerError,
) -> Result<(), ServerError> {
    let payload = protocol::encode_error(error.code(), &error.to_string());
    send(writer, FrameKind::ERROR, stream, &payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one frame, or `None` at a clean end of stream.
///
/// The header comes first and alone, which is the whole reason its length is
/// fixed-width and up front: nine bytes say how many more to await, so a reader never
/// guesses and never over-reads into the next frame.
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<(FrameHeader, Vec<u8>)>, ServerError> {
    let mut head = [0u8; frame::HEADER_LEN];

    if !read_full(reader, &mut head).await? {
        return Ok(None);
    }

    let header = frame::decode_header(&head)?;
    let mut payload = vec![0u8; header.length as usize];

    if !read_full(reader, &mut payload).await? {
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
async fn read_full<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
) -> Result<bool, ServerError> {
    let mut filled = 0;

    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]).await? {
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
