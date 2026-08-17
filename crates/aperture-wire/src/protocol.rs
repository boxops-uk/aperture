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

use crate::{WireError, varint};

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

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(Startup {
        version: u32::try_from(version).map_err(|_| WireError::TypeMismatch("version"))?,
        database,
        mode,
        schema_fingerprint,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_messages_round_trip() {
        let startup = Startup {
            version: VERSION,
            database: "code".to_owned(),
            mode: Mode::ReadWrite,
            schema_fingerprint: 0xDEAD_BEEF,
        };
        assert_eq!(decode_startup(&encode_startup(&startup)), Ok(startup));

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
}
