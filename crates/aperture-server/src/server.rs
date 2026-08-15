//! The Unix socket listener.
//!
//! **Unix socket only, and that is `ops-I10` rather than a shortcut.** The design is
//! default-closed: TCP is an explicit opt-in the operator puts behind an
//! authenticated gateway, and a server that binds a network interface because nobody
//! said not to is the failure that rule exists to prevent. Adding the opt-in is a
//! second listener and a flag; leaving it out is the safe default, not an omission to
//! be tidied up later.
//!
//! One thread per connection. §5 asks for a per-connection writer task that fairly
//! interleaves ready streams, which this is not — see [`session`](crate::session) for
//! what that would change and what it would not. A thread per connection is enough
//! for P0's `max_connections` and needs no runtime, which keeps a large dependency
//! decision out of a phase that does not need to make it.

use std::{
    fs,
    io::Write,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use crate::{error::ServerError, session::Database};

/// A bound listener, and the socket path it owns.
pub struct Listener {
    listener: UnixListener,
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

        let listener = UnixListener::bind(&path)?;
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
    /// waits on it would connect to nothing and blame the server.
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

    /// Accept forever, serving each connection on its own thread.
    ///
    /// # Errors
    ///
    /// [`ServerError::Io`] if accepting fails. A *connection* failing never reaches
    /// here: it ends that connection and the server carries on, because one client
    /// sending nonsense is not a reason to stop serving the others.
    pub fn run(&self, databases: Vec<Arc<Database>>) -> Result<(), ServerError> {
        for stream in self.listener.incoming() {
            let stream = stream?;
            let databases = databases.clone();

            thread::spawn(move || {
                if let Err(error) = serve_stream(stream, &databases) {
                    eprintln!("connection ended: {error}");
                }
            });
        }

        Ok(())
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
pub fn serve_stream(stream: UnixStream, databases: &[Arc<Database>]) -> Result<(), ServerError> {
    // Two handles on one socket: the session reads from one and writes to the other,
    // which is what lets it hold a buffered reader and a buffered writer at once
    // without either borrowing the other.
    let reader = stream.try_clone()?;
    crate::session::serve(reader, stream, databases)
}

/// Bind, announce, and serve — the whole of what a `serve` command does.
///
/// # Errors
///
/// [`ServerError::Io`] if the socket cannot be bound or the readiness file written.
pub fn serve_unix(
    socket: impl AsRef<Path>,
    ready_file: Option<&Path>,
    databases: Vec<Arc<Database>>,
) -> Result<(), ServerError> {
    let listener = Listener::bind(socket)?;

    if let Some(at) = ready_file {
        listener.announce(at)?;
    }

    listener.run(databases)
}
