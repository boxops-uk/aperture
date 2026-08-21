//! The **storage seam**: what a fact is on disk, and the trait the engine reads
//! it through — and no implementation of either.
//!
//! [`fact_store`] is the seam itself, deliberately its own module so that
//! neither implementation can be mistaken for the definition. What makes this
//! crate the seam rather than a layer of one backend is what it does *not*
//! link: no fjall, no filesystem, no threads. The implementations are
//! `fjord-store-fjall` (a database on disk, and the lifecycle around it) and
//! `fjord-store-mem` (the differential oracle, and the store an engine compiled
//! to WebAssembly runs on). A third is additive because of this cut.
//!
//! [`fact`] is how a fact is written *by hand*, resolved against the schema by
//! name; [`keys`] reads the one part of a stored key every implementation must
//! agree about; and [`mod@format`] is the twelve-byte stamp that says which build
//! wrote a database ([I15](../../../website/content/invariants.md#i15)) — a
//! *format*, so it belongs with the definition rather than with the backend that
//! stamps it.
//!
//! [`error::StoreError`] is what the seam can refuse, and it names no backend:
//! `Backend` carries a boxed source, because *the backend failed* is the trait's
//! business and *which* backend is not.
//!
//! The executor consumes a `(handle, snapshot)` and assumes nothing about a
//! connection — the cut that lets the same engine run embedded and served
//! ([operations §10](../../../website/content/operations.md)).
//!
//! Design of record: [chapter 3](../../../website/content/storage.md).

pub mod error;
pub mod fact;
pub mod fact_store;
pub mod format;
pub mod keys;

// Test-support surface: the shared fixture database, and the probes that hold a
// store to the seam's contract. Gated so `--features proptest` exposes them to
// consumers outside `cfg(test)` (see `website/content/testing.md`) — which is
// every battery in the engine crate, and both implementations.
#[cfg(any(test, feature = "proptest"))]
pub mod fixture;

#[cfg(any(test, feature = "proptest"))]
pub mod fixtures;
