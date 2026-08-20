//! A pool of connections, **recycled by query count**.
//!
//! The client is synchronous by design ([`fjord_client`]'s module docs say why),
//! and this server is async. So every question runs on a blocking thread with a
//! connection checked out of here, which is the ordinary shape for a blocking client
//! behind an async front end.
//!
//! [`RETIRE_AFTER`] is a guardrail, not a workaround: a long-lived connection
//! accumulates *whatever* the server attaches to a session, and this tier has no way
//! to know when that changes (`bench/FINDINGS.md` §7 is the incident that proved the
//! shape — a pool is exactly what hits a per-query retention ceiling). The mechanism
//! it guarded against is fixed; the number stays generous because it now guards a
//! regression.

use std::sync::{Arc, Mutex};

use fjord_client::{Address, ClientError, Connection, Mode};
use fjord_schema::schema::Schema;

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
    address: Address,
    schema: Arc<Schema>,
    idle: Mutex<Vec<Pooled>>,
    /// The most connections to keep *idle*. Beyond this they are closed on return
    /// rather than kept, so a burst does not become a floor.
    capacity: usize,
}

impl Pool {
    #[must_use]
    pub fn new(address: Address, schema: Arc<Schema>, capacity: usize) -> Pool {
        Pool {
            address,
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
                connection: Connection::open(
                    self.address.endpoint().expect("a resolved address"),
                    self.address.database(),
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
