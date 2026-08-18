//! One connection, and the streams sharing it.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::Arc,
};

use aperture_schema::{
    id::FactId,
    schema::{LocalInterner, PredicateId, Schema},
};
use aperture_wire::{
    Control, ControlOp, ControlReply, FrameHeader, FrameKind, Mode, Startup, StreamId, WireFact,
    decode_desc, encode_block, encode_frame, frame,
    protocol::{self, kinds},
};

use crate::{error::ClientError, rows::Rows};

/// What the server said when the session opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub schema_fingerprint: u64,
    pub predicates: u64,
}

/// What a write stream did.
///
/// `created` counts **every** fact written, nested targets included, and `deduped`
/// those already there. A producer sending a thousand declarations that all name one
/// file sees a thousand and one created and nine hundred and ninety-nine deduped —
/// which is how it can tell interning is working without querying anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub created: u64,
    pub deduped: u64,
}

impl Written {
    /// Facts touched, however they resolved.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.created + self.deduped
    }
}

/// What sealing a database came to.
///
/// The client's own type rather than `aperture-store`'s: a client does not depend on
/// a storage engine to be told what a fingerprint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sealed {
    pub fingerprint: u64,
    pub facts: u64,
    pub bytes: u64,
    /// It was already Complete, so nothing was done. A re-run after a crash cannot
    /// tell whether it is the re-run or the original, and both must succeed.
    pub already_complete: bool,
}

/// The socket underneath, whichever kind it is.
///
/// **One enum rather than a generic parameter**, and that is a deliberate trade: making
/// [`Connection`] generic over `Read + Write` would put a type parameter into every
/// signature that touches it — `Rows`, the CLI's command functions, the shell's `Repl` —
/// to express a choice made once, at connect, and never again. The dispatch it costs is
/// one branch per frame, against a syscall.
enum Transport {
    Unix(UnixStream),
    Tcp(std::net::TcpStream),
}

impl Read for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Unix(socket) => socket.read(buffer),
            Transport::Tcp(socket) => socket.read(buffer),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Unix(socket) => socket.write(buffer),
            Transport::Tcp(socket) => socket.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Unix(socket) => socket.flush(),
            Transport::Tcp(socket) => socket.flush(),
        }
    }
}

/// A connection to an Aperture server.
///
/// # Several streams, one socket, and no runtime
///
/// A write is a stream and a query is a stream, each with an id this client assigns.
/// The server interleaves them — since 9d-ii it reads, routes to a per-stream task and
/// goes back to reading — so frames arrive in whatever order the work finishes, and a
/// client that assumed its own order would drop other people's answers on the floor.
///
/// So frames for a stream this call is not waiting on are **parked**, not discarded,
/// and delivered when that stream is next read. That is the whole of the multiplexing,
/// and it is why several [`Rows`] can be open at once.
///
/// It is synchronous, deliberately. The server is async; a client written against the
/// wire format should need nothing of the server's runtime, and this is where that
/// claim is either true or not.
pub struct Connection {
    socket: Transport,
    schema: Arc<Schema>,
    hello: Hello,
    next_stream: u32,
    /// Frames read while awaiting a different stream.
    parked: HashMap<u32, VecDeque<(FrameKind, Vec<u8>)>>,
    /// Streams with work outstanding — what makes a bookmark from another connection,
    /// or one already finished, an error rather than a read that never returns.
    open: HashSet<u32>,
    /// Ids whose stream ended **cleanly**, ready to be claimed again.
    ///
    /// A monotonic counter is what a client wants until it is long-lived: the server
    /// keys a per-connection map by stream id, so ids that are never reused make that
    /// map grow with the query count rather than with concurrency
    /// (`bench/FINDINGS.md` §7). A pooled connection is exactly the shape that reaches
    /// it, and reuse is the client's half of the fix.
    ///
    /// **Only clean ends come back here.** An id is recycled where a `COMPLETE` was
    /// read and nothing is parked for it — anywhere the stream's fate is uncertain, the
    /// id is retired instead, because reusing one the server might still be writing to
    /// would splice two results together.
    free: Vec<u32>,
}

impl Connection {
    /// Connect over a Unix socket and complete the handshake.
    ///
    /// `assert_schema` sends the schema fingerprint as a **claim**. `true` is right for
    /// a producer: a disagreement is refused at the handshake instead of by writing
    /// facts nobody can read back. `false` sends `0`, which means "do not check" and is
    /// what a reader wants.
    ///
    /// # Errors
    ///
    /// [`ClientError::Io`] if the socket will not connect, or
    /// [`ClientError::Server`] if the server refuses the session — no such database, a
    /// schema that disagrees, or a write mode asked of a sealed database (`ops-I2`).
    pub fn connect(
        socket: &Path,
        database: &str,
        schema: Arc<Schema>,
        mode: Mode,
        assert_schema: bool,
    ) -> Result<Connection, ClientError> {
        let stream = UnixStream::connect(socket)?;
        Connection::establish(
            Transport::Unix(stream),
            database,
            schema,
            mode,
            assert_schema,
        )
    }

    /// The same handshake, over TCP.
    ///
    /// **What this is not is a different protocol.** The frames, the handshake and the
    /// stream multiplexing are identical — the transport is the only thing that changes,
    /// which is why it is one enum here rather than a second client. §2's
    /// `aperture://host:port/db` address form is what reaches it.
    ///
    /// The server end is default-closed (`ops-I10`) and only listens when an operator
    /// passed `--listen-tcp`, so a connection here means somebody opted in; nothing about
    /// *this* end asserts anything about who may.
    ///
    /// # Errors
    ///
    /// As [`connect`](Connection::connect), plus [`ClientError::Io`] if the address does
    /// not resolve.
    pub fn connect_tcp(
        address: &str,
        database: &str,
        schema: Arc<Schema>,
        mode: Mode,
        assert_schema: bool,
    ) -> Result<Connection, ClientError> {
        let stream = std::net::TcpStream::connect(address)?;

        // Small frames, answered one at a time: Nagle would hold a handshake back
        // waiting for company that is not coming.
        stream.set_nodelay(true)?;

        Connection::establish(
            Transport::Tcp(stream),
            database,
            schema,
            mode,
            assert_schema,
        )
    }

    /// Open a **control session**: bound to no database, for lifecycle requests.
    ///
    /// Which exists because [`create`](Connection::create) names a database that does
    /// not exist yet, so there is nothing for the session to bind.
    ///
    /// # Errors
    ///
    /// As [`connect`](Connection::connect).
    pub fn control(socket: &Path, schema: Arc<Schema>) -> Result<Connection, ClientError> {
        Connection::connect(socket, "", schema, Mode::ReadWrite, true)
    }

    fn establish(
        socket: Transport,
        database: &str,
        schema: Arc<Schema>,
        mode: Mode,
        assert_schema: bool,
    ) -> Result<Connection, ClientError> {
        let mut connection = Connection {
            socket,
            schema,
            hello: Hello {
                version: 0,
                schema_fingerprint: 0,
                predicates: 0,
            },
            next_stream: 1,
            parked: HashMap::new(),
            free: Vec::new(),
            open: HashSet::new(),
        };

        // **Both halves of the claim, and they answer different questions.** The
        // number says "my schema is your schema", which is what a client holding the
        // whole of it means; the map says "these predicates are yours", which is the
        // only thing a client holding *part* of one can honestly say — an indexer that
        // writes six of twenty-seven predicates has a different whole-schema
        // fingerprint and is not wrong about anything ([I13](../../../docs/invariants.md#i13)).
        //
        // A Rust client computes both because it links the algorithm. That is not the
        // thing [D2](../../../docs/open-decisions.md) rules out: what a *foreign*
        // client must not do is reimplement the canonical form, and one that carries a
        // constant simply sends no map.
        let (fingerprint, predicates) = if assert_schema {
            let identity = aperture_schema::fingerprint::identity(&connection.schema);

            (
                identity.schema(),
                identity
                    .predicates()
                    .iter()
                    .map(|(name, fingerprint)| (name.clone(), *fingerprint))
                    .collect(),
            )
        } else {
            (0, vec![])
        };

        connection.send(
            kinds::STARTUP,
            StreamId(0),
            &protocol::encode_startup(&Startup {
                version: protocol::VERSION,
                database: database.to_owned(),
                mode,
                schema_fingerprint: fingerprint,
                predicates,
            }),
        )?;

        let (kind, payload) = connection.recv_on(StreamId(0))?;

        if kind != kinds::READY {
            return Err(unexpected("a ready frame", kind));
        }

        let ready = protocol::decode_ready(&payload)?;

        // Checked here rather than trusted: the server checks the client's version too,
        // and a version that got past both ends would mean neither did.
        if ready.version != protocol::VERSION {
            return Err(ClientError::Protocol(format!(
                "this client speaks protocol {}, the server speaks {}",
                protocol::VERSION,
                ready.version
            )));
        }

        connection.hello = Hello {
            version: ready.version,
            schema_fingerprint: ready.schema_fingerprint,
            predicates: ready.predicates,
        };

        Ok(connection)
    }

    /// How many distinct stream ids this connection has had to invent.
    ///
    /// Diagnostic, and the guard for id reuse: a connection that recycles the ids of
    /// cleanly-ended streams stays at its high-water *concurrency* however many queries
    /// it runs, where one that counts upwards grows with the query count and takes the
    /// server's per-connection map with it (`bench/FINDINGS.md` §7).
    #[must_use]
    pub fn stream_ids_issued(&self) -> u32 {
        self.next_stream - 1
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    // ---- writing ------------------------------------------------------------

    /// Write facts, all of one predicate, as one block on one write stream.
    ///
    /// References inside the facts may be **nested** — the whole target fact rather
    /// than an id — and the server interns them. That is what lets a producer keep no
    /// book of what it has already sent.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] if the session may not write, the database is sealed, or
    /// the facts conflict with what is already there.
    pub fn write(
        &mut self,
        predicate: PredicateId,
        facts: &[WireFact],
    ) -> Result<Written, ClientError> {
        self.write_blocks(&[(predicate, facts)])
    }

    /// Write several blocks on one write stream.
    ///
    /// One stream, so the counts that come back describe the whole batch — and one
    /// `COPY_DONE`, so the server answers once rather than per block.
    ///
    /// # Errors
    ///
    /// As [`write`](Connection::write).
    pub fn write_blocks(
        &mut self,
        blocks: &[(PredicateId, &[WireFact])],
    ) -> Result<Written, ClientError> {
        let stream = self.claim_stream();

        self.send(kinds::OPEN_WRITE, stream, &[])?;

        let (kind, _) = self.recv_on(stream)?;
        if kind != FrameKind::COPY_IN_RESPONSE {
            self.open.remove(&stream.0);
            return Err(unexpected("a copy-in response", kind));
        }

        for (predicate, facts) in blocks {
            let mut block = vec![];
            encode_block(&mut block, &self.schema, *predicate, facts)?;
            self.send(FrameKind::COPY_DATA, stream, &block)?;
        }

        self.send(FrameKind::COPY_DONE, stream, &[])?;

        let (kind, payload) = self.recv_on(stream)?;
        self.release_stream(stream);

        if kind != kinds::COMPLETE {
            return Err(unexpected("a complete frame", kind));
        }

        let (created, deduped) = protocol::decode_complete(&payload)?;
        Ok(Written { created, deduped })
    }

    // ---- querying -----------------------------------------------------------

    /// Start a query, and read its **row descriptor**.
    ///
    /// The descriptor comes first because a query's shape comes from its *head* rather
    /// than from any predicate — `{a = X, b = Y}` is a record no predicate declares —
    /// and it arrives once per stream rather than once per field per row.
    ///
    /// No rows are read here. What comes back is a [`Rows`] bookmark; pulling from it
    /// is what draws them, and stopping is what makes `\more` possible.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] with [`ErrorCode::BadQuery`](aperture_wire::ErrorCode)
    /// if it does not compile — carrying the compiler's own rendered diagnostics.
    pub fn query(&mut self, focus: &str) -> Result<Rows, ClientError> {
        self.start_query(focus, kinds::QUERY)
    }

    /// Start a query **and ask what it examined**.
    ///
    /// The answer lands on [`Rows::profile`] once the result ends, because the tally
    /// is not final until the last chunk has run. Everything else is
    /// [`query`](Connection::query) — same rows, same paging, same cursor.
    ///
    /// # Errors
    ///
    /// As [`query`](Connection::query).
    pub fn query_profiled(&mut self, focus: &str) -> Result<Rows, ClientError> {
        self.start_query(focus, kinds::QUERY_PROFILE)
    }

    /// Run a query as a **page**: at most `limit` rows, carrying on from `cursor`.
    ///
    /// `cursor` is `None` for the first page, and afterwards the previous page's
    /// [`Rows::resume_token`]. The token is opaque here — it is the engine's cursor
    /// as bytes — and it is checked against the plan that made it, so the same query
    /// text has to be passed with it.
    ///
    /// **What this buys is a caller that does not hold the connection.** An ordinary
    /// [`query`](Connection::query) streams its whole result on one stream, so
    /// "page two" means the connection that asked for page one; and there is no
    /// workaround in the language, since "everything after key K" cannot be written.
    /// A page hands the position back instead.
    ///
    /// `limit` of zero means no limit, which is exactly what
    /// [`query`](Connection::query) asks.
    ///
    /// # Errors
    ///
    /// As [`query`](Connection::query), plus [`ClientError::Server`] if the token
    /// does not belong to this query's plan.
    pub fn query_page(
        &mut self,
        focus: &str,
        limit: u64,
        cursor: Option<&[u8]>,
    ) -> Result<Rows, ClientError> {
        let payload = protocol::encode_page(&protocol::Page {
            limit,
            cursor: cursor.unwrap_or_default().to_vec(),
            query: focus.to_owned(),
        });

        self.start_query_with(kinds::QUERY_PAGE, &payload)
    }

    /// **How many rows a query has**, without receiving them.
    ///
    /// The same plan and the same executor as [`query`](Connection::query); what
    /// differs is that the server counts instead of encoding. That is the part that
    /// costs — `bench/FINDINGS.md` §9 puts row encoding at 1.5× the executor's own
    /// work and the wire above it at another 3.6× — so counting a large result is a
    /// different order of expense from receiving one and calling `.len()`.
    ///
    /// **Not aggregation in the language.** focus has no `count`, and this does not
    /// give it one: a query still answers rows, and this asks a question *about* the
    /// answer. It is what a search UI needs to say "1,234 results" honestly.
    ///
    /// # Errors
    ///
    /// As [`query`](Connection::query).
    pub fn count(&mut self, focus: &str) -> Result<u64, ClientError> {
        let stream = self.claim_stream();
        self.send(kinds::QUERY_COUNT, stream, focus.as_bytes())?;

        let (kind, payload) = self.recv_on(stream)?;
        if kind != kinds::COUNT {
            self.open.remove(&stream.0);
            return Err(unexpected("a count", kind));
        }

        let count = u64::from_le_bytes(
            payload
                .get(..8)
                .ok_or_else(|| ClientError::Protocol("a truncated count".to_owned()))?
                .try_into()
                .map_err(|_| ClientError::Protocol("a truncated count".to_owned()))?,
        );

        // The stream still owes a `COMPLETE`, and reading it here is what lets the
        // id be recycled — a stream left half-read is one that never comes back.
        let (kind, _) = self.recv_on(stream)?;
        self.release_stream(stream);

        if kind != kinds::COMPLETE {
            return Err(unexpected("a complete frame", kind));
        }

        Ok(count)
    }

    /// **The schema this session can ask about**, fetched from the server.
    ///
    /// Not the one this connection was built with, and the difference is the point: a
    /// database carries the schema it was created against
    /// ([I13](../../../docs/invariants.md#i13)), so a client's built-in copy is its own
    /// opinion and this is the answer. It includes the predicates the *server* answers
    /// (`aperture.db.List`), because the question is what can be asked here.
    ///
    /// Read back with [`recover`](aperture_schema::syntax::recover) rather than with
    /// ordinary lowering, so the ids in it are the server's — which is what makes a
    /// plan compiled from it name the predicates the server would name.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] if the server declines, or
    /// [`ClientError::Protocol`] if what comes back is not a schema.
    pub fn served_schema(&mut self) -> Result<Schema, ClientError> {
        let source = self.served_schema_source()?;

        aperture_schema::syntax::recover("the schema this server serves", &source)
            .map_err(ClientError::Protocol)
    }

    /// The same, as the text the server sent.
    ///
    /// What a shell prints for `:schema`: comments and layout are the printer's rather
    /// than anybody's, but it is the source form, so it reads as a schema rather than
    /// as a dump of one.
    ///
    /// # Errors
    ///
    /// As [`served_schema`](Connection::served_schema).
    pub fn served_schema_source(&mut self) -> Result<String, ClientError> {
        let stream = self.claim_stream();
        self.send(kinds::SCHEMA, stream, &[])?;

        let (kind, payload) = self.recv_on(stream)?;
        self.release_stream(stream);

        if kind != kinds::SCHEMA_REPLY {
            return Err(unexpected("a schema", kind));
        }

        String::from_utf8(payload)
            .map_err(|_| ClientError::Protocol("a schema that is not UTF-8".to_owned()))
    }

    /// **What facts do these ids name?** One answer each, in the order asked: the key, or
    /// which kind of nothing was there — a *missing* stored fact, which is corruption, or
    /// a row of a predicate the server **answers rather than stores**, whose listing has
    /// simply moved on. See [`Found`](aperture_wire::protocol::Found).
    ///
    /// A row carries a reference as a `FactId`, because that is what a reference is once
    /// stored, and focus cannot ask what one names — a query names a fact by its key. So
    /// this is how a client holding `#3:7` reaches the declaration behind it, and it is
    /// what [`Expander`](crate::expand::Expander) is built on.
    ///
    /// # The schema is a parameter, and it has to be
    ///
    /// The reply is **schema-driven** — each key is encoded against its own predicate's
    /// key type, with no descriptor to carry the shape — so it can only be read against
    /// the schema the *server* encoded it with. That is
    /// [`served_schema`](Connection::served_schema)'s answer, not this connection's own
    /// copy: a reader makes no claim about the schema, so `self.schema` here may be this
    /// build's built-in one while the database was created against something else
    /// ([I13](../../../docs/invariants.md#i13)). Passing the wrong one does not fail
    /// loudly — it decodes the bytes as a different shape — so the caller names it
    /// rather than this having an opinion.
    ///
    /// Rows have no such problem: the server sends a descriptor and they decode against
    /// that.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] if the server declines — an id naming a predicate it does
    /// not have, or a virtual one — or [`ClientError::Protocol`] if what comes back is
    /// not an answer to this question.
    pub fn fetch(
        &mut self,
        schema: &Schema,
        ids: &[FactId],
    ) -> Result<Vec<aperture_wire::protocol::Found>, ClientError> {
        // No round trip for nothing: a row with no references at all is the common case
        // in a query somebody narrowed by hand, and it should cost what it did before
        // expansion existed.
        if ids.is_empty() {
            return Ok(vec![]);
        }

        if ids.len() > protocol::MAX_FETCH {
            return Err(ClientError::Protocol(format!(
                "a fetch names {} ids, and {} is the most one may carry",
                ids.len(),
                protocol::MAX_FETCH
            )));
        }

        let stream = self.claim_stream();
        self.send(kinds::FETCH, stream, &protocol::encode_fetch(ids))?;

        let answer = self.recv_on(stream);
        self.release_stream(stream);

        let (kind, payload) = match answer {
            Ok(answer) => answer,

            // **A server that predates the question says so as a protocol fault.** An
            // unrecognised frame kind is handed up intact by the framing layer rather
            // than failing the decode, precisely so a peer can be told it — and a server
            // built before this frame existed answers `no handler for frame kind F`,
            // which is not a sentence anybody can act on, and whose remedy is not in it.
            //
            // **Named as the likely cause rather than the certain one**, because the code
            // is shared: `ErrorCode::Protocol` also carries a wire fault, so a server that
            // failed to encode a reply arrives here too. That case means a damaged
            // database rather than an old binary, so this says which is usual, gives the
            // remedy for it, and passes the server's own words along for the rest.
            Err(ClientError::Server {
                code: aperture_wire::ErrorCode::Protocol,
                message,
            }) => {
                return Err(ClientError::Unsupported(format!(
                    "this server did not accept a fetch frame, so a reference cannot be \
                     expanded into the fact it names — usually that means a build from \
                     before expansion existed, and restarting it with a current one is \
                     the fix. The server said: {message}"
                )));
            }

            Err(other) => return Err(other),
        };

        if kind != kinds::FETCHED {
            return Err(unexpected("the facts some ids name", kind));
        }

        Ok(protocol::decode_fetched(&payload, schema, ids)?)
    }

    fn start_query(&mut self, focus: &str, kind: FrameKind) -> Result<Rows, ClientError> {
        self.start_query_with(kind, focus.as_bytes())
    }

    fn start_query_with(&mut self, kind: FrameKind, payload: &[u8]) -> Result<Rows, ClientError> {
        let stream = self.claim_stream();
        self.send(kind, stream, payload)?;

        let (kind, payload) = self.recv_on(stream)?;
        if kind != FrameKind::ROW_DESCRIPTION {
            self.open.remove(&stream.0);
            return Err(unexpected("a row description", kind));
        }

        let (desc, _) = decode_desc(&payload)?;

        // One interner per result, built from the schema's: the descriptor names fields
        // no predicate declares, so they have to be minted somewhere, and a per-result
        // namespace is the smallest thing that can hold them.
        let mut interner = LocalInterner::new(self.schema.interner().clone());
        let ty = desc.to_ty(&mut interner);

        Ok(Rows::new(stream, desc, ty, interner))
    }

    /// Pull the next row, or `None` once the result is finished.
    ///
    /// # Errors
    ///
    /// [`ClientError::Protocol`] if the bookmark is not this connection's, or if it is
    /// already finished.
    pub fn next_row(
        &mut self,
        rows: &mut Rows,
    ) -> Result<Option<aperture_wire::WireValue>, ClientError> {
        if rows.finished() {
            return Ok(None);
        }

        self.check_open(rows)?;

        let (kind, payload) = self.recv_on(rows.stream())?;

        // Sent once, just before the result ends. Taken here rather than in a
        // separate call so a caller that only pulls rows still ends up holding it.
        if kind == kinds::PROFILE {
            rows.set_profile(protocol::decode_profile(&payload)?);
            return self.next_row(rows);
        }

        // Like the profile: sent once, just before the result ends, and taken here
        // rather than in a separate call so a caller that only pulls rows still ends
        // up holding it.
        if kind == kinds::RESUME {
            rows.set_resume(payload);
            return self.next_row(rows);
        }

        if kind == kinds::COMPLETE {
            let (sent, _) = protocol::decode_complete(&payload)?;
            self.release_stream(rows.stream());
            rows.finish(sent)?;
            return Ok(None);
        }

        if kind != FrameKind::DATA_ROW {
            return Err(unexpected("a data row", kind));
        }

        Ok(Some(rows.decode(&payload, &self.schema)?))
    }

    /// Pull up to `limit` rows, and **stop**.
    ///
    /// This is the page. The stream stays open across the pause and the next call
    /// carries on where this one left off — which is not a client-side buffer being
    /// drained but the server genuinely parked: its outbound queue for this stream
    /// fills, its query loop suspends holding a bytes-only
    /// [`Cursor`](../../aperture-engine/src/iter.rs), and the snapshot is already
    /// released at the chunk boundary ([I8](../../../docs/invariants.md#i8)). A pause
    /// of a millisecond and a pause of an hour cost the server the same thing.
    ///
    /// That is what `\more` is, and what makes it the first interactive exerciser of
    /// [I4](../../../docs/invariants.md#i4).
    ///
    /// # Errors
    ///
    /// As [`next_row`](Connection::next_row).
    pub fn take(
        &mut self,
        rows: &mut Rows,
        limit: usize,
    ) -> Result<Vec<aperture_wire::WireValue>, ClientError> {
        let mut page = Vec::with_capacity(limit.min(1024));

        while page.len() < limit {
            match self.next_row(rows)? {
                Some(row) => page.push(row),
                None => break,
            }
        }

        Ok(page)
    }

    /// Pull every remaining row.
    ///
    /// Convenience, and named for what it costs: a result of unknown size is held in
    /// memory here. [`take`](Connection::take) is what a shell wants.
    ///
    /// # Errors
    ///
    /// As [`next_row`](Connection::next_row).
    pub fn drain(&mut self, rows: &mut Rows) -> Result<Vec<aperture_wire::WireValue>, ClientError> {
        let mut all = vec![];
        while let Some(row) = self.next_row(rows)? {
            all.push(row);
        }
        Ok(all)
    }

    /// Stop a result early, in band, and answer with how many rows the server sent.
    ///
    /// A cancel is an **early end, not a failure**: the server completes the stream
    /// with what it had sent, and a client that asked for one is not owed an error. So
    /// the rows already in flight are read and dropped rather than left in the socket
    /// for the next stream to trip over.
    ///
    /// # Errors
    ///
    /// As [`next_row`](Connection::next_row).
    pub fn cancel(&mut self, rows: &mut Rows) -> Result<u64, ClientError> {
        if rows.finished() {
            return Ok(rows.sent());
        }

        self.check_open(rows)?;
        self.send(kinds::CANCEL, rows.stream(), &[])?;

        loop {
            let (kind, payload) = self.recv_on(rows.stream())?;

            match kind {
                // Counted rather than merely dropped, so the tally at the end still
                // means "everything the server said it sent reached here".
                FrameKind::DATA_ROW => rows.skip(),
                _ if kind == kinds::PROFILE => {
                    rows.set_profile(protocol::decode_profile(&payload)?);
                }
                _ if kind == kinds::RESUME => rows.set_resume(payload),
                _ if kind == kinds::COMPLETE => {
                    let (sent, _) = protocol::decode_complete(&payload)?;
                    self.release_stream(rows.stream());
                    rows.finish(sent)?;
                    return Ok(sent);
                }
                other => return Err(unexpected("a data row or a complete frame", other)),
            }
        }
    }

    // ---- lifecycle ----------------------------------------------------------

    /// Create a database, and answer with the provisional instance it was given.
    ///
    /// `schema` is **resolved source** — the union of an entry file and its imports —
    /// or empty for "the server's own". Source rather than a fingerprint because the
    /// database has to embed it, and resolved by the caller because the files are on
    /// the caller's machine.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] if the server declines — a name already taken, a name
    /// that cannot be a directory, a schema it will not accept, or a read-only session
    /// asking.
    pub fn create(&mut self, database: &str, schema: &str) -> Result<String, ClientError> {
        match self.control_request(ControlOp::Create, database, false, schema)? {
            ControlReply::Created { instance } => Ok(instance),
            other => Err(mismatched(&other)),
        }
    }

    /// Seal a database.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] if the server declines — no such database, or one
    /// holding no facts without `allow_zero_facts`.
    pub fn finish(
        &mut self,
        database: &str,
        allow_zero_facts: bool,
    ) -> Result<Sealed, ClientError> {
        match self.control_request(ControlOp::Finish, database, allow_zero_facts, "")? {
            ControlReply::Finished {
                fingerprint,
                facts,
                bytes,
                already_complete,
            } => Ok(Sealed {
                fingerprint,
                facts,
                bytes,
                already_complete,
            }),
            other => Err(mismatched(&other)),
        }
    }

    /// Delete a database.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] if the server declines — no such database, or one a
    /// session still holds ([`ErrorCode::InUse`](aperture_wire::ErrorCode), which is
    /// the one worth retrying).
    pub fn remove(&mut self, database: &str) -> Result<(), ClientError> {
        match self.control_request(ControlOp::Remove, database, false, "")? {
            ControlReply::Removed => Ok(()),
            other => Err(mismatched(&other)),
        }
    }

    fn control_request(
        &mut self,
        op: ControlOp,
        database: &str,
        allow_zero_facts: bool,
        schema: &str,
    ) -> Result<ControlReply, ClientError> {
        let stream = self.claim_stream();

        self.send(
            kinds::CONTROL,
            stream,
            &protocol::encode_control(&Control {
                op,
                database: database.to_owned(),
                allow_zero_facts,
                schema: schema.to_owned(),
            }),
        )?;

        let (kind, payload) = self.recv_on(stream)?;
        self.release_stream(stream);

        if kind != kinds::CONTROL_REPLY {
            return Err(unexpected("a control reply", kind));
        }

        Ok(protocol::decode_control_reply(&payload)?)
    }

    // ---- frames -------------------------------------------------------------

    /// A stream id, marked as having work outstanding.
    ///
    /// A recycled one first — see [`free`](Self::free) for why a long-lived connection
    /// must not simply count upwards.
    fn claim_stream(&mut self) -> StreamId {
        let stream = StreamId(match self.free.pop() {
            Some(id) => id,
            None => {
                let id = self.next_stream;
                self.next_stream += 1;
                id
            }
        });

        self.open.insert(stream.0);
        stream
    }

    /// A stream that ended cleanly: no longer outstanding, and its id claimable again.
    ///
    /// The parked check is the guard. A queue with anything in it means a frame for
    /// this id arrived that nobody read, so the id's story is not over and handing it
    /// to the next query would deliver that frame to the wrong result.
    fn release_stream(&mut self, stream: StreamId) {
        self.open.remove(&stream.0);

        if self.parked.get(&stream.0).is_none_or(VecDeque::is_empty) {
            self.parked.remove(&stream.0);
            self.free.push(stream.0);
        }
    }

    fn check_open(&self, rows: &Rows) -> Result<(), ClientError> {
        if self.open.contains(&rows.stream().0) {
            return Ok(());
        }

        Err(ClientError::Protocol(format!(
            "stream {} has no result outstanding on this connection",
            rows.stream().0
        )))
    }

    fn send(
        &mut self,
        kind: FrameKind,
        stream: StreamId,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        let mut out = Vec::with_capacity(frame::HEADER_LEN + payload.len());
        encode_frame(&mut out, kind, stream, payload)?;
        self.socket.write_all(&out)?;
        Ok(())
    }

    /// The next frame **for `stream`**, parking anything that arrives for another.
    ///
    /// An error on stream 0 is raised wherever it lands: stream 0 is the session
    /// rather than a unit of work, so a fault there is not something to park for a
    /// reader that may never come.
    fn recv_on(&mut self, stream: StreamId) -> Result<(FrameKind, Vec<u8>), ClientError> {
        if let Some(frame) = self.parked.get_mut(&stream.0).and_then(VecDeque::pop_front) {
            return raise_if_error(frame);
        }

        loop {
            let (header, payload) = self.recv_any()?;

            if header.stream == stream {
                return raise_if_error((header.kind, payload));
            }

            if header.stream == StreamId(0) && header.kind == FrameKind::ERROR {
                return raise_if_error((header.kind, payload));
            }

            self.parked
                .entry(header.stream.0)
                .or_default()
                .push_back((header.kind, payload));
        }
    }

    fn recv_any(&mut self) -> Result<(FrameHeader, Vec<u8>), ClientError> {
        let mut head = [0u8; frame::HEADER_LEN];
        self.socket.read_exact(&mut head)?;

        let header = frame::decode_header(&head)?;

        let mut payload = vec![0u8; header.length as usize];
        self.socket.read_exact(&mut payload)?;

        Ok((header, payload))
    }
}

impl std::fmt::Debug for Connection {
    /// By hand, because the socket and the schema have nothing useful to show and the
    /// session does: what was agreed, and what is still in flight.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("hello", &self.hello)
            .field("open_streams", &self.open.len())
            .field(
                "parked_frames",
                &self.parked.values().map(VecDeque::len).sum::<usize>(),
            )
            .finish()
    }
}

/// Turn an error frame into an error, and leave everything else alone.
fn raise_if_error(frame: (FrameKind, Vec<u8>)) -> Result<(FrameKind, Vec<u8>), ClientError> {
    let (kind, payload) = frame;

    if kind != FrameKind::ERROR {
        return Ok((kind, payload));
    }

    let (code, message) = protocol::decode_error(&payload)?;
    Err(ClientError::Server { code, message })
}

fn unexpected(wanted: &str, got: FrameKind) -> ClientError {
    ClientError::Protocol(format!("expected {wanted}, got `{got}`"))
}

fn mismatched(reply: &ControlReply) -> ClientError {
    ClientError::Protocol(format!(
        "the server answered a different operation than the one asked: {reply:?}"
    ))
}
