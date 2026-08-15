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
//! - [`protocol`] — the message vocabulary. Kept apart from the codec so a client can
//!   be written against the wire format without adopting a server's idea of a
//!   session, which is what the .NET client under `clients/dotnet` does.
//! - [`session`] — one connection, from handshake to close.
//! - [`rows`] — a query result on the wire, without a fourth encoder appearing.
//! - [`server`] — the Unix socket listener, and the readiness file a test waits on.
//!
//! # What is deliberately not built
//!
//! Named here rather than discovered, and each is named as deferred in
//! [operations §5](../../docs/aperture-cli-design.md) too:
//!
//! - **Fair interleaving between streams.** Frames carry a stream id and the server
//!   honours it, so two streams coexist on a connection; but frames are processed to
//!   completion as they arrive rather than on per-stream tasks, so a long query does
//!   delay a short one behind it. That is a scheduler on top of this loop, not a
//!   different loop.
//! - **Chunked incremental results.** A query's rows are collected and then sent. The
//!   executor already suspends — `enumerate` returns `Suspended` — so what is missing
//!   is the loop that resumes it between chunks, not the machinery under it.
//! - **In-band cancellation** and **per-stream flow control**, both explicitly past
//!   P0.
//! - **TCP.** `ops-I10` is default-closed: a Unix socket only, with TCP an explicit
//!   opt-in behind an authenticated gateway. The opt-in flag is not wired yet, and
//!   binding a network interface is not something to do by accident.
//! - **Authentication.** `ops-I10` again: the handshake has a reserved credential slot
//!   and accepts anonymous. Access control is the transport's job — socket
//!   permissions, or the gateway in front of opted-in TCP.

pub mod error;
pub mod protocol;
pub mod rows;
pub mod server;
pub mod session;

pub use error::ServerError;
pub use protocol::{ErrorCode, Mode, Ready, Startup, VERSION};
pub use server::{Listener, serve_unix};
pub use session::{Database, serve};
