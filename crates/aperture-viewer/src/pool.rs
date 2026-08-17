//! A pool of connections, **recycled by query count**.
//!
//! The client is synchronous by design ([`aperture_client`]'s module docs say why),
//! and this server is async. So every question runs on a blocking thread with a
//! connection checked out of here, which is the ordinary shape for a blocking client
//! behind an async front end.
//!
//! # Why a pool needs a policy, not just a queue
//!
//! `bench/FINDINGS.md` §7 measured ~3.5 kB retained per query for the life of a
//! connection and named this exact shape: *"a connection pool is exactly the shape
//! that hits the bottom row, and it is the one to size RAM for."* Phase 11 fixed the
//! mechanism — a stream's task now ends when its work does, and the client recycles
//! stream ids — so the ceiling is gone rather than merely deferred.
//!
//! [`RETIRE_AFTER`] stays anyway, and the reason is worth writing down rather than
//! leaving as a habit: a long-lived connection accumulates *whatever* the server
//! attaches to a session, and this tier has no way to know when that changes. The
//! number is a guardrail against a future regression, not a workaround for a present
//! one — which is why it is generous.

use std::sync::{Arc, Mutex};

use aperture_client::{ClientError, Connection, Mode};
use aperture_schema::schema::Schema;

/// How many queries one connection answers before it is closed and replaced.
///
/// Generous on purpose: the leak this guards against is fixed, so the cost of the
/// policy should be as close to nothing as it can be while still being a policy.
const RETIRE_AFTER: u64 = 10_000;

/// One pooled connection, and how much work it has done.
struct Pooled {
    connection: Connection,
    served: u64,
}

/// Connections to one database, handed out one at a time.
pub struct Pool {
    socket: std::path::PathBuf,
    database: String,
    schema: Arc<Schema>,
    idle: Mutex<Vec<Pooled>>,
    /// The most connections to keep *idle*. Beyond this they are closed on return
    /// rather than kept, so a burst does not become a floor.
    capacity: usize,
}

impl Pool {
    #[must_use]
    pub fn new(
        socket: impl Into<std::path::PathBuf>,
        database: impl Into<String>,
        schema: Arc<Schema>,
        capacity: usize,
    ) -> Pool {
        Pool {
            socket: socket.into(),
            database: database.into(),
            schema,
            idle: Mutex::new(Vec::new()),
            capacity,
        }
    }

    /// Run `f` against a connection, returning it to the pool afterwards.
    ///
    /// **A connection that errored is not returned.** A stream-level fault leaves the
    /// connection usable and a transport fault does not, and this tier cannot tell
    /// them apart from out here — so it closes rather than hand back something that
    /// might be half a conversation. The cost is one reconnect per failed request.
    ///
    /// # Errors
    ///
    /// Whatever `f` returns, or [`ClientError::Io`] if a connection cannot be opened.
    pub fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        let mut held = match self.idle.lock().expect("the pool lock").pop() {
            Some(pooled) => pooled,
            None => Pooled {
                connection: Connection::connect(
                    &self.socket,
                    &self.database,
                    Arc::clone(&self.schema),
                    // **Read-only, and asserting nothing.** A viewer has no claim to
                    // make about the schema: the database's is the one that matters,
                    // and refusing to *read* one because a built-in copy had moved on
                    // would refuse the one thing that still works.
                    Mode::ReadOnly,
                    false,
                )?,
                served: 0,
            },
        };

        let out = f(&mut held.connection);
        held.served += 1;

        if out.is_ok() && held.served < RETIRE_AFTER {
            let mut idle = self.idle.lock().expect("the pool lock");
            if idle.len() < self.capacity {
                idle.push(held);
            }
        }

        out
    }
}
