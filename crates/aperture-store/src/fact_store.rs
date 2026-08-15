//! The **storage seam**: what the engine is allowed to ask of a store.
//!
//! Its own module rather than a section of [`store`](crate::store), which
//! is the fjall implementation, or of the engine's `plan`, where it used
//! to live. A plan is a description; a store is a thing that answers. Keeping the
//! trait where either implementation could be mistaken for the definition is what
//! makes a seam quietly grow the shape of one side of it.
//!
//! Two implementations meet here: [`FjallDb`](crate::store::FjallStore) on
//! disk and [`MemStore`](crate::mem_store::MemStore) in memory, held to
//! each other as a differential oracle
//! ([testing](../../docs/testing.md)). The executor consumes a `(handle,
//! snapshot)` and assumes nothing about a connection, which is the cut that lets
//! the same engine run embedded and served
//! ([operations §10](../../docs/aperture-cli-design.md)).

use byteview::ByteView;

use crate::error::StoreError;
use aperture_schema::id::FactId;

#[derive(Debug)]
pub struct Entity {
    pub key: ByteView,
    pub value: ByteView,
}

pub trait FactStore {
    type Scan: Iterator<Item = Result<(ByteView, FactId), StoreError>>;

    /// Open a scan of `lo..hi`, bounded to the predicate named by `lo`'s first
    /// [`PREDICATE_ID_SIZE`](aperture_schema::schema::PREDICATE_ID_SIZE) bytes.
    ///
    /// Fallible, because opening genuinely can fail: a `lo` too short to name a
    /// predicate names nothing, and that is a fault in the *call*, not in a row.
    /// While this returned the iterator directly there was nowhere to say so, and
    /// each implementation invented an answer — one smuggled the error out as a
    /// first row, the others scanned across the predicate boundary and reported
    /// nothing.
    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Result<Self::Scan, StoreError>;

    fn point(&self, id: FactId) -> Result<Option<Entity>, StoreError>;
}
