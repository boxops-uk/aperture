//! In-memory `FactStore` for tests.
//!
//! A `BTreeMap` model of the two column families (`keys` and `entities`), used
//! to exercise the codec and executor against the `FactStore` trait. This is
//! test machinery, not a product backend.

use std::collections::BTreeMap;

use byteview::ByteView;

use crate::focus::{
    error::StoreError,
    fact_store::{Entity, FactStore},
    store::predicate_of,
    tuple::strinc,
};
use aperture_schema::{id::FactId, schema::PredicateId};

#[derive(Default)]
pub struct MemStore {
    index: BTreeMap<Vec<u8>, u64>,
    by_id: BTreeMap<u64, (Vec<u8>, Vec<u8>)>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value-less fact (key only) as `predicate`'s fact number
    /// `sequence`.
    pub fn insert(&mut self, predicate_id: PredicateId, key_fields: Vec<u8>, sequence: u64) {
        self.insert_valued(predicate_id, key_fields, sequence, Vec::new());
    }

    /// Insert a fact with both key and value bytes.
    ///
    /// `sequence` is the fact's number *within its predicate*, not a raw
    /// [`FactId`]: the real store composes a snowflake id from the two
    /// ([I11](../../docs/invariants.md#i11)), so a model that took whole ids could
    /// hold a fact whose id is tagged for a different predicate — a state fjall
    /// rejects, and one that would make this store a dishonest oracle.
    pub fn insert_valued(
        &mut self,
        predicate_id: PredicateId,
        key_fields: Vec<u8>,
        sequence: u64,
        value: Vec<u8>,
    ) {
        let fact_id = FactId::new(predicate_id, sequence).expect("test fixture fact id");

        let mut full_key = predicate_id.0.to_be_bytes().to_vec();
        full_key.extend_from_slice(&key_fields);
        self.index.insert(full_key, fact_id.raw());
        self.by_id.insert(fact_id.raw(), (key_fields, value));
    }
}

pub struct MemScan {
    rows: std::vec::IntoIter<(Vec<u8>, u64)>,
}

impl Iterator for MemScan {
    type Item = Result<(ByteView, FactId), StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        // The ids in this map came from `FactId::new`, so they are already valid.
        self.rows
            .next()
            .map(|(k, id)| Ok((k.into(), FactId::from_raw(id))))
    }
}

impl FactStore for MemStore {
    type Scan = MemScan;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Result<MemScan, StoreError> {
        // A bound too short to name a predicate is rejected here exactly as the
        // real store rejects it. Reading it as "no predicate end, so scan on"
        // is how this store used to walk straight across the boundary while
        // fjall returned an error for the same call.
        let predicate = predicate_of(lo)?;

        // A scan is a *predicate* query ([chapter 3](../../docs/03-storage-model.md)):
        // it never crosses out of the predicate named by `lo`'s prefix. One
        // `BTreeMap` holds every predicate here, so that bound has to be applied
        // explicitly — the real store gets it structurally, from one keyspace per
        // predicate. Without it an absent `hi` (which the executor produces only
        // for an all-`0xFF` prefix) would walk on into the next predicate's rows.
        let predicate_end = strinc(&predicate.to_be_bytes());
        let end = match (hi, predicate_end.as_deref()) {
            (Some(hi), Some(predicate_end)) => Some(hi.min(predicate_end)),
            (hi, predicate_end) => hi.or(predicate_end),
        };

        let rows: Vec<_> = match end {
            Some(end) => self
                .index
                .range(lo.to_vec()..end.to_vec())
                .map(|(k, &v)| (k.clone(), v))
                .collect(),
            None => self
                .index
                .range(lo.to_vec()..)
                .map(|(k, &v)| (k.clone(), v))
                .collect(),
        };
        Ok(MemScan {
            rows: rows.into_iter(),
        })
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, StoreError> {
        Ok(self.by_id.get(&id.raw()).map(|(k, v)| Entity {
            key: k.clone().into(),
            value: v.clone().into(),
        }))
    }
}
