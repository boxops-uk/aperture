//! **The sample code index** — `schemas/code.sigla`, parsed, and the name lookups the
//! rest of this crate's tests, benchmarks and instruments resolve against it.
//!
//! One fact per thing, and everything about a thing pointing at it by
//! [`FactId`](fjord_schema::id::FactId) rather than repeating it. It is the shape the
//! .NET demo and the real Roslyn indexer write, the shape the viewer reads, and the
//! shape every number in `bench/FINDINGS.md` was measured over.
//!
//! **It is not a default, and there is no longer such a thing.** Until 0.0.1 this was
//! *the built-in schema*: what a database got when `create` was not given a path. That
//! made a default decide how every stored row of somebody's database decoded, and made
//! the artifact a property of which build of the tool created it. `--schema` is required
//! now, and what is left here is a **fixture** — one worked example, in one place, so an
//! instrument cannot declare its own and end up measuring a database it could not have
//! written.
//!
//! **Nothing here states a schema.** Until Phase 8.4 this file *was* one: twenty-two
//! predicates of hand-written Rust, with six id constants beside them written down a
//! second time. Both are gone. `schemas/code.sigla` is the single statement, in the
//! language `fjord create --schema` takes, and this module is the two lines that parse it
//! plus the lookups that used to be constants. What is left to guard is therefore not
//! "does the vector still say what it said" but "does the *file* still declare what the
//! rest of the tree names" — which is what `tests` below asks.
//!
//! **Three layers, and the joins between them are the point.** Nine predicates are the
//! source layer any syntax walk can fill — files, modules, declarations, references,
//! their spans, the two search indexes — with `src.Line` holding the file's text beside
//! them. Fifteen more are what a *compiler* and a *build system* know and a syntax walk
//! does not: which project a file is compiled by and into which assembly, what a type
//! extends, what a member overrides, what a parameter's type is spelled as, what the doc
//! comment says. Those are written by
//! [`Boxops.Fjord.Indexer`](../../../clients/dotnet/Boxops.Fjord.Indexer/README.md), which has Roslyn and
//! MSBuild to answer them with. A predicate nobody fills is an empty keyspace pair, which
//! costs the ~30 ms it takes to create it and nothing after that.
//!
//! **Three of the twenty-seven are the same data keyed a second way**, and they are here
//! because a predicate leads with one field and two questions want different ones:
//! `src.SearchByName` against `src.Decl`, `src.FileXRef` against `src.Ref`,
//! `src.AttributeOf` and `src.DerivesFrom` against their originals. Each is what a
//! *stored derivation* would materialise ([Phase 8b](../../../PLAN.md)); until one can be
//! declared, the producer writes both orders.

use std::sync::LazyLock;

use fjord_schema::{
    schema::{PredicateId, Schema},
    syntax,
};

/// The schema itself, as text.
///
/// **The file is the schema**, and it is a file a person can read, diff, and pass to
/// `fjord create --schema` — which is exactly what the scripts and the two integration
/// suites do. Compiled in here so a bench does not have to find it on disk.
const SOURCE: &str = include_str!("../../../schemas/code.sigla");

/// The schema everything here resolves names against: **a code index**, which is the
/// canonical shape for a fact database — one fact per thing, and everything about a
/// thing pointing at it rather than repeating it.
///
/// **There are no id constants, and that is the point.** An id is a *position*, and
/// positions come from sorting the schema's names ([D1](../../../website/content/schema-language.md)),
/// so a constant would be a second statement of something the schema already decides —
/// wrong the first time somebody adds a predicate that sorts earlier. Ask [`id`] by
/// name. Nothing outside this process ever sees one anyway: a block header carries the
/// predicate's *name*, so a client keeps no table to fall out of step.
///
/// What each predicate is here to show:
///
/// | predicate | shows |
/// |---|---|
/// | `src.File` | a **scalar** key — a path is one string, and needs no record |
/// | `src.Module` | a **reference**, so a module names its file rather than repeating the path |
/// | `src.Decl` | a **value side**, so `D.value` has something to read, plus a second reference |
/// | `src.SearchByName` | **key order is the index**: the same names keyed so a prefix narrows |
/// | `src.Ref` | a **nested record** key field, and two references to two predicates, reached through an open pattern |
/// | `src.Import` | two references to one predicate, which is what a graph edge is |
/// | `src.Project` · `src.Assembly` | scalar keys again, and the two ends of the build layer |
/// | `src.Compilation` | the **crossing**: a project, a target framework, and the assembly the pair produces |
/// | `src.ProjectSource` · `src.ProjectRef` · `src.PackageRef` | edges, which is how a many-to-many is said without arrays |
/// | `src.Package` | a **compound identity** — a package is its name *and* its version, and neither alone |
/// | `src.Member` · `src.Extends` · `src.Implements` · `src.Override` | the declaration graph, all four keyed **container-first** so the fan-out direction is the seek |
/// | `src.Param` | an `Int` in the middle of a key, so a method's parameters come back **in order** |
/// | `src.TypeOf` · `src.Doc` | a key of one field, which is a thing an *attribute* of something else is |
/// | `src.Attribute` | a string leading the key, so `[Obsolete]` everywhere is a seek rather than a scan |
/// | `src.Line` | the **wide row**: a file's line table, one fact per line, the text on the value side |
///
/// **Why the field order decides the seeks, and why it is declared rather than derived.**
/// A record's fields are stored in the order `schemas/code.sigla` lists them, that order
/// *is* the key order, and a query can only narrow on a leading run of it. So
/// `src.Extends` is declared `{base, type}` because "everything deriving from this" is
/// the question worth a seek; `{iface, type}`, `{container, member}` and
/// `{attribute, target}` are the same choice made three more times. Lowering preserves
/// declaration order for exactly this reason — it does not sort a record's fields.
///
/// The schema used to keep every field list in **alphabetical** order and to say that
/// the order followed from the names — that renaming `base` to `super` would silently
/// change what `src.Extends` answers. Nothing sorts these slices: `flatten` walks the
/// schema's own slice by index and looks each query field up by name, and
/// `fjord_store::fact`'s `the_encoding_order_is_the_declared_order` has always pinned
/// that. What the alphabetical habit did was make the physical key order a *consequence*
/// of naming, which is how `src.Decl` came to lead with a line number and `src.Ref` with
/// a column — the two most expensive keys in the index, both by accident.
///
/// The order is now chosen per predicate and stated in `tests::KEY_ORDER`, which is the
/// guard: a field list that changes silently answers a different question, and asserting
/// the intended order catches that where asserting sortedness only caught it when the
/// intended order happened to be alphabetical.
pub fn schema() -> Schema {
    /// Parsed once. `Schema` is `Arc`-backed, so handing out clones is a refcount bump
    /// rather than a re-parse — which matters because every connection asks for one.
    static SCHEMA: LazyLock<Schema> = LazyLock::new(|| parse_or_panic(SOURCE, None));

    SCHEMA.clone()
}

/// The predicate a name denotes in the sample schema.
///
/// # Panics
///
/// If the schema does not declare it, which is a bug in the caller rather than input:
/// every name passed here is a literal in this repository.
#[must_use]
pub fn id(name: &str) -> PredicateId {
    schema()
        .find_position(name)
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("`schemas/code.sigla` declares no `{name}`"))
}

/// Parse a schema, or explain why the build is broken.
///
/// A schema compiled into the binary is not input — it ships with the program — so a
/// failure here is a bug rather than a bad file, and the panic carries every diagnostic
/// so it says which line.
fn parse_or_panic(source: &str, _path: Option<&str>) -> Schema {
    let mut diags = vec![];

    let Some(cst) = syntax::parse::parse(source, &mut diags) else {
        panic!("`schemas/code.sigla` does not parse: {diags:#?}");
    };

    let Some(lowered) = syntax::lower::lower(&cst, &mut diags) else {
        panic!("`schemas/code.sigla` does not lower: {diags:#?}");
    };

    assert!(
        diags.is_empty(),
        "`schemas/code.sigla` is not clean: {diags:#?}"
    );

    lowered.schema
}

#[cfg(test)]
mod tests {
    use fjord_schema::schema::PredicateTy;

    use super::*;

    /// **The schema declares what it is supposed to declare.**
    ///
    /// This used to check six hand-written id constants against their names, which was
    /// the right guard while a position was written down twice. The positions are gone —
    /// `id` asks the schema — so what is left to check is the *membership*: that the file
    /// still holds the predicates the rest of the tree names, and that asking
    /// for one by name answers with it.
    #[test]
    fn the_schema_declares_what_the_tree_names() {
        let schema = schema();
        assert_eq!(
            schema.len(),
            27,
            "`schemas/code.sigla` is twenty-seven predicates"
        );

        for name in [
            "src.File",
            "src.Module",
            "src.Decl",
            "src.SearchByName",
            "src.Ref",
            "src.Import",
            "src.Project",
            "src.Assembly",
            "src.Package",
            "src.Line",
            // The five a code-search viewer needs and a syntax walk alone cannot
            // key for — three of them second key orders over data already here.
            "src.DeclSpan",
            "src.SearchByLowerName",
            "src.FileXRef",
            "src.DerivesFrom",
            "src.AttributeOf",
        ] {
            assert_eq!(
                schema.get(id(name)).and_then(|p| p.name()),
                Some(name),
                "`{name}` is not where the schema says it is"
            );
        }
    }

    /// **The .NET client states this schema independently, and must still agree.**
    ///
    /// The golden records the fingerprint `Boxops.Fjord.Demo` computed from its own
    /// twenty-seven declarations. `byte_identical_with_dotnet` compares that against a
    /// *third* statement in Rust, which is what makes the codec argument; what neither
    /// checks is whether either agrees with the schema the **server** actually serves,
    /// because that one is parsed from `schemas/code.sigla` and nothing else reads it.
    ///
    /// Until this test, drift there surfaced as a failed handshake in `run-demo.sh` —
    /// a real guard, but one that needs `dotnet` and a running server to fire. This is
    /// the same claim as a string compare.
    ///
    /// Regenerate with `./clients/dotnet/emit-golden.sh` when the schema moves on
    /// purpose; both sides move together, which is the point.
    #[test]
    fn the_dotnet_clients_schema_is_this_one() {
        const GOLDEN: &str = include_str!("../../../clients/dotnet/golden/blocks.txt");

        let recorded = GOLDEN
            .lines()
            .find_map(|line| line.strip_prefix("schema-fingerprint "))
            .map(str::trim)
            .and_then(|hex| u64::from_str_radix(hex, 16).ok())
            .expect("the golden names a schema fingerprint");

        assert_eq!(
            fjord_schema::fingerprint::of(&schema()),
            recorded,
            "`schemas/code.sigla` and the .NET client's declaration have drifted — \
             the demo would be refused at the handshake"
        );
    }

    /// **Every stored key, flat, in the order its bytes go down in.**
    ///
    /// A nested record is spliced into its parent's key rather than framed, so this is
    /// the physical key and `at.line` is a position in it exactly as `to` is. Read it as
    /// the index design: a query narrows on a **leading run** of these fields and filters
    /// on the rest, so the first name in each row is the question that predicate is fast
    /// at, and everything after it is a tie-break.
    ///
    /// The build layer's four are alphabetical because they were written that way and
    /// nothing has measured a reason to disagree — they are thousands of rows, not
    /// millions. That is a different statement from the two that were changed, and it is
    /// here so the next reader can tell a decision from an inheritance.
    const KEY_ORDER: &[(&str, &[&str])] = &[
        ("src.Module", &["file", "name"]),
        ("src.Decl", &["module", "name", "line"]),
        ("src.DeclSpan", &["decl", "col", "endLine", "endCol"]),
        ("src.SearchByName", &["name", "to"]),
        ("src.SearchByLowerName", &["name", "to"]),
        ("src.Ref", &["to", "file", "at.line", "at.col", "at.length"]),
        (
            "src.FileXRef",
            &["file", "at.line", "at.col", "at.length", "to"],
        ),
        ("src.Import", &["from", "to"]),
        ("src.Compilation", &["assembly", "framework", "project"]),
        ("src.ProjectSource", &["file", "project"]),
        ("src.ProjectRef", &["from", "to"]),
        ("src.Package", &["name", "version"]),
        ("src.PackageRef", &["package", "project"]),
        ("src.Member", &["container", "member"]),
        ("src.Extends", &["base", "type"]),
        ("src.Implements", &["iface", "type"]),
        ("src.Override", &["base", "member"]),
        ("src.DerivesFrom", &["type", "base"]),
        ("src.Param", &["decl", "index", "name"]),
        ("src.TypeOf", &["decl"]),
        ("src.Doc", &["decl"]),
        ("src.Attribute", &["attribute", "target"]),
        ("src.AttributeOf", &["target", "attribute"]),
        ("src.Line", &["file", "line"]),
    ];

    /// **A record's fields are stored in the order this file declares them.**
    ///
    /// A field list one swap out of order encodes fine, stores fine, and answers a
    /// different question — a predicate narrows on its leading fields, so `{base, type}`
    /// typed the other way round silently indexes the derived type instead of the base.
    /// Nothing else in the tree would notice.
    ///
    /// **This used to assert the fields were *sorted*, and that guard was worse than it
    /// looked.** It caught a swap only where the intended order happened to be
    /// alphabetical, and everywhere else it enforced the accident: `src.Decl` led with a
    /// line number and `src.Ref` with a column, which cost 56,274 rows examined per row
    /// produced on an ordinary join and made find-references unanswerable
    /// ([findings §2 and §11](../../../bench/FINDINGS.md)). A guard that pins the *intended*
    /// order catches the same swap and cannot enforce an accident, because somebody has
    /// to write the intention down.
    #[test]
    fn every_record_lists_its_fields_in_the_intended_order() {
        let schema = schema();

        fn walk(ty: &PredicateTy, schema: &Schema, prefix: &str, into: &mut Vec<String>) {
            let PredicateTy::Record(fields) = ty else {
                return;
            };

            for (field, ty) in fields.iter() {
                let name = schema.interner().resolve(*field).expect("a field name");
                let path = if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}.{name}")
                };

                if matches!(ty, PredicateTy::Record(_)) {
                    walk(ty, schema, &path, into);
                } else {
                    into.push(path);
                }
            }
        }

        for index in 0..schema.len() {
            let predicate = schema.get(PredicateId(index as u32)).expect("in range");
            let name = predicate.name().expect("a name");

            let mut key = Vec::new();
            walk(&predicate.predicate().key, &schema, "", &mut key);

            // A value side is not a key and never seeks, but a record in one would
            // still be stored in declaration order — and there is none today, so this
            // asserts that rather than leaving the next one unexamined.
            assert!(
                !matches!(predicate.predicate().value, Some(PredicateTy::Record(_))),
                "`{name}` has a record value side, which needs a decision and an entry here"
            );

            let expected = KEY_ORDER.iter().find(|(p, _)| *p == name).map(|(_, k)| *k);

            match expected {
                Some(expected) => assert_eq!(
                    key, expected,
                    "`{name}`'s stored key is not the one KEY_ORDER declares, \
                     which changes what it narrows on"
                ),
                None => assert!(
                    key.is_empty(),
                    "`{name}` has a record key and no entry in KEY_ORDER"
                ),
            }
        }

        for (name, _) in KEY_ORDER {
            assert!(
                (0..schema.len()).any(|index| schema
                    .get(PredicateId(index as u32))
                    .and_then(|p| p.name())
                    == Some(name)),
                "KEY_ORDER names `{name}`, which is not in the schema"
            );
        }
    }

    /// Every reference has to name a predicate that is in the schema — the one way an
    /// appended predicate can still break an existing one is by being pointed at from
    /// a type whose target was mistyped.
    #[test]
    fn every_reference_points_somewhere_that_exists() {
        let schema = schema();

        fn walk(ty: &PredicateTy, len: usize, name: &str) {
            match ty {
                PredicateTy::Fact(target) => assert!(
                    (target.0 as usize) < len,
                    "`{name}` references predicate {}, and the schema holds {len}",
                    target.0
                ),
                PredicateTy::Record(fields) => {
                    for (_, field) in fields.iter() {
                        walk(field, len, name);
                    }
                }
                PredicateTy::Union(alts) => {
                    for alt in alts.iter() {
                        walk(&alt.ty, len, name);
                    }
                }
                PredicateTy::Int | PredicateTy::Str => {}
            }
        }

        for index in 0..schema.len() {
            let id = PredicateId(index as u32);
            let predicate = schema.get(id).expect("in range");
            let name = predicate.name().expect("a name");

            walk(&predicate.predicate().key, schema.len(), name);

            if let Some(value) = predicate.predicate().value.as_ref() {
                walk(value, schema.len(), name);
            }
        }
    }
}
