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

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::{Mutex, mpsc},
};

use aperture_encoding::tuple::Value;
use aperture_engine::{
    compile::Compilation,
    iter::{Cursor, Executor, Iteratee, Stream},
    plan::Plan,
};
use aperture_ingest::{Ingested, intern_block};
use aperture_schema::schema::{LocalInterner, PredicateTy, Schema};
use aperture_store::store::FjallDb;
use aperture_wire::{
    FrameHeader, FrameKind, StreamId, encode_desc, encode_frame, frame, value::encode_value,
};
use tokio_util::sync::CancellationToken;

use crate::{
    error::ServerError,
    outbound::{Outbound, run as outbound_run},
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
    /// **The per-database single writer** (`ops-I1`, `ops-I5`).
    ///
    /// Held across an ingest, so writes to one database are serialised however many
    /// connections and streams are writing. That is not caution: fjall's
    /// non-transactional path loses updates on a concurrent read-modify-write, and
    /// interning *is* one — look the key up, write it if it is not there. `ops-I1`
    /// gives one server the database and this gives one writer the server's half, so
    /// serialisation is free rather than a cost, and the transactional keyspace is
    /// unnecessary.
    ///
    /// Reads take nothing: they run against an immutable snapshot.
    pub writer: Mutex<()>,
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
            writer: Mutex::new(()),
        }
    }
}

/// What a write stream has accumulated so far.
#[derive(Debug, Default)]
struct Writing {
    created: u64,
    deduped: u64,
}

/// What a connection knows, once the handshake has settled it.
///
/// Immutable and shared: the mode is resolved **once** at establishment (`ops-I6`),
/// and per-stream state lives in the stream's own task, which is what lets a write
/// stream's counters need no lock.
struct Session {
    database: Arc<Database>,
    mode: Mode,
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
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let session = match handshake(&mut reader, &mut writer, databases).await {
        Ok(session) => session,
        Err(error) => {
            // A failed handshake is answered and then the connection ends: there is
            // no session to keep, and pretending otherwise leaves a client waiting
            // for a `Ready` that is never coming.
            let _ = send_direct(&mut writer, StreamId(0), &error).await;
            let _ = writer.flush().await;
            return if error.is_fatal() { Err(error) } else { Ok(()) };
        }
    };

    let session = Arc::new(session);
    let outbound = Arc::new(Outbound::new());

    // The one task that writes. Everything else queues.
    let pump = {
        let outbound = Arc::clone(&outbound);
        tokio::spawn(async move { outbound_run(&outbound, &mut writer).await })
    };

    let result = read_loop(&mut reader, &session, &outbound).await;

    // Drain what streams have already produced before stopping: a frame a stream
    // believed it had sent must not vanish because the reader hit EOF.
    outbound.close().await;
    let _ = pump.await;

    result
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
    })
}

/// Read frames and route each to its stream, forever.
///
/// **This loop never does a stream's work.** It reads, routes, and goes back to
/// reading — which is what makes a long query on one stream not delay a short one on
/// another, and is the whole difference from the loop it replaces.
async fn read_loop<R: AsyncRead + Unpin>(
    reader: &mut R,
    session: &Arc<Session>,
    outbound: &Arc<Outbound>,
) -> Result<(), ServerError> {
    let mut streams: HashMap<u32, StreamHandle> = HashMap::new();

    loop {
        let Some((header, payload)) = read_frame(reader).await? else {
            return Ok(());
        };

        // Cancellation is handled *here* rather than in the stream, because a stream
        // busy inside a scan is exactly the one that cannot be listening.
        if header.kind == kinds::CANCEL {
            if let Some(handle) = streams.get(&header.stream.0) {
                handle.cancel.cancel();
            }
            continue;
        }

        // A second startup is a protocol fault of the connection rather than of a
        // stream, so it stops everything.
        if header.kind == kinds::STARTUP {
            let error =
                ServerError::Protocol("a second startup frame on an open session".to_owned());
            let _ = outbound
                .send(
                    FrameKind::ERROR,
                    header.stream,
                    &protocol::encode_error(error.code(), &error.to_string()),
                )
                .await;
            return Err(error);
        }

        let handle = streams
            .entry(header.stream.0)
            .or_insert_with(|| StreamHandle::spawn(header.stream, session, outbound));

        // A stream whose task has ended — it completed, or it failed — is started
        // again rather than silently dropping the frame.
        if let Err(returned) = handle.inbound.send((header, payload)).await {
            let handle = StreamHandle::spawn(header.stream, session, outbound);
            let _ = handle.inbound.send(returned.0).await;
            streams.insert(header.stream.0, handle);
        }
    }
}

/// One stream's task, and the way to reach it.
struct StreamHandle {
    inbound: mpsc::Sender<(FrameHeader, Vec<u8>)>,
    /// Cancelling this stops the stream's current work — and only this stream's.
    cancel: CancellationToken,
}

impl StreamHandle {
    fn spawn(stream: StreamId, session: &Arc<Session>, outbound: &Arc<Outbound>) -> StreamHandle {
        // Bounded, so a client that floods one stream is made to wait on that stream
        // rather than filling memory. One in flight plus one queued is enough: the
        // work is the slow part, not the routing.
        let (inbound, mut receiver) = mpsc::channel::<(FrameHeader, Vec<u8>)>(2);
        let cancel = CancellationToken::new();

        let task = StreamTask {
            stream,
            session: Arc::clone(session),
            outbound: Arc::clone(outbound),
            cancel: cancel.clone(),
            writing: None,
        };

        tokio::spawn(async move {
            let mut task = task;

            while let Some((header, payload)) = receiver.recv().await {
                if let Err(error) = task.handle(&header, &payload).await {
                    let _ = task
                        .outbound
                        .send(
                            FrameKind::ERROR,
                            stream,
                            &protocol::encode_error(error.code(), &error.to_string()),
                        )
                        .await;

                    // A stream-level fault ends the stream, not the connection. The
                    // reader starts a fresh task if the client uses the id again.
                    return;
                }
            }
        });

        StreamHandle { inbound, cancel }
    }
}

/// The state one stream carries.
///
/// Per-stream rather than shared, which is what makes a write stream's counters need
/// no lock: exactly one task ever touches them.
struct StreamTask {
    stream: StreamId,
    session: Arc<Session>,
    outbound: Arc<Outbound>,
    cancel: CancellationToken,
    /// `Some` once this stream is a write stream.
    writing: Option<Writing>,
}

impl StreamTask {
    async fn handle(&mut self, header: &FrameHeader, payload: &[u8]) -> Result<(), ServerError> {
        match header.kind {
            kinds::OPEN_WRITE => self.open_write().await,
            FrameKind::COPY_DATA => self.copy_data(payload).await,
            FrameKind::COPY_DONE => self.copy_done().await,
            kinds::QUERY => self.query(payload).await,

            other => Err(ServerError::Protocol(format!(
                "no handler for frame kind `{other}`"
            ))),
        }
    }

    async fn open_write(&mut self) -> Result<(), ServerError> {
        if self.session.mode != Mode::ReadWrite {
            return Err(ServerError::ModeRefused);
        }

        if self.writing.is_some() {
            return Err(ServerError::Protocol(format!(
                "stream {} is already a write stream",
                self.stream.0
            )));
        }

        self.writing = Some(Writing::default());
        self.outbound
            .send(FrameKind::COPY_IN_RESPONSE, self.stream, &[])
            .await
    }

    async fn copy_data(&mut self, payload: &[u8]) -> Result<(), ServerError> {
        if self.writing.is_none() {
            return Err(ServerError::Protocol(format!(
                "stream {} carries fact blocks but was never opened for writing",
                self.stream.0
            )));
        }

        let database = Arc::clone(&self.session.database);
        let working = Arc::clone(&database);
        let block = payload.to_vec();

        // Serialised per database, not merely per stream: fjall's non-transactional
        // path loses updates on a concurrent read-modify-write, and interning is
        // exactly that — look the key up, write it if absent. `ops-I1` gives one
        // server the database, and this gives one writer the server's half of it.
        let out: Ingested = {
            let _writing = database.writer.lock().await;
            blocking(move || {
                intern_block(working.db.as_ref(), &working.schema, &block)
                    .map_err(ServerError::from)
            })
            .await?
        };

        let writing = self.writing.as_mut().expect("checked just above");
        writing.created += out.created as u64;
        writing.deduped += out.deduped as u64;

        Ok(())
    }

    async fn copy_done(&mut self) -> Result<(), ServerError> {
        let writing = self.writing.take().ok_or_else(|| {
            ServerError::Protocol(format!(
                "stream {} was closed for writing but never opened",
                self.stream.0
            ))
        })?;

        self.outbound
            .send(
                kinds::COMPLETE,
                self.stream,
                &protocol::encode_complete(writing.created, writing.deduped),
            )
            .await
    }

    /// Run a query, **sending rows as they are found**.
    ///
    /// The loop is the point. Each turn computes at most [`CHUNK_ROWS`] rows on a
    /// blocking thread and hands back a [`Cursor`] if there are more; the rows go out
    /// while the next chunk is computed. A result of any size therefore never buffers
    /// in the server and never monopolises the socket — and between chunks is exactly
    /// where a cancel gets its chance to land.
    ///
    /// It is also the first thing in this project to *use* resume for what it is for
    /// rather than to test it: the cursor here is the same bytes-only token
    /// [chapter 5](../../../docs/05-resume.md) is about.
    async fn query(&mut self, payload: &[u8]) -> Result<(), ServerError> {
        let source = std::str::from_utf8(payload)
            .map_err(|_| ServerError::Protocol("a query that is not UTF-8".to_owned()))?
            .to_owned();

        let database = Arc::clone(&self.session.database);
        let prepared = blocking({
            let database = Arc::clone(&database);
            let source = source.clone();
            move || prepare(&database, &source)
        })
        .await?;

        self.outbound
            .send(
                FrameKind::ROW_DESCRIPTION,
                self.stream,
                &prepared.descriptor,
            )
            .await?;

        let mut cursor: Option<Cursor> = None;
        let mut sent: u64 = 0;

        loop {
            let database = Arc::clone(&database);
            let plan = prepared.plan.clone();
            let shape = prepared.shape.clone();
            let token = self.cancel.clone();
            let resume = cursor.take();

            let chunk =
                blocking(move || run_chunk(&database, &plan, &shape, resume, &token)).await?;

            for row in &chunk.rows {
                self.outbound
                    .send(FrameKind::DATA_ROW, self.stream, row)
                    .await?;
                sent += 1;
            }

            match chunk.next {
                Some(next) if !self.cancel.is_cancelled() => cursor = Some(next),
                // Cancelled, or there is no more. Either way the stream completes
                // with what it sent — a cancel is an early end, not a failure, and a
                // client that asked for one is not owed an error.
                _ => break,
            }
        }

        self.outbound
            .send(
                kinds::COMPLETE,
                self.stream,
                &protocol::encode_complete(sent, 0),
            )
            .await
    }
}

/// Rows per chunk.
///
/// Small enough that a cancel lands promptly and a first row appears early; large
/// enough that the per-chunk cost — a compile-free re-entry into the executor and a
/// hop to the blocking pool — is amortised.
const CHUNK_ROWS: usize = 256;

/// What compiling a query produced, before any of it has run.
struct Prepared {
    descriptor: Vec<u8>,
    plan: Plan,
    shape: RowShape,
}

/// The type rows are encoded against, and the interner that resolves its names.
#[derive(Clone)]
struct RowShape {
    ty: PredicateTy,
    /// Shared rather than cloned: a chunk hands it to a blocking thread every turn,
    /// and `LocalInterner` is not `Clone` — which is right, since two of them would
    /// be two name spaces.
    interner: Arc<LocalInterner>,
}

/// One chunk of rows, and where to carry on from.
struct Chunk {
    rows: Vec<Vec<u8>>,
    next: Option<Cursor>,
}

/// Compile, and work out what the rows will look like. No execution.
fn prepare(database: &Database, source: &str) -> Result<Prepared, ServerError> {
    let schema = &database.schema;

    let mut compilation = Compilation::new(source, schema);
    let plan = compilation.plan();

    if compilation.diagnostics().has_errors() {
        return Err(ServerError::BadQuery(compilation.render_to_string()));
    }

    let head = compilation
        .head_ty()
        .ok_or_else(|| ServerError::BadQuery("this query has no head type".to_owned()))?;

    let desc = rows::desc_of(head, compilation.interner())?;

    let Some(plan) = plan else {
        return Err(ServerError::BadQuery(
            "no plan, and no diagnostic saying why — that is a compiler bug".to_owned(),
        ));
    };

    let mut descriptor = vec![];
    encode_desc(&mut descriptor, &desc);

    // **One interner, not two.** The plan's projections hold symbols this compilation
    // minted, so `Row::to_value` has to resolve against it; the row *type* is then
    // interned into the same one, so `to_wire` resolves against it too. Two interners
    // would agree about schema names and disagree about every head field name — a row
    // that decodes and then cannot be matched to its own shape.
    let mut interner = compilation.into_interner();
    let ty = desc.to_ty(&mut interner);

    Ok(Prepared {
        descriptor,
        plan,
        shape: RowShape {
            ty,
            interner: Arc::new(interner),
        },
    })
}

/// Run at most [`CHUNK_ROWS`] rows, from the start or from `resume`.
fn run_chunk(
    database: &Database,
    plan: &Plan,
    shape: &RowShape,
    resume: Option<Cursor>,
    cancel: &CancellationToken,
) -> Result<Chunk, ServerError> {
    let store = database.db.reader();

    let executor = match resume {
        Some(cursor) => Executor::resume(store, plan.clone(), cursor),
        None => Ok(Executor::new(store, plan.clone())),
    }
    .map_err(|error| ServerError::Execution(error.to_string()))?;

    // `Suspend` at the chunk boundary is what makes `enumerate` hand back a cursor;
    // the executor then drops its snapshot, which is [I8] holding through a portal
    // rather than only in a test.
    // The step closure stays in the engine's error type and does nothing but collect:
    // encoding happens below, where a `ServerError` is expressible. Mixing the two
    // would mean inventing an engine error variant for a wire fault.
    let outcome = executor
        .enumerate(
            Vec::new(),
            |mut acc: Vec<Value>, mut row| {
                acc.push(row.to_value(&shape.interner)?);

                Ok(if acc.len() >= CHUNK_ROWS {
                    Stream::Suspend(acc)
                } else {
                    Stream::Continue(acc)
                })
            },
            cancel,
        )
        .map_err(|error| ServerError::Execution(error.to_string()))?;

    let (Iteratee::Done(values) | Iteratee::Suspended(values, _)) = &outcome;

    let mut rows = Vec::with_capacity(values.len());
    for value in values {
        let wire = rows::to_wire(&shape.ty, value)?;

        let mut buffer = vec![];
        encode_value(&mut buffer, &database.schema, &shape.ty, &wire)?;
        rows.push(buffer);
    }

    Ok(Chunk {
        rows,
        next: match outcome {
            Iteratee::Done(_) => None,
            Iteratee::Suspended(_, cursor) => Some(cursor),
        },
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

/// Report an error straight to the socket, bypassing the queues.
///
/// Only the handshake uses this, and only because it runs *before* the writer task
/// exists — there is nothing to queue into yet. Everything after it goes through
/// [`Outbound`], which is what makes the interleaving a property of the connection.
async fn send_direct<W: AsyncWrite + Unpin>(
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
