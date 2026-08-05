//! CST façade → the typed [`SyntaxTree`] store.
//!
//! The second of the three tree representations ([chapter 7]). Where the façade's
//! job is fidelity, this one's is being the substrate the phases run on: a
//! struct-of-arrays tree whose `NodeId`s are stable, so typecheck annotates through
//! a side table instead of mutating, and flatten is an append-and-reindex into a
//! new store.
//!
//! Lowering is where the *permissive* grammar first meets meaning, so it is also
//! where two kinds of rejection happen: a name the schema doesn't have, and a
//! literal whose text doesn't denote a value. Everything else the grammar allowed
//! through is lowered faithfully and left for typecheck to report.
//!
//! Nothing here panics on a malformed tree. `parse` accumulates diagnostics and
//! still returns a tree, so lowering routinely sees one with holes in it; a missing
//! child becomes an [`ExprKind::Error`] node, never an `expect`.
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use codespan_reporting::diagnostic::Label;

use crate::focus::{
    cst::{CstKind, CstNode},
    lexer::{self, LiteralError, Token},
    parser::{Diagnostic, Rule, Span},
    schema::{LocalInterner, Schema, Symbol},
    syntax::{Ast, ExprKind, FieldRef, Literal, NodeId, Query, QueryStmt, SyntaxTree},
};

/// The field name that reads a fact's value side rather than a key field.
pub const VALUE_FIELD: &str = "value";

/// Lower a parse into the typed store.
///
/// The returned diagnostics are additional to the parse's own; Phase 3 replaces
/// both with the compilation context's single sink.
pub fn lower(
    root: &CstNode<'_>,
    schema: &Schema,
    interner: &mut LocalInterner,
) -> (Ast, Vec<Diagnostic>) {
    let mut lowering = Lowering {
        store: SyntaxTree::new(),
        schema,
        interner,
        diagnostics: vec![],
    };

    let query = match root.para(&mut |kind| lowering.algebra(kind)) {
        Out::Query(query) => query,
        // The root rule is `root: query`, so this is only reachable when the parse
        // failed badly enough that no query node was built.
        _ => {
            let head = lowering.push(ExprKind::Error, &root.span());
            Query::new(head, Box::from([]))
        }
    };

    (Ast::new(query, lowering.store), lowering.diagnostics)
}

/// One record field, with the span its name was written at so a duplicate can be
/// pointed at.
struct Field {
    name: Symbol,
    value: NodeId,
    span: Span,
}

/// What a lowered CST node contributes to its parent.
enum Out {
    Query(Query<NodeId>),
    Stmts(Vec<QueryStmt<NodeId>>),
    Stmt(QueryStmt<NodeId>),
    Pattern(NodeId),
    Fields(Vec<Field>),
    Field(Field),
    /// A token, or a node whose meaning lives entirely in its children.
    Nothing,
}

struct Lowering<'a> {
    store: SyntaxTree<ExprKind<NodeId>>,
    schema: &'a Schema,
    interner: &'a mut LocalInterner,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lowering<'a> {
    fn push(&mut self, kind: ExprKind<NodeId>, span: &Span) -> NodeId {
        // Spans are `u32` in the store to keep nodes compact; `parse` refuses a
        // source that could not be addressed by one.
        let start = span.start as u32;
        let end = span.end as u32;
        self.store.push(kind, start..end)
    }

    fn error(&mut self, span: &Span, code: &str, message: impl Into<String>) {
        self.diagnostics.push(
            Diagnostic::error()
                .with_code(code)
                .with_message(message.into())
                .with_label(Label::primary((), span.clone())),
        );
    }

    /// An `Error` node, reported.
    fn error_node(&mut self, span: &Span, code: &str, message: impl Into<String>) -> NodeId {
        self.error(span, code, message);
        self.push(ExprKind::Error, span)
    }

    /// An `Error` node standing in for a child the parser failed to produce. The
    /// parse diagnostic already said what was wrong, so this adds none.
    fn hole(&mut self, span: &Span) -> NodeId {
        self.push(ExprKind::Error, span)
    }

    fn literal_error(&mut self, span: &Span, err: LiteralError) -> NodeId {
        self.error_node(span, err.code(), err.message())
    }

    fn algebra<'s>(&mut self, kind: CstKind<'s, (CstNode<'s>, Out)>) -> Out {
        let CstKind::Rule {
            rule,
            span,
            children,
        } = kind
        else {
            return Out::Nothing;
        };

        match rule {
            // `root: query`
            Rule::Root => take(children, |out| match out {
                Out::Query(query) => Some(Out::Query(query)),
                _ => None,
            })
            .unwrap_or(Out::Nothing),

            // `query: pattern 'where' stmt_list`
            Rule::Query => {
                let mut head = None;
                let mut body = vec![];
                for (_, out) in children {
                    match out {
                        Out::Pattern(id) => head = Some(id),
                        Out::Stmts(stmts) => body = stmts,
                        _ => {}
                    }
                }
                let head = match head {
                    Some(id) => id,
                    None => self.hole(&span),
                };
                Out::Query(Query::new(head, body.into()))
            }

            Rule::StmtList => Out::Stmts(
                children
                    .into_iter()
                    .filter_map(|(_, out)| match out {
                        Out::Stmt(stmt) => Some(stmt),
                        _ => None,
                    })
                    .collect(),
            ),

            // `pattern ('=' pattern)`
            Rule::BindStmt => {
                let mut ids = patterns(children).into_iter();
                match (ids.next(), ids.next()) {
                    (Some(lhs), Some(rhs)) => Out::Stmt(QueryStmt::Bind(lhs, rhs)),
                    // Half a bind: the parse already reported the missing side.
                    (Some(only), _) => {
                        let hole = self.hole(&span);
                        Out::Stmt(QueryStmt::Bind(only, hole))
                    }
                    (None, _) => {
                        let hole = self.hole(&span);
                        Out::Stmt(QueryStmt::Implicit(hole))
                    }
                }
            }

            // `Rule::Stmt` and `Rule::Primary` never appear in a well-formed tree —
            // every alternative of those rules renames its node — but a parse that
            // failed before reaching the rename leaves the bare rule behind. They
            // are handled, not assumed away.
            Rule::ImplicitBindStmt | Rule::Stmt => {
                let id = self.one_pattern(children, &span);
                Out::Stmt(QueryStmt::Implicit(id))
            }

            Rule::NegationStmt => {
                let id = self.one_pattern(children, &span);
                Out::Stmt(QueryStmt::Negation(id))
            }

            // Pass-throughs: a `pattern` with no `|`, a `branch` with no access
            // chain, and a parenthesised group are all their single child.
            Rule::Pattern | Rule::Branch | Rule::Fact | Rule::ParenPrimary | Rule::Primary => {
                let id = self.one_pattern(children, &span);
                Out::Pattern(id)
            }

            Rule::Disjunction => {
                let branches: Box<[NodeId]> = patterns(children).into();
                let id = self.push(ExprKind::Disjunction(branches), &span);
                Out::Pattern(id)
            }

            Rule::WildcardPrimary => {
                let id = self.push(ExprKind::Wildcard, &span);
                Out::Pattern(id)
            }

            Rule::NeverPrimary => {
                let id = self.push(ExprKind::Never, &span);
                Out::Pattern(id)
            }

            Rule::VarPrimary => {
                let id = match token_text(&children, Token::UId) {
                    Some(text) => {
                        let symbol = self.interner.get_or_intern(text);
                        self.push(ExprKind::Var(symbol), &span)
                    }
                    None => self.hole(&span),
                };
                Out::Pattern(id)
            }

            Rule::NatPrimary => {
                let id = self.int_literal(&children, &span, false);
                Out::Pattern(id)
            }

            Rule::IntPrimary => {
                let id = self.int_literal(&children, &span, true);
                Out::Pattern(id)
            }

            Rule::StringPrimary => {
                let id = match self.string_literal(&children, &span) {
                    Ok(symbol) => self.push(ExprKind::Lit(Literal::Str(symbol)), &span),
                    Err(id) => id,
                };
                Out::Pattern(id)
            }

            Rule::StringPrefixPrimary => {
                let id = match self.string_literal(&children, &span) {
                    Ok(symbol) => self.push(ExprKind::Prefix(symbol), &span),
                    Err(id) => id,
                };
                Out::Pattern(id)
            }

            Rule::AnonRecordPrimary => {
                let fields = children
                    .into_iter()
                    .find_map(|(_, out)| match out {
                        Out::Fields(fields) => Some(fields),
                        _ => None,
                    })
                    .unwrap_or_default();
                let id = self.record(fields, &span);
                Out::Pattern(id)
            }

            Rule::FieldList => Out::Fields(
                children
                    .into_iter()
                    .filter_map(|(_, out)| match out {
                        Out::Field(field) => Some(field),
                        _ => None,
                    })
                    .collect(),
            ),

            // `field: LId '=' pattern`
            Rule::Field => {
                let name = token_text(&children, Token::LId).unwrap_or_default();
                let name = self.interner.get_or_intern(name);
                let value = self.one_pattern(children, &span);
                Out::Field(Field {
                    name,
                    value,
                    span: span.clone(),
                })
            }

            // `fact_pattern: QId branch`
            Rule::FactPattern => {
                let id = self.fact(children, &span);
                Out::Pattern(id)
            }

            // `primary ('.' LId ['?'])*` — one node per step, left-nested.
            Rule::AccessPattern => {
                let id = self.access_chain(children, &span);
                Out::Pattern(id)
            }

            // `'(' pattern 'where' stmt_list ')'` — the same shape as a query.
            Rule::SubqueryPrimary => {
                let mut head = None;
                let mut body = vec![];
                for (_, out) in children {
                    match out {
                        Out::Pattern(id) => head = Some(id),
                        Out::Stmts(stmts) => body = stmts,
                        _ => {}
                    }
                }
                let head = match head {
                    Some(id) => id,
                    None => self.hole(&span),
                };
                let id = self.push(ExprKind::Subquery(Query::new(head, body.into())), &span);
                Out::Pattern(id)
            }

            // The parser's own error node: the diagnostic is already reported.
            Rule::Error => Out::Nothing,
        }
    }

    /// The single pattern among `children`, or a hole.
    fn one_pattern(&mut self, children: Box<[(CstNode<'_>, Out)]>, span: &Span) -> NodeId {
        match patterns(children).into_iter().next() {
            Some(id) => id,
            None => self.hole(span),
        }
    }

    fn int_literal(
        &mut self,
        children: &[(CstNode<'_>, Out)],
        span: &Span,
        negative: bool,
    ) -> NodeId {
        let Some(text) = token_text(children, Token::Nat) else {
            return self.hole(span);
        };

        match lexer::parse_nat(text).and_then(|n| lexer::signed_literal(n, negative)) {
            Ok(value) => self.push(ExprKind::Lit(Literal::Int(value)), span),
            Err(err) => self.literal_error(span, err),
        }
    }

    /// The decoded, interned string of a `String` token. `Err` carries the node
    /// already pushed for the failure.
    fn string_literal(
        &mut self,
        children: &[(CstNode<'_>, Out)],
        span: &Span,
    ) -> Result<Symbol, NodeId> {
        let Some(text) = token_text(children, Token::String) else {
            return Err(self.hole(span));
        };

        match lexer::unescape_str(text) {
            Ok(decoded) => Ok(self.interner.get_or_intern(&decoded)),
            Err(err) => Err(self.literal_error(span, err)),
        }
    }

    /// Record fields, sorted by name with duplicates rejected.
    ///
    /// Sorted by *name*, not by `Symbol`: a `Symbol` orders by interning order,
    /// which is an accident of what the schema happened to see first. Sorting is a
    /// codec-level requirement ([chapter 6]) and must mean the same thing every
    /// run.
    ///
    /// [chapter 6]: ../../../docs/06-types-and-schema.md
    fn record(&mut self, mut fields: Vec<Field>, span: &Span) -> NodeId {
        fields.sort_by(|a, b| self.name_of(a.name).cmp(self.name_of(b.name)));

        let mut kept: Vec<(Symbol, NodeId)> = Vec::with_capacity(fields.len());
        let mut duplicates = vec![];

        for field in fields {
            if kept.last().is_some_and(|(name, _)| *name == field.name) {
                duplicates.push((field.name, field.span));
                continue;
            }
            kept.push((field.name, field.value));
        }

        for (name, at) in duplicates {
            let name = self.name_of(name).to_owned();
            self.error(
                &at,
                "reject/duplicate-field",
                format!("field `{name}` is given twice"),
            );
        }

        self.push(ExprKind::Record(kept.into()), span)
    }

    fn fact(&mut self, children: Box<[(CstNode<'_>, Out)]>, span: &Span) -> NodeId {
        let name = token_text(&children, Token::QId)
            .unwrap_or_default()
            .to_owned();
        let predicate = self.schema.find_position(&name).map(|(id, _)| id);
        let key = self.one_pattern(children, span);

        match predicate {
            Some(id) => self.push(ExprKind::Fact(id, key), span),
            None => self.error_node(
                span,
                "reject/unknown-predicate",
                format!("`{name}` is not a predicate in this schema"),
            ),
        }
    }

    /// `X.a.b?` → `Select(b, Access(a, Var(X)))`.
    ///
    /// The CST holds the chain flat — one node with the base and every step — so the
    /// nesting is built here, innermost first.
    fn access_chain(&mut self, children: Box<[(CstNode<'_>, Out)]>, span: &Span) -> NodeId {
        // Walk the children in order: the base pattern, then `.name` steps each
        // optionally followed by `?`.
        let mut current = None;
        let mut pending: Option<(Symbol, Span)> = None;

        for (node, out) in children {
            if let Out::Pattern(id) = out {
                current = Some(id);
                continue;
            }

            let CstKind::Token {
                token,
                text,
                span: at,
                ..
            } = node.kind()
            else {
                continue;
            };

            match token {
                Token::LId => {
                    // A step is only complete once we know whether `?` follows, so
                    // the previous one is emitted here.
                    if let Some((name, step)) = pending.take() {
                        current = Some(self.access(current, name, &step));
                    }
                    pending = Some((self.interner.get_or_intern(text), step_span(span, &at)));
                }
                Token::Question => {
                    if let Some((name, step)) = pending.take() {
                        let base = match current {
                            Some(id) => id,
                            None => self.hole(&step),
                        };
                        // Extended through the `?`, this node's own last token.
                        let step = step_span(span, &at);
                        current = Some(self.push(ExprKind::Select(name, base), &step));
                    }
                }
                _ => {}
            }
        }

        if let Some((name, step)) = pending.take() {
            current = Some(self.access(current, name, &step));
        }

        match current {
            Some(id) => id,
            None => self.hole(span),
        }
    }

    /// One `.name` step. `value` names the fact's value side rather than a key
    /// field; whether that is *ambiguous* — a key field also called `value` — is a
    /// schema question, so typecheck reports it.
    fn access(&mut self, base: Option<NodeId>, name: Symbol, span: &Span) -> NodeId {
        let base = match base {
            Some(id) => id,
            None => self.hole(span),
        };
        let field = if self.name_of(name) == VALUE_FIELD {
            FieldRef::Value
        } else {
            FieldRef::Key(name)
        };
        self.push(ExprKind::Access(field, base), span)
    }

    fn name_of(&self, symbol: Symbol) -> &str {
        self.interner.try_resolve(symbol).unwrap_or_default()
    }
}

/// The span of one step of an access chain: from the start of the chain's source
/// text through the step's own last token.
///
/// A step is written postfix, so its own tokens (`name`, or `name` and `?`) are not
/// the text it stands for — `X.a.b` is one node covering all of it. Two things force
/// this shape. Typecheck labels a diagnostic with the node's span whatever the kind
/// (`ty.rs`), so a step spanning only its name would underline `b` where an
/// application underlines the whole of `test.Foo X`. And the start has to come from
/// the *chain's* span rather than the base node's, because a parenthesised base
/// passes its parens through (`Rule::ParenPrimary` above): taking the base node's
/// start would put `(test.Foo _).id?` at `test.Foo _).id?`, an underline that opens
/// inside a paren it never closes.
fn step_span(chain: &Span, last: &Span) -> Span {
    chain.start..last.end
}

/// The first `Some` a picker returns over `children`.
fn take<'s, T>(
    children: Box<[(CstNode<'s>, Out)]>,
    mut pick: impl FnMut(Out) -> Option<T>,
) -> Option<T> {
    children.into_iter().find_map(|(_, out)| pick(out))
}

/// Every pattern among `children`, in order.
fn patterns(children: Box<[(CstNode<'_>, Out)]>) -> Vec<NodeId> {
    children
        .into_iter()
        .filter_map(|(_, out)| match out {
            Out::Pattern(id) => Some(id),
            _ => None,
        })
        .collect()
}

/// The text of the first `token` directly among `children`.
fn token_text<'s>(children: &[(CstNode<'s>, Out)], token: Token) -> Option<&'s str> {
    children.iter().find_map(|(node, _)| match node.kind() {
        CstKind::Token {
            token: found, text, ..
        } if found == token => Some(text),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{corpus, parse::parse, schema::SchemaInterner};
    use proptest::prelude::*;

    /// Lower `source` against the corpus schema.
    fn lower_source(source: &str) -> (Ast, Vec<Diagnostic>, LocalInterner) {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let parsed = parse(source);
        let root = parsed.root().expect("a tree");
        let (ast, diags) = lower(&root, &schema, &mut interner);
        (ast, diags, interner)
    }

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().filter_map(|d| d.code.as_deref()).collect()
    }

    /// Render a node as `kind(child …)`, so a test states the shape it means.
    fn shape(ast: &Ast, interner: &LocalInterner, id: NodeId) -> String {
        let name = |s: Symbol| interner.try_resolve(s).unwrap_or("?").to_owned();
        ast.store().reduce(id, &mut |_, kind| match kind {
            ExprKind::Lit(Literal::Int(v)) => format!("{v}"),
            ExprKind::Lit(Literal::Str(s)) => format!("{:?}", name(s)),
            ExprKind::Prefix(s) => format!("prefix({:?})", name(s)),
            ExprKind::Var(s) => format!("var({})", name(s)),
            ExprKind::Wildcard => "_".to_owned(),
            ExprKind::Never => "never".to_owned(),
            ExprKind::Record(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(f, v)| format!("{}={v}", name(*f)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ExprKind::Access(FieldRef::Key(f), base) => format!("{base}.{}", name(f)),
            ExprKind::Access(FieldRef::Value, base) => format!("{base}.value!"),
            ExprKind::Select(alt, base) => format!("{base}.{}?", name(alt)),
            ExprKind::Disjunction(branches) => format!("({})", branches.join(" | ")),
            ExprKind::Subquery(q) => format!("subquery({})", q.head()),
            ExprKind::Fact(p, key) => format!("fact({}, {key})", p.0),
            ExprKind::Error => "!error".to_owned(),
        })
    }

    fn head_shape(source: &str) -> String {
        let (ast, diags, interner) = lower_source(source);
        assert!(codes(&diags).is_empty(), "{source:?}: {:?}", codes(&diags));
        shape(&ast, &interner, *ast.query().head())
    }

    /// The first statement's pattern, as a shape.
    fn stmt_shape(source: &str) -> String {
        let (ast, diags, interner) = lower_source(source);
        assert!(codes(&diags).is_empty(), "{source:?}: {:?}", codes(&diags));
        let id = match &ast.query().body()[0] {
            QueryStmt::Bind(_, rhs) => *rhs,
            QueryStmt::Implicit(id) | QueryStmt::Negation(id) => *id,
        };
        shape(&ast, &interner, id)
    }

    /// The text each node of an access chain covers, outermost first.
    fn chain_spans(source: &str) -> Vec<&str> {
        let (ast, diags, _) = lower_source(source);
        assert!(codes(&diags).is_empty(), "{source:?}: {:?}", codes(&diags));

        let mut out = vec![];
        let mut id = *ast.query().head();
        loop {
            let span = ast.store().span(id);
            out.push(&source[span.start as usize..span.end as usize]);
            id = match ast.store().kind(id) {
                ExprKind::Access(_, base) | ExprKind::Select(_, base) => *base,
                _ => break,
            };
        }
        out
    }

    /// A chain step spans the whole chain, not the name it was written with — the
    /// worked example behind [`step_span`]. Typecheck labels with the node's span
    /// whatever the kind, so a step has to underline all of `X.a.b` just as an
    /// application underlines all of `test.Foo X`.
    ///
    /// The last case is why the start comes from the chain's span and not the base
    /// node's: a parenthesised base *excludes* its parens (`Rule::ParenPrimary` is a
    /// pass-through to its child), so measuring from there would open an underline
    /// inside a paren it never closes.
    #[test]
    fn a_chain_step_spans_the_whole_chain() {
        assert_eq!(chain_spans("X.a.b where test.Foo _"), ["X.a.b", "X.a", "X"]);
        assert_eq!(chain_spans("X.alt? where test.Foo _"), ["X.alt?", "X"]);
        assert_eq!(chain_spans("X.value where test.Foo _"), ["X.value", "X"]);
        assert_eq!(
            chain_spans("(test.Bar {id = 1}).value where test.Foo _"),
            ["(test.Bar {id = 1}).value", "test.Bar {id = 1}"]
        );
    }

    #[test]
    fn literals_are_decoded_and_ranged() {
        assert_eq!(stmt_shape("X where X = test.Count 42"), "fact(6, 42)");
        assert_eq!(stmt_shape("X where X = test.Count -42"), "fact(6, -42)");
        assert_eq!(stmt_shape("X where X = test.Count 1_000"), "fact(6, 1000)");
        assert_eq!(
            stmt_shape("X where X = test.Count -9223372036854775808"),
            format!("fact(6, {})", i64::MIN)
        );
        assert_eq!(
            stmt_shape(r#"X where X = test.Name "a\nb""#),
            "fact(5, \"a\\nb\")"
        );
        assert_eq!(
            stmt_shape(r#"X where X = test.Name "abc".."#),
            "fact(5, prefix(\"abc\"))"
        );
    }

    #[test]
    fn malformed_literals_are_reported_by_code() {
        for (source, code) in [
            ("X where X = test.Count 1__0", "lit/int-underscore"),
            ("X where X = test.Count 1_", "lit/int-underscore"),
            ("X where X = test.Count 007", "lit/int-leading-zero"),
            (
                "X where X = test.Count 99999999999999999999",
                "lit/int-range",
            ),
            // One past i64::MAX without a minus in front of it.
            (
                "X where X = test.Count 9223372036854775808",
                "lit/int-range",
            ),
        ] {
            let (_, diags, _) = lower_source(source);
            assert_eq!(codes(&diags), [code], "for {source:?}");
        }
    }

    /// Record fields are a sorted set, so lowering sorts by name and rejects a
    /// duplicate rather than letting the last one win.
    #[test]
    fn record_fields_are_sorted_and_deduplicated() {
        // Written name-then-id; stored id-then-name.
        assert_eq!(
            stmt_shape("X where test.Foo {name = X, id = 1}"),
            "fact(0, {id=1, name=var(X)})"
        );

        let (_, diags, _) = lower_source("X where test.Foo {name = X, name = Y}");
        assert_eq!(codes(&diags), ["reject/duplicate-field"]);
    }

    #[test]
    fn an_access_chain_nests_left() {
        assert_eq!(head_shape("X.name where test.Foo _"), "var(X).name");
        assert_eq!(
            head_shape("X.outer.inner where test.Nested _"),
            "var(X).outer.inner"
        );
        // `.value` is the fact's value side, not a key field.
        assert_eq!(head_shape("X.value where test.Foo _"), "var(X).value!");
    }

    /// Union select is its own node, and mixes with plain access in one chain.
    #[test]
    fn union_select_is_distinct_from_access() {
        assert_eq!(head_shape("X.alt? where test.Foo _"), "var(X).alt?");
        assert_eq!(
            head_shape("X.a?.b where test.Foo _"),
            "var(X).a?.b",
            "the `?` must attach to `a`, not to `b`"
        );
        assert_eq!(head_shape("X.a.b? where test.Foo _"), "var(X).a.b?");
    }

    #[test]
    fn disjunction_stays_one_flat_node() {
        assert_eq!(
            stmt_shape("X where X = A | B | C"),
            "(var(A) | var(B) | var(C))"
        );
    }

    #[test]
    fn never_and_subqueries_lower_to_their_own_nodes() {
        assert_eq!(stmt_shape("X where X = never"), "never");
        assert_eq!(
            stmt_shape("X where X = (Y where test.Foo {id = Y})"),
            "subquery(var(Y))"
        );
    }

    #[test]
    fn negation_is_a_statement() {
        let (ast, _, _) = lower_source("X where test.Foo {id = X}; !test.Bar {id = X}");
        assert!(matches!(
            ast.query().body(),
            [QueryStmt::Implicit(_), QueryStmt::Negation(_)]
        ));
    }

    #[test]
    fn an_unknown_predicate_is_reported() {
        let (_, diags, _) = lower_source("X where X = nosuch.Pred _");
        assert_eq!(codes(&diags), ["reject/unknown-predicate"]);
    }

    /// Nothing in the corpus makes lowering panic — including the entries that are
    /// deliberately not focus, whose trees have holes in them.
    #[test]
    fn every_corpus_entry_lowers_without_panicking() {
        for entry in corpus::CORPUS {
            let schema = corpus::schema();
            let mut interner = LocalInterner::new(schema.interner().clone());
            let parsed = parse(entry.source);
            if let Some(root) = parsed.root() {
                let _ = lower(&root, &schema, &mut interner);
            }
        }
    }

    /// An empty interner: a query whose names are all local, so the schema-first
    /// path is not what keeps lowering upright.
    fn bare_interner() -> LocalInterner {
        LocalInterner::new(SchemaInterner::new(Rodeo::new().into_reader()))
    }

    proptest! {
        /// Lowering a broken tree yields error nodes, never a panic. `parse`
        /// accumulates diagnostics and still returns a tree, so this is the
        /// ordinary case, not an edge one.
        #[test]
        fn lowering_arbitrary_sources_never_panics(source in arb_source()) {
            let schema = corpus::schema();
            let mut interner = LocalInterner::new(schema.interner().clone());
            let parsed = parse(&source);
            if let Some(root) = parsed.root() {
                let (ast, _) = lower(&root, &schema, &mut interner);
                // The head is always a real node, even when nothing parsed.
                prop_assert!(!ast.store().is_empty());
            }
        }
    }

    use lasso::Rodeo;

    /// Fragments that reach every rule, including ones that will not compose.
    fn arb_source() -> impl Strategy<Value = String> {
        let fragment = prop_oneof![
            Just("where"),
            Just("X"),
            Just("_"),
            Just("never"),
            Just("test.Foo"),
            Just("nosuch.Pred"),
            Just("{"),
            Just("}"),
            Just("("),
            Just(")"),
            Just("="),
            Just(";"),
            Just(","),
            Just("."),
            Just(".."),
            Just("|"),
            Just("?"),
            Just("!"),
            Just("-"),
            Just("1__0"),
            Just("42"),
            Just("\"s\""),
            Just("name"),
        ];
        proptest::collection::vec(fragment, 0..24).prop_map(|parts| parts.join(" "))
    }

    #[test]
    fn a_bare_interner_still_lowers() {
        let schema = corpus::schema();
        let mut interner = bare_interner();
        let parsed = parse("X.name where test.Foo {id = X}");
        let root = parsed.root().expect("a tree");
        let (ast, _) = lower(&root, &schema, &mut interner);
        assert!(!ast.store().is_empty());
    }
}
