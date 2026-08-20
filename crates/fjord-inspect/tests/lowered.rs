//! The lowered view and the schema view, held to what a page reads off them.
//!
//! The load-bearing one is `every_symbol_in_a_view_is_resolved_to_text`: a
//! `Symbol` is an index into the interner that minted it, and a view that let
//! one escape unresolved would print `?` where a variable's name belongs — or,
//! worse, resolve it against a *different* interner and print somebody else's
//! name with complete confidence. That is the failure `fjord_wire::desc`
//! documents for the wire, and this is the same boundary.

use std::collections::BTreeSet;

use fjord_engine::corpus::{CORPUS, Expectation};
use fjord_inspect::{SAMPLES, SCHEMA, lowered, lowered::UNRESOLVED, rows, schema};

/// The fixture schema, as text — the corpus is written against it, and a view
/// can only be checked over queries somebody has already classified.
fn fixture_schema() -> String {
    fjord_schema::syntax::print::print(&fjord_store::fixture::schema())
}

#[test]
fn the_shipped_schema_is_a_schema() {
    let view = schema(SCHEMA);

    assert!(
        view.ok,
        "`schemas/demo.sigla` does not lower: {:?}",
        view.diagnostics
    );
    assert_eq!(
        view.predicates.len(),
        6,
        "the site's schema is six predicates, one per shape the language has"
    );
    for wanted in [
        "code.File",
        "code.Decl",
        "code.Ref",
        "code.Span",
        "code.Kind",
        "code.KindOf",
    ] {
        assert!(
            view.predicates.iter().any(|p| p.name == wanted),
            "the schema the site opens with does not declare `{wanted}`"
        );
    }
    for predicate in &view.predicates {
        assert!(
            !predicate.ty.is_empty(),
            "`{}` has no declared type",
            predicate.name
        );
        assert!(
            !predicate.ty.contains(UNRESOLVED),
            "`{}` renders as {:?}, which carries a name that did not resolve",
            predicate.name,
            predicate.ty
        );
    }
}

/// **The samples are claims about the language, so they are tested like any
/// other claim.** Every one but the two written to fail must compile clean
/// against the schema the site ships.
#[test]
fn every_sample_compiles_clean() {
    for sample in SAMPLES {
        let view = lowered(SCHEMA, sample.source);
        assert!(view.schema_ok, "the shipped schema did not lower");

        if sample.rows.is_none() {
            assert!(
                !view.diagnostics.is_empty(),
                "`{}` is meant to be refused and was not: {}",
                sample.label,
                sample.source
            );
        } else {
            assert!(
                view.diagnostics.is_empty(),
                "`{}` does not compile: {}\n  {:?}",
                sample.label,
                sample.source,
                view.diagnostics
            );
            assert!(
                view.head_ty.is_some(),
                "`{}` compiled without a head type",
                sample.label
            );
        }
    }
}

/// **Every name a page shows came from the interner that minted it.**
///
/// `?` is what the view writes when `try_resolve` answers `None`, which can only
/// happen if a symbol from another interner reached it. Nothing here should
/// produce one — except `Ty::Var`, which is inference not settling and is a
/// legitimate answer, so it is checked by kind rather than by scanning text.
#[test]
fn every_symbol_in_a_view_is_resolved_to_text() {
    let schema_source = fixture_schema();

    for entry in CORPUS {
        let view = lowered(&schema_source, entry.source);

        for node in &view.nodes {
            for shown in [node.label.as_deref(), node.ty.as_deref()]
                .into_iter()
                .flatten()
            {
                assert!(
                    !shown.contains(UNRESOLVED),
                    "`{}` shows `{}` as {shown:?} — a symbol reached the view from \
                     another interner",
                    entry.source,
                    node.kind
                );
            }
        }
    }
}

/// The arena a page walks: children resolve, spans nest, nothing is its own
/// ancestor.
#[test]
fn the_lowered_arena_is_walkable() {
    let schema_source = fixture_schema();

    for entry in CORPUS {
        let view = lowered(&schema_source, entry.source);
        let ids: BTreeSet<_> = view.nodes.iter().map(|node| node.id).collect();

        assert_eq!(ids.len(), view.nodes.len(), "a node id appears twice");

        for node in &view.nodes {
            assert!(
                node.span.start <= node.span.end && node.span.end <= entry.source.len(),
                "`{}` places {} at {:?}, outside the source",
                entry.source,
                node.kind,
                node.span
            );
            for child in &node.children {
                assert!(
                    ids.contains(child),
                    "`{}`: {} names a child {child} that is not in the view",
                    entry.source,
                    node.kind
                );
                assert_ne!(*child, node.id, "a node is its own child");
            }
        }

        if let Some(head) = view.head {
            assert!(ids.contains(&head), "the head is not in the view");
        }
    }
}

/// **A diagnostic's code reaches the page.** The corpus classifies entries by
/// code, and a view that dropped `code` would leave a reader with a sentence and
/// no way to look the refusal up.
#[test]
fn a_refusal_reaches_the_view_with_its_code() {
    let schema_source = fixture_schema();
    let mut seen = BTreeSet::new();

    for entry in CORPUS {
        if let Expectation::Diagnosed(code) = entry.expect {
            let view = lowered(&schema_source, entry.source);
            let codes: Vec<_> = view
                .diagnostics
                .iter()
                .filter_map(|diagnostic| diagnostic.code.clone())
                .collect();

            assert!(
                codes.iter().any(|reported| reported == code.as_str()),
                "`{}` is classified `{}` but the view reports {codes:?}",
                entry.source,
                code.as_str()
            );
            seen.insert(code.as_str());
        }
    }

    assert!(
        seen.len() > 10,
        "only {} distinct codes reached the view — the corpus should exercise far more",
        seen.len()
    );
}

/// **The census.** A view of types is worth what the corpus reaches: if nothing
/// produced a record, a union or a fact type, the renderer's interesting arms
/// would go untested while every property above stayed green.
#[test]
fn the_corpus_reaches_every_shape_the_type_panel_shows() {
    let schema_source = fixture_schema();
    let mut kinds = BTreeSet::new();
    let mut types = BTreeSet::new();

    for entry in CORPUS {
        let view = lowered(&schema_source, entry.source);
        for node in &view.nodes {
            kinds.insert(node.kind);
            if let Some(ty) = &node.ty {
                types.insert(ty.clone());
            }
        }
    }

    for wanted in [
        "Var",
        "Int",
        "Str",
        "Record",
        "Access",
        "Fact",
        "Wildcard",
        "Select",
        "Arith",
        "Disjunction",
        "Subquery",
        "Prefix",
        "Value",
    ] {
        assert!(
            kinds.contains(wanted),
            "the corpus never lowers a `{wanted}`, so nothing tests how it is shown"
        );
    }

    assert!(
        types.contains("int") && types.contains("string"),
        "scalars missing"
    );
    assert!(
        types.iter().any(|ty| ty.starts_with("test.")),
        "no fact type reached the view"
    );
    assert!(
        types
            .iter()
            .any(|ty| ty.contains(" : ") && ty.contains('|')),
        "no union type reached the view, so the discriminants are unchecked"
    );
}

/// The JSON a page parses, pinned by example.
#[test]
fn the_json_is_the_shape_the_page_reads() {
    let json = serde_json::to_value(lowered(SCHEMA, "P where code.File P")).expect("serialises");

    assert_eq!(json["schema_ok"], true);
    assert_eq!(json["head_ty"], "string");
    assert_eq!(json["statements"][0]["kind"], "Implicit");
    assert_eq!(json["diagnostics"].as_array().map(Vec::len), Some(0));

    let head = json["head"].as_u64().expect("a head");
    let node = json["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["id"] == head)
        .expect("the head is in the arena");
    assert_eq!(node["kind"], "Var");
    assert_eq!(node["label"], "P");
    assert_eq!(node["ty"], "string");
}

/// **The samples answer, and answer what they say.**
///
/// A demo query that returns nothing demonstrates nothing, and one that returns
/// a single row shows no backtracking — which is most of what there is to watch
/// in a debugger. So the count is part of the sample, and this is what holds it
/// there: change the database and a sample that stops answering says so, rather
/// than quietly becoming a blank panel.
#[test]
fn every_sample_answers_what_it_says() {
    for sample in SAMPLES {
        let answered = rows(SCHEMA, sample.source);

        assert!(
            answered.diagnostics.is_empty() || sample.rows.is_none(),
            "`{}` does not compile: {:?}",
            sample.label,
            answered.diagnostics
        );

        match sample.rows {
            Some(expected) => {
                assert_eq!(
                    answered.rows.len(),
                    expected,
                    "`{}` says it answers {expected} rows and answers {}: {}\n  {:?}",
                    sample.label,
                    answered.rows.len(),
                    sample.source,
                    answered.rows
                );
                assert!(
                    !answered.truncated,
                    "`{}` hit the row cap, which no sample should",
                    sample.label
                );
                assert!(
                    answered.examined_total >= answered.rows.len() as u64,
                    "`{}` answered more rows than it examined",
                    sample.label
                );
            }
            None => assert!(
                answered.rows.is_empty(),
                "`{}` is meant to be refused and answered rows",
                sample.label
            ),
        }
    }
}

/// **Every sample answers two or three rows**, which is the constraint the
/// database was sized for. Stated separately from the counts above so that a
/// sample added with `rows: Some(1)` fails here rather than passing quietly.
#[test]
fn a_sample_answers_more_than_one_row() {
    for sample in SAMPLES {
        if let Some(rows) = sample.rows {
            assert!(
                (2..=4).contains(&rows),
                "`{}` answers {rows} rows; a demo query wants two or three — one \
                 shows no backtracking and a dozen shows no detail",
                sample.label
            );
        }
    }
}
