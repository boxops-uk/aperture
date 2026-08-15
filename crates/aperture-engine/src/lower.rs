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

use crate::{
    cst::{CstKind, CstNode},
    diag::{Code, Diagnostics},
    lexer::{self, LiteralError, Token},
    parser::{Rule, Span},
    syntax::{
        Ast, ExprKind, FieldRef, Literal, NodeId, Query, QueryStmt, SyntaxTree, narrow_offset,
    },
};
use aperture_schema::schema::{LocalInterner, Schema, Symbol};

/// The field name that reads a fact's value side rather than a key field.
pub const VALUE_FIELD: &str = "value";

/// Lower a parse into the typed store, reporting into `diagnostics`.
///
/// The sink is the parse's own: lowering's faults and the parse's belong to one
/// compilation and are read together, in source order.
pub fn lower(
    root: &CstNode<'_>,
    schema: &Schema,
    interner: &mut LocalInterner,
    diagnostics: &mut Diagnostics,
) -> Ast {
    // Interned once, up front: `access` decides `.value` by comparing against it,
    // and that decision must not depend on resolving a symbol back to text.
    let value_field = interner.get_or_intern(VALUE_FIELD);

    let mut lowering = Lowering {
        store: SyntaxTree::new(),
        schema,
        interner,
        value_field,
        diagnostics,
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

    Ast::new(query, lowering.store)
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
    /// [`VALUE_FIELD`] interned in this query's interner.
    value_field: Symbol,
    diagnostics: &'a mut Diagnostics,
}

impl Lowering<'_> {
    fn push(&mut self, kind: ExprKind<NodeId>, span: &Span) -> NodeId {
        // `span` is a `parser::Span` — a `usize` range — and the store holds a
        // `NodeSpan`. The two are named apart precisely so this crossing is
        // visible; `parse` refusing an over-long source is what makes it lossless.
        self.store
            .push(kind, narrow_offset(span.start)..narrow_offset(span.end))
    }

    fn error(&mut self, span: &Span, code: Code, message: impl Into<String>) {
        self.diagnostics.error(code, message, span);
    }

    /// An `Error` node, reported.
    fn error_node(&mut self, span: &Span, code: Code, message: impl Into<String>) -> NodeId {
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
            Rule::Query => Out::Query(self.head_and_body(children, &span)),

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
            Rule::BindStmt => Out::Stmt(self.bind_stmt(children, &span, QueryStmt::Bind)),

            // `pattern ('!=' pattern)` — the same two sides, and the same repair
            // for half of one; only what the statement *means* differs.
            Rule::DenyStmt => Out::Stmt(self.bind_stmt(children, &span, QueryStmt::Deny)),

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
                // A field with no name matches nothing and names nothing, and the
                // parse already reported what was missing. Dropped, rather than
                // interned as `""` — which sorts ahead of every real field and then
                // draws a second, baffling "`` is not a field here" from typecheck.
                let Some(name) = token_text(&children, Token::LId) else {
                    return Out::Nothing;
                };
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

            // `'(' pattern 'where' stmt_list ')'` — the same shape as a query, and
            // collected the same way.
            Rule::SubqueryPrimary => {
                let query = self.head_and_body(children, &span);
                let id = self.push(ExprKind::Subquery(query), &span);
                Out::Pattern(id)
            }

            // The parser's own error node: the diagnostic is already reported.
            Rule::Error => Out::Nothing,
        }
    }

    /// A head pattern and a statement list, which is the shape of both `query` and
    /// `subquery_primary` — the grammar reuses the rule, so lowering reuses this.
    ///
    /// A missing head is a hole: the parse reported whatever was wrong, and every
    /// query has to have one for the tree to be walkable.
    fn head_and_body(&mut self, children: Box<[(CstNode<'_>, Out)]>, span: &Span) -> Query<NodeId> {
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
            None => self.hole(span),
        };

        Query::new(head, body.into())
    }

    /// `pattern ('=' pattern)` or `pattern ('!=' pattern)`, with either side
    /// possibly missing.
    ///
    /// `build` is which two-sided statement to make — the operator is the only
    /// thing that differs, and a second copy of the missing-side repair would be a
    /// second place for it to drift.
    fn bind_stmt(
        &mut self,
        children: Box<[(CstNode<'_>, Out)]>,
        span: &Span,
        build: fn(NodeId, NodeId) -> QueryStmt<NodeId>,
    ) -> QueryStmt<NodeId> {
        let mut ids = patterns(children).into_iter();

        match (ids.next(), ids.next()) {
            (Some(lhs), Some(rhs)) => build(lhs, rhs),
            // Half a bind: the parse already reported the missing side.
            (Some(only), _) => {
                let hole = self.hole(span);
                build(only, hole)
            }
            (None, _) => {
                let hole = self.hole(span);
                QueryStmt::Implicit(hole)
            }
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
                Code::RejectDuplicateField,
                format!("field `{name}` is given twice"),
            );
        }

        self.push(ExprKind::Record(kept.into()), span)
    }

    fn fact(&mut self, children: Box<[(CstNode<'_>, Out)]>, span: &Span) -> NodeId {
        // Borrowed from the *source*, not from `children`, so this outlives the
        // move below and needs no clone.
        let name = token_text(&children, Token::QId).unwrap_or_default();
        let predicate = self.schema.find_position(name).map(|(id, _)| id);
        let key = self.one_pattern(children, span);

        match predicate {
            Some(id) => self.push(ExprKind::Fact(id, key), span),
            None => self.error_node(
                span,
                Code::RejectUnknownPredicate,
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
        // Compared as symbols, not as text. Two names are the same name exactly
        // when one interner gives them the same symbol, and going back through
        // the text meant this — a decision about what the query *means* — resting
        // on `name_of`, whose fallback exists only so a diagnostic can render
        // something.
        let field = if name == self.value_field {
            FieldRef::Value
        } else {
            FieldRef::Key(name)
        };
        self.push(ExprKind::Access(field, base), span)
    }

    /// A symbol's text, for putting in a diagnostic or ordering record fields.
    ///
    /// The fallback is why this must not be used for a decision about meaning: a
    /// symbol this interner cannot resolve is a bug, but a diagnostic still has to
    /// render something rather than fail.
    fn name_of(&self, symbol: Symbol) -> &str {
        self.interner.try_resolve(symbol).unwrap_or("?")
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
fn take<T>(
    children: Box<[(CstNode<'_>, Out)]>,
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
    use crate::{corpus, parse::parse, syntax::source_range};
    use aperture_schema::schema::SchemaInterner;
    use lasso::Rodeo;
    use proptest::prelude::*;

    /// Lower `source` against the corpus schema.
    ///
    /// One sink for the parse and the lowering both, as a compilation has.
    fn lower_source(source: &str) -> (Ast, Diagnostics, LocalInterner) {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();

        let cst = parse(source, &mut diagnostics).expect("a tree");
        let ast = lower(
            &CstNode::new(&cst),
            &schema,
            &mut interner,
            &mut diagnostics,
        );

        (ast, diagnostics, interner)
    }

    fn codes(diags: &Diagnostics) -> Vec<&str> {
        diags.codes().collect()
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
            QueryStmt::Bind(_, rhs) | QueryStmt::Deny(_, rhs) => *rhs,
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
            out.push(&source[source_range(&span)]);
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

    /// A record field the parser could not name is dropped rather than interned
    /// as the empty string. It used to become a real field called `""`, which
    /// sorts ahead of every other one — so `{a = 1, = 2}` lowered to `{=2, a=1}`
    /// and typecheck then reported the nameless field as unknown, on top of the
    /// parse error that had already explained it.
    #[test]
    fn a_nameless_field_is_dropped_not_named_empty() {
        assert_eq!(
            stmt_shape("X where test.Foo {a = 1, = 2}"),
            "fact(0, {a=1})"
        );
    }

    /// `.value` is decided by symbol identity, so it works whichever tier interned
    /// the name — the schema's, or the query's own when the schema never declared
    /// a field called `value`.
    #[test]
    fn value_access_is_decided_by_symbol_not_text() {
        // Schema tier: the corpus interns `value` (test.Shadow has such a field).
        assert_eq!(head_shape("X.value where test.Foo _"), "var(X).value!");

        // Local tier: nothing is declared at all, so `value` is a query-local
        // symbol and the comparison has to hold there too.
        let schema = corpus::schema();
        let mut interner = bare_interner();
        let mut diagnostics = Diagnostics::new();
        let cst = parse("X.value where test.Foo _", &mut diagnostics).expect("a tree");
        let ast = lower(
            &CstNode::new(&cst),
            &schema,
            &mut interner,
            &mut diagnostics,
        );

        assert_eq!(
            shape(&ast, &interner, *ast.query().head()),
            "var(X).value!",
            "`.value` must read the value side however `value` was interned"
        );
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
            let mut diagnostics = Diagnostics::new();
            if let Some(cst) = parse(entry.source, &mut diagnostics) {
                let _ = lower(
                    &CstNode::new(&cst),
                    &schema,
                    &mut interner,
                    &mut diagnostics,
                );
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
            let mut diagnostics = Diagnostics::new();
            if let Some(cst) = parse(&source, &mut diagnostics) {
                let ast = lower(
                    &CstNode::new(&cst),
                    &schema,
                    &mut interner,
                    &mut diagnostics,
                );
                // The head is always a real node, even when nothing parsed.
                prop_assert!(!ast.store().is_empty());
            }
        }
    }

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
        let mut diagnostics = Diagnostics::new();
        let cst = parse("X.name where test.Foo {id = X}", &mut diagnostics).expect("a tree");
        let root = CstNode::new(&cst);
        let ast = lower(&root, &schema, &mut interner, &mut diagnostics);
        assert!(!ast.store().is_empty());
    }
}
