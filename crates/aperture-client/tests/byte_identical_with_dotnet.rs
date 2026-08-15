//! **Phase 9e's acceptance criterion**: the Rust and C# clients produce byte-identical
//! blocks for the same facts.
//!
//! Interoperating today does not prove that, and the difference matters. Two encoders
//! can disagree about something the server happens to tolerate, or about a case neither
//! demo exercises, and a fact file written by one would then not be the file the other
//! writes — which is a problem that surfaces at 7b, in a fact file nobody can split,
//! long after the two implementations parted company.
//!
//! So the C# client writes its answer for a fixed corpus to
//! `clients/dotnet/golden/blocks.txt` (`./clients/dotnet/emit-golden.sh`), and this
//! encodes the same facts and compares. **The schema and the corpus are stated
//! independently on each side** — three times over, counting `aperture::code_index` —
//! and that is deliberate rather than duplication to be tidied away: a shared statement
//! would make the two encoders agree by construction, which is precisely the agreement
//! this is trying to test.
//!
//! The test needs no `dotnet` to run. Regenerating the golden does.

use std::sync::Arc;

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_wire::{WireFact, WireRef, WireValue, encode_block, provisional_fingerprint};
use lasso::Rodeo;

const FILE: PredicateId = PredicateId(0);
const MODULE: PredicateId = PredicateId(1);
const DECL: PredicateId = PredicateId(2);
const REFERENCE: PredicateId = PredicateId(4);

/// The demo's schema, restated in Rust.
///
/// Two rules here are load-bearing and are exactly what the fingerprint checks: a
/// predicate's id **is** its position, and a record's fields are in declared order,
/// sorted by name, because that order is part of the encoding.
fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let mut sym = |name: &str| rodeo.get_or_intern(name);

    let (file, module, decl) = (sym("src.File"), sym("src.Module"), sym("src.Decl"));
    let (search, reference, import) = (sym("src.SearchByName"), sym("src.Ref"), sym("src.Import"));

    let (f_at, f_col, f_file, f_from) = (sym("at"), sym("col"), sym("file"), sym("from"));
    let (f_line, f_module, f_name, f_to) = (sym("line"), sym("module"), sym("name"), sym("to"));

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![
            Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: module,
                key: PredicateTy::Record(Arc::from([
                    (f_file, PredicateTy::Fact(FILE)),
                    (f_name, PredicateTy::Str),
                ])),
                value: None,
            },
            // A value side: the declaration's kind.
            Predicate {
                name: decl,
                key: PredicateTy::Record(Arc::from([
                    (f_line, PredicateTy::Int),
                    (f_module, PredicateTy::Fact(MODULE)),
                    (f_name, PredicateTy::Str),
                ])),
                value: Some(PredicateTy::Str),
            },
            Predicate {
                name: search,
                key: PredicateTy::Record(Arc::from([
                    (f_name, PredicateTy::Str),
                    (f_to, PredicateTy::Fact(DECL)),
                ])),
                value: None,
            },
            // A nested record inside a key, and two references to two predicates.
            Predicate {
                name: reference,
                key: PredicateTy::Record(Arc::from([
                    (
                        f_at,
                        PredicateTy::Record(Arc::from([
                            (f_col, PredicateTy::Int),
                            (f_line, PredicateTy::Int),
                        ])),
                    ),
                    (f_file, PredicateTy::Fact(FILE)),
                    (f_to, PredicateTy::Fact(DECL)),
                ])),
                value: None,
            },
            Predicate {
                name: import,
                key: PredicateTy::Record(Arc::from([
                    (f_from, PredicateTy::Fact(MODULE)),
                    (f_to, PredicateTy::Fact(MODULE)),
                ])),
                value: None,
            },
        ]),
    )
}

fn file(path: &str) -> WireFact {
    WireFact {
        predicate: FILE,
        key: WireValue::Str(path.to_owned()),
        value: None,
    }
}

fn module(path: &str, name: &str) -> WireFact {
    WireFact {
        predicate: MODULE,
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(file(path)))),
            WireValue::Str(name.to_owned()),
        ])),
        value: None,
    }
}

/// Fields in the schema's order — line, module, name — and the kind on the value side.
fn decl(path: &str, module_name: &str, kind: &str, line: i64, name: &str) -> WireFact {
    WireFact {
        predicate: DECL,
        key: WireValue::Record(Box::from([
            WireValue::Int(line),
            WireValue::Ref(WireRef::Nested(Box::new(module(path, module_name)))),
            WireValue::Str(name.to_owned()),
        ])),
        value: Some(WireValue::Str(kind.to_owned())),
    }
}

/// The same corpus `EmitGolden` encodes, stated here in Rust.
fn corpus() -> Vec<(&'static str, PredicateId, Vec<WireFact>)> {
    vec![
        (
            "src.File",
            FILE,
            vec![file("store/keys.py"), file("query/plan.py")],
        ),
        (
            "src.Decl",
            DECL,
            vec![
                decl("store/keys.py", "keys", "def", 12, "key_of"),
                decl("store/keys.py", "keys", "def", 0, "zero"),
                decl("query/plan.py", "plan", "class", 2_147_483_648, "Plan"),
            ],
        ),
        (
            "src.Ref",
            REFERENCE,
            vec![WireFact {
                predicate: REFERENCE,
                key: WireValue::Record(Box::from([
                    WireValue::Record(Box::from([WireValue::Int(4), WireValue::Int(19)])),
                    WireValue::Ref(WireRef::Nested(Box::new(file("query/plan.py")))),
                    WireValue::Ref(WireRef::Nested(Box::new(decl(
                        "store/keys.py",
                        "keys",
                        "def",
                        12,
                        "key_of",
                    )))),
                ])),
                value: None,
            }],
        ),
    ]
}

/// One golden line: what the C# client said a block's bytes are.
struct Golden {
    fingerprint: u64,
    blocks: Vec<(String, u32, Vec<u8>)>,
}

fn golden() -> Golden {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../clients/dotnet/golden/blocks.txt"
    );

    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("cannot read {path}: {error}\nregenerate with ./clients/dotnet/emit-golden.sh")
    });

    let mut fingerprint = None;
    let mut blocks = vec![];

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();

        match parts.next() {
            Some("schema-fingerprint") => {
                let hex = parts.next().expect("a fingerprint");
                fingerprint = Some(u64::from_str_radix(hex, 16).expect("hex"));
            }
            Some("block") => {
                let name = parts.next().expect("a predicate name").to_owned();
                let predicate: u32 = parts
                    .next()
                    .expect("a predicate id")
                    .parse()
                    .expect("a u32");
                let bytes = unhex(parts.next().expect("the block's bytes"));
                blocks.push((name, predicate, bytes));
            }
            other => panic!("a golden line this test does not understand: {other:?}"),
        }
    }

    Golden {
        fingerprint: fingerprint.expect("the golden names a schema fingerprint"),
        blocks,
    }
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "hex comes in pairs");

    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// **The criterion.** Same facts, same schema, same bytes.
#[test]
fn byte_identical_with_the_dotnet_client() {
    let golden = golden();
    let schema = schema();

    // First, because it explains every failure below it. Two clients that disagree
    // about the schema are not two clients that disagree about the codec, and being
    // told which one it is saves reading a hex diff to find out.
    assert_eq!(
        provisional_fingerprint(&schema),
        golden.fingerprint,
        "the two clients' schemas disagree, so their blocks were never going to match"
    );

    let corpus = corpus();
    assert_eq!(
        corpus.len(),
        golden.blocks.len(),
        "the corpora have drifted: {} blocks here, {} in the golden",
        corpus.len(),
        golden.blocks.len()
    );

    for ((name, predicate, facts), (golden_name, golden_predicate, expected)) in
        corpus.iter().zip(&golden.blocks)
    {
        assert_eq!(name, golden_name, "the corpora are in different orders");
        assert_eq!(predicate.0, *golden_predicate, "{name}");

        let mut block = vec![];
        encode_block(&mut block, &schema, *predicate, facts).expect("it encodes");

        assert_eq!(
            hex(&block),
            hex(expected),
            "`{name}` differs between the Rust and C# clients"
        );
    }
}

/// The golden is bytes on the wire, so it is also bytes this build can *read* — which
/// is worth checking separately, because an encoder and a decoder can agree with each
/// other while both disagree with everyone else.
#[test]
fn the_dotnet_clients_blocks_decode_here() {
    let schema = schema();

    for ((name, predicate, facts), (_, _, bytes)) in corpus().iter().zip(&golden().blocks) {
        let header = aperture_wire::block::decode_header(bytes)
            .unwrap_or_else(|error| panic!("`{name}`'s header does not decode: {error}"));
        let (decoded, _) = aperture_wire::decode_block(bytes, &schema)
            .unwrap_or_else(|error| panic!("`{name}` does not decode: {error}"));

        assert_eq!(header.predicate, *predicate, "{name}");
        assert_eq!(header.count as usize, facts.len(), "{name}");
        assert_eq!(&decoded, facts, "`{name}` decodes to different facts");
    }
}
