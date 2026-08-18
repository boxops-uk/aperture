//! One connection, from handshake to close.
//!
//! # What this is
//!
//! The frame loop [operations §5 `serve`](../../../docs/aperture-cli-design.md)
//! describes: a PG-shaped handshake, then framed messages tagged by stream, with a
//! write stream, a query stream and a lifecycle request living on one connection at
//! once — each on its own task, so none of them waits on another.
//!
//! The reader loop never does a stream's work. It reads, routes to that stream's task,
//! and goes back to reading; the one writer task takes a frame from each stream's
//! queue in turn. That is what makes a long query stop starving a short one, and it is
//! why a `create` — tens of keyspaces, tens of milliseconds each — costs the
//! connection's other streams nothing.
//!
//! Still deferred, and named in §5 as deferred: per-stream flow-control windows. What
//! is here instead is bounded per-stream queues plus connection backpressure, which is
//! what §5 says to start with.
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

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::{Mutex, mpsc},
};

use aperture_encoding::tuple::Value;
use aperture_engine::{
    compile::Compilation,
    iter::{Cursor, Executor, Iteratee, Profile, Stream},
    plan::{Plan, SeekKey, Source, Step, Test},
};
use aperture_ingest::{Ingested, intern_block};
use aperture_schema::{
    fingerprint::Identity,
    schema::{LocalInterner, PredicateId, PredicateTy, Schema},
};
use aperture_store::{meta::Status, store::FjallDb};
use aperture_wire::{
    FrameHeader, FrameKind, StreamId, encode_desc, encode_frame, frame,
    protocol::{self, ErrorCode, Mode, ProfileStep, QueryProfile, Ready, Startup, kinds},
    value::encode_value,
};
use tokio_util::sync::CancellationToken;

use crate::{
    blocking,
    catalogue::{self, Catalogue, Catalogued},
    error::ServerError,
    outbound::{Outbound, run as outbound_run},
    registry::Registry,
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
    /// This database's schema identity — the whole-schema number a handshake compares
    /// against, and the per-predicate map a **subset** claim is checked against
    /// ([chapter 6](../../../docs/06-types-and-schema.md), [I13](../../../docs/invariants.md#i13)).
    ///
    /// Computed once at open rather than per connection: it walks the schema and hashes
    /// it, which is nothing next to opening a store and everything next to doing it on
    /// every handshake.
    pub identity: Identity,
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

    /// Whether this database still takes writes (`ops-I2`).
    ///
    /// **Read twice, on purpose, and the two readings do different jobs.** The
    /// handshake reads it without the lock, which is `ops-I2`'s "refused at
    /// establishment": a client asking to write a sealed database is told so before it
    /// sends anything, and no session waits on an in-flight ingest to be told. A write
    /// reads it again *inside* the writer lock, and that reading is the exact one —
    /// see [`Registry::finish`](crate::registry::Registry) for why the pair leaves no
    /// third ordering.
    writable: AtomicBool,
}

impl Database {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        db: FjallDb,
        schema: Arc<Schema>,
        status: Status,
    ) -> Database {
        Database {
            name: name.into(),
            db: Arc::new(db),
            identity: aperture_schema::fingerprint::identity(&schema),
            schema,
            writer: Mutex::new(()),
            writable: AtomicBool::new(status.is_writable()),
        }
    }

    /// Whether a write may still be accepted for this database.
    #[must_use]
    pub fn writable(&self) -> bool {
        self.writable.load(Ordering::SeqCst)
    }

    /// Stop taking writes, forever.
    ///
    /// `pub(crate)` because it is only correct while the writer lock is held, and the
    /// registry is the only caller that holds it.
    pub(crate) fn seal(&self) {
        self.writable.store(false, Ordering::SeqCst);
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
    registry: Arc<Registry>,
    /// `None` for a **control session** — one bound to no database at all.
    ///
    /// Which exists because `create` names a database that does not exist yet: a
    /// lifecycle client cannot bind the thing it is about to make, and making it bind
    /// some *other* database first would be a rule with no meaning behind it.
    database: Option<Arc<Database>>,
    mode: Mode,
}

/// Serve one connection to completion.
///
/// # Errors
///
/// Only fatal faults escape: an I/O failure, or a peer whose frames no longer parse.
/// Everything else is answered with an error frame on the stream that caused it and
/// the connection carries on.
pub async fn serve<R, W>(reader: R, writer: W, registry: &Arc<Registry>) -> Result<(), ServerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let session = match handshake(&mut reader, &mut writer, registry).await {
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

    let _connection = registry.stats().connection_opened();

    let session = Arc::new(session);
    let outbound = Arc::new(Outbound::new(Arc::clone(registry.stats())));

    // The one task that writes. Everything else queues.
    let pump = {
        let outbound = Arc::clone(&outbound);
        tokio::spawn(async move {
            let result = outbound_run(&outbound, &mut writer).await;

            // **Whichever half stops first tells the other.** The writer is the only
            // thing that ever frees a queue slot, so a producer waiting for one waits
            // on this task specifically; if the socket failed under it, that wait can
            // never end on its own. Closing here rather than only after `read_loop`
            // returns covers the case where the write side dies while the read side is
            // still open, which is a half-closed peer rather than a departed one.
            outbound.close().await;
            result
        })
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
    registry: &Arc<Registry>,
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

    // An empty name is a **control session**: bound to no database, and the only
    // session `create` could possibly be sent on. Every other name must resolve.
    let database = if startup.database.is_empty() {
        None
    } else {
        Some(
            registry
                .find(&startup.database)
                .ok_or_else(|| ServerError::UnknownDatabase(startup.database.clone()))?,
        )
    };

    let (identity, predicates) = match &database {
        Some(database) => (&database.identity, database.schema.len()),
        None => (registry.identity(), registry.schema().len()),
    };

    let fingerprint = identity.schema();

    // **Two claims, checked in the order that lets the weaker one be useful.**
    //
    // The number is equality: "my schema is your schema", which is what a client
    // holding the whole of one means and what every client meant before 8.4. It is
    // checked first because it is the common case and answers in one comparison.
    //
    // Containment is the fallback, and it is [I13](../../../docs/invariants.md#i13)'s
    // actual rule: a producer that writes six of twenty-seven predicates has a
    // different whole-schema fingerprint and is not wrong about anything. What it
    // claims is the shapes it uses; what is checked is that this database holds each
    // of them, *identically* — a predicate whose key gained a field is a predicate
    // whose stored rows this client would encode wrongly, and it is refused by name
    // rather than as two numbers that differ.
    //
    // Zero and empty together mean "no opinion" — a reader, or a client written
    // against whatever the server has.
    if startup.schema_fingerprint != 0 && startup.schema_fingerprint != fingerprint {
        if startup.predicates.is_empty() {
            return Err(ServerError::SchemaMismatch {
                expected: startup.schema_fingerprint,
                actual: fingerprint,
            });
        }

        let broken: Vec<String> = startup
            .predicates
            .iter()
            .filter(|(name, claimed)| identity.of(name) != Some(*claimed))
            .map(|(name, _)| name.clone())
            .collect();

        if !broken.is_empty() {
            let missing = broken
                .iter()
                .filter(|name| identity.of(name).is_none())
                .count();

            return Err(ServerError::SchemaNotContained {
                database: if startup.database.is_empty() {
                    "this server".to_owned()
                } else {
                    startup.database.clone()
                },
                detail: format!(
                    "{missing} not declared here, {} declared differently",
                    broken.len() - missing
                ),
                broken,
            });
        }
    }

    // **`ops-I2`, at establishment.** Once a database is Complete every write-mode open
    // is refused, forever — and refusing it *here* is what makes immutability the
    // absence of a writable session rather than a check each write has to remember.
    if startup.mode == Mode::ReadWrite
        && let Some(database) = &database
        && !database.writable()
    {
        return Err(ServerError::Sealed(database.name.clone()));
    }

    send(
        writer,
        kinds::READY,
        StreamId(0),
        &protocol::encode_ready(&Ready {
            version: protocol::VERSION,
            schema_fingerprint: fingerprint,
            predicates: predicates as u64,
        }),
    )
    .await?;
    writer.flush().await?;

    Ok(Session {
        registry: Arc::clone(registry),
        database,
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

    // **The map has to shed finished streams, or it is the leak on its own.** A stream's
    // task now ends when its work does, which frees the task — but the entry naming it
    // stays until something removes it, and a client that never reuses an id would leave
    // one per query.
    //
    // Swept rather than signalled: a handle whose task has ended has a closed `Sender`,
    // which is the fact already available without a second channel to carry it. The
    // watermark is what keeps it amortised — a sweep costs one pass and then does not
    // run again until the map has doubled, so a connection genuinely holding many live
    // streams does not sweep on every frame.
    const MIN_SWEEP_AT: usize = 32;
    let mut sweep_at = MIN_SWEEP_AT;

    loop {
        let Some((header, payload)) = read_frame(reader).await? else {
            return Ok(());
        };

        if streams.len() >= sweep_at {
            streams.retain(|_, handle| !handle.inbound.is_closed());
            sweep_at = streams.len().saturating_mul(2).max(MIN_SWEEP_AT);
        }

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

/// **A stream's work belongs to its connection, and ends with it.**
///
/// Dropping a [`CancellationToken`] does not cancel it, so without this the map in
/// [`read_loop`] could go — taking the only `Sender` with it — while the task it named
/// was still inside a query, computing chunk after chunk for a client that had gone. The
/// task would only find out when it next tried to *send*, and it does not try until the
/// chunk it is on is finished; a large result is many chunks, each one a job on the
/// blocking pool that nobody will ever read.
///
/// Dropping the handle is the one event that means "nobody is listening any more" — it
/// covers the reader ending for *any* reason, which is why the cancel lives here rather
/// than at one of `read_loop`'s several exits.
///
/// **This is about wasted work, and it is not what fixed the leak** — worth stating
/// plainly, because the two look like the same bug and one of them is a decoy. Adding
/// this alone left 106,215 stream tasks stuck after 383,121 abandoned connections,
/// because they were parked in [`Outbound::send`](crate::outbound::Outbound::send)
/// waiting for queue room rather than anywhere a cancellation could reach them. What
/// releases those is [`Outbound::close`](crate::outbound::Outbound::close) waking its
/// waiters; see `bench/FINDINGS.md` §10.
impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
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

        let live = session.registry.stats().stream_opened();

        tokio::spawn(async move {
            // Held by the task, so the gauge falls on every way out there is — return,
            // error, the channel closing, or the task being dropped mid-await.
            let _live = live;
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

                // **A stream that has finished its work ends here, and that is what
                // stops a connection accumulating one parked task per query.**
                //
                // `handle` returning means the request is *done*: a query runs its
                // whole result inside one call, chunk after chunk, and a control
                // request answers in one. The single exception is a write stream
                // between `OPEN_WRITE` and `COPY_DONE`, which spans frames by
                // definition — and `copy_done` takes the `Writing`, so this same test
                // ends the stream on the frame that closes it.
                //
                // Before this, the loop waited on a channel whose only `Sender` lived
                // in a map with no removal path, so every task stayed parked for the
                // life of the connection: `bench/FINDINGS.md` §7 measured ~3.5 kB
                // retained per query, and a connection pool is exactly the shape that
                // hits it. Ending here is what the gauge `streams_live` was already
                // counting and nothing was yet allowed to decrement.
                if task.writing.is_none() {
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
            kinds::QUERY => self.query(payload, false, None).await,
            kinds::QUERY_PROFILE => self.query(payload, true, None).await,
            kinds::QUERY_COUNT => self.count(payload).await,
            kinds::QUERY_PAGE => {
                let page = protocol::decode_page(payload)?;
                self.query(page.query.as_bytes(), false, Some(&page)).await
            }
            kinds::CONTROL => self.control(payload).await,
            kinds::SCHEMA => self.schema().await,

            other => Err(ServerError::Protocol(format!(
                "no handler for frame kind `{other}`"
            ))),
        }
    }

    /// The database this session is bound to, or the fault of asking without one.
    fn database(&self) -> Result<&Arc<Database>, ServerError> {
        self.session
            .database
            .as_ref()
            .ok_or(ServerError::NoDatabase)
    }

    /// Carry out a lifecycle request.
    ///
    /// **Read-only means read-only, whatever the frame kind.** `ops-I6` resolves a
    /// session's mode once at establishment, and a session that may not write facts
    /// does not get to create, seal or delete a whole database by asking on a
    /// different frame.
    ///
    /// It runs on a stream task like everything else, which is what keeps a `create` —
    /// tens of keyspaces, tens of milliseconds each — off the reader loop and out of
    /// the way of the queries sharing the connection.
    async fn control(&mut self, payload: &[u8]) -> Result<(), ServerError> {
        if self.session.mode != Mode::ReadWrite {
            return Err(ServerError::ModeRefused);
        }

        let request = protocol::decode_control(payload)?;
        let reply = self.session.registry.execute(&request).await?;

        self.outbound
            .send(
                kinds::CONTROL_REPLY,
                self.stream,
                &protocol::encode_control_reply(&reply),
            )
            .await
    }

    /// Answer with the schema this session can ask about, as source.
    ///
    /// **The served schema, virtual predicates and all** — the question is what this
    /// session can ask, not what the database holds. A control session, bound to no
    /// database, gets the server's own, which is what its `aperture.db.List` is
    /// answered against.
    ///
    /// Printed rather than read off the disk copy: the copy is what a database
    /// *embedded*, and a session may be served something a shade wider than that (the
    /// virtuals) or, for an artifact from before the copy was kept, something the disk
    /// does not hold at all. Printing what is actually being served cannot disagree
    /// with what queries are compiled against, because it is the same value.
    async fn schema(&mut self) -> Result<(), ServerError> {
        let schema = match &self.session.database {
            Some(database) => Arc::clone(&database.schema),
            None => Arc::clone(self.session.registry.schema()),
        };

        let source = aperture_schema::syntax::print::served(&schema);

        self.outbound
            .send(kinds::SCHEMA_REPLY, self.stream, source.as_bytes())
            .await
    }

    async fn open_write(&mut self) -> Result<(), ServerError> {
        if self.session.mode != Mode::ReadWrite {
            return Err(ServerError::ModeRefused);
        }

        let database = self.database()?;

        // The establishment check again, for a session that was established *before*
        // the seal — refusing here rather than at the first block, so a client is not
        // told it may write and then told it may not. The binding refusal is the one
        // inside the writer lock, in [`copy_data`](Self::copy_data).
        if !database.writable() {
            return Err(ServerError::Sealed(database.name.clone()));
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

        let database = Arc::clone(self.database()?);
        let working = Arc::clone(&database);
        let block = payload.to_vec();

        // Serialised per database, not merely per stream: fjall's non-transactional
        // path loses updates on a concurrent read-modify-write, and interning is
        // exactly that — look the key up, write it if absent. `ops-I1` gives one
        // server the database, and this gives one writer the server's half of it.
        let out: Ingested = {
            let _writing = database.writer.lock().await;

            // **`ops-I2`, exactly.** The establishment check refused every session that
            // began after the seal; this one catches the session that began *before*
            // it, whose block would otherwise land in a database whose identity has
            // already been recorded. Inside the lock, so there is no gap to slip
            // through: a seal cannot happen while this guard is held.
            if !database.writable() {
                return Err(ServerError::Sealed(database.name.clone()));
            }

            blocking::run(move || {
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
    async fn query(
        &mut self,
        payload: &[u8],
        profiled: bool,
        page: Option<&protocol::Page>,
    ) -> Result<(), ServerError> {
        let stats = Arc::clone(self.session.registry.stats());
        stats.query_started();

        let outcome = self.run_query(payload, profiled, page, &stats).await;
        match &outcome {
            Ok(()) => stats.query_completed(),
            Err(_) => stats.query_failed(),
        }
        outcome
    }

    /// Run a query and answer **how many rows** it has.
    ///
    /// The same plan and the same executor as [`query`](Self::query); what differs is
    /// the accumulator, because `enumerate` is a fold and counting is a fold that
    /// keeps a number. Chunked exactly as the row path is, and for the same two
    /// reasons rather than for streaming: the snapshot is released at each suspend
    /// ([I8](../../../docs/invariants.md#i8)), and a cancel lands between chunks.
    ///
    /// **This is not aggregation in the language.** A query still answers rows; this
    /// asks a question about the answer. What it saves is the part that costs —
    /// `bench/FINDINGS.md` §9 puts row encoding at 1.5× the executor's own work and
    /// the wire above it at another 3.6×, all of which a caller that only wants a
    /// number throws away.
    async fn count(&mut self, payload: &[u8]) -> Result<(), ServerError> {
        let stats = Arc::clone(self.session.registry.stats());
        stats.query_started();

        let outcome = self.run_count(payload, &stats).await;
        match &outcome {
            Ok(()) => stats.query_completed(),
            Err(_) => stats.query_failed(),
        }
        outcome
    }

    async fn run_count(
        &mut self,
        payload: &[u8],
        stats: &Arc<crate::stats::ServerStats>,
    ) -> Result<(), ServerError> {
        let source = std::str::from_utf8(payload)
            .map_err(|_| ServerError::Protocol("a query that is not UTF-8".to_owned()))?
            .to_owned();

        let database = Arc::clone(self.database()?);
        let prepared = {
            let queued = std::time::Instant::now();
            let stats = Arc::clone(stats);
            let database = Arc::clone(&database);
            let catalog = self.session.registry.catalog().clone();
            blocking::run(move || {
                stats.blocking_dispatched(queued.elapsed().as_micros() as u64);
                prepare(&database, &catalog, &source)
            })
            .await?
        };

        let mut cursor: Option<Cursor> = None;
        let mut total: u64 = 0;

        loop {
            let database = Arc::clone(&database);
            let plan = prepared.plan.clone();
            let token = self.cancel.clone();
            let resume = cursor.take();
            let listing = prepared.catalogue.clone();

            let counting = {
                let queued = std::time::Instant::now();
                let stats = Arc::clone(stats);
                blocking::run(move || {
                    stats.blocking_dispatched(queued.elapsed().as_micros() as u64);
                    count_chunk(&database, listing.as_ref(), &plan, resume, &token)
                })
                .await
            };

            let (counted, next) = match counting {
                Ok(counted) => counted,
                Err(_) if self.cancel.is_cancelled() => break,
                Err(error) => return Err(error),
            };

            total += counted;

            match next {
                Some(next) if !self.cancel.is_cancelled() => cursor = Some(next),
                _ => break,
            }
        }

        self.outbound
            .send(kinds::COUNT, self.stream, &total.to_le_bytes())
            .await?;

        self.outbound
            .send(
                kinds::COMPLETE,
                self.stream,
                &protocol::encode_complete(total, 0),
            )
            .await
    }

    async fn run_query(
        &mut self,
        payload: &[u8],
        profiled: bool,
        page: Option<&protocol::Page>,
        stats: &Arc<crate::stats::ServerStats>,
    ) -> Result<(), ServerError> {
        let source = std::str::from_utf8(payload)
            .map_err(|_| ServerError::Protocol("a query that is not UTF-8".to_owned()))?
            .to_owned();

        let database = Arc::clone(self.database()?);
        let prepared = {
            let queued = std::time::Instant::now();
            let stats = Arc::clone(stats);
            let database = Arc::clone(&database);
            let source = source.clone();
            let catalog = self.session.registry.catalog().clone();
            blocking::run(move || {
                stats.blocking_dispatched(queued.elapsed().as_micros() as u64);
                prepare(&database, &catalog, &source)
            })
            .await?
        };

        // **Where this page starts, decided before a byte of answer goes out.** The
        // token is the client's, so it is untrusted, and the checks are ordered the
        // way `Executor::resume` orders them: is this a cursor at all, is it in a
        // layout this build reads, and is it *this plan's*.
        //
        // Checked here rather than left to the first chunk because of *when* the
        // failure lands: a caller that has already been sent a row description and a
        // row or two, and then an error, has to unpick a result it was told existed.
        // A refusal before the descriptor is a refusal of the request.
        let mut cursor: Option<Cursor> = match page {
            Some(page) if !page.cursor.is_empty() => {
                let cursor = Cursor::from_bytes(&page.cursor)
                    .map_err(|error| ServerError::Execution(error.to_string()))?;

                if cursor.version() != aperture_engine::iter::CURSOR_VERSION {
                    return Err(ServerError::Execution(format!(
                        "resume token is from cursor layout {}, this server reads {}",
                        cursor.version(),
                        aperture_engine::iter::CURSOR_VERSION
                    )));
                }

                // Entries are paired with levels by order, so a token from a
                // different plan does not fail — it answers, from the wrong rows.
                if cursor.plan() != prepared.plan.fingerprint() {
                    return Err(ServerError::Execution(
                        "resume token belongs to a different query".to_owned(),
                    ));
                }

                Some(cursor)
            }
            _ => None,
        };

        self.outbound
            .send(
                FrameKind::ROW_DESCRIPTION,
                self.stream,
                &prepared.descriptor,
            )
            .await?;

        let limit = page.map_or(0, |page| page.limit);
        let mut sent: u64 = 0;

        // **One profile for the whole run**, carried across every chunk. A chunk
        // boundary is a real resume, so a profile made per chunk would report the
        // last page's work and call it the query's.
        let mut profile = Profile::for_plan(&prepared.plan);

        loop {
            let database = Arc::clone(&database);
            let plan = prepared.plan.clone();
            let shape = prepared.shape.clone();
            let token = self.cancel.clone();
            let resume = cursor.take();
            let mut counted = std::mem::take(&mut profile);

            // A page smaller than a chunk must not overshoot it: the rows past the
            // limit would be computed, encoded and thrown away, and the token would
            // name a position the client was never told about.
            let budget = match limit {
                0 => CHUNK_ROWS,
                limit => CHUNK_ROWS.min((limit - sent) as usize),
            };

            // The **same** listing every chunk, which is what makes a virtual predicate
            // behave like a snapshot: cloning the `Arc` shares the rows rather than
            // re-reading the store root, so a database created between two pages is
            // invisible to the result in flight.
            let listing = prepared.catalogue.clone();

            let chunk = {
                // Timed from *here* rather than inside: what this measures is how long
                // the hop waited before the pool had room for it, which is the only
                // sight there is of a blocking pool nothing throttles (`F8`).
                let queued = std::time::Instant::now();
                let stats = Arc::clone(stats);
                blocking::run(move || {
                    stats.blocking_dispatched(queued.elapsed().as_micros() as u64);
                    let chunk = run_chunk(
                        &database,
                        listing.as_ref(),
                        &Chunking {
                            plan: &plan,
                            shape: &shape,
                            budget,
                            cancel: &token,
                        },
                        resume,
                        &mut counted,
                    )?;
                    Ok((chunk, counted))
                })
                .await
            };

            // **A cancel that lands *inside* a chunk ends the stream, exactly as one
            // that lands between chunks does** — the arm below this loop says so in
            // those words, and this is the same rule where the executor gets to the
            // news first. To the executor a cancellation *is* an error: it stops
            // mid-scan and says so. To a client that asked for one it is the answer,
            // and sending it on has two costs rather than one — the caller is handed a
            // failure for something it requested, and the `COMPLETE` it was waiting for
            // never arrives, so the stream is never drained and its id never returned.
            //
            // Any error while cancelled is treated this way, not only the executor's
            // own: a client that has asked to stop is owed a clean stop, and whatever
            // else went wrong on the way down will be found again by the next query.
            let (chunk, counted) = match chunk {
                Ok(chunk) => chunk,
                Err(_) if self.cancel.is_cancelled() => break,
                Err(error) => return Err(error),
            };
            profile = counted;

            stats.chunk_sent(chunk.rows.len() as u64);

            for row in &chunk.rows {
                self.outbound
                    .send(FrameKind::DATA_ROW, self.stream, row)
                    .await?;
                sent += 1;
            }

            // The page is full and there is more: hand the position back rather
            // than carry on. A page that reached the end sends no token, which is
            // how a caller knows it has seen everything without asking again to be
            // told nothing.
            if limit > 0 && sent >= limit {
                if let Some(next) = chunk.next {
                    self.outbound
                        .send(kinds::RESUME, self.stream, &next.to_bytes())
                        .await?;
                }
                break;
            }

            match chunk.next {
                Some(next) if !self.cancel.is_cancelled() => cursor = Some(next),
                // Cancelled, or there is no more. Either way the stream completes
                // with what it sent — a cancel is an early end, not a failure, and a
                // client that asked for one is not owed an error.
                _ => break,
            }
        }

        // **A cancelled query reports no profile, and that is the design's rule rather
        // than an oversight** ([operations §5](../../../docs/aperture-cli-design.md)):
        // the tally is not final until the last chunk has run, so one taken here counts
        // what a *different* query examined — the prefix the client was willing to wait
        // for. Sent anyway, it lands beside a truncated row count and invites exactly
        // the ratio it cannot support.
        //
        // The client is what makes this observable, since a `--limit` cancels in band
        // and then reads the profile that follows.
        if profiled && !self.cancel.is_cancelled() {
            self.outbound
                .send(
                    kinds::PROFILE,
                    self.stream,
                    &protocol::encode_profile(&describe_profile(
                        &prepared.plan,
                        &database.schema,
                        &profile,
                    )),
                )
                .await?;
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

/// Name every step of a plan, and pair each with what it read.
///
/// The engine counts by **position in the body** and knows nothing about names; the
/// schema knows names and nothing about what ran. This is the one place the two meet,
/// and it is on the server because the plan is the server's — a client holds a query's
/// text and its row shape, never its plan.
fn describe_profile(plan: &Plan, schema: &Schema, profile: &Profile) -> QueryProfile {
    let steps = plan
        .body
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let (label, full_scan) = label_step(step, schema);

            ProfileStep {
                label,
                examined: profile.examined.get(index).copied().unwrap_or(0),
                full_scan,
            }
        })
        .collect();

    QueryProfile { steps }
}

fn label_step(step: &Step, schema: &Schema) -> (String, bool) {
    match step {
        Step::Level(level) => {
            let mut names = vec![];
            let mut full_scan = false;

            for source in &level.sources {
                match source {
                    Source::Seek { access, .. } => {
                        names.push(predicate_name(schema, access.predicate_id));

                        // A seek that pins nothing reads the predicate whole. That is
                        // the one line of a profile which names something to go and
                        // fix, so it is worth being exact about: an empty prefix, or
                        // a composite with no parts.
                        full_scan |= match &access.seek_key {
                            SeekKey::Prefix(bytes) => bytes.is_empty(),
                            SeekKey::Composite(parts) => parts.is_empty(),
                        };
                    }

                    // One point read per row of the level above — never a scan, so
                    // never a full one however many rows it answers.
                    Source::Fetch { predicate_id, .. } => {
                        names.push(format!("fetch {}", predicate_name(schema, *predicate_id)));
                    }
                }
            }

            // A level with no sources at all is `never`: the empty relation, which
            // reads nothing and says so rather than printing an empty name.
            if names.is_empty() {
                return ("never".to_owned(), false);
            }

            (names.join(" | "), full_scan)
        }

        // A derived bind is one value, not a relation: it takes a slot in the tally
        // so the positions line up, and it will always read zero.
        Step::Derive(_) => ("derive".to_owned(), false),

        // Reads no predicate, so there is nothing for a profile to name — but it is
        // a step and the profile has one entry per step, so it says what it is.
        Step::Test(Test::Compare { op, .. }) => (format!("test {}", op.symbol()), false),

        Step::Test(Test::Absent(sources)) => {
            let names: Vec<String> = sources
                .iter()
                .map(|source| match source {
                    Source::Seek { access, .. } => predicate_name(schema, access.predicate_id),
                    Source::Fetch { predicate_id, .. } => predicate_name(schema, *predicate_id),
                })
                .collect();

            // A probe stops at its first row, so it is never a full scan whatever its
            // seek pinned — which is exactly the distinction the flag is for.
            (format!("!{}", names.join(" | ")), false)
        }
    }
}

fn predicate_name(schema: &Schema, id: PredicateId) -> String {
    schema
        .get(id)
        .and_then(|predicate| predicate.name())
        .map_or_else(|| format!("predicate {}", id.0), ToOwned::to_owned)
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
    /// The listing this query reads, if it reads one.
    ///
    /// **Materialised once, here, and shared by every chunk** — which is what makes a
    /// virtual predicate behave like a snapshot: a database created between two pages
    /// of `\more` is invisible to the result in flight, exactly as a write to a
    /// keyspace is. `None` for every query that does not name it, which is nearly all
    /// of them, because building it walks the store root.
    catalogue: Option<Arc<Catalogue>>,
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
fn prepare(
    database: &Database,
    catalog: &aperture_store::catalog::Catalog,
    source: &str,
) -> Result<Prepared, ServerError> {
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

    // **Only if the query asks for it.** A plan names its predicates, so the cheap
    // question is answered before the expensive work: building the listing walks the
    // store root and reads a sidecar per database, which is `ops-I7` doing exactly what
    // it is for and still far too much to do on every query about `src.File`.
    let catalogue = match database
        .schema
        .find_position(catalogue::PREDICATE)
        .map(|(id, _)| id)
    {
        Some(id) if catalogue::reads(&plan, id) => {
            let listing = catalog.list()?;
            Catalogue::materialise(&database.schema, &listing)?.map(Arc::new)
        }
        _ => None,
    };

    Ok(Prepared {
        descriptor,
        plan,
        shape: RowShape {
            ty,
            interner: Arc::new(interner),
        },
        catalogue,
    })
}

/// One chunk of a **count**: how many rows, and where to carry on.
///
/// A sibling of [`run_chunk`] rather than a mode of it, and deliberately: the row
/// path is the hot one, and threading "do not encode" through it would put a branch
/// per row in the middle of what `bench/FINDINGS.md` §9 measured. This shares the
/// executor and shares nothing else.
fn count_chunk(
    database: &Database,
    catalogue: Option<&Arc<Catalogue>>,
    plan: &Plan,
    resume: Option<Cursor>,
    cancel: &CancellationToken,
) -> Result<(u64, Option<Cursor>), ServerError> {
    let store = database.db.reader();

    match catalogue {
        Some(catalogue) => counting(
            Catalogued::new(store, Arc::clone(catalogue)),
            plan,
            resume,
            cancel,
        ),
        None => counting(store, plan, resume, cancel),
    }
}

/// [`count_chunk`], once the store is known.
fn counting<S: aperture_store::fact_store::FactStore>(
    store: S,
    plan: &Plan,
    resume: Option<Cursor>,
    cancel: &CancellationToken,
) -> Result<(u64, Option<Cursor>), ServerError> {
    let executor = match resume {
        Some(cursor) => Executor::resume(store, plan.clone(), cursor),
        None => Ok(Executor::new(store, plan.clone())),
    }
    .map_err(|error| ServerError::Execution(error.to_string()))?;

    // **The row is never built.** `to_value` is what allocates and what decodes; a
    // count needs neither, so the closure looks at nothing and adds one. That is the
    // whole saving, and it is why this is a different accumulator rather than a
    // different plan.
    let outcome = executor
        .enumerate(
            0u64,
            |n, _row| {
                let n = n + 1;
                Ok(if n % CHUNK_ROWS as u64 == 0 {
                    Stream::Suspend(n)
                } else {
                    Stream::Continue(n)
                })
            },
            cancel,
        )
        .map_err(|error| ServerError::Execution(error.to_string()))?;

    Ok(match outcome {
        Iteratee::Done(n) => (n, None),
        Iteratee::Suspended(n, cursor) => (n, Some(cursor)),
    })
}

/// One turn of the chunk loop: what to run, how much of it, and where to stop.
///
/// A struct rather than four more parameters — the loop reads better for it, and
/// `budget` in particular is the one a reader has to be able to find, since a page
/// limit under [`CHUNK_ROWS`] arriving here is what stops a page overshooting.
struct Chunking<'a> {
    plan: &'a Plan,
    shape: &'a RowShape,
    /// The most rows this turn may produce: [`CHUNK_ROWS`], or what is left of a page.
    budget: usize,
    cancel: &'a CancellationToken,
}

/// Run at most `work.budget` rows, from the start or from `resume`.
fn run_chunk(
    database: &Database,
    catalogue: Option<&Arc<Catalogue>>,
    work: &Chunking<'_>,
    resume: Option<Cursor>,
    profile: &mut Profile,
) -> Result<Chunk, ServerError> {
    // **The one place a virtual predicate is visible**, and it is a *store*, not a plan
    // and not a step: `Catalogued` answers the catalogue's keyspace from memory and
    // delegates every other to fjall. The executor is generic over `FactStore`, so
    // `over` below is running the same code either way — which is the whole reason a
    // listing can be seeked into, joined against, paged and profiled without the
    // machine learning anything (see [`catalogue`](crate::catalogue)).
    //
    // Two calls rather than one boxed store, because `FactStore::Scan` is an associated
    // type: a `dyn FactStore` would have to erase the scan too, which costs an
    // allocation and a virtual call **per row** on the hot path, to save one line here.
    let store = database.db.reader();

    match catalogue {
        Some(catalogue) => over(
            Catalogued::new(store, Arc::clone(catalogue)),
            database,
            work,
            resume,
            profile,
        ),
        None => over(store, database, work, resume, profile),
    }
}

/// [`run_chunk`], once the store is known.
fn over<S: aperture_store::fact_store::FactStore>(
    store: S,
    database: &Database,
    work: &Chunking<'_>,
    resume: Option<Cursor>,
    profile: &mut Profile,
) -> Result<Chunk, ServerError> {
    let Chunking {
        plan,
        shape,
        budget,
        cancel,
    } = work;
    let budget = *budget;
    let executor = match resume {
        Some(cursor) => Executor::resume(store, (*plan).clone(), cursor),
        None => Ok(Executor::new(store, (*plan).clone())),
    }
    .map_err(|error| ServerError::Execution(error.to_string()))?;

    // `Suspend` at the chunk boundary is what makes `enumerate` hand back a cursor;
    // the executor then drops its snapshot, which is [I8] holding through a portal
    // rather than only in a test.
    // The step closure stays in the engine's error type and does nothing but collect:
    // encoding happens below, where a `ServerError` is expressible. Mixing the two
    // would mean inventing an engine error variant for a wire fault.
    let outcome = executor
        .enumerate_profiled(
            Vec::new(),
            |mut acc: Vec<Value>, mut row| {
                acc.push(row.to_value(&shape.interner)?);

                Ok(if acc.len() >= budget {
                    Stream::Suspend(acc)
                } else {
                    Stream::Continue(acc)
                })
            },
            cancel,
            profile,
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
