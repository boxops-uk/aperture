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
pub const VERSION: u32 = 1;

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

    if at != bytes.len() {
        return Err(WireError::TrailingBytes(bytes.len() - at));
    }

    Ok(Control {
        op,
        database,
        allow_zero_facts,
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

/// A **provisional** schema fingerprint.
///
/// Chapter 6 specifies the real one — canonical form, per-predicate fingerprints,
/// then a whole-schema fingerprint — and it is
/// [Phase 8](../../../PLAN.md)'s, because there is no schema *syntax* to canonicalise
/// until schemas are parsed. This is not that. It is a stable hash over the predicate
/// names and types a `Schema` holds, so that a client and a server disagreeing about
/// the schema find out at the handshake instead of by writing facts nobody can read.
///
/// **Delete this when Phase 8 lands**, and bump [`VERSION`] when you do: the number
/// will change, which is the whole point of a fingerprint, and a client pinned to the
/// old one should be told rather than left to mismatch.
#[must_use]
pub fn provisional_fingerprint(schema: &aperture_schema::schema::Schema) -> u64 {
    use aperture_schema::schema::PredicateTy;

    // FNV-1a, 64-bit: small, dependency-free, and adequate for "did we mean the same
    // schema" — this is not a security boundary and not the identity of a database.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;

    fn feed(hash: &mut u64, bytes: &[u8]) {
        for &byte in bytes {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(PRIME);
        }
    }

    fn feed_ty(hash: &mut u64, schema: &aperture_schema::schema::Schema, shape: &PredicateTy) {
        match shape {
            PredicateTy::Int => feed(hash, b"int"),
            PredicateTy::Str => feed(hash, b"str"),
            PredicateTy::Fact(id) => {
                feed(hash, b"fact");
                feed(hash, &id.0.to_le_bytes());
            }
            PredicateTy::Record(fields) => {
                feed(hash, b"record");
                feed(hash, &(fields.len() as u64).to_le_bytes());
                for (name, field) in fields.iter() {
                    feed(
                        hash,
                        schema.interner().resolve(*name).unwrap_or("?").as_bytes(),
                    );
                    feed_ty(hash, schema, field);
                }
            }
        }
    }

    let mut hash = OFFSET;

    for index in 0..schema.len() {
        let id = aperture_schema::schema::PredicateId(index as u32);
        let Some(predicate) = schema.get(id) else {
            continue;
        };

        // A predicate with no resolvable name still has to contribute *something*
        // distinct, or two of them would be indistinguishable in the hash.
        feed(&mut hash, predicate.name().unwrap_or("\u{0}?").as_bytes());
        feed_ty(&mut hash, schema, predicate.key().ty);

        match predicate.value() {
            Some(value) => {
                feed(&mut hash, b"+value");
                feed_ty(&mut hash, schema, value.ty);
            }
            None => feed(&mut hash, b"-value"),
        }
    }

    // Never zero: zero is the client's "do not check", so a schema that happened to
    // hash to it would silently disable the check for everyone.
    if hash == 0 { 1 } else { hash }
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
            Control {
                op: ControlOp::Create,
                database: "code".to_owned(),
                allow_zero_facts: false,
            },
            Control {
                op: ControlOp::Finish,
                database: "code".to_owned(),
                allow_zero_facts: true,
            },
            Control {
                op: ControlOp::Remove,
                database: String::new(),
                allow_zero_facts: false,
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
