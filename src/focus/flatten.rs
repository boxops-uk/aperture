//! flatten — the typed query becomes the [`Plan`] the executor runs.
//!
//! The last front-end phase and the one the two halves of the system meet at
//! ([chapter 7]). It takes the typed tree and produces an ordered `[Generator]`
//! plus a `head: Project`, which is the fixed contract
//! ([chapter 4](../../../docs/04-executor.md)); everything after this point is the
//! executor's.
//!
//! Four things happen here, in this order, and the order is the design:
//!
//! 1. **Collect** the statements into generators. A statement is a fact pattern —
//!    `test.Foo {…}`, optionally bound to a variable by `X = test.Foo {…}` — and
//!    each becomes one loop level holding one register.
//! 2. **Safety.** Every variable a seek, residual or the head *reads* must be
//!    **captured** by some generator's key pattern. That is the whole of what
//!    correctness needs — see [`reorder`](crate::focus::reorder) for why it is not
//!    an ordering problem — and a query with an uncaptured variable is rejected
//!    (`reject/unbound-variable`), not answered.
//! 3. **Reorder**, which is the identity. Correct rather than a stub, for the same
//!    reason: any order that binds before it reads gives the same answer.
//! 4. **Sargeability**, walking each level's key fields *in the chosen order* and
//!    deciding, per field, whether it narrows the scan (a **seek**), is filled from
//!    a register bound at an outer level (a **splice**), or filters rows as they
//!    come (a **residual**). This is order-dependent — a variable being captured
//!    cannot seek, because it is an output — which is why it runs after the order
//!    is fixed rather than before.
//!
//! # What it does not do
//!
//! **Hoist nested generators.** A fact pattern is a generator wherever it appears,
//! so one written inside a key field or in the head would have to become a loop
//! level of its own. In the implemented subset that can only arise through a
//! fact-typed field or a head that is itself a fact pattern; both draw
//! `nyi/nested-generator`, and this is the seam hoisting lands at.
//!
//! **Reach through a fact reference.** A fact-typed key field holds a `FactId`, so
//! matching one against a bound row, or capturing one and reading *its* fields,
//! needs cross-fact navigation (`Access::Fetch`) and a fact-id splice — neither of
//! which the `Plan` IR has. `nyi/fact-field`. Getting this wrong silently is the
//! trap it exists to close: a register holds its own row's key bytes, which are not
//! the referenced fact's, and comparing them would give wrong answers rather than
//! an error.
//!
//! **Bind a computed value.** `X = 42`, `X = Y`, `X = Y.name` bind a variable to
//! something no generator produced. That is a *derived bind* and needs the value
//! slots Phase 6 adds (`nyi/value-bind`).
//!
//! **Match on a value.** A value lives in `entities`, and [I6] keeps `entities`
//! out of the scan loop, so `.value` may be projected but not matched
//! (`nyi/value-match`).
//!
//! **Bind a whole record key.** A stored key is its fields back to back with no
//! wrapper ([chapter 3]), so a record key is not *a* field and has no
//! [`FieldPath`]. A scalar key is one field and binds fine; a record key needs its
//! fields named (`nyi/whole-key`).
//!
//! Each of those is a corpus entry, so the promise is checked rather than
//! described ([`corpus`](crate::focus::corpus)).
//!
//! [chapter 7]: ../../../docs/07-compilation.md
//! [chapter 3]: ../../../docs/03-storage-model.md
//! [I6]: ../../../docs/invariants.md#i6

use crate::focus::{
    diag::{Code, Diagnostics},
    iter::Address,
    plan::{
        Access, FieldPath, Generator, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart,
    },
    reorder::{Deps, StmtDeps, reorder},
    schema::{LocalInterner, PredicateId, PredicateTy, Schema, Symbol},
    syntax::{Ast, ExprKind, FieldRef, Literal, NodeId, NodeSpan, QueryStmt},
    tuple::{MARK_RECORD, MARK_TERM, Value, put_i64, put_str},
};

/// Where a pattern's value lives when the plan runs.
///
/// The three cases are what a variable can be bound to, and they are not
/// interchangeable: a row is an identity (`Project::FactRef`), a field is bytes
/// inside the row's key (`Project::RegisterField`), and a value is in the other
/// column family (`Project::Value`, one point read).
#[derive(Debug, Clone)]
enum Slot {
    /// The whole row of a loop level — `X = test.Foo …`.
    Row {
        address: Address,
        predicate: PredicateId,
    },
    /// A key field of a row, reached by a path.
    Field {
        address: Address,
        path: FieldPath,
        ty: PredicateTy,
    },
    /// A row's value side. Projectable; never matched ([I6]).
    ///
    /// [I6]: ../../../docs/invariants.md#i6
    Value { address: Address, ty: PredicateTy },
}

/// One statement, as a generator-to-be — before an order is chosen, so before any
/// register is assigned.
#[derive(Debug, Clone)]
struct Gen {
    predicate: PredicateId,
    /// The key pattern node, which sargeability walks once the order is fixed.
    key: NodeId,
    /// The variable the whole row binds, from `X = test.Foo …`.
    row: Option<Symbol>,
    span: NodeSpan,
}

/// What flatten works out *before* an order is chosen: the generators, the
/// dependency graph over them, and the variables the head reads.
#[derive(Debug)]
struct Collected {
    stmts: Vec<Gen>,
    deps: Deps,
    /// The head's variables. Reads, always — a head projects, it never captures —
    /// and so the last thing the safety check has to account for.
    head_reads: Vec<Symbol>,
}

/// A constant a key field is matched against.
///
/// The two are different *only* in how they narrow: a whole value is an equality,
/// and a string prefix is a range — which is also why a prefix can be the last
/// thing in a seek but never the middle of one.
enum Const {
    Bytes(Vec<u8>),
    Prefix(Vec<u8>),
}

/// One statement's variable occurrences, gathered before any order is chosen.
#[derive(Debug, Default)]
struct Occurrences {
    /// Variables in a *capturable* position — a bare variable at a key field.
    /// Whether one is actually captured there depends on the order, so this is
    /// what a statement *can* bind, not what it does.
    captures: Vec<Symbol>,
    /// Variables it can only read: the base of an access chain.
    reads: Vec<Symbol>,
}

impl Occurrences {
    /// Deduplicated, so a variable named twice draws one diagnostic and appears
    /// once in the graph.
    fn capture(&mut self, var: Symbol) {
        if !self.captures.contains(&var) {
            self.captures.push(var);
        }
    }

    fn read(&mut self, var: Symbol) {
        if !self.reads.contains(&var) {
            self.reads.push(var);
        }
    }
}

/// The seek and residuals of one level, built field by field.
struct Level {
    parts: Vec<SeekKeyPart>,
    residuals: Vec<Residual>,
    /// Whether the seek prefix is still **contiguous from field 0**.
    ///
    /// A seek is a byte prefix of the stored key, so it can only be extended while
    /// every field so far has been fully determined. The first field that is not —
    /// a capture, a wildcard, an unmentioned field, a partly-given record, or a
    /// string prefix (which ends the prefix *after* itself) — closes it, and
    /// everything later filters instead.
    building: bool,
}

impl Level {
    fn new() -> Self {
        Self {
            parts: vec![],
            residuals: vec![],
            building: true,
        }
    }

    /// The finished seek: a plain byte prefix where every part is constant — which
    /// is the common case and needs no per-row work — and a composite where a
    /// register's bytes have to be spliced in each time the level is opened.
    fn seek_key(&self) -> SeekKey {
        if self
            .parts
            .iter()
            .all(|part| matches!(part, SeekKeyPart::Bytes(_)))
        {
            let mut bytes = vec![];
            for part in &self.parts {
                if let SeekKeyPart::Bytes(constant) = part {
                    bytes.extend_from_slice(constant);
                }
            }
            return SeekKey::Prefix(bytes.into());
        }

        SeekKey::Composite(self.parts.clone().into())
    }
}

/// Lower a typechecked query to a [`Plan`], reporting into `diagnostics`.
///
/// `None` means the query has no plan — every reason is reported by code, as
/// everywhere else in the front end. A caller decides validity by asking the sink,
/// not by the `Option`.
///
/// The query must have **typechecked cleanly**: flatten handles the implemented
/// subset, and every construct deferred at typecheck (disjunction, negation,
/// subqueries, union select, `never`, the hard half of `pattern = pattern`) has
/// already been reported by then. [`Compilation::plan`] enforces that ordering.
///
/// # Why it does not read the annotation table
///
/// Typecheck's side table holds **query-level** types (`Ty`, with variables), and a
/// plan needs **declared** ones (`PredicateTy`, which is what the codec decodes
/// against). Every type flatten puts in a plan therefore comes from the schema,
/// walked along the same path the plan will read at run time — the annotations are
/// what the *diagnostics* were built from, and re-deriving from the schema means a
/// projection cannot disagree with the bytes it decodes. Phase 6's derived binds
/// are the first thing that will need the table itself, since a computed value has
/// no declared type to look up.
///
/// [`Compilation::plan`]: crate::focus::compile::Compilation::plan
pub fn flatten(
    ast: &Ast,
    schema: &Schema,
    interner: &LocalInterner,
    diagnostics: &mut Diagnostics,
) -> Option<Plan> {
    flatten_ordered(ast, schema, interner, diagnostics, None)
}

/// Flatten with the loop order **given** rather than chosen.
///
/// Test-only, and the seam the reorderability property runs through: the claim
/// that ordering is a performance choice is only worth anything if the *same
/// query* can be run in every order and give the same rows. It is also what a real
/// [`reorder`](crate::focus::reorder::reorder) will hand back, so it is not a
/// second code path — [`flatten`] is this function with the identity.
///
/// `order` must be a permutation of `0..statements`; an order that reads a
/// variable before anything binds it is reported like any other unbound variable.
#[cfg(any(test, feature = "proptest"))]
pub fn flatten_in_order(
    ast: &Ast,
    schema: &Schema,
    interner: &LocalInterner,
    diagnostics: &mut Diagnostics,
    order: &[usize],
) -> Option<Plan> {
    flatten_ordered(ast, schema, interner, diagnostics, Some(order))
}

/// The statements' dependency graph, without building a plan — what
/// [`reorder`](crate::focus::reorder::reorder) is handed.
///
/// Test-only today. It is the natural shape for a `:plan`-style introspection
/// command to show, and Phase 6 needs it for the topological ordering derived binds
/// impose, but exporting it before either exists would be speculative.
#[cfg(any(test, feature = "proptest"))]
pub fn dependencies(
    ast: &Ast,
    schema: &Schema,
    interner: &LocalInterner,
    diagnostics: &mut Diagnostics,
) -> Option<Deps> {
    let mut flattener = Flattener {
        ast,
        schema,
        interner,
        diagnostics,
        bindings: vec![],
    };

    Some(flattener.collect()?.deps)
}

fn flatten_ordered(
    ast: &Ast,
    schema: &Schema,
    interner: &LocalInterner,
    diagnostics: &mut Diagnostics,
    order: Option<&[usize]>,
) -> Option<Plan> {
    let mark = diagnostics.len();
    let plan = flatten_reporting(ast, schema, interner, diagnostics, order);

    // **No plan without a reason.** `plan()` promises that `None` always comes with
    // a diagnostic, and several arms of the walk decline *quietly* on purpose —
    // because the shape they saw was already reported by an earlier pass. That makes
    // the promise a property of which passes ran, which is exactly the kind of claim
    // that rots: relaxing one narrowing check turns a quiet `None` into a silent
    // failure with an empty sink. Checked here, once, so every rejection test is
    // also a test of the promise.
    debug_assert!(
        plan.is_some() || diagnostics.len() > mark,
        "flatten declined to build a plan without reporting why"
    );

    plan
}

fn flatten_reporting(
    ast: &Ast,
    schema: &Schema,
    interner: &LocalInterner,
    diagnostics: &mut Diagnostics,
    order: Option<&[usize]>,
) -> Option<Plan> {
    let mut flattener = Flattener {
        ast,
        schema,
        interner,
        diagnostics,
        bindings: vec![],
    };

    let collected = flattener.collect()?;

    let chosen: Vec<usize> = match order {
        Some(given) => {
            assert_eq!(
                given.len(),
                collected.stmts.len(),
                "an order must name every statement"
            );
            given.to_vec()
        }
        None => reorder(&collected.deps).into_vec(),
    };

    // Over the *chosen* order, not the collection order: whether a variable is
    // bound before it is read is a property of the order that was picked, so this
    // is also the check on whatever `reorder` handed back.
    if !flattener.safe(&collected, &chosen) {
        return None;
    }

    flattener.emit(&collected.stmts, &chosen)
}

struct Flattener<'a> {
    ast: &'a Ast,
    schema: &'a Schema,
    interner: &'a LocalInterner,
    diagnostics: &'a mut Diagnostics,
    /// Variable → where its value lives, as the levels are emitted in order.
    ///
    /// Append-only, and searched from the back: a variable is bound once, at its
    /// first occurrence in the chosen order, and every later occurrence reads it.
    bindings: Vec<(Symbol, Slot)>,
}

impl Flattener<'_> {
    // ---- collect ------------------------------------------------------------

    /// Statements → generators, plus the dependency graph and the head's reads.
    ///
    /// Everything that does not depend on the order is decided here, which is what
    /// makes the later passes simple: a statement that generates nothing, a
    /// construct the plan cannot express, and a head that is not a value are all
    /// reported now. Reports **everything** it finds before giving up — a query can
    /// be wrong in several places, as it can in every other phase.
    fn collect(&mut self) -> Option<Collected> {
        let mark = self.diagnostics.len();
        let mut stmts: Vec<Gen> = vec![];

        for stmt in self.ast.query().body() {
            match stmt {
                QueryStmt::Implicit(node) => {
                    if let Some(generator) = self.generator(*node, None) {
                        stmts.push(generator);
                    }
                }

                QueryStmt::Bind(lhs, rhs) => {
                    // Typecheck accepts a bind only where the left side is a fresh
                    // variable or a wildcard, so this is the whole of what it can be.
                    let row = match self.ast.store().kind(*lhs) {
                        ExprKind::Var(symbol) => Some(*symbol),
                        _ => None,
                    };

                    if matches!(self.ast.store().kind(*rhs), ExprKind::Fact(..)) {
                        if let Some(generator) = self.generator(*rhs, row) {
                            stmts.push(generator);
                        }
                    } else {
                        self.report(
                            *rhs,
                            Code::NyiValueBind,
                            "binding a variable to a value no fact produced is not implemented \
                             yet; it needs a derived bind",
                        );
                    }
                }

                // Typecheck reports negation, so a query reaching flatten has none.
                QueryStmt::Negation(node) => {
                    self.report(
                        *node,
                        Code::RejectNotAGenerator,
                        "a statement has to match facts; this one matches nothing",
                    );
                }
            }
        }

        // The variable occurrences, per statement and in the head. A statement's
        // *row* variable is a capture like any other — it is bound by the level
        // running, which is what a seek splicing it depends on.
        let mut deps = Vec::with_capacity(stmts.len());

        for generator in &stmts {
            let mut occurrences = Occurrences::default();

            if let Some(row) = generator.row {
                occurrences.capture(row);
            }
            self.scan_key(generator.key, generator.predicate, &mut occurrences);

            deps.push(StmtDeps {
                captures: occurrences.captures.into(),
                reads: occurrences.reads.into(),
            });
        }

        let mut head = Occurrences::default();
        self.scan_head(*self.ast.query().head(), &mut head);

        if self.diagnostics.len() != mark {
            return None;
        }

        Some(Collected {
            stmts,
            deps: Deps::new(deps),
            head_reads: head.reads,
        })
    }

    /// One statement as a generator, or a report that it is not one.
    fn generator(&mut self, node: NodeId, row: Option<Symbol>) -> Option<Gen> {
        match self.ast.store().kind(node) {
            ExprKind::Fact(predicate, key) => Some(Gen {
                predicate: *predicate,
                key: *key,
                row,
                span: self.ast.store().span(node),
            }),
            _ => {
                self.report(
                    node,
                    Code::RejectNotAGenerator,
                    "a statement has to match facts; this one matches nothing",
                );
                None
            }
        }
    }

    /// Walk a key pattern for variable occurrences, reporting anything the plan
    /// cannot express.
    fn scan_key(&mut self, node: NodeId, predicate: PredicateId, occurrences: &mut Occurrences) {
        let Some(key_ty) = self.schema.get(predicate).map(|p| p.key().ty.clone()) else {
            return;
        };

        match (&key_ty, self.ast.store().kind(node)) {
            (PredicateTy::Record(_), ExprKind::Record(_)) => {
                self.scan_field(node, &key_ty, occurrences);
            }
            // A whole-predicate scan.
            (PredicateTy::Record(_), ExprKind::Wildcard) => {}
            (PredicateTy::Record(_), _) => self.report(
                node,
                Code::NyiWholeKey,
                "binding a fact's whole key to one variable is not implemented yet; \
                 a stored key is its fields, so name the fields instead",
            ),
            // A scalar key is one field, and the pattern is that field's.
            (scalar, _) => self.scan_field(node, scalar, occurrences),
        }
    }

    fn scan_field(&mut self, node: NodeId, ty: &PredicateTy, occurrences: &mut Occurrences) {
        match self.ast.store().kind(node) {
            ExprKind::Wildcard | ExprKind::Lit(_) | ExprKind::Prefix(_) => {}

            ExprKind::Var(symbol) => {
                if self.fact_field(node, ty) {
                    return;
                }
                occurrences.capture(*symbol);
            }

            ExprKind::Record(fields) => {
                let PredicateTy::Record(field_tys) = ty else {
                    return;
                };

                for (name, field_ty) in field_tys.iter() {
                    if let Some(pattern) = field_pattern(fields, Symbol::Schema(*name)) {
                        self.scan_field(pattern, field_ty, occurrences);
                    }
                }
            }

            ExprKind::Access(FieldRef::Value, _) => self.report(
                node,
                Code::NyiValueMatch,
                "matching on a fact's value is not implemented yet; a value is fetched \
                 per row, and residuals run inside the scan",
            ),

            ExprKind::Access(FieldRef::Key(_), _) | ExprKind::Select(..) => {
                if self.fact_field(node, ty) {
                    return;
                }
                match self.ast.store().kind(self.chain_root(node)) {
                    ExprKind::Var(symbol) => occurrences.read(*symbol),
                    ExprKind::Fact(..) => self.nested_generator(node),
                    _ => {}
                }
            }

            ExprKind::Fact(..) => self.nested_generator(node),

            // Deferred constructs, all of which typecheck has already reported.
            ExprKind::Never
            | ExprKind::Disjunction(_)
            | ExprKind::Subquery(_)
            | ExprKind::Error => {}
        }
    }

    /// Whether `ty` is a fact reference, reported if so.
    ///
    /// A fact-typed field holds a `FactId`, and a register holds the row it came
    /// from — so matching one against a bound row would compare a key against an id,
    /// and capturing one and reading its fields needs a fetch the plan cannot
    /// express. Both are the same missing feature.
    fn fact_field(&mut self, node: NodeId, ty: &PredicateTy) -> bool {
        if !matches!(ty, PredicateTy::Fact(_)) {
            return false;
        }

        self.report_fact_field(node);
        true
    }

    fn report_fact_field(&mut self, node: NodeId) {
        self.report(
            node,
            Code::NyiFactField,
            "matching or capturing a field that holds a fact reference is not \
             implemented yet; it needs cross-fact navigation",
        );
    }

    fn nested_generator(&mut self, node: NodeId) {
        self.report(
            node,
            Code::NyiNestedGenerator,
            "a fact pattern here would be a generator of its own, which is not \
             implemented yet; write it as its own statement",
        );
    }

    /// Walk the head for the variables it reads, reporting anything unprojectable.
    ///
    /// A head never captures: it is read after every generator has run, which is
    /// also why the safety check accounts for it last.
    fn scan_head(&mut self, node: NodeId, occurrences: &mut Occurrences) {
        match self.ast.store().kind(node) {
            ExprKind::Lit(_) => {}

            ExprKind::Var(symbol) => occurrences.read(*symbol),

            ExprKind::Record(fields) => {
                for (_, value) in fields.iter() {
                    self.scan_head(*value, occurrences);
                }
            }

            ExprKind::Access(..) | ExprKind::Select(..) => {
                match self.ast.store().kind(self.chain_root(node)) {
                    ExprKind::Var(symbol) => occurrences.read(*symbol),
                    ExprKind::Fact(..) => self.nested_generator(node),
                    _ => self.not_projectable(node),
                }
            }

            ExprKind::Fact(..) => self.nested_generator(node),

            // A prefix is a pattern, not a value; a wildcard was rejected at
            // typecheck; the rest are deferred constructs it also reported.
            _ => self.not_projectable(node),
        }
    }

    fn not_projectable(&mut self, node: NodeId) {
        self.report(
            node,
            Code::RejectNotProjectable,
            "this cannot be projected: a head has to name a value",
        );
    }

    /// The pattern an access chain reads from — `X` in `X.a.b?`.
    fn chain_root(&self, node: NodeId) -> NodeId {
        let mut current = node;

        loop {
            match self.ast.store().kind(current) {
                ExprKind::Access(_, base) | ExprKind::Select(_, base) => current = *base,
                _ => return current,
            }
        }
    }

    // ---- safety -------------------------------------------------------------

    /// **Range restriction, over the chosen order.** Every variable a statement or
    /// the head reads must have been captured by then.
    ///
    /// One check covers both ways it can fail: a variable nothing captures at all,
    /// and one captured only *after* it is read. They are the same fault to a
    /// reader — nothing has bound it yet — and the second is what makes this the
    /// check on the order rather than on the query.
    fn safe(&mut self, collected: &Collected, order: &[usize]) -> bool {
        let mut bound: Vec<Symbol> = vec![];
        let mut ok = true;

        for &stmt in order {
            let (Some(deps), Some(generator)) =
                (collected.deps.stmt(stmt), collected.stmts.get(stmt))
            else {
                return false;
            };

            for read in deps.reads.iter() {
                if !bound.contains(read) {
                    let at = generator.span.clone();
                    self.unbound(*read, at);
                    ok = false;
                }
            }

            bound.extend(deps.captures.iter().copied());
        }

        for read in &collected.head_reads {
            if !bound.contains(read) {
                let at = self.ast.store().span(*self.ast.query().head());
                self.unbound(*read, at);
                ok = false;
            }
        }

        ok
    }

    fn unbound(&mut self, var: Symbol, at: NodeSpan) {
        let name = self.name(var).to_owned();
        self.diagnostics.error(
            Code::RejectUnboundVariable,
            format!(
                "nothing binds `{name}`: every variable has to be captured by a fact \
                 pattern's key"
            ),
            at,
        );
    }

    // ---- emit ---------------------------------------------------------------

    /// Walk the statements in `order`, assigning a register per level and deciding
    /// each key field's fate, then project the head.
    fn emit(&mut self, stmts: &[Gen], order: &[usize]) -> Option<Plan> {
        let mark = self.diagnostics.len();
        let mut body = Vec::with_capacity(order.len());

        for (level, &stmt) in order.iter().enumerate() {
            let generator = stmts.get(stmt)?;
            let address = Address::new(level);
            let key_ty = self.schema.get(generator.predicate)?.key().ty.clone();

            let mut current = Level::new();
            self.key(generator.key, &key_ty, address, &mut current);

            // After the key: `X = test.Foo {id = X}` cannot typecheck, so nothing in
            // a level's own key can read the row it binds.
            if let Some(row) = generator.row {
                self.bindings.push((
                    row,
                    Slot::Row {
                        address,
                        predicate: generator.predicate,
                    },
                ));
            }

            body.push(Generator {
                access: Access {
                    predicate_id: generator.predicate,
                    seek_key: current.seek_key(),
                },
                binds: Box::new([address]),
                residuals: current.residuals.into(),
            });
        }

        let head = self.project(*self.ast.query().head());

        if self.diagnostics.len() != mark {
            return None;
        }

        Some(Plan {
            nvars: order.len(),
            body: body.into(),
            head: head?,
        })
    }

    /// A level's key pattern, field by field in **declared order** — which is
    /// encoding order, and so the order a seek prefix has to be built in.
    fn key(&mut self, node: NodeId, key_ty: &PredicateTy, address: Address, level: &mut Level) {
        match (key_ty, self.ast.store().kind(node)) {
            (PredicateTy::Record(field_tys), ExprKind::Record(fields)) => {
                let fields = fields.clone();

                for (idx, (name, field_ty)) in field_tys.clone().iter().enumerate() {
                    match field_pattern(&fields, Symbol::Schema(*name)) {
                        Some(pattern) => {
                            self.field(pattern, field_ty, address, &FieldPath::field(idx), level);
                        }
                        // An unmentioned field is a wildcard, so it constrains
                        // nothing — and closes the seek prefix.
                        None => level.building = false,
                    }
                }
            }

            // A wildcard key, or a shape `collect` has already reported.
            (PredicateTy::Record(_), _) => {}

            (scalar, _) => self.field(node, scalar, address, &FieldPath::field(0), level),
        }
    }

    /// One key field: seek, splice, residual, or capture.
    fn field(
        &mut self,
        node: NodeId,
        ty: &PredicateTy,
        address: Address,
        path: &FieldPath,
        level: &mut Level,
    ) {
        match self.ast.store().kind(node) {
            ExprKind::Wildcard => level.building = false,

            ExprKind::Var(symbol) => match self.lookup(*symbol) {
                // First occurrence in this order: the field is an *output*, so it
                // cannot narrow the scan.
                None => {
                    level.building = false;
                    self.bindings.push((
                        *symbol,
                        Slot::Field {
                            address,
                            path: path.clone(),
                            ty: ty.clone(),
                        },
                    ));
                }
                Some(slot) => self.matched(node, &slot, address, path, level),
            },

            ExprKind::Access(..) | ExprKind::Select(..) => {
                if let Some(slot) = self.resolve(node) {
                    self.matched(node, &slot, address, path, level);
                }
            }

            // A literal, a string prefix, or a record of them.
            _ => match self.constant(node, ty) {
                Some(Const::Bytes(bytes)) => {
                    if level.building {
                        level.parts.push(SeekKeyPart::Bytes(bytes.into()));
                    } else {
                        level.residuals.push(Residual {
                            path: path.clone(),
                            op: ResidualOp::EqConst(bytes.into()),
                        });
                    }
                }

                // A prefix narrows to a *range*, so it can end a seek but nothing
                // may follow it in one: the bytes after it are not the field's.
                Some(Const::Prefix(bytes)) => {
                    if level.building {
                        level.parts.push(SeekKeyPart::Bytes(bytes.into()));
                        level.building = false;
                    } else {
                        level.residuals.push(Residual {
                            path: path.clone(),
                            op: ResidualOp::Prefix(bytes.into()),
                        });
                    }
                }

                // Not fully determined: a record giving only some of its fields.
                // Those become residuals and captures one step deeper, and the
                // field itself cannot narrow the scan.
                None => {
                    level.building = false;

                    if let (ExprKind::Record(fields), PredicateTy::Record(field_tys)) =
                        (self.ast.store().kind(node), ty)
                    {
                        let fields = fields.clone();

                        for (idx, (name, field_ty)) in field_tys.clone().iter().enumerate() {
                            if let Some(pattern) = field_pattern(&fields, Symbol::Schema(*name)) {
                                self.field(pattern, field_ty, address, &path.then(idx), level);
                            }
                        }
                    }
                }
            },
        }
    }

    /// A field matched against something already bound: a splice while the seek is
    /// still being built, a residual once it is closed.
    fn matched(
        &mut self,
        node: NodeId,
        slot: &Slot,
        address: Address,
        path: &FieldPath,
        level: &mut Level,
    ) {
        match slot {
            Slot::Field {
                address: from,
                path: at,
                ..
            } => {
                // The same register is *this* row: an intra-row equality, which
                // needs a same-row residual the executor does not have. Rejected
                // for Phase 4 rather than adding an operator nothing else uses
                // ([open decisions](../../../docs/open-decisions.md)).
                if *from == address {
                    self.report(
                        node,
                        Code::NyiRepeatedVariable,
                        "matching one variable against two fields of the same fact is not \
                         implemented yet; it needs a same-row equality",
                    );
                    return;
                }

                if level.building {
                    level.parts.push(SeekKeyPart::RegisterField {
                        address: *from,
                        path: at.clone(),
                    });
                } else {
                    level.residuals.push(Residual {
                        path: path.clone(),
                        op: ResidualOp::EqRegisterField {
                            address: *from,
                            path: at.clone(),
                        },
                    });
                }
            }

            // Both are reported by `collect`, which sees the field's declared type
            // before any of this; reported here too so that no path can decline to
            // build a plan without saying why.
            Slot::Row { .. } => self.report_fact_field(node),
            Slot::Value { .. } => self.report(
                node,
                Code::NyiValueMatch,
                "matching on a fact's value is not implemented yet",
            ),
        }
    }

    /// The bytes a pattern determines, if it determines all of them.
    ///
    /// `None` is "not a constant" — a variable, a wildcard, or a record giving only
    /// part of itself. A record is only constant when the *type's* every field is
    /// given, because the encoding is positional: a missing field would leave the
    /// bytes of the ones after it in the wrong place.
    fn constant(&self, node: NodeId, ty: &PredicateTy) -> Option<Const> {
        match (self.ast.store().kind(node), ty) {
            (ExprKind::Lit(Literal::Int(value)), PredicateTy::Int) => {
                let mut out = vec![];
                put_i64(&mut out, *value);
                Some(Const::Bytes(out))
            }

            (ExprKind::Lit(Literal::Str(text)), PredicateTy::Str) => {
                let mut out = vec![];
                put_str(&mut out, self.interner.try_resolve(*text)?);
                Some(Const::Bytes(out))
            }

            (ExprKind::Prefix(text), PredicateTy::Str) => {
                let mut out = vec![];
                put_str(&mut out, self.interner.try_resolve(*text)?);
                // A string's encoding without its terminator is exactly the bytes
                // every string starting with it begins with, which is what makes a
                // prefix pattern a range scan ([I1]).
                out.pop()?;
                Some(Const::Prefix(out))
            }

            (ExprKind::Record(fields), PredicateTy::Record(field_tys)) => {
                let mut out = vec![MARK_RECORD];

                for (name, field_ty) in field_tys.iter() {
                    let pattern = field_pattern(fields, Symbol::Schema(*name))?;

                    match self.constant(pattern, field_ty)? {
                        Const::Bytes(bytes) => out.extend_from_slice(&bytes),
                        // A prefix cannot sit inside a record: the fields after it
                        // and the terminator follow, so the bytes would not be a
                        // prefix of anything.
                        Const::Prefix(_) => return None,
                    }
                }

                out.push(MARK_TERM);
                Some(Const::Bytes(out))
            }

            _ => None,
        }
    }

    /// Where a read pattern's value lives — a variable, or a chain of field reads
    /// from one.
    fn resolve(&mut self, node: NodeId) -> Option<Slot> {
        match self.ast.store().kind(node) {
            ExprKind::Var(symbol) => self.lookup(*symbol),

            ExprKind::Access(FieldRef::Value, base) => {
                // Only a *row* has a value side. A fact-typed field denotes a row
                // too, and reading its value is a fetch — but a variable cannot be
                // bound to one yet (`collect` reports `nyi/fact-field` first), so
                // this returns quietly. Whoever allows that capture owes this arm a
                // diagnostic; the `debug_assert` in `flatten_ordered` is what will
                // say so.
                let Slot::Row { address, predicate } = self.resolve(*base)? else {
                    return None;
                };
                let ty = self.schema.get(predicate)?.value()?.ty.clone();

                Some(Slot::Value { address, ty })
            }

            ExprKind::Access(FieldRef::Key(name), base) => {
                let name = *name;

                match self.resolve(*base)? {
                    Slot::Row { address, predicate } => {
                        let key_ty = self.schema.get(predicate)?.key().ty.clone();
                        let (idx, ty) = field_of(&key_ty, name)?;

                        Some(Slot::Field {
                            address,
                            path: FieldPath::field(idx),
                            ty,
                        })
                    }

                    Slot::Field { address, path, ty } => match ty {
                        PredicateTy::Record(_) => {
                            let (idx, field_ty) = field_of(&ty, name)?;

                            Some(Slot::Field {
                                address,
                                path: path.then(idx),
                                ty: field_ty,
                            })
                        }
                        // Reading a field *of* a referenced fact is a fetch.
                        PredicateTy::Fact(_) => {
                            self.report(
                                node,
                                Code::NyiFactField,
                                "reading a field through a fact reference is not implemented \
                                 yet; it needs cross-fact navigation",
                            );
                            None
                        }
                        _ => None,
                    },

                    // Reading a field of a scalar value: typecheck rejects it, since
                    // a value's type has no fields.
                    Slot::Value { .. } => None,
                }
            }

            _ => None,
        }
    }

    /// The head as a projection.
    ///
    /// Quiet on failure: every shape it can decline was reported by
    /// [`scan_head`](Self::scan_head), and every unbound variable by
    /// [`safe`](Self::safe), so reporting again here would say it twice.
    fn project(&mut self, node: NodeId) -> Option<Project> {
        match self.ast.store().kind(node) {
            ExprKind::Lit(Literal::Int(value)) => Some(Project::Lit(Value::Int(*value))),

            ExprKind::Lit(Literal::Str(text)) => Some(Project::Lit(Value::Str(
                self.interner.try_resolve(*text)?.to_owned(),
            ))),

            ExprKind::Record(fields) => {
                let fields = fields.clone();
                let mut out = Vec::with_capacity(fields.len());

                for (name, value) in fields.iter() {
                    out.push((*name, self.project(*value)?));
                }

                Some(Project::Record(out.into()))
            }

            ExprKind::Var(_) | ExprKind::Access(..) => match self.resolve(node)? {
                // A variable bound to a whole row projects its identity: the row
                // itself is not bytes in the register, the fact id is.
                Slot::Row { address, .. } => Some(Project::FactRef(address)),
                Slot::Field { address, path, ty } => {
                    Some(Project::RegisterField { address, path, ty })
                }
                Slot::Value { address, ty } => Some(Project::Value { address, ty }),
            },

            _ => None,
        }
    }

    // ---- state --------------------------------------------------------------

    fn lookup(&self, symbol: Symbol) -> Option<Slot> {
        self.bindings
            .iter()
            .rev()
            .find(|(name, _)| *name == symbol)
            .map(|(_, slot)| slot.clone())
    }

    fn name(&self, symbol: Symbol) -> &str {
        self.interner.try_resolve(symbol).unwrap_or("?")
    }

    fn report(&mut self, node: NodeId, code: Code, message: impl Into<String>) {
        self.diagnostics
            .error(code, message, self.ast.store().span(node));
    }
}

/// The pattern a record gives for `name`, if it gives one. An omitted field is a
/// wildcard ([chapter 7](../../../docs/07-compilation.md)).
fn field_pattern(fields: &[(Symbol, NodeId)], name: Symbol) -> Option<NodeId> {
    fields
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, node)| *node)
}

/// A record type's field by name, with its position — which **is** its position in
/// the encoding, since a record's fields are encoded in declared order.
fn field_of(ty: &PredicateTy, name: Symbol) -> Option<(usize, PredicateTy)> {
    let PredicateTy::Record(fields) = ty else {
        return None;
    };

    fields
        .iter()
        .enumerate()
        .find(|(_, (field, _))| Symbol::Schema(*field) == name)
        .map(|(idx, (_, field_ty))| (idx, field_ty.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{
        corpus,
        cst::CstNode,
        fixtures::{collect_rows, compose, i64_field, str_field},
        lower::lower,
        mem_store::MemStore,
        parse::parse,
        plan::{Project, Residual, ResidualOp, SeekKey, SeekKeyPart},
        tuple::Value,
        ty,
    };

    // ---- driving the front end ---------------------------------------------

    struct Flattened {
        plan: Option<Plan>,
        diagnostics: Diagnostics,
        interner: LocalInterner,
    }

    impl Flattened {
        fn codes(&self) -> Vec<&str> {
            self.diagnostics.codes().collect()
        }

        /// The plan, insisting the front end was clean — what a test asserting a
        /// *shape* wants, since a missing plan and a wrong plan should not read the
        /// same way.
        fn plan(&self) -> &Plan {
            assert!(
                self.codes().is_empty(),
                "expected a plan, got {:?}",
                self.codes()
            );
            self.plan.as_ref().expect("a plan")
        }
    }

    /// Run `parse → lower → typecheck → flatten` over the corpus schema.
    ///
    /// The phases before flatten must be clean: flatten only ever runs on a query
    /// that typechecked, so a test whose source does not is testing something else.
    fn flatten_source(source: &str, order: Option<&[usize]>) -> Flattened {
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
        let _typed = ty::check(&ast, &schema, &interner, &mut diagnostics);
        assert!(
            !diagnostics.has_errors(),
            "{source:?} must typecheck before flatten sees it: {:?}",
            diagnostics.codes().collect::<Vec<_>>()
        );

        let plan = match order {
            None => flatten(&ast, &schema, &interner, &mut diagnostics),
            Some(order) => flatten_in_order(&ast, &schema, &interner, &mut diagnostics, order),
        };

        Flattened {
            plan,
            diagnostics,
            interner,
        }
    }

    fn compile(source: &str) -> Flattened {
        flatten_source(source, None)
    }

    fn compile_in_order(source: &str, order: &[usize]) -> Flattened {
        flatten_source(source, Some(order))
    }

    /// The statements' dependency graph, as flatten builds it.
    fn deps_of(source: &str) -> Deps {
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
        let _typed = ty::check(&ast, &schema, &interner, &mut diagnostics);
        assert!(!diagnostics.has_errors(), "{source:?} must typecheck");

        dependencies(&ast, &schema, &interner, &mut diagnostics).expect("a collectable query")
    }

    // ---- rendering a plan --------------------------------------------------

    /// A plan as one line per level plus its head, so a test states the shape it
    /// means rather than matching a tree of enums.
    ///
    /// Constant bytes render as `k`: *where* a constant went is what these tests
    /// are about, and the bytes themselves are asserted structurally by the few
    /// tests that are about the encoding.
    fn describe(plan: &Plan, interner: &LocalInterner) -> String {
        let schema = corpus::schema();
        let mut out = vec![];

        for (level, generator) in plan.body.iter().enumerate() {
            let name = schema
                .get(generator.access.predicate_id)
                .and_then(|p| p.name())
                .unwrap_or("?")
                .to_owned();

            let access = match &generator.access.seek_key {
                SeekKey::Prefix(bytes) if bytes.is_empty() => "scan".to_owned(),
                SeekKey::Prefix(_) => "seek[k]".to_owned(),
                SeekKey::Composite(parts) => format!(
                    "seek[{}]",
                    parts
                        .iter()
                        .map(|part| match part {
                            SeekKeyPart::Bytes(_) => "k".to_owned(),
                            SeekKeyPart::RegisterField { address, path } => {
                                format!("{address}.{path}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            };

            let residuals = generator
                .residuals
                .iter()
                .map(|Residual { path, op }| match op {
                    ResidualOp::EqConst(_) => format!("{path} == k"),
                    ResidualOp::Prefix(_) => format!("{path} ^= k"),
                    ResidualOp::EqRegisterField { address, path: at } => {
                        format!("{path} == {address}.{at}")
                    }
                })
                .collect::<Vec<_>>();

            let residuals = if residuals.is_empty() {
                String::new()
            } else {
                format!(" where {}", residuals.join(" and "))
            };

            let binds = generator
                .binds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");

            out.push(format!("{binds} <- {name} {access}{residuals}"));
            let _ = level;
        }

        out.push(format!("head {}", project(&plan.head, interner)));
        out.join("\n")
    }

    fn project(p: &Project, interner: &LocalInterner) -> String {
        match p {
            Project::Lit(Value::Int(n)) => n.to_string(),
            Project::Lit(Value::Str(s)) => format!("{s:?}"),
            Project::Lit(other) => format!("{other:?}"),
            Project::FactRef(address) => address.to_string(),
            Project::RegisterField { address, path, ty } => {
                format!("{address}.{path}:{}", render_ty(ty))
            }
            Project::Value { address, ty } => format!("{address}.value:{}", render_ty(ty)),
            Project::Record(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(name, field)| format!(
                        "{} = {}",
                        interner.try_resolve(*name).unwrap_or("?"),
                        project(field, interner)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn render_ty(ty: &PredicateTy) -> String {
        match ty {
            PredicateTy::Int => "int".to_owned(),
            PredicateTy::Str => "str".to_owned(),
            PredicateTy::Fact(p) => format!("fact({})", p.0),
            PredicateTy::Record(fields) => format!("{{{} fields}}", fields.len()),
        }
    }

    fn lines(ls: &[&str]) -> String {
        ls.join("\n")
    }

    /// The shape of `source`'s plan.
    fn shape(source: &str) -> String {
        let flattened = compile(source);
        describe(flattened.plan(), &flattened.interner)
    }

    // ---- what a generator is ------------------------------------------------

    /// A whole-predicate scan binding the row, which is what `X = test.Foo _` is:
    /// one level, one register, no narrowing, and a head that projects the row's
    /// identity rather than any of its bytes.
    #[test]
    fn a_scan_binds_the_whole_row() {
        let flattened = compile("X where X = test.Foo _");
        let plan = flattened.plan();

        assert_eq!(plan.nvars, 1);
        assert_eq!(plan.body.len(), 1);
        assert_eq!(plan.body[0].binds.as_ref(), [Address::new(0)]);
        assert_eq!(
            describe(plan, &flattened.interner),
            lines(&["r0 <- test.Foo scan", "head r0"])
        );
    }

    /// A variable in a key field is *captured* there: it names the field, and the
    /// head reads it by path.
    #[test]
    fn a_key_field_pattern_captures_the_field() {
        assert_eq!(
            shape("X where test.Foo {name = X}"),
            lines(&["r0 <- test.Foo scan", "head r0.1:str"]),
            "`name` is field 1 of `{{id, name}}` — fields are sorted by name",
        );
    }

    /// Reading a field of a bound row, and reading its value side, are different
    /// projections: one is bytes in the register, the other a point read of
    /// `entities`.
    #[test]
    fn the_head_projects_fields_values_and_records() {
        assert_eq!(
            shape("X.name where X = test.Foo _"),
            lines(&["r0 <- test.Foo scan", "head r0.1:str"])
        );
        assert_eq!(
            shape("X.value where X = test.Foo _"),
            lines(&["r0 <- test.Foo scan", "head r0.value:str"])
        );
        assert_eq!(
            shape("{a = X, b = Y} where test.Foo {name = X, id = Y}"),
            lines(&["r0 <- test.Foo scan", "head {a = r0.1:str, b = r0.0:int}"]),
            "record fields are sorted by name, in the head as everywhere",
        );
        assert_eq!(
            shape("42 where test.Foo _"),
            lines(&["r0 <- test.Foo scan", "head 42"]),
            "a literal head is a constant row",
        );
    }

    /// A capture inside a nested record is reached by a path, not by a flat index —
    /// the case the `Plan` IR grew [`FieldPath`] for.
    #[test]
    fn a_nested_capture_is_projected_through_a_path() {
        assert_eq!(
            shape("X where test.Nested {outer = {inner = X}}"),
            lines(&["r0 <- test.Nested scan", "head r0.0.0:int"])
        );
        // A record-typed field captured whole keeps its own wrapper, so it decodes
        // as the record it is.
        assert_eq!(
            shape("X where test.Nested {outer = X}"),
            lines(&["r0 <- test.Nested scan", "head r0.0:{1 fields}"])
        );
    }

    // ---- sargeability -------------------------------------------------------

    /// A constant in the *leading* key field narrows the scan, and the bytes it
    /// narrows with are the field's encoding — which is what makes the narrowing a
    /// prefix scan at all ([I1]).
    ///
    /// [I1]: ../../../docs/invariants.md#i1
    #[test]
    fn a_leading_constant_becomes_a_seek_prefix() {
        let flattened = compile("X where X = test.Foo {id = 1}");
        let plan = flattened.plan();

        assert_eq!(
            describe(plan, &flattened.interner),
            lines(&["r0 <- test.Foo seek[k]", "head r0"])
        );
        match &plan.body[0].access.seek_key {
            SeekKey::Prefix(bytes) => assert_eq!(bytes.as_ref(), i64_field(1).as_slice()),
            other => panic!("expected a constant prefix, got {other:?}"),
        }
    }

    /// A scalar key is one field, so a constant against it is the whole seek.
    #[test]
    fn a_scalar_key_constant_is_the_whole_seek() {
        let flattened = compile("X where X = test.Count -42");

        match &flattened.plan().body[0].access.seek_key {
            SeekKey::Prefix(bytes) => assert_eq!(bytes.as_ref(), i64_field(-42).as_slice()),
            other => panic!("expected a constant prefix, got {other:?}"),
        }
    }

    /// A constant *after* an unnarrowable field cannot extend the prefix, so it
    /// filters instead. `id` is field 0 and is captured — an output — so the scan
    /// starts at the predicate and `name` is checked per row.
    #[test]
    fn a_constant_after_a_capture_becomes_a_residual() {
        let flattened = compile("X where test.Foo {id = X, name = \"a\"}");
        let plan = flattened.plan();

        assert_eq!(
            describe(plan, &flattened.interner),
            lines(&["r0 <- test.Foo scan where 1 == k", "head r0.0:int"])
        );
        match &plan.body[0].residuals[0].op {
            ResidualOp::EqConst(bytes) => assert_eq!(bytes.as_ref(), str_field("a").as_slice()),
            other => panic!("expected a constant residual, got {other:?}"),
        }
    }

    /// A string prefix in the leading field is a *seek*: the encoded prefix of a
    /// string is a byte prefix of every string that starts with it, so the range
    /// scan is exactly the match ([I1]). The terminator is what it drops — a
    /// terminated string would be the equality, not the prefix.
    #[test]
    fn a_string_prefix_narrows_the_scan() {
        let flattened = compile("X where X = test.Name \"abc\"..");
        let plan = flattened.plan();

        assert_eq!(
            describe(plan, &flattened.interner),
            lines(&["r0 <- test.Name seek[k]", "head r0"])
        );

        let mut expected = str_field("abc");
        expected.pop().expect("a terminated string");
        match &plan.body[0].access.seek_key {
            SeekKey::Prefix(bytes) => assert_eq!(bytes.as_ref(), expected.as_slice()),
            other => panic!("expected a prefix seek, got {other:?}"),
        }
    }

    /// Elsewhere a prefix has to filter, and does so as a prefix rather than an
    /// equality.
    #[test]
    fn a_string_prefix_after_a_capture_is_a_prefix_residual() {
        let flattened = compile("X where test.Foo {id = X, name = \"a\"..}");
        let plan = flattened.plan();

        assert_eq!(
            describe(plan, &flattened.interner),
            lines(&["r0 <- test.Foo scan where 1 ^= k", "head r0.0:int"])
        );

        let mut expected = str_field("a");
        expected.pop().expect("a terminated string");
        match &plan.body[0].residuals[0].op {
            ResidualOp::Prefix(bytes) => assert_eq!(bytes.as_ref(), expected.as_slice()),
            other => panic!("expected a prefix residual, got {other:?}"),
        }
    }

    /// A variable bound at an outer level is an *input*, so it splices into the
    /// seek — the join the storage model is built for.
    #[test]
    fn a_bound_variable_in_the_leading_field_splices_into_the_seek() {
        assert_eq!(
            shape("X where test.Edge {from = X, to = Y}; test.Node {id = Y}"),
            lines(&[
                "r0 <- test.Edge scan",
                "r1 <- test.Node seek[r0.1]",
                "head r0.0:int",
            ])
        );
    }

    /// The same variable, but not in the leading field: nothing narrows the scan,
    /// so the join becomes a filter.
    #[test]
    fn a_bound_variable_after_an_open_field_becomes_a_residual() {
        assert_eq!(
            shape("X where test.Edge {from = X, to = Y}; test.Edge {to = Y}"),
            lines(&[
                "r0 <- test.Edge scan",
                "r1 <- test.Edge scan where 1 == r0.1",
                "head r0.0:int",
            ])
        );
    }

    /// A field *read* — `Y.name` — is an input like any other bound value, and
    /// splices the field it names.
    #[test]
    fn a_field_read_splices_the_field_it_names() {
        assert_eq!(
            shape("Y where Y = test.Foo _; test.Name Y.name"),
            lines(&[
                "r0 <- test.Foo scan",
                "r1 <- test.Name seek[r0.1]",
                "head r0",
            ])
        );
    }

    /// **Sargeability is order-dependent, and that is the whole reason it runs
    /// after the order is chosen.**
    ///
    /// The same two statements, written the other way round: whichever comes first
    /// *captures* the shared variable, and the other one gets to use it. One order
    /// yields a seek, the other a residual — different plans, and (below) the same
    /// rows.
    #[test]
    fn which_statement_comes_first_decides_seek_or_residual() {
        assert_eq!(
            shape("X where test.Edge {from = X, to = Y}; test.Node {id = Y}"),
            lines(&[
                "r0 <- test.Edge scan",
                "r1 <- test.Node seek[r0.1]",
                "head r0.0:int",
            ])
        );
        assert_eq!(
            shape("X where test.Node {id = Y}; test.Edge {from = X, to = Y}"),
            lines(&[
                "r0 <- test.Node scan",
                "r1 <- test.Edge scan where 1 == r0.0",
                "head r1.0:int",
            ]),
            "`from = X` is a capture, so it cannot narrow the scan; `to = Y` filters",
        );
    }

    /// Reading one bound variable twice in a row is fine — it is *capturing* twice
    /// that is rejected. Both fields are inputs, so the whole key is determined and
    /// the seek becomes a point match rather than a scan with a filter.
    #[test]
    fn a_bound_variable_may_be_read_twice_in_one_row() {
        assert_eq!(
            shape("X where test.Node {id = X}; test.Edge {from = X, to = X}"),
            lines(&[
                "r0 <- test.Node scan",
                "r1 <- test.Edge seek[r0.0 r0.0]",
                "head r0.0:int",
            ])
        );
    }

    // ---- safety, and the four rejections ------------------------------------

    /// **Range restriction.** A variable no generator captures has no values to
    /// range over, so the query is rejected rather than answered.
    ///
    /// Only where typecheck has not already spoken: *reading a field* of an
    /// uncaptured variable (`X.name where …`) is `reject/unresolved-access`, because
    /// there is no type to read the field from — the earlier and more specific
    /// diagnostic for the same underlying mistake.
    #[test]
    fn an_uncaptured_variable_is_rejected() {
        for source in [
            "X where test.Foo _",
            "X where test.Foo {id = Y}",
            "{a = X} where test.Foo {id = Y}",
        ] {
            let flattened = compile(source);
            assert_eq!(flattened.codes(), ["reject/unbound-variable"], "{source:?}");
            assert!(flattened.plan.is_none());
        }
    }

    /// **The Phase 4 decision on intra-row repeats: rejected.**
    ///
    /// `Edge {from = X, to = X}` constrains two fields of the *same* row to be
    /// equal, which needs a same-row `ResidualOp::EqField` — distinct from the
    /// cross-level `EqRegisterField`, because there is no outer register to compare
    /// against. Rather than add an operator the executor has no other use for yet,
    /// the pattern is rejected, with the diagnostic saying what to write instead
    /// ([open decisions](../../../docs/open-decisions.md)).
    #[test]
    fn an_intra_row_repeat_is_rejected() {
        for source in [
            "X where test.Edge {from = X, to = X}",
            "X where test.Wide {outer = {extra = X, inner = X}}",
        ] {
            let flattened = compile(source);
            assert_eq!(flattened.codes(), ["nyi/repeated-variable"], "{source:?}");
            assert!(flattened.plan.is_none());
        }
    }

    /// A statement that is not a fact pattern generates nothing, so it constrains
    /// nothing — meaningless rather than deferred.
    #[test]
    fn a_statement_that_is_not_a_generator_is_rejected() {
        let flattened = compile("X where X = test.Foo _; 42");
        assert_eq!(flattened.codes(), ["reject/not-a-generator"]);
    }

    /// A head that is a pattern rather than a value cannot be projected.
    #[test]
    fn a_head_that_is_not_a_value_is_rejected() {
        let flattened = compile("\"abc\".. where test.Foo _");
        assert_eq!(flattened.codes(), ["reject/not-projectable"]);
    }

    /// Flatten keeps going, like every other phase: one run reports everything.
    #[test]
    fn flatten_reports_every_fault_it_finds() {
        let flattened = compile("X where 42; 43");
        assert_eq!(
            flattened.codes(),
            ["reject/not-a-generator", "reject/not-a-generator"],
            "both statements, not just the first",
        );
    }

    // ---- the deferred constructs -------------------------------------------

    /// Binding a variable to something no generator produced is a *derived bind*,
    /// which needs Phase 6's value slots.
    #[test]
    fn a_computed_bind_is_not_implemented_yet() {
        for source in [
            "X where X = 42",
            "X where X = test.Foo _; Y = X.name; test.Name Y",
        ] {
            assert_eq!(compile(source).codes(), ["nyi/value-bind"], "{source:?}");
        }
    }

    /// A fact pattern away from the top level of a statement is a generator that
    /// would have to be hoisted into its own loop level.
    #[test]
    fn a_nested_generator_is_not_implemented_yet() {
        for source in [
            "test.Bar {id = 1} where test.Foo _",
            "X where test.Ref {of = test.Foo _}",
        ] {
            assert_eq!(
                compile(source).codes(),
                ["nyi/nested-generator"],
                "{source:?}"
            );
        }
    }

    /// A fact-typed field holds a reference, and a register holds its own row —
    /// so matching or capturing one needs cross-fact navigation. **The trap this
    /// closes:** splicing the register's key bytes would compare a row's key
    /// against a fact id and quietly match nothing.
    #[test]
    fn a_fact_typed_field_is_not_implemented_yet() {
        for source in [
            "X where test.Ref {of = X}",
            "X where X = test.Foo _; test.Ref {of = X}",
        ] {
            assert_eq!(compile(source).codes(), ["nyi/fact-field"], "{source:?}");
        }
    }

    /// A value may be projected but not matched: it lives in `entities`, which
    /// [I6](../../../docs/invariants.md#i6) keeps out of the scan loop.
    #[test]
    fn matching_on_a_value_is_not_implemented_yet() {
        assert_eq!(
            compile("Y where Y = test.Foo _; test.Name Y.value").codes(),
            ["nyi/value-match"]
        );
    }

    /// A stored key is its fields with no wrapper, so a *record* key is not one
    /// field and has no path to project. A *scalar* key is one field, and works.
    #[test]
    fn binding_a_whole_record_key_is_not_implemented_yet() {
        assert_eq!(compile("Y where test.Foo Y").codes(), ["nyi/whole-key"]);

        assert_eq!(
            shape("Y where test.Count Y"),
            lines(&["r0 <- test.Count scan", "head r0.0:int"]),
            "a scalar key is one field",
        );
    }

    // ---- reorder ------------------------------------------------------------

    /// The dependency graph is over *variables*, not statements — so two fact
    /// patterns sharing a variable are one antichain: either may capture it, and
    /// the order is free.
    #[test]
    fn two_fact_patterns_sharing_a_variable_are_one_antichain() {
        let deps = deps_of("X where test.Edge {from = X, to = Y}; test.Node {id = Y}");

        assert_eq!(deps.antichains(), Some(vec![vec![0, 1]]));
        assert!(deps.respects(&[0, 1]));
        assert!(deps.respects(&[1, 0]));
    }

    /// A *read* is not a capture, so it does constrain the order: `Y.name` can only
    /// be evaluated once something has bound `Y`.
    #[test]
    fn a_field_read_constrains_the_order() {
        let deps = deps_of("Y where Y = test.Foo _; test.Name Y.name");

        assert_eq!(deps.antichains(), Some(vec![vec![0], vec![1]]));
        assert!(deps.respects(&[0, 1]));
        assert!(!deps.respects(&[1, 0]));

        // ...and an order that violates it is refused rather than compiled into a
        // plan that reads an unbound register.
        let flattened = compile_in_order("Y where Y = test.Foo _; test.Name Y.name", &[1, 0]);
        assert_eq!(flattened.codes(), ["reject/unbound-variable"]);
        assert!(flattened.plan.is_none());
    }

    /// Reorder is the identity, verified by plan equality: flattening a query is
    /// the same as flattening it in the order it was written.
    #[test]
    fn reorder_is_a_verified_identity() {
        for source in [
            "X where X = test.Foo _",
            "X where test.Edge {from = X, to = Y}; test.Node {id = Y}",
            "X where test.Edge {from = X, to = Y}; test.Node {id = Y}; test.Bar {id = X}",
        ] {
            let chosen = compile(source);
            let statements = chosen.plan().body.len();
            let identity: Vec<usize> = (0..statements).collect();
            let given = compile_in_order(source, &identity);

            assert_eq!(
                describe(chosen.plan(), &chosen.interner),
                describe(given.plan(), &given.interner),
                "{source:?}",
            );
        }
    }

    // ---- running what it produced ------------------------------------------

    /// Facts for the corpus schema, so a plan can be *run* rather than only
    /// inspected.
    ///
    /// | predicate | facts |
    /// |---|---|
    /// | `test.Foo {id, name}` | `(1, "ann")`, `(2, "bob")`, `(3, "ann")` — values `"one"`, `"two"`, `"three"` |
    /// | `test.Edge {from, to}` | `(1, 2)`, `(1, 3)`, `(2, 3)` |
    /// | `test.Node {id}` | `2`, `3` |
    /// | `test.Nested {outer = {inner}}` | `1`, `7` |
    /// | `test.Name` | `"ann"`, `"anna"`, `"bob"` |
    /// | `test.Count` | `-42`, `7` |
    fn store() -> MemStore {
        let mut store = MemStore::new();
        let schema = corpus::schema();
        let id = |name: &str| schema.find_position(name).expect(name).0;

        for (i, (n, name, value)) in [(1i64, "ann", "one"), (2, "bob", "two"), (3, "ann", "three")]
            .into_iter()
            .enumerate()
        {
            store.insert_valued(
                id("test.Foo"),
                compose(&[&i64_field(n), &str_field(name)]),
                i as u64 + 1,
                str_field(value),
            );
        }

        for (i, (from, to)) in [(1i64, 2i64), (1, 3), (2, 3)].into_iter().enumerate() {
            store.insert(
                id("test.Edge"),
                compose(&[&i64_field(from), &i64_field(to)]),
                i as u64 + 1,
            );
        }

        for (i, n) in [2i64, 3].into_iter().enumerate() {
            store.insert(id("test.Node"), i64_field(n), i as u64 + 1);
        }

        for (i, n) in [1i64, 7].into_iter().enumerate() {
            let mut key = vec![crate::focus::tuple::MARK_RECORD];
            key.extend_from_slice(&i64_field(n));
            key.push(crate::focus::tuple::MARK_TERM);
            store.insert(id("test.Nested"), key, i as u64 + 1);
        }

        for (i, s) in ["ann", "anna", "bob"].into_iter().enumerate() {
            store.insert(id("test.Name"), str_field(s), i as u64 + 1);
        }

        for (i, n) in [-42i64, 7].into_iter().enumerate() {
            store.insert(id("test.Count"), i64_field(n), i as u64 + 1);
        }

        store
    }

    fn rows(source: &str) -> Vec<Value> {
        let flattened = compile(source);
        let plan = flattened.plan().clone();
        collect_rows(store(), plan, &flattened.interner).expect("run")
    }

    fn ints(ns: &[i64]) -> Vec<Value> {
        ns.iter().copied().map(Value::Int).collect()
    }

    fn strs(ss: &[&str]) -> Vec<Value> {
        ss.iter().map(|s| Value::Str((*s).to_owned())).collect()
    }

    /// **The end-to-end claim: a query compiled from text returns the rows it
    /// means.** The generated battery below says this over arbitrary queries; these
    /// are the worked examples, one per construct.
    #[test]
    fn a_plan_from_text_runs_to_the_rows_the_query_means() {
        // A capture, and a whole-predicate scan behind it.
        assert_eq!(rows("X where test.Foo {id = X}"), ints(&[1, 2, 3]));
        assert_eq!(
            rows("X where test.Foo {name = X}"),
            strs(&["ann", "bob", "ann"])
        );

        // A seek: only the matching row is examined, and it is the right one.
        assert_eq!(rows("X where test.Foo {id = 2, name = X}"), strs(&["bob"]));

        // A residual behind a capture.
        assert_eq!(
            rows("X where test.Foo {id = X, name = \"ann\"}"),
            ints(&[1, 3])
        );

        // A join, spliced into the inner seek.
        assert_eq!(
            rows("X where test.Edge {from = X, to = Y}; test.Node {id = Y}"),
            ints(&[1, 1, 2]),
            "edges (1,2), (1,3) and (2,3) all have a node at their `to`",
        );

        // The value side, one point read per surviving row.
        assert_eq!(
            rows("X.value where X = test.Foo _"),
            strs(&["one", "two", "three"])
        );

        // A nested capture, through a path.
        assert_eq!(
            rows("X where test.Nested {outer = {inner = X}}"),
            ints(&[1, 7])
        );

        // A string prefix, as a narrowed scan.
        assert_eq!(rows("X where X = test.Name \"ann\".."), {
            let flattened = compile("X where X = test.Name \"ann\"..");
            let plan = flattened.plan().clone();
            let out = collect_rows(store(), plan, &flattened.interner).expect("run");
            assert_eq!(out.len(), 2, "\"ann\" and \"anna\", not \"bob\"");
            out
        });

        // A negative literal, which the seek has to encode order-preservingly.
        assert_eq!(rows("X.value where X = test.Foo _").len(), 3);
        assert_eq!(
            rows("Y where test.Count Y").len(),
            2,
            "a scalar key binds its one field"
        );

        // A record head.
        assert_eq!(
            rows("{a = X, b = Y} where test.Foo {id = X, name = Y}").len(),
            3
        );
    }

    /// **Every order of the body gives the same rows** — the executable form of
    /// "ordering is a performance choice".
    ///
    /// The plans differ (one seeks where the other filters); the answers do not.
    #[test]
    fn every_order_of_the_body_gives_the_same_rows() {
        let source = "{a = X, b = Y} where test.Edge {from = X, to = Y}; test.Node {id = Y}";

        let mut shapes = vec![];
        let mut answers = vec![];

        for order in [vec![0, 1], vec![1, 0]] {
            let flattened = compile_in_order(source, &order);
            let plan = flattened.plan().clone();
            shapes.push(describe(&plan, &flattened.interner));

            let mut rows = collect_rows(store(), plan, &flattened.interner).expect("run");
            rows.sort();
            answers.push(rows);
        }

        assert_ne!(
            shapes[0], shapes[1],
            "the two orders must be different plans"
        );
        assert_eq!(answers[0], answers[1], "...and the same answer");
        assert_eq!(answers[0].len(), 3);
    }
}
/// Schema-first `(query, store)` generator — the front end's tier-3 case.
///
/// The executor's generator ([`plan::proptest`](crate::focus::plan::proptest))
/// draws a *plan* directly, which tests the machine but not the compiler. This one
/// draws a **query, in focus text**, together with a store it runs against and an
/// **independent model** of what it means — so the property is "compiling and
/// running this query gives the rows the query denotes", with the model as the
/// oracle ([testing](../../../docs/testing.md), tier 3).
///
/// Valid by construction, in the same style: draw a schema (predicates × key field
/// types) → draw conforming facts → draw statements over that schema whose every
/// variable occurs in a *capturable* position. Two consequences are what make the
/// battery worth running:
///
/// - **Range restriction is automatic**, so the generator never has to guess
///   whether a query should compile: every draw must.
/// - **Every permutation of the body is a valid order**, because a variable only
///   ever appears in a key field, where whichever statement runs first captures it.
///   That is what lets the reorderability property enumerate *all* orders rather
///   than only the ones some analysis says are safe.
///
/// # What each construct is here to reach
///
/// The generator's job is to make flatten emit every shape a `Plan` can hold — the
/// census (`the_generator_reaches_every_plan_shape`) is what says it does, and each
/// of these was added because the census failed without it:
///
/// | construct drawn | plan shape it produces |
/// |---|---|
/// | a constant in the leading key field | `SeekKey::Prefix(non-empty)` |
/// | a bound variable, then anything determined | a composite seek of several parts |
/// | a string prefix (`"a"..`) behind an open field | `ResidualOp::Prefix` |
/// | a **record-typed** key field given sub-field by sub-field | nested `FieldPath`s |
/// | three-field keys | more than one residual on a level |
/// | a **row bind** (`R0 = gen.P0 {…}`) | `Project::FactRef`, and a register a head reads through |
/// | a predicate with a **value** | `Project::Value` — a point read at projection |
#[cfg(any(test, feature = "proptest"))]
pub mod proptest {
    use std::{collections::BTreeSet, sync::Arc};

    use ::proptest::prelude::*;
    use lasso::Rodeo;

    use crate::focus::{
        mem_store::MemStore,
        plan::{
            FactId,
            proptest::{FieldTy, FieldVal},
        },
        schema::{Predicate, PredicateId, PredicateTy, Schema},
        tuple::{MARK_RECORD, MARK_TERM, Value},
    };

    /// Bounds are tight for the same reason the executor's are: the reorderability
    /// property re-runs each case once per permutation of the body, and the resume
    /// property once per cut point.
    const MAX_PREDICATES: usize = 2;
    const MAX_STMTS: usize = 3;
    const MAX_FACTS: usize = 5;

    /// Up to three key fields. Three is not decoration: two determined fields
    /// *behind* an open one is the only way a level gets more than one residual.
    const MAX_ARITY: usize = 3;

    /// Sub-fields in a record-typed key field. One level of nesting is enough —
    /// a `FieldPath`'s steps are a loop, so depth 2 exercises what depth 5 would.
    const NESTED: usize = 2;

    /// How many variables a query may use. Small on purpose — a wide pool means
    /// every join is unique and nothing ever matches twice.
    const VARS: usize = 3;

    /// Row variables (`R0`, `R1`) — a whole row bound by `R = gen.P {…}`. A
    /// separate pool from the field variables, because a row and a field value are
    /// different types and mixing the namespaces would draw queries that cannot
    /// typecheck.
    const ROWS: usize = 2;

    /// Upper bound (exclusive) on every "pick" draw, resolved modulo the legal
    /// options in context.
    const PICKS: u8 = 4;

    /// Prefixes to draw for a string-prefix pattern. `""` matches every string and
    /// `"a"` matches `"a"` and `"ab"` but not `"b"` — the domain
    /// ([`plan::proptest`](crate::focus::plan::proptest)) is chosen so that middle
    /// case exists.
    const PREFIXES: [&str; 3] = ["", "a", "b"];

    /// A generated key field's type: a scalar, or a record of scalars.
    #[derive(Debug, Clone)]
    enum GenTy {
        Scalar(FieldTy),
        Record(Vec<FieldTy>),
    }

    /// A value of a [`GenTy`].
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum GenVal {
        Scalar(FieldVal),
        Record(Vec<FieldVal>),
    }

    impl GenVal {
        /// The field's stored bytes. A **record keeps its wrapper** — it is one
        /// value among others inside the key, and has to be skippable as one
        /// ([chapter 3](../../../docs/03-storage-model.md#a-stored-key-is-flat)).
        fn encode(&self) -> Vec<u8> {
            match self {
                GenVal::Scalar(val) => val.encode(),
                GenVal::Record(fields) => {
                    let mut out = vec![MARK_RECORD];
                    for field in fields {
                        out.extend_from_slice(&field.encode());
                    }
                    out.push(MARK_TERM);
                    out
                }
            }
        }

        /// This field as a projected row carries it. A record's field *names* come
        /// from the schema, so the model has to agree with what the schema declares
        /// — `g0`, `g1`, … in declaration order.
        fn to_value(&self) -> Value {
            match self {
                GenVal::Scalar(val) => val.to_value(),
                GenVal::Record(fields) => Value::Record(
                    fields
                        .iter()
                        .enumerate()
                        .map(|(g, field)| (format!("g{g}"), field.to_value()))
                        .collect(),
                ),
            }
        }

        fn source(&self) -> String {
            match self {
                GenVal::Scalar(val) => val.source(),
                GenVal::Record(fields) => format!(
                    "{{{}}}",
                    fields
                        .iter()
                        .enumerate()
                        .map(|(g, field)| format!("g{g} = {}", field.source()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }

        fn scalar(&self) -> Option<&FieldVal> {
            match self {
                GenVal::Scalar(val) => Some(val),
                GenVal::Record(_) => None,
            }
        }
    }

    /// What a *leaf* position can be — a scalar key field, or one sub-field of a
    /// record-typed one.
    #[derive(Debug, Clone)]
    enum Leaf {
        /// Not written at all, which the type checker reads as a wildcard.
        Omitted,
        Wildcard,
        Const(GenVal),
        /// A string prefix, `"ab"..`. Only drawn for `Str` positions.
        Prefix(&'static str),
        /// A variable. Whether this *captures* or *reads* depends on the order the
        /// statements run in, which is exactly why the spec does not say.
        Var(usize),
    }

    /// A whole key field's pattern.
    #[derive(Debug, Clone)]
    enum FieldPat {
        /// A scalar field, or a record field matched whole (as a constant).
        Leaf(Leaf),
        /// A record field, given sub-field by sub-field — which is what puts a
        /// **nested path** in the plan.
        Nested(Vec<Leaf>),
    }

    #[derive(Debug, Clone)]
    struct StmtSpec {
        predicate: usize,
        /// The row variable this statement binds, from `R = gen.P {…}`. At most one
        /// statement binds any given row variable: binding one twice is
        /// `nyi/bind-unification`, not a query this generator may draw.
        row: Option<usize>,
        fields: Vec<FieldPat>,
    }

    /// One predicate: its key field types, and whether it has a value side.
    #[derive(Debug, Clone)]
    struct PredSpec {
        fields: Vec<GenTy>,
        value: Option<FieldTy>,
    }

    /// One fact: a key, and the value the predicate's type calls for.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct Fact {
        key: Vec<GenVal>,
        value: Option<FieldVal>,
    }

    /// What the head projects.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum HeadItem {
        /// A captured field variable → `Project::RegisterField`.
        Var(usize),
        /// A row variable → `Project::FactRef`, the row's identity.
        Row(usize),
        /// `R.value` → `Project::Value`, one point read per surviving row.
        Value(usize),
        /// `R.f{k}` → a field read *through* a bound row.
        RowField(usize, usize),
    }

    /// A generated query, the store it runs against, and what it means.
    #[derive(Debug, Clone)]
    pub struct QueryAndStore {
        schema: Vec<PredSpec>,
        /// `facts[p]` — predicate `p`'s facts, deduplicated and sorted by key.
        facts: Vec<Vec<Fact>>,
        stmts: Vec<StmtSpec>,
        head: Vec<HeadItem>,
    }

    impl QueryAndStore {
        pub fn statements(&self) -> usize {
            self.stmts.len()
        }

        /// The schema the query is written against: `gen.P0…`, fields `f0…`, and
        /// `g0…` inside a record-typed field.
        ///
        /// Field names are ascending so that sorted-by-name is also declaration
        /// order — a record's field order is part of its encoding
        /// ([chapter 6](../../../docs/06-types-and-schema.md)).
        pub fn schema(&self) -> Schema {
            let mut rodeo = Rodeo::new();
            let fields: Vec<_> = (0..MAX_ARITY)
                .map(|f| rodeo.get_or_intern(format!("f{f}")))
                .collect();
            let nested: Vec<_> = (0..NESTED)
                .map(|g| rodeo.get_or_intern(format!("g{g}")))
                .collect();

            let predicates: Vec<Predicate> = self
                .schema
                .iter()
                .enumerate()
                .map(|(p, spec)| Predicate {
                    name: rodeo.get_or_intern(format!("gen.P{p}")),
                    key: PredicateTy::Record(
                        spec.fields
                            .iter()
                            .enumerate()
                            .map(|(f, ty)| {
                                let ty = match ty {
                                    GenTy::Scalar(scalar) => scalar.predicate_ty(),
                                    GenTy::Record(subs) => PredicateTy::Record(
                                        subs.iter()
                                            .enumerate()
                                            .map(|(g, sub)| (nested[g], sub.predicate_ty()))
                                            .collect(),
                                    ),
                                };
                                (fields[f], ty)
                            })
                            .collect(),
                    ),
                    value: spec.value.map(FieldTy::predicate_ty),
                })
                .collect();

            // The head's field names, which no declaration interns.
            for h in 0..VARS + ROWS * 2 {
                rodeo.get_or_intern(format!("h{h}"));
            }

            Schema::new(rodeo.into_reader(), Arc::from(predicates))
        }

        pub fn source(&self) -> String {
            self.source_in_order(&self.identity())
        }

        /// The query as focus text, with its statements written in `order`.
        ///
        /// Writing the *source* in a different order is not the same experiment as
        /// flattening in a different order — this one moves the capture, which is
        /// what a person editing a query does.
        pub fn source_in_order(&self, order: &[usize]) -> String {
            let body: Vec<String> = order
                .iter()
                .map(|&stmt| self.statement_source(stmt))
                .collect();

            format!("{} where {}", self.head_source(), body.join("; "))
        }

        fn head_source(&self) -> String {
            if self.head.is_empty() {
                // Nothing to project: a constant head, which is still a row per
                // match.
                return "0".to_owned();
            }

            let fields: Vec<String> = self
                .head
                .iter()
                .enumerate()
                .map(|(h, item)| {
                    let item = match item {
                        HeadItem::Var(var) => format!("V{var}"),
                        HeadItem::Row(row) => format!("R{row}"),
                        HeadItem::Value(row) => format!("R{row}.value"),
                        HeadItem::RowField(row, field) => format!("R{row}.f{field}"),
                    };
                    format!("h{h} = {item}")
                })
                .collect();

            format!("{{{}}}", fields.join(", "))
        }

        fn statement_source(&self, stmt: usize) -> String {
            let spec = &self.stmts[stmt];

            let fields: Vec<String> = spec
                .fields
                .iter()
                .enumerate()
                .filter_map(|(f, pat)| match pat {
                    FieldPat::Leaf(leaf) => leaf_source(leaf).map(|text| format!("f{f} = {text}")),
                    FieldPat::Nested(subs) => {
                        let given: Vec<String> = subs
                            .iter()
                            .enumerate()
                            .filter_map(|(g, leaf)| {
                                leaf_source(leaf).map(|text| format!("g{g} = {text}"))
                            })
                            .collect();

                        Some(format!("f{f} = {{{}}}", given.join(", ")))
                    }
                })
                .collect();

            let bind = match spec.row {
                Some(row) => format!("R{row} = "),
                None => String::new(),
            };

            format!("{bind}gen.P{} {{{}}}", spec.predicate, fields.join(", "))
        }

        /// The spec's facts in insertion order: `(predicate, key bytes, value bytes,
        /// sequence within that predicate)`.
        ///
        /// One deterministic order, walked by every store this spec seeds — which is
        /// what makes a `MemStore` and a fjall DB built from it agree fact for fact,
        /// ids included, since the numbering matches what the real per-predicate
        /// allocator hands out ([I11](../../../docs/invariants.md#i11)). A projected
        /// `FactRef` is comparable against the model only because of that.
        pub fn facts(&self) -> impl Iterator<Item = (PredicateId, Vec<u8>, Vec<u8>, u64)> + '_ {
            self.facts
                .iter()
                .enumerate()
                .flat_map(|(predicate, facts)| {
                    facts.iter().enumerate().map(move |(i, fact)| {
                        let key: Vec<u8> = fact.key.iter().flat_map(GenVal::encode).collect();
                        let value = fact
                            .value
                            .as_ref()
                            .map(FieldVal::encode)
                            .unwrap_or_default();

                        (PredicateId(predicate as u32), key, value, i as u64 + 1)
                    })
                })
        }

        pub fn build_store(&self) -> MemStore {
            let mut store = MemStore::new();

            for (predicate, key, value, sequence) in self.facts() {
                store.insert_valued(predicate, key, sequence, value);
            }

            store
        }

        pub fn identity(&self) -> Vec<usize> {
            (0..self.stmts.len()).collect()
        }

        /// Every permutation of the body — all of which are valid orders.
        pub fn orders(&self) -> Vec<Vec<usize>> {
            permutations(&self.identity())
        }

        /// **The model.** Nested loops over the facts, in `order`, binding a
        /// variable at its first occurrence and comparing at every later one.
        ///
        /// Deliberately the slow, obvious reading of the query — no seeks, no
        /// residuals, no registers — so that agreeing with it says something about
        /// the compiler and the executor rather than about a shared idea of how to
        /// go fast.
        pub fn expected_in_order(&self, order: &[usize]) -> Vec<Value> {
            let mut rows = vec![];
            let mut env = Env {
                vars: vec![None; VARS],
                rows: vec![None; ROWS],
            };

            self.walk(order, 0, &mut env, &mut rows);

            rows
        }

        pub fn expected(&self) -> Vec<Value> {
            self.expected_in_order(&self.identity())
        }

        fn walk(&self, order: &[usize], depth: usize, env: &mut Env, rows: &mut Vec<Value>) {
            if depth == order.len() {
                rows.push(self.project(env));
                return;
            }

            let spec = &self.stmts[order[depth]];

            for (index, fact) in self.facts[spec.predicate].iter().enumerate() {
                let saved = env.clone();

                if matches(spec, fact, env) {
                    if let Some(row) = spec.row {
                        env.rows[row] = Some((spec.predicate, index));
                    }
                    self.walk(order, depth + 1, env, rows);
                }

                *env = saved;
            }
        }

        fn project(&self, env: &Env) -> Value {
            if self.head.is_empty() {
                return Value::Int(0);
            }

            Value::Record(
                self.head
                    .iter()
                    .enumerate()
                    .map(|(h, item)| (format!("h{h}"), self.project_item(item, env)))
                    .collect(),
            )
        }

        fn project_item(&self, item: &HeadItem, env: &Env) -> Value {
            let row = |row: usize| {
                env.rows[row].expect("every projected row variable is bound by a statement")
            };

            match item {
                HeadItem::Var(var) => env.vars[*var]
                    .as_ref()
                    .expect("every projected variable is captured somewhere")
                    .to_value(),

                HeadItem::Row(r) => {
                    let (predicate, index) = row(*r);
                    Value::FactRef(
                        FactId::new(PredicateId(predicate as u32), index as u64 + 1)
                            .expect("a spec fact id"),
                    )
                }

                HeadItem::Value(r) => {
                    let (predicate, index) = row(*r);
                    self.facts[predicate][index]
                        .value
                        .as_ref()
                        .expect("a value is only projected where the predicate has one")
                        .to_value()
                }

                HeadItem::RowField(r, field) => {
                    let (predicate, index) = row(*r);
                    self.facts[predicate][index].key[*field].to_value()
                }
            }
        }
    }

    /// The model's bindings: field variables, and whole rows.
    #[derive(Debug, Clone)]
    struct Env {
        vars: Vec<Option<FieldVal>>,
        rows: Vec<Option<(usize, usize)>>,
    }

    fn leaf_source(leaf: &Leaf) -> Option<String> {
        match leaf {
            Leaf::Omitted => None,
            Leaf::Wildcard => Some("_".to_owned()),
            Leaf::Const(val) => Some(val.source()),
            Leaf::Prefix(prefix) => Some(format!("{prefix:?}..")),
            Leaf::Var(var) => Some(format!("V{var}")),
        }
    }

    /// Match one statement against one fact, binding what it captures.
    ///
    /// Leaves partial bindings behind on failure; the caller restores from its own
    /// copy, which is what makes backtracking here trivial and the model easy to
    /// believe.
    fn matches(spec: &StmtSpec, fact: &Fact, env: &mut Env) -> bool {
        for (f, pat) in spec.fields.iter().enumerate() {
            match pat {
                FieldPat::Leaf(leaf) => {
                    if !matches_leaf(leaf, &fact.key[f], env) {
                        return false;
                    }
                }
                FieldPat::Nested(subs) => {
                    let GenVal::Record(values) = &fact.key[f] else {
                        return false;
                    };

                    for (g, leaf) in subs.iter().enumerate() {
                        if !matches_leaf(leaf, &GenVal::Scalar(values[g].clone()), env) {
                            return false;
                        }
                    }
                }
            }
        }

        true
    }

    fn matches_leaf(leaf: &Leaf, value: &GenVal, env: &mut Env) -> bool {
        match leaf {
            Leaf::Omitted | Leaf::Wildcard => true,

            Leaf::Const(constant) => value == constant,

            Leaf::Prefix(prefix) => match value.scalar() {
                Some(FieldVal::Str(text)) => text.starts_with(prefix),
                _ => false,
            },

            Leaf::Var(var) => {
                // A variable only ever stands in a scalar position, so this cannot
                // be a record.
                let Some(scalar) = value.scalar() else {
                    return false;
                };

                match &env.vars[*var] {
                    Some(bound) => scalar == bound,
                    None => {
                        env.vars[*var] = Some(scalar.clone());
                        true
                    }
                }
            }
        }
    }

    /// Every permutation of `items`, in a deterministic order.
    fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }

        let mut out = vec![];

        for (i, &item) in items.iter().enumerate() {
            let mut rest = items.to_vec();
            rest.remove(i);

            for mut tail in permutations(&rest) {
                tail.insert(0, item);
                out.push(tail);
            }
        }

        out
    }

    // ---- the draws ---------------------------------------------------------

    #[derive(Debug, Clone)]
    struct PredicateDraw {
        arity: usize,
        /// Per field: whether it is a record, and the scalar type(s) inside it.
        field_kinds: Vec<u8>,
        field_tys: Vec<Vec<u8>>,
        value: u8,
    }

    #[derive(Debug, Clone)]
    struct LeafDraw {
        kind: u8,
        var: u8,
        constant: u8,
        prefix: u8,
    }

    #[derive(Debug, Clone)]
    struct FieldDraw {
        /// Whether a record-typed field is matched whole or sub-field by sub-field.
        whole: bool,
        leaf: LeafDraw,
        subs: Vec<LeafDraw>,
    }

    #[derive(Debug, Clone)]
    struct StmtDraw {
        predicate: u8,
        row: u8,
        fields: Vec<FieldDraw>,
    }

    #[derive(Debug, Clone)]
    struct HeadDraw {
        kind: u8,
        which: u8,
        field: u8,
    }

    /// A constant that actually occurs at that field of that predicate, so the
    /// statement matches something. Falls back to the domain for an empty
    /// predicate.
    fn constant_for(facts: &[Fact], field: usize, ty: &GenTy, pick: u8) -> GenVal {
        match facts.len() {
            0 => match ty {
                GenTy::Scalar(scalar) => GenVal::Scalar(FieldVal::of(*scalar, pick)),
                GenTy::Record(subs) => GenVal::Record(
                    subs.iter()
                        .map(|scalar| FieldVal::of(*scalar, pick))
                        .collect(),
                ),
            },
            len => facts[pick as usize % len].key[field].clone(),
        }
    }

    /// One leaf position: its type, the facts it could match, and what was drawn.
    struct Position<'a> {
        ty: FieldTy,
        /// The value at this position in each of the predicate's facts, for drawing
        /// a constant that matches one of them.
        occurring: Vec<FieldVal>,
        used: &'a mut BTreeSet<usize>,
        var_tys: &'a [FieldTy],
    }

    fn resolve_leaf(draw: &LeafDraw, position: Position<'_>) -> Leaf {
        let Position {
            ty,
            occurring,
            used,
            var_tys,
        } = position;

        let constant = || match occurring.len() {
            0 => GenVal::Scalar(FieldVal::of(ty, draw.constant)),
            len => GenVal::Scalar(occurring[draw.constant as usize % len].clone()),
        };

        // Weighted towards the permissive: with three key fields, a statement that
        // pins two of them matches nothing, and an empty answer tests less than a
        // matched one. Two draws in six are a variable, which is the construct that
        // makes a join.
        match draw.kind % 6 {
            0 => Leaf::Omitted,
            1 => Leaf::Wildcard,
            2 => Leaf::Const(constant()),

            // A prefix only means anything on a string; on an integer this would
            // otherwise become a second constant draw.
            3 => match ty {
                FieldTy::Str => Leaf::Prefix(PREFIXES[draw.prefix as usize % PREFIXES.len()]),
                FieldTy::Int => Leaf::Wildcard,
            },

            // A variable, if one of this type is free in this statement. Variables
            // are typed, so a mismatched one would not typecheck; a repeat *within*
            // one statement is an intra-row equality, which Phase 4 rejects.
            _ => {
                let candidates: Vec<usize> = (0..VARS)
                    .filter(|v| var_tys[*v] == ty && !used.contains(v))
                    .collect();

                match candidates.len() {
                    0 => Leaf::Const(constant()),
                    len => {
                        let var = candidates[draw.var as usize % len];
                        used.insert(var);
                        Leaf::Var(var)
                    }
                }
            }
        }
    }

    fn resolve(
        npredicates: usize,
        predicates: Vec<PredicateDraw>,
        facts: Vec<Vec<Vec<u8>>>,
        var_tys: Vec<u8>,
        stmts: Vec<StmtDraw>,
        heads: Vec<HeadDraw>,
    ) -> QueryAndStore {
        let schema: Vec<PredSpec> = predicates
            .iter()
            .take(npredicates)
            .map(|draw| PredSpec {
                fields: (0..draw.arity)
                    .map(|f| {
                        // Every third field is a record, so nesting is reached
                        // often without crowding out the flat case the cache
                        // serves.
                        if draw.field_kinds[f] % 3 == 0 {
                            GenTy::Record(
                                draw.field_tys[f]
                                    .iter()
                                    .take(NESTED)
                                    .map(|&pick| FieldTy::of(pick))
                                    .collect(),
                            )
                        } else {
                            GenTy::Scalar(FieldTy::of(draw.field_tys[f][0]))
                        }
                    })
                    .collect(),
                // Two predicates in three carry a value, so `.value` is reachable
                // without every predicate paying for one.
                value: (draw.value % 3 != 0).then(|| FieldTy::of(draw.value)),
            })
            .collect();

        let facts: Vec<Vec<Fact>> = schema
            .iter()
            .zip(facts)
            .map(|(spec, drawn)| {
                let mut facts: Vec<Fact> = drawn
                    .iter()
                    .map(|picks| Fact {
                        key: spec
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(f, ty)| match ty {
                                GenTy::Scalar(scalar) => {
                                    GenVal::Scalar(FieldVal::of(*scalar, picks[f]))
                                }
                                GenTy::Record(subs) => GenVal::Record(
                                    subs.iter()
                                        .enumerate()
                                        .map(|(g, scalar)| {
                                            // A sub-field varies with its position
                                            // so a record is not all one value.
                                            FieldVal::of(*scalar, picks[f].wrapping_add(g as u8))
                                        })
                                        .collect(),
                                ),
                            })
                            .collect(),
                        value: None,
                    })
                    .collect();

                // One key, one fact — a repeated draw would otherwise shadow an
                // earlier fact.
                facts.sort();
                facts.dedup();

                // The value follows from the fact's position, so it needs no draw
                // of its own and cannot make two facts differ only in their value.
                for (i, fact) in facts.iter_mut().enumerate() {
                    fact.value = spec.value.map(|ty| FieldVal::of(ty, i as u8));
                }

                facts
            })
            .collect();

        let var_tys: Vec<FieldTy> = var_tys.iter().map(|&pick| FieldTy::of(pick)).collect();

        let mut used_vars = BTreeSet::new();
        let mut bound_rows: Vec<Option<usize>> = vec![];
        let mut resolved: Vec<StmtSpec> = Vec::with_capacity(stmts.len());

        for draw in &stmts {
            let predicate = draw.predicate as usize % schema.len();
            let spec = &schema[predicate];

            // A row variable is bound by at most one statement: binding one twice
            // is `nyi/bind-unification`.
            let row = match draw.row % 2 {
                0 => None,
                _ => (0..ROWS).find(|r| !bound_rows.contains(&Some(*r))),
            };
            bound_rows.push(row);

            let mut here = BTreeSet::new();
            let mut fields = Vec::with_capacity(spec.fields.len());

            for (f, ty) in spec.fields.iter().enumerate() {
                let draw = &draw.fields[f];

                let pat = match ty {
                    GenTy::Scalar(scalar) => FieldPat::Leaf(resolve_leaf(
                        &draw.leaf,
                        Position {
                            ty: *scalar,
                            occurring: occurring_scalars(&facts[predicate], f, None),
                            used: &mut here,
                            var_tys: &var_tys,
                        },
                    )),

                    // A record field: matched whole as a constant (which can extend
                    // a seek prefix), or field by field (which cannot, and puts
                    // nested paths in the residuals).
                    GenTy::Record(subs) if draw.whole => FieldPat::Leaf(Leaf::Const(constant_for(
                        &facts[predicate],
                        f,
                        ty,
                        draw.leaf.constant,
                    ))),

                    GenTy::Record(subs) => FieldPat::Nested(
                        subs.iter()
                            .enumerate()
                            .map(|(g, scalar)| {
                                resolve_leaf(
                                    &draw.subs[g],
                                    Position {
                                        ty: *scalar,
                                        occurring: occurring_scalars(&facts[predicate], f, Some(g)),
                                        used: &mut here,
                                        var_tys: &var_tys,
                                    },
                                )
                            })
                            .collect(),
                    ),
                };

                fields.push(pat);
            }

            used_vars.extend(here);
            resolved.push(StmtSpec {
                predicate,
                row,
                fields,
            });
        }

        // The head: every variable the query captured (so nothing is bound and then
        // ignored), plus whatever the draws ask of the rows that are bound.
        let mut head: Vec<HeadItem> = used_vars.iter().map(|v| HeadItem::Var(*v)).collect();

        for draw in &heads {
            let rows: Vec<(usize, usize)> = resolved
                .iter()
                .enumerate()
                .filter_map(|(stmt, spec)| spec.row.map(|row| (row, stmt)))
                .collect();

            if rows.is_empty() {
                break;
            }

            let (row, stmt) = rows[draw.which as usize % rows.len()];
            let spec = &schema[resolved[stmt].predicate];

            let item = match draw.kind % 3 {
                0 => HeadItem::Row(row),
                // Only where the predicate has a value to read.
                1 if spec.value.is_some() => HeadItem::Value(row),
                1 => HeadItem::Row(row),
                _ => HeadItem::RowField(row, draw.field as usize % spec.fields.len()),
            };

            if !head.contains(&item) {
                head.push(item);
            }
        }

        QueryAndStore {
            schema,
            facts,
            stmts: resolved,
            head,
        }
    }

    /// The scalar values occurring at one leaf position across a predicate's facts —
    /// `field` for a scalar field, `field`'s sub-field `sub` for a record one.
    fn occurring_scalars(facts: &[Fact], field: usize, sub: Option<usize>) -> Vec<FieldVal> {
        facts
            .iter()
            .filter_map(|fact| match (&fact.key[field], sub) {
                (GenVal::Scalar(val), None) => Some(val.clone()),
                (GenVal::Record(values), Some(g)) => values.get(g).cloned(),
                _ => None,
            })
            .collect()
    }

    fn arb_leaf() -> impl Strategy<Value = LeafDraw> {
        (0u8..6, 0u8..PICKS, 0u8..PICKS, 0u8..PICKS).prop_map(|(kind, var, constant, prefix)| {
            LeafDraw {
                kind,
                var,
                constant,
                prefix,
            }
        })
    }

    fn arb_field() -> impl Strategy<Value = FieldDraw> {
        (
            any::<bool>(),
            arb_leaf(),
            prop::collection::vec(arb_leaf(), NESTED),
        )
            .prop_map(|(whole, leaf, subs)| FieldDraw { whole, leaf, subs })
    }

    fn arb_predicate() -> impl Strategy<Value = PredicateDraw> {
        (
            1..=MAX_ARITY,
            prop::collection::vec(0u8..PICKS, MAX_ARITY),
            prop::collection::vec(prop::collection::vec(0u8..PICKS, NESTED), MAX_ARITY),
            0u8..PICKS,
        )
            .prop_map(|(arity, field_kinds, field_tys, value)| PredicateDraw {
                arity,
                field_kinds,
                field_tys,
                value,
            })
    }

    /// Every predicate gets at least one fact: an empty one at the outermost level
    /// makes the whole run empty, and "the scan finds nothing" is already reached
    /// constantly by constants and joins that match no row.
    fn arb_predicate_facts() -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(prop::collection::vec(0u8..PICKS, MAX_ARITY), 1..=MAX_FACTS)
    }

    fn arb_stmt() -> impl Strategy<Value = StmtDraw> {
        (
            0u8..PICKS,
            0u8..PICKS,
            prop::collection::vec(arb_field(), MAX_ARITY),
        )
            .prop_map(|(predicate, row, fields)| StmtDraw {
                predicate,
                row,
                fields,
            })
    }

    fn arb_head() -> impl Strategy<Value = HeadDraw> {
        (0u8..3, 0u8..PICKS, 0u8..PICKS).prop_map(|(kind, which, field)| HeadDraw {
            kind,
            which,
            field,
        })
    }

    /// A valid `(query, store)` pair: 1-, 2- or 3-statement queries over a small
    /// generated schema, with captures, reads, constants, wildcards, string
    /// prefixes, nested record keys, row binds and values — against a conforming
    /// store.
    pub fn arb_query_and_store() -> impl Strategy<Value = QueryAndStore> {
        (
            1..=MAX_PREDICATES,
            prop::collection::vec(arb_predicate(), MAX_PREDICATES),
            prop::collection::vec(arb_predicate_facts(), MAX_PREDICATES),
            prop::collection::vec(0u8..PICKS, VARS),
            prop::collection::vec(arb_stmt(), 1..=MAX_STMTS),
            prop::collection::vec(arb_head(), 0..=3),
        )
            .prop_map(|(npredicates, predicates, facts, var_tys, stmts, heads)| {
                resolve(npredicates, predicates, facts, var_tys, stmts, heads)
            })
    }
}

#[cfg(test)]
mod battery {
    use super::{
        flatten_in_order,
        proptest::{QueryAndStore, arb_query_and_store},
    };
    use crate::focus::{
        cst::CstNode,
        diag::Diagnostics,
        fixtures::{collect_rows, run_with_suspends},
        lower::lower,
        parse::parse,
        plan::{
            FactId, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart,
            proptest::{arb_interruption_schedule, cut_points},
        },
        schema::{LocalInterner, Schema},
        store::FjallDb,
        tuple::Value,
        ty,
    };
    use ::proptest::prelude::*;
    use tempfile::TempDir;

    /// Compile `source` against `schema`, in the given loop order.
    ///
    /// Asserts nothing was reported: the generator is valid by construction, so a
    /// diagnostic here is a fault in flatten or in the generator, and either way
    /// the message should say which query.
    fn plan_of(schema: &Schema, source: &str, order: &[usize]) -> (Plan, LocalInterner) {
        let mut interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();

        let cst = parse(source, &mut diagnostics).expect("a generated query parses");
        let ast = lower(&CstNode::new(&cst), schema, &mut interner, &mut diagnostics);
        let _typed = ty::check(&ast, schema, &interner, &mut diagnostics);
        assert!(
            !diagnostics.has_errors(),
            "{source:?} did not typecheck: {:?}",
            diagnostics.codes().collect::<Vec<_>>()
        );

        let plan = flatten_in_order(&ast, schema, &interner, &mut diagnostics, order);

        assert!(
            !diagnostics.has_errors(),
            "{source:?} did not flatten: {:?}",
            diagnostics.codes().collect::<Vec<_>>()
        );

        (plan.expect("a plan"), interner)
    }

    fn run(spec: &QueryAndStore, order: &[usize]) -> Vec<Value> {
        let schema = spec.schema();
        let (plan, interner) = plan_of(&schema, &spec.source(), order);

        collect_rows(spec.build_store(), plan, &interner).expect("run")
    }

    proptest! {
        /// **The headline gate: a flattened plan runs to the rows the query
        /// means.**
        ///
        /// Tier 3 — the model is the slow, obvious nested-loop reading of the query
        /// ([`proptest`](super::proptest)), and the comparison is exact, rows *in
        /// order*, because the executor's loop nesting follows the plan's.
        #[test]
        fn a_flattened_plan_runs_to_the_rows_the_query_means(spec in arb_query_and_store()) {
            let identity = spec.identity();

            prop_assert_eq!(run(&spec, &identity), spec.expected());
        }

        /// **Every loop order gives the same rows.**
        ///
        /// The reorderability claim, over generated queries: reordering the body
        /// changes which statement captures a shared variable — and so whether a
        /// field seeks or filters — but never the answer. Compared exactly against
        /// the model *in that order* (the rows come out in loop order), and as a
        /// multiset against the model in the identity order (the answer itself does
        /// not depend on the order at all).
        #[test]
        fn every_loop_order_gives_the_same_rows(spec in arb_query_and_store()) {
            let mut want = spec.expected();
            want.sort();

            for order in spec.orders() {
                let rows = run(&spec, &order);
                prop_assert_eq!(&rows, &spec.expected_in_order(&order));

                let mut sorted = rows;
                sorted.sort();
                prop_assert_eq!(&sorted, &want, "order {:?} of {:?}", order, spec.source());
            }
        }

        /// The same claim from the *source* end: writing the statements in another
        /// order is a different query text, and must still mean the same thing.
        #[test]
        fn rewriting_the_body_in_another_order_means_the_same_query(spec in arb_query_and_store()) {
            let mut want = spec.expected();
            want.sort();

            let schema = spec.schema();

            for order in spec.orders() {
                let source = spec.source_in_order(&order);
                let identity: Vec<usize> = (0..spec.statements()).collect();
                let (plan, interner) = plan_of(&schema, &source, &identity);

                let mut rows = collect_rows(spec.build_store(), plan, &interner).expect("run");
                rows.sort();

                prop_assert_eq!(&rows, &want, "{:?}", source);
            }
        }

        /// Flattening is deterministic: the same query twice is the same plan.
        ///
        /// The driver's determinism property stops at the typed tree; a plan is
        /// where a `HashMap`'s iteration order or an interning accident would show
        /// up as a different seek.
        #[test]
        fn flattening_the_same_query_twice_gives_the_same_plan(spec in arb_query_and_store()) {
            let schema = spec.schema();
            let source = spec.source();
            let identity = spec.identity();

            let (first, _) = plan_of(&schema, &source, &identity);
            let (second, _) = plan_of(&schema, &source, &identity);

            prop_assert_eq!(format!("{first:?}"), format!("{second:?}"));
        }
    }

    // ---- resume, over plans the compiler produced --------------------------
    //
    // [I4](../../docs/invariants.md#i4) is guarded over *hand-built* plan shapes
    // (`plan::proptest`), which is where it belongs — the executor is what it is
    // about. But flatten emits shapes that generator never draws: constant seek
    // prefixes, composite seeks of several parts, `ResidualOp::Prefix`, nested
    // field paths, more than one residual on a level, and `Project::Value`. A
    // resume that mishandled any of them would be invisible to the executor's own
    // battery, so the same property runs here over compiled plans — with a census
    // (below) proving those shapes are actually reached rather than hoped for.

    /// Which of flatten's plan shapes a run has produced.
    #[derive(Debug, Default)]
    struct Shapes {
        constant_seek: bool,
        multi_part_seek: bool,
        constant_in_composite: bool,
        prefix_residual: bool,
        nested_path: bool,
        several_residuals: bool,
        value_projection: bool,
        fact_ref_projection: bool,
    }

    impl Shapes {
        fn missing(&self) -> Vec<&'static str> {
            let mut out = vec![];

            for (present, what) in [
                (self.constant_seek, "a constant seek prefix"),
                (self.multi_part_seek, "a composite seek of several parts"),
                (
                    self.constant_in_composite,
                    "a constant inside a composite seek",
                ),
                (self.prefix_residual, "a `ResidualOp::Prefix`"),
                (self.nested_path, "a nested field path"),
                (self.several_residuals, "more than one residual on a level"),
                (self.value_projection, "a `Project::Value`"),
                (self.fact_ref_projection, "a `Project::FactRef`"),
            ] {
                if !present {
                    out.push(what);
                }
            }

            out
        }

        fn observe(&mut self, plan: &Plan) {
            for generator in plan.body.iter() {
                match &generator.access.seek_key {
                    SeekKey::Prefix(bytes) => self.constant_seek |= !bytes.is_empty(),
                    SeekKey::Composite(parts) => {
                        self.multi_part_seek |= parts.len() > 1;

                        for part in parts.iter() {
                            match part {
                                SeekKeyPart::Bytes(_) => self.constant_in_composite = true,
                                SeekKeyPart::RegisterField { path, .. } => {
                                    self.nested_path |= !path.is_flat();
                                }
                            }
                        }
                    }
                }

                self.several_residuals |= generator.residuals.len() > 1;

                for Residual { path, op } in generator.residuals.iter() {
                    self.nested_path |= !path.is_flat();
                    match op {
                        ResidualOp::Prefix(_) => self.prefix_residual = true,
                        ResidualOp::EqRegisterField { path, .. } => {
                            self.nested_path |= !path.is_flat();
                        }
                        ResidualOp::EqConst(_) => {}
                    }
                }
            }

            self.observe_head(&plan.head);
        }

        fn observe_head(&mut self, head: &Project) {
            match head {
                Project::Value { .. } => self.value_projection = true,
                Project::FactRef(_) => self.fact_ref_projection = true,
                Project::RegisterField { path, .. } => self.nested_path |= !path.is_flat(),
                Project::Record(fields) => {
                    for (_, field) in fields.iter() {
                        self.observe_head(field);
                    }
                }
                Project::Lit(_) => {}
            }
        }
    }

    /// **The census.** Every plan shape flatten can emit is reached by the
    /// generator — which is what licenses the resume property above it to claim
    /// anything about those shapes.
    ///
    /// Written before the generator could produce most of them, and failing until
    /// it could: string prefixes, nested record keys, three-field keys, row binds
    /// and values were all added to satisfy this.
    #[test]
    fn the_generator_reaches_every_plan_shape() {
        use ::proptest::{
            strategy::{Strategy, ValueTree},
            test_runner::TestRunner,
        };

        const RUNS: usize = 300;

        let mut runner = TestRunner::deterministic();
        let mut shapes = Shapes::default();

        for _ in 0..RUNS {
            let spec = arb_query_and_store()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let schema = spec.schema();
            let (plan, _) = plan_of(&schema, &spec.source(), &spec.identity());

            shapes.observe(&plan);
        }

        let missing = shapes.missing();
        assert!(
            missing.is_empty(),
            "{RUNS} generated queries never produced: {}",
            missing.join(", ")
        );
    }

    proptest! {
        // A case runs the plan once per cut point, so it is dearer than the
        // completion property above — enough cases to be a real battery, given the
        // shapes themselves are what the census pins.
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// **I4 over compiled plans: resume == the query's meaning.**
        ///
        /// Compared against the *model*, not against an uninterrupted run of the
        /// same plan — which is strictly stronger, and says the same thing twice
        /// over: suspending anywhere changes neither the rows nor their order.
        ///
        /// A case whose query matches nothing has no cut points and so says nothing
        /// about resume; that most cases do match is what the population assertion
        /// below is for.
        #[test]
        fn resume_of_a_compiled_plan_equals_the_query(
            spec in arb_query_and_store(),
            schedule in arb_interruption_schedule(),
        ) {
            let schema = spec.schema();
            let (plan, interner) = plan_of(&schema, &spec.source(), &spec.identity());
            let model = spec.expected();

            let cuts = cut_points(&schedule, model.len());
            let (rows, suspends) =
                run_with_suspends(|| (spec.build_store(), plan.clone()), &interner, &cuts)
                    .unwrap();

            prop_assert_eq!(suspends, cuts.len(), "expected one suspend per scheduled row");
            prop_assert_eq!(rows, model, "schedule {:?} changed the run", cuts);
        }
    }

    /// Seed a fjall DB with a spec's facts, in the spec's order.
    ///
    /// The ids are asserted to be exactly what the spec numbers them, which pins
    /// that the real per-predicate allocator and the generator's order agree —
    /// without that, a projected `FactRef` would diverge from the model while every
    /// row was otherwise right.
    fn seed_fjall(spec: &QueryAndStore, path: &std::path::Path) -> FjallDb {
        let db = FjallDb::open(path).expect("open");

        for (predicate, key, value, sequence) in spec.facts() {
            let id = db.put_fact(predicate, &key, &value).expect("put");
            assert_eq!(
                id,
                FactId::new(predicate, sequence).expect("spec fact id"),
                "the allocator diverged from the spec's fact order"
            );
        }

        db
    }

    proptest! {
        // A case builds a real DB — keyspace creation is fsync-bound at ~30 ms a
        // tree — so this is a small battery over the same shapes the cheap one
        // above covers exhaustively. What is under test here is the *store* beneath
        // a compiled plan.
        #![proptest_config(ProptestConfig::with_cases(16))]

        /// The same claim against **fjall**, because a compiled plan seeks
        /// differently than a hand-built one.
        ///
        /// Phase 1 licensed every executor battery to run on `MemStore` by showing
        /// the two stores agree on generated `(plan, store)` pairs — but those plans
        /// only ever seek by a whole spliced field from an empty prefix. Flatten
        /// emits constant prefixes, several-part composites and nested paths, so the
        /// range bounds a scan is opened with (and re-opened with, on resume) are
        /// shapes the differential has never seen on a real LSM store.
        #[test]
        fn a_compiled_plan_runs_the_same_on_fjall(
            spec in arb_query_and_store(),
            schedule in arb_interruption_schedule(),
        ) {
            let schema = spec.schema();
            let (plan, interner) = plan_of(&schema, &spec.source(), &spec.identity());
            let model = spec.expected();

            let dir = TempDir::new().expect("tempdir");
            let db = seed_fjall(&spec, dir.path());

            // Run to completion...
            let rows = collect_rows(db.reader(), plan.clone(), &interner).unwrap();
            prop_assert_eq!(&rows, &model, "fjall and the model disagree");

            // ...and again, suspending to bytes and resuming against a fresh
            // snapshot at every scheduled row.
            let cuts = cut_points(&schedule, model.len());
            let (resumed, suspends) =
                run_with_suspends(|| (db.reader(), plan.clone()), &interner, &cuts).unwrap();

            prop_assert_eq!(suspends, cuts.len(), "expected one suspend per scheduled row");
            prop_assert_eq!(resumed, model, "schedule {:?} changed the run on fjall", cuts);
        }
    }

    /// The generated population is asserted, because a property over a degenerate
    /// generator is green and vacuous. A draw that never joins, or never produces a
    /// row, would test the empty answer over and over.
    #[test]
    fn the_generator_is_not_degenerate() {
        use ::proptest::{
            strategy::{Strategy, ValueTree},
            test_runner::TestRunner,
        };

        const RUNS: usize = 200;

        let mut runner = TestRunner::deterministic();
        let mut multi_statement = 0;
        let mut with_rows = 0;
        let mut with_join = 0;
        let mut with_const = 0;
        let mut with_wildcard = 0;
        let mut rows_total = 0;

        for _ in 0..RUNS {
            let spec = arb_query_and_store()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let source = spec.source();
            let rows = spec.expected();

            if spec.statements() > 1 {
                multi_statement += 1;
            }
            if !rows.is_empty() {
                with_rows += 1;
            }
            rows_total += rows.len();
            // A variable named twice across the query is a join.
            if source.matches("V0").count() > 1
                || source.matches("V1").count() > 1
                || source.matches("V2").count() > 1
            {
                with_join += 1;
            }
            if source.contains('"') || source.contains(char::is_numeric) {
                with_const += 1;
            }
            if source.contains('_') {
                with_wildcard += 1;
            }

            let _ = &source;
        }

        assert!(
            multi_statement * 2 > RUNS,
            "{multi_statement}/{RUNS} queries have more than one statement"
        );
        assert!(
            with_rows * 2 > RUNS,
            "only {with_rows}/{RUNS} queries return a row"
        );
        assert!(with_join * 3 > RUNS, "only {with_join}/{RUNS} queries join");
        assert!(
            with_const * 2 > RUNS,
            "only {with_const}/{RUNS} queries match a constant"
        );
        assert!(
            with_wildcard * 3 > RUNS,
            "only {with_wildcard}/{RUNS} queries use a wildcard"
        );
        assert!(
            rows_total > RUNS * 2,
            "{rows_total} rows over {RUNS} queries is too thin"
        );
    }
}
