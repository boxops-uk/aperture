//! **The write funnel** — where a fact that arrived on the wire becomes a fact in
//! the store.
//!
//! One crate for one crossing. A fact reaches here as a
//! [`WireFact`](aperture_wire::WireFact) — schema-driven, references possibly
//! *nested* — and leaves as rows in two column families, keyed by the
//! order-preserving [storage codec](aperture_encoding::tuple) and identified by a
//! [`FactId`](aperture_schema::id::FactId). Three crates meet to make that happen and
//! none of them should have to know about the other two, which is why this is not a
//! module of any of them.
//!
//! It is also `ops-I5`'s **one funnel**: schema validation, interning, dedup and the
//! conflict reject all happen here, for every writer, whatever transport it came in
//! on. A bad tool's blast radius is "wrong facts", never "broken database".
//!
//! # Interning, and why the walk is bottom-up
//!
//! A producer sends the target fact rather than an id, because every id-based
//! alternative makes the *indexer* keep a map from each entity to its assigned
//! identity ([settled]). Turning that back into an id is **interning**:
//! resolve-or-create against the target predicate, then substitute
//! ([chapter 3](../../docs/03-storage-model.md#interning-a-nested-fact)).
//!
//! ```text
//!   src.Decl { file = src.File "keys.py", line = 12, name = "key_of" }
//!                     └──────┬─────────┘
//!                            │  1. intern the target      -> src.File#3
//!                            ▼
//!   src.Decl { file = #3, line = 12, name = "key_of" }
//!                            │  2. *now* the key has bytes
//!                            ▼  3. intern the parent      -> src.Decl#8
//! ```
//!
//! The order is forced rather than chosen. A parent's key holds its child's *id*, so
//! the parent has no bytes — and therefore no identity of its own, and nothing to
//! look up — until the child has one. That is the same fact that makes a reference
//! in a key impossible to put in a cycle, and so the same fact that makes this walk
//! terminate.
//!
//! # What is not new here
//!
//! Almost everything, and that is the design working rather than a thin
//! implementation:
//!
//! - **Dedup** is [`intern`](aperture_store::store::Interned)'s "the id already
//!   assigned comes back". A target nested under a thousand parents is one row.
//! - **The conflict reject** is the same call's `KeyAlreadyWritten`. A nested fact
//!   both names and *defines* its target, so a nested value disagreeing with a
//!   stored one is exactly `ops-I5`'s same-key-different-value case.
//! - **Atomicity** is `put_fact`'s single batch across both column families
//!   ([I12](../../docs/invariants.md#i12)).
//! - **Id allocation** is the per-predicate counter
//!   ([I11](../../docs/invariants.md#i11)).
//!
//! What this crate adds is the *walk*, the substitution, and the type checking of a
//! wire value against the schema on the way through.
//!
//! # A fact can contradict itself
//!
//! Worth stating because it is not obvious and the property battery is what found
//! it: **type-correctness does not imply ingestibility.** A nested fact both names
//! and *defines* its target, so one message naming a target twice with two different
//! value sides —
//!
//! ```text
//!   gen.Pair { l = gen.Target "same" -> 1,
//!              r = gen.Target "same" -> 2 }
//! ```
//!
//! — is a producer disagreeing with itself, and is refused as an ordinary
//! same-key-different-value conflict. It is *not* a new rule and not a special case
//! in the walk: the second occurrence simply finds what the first one wrote.
//!
//! Refused rather than resolved, because picking either occurrence would be
//! order-dependent and `ops-I4` forbids that. Both orders reject, so the answer does
//! not depend on the order the walk happens to take.
//!
//! # What a failed ingest leaves behind
//!
//! Not nothing, and that is recorded rather than hidden. A fact whose nested target
//! interns cleanly and which then conflicts has **written the target**: interning is
//! not a transaction, and whether a failed stream's already-written facts are rolled
//! back is a P0 decision that belongs with the transaction story
//! ([operations §6](../../docs/aperture-cli-design.md#6-wire-protocol--the-write-stream)).
//!
//! It is also close to harmless, for a reason worth knowing. A written target is a
//! fact that was legitimately named and legitimately defined; facts are immutable and
//! interning is **idempotent**, so retrying the whole message after fixing the
//! conflict dedups against it rather than duplicating it. The failure mode a
//! transaction would prevent here is a wasted row, not a wrong answer.
//!
//! [settled]: ../../docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline

pub mod error;
pub mod intern;
pub mod sink;

pub use error::IngestError;
pub use intern::{Ingested, intern_block, intern_fact};
pub use sink::FactSink;
