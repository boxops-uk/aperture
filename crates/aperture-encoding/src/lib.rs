//! The **storage tuple codec** — the order-preserving encoding every stored byte
//! goes through, and the faults it can refuse with.
//!
//! Above [`aperture-schema`](aperture_schema) and below everything else: a store
//! writes these bytes, a plan compares against them, and the executor decodes
//! them, but the encoding itself knows about none of that. What it knows is a
//! type and a byte order.
//!
//! Design of record: [chapter 2](../../../docs/02-tuple-codec.md), and the three
//! invariants it holds — [I1](../../../docs/invariants.md#i1) order-preservation,
//! [I2](../../../docs/invariants.md#i2) self-delimiting, and
//! [I3](../../../docs/invariants.md#i3) the frozen marker table.
//!
//! This is deliberately **not** the transport codec: rows leaving the executor
//! are framed by a different, non-order-preserving encoding that never touches
//! stored bytes ([chapter 3](../../../docs/03-storage-model.md#storage-codec-vs-transport-codec)).

pub mod error;
pub mod tuple;
