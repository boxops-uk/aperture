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
const PROJECT: PredicateId = PredicateId(6);
const ASSEMBLY: PredicateId = PredicateId(7);
const PACKAGE: PredicateId = PredicateId(11);
const PARAM: PredicateId = PredicateId(17);
const DOC: PredicateId = PredicateId(19);

/// The demo's schema, restated in Rust.
///
/// Two rules here are load-bearing and are exactly what the fingerprint checks: a
/// predicate's id **is** its position, and a record's fields are in the order the schema
/// declares them, because that order is part of the encoding. It is *not* alphabetical —
/// `src.Decl` and `src.Ref` are declared for the seeks they are asked to serve — which is
/// the whole reason this file states the schema again instead of importing one.
fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let mut sym = |name: &str| rodeo.get_or_intern(name);

    let (file, module, decl) = (sym("src.File"), sym("src.Module"), sym("src.Decl"));
    let (search, reference, import) = (sym("src.SearchByName"), sym("src.Ref"), sym("src.Import"));
    let (project, assembly, compilation) = (
        sym("src.Project"),
        sym("src.Assembly"),
        sym("src.Compilation"),
    );
    let (project_source, project_ref) = (sym("src.ProjectSource"), sym("src.ProjectRef"));
    let (package, package_ref) = (sym("src.Package"), sym("src.PackageRef"));
    let (member, extends, implements, overrides) = (
        sym("src.Member"),
        sym("src.Extends"),
        sym("src.Implements"),
        sym("src.Override"),
    );
    let (param, type_of, doc, attribute, line_of) = (
        sym("src.Param"),
        sym("src.TypeOf"),
        sym("src.Doc"),
        sym("src.Attribute"),
        sym("src.Line"),
    );

    let (f_at, f_col, f_file, f_from) = (sym("at"), sym("col"), sym("file"), sym("from"));
    let (f_line, f_module, f_name, f_to) = (sym("line"), sym("module"), sym("name"), sym("to"));
    let (f_assembly, f_framework, f_project) = (sym("assembly"), sym("framework"), sym("project"));
    let (f_package, f_version, f_container) = (sym("package"), sym("version"), sym("container"));
    let (f_member, f_base, f_type, f_iface) =
        (sym("member"), sym("base"), sym("type"), sym("iface"));
    let (f_decl, f_index, f_attribute, f_target) =
        (sym("decl"), sym("index"), sym("attribute"), sym("target"));

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
                    (f_module, PredicateTy::Fact(MODULE)),
                    (f_name, PredicateTy::Str),
                    (f_line, PredicateTy::Int),
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
                    (f_to, PredicateTy::Fact(DECL)),
                    (f_file, PredicateTy::Fact(FILE)),
                    (
                        f_at,
                        PredicateTy::Record(Arc::from([
                            (f_line, PredicateTy::Int),
                            (f_col, PredicateTy::Int),
                        ])),
                    ),
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
            // The build layer. Nothing below is written by the demo, and all of it is
            // in the fingerprint — which is the point: a client that states a shorter
            // schema is refused at the handshake rather than at the first fact.
            Predicate {
                name: project,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: assembly,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: compilation,
                key: PredicateTy::Record(Arc::from([
                    (f_assembly, PredicateTy::Fact(ASSEMBLY)),
                    (f_framework, PredicateTy::Str),
                    (f_project, PredicateTy::Fact(PROJECT)),
                ])),
                value: None,
            },
            Predicate {
                name: project_source,
                key: PredicateTy::Record(Arc::from([
                    (f_file, PredicateTy::Fact(FILE)),
                    (f_project, PredicateTy::Fact(PROJECT)),
                ])),
                value: None,
            },
            Predicate {
                name: project_ref,
                key: PredicateTy::Record(Arc::from([
                    (f_from, PredicateTy::Fact(PROJECT)),
                    (f_to, PredicateTy::Fact(PROJECT)),
                ])),
                value: None,
            },
            Predicate {
                name: package,
                key: PredicateTy::Record(Arc::from([
                    (f_name, PredicateTy::Str),
                    (f_version, PredicateTy::Str),
                ])),
                value: None,
            },
            Predicate {
                name: package_ref,
                key: PredicateTy::Record(Arc::from([
                    (f_package, PredicateTy::Fact(PACKAGE)),
                    (f_project, PredicateTy::Fact(PROJECT)),
                ])),
                value: None,
            },
            // The declaration graph.
            Predicate {
                name: member,
                key: PredicateTy::Record(Arc::from([
                    (f_container, PredicateTy::Fact(DECL)),
                    (f_member, PredicateTy::Fact(DECL)),
                ])),
                value: None,
            },
            Predicate {
                name: extends,
                key: PredicateTy::Record(Arc::from([
                    (f_base, PredicateTy::Fact(DECL)),
                    (f_type, PredicateTy::Fact(DECL)),
                ])),
                value: None,
            },
            Predicate {
                name: implements,
                key: PredicateTy::Record(Arc::from([
                    (f_iface, PredicateTy::Fact(DECL)),
                    (f_type, PredicateTy::Fact(DECL)),
                ])),
                value: None,
            },
            Predicate {
                name: overrides,
                key: PredicateTy::Record(Arc::from([
                    (f_base, PredicateTy::Fact(DECL)),
                    (f_member, PredicateTy::Fact(DECL)),
                ])),
                value: None,
            },
            // A reference in the middle of a key, an integer after it, and a value
            // behind both.
            Predicate {
                name: param,
                key: PredicateTy::Record(Arc::from([
                    (f_decl, PredicateTy::Fact(DECL)),
                    (f_index, PredicateTy::Int),
                    (f_name, PredicateTy::Str),
                ])),
                value: Some(PredicateTy::Str),
            },
            // A key of one field.
            Predicate {
                name: type_of,
                key: PredicateTy::Record(Arc::from([(f_decl, PredicateTy::Fact(DECL))])),
                value: Some(PredicateTy::Str),
            },
            Predicate {
                name: doc,
                key: PredicateTy::Record(Arc::from([(f_decl, PredicateTy::Fact(DECL))])),
                value: Some(PredicateTy::Str),
            },
            Predicate {
                name: attribute,
                key: PredicateTy::Record(Arc::from([
                    (f_attribute, PredicateTy::Str),
                    (f_target, PredicateTy::Fact(DECL)),
                ])),
                value: None,
            },
            Predicate {
                name: line_of,
                key: PredicateTy::Record(Arc::from([
                    (f_file, PredicateTy::Fact(FILE)),
                    (f_line, PredicateTy::Int),
                ])),
                value: Some(PredicateTy::Str),
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

/// Fields in the schema's order — module, name, line — and the kind on the value side.
fn decl(path: &str, module_name: &str, kind: &str, line: i64, name: &str) -> WireFact {
    WireFact {
        predicate: DECL,
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(module(path, module_name)))),
            WireValue::Str(name.to_owned()),
            WireValue::Int(line),
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
                    WireValue::Ref(WireRef::Nested(Box::new(decl(
                        "store/keys.py",
                        "keys",
                        "def",
                        12,
                        "key_of",
                    )))),
                    WireValue::Ref(WireRef::Nested(Box::new(file("query/plan.py")))),
                    WireValue::Record(Box::from([WireValue::Int(19), WireValue::Int(4)])),
                ])),
                value: None,
            }],
        ),
        // A reference in the *middle* of a key, an integer after it, and a value side
        // behind all three — and a negative integer, since zigzag is where two codecs
        // that agree about every positive one can still disagree.
        (
            "src.Param",
            PARAM,
            vec![param(0, "key", "bytes"), param(-1, "rest", "int")],
        ),
        // A key of one field, which encodes as the bare reference does.
        (
            "src.Doc",
            DOC,
            vec![WireFact {
                predicate: DOC,
                key: WireValue::Record(Box::from([WireValue::Ref(WireRef::Nested(Box::new(
                    decl("query/plan.py", "plan", "class", 5, "Plan"),
                )))])),
                value: Some(WireValue::Str(
                    "A plan is an ordered list of steps.".to_owned(),
                )),
            }],
        ),
    ]
}

/// A parameter of `key_of`, which is the declaration three of the blocks above already
/// nest — so this block is also the same nested fact reached a fourth way.
fn param(index: i64, name: &str, ty: &str) -> WireFact {
    WireFact {
        predicate: PARAM,
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(decl(
                "store/keys.py",
                "keys",
                "def",
                12,
                "key_of",
            )))),
            WireValue::Int(index),
            WireValue::Str(name.to_owned()),
        ])),
        value: Some(WireValue::Str(ty.to_owned())),
    }
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

        // The header names its predicate now rather than numbering it, so this
        // asserts the *name* — and that the reader resolves it to the id it expects.
        assert_eq!(header.predicate, *name, "{name}");
        assert_eq!(
            schema.find_position(name).map(|(id, _)| id),
            Some(*predicate),
            "`{name}` does not resolve to the id the corpus declares"
        );
        assert_eq!(header.count as usize, facts.len(), "{name}");
        assert_eq!(&decoded, facts, "`{name}` decodes to different facts");
    }
}
