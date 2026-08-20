//! The token view, held to the two things a page depends on: that it covers the
//! source exactly, and that it says what a token *is*.
//!
//! An integration test rather than a unit one because the corpus it draws on
//! lives behind the engine's `proptest` feature, and a view crate that enabled
//! that feature for itself would ship the strategies to a browser.

use fjord_inspect::{TokenClass, tokens};
use proptest::prelude::*;

/// **The claim the old hand-written highlighter made and could not prove.**
///
/// A view that drops a byte mis-aligns every span after it, and the symptom is
/// a highlight in the wrong place — which reads as a styling bug and gets
/// chased in the stylesheet. Reassembling the source from the view is the
/// cheapest complete statement that nothing was lost.
fn assert_lossless(source: &str) {
    let view = tokens(source);

    let rebuilt: String = view.tokens.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(
        rebuilt, source,
        "the token texts do not reassemble the source"
    );

    let mut at = 0;
    for token in &view.tokens {
        assert_eq!(
            token.span.start, at,
            "a gap or an overlap before {:?} at {}",
            token.kind, at
        );
        assert_eq!(
            &source[token.span.start..token.span.end],
            token.text,
            "a token's text is not the bytes its span names"
        );
        at = token.span.end;
    }
    assert_eq!(
        at,
        source.len(),
        "the tokens stop {} bytes short of the source",
        source.len() - at
    );
}

#[test]
fn token_spans_reproduce_the_source_exactly() {
    for entry in fjord_engine::corpus::CORPUS {
        assert_lossless(entry.source);
    }
}

proptest! {
    /// Arbitrary text, not arbitrary sigla: a page's input is whatever somebody
    /// typed, and half-typed text is the state an interactive view spends most
    /// of its time in.
    #[test]
    fn any_text_is_covered_exactly(source in ".*") {
        assert_lossless(&source);
    }

    /// The same, over text drawn from sigla's own alphabet, so the generator
    /// spends its draws on token boundaries rather than on bytes the lexer
    /// refuses wholesale.
    #[test]
    fn sigla_shaped_text_is_covered_exactly(source in "[a-zA-Z0-9_ .,;{}()|!=<>+\"-]{0,64}") {
        assert_lossless(&source);
    }
}

/// Multi-byte input is where a byte-offset view either works or corrupts a
/// string: slicing a `&str` at a non-boundary panics, so this is also the guard
/// that the view never slices one.
#[test]
fn a_source_of_multi_byte_characters_is_covered_exactly() {
    for source in ["\"héllo\"", "X = \"日本語\"", "# ✓", "\u{1F600}"] {
        assert_lossless(source);
    }
}

/// **A byte the lexer cannot read is a token, not a hole.** The stream stays
/// aligned and the diagnostic points at the same span, which is what lets a
/// page underline exactly the offending bytes.
#[test]
fn an_unreadable_byte_is_a_token_and_a_diagnostic_at_the_same_span() {
    let view = tokens("where { src.File { name = X } } @");
    let error = view
        .tokens
        .iter()
        .find(|t| t.class == TokenClass::Error)
        .expect("`@` is not in sigla's alphabet");

    assert_eq!(error.text, "@");
    assert_eq!(view.diagnostics.len(), 1, "one bad byte, one diagnostic");
    let label = view.diagnostics[0]
        .labels
        .iter()
        .find(|label| label.primary)
        .expect("a lexer diagnostic points at the byte it could not read");
    assert_eq!(
        label.span, error.span,
        "the diagnostic points somewhere other than the token it is about"
    );
    assert_lossless("where { src.File { name = X } } @");
}

/// The classes a page styles, on a query that uses each of them. Written as one
/// worked example rather than a table of pairs, because what matters is that
/// the *distinctions* survive: a predicate is not a variable, and a field is
/// neither.
#[test]
fn a_query_is_classified_the_way_the_language_reads_it() {
    let view = tokens("where { src.File { name = Name } }; Name != \"x\"");
    let classes: Vec<_> = view
        .tokens
        .iter()
        .filter(|t| t.class != TokenClass::Whitespace)
        .map(|t| (t.text.as_str(), t.class))
        .collect();

    assert!(classes.contains(&("where", TokenClass::Keyword)));
    assert!(classes.contains(&("src.File", TokenClass::Predicate)));
    assert!(classes.contains(&("name", TokenClass::Field)));
    assert!(classes.contains(&("Name", TokenClass::Variable)));
    assert!(classes.contains(&("\"x\"", TokenClass::String)));
    assert!(classes.contains(&("!=", TokenClass::Punctuation)));
    assert!(
        !classes.iter().any(|(_, class)| *class == TokenClass::Error),
        "a query the corpus calls supported must not lex with an error token"
    );
}

/// **The census.** A view is worth what its generator reaches, and a corpus
/// walk that never produced a string, a number or a denial would leave the
/// classifier's interesting arms untested while every property stayed green.
#[test]
fn the_corpus_reaches_every_class_a_page_styles() {
    let mut seen = std::collections::BTreeSet::new();
    for entry in fjord_engine::corpus::CORPUS {
        for token in tokens(entry.source).tokens {
            seen.insert(token.class);
        }
    }

    for wanted in [
        TokenClass::Keyword,
        TokenClass::Predicate,
        TokenClass::Variable,
        TokenClass::Field,
        TokenClass::Number,
        TokenClass::String,
        TokenClass::Wildcard,
        TokenClass::Punctuation,
        TokenClass::Whitespace,
    ] {
        assert!(
            seen.contains(&wanted),
            "the corpus never produces {wanted:?}, so nothing tests how it is classified"
        );
    }
}

/// The JSON a page actually parses, pinned by example: field names are part of
/// the contract with the site, and renaming one silently breaks a view that has
/// no types to check it.
#[test]
fn the_json_is_the_shape_the_page_reads() {
    let json = serde_json::to_value(tokens("X")).expect("a view serialises");
    let token = &json["tokens"][0];

    assert_eq!(token["kind"], "UId");
    assert_eq!(token["class"], "variable");
    assert_eq!(token["span"]["start"], 0);
    assert_eq!(token["span"]["end"], 1);
    assert_eq!(token["text"], "X");
    assert_eq!(json["diagnostics"].as_array().map(Vec::len), Some(0));
}
