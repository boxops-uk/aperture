//! **The built-in schema** — a code index, now *read* rather than written.
//!
//! One fact per thing, and everything about a thing pointing at it by
//! [`FactId`](aperture_schema::id::FactId) rather than repeating it. It is the shape
//! `example/index.py` emits ([`example/README.md`](../../example/README.md)), the
//! shape the shell queries, and the shape a client writes against — which is the
//! point of it living here rather than in either binary.
//!
//! **Nothing here states a schema any more.** Until Phase 8.4 this file *was* the
//! schema: twenty-two predicates of hand-written Rust, with six id constants beside
//! them written down a second time. Both are gone. `schemas/code.aps` is the single
//! statement, in the language `aperture create --schema` takes, and this module is the
//! two lines that parse it plus the lookups that used to be constants. What is left to
//! guard is therefore not "does the vector still say what it said" but "does the *file*
//! still declare what the rest of the tree names" — which is what `tests` below asks.
//!
//! **Three layers, and the joins between them are the point.** Six predicates are the
//! source layer every indexer here fills — files, modules, declarations, references —
//! and `src.Line` is the file's text beside them. The other fifteen are what a
//! *compiler* and a *build system* know and a syntax walk does not: which project a
//! file is compiled by and into which assembly, what a type extends, what a member
//! overrides, what a parameter's type is spelled as, what the doc comment says.
//! They are written by
//! [`Aperture.Indexer`](../../clients/dotnet/Aperture.Indexer/README.md), which has
//! Roslyn and MSBuild to answer them with; `example/index.py` fills only the first six
//! and says so. A predicate nobody fills is an empty keyspace pair, which costs the
//! ~30 ms it takes to create it and nothing after that.
//!
//! **This module is still not the end state.** A database carries its own canonical
//! schema ([I13](../../docs/invariants.md#i13)), so once `create --schema <path>` is the
//! ordinary way in, what remains here is a *default* rather than a definition — the
//! schema you get when you did not name one.

use std::sync::LazyLock;

use aperture_schema::{
    schema::{PredicateId, Schema},
    syntax,
};

/// The schema itself, as text.
///
/// **This file is the schema.** It was twenty-two predicates of hand-written Rust until
/// Phase 8.4; `schemas/code.aps` is now the only statement of it in this crate, and it
/// is a file a person can read, diff, and pass to `aperture create --schema`.
const SOURCE: &str = include_str!("../schemas/code.aps");

/// The catalogue, declared in the same language as everything else.
const CATALOGUE_SOURCE: &str = include_str!("../schemas/catalogue.aps");

/// The one virtual predicate, by name.
pub const CATALOGUE_NAME: &str = "aperture.db.List";

/// The schema everything here resolves names against: **a code index**, which is the
/// canonical shape for a fact database — one fact per thing, and everything about a
/// thing pointing at it rather than repeating it.
///
/// **There are no id constants, and that is the point.** An id is a *position*, and
/// positions come from sorting the schema's names ([D1](../../docs/phase-8-schemas.md)),
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
/// A record's fields are stored in the order `schemas/code.aps` lists them, that order
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
/// `aperture_store::fact`'s `the_encoding_order_is_the_declared_order` has always pinned
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

/// The id `aperture.db.List` takes — **looked up, never assumed**.
///
/// Ids come from sorting a schema's names ([D1](../../docs/phase-8-schemas.md)), so a
/// position is a fact about the whole schema rather than about one declaration. A
/// constant here would be a second statement of it, and the wrong one the first time
/// somebody adds a predicate sorting earlier.
#[must_use]
pub fn catalogue_id() -> PredicateId {
    with_catalogue()
        .find_position(CATALOGUE_NAME)
        .map(|(id, _)| id)
        .expect("with_catalogue declares the catalogue")
}

/// The predicate a name denotes in the built-in schema.
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
        .unwrap_or_else(|| panic!("the built-in schema declares no `{name}`"))
}

/// The schema a **server** answers queries against: the stored one, plus the catalogue.
///
/// **Virtual predicates belong to the server, not to the database**, and everything
/// about how this is put together follows from that one sentence. `aperture.db.List` is
/// not in [`schema`], so it is not in the handshake fingerprint, not in the copy
/// embedded at create, and not a pair of keyspaces in any artifact — which is why a
/// client that has never heard of it still connects, and why the .NET clients did not
/// have to be told.
///
/// Assembled by parsing the stored source **and** the catalogue's, rather than by
/// restating either: two schemas built from one text cannot drift, and the catalogue is
/// declared in the same language as everything else.
#[must_use]
pub fn with_catalogue() -> Schema {
    static SERVED: LazyLock<Schema> = LazyLock::new(|| {
        let schema = parse_or_panic(&format!("{SOURCE}\n{CATALOGUE_SOURCE}"), None);
        let id = schema
            .find_position(CATALOGUE_NAME)
            .map(|(id, _)| id)
            .expect("the catalogue source declares it");

        schema.with_virtual([id])
    });

    SERVED.clone()
}

/// Parse a schema, or explain why the build is broken.
///
/// A schema compiled into the binary is not input — it ships with the program — so a
/// failure here is a bug rather than a bad file, and the panic carries every diagnostic
/// so it says which line.
fn parse_or_panic(source: &str, _path: Option<&str>) -> Schema {
    let mut diags = vec![];

    let Some(cst) = syntax::parse::parse(source, &mut diags) else {
        panic!("the built-in schema does not parse: {diags:#?}");
    };

    let Some(lowered) = syntax::lower::lower(&cst, &mut diags) else {
        panic!("the built-in schema does not lower: {diags:#?}");
    };

    assert!(
        diags.is_empty(),
        "the built-in schema is not clean: {diags:#?}"
    );

    lowered.schema
}

#[cfg(test)]
mod tests {
    use aperture_schema::schema::PredicateTy;

    use super::*;

    /// **The schema declares what it is supposed to declare.**
    ///
    /// This used to check six hand-written id constants against their names, which was
    /// the right guard while a position was written down twice. The positions are gone —
    /// `id` asks the schema — so what is left to check is the *membership*: that the file
    /// still holds the twenty-two predicates the rest of the tree names, and that asking
    /// for one by name answers with it.
    #[test]
    fn the_schema_declares_what_the_tree_names() {
        let schema = schema();
        assert_eq!(
            schema.len(),
            22,
            "the built-in schema is twenty-two predicates"
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
        ] {
            assert_eq!(
                schema.get(id(name)).and_then(|p| p.name()),
                Some(name),
                "`{name}` is not where the schema says it is"
            );
        }
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
        ("src.SearchByName", &["name", "to"]),
        ("src.Ref", &["to", "file", "at.line", "at.col"]),
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
        ("src.Param", &["decl", "index", "name"]),
        ("src.TypeOf", &["decl"]),
        ("src.Doc", &["decl"]),
        ("src.Attribute", &["attribute", "target"]),
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
    /// ([findings §2 and §11](../bench/FINDINGS.md)). A guard that pins the *intended*
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

#[cfg(test)]
mod catalogue {
    use super::*;

    /// **The property the whole arrangement rests on**: appending a virtual predicate
    /// does not change what a client has to agree with.
    ///
    /// If this ever fails, every .NET client stops connecting until it declares a
    /// predicate it can never write to — which is the outcome the virtual/stored split
    /// exists to avoid, and the reason `provisional_fingerprint` skips virtuals rather
    /// than the server keeping two schemas and hoping they stay in step.
    #[test]
    fn the_catalogue_does_not_change_the_handshake() {
        let stored = aperture_wire::protocol::provisional_fingerprint(&schema());
        let served = aperture_wire::protocol::provisional_fingerprint(&with_catalogue());

        assert_eq!(
            stored, served,
            "a virtual predicate must be invisible to the handshake"
        );
    }

    /// Restating the stored schema must not move an id, because an id is a position
    /// and is the tag in every `FactId` already written.
    #[test]
    fn restating_the_stored_schema_moves_no_id() {
        let stored = schema();
        let served = with_catalogue();

        assert_eq!(served.len(), stored.len() + 1, "one appended, nothing else");

        for index in 0..stored.len() {
            let id = PredicateId(index as u32);
            assert_eq!(
                served.get(id).and_then(|p| p.name()),
                stored.get(id).and_then(|p| p.name()),
                "predicate {index} moved"
            );
        }
    }

    /// Only the catalogue is virtual, and it is.
    #[test]
    fn the_catalogue_is_the_only_virtual_predicate() {
        let served = with_catalogue();

        let catalogue = catalogue_id();

        assert_eq!(served.virtuals(), [catalogue]);
        assert_eq!(
            served.get(catalogue).and_then(|p| p.name()),
            Some(CATALOGUE_NAME)
        );
        assert!(schema().virtuals().is_empty(), "the stored schema has none");
    }
}
