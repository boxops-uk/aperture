//! The parse entry point: focus text → a lossless CST + diagnostics.
//!
//! `lex → parse` is the permissive-early half of the front end
//! ([chapter 7]): it accepts the full intended feature surface and leaves
//! *meaning* — including "not yet implemented" — to typecheck and flatten.
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use codespan_reporting::diagnostic::Label;

use crate::focus::{
    cst::CstNode,
    lexer::{Token, tokenize},
    parser::{Cst, Diagnostic, Parser},
};

/// The deepest pattern nesting [`parse`] will hand to the parser.
///
/// The generated parser is recursive descent, and `pattern` is mutually
/// recursive with itself through both records (`pattern` → `primary` →
/// `anon_record_primary` → `field_list` → `field` → `pattern`) and fact
/// application (`fact_pattern: QId pattern`). A deeply nested query would
/// therefore overflow the stack, and on a data path that must be an error rather
/// than a crash ([conventions]). The generated parser can't be made iterative
/// here, so the depth is bounded *before* parsing, from the token stream.
///
/// Deliberately the same limit as the codec's `MAX_RECORD_DEPTH`: a pattern
/// nested deeper than the codec can encode has nothing to match against.
///
/// [conventions]: ../../../docs/conventions.md
const MAX_NEST_DEPTH: usize = 256;

/// The result of parsing: the tree, and every diagnostic the lexer and parser
/// produced.
///
/// Diagnostics accumulate rather than fail fast — permissive-grammar-narrow-later
/// needs multi-error reporting. Phase 3 replaces this plain `Vec` with the
/// compilation context's pooled sink; nothing here should assume one query.
pub struct Parsed<'src> {
    cst: Option<Cst<'src>>,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Parsed<'src> {
    /// The root of the tree, or `None` if parsing was refused outright (see
    /// [`MAX_NEST_DEPTH`]).
    pub fn root(&self) -> Option<CstNode<'_>> {
        self.cst.as_ref().map(CstNode::new)
    }

    /// The raw tree — `Display`s as an indented rule/token listing, which is what
    /// the grammar's structure tests assert against.
    pub fn cst(&self) -> Option<&Cst<'src>> {
        self.cst.as_ref()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn has_errors(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }
}

/// Parse `source` into a lossless CST, collecting diagnostics as it goes.
pub fn parse(source: &str) -> Parsed<'_> {
    let mut diagnostics = vec![];

    // Lexed here only to bound the nesting; `Parser::new` lexes again. Two
    // passes over query text isn't worth a shared-token API.
    let (tokens, spans) = tokenize(source, &mut diagnostics);

    if let Some(idx) = nesting_overflow(&tokens) {
        let span = spans.get(idx).cloned().unwrap_or(0..source.len());
        diagnostics.push(
            Diagnostic::error()
                .with_message(format!("pattern nested deeper than {MAX_NEST_DEPTH}"))
                .with_label(Label::primary((), span)),
        );
        return Parsed {
            cst: None,
            diagnostics,
        };
    }

    let cst = Parser::new(source, &mut diagnostics).parse(&mut diagnostics);
    Parsed {
        cst: Some(cst),
        diagnostics,
    }
}

/// The index of the first token at which nesting exceeds [`MAX_NEST_DEPTH`].
///
/// A conservative upper bound on the parser's recursion depth: each unclosed `{`
/// opens a nested `pattern`, and so does each `QId`, since a fact pattern
/// recurses on its key. Application depth is counted per statement — a `;` at
/// bracket depth 0 ends one — so a query with very many flat statements is not
/// mistaken for a deep one.
fn nesting_overflow(tokens: &[Token]) -> Option<usize> {
    let mut brackets = 0usize;
    let mut applications = 0usize;

    for (idx, token) in tokens.iter().enumerate() {
        match token {
            Token::LBrace => brackets += 1,
            Token::RBrace => brackets = brackets.saturating_sub(1),
            Token::QId => applications += 1,
            Token::Semi if brackets == 0 => applications = 0,
            _ => {}
        }
        if brackets + applications > MAX_NEST_DEPTH {
            return Some(idx);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::cst::CstKind;
    use proptest::prelude::*;

    /// Every token leaf's text, in order.
    fn token_text(node: &CstNode<'_>) -> String {
        node.cata(&mut |kind| match kind {
            CstKind::Token { text, .. } => text.to_owned(),
            CstKind::Rule { children, .. } => children.concat(),
        })
    }

    /// Sources exercising the surface the grammar accepts today.
    const SOURCES: &[&str] = &[
        "X where X = test.Foo _",
        "X where test.Foo {name = X}",
        "{a = X, b = Y} where test.Foo {name = X}; test.Bar {id = Y}",
        "X.name where X = test.Foo _",
        "X where X = test.Foo \"abc\"..",
        "X where X = test.Foo -42",
        // Trivia is part of the tree, so odd spacing must round-trip too.
        "  X\n  where\tX = test.Foo _  ",
    ];

    #[test]
    fn minimal_query_parses_without_diagnostics() {
        let parsed = parse("X where X = test.Foo _");
        assert!(
            !parsed.has_errors(),
            "unexpected diagnostics: {:?}",
            parsed
                .diagnostics()
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        assert!(parsed.root().is_some());
    }

    /// The façade is lossless: no byte of the source is dropped, including the
    /// whitespace the parser skips. This is what licenses perfect spans and
    /// round-tripping back to text.
    #[test]
    fn token_text_reproduces_the_source() {
        for source in SOURCES {
            let parsed = parse(source);
            let root = parsed.root().expect("a tree");
            assert_eq!(&token_text(&root), source, "lost bytes for {source:?}");
        }
    }

    /// Nesting past the cap is refused with a diagnostic. The cap is a policy
    /// limit (the codec's, see [`MAX_NEST_DEPTH`]) reached long before the stack
    /// depth that would actually overflow, so this pins the guard's behaviour;
    /// what it buys is that the overflow depth is unreachable at all.
    #[test]
    fn deep_nesting_is_a_diagnostic_not_a_crash() {
        let depth = MAX_NEST_DEPTH + 1;
        let source = format!(
            "X where X = {}_{}",
            "{a = ".repeat(depth),
            "}".repeat(depth)
        );

        let parsed = parse(&source);
        assert!(parsed.root().is_none(), "the tree must be refused");
        assert!(
            parsed
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("nested deeper than")),
            "expected a nesting diagnostic, got {:?}",
            parsed
                .diagnostics()
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    /// Many statements are not deep nesting — the per-statement reset means a
    /// machine-generated query with hundreds of conjuncts is still accepted.
    #[test]
    fn many_flat_statements_are_not_deep() {
        let body = (0..MAX_NEST_DEPTH * 4)
            .map(|_| "test.Foo _".to_string())
            .collect::<Vec<_>>()
            .join("; ");
        let source = format!("X where {body}");
        let parsed = parse(&source);
        assert!(parsed.root().is_some());
    }

    /// A token soup, plus raw junk to reach the lexer's error path.
    fn arb_source() -> impl Strategy<Value = String> {
        let fragment = prop_oneof![
            Just("where"),
            Just("X"),
            Just("_"),
            Just("test.Foo"),
            Just("{"),
            Just("}"),
            Just("="),
            Just(";"),
            Just(","),
            Just("."),
            Just(".."),
            Just("-"),
            Just("42"),
            Just("\"s\""),
            Just("name"),
            Just("@"),
        ];
        prop_oneof![
            8 => proptest::collection::vec(fragment, 0..24)
                .prop_map(|parts| parts.join(" ")),
            1 => any::<String>(),
        ]
    }

    proptest! {
        /// Parsing arbitrary text terminates with diagnostics, never a panic, and
        /// every diagnostic points inside the source — a label out of bounds
        /// panics the renderer downstream.
        #[test]
        fn parse_never_panics_and_spans_stay_in_bounds(source in arb_source()) {
            let parsed = parse(&source);
            for diag in parsed.diagnostics() {
                for label in &diag.labels {
                    prop_assert!(
                        label.range.start <= label.range.end,
                        "inverted label range {:?}", label.range
                    );
                    prop_assert!(
                        label.range.end <= source.len(),
                        "label range {:?} past the end of a {}-byte source",
                        label.range, source.len()
                    );
                }
            }
        }
    }
}
