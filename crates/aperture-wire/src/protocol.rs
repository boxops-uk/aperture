//! **The message vocabulary** — what a frame's payload means.
//!
//! [`frame`](crate::frame) delimits messages and deliberately does not interpret
//! them. This is the layer that does: which kinds exist, what a startup frame
//! carries, and what a stream's life looks like. Kept apart from the codec so a
//! client can be written against the wire format without adopting a server's idea of
//! a session — which is exactly what the .NET client does.
//!
//! ```text
//!   client                                    server
//!     ── S startup{version, db, mode, fp} ──▶
//!     ◀──────────── R ready{version, fp} ────      or E error
//!
//!     ── W open-write        (stream 1) ────▶
//!     ◀──────────── G copy-in-response ─────
//!     ── d copy-data [block] (stream 1) ────▶
//!     ── c copy-done         (stream 1) ────▶
//!     ◀──── C complete{created, deduped} ────      or E error
//!
//!     ── Q query "X where …"  (stream 2) ───▶
//!     ◀──────── T row-description[desc] ────
//!     ◀──────── D data-row[value] ──────────
//!     ◀──────── C complete{rows} ───────────      or E error
//!
//!     ── L control{op, name}  (stream 3) ───▶
//!     ◀──────── M control-reply[…] ─────────      or E error
//!
//!     ── F fetch{ids}         (stream 4) ───▶
//!     ◀──────── f fetched[keys] ────────────      or E error
//! ```
//!
//! # The lifecycle is a stream like any other
//!
//! `create`, `finish` and `remove` are [control](Control) frames on an ordinary
//! stream, which is what makes them work **against a running server** instead of
//! requiring one to be stopped ([operations §5](../../../docs/aperture-cli-design.md)).
//! Putting them on a stream rather than on stream 0 buys the whole of the existing
//! machinery: they queue fairly behind other work, a failure answers on the stream that
//! caused it, and a slow `create` does not stall the connection's reader.
//!
//! `list` and `describe` are **not** here, and their absence is `ops-I7` rather than a
//! gap: enumeration reads sidecars and never opens fjall, so it already works while a
//! server holds every database under the root. §5's remote branch answers them through
//! the virtual predicate `aperture.db.List` — the normal query machinery, no bespoke
//! message — which is Phase 9f's.
//!
//! All of it is **additive**, so [`VERSION`] does not move: a client that predates
//! control frames never sends one and is never sent one. The .NET client under
//! `clients/dotnet` is the check that this is true rather than hoped.
//!
//! # Asking what an id names is a question of its own
//!
//! [`FETCH`](kinds::FETCH) is the read-path twin of a nested reference on the way in: a
//! row carries a reference as a `FactId`, and this answers with the fact. It is additive
//! on the same terms — a client that never asks neither sends it nor receives a reply —
//! and it is deliberately **not** a fifth query kind. Expansion is orthogonal to paging,
//! profiling and counting, so a query kind for it would need one per combination;
//! asking about the ids in a row after the row arrived composes with all of them.
//!
//! # Every message is a frame, including the handshake
//!
//! PostgreSQL's startup packet is special-cased — length-prefixed with no type byte —
//! because it predates its own message framing. There is no reason to inherit that:
//! a startup frame here is an ordinary frame with kind `S`, so a reader has one loop
//! rather than a preamble and then a loop.
//!
//! # Numbers are varints, not fixed width
//!
//! Payload fields use the same [`varint`](crate::varint) the value codec
//! does. The fixed-width fields in the format are exactly the ones something must
//! *skip* without parsing — a frame's length, a block's — and a handshake field is
//! never skipped.

use aperture_schema::{
    id::FactId,
    schema::{PredicateId, Schema},
};

use crate::{
    WireError,
    value::{WireValue, decode_value, encode_value},
    varint,
};

/// The protocol version this build speaks.
///
/// Bumped when the *meaning* of a frame changes. The schema fingerprint below is a
/// separate axis: one says "we disagree about the protocol", the other "we agree
/// about the protocol and disagree about the data".
///
/// **2 is Phase 8's.** A startup frame's `schema_fingerprint` used to carry a
/// provisional hash this crate computed; it now carries
/// [chapter 6](../../../docs/06-types-and-schema.md)'s schema identity, computed in
/// `aperture-schema` over the canonical form. Every number changed, so a client pinned
/// to the old one is told it speaks a different protocol rather than left to fail a
/// comparison it cannot interpret.
pub const VERSION: u32 = 2;

/// Frame kinds this protocol assigns, beyond the ones the codec already names.
pub mod kinds {
    use crate::FrameKind;

    /// Client → server, stream 0: open the session.
    pub const STARTUP: FrameKind = FrameKind(b'S');
    /// Server → client, stream 0: the session is open.
    pub const READY: FrameKind = FrameKind(b'R');
    /// Client → server: open a write stream.
    pub const OPEN_WRITE: FrameKind = FrameKind(b'W');
    /// Client → server: run a query on a new stream.
    pub const QUERY: FrameKind = FrameKind(b'Q');
    /// Client → server: run a query, and report what it examined.
    ///
    /// A second kind rather than a flag in [`QUERY`]'s payload, because that payload
    /// is the query text and nothing else — a leading flag byte would be a silent
    /// change of meaning for every client already sending UTF-8. This way a client
    /// that has never heard of profiling neither sends this nor receives a
    /// [`PROFILE`] frame, which is what "additive" has to mean if the protocol
    /// version is to stay where it is.
    pub const QUERY_PROFILE: FrameKind = FrameKind(b'P');
    /// Server → client: what the query examined, sent once, just before its
    /// [`COMPLETE`].
    pub const PROFILE: FrameKind = FrameKind(b'p');
    /// Client → server: run a query, stop after N rows, and hand back a token.
    ///
    /// A third query kind rather than a flag, for the reason
    /// [`QUERY_PROFILE`] is a second one: [`QUERY`]'s payload is the query text and
    /// nothing else, and a client that has never heard of paging neither sends this
    /// nor receives a [`RESUME`] frame.
    ///
    /// **This is what makes paging stateless.** Without it a result lives in the
    /// server's session, keyed by stream id, and a caller has to hold the connection
    /// to see page two — which a web tier cannot do, and cannot work around either,
    /// because "everything after key K" is not expressible in the language.
    pub const QUERY_PAGE: FrameKind = FrameKind(b'G');
    /// Client → server: run a query and report only **how many rows** it has.
    ///
    /// A fourth query kind, and the cheapest one to justify: the plan is the same,
    /// the executor is the same, and what differs is the accumulator — `enumerate`
    /// is a fold, so counting is a fold that keeps a number instead of a row.
    ///
    /// **Not aggregation in the language.** focus has no `count`, and this does not
    /// give it one: a query still answers rows, and this asks a question *about* the
    /// answer rather than computing one. What it saves is the part that costs —
    /// `bench/FINDINGS.md` §9 puts row encoding at 1.5× the executor and the wire
    /// above it at another 3.6×, all of which a caller counting rows throws away.
    pub const QUERY_COUNT: FrameKind = FrameKind(b'N');
    /// Server → client: how many rows the query has.
    pub const COUNT: FrameKind = FrameKind(b'n');
    /// Server → client: the resume token, sent once, just before [`COMPLETE`].
    ///
    /// Only when the result was cut short by a page limit *and* there is more. A
    /// page that reached the end of the result sends no token, which is how a caller
    /// knows it has seen everything without asking again to be told nothing.
    pub const RESUME: FrameKind = FrameKind(b'r');
    /// Server → client: the stream finished, with counts.
    pub const COMPLETE: FrameKind = FrameKind(b'C');
    /// Client → server: stop this stream.
    ///
    /// **In band, on the stream it cancels** — not a connection teardown and not a
    /// side channel. That is the whole reason frames carry a stream id: cancelling a
    /// long query has to be possible without disturbing the other streams sharing the
    /// socket, and a second connection could not do it because the first one's state
    /// is not there.
    pub const CANCEL: FrameKind = FrameKind(b'X');
    /// Client → server: a lifecycle request — create, finish, remove.
    pub const CONTROL: FrameKind = FrameKind(b'L');
    /// Server → client: what the lifecycle request came to.
    pub const CONTROL_REPLY: FrameKind = FrameKind(b'M');
    /// Client → server: **what can I ask you?** No payload.
    ///
    /// The answer is the schema this session's database is served with, as source —
    /// [`SCHEMA_REPLY`]. Since Phase 8.4 that is a real question rather than a
    /// formality: a database carries the schema it was created against, so a store root
    /// holds artifacts of different shapes and a client's built-in idea of one is
    /// nobody's answer but its own.
    ///
    /// What it buys is everything a client can then do *locally*: describe the
    /// predicates it can actually ask about, compile a query before sending it — so a
    /// mistake is a caret under the word rather than a round trip and a sentence — and
    /// show the plan, which a client otherwise never holds. The letters are a pair
    /// rather than an inheritance: PostgreSQL has nothing to borrow here.
    pub const SCHEMA: FrameKind = FrameKind(b'H');
    /// Server → client: the schema, as source. UTF-8, no framing of its own.
    ///
    /// **Virtual predicates included**, because the question is what this session can
    /// *ask*, not what the database holds — a client that cannot see `aperture.db.List`
    /// cannot compile the one query every server answers.
    pub const SCHEMA_REPLY: FrameKind = FrameKind(b'h');
    /// Client → server: **what facts do these ids name?**
    ///
    /// The read-path twin of [a reference on the way in][settled]. Stored, a reference
    /// *is* a `FactId` and nothing else, so a row carries `#3:7` where the thing worth
    /// reading is the declaration it names — and nothing in focus can ask, because a
    /// query names a fact by its key and never by its number. That is deliberate and
    /// stays that way: an id is physical, and putting one in the language would put a
    /// storage detail in a query. So the question goes on the protocol, which is the one
    /// place an id has already legitimately crossed.
    ///
    /// **Not a fifth query kind.** [`QUERY_PROFILE`] and [`QUERY_PAGE`] are separate
    /// kinds because a flag in [`QUERY`]'s payload would change what its bytes mean;
    /// a query kind for expansion would be worse than that, since expansion is
    /// orthogonal to paging, profiling and counting alike and would need a kind per
    /// combination. Asking about the ids in a row *after* the row arrived composes with
    /// every way of asking for rows, and costs a client that never asks nothing at all.
    ///
    /// [settled]: ../../../docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline
    pub const FETCH: FrameKind = FrameKind(b'F');
    /// Server → client: the facts those ids name, **in the order they were asked
    /// about**.
    ///
    /// Positional against the request, which is the same bargain
    /// [`ROW_DESCRIPTION`](crate::FrameKind::ROW_DESCRIPTION) strikes with its rows: the
    /// asker still holds the ids, so echoing them back would be sending the question
    /// with the answer. The reply carries its own count, so the one fault positional
    /// encoding is exposed to — two peers disagreeing about how many answers there are
    /// — is caught rather than mis-paired.
    pub const FETCHED: FrameKind = FrameKind(b'f');
}

/// Which way a session may go, declared at startup and resolved once against the
/// database's status (`ops-I6`, `ops-I2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    ReadOnly,
    ReadWrite,
}

impl Mode {
    #[must_use]
    pub fn as_byte(self) -> u8 {
        match self {
            Mode::ReadOnly => 0,
            Mode::ReadWrite => 1,
        }
    }

    #[must_use]
    pub fn from_byte(byte: u8) -> Option<Mode> {
        match byte {
            0 => Some(Mode::ReadOnly),
            1 => Some(Mode::ReadWrite),
            _ => None,
        }
    }
}

/// What a client says to open a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Startup {
    pub version: u32,
    pub database: String,
    pub mode: Mode,
    /// The schema the client believes it is writing against, or `0` for "do not
    /// check".
    ///
    /// Zero is a real answer rather than a hole: a client that only reads, or that
    /// was written against whatever the server has, has nothing to assert. A
    /// *non-zero* value is a claim, and a claim that disagrees is refused before any
    /// data flows — which is the cheap early mismatch detection §6 is after.
    pub schema_fingerprint: u64,

    /// The predicates this client claims, each with its own fingerprint — **subset
    /// containment**, which is [I13](../../../docs/invariants.md#i13)'s actual rule.
    ///
    /// The field above is an equality check, and equality is the wrong question for a
    /// producer that writes six of a database's twenty-seven predicates: its whole
    /// schema is a different schema, and refusing it would force every indexer to
    /// restate every predicate it never touches. What it can honestly claim is the
    /// shapes it *uses*, and containment is what chapter 6 says compatibility is.
    ///
    /// Empty means "no per-predicate claim", which is what a client carrying a single
    /// constant sends — see [the decision](../../../docs/open-decisions.md) that a
    /// client never computes a fingerprint. A Rust client links the algorithm and can
    /// compute this from the schema it holds; a hand-written one carries a number and
    /// leaves this empty.
    pub predicates: Vec<(String, u64)>,
}

/// What the server answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub version: u32,
    pub schema_fingerprint: u64,
    pub predicates: u64,
}

/// Why a stream or a session failed.
///
/// The code exists so a client can branch without parsing English. The message
/// exists because a person reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorCode {
    Protocol = 1,
    UnknownDatabase = 2,
    SchemaMismatch = 3,
    ModeRefused = 4,
    BadFacts = 5,
    Conflict = 6,
    BadQuery = 7,
    Internal = 8,
    /// A database something else is holding — a session has it open, so it cannot be
    /// taken away underneath. The one code here worth *retrying*.
    InUse = 9,
    /// A well-formed request the server will not carry out: a name already taken, a
    /// name that cannot be a directory, an empty database sealed without the flag.
    ///
    /// Distinct from [`Internal`](ErrorCode::Internal), and the distinction is the
    /// whole point of having it: `Internal` says look at the server's logs, and this
    /// says the answer is in the message you are holding.
    Refused = 10,
}

impl ErrorCode {
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<ErrorCode> {
        Some(match byte {
            1 => ErrorCode::Protocol,
            2 => ErrorCode::UnknownDatabase,
            3 => ErrorCode::SchemaMismatch,
            4 => ErrorCode::ModeRefused,
            5 => ErrorCode::BadFacts,
            6 => ErrorCode::Conflict,
            7 => ErrorCode::BadQuery,
            8 => ErrorCode::Internal,
            9 => ErrorCode::InUse,
            10 => ErrorCode::Refused,
            _ => return None,
        })
    }
}

/// Which lifecycle operation a [`Control`] frame asks for.
///
/// The discriminants are a wire contract: **append only**, never renumber. A reply
/// carries the same byte, so a client decodes an answer without having to remember
/// what it asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlOp {
    Create = 1,
    Finish = 2,
    Remove = 3,
}

impl ControlOp {
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<ControlOp> {
        Some(match byte {
            1 => ControlOp::Create,
            2 => ControlOp::Finish,
            3 => ControlOp::Remove,
            _ => return None,
        })
    }
}

/// A lifecycle request.
///
/// The database is named in the frame rather than taken from the session, because
/// `create` names one that does not exist yet — which is also why a session may be
/// bound to no database at all (see [`Startup::database`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub op: ControlOp,
    pub database: String,
    /// `finish` only: seal a database holding no facts.
    ///
    /// A flag on the request rather than a separate op, because it changes what one
    /// operation *permits* rather than what it does.
    pub allow_zero_facts: bool,

    /// `create` only: the schema to create it against, as **resolved source**.
    ///
    /// Empty means "the server's own", which is what a client that has no opinion
    /// sends and what every client sent before 8.4. Source rather than a fingerprint
    /// because the server has to *embed* it: a number would only let it check a schema
    /// it already had, which is the case that needs no message at all.
    ///
    /// Imports are resolved by the **caller**, so a schema path is a property of the
    /// machine holding the files rather than of the one holding the databases — a
    /// server asked to read a path it cannot see would be a worse error than the one
    /// this avoids.
    pub schema: String,
}

/// What a lifecycle request came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlReply {
    /// The provisional instance the new database was given.
    Created {
        instance: String,
    },
    Finished {
        fingerprint: u64,
        facts: u64,
        bytes: u64,
        already_complete: bool,
    },
    Removed,
}

// ---- encoding ---------------------------------------------------------------

fn put_str(out: &mut Vec<u8>, text: &str) {
    varint::put_u64(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

fn get_str(bytes: &[u8]) -> Result<(String, usize), WireError> {
    let (len, used) = varint::get_u64(bytes)?;
    let rest = &bytes[used..];

    let len = usize::try_from(len)
        .ok()
        .filter(|len| *len <= rest.len())
        .ok_or(WireError::LengthOutOfRange {
            declared: len,
            available: rest.len(),
        })?;

    let text = std::str::from_utf8(&rest[..len])
        .map_err(|_| WireError::BadString)?
        .to_owned();

    Ok((text, used + len))
}

#[must_use]
pub fn encode_startup(startup: &Startup) -> Vec<u8> {
    let mut out = vec![];
    varint::put_u64(&mut out, u64::from(startup.version));
    put_str(&mut out, &startup.database);
    out.push(startup.mode.as_byte());
    varint::put_u64(&mut out, startup.schema_fingerprint);

    varint::put_u64(&mut out, startup.predicates.len() as u64);
    for (name, fingerprint) in &startup.predicates {
        put_str(&mut out, name);
        varint::put_u64(&mut out, *fingerprint);
    }

    out
}

/// # Errors
///
/// [`WireError`] if the payload is malformed or the mode byte is not one.
pub fn decode_startup(bytes: &[u8]) -> Result<Startup, WireError> {
    let (version, mut at) = varint::get_u64(bytes)?;
    let (database, used) = get_str(&bytes[at..])?;
    at += used;

    let mode = bytes
        .get(at)
        .copied()
        .and_then(Mode::from_byte)
        .ok_or(WireError::TypeMismatch("session mode"))?;
    at += 1;

    let (schema_fingerprint, used) = varint::get_u64(&bytes[at..])?;
    at += used;

    let (claimed, used) = varint::get_u64(&bytes[at..])?;
    at += used;

    let claimed = usize::try_from(claimed).map_err(|_| WireError::TypeMismatch("claims"))?;
    let mut predicates = Vec::with_capacity(claimed.min(64));

    for _ in 0..claimed {
        let (name, used) = get_str(&bytes[at..])?;
        at += used;

        let (fingerprint, used) = varint::get_u64(&bytes[at..])?;
        at += used;

        predicates.push((name, fingerprint));
    }

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(Startup {
        version: u32::try_from(version).map_err(|_| WireError::TypeMismatch("version"))?,
        database,
        mode,
        schema_fingerprint,
        predicates,
    })
}

#[must_use]
pub fn encode_ready(ready: &Ready) -> Vec<u8> {
    let mut out = vec![];
    varint::put_u64(&mut out, u64::from(ready.version));
    varint::put_u64(&mut out, ready.schema_fingerprint);
    varint::put_u64(&mut out, ready.predicates);
    out
}

/// # Errors
///
/// [`WireError`] if the payload is malformed.
pub fn decode_ready(bytes: &[u8]) -> Result<Ready, WireError> {
    let (version, mut at) = varint::get_u64(bytes)?;
    let (schema_fingerprint, used) = varint::get_u64(&bytes[at..])?;
    at += used;
    let (predicates, used) = varint::get_u64(&bytes[at..])?;
    at += used;

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(Ready {
        version: u32::try_from(version).map_err(|_| WireError::TypeMismatch("version"))?,
        schema_fingerprint,
        predicates,
    })
}

#[must_use]
pub fn encode_error(code: ErrorCode, message: &str) -> Vec<u8> {
    let mut out = vec![code as u8];
    put_str(&mut out, message);
    out
}

/// # Errors
///
/// [`WireError`] if the payload is malformed or the code is not one.
pub fn decode_error(bytes: &[u8]) -> Result<(ErrorCode, String), WireError> {
    let code = bytes
        .first()
        .copied()
        .and_then(ErrorCode::from_byte)
        .ok_or(WireError::TypeMismatch("error code"))?;

    let (message, _) = get_str(&bytes[1..])?;
    Ok((code, message))
}

/// What a stream did: a write's `(created, deduped)` or a query's `(rows, 0)`.
#[must_use]
pub fn encode_complete(first: u64, second: u64) -> Vec<u8> {
    let mut out = vec![];
    varint::put_u64(&mut out, first);
    varint::put_u64(&mut out, second);
    out
}

/// # Errors
///
/// [`WireError`] if the payload is malformed.
pub fn decode_complete(bytes: &[u8]) -> Result<(u64, u64), WireError> {
    let (first, mut at) = varint::get_u64(bytes)?;
    let (second, used) = varint::get_u64(&bytes[at..])?;
    at += used;

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok((first, second))
}

#[must_use]
pub fn encode_control(control: &Control) -> Vec<u8> {
    let mut out = vec![control.op as u8];
    put_str(&mut out, &control.database);
    out.push(u8::from(control.allow_zero_facts));
    put_str(&mut out, &control.schema);
    out
}

/// # Errors
///
/// [`WireError`] if the payload is malformed or the op is not one this build knows.
pub fn decode_control(bytes: &[u8]) -> Result<Control, WireError> {
    let op = bytes
        .first()
        .copied()
        .and_then(ControlOp::from_byte)
        .ok_or(WireError::TypeMismatch("control op"))?;

    let (database, used) = get_str(&bytes[1..])?;
    let mut at = 1 + used;

    let allow_zero_facts = bytes
        .get(at)
        .copied()
        .ok_or(WireError::TypeMismatch("control flags"))?
        != 0;
    at += 1;

    let (schema, used) = get_str(&bytes[at..])?;
    at += used;

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(Control {
        op,
        database,
        allow_zero_facts,
        schema,
    })
}

#[must_use]
pub fn encode_control_reply(reply: &ControlReply) -> Vec<u8> {
    let mut out = vec![];

    match reply {
        ControlReply::Created { instance } => {
            out.push(ControlOp::Create as u8);
            put_str(&mut out, instance);
        }
        ControlReply::Finished {
            fingerprint,
            facts,
            bytes,
            already_complete,
        } => {
            out.push(ControlOp::Finish as u8);
            varint::put_u64(&mut out, *fingerprint);
            varint::put_u64(&mut out, *facts);
            varint::put_u64(&mut out, *bytes);
            out.push(u8::from(*already_complete));
        }
        ControlReply::Removed => out.push(ControlOp::Remove as u8),
    }

    out
}

/// # Errors
///
/// [`WireError`] if the payload is malformed or the op is not one this build knows.
pub fn decode_control_reply(bytes: &[u8]) -> Result<ControlReply, WireError> {
    let op = bytes
        .first()
        .copied()
        .and_then(ControlOp::from_byte)
        .ok_or(WireError::TypeMismatch("control op"))?;

    let rest = &bytes[1..];

    let (reply, at) = match op {
        ControlOp::Create => {
            let (instance, used) = get_str(rest)?;
            (ControlReply::Created { instance }, used)
        }

        ControlOp::Finish => {
            let (fingerprint, mut at) = varint::get_u64(rest)?;
            let (facts, used) = varint::get_u64(&rest[at..])?;
            at += used;
            let (size, used) = varint::get_u64(&rest[at..])?;
            at += used;

            let already_complete = rest
                .get(at)
                .copied()
                .ok_or(WireError::TypeMismatch("already complete"))?
                != 0;
            at += 1;

            (
                ControlReply::Finished {
                    fingerprint,
                    facts,
                    bytes: size,
                    already_complete,
                },
                at,
            )
        }

        ControlOp::Remove => (ControlReply::Removed, 0),
    };

    if at != rest.len() {
        return Err(WireError::TrailingBytes(rest.len() - at));
    }

    Ok(reply)
}

/// One step of a plan, and what running it read.
///
/// The *outcome* to a plan's *intent*: a plan says which field narrowed the scan and
/// which one only filters, and this says how many rows that came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileStep {
    /// What the step is, in the schema's names — a predicate, a fetch through a
    /// reference, a negation, a derived bind.
    pub label: String,
    /// Rows pulled from a scan here, **matched or skipped**.
    pub examined: u64,
    /// Whether this step read a predicate whole.
    ///
    /// Glean prints `" (full scan)"` for the same reason: it is the one line of a
    /// profile that names a thing to go and fix.
    pub full_scan: bool,
}

/// What a query examined, step by step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryProfile {
    pub steps: Vec<ProfileStep>,
}

impl QueryProfile {
    /// Rows examined across every step.
    #[must_use]
    pub fn examined(&self) -> u64 {
        self.steps.iter().map(|step| step.examined).sum()
    }
}

#[must_use]
pub fn encode_profile(profile: &QueryProfile) -> Vec<u8> {
    let mut out = vec![];
    varint::put_u64(&mut out, profile.steps.len() as u64);

    for step in &profile.steps {
        put_str(&mut out, &step.label);
        varint::put_u64(&mut out, step.examined);
        out.push(u8::from(step.full_scan));
    }

    out
}

/// # Errors
///
/// [`WireError`] if the payload is malformed.
pub fn decode_profile(bytes: &[u8]) -> Result<QueryProfile, WireError> {
    let (count, mut at) = varint::get_u64(bytes)?;

    // A declared count larger than the bytes could hold is a fault, not an allocation
    // request: the same rule the descriptor follows, and for the same reason.
    let count = usize::try_from(count)
        .ok()
        .filter(|count| *count <= bytes.len())
        .ok_or(WireError::LengthOutOfRange {
            declared: count,
            available: bytes.len(),
        })?;

    let mut steps = Vec::with_capacity(count);

    for _ in 0..count {
        let (label, used) = get_str(&bytes[at..])?;
        at += used;

        let (examined, used) = varint::get_u64(&bytes[at..])?;
        at += used;

        let full_scan = bytes
            .get(at)
            .copied()
            .ok_or(WireError::TypeMismatch("full scan flag"))?
            != 0;
        at += 1;

        steps.push(ProfileStep {
            label,
            examined,
            full_scan,
        });
    }

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(QueryProfile { steps })
}

/// What a paged query asks for: how many rows, and where to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The most rows this page may carry. Zero means no limit, which is the same
    /// question an ordinary [`QUERY`](kinds::QUERY) asks.
    pub limit: u64,
    /// A token from a previous page's [`RESUME`](kinds::RESUME), or empty to start.
    ///
    /// **Opaque here, and that is the layering.** A cursor is the engine's, a client
    /// depends on `aperture-wire` and not on the engine, and the only thing either
    /// end of the wire does with these bytes is carry them. What they mean — and
    /// whether they mean it for *this* plan — is checked where the plan is.
    pub cursor: Vec<u8>,
    /// The query itself.
    pub query: String,
}

/// Encode a paged query request.
#[must_use]
pub fn encode_page(page: &Page) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + page.cursor.len() + page.query.len());

    out.extend_from_slice(&page.limit.to_le_bytes());
    out.extend_from_slice(&(page.cursor.len() as u32).to_le_bytes());
    out.extend_from_slice(&page.cursor);
    out.extend_from_slice(page.query.as_bytes());

    out
}

/// Decode a paged query request.
///
/// # Errors
///
/// [`WireError::UnexpectedEof`] if the frame is shorter than its own lengths claim,
/// or [`WireError::BadString`] if the query text is not UTF-8.
pub fn decode_page(bytes: &[u8]) -> Result<Page, WireError> {
    let mut at = 0usize;

    let limit = u64::from_le_bytes(
        bytes
            .get(at..at + 8)
            .ok_or(WireError::UnexpectedEof)?
            .try_into()
            .map_err(|_| WireError::UnexpectedEof)?,
    );
    at += 8;

    let cursor_len = u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .ok_or(WireError::UnexpectedEof)?
            .try_into()
            .map_err(|_| WireError::UnexpectedEof)?,
    ) as usize;
    at += 4;

    let cursor = bytes
        .get(at..at + cursor_len)
        .ok_or(WireError::UnexpectedEof)?
        .to_vec();
    at += cursor_len;

    let query = std::str::from_utf8(bytes.get(at..).ok_or(WireError::UnexpectedEof)?)
        .map_err(|_| WireError::BadString)?
        .to_owned();

    Ok(Page {
        limit,
        cursor,
        query,
    })
}

// ---- fetching the fact an id names ------------------------------------------

/// The most ids one [`FETCH`](kinds::FETCH) may name.
///
/// Bounded for the reason a block's fact count is: a count read off a socket sizes an
/// allocation, and here it also buys a point read each. The number is what a *page* of
/// rows can plausibly name — a client expanding one row at a time never comes near it —
/// and a caller holding more ids than this has to ask twice, which is a loop it already
/// has.
pub const MAX_FETCH: usize = 4096;

/// One answer in a [`FETCHED`](kinds::FETCHED) reply.
///
/// # The **key**, and not the value side
///
/// A reference names a fact's *identity*, and the identity is the key
/// ([I11](../../../docs/invariants.md#i11)). Expanding one to its target's key,
/// recursively, is already the definition of a database's canonical logical form — it
/// is what `ops-I4`'s content hash is computed over, and what a producer sends when it
/// nests a reference instead of holding an id. Answering with the same thing means the
/// expanded form of a row and the form a producer would have written are one shape,
/// rather than two that have to be kept in step.
///
/// The value side is left out because it is a *different read* with a different cost
/// ([I6](../../../docs/invariants.md#i6)), and one a query can already ask for by name:
/// `X.value` projects it. Folding it in here would make every expansion pay for a
/// column family nobody asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The id this answers about. **Not encoded** — the reply is positional — and
    /// carried because the encoder needs it to find the key's type.
    pub id: FactId,
    pub found: Found,
}

/// What was there.
///
/// **Three answers rather than two, and the third is the interesting one.** An absence
/// means opposite things depending on what kind of predicate was asked about, and only
/// the server knows which: a *stored* fact cannot dangle — both column families are
/// written together ([I12](../../../docs/invariants.md#i12)) and ids are never reused
/// ([I11](../../../docs/invariants.md#i11)) — so a missing one is corruption and should
/// be said out loud. A **virtual** predicate's rows are a view of the server, materialised
/// per query, so one going missing between a query and an expansion of it is a database
/// having been created or removed in between, which is ordinary. Collapsing the two would
/// mean either crying corruption at a `db rm` or staying quiet about a damaged store, and
/// the client cannot tell them apart: virtuality belongs to the server, and the schema it
/// serves is *printed* with its virtual predicates written like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The fact's key.
    Key(WireValue),
    /// No such fact, in a predicate that stores them. Corruption, if the id came from a
    /// row.
    Missing,
    /// No such row, in a predicate that is **answered rather than stored** — so nothing
    /// was promised, and the listing has simply moved on.
    Unstored,
}

/// Encode a request naming the ids to resolve.
#[must_use]
pub fn encode_fetch(ids: &[FactId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + ids.len() * 4);
    varint::put_u64(&mut out, ids.len() as u64);

    for id in ids {
        // **The two halves, not the raw `u64`.** A fact id carries its predicate in the
        // *high* 24 bits (I11), so a varint of the whole number sets a continuation bit
        // in every byte — eight or nine of them, for every id. A predicate and a
        // sequence are one or two each, and `FactId::new` re-checks the pair on the way
        // back in, which the raw form would have skipped.
        varint::put_u64(&mut out, u64::from(id.predicate().0));
        varint::put_u64(&mut out, id.sequence());
    }

    out
}

/// Decode a fetch request.
///
/// # Errors
///
/// [`WireError::BlockTooLarge`] past [`MAX_FETCH`], [`WireError::BadFactId`] for a pair
/// that is not a fact id — sequence zero is reserved, so a zeroed frame is detectably
/// not one — or [`WireError::TrailingBytes`] if the frame says more than it counted.
pub fn decode_fetch(bytes: &[u8]) -> Result<Vec<FactId>, WireError> {
    let (count, mut at) = varint::get_u64(bytes)?;

    if count > MAX_FETCH as u64 {
        return Err(WireError::BlockTooLarge {
            what: "fetch ids",
            declared: count,
            max: MAX_FETCH as u64,
        });
    }

    let count = usize::try_from(count).map_err(|_| WireError::LengthOutOfRange {
        declared: count,
        available: bytes.len(),
    })?;

    let mut ids = Vec::with_capacity(count);

    for _ in 0..count {
        let (predicate, used) = varint::get_u64(&bytes[at..])?;
        at += used;

        let (sequence, used) = varint::get_u64(&bytes[at..])?;
        at += used;

        let predicate = u32::try_from(predicate)
            .map_err(|_| WireError::UnknownPredicate(u32::MAX))
            .map(PredicateId)?;

        ids.push(
            FactId::new(predicate, sequence)
                .map_err(|_| WireError::BadFactId((u64::from(predicate.0) << 40) | sequence))?,
        );
    }

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(ids)
}

/// Encode the answers, in the order they were asked about.
///
/// # Errors
///
/// [`WireError::UnknownPredicate`] for an id naming a predicate this schema does not
/// declare, or whatever [`encode_value`] reports about a key that does not fit the type
/// its own predicate declares.
pub fn encode_fetched(schema: &Schema, found: &[Fetched]) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::with_capacity(1 + found.len() * 8);
    varint::put_u64(&mut out, found.len() as u64);

    for answer in found {
        match &answer.found {
            Found::Key(key) => {
                out.push(PRESENT);
                encode_value(
                    &mut out,
                    schema,
                    key_ty(schema, answer.id.predicate())?,
                    key,
                )?;
            }
            // No key follows either way, so nothing has to be skipped: the reader consults
            // the schema for the *next* answer's type and this one contributed no bytes.
            Found::Missing => out.push(MISSING),
            Found::Unstored => out.push(UNSTORED),
        }
    }

    Ok(out)
}

/// Decode the answers to `asked`, in that order.
///
/// The ids are a parameter rather than something on the wire because they are what
/// says how to read the bytes: each key is encoded against its own predicate's key
/// type, and the predicate comes from the id ([I11](../../../docs/invariants.md#i11)
/// tags one). A reply is therefore only readable by the peer that asked, which is the
/// same property a row has against its descriptor.
///
/// # Errors
///
/// [`WireError::TypeMismatch`] if the reply answers a different number of ids than
/// were asked about, [`WireError::UnknownPredicate`] for an id this schema cannot
/// place, or whatever [`decode_value`] reports about the bytes.
pub fn decode_fetched(
    bytes: &[u8],
    schema: &Schema,
    asked: &[FactId],
) -> Result<Vec<Found>, WireError> {
    let (count, mut at) = varint::get_u64(bytes)?;

    if count != asked.len() as u64 {
        return Err(WireError::TypeMismatch(
            "a fetch reply answering a different number of ids than were asked about",
        ));
    }

    let mut out = Vec::with_capacity(asked.len());

    for id in asked {
        let present = *bytes.get(at).ok_or(WireError::UnexpectedEof)?;
        at += 1;

        match present {
            MISSING => out.push(Found::Missing),
            UNSTORED => out.push(Found::Unstored),
            PRESENT => {
                let (key, used) =
                    decode_value(&bytes[at..], schema, key_ty(schema, id.predicate())?)?;
                at += used;
                out.push(Found::Key(key));
            }
            other => return Err(WireError::UnknownRefForm(u64::from(other))),
        }
    }

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(out)
}

/// The three answers, as one byte each.
///
/// **Append only, like every other discriminant on this wire.** `0` and `1` are the two
/// this reply started with; `2` was added when an absence turned out to mean two different
/// things, which is why a reader refuses a byte it does not know rather than guessing at
/// the nearest one it does.
const MISSING: u8 = 0;
const PRESENT: u8 = 1;
const UNSTORED: u8 = 2;

/// A predicate's key type, which is what a fetched key is encoded against.
fn key_ty(
    schema: &Schema,
    predicate: PredicateId,
) -> Result<&aperture_schema::schema::PredicateTy, WireError> {
    Ok(&schema
        .get(predicate)
        .ok_or(WireError::UnknownPredicate(predicate.0))?
        .predicate()
        .key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::WireRef;
    use ::proptest::prelude::*;

    #[test]
    fn the_handshake_messages_round_trip() {
        let startup = Startup {
            version: VERSION,
            database: "code".to_owned(),
            mode: Mode::ReadWrite,
            schema_fingerprint: 0xDEAD_BEEF,
            // A subset claim, which is the shape with a length prefix in it — the one
            // a decoder that stopped at the fingerprint would read as trailing bytes.
            predicates: vec![("src.File".to_owned(), 7), ("src.Decl".to_owned(), 9)],
        };
        assert_eq!(decode_startup(&encode_startup(&startup)), Ok(startup));

        // And the shape a client carrying one constant sends: a number, no map.
        let carried = Startup {
            version: VERSION,
            database: "code".to_owned(),
            mode: Mode::ReadOnly,
            schema_fingerprint: 11,
            predicates: vec![],
        };
        assert_eq!(decode_startup(&encode_startup(&carried)), Ok(carried));

        let ready = Ready {
            version: VERSION,
            schema_fingerprint: 7,
            predicates: 12,
        };
        assert_eq!(decode_ready(&encode_ready(&ready)), Ok(ready));

        assert_eq!(
            decode_error(&encode_error(ErrorCode::SchemaMismatch, "nope")),
            Ok((ErrorCode::SchemaMismatch, "nope".to_owned()))
        );

        assert_eq!(decode_complete(&encode_complete(3, 4)), Ok((3, 4)));
    }

    #[test]
    fn a_truncated_handshake_is_refused_rather_than_defaulted() {
        let bytes = encode_startup(&Startup {
            version: VERSION,
            database: "code".to_owned(),
            mode: Mode::ReadOnly,
            schema_fingerprint: 1,
            predicates: vec![("src.File".to_owned(), 2)],
        });

        for cut in 0..bytes.len() {
            assert!(decode_startup(&bytes[..cut]).is_err(), "cut to {cut}");
        }
    }

    #[test]
    fn the_control_messages_round_trip() {
        for control in [
            // A create carrying a schema, which is the message with something after
            // its flag byte — the one a decoder that stopped early would get wrong.
            Control {
                op: ControlOp::Create,
                database: "code".to_owned(),
                allow_zero_facts: false,
                schema: "schema src { predicate File : string }".to_owned(),
            },
            Control {
                op: ControlOp::Create,
                database: "code".to_owned(),
                allow_zero_facts: false,
                schema: String::new(),
            },
            Control {
                op: ControlOp::Finish,
                database: "code".to_owned(),
                allow_zero_facts: true,
                schema: String::new(),
            },
            Control {
                op: ControlOp::Remove,
                database: String::new(),
                allow_zero_facts: false,
                schema: String::new(),
            },
        ] {
            let bytes = encode_control(&control);
            assert_eq!(decode_control(&bytes), Ok(control.clone()));

            // A cut message is refused rather than defaulted — the same rule the
            // handshake follows, and it matters more here: a `remove` decoded from a
            // truncated frame would name the wrong database.
            for cut in 0..bytes.len() {
                assert!(
                    decode_control(&bytes[..cut]).is_err(),
                    "{control:?} @ {cut}"
                );
            }
        }

        for reply in [
            ControlReply::Created {
                instance: "01JABCDEF".to_owned(),
            },
            ControlReply::Finished {
                fingerprint: u64::MAX,
                facts: 7,
                bytes: 4096,
                already_complete: true,
            },
            ControlReply::Removed,
        ] {
            let bytes = encode_control_reply(&reply);
            assert_eq!(decode_control_reply(&bytes), Ok(reply.clone()));

            for cut in 0..bytes.len() {
                assert!(
                    decode_control_reply(&bytes[..cut]).is_err(),
                    "{reply:?} @ {cut}"
                );
            }
        }
    }

    #[test]
    fn a_profile_round_trips() {
        let profile = QueryProfile {
            steps: vec![
                ProfileStep {
                    label: "src.Decl".to_owned(),
                    examined: 100_000,
                    full_scan: true,
                },
                ProfileStep {
                    label: "fetch src.File".to_owned(),
                    examined: 0,
                    full_scan: false,
                },
            ],
        };

        let bytes = encode_profile(&profile);
        assert_eq!(decode_profile(&bytes), Ok(profile.clone()));
        assert_eq!(profile.examined(), 100_000);

        for cut in 0..bytes.len() {
            assert!(decode_profile(&bytes[..cut]).is_err(), "cut to {cut}");
        }

        assert_eq!(
            decode_profile(&encode_profile(&QueryProfile::default())),
            Ok(QueryProfile::default())
        );
    }

    /// An op byte this build does not know is a refusal, not a guess. The
    /// discriminants are append-only, so a byte from the future means a peer that
    /// knows an operation we do not — and doing *some other* lifecycle operation
    /// instead is the worst possible answer.
    #[test]
    fn an_unknown_control_op_is_refused() {
        assert_eq!(ControlOp::from_byte(0), None);
        assert_eq!(ControlOp::from_byte(4), None);

        let mut bytes = encode_control(&Control {
            op: ControlOp::Remove,
            database: "code".to_owned(),
            allow_zero_facts: false,
            schema: String::new(),
        });
        bytes[0] = 4;

        assert!(decode_control(&bytes).is_err());
    }

    /// Trailing bytes are a fault, not slack: a peer whose idea of the message is
    /// longer than ours has a different protocol, and reading the prefix would let it
    /// think we agreed.
    #[test]
    fn trailing_bytes_in_a_handshake_are_a_fault() {
        let mut bytes = encode_ready(&Ready {
            version: VERSION,
            schema_fingerprint: 1,
            predicates: 1,
        });
        bytes.push(0);

        assert!(matches!(
            decode_ready(&bytes),
            Err(WireError::TrailingBytes(1))
        ));
    }

    // ---- fetching the fact an id names -------------------------------------

    /// `two.Ref : { to : two.Named }` over `two.Named : string` — the smallest schema
    /// with a reference in it, which is what an expansion is about.
    fn two_predicates() -> Schema {
        use aperture_schema::schema::{Predicate, PredicateTy};
        use lasso::Rodeo;
        use std::sync::Arc;

        let mut rodeo = Rodeo::new();
        let named = rodeo.get_or_intern("two.Named");
        let reference = rodeo.get_or_intern("two.Ref");
        let to = rodeo.get_or_intern("to");

        Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: named,
                    key: PredicateTy::Str,
                    value: None,
                },
                Predicate {
                    name: reference,
                    key: PredicateTy::Record(Arc::from(vec![(
                        to,
                        PredicateTy::Fact(PredicateId(0)),
                    )])),
                    value: None,
                },
            ]),
        )
    }

    fn id(predicate: u32, sequence: u64) -> FactId {
        FactId::new(PredicateId(predicate), sequence).expect("a fact id")
    }

    #[test]
    fn the_fetch_messages_round_trip() {
        let schema = two_predicates();
        let asked = vec![id(0, 1), id(1, 7), id(0, 4_000_000)];

        let request = encode_fetch(&asked);
        assert_eq!(decode_fetch(&request), Ok(asked.clone()));

        // **All three answers in one reply**, which is what exercises the presence byte
        // over its whole range — and the second is a *record* key, so a decoder that read
        // the wrong type for answer two would mis-pair every answer after it.
        let found = vec![
            Fetched {
                id: asked[0],
                found: Found::Key(WireValue::Str("store/codec.py".to_owned())),
            },
            Fetched {
                id: asked[1],
                found: Found::Key(WireValue::Record(Box::from([WireValue::Ref(WireRef::Id(
                    asked[0],
                ))]))),
            },
            Fetched {
                id: asked[2],
                found: Found::Missing,
            },
        ];

        let reply = encode_fetched(&schema, &found).expect("well-typed keys");
        assert_eq!(
            decode_fetched(&reply, &schema, &asked),
            Ok(vec![
                Found::Key(WireValue::Str("store/codec.py".to_owned())),
                Found::Key(WireValue::Record(Box::from([WireValue::Ref(WireRef::Id(
                    asked[0]
                ))]))),
                Found::Missing,
            ])
        );

        // **The two absences are distinct on the wire**, which is the whole reason the
        // third byte exists: one says a stored fact is not there, which is corruption, and
        // the other says a predicate the server *answers* had no such row, which is a
        // listing that moved on. A reply that could not tell them apart would leave the
        // client crying corruption at an ordinary `db rm`.
        let absences = vec![
            Fetched {
                id: asked[0],
                found: Found::Missing,
            },
            Fetched {
                id: asked[1],
                found: Found::Unstored,
            },
        ];
        let two = [asked[0], asked[1]];

        assert_eq!(
            decode_fetched(
                &encode_fetched(&schema, &absences).expect("no keys to type"),
                &schema,
                &two
            ),
            Ok(vec![Found::Missing, Found::Unstored])
        );

        // An empty ask is a well-formed message rather than a special case: a client
        // whose page held no references still has a code path.
        assert_eq!(decode_fetch(&encode_fetch(&[])), Ok(vec![]));
        assert_eq!(
            decode_fetched(&encode_fetched(&schema, &[]).unwrap(), &schema, &[]),
            Ok(vec![])
        );
    }

    /// A cut message is refused rather than defaulted — the rule the handshake and the
    /// control messages follow. It matters as much here: a reply truncated inside its
    /// third answer, read as two, would silently pair every id with the wrong fact.
    #[test]
    fn a_truncated_fetch_is_refused_rather_than_defaulted() {
        let schema = two_predicates();
        let asked = vec![id(0, 1), id(0, 2)];

        let request = encode_fetch(&asked);
        for cut in 0..request.len() {
            assert!(decode_fetch(&request[..cut]).is_err(), "cut to {cut}");
        }

        let reply = encode_fetched(
            &schema,
            &[
                Fetched {
                    id: asked[0],
                    found: Found::Key(WireValue::Str("a.py".to_owned())),
                },
                Fetched {
                    id: asked[1],
                    found: Found::Key(WireValue::Str("b.py".to_owned())),
                },
            ],
        )
        .expect("well-typed keys");

        for cut in 0..reply.len() {
            assert!(
                decode_fetched(&reply[..cut], &schema, &asked).is_err(),
                "cut to {cut}"
            );
        }

        let mut over = reply.clone();
        over.push(0);
        assert!(matches!(
            decode_fetched(&over, &schema, &asked),
            Err(WireError::TrailingBytes(1))
        ));
    }

    /// **The one fault positional pairing is exposed to**, and it is caught.
    ///
    /// A reply is read against the ids the caller still holds, so a reply that answers
    /// a different number of them is not a wrong answer to one id — it is every answer
    /// after the divergence attached to the wrong id. The count makes that a refusal.
    #[test]
    fn a_reply_that_answers_a_different_number_of_ids_is_refused() {
        let schema = two_predicates();
        let asked = vec![id(0, 1), id(0, 2)];

        let short = encode_fetched(
            &schema,
            &[Fetched {
                id: asked[0],
                found: Found::Key(WireValue::Str("a.py".to_owned())),
            }],
        )
        .expect("well-typed key");

        assert!(matches!(
            decode_fetched(&short, &schema, &asked),
            Err(WireError::TypeMismatch(_))
        ));
    }

    /// A count past the cap is refused before it sizes anything, and an id whose
    /// sequence is the reserved zero is refused as not being an id at all.
    #[test]
    fn a_fetch_a_peer_should_not_have_sent_is_refused() {
        let mut huge = vec![];
        varint::put_u64(&mut huge, MAX_FETCH as u64 + 1);
        assert!(matches!(
            decode_fetch(&huge),
            Err(WireError::BlockTooLarge {
                what: "fetch ids",
                ..
            })
        ));

        // Sequence zero is reserved (I11), so a zeroed frame is detectably not a fact
        // id rather than an id of fact zero.
        let mut zeroed = vec![];
        varint::put_u64(&mut zeroed, 1);
        varint::put_u64(&mut zeroed, 0);
        varint::put_u64(&mut zeroed, 0);
        assert!(matches!(
            decode_fetch(&zeroed),
            Err(WireError::BadFactId(_))
        ));
    }

    /// **An id costs its two halves, not its sixty-four bits.**
    ///
    /// The reason [`encode_fetch`] splits one: a fact id keeps its predicate in the top
    /// 24 bits, so a varint over the raw number sets a continuation bit in every byte
    /// it has. Stated as arithmetic rather than as a golden count, so it says why.
    #[test]
    fn an_id_costs_its_halves_rather_than_its_bits() {
        let low = encode_fetch(&[id(3, 7)]);
        assert_eq!(low.len(), 1 + 1 + 1, "count, predicate 3, sequence 7");

        let raw_would_be = {
            let mut out = vec![];
            varint::put_u64(&mut out, id(3, 7).raw());
            out.len()
        };
        assert!(
            raw_would_be > 2,
            "the raw form spends {raw_would_be} bytes on the same id"
        );
    }

    proptest! {
        /// Any key a fact can have, resolved and read back — the property the two
        /// small schemas above cannot cover, since a key is any type the schema
        /// language can write.
        #[test]
        fn a_fetched_key_round_trips(drawn in crate::value::proptest::arb_schema_and_fact()) {
            let schema = drawn.schema();
            let fact = drawn.fact(&schema);
            let asked = [FactId::new(fact.predicate, 12).expect("a fact id")];

            let reply = encode_fetched(
                &schema,
                &[Fetched { id: asked[0], found: Found::Key(fact.key.clone()) }],
            )?;

            prop_assert_eq!(
                decode_fetched(&reply, &schema, &asked)?,
                vec![Found::Key(fact.key)]
            );
        }
    }
}
