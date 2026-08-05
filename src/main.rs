//! A worked example: build a schema, write facts, run a two-level plan.
//!
//! This runs against the real [`FjallDb`] rather than a model store. There was a
//! second, private `MemStore` here once — a stale copy of
//! `focus::mem_store::MemStore`, still carrying the unbounded-scan bug that one
//! was fixed for. A demo whose store disagrees with the product's is worse than
//! no demo, and the model store is test machinery (gated behind `cfg(test)` /
//! the `proptest` feature) rather than something a binary should reach for.

use std::{path::PathBuf, sync::Arc};

use aperture::focus::{
    error::{ApertureError, StoreCodecError},
    iter::{Address, Executor, Iteratee, Stream},
    plan::{Access, Generator, Plan, Project, SeekKey, SeekKeyPart},
    schema::{LocalInterner, Predicate, PredicateTy, Schema},
    store::FjallDb,
    tuple::{MARK_RECORD, TupleEncode, TupleEncoder, Value, encode_tuple, put_str},
};
use lasso::Rodeo;
use tokio_util::sync::CancellationToken;

struct FileFact(String);

impl TupleEncode for FileFact {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.put_str(&self.0);
        Ok(())
    }
}

struct FunctionFact {
    file_path: String,
    name: String,
}

impl TupleEncode for FunctionFact {
    fn tuple_encode(&self, enc: &mut TupleEncoder<'_>) -> Result<(), StoreCodecError> {
        enc.record(|enc| {
            enc.put_str(&self.file_path);
            enc.put_str(&self.name);
            Ok(())
        })
    }
}

/// A scratch directory of this run's own, so re-running the demo neither
/// inherits the last run's facts nor writes a key twice (see
/// [`FjallDb::put_fact`] — a key is written once).
fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!("aperture-demo-{}", std::process::id()))
}

fn main() -> Result<(), ApertureError> {
    let dir = scratch_dir();
    let result = demo(&dir);

    // On *every* path, including an early `?`: best effort, since failing to
    // tidy up is not a reason to fail the run.
    let _ = std::fs::remove_dir_all(&dir);

    result
}

fn demo(dir: &std::path::Path) -> Result<(), ApertureError> {
    let mut rodeo = Rodeo::new();
    let fn_file_path = rodeo.get_or_intern("file_path");
    let fn_name = rodeo.get_or_intern("name");

    let fn_record_ty = PredicateTy::Record(Arc::from([
        (fn_file_path, PredicateTy::Str),
        (fn_name, PredicateTy::Str),
    ]));

    let predicates = Arc::from([
        Predicate {
            name: rodeo.get_or_intern("src.File"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: rodeo.get_or_intern("src.Function"),
            key: fn_record_ty.clone(),
            value: None,
        },
    ]);

    let schema = Schema::new(rodeo.into_reader(), predicates);
    let interner = LocalInterner::new(schema.interner().clone());

    let (src_file_id, _) = schema
        .find_position("src.File")
        .expect("src.File is declared above");
    let (src_fn_id, _) = schema
        .find_position("src.Function")
        .expect("src.Function is declared above");

    let db = FjallDb::open(dir)?;

    // The schema is known up front, so pay for the trees here rather than at an
    // arbitrary point inside the writes below.
    db.create_predicates([src_file_id, src_fn_id])?;

    for path in ["src/main.rs", "src/lib.rs", "src/utils.rs"] {
        let key = encode_tuple(&FileFact(path.to_string()))?;
        db.put_fact(src_file_id, &key, &[])?;
    }

    for (file_path, name) in [
        ("src/main.rs", "main"),
        ("src/main.rs", "setup"),
        ("src/lib.rs", "new"),
        ("src/lib.rs", "parse"),
        ("src/lib.rs", "execute"),
        ("src/utils.rs", "helper"),
    ] {
        let key = encode_tuple(&FunctionFact {
            file_path: file_path.to_string(),
            name: name.to_string(),
        })?;
        db.put_fact(src_fn_id, &key, &[])?;
    }

    // "the functions declared in src/main.rs": seek `src.File` straight to that
    // one path, then splice the bound row's path field into a scan of
    // `src.Function`, whose key is a record and so starts with MARK_RECORD.
    let mut file_seek = vec![];
    put_str(&mut file_seek, "src/main.rs");

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
        // `field_idx` indexes the key's *top-level* fields, and a `src.Function`
        // key is one record, so field 0 is the whole `{file_path, name}`.
        head: Project::RegisterField {
            address: Address::new(1),
            field_idx: 0,
            ty: fn_record_ty,
        },
    };

    let result = Executor::new(db.reader(), plan).enumerate(
        Vec::<Value>::new(),
        |mut acc, mut row| {
            acc.push(row.to_value(&interner)?);
            Ok(Stream::Continue(acc))
        },
        &CancellationToken::new(),
    )?;

    let (Iteratee::Done(values) | Iteratee::Suspended(values, _)) = result;
    println!(
        "{}",
        serde_json::to_string_pretty(&values).expect("`Value` serialises infallibly")
    );

    Ok(())
}
