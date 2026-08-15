//! Interning against a **real** `FjallDb` in a scratch directory.
//!
//! An integration test rather than unit tests, because the claims are about what
//! ends up on disk: the ids a producer gets back, the rows a second producer dedups
//! against, and the point at which two producers disagree. A double can say the walk
//! is bottom-up; only a store can say a thousand parents named one file and the file
//! was written once.

use aperture_ingest::{IngestError, intern_block, intern_fact};
use aperture_schema::{
    id::FactId,
    schema::{Predicate, PredicateId, PredicateTy, Schema},
};
use aperture_store::store::FjallDb;
use aperture_wire::{WireFact, WireRef, WireValue, encode_block};
use lasso::Rodeo;
use std::sync::Arc;

const FILE: PredicateId = PredicateId(0);
const DECL: PredicateId = PredicateId(1);

/// `src.File : string` and `src.Decl : { file : src.File, line : int, name : string }`
/// — a reference in a **key**, which is the case that forces the walk's order.
fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let (file, decl) = (
        rodeo.get_or_intern("src.File"),
        rodeo.get_or_intern("src.Decl"),
    );
    let (f_file, f_line, f_name) = (
        rodeo.get_or_intern("file"),
        rodeo.get_or_intern("line"),
        rodeo.get_or_intern("name"),
    );

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![
            Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: decl,
                key: PredicateTy::Record(
                    vec![
                        (f_file, PredicateTy::Fact(FILE)),
                        (f_line, PredicateTy::Int),
                        (f_name, PredicateTy::Str),
                    ]
                    .into(),
                ),
                value: None,
            },
        ]),
    )
}

fn db() -> (tempfile::TempDir, FjallDb) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let db = FjallDb::open(dir.path()).expect("a database");
    (dir, db)
}

/// Whether a fact with this id is on disk. Reads go through a reader, which is the
/// seam the executor uses — so this asks the question a query would.
fn stored(db: &FjallDb, id: FactId) -> bool {
    use aperture_store::fact_store::FactStore;
    db.reader().point(id).expect("a read").is_some()
}

fn file(path: &str) -> WireFact {
    WireFact {
        predicate: FILE,
        key: WireValue::Str(path.to_owned()),
        value: None,
    }
}

fn decl(file: WireRef, line: i64, name: &str) -> WireFact {
    WireFact {
        predicate: DECL,
        key: WireValue::Record(
            vec![
                WireValue::Ref(file),
                WireValue::Int(line),
                WireValue::Str(name.to_owned()),
            ]
            .into(),
        ),
        value: None,
    }
}

fn nested(path: &str) -> WireRef {
    WireRef::Nested(Box::new(file(path)))
}

/// **The headline: a producer sends facts holding no ids at all, and they land.**
///
/// This is what interning is for. The declaration names its file by *being given*
/// the file, which is what an indexer walking a syntax tree has in hand; the id it
/// gets back is the one the file was written under.
#[test]
fn a_producer_that_holds_no_ids_can_write_a_subgraph() {
    let (_dir, db) = db();
    let schema = schema();

    let out = intern_fact(&db, &schema, &decl(nested("store/keys.py"), 12, "key_of"))
        .expect("it ingests");

    // Two facts written by one call: the declaration, and the file it named.
    assert_eq!(out.created, 2);
    assert_eq!(out.deduped, 0);
    assert_eq!(out.ids.len(), 1, "one top-level fact was given");

    let decl_id = out.ids[0];
    assert_eq!(decl_id.predicate(), DECL);

    // The file exists in its own right, under its own predicate — a nested fact both
    // names and *defines* its target.
    assert!(
        stored(&db, FactId::new(FILE, 1).expect("an id")),
        "the nested target was written as a fact of its own"
    );
}

/// **Dedup: a thousand parents naming one file write the file once.**
///
/// `ops-I5`'s "dedup byte-identical facts silently", reached from the interning side
/// rather than from the merge frontier — and the reason a producer may send the same
/// target as often as it likes without keeping a book of what it has sent.
#[test]
fn many_parents_naming_one_target_write_it_once() {
    let (_dir, db) = db();
    let schema = schema();

    let mut created = 0;
    let mut deduped = 0;

    for line in 0..100i64 {
        let out = intern_fact(
            &db,
            &schema,
            &decl(nested("store/keys.py"), line, "declaration"),
        )
        .expect("it ingests");

        created += out.created;
        deduped += out.deduped;
    }

    // 100 declarations, each new; the file written on the first pass and found on
    // the other 99.
    assert_eq!(created, 101, "100 declarations and one file");
    assert_eq!(deduped, 99, "the file was already there 99 times");

    // And exactly one file row exists, whatever the count said.
    assert!(stored(&db, FactId::new(FILE, 1).expect("an id")));
    assert!(
        !stored(&db, FactId::new(FILE, 2).expect("an id")),
        "a second file row would mean the target was written twice"
    );
}

/// **The same fact sent twice is one row, and the same id comes back.** Idempotence
/// is what lets a producer retry a block after a dropped connection.
#[test]
fn re_sending_a_fact_returns_the_id_it_already_has() {
    let (_dir, db) = db();
    let schema = schema();

    let fact = decl(nested("a.py"), 1, "f");

    let first = intern_fact(&db, &schema, &fact).expect("it ingests");
    let second = intern_fact(&db, &schema, &fact).expect("it ingests again");

    assert_eq!(first.ids, second.ids, "the same fact has the same id");
    assert_eq!(first.created, 2);
    assert_eq!(second.created, 0, "nothing new the second time");
    assert_eq!(second.deduped, 2);
}

/// **A reference the producer already holds is the other branch, and it lands in the
/// same place.** The two spellings of one reference must produce the *same fact* —
/// otherwise a deriver holding ids and an indexer holding none would populate the
/// database differently.
#[test]
fn a_nested_reference_and_an_id_reference_agree() {
    let (_dir, db) = db();
    let schema = schema();

    let by_nesting = intern_fact(&db, &schema, &decl(nested("a.py"), 7, "f")).expect("it ingests");
    let file_id = FactId::new(FILE, 1).expect("an id");

    // The same declaration, now naming the file by the id the first call minted.
    let by_id = intern_fact(&db, &schema, &decl(WireRef::Id(file_id), 7, "f")).expect("it ingests");

    assert_eq!(
        by_nesting.ids, by_id.ids,
        "the two spellings are the same fact"
    );
    assert_eq!(
        by_id.created, 0,
        "and so nothing was written the second time"
    );
}

/// **A nested fact that disagrees with a stored one is rejected**, and the rejection
/// names what is already there. A nested fact defines its target, so this is exactly
/// `ops-I5`'s same-key-different-value case — no new rule, and no polarity: never
/// last-writer-wins, never first-writer-wins.
#[test]
fn a_nested_fact_disagreeing_with_a_stored_one_is_rejected() {
    let mut rodeo = Rodeo::new();
    let name = rodeo.get_or_intern("src.Blob");
    // A predicate *with* a value side, so two facts can share a key and differ.
    let schema = Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![Predicate {
            name,
            key: PredicateTy::Str,
            value: Some(PredicateTy::Int),
        }]),
    );

    let blob = |contents: i64| WireFact {
        predicate: PredicateId(0),
        key: WireValue::Str("same/key.py".to_owned()),
        value: Some(WireValue::Int(contents)),
    };

    let (_dir, db) = db();
    intern_fact(&db, &schema, &blob(1)).expect("the first lands");

    match intern_fact(&db, &schema, &blob(2)) {
        Err(IngestError::Conflict {
            predicate,
            existing,
        }) => {
            assert_eq!(predicate, PredicateId(0));
            assert_eq!(existing, FactId::new(PredicateId(0), 1).expect("an id"));
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

/// A reference naming the wrong predicate is refused before anything is written —
/// the check the snowflake tag makes free, applied here as well as at wire decode
/// because a `WireFact` can be built by hand.
#[test]
fn a_reference_to_the_wrong_predicate_is_refused() {
    let (_dir, db) = db();
    let schema = schema();

    let wrong = FactId::new(DECL, 1).expect("an id");

    assert!(matches!(
        intern_fact(&db, &schema, &decl(WireRef::Id(wrong), 1, "f")),
        Err(IngestError::TypeMismatch { .. })
    ));
}

/// **A whole block ingests**, which is what a `CopyData` frame carries — the write
/// stream's actual unit, end to end from encoded bytes to rows on disk.
#[test]
fn a_block_of_facts_ingests_and_dedups_across_its_facts() {
    let (_dir, db) = db();
    let schema = schema();

    let facts = vec![
        decl(nested("store/keys.py"), 12, "key_of"),
        decl(nested("store/keys.py"), 48, "key_prefix"),
        decl(nested("store/codec.py"), 7, "encode_key"),
    ];

    let mut block = vec![];
    encode_block(&mut block, &schema, DECL, &facts).expect("a block");

    let out = intern_block(&db, &schema, &block).expect("the block ingests");

    // Three declarations and two files: `store/keys.py` is named twice and written
    // once, which is dedup happening *within* one block rather than against the
    // store's prior contents.
    assert_eq!(out.created, 5);
    assert_eq!(out.deduped, 1);
    assert_eq!(out.ids.len(), 3);
    assert_eq!(out.seen(), 6);
}

/// A block that decodes but whose facts conflict fails, and the failure is the
/// peer's rather than the database's — which is what decides between failing a
/// stream and taking a database out of service.
#[test]
fn an_ingest_fault_says_whose_fault_it_is() {
    let (_dir, db) = db();
    let schema = schema();

    let wrong = FactId::new(DECL, 1).expect("an id");
    let err =
        intern_fact(&db, &schema, &decl(WireRef::Id(wrong), 1, "f")).expect_err("a type mismatch");

    assert!(err.is_peers_fault());
}
