//! The parse tree view, held to the two things a page depends on: that a node's
//! span really is where its text is, and that the arena is walkable.
//!
//! An integration test because it draws on the corpus, which lives behind the
//! engine's `proptest` feature — a view crate that enabled that feature for
//! itself would ship the strategies to a browser.

use std::collections::BTreeSet;

use fjord_engine::corpus::{CORPUS, Expectation};
use fjord_inspect::{Tree, tree};
use proptest::prelude::*;

/// Every structural claim a page makes when it renders the tree, in one place.
///
/// **Containment is the load-bearing one**: a view that widened a span by a byte
/// would still look plausible in a tree and would highlight the wrong text.
/// Losslessness comes with it — the grammar's `skip Whitespace` keeps trivia out
/// of what the parser *matches on*, not out of the tree, so the leaves are the
/// source and a page can render either from the same view.
fn assert_walkable(source: &str, view: &Tree) {
    let mut reached = BTreeSet::new();

    let leaves: String = view
        .nodes
        .iter()
        .filter(|node| node.token)
        .map(|node| node.label.clone().unwrap_or_default())
        .collect();
    if view.root.is_some() {
        assert_eq!(
            leaves, source,
            "the tree's leaves do not reassemble the source"
        );
    }

    for node in &view.nodes {
        assert!(
            node.span.start <= node.span.end && node.span.end <= source.len(),
            "{} at {:?} names bytes outside a {}-byte source",
            node.kind,
            node.span,
            source.len()
        );
        assert!(
            source.is_char_boundary(node.span.start) && source.is_char_boundary(node.span.end),
            "{} at {:?} splits a character — slicing it would panic in Rust and \
             mis-highlight in a browser",
            node.kind,
            node.span
        );

        for &child in &node.children {
            assert!(
                child < view.nodes.len(),
                "child {child} is not in the arena"
            );
            assert!(
                reached.insert(child),
                "node {child} has two parents, so the arena is not a tree"
            );

            let child = &view.nodes[child];
            assert!(
                node.span.start <= child.span.start && child.span.end <= node.span.end,
                "{} at {:?} does not contain its child {} at {:?}",
                node.kind,
                node.span,
                child.kind,
                child.span
            );
        }

        if node.token {
            assert_eq!(
                node.label.as_deref(),
                Some(&source[node.span.start..node.span.end]),
                "a token leaf's text is not the bytes its span names"
            );
            assert!(node.children.is_empty(), "a token leaf has children");
        } else {
            assert!(node.label.is_none(), "a rule carries a token's text");
        }
    }

    match view.root {
        Some(root) => {
            assert!(!reached.contains(&root), "the root is somebody's child");
            assert_eq!(
                reached.len() + 1,
                view.nodes.len(),
                "the arena holds nodes the walk from the root never reaches"
            );
        }
        None => assert!(
            view.nodes.is_empty(),
            "a refused parse produced nodes anyway"
        ),
    }
}

#[test]
fn tree_spans_nest() {
    for entry in CORPUS {
        assert_walkable(entry.source, &tree(entry.source));
    }
}

proptest! {
    /// Arbitrary text, because a page's input is whatever somebody typed, and a
    /// half-typed query is the state an interactive view is in most of the time.
    #[test]
    fn any_text_produces_a_walkable_tree(source in ".*") {
        assert_walkable(&source, &tree(&source));
    }

    /// The same over sigla's own alphabet, so the generator spends its draws on
    /// grammar shapes rather than on bytes the lexer refuses wholesale.
    #[test]
    fn sigla_shaped_text_produces_a_walkable_tree(
        source in "[a-zA-Z0-9_ .,;{}()|!=<>+\"-]{0,64}"
    ) {
        assert_walkable(&source, &tree(&source));
    }
}

/// **A parse that recovered has both a tree and diagnostics**, and the tree
/// carries `Error` where recovery happened. That pair is what an interactive
/// view renders on almost every keystroke, so it is asserted rather than
/// assumed.
#[test]
fn a_recovered_parse_keeps_the_tree_it_managed() {
    // The corpus's own junk case, classified `ParseError`.
    let source = "X where X = }";
    let view = tree(source);

    assert!(view.root.is_some(), "recovery produced no tree at all");
    assert!(
        !view.diagnostics.is_empty(),
        "a query with junk on the right of a bind parsed silently"
    );
    assert!(
        view.nodes.iter().any(|node| node.kind == "Error"),
        "recovery left no `Error` node, so nothing in the tree says where it happened"
    );
    assert_walkable(source, &view);
}

/// The corpus's `ParseError` entries are the ones the grammar refuses. Every one
/// must report something a reader can act on — a refusal with no diagnostic is a
/// query that vanishes.
#[test]
fn every_refused_entry_says_why() {
    for entry in CORPUS {
        if matches!(entry.expect, Expectation::ParseError) {
            let view = tree(entry.source);
            assert!(
                !view.diagnostics.is_empty(),
                "`{}` is classified ParseError but the view reports nothing",
                entry.source
            );
        }
    }
}

/// **The census.** A tree view is worth what its corpus reaches: if no entry
/// produced a nested statement or a token leaf, the containment property above
/// would hold vacuously over a tree of one node.
#[test]
fn the_corpus_reaches_the_shapes_a_tree_view_is_for() {
    let mut kinds = BTreeSet::new();
    let mut deepest = 0;

    for entry in CORPUS {
        let view = tree(entry.source);
        for node in &view.nodes {
            kinds.insert(node.kind);
        }
        deepest = deepest.max(depth(&view));
    }

    for wanted in [
        "Root",
        "Query",
        "StmtList",
        "Stmt",
        "FactPattern",
        "FieldList",
        "Field",
        "QId",
        "UId",
        "Error",
    ] {
        assert!(
            kinds.contains(wanted),
            "the corpus never produces a `{wanted}` node, so nothing tests how it is shown"
        );
    }
    assert!(
        deepest >= 6,
        "the deepest corpus tree is {deepest} nodes — too shallow to test nesting"
    );
}

fn depth(view: &Tree) -> usize {
    fn walk(view: &Tree, id: usize) -> usize {
        1 + view.nodes[id]
            .children
            .iter()
            .map(|&child| walk(view, child))
            .max()
            .unwrap_or(0)
    }
    view.root.map_or(0, |root| walk(view, root))
}

/// The JSON a page parses, pinned by example: these field names are the contract
/// with the site, and renaming one silently breaks a view that has no types to
/// check it against.
#[test]
fn the_json_is_the_shape_the_page_reads() {
    let json = serde_json::to_value(tree("X where test.Count X")).expect("a view serialises");

    assert_eq!(json["root"], 0);
    let root = &json["nodes"][0];
    assert_eq!(root["kind"], "Root");
    assert_eq!(root["token"], false);
    assert_eq!(root["label"], serde_json::Value::Null);
    assert_eq!(root["span"]["start"], 0);
    assert!(
        root["children"]
            .as_array()
            .is_some_and(|kids| !kids.is_empty()),
        "the root has no children"
    );
    assert_eq!(json["diagnostics"].as_array().map(Vec::len), Some(0));
}
