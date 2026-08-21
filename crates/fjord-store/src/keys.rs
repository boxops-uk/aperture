//! Reading the parts of a stored key that every implementation agrees on.
//!
//! A stored key begins with its predicate's id, and a scan bound is a key
//! prefix — so *which predicate a bound names* is the seam's business rather
//! than a backend's. Kept here so the two implementations cannot disagree about
//! a malformed bound: one refusing and the other reading "no predicate end, so
//! scan on" would walk out of the predicate the caller asked for, and the
//! difference would show up as extra rows rather than as an error.

use fjord_schema::schema::PREDICATE_ID_SIZE;

use crate::error::StoreError;

/// The predicate a scan bound names — its first four bytes.
///
/// Shared by every [`FactStore`](crate::fact_store::FactStore), so the contract
/// for a malformed bound is one behaviour rather than one per implementation.
pub fn predicate_of(lo: &[u8]) -> Result<u32, StoreError> {
    let prefix = lo
        .get(..PREDICATE_ID_SIZE)
        .ok_or(StoreError::ShortScanBound {
            len: lo.len(),
            expected: PREDICATE_ID_SIZE,
        })?;

    Ok(u32::from_be_bytes(
        prefix.try_into().expect("checked four bytes above"),
    ))
}
