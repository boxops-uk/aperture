use std::{ops::Range, sync::Arc};

use aperture_schema::schema::{PredicateId, Symbol};

/// A byte range in the query's source text, as stored on a node.
///
/// `u32` rather than the parser's `usize`, to keep a node compact:
/// [`parse`](crate::parse::parse) refuses a source longer than `u32::MAX`,
/// which is what makes the narrowing lossless.
///
/// **Named apart from [`parser::Span`](crate::parser::Span) deliberately.**
/// Both are byte ranges into the same text and differ only in width, so one name
/// for both meant either could be passed where the other was meant with nothing
/// but an `as u32` to tell them apart — inside a function handling both, which
/// `lower::push` does. Crossing between them goes through [`narrow_offset`] and
/// [`source_range`].
pub type NodeSpan = Range<u32>;

/// Narrow a source byte offset for storage in a [`NodeSpan`].
///
/// Lossless because `parse` refuses a source that could not be addressed by a
/// `u32`. Asserted in debug builds all the same, so a caller building a tree
/// without going through `parse` finds out here rather than storing a wrapped
/// offset that silently points at the wrong bytes.
#[must_use]
pub fn narrow_offset(offset: usize) -> u32 {
    debug_assert!(
        u32::try_from(offset).is_ok(),
        "byte offset {offset} does not fit a node span; `parse` refuses a source this long"
    );
    offset as u32
}

/// A [`NodeSpan`] widened back to source byte offsets — for slicing the source, or
/// labelling a diagnostic.
#[must_use]
pub fn source_range(span: &NodeSpan) -> Range<usize> {
    span.start as usize..span.end as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TyVarId(u32);

impl TyVarId {
    pub fn new(index: usize) -> Self {
        TyVarId(index as u32)
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeId(u32);

impl NodeId {
    /// Index into a side table. `NodeId`s are dense — the store is append-only —
    /// so an annotation table is a `Vec`, not a map ([chapter 7]).
    ///
    /// [chapter 7]: ../../../docs/07-compilation.md
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A query-level type.
///
/// Distinct from the schema's [`PredicateTy`](aperture_schema::schema::PredicateTy),
/// which has no type variables: a query is inferred, a schema is declared.
/// `Ty::Error` is a poison that unifies with anything, so one mistake reports once
/// instead of cascading.
///
/// There is no `Never` yet. `never` parses and lowers, but typecheck reports it as
/// not yet implemented, so a type for it would be speculative.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Int,
    String,
    Fact(PredicateId),
    /// Fields sorted by name, as everywhere. `Arc` rather than `Box` because
    /// substitution genuinely shares: one inferred type ends up in the
    /// substitution, in the annotation side table, and inside error values, so a
    /// `Ty` clone has to be a refcount bump rather than a deep copy of the tree
    /// (see `ty::Checker::repr`).
    Record(Arc<[(Symbol, Ty)]>),
    Var(TyVarId),
    Error,
}

#[derive(Clone, Copy)]
pub enum Literal {
    Int(i64),
    Str(Symbol),
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
    /// `pattern != pattern` — a **denial**: the value on the left must *not* match
    /// the pattern on the right.
    ///
    /// The negative of the constraint [`Bind`](Self::Bind) can mean, and a separate
    /// statement rather than a flag on it because the two are not symmetric where
    /// it matters. A constraint is *sargeable* — `X = "a".."` narrows the level
    /// capturing `X` to a range — and a denial can never be: "does not start with
    /// `a`" is two ranges, so it filters rows a scan has already produced. Keeping
    /// them apart in the tree is what keeps that difference from having to be
    /// rediscovered in flatten.
    Deny(T, T),
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

    /// Whether `node` is a value known at **compile time** — a literal, or a record
    /// of them to any depth.
    ///
    /// Lives here because two phases have to agree on it exactly. Typecheck uses it
    /// to decide that `N = "abc"` is a *substitution* rather than the unification it
    /// defers, and flatten uses it to actually fold — so a phase that thought
    /// something folded when the other did not would accept a bind nothing can lower,
    /// and flatten's "no plan without a reason" assertion is all that would catch it.
    ///
    /// A string **prefix** is deliberately not constant: `"a"..` denotes a range, so
    /// there is no single value for a variable bound to it to be.
    #[must_use]
    pub fn is_constant(&self, node: NodeId) -> bool {
        match self.store.kind(node) {
            ExprKind::Lit(Literal::Int(_) | Literal::Str(_)) => true,
            ExprKind::Record(fields) => {
                fields.iter().all(|(_, pattern)| self.is_constant(*pattern))
            }
            _ => false,
        }
    }

    /// Whether a constant can be **destructured into** `node`: a variable, a
    /// wildcard, or a record of them to any depth.
    ///
    /// Shared between typecheck and flatten for the same reason as
    /// [`is_constant`](Self::is_constant) — one gates on it, the other walks it.
    ///
    /// A **literal** leaf is excluded, and that exclusion is load-bearing rather than
    /// conservative. `{a = 1} = {a = 2}` typechecks (both sides are `int`) and binds
    /// nothing, so flatten would emit no constraint and the statement would silently
    /// mean *true* where it means the empty relation. Deciding it needs the two
    /// constants' bytes compared, which is unification.
    ///
    /// A wildcard leaf is fine by the same reasoning read the other way: it binds
    /// nothing, but it also cannot fail, so "no constraint" is the right answer for it.
    #[must_use]
    pub fn is_destructurable(&self, node: NodeId) -> bool {
        match self.store.kind(node) {
            ExprKind::Var(_) | ExprKind::Wildcard => true,
            ExprKind::Record(fields) => fields
                .iter()
                .all(|(_, pattern)| self.is_destructurable(*pattern)),
            _ => false,
        }
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
    spans: Vec<NodeSpan>,
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
    pub fn push(&mut self, kind: K, span: NodeSpan) -> NodeId {
        let id = NodeId(self.kinds.len() as u32);
        self.kinds.push(kind);
        self.spans.push(span);
        id
    }

    pub fn kind(&self, id: NodeId) -> &K {
        &self.kinds[id.0 as usize]
    }

    pub fn span(&self, id: NodeId) -> NodeSpan {
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
            QueryStmt::Deny(lhs, rhs) => QueryStmt::Deny(f(*lhs), f(*rhs)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The two directions are inverse, which is what makes the narrower stored
    /// width a storage detail rather than a lossy one.
    #[test]
    fn narrowing_and_widening_round_trip() {
        for (start, end) in [(0usize, 0usize), (0, 12), (7, 7), (1_000, 1_048_576)] {
            let span = narrow_offset(start)..narrow_offset(end);
            assert_eq!(source_range(&span), start..end);
        }
    }

    /// The assertion is not decorative: an offset `parse` should have refused is
    /// caught, rather than wrapping to point silently at the wrong bytes.
    #[test]
    #[cfg(all(debug_assertions, target_pointer_width = "64"))]
    #[should_panic(expected = "does not fit a node span")]
    fn narrowing_rejects_an_offset_parse_would_have_refused() {
        let _ = narrow_offset(u32::MAX as usize + 1);
    }
}

/// Generators for the typed tree.
///
/// The strategy produces an interner-free **spec** rather than an [`Ast`] directly,
/// following the convention `tuple::proptest` sets: a spec holds names as `String`s,
/// so it shrinks to a small readable counterexample instead of to an opaque pile of
/// interner handles, and the real tree is materialised from it by [`QuerySpec::build`].
///
/// Every spec must be **buildable into a tree lowering could have produced**, since
/// that is what the round-trip property compares against
/// ([`print`](crate::print)). Three constraints follow, and each is enforced
/// by the generator rather than patched up when building:
///
/// - **field names exclude `value`**, which is the surface for a fact's value side —
///   `Access(Key("value"))` would print as `.value` and lower back as
///   `Access(Value)`. That the surface is genuinely ambiguous there is why
///   `reject/value-shadowed` exists.
/// - **a disjunction has at least two branches**, since one branch has no `|` to
///   print and would come back as a bare pattern.
/// - **a subquery has at least one statement**, as the grammar requires.
#[cfg(any(test, feature = "proptest"))]
pub mod proptest {
    use super::*;
    use ::proptest::prelude::*;
    use aperture_schema::schema::{LocalInterner, Schema};

    #[derive(Debug, Clone)]
    pub enum PatternSpec {
        Wildcard,
        Never,
        Var(String),
        Int(i64),
        Str(String),
        Prefix(String),
        Record(Vec<(String, PatternSpec)>),
        Field(String, Box<PatternSpec>),
        Value(Box<PatternSpec>),
        Select(String, Box<PatternSpec>),
        /// A predicate, by an index reduced modulo the schema's size at build time —
        /// so the spec shrinks as a small number and never names a predicate that
        /// does not exist.
        Fact(u32, Box<PatternSpec>),
        Or(Vec<PatternSpec>),
        Subquery(Box<QuerySpec>),
    }

    #[derive(Debug, Clone)]
    pub enum StmtSpec {
        Implicit(PatternSpec),
        Bind(PatternSpec, PatternSpec),
        Negation(PatternSpec),
        Deny(PatternSpec, PatternSpec),
    }

    #[derive(Debug, Clone)]
    pub struct QuerySpec {
        pub head: PatternSpec,
        pub body: Vec<StmtSpec>,
    }

    /// Variable names: valid `UId`s.
    fn arb_var() -> impl Strategy<Value = String> {
        ::proptest::sample::select(vec!["X", "Y", "Z", "Row", "A"]).prop_map(str::to_owned)
    }

    /// Field and alternative names: valid `LId`s, no keyword, and never `value`.
    fn arb_field() -> impl Strategy<Value = String> {
        ::proptest::sample::select(vec!["id", "name", "from", "to", "outer", "inner", "alt"])
            .prop_map(str::to_owned)
    }

    /// Strings, with the cases that stress escaping injected explicitly rather than
    /// left to a random draw: quotes, backslashes, control characters, an embedded
    /// NUL, and a non-BMP character.
    fn arb_text() -> impl Strategy<Value = String> {
        prop_oneof![
            4 => any::<String>(),
            1 => Just(String::new()),
            1 => Just("\0".to_owned()),
            1 => Just("a\"b\\c/d".to_owned()),
            1 => Just("\u{1}\u{7f}\n\r\t\u{8}\u{c}".to_owned()),
            1 => Just("\u{1f600}".to_owned()),
        ]
    }

    fn arb_int() -> impl Strategy<Value = i64> {
        prop_oneof![
            4 => any::<i64>(),
            1 => Just(i64::MIN),
            1 => Just(i64::MAX),
            1 => Just(0),
        ]
    }

    fn arb_stmt(
        pattern: BoxedStrategy<PatternSpec>,
    ) -> impl Strategy<Value = StmtSpec> + Clone + use<> {
        prop_oneof![
            pattern.clone().prop_map(StmtSpec::Implicit),
            (pattern.clone(), pattern.clone()).prop_map(|(l, r)| StmtSpec::Bind(l, r)),
            pattern.clone().prop_map(StmtSpec::Negation),
            (pattern.clone(), pattern).prop_map(|(l, r)| StmtSpec::Deny(l, r)),
        ]
    }

    pub fn arb_pattern_spec() -> BoxedStrategy<PatternSpec> {
        let leaf = prop_oneof![
            Just(PatternSpec::Wildcard),
            Just(PatternSpec::Never),
            arb_var().prop_map(PatternSpec::Var),
            arb_int().prop_map(PatternSpec::Int),
            arb_text().prop_map(PatternSpec::Str),
            arb_text().prop_map(PatternSpec::Prefix),
        ];

        leaf.prop_recursive(4, 48, 4, |inner| {
            prop_oneof![
                ::proptest::collection::vec((arb_field(), inner.clone()), 0..4)
                    .prop_map(PatternSpec::Record),
                (arb_field(), inner.clone())
                    .prop_map(|(name, base)| PatternSpec::Field(name, Box::new(base))),
                inner
                    .clone()
                    .prop_map(|base| PatternSpec::Value(Box::new(base))),
                (arb_field(), inner.clone())
                    .prop_map(|(alt, base)| PatternSpec::Select(alt, Box::new(base))),
                (0u32..64, inner.clone())
                    .prop_map(|(index, key)| PatternSpec::Fact(index, Box::new(key))),
                ::proptest::collection::vec(inner.clone(), 2..4).prop_map(PatternSpec::Or),
                (
                    inner.clone(),
                    ::proptest::collection::vec(arb_stmt(inner.boxed()), 1..3)
                )
                    .prop_map(|(head, body)| PatternSpec::Subquery(Box::new(
                        QuerySpec { head, body }
                    ))),
            ]
        })
        .boxed()
    }

    pub fn arb_query_spec() -> impl Strategy<Value = QuerySpec> {
        let pattern = arb_pattern_spec();
        (
            pattern.clone(),
            ::proptest::collection::vec(arb_stmt(pattern), 1..4),
        )
            .prop_map(|(head, body)| QuerySpec { head, body })
    }

    impl QuerySpec {
        /// Materialise the spec as a tree, against `schema`.
        pub fn build(&self, schema: &Schema) -> (Ast, LocalInterner) {
            let mut builder = Builder {
                store: SyntaxTree::new(),
                interner: LocalInterner::new(schema.interner().clone()),
                predicates: schema.len().max(1),
            };
            let query = builder.query(self);
            (Ast::new(query, builder.store), builder.interner)
        }
    }

    struct Builder {
        store: SyntaxTree<ExprKind<NodeId>>,
        interner: LocalInterner,
        predicates: usize,
    }

    impl Builder {
        /// Spans are all empty: a built tree has no source, and neither the printer
        /// nor the canonical form reads them.
        fn push(&mut self, kind: ExprKind<NodeId>) -> NodeId {
            self.store.push(kind, 0..0)
        }

        fn query(&mut self, spec: &QuerySpec) -> Query<NodeId> {
            let body = spec
                .body
                .iter()
                .map(|stmt| match stmt {
                    StmtSpec::Implicit(p) => QueryStmt::Implicit(self.pattern(p)),
                    StmtSpec::Bind(l, r) => {
                        let lhs = self.pattern(l);
                        let rhs = self.pattern(r);
                        QueryStmt::Bind(lhs, rhs)
                    }
                    StmtSpec::Negation(p) => QueryStmt::Negation(self.pattern(p)),
                    StmtSpec::Deny(l, r) => {
                        let lhs = self.pattern(l);
                        let rhs = self.pattern(r);
                        QueryStmt::Deny(lhs, rhs)
                    }
                })
                .collect();
            let head = self.pattern(&spec.head);
            Query::new(head, body)
        }

        fn pattern(&mut self, spec: &PatternSpec) -> NodeId {
            let kind = match spec {
                PatternSpec::Wildcard => ExprKind::Wildcard,
                PatternSpec::Never => ExprKind::Never,
                PatternSpec::Var(name) => ExprKind::Var(self.interner.get_or_intern(name)),
                PatternSpec::Int(value) => ExprKind::Lit(Literal::Int(*value)),
                PatternSpec::Str(text) => {
                    ExprKind::Lit(Literal::Str(self.interner.get_or_intern(text)))
                }
                PatternSpec::Prefix(text) => ExprKind::Prefix(self.interner.get_or_intern(text)),

                PatternSpec::Record(fields) => {
                    // Sorted by name and deduplicated, exactly as lowering leaves a
                    // record — otherwise the round-trip would compare a tree lowering
                    // could not have produced.
                    let mut built: Vec<(Symbol, NodeId)> = vec![];
                    let mut names: Vec<&str> = vec![];
                    let mut sorted: Vec<&(String, PatternSpec)> = fields.iter().collect();
                    sorted.sort_by(|a, b| a.0.cmp(&b.0));
                    for (name, value) in sorted {
                        if names.last() == Some(&name.as_str()) {
                            continue;
                        }
                        names.push(name);
                        let symbol = self.interner.get_or_intern(name);
                        let value = self.pattern(value);
                        built.push((symbol, value));
                    }
                    ExprKind::Record(built.into())
                }

                PatternSpec::Field(name, base) => {
                    let symbol = self.interner.get_or_intern(name);
                    let base = self.pattern(base);
                    ExprKind::Access(FieldRef::Key(symbol), base)
                }
                PatternSpec::Value(base) => {
                    let base = self.pattern(base);
                    ExprKind::Access(FieldRef::Value, base)
                }
                PatternSpec::Select(alt, base) => {
                    let symbol = self.interner.get_or_intern(alt);
                    let base = self.pattern(base);
                    ExprKind::Select(symbol, base)
                }
                PatternSpec::Fact(index, key) => {
                    let predicate = PredicateId((*index as usize % self.predicates) as u32);
                    let key = self.pattern(key);
                    ExprKind::Fact(predicate, key)
                }
                PatternSpec::Or(branches) => ExprKind::Disjunction(
                    branches
                        .iter()
                        .map(|branch| self.pattern(branch))
                        .collect::<Vec<_>>()
                        .into(),
                ),
                PatternSpec::Subquery(spec) => ExprKind::Subquery(self.query(spec)),
            };
            self.push(kind)
        }
    }
}
