//! The parse entry point: focus text → a lossless CST + diagnostics.
//!
//! `lex → parse` is the permissive-early half of the front end
//! ([chapter 7]): it accepts the full intended feature surface and leaves
//! *meaning* — including "not yet implemented" — to typecheck and flatten.
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use codespan_reporting::diagnostic::{Label, Severity};

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

/// The longest source [`parse`] accepts.
///
/// Spans in the typed store are `u32` to keep nodes compact
/// ([`syntax::NodeSpan`](crate::focus::syntax::NodeSpan)), and lowering narrows the
/// parser's `usize` spans to fit. Refusing an unaddressable source here is what
/// makes that narrowing lossless — otherwise every span past the 4 GiB mark would
/// silently wrap and point at the wrong bytes, which is the one thing a span may
/// not do.
const MAX_SOURCE_LEN: usize = u32::MAX as usize;

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

    /// Whether anything error-or-worse was reported.
    ///
    /// Filtered by severity rather than asking whether the list is empty: the
    /// sink is shared, and the first warning or note added to it must not start
    /// reading as a failed parse.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity >= Severity::Error)
    }

    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }
}

/// Parse `source` into a lossless CST, collecting diagnostics as it goes.
pub fn parse(source: &str) -> Parsed<'_> {
    let mut diagnostics = vec![];

    // Refused without a label: there is no span to point at that the renderer
    // could address, which is the whole reason for the limit.
    if source.len() > MAX_SOURCE_LEN {
        diagnostics.push(Diagnostic::error().with_message(format!(
            "source is {} bytes; the limit is {MAX_SOURCE_LEN}",
            source.len()
        )));
        return Parsed {
            cst: None,
            diagnostics,
        };
    }

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
/// A bound on the parser's recursion depth, read straight off the token stream:
/// each unclosed `{` or `(` opens a nested `pattern`, and so does each `QId`,
/// since `fact_pattern: QId branch` recurses on its key.
///
/// The subtlety is that applications nest only along a *path*. `test.A test.B _`
/// is two levels deep, but `{a = test.A _, b = test.B _}` is a record of two
/// siblings — one level, whichever way it is counted. So application counts are
/// kept **per bracket level** and reset at the tokens that end an argument (`,`
/// `|` `;` `=` `where`), and the depth at any point is the open-bracket count plus
/// the applications still on the path. Counting every `QId` in a statement
/// instead — which is what this did — made a *wide* record read as a deep one, so
/// a machine-generated query with more than [`MAX_NEST_DEPTH`] fact-valued fields
/// was refused for nesting two levels.
///
/// Still only a bound, and deliberately: the parser spends a few frames per level
/// (`pattern` → `branch` → `primary`), so the real depth is a small multiple of
/// this. That is fine — the cap is a policy limit borrowed from the codec, far
/// below the depth that would actually exhaust the stack.
fn nesting_overflow(tokens: &[Token]) -> Option<usize> {
    // One entry per open bracket level, holding that level's application count;
    // `total` is their sum, maintained incrementally so this stays O(1) a token.
    let mut levels: Vec<usize> = vec![0];
    let mut total = 0usize;

    for (idx, token) in tokens.iter().enumerate() {
        match token {
            Token::LBrace | Token::LPar => levels.push(0),

            Token::RBrace | Token::RPar => {
                // Guarded: unbalanced brackets are a parse error, reported by the
                // parser, and must not underflow the level stack on the way there.
                if levels.len() > 1 {
                    total -= levels.pop().unwrap_or_default();
                }
            }

            Token::QId => {
                if let Some(level) = levels.last_mut() {
                    *level += 1;
                    total += 1;
                }
            }

            // An argument ends here, so whatever was applied at this level is no
            // longer on the path.
            Token::Comma | Token::Pipe | Token::Semi | Token::Eq | Token::Where => {
                if let Some(level) = levels.last_mut() {
                    total -= *level;
                    *level = 0;
                }
            }

            _ => {}
        }

        if levels.len() - 1 + total > MAX_NEST_DEPTH {
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

    /// How many nodes in the tree carry this rule name.
    fn count(node: &CstNode<'_>, name: &str) -> usize {
        rules(node).iter().filter(|n| *n == name).count()
    }

    /// Every rule name in the tree, outermost first.
    fn rules(node: &CstNode<'_>) -> Vec<String> {
        node.cata(&mut |kind| match kind {
            CstKind::Token { .. } => vec![],
            CstKind::Rule { rule, children, .. } => {
                let mut out = vec![format!("{rule:?}")];
                out.extend(children.into_iter().flatten());
                out
            }
        })
    }

    /// The span of the outermost node with this rule name.
    ///
    /// Rules are matched by their grammar name (what `Rule`'s `Debug` prints) so
    /// the tests read like the grammar and don't import generated identifiers.
    fn rule_span(node: &CstNode<'_>, name: &str) -> Option<super::super::parser::Span> {
        node.cata(&mut |kind| match kind {
            CstKind::Token { .. } => None,
            CstKind::Rule {
                rule,
                span,
                children,
            } => {
                if format!("{rule:?}") == name {
                    Some(span)
                } else {
                    children.into_iter().flatten().next()
                }
            }
        })
    }

    fn parse_clean(source: &str) -> Parsed<'_> {
        let parsed = parse(source);
        assert!(
            !parsed.has_errors(),
            "{source:?} should parse, got {:?}",
            parsed
                .diagnostics()
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
        parsed
    }

    /// Sources exercising the surface the grammar accepts today.
    const SOURCES: &[&str] = &[
        "X where X = test.Foo _",
        "X where test.Foo {name = X}",
        "{a = X, b = Y} where test.Foo {name = X}; test.Bar {id = Y}",
        "X.name where X = test.Foo _",
        "X where X = test.Foo \"abc\"..",
        "X where X = test.Foo -42",
        "X where X = never",
        "X where test.Foo (X.name)",
        "X where X = (Y where test.Foo {id = Y})",
        "X where test.Foo {id = X} | test.Bar {id = X}",
        "X where test.Foo {id = X}; !test.Bar {id = X}",
        "X.alt? where X = test.Foo _",
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

    /// The precedence the grammar exists to fix: `.` binds tighter than
    /// application, so `test.Foo X.name` is `test.Foo (X.name)` — the access is the
    /// *argument*, not applied to the result.
    ///
    /// Stated structurally rather than by comparing against the parenthesised form,
    /// which would beg the question: the access node must sit strictly inside the
    /// fact pattern's span. Were it the other way round the fact pattern would be
    /// inside the access.
    #[test]
    fn dot_binds_tighter_than_application() {
        let parsed = parse_clean("X where test.Foo X.name");
        let root = parsed.root().expect("a tree");

        let fact = rule_span(&root, "fact_pattern").expect("a fact pattern");
        let access = rule_span(&root, "access_pattern").expect("an access");

        assert!(
            fact.start <= access.start && access.end <= fact.end && fact != access,
            "the access {access:?} must sit inside the application {fact:?}"
        );
    }

    /// A parenthesised group is one pattern, and transparent — the inner pattern is
    /// the paren node's only meaningful child, which is what lets lowering drop it.
    #[test]
    fn a_paren_group_wraps_one_pattern() {
        let parsed = parse_clean("X where test.Foo (X.name)");
        let root = parsed.root().expect("a tree");

        assert!(rules(&root).contains(&"paren_primary".to_string()));

        let paren = rule_span(&root, "paren_primary").expect("a paren");
        let access = rule_span(&root, "access_pattern").expect("an access");
        assert!(paren.start < access.start && access.end < paren.end);
    }

    /// A subquery is the same shape as a query — a head pattern and a statement
    /// list — so it reuses the query rule rather than a parallel one. The optional
    /// `where` is also what keeps the group and the subquery a single LL(1) rule
    /// with no backtracking.
    #[test]
    fn a_subquery_is_a_head_and_a_body() {
        let parsed = parse_clean("X where X = (Y where test.Foo {id = Y})");
        let root = parsed.root().expect("a tree");
        let names = rules(&root);

        assert!(names.contains(&"subquery_primary".to_string()));
        // Two statement lists: the outer query's and the subquery's.
        assert_eq!(
            names.iter().filter(|n| *n == "stmt_list").count(),
            2,
            "got {names:?}"
        );
    }

    #[test]
    fn never_is_a_pattern() {
        let parsed = parse_clean("X where X = never");
        let root = parsed.root().expect("a tree");
        assert!(rules(&root).contains(&"never_primary".to_string()));
    }

    /// Disjunction is **flat**: N branches under one node, whatever N is. A
    /// right-leaning tree would give N-1 nodes, and flatten wants the branch list
    /// whole so it can keep it as a single `FlatDisjunction` rather than
    /// DNF-expanding it.
    #[test]
    fn disjunction_is_flat_however_many_branches() {
        for source in [
            "X where X = A | B",
            "X where X = A | B | C",
            "X where X = A | B | C | D | E",
        ] {
            let parsed = parse_clean(source);
            let root = parsed.root().expect("a tree");
            assert_eq!(
                count(&root, "disjunction"),
                1,
                "{source:?} must be one flat disjunction"
            );
        }
    }

    /// `|` is looser than application, so this is a disjunction of two fact
    /// patterns — not one fact pattern whose key is a disjunction.
    #[test]
    fn disjunction_is_looser_than_application() {
        let parsed = parse_clean("X where test.Foo {id = X} | test.Bar {id = X}");
        let root = parsed.root().expect("a tree");

        assert_eq!(count(&root, "fact"), 2);

        let disjunction = rule_span(&root, "disjunction").expect("a disjunction");
        let fact = rule_span(&root, "fact").expect("a fact pattern");
        assert!(
            disjunction.start <= fact.start && fact.end < disjunction.end,
            "the applications {fact:?} must sit inside the disjunction {disjunction:?}"
        );
    }

    /// The other side of that decision: a disjunction *inside* a key is written
    /// with parens, exactly as dot-tighter-than-application already requires.
    #[test]
    fn a_disjunction_inside_a_key_needs_parens() {
        let parsed = parse_clean("X where test.Foo (A | B)");
        let root = parsed.root().expect("a tree");

        let fact = rule_span(&root, "fact").expect("a fact pattern");
        let disjunction = rule_span(&root, "disjunction").expect("a disjunction");
        assert!(
            fact.start < disjunction.start && disjunction.end <= fact.end,
            "the disjunction {disjunction:?} must sit inside the key {fact:?}"
        );
    }

    /// Union select is a postfix on an access step — always `.name?`, since it
    /// selects an alternative by name — and chains.
    #[test]
    fn union_select_is_a_postfix_on_an_access() {
        let parsed = parse_clean("X.alt? where X = test.Foo _");
        let root = parsed.root().expect("a tree");
        assert_eq!(count(&root, "access_pattern"), 1);

        let parsed = parse_clean("X.a?.b? where X = test.Foo _");
        let root = parsed.root().expect("a tree");
        assert_eq!(
            count(&root, "access_pattern"),
            1,
            "an access chain is one node, like a disjunction"
        );
        assert!(token_text(&root).contains("X.a?.b?"));
    }

    /// Negation prefixes a statement — which is the level at which chapter 7 talks
    /// about it ("negations move after their non-locals are bound"). `!(…)` groups.
    #[test]
    fn negation_is_a_statement_prefix() {
        let parsed = parse_clean("X where test.Foo {id = X}; !test.Bar {id = X}");
        let root = parsed.root().expect("a tree");
        assert_eq!(count(&root, "negation_stmt"), 1);
        assert_eq!(count(&root, "implicit_bind_stmt"), 1);

        let parsed = parse_clean("X where test.Foo {id = X}; !(Y where test.Bar {id = Y})");
        let root = parsed.root().expect("a tree");
        assert_eq!(count(&root, "negation_stmt"), 1);
        assert_eq!(count(&root, "subquery_primary"), 1);
    }

    /// A *wide* record is not a deep one. Its fields are siblings, so however many
    /// of them apply a fact pattern the nesting is two levels — and a
    /// machine-generated query with hundreds of them must be accepted.
    ///
    /// This was the false rejection: every `QId` in a statement counted toward one
    /// running total, so field count read as depth.
    #[test]
    fn a_wide_record_is_not_deep() {
        let fields = (0..MAX_NEST_DEPTH * 2)
            .map(|i| format!("f{i} = test.Foo _"))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("X where X = {{{fields}}}");

        let parsed = parse(&source);
        assert!(
            parsed.root().is_some(),
            "a record of {} sibling fact patterns was refused as deeply nested",
            MAX_NEST_DEPTH * 2
        );
    }

    /// The other direction, and the reason applications are counted at all: a
    /// *chain* of them does nest, because each fact pattern's key is the next.
    #[test]
    fn a_deep_application_chain_is_still_refused() {
        let source = format!("X where X = {}_", "test.Foo ".repeat(MAX_NEST_DEPTH + 1));

        let parsed = parse(&source);
        assert!(
            parsed.root().is_none(),
            "a chain of {} applications must be refused",
            MAX_NEST_DEPTH + 1
        );
    }

    /// Disjunction branches are siblings too, at whatever bracket level they sit.
    #[test]
    fn wide_disjunction_and_many_fields_are_not_deep() {
        let branches = (0..MAX_NEST_DEPTH * 2)
            .map(|_| "test.Foo _")
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(parse(&format!("X where {branches}")).root().is_some());

        // Nested one level in, so the reset has to be per bracket level rather
        // than global.
        assert!(
            parse(&format!("X where X = {{a = {branches}}}"))
                .root()
                .is_some()
        );
    }

    /// Parens nest patterns, so they count toward the cap like braces do.
    #[test]
    fn paren_depth_counts_toward_the_nesting_cap() {
        let depth = MAX_NEST_DEPTH + 1;
        let source = format!("X where X = {}_{}", "(".repeat(depth), ")".repeat(depth));
        let parsed = parse(&source);
        assert!(parsed.root().is_none(), "the tree must be refused");
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
