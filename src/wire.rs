//! Talking to a running server.
//!
//! A **control session**: bound to no database, opened for one request and closed
//! after it. That is all a lifecycle command needs, and it is deliberately not the
//! whole of a client — connecting, multiplexing, a write stream and a query stream
//! that holds its cursor are [`aperture-client`](../PLAN.md)'s, in 9e. When that lands
//! this module becomes three calls into it rather than three of its own.
//!
//! # Synchronous, and that is the point
//!
//! The server is async; this is a `std::os::unix::net::UnixStream` and a loop. A
//! client written against the wire format should need nothing of the server's runtime,
//! and a CLI that started a tokio runtime to send two frames would be evidence against
//! that. The socket tests make the same choice for the same reason.
//!
//! # The address rule, in one place
//!
//! §2 says the socket *is* the server-detection mechanism, and that there is no other
//! autodetect. So [`connect`] answers exactly one question — is one listening? — and
//! [`commands::route`](crate::commands::route) is what turns the answer into a
//! decision. Nothing here falls back to opening a directory: `ops-I1`'s refusal is
//! that there is never a *silent* fallback from connect to open, and a caller that
//! knows no server is listening is not falling back from anything.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
};

use aperture_store::catalog::Finished;
use aperture_wire::{
    Control, ControlOp, ControlReply, FrameKind, Mode, Startup, StreamId, encode_frame, frame,
    protocol::{self, kinds},
};

use crate::{CliError, code_index};

/// The one stream a control session uses. Nothing else shares this connection.
const STREAM: StreamId = StreamId(1);

/// A connection to a running server, already handshaken.
pub struct Server {
    stream: UnixStream,
}

/// Connect to the server at `socket`, or answer that none is listening.
///
/// **A missing socket and a refused one are the same answer**: no server. The first is
/// a root nothing has served; the second is the file a killed server left behind, and
/// treating it as "a server is there" would refuse every command until someone deleted
/// a stale inode by hand. Anything else — a socket that exists and will not talk to us
/// — is reported rather than assumed away, because that is a server we are being kept
/// out of, not the absence of one.
///
/// # Errors
///
/// [`CliError::Server`] if the socket is there and cannot be used, or if the handshake
/// fails — including a server whose schema is not the one this build was compiled
/// against, which is worth knowing before it makes a database.
pub fn connect(socket: &Path) -> Result<Option<Server>, CliError> {
    use std::io::ErrorKind;

    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(aperture_server::ServerError::Io(error).into()),
    };

    let mut server = Server { stream };
    server.hello()?;
    Ok(Some(server))
}

impl Server {
    /// Open a control session: no database named, and read-write, because every
    /// operation it exists to perform changes one.
    ///
    /// The fingerprint is **asserted rather than accepted**. A tool whose built-in
    /// schema is not the server's would otherwise create a database against a schema
    /// it does not have, and find out by writing facts nobody can read back — which is
    /// precisely the mismatch the handshake field exists to catch early.
    fn hello(&mut self) -> Result<(), CliError> {
        let startup = protocol::encode_startup(&Startup {
            version: protocol::VERSION,
            database: String::new(),
            mode: Mode::ReadWrite,
            schema_fingerprint: protocol::provisional_fingerprint(&code_index::schema()),
        });

        self.send(kinds::STARTUP, &startup)?;

        let (kind, payload) = self.recv()?;
        if kind == kinds::READY {
            return Ok(());
        }

        Err(refusal(kind, &payload))
    }

    /// Create a database, and answer with the provisional instance it was given.
    ///
    /// # Errors
    ///
    /// [`CliError::Refused`] if the server declines — a name already taken, a name
    /// that cannot be a directory.
    pub fn create(&mut self, name: &str) -> Result<String, CliError> {
        match self.request(ControlOp::Create, name, false)? {
            ControlReply::Created { instance } => Ok(instance),
            other => Err(mismatched(&other)),
        }
    }

    /// Seal a database.
    ///
    /// # Errors
    ///
    /// [`CliError::Refused`] if the server declines — no such database, no facts and
    /// no flag.
    pub fn finish(&mut self, name: &str, allow_zero_facts: bool) -> Result<Finished, CliError> {
        match self.request(ControlOp::Finish, name, allow_zero_facts)? {
            ControlReply::Finished {
                fingerprint,
                facts,
                bytes,
                already_complete,
            } => Ok(Finished {
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
    /// [`CliError::Refused`] if the server declines — no such database, or one a
    /// session still holds.
    pub fn remove(&mut self, name: &str) -> Result<(), CliError> {
        match self.request(ControlOp::Remove, name, false)? {
            ControlReply::Removed => Ok(()),
            other => Err(mismatched(&other)),
        }
    }

    fn request(
        &mut self,
        op: ControlOp,
        database: &str,
        allow_zero_facts: bool,
    ) -> Result<ControlReply, CliError> {
        let request = protocol::encode_control(&Control {
            op,
            database: database.to_owned(),
            allow_zero_facts,
        });

        self.send(kinds::CONTROL, &request)?;

        let (kind, payload) = self.recv()?;
        if kind != kinds::CONTROL_REPLY {
            return Err(refusal(kind, &payload));
        }

        Ok(protocol::decode_control_reply(&payload).map_err(aperture_server::ServerError::Wire)?)
    }

    fn send(&mut self, kind: FrameKind, payload: &[u8]) -> Result<(), CliError> {
        let mut out = Vec::with_capacity(frame::HEADER_LEN + payload.len());
        encode_frame(&mut out, kind, STREAM, payload)
            .map_err(aperture_server::ServerError::Wire)?;

        self.stream
            .write_all(&out)
            .map_err(aperture_server::ServerError::Io)?;

        Ok(())
    }

    fn recv(&mut self) -> Result<(FrameKind, Vec<u8>), CliError> {
        let mut head = [0u8; frame::HEADER_LEN];
        self.stream
            .read_exact(&mut head)
            .map_err(aperture_server::ServerError::Io)?;

        let header = frame::decode_header(&head).map_err(aperture_server::ServerError::Wire)?;

        let mut payload = vec![0u8; header.length as usize];
        self.stream
            .read_exact(&mut payload)
            .map_err(aperture_server::ServerError::Io)?;

        Ok((header.kind, payload))
    }
}

/// Turn a frame that is not the expected answer into the error a person reads.
///
/// The [`ErrorCode`](aperture_server::ErrorCode) is deliberately dropped: it exists so
/// a *program* can branch without parsing English, and this tool has nothing to branch
/// on — it prints the message and exits. A client that grows a reason to branch should
/// carry the code rather than reconstruct it from the words.
fn refusal(kind: FrameKind, payload: &[u8]) -> CliError {
    if kind != FrameKind::ERROR {
        return CliError::Refused(format!(
            "the server answered with an unexpected frame `{kind}`"
        ));
    }

    match protocol::decode_error(payload) {
        Ok((_code, message)) => CliError::Refused(message),
        Err(error) => {
            CliError::Refused(format!("the server's error frame did not decode: {error}"))
        }
    }
}

/// A reply to a request that was not the one asked.
///
/// Not a refusal by the server but a disagreement with it, and the reply carries the op
/// byte precisely so this is detectable rather than silently misread.
fn mismatched(reply: &ControlReply) -> CliError {
    CliError::Refused(format!(
        "the server answered a different operation than the one asked: {reply:?}"
    ))
}
