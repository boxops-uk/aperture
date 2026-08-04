use std::{collections::BTreeMap, sync::Arc};

use aperture::focus::{
    error::{ApertureError, StoreCodecError},
    iter::{Address, Executor, Iteratee, Stream},
    plan::{Access, Entity, FactId, FactStore, Generator, Plan, Project, SeekKey, SeekKeyPart},
    schema::{LocalInterner, Predicate, PredicateId, PredicateTy, Schema},
    tuple::{MARK_RECORD, TupleEncode, TupleEncoder, Value, encode_tuple, put_str},
};
use byteview::ByteView;
use lasso::Rodeo;
use tokio_util::sync::CancellationToken;

struct MemStore {
    index: BTreeMap<Vec<u8>, u64>,
    by_id: BTreeMap<u64, (Vec<u8>, Vec<u8>)>,
}

impl MemStore {
    fn new() -> Self {
        Self {
            index: BTreeMap::new(),
            by_id: BTreeMap::new(),
        }
    }

    fn insert(
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

struct MemScan {
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

struct FileFact(String);

impl TupleEncode for FileFact {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(&self.0)
    }
}

struct FunctionFact {
    file_path: String,
    name: String,
}

impl TupleEncode for FunctionFact {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.record(|enc| {
            enc.put_str(&self.file_path)?;
            enc.put_str(&self.name)?;
            Ok(())
        })
    }
}

fn main() -> Result<(), StoreCodecError> {
    let mut rodeo = Rodeo::new();
    let fn_file_path_spur = rodeo.get_or_intern("file_path");
    let fn_name_spur = rodeo.get_or_intern("name");

    let src_file_pred = Predicate {
        name: rodeo.get_or_intern("src.File"),
        key: PredicateTy::Str,
        value: None,
    };

    let src_fn_pred = Predicate {
        name: rodeo.get_or_intern("src.Function"),
        key: PredicateTy::Record(Arc::from(vec![
            (fn_file_path_spur, PredicateTy::Str),
            (fn_name_spur, PredicateTy::Str),
        ])),
        value: None,
    };

    let schema = Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![src_file_pred, src_fn_pred]),
    );

    let interner = LocalInterner::new(schema.interner().clone());

    let (src_file_id, _) = schema.find_position("src.File").unwrap();
    let (src_fn_id, _) = schema.find_position("src.Function").unwrap();

    let mut store = MemStore::new();
    let mut next_id = 1u64;

    for path in ["src/main.rs", "src/lib.rs", "src/utils.rs"] {
        let fact = FileFact(path.to_string());
        let key = encode_tuple(&fact)?;
        store.insert(src_file_id, key, next_id, vec![]);
        next_id += 1;
    }

    for (file_path, name) in [
        ("src/main.rs", "main"),
        ("src/main.rs", "setup"),
        ("src/lib.rs", "new"),
        ("src/lib.rs", "parse"),
        ("src/lib.rs", "execute"),
        ("src/utils.rs", "helper"),
    ] {
        let fact = FunctionFact {
            file_path: file_path.to_string(),
            name: name.to_string(),
        };
        let key = encode_tuple(&fact)?;
        store.insert(src_fn_id, key, next_id, vec![]);
        next_id += 1;
    }

    let mut file_seek = vec![];
    put_str(&mut file_seek, "src/main.rs");

    let fn_record_ty = PredicateTy::Record(Arc::from(vec![
        (fn_file_path_spur, PredicateTy::Str),
        (fn_name_spur, PredicateTy::Str),
    ]));

    let plan = Plan {
        nvars: 2,
        body: Box::new([
            Generator {
                access: Access {
                    predicate_id: src_file_id,
                    seek_key: SeekKey::Prefix(file_seek.into_boxed_slice()),
                },
                binds: Box::new([Address::new(0)]),
                residuals: Box::new([]),
            },
            Generator {
                access: Access {
                    predicate_id: src_fn_id,
                    seek_key: SeekKey::Composite(Box::new([
                        SeekKeyPart::Bytes(Box::new([MARK_RECORD])),
                        SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            field_idx: 0,
                        },
                    ])),
                },
                binds: Box::new([Address::new(1)]),
                residuals: Box::new([]),
            },
        ]),
        head: Project::RegisterField {
            address: Address::new(1),
            field_idx: 0,
            ty: fn_record_ty,
        },
    };

    let executor = Executor::new(store, plan);
    let cancel = CancellationToken::new();

    let result = executor
        .enumerate(
            Vec::<Value>::new(),
            |mut acc, row| {
                let value = row.to_value(&interner)?;
                acc.push(value);
                Ok(Stream::Continue(acc))
            },
            &cancel,
        )
        .expect("query failed");

    let values = match result {
        Iteratee::Done(v) | Iteratee::Suspended(v, _) => v,
    };

    println!("{}", serde_json::to_string_pretty(&values).unwrap());

    Ok(())
}
