//! The **fjall implementation** of the [`FactStore`](fjord_store::fact_store::FactStore)
//! seam, and the database lifecycle around it.
//!
//! [`store`] is the backend: two column families per predicate, the id
//! allocator, and the striped merge frontier a parallel ingest writes through.
//! [`catalog`] is the store root — databases as artifacts, the sidecar
//! ([`meta`]), the embedded schema copy ([`schema_doc`]) and the root lock, with
//! the filesystem itself as the catalog
//! ([`ops-I7`](../../../website/content/operations.md)).
//!
//! [`error::CatalogError`] is the lifecycle's error type, and it is *this*
//! crate's rather than the seam's: a sidecar path, a held root lock and a
//! database that is not writable are facts about how this backend stores a
//! database, not about what any backend can refuse. It carries
//! [`StoreError`](fjord_store::error::StoreError) so a seam fault still bubbles
//! through one `?`.
//!
//! Design of record: [chapter 3](../../../website/content/storage.md).

pub mod catalog;
pub mod error;
pub mod identity;
/// The ingest-time lookup cache (see `docs/glean.md` §2.3).
mod lookup_cache;
pub mod meta;
pub mod schema_doc;
pub mod store;
pub mod ulid;
