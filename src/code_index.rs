//! **The built-in schema** — a code index, hardcoded until schemas are parsed.
//!
//! One fact per thing, and everything about a thing pointing at it by
//! [`FactId`](aperture_schema::id::FactId) rather than repeating it. It is the shape
//! `example/index.py` emits ([`example/README.md`](../../example/README.md)), the
//! shape the shell queries, and the shape a client writes against — which is the
//! point of it living here rather than in either binary.
//!
//! **Three layers, and the joins between them are the point.** Predicates 0–5 are the
//! source layer every indexer here fills — files, modules, declarations, references —
//! and `src.Line` (21) is the file's text beside them. The other fifteen are what a
//! *compiler* and a *build system* know and a syntax walk does not: which project a
//! file is compiled by and into which assembly, what a type extends, what a member
//! overrides, what a parameter's type is spelled as, what the doc comment says.
//! They are written by
//! [`Aperture.Indexer`](../../clients/dotnet/Aperture.Indexer/README.md), which has
//! Roslyn and MSBuild to answer them with; `example/index.py` fills only the first six
//! and says so. A predicate nobody fills is an empty keyspace pair, which costs the
//! ~30 ms it takes to create it and nothing after that.
//!
//! **This function is deleted, not ported, when [Phase 8](../../PLAN.md) lands.** A
//! database will carry its own canonical schema then
//! ([I13](../../docs/invariants.md#i13)), and nothing will need a schema compiled
//! into a binary.

use std::sync::Arc;

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use lasso::Rodeo;

/// A predicate id **is** its position in the schema, and a `Fact` field names one — so
/// the ids of the predicates that are *pointed at* have to be written down before the
/// vector that defines them. Nothing checks it — this shell is a scaffold Phase 9
/// re-points at the wire client — so a wrong id here writes facts under the wrong
/// predicate and the queries below quietly return nothing.
///
/// **Appending is the only safe edit.** An id is a position, it is the tag in every
/// [`FactId`](aperture_schema::id::FactId) already written, and three other statements
/// of this schema agree with it by fingerprint rather than by construction — the two
/// .NET clients and `aperture-client`'s golden test. Inserting a predicate renumbers
/// every one below it.
pub const FILE: PredicateId = PredicateId(0);
pub const MODULE: PredicateId = PredicateId(1);
pub const DECL: PredicateId = PredicateId(2);
pub const PROJECT: PredicateId = PredicateId(6);
pub const ASSEMBLY: PredicateId = PredicateId(7);
pub const PACKAGE: PredicateId = PredicateId(11);

/// The schema this shell resolves names against: **a code index**, which is the
/// canonical shape for a fact database — one fact per thing, and everything about a
/// thing pointing at it rather than repeating it.
///
/// Record fields are listed **sorted by name**, as everywhere: a record's field order
/// is part of its encoding. The `Fact` impls below deliberately do *not* list them in
/// that order, because a hand-written deriver has no reason to know it — see
/// [`focus::fact`](aperture_store::fact).
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
/// A record's fields are stored in the order this file lists them, that order *is* the
/// key order, and a query can only narrow on a leading run of it. So `src.Extends` is
/// declared `{base, type}` because "everything deriving from this" is the question worth
/// a seek; `{iface, type}`, `{container, member}` and `{attribute, target}` are the same
/// choice made three more times.
///
/// This file used to keep every field list in **alphabetical** order and to say that the
/// order followed from the names — that renaming `base` to `super` would silently change
/// what `src.Extends` answers. Nothing sorts these slices: `flatten` walks the schema's
/// own slice by index and looks each query field up by name, and
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
    let mut rodeo = Rodeo::new();
    let mut sym = |name: &str| rodeo.get_or_intern(name);

    let predicates = vec![
        Predicate {
            name: sym("src.File"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: sym("src.Module"),
            key: PredicateTy::Record(Arc::from([
                (sym("file"), PredicateTy::Fact(FILE)),
                (sym("name"), PredicateTy::Str),
            ])),
            value: None,
        },
        // The value side is the declaration's *kind* — `def`, `class`, `method`,
        // `const` — because it is the one thing a query would want without matching on
        // it, and a value cannot be matched ([I6](../../docs/invariants.md#i6)).
        //
        // **Declared `{module, name, line}`, and the line goes last on purpose.** The
        // join anybody writes is "the declarations in this module", often narrowed by
        // name; the line number is what distinguishes two declarations that agree on
        // both, which is the definition of a field that belongs at the end of a key.
        // Alphabetical order put `line` first, and measured, that made the ordinary
        // join read all 888,177 declarations once per module — 56,274 rows examined per
        // row produced ([findings §2](../bench/FINDINGS.md)).
        Predicate {
            name: sym("src.Decl"),
            key: PredicateTy::Record(Arc::from([
                (sym("module"), PredicateTy::Fact(MODULE)),
                (sym("name"), PredicateTy::Str),
                (sym("line"), PredicateTy::Int),
            ])),
            value: Some(PredicateTy::Str),
        },
        // **The search index over declaration names**, and the one predicate here that
        // exists for a reason about *keys* rather than about the code: a declaration's
        // key begins with its module, so `src.Decl {name = "encode"..}` reaches the name
        // only after the scan has opened, and the prefix can filter rows but not narrow
        // to them. Keyed with `name` leading, the same prefix is a range, and `:plan`
        // shows the difference as `seek[name = "encode".., to = _]` against a `scan`
        // whose `where name starts with "encode"` is all it has.
        //
        // A key can only lead with one field and `src.Decl`'s leads with its module, so
        // choosing that order does not remove the need for this one — it is why both
        // orders exist rather than why one of them is wrong.
        //
        // It is the same names twice over, which is what a *derived* predicate is: data
        // a query could compute, stored keyed the way the query wants to read it.
        // Written by hand here because nothing can declare one yet
        // ([Phase 8b](../../PLAN.md)) — `example/index.py` emits it exactly as a deriver
        // would.
        Predicate {
            name: sym("src.SearchByName"),
            key: PredicateTy::Record(Arc::from([
                (sym("name"), PredicateTy::Str),
                (sym("to"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        // A location is the **file and the position together**, and the file is not
        // derivable from the rest of the row: `to` reaches the file the *declaration*
        // is in, which for most references is a different one — that being what a
        // reference is for. So the file is a key field of its own, and the row names
        // somewhere someone can go and look.
        //
        // **Declared `{to, file, at}`, because find-references is the question.** "Where
        // is this used" is the second thing anyone asks a code index and the first one
        // that has to be fast; it seeks only if the target leads. Alphabetical order led
        // with `at.col`, so a lookup by target scanned every reference in the database —
        // 4.9M rows for one declaration, 2.2 s, and unanswerable at all for a name many
        // declarations share ([findings §11](../bench/FINDINGS.md)). The file comes
        // second so "this declaration's uses in this file" narrows too.
        Predicate {
            name: sym("src.Ref"),
            key: PredicateTy::Record(Arc::from([
                (sym("to"), PredicateTy::Fact(DECL)),
                (sym("file"), PredicateTy::Fact(FILE)),
                (
                    sym("at"),
                    PredicateTy::Record(Arc::from([
                        (sym("line"), PredicateTy::Int),
                        (sym("col"), PredicateTy::Int),
                    ])),
                ),
            ])),
            value: None,
        },
        Predicate {
            name: sym("src.Import"),
            key: PredicateTy::Record(Arc::from([
                (sym("from"), PredicateTy::Fact(MODULE)),
                (sym("to"), PredicateTy::Fact(MODULE)),
            ])),
            value: None,
        },
        // ---- the build layer: what compiled this file, and into what ---------------
        //
        // A path, like `src.File`, and for the same reason: a project is one string and
        // needs no record. What it is *not* is the module — a module is a namespace,
        // which spans projects as freely as a project spans namespaces, and conflating
        // the two is the mistake that makes a dependency graph answer nonsense.
        Predicate {
            name: sym("src.Project"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: sym("src.Assembly"),
            key: PredicateTy::Str,
            value: None,
        },
        // **A compilation is the crossing, and it is where the multiplicity lives.**
        // One project builds for several target frameworks and each of those is a
        // separate compilation of the same sources into the same assembly name; one
        // assembly name is produced by several projects (a reference assembly and its
        // implementation, most obviously). Neither end is a function of the other, so
        // the fact is the pair, and the framework is a key field rather than a value
        // because "what does this project build for" is a question worth matching on.
        Predicate {
            name: sym("src.Compilation"),
            key: PredicateTy::Record(Arc::from([
                (sym("assembly"), PredicateTy::Fact(ASSEMBLY)),
                (sym("framework"), PredicateTy::Str),
                (sym("project"), PredicateTy::Fact(PROJECT)),
            ])),
            value: None,
        },
        // File → project, and it is genuinely many-to-many: a shared source file is
        // compiled by every project that includes it, which in a .NET repository is the
        // normal case rather than the exotic one. `file` leads the key because the
        // question asked of a search hit is "what builds this", and the hit is the file.
        Predicate {
            name: sym("src.ProjectSource"),
            key: PredicateTy::Record(Arc::from([
                (sym("file"), PredicateTy::Fact(FILE)),
                (sym("project"), PredicateTy::Fact(PROJECT)),
            ])),
            value: None,
        },
        Predicate {
            name: sym("src.ProjectRef"),
            key: PredicateTy::Record(Arc::from([
                (sym("from"), PredicateTy::Fact(PROJECT)),
                (sym("to"), PredicateTy::Fact(PROJECT)),
            ])),
            value: None,
        },
        // **A package's identity is the pair.** `Newtonsoft.Json 12.0.3` and
        // `Newtonsoft.Json 13.0.1` are not one thing that happens to have two versions
        // — a repository with both has a problem, and a schema that cannot say so
        // cannot be asked about it. `name` leads, so every version of one package is a
        // prefix seek.
        Predicate {
            name: sym("src.Package"),
            key: PredicateTy::Record(Arc::from([
                (sym("name"), PredicateTy::Str),
                (sym("version"), PredicateTy::Str),
            ])),
            value: None,
        },
        Predicate {
            name: sym("src.PackageRef"),
            key: PredicateTy::Record(Arc::from([
                (sym("package"), PredicateTy::Fact(PACKAGE)),
                (sym("project"), PredicateTy::Fact(PROJECT)),
            ])),
            value: None,
        },
        // ---- the declaration graph: four edges a syntax walk cannot see -------------
        //
        // Containment. `src.Decl`'s name is qualified — `Store.Cursor.Next` — which is
        // how a *person* reads the nesting; this is how a *query* joins on it, and the
        // two are not interchangeable. Splitting a name on dots to find a type's
        // members is string surgery that a generic arity or a nested name defeats.
        Predicate {
            name: sym("src.Member"),
            key: PredicateTy::Record(Arc::from([
                (sym("container"), PredicateTy::Fact(DECL)),
                (sym("member"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        // A type has one base and many descendants, so the useful direction is
        // base-first — and `base` sorting before `type` is what puts it there.
        Predicate {
            name: sym("src.Extends"),
            key: PredicateTy::Record(Arc::from([
                (sym("base"), PredicateTy::Fact(DECL)),
                (sym("type"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        Predicate {
            name: sym("src.Implements"),
            key: PredicateTy::Record(Arc::from([
                (sym("iface"), PredicateTy::Fact(DECL)),
                (sym("type"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        // Override *and* interface implementation, which are the same question asked of
        // a member — "who else is this, further down" — and are one predicate because a
        // caller looking at `IDisposable.Dispose` wants both answers in one scan.
        Predicate {
            name: sym("src.Override"),
            key: PredicateTy::Record(Arc::from([
                (sym("base"), PredicateTy::Fact(DECL)),
                (sym("member"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        // **The `Int` in the middle of the key is doing work.** Fields sort to
        // `decl, index, name`, so one seek on `decl` walks a method's parameters in
        // declaration order — the order they have to be printed in, and the order a
        // signature is. The type is on the value side because it is what a reader wants
        // shown and never what a query filters by: it is a *spelling*
        // (`ReadOnlySpan<byte>`), not an identity, and the identity is already in
        // `src.Ref` — the type name in a parameter list is an ordinary reference.
        Predicate {
            name: sym("src.Param"),
            key: PredicateTy::Record(Arc::from([
                (sym("decl"), PredicateTy::Fact(DECL)),
                (sym("index"), PredicateTy::Int),
                (sym("name"), PredicateTy::Str),
            ])),
            value: Some(PredicateTy::Str),
        },
        // A key of one field, which is the shape of *an attribute of something else*: a
        // declaration has at most one type and at most one doc comment, so the
        // declaration alone is the identity and the answer is the value. It encodes
        // exactly as the bare reference would — a one-field record is concatenation of
        // one — and reads as `src.TypeOf {decl = D}`, which is what a query wants to
        // write.
        Predicate {
            name: sym("src.TypeOf"),
            key: PredicateTy::Record(Arc::from([(sym("decl"), PredicateTy::Fact(DECL))])),
            value: Some(PredicateTy::Str),
        },
        Predicate {
            name: sym("src.Doc"),
            key: PredicateTy::Record(Arc::from([(sym("decl"), PredicateTy::Fact(DECL))])),
            value: Some(PredicateTy::Str),
        },
        // The attribute is a **name**, not a reference, and that is a decision rather
        // than a shortcut: `[Obsolete]` on everything in a repository is one prefix
        // seek here, where a reference would make it a join through a declaration that
        // — for the framework's own attributes — is not in the index at all.
        Predicate {
            name: sym("src.Attribute"),
            key: PredicateTy::Record(Arc::from([
                (sym("attribute"), PredicateTy::Str),
                (sym("target"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        // **A file's line table, one fact per line.** Multiplicity is undecided
        // ([open decisions](../../docs/open-decisions.md)) and there are no arrays, so a
        // sequence is said the only way this type model can say one: a fact per element,
        // with the position in the key. `file, line` sorts that way already, so one seek
        // reads a file's lines in order and a range reads the ten around a hit — which
        // is what a search result is rendered from.
        //
        // It is also the **widest row in the schema on purpose**: the value is a line of
        // source, so a scan of it is the one workload here that moves bytes rather than
        // rows.
        Predicate {
            name: sym("src.Line"),
            key: PredicateTy::Record(Arc::from([
                (sym("file"), PredicateTy::Fact(FILE)),
                (sym("line"), PredicateTy::Int),
            ])),
            value: Some(PredicateTy::Str),
        },
    ];

    Schema::new(rodeo.into_reader(), Arc::from(predicates))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file says a wrong id here "quietly returns nothing". It said that because
    /// nothing checked, and the list has since grown from six predicates to
    /// twenty-two — at which point the position of `src.Package` is not something to
    /// count by eye.
    #[test]
    fn the_constants_name_the_predicates_they_claim_to() {
        let schema = schema();

        for (id, expected) in [
            (FILE, "src.File"),
            (MODULE, "src.Module"),
            (DECL, "src.Decl"),
            (PROJECT, "src.Project"),
            (ASSEMBLY, "src.Assembly"),
            (PACKAGE, "src.Package"),
        ] {
            assert_eq!(
                schema.get(id).and_then(|p| p.name()),
                Some(expected),
                "predicate {} is not `{expected}`",
                id.0
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
