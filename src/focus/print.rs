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

use crate::focus::{
    schema::{LocalInterner, Schema, Symbol},
    syntax::{Ast, ExprKind, FieldRef, Literal, NodeId, Query, QueryStmt},
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
    Printer {
        ast,
        schema: Some(schema),
        interner,
    }
    .query(ast.query())
}

/// Render `ast` as an s-expression: its structure, with no `NodeId`s or spans.
///
/// Not focus syntax, and not parseable. This is what two trees are compared by.
pub fn canonical(ast: &Ast, interner: &LocalInterner) -> String {
    Printer {
        ast,
        // Predicates are named by id here, so no schema is needed — which is also
        // why a canonical form survives being compared across two schemas.
        schema: None,
        interner,
    }
    .canonical_query(ast.query())
}

struct Printer<'a> {
    ast: &'a Ast,
    schema: Option<&'a Schema>,
    interner: &'a LocalInterner,
}

impl<'a> Printer<'a> {
    // ---- focus source ---------------------------------------------------------

    fn query(&self, query: &Query<NodeId>) -> String {
        let head = self.pattern(*query.head(), Level::Disjunction);
        let body = query
            .body()
            .iter()
            .map(|stmt| self.stmt(stmt))
            .collect::<Vec<_>>()
            .join("; ");
        format!("{head} where {body}")
    }

    fn stmt(&self, stmt: &QueryStmt<NodeId>) -> String {
        match stmt {
            QueryStmt::Implicit(id) => self.pattern(*id, Level::Disjunction),
            QueryStmt::Bind(lhs, rhs) => format!(
                "{} = {}",
                self.pattern(*lhs, Level::Disjunction),
                self.pattern(*rhs, Level::Disjunction)
            ),
            QueryStmt::Negation(id) => format!("!{}", self.pattern(*id, Level::Disjunction)),
        }
    }

    /// Print the node at `id`, wrapping it if it binds more loosely than `permitted`.
    fn pattern(&self, id: NodeId, permitted: Level) -> String {
        let text = self.bare(id);
        if self.level(id) > permitted {
            format!("({text})")
        } else {
            text
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

    fn bare(&self, id: NodeId) -> String {
        match self.ast.store().kind(id) {
            ExprKind::Wildcard => "_".to_owned(),
            ExprKind::Never => "never".to_owned(),
            ExprKind::Var(symbol) => self.name(*symbol).to_owned(),

            ExprKind::Lit(Literal::Int(value)) => {
                // `i64::MIN`'s magnitude does not fit an `i64`, and the grammar's
                // negative literal is `'-' Nat`, so the sign is printed separately
                // from an unsigned magnitude.
                if *value < 0 {
                    format!("-{}", value.unsigned_abs())
                } else {
                    value.to_string()
                }
            }
            ExprKind::Lit(Literal::Str(symbol)) => escape(self.name(*symbol)),
            ExprKind::Prefix(symbol) => format!("{}..", escape(self.name(*symbol))),

            ExprKind::Record(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(name, value)| format!(
                        "{} = {}",
                        self.name(*name),
                        self.pattern(*value, Level::Disjunction)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),

            // An access chain's base is a primary or another chain; anything looser
            // is wrapped.
            ExprKind::Access(FieldRef::Key(name), base) => {
                format!("{}.{}", self.pattern(*base, Level::Chain), self.name(*name))
            }
            ExprKind::Access(FieldRef::Value, base) => {
                format!("{}.value", self.pattern(*base, Level::Chain))
            }
            ExprKind::Select(alt, base) => {
                format!("{}.{}?", self.pattern(*base, Level::Chain), self.name(*alt))
            }

            ExprKind::Fact(predicate, key) => {
                let name = match self.schema.and_then(|s| s.get(*predicate)) {
                    Some(p) => p.name().to_owned(),
                    // Unreachable from a lowered tree — lowering only builds a
                    // `Fact` for a predicate it resolved — but printing must not
                    // panic on a hand-built one.
                    None => format!("unknown.Predicate{}", predicate.0),
                };
                format!("{name} {}", self.pattern(*key, Level::Application))
            }

            ExprKind::Disjunction(branches) => branches
                .iter()
                .map(|branch| self.pattern(*branch, Level::Application))
                .collect::<Vec<_>>()
                .join(" | "),

            ExprKind::Subquery(query) => format!("({})", self.query(query)),

            // Deliberately not valid focus: a tree with an error node has no source,
            // and emitting something plausible would hide that.
            ExprKind::Error => "!error".to_owned(),
        }
    }

    // ---- canonical form -------------------------------------------------------

    fn canonical_query(&self, query: &Query<NodeId>) -> String {
        let body = query
            .body()
            .iter()
            .map(|stmt| match stmt {
                QueryStmt::Implicit(id) => format!("(implicit {})", self.canonical_pattern(*id)),
                QueryStmt::Bind(lhs, rhs) => format!(
                    "(bind {} {})",
                    self.canonical_pattern(*lhs),
                    self.canonical_pattern(*rhs)
                ),
                QueryStmt::Negation(id) => format!("(not {})", self.canonical_pattern(*id)),
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!("(query {} {body})", self.canonical_pattern(*query.head()))
    }

    fn canonical_pattern(&self, id: NodeId) -> String {
        self.ast.store().reduce(id, &mut |_, kind| match kind {
            ExprKind::Wildcard => "(wild)".to_owned(),
            ExprKind::Never => "(never)".to_owned(),
            ExprKind::Error => "(error)".to_owned(),
            ExprKind::Var(symbol) => format!("(var {})", self.name(symbol)),
            ExprKind::Lit(Literal::Int(value)) => format!("(int {value})"),
            ExprKind::Lit(Literal::Str(symbol)) => format!("(str {:?})", self.name(symbol)),
            ExprKind::Prefix(symbol) => format!("(prefix {:?})", self.name(symbol)),
            ExprKind::Record(fields) => format!(
                "(record{})",
                fields
                    .iter()
                    .map(|(name, value)| format!(" ({} {value})", self.name(*name)))
                    .collect::<String>()
            ),
            ExprKind::Access(FieldRef::Key(name), base) => {
                format!("(field {} {base})", self.name(name))
            }
            ExprKind::Access(FieldRef::Value, base) => format!("(value {base})"),
            ExprKind::Select(alt, base) => format!("(select {} {base})", self.name(alt)),
            ExprKind::Fact(predicate, key) => format!("(fact {} {key})", predicate.0),
            ExprKind::Disjunction(branches) => format!("(or {})", branches.join(" ")),
            ExprKind::Subquery(query) => format!(
                "(subquery {} {})",
                query.head(),
                query
                    .body()
                    .iter()
                    .map(|stmt| match stmt {
                        QueryStmt::Implicit(text) => format!("(implicit {text})"),
                        QueryStmt::Bind(lhs, rhs) => format!("(bind {lhs} {rhs})"),
                        QueryStmt::Negation(text) => format!("(not {text})"),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        })
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
    use crate::focus::{corpus, lower::lower, parse::parse, syntax::proptest::arb_query_spec};
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
