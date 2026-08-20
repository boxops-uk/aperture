//! The **lowered tree** and the types typecheck gave it.
//!
//! Where the parse tree is grammar-shaped and knows nothing, this one is what
//! the later phases actually run on: names resolved against a schema, records
//! sorted by field name, `.value` distinguished from a key field, and every node
//! carrying whatever type inference reached for it.
//!
//! **Two things a browser has to be told, because a `NodeId` cannot carry
//! them.** A `Symbol` is an index into the interner that minted it and means
//! nothing without it, so every name here is resolved to text at the boundary —
//! `every_symbol_in_a_view_is_resolved_to_text` is the guard, and the failure it
//! catches is a view that prints `?` where a variable's name should be. And a
//! `Ty` is rendered here rather than by the engine, because the engine's own
//! rendering is prose for a diagnostic ("an integer", "(already reported)")
//! while a panel wants the notation a schema is written in.
//!
//! The statements are listed beside the tree rather than in it: a query's body
//! is a `[QueryStmt]`, not an expression, and flattening it into the node arena
//! would invent a parent the engine does not have.

use fjord_engine::{
    compile::Compilation,
    syntax::{Ast, ExprKind, FieldRef, Literal, NodeId, QueryStmt, Recursive, Ty},
};
use fjord_schema::schema::{LocalInterner, Schema, Symbol};
use serde::Serialize;

use crate::{
    schema::compile as compile_schema,
    view::{DiagnosticView, Span, views_of},
};

/// One node of the lowered tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoweredNode {
    pub id: usize,
    /// The construct: `Var`, `Record`, `Access`, `Fact`, `Select`, `Arith`…
    pub kind: &'static str,
    /// What distinguishes this node from another of the same kind — a variable's
    /// name, a literal's value, the field being read, the predicate being
    /// matched. Every symbol in it is resolved to text.
    pub label: Option<String>,
    /// The type inference reached for it, in schema notation. Absent where the
    /// node sits under a construct that was rejected outright, so nothing typed
    /// it.
    pub ty: Option<String>,
    pub span: Span,
    pub children: Vec<usize>,
}

/// One statement of the query's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatementView {
    /// `Bind`, `Implicit`, `Negation`, `Deny`, or `Compare`.
    pub kind: &'static str,
    /// The comparison's operator, for the one statement that has one.
    pub op: Option<&'static str>,
    /// The nodes it relates, in the order they are written.
    pub nodes: Vec<usize>,
}

/// A query compiled through lex, parse, lower, typecheck, flatten and reorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lowered {
    /// Whether the schema itself lowered. A query cannot be resolved against a
    /// schema that does not exist, so this says which half a reader should fix.
    pub schema_ok: bool,
    /// The head — what the query answers — or `None` when nothing lowered.
    pub head: Option<usize>,
    /// The head's type, which is the type of every row the query returns.
    pub head_ty: Option<String>,
    pub statements: Vec<StatementView>,
    pub nodes: Vec<LoweredNode>,
    pub diagnostics: Vec<DiagnosticView>,
}

/// Compile `query` against `schema_source`, through the whole front end.
///
/// Never fails: a schema that does not lower gives `schema_ok: false` and its
/// own diagnostics, and a query that does not lower gives no tree and says why.
#[must_use]
pub fn lowered(schema_source: &str, query: &str) -> Lowered {
    let (schema, schema_diagnostics) = compile_schema(schema_source);

    let Some(schema) = schema else {
        return Lowered {
            schema_ok: false,
            head: None,
            head_ty: None,
            statements: Vec::new(),
            nodes: Vec::new(),
            diagnostics: schema_diagnostics,
        };
    };

    let mut compilation = Compilation::new(query, &schema);
    // The **whole** front end, not just typecheck: a page that showed "no
    // errors" for a query flatten would refuse would be lying, and several
    // refusals a reader meets first (`nyi/value-field`, `reject/not-a-generator`)
    // are flatten's. The plan itself is not shown yet; the diagnostics are.
    compilation.plan();

    let interner = compilation.interner();
    let head_ty = compilation
        .head_ty()
        .map(|ty| render(ty, &schema, interner));

    let (head, statements, nodes) = match compilation.ast() {
        Some(ast) => (
            Some(ast.query().head().index()),
            statements_of(ast),
            nodes_of(ast, &compilation, &schema, interner),
        ),
        None => (None, Vec::new(), Vec::new()),
    };

    Lowered {
        schema_ok: true,
        head,
        head_ty,
        statements,
        nodes,
        diagnostics: views_of(compilation.diagnostics()),
    }
}

/// The same view, already JSON.
#[must_use]
pub fn lowered_json(schema_source: &str, query: &str) -> String {
    serde_json::to_string(&lowered(schema_source, query)).expect("a lowered view serialises")
}

fn statements_of(ast: &Ast) -> Vec<StatementView> {
    ast.query()
        .body()
        .iter()
        .map(|statement| match statement {
            QueryStmt::Bind(lhs, rhs) => StatementView {
                kind: "Bind",
                op: None,
                nodes: vec![lhs.index(), rhs.index()],
            },
            QueryStmt::Implicit(node) => StatementView {
                kind: "Implicit",
                op: None,
                nodes: vec![node.index()],
            },
            QueryStmt::Negation(node) => StatementView {
                kind: "Negation",
                op: None,
                nodes: vec![node.index()],
            },
            QueryStmt::Deny(lhs, rhs) => StatementView {
                kind: "Deny",
                op: None,
                nodes: vec![lhs.index(), rhs.index()],
            },
            QueryStmt::Compare(lhs, rhs, op) => StatementView {
                kind: "Compare",
                op: Some(op.symbol()),
                nodes: vec![lhs.index(), rhs.index()],
            },
        })
        .collect()
}

/// What a walk of the lowered tree needs in hand at every node.
struct Walk<'a> {
    ast: &'a Ast,
    compilation: &'a Compilation<'a>,
    schema: &'a Schema,
    interner: &'a LocalInterner,
    seen: Vec<bool>,
    nodes: Vec<LoweredNode>,
}

impl Walk<'_> {
    fn visit(&mut self, id: NodeId) {
        if self.seen[id.index()] {
            return;
        }
        self.seen[id.index()] = true;

        let kind = self.ast.store().kind(id);
        let span = self.ast.store().span(id);

        // The same traversal every phase uses, so a construct cannot be shown
        // with a child missing: `map` is the `Recursive` impl's, and it visits a
        // subquery's head and body too.
        let mut children = Vec::new();
        kind.map(|child| children.push(child));

        self.nodes.push(LoweredNode {
            id: id.index(),
            kind: kind_name(kind),
            label: label_of(kind, self.schema, self.interner),
            ty: self
                .compilation
                .typed()
                .and_then(|typed| typed.ty(id))
                .map(|ty| render(ty, self.schema, self.interner)),
            span: Span {
                start: span.start as usize,
                end: span.end as usize,
            },
            children: children.iter().map(|child| child.index()).collect(),
        });

        for child in children {
            self.visit(child);
        }
    }
}

/// Every node the query reaches, ordered by id.
///
/// Walked from the head and the statements rather than counted from zero: the
/// store is dense, but nothing in its public surface turns an index back into a
/// `NodeId`, and inventing one would be a way to name a node that is not there.
/// A page keys on `id`, so the order is a convenience rather than a contract.
fn nodes_of(
    ast: &Ast,
    compilation: &Compilation<'_>,
    schema: &Schema,
    interner: &LocalInterner,
) -> Vec<LoweredNode> {
    let mut walk = Walk {
        ast,
        compilation,
        schema,
        interner,
        seen: vec![false; ast.store().len()],
        nodes: Vec::new(),
    };

    let query = ast.query();
    walk.visit(*query.head());
    for statement in query.body() {
        match statement {
            QueryStmt::Bind(lhs, rhs)
            | QueryStmt::Deny(lhs, rhs)
            | QueryStmt::Compare(lhs, rhs, _) => {
                walk.visit(*lhs);
                walk.visit(*rhs);
            }
            QueryStmt::Implicit(node) | QueryStmt::Negation(node) => walk.visit(*node),
        }
    }

    walk.nodes.sort_by_key(|node| node.id);
    walk.nodes
}

/// The construct's name. Exhaustive, so a construct added to sigla does not
/// compile until somebody says what it is called here.
const fn kind_name(kind: &ExprKind<NodeId>) -> &'static str {
    match kind {
        ExprKind::Lit(Literal::Int(_)) => "Int",
        ExprKind::Lit(Literal::Str(_)) => "Str",
        ExprKind::Var(_) => "Var",
        ExprKind::Wildcard => "Wildcard",
        ExprKind::Never => "Never",
        ExprKind::Prefix(_) => "Prefix",
        ExprKind::Record(_) => "Record",
        ExprKind::Access(FieldRef::Key(_), _) => "Access",
        ExprKind::Access(FieldRef::Value, _) => "Value",
        ExprKind::Select(_, _) => "Select",
        ExprKind::Disjunction(_) => "Disjunction",
        ExprKind::Subquery(_) => "Subquery",
        ExprKind::Fact(_, _) => "Fact",
        ExprKind::Arith(_, _) => "Arith",
        ExprKind::Error => "Error",
    }
}

/// A name, through the interner that minted it.
///
/// `try_resolve` answering `None` means a symbol from *another* interner reached
/// this view — the failure `fjord_wire::desc` documents for the wire, where a
/// local and a schema symbol of the same number are different names and
/// resolving one against the wrong interner answers confidently with somebody
/// else's. It is written loudly rather than hidden, and
/// `every_symbol_in_a_view_is_resolved_to_text` fails on it.
fn resolve(symbol: Symbol, interner: &LocalInterner) -> String {
    interner
        .try_resolve(symbol)
        .unwrap_or(UNRESOLVED)
        .to_owned()
}

/// What a name that did not resolve is written as. Not `?`, which is a type
/// inference did not settle — a real answer, and one a reader should be able to
/// tell apart from a broken view at a glance.
pub const UNRESOLVED: &str = "<unresolved>";

/// What distinguishes this node from another of its kind.
fn label_of(kind: &ExprKind<NodeId>, schema: &Schema, interner: &LocalInterner) -> Option<String> {
    let name = |symbol: Symbol| resolve(symbol, interner);

    match kind {
        ExprKind::Lit(Literal::Int(value)) => Some(value.to_string()),
        ExprKind::Lit(Literal::Str(symbol)) => Some(format!("{:?}", name(*symbol))),
        ExprKind::Var(symbol) | ExprKind::Select(symbol, _) => Some(name(*symbol)),
        ExprKind::Prefix(symbol) => Some(format!("{:?}..", name(*symbol))),
        ExprKind::Access(FieldRef::Key(symbol), _) => Some(format!(".{}", name(*symbol))),
        ExprKind::Access(FieldRef::Value, _) => Some(".value".to_owned()),
        ExprKind::Record(fields) => Some(
            fields
                .iter()
                .map(|(field, _)| name(*field))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        ExprKind::Fact(predicate, _) => Some(predicate_name(*predicate, schema)),
        ExprKind::Arith(_, ops) => Some(
            ops.iter()
                .map(|op| op.symbol())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        ExprKind::Wildcard | ExprKind::Never | ExprKind::Disjunction(_) | ExprKind::Subquery(_) => {
            None
        }
        ExprKind::Error => None,
    }
}

/// A predicate's qualified name, as a query writes it.
fn predicate_name(predicate: fjord_schema::schema::PredicateId, schema: &Schema) -> String {
    schema
        .get(predicate)
        .and_then(|declared| declared.name())
        .unwrap_or(UNRESOLVED)
        .to_owned()
}

/// A type in the notation a schema is written in.
///
/// Not the engine's own rendering, which is prose for a diagnostic ("an
/// integer") and deliberately so. A panel showing one type per node wants the
/// form a reader can compare against the schema beside it.
fn render(ty: &Ty, schema: &Schema, interner: &LocalInterner) -> String {
    let name = |symbol: Symbol| resolve(symbol, interner);

    match ty {
        Ty::Int => "int".to_owned(),
        Ty::String => "string".to_owned(),
        Ty::Fact(predicate) => predicate_name(*predicate, schema),
        Ty::Record(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(field, ty)| format!("{} : {}", name(*field), render(ty, schema, interner)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Ty::Union(alternatives) => format!(
            "{{{}}}",
            alternatives
                .iter()
                .map(|(alternative, disc, ty)| format!(
                    "{} : {} = {disc}",
                    name(*alternative),
                    render(ty, schema, interner)
                ))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        // Inference did not settle it. Written as the question it is rather than
        // as a type, because there is no such type in sigla.
        Ty::Var(_) => "?".to_owned(),
        // The poison that stops one fault cascading. A node carrying it has
        // already been reported against.
        Ty::Error => "(rejected)".to_owned(),
    }
}
