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
//!    each becomes one loop level holding one register. A fact pattern written
//!    *inside* another is a generator too, and is **hoisted** into a level of its
//!    own, bound to a name the query did not write; everything after this point
//!    sees an ordinary row bind.
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
//! **Read *through* a fact reference.** A fact-typed key field holds a `FactId`, and
//! that id is enough to *follow* a reference — [`SeekKeyPart::RegisterFactId`] splices
//! it, so a join through one costs no store read. Reaching the fact it names is the
//! other half: `X.name` or `X.value` where `X` came out of a reference field needs a
//! second lookup the `Plan` IR has no access kind for (`nyi/fact-field`). Getting the
//! first half wrong *silently* is the trap the split exists to close — a register
//! holds its own row's key bytes, which are not the referenced fact's, and comparing
//! them would give wrong answers rather than an error.
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
        Step,
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
    interner: &mut LocalInterner,
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
    interner: &mut LocalInterner,
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
    interner: &mut LocalInterner,
    diagnostics: &mut Diagnostics,
) -> Option<Deps> {
    let mut flattener = Flattener {
        ast,
        schema,
        interner,
        diagnostics,
        bindings: vec![],
        hoisted: vec![],
    };

    Some(flattener.collect()?.deps)
}

fn flatten_ordered(
    ast: &Ast,
    schema: &Schema,
    interner: &mut LocalInterner,
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
    interner: &mut LocalInterner,
    diagnostics: &mut Diagnostics,
    order: Option<&[usize]>,
) -> Option<Plan> {
    let mut flattener = Flattener {
        ast,
        schema,
        interner,
        diagnostics,
        bindings: vec![],
        hoisted: vec![],
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
    interner: &'a mut LocalInterner,
    diagnostics: &'a mut Diagnostics,
    /// Variable → where its value lives, as the levels are emitted in order.
    ///
    /// Append-only, and searched from the back: a variable is bound once, at its
    /// first occurrence in the chosen order, and every later occurrence reads it.
    bindings: Vec<(Symbol, Slot)>,
    /// Nested fact pattern → the row variable it was **hoisted** to.
    ///
    /// A generator written inside another has no name, and everything downstream —
    /// the dependency graph, the safety check, sargeability, projection — is written
    /// in terms of variables. Rather than give each of those a second code path, the
    /// hoist invents the name the user did not write and the rest of the pass sees an
    /// ordinary row bind.
    hoisted: Vec<(NodeId, Symbol)>,
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
                        self.hoist_within(generator.key, &mut stmts);
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
                            self.hoist_within(generator.key, &mut stmts);
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

        // The head last, and after every statement: a generator in the head is read by
        // the projection, which runs once every level has bound, and nothing reads it.
        let head = *self.ast.query().head();
        self.hoist_node(head, &mut stmts);

        // Which variables name a whole row. A bare variable at a *fact-typed* field
        // is capturable like any other — the field holds a reference and a reference
        // is a value — but if the same variable is a row somewhere, the field can only
        // be **matched against** that row, never bind it: the level would have to
        // find its own fact by id, which is a point access the plan cannot express.
        // Deciding it here, from the whole statement list, keeps it a property of the
        // query rather than of the order.
        let rows: Vec<Symbol> = stmts.iter().filter_map(|generator| generator.row).collect();

        for generator in &stmts {
            let mut occurrences = Occurrences::default();

            if let Some(row) = generator.row {
                occurrences.capture(row);
            }
            self.scan_key(generator.key, generator.predicate, &rows, &mut occurrences);

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

    // ---- hoisting -----------------------------------------------------------

    /// Hoist every fact pattern **inside** `node` into a generator of its own.
    ///
    /// A fact pattern denotes the facts matching it, so it is a generator wherever it
    /// is written — in a key field, in the head, under a field read. Only the one a
    /// statement *is* stays where it is; the rest become levels, appended here so that
    /// each precedes whatever named it.
    fn hoist_within(&mut self, node: NodeId, stmts: &mut Vec<Gen>) {
        match self.ast.store().kind(node) {
            ExprKind::Record(fields) => {
                for (_, value) in fields.clone().iter() {
                    self.hoist_node(*value, stmts);
                }
            }

            ExprKind::Access(_, base) | ExprKind::Select(_, base) => {
                self.hoist_node(*base, stmts);
            }

            // A fact pattern reached directly is the caller's own statement — or a
            // whole-key pattern, which `scan_key` reports. Either way it is not
            // hoisted from here; `hoist_node` is the entry point that does that.
            _ => {}
        }
    }

    /// [`hoist_within`](Self::hoist_within), and `node` itself if it is a generator.
    fn hoist_node(&mut self, node: NodeId, stmts: &mut Vec<Gen>) {
        let ExprKind::Fact(predicate, key) = *self.ast.store().kind(node) else {
            self.hoist_within(node, stmts);
            return;
        };

        // Innermost first: a generator nested inside this one has to be a level
        // *before* it, because this one's key reads what that one binds.
        self.hoist_within(key, stmts);

        let row = self.fresh(stmts.len());
        stmts.push(Gen {
            predicate,
            key,
            row: Some(row),
            span: self.ast.store().span(node),
        });
        self.hoisted.push((node, row));
    }

    /// A name for a hoisted row that no source can collide with: the lexer has no
    /// rule producing `%`, and no schema declares one.
    fn fresh(&mut self, level: usize) -> Symbol {
        self.interner.get_or_intern(&format!("%h{level}"))
    }

    /// The slot a hoisted generator's row is in, once its level has been emitted.
    fn hoisted_slot(&self, node: NodeId) -> Option<Slot> {
        let row = self
            .hoisted
            .iter()
            .find(|(hoisted, _)| *hoisted == node)
            .map(|(_, row)| *row)?;

        self.lookup(row)
    }

    /// The row variable a hoisted generator was given, for recording the read of it.
    fn hoisted_row(&self, node: NodeId) -> Option<Symbol> {
        self.hoisted
            .iter()
            .find(|(hoisted, _)| *hoisted == node)
            .map(|(_, row)| *row)
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
    fn scan_key(
        &mut self,
        node: NodeId,
        predicate: PredicateId,
        rows: &[Symbol],
        occurrences: &mut Occurrences,
    ) {
        let Some(key_ty) = self.schema.get(predicate).map(|p| p.key().ty.clone()) else {
            return;
        };

        match (&key_ty, self.ast.store().kind(node)) {
            (PredicateTy::Record(_), ExprKind::Record(_)) => {
                self.scan_field(node, &key_ty, rows, occurrences);
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
            (scalar, _) => self.scan_field(node, scalar, rows, occurrences),
        }
    }

    fn scan_field(
        &mut self,
        node: NodeId,
        ty: &PredicateTy,
        rows: &[Symbol],
        occurrences: &mut Occurrences,
    ) {
        match self.ast.store().kind(node) {
            ExprKind::Wildcard | ExprKind::Lit(_) | ExprKind::Prefix(_) => {}

            ExprKind::Var(symbol) => {
                // A reference field whose variable is a row elsewhere can only read
                // it; anything else is a capture. See `rows` in `collect`.
                if matches!(ty, PredicateTy::Fact(_)) && rows.contains(symbol) {
                    occurrences.read(*symbol);
                } else {
                    occurrences.capture(*symbol);
                }
            }

            ExprKind::Record(fields) => {
                let PredicateTy::Record(field_tys) = ty else {
                    return;
                };

                for (name, field_ty) in field_tys.iter() {
                    if let Some(pattern) = field_pattern(fields, Symbol::Schema(*name)) {
                        self.scan_field(pattern, field_ty, rows, occurrences);
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
                let root = self.chain_root(node);

                match self.ast.store().kind(root) {
                    ExprKind::Var(symbol) => occurrences.read(*symbol),
                    ExprKind::Fact(..) => self.read_hoisted(root, occurrences),
                    _ => {}
                }
            }

            // Hoisted into its own level by now, and read from here like the row bind
            // it became.
            ExprKind::Fact(..) => self.read_hoisted(node, occurrences),

            // Deferred constructs, all of which typecheck has already reported.
            ExprKind::Never
            | ExprKind::Disjunction(_)
            | ExprKind::Subquery(_)
            | ExprKind::Error => {}
        }
    }

    /// Reading *through* a reference — the half of cross-fact navigation that is
    /// still deferred.
    ///
    /// A reference may be captured, projected and matched, because all three are
    /// operations on the id itself. Reaching the fact it names — its key fields or
    /// its value — is a second lookup the `Plan` IR has no access kind for.
    fn report_through_reference(&mut self, node: NodeId, what: &str) {
        self.report(
            node,
            Code::NyiFactField,
            format!(
                "reading {what} through a fact reference is not implemented yet; it needs \
                 cross-fact navigation"
            ),
        );
    }

    /// Record the read of a hoisted generator's row.
    fn read_hoisted(&mut self, node: NodeId, occurrences: &mut Occurrences) {
        if let Some(row) = self.hoisted_row(node) {
            occurrences.read(row);
        }
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
                let root = self.chain_root(node);

                match self.ast.store().kind(root) {
                    ExprKind::Var(symbol) => occurrences.read(*symbol),
                    ExprKind::Fact(..) => self.read_hoisted(root, occurrences),
                    _ => self.not_projectable(node),
                }
            }

            ExprKind::Fact(..) => self.read_hoisted(node, occurrences),

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
            body: body.into_iter().map(Step::Scan).collect(),
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
                Some(slot) => self.matched(node, &slot, ty, address, path, level),
            },

            ExprKind::Access(..) | ExprKind::Select(..) | ExprKind::Fact(..) => {
                if let Some(slot) = self.resolve(node) {
                    self.matched(node, &slot, ty, address, path, level);
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
    ///
    /// `ty` is the field's **declared** type, which is what says whether the bytes
    /// there are a value or a reference — the one thing a register's contents cannot
    /// tell you.
    fn matched(
        &mut self,
        node: NodeId,
        slot: &Slot,
        ty: &PredicateTy,
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

            // **A join through a reference.** The field holds an id, and the bound
            // row *is* that id, so the compare is against the register's identity.
            // Its key bytes would be the wrong thing entirely — see
            // [`SeekKeyPart::RegisterFactId`].
            Slot::Row {
                address: from,
                predicate,
            } => match ty {
                PredicateTy::Fact(referenced) if referenced == predicate => {
                    if level.building {
                        level.parts.push(SeekKeyPart::RegisterFactId(*from));
                    } else {
                        level.residuals.push(Residual {
                            path: path.clone(),
                            op: ResidualOp::EqRegisterFactId(*from),
                        });
                    }
                }

                // A row where the field is not a reference to *its* predicate.
                // Typecheck unifies `Fact(p)` only with `Fact(p)`, so this is
                // unreachable — reported rather than declined so that no path can
                // refuse a plan without saying why.
                _ => self.report(
                    node,
                    Code::RejectTypeMismatch,
                    "this field does not hold a reference to that fact",
                ),
            },

            // Reported by `collect` too, which sees the field's declared type before
            // any of this; reported here for the same reason.
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

            // A hoisted generator, which by now is a level with a row of its own.
            ExprKind::Fact(..) => self.hoisted_slot(node),

            ExprKind::Access(FieldRef::Value, base) => {
                // Only a *row* has a value side that is one point read away. A
                // captured reference denotes a row too, but reaching its value means
                // finding the fact first — the deferred half.
                match self.resolve(*base)? {
                    Slot::Row { address, predicate } => {
                        let ty = self.schema.get(predicate)?.value()?.ty.clone();
                        Some(Slot::Value { address, ty })
                    }
                    Slot::Field {
                        ty: PredicateTy::Fact(_),
                        ..
                    } => {
                        self.report_through_reference(node, "the value");
                        None
                    }
                    // Typecheck rejects `.value` on anything else: a field's type has
                    // no value side, and a value's is not a fact.
                    _ => None,
                }
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
                        // Reading a field *of* a referenced fact is a second lookup.
                        PredicateTy::Fact(_) => {
                            self.report_through_reference(node, "a field");
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

            ExprKind::Var(_) | ExprKind::Access(..) | ExprKind::Fact(..) => {
                match self.resolve(node)? {
                    // A variable bound to a whole row projects its identity: the row
                    // itself is not bytes in the register, the fact id is.
                    Slot::Row { address, .. } => Some(Project::FactRef(address)),
                    Slot::Field { address, path, ty } => {
                        Some(Project::RegisterField { address, path, ty })
                    }
                    Slot::Value { address, ty } => Some(Project::Value { address, ty }),
                }
            }

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
        compile::Compilation,
        corpus,
        cst::CstNode,
        fixture,
        fixtures::{collect_rows, i64_field, run_with_suspends, str_field},
        lower::lower,
        mem_store::MemStore,
        parse::parse,
        plan::{FactId, Project, Residual, ResidualOp, SeekKey, SeekKeyPart},
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
            None => flatten(&ast, &schema, &mut interner, &mut diagnostics),
            Some(order) => flatten_in_order(&ast, &schema, &mut interner, &mut diagnostics, order),
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

        dependencies(&ast, &schema, &mut interner, &mut diagnostics).expect("a collectable query")
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

        for step in plan.body.iter() {
            let generator = match step {
                Step::Scan(generator) => generator,
                // A derived bind, which binds a value rather than a level.
                Step::Derive(derived) => {
                    out.push(format!("{} = <computed>", derived.bind));
                    continue;
                }
            };

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
                            // `r0#` — the row's identity, not any field of it.
                            SeekKeyPart::RegisterFactId(address) => format!("{address}#"),
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
                    ResidualOp::EqRegisterFactId(address) => format!("{path} == {address}#"),
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
            Project::Computed(address) => format!("{address}="),
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
        assert_eq!(
            plan.level(0).expect("a level").binds.as_ref(),
            [Address::new(0)]
        );
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
        match &plan.level(0).expect("a level").access.seek_key {
            SeekKey::Prefix(bytes) => assert_eq!(bytes.as_ref(), i64_field(1).as_slice()),
            other => panic!("expected a constant prefix, got {other:?}"),
        }
    }

    /// A scalar key is one field, so a constant against it is the whole seek.
    #[test]
    fn a_scalar_key_constant_is_the_whole_seek() {
        let flattened = compile("X where X = test.Count -42");

        match &flattened.plan().level(0).expect("a level").access.seek_key {
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
        match &plan.level(0).expect("a level").residuals[0].op {
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
        match &plan.level(0).expect("a level").access.seek_key {
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
        match &plan.level(0).expect("a level").residuals[0].op {
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

    // ---- reaching a fact through a reference ---------------------------------

    /// **A join through a reference.** The bound row's *identity* is what a
    /// fact-typed field holds, so the splice is its fact id — not its key bytes,
    /// which is the trap — and it narrows the scan like any other leading constant.
    /// No store read is involved, so [I6](../../../docs/invariants.md#i6) stays
    /// structural.
    #[test]
    fn a_bound_row_splices_its_fact_id_into_the_seek() {
        assert_eq!(
            shape("P where P = test.Foo {id = 1}; test.Ref {of = P}"),
            lines(&[
                "r0 <- test.Foo seek[k]",
                "r1 <- test.Ref seek[r0#]",
                "head r0",
            ]),
        );
    }

    /// The same compare once the seek prefix has closed: a capture at the leading
    /// field ends it, so the reference filters rows as they come instead.
    #[test]
    fn a_reference_after_an_open_field_becomes_a_residual() {
        assert_eq!(
            shape("{a = X} where P = test.Foo {id = 1}; test.Link {at = X, of = P}"),
            lines(&[
                "r0 <- test.Foo seek[k]",
                "r1 <- test.Link scan where 1 == r0#",
                "head {a = r1.0:int}",
            ]),
        );
    }

    /// A reference **captured**, which reads no second fact: the field's bytes are a
    /// fact id, and projecting them is a `Value::FactRef` naming the row.
    #[test]
    fn a_fact_typed_field_may_be_captured_and_projected() {
        assert_eq!(
            shape("X where test.Ref {of = X}"),
            lines(&["r0 <- test.Ref scan", "head r0.0:fact(0)"]),
            "`fact(0)` is `test.Foo` — the predicate the field is declared against",
        );
    }

    /// Two references to the same fact meet as **bytes**, with no fact id in the
    /// plan at all: a captured reference is a key field like any other, so the
    /// existing field compare is already the right operator.
    #[test]
    fn two_references_to_one_fact_compare_as_fields() {
        assert_eq!(
            shape("X where test.Ref {of = X}; test.Link {at = 1, of = X}"),
            lines(&[
                "r0 <- test.Ref scan",
                "r1 <- test.Link seek[k r0.0]",
                "head r0.0:fact(0)",
            ]),
        );
    }

    /// Which occurrence *captures* a reference depends on whether the variable is a
    /// row somewhere: `P = test.Foo …` binds a row, so `of = P` can only read it —
    /// and a read constrains the order, exactly as `Y.name` does.
    #[test]
    fn a_row_variable_at_a_reference_field_is_a_read() {
        let schema = corpus::schema();
        let deps = deps_of("P where P = test.Foo {id = 1}; test.Ref {of = P}");

        assert_eq!(deps.antichains(), Some(vec![vec![0], vec![1]]));
        assert!(deps.respects(&[0, 1]));
        assert!(!deps.respects(&[1, 0]));

        // Reorder being the identity, the other spelling has to be refused — and it
        // never reaches flatten to be refused *here*: binding a row variable that a
        // reference field has already mentioned is a `pattern = pattern`, which
        // typecheck defers.
        let mut compilation =
            Compilation::new("P where test.Ref {of = P}; P = test.Foo {id = 1}", &schema);
        assert!(compilation.plan().is_none());
        assert_eq!(
            compilation.diagnostics().codes().collect::<Vec<_>>(),
            ["nyi/bind-unification"],
        );
    }

    /// A reference field with no row behind it is a plain capture, so either
    /// statement may bind it and the order is free.
    #[test]
    fn two_reference_fields_sharing_a_variable_are_one_antichain() {
        let deps = deps_of("X where test.Ref {of = X}; test.Link {at = 1, of = X}");

        assert_eq!(deps.antichains(), Some(vec![vec![0, 1]]));
        assert!(deps.respects(&[0, 1]));
        assert!(deps.respects(&[1, 0]));
    }

    // ---- hoisting a nested generator ----------------------------------------

    /// A fact pattern written *inside* another is a generator of its own, so it
    /// becomes **its own loop level**, bound to a row nobody named, and the field it
    /// stood in matches that row's id.
    ///
    /// The hoisted level comes first: the field reads it, so it has to be bound by
    /// then — which is the same rule every other read follows.
    #[test]
    fn a_nested_fact_pattern_becomes_its_own_level() {
        assert_eq!(
            shape("X where X = test.Ref {of = test.Foo {id = 1}}"),
            lines(&[
                "r0 <- test.Foo seek[k]",
                "r1 <- test.Ref seek[r0#]",
                "head r1",
            ]),
        );
    }

    /// Hoisting is **recursive**, innermost first: each generator is a level before
    /// the one that names it, so a two-hop chain reads outwards.
    #[test]
    fn hoisting_nests() {
        assert_eq!(
            shape("X where X = test.Deep {via = test.Ref {of = test.Foo {id = 1}}}"),
            lines(&[
                "r0 <- test.Foo seek[k]",
                "r1 <- test.Ref seek[r0#]",
                "r2 <- test.Deep seek[r1#]",
                "head r2",
            ]),
        );
    }

    /// A hoisted generator is a pattern like any other, so it can capture — and what
    /// it captures is projectable.
    #[test]
    fn a_hoisted_generator_captures_its_own_fields() {
        assert_eq!(
            shape("X where test.Ref {of = test.Foo {name = X}}"),
            lines(&[
                "r0 <- test.Foo scan",
                "r1 <- test.Ref seek[r0#]",
                "head r0.1:str",
            ]),
        );
    }

    /// ...and it can *read* an outer capture, which orders it after the statement
    /// that binds one.
    #[test]
    fn a_hoisted_generator_may_read_an_outer_capture() {
        assert_eq!(
            shape("X where test.Node {id = X}; test.Ref {of = test.Foo {id = X}}"),
            lines(&[
                "r0 <- test.Node scan",
                "r1 <- test.Foo seek[r0.0]",
                "r2 <- test.Ref seek[r1#]",
                "head r0.0:int",
            ]),
            "the hoisted level lands between the statement it reads and the one that \
             names it",
        );
    }

    /// **A fact pattern in the head** is the same construct in the other position:
    /// hoisted into a level, and projected as the fact it names.
    #[test]
    fn a_fact_pattern_in_the_head_is_hoisted_too() {
        assert_eq!(
            shape("test.Bar {id = 1} where test.Foo _"),
            lines(&["r0 <- test.Foo scan", "r1 <- test.Bar seek[k]", "head r1",]),
            "the head's generator is the last level: it can read every capture, and \
             nothing reads it",
        );
        assert_eq!(
            shape("{a = test.Bar {id = 1}} where test.Foo _"),
            lines(&[
                "r0 <- test.Foo scan",
                "r1 <- test.Bar seek[k]",
                "head {a = r1}",
            ]),
        );
    }

    /// A field read *of* a hoisted generator, which is how one writes "the name of
    /// the fact matching this" without a second variable.
    ///
    /// Parenthesised because dot binds tighter than application: without the group
    /// this is `test.Foo ({id = 1}.name)`, and the field is looked for on the record.
    #[test]
    fn a_hoisted_generators_field_may_be_read() {
        assert_eq!(
            shape("(test.Foo {id = 1}).name where test.Bar _"),
            lines(&[
                "r0 <- test.Bar scan",
                "r1 <- test.Foo seek[k]",
                "head r1.1:str",
            ]),
        );
    }

    /// **Hoisting is exactly the rewrite it claims to be.** The nested spelling and
    /// the two-statement spelling of the same query compile to the *same plan*, down
    /// to which field seeks and which register each level reads.
    ///
    /// This is the whole warrant for hoisting being flatten-local: if the two agreed
    /// only on their answers, the nested form would be a second way of running a
    /// query. They agree on the plan, so it is a spelling.
    #[test]
    fn the_nested_spelling_is_the_two_statement_one() {
        assert_eq!(
            shape("X where X = test.Ref {of = test.Foo {id = 1}}"),
            shape("X where P = test.Foo {id = 1}; X = test.Ref {of = P}"),
        );
        assert_eq!(
            shape("X where test.Ref {of = test.Foo {name = X}}"),
            shape("X where P = test.Foo {name = X}; test.Ref {of = P}"),
        );
        assert_eq!(
            shape("X where X = test.Deep {via = test.Ref {of = test.Foo {id = 1}}}"),
            shape(
                "X where P = test.Foo {id = 1}; Q = test.Ref {of = P}; \
                 X = test.Deep {via = Q}"
            ),
        );

        // ...and the same rows, which is the claim a reader actually cares about.
        assert_eq!(
            rows("X where test.Ref {of = test.Foo {name = X}}"),
            rows("X where P = test.Foo {name = X}; test.Ref {of = P}"),
        );
    }

    /// The hoisted row is a **read** of the level it introduces, so the dependency
    /// graph says the order is forced rather than free.
    #[test]
    fn a_hoisted_generator_constrains_the_order() {
        let deps = deps_of("X where X = test.Ref {of = test.Foo {id = 1}}");

        assert_eq!(deps.len(), 2, "one statement became two levels");
        assert_eq!(deps.antichains(), Some(vec![vec![0], vec![1]]));
        assert!(deps.respects(&[0, 1]));
        assert!(!deps.respects(&[1, 0]));

        let flattened = compile_in_order("X where X = test.Ref {of = test.Foo {id = 1}}", &[1, 0]);
        assert_eq!(flattened.codes(), ["reject/unbound-variable"]);
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

    /// A reference may be **captured, projected and matched**; what stays deferred
    /// is reading *through* one, on either side of the fact it names.
    ///
    /// **The trap this closes:** a register holds its own row's key bytes, so
    /// splicing those where a fact id belongs would compare a key against an id and
    /// quietly match nothing. The splice is off `Register::fact_id` for exactly that
    /// reason.
    #[test]
    fn reading_through_a_reference_is_not_implemented_yet() {
        for source in [
            "X.name where test.Ref {of = X}",
            "X.value where test.Ref {of = X}",
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

    /// The shared [`fixture`](crate::focus::fixture)'s facts, in memory — the same
    /// rows the corpus gate runs against a real store and the same rows the shell
    /// serves, so a shape asserted here and an answer asserted there are about one
    /// database.
    fn store() -> MemStore {
        let mut store = MemStore::new();

        for fixture::Fact {
            predicate,
            key,
            value,
            sequence,
        } in fixture::facts()
        {
            store.insert_valued(predicate, key, sequence, value);
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

        // A string prefix, as a narrowed scan: `"ann"` and `"anna"`, not `"abc"`
        // before them or `"bob"` after.
        assert_eq!(rows("X where X = test.Name \"ann\"..").len(), 2);

        // A negative literal, which the seek has to encode order-preservingly.
        assert_eq!(rows("X.value where X = test.Foo _").len(), 3);
        assert_eq!(
            rows("Y where test.Count Y"),
            ints(&[i64::MIN, -42, 7, 1_000]),
            "a scalar key binds its one field",
        );

        // A record head.
        assert_eq!(
            rows("{a = X, b = Y} where test.Foo {id = X, name = Y}").len(),
            3
        );

        // A join *through a reference*, spliced as a fact id.
        assert_eq!(
            rows("P.name where P = test.Foo {id = 1}; test.Ref {of = P}"),
            strs(&["ann"]),
        );
        assert_eq!(
            rows("P.name where P = test.Foo {id = 3}; test.Ref {of = P}"),
            strs(&[]),
            "nothing references `(3, \"ann\")`",
        );

        // The same compare as a residual, behind an open field — and two referrers
        // to one fact, so the bound row comes back once per reference to it.
        assert_eq!(
            rows("P.name where P = test.Foo {id = 2}; test.Link {at = _, of = P}"),
            strs(&["bob", "bob"]),
        );
        assert_eq!(
            rows("X where P = test.Foo {id = 2}; test.Link {at = X, of = P}"),
            ints(&[11, 12]),
        );

        // A reference captured and projected, which names a fact rather than
        // reading it.
        let foo = |sequence| {
            let schema = corpus::schema();
            let predicate = schema.find_position("test.Foo").expect("test.Foo").0;
            Value::FactRef(FactId::new(predicate, sequence).expect("id"))
        };
        assert_eq!(rows("X where test.Ref {of = X}"), vec![foo(1), foo(2)]);

        // A nested generator, hoisted: the idiomatic spelling of the join above.
        assert_eq!(
            rows("X where test.Ref {of = test.Foo {name = X}}"),
            strs(&["ann", "bob"]),
            "`(3, \"ann\")` is referenced by nothing, so its name is not a row",
        );

        // Two hops, innermost first.
        assert_eq!(
            rows("X where test.Deep {via = test.Ref {of = test.Foo {name = X}}}"),
            strs(&["ann", "bob"]),
        );

        // A generator in the head, which is a level like any other — and one that
        // matches nothing empties the answer, because it is a level.
        assert_eq!(rows("test.Bar {id = 1} where test.Foo {id = 1}").len(), 1);
        assert_eq!(
            rows("test.Bar {id = 99} where test.Foo {id = 1}"),
            vec![],
            "no such `test.Bar` exists, so nothing survives the last level",
        );
    }

    // ---- Phase 6: derived binds (red, pending the `Slot` promotion) ---------
    //
    // Phase 6's acceptance criteria, as tests, written before the machine that
    // satisfies them ([`PLAN.md`](../../PLAN.md) Phase 6). They are deliberately
    // written **through the driver** — focus text in, rows out — and name no plan
    // type that does not exist yet, so they compile today, fail today for the
    // right reason (`nyi/value-bind`, reported by `collect`), and go green when
    // the feature lands rather than when a test is rewritten. That also means the
    // still-open question of *how* a derived bind sits in the `Plan` IR cannot be
    // pre-judged by its own acceptance test.
    //
    // Un-ignore each as its leaf lands; the ledger
    // (`cargo test -- --ignored --list`) is what says the phase is unfinished.

    /// The smallest derived bind there is: a variable bound to a value no
    /// generator produced.
    ///
    /// Also the shape with **no generator at all**, which is the edge the
    /// executor's `EmptyPlan` check currently rejects — a plan whose whole body is
    /// a derived bind has no loop level to iterate, and must still answer one row.
    #[test]
    #[ignore = "PLAN Phase 6 — derived binds; needs the Slot value variant"]
    fn a_value_bind_returns_the_value() {
        assert_eq!(rows("X where X = 42"), ints(&[42]));
        assert_eq!(rows("X where X = \"ann\""), strs(&["ann"]));
    }

    /// A derived bind **feeding a seek**: the value slot's bytes are spliced into
    /// the scan prefix exactly as an outer row's field would be.
    ///
    /// This is the case that decides the derived bind has to be computable
    /// *before* the level that reads it opens — the ordering constraint chapter 7
    /// calls "the hard topological ordering the reorder interface was built for".
    #[test]
    #[ignore = "PLAN Phase 6 — derived binds; needs the Slot value variant"]
    fn a_derived_bind_feeds_a_seek() {
        assert_eq!(rows("Z where Z = 1; test.Bar {id = Z}"), ints(&[1]));
        assert_eq!(
            rows("Z where Z = 99; test.Bar {id = Z}"),
            vec![],
            "no `test.Bar` has id 99, so the spliced seek finds nothing",
        );
        assert_eq!(
            rows("X where Z = 1; test.Edge {from = Z, to = X}"),
            ints(&[2, 3]),
            "edges (1,2) and (1,3), reached by a seek on a derived value",
        );
    }

    /// **The new invariant's guard: derived binds are pure functions of the fact
    /// bindings.**
    ///
    /// Chapter 7 specifies the guard as a resume battery, and that is not an
    /// approximation of purity — it is the operational content of it. A derived
    /// bind is not a loop level and the [`Cursor`](crate::focus::iter::Cursor)
    /// does not store it, so a resume *recomputes* it from the fact-slots it
    /// re-binds. If a derived bind were impure — if it iterated, or read state the
    /// cursor does not carry — the recomputed value would differ from the one the
    /// uninterrupted run had, and the row sequences would diverge at exactly the
    /// cut point.
    ///
    /// So this asserts resume == uninterrupted over plans *containing* a derived
    /// bind, at every cut point. The generated form comes with the query
    /// generator's derived-bind draw; these are the worked examples.
    #[test]
    #[ignore = "PLAN Phase 6 — the purity invariant; needs the Slot value variant"]
    fn a_derived_bind_is_recomputed_on_resume() {
        for source in [
            // Recomputed with no fact bindings to recompute it from.
            "X where X = 42",
            // Recomputed before the level whose seek splices it.
            "X where Z = 1; test.Edge {from = Z, to = X}",
            // Recomputed at a level *under* a fact binding, so the cut points fall
            // either side of a backtrack.
            "{a = X, b = Z} where test.Edge {from = X, to = _}; Z = 7",
        ] {
            let flattened = compile(source);
            let plan = flattened.plan().clone();
            let interner = &flattened.interner;

            let model = collect_rows(store(), plan.clone(), interner).expect("run");
            assert!(
                !model.is_empty(),
                "{source:?} must produce rows for a cut point to mean anything",
            );

            for k in 1..=model.len() {
                let mut mk = || (store(), plan.clone());
                let cuts = std::collections::BTreeSet::from([k]);
                let (rows, suspends) = run_with_suspends(&mut mk, interner, &cuts).expect("resume");

                assert_eq!(suspends, 1, "{source:?}: schedule {{{k}}} never suspended");
                assert_eq!(
                    rows, model,
                    "{source:?}: suspending after row {k} changed the run — the \
                     derived bind did not recompute to the same value",
                );
            }
        }
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
        tuple::{MARK_RECORD, MARK_TERM, Value, fact_ref_bytes},
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

    /// The predicate a **fact-typed field** points at. Always the first one, and only
    /// the *others* may have such a field, so the reference graph is acyclic by
    /// construction: `gen.P0` is the referenced predicate and never a referrer.
    ///
    /// Cycles are not wrong — a fact database is full of them — but a generator that
    /// drew them would have to draw facts in dependency order to keep every reference
    /// resolvable, and nothing here needs that to reach the plan shapes.
    const REFERENCED: PredicateId = PredicateId(0);

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

    /// A generated key field's type: a scalar, a record of scalars, or a **reference**
    /// to a fact of [`REFERENCED`].
    #[derive(Debug, Clone)]
    enum GenTy {
        Scalar(FieldTy),
        Record(Vec<FieldTy>),
        Ref,
    }

    /// A value of a [`GenTy`].
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum GenVal {
        Scalar(FieldVal),
        Record(Vec<FieldVal>),
        /// A reference to fact number `n` of [`REFERENCED`] — the *sequence*, since a
        /// whole [`FactId`] is that plus which predicate it belongs to.
        Ref(u64),
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
                GenVal::Ref(sequence) => fact_ref_bytes(self.fact_id(*sequence)).to_vec(),
            }
        }

        /// The whole id a reference sequence names.
        fn fact_id(&self, sequence: u64) -> FactId {
            FactId::new(REFERENCED, sequence).expect("a spec fact id")
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
                GenVal::Ref(sequence) => Value::FactRef(self.fact_id(*sequence)),
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
                // A reference has no literal spelling — it names a fact, and focus has
                // no syntax for a fact id. `resolve_leaf` never draws a constant at a
                // reference position for exactly that reason.
                GenVal::Ref(_) => unreachable!("a fact reference is never written as a constant"),
            }
        }

        fn scalar(&self) -> Option<&FieldVal> {
            match self {
                GenVal::Scalar(val) => Some(val),
                GenVal::Record(_) | GenVal::Ref(_) => None,
            }
        }

        /// The fact this value references, if it is one.
        fn reference(&self) -> Option<u64> {
            match self {
                GenVal::Ref(sequence) => Some(*sequence),
                _ => None,
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
        /// A **row variable at a reference field** — `f1 = R0`, the join through a
        /// reference. Only ever a read: the row is bound elsewhere, and this field
        /// holds its id.
        ///
        /// The one leaf that constrains the order, which is why [`orders`] exists
        /// rather than every permutation being valid.
        ///
        /// [`orders`]: QueryAndStore::orders
        Row(usize),
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
                                    GenTy::Ref => PredicateTy::Fact(REFERENCED),
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

        /// Every **safe** order of the body.
        ///
        /// Every permutation, except where a reference field names a row: `f1 = R0`
        /// reads `R0`, so the statement binding it has to come first. Field variables
        /// impose no such constraint — either occurrence may capture — which is why
        /// this is the identity filter it is and not a topological sort.
        pub fn orders(&self) -> Vec<Vec<usize>> {
            permutations(&self.identity())
                .into_iter()
                .filter(|order| self.respects(order))
                .collect()
        }

        /// Whether `order` binds every row before a reference field reads it.
        fn respects(&self, order: &[usize]) -> bool {
            let mut bound: Vec<usize> = vec![];

            for &stmt in order {
                let spec = &self.stmts[stmt];

                for pat in &spec.fields {
                    if let FieldPat::Leaf(Leaf::Row(row)) = pat
                        && !bound.contains(row)
                    {
                        return false;
                    }
                }

                bound.extend(spec.row);
            }

            true
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
            Leaf::Row(row) => Some(format!("R{row}")),
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

            // The field references a fact; the row variable is bound to one. They
            // match when they are the same fact — which the model states as the
            // *identity* it is, never as the key bytes.
            Leaf::Row(row) => match (value.reference(), env.rows[*row]) {
                (Some(sequence), Some((predicate, index))) => {
                    PredicateId(predicate as u32) == REFERENCED && index as u64 + 1 == sequence
                }
                _ => false,
            },
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

    /// A whole **record** field's constant, taken from a fact that actually has it so
    /// the statement matches something. Falls back to the domain for an empty
    /// predicate.
    ///
    /// Record-only: a scalar's constant comes from [`resolve_leaf`], and a reference
    /// has no literal to be a constant of.
    fn constant_for(facts: &[Fact], field: usize, subs: &[FieldTy], pick: u8) -> GenVal {
        match facts.len() {
            0 => GenVal::Record(
                subs.iter()
                    .map(|scalar| FieldVal::of(*scalar, pick))
                    .collect(),
            ),
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

    /// A reference position: unconstrained, or **this bound row**.
    ///
    /// Weighted so that two draws in three name a row where one is available — the
    /// splice and its residual form are what the census is here to reach, and an
    /// unconstrained reference field reaches neither.
    fn resolve_ref_leaf(draw: &LeafDraw, referencing: &[usize]) -> Leaf {
        match (draw.kind % 3, referencing.len()) {
            (_, 0) | (0, _) => Leaf::Wildcard,
            (_, len) => Leaf::Row(referencing[draw.var as usize % len]),
        }
    }

    fn resolve(
        npredicates: usize,
        predicates: Vec<PredicateDraw>,
        facts_drawn: Vec<Vec<Vec<u8>>>,
        var_tys: Vec<u8>,
        stmts: Vec<StmtDraw>,
        heads: Vec<HeadDraw>,
    ) -> QueryAndStore {
        let schema: Vec<PredSpec> = predicates
            .iter()
            .take(npredicates)
            .enumerate()
            .map(|(p, draw)| PredSpec {
                fields: (0..draw.arity)
                    .map(|f| {
                        // Every predicate other than the referenced one ends its key
                        // with a **reference**, rather than drawing for it. Left to a
                        // draw, a reference join needs four independent coincidences
                        // (two predicates, a reference field, a statement over the
                        // referrer, and an earlier row bound over the referenced
                        // predicate) and the census reached the residual form twice in
                        // 300 runs. Last rather than first, so an open field can
                        // precede it: leading is the seek splice, behind an open field
                        // is its residual — and a one-field key gives the leading case.
                        if PredicateId(p as u32) != REFERENCED && f + 1 == draw.arity {
                            return GenTy::Ref;
                        }

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

        // Built in predicate order rather than mapped, because a reference field has
        // to name a fact that *exists*: `REFERENCED` is predicate 0, so its facts are
        // already settled by the time a referrer's are drawn. A dangling reference is
        // a legal database state, but one drawn at random would make every join
        // through a reference empty and the battery would exercise nothing.
        let mut facts: Vec<Vec<Fact>> = Vec::with_capacity(schema.len());

        for (spec, drawn) in schema.iter().zip(facts_drawn) {
            let referenced = facts.first().map_or(0, Vec::len);

            let mut built: Vec<Fact> = drawn
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
                            // Sequences are 1-based ([I11]), so this is the pick
                            // resolved over however many facts predicate 0 has.
                            GenTy::Ref => {
                                GenVal::Ref(picks[f] as u64 % referenced.max(1) as u64 + 1)
                            }
                        })
                        .collect(),
                    value: None,
                })
                .collect();

            // One key, one fact — a repeated draw would otherwise shadow an
            // earlier fact.
            built.sort();
            built.dedup();

            // The value follows from the fact's position, so it needs no draw
            // of its own and cannot make two facts differ only in their value.
            for (i, fact) in built.iter_mut().enumerate() {
                fact.value = spec.value.map(|ty| FieldVal::of(ty, i as u8));
            }

            facts.push(built);
        }

        let var_tys: Vec<FieldTy> = var_tys.iter().map(|&pick| FieldTy::of(pick)).collect();

        let mut used_vars = BTreeSet::new();
        let mut bound_rows: Vec<Option<usize>> = vec![];
        let mut resolved: Vec<StmtSpec> = Vec::with_capacity(stmts.len());

        for draw in &stmts {
            let predicate = draw.predicate as usize % schema.len();
            let spec = &schema[predicate];

            // A row variable is bound by at most one statement: binding one twice
            // is `nyi/bind-unification`.
            //
            // A statement over `REFERENCED` always binds one, where the draw would
            // otherwise leave it anonymous: a row variable is the **only** way a query
            // can name a fact, so a reference join cannot be drawn at all unless the
            // referenced fact is bound somewhere. Without this the census reached a
            // fact-id splice in 2% of runs and its residual form in none.
            let wants_row = draw.row % 2 != 0 || PredicateId(predicate as u32) == REFERENCED;
            let row = wants_row
                .then(|| (0..ROWS).find(|r| !bound_rows.contains(&Some(*r))))
                .flatten();
            bound_rows.push(row);

            // The rows a reference field in *this* statement may name: bound by an
            // earlier statement (so the identity order is safe) and over the predicate
            // references point at. `bound_rows` already has this statement's own row
            // appended, which is why the zip stops one short of it.
            let referencing: Vec<usize> = bound_rows
                .iter()
                .zip(&resolved)
                .filter_map(|(row, spec): (&Option<usize>, &StmtSpec)| {
                    row.filter(|_| PredicateId(spec.predicate as u32) == REFERENCED)
                })
                .collect();

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

                    // A reference field. There is no literal for a fact id, so the
                    // only patterns are "don't constrain it" and "it is this bound
                    // row" — which is the whole point: the row's id is the only way
                    // to name a fact in a query.
                    GenTy::Ref => FieldPat::Leaf(resolve_ref_leaf(&draw.leaf, &referencing)),

                    // A record field: matched whole as a constant (which can extend
                    // a seek prefix), or field by field (which cannot, and puts
                    // nested paths in the residuals).
                    GenTy::Record(subs) if draw.whole => FieldPat::Leaf(Leaf::Const(constant_for(
                        &facts[predicate],
                        f,
                        subs,
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
            FactId, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart, Step,
            proptest::{arb_interruption_schedule, cut_points},
        },
        schema::{LocalInterner, PredicateTy, Schema},
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

        let plan = flatten_in_order(&ast, schema, &mut interner, &mut diagnostics, order);

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
        fact_id_splice: bool,
        fact_id_residual: bool,
        reference_capture: bool,
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
                (self.fact_id_splice, "a `SeekKeyPart::RegisterFactId`"),
                (self.fact_id_residual, "a `ResidualOp::EqRegisterFactId`"),
                (
                    self.reference_capture,
                    "a captured reference (`Project::RegisterField` of a `Fact` type)",
                ),
            ] {
                if !present {
                    out.push(what);
                }
            }

            out
        }

        fn observe(&mut self, plan: &Plan) {
            for step in plan.body.iter() {
                // The census is about the shapes a *scan* can take; a derive step
                // has no seek and no residuals. When the generator learns to draw
                // one, it gets its own census entry rather than being folded in
                // here, since "reached a derive step" is a different claim.
                let Step::Scan(generator) = step else {
                    continue;
                };

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
                                SeekKeyPart::RegisterFactId(_) => self.fact_id_splice = true,
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
                        ResidualOp::EqRegisterFactId(_) => self.fact_id_residual = true,
                        ResidualOp::EqConst(_) => {}
                    }
                }
            }

            self.observe_head(&plan.head);
        }

        fn observe_head(&mut self, head: &Project) {
            match head {
                // A derived bind's output. Not a census entry yet: the query
                // generator draws no derived binds, so claiming coverage of one
                // would be claiming what nothing checks.
                Project::Computed(_) => {}
                Project::Value { .. } => self.value_projection = true,
                Project::FactRef(_) => self.fact_ref_projection = true,
                Project::RegisterField { path, ty, .. } => {
                    self.nested_path |= !path.is_flat();
                    self.reference_capture |= matches!(ty, PredicateTy::Fact(_));
                }
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
        let mut through_a_reference = 0;

        for _ in 0..RUNS {
            let spec = arb_query_and_store()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let source = spec.source();
            let rows = spec.expected();

            // The census says a reference join is *reached*; this says how often, so
            // the batteries that run over these draws are known not to be relying on
            // a handful of cases. It needed the generator to reserve a key field for
            // a reference to get this far — left to chance it was under 1%.
            let mut shapes = Shapes::default();
            shapes.observe(&plan_of(&spec.schema(), &source, &spec.identity()).0);
            if shapes.fact_id_splice || shapes.fact_id_residual {
                through_a_reference += 1;
            }

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
        assert!(
            through_a_reference * 25 > RUNS,
            "only {through_a_reference}/{RUNS} queries follow a reference"
        );
    }
}
