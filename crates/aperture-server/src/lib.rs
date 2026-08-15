//! **The server** — the wire protocol, over a socket, in front of a real store.
//!
//! Phase 7a's last piece: a client connects, handshakes, opens a write stream, sends
//! blocks of facts, and queries them back on the same connection. What it is made of
//! is almost entirely other crates —
//! [`aperture-wire`](aperture_wire) frames and encodes,
//! [`aperture-ingest`](aperture_ingest) interns and writes,
//! [`aperture-engine`](aperture_engine) compiles and runs — so what lives here is the
//! *conversation*: which frame means what, and what a stream's life looks like.
//!
//! The **message vocabulary** is not here: it is
//! [`aperture_wire::protocol`], shared with `aperture-client`, because nothing should
//! have to depend on a server to speak to one
//! ([operations §10](../../docs/aperture-cli-design.md)).
//!
//! - [`session`] — one connection, from handshake to close.
//! - [`registry`] — the store root and the databases open under it, which is what
//!   makes `create`, `finish` and `remove` work *against a running server*.
//! - [`outbound`] — the fair writer: per-stream queues, round-robin, bounded.
//! - [`rows`] — a query result on the wire, without a fourth encoder appearing.
//! - [`blocking`] — the hop off the reactor that everything touching a store takes.
//! - [`server`] — the Unix socket listener, and the readiness file a test waits on.
//!
//! # What is deliberately not built
//!
//! Named here rather than discovered, and each is named as deferred in
//! [operations §5](../../docs/aperture-cli-design.md) too:
//!
//! - **Per-stream flow-control windows**, explicitly past P0: bounded per-stream
//!   queues plus connection backpressure are what §5 says to start with, and are what
//!   [`outbound`] does.
//! - **Remote `list` and `describe`.** Locally they need nothing from the server —
//!   `ops-I7` reads sidecars and never opens fjall, so they already work while it
//!   holds every database. The remote branch is the virtual predicate
//!   `aperture.db.List` through the normal query machinery, in Phase 9f.
//! - **TCP.** `ops-I10` is default-closed: a Unix socket only, with TCP an explicit
//!   opt-in behind an authenticated gateway. The opt-in flag is not wired yet, and
//!   binding a network interface is not something to do by accident.
//! - **Authentication.** `ops-I10` again: the handshake has a reserved credential slot
//!   and accepts anonymous. Access control is the transport's job — socket
//!   permissions, or the gateway in front of opted-in TCP.

pub(crate) mod blocking;
pub mod error;
pub mod outbound;
pub mod registry;
pub mod rows;
pub mod server;
pub mod session;

pub use error::ServerError;
pub use registry::Registry;
pub use server::{Listener, serve_unix};
pub use session::{Database, serve};
