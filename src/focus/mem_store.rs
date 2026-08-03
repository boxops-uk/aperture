//! In-memory `FactStore` for tests.
//!
//! A `BTreeMap` model of the two column families (`keys` and `entities`), used
//! to exercise the codec and executor against the `FactStore` trait. This is
//! test machinery, not a product backend.

use std::collections::BTreeMap;

use byteview::ByteView;

use crate::focus::{
    error::ApertureError,
    plan::{Entity, FactId, FactStore},
    schema::PredicateId,
};

#[derive(Default)]
pub struct MemStore {
    index: BTreeMap<Vec<u8>, u64>,
    by_id: BTreeMap<u64, (Vec<u8>, Vec<u8>)>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value-less fact (key only).
    pub fn insert(&mut self, predicate_id: PredicateId, key_fields: Vec<u8>, fact_id: u64) {
        self.insert_valued(predicate_id, key_fields, fact_id, Vec::new());
    }

    /// Insert a fact with both key and value bytes.
    pub fn insert_valued(
        &mut self,
        predicate_id: PredicateId,
        key_fields: Vec<u8>,
        fact_id: u64,
        value: Vec<u8>,
    ) {
        let mut full_key = predicate_id.0.to_be_bytes().to_vec();
        full_key.extend_from_slice(&key_fields);
        self.index.insert(full_key, fact_id);
        self.by_id.insert(fact_id, (key_fields, value));
    }
}

pub struct MemScan {
    rows: std::vec::IntoIter<(Vec<u8>, u64)>,
}

impl Iterator for MemScan {
    type Item = Result<(ByteView, FactId), ApertureError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next().map(|(k, id)| Ok((k.into(), FactId(id))))
    }
}

impl FactStore for MemStore {
    type Scan = MemScan;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> MemScan {
        let rows: Vec<_> = match hi {
            Some(hi_bytes) => self
                .index
                .range(lo.to_vec()..hi_bytes.to_vec())
                .map(|(k, &v)| (k.clone(), v))
                .collect(),
            None => self
                .index
                .range(lo.to_vec()..)
                .map(|(k, &v)| (k.clone(), v))
                .collect(),
        };
        MemScan {
            rows: rows.into_iter(),
        }
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, ApertureError> {
        Ok(self.by_id.get(&id.0).map(|(k, v)| Entity {
            key: k.clone().into(),
            value: v.clone().into(),
        }))
    }
}
