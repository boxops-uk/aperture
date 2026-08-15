//! The **storage layer**: what a fact is on disk, and the seam the engine reads
//! it through.
//!
//! [`fact_store`] is the seam — deliberately its own module, so neither
//! implementation can be mistaken for the definition. [`store`] is the fjall
//! backend, [`mem_store`] the in-memory model the batteries hold it against as a
//! differential oracle. [`fact`] is how a fact is written *by hand*, resolved
//! against the schema by name, and [`format`] is the twelve-byte stamp that says
//! which build wrote a database ([I15](../../../docs/invariants.md#i15)).
//!
//! The executor consumes a `(handle, snapshot)` and assumes nothing about a
//! connection — the cut that lets the same engine run embedded and served
//! ([operations §10](../../../docs/aperture-cli-design.md)).
//!
//! Design of record: [chapter 3](../../../docs/03-storage-model.md).

pub mod error;
pub mod fact;
pub mod fact_store;
pub mod format;
pub mod store;

// Test-support surface: the in-memory store and the shared fixture database.
// Gated so `--features proptest` exposes them to consumers outside `cfg(test)`
// (see `docs/testing.md`) — which is every battery in the engine crate.
#[cfg(any(test, feature = "proptest"))]
pub mod fixture;

#[cfg(any(test, feature = "proptest"))]
pub mod fixtures;

#[cfg(any(test, feature = "proptest"))]
pub mod mem_store;
