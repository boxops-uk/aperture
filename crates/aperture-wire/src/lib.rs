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
//! # The three modules
//!
//! - [`varint`] — LEB128 over zigzag, the primitive everything else is built from,
//!   and where "not order-preserving" turns into bytes saved.
//! - [`value`] — the schema-driven value and fact encoding, and the **one** tag on
//!   the wire: a reference is a union of *an id* and *the target fact itself*
//!   ([settled]).
//! - [`error`] — decode faults, kept apart from the storage codec's on purpose: a
//!   wire fault means a peer sent something wrong, which is an ordinary event, not a
//!   database to doubt.
//!
//! # What is deliberately not here yet
//!
//! **Framing.** Blocks, sync markers, CRC32 and the `[type][stream_id][len]` frame
//! header are the layer above ([operations §6 and §8]); this crate encodes one fact
//! and one value, and knows nothing about where they sit. Keeping the split means the
//! same fact encoding serves the wire and the fact file, which is what makes "one
//! encoding, not two" checkable rather than aspirational.
//!
//! **The outbound direction.** A query row is shaped by the query's *head*, not by a
//! predicate, so it needs a row descriptor sent once per stream — PostgreSQL's
//! `RowDescription` before its `DataRow`s, which is the model §6 already borrows. The
//! value encoding below is the same one; only where the type comes from differs.
//!
//! [I1]: ../../docs/invariants.md#i1
//! [I2]: ../../docs/invariants.md#i2
//! [I3]: ../../docs/invariants.md#i3
//! [I13]: ../../docs/invariants.md#i13
//! [settled]: ../../docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline
//! [operations §6 and §8]: ../../docs/aperture-cli-design.md#6-wire-protocol--the-write-stream

pub mod error;
pub mod value;
pub mod varint;

pub use error::WireError;
pub use value::{WireFact, WireRef, WireValue, decode_fact, encode_fact, from_bytes, to_bytes};
