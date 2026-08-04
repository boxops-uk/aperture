use std::ops::Range;

use crate::focus::{
    iter::Address,
    plan::Project,
    schema::{PredicateId, Symbol},
};

pub type Span = Range<u32>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TyVarId(u32);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NodeId(u32);

pub enum Ty {
    Int,
    String,
    Fact(PredicateId),
    Record(Box<[(Symbol, Ty)]>),
    Var(TyVarId),
    Error,
}

pub enum GroundKind<T> {
    Lit(Literal),
    Var(Address),
    Wildcard,
    Prefix(Symbol),
    Record(Box<[(usize, T)]>),
}

#[derive(Clone, Copy)]
pub enum Literal {
    Int(i64),
    Str(Symbol),
}

#[derive(Debug)]
pub enum FactSource {
    Var(Address),
    Field(Box<FactSource>, usize),
}

pub enum FlatAccess {
    Scan(PredicateId, NodeId),
    Fetch(PredicateId, FactSource),
}

// Front-end scaffolding not yet wired into the pipeline (Phases 2–4); the
// fields are read once flatten/lowering lands.
#[allow(dead_code)]
pub struct FlatStmt {
    out: Option<Address>,
    access: FlatAccess,
}

#[allow(dead_code)]
pub struct FlatPlan {
    nvars: u32,
    body: Box<[FlatStmt]>,
    head: Project,
    store: SyntaxTree<GroundKind<NodeId>>,
}

#[derive(Clone, Copy)]
pub enum FieldRef {
    Key(Symbol),
    Value,
}

pub enum ExprKind<T> {
    Lit(Literal),
    Var(Symbol),
    Wildcard,
    /// The empty pattern — `never`. Deferred; typecheck reports it.
    Never,
    Prefix(Symbol),
    Record(Box<[(Symbol, T)]>),
    Access(FieldRef, T),
    /// Union select — `x.alt?`. A distinct operation from [`ExprKind::Access`]: it
    /// matches a discriminant and binds a payload rather than reading a field.
    Select(Symbol, T),
    /// `a | b | c` — **flat**, N branches, never a right-leaning tree. Flatten
    /// keeps this as one node and must not DNF-expand it across sibling conjuncts.
    Disjunction(Box<[T]>),
    Subquery(Query<T>),
    Fact(PredicateId, T),
    Error,
}

pub enum QueryStmt<T> {
    Bind(T, T),
    Implicit(T),
    /// `!pattern`. Negation is a statement, not a pattern — it is reordered
    /// relative to the statements that bind its non-locals.
    Negation(T),
}

pub struct Query<T> {
    body: Box<[QueryStmt<T>]>,
    head: T,
}

impl<T> Query<T> {
    pub fn new(head: T, body: Box<[QueryStmt<T>]>) -> Self {
        Query { body, head }
    }

    pub fn head(&self) -> &T {
        &self.head
    }

    pub fn body(&self) -> &[QueryStmt<T>] {
        &self.body
    }
}

pub struct Ast {
    query: Query<NodeId>,
    store: SyntaxTree<ExprKind<NodeId>>,
}

impl Ast {
    pub fn new(query: Query<NodeId>, store: SyntaxTree<ExprKind<NodeId>>) -> Self {
        Ast { query, store }
    }

    pub fn query(&self) -> &Query<NodeId> {
        &self.query
    }

    pub fn store(&self) -> &SyntaxTree<ExprKind<NodeId>> {
        &self.store
    }
}

/// A struct-of-arrays tree indexed by [`NodeId`].
///
/// Append-only: lowering pushes children before their parent, so a `NodeId` is
/// stable for the tree's life and later phases annotate it through *side tables*
/// rather than mutating the tree ([chapter 7]).
///
/// [chapter 7]: ../../../docs/07-compilation.md
pub struct SyntaxTree<K: Recursive> {
    kinds: Vec<K>,
    spans: Vec<Span>,
}

impl<K: Recursive> Default for SyntaxTree<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Recursive> SyntaxTree<K> {
    pub fn new() -> Self {
        SyntaxTree {
            kinds: vec![],
            spans: vec![],
        }
    }

    /// Append a node and return its id.
    pub fn push(&mut self, kind: K, span: Span) -> NodeId {
        let id = NodeId(self.kinds.len() as u32);
        self.kinds.push(kind);
        self.spans.push(span);
        id
    }

    pub fn kind(&self, id: NodeId) -> &K {
        &self.kinds[id.0 as usize]
    }

    pub fn span(&self, id: NodeId) -> Span {
        self.spans[id.0 as usize].clone()
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// Fold the subtree at `id` bottom-up.
    ///
    /// The algebra is handed the node's kind with each child already replaced by
    /// that child's result — `K::Base<R>` — which is what makes one generic fold
    /// serve every phase.
    pub fn reduce<R, F>(&self, id: NodeId, f: &mut F) -> R
    where
        F: FnMut(NodeId, K::Base<R>) -> R,
    {
        let acc = self.kinds[id.0 as usize].map(|child_id| self.reduce(child_id, f));
        f(id, acc)
    }
}

pub trait Recursive {
    type Base<R>;
    fn map<R, F: FnMut(NodeId) -> R>(&self, f: F) -> Self::Base<R>;
}

impl Recursive for GroundKind<NodeId> {
    type Base<R> = GroundKind<R>;

    fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
        match self {
            GroundKind::Lit(lit) => GroundKind::Lit(*lit),
            GroundKind::Var(var) => GroundKind::Var(*var),
            GroundKind::Wildcard => GroundKind::Wildcard,
            GroundKind::Prefix(symbol) => GroundKind::Prefix(*symbol),
            GroundKind::Record(fields) => {
                let new_fields = fields
                    .iter()
                    .map(|(idx, node_id)| (*idx, f(*node_id)))
                    .collect();
                GroundKind::Record(new_fields)
            }
        }
    }
}

impl Recursive for ExprKind<NodeId> {
    type Base<R> = ExprKind<R>;

    fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
        match self {
            ExprKind::Lit(lit) => ExprKind::Lit(*lit),
            ExprKind::Var(symbol) => ExprKind::Var(*symbol),
            ExprKind::Wildcard => ExprKind::Wildcard,
            ExprKind::Prefix(symbol) => ExprKind::Prefix(*symbol),
            ExprKind::Record(fields) => ExprKind::Record(
                fields
                    .iter()
                    .map(|(symbol, node_id)| (*symbol, f(*node_id)))
                    .collect(),
            ),
            ExprKind::Access(field_ref, node_id) => ExprKind::Access(*field_ref, f(*node_id)),
            ExprKind::Select(symbol, node_id) => ExprKind::Select(*symbol, f(*node_id)),
            ExprKind::Disjunction(branches) => {
                ExprKind::Disjunction(branches.iter().map(|id| f(*id)).collect())
            }
            ExprKind::Subquery(query) => ExprKind::Subquery(query.map(&mut f)),
            ExprKind::Fact(pred_id, node_id) => ExprKind::Fact(*pred_id, f(*node_id)),
            ExprKind::Never => ExprKind::Never,
            ExprKind::Error => ExprKind::Error,
        }
    }
}

impl Recursive for QueryStmt<NodeId> {
    type Base<R> = QueryStmt<R>;

    fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
        match self {
            QueryStmt::Bind(lhs, rhs) => QueryStmt::Bind(f(*lhs), f(*rhs)),
            QueryStmt::Implicit(node_id) => QueryStmt::Implicit(f(*node_id)),
            QueryStmt::Negation(node_id) => QueryStmt::Negation(f(*node_id)),
        }
    }
}

impl Recursive for Query<NodeId> {
    type Base<R> = Query<R>;

    fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
        Query {
            body: self.body.iter().map(|stmt| stmt.map(&mut f)).collect(),
            head: f(self.head),
        }
    }
}
