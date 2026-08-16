//! **What the server is doing, counted — and nothing else.**
//!
//! There is deliberately no exporter, no endpoint, no stats file and no format. That is
//! not an omission to be filled in later by whoever needs a number: exposing this is a
//! *separate* decision with an operational cost, and the counters are worth having
//! before it is made. A `/metrics` listener would be a second port on a server whose
//! [`ops-I10`](../../../docs/aperture-cli-design.md) safety argument rests on binding
//! being default-closed, and the durable home the design already names is a virtual
//! predicate over the socket that exists. Either is an adapter over what is here.
//!
//! So what this module owes is that the *counting* be right, because a counter that
//! drifts is worse than none — it reads as a leak that is not there, or hides one that
//! is.
//!
//! # Gauges are guards, not increments
//!
//! Two of these go up and down, and both count things that end on many paths — a task
//! that returns, fails, is cancelled, or is dropped mid-await. A matched
//! `fetch_add`/`fetch_sub` pair written at what look like the entry and the exit is
//! wrong the first time somebody adds a `?`. [`Live`] decrements on `Drop`, so the
//! count is right on every path there is, including the ones nobody thought of.
//!
//! That is not a hypothetical worry here. The bug these were built for
//! (`bench/FINDINGS.md` §10) was precisely a task that never reached its exit.
//!
//! # Relaxed, and why that is enough
//!
//! Every counter is [`Ordering::Relaxed`]. Nothing branches on these values — they are
//! read by tests and, eventually, by whatever reports them — so there is nothing for a
//! stronger ordering to protect. What relaxed does *not* give is a consistent snapshot
//! across counters, which is why a reader should treat two of them as two facts rather
//! than as a ratio taken at one instant.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering::Relaxed},
};

/// One server's counters.
///
/// Per-server rather than global, because a process can run two — the tests do — and a
/// static would have them counting each other's work.
#[derive(Debug, Default)]
pub struct ServerStats {
    connections_open: AtomicU64,
    streams_live: AtomicU64,
    connections_total: AtomicU64,
    queries_started: AtomicU64,
    queries_completed: AtomicU64,
    queries_failed: AtomicU64,
    chunks: AtomicU64,
    rows_sent: AtomicU64,
    blocking_dispatches: AtomicU64,
    blocking_wait_micros: AtomicU64,
    queue_full_waits: AtomicU64,
}

/// Which gauge a [`Live`] is holding down.
#[derive(Debug, Clone, Copy)]
enum Gauge {
    Connection,
    Stream,
}

/// A gauge held up for as long as this value lives.
///
/// Owns an `Arc` rather than borrowing, because the things being counted outlive the
/// stack frame that starts them: a stream's task is spawned and then nobody holds it.
#[derive(Debug)]
pub struct Live {
    stats: Arc<ServerStats>,
    gauge: Gauge,
}

impl Drop for Live {
    fn drop(&mut self) {
        let counter = match self.gauge {
            Gauge::Connection => &self.stats.connections_open,
            Gauge::Stream => &self.stats.streams_live,
        };
        counter.fetch_sub(1, Relaxed);
    }
}

impl ServerStats {
    /// A connection has been established; the gauge falls when the returned value is
    /// dropped.
    pub fn connection_opened(self: &Arc<Self>) -> Live {
        self.connections_open.fetch_add(1, Relaxed);
        self.connections_total.fetch_add(1, Relaxed);
        Live {
            stats: Arc::clone(self),
            gauge: Gauge::Connection,
        }
    }

    /// A stream's task has started; the gauge falls when the task ends, however it ends.
    pub fn stream_opened(self: &Arc<Self>) -> Live {
        self.streams_live.fetch_add(1, Relaxed);
        Live {
            stats: Arc::clone(self),
            gauge: Gauge::Stream,
        }
    }

    pub fn query_started(&self) {
        self.queries_started.fetch_add(1, Relaxed);
    }

    pub fn query_completed(&self) {
        self.queries_completed.fetch_add(1, Relaxed);
    }

    pub fn query_failed(&self) {
        self.queries_failed.fetch_add(1, Relaxed);
    }

    /// One chunk computed, and the rows it produced sent.
    pub fn chunk_sent(&self, rows: u64) {
        self.chunks.fetch_add(1, Relaxed);
        self.rows_sent.fetch_add(rows, Relaxed);
    }

    /// One hop to the blocking pool, and how long it waited before starting.
    ///
    /// The wait is the interesting half: a dispatch that starts immediately says the
    /// pool has room, and one that waits says the server is queueing work it cannot yet
    /// do — which is the only visibility there is into a pool with no admission control
    /// in front of it (`F8`).
    pub fn blocking_dispatched(&self, waited_micros: u64) {
        self.blocking_dispatches.fetch_add(1, Relaxed);
        self.blocking_wait_micros.fetch_add(waited_micros, Relaxed);
    }

    /// A stream found its outbound queue full and had to wait for room.
    ///
    /// Worth its own counter because it is the precondition of the worst bug this
    /// server has had: a producer waiting here when the writer has already died.
    pub fn queue_full_wait(&self) {
        self.queue_full_waits.fetch_add(1, Relaxed);
    }

    #[must_use]
    pub fn connections_open(&self) -> u64 {
        self.connections_open.load(Relaxed)
    }

    /// **Live stream tasks** — the count that says whether work is being stranded.
    #[must_use]
    pub fn streams_live(&self) -> u64 {
        self.streams_live.load(Relaxed)
    }

    #[must_use]
    pub fn connections_total(&self) -> u64 {
        self.connections_total.load(Relaxed)
    }

    #[must_use]
    pub fn queries_started(&self) -> u64 {
        self.queries_started.load(Relaxed)
    }

    #[must_use]
    pub fn queries_completed(&self) -> u64 {
        self.queries_completed.load(Relaxed)
    }

    #[must_use]
    pub fn queries_failed(&self) -> u64 {
        self.queries_failed.load(Relaxed)
    }

    #[must_use]
    pub fn chunks(&self) -> u64 {
        self.chunks.load(Relaxed)
    }

    #[must_use]
    pub fn rows_sent(&self) -> u64 {
        self.rows_sent.load(Relaxed)
    }

    #[must_use]
    pub fn blocking_dispatches(&self) -> u64 {
        self.blocking_dispatches.load(Relaxed)
    }

    #[must_use]
    pub fn blocking_wait_micros(&self) -> u64 {
        self.blocking_wait_micros.load(Relaxed)
    }

    #[must_use]
    pub fn queue_full_waits(&self) -> u64 {
        self.queue_full_waits.load(Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the guard exists for: the gauge falls on a path nobody wrote an
    /// exit for.
    #[test]
    fn a_gauge_falls_however_its_holder_ends() {
        let stats = Arc::new(ServerStats::default());

        let held = stats.stream_opened();
        assert_eq!(stats.streams_live(), 1);

        // Not `drop(held)` — a panic unwinding past it is the path a hand-written
        // decrement misses, and the one that matters.
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = held;
            panic!("a stream's task failing is an ordinary outcome");
        }));

        assert!(unwound.is_err());
        assert_eq!(
            stats.streams_live(),
            0,
            "the gauge must fall when its holder is dropped by an unwind"
        );
    }

    /// Two servers in one process count separately, which is the reason these are not
    /// statics.
    #[test]
    fn two_servers_do_not_share_counters() {
        let one = Arc::new(ServerStats::default());
        let other = Arc::new(ServerStats::default());

        let _held = one.connection_opened();

        assert_eq!(one.connections_open(), 1);
        assert_eq!(other.connections_open(), 0);
    }
}
