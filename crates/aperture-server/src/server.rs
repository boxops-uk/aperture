//! The Unix socket listener.
//!
//! **Unix socket only, and that is `ops-I10` rather than a shortcut.** The design is
//! default-closed: TCP is an explicit opt-in the operator puts behind an
//! authenticated gateway, and a server that binds a network interface because nobody
//! said not to is the failure that rule exists to prevent. Adding the opt-in is a
//! second listener and a flag; leaving it out is the safe default, not an omission to
//! be tidied up later.
//!
//! # Binding is synchronous, accepting is not
//!
//! [`Listener::bind`] takes the socket with `std` and only converts to tokio's inside
//! [`run`](Listener::run). That is not fussiness: `tokio::net::UnixListener::bind`
//! requires a runtime context, so binding there would mean a caller could not find out
//! whether the socket was available until it had already started a runtime — and the
//! *readiness file* has to be written after a successful bind and before anything
//! connects, which is much easier to get right when binding is an ordinary fallible
//! call.
//!
//! One task per connection, spawned onto the runtime. The blocking half of a
//! connection's work — every fjall read and write — goes to
//! [`spawn_blocking`](tokio::task::spawn_blocking) from inside
//! [`session`](crate::session), so a task here is doing framing and nothing else.

use std::{
    fs,
    io::Write,
    os::unix::net::UnixListener as StdUnixListener,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::net::{UnixListener, UnixStream};

use crate::{error::ServerError, registry::Registry};

/// A bound listener, and the socket path it owns.
pub struct Listener {
    listener: StdUnixListener,
    path: PathBuf,
}

impl Listener {
    /// Bind the socket at `path`.
    ///
    /// A stale socket file from a killed server is removed first. That is safe only
    /// because `ops-I1` gives one process ownership of the store root — the lock on
    /// the data directory is what says nobody is serving it, and the socket file is a
    /// consequence rather than the lock itself.
    ///
    /// # Errors
    ///
    /// [`ServerError::Io`] if the socket cannot be bound.
    pub fn bind(path: impl AsRef<Path>) -> Result<Listener, ServerError> {
        let path = path.as_ref().to_path_buf();

        if path.exists() {
            fs::remove_file(&path)?;
        }

        let listener = StdUnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;

        Ok(Listener { listener, path })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the readiness file, **after** the listener is accepting.
    ///
    /// Glean's `--write-port`, and the ordering is the whole of it: a signal that
    /// appears before the listener does is a race dressed as a signal, and a test that
    /// waits on it would connect to nothing and blame the server. Because
    /// [`bind`](Self::bind) has already taken the socket by the time this is called,
    /// a client that sees the file can connect.
    ///
    /// # Errors
    ///
    /// [`ServerError::Io`] if the file cannot be written.
    pub fn announce(&self, at: impl AsRef<Path>) -> Result<(), ServerError> {
        let mut file = fs::File::create(at)?;
        file.write_all(self.path.as_os_str().as_encoded_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// Accept forever, serving each connection on its own task.
    ///
    /// # Errors
    ///
    /// [`ServerError::Io`] if accepting fails. A *connection* failing never reaches
    /// here: it ends that connection and the server carries on, because one client
    /// sending nonsense is not a reason to stop serving the others.
    pub async fn run(self, registry: Arc<Registry>) -> Result<(), ServerError> {
        let listener = UnixListener::from_std(self.listener.try_clone()?)?;

        loop {
            let (stream, _address) = listener.accept().await?;
            let registry = Arc::clone(&registry);

            tokio::spawn(async move {
                if let Err(error) = serve_stream(stream, &registry).await {
                    eprintln!("connection ended: {error}");
                }
            });
        }
    }

    /// [`run`](Self::run) on a runtime of its own, for a caller that has none.
    ///
    /// # Errors
    ///
    /// [`ServerError::Io`] if the runtime cannot be built, or whatever `run` reports.
    pub fn run_blocking(self, registry: Arc<Registry>) -> Result<(), ServerError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        runtime.block_on(self.run(registry))
    }
}

impl Drop for Listener {
    /// Take the socket file with it. A leftover file is what the next `bind` has to
    /// clean up, and leaving one behind makes "is a server running?" ambiguous —
    /// which §2 says the socket is supposed to answer.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Serve one accepted connection.
///
/// # Errors
///
/// Whatever [`session::serve`](crate::session::serve) reports as fatal.
pub async fn serve_stream(stream: UnixStream, registry: &Arc<Registry>) -> Result<(), ServerError> {
    // Split rather than cloned: the session holds a buffered reader and a buffered
    // writer at once, and `into_split` is what gives it two independently-owned halves
    // of one socket.
    let (reader, writer) = stream.into_split();
    crate::session::serve(reader, writer, registry).await
}

/// Bind, announce, and serve — the whole of what a `serve` command does, on a runtime
/// of its own.
///
/// # Errors
///
/// [`ServerError::Io`] if the socket cannot be bound or the readiness file written.
pub fn serve_unix(
    socket: impl AsRef<Path>,
    ready_file: Option<&Path>,
    registry: Arc<Registry>,
) -> Result<(), ServerError> {
    let listener = Listener::bind(socket)?;

    if let Some(at) = ready_file {
        listener.announce(at)?;
    }

    listener.run_blocking(registry)
}
