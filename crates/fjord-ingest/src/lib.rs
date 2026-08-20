//! **The write funnel** — where a fact that arrived on the wire becomes rows in the
//! store. `ops-I5`'s one funnel: schema validation, interning, dedup and the conflict
//! reject happen here for every writer, whatever transport it came in on. Its own crate
//! because it is the crossing between [`fjord_wire`] and [`fjord_store`], and neither
//! should know the other.
//!
//! Interning is resolve-or-create, **bottom-up** — a parent's key holds its child's id,
//! so it has no bytes to look up until the child has one; the same fact makes the walk
//! terminate. Design: [the storage
//! model](../../../website/content/storage.md#interning-a-nested-fact).
//!
//! Two behaviours to keep in mind when changing the walk, each pinned by a test:
//!
//! - **Type-correctness does not imply ingestibility.** One message may name a target
//!   twice with two different value sides; the second occurrence finds what the first
//!   wrote and is refused as an ordinary same-key-different-value conflict — never
//!   resolved, since picking either occurrence would be order-dependent (`ops-I4`).
//! - **Interning is not a transaction.** A fact whose nested target interned cleanly
//!   and which then conflicts has written the target. Harmless — interning is
//!   idempotent, so a retry dedups against it — but a rollback story would have to know
//!   (`a_staged_block_that_fails_keeps_what_it_had_already_written`).

pub mod error;
pub mod intern;
pub mod sink;

pub use error::IngestError;
pub use intern::{Ingested, intern_block, intern_fact};
pub use sink::FactSink;
