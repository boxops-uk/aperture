//! **The transport codec** — how a fact travels, as against how one is stored.
//!
//! There are two codecs in Aperture and they share no bytes, no code and no
//! constraints. Blurring them is the mistake this crate exists to make structurally
//! impossible, so it is worth saying once, at the top, what each is for:
//!
//! | | storage — `aperture-encoding` | transport — here |
//! |---|---|---|
//! | read by | the executor, off disk, in the scan hot loop | a peer, off a socket |
//! | ordered? | **yes** — `memcmp` *is* semantic order ([I1]) | no. Nothing memcmps a frame |
//! | self-delimiting? | **yes** — skip a field with no schema ([I2]) | no. The reader has the schema |
//! | frozen? | **yes**, the moment data exists ([I3]) | no — versioned by the handshake, and a stream is a moment long |
//! | optimised for | seeking, skipping, ranges | **bytes on the wire, and nothing else** |
//!
//! Every marker byte in the storage codec buys one of the first three properties.
//! None of them is worth anything on a socket, so none of them is here. What replaces
//! them is the observation that **both peers already have the schema** — the handshake
//! compares fingerprints before any data flows, and [I13] freezes a DB's schema at
//! create — which means field names, field order, arities and types need not be sent
//! at all.
//!
//! That is Avro's model, arrived at from Avro's premise. Avro: *"Binary encoded Avro
//! data does not include type information or field names"*, and so *"a schema must
//! always be used in order to read Avro data correctly"*. The alternative — Protocol
//! Buffers' and Thrift's per-field tags — buys readers that do **not** have the
//! writer's schema, which is a property this connection has already established by
//! other means and would be paying for twice.
//!
//! ```text
//!   src.Decl { module = <src.Module …>, name = "key_of", line = 12 }
//!
//!   storage    22 51 <8-byte id> 21 6B 65 79 5F 6F 66 00 49 0C 00
//!              └ record          └ string, escaped, terminated
//!                                              └ int: marker + magnitude
//!
//!   transport  01 <nested fact…> 06 6B 65 79 5F 6F 66 18
//!              └ union branch    └ len + raw bytes      └ zigzag varint
//! ```
//!
//! # The modules, bottom to top
//!
//! - [`varint`] — LEB128 over zigzag, the primitive everything else is built from,
//!   and where "not order-preserving" turns into bytes saved.
//! - [`value`] — the schema-driven value and fact encoding, and the **one** tag on
//!   the wire: a reference is a union of *an id* and *the target fact itself*
//!   ([settled]).
//! - [`crc`] — CRC-32, the standard one, for a block's integrity check.
//! - [`block`] — a run of facts of one predicate, behind a sync marker and a
//!   checksummed header. **The same bytes on a socket and on disk**: a `CopyData`
//!   frame's payload is a block, and a fact file is blocks back to back, which is
//!   what makes "one fact encoding, not two" checkable rather than aspirational.
//! - [`desc`] — a **row descriptor**: the outbound direction's type source, since a
//!   query row is shaped by the head rather than by a predicate. Sent once per
//!   stream; rows then use the same value codec a fact's key does.
//! - [`frame`] — `[kind][stream][length]`, the connection's multiplexing unit.
//! - [`protocol`] — the **message vocabulary**: which kinds exist, what a startup
//!   frame carries, what a stream's life looks like. Strictly above [`frame`], which
//!   delimits messages and deliberately does not interpret them.
//!
//! The layering is worth reading as a claim about *where a length comes from*.
//! [`value`] has no lengths at all — the schema says where every field ends.
//! [`block`] has one, because a splitter must skip a block it will not parse.
//! [`frame`] has one, because a socket reader must know how many bytes to await.
//! Each is the least that layer can do its job with.
//!
//! # What is deliberately not here yet
//!
//! **The file envelope.** A fact file's header (magic, format version, producing
//! schema fingerprint) and its optional footer of block offsets are
//! [operations §8](../../docs/aperture-cli-design.md)'s and belong to Phase 7b with
//! the rest of the file pipeline. Blocks are here because they are shared with the
//! wire; the envelope is not shared with anything.
//!
//! **A session, and any I/O policy.** [`protocol`] says what a startup frame *means*;
//! it opens no socket, holds no state and decides nothing about retries, timeouts or
//! concurrency. That is `aperture-client`'s on one side and `aperture-server`'s on the
//! other, and it is why both can share this crate without sharing each other —
//! [operations §10](../../docs/aperture-cli-design.md)'s "shared by server and client,
//! no I/O policy", and its rule that nothing depends on the server.
//!
//! The vocabulary lived in `aperture-server` until 9e, which was fine while the server
//! was its only speaker and wrong the moment a Rust client existed: a client would have
//! had to depend on the server — dragging in fjall, the engine and a runtime to send a
//! handshake — or write a second copy of the message formats, which is exactly the
//! drift the .NET client exists to detect rather than to cause.
//!
//! [I1]: ../../docs/invariants.md#i1
//! [I2]: ../../docs/invariants.md#i2
//! [I3]: ../../docs/invariants.md#i3
//! [I13]: ../../docs/invariants.md#i13
//! [settled]: ../../docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline
//! [operations §6 and §8]: ../../docs/aperture-cli-design.md#6-wire-protocol--the-write-stream

pub mod block;
pub mod crc;
pub mod desc;
pub mod error;
pub mod frame;
pub mod protocol;
pub mod value;
pub mod varint;

pub use block::{BlockHeader, decode_block, encode_block, find_sync};
pub use desc::{Desc, decode_desc, encode_desc};
pub use error::WireError;
pub use frame::{FrameHeader, FrameKind, StreamId, decode_frame, encode_frame};
pub use protocol::{
    Control, ControlOp, ControlReply, ErrorCode, Mode, ProfileStep, QueryProfile, Ready, Startup,
    kinds, provisional_fingerprint,
};
pub use value::{WireFact, WireRef, WireValue, decode_fact, encode_fact, from_bytes, to_bytes};
