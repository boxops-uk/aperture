//! [`Ast`] → text, in two renderings that must not be confused.
//!
//! - [`print`] emits **focus source**: text that parses and lowers back to the tree
//!   it came from. That makes lowering invertible, which is what lets the front end
//!   be property-tested — generate a tree, print it, parse it, compare — rather than
//!   only checked against hand-written snippets.
//! - [`canonical`] emits an **s-expression**, which is deliberately *not* focus
//!   syntax. It is the structural identity of a tree: two trees built by different
//!   routes have different `NodeId`s and different spans but the same canonical
//!   form, so this is what a round-trip compares. Keeping it a separate rendering is
//!   what stops the round-trip property being circular.
//!
//! Printing is **not** the inverse of parsing in the other direction: whitespace,
//! redundant parens and the choice of string escapes are all lost. Only
//! `parse ∘ print == id` on trees is claimed, and only that is tested.
//!
//! The hard part is parentheses. The grammar has three precedence levels, and a
//! child looser than its position allows has to be wrapped — see [`Level`].

use std::fmt::Write as _;

use crate::focus::{
    schema::{LocalInterner, Schema, Symbol},
    syntax::{Ast, ExprKind, FieldRef, Literal, NodeId, NodeSpan, Query, QueryStmt, narrow_offset},
};

/// How loosely a pattern binds, from the grammar:
///
/// ```text
/// pattern := branch ('|' branch)*                        -- Disjunction
/// branch  := fact_pattern | primary ('.' LId ['?'])*     -- Application | Chain
/// primary := '_' | UId | Nat | … | '(' pattern … ')'     -- Primary
/// ```
///
/// A child is parenthesised exactly when its level is *greater* than the level its
/// position permits. `Application` and `Chain` are siblings in the grammar but must
/// be ordered here, because an access chain's base may be a chain (`X.a.b`) while an
/// application in that position needs wrapping (`(test.Foo X).name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Primary,
    Chain,
    Application,
    Disjunction,
}

/// Render `ast` as focus source.
pub fn print(ast: &Ast, schema: &Schema, interner: &LocalInterner) -> String {
    spanned(ast, schema, interner).text
}

/// Render `ast` as focus source, keeping the range each node's text occupies.
///
/// Printing is where a span can be *predicted*: the printer knows what it emitted
/// and where, so lowering the result must hand back exactly these ranges. That is
/// what makes spans property-testable at all — a generated tree has no source to
/// compare against, and re-deriving one by slicing and re-parsing would only ever
/// check that a span looks plausible.
pub fn spanned(ast: &Ast, schema: &Schema, interner: &LocalInterner) -> Spanned {
    let mut out = Spanned {
        text: String::new(),
        spans: vec![0..0; ast.store().len()],
    };
    Printer {
        ast,
        schema: Some(schema),
        interner,
    }
    .query(&mut out, ast.query());
    out
}

/// Focus source under construction, with the span each node was printed at.
pub struct Spanned {
    text: String,
    /// By `NodeId`, which indexes the store densely.
    spans: Vec<NodeSpan>,
}

impl Spanned {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Where `id`'s own text landed.
    ///
    /// **Parentheses the printer wrapped around `id` are excluded**, because that is
    /// lowering's convention: a `paren_primary` is a pass-through to its child
    /// (`lower.rs`), so the child keeps the span it was pushed with. A subquery's
    /// parens *are* included, since there the parens belong to the node's own rule.
    /// The two conventions must agree, or `spans_are_where_the_text_was_printed`
    /// would be pinning the printer's rather than lowering's.
    pub fn span(&self, id: NodeId) -> NodeSpan {
        self.spans[id.index()].clone()
    }

    fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Record `id` as covering exactly what `f` emits.
    fn node(&mut self, id: NodeId, f: impl FnOnce(&mut Self)) {
        let start = narrow_offset(self.text.len());
        f(self);
        self.spans[id.index()] = start..narrow_offset(self.text.len());
    }

    /// Emit `items` separated by `sep`.
    fn join<T>(
        &mut self,
        sep: &str,
        items: impl IntoIterator<Item = T>,
        mut f: impl FnMut(&mut Self, T),
    ) {
        for (index, item) in items.into_iter().enumerate() {
            if index > 0 {
                self.push(sep);
            }
            f(self, item);
        }
    }
}

/// Render `ast` as an s-expression: its structure, with no `NodeId`s or spans.
///
/// Not focus syntax, and not parseable. This is what two trees are compared by.
pub fn canonical(ast: &Ast, interner: &LocalInterner) -> String {
    let printer = Printer {
        ast,
        // Predicates are named by id here, so no schema is needed — which is also
        // why a canonical form survives being compared across two schemas.
        schema: None,
        interner,
    };

    let mut out = String::new();
    printer.canonical_query(&mut out, ast.query());
    out
}

struct Printer<'a> {
    ast: &'a Ast,
    schema: Option<&'a Schema>,
    interner: &'a LocalInterner,
}

impl Printer<'_> {
    // ---- focus source ---------------------------------------------------------

    fn query(&self, out: &mut Spanned, query: &Query<NodeId>) {
        self.pattern(out, *query.head(), Level::Disjunction);
        out.push(" where ");
        out.join("; ", query.body(), |out, stmt| self.stmt(out, stmt));
    }

    fn stmt(&self, out: &mut Spanned, stmt: &QueryStmt<NodeId>) {
        match stmt {
            QueryStmt::Implicit(id) => self.pattern(out, *id, Level::Disjunction),
            QueryStmt::Bind(lhs, rhs) => {
                self.pattern(out, *lhs, Level::Disjunction);
                out.push(" = ");
                self.pattern(out, *rhs, Level::Disjunction);
            }
            QueryStmt::Negation(id) => {
                out.push("!");
                self.pattern(out, *id, Level::Disjunction);
            }
        }
    }

    /// Print the node at `id`, wrapping it if it binds more loosely than `permitted`.
    ///
    /// The wrapping parens are emitted *outside* the recorded span — see
    /// [`Spanned::span`] for why that is lowering's convention and not a choice.
    fn pattern(&self, out: &mut Spanned, id: NodeId, permitted: Level) {
        let wrapped = self.level(id) > permitted;
        if wrapped {
            out.push("(");
        }
        out.node(id, |out| self.bare(out, id));
        if wrapped {
            out.push(")");
        }
    }

    fn level(&self, id: NodeId) -> Level {
        match self.ast.store().kind(id) {
            ExprKind::Disjunction(_) => Level::Disjunction,
            ExprKind::Fact(..) => Level::Application,
            ExprKind::Access(..) | ExprKind::Select(..) => Level::Chain,
            _ => Level::Primary,
        }
    }

    fn bare(&self, out: &mut Spanned, id: NodeId) {
        match self.ast.store().kind(id) {
            ExprKind::Wildcard => out.push("_"),
            ExprKind::Never => out.push("never"),
            ExprKind::Var(symbol) => out.push(self.name(*symbol)),

            ExprKind::Lit(Literal::Int(value)) => {
                // `i64::MIN`'s magnitude does not fit an `i64`, and the grammar's
                // negative literal is `'-' Nat`, so the sign is printed separately
                // from an unsigned magnitude.
                if *value < 0 {
                    out.push(&format!("-{}", value.unsigned_abs()));
                } else {
                    out.push(&value.to_string());
                }
            }
            ExprKind::Lit(Literal::Str(symbol)) => out.push(&escape(self.name(*symbol))),
            ExprKind::Prefix(symbol) => {
                out.push(&escape(self.name(*symbol)));
                out.push("..");
            }

            ExprKind::Record(fields) => {
                out.push("{");
                out.join(", ", fields.iter(), |out, (name, value)| {
                    out.push(self.name(*name));
                    out.push(" = ");
                    self.pattern(out, *value, Level::Disjunction);
                });
                out.push("}");
            }

            // An access chain's base is a primary or another chain; anything looser
            // is wrapped.
            ExprKind::Access(FieldRef::Key(name), base) => {
                self.pattern(out, *base, Level::Chain);
                out.push(".");
                out.push(self.name(*name));
            }
            ExprKind::Access(FieldRef::Value, base) => {
                self.pattern(out, *base, Level::Chain);
                out.push(".value");
            }
            ExprKind::Select(alt, base) => {
                self.pattern(out, *base, Level::Chain);
                out.push(".");
                out.push(self.name(*alt));
                out.push("?");
            }

            ExprKind::Fact(predicate, key) => {
                // Unreachable from a lowered tree — lowering only builds a `Fact`
                // for a predicate it resolved, under a schema that could name it —
                // but printing must not panic on a hand-built one.
                let name = self
                    .schema
                    .and_then(|s| s.get(*predicate))
                    .and_then(|p| p.name())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("unknown.Predicate{}", predicate.0));
                out.push(&name);
                out.push(" ");
                self.pattern(out, *key, Level::Application);
            }

            ExprKind::Disjunction(branches) => {
                out.join(" | ", branches.iter(), |out, branch| {
                    self.pattern(out, *branch, Level::Application)
                });
            }

            // Unlike a precedence paren, these belong to the subquery's own rule, so
            // they are emitted inside the node's span — which is where lowering puts
            // them too.
            ExprKind::Subquery(query) => {
                out.push("(");
                self.query(out, query);
                out.push(")");
            }

            // Deliberately not valid focus: a tree with an error node has no source,
            // and emitting something plausible would hide that.
            ExprKind::Error => out.push("!error"),
        }
    }

    // ---- canonical form -------------------------------------------------------

    fn canonical_query(&self, out: &mut String, query: &Query<NodeId>) {
        out.push_str("(query ");
        self.canonical_body(out, query);
        out.push(')');
    }

    /// `head stmt stmt …` — the inside a query and a subquery share.
    fn canonical_body(&self, out: &mut String, query: &Query<NodeId>) {
        self.canonical_pattern(out, *query.head());
        out.push(' ');

        for (index, stmt) in query.body().iter().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            match stmt {
                QueryStmt::Implicit(id) => {
                    out.push_str("(implicit ");
                    self.canonical_pattern(out, *id);
                    out.push(')');
                }
                QueryStmt::Bind(lhs, rhs) => {
                    out.push_str("(bind ");
                    self.canonical_pattern(out, *lhs);
                    out.push(' ');
                    self.canonical_pattern(out, *rhs);
                    out.push(')');
                }
                QueryStmt::Negation(id) => {
                    out.push_str("(not ");
                    self.canonical_pattern(out, *id);
                    out.push(')');
                }
            }
        }
    }

    /// Written into one buffer rather than folded up as a `String` per node: the
    /// fold concatenated whole subtrees at every level, so a tree of n nodes cost
    /// O(n²) copying to render.
    fn canonical_pattern(&self, out: &mut String, id: NodeId) {
        /// A `String` is an infallible sink; `write!` returns `Result` regardless.
        const SINK: &str = "writing to a String cannot fail";

        match self.ast.store().kind(id) {
            ExprKind::Wildcard => out.push_str("(wild)"),
            ExprKind::Never => out.push_str("(never)"),
            ExprKind::Error => out.push_str("(error)"),

            ExprKind::Var(symbol) => {
                out.push_str("(var ");
                out.push_str(self.name(*symbol));
                out.push(')');
            }

            ExprKind::Lit(Literal::Int(value)) => write!(out, "(int {value})").expect(SINK),
            ExprKind::Lit(Literal::Str(symbol)) => {
                write!(out, "(str {:?})", self.name(*symbol)).expect(SINK);
            }
            ExprKind::Prefix(symbol) => {
                write!(out, "(prefix {:?})", self.name(*symbol)).expect(SINK);
            }

            ExprKind::Record(fields) => {
                out.push_str("(record");
                for (name, value) in fields.iter() {
                    out.push_str(" (");
                    out.push_str(self.name(*name));
                    out.push(' ');
                    self.canonical_pattern(out, *value);
                    out.push(')');
                }
                out.push(')');
            }

            ExprKind::Access(FieldRef::Key(name), base) => {
                out.push_str("(field ");
                out.push_str(self.name(*name));
                out.push(' ');
                self.canonical_pattern(out, *base);
                out.push(')');
            }
            ExprKind::Access(FieldRef::Value, base) => {
                out.push_str("(value ");
                self.canonical_pattern(out, *base);
                out.push(')');
            }
            ExprKind::Select(alt, base) => {
                out.push_str("(select ");
                out.push_str(self.name(*alt));
                out.push(' ');
                self.canonical_pattern(out, *base);
                out.push(')');
            }

            ExprKind::Fact(predicate, key) => {
                write!(out, "(fact {} ", predicate.0).expect(SINK);
                self.canonical_pattern(out, *key);
                out.push(')');
            }

            ExprKind::Disjunction(branches) => {
                out.push_str("(or ");
                for (index, branch) in branches.iter().enumerate() {
                    if index > 0 {
                        out.push(' ');
                    }
                    self.canonical_pattern(out, *branch);
                }
                out.push(')');
            }

            ExprKind::Subquery(query) => {
                out.push_str("(subquery ");
                self.canonical_body(out, query);
                out.push(')');
            }
        }
    }

    fn name(&self, symbol: Symbol) -> &str {
        self.interner.try_resolve(symbol).unwrap_or("?")
    }
}

/// Quote and escape a string so the lexer accepts it and `unescape_str` inverts it.
///
/// The lexer's `String` regex admits `\" \\ \/ \b \f \n \r \t \uXXXX` and any other
/// character that is neither a quote, a backslash, nor a control character — so
/// control characters *must* be escaped, and everything else may be literal.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // Every other control character, DEL included — the regex's `[:cntrl:]`
            // covers 0x00–0x1F and 0x7F — has no short escape.
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{
        corpus,
        lower::lower,
        parse::parse,
        syntax::{proptest::arb_query_spec, source_range},
    };
    use ::proptest::prelude::*;

    /// Parse and lower `source`, requiring both to be clean.
    fn tree(source: &str) -> (Ast, LocalInterner, Schema) {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let parsed = parse(source);
        assert!(!parsed.has_errors(), "{source:?} must parse");
        let root = parsed.root().expect("a tree");
        let (ast, diags) = lower(&root, &schema, &mut interner);
        assert!(diags.is_empty(), "{source:?} must lower cleanly");
        (ast, interner, schema)
    }

    fn printed(source: &str) -> String {
        let (ast, interner, schema) = tree(source);
        print(&ast, &schema, &interner)
    }

    /// Printing puts parens exactly where the grammar needs them — no more, no less.
    #[test]
    fn parentheses_go_where_precedence_requires() {
        // Dot is tighter than application, so the access needs none.
        assert_eq!(
            printed("Y where test.Name Y.name"),
            "Y where test.Name Y.name"
        );
        // ...and a redundant pair is dropped.
        assert_eq!(
            printed("Y where test.Name (Y.name)"),
            "Y where test.Name Y.name"
        );

        // An application *under* an access does need them.
        assert_eq!(
            printed("(test.Bar {id = 1}).value where test.Foo _"),
            "(test.Bar {id = 1}).value where test.Foo _"
        );

        // `|` is looser than application: as a fact's key it is wrapped, as a
        // statement it is not.
        assert_eq!(
            printed("X where test.Foo (A | B)"),
            "X where test.Foo (A | B)"
        );
        assert_eq!(printed("X where A | B"), "X where A | B");

        // A disjunction branch that is itself a disjunction keeps its parens, or it
        // would re-parse as one flat three-branch node.
        assert_eq!(printed("X where (A | B) | C"), "X where (A | B) | C");
    }

    #[test]
    fn literals_and_names_survive_printing() {
        assert_eq!(
            printed("X where X = test.Count -42"),
            "X where X = test.Count -42"
        );
        assert_eq!(
            printed("X where X = test.Count -9223372036854775808"),
            "X where X = test.Count -9223372036854775808"
        );
        // Separators are not part of the value.
        assert_eq!(
            printed("X where X = test.Count 1_000"),
            "X where X = test.Count 1000"
        );
        assert_eq!(
            printed(r#"X where X = test.Name "a\nb""#),
            r#"X where X = test.Name "a\nb""#
        );
        assert_eq!(
            printed(r#"X where X = test.Name "abc".."#),
            r#"X where X = test.Name "abc".."#
        );
    }

    #[test]
    fn every_construct_prints() {
        assert_eq!(printed("X where X = never"), "X where X = never");
        assert_eq!(
            printed("X.alt? where test.Foo _"),
            "X.alt? where test.Foo _"
        );
        assert_eq!(
            printed("X.value where test.Foo _"),
            "X.value where test.Foo _"
        );
        assert_eq!(printed("_ where test.Foo {}"), "_ where test.Foo {}");
        assert_eq!(
            printed("X where !test.Bar {id = 1}"),
            "X where !test.Bar {id = 1}"
        );
        assert_eq!(
            printed("X where X = (Y where test.Foo {id = Y})"),
            "X where X = (Y where test.Foo {id = Y})"
        );
    }

    /// The property the printer exists for, over the hand-written corpus:
    /// **parse ∘ print is the identity on trees.** Printing then re-lowering must
    /// give a structurally identical tree.
    ///
    /// Entries whose lowering reports something are skipped — an error node has no
    /// source text, by design.
    #[test]
    fn printing_and_reparsing_the_corpus_is_the_identity() {
        let mut checked = 0;

        for entry in corpus::CORPUS {
            let schema = corpus::schema();
            let mut interner = LocalInterner::new(schema.interner().clone());

            let parsed = parse(entry.source);
            let Some(root) = parsed.root() else { continue };
            if parsed.has_errors() {
                continue;
            }
            let (ast, diags) = lower(&root, &schema, &mut interner);
            if !diags.is_empty() {
                continue;
            }

            let text = print(&ast, &schema, &interner);

            // Re-parse with a *fresh* interner, so the comparison cannot accidentally
            // depend on interning order.
            let mut reinterner = LocalInterner::new(schema.interner().clone());
            let reparsed = parse(&text);
            assert!(
                !reparsed.has_errors(),
                "printing {:?} gave {text:?}, which does not parse: {:?}",
                entry.source,
                reparsed
                    .diagnostics()
                    .iter()
                    .map(|d| &d.message)
                    .collect::<Vec<_>>()
            );
            let reroot = reparsed.root().expect("a tree");
            let (reast, rediags) = lower(&reroot, &schema, &mut reinterner);
            assert!(
                rediags.is_empty(),
                "printing {:?} gave {text:?}, which does not lower cleanly",
                entry.source
            );

            assert_eq!(
                canonical(&ast, &interner),
                canonical(&reast, &reinterner),
                "{:?} printed to {text:?}, which lowered to a different tree",
                entry.source
            );
            checked += 1;
        }

        assert!(checked > 20, "only {checked} entries were round-tripped");
    }

    /// Printing is idempotent: the second printing is byte-identical, which is what
    /// makes the output a normal form rather than merely valid.
    #[test]
    fn printing_is_idempotent() {
        for entry in corpus::CORPUS {
            let schema = corpus::schema();
            let mut interner = LocalInterner::new(schema.interner().clone());
            let parsed = parse(entry.source);
            let Some(root) = parsed.root() else { continue };
            if parsed.has_errors() {
                continue;
            }
            let (ast, diags) = lower(&root, &schema, &mut interner);
            if !diags.is_empty() {
                continue;
            }

            let once = print(&ast, &schema, &interner);
            let (reast, reinterner, _) = tree(&once);
            let twice = print(&reast, &schema, &reinterner);
            assert_eq!(once, twice, "for {:?}", entry.source);
        }
    }

    proptest! {
        /// **`parse ∘ print == id` on trees.** Generate a tree, print it, parse and
        /// lower the text, and the tree must come back structurally identical.
        ///
        /// Only that direction is claimed. `print ∘ parse` is not the identity on
        /// *text* — whitespace, redundant parens and the choice of escapes are all
        /// normalised away — which is why the comparison is between canonical forms
        /// of trees rather than between strings.
        ///
        /// This is what turns the hand-written corpus from the whole specification of
        /// the surface into a set of worked examples: the corpus says which syntax is
        /// acceptable, and this says the front end is faithful across all of it.
        #[test]
        fn lowering_a_printed_tree_gives_the_same_tree(spec in arb_query_spec()) {
            let schema = corpus::schema();
            let (ast, interner) = spec.build(&schema);
            let text = print(&ast, &schema, &interner);

            let parsed = parse(&text);
            prop_assert!(
                !parsed.has_errors(),
                "printed {text:?}, which does not parse: {:?}",
                parsed.diagnostics().iter().map(|d| &d.message).collect::<Vec<_>>()
            );
            let root = parsed.root().expect("a tree");

            // A fresh interner: the comparison must not depend on interning order.
            let mut reinterner = LocalInterner::new(schema.interner().clone());
            let (reast, diags) = lower(&root, &schema, &mut reinterner);
            prop_assert!(
                diags.is_empty(),
                "printed {text:?}, which does not lower cleanly: {:?}",
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            );

            prop_assert_eq!(
                canonical(&ast, &interner),
                canonical(&reast, &reinterner),
                "printed {:?}", text
            );
        }

        /// **A node's span is where its text was printed.** The printer records the
        /// range it emitted each node at; parsing and lowering that text must give
        /// back exactly those ranges.
        ///
        /// This is the half of the front end the tree round-trip is blind to. Spans
        /// carry no structure, so every one of them could be off by a byte, name a
        /// sibling, or swallow a precedence paren while the tree comparison stayed
        /// green — and spans are what every diagnostic points with.
        ///
        /// It is testable only because printing *predicts* the spans. A generated
        /// tree has no source of its own (`QuerySpec::build` pushes `0..0`), and
        /// re-deriving one by slicing a span and re-parsing it would only ever check
        /// that the span looks plausible, not that it is right.
        #[test]
        fn spans_are_where_the_text_was_printed(spec in arb_query_spec()) {
            let schema = corpus::schema();
            let (ast, interner) = spec.build(&schema);
            let printed = spanned(&ast, &schema, &interner);

            let parsed = parse(printed.text());
            prop_assert!(
                !parsed.has_errors(),
                "printed {:?}, which does not parse: {:?}",
                printed.text(),
                parsed.diagnostics().iter().map(|d| &d.message).collect::<Vec<_>>()
            );
            let root = parsed.root().expect("a tree");

            let mut reinterner = LocalInterner::new(schema.interner().clone());
            let (reast, diags) = lower(&root, &schema, &mut reinterner);
            prop_assert!(
                diags.is_empty(),
                "printed {:?}, which does not lower cleanly: {:?}",
                printed.text(),
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            );

            // The walk pairs nodes positionally, which only means anything if the two
            // trees have the same shape to begin with.
            prop_assert_eq!(
                canonical(&ast, &interner),
                canonical(&reast, &reinterner),
                "printed {:?}", printed.text()
            );

            spans_agree_in_query(&printed, (&ast, ast.query()), (&reast, reast.query()))?;
        }
    }

    /// The text a span covers, for a failure message.
    fn slice(text: &str, span: &NodeSpan) -> String {
        match text.get(source_range(span)) {
            Some(text) => format!("{text:?}"),
            None => "<not a valid range>".to_owned(),
        }
    }

    /// Walk two same-shaped trees together, checking each printed span against the
    /// one lowering recovered.
    fn spans_agree(
        printed: &Spanned,
        (ast, id): (&Ast, NodeId),
        (reast, reid): (&Ast, NodeId),
    ) -> Result<(), TestCaseError> {
        let expected = printed.span(id);
        let found = reast.store().span(reid);
        prop_assert_eq!(
            expected.clone(),
            found.clone(),
            "printed at {:?} = {}, lowered back at {:?} = {} — in {:?}",
            expected,
            slice(printed.text(), &expected),
            found,
            slice(printed.text(), &found),
            printed.text()
        );

        // Leaves have no children, and a variant mismatch is impossible: the caller
        // has already compared canonical forms.
        match (ast.store().kind(id), reast.store().kind(reid)) {
            (ExprKind::Record(fields), ExprKind::Record(refields)) => {
                for ((_, value), (_, revalue)) in fields.iter().zip(refields.iter()) {
                    spans_agree(printed, (ast, *value), (reast, *revalue))?;
                }
            }
            (ExprKind::Access(_, base), ExprKind::Access(_, rebase))
            | (ExprKind::Select(_, base), ExprKind::Select(_, rebase))
            | (ExprKind::Fact(_, base), ExprKind::Fact(_, rebase)) => {
                spans_agree(printed, (ast, *base), (reast, *rebase))?;
            }
            (ExprKind::Disjunction(branches), ExprKind::Disjunction(rebranches)) => {
                for (branch, rebranch) in branches.iter().zip(rebranches.iter()) {
                    spans_agree(printed, (ast, *branch), (reast, *rebranch))?;
                }
            }
            (ExprKind::Subquery(query), ExprKind::Subquery(requery)) => {
                spans_agree_in_query(printed, (ast, query), (reast, requery))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn spans_agree_in_query(
        printed: &Spanned,
        (ast, query): (&Ast, &Query<NodeId>),
        (reast, requery): (&Ast, &Query<NodeId>),
    ) -> Result<(), TestCaseError> {
        spans_agree(printed, (ast, *query.head()), (reast, *requery.head()))?;
        for (stmt, restmt) in query.body().iter().zip(requery.body()) {
            match (stmt, restmt) {
                (QueryStmt::Implicit(id), QueryStmt::Implicit(reid))
                | (QueryStmt::Negation(id), QueryStmt::Negation(reid)) => {
                    spans_agree(printed, (ast, *id), (reast, *reid))?;
                }
                (QueryStmt::Bind(lhs, rhs), QueryStmt::Bind(relhs, rerhs)) => {
                    spans_agree(printed, (ast, *lhs), (reast, *relhs))?;
                    spans_agree(printed, (ast, *rhs), (reast, *rerhs))?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod generator {
    use crate::focus::{corpus, print::print, syntax::proptest::arb_query_spec};
    use proptest::{
        strategy::{Strategy, ValueTree},
        test_runner::TestRunner,
    };

    /// The round-trip property is only as good as what it is handed, and a
    /// generator can degenerate silently — a change to a `prop_recursive` weight or
    /// a leaf set can quietly reduce it to variables and wildcards, leaving the
    /// property green and vacuous.
    ///
    /// So the shape of the generated population is itself asserted: mostly
    /// non-trivial trees, and every construct reached.
    #[test]
    fn the_generator_is_not_degenerate() {
        const RUNS: usize = 400;

        let schema = corpus::schema();
        let mut runner = TestRunner::deterministic();
        let mut sizes = vec![];
        let mut text = String::new();

        for _ in 0..RUNS {
            let spec = arb_query_spec().new_tree(&mut runner).unwrap().current();
            let (ast, interner) = spec.build(&schema);
            sizes.push(ast.store().len());
            text.push_str(&print(&ast, &schema, &interner));
            text.push('\n');
        }

        sizes.sort_unstable();
        let median = sizes[RUNS / 2];
        assert!(median >= 8, "median tree is only {median} nodes");

        let trivial = sizes.iter().filter(|n| **n <= 3).count();
        assert!(
            trivial * 10 < RUNS,
            "{trivial} of {RUNS} trees are trivial (<= 3 nodes)"
        );

        // Every construct on the surface must actually be reached, including the ones
        // whose *printing* is the interesting part.
        for (what, needle) in [
            ("disjunction", " | "),
            ("subquery", " where "),
            ("negation", "!"),
            ("record", "{"),
            ("empty record", "{}"),
            ("field access", "."),
            ("value access", ".value"),
            ("union select", "?"),
            ("never", "never"),
            ("wildcard", "_"),
            ("string prefix", ".."),
            ("negative literal", "-"),
            ("i64::MIN", "-9223372036854775808"),
            ("escaped quote", "\\\""),
            ("escaped control char", "\\u00"),
            ("parenthesised group", "("),
        ] {
            assert!(
                text.contains(needle),
                "the generator never produced a {what}"
            );
        }
    }
}
