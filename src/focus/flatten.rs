//! flatten — the typed query becomes the [`Plan`] the executor runs.
//!
//! The last front-end phase and the one the two halves of the system meet at
//! ([chapter 7]). It takes the typed tree and produces an ordered `[Level]`
//! plus a `head: Project`, which is the fixed contract
//! ([chapter 4](../../../docs/04-executor.md)); everything after this point is the
//! executor's.
//!
//! Four things happen here, in this order, and the order is the design:
//!
//! 1. **Collect** the statements. A statement is a fact pattern — `test.Foo {…}`,
//!    optionally bound to a variable by `X = test.Foo {…}` — and each becomes one
//!    loop level holding one register. A fact pattern written *inside* another is a
//!    generator too, and is **hoisted** into a level of its own, bound to a name the
//!    query did not write; everything after this point sees an ordinary row bind.
//!    Two kinds of statement iterate nothing and are settled here: a **substitution**
//!    (a folded constant, or an alias naming a place) and a **constraint** — `X =
//!    "a"..`, a pattern the value wherever `X` lives has to match.
//! 2. **Safety.** Every variable a seek, residual or the head *reads* must be
//!    **captured** by some generator's key pattern. That is the whole of what
//!    correctness needs — see [`reorder`](crate::focus::reorder) for why it is not
//!    an ordering problem — and a query with an uncaptured variable is rejected
//!    (`reject/unbound-variable`), not answered.
//! 3. **Reorder**, over the dependency graph collect built: any order that binds
//!    before it reads gives the same answer, so which one is a performance choice
//!    among the safe ones rather than a correctness question.
//! 4. **Sargeability**, walking each level's key fields *in the chosen order* and
//!    deciding, per field, whether it narrows the scan (a **seek**), is filled from
//!    a register bound at an outer level (a **splice**), or filters rows as they
//!    come (a **residual**). This is order-dependent — a variable being captured
//!    cannot seek, because it is an output, unless a constraint says what the output
//!    has to look like — which is why it runs after the order is fixed rather than
//!    before.
//!
//! # What it does not do
//!
//! **Read through a reference held in a fact's *value*.** Following a reference is a
//! compare ([`SeekKeyPart::RegisterFactId`], no store read) and reading through one
//! is a [`Source::Fetch`] level, so both halves work — but a fetch reads the id out
//! of a register's *key* bytes, and a value is in the other column family
//! (`nyi/fact-field`).
//!
//! **Bind a value that is in no register.** `X = {a = 1, b = Y}` mentions a captured
//! variable, so it differs per row and would have to be *built*. That is a derived
//! bind ([`Step::Derive`]), and nothing in focus lowers one yet (`nyi/value-bind`).
//! A constant folds and a field read is an alias; neither is this.
//!
//! **Match on a value.** A value lives in `entities`, and [I6] keeps `entities`
//! out of the scan loop, so `.value` may be projected but not matched
//! (`nyi/value-match`).
//!
//! **Match a whole key into a record field.** A stored key is its fields back to
//! back with no wrapper ([chapter 3]) and a record *field* keeps its wrapper, so the
//! same record is not the same bytes (`nyi/whole-key`). Binding a whole key works —
//! it decomposes into the per-field questions.
//!
//! **Push a pattern into something that is not a target.** `test.Foo {…} = test.Bar
//! {…}` and `Y.name = X` have no variable to bind, so they are not binds at all
//! (`nyi/bind-unification`), and a variable at two key fields of one row needs a
//! same-row residual the executor has no operator for (`nyi/repeated-variable`).
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
        Access, FieldPath, Level, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart,
        Source, Step, Test,
    },
    reorder::{Deps, Placement, StmtDeps, reorder},
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
    ///
    /// `predicate` is `None` for `X = never`: the level has no alternative, so no
    /// row ever reaches the register and there is no predicate the row is *of*.
    /// The distinction only matters to a reader of the row's fields, which is why
    /// [`field_slot`](Flatten::field_slot) is the one place that has to answer for
    /// it.
    Row {
        address: Address,
        predicate: Option<PredicateId>,
    },
    /// A row's **whole key** — `test.Foo Y`, where `Y` is every key field at once.
    ///
    /// Distinct from [`Row`](Slot::Row), which is the *fact*: `Y = test.Foo …`
    /// binds the thing a reference points at, and projects as an id, while this
    /// binds what the key says and projects as a record. Both name the same
    /// register.
    ///
    /// It needs no plan support, because [a stored key is
    /// flat](../../../docs/03-storage-model.md#a-stored-key-is-flat): its top-level
    /// fields sit back to back, so splicing every field in declared order
    /// reconstructs exactly the bytes of the whole key, and projecting is a record
    /// over those fields. Under a wrapped layout neither would be true — the
    /// wrapper's markers are not any field's bytes.
    Key { address: Address, ty: PredicateTy },
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
    /// A **constant**, from `X = 42` — held as the literal's own node rather than
    /// as bytes or a decoded value, so every use resolves by *substitution*: a key
    /// field asks [`constant`](Flatten::constant) and a head asks
    /// [`project`](Flatten::project), each reaching the same arm it would have
    /// reached had the literal been written in place.
    ///
    /// So a constant bind occupies **no register and no plan step**. The machine
    /// has a derived-bind step ([`Step::Derive`]) and this deliberately does not
    /// use it: introducing a slot to hold a value already known at compile time
    /// would be a level for the executor to walk and a value for a resume to
    /// recompute, both to arrive back at the literal. The step exists for a value
    /// that *cannot* be folded — one computed from a row — and nothing in the
    /// language produces one yet.
    Const(NodeId),
}

/// One **alternative** of a generator: a predicate, and the key pattern
/// sargeability walks for it once the order is fixed.
#[derive(Debug, Clone)]
struct Alt {
    predicate: PredicateId,
    key: NodeId,
}

/// One statement, as a generator-to-be — before an order is chosen, so before any
/// register is assigned.
///
/// `alternatives` is the statement's branches, and the count is the construct, as
/// it is for the [`Level`] this becomes: none is `never`, one is an ordinary fact
/// pattern, and several is a disjunction.
#[derive(Debug, Clone)]
struct Gen {
    alternatives: Box<[Alt]>,
    /// The variable the whole row binds, from `X = test.Foo …`.
    row: Option<Symbol>,
    span: NodeSpan,
    placement: Placement,
}

impl Stmt {
    /// Whether the query wrote this statement, or flatten invented it.
    ///
    /// A hoisted generator is the name the query did not write, and an alias
    /// emits no level at all — neither has a position a person chose.
    fn placement(&self) -> Placement {
        match self {
            Stmt::Scan(generator) | Stmt::Negate(generator) => generator.placement,
            Stmt::Alias(_) | Stmt::Constrain(_) => Placement::Floating,
        }
    }
}

/// A **name for a value that is already somewhere** — `Y = X.name`.
///
/// Not a level and not a computation: the right side denotes a [`Slot`], and the
/// statement binds the left side to it. So an alias occupies no register and emits
/// no [`Step`], exactly as a folded constant does — the difference is only that
/// *which* slot it names depends on the order, since the register it points into
/// is assigned when the level that owns it is emitted.
#[derive(Debug, Clone)]
struct Alias {
    /// The pattern being named — a variable or a wildcard today, a record once
    /// destructuring is general.
    pattern: NodeId,
    /// The expression whose location it names.
    value: NodeId,
    span: NodeSpan,
}

/// A **pattern the value at a place has to match** — `X = "a"..`.
///
/// The third thing a bind's right side can be, beside a generator and a value. A
/// string prefix denotes a *range*, so there is nothing for `X` to be bound to and
/// nothing to substitute at `X`'s uses: what it says is that wherever `X` already
/// lives, the bytes there start with these. So it binds nothing, takes no register
/// and emits no [`Step`] — it narrows the level that binds `X` instead.
///
/// It is a statement all the same, because it **reads** `X`, and the safety check
/// is what turns "constrains a variable nothing binds" into a diagnostic rather
/// than a constraint quietly dropped. Where it sits in the order does not matter:
/// [`constraints`](Flattener::constraints) is collected from the whole body before
/// any statement is lowered, exactly as the constant fold is, so the level that
/// captures `X` sees it whenever that level runs.
#[derive(Debug, Clone)]
struct Constraint {
    /// The variable being constrained — the only thing the *order* has to know,
    /// since it is the statement's one read.
    ///
    /// The pattern itself is not here: it belongs to
    /// [`constraints`](Flattener::constraints), which is keyed by variable because
    /// that is what the level applying it has in hand — a key field knows the
    /// variable it is capturing and nothing about which statement mentioned it.
    var: NodeId,
    span: NodeSpan,
}

/// One statement before an order is chosen: a level to iterate, a substitution, or
/// a pattern something already bound has to match.
///
/// One sequence rather than three collections, for the reason [`Step`] gives: an
/// order is a single thing, and collections joined by an index would be several
/// sources of truth for it.
#[derive(Debug, Clone)]
enum Stmt {
    Scan(Gen),
    /// A statement whose rows must **not** exist — `!test.Bar {id = X}`.
    ///
    /// The very same [`Gen`] a scan is built from, because a fact pattern is a
    /// generator wherever it is written and its seek is built the same way. What
    /// differs is only what the machine does with the rows it finds, and that is a
    /// [`Test`] rather than a level: no register, no row, and every variable it
    /// names a **read**.
    ///
    /// `row` is therefore always `None`. There is no row to name — `X = !test.Bar
    /// {…}` is not expressible, since `!` prefixes a statement.
    Negate(Gen),
    Alias(Alias),
    Constrain(Constraint),
}

impl Stmt {
    fn span(&self) -> NodeSpan {
        match self {
            Stmt::Scan(generator) | Stmt::Negate(generator) => generator.span.clone(),
            Stmt::Alias(alias) => alias.span.clone(),
            Stmt::Constrain(constraint) => constraint.span.clone(),
        }
    }
}

/// The variables some statement has already **claimed** — said what they are,
/// rather than offering to bind them.
///
/// A claim is not an ordering question, so `reorder` must never be handed one: if
/// two statements could both bind a variable the plan would quietly keep whichever
/// ran second. Every *other* occurrence of a claimed variable is therefore a read,
/// which is what forces the claiming statement to run first.
#[derive(Debug, Default)]
struct Claims {
    /// Variables bound to a whole row, from `X = test.Foo …`.
    rows: Vec<Symbol>,
    /// Variables an [`Alias`] names.
    aliased: Vec<Symbol>,
    /// Variables some fact pattern's key can **capture**.
    ///
    /// Not a claim — several statements may offer to capture one variable and the
    /// order picks which does — but the one thing that tells `X = Y` apart from
    /// an alias: if a key can bind `X`, then `X = Y` compares two bound values
    /// rather than giving `Y` a second name.
    capturable: Vec<Symbol>,
}

/// Two places whose values have to be **equal per row** — `X = Y` with both
/// already bound.
///
/// Held until every level exists, because the constraint belongs to whichever of
/// the two binds *later*: a residual is checked against the row a level is
/// scanning, so comparing against a register only means anything once that
/// register is filled.
#[derive(Debug, Clone)]
struct Compare {
    left: Slot,
    right: Slot,
    at: NodeId,
}

/// What flatten works out *before* an order is chosen: the statements, the
/// dependency graph over them, and the variables the head reads.
#[derive(Debug)]
struct Collected {
    stmts: Vec<Stmt>,
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

/// The plan body under construction: the steps in order, and the count of those
/// that are **levels**.
///
/// The count is the whole reason this is a type rather than a `Vec`. A register
/// address is the position of the level that fills it, and only a level fills one —
/// so once a step can be a derive or a [`Test`], `steps.len()` and the next address
/// are two different numbers. They were the same number for as long as every step
/// was a level, which is exactly the shape of arithmetic that goes wrong silently:
/// an address one too high names a register no level ever binds, and a seek splices
/// whatever the executor finds there.
///
/// It is the same distinction [`Plan::levels`] draws for a finished plan, held here
/// while the plan is being built.
struct Body {
    steps: Vec<Step>,
    levels: usize,
}

impl Body {
    fn new(capacity: usize) -> Self {
        Self {
            steps: Vec::with_capacity(capacity),
            levels: 0,
        }
    }

    /// The address the **next** level will bind — which is also an address no
    /// existing register has, and so the one a test can safely be walked against.
    fn next_address(&self) -> Address {
        Address::new(self.levels)
    }

    /// Append a level, returning the register it binds.
    fn push_level(&mut self, level: Level) -> Address {
        let address = self.next_address();
        self.steps.push(Step::Level(level));
        self.levels += 1;
        address
    }

    fn push_test(&mut self, test: Test) {
        self.steps.push(Step::Test(test));
    }

    /// The level that binds `address`, to add a residual to it.
    ///
    /// By position among the *levels*, which is what an address counts.
    fn level_mut(&mut self, address: Address) -> Option<&mut Level> {
        self.levels_mut().nth(address.index())
    }

    fn levels_mut(&mut self) -> impl Iterator<Item = &mut Level> {
        self.steps.iter_mut().filter_map(|step| match step {
            Step::Level(level) => Some(level),
            Step::Derive(_) | Step::Test(_) => None,
        })
    }
}

/// The seek and residuals of one level, built field by field.
struct SeekBuilder {
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

impl SeekBuilder {
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
        compares: vec![],
        hoisted: vec![],
        fetched: vec![],
        constraints: vec![],
        constrained: vec![],
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
        compares: vec![],
        hoisted: vec![],
        fetched: vec![],
        constraints: vec![],
        constrained: vec![],
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
    /// Equalities between two already-bound places, applied once every level
    /// exists — see [`Compare`].
    compares: Vec<Compare>,
    /// Nested fact pattern → the row variable it was **hoisted** to.
    ///
    /// A generator written inside another has no name, and everything downstream —
    /// the dependency graph, the safety check, sargeability, projection — is written
    /// in terms of variables. Rather than give each of those a second code path, the
    /// hoist invents the name the user did not write and the rest of the pass sees an
    /// ordinary row bind.
    hoisted: Vec<(NodeId, Symbol)>,
    /// A **reference already followed** → the register the fact it names is in.
    ///
    /// Keyed by the reference's *place* rather than by the expression that named
    /// it, because that is what decides whether two reads are the same fetch:
    /// `X.file.name` and `X.file.line` are two nodes and one point read, and a
    /// second level for the second read would fetch the same row twice per row of
    /// `X`. A register holds one row and a path is fixed, so the pair names one
    /// reference for the whole plan.
    fetched: Vec<(Address, FieldPath, Address)>,
    /// Variable → a **pattern its value has to match**, from `X = "a"..`.
    ///
    /// Collected from the whole body before an order is chosen, exactly as the
    /// constant fold is and for the same reason: the level that captures the
    /// variable applies it, and which level that is depends on the order. A
    /// variable may carry more than one — every one of them has to hold.
    constraints: Vec<(Symbol, NodeId)>,
    /// The variables whose constraints a **capture** has already applied, so the
    /// pass over what is left does not apply them twice.
    constrained: Vec<Symbol>,
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
        let mut stmts: Vec<Stmt> = vec![];

        for stmt in self.ast.query().body() {
            match stmt {
                // **A subquery inlines.** Its statements are the enclosing query's,
                // and its head is the value the bind names — which is why the
                // grammar makes group and subquery one rule: a subquery is shaped
                // like a query, so lowering reuses the query algebra rather than
                // needing an operator.
                QueryStmt::Bind(lhs, rhs)
                    if matches!(self.ast.store().kind(*rhs), ExprKind::Subquery(_)) =>
                {
                    let ExprKind::Subquery(query) = self.ast.store().kind(*rhs) else {
                        continue;
                    };

                    // Copied out rather than borrowed: the statements live in the
                    // tree, and inlining them calls back into `self`. Neither
                    // `Query` nor `QueryStmt` is `Clone` on purpose — ownership
                    // signals sharing here — so this says what it copies.
                    let head = *query.head();
                    let body: Vec<QueryStmt<NodeId>> = query
                        .body()
                        .iter()
                        .map(|stmt| match stmt {
                            QueryStmt::Implicit(node) => QueryStmt::Implicit(*node),
                            QueryStmt::Bind(lhs, rhs) => QueryStmt::Bind(*lhs, *rhs),
                            QueryStmt::Negation(node) => QueryStmt::Negation(*node),
                        })
                        .collect();

                    if self.subquery_shadows(&body, *rhs) {
                        continue;
                    }

                    self.inline(&body, &mut stmts);

                    stmts.push(Stmt::Alias(Alias {
                        pattern: *lhs,
                        value: head,
                        span: self.ast.store().span(*rhs),
                    }));
                }

                QueryStmt::Implicit(node) => {
                    if let Some(generator) = self.generator(*node, None) {
                        for alt in generator.alternatives.clone().iter() {
                            self.hoist_within(alt.key, &mut stmts);
                        }
                        stmts.push(Stmt::Scan(generator));
                    }
                }

                QueryStmt::Bind(lhs, rhs) => {
                    // Typecheck accepts a bind only where the left side is a
                    // variable or a wildcard, so this is the whole of what it can be.
                    // The variable need not be one the query mentions here first —
                    // binding a row a field already named is an ordering question,
                    // and the duplicate-row check below is what it is *not*.
                    let row = match self.ast.store().kind(*lhs) {
                        ExprKind::Var(symbol) => Some(*symbol),
                        _ => None,
                    };

                    // A generator on the right — a fact pattern, a disjunction of
                    // them, or `never`. All three bind the left side to a *row* of
                    // the level they become, which is why they share this arm.
                    if matches!(
                        self.ast.store().kind(*rhs),
                        ExprKind::Fact(..) | ExprKind::Disjunction(_) | ExprKind::Never
                    ) {
                        if let Some(generator) = self.generator(*rhs, row) {
                            for alt in generator.alternatives.clone().iter() {
                                self.hoist_within(alt.key, &mut stmts);
                            }
                            stmts.push(Stmt::Scan(generator));
                        }
                    } else if self.is_foldable(*rhs) {
                        // A constant bind: recorded and substituted at every use, so
                        // it contributes no generator and no step. The left side is a
                        // variable, a wildcard, or a record destructured piece by
                        // piece — all three are the same substitution.
                        //
                        // Folded **here**, before any order is chosen, and not as the
                        // alias below: a bare variable at a key field is capturable,
                        // so a statement reading `N` would otherwise offer to bind it
                        // and `reorder` would be free to run that first. A constant
                        // is what `N` *is*, in every order, so it cannot wait.
                        self.fold_into(*lhs, *rhs);
                    } else if matches!(self.ast.store().kind(*rhs), ExprKind::Prefix(_)) {
                        // A **pattern**, not a value: a range has nothing for the
                        // left side to be, so this constrains where that side
                        // already lives rather than binding it — see
                        // [`Constraint`]. Recorded here for the same reason the
                        // fold above is: the level that captures the variable has
                        // to see it whatever order that level runs in.
                        if let ExprKind::Var(symbol) = self.ast.store().kind(*lhs) {
                            self.constraints.push((*symbol, *rhs));
                        }

                        stmts.push(Stmt::Constrain(Constraint {
                            var: *lhs,
                            span: self.ast.store().span(*rhs),
                        }));
                    } else if self.names_a_location(*rhs) {
                        // An **alias**: the right side denotes a place — a register,
                        // a field inside one, a fact's value — so the left side is a
                        // second name for it and needs nothing computed. A generator
                        // written under the read is a level like any other.
                        self.hoist_within(*rhs, &mut stmts);
                        stmts.push(Stmt::Alias(Alias {
                            pattern: *lhs,
                            value: *rhs,
                            span: self.ast.store().span(*rhs),
                        }));
                    } else {
                        self.report(
                            *rhs,
                            Code::NyiValueBind,
                            "binding a variable to a value that is in no register is not \
                             implemented yet; it needs a derived bind",
                        );
                    }
                }

                QueryStmt::Negation(node) => {
                    if let Some(generator) = self.negated(*node) {
                        stmts.push(Stmt::Negate(generator));
                    }
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

        // `X = Y` is symmetric and its spelling is not, so the direction is settled
        // before anything asks what a statement binds.
        self.orient(&mut stmts);

        // Which variables some statement has already said what *are*, rather than
        // offering to bind — see [`Claims`]. Decided here, from the whole statement
        // list, so it is a property of the query rather than of the order.
        let claims = self.claims(&stmts);

        for stmt in &stmts {
            let mut occurrences = Occurrences::default();

            match stmt {
                Stmt::Scan(generator) => {
                    if let Some(row) = generator.row {
                        occurrences.capture(row);
                    }

                    // **Captures intersect; reads unite.** A variable only some
                    // branch binds is not bound after the statement — the branch
                    // that ran may not have written it — so it cannot count as a
                    // capture, and a later read of it is then unbound and reported
                    // where a person can act on it. Anything any branch *reads* has
                    // to be bound before the statement runs, whichever branch that
                    // is, so those unite.
                    let mut per_branch = Vec::with_capacity(generator.alternatives.len());

                    for alt in generator.alternatives.clone().iter() {
                        let mut branch = Occurrences::default();
                        self.scan_key(alt.key, alt.predicate, &claims, &mut branch);

                        occurrences.reads.extend(branch.reads.iter().copied());
                        per_branch.push(branch.captures);
                    }

                    if let Some((first, rest)) = per_branch.split_first() {
                        for capture in first {
                            if rest.iter().all(|branch| branch.contains(capture)) {
                                occurrences.capture(*capture);
                            }
                        }
                    }
                }

                // An alias binds what its pattern names and reads what its value is
                // rooted at, which is the shape `reorder` was written for: reads it
                // cannot satisfy itself, captures it offers.
                Stmt::Alias(alias) => {
                    // **`X = Y` with both sides bare variables a key can bind is a
                    // compare, not a definition**: it reads both and binds
                    // neither, so it has to run after both. Anything else — a
                    // field read, a variable no key mentions — *defines* its left
                    // side, and reordering is free to run it first.
                    let compares = self.bare_capturable(alias.pattern, &claims)
                        && self.bare_capturable(alias.value, &claims);

                    if compares {
                        self.scan_read(alias.pattern, &mut occurrences);
                    } else {
                        self.scan_pattern(alias.pattern, &mut occurrences);
                    }

                    self.scan_read(alias.value, &mut occurrences);
                }

                // **A negation reads everything and captures nothing**, and those
                // two halves are one rule rather than two.
                //
                // Glean states the scope half as `FlatNegation -> mempty`
                // (`Query/Scope.hs`): nothing inside a negated group escapes it,
                // because the branch that would have bound a variable is the branch
                // that must not have matched. The ordering half is its
                // `Note [Reordering negations]` — *"always move negated subqueries
                // after the binding of all variables from the parent scope that it
                // uses"* — and it is **semantic, not a heuristic**: an unbound
                // variable inside a negation behaves as a wildcard, so running the
                // negation earlier asks a different question.
                //
                // Both fall out of walking the key and then moving every capture
                // into `reads`. `reads` is what the frontier will not run a
                // statement before, so the placement rule *is* the graph — no
                // immovability tag, no second mechanism, and completeness survives
                // because `reads` is still structural and `bound` still only grows
                // ([the query-surface note](../../../docs/query-surface.md)).
                Stmt::Negate(generator) => {
                    for alt in generator.alternatives.clone().iter() {
                        self.scan_key(alt.key, alt.predicate, &claims, &mut occurrences);
                    }

                    for capture in std::mem::take(&mut occurrences.captures) {
                        occurrences.read(capture);
                    }
                }

                // A pure **read**: it says what the value at a place has to look
                // like, and binds nothing. So the statement that binds the variable
                // has to run first — which costs nothing, since the constraint is
                // applied by that statement rather than by this one.
                Stmt::Constrain(constraint) => self.scan_read(constraint.var, &mut occurrences),
            }

            deps.push(StmtDeps {
                captures: occurrences.captures.into(),
                reads: occurrences.reads.into(),
                placement: stmt.placement(),
            });
        }

        self.negated_wildcards(&stmts, &deps);

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

    /// **Which side of `X = Y` defines the other**, settled before anything else
    /// asks what a statement binds.
    ///
    /// The statement is symmetric and the two spellings of it are not: whichever
    /// side some fact pattern can bind is where the value comes from, and the other
    /// is a second name for it. Written the way round that names the *bound* side
    /// first, the alias would claim it — and then the key that offered to capture it
    /// is demoted to a read, so nothing binds it and the free variable is unbound
    /// too. Two diagnostics, for a query with a perfectly good plan.
    ///
    /// Only `X = Y` with both sides bare variables can be turned round, because only
    /// then is the flipped statement still a bind: a field read on the left is
    /// pattern-pushing, not an alias. And only when exactly one side is bound
    /// elsewhere — with both, the statement is a **compare** that belongs to neither
    /// side, which [`claims`](Self::claims) decides from the same fact.
    fn orient(&self, stmts: &mut [Stmt]) {
        // Every variable some statement can bind: a row it names, or a bare
        // variable in a key it could capture. Shape only, as [`Claims`] is — which
        // is what makes the direction a property of the query and not of the order.
        let mut bound = vec![];

        for stmt in stmts.iter() {
            if let Stmt::Scan(generator) = stmt {
                bound.extend(generator.row);

                for alt in generator.alternatives.iter() {
                    self.key_captures(alt.key, &mut bound);
                }
            }
        }

        let defined = |node: NodeId| {
            matches!(self.ast.store().kind(node), ExprKind::Var(symbol)
                if bound.contains(symbol))
        };

        for stmt in stmts.iter_mut() {
            let Stmt::Alias(alias) = stmt else { continue };

            if matches!(self.ast.store().kind(alias.value), ExprKind::Var(_))
                && defined(alias.pattern)
                && !defined(alias.value)
            {
                std::mem::swap(&mut alias.pattern, &mut alias.value);
            }
        }
    }

    /// Every variable a statement **claims**, reporting a second claim.
    ///
    /// Two statements claiming one variable is unification — *these two things are
    /// the same thing* — and this is the only place it can be seen, with every
    /// statement in hand. It is not an ordering question, so `reorder` must never
    /// be handed it: both claims would satisfy every read and the plan would
    /// quietly keep whichever ran last. Reported at the later claim, which is the
    /// one that could have been written another way.
    fn claims(&mut self, stmts: &[Stmt]) -> Claims {
        let mut claims = Claims::default();

        // What every key could capture, gathered first so that the pass below can
        // tell a compare from an alias whatever order the statements are in.
        for stmt in stmts {
            if let Stmt::Scan(generator) = stmt {
                for alt in generator.alternatives.clone().iter() {
                    let mut names = vec![];
                    self.key_captures(alt.key, &mut names);
                    claims.capturable.extend(names);
                }
            }
        }

        for stmt in stmts {
            // A record pattern claims each of its pieces, and a wildcard claims
            // nothing — so a statement claims a *set*, not a name.
            let (claimed, span, is_row) = match stmt {
                Stmt::Scan(generator) => {
                    let Some(row) = generator.row else { continue };
                    (vec![row], generator.span.clone(), true)
                }
                Stmt::Alias(alias) => {
                    // A **compare** claims nothing: it says two things are equal,
                    // not what either one is, so the keys that mention them still
                    // capture them.
                    if self.bare_capturable(alias.pattern, &claims)
                        && self.bare_capturable(alias.value, &claims)
                    {
                        continue;
                    }

                    let mut names = vec![];
                    self.pattern_claims(alias.pattern, &mut names);
                    (names, alias.span.clone(), false)
                }

                // A constraint claims nothing — it does not say what a variable
                // *is*, so the key that mentions it still captures it. That is the
                // whole difference between `X = "a".."` and `X = "a"`.
                //
                // Nor does a negation, and for a stronger reason: it binds nothing
                // at all, so every variable it names belongs to whatever else in
                // the query does bind it.
                Stmt::Constrain(_) | Stmt::Negate(_) => continue,
            };

            for name in claimed {
                if claims.rows.contains(&name) || claims.aliased.contains(&name) {
                    let text = self.name(name).to_owned();
                    self.diagnostics.error(
                        Code::NyiBindUnification,
                        format!(
                            "an earlier statement already says what `{text}` is; matching two \
                             values against each other is not implemented yet"
                        ),
                        span.clone(),
                    );
                } else if is_row {
                    claims.rows.push(name);
                } else {
                    claims.aliased.push(name);
                }
            }
        }

        claims
    }

    /// The variables a pattern being bound **captures** — every leaf of it, since a
    /// record pattern binds each of its pieces.
    ///
    /// Anything else binds nothing: a wildcard cannot fail, and a literal leaf was
    /// refused at typecheck.
    fn scan_pattern(&mut self, node: NodeId, occurrences: &mut Occurrences) {
        match self.ast.store().kind(node) {
            ExprKind::Var(symbol) => occurrences.capture(*symbol),
            ExprKind::Record(fields) => {
                for (_, piece) in fields.clone().iter() {
                    self.scan_pattern(*piece, occurrences);
                }
            }
            _ => {}
        }
    }

    /// The variables a pattern being bound **claims**, appended to `out`.
    fn pattern_claims(&self, node: NodeId, out: &mut Vec<Symbol>) {
        match self.ast.store().kind(node) {
            ExprKind::Var(symbol) => out.push(*symbol),
            ExprKind::Record(fields) => {
                for (_, piece) in fields.iter() {
                    self.pattern_claims(*piece, out);
                }
            }
            _ => {}
        }
    }

    /// Whether `node` is a bare variable some key pattern can capture.
    fn bare_capturable(&self, node: NodeId, claims: &Claims) -> bool {
        matches!(self.ast.store().kind(node), ExprKind::Var(symbol)
            if claims.capturable.contains(symbol))
    }

    /// Every variable a key pattern could **capture** — every bare variable in it,
    /// at any depth.
    ///
    /// Deliberately shape-only, like [`Claims`] itself: whether a given statement
    /// actually captures a variable depends on the order, and this is asked before
    /// one is chosen.
    fn key_captures(&self, node: NodeId, out: &mut Vec<Symbol>) {
        match self.ast.store().kind(node) {
            ExprKind::Var(symbol) => out.push(*symbol),
            ExprKind::Record(fields) => {
                for (_, piece) in fields.clone().iter() {
                    self.key_captures(*piece, out);
                }
            }
            _ => {}
        }
    }

    /// The variable a read is rooted at — `X` in `X.a.b`, or the row a hoisted
    /// generator was given.
    fn scan_read(&mut self, node: NodeId, occurrences: &mut Occurrences) {
        let root = self.chain_root(node);

        match self.ast.store().kind(root) {
            ExprKind::Var(symbol) => occurrences.read(*symbol),
            ExprKind::Fact(..) => self.read_hoisted(root, occurrences),
            _ => {}
        }
    }

    /// Whether `node` denotes a **place** — something a name can be another name
    /// for, rather than a value that would have to be computed.
    ///
    /// Shape only, because that is all `collect` can know: whether the place
    /// actually resolves depends on the order, and `emit` reports what does not.
    /// What this excludes is the derived bind — a record mentioning a captured
    /// variable, a string prefix — which is in no register and would have to be
    /// built ([chapter 7](../../../docs/07-compilation.md#derived-facts)).
    fn names_a_location(&self, node: NodeId) -> bool {
        match self.ast.store().kind(node) {
            ExprKind::Var(_) | ExprKind::Fact(..) => true,
            ExprKind::Access(_, base) | ExprKind::Select(_, base) => self.names_a_location(*base),
            _ => false,
        }
    }

    // ---- hoisting -----------------------------------------------------------

    /// Hoist every fact pattern **inside** `node` into a generator of its own.
    ///
    /// A fact pattern denotes the facts matching it, so it is a generator wherever it
    /// is written — in a key field, in the head, under a field read. Only the one a
    /// statement *is* stays where it is; the rest become levels, appended here so that
    /// each precedes whatever named it.
    fn hoist_within(&mut self, node: NodeId, stmts: &mut Vec<Stmt>) {
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
    fn hoist_node(&mut self, node: NodeId, stmts: &mut Vec<Stmt>) {
        let ExprKind::Fact(predicate, key) = *self.ast.store().kind(node) else {
            self.hoist_within(node, stmts);
            return;
        };

        // Innermost first: a generator nested inside this one has to be a level
        // *before* it, because this one's key reads what that one binds.
        self.hoist_within(key, stmts);

        let row = self.fresh(stmts.len());
        stmts.push(Stmt::Scan(Gen {
            alternatives: Box::new([Alt { predicate, key }]),
            row: Some(row),
            span: self.ast.store().span(node),
            placement: Placement::Floating,
        }));
        self.hoisted.push((node, row));
    }

    /// Inline a subquery's statements into the enclosing list.
    ///
    /// One level deep per call, and recursive through `collect`'s own arms, so a
    /// subquery inside a subquery flattens the same way. Nothing about the
    /// statements changes: after this they are the outer query's, and `reorder`
    /// orders them with everything else — which is the point, since a subquery in
    /// a generating position constrains the same rows.
    fn inline(&mut self, body: &[QueryStmt<NodeId>], stmts: &mut Vec<Stmt>) {
        for stmt in body {
            match stmt {
                QueryStmt::Implicit(node) => {
                    if let Some(generator) = self.generator(*node, None) {
                        for alt in generator.alternatives.clone().iter() {
                            self.hoist_within(alt.key, stmts);
                        }
                        stmts.push(Stmt::Scan(generator));
                    }
                }
                QueryStmt::Bind(lhs, rhs) => {
                    self.hoist_within(*rhs, stmts);
                    stmts.push(Stmt::Alias(Alias {
                        pattern: *lhs,
                        value: *rhs,
                        span: self.ast.store().span(*rhs),
                    }));
                }
                QueryStmt::Negation(node) => {
                    self.report(
                        *node,
                        Code::NyiNegation,
                        "negation inside a subquery is not implemented yet",
                    );
                }
            }
        }
    }

    /// Whether a subquery binds a name that only becomes an outer name **later**.
    ///
    /// Sharing a name with the scope *around* it is how a correlated subquery
    /// works — `W = (Y where test.Foo {id = X, name = Y})` reads the outer `X`,
    /// and typecheck agrees, because `X` was already in the environment when the
    /// subquery was checked. Inlining preserves that exactly.
    ///
    /// What inlining does **not** preserve is a name the subquery binds fresh that
    /// some *later* statement also binds: typecheck scoped the first away, so they
    /// are two variables to it and one to flatten. Rather than silently conflate
    /// them, this refuses and says to rename — scoping them properly means
    /// renaming into fresh symbols, which is a rewrite of the tree flatten cannot
    /// do.
    fn subquery_shadows(&mut self, body: &[QueryStmt<NodeId>], at: NodeId) -> bool {
        let mut inner = vec![];
        for stmt in body {
            if let QueryStmt::Implicit(node) = stmt
                && let ExprKind::Fact(_, key) = self.ast.store().kind(*node)
            {
                self.key_captures(*key, &mut inner);
            }
        }

        // Only what comes *after*: a name bound before the subquery is one the
        // subquery reads, which is correlation rather than collision.
        let mut outer = vec![];
        let mut seen_subquery = false;

        for stmt in self.ast.query().body() {
            match stmt {
                QueryStmt::Bind(_, rhs) if *rhs == at => seen_subquery = true,
                QueryStmt::Implicit(node) if seen_subquery => {
                    if let ExprKind::Fact(_, key) = self.ast.store().kind(*node) {
                        self.key_captures(*key, &mut outer);
                    }
                }
                _ => {}
            }
        }

        let Some(shadowed) = inner.iter().find(|name| outer.contains(name)) else {
            return false;
        };

        let name = self.name(*shadowed).to_owned();
        self.report(
            at,
            Code::NyiSubquery,
            format!(
                "this subquery binds `{name}`, which the query around it also binds; a \
                 subquery that reuses an outer name is not implemented yet — rename it"
            ),
        );
        true
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

    // ---- following a reference ----------------------------------------------

    /// Hoist a **fetch level** for every reference `node` reads *through*.
    ///
    /// A reference is an id, and a field of the fact it names is bytes in *that*
    /// fact's key — so `X.file.path` is two rows and one register cannot hold
    /// both. The second row becomes a level of its own, exactly as a nested fact
    /// pattern does: `hoist_node` invents the name a query did not write for a
    /// generator, and this invents the register for a fact the query named only by
    /// reference.
    ///
    /// Called **before** the reading statement's own address is taken, which is
    /// what keeps two properties true at once: a register is still its level's
    /// position in the body, and the fetch is an *outer* level — so a seek may
    /// splice it, a residual may compare against it, and the reference it reads
    /// cannot move while it is open.
    fn fetch_within(&mut self, node: NodeId, body: &mut Body) {
        match self.ast.store().kind(node) {
            ExprKind::Record(fields) => {
                for (_, value) in fields.clone().iter() {
                    self.fetch_within(*value, body);
                }
            }

            ExprKind::Access(_, base) | ExprKind::Select(_, base) => {
                // Innermost first: `X.via.of.name` reads through `X.via` to reach
                // `of`, and through *that* to reach `name`, so each hop has to be a
                // register before the next one can be read out of it.
                let base = *base;
                self.fetch_within(base, body);

                // Only a fact-typed *field* is a reference to follow. A row is
                // already a register, a constant is not a fact, and a reference in
                // a fact's value is the case `dereference` still defers.
                let Some(Slot::Field {
                    address,
                    path,
                    ty: PredicateTy::Fact(predicate),
                }) = self.resolve(base)
                else {
                    return;
                };

                self.fetch_level(address, path, predicate, body);
            }

            _ => {}
        }
    }

    /// The register holding the fact `address`'s `path` points at, adding the
    /// level that fetches it the first time that reference is read through.
    fn fetch_level(
        &mut self,
        address: Address,
        path: FieldPath,
        predicate: PredicateId,
        body: &mut Body,
    ) -> Address {
        if let Some(register) = self.fetched_register(address, &path) {
            return register;
        }

        let register = body.next_address();

        body.push_level(Level::fetch(
            address,
            path.clone(),
            predicate,
            Box::new([register]),
            Box::new([]),
        ));

        self.fetched.push((address, path, register));
        register
    }

    /// The register a reference has **already** been followed into, if any.
    fn fetched_register(&self, address: Address, path: &FieldPath) -> Option<Address> {
        self.fetched
            .iter()
            .find(|(reference, at, _)| *reference == address && at == path)
            .map(|(_, _, register)| *register)
    }

    /// One statement as a generator, or a report that it is not one.
    fn generator(&mut self, node: NodeId, row: Option<Symbol>) -> Option<Gen> {
        let span = self.ast.store().span(node);

        match self.ast.store().kind(node) {
            ExprKind::Fact(predicate, key) => Some(Gen {
                alternatives: Box::new([Alt {
                    predicate: *predicate,
                    key: *key,
                }]),
                row,
                span,
                placement: Placement::Written,
            }),

            // **The empty relation.** No alternative to open, so the level is
            // exhausted the moment it is entered — which is exactly what a level
            // with no sources does, and why `never` needs nothing else.
            ExprKind::Never => Some(Gen {
                alternatives: Box::new([]),
                row,
                span,
                placement: Placement::Written,
            }),

            // **A disjunction is one level with one alternative per branch.** Each
            // branch has to be a generator in its own right: distributing an
            // alternation that sits *inside* a pattern — Glean's "PLAN B" — means
            // rewriting the enclosing pattern per branch, and the tree is not ours
            // to extend here, so that stays deferred with a message saying which
            // half is missing.
            //
            // A `never` branch is **dropped**, which is the identity law made
            // literal rather than a special case bolted on.
            ExprKind::Disjunction(branches) => {
                let branches = branches.clone();
                let mut alternatives = Vec::with_capacity(branches.len());

                for branch in branches.iter() {
                    match self.ast.store().kind(*branch) {
                        ExprKind::Fact(predicate, key) => alternatives.push(Alt {
                            predicate: *predicate,
                            key: *key,
                        }),
                        ExprKind::Never => {}
                        _ => {
                            self.report(
                                *branch,
                                Code::NyiDisjunction,
                                "every branch of a disjunction has to be a fact pattern of \
                                 its own for now; an alternation inside a pattern is not \
                                 implemented yet",
                            );
                            return None;
                        }
                    }
                }

                Some(Gen {
                    alternatives: alternatives.into(),
                    row,
                    span,
                    placement: Placement::Written,
                })
            }
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

    /// **A variable only a negation names would be a wildcard**, and that is a
    /// meaning worth refusing to guess at.
    ///
    /// `test.Foo {id = X}; !test.Bar {id = Y}` reads, to a Datalog eye, as "no
    /// `test.Bar` exists at all" — `Y` is existentially quantified inside the
    /// negation, which is the standard reading and Glean's. But every *other*
    /// statement here binds what it names, and the two readings of `!test.Bar {id =
    /// Y}` are indistinguishable at a glance, which is the argument for refusing it
    /// rather than picking one ([the query-surface
    /// note](../../../docs/query-surface.md)). The wildcard reading is spellable —
    /// `_` — so this asks for that spelling.
    ///
    /// **A rejection, not a deferral**, and it carries the code the safety check
    /// would have reported anyway: nothing binds the variable, which is exactly
    /// true. What this adds is the sentence a reader can act on, and a span
    /// pointing at the negation rather than at whatever read it last.
    fn negated_wildcards(&mut self, stmts: &[Stmt], deps: &[StmtDeps]) {
        let mut bindable: Vec<Symbol> = deps
            .iter()
            .flat_map(|stmt| stmt.captures.iter().copied())
            .collect();

        // A folded constant binds before any level runs, so a negation reading one
        // is reading a value, not quantifying over a predicate.
        bindable.extend(
            self.bindings
                .iter()
                .filter(|(_, slot)| matches!(slot, Slot::Const(_)))
                .map(|(symbol, _)| *symbol),
        );

        for (index, stmt) in stmts.iter().enumerate() {
            let (Stmt::Negate(generator), Some(occurrences)) = (stmt, deps.get(index)) else {
                continue;
            };

            for read in occurrences.reads.iter() {
                if bindable.contains(read) {
                    continue;
                }

                let name = self.name(*read).to_owned();
                self.diagnostics.error(
                    Code::RejectUnboundVariable,
                    format!(
                        "nothing binds `{name}`, and inside a negation that would quietly \
                         mean *any* matching fact rather than a value — write `_` if that \
                         is what you mean"
                    ),
                    generator.span.clone(),
                );
            }
        }
    }

    /// The generator a **negation** tests for emptiness — or `None`, with the
    /// reason reported.
    ///
    /// [`generator`](Self::generator) does the lowering, because a negated pattern
    /// is a generator like any other: `!never` is a test with no source, which
    /// every row passes, and `!(A | B)` is one test over both alternatives. Two
    /// shapes are refused here first, and the second is the one worth knowing.
    ///
    /// - **A subquery** — `!(Y where …)` — is a nested group. It is the one
    ///   construct in this phase that would need a *level inside a test*, so it is
    ///   named rather than half-built.
    /// - **A fact pattern inside the key** — `!test.Ref {of = test.Foo {id = 1}}` —
    ///   would ordinarily be [hoisted](Self::hoist_within) into a level of its own,
    ///   and hoisting **out of a negation changes what the query means**. `¬∃f∃r`
    ///   and `∀f ¬∃r` agree on every `f` the hoisted level produces, and disagree
    ///   exactly when it produces none: the negation is then vacuously true, while
    ///   the hoisted plan has an empty level above the test and answers no rows at
    ///   all. So this is not a lowering that is missing, it is one that would be
    ///   wrong, and the diagnostic says so rather than saying "not yet".
    fn negated(&mut self, node: NodeId) -> Option<Gen> {
        if matches!(self.ast.store().kind(node), ExprKind::Subquery(_)) {
            self.report(
                node,
                Code::NyiNegation,
                "negating a subquery is not implemented yet; a negated group needs a \
                 level inside a test, which the machine has no shape for",
            );
            return None;
        }

        let generator = self.generator(node, None)?;

        for alt in generator.alternatives.iter() {
            if self.nests_a_generator(alt.key) {
                self.report(
                    alt.key,
                    Code::NyiNegation,
                    "a fact pattern inside a negation's key is not implemented yet: \
                     hoisting it out would answer differently when it matches nothing — \
                     bind it in a statement of its own first",
                );
                return None;
            }
        }

        Some(generator)
    }

    /// Whether a pattern has a **fact pattern inside it** — the thing hoisting
    /// exists for, asked before hoisting rather than after.
    fn nests_a_generator(&self, node: NodeId) -> bool {
        match self.ast.store().kind(node) {
            ExprKind::Fact(..) => true,
            ExprKind::Record(fields) => fields
                .iter()
                .any(|(_, value)| self.nests_a_generator(*value)),
            ExprKind::Access(_, base) | ExprKind::Select(_, base) => self.nests_a_generator(*base),
            _ => false,
        }
    }

    /// Walk a key pattern for variable occurrences, reporting anything the plan
    /// cannot express.
    fn scan_key(
        &mut self,
        node: NodeId,
        predicate: PredicateId,
        claims: &Claims,
        occurrences: &mut Occurrences,
    ) {
        let Some(key_ty) = self.schema.get(predicate).map(|p| p.key().ty.clone()) else {
            return;
        };

        match (&key_ty, self.ast.store().kind(node)) {
            (PredicateTy::Record(_), ExprKind::Record(_)) => {
                self.scan_field(node, &key_ty, claims, occurrences);
            }
            // A **whole key** — a wildcard (a whole-predicate scan), or one variable
            // standing for every field at once. Both are the same question the field
            // walk already answers, asked of the key's own type: a wildcard occurs
            // nowhere, and a variable is captured here or read from elsewhere.
            (PredicateTy::Record(_), _) => self.scan_field(node, &key_ty, claims, occurrences),
            // A scalar key is one field, and the pattern is that field's.
            (scalar, _) => self.scan_field(node, scalar, claims, occurrences),
        }
    }

    fn scan_field(
        &mut self,
        node: NodeId,
        ty: &PredicateTy,
        claims: &Claims,
        occurrences: &mut Occurrences,
    ) {
        match self.ast.store().kind(node) {
            ExprKind::Wildcard | ExprKind::Lit(_) | ExprKind::Prefix(_) => {}

            ExprKind::Var(symbol) => {
                // A variable something else has **claimed** can only be read here —
                // see [`Claims`]. For an alias that holds wherever it occurs: the
                // statement saying what it is has to run first, whatever its type.
                //
                // For a row it holds only at a *fact-typed* field, because that is
                // the one place the two could be confused: the field holds a
                // reference and a reference is a value, so a bare variable there is
                // ordinarily capturable — but if it is a row, binding it here would
                // need the level to find its own fact by id, a point access the plan
                // cannot express.
                let claimed = claims.aliased.contains(symbol)
                    || (matches!(ty, PredicateTy::Fact(_)) && claims.rows.contains(symbol));

                if claimed {
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
                        self.scan_field(pattern, field_ty, claims, occurrences);
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
        // A folded constant is bound **before any level runs**, and range
        // restriction is satisfied: `X = 42` gives `X` exactly one value, which is
        // what the check is for — that a variable ranges over something finite. It
        // needs no ordering either, since nothing it depends on can move.
        let mut bound: Vec<Symbol> = self
            .bindings
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Const(_)))
            .map(|(symbol, _)| *symbol)
            .collect();
        // **One variable, one diagnostic.** "Nothing binds `X`" is a fault of the
        // query rather than of the statement that noticed, and several statements
        // can read one unbound variable — a constraint and the head, most simply.
        // Saying it once per reader would be the same sentence twice.
        let mut missing: Vec<Symbol> = vec![];
        let mut ok = true;

        for &stmt in order {
            let (Some(deps), Some(statement)) =
                (collected.deps.stmt(stmt), collected.stmts.get(stmt))
            else {
                return false;
            };

            for read in deps.reads.iter() {
                if !bound.contains(read) && !missing.contains(read) {
                    let at = statement.span();
                    missing.push(*read);
                    self.unbound(*read, at);
                    ok = false;
                }
            }

            bound.extend(deps.captures.iter().copied());
        }

        for read in &collected.head_reads {
            if !bound.contains(read) && !missing.contains(read) {
                let at = self.ast.store().span(*self.ast.query().head());
                missing.push(*read);
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
    ///
    /// A **register is a level's**, not a statement's, so the address is counted off
    /// the levels emitted rather than the position in the order: an alias is a
    /// statement that binds without iterating, exactly as
    /// [`Plan::levels`](crate::focus::plan::Plan::levels) counts them for a plan that
    /// derives.
    fn emit(&mut self, stmts: &[Stmt], order: &[usize]) -> Option<Plan> {
        let mark = self.diagnostics.len();
        let mut body = Body::new(order.len());

        for &stmt in order {
            match stmts.get(stmt)? {
                Stmt::Scan(generator) => {
                    // A key reading *through* a reference needs the fact it names
                    // in a register of its own, which is a level — and an outer
                    // one, so that this level's seek may splice it.
                    for alt in generator.alternatives.clone().iter() {
                        self.fetch_within(alt.key, &mut body);
                    }

                    let address = body.next_address();
                    let mut sources = Vec::with_capacity(generator.alternatives.len());
                    // Where this level's own bindings start, which is what a later
                    // branch is reconciled against — the first branch's.
                    let level_start = self.bindings.len();

                    // Each alternative builds its own seek and its own residuals,
                    // because they are two key layouts and a `FieldPath` means
                    // something different in each.
                    let mut first: Vec<(Symbol, Slot)> = vec![];

                    for (alternative, alt) in generator.alternatives.clone().iter().enumerate() {
                        let key_ty = self.schema.get(alt.predicate)?.key().ty.clone();

                        let mut current = SeekBuilder::new();
                        self.key(alt.key, &key_ty, address, &mut current);

                        // **Every branch is walked in the same environment.** Its
                        // own bindings are taken off before the next one runs, so a
                        // variable two branches bind is a capture in both rather
                        // than a capture and then a read of itself — which is what
                        // an intra-row repeat is, and is a different thing.
                        let branch = self.bindings.split_off(level_start);

                        if alternative == 0 {
                            first = branch;
                        } else {
                            // **Every branch has to agree about where a variable it
                            // binds lives.** A register holds one row and the plan
                            // holds one path into it, so a variable reached at a
                            // different field in another branch would decode the
                            // wrong bytes for half the rows — silently.
                            self.reconcile(alt.key, &first, branch);
                        }

                        sources.push(Source::Seek {
                            access: Access {
                                predicate_id: alt.predicate,
                                seek_key: current.seek_key(),
                            },
                            residuals: current.residuals.into(),
                        });
                    }

                    self.bindings.extend(first);

                    // After the key: `X = test.Foo {id = X}` cannot typecheck, so
                    // nothing in a level's own key can read the row it binds.
                    if let Some(row) = generator.row {
                        self.bindings.push((
                            row,
                            Slot::Row {
                                address,
                                predicate: generator.alternatives.first().map(|alt| alt.predicate),
                            },
                        ));
                    }

                    body.push_level(Level {
                        sources: sources.into(),
                        binds: Box::new([address]),
                    });
                }

                // The order has already put every level this reads into a register,
                // so the slot it names exists by now. A `None` was reported by
                // `resolve` — a read through a reference, say — and the mark below
                // is what insists on that.
                Stmt::Alias(alias) => {
                    let (pattern, value) = (alias.pattern, alias.value);

                    // Both sides: `Y = X.file.path` reads through a reference on
                    // the right, and a compare — `X.file.path = Z.path` — reads
                    // through one on the left.
                    self.fetch_within(value, &mut body);
                    self.fetch_within(pattern, &mut body);

                    if let Some(slot) = self.resolve(value) {
                        self.bind_pattern(pattern, slot);
                    }
                }

                // **A negation is the same seek, and no register.** Every variable
                // it names is bound by now — `collect` made them all reads, and the
                // safety check over the chosen order is what guarantees the rest —
                // so the key walk finds every one of them already in a register and
                // splices or filters. It cannot capture: `field` captures only where
                // `lookup` finds nothing.
                //
                // The address it is walked against is the one the *next* level will
                // take, which is no register's. That matters for one arm only: the
                // intra-row check compares a spliced register against this
                // statement's own, and a test has none to compare against.
                Stmt::Negate(generator) => {
                    let address = body.next_address();
                    let mut sources = Vec::with_capacity(generator.alternatives.len());
                    let bound = self.bindings.len();

                    for alt in generator.alternatives.clone().iter() {
                        let key_ty = self.schema.get(alt.predicate)?.key().ty.clone();

                        let mut current = SeekBuilder::new();
                        self.key(alt.key, &key_ty, address, &mut current);

                        sources.push(Source::Seek {
                            access: Access {
                                predicate_id: alt.predicate,
                                seek_key: current.seek_key(),
                            },
                            residuals: current.residuals.into(),
                        });
                    }

                    // Nothing above can have bound anything, and a binding recorded
                    // here would point into a register no level fills. Truncated
                    // rather than trusted, because the cost of being wrong is a
                    // seek splicing an unbound register at run time.
                    debug_assert_eq!(
                        self.bindings.len(),
                        bound,
                        "a negation captured a variable, which nothing binds"
                    );
                    self.bindings.truncate(bound);

                    body.push_test(Test::Absent(sources.into()));
                }

                // Applied by the level that binds the variable, or by
                // `apply_constraints` below where no level does — never from here,
                // because where a constraint belongs has nothing to do with where
                // the statement stating it was written.
                Stmt::Constrain(_) => {}
            }
        }

        // The head reads after every statement has bound, so its own fetches are
        // the innermost levels — one row each, so the row count is unchanged.
        self.fetch_within(*self.ast.query().head(), &mut body);

        self.apply_compares(&mut body);
        self.apply_constraints(&mut body);

        let head = self.project(*self.ast.query().head());

        if self.diagnostics.len() != mark {
            return None;
        }

        Some(Plan {
            nvars: body.levels,
            body: body.steps.into(),
            head: head?,
        })
    }

    /// Turn each recorded [`Compare`] into a residual on the level that binds
    /// **later**.
    ///
    /// A residual is checked against the row a level is scanning, against
    /// registers filled by levels outside it — so `X = Y` belongs to whichever of
    /// the two is inner. That is the same rule sargeability follows for a key
    /// field reading an outer register, reached from the other direction: there
    /// the field is written where the level is, here the level is chosen from
    /// where the fields are.
    ///
    /// Both sides must be a **field of a row**. Two rows would compare identities
    /// (`EqRegisterFactId` — nothing writes one yet), and anything else is not in
    /// a register to be compared.
    fn apply_compares(&mut self, body: &mut Body) {
        for compare in std::mem::take(&mut self.compares) {
            let (
                Slot::Field {
                    address: left,
                    path: left_path,
                    ..
                },
                Slot::Field {
                    address: right,
                    path: right_path,
                    ..
                },
            ) = (&compare.left, &compare.right)
            else {
                self.report(
                    compare.at,
                    Code::NyiBindUnification,
                    "matching these two against each other is not implemented yet; both \
                     sides have to be a field of a row",
                );
                continue;
            };

            // Same register: an intra-row repeat, which is its own deferral and
            // its own decision ([open decisions]).
            if left == right {
                self.report(
                    compare.at,
                    Code::NyiRepeatedVariable,
                    "matching two fields of the *same* row against each other is not \
                     implemented yet",
                );
                continue;
            }

            let (inner, outer, inner_path, outer_path) = if left.index() > right.index() {
                (left, right, left_path, right_path)
            } else {
                (right, left, right_path, left_path)
            };

            let Some(level) = body.level_mut(*inner) else {
                continue;
            };

            // Every alternative gets it: a variable a disjunction binds is in the
            // same place in each, so one residual is right for all of them.
            for source in level.sources.iter_mut() {
                let residuals = source.residuals_mut();
                let mut extended = residuals.to_vec();

                extended.push(Residual {
                    path: inner_path.clone(),
                    op: ResidualOp::EqRegisterField {
                        address: *outer,
                        path: outer_path.clone(),
                    },
                });

                *residuals = extended.into();
            }
        }
    }

    /// Check a later branch's bindings against the ones the first branch made.
    ///
    /// A variable both bind has to name the **same place** — the same path, of the
    /// same type — because the plan carries one path per read and the register
    /// holds whichever branch's row matched. A variable only one branch binds is
    /// dropped rather than reported: it is simply not bound after the statement,
    /// which `collect` already decided by intersecting the captures, and the read
    /// that wanted it draws `reject/unbound-variable` where a person can see why.
    fn reconcile(&mut self, at: NodeId, first: &[(Symbol, Slot)], branch: Vec<(Symbol, Slot)>) {
        for (symbol, slot) in branch {
            let Some(first) = first
                .iter()
                .find(|(bound, _)| *bound == symbol)
                .map(|(_, slot)| slot.clone())
            else {
                continue;
            };

            // Only the *place* is compared. The two types are equal already:
            // typecheck unified the branches, and a variable both bind was seen
            // twice by one environment.
            let agrees = match (&first, &slot) {
                (
                    Slot::Field {
                        address: a,
                        path: p,
                        ..
                    },
                    Slot::Field {
                        address: b,
                        path: q,
                        ..
                    },
                ) => a == b && p == q,
                (Slot::Key { address: a, .. }, Slot::Key { address: b, .. }) => a == b,
                _ => false,
            };

            if !agrees {
                let name = self.name(symbol).to_owned();
                self.report(
                    at,
                    Code::NyiDisjunction,
                    format!(
                        "`{name}` is at a different field in two branches; a variable a \
                         disjunction binds has to be in the same place in every branch, \
                         because the plan reads it from one"
                    ),
                );
            }
        }
    }

    /// **Bind a pattern to the place it names**, piece by piece.
    ///
    /// One walk for every shape a bind's left side can have, against every shape a
    /// slot can be. A record decomposes by *field name* rather than by zipping two
    /// trees, which is what lets the right side be anything with pieces — a
    /// constant, a register's field, a row — instead of only another record
    /// literal. Glean reaches the same place by decomposing `T = U` into leaf
    /// equations and dropping the trivial ones (`Opt.hs:592-663`); decomposing
    /// against a slot means the trivial leaves are never built.
    ///
    /// A wildcard piece is exactly a piece the pattern omits: no constraint, which
    /// is the right answer because a wildcard cannot fail.
    fn bind_pattern(&mut self, pattern: NodeId, slot: Slot) {
        match self.ast.store().kind(pattern) {
            ExprKind::Var(symbol) => {
                let symbol = *symbol;

                // **One variable, one claim.** Two is unification, and the dangerous
                // kind: `lookup` walks the bindings in reverse and would keep the
                // last silently. Typecheck cannot decide this — it would have to
                // decide it in source order, and only `bindings` knows whether the
                // variable is already a substitution rather than a capture.
                match self.lookup(symbol) {
                    // **Both sides are already somewhere**, so this is a compare
                    // rather than a name for a place: the two have to be equal
                    // per row, which is a residual on whichever level binds
                    // later. Recorded here and applied once every level exists.
                    Some(bound) => self.compares.push(Compare {
                        left: bound,
                        right: slot,
                        at: pattern,
                    }),
                    None => self.bindings.push((symbol, slot)),
                }
            }

            // Binds nothing, and cannot fail.
            ExprKind::Wildcard => {}

            ExprKind::Record(fields) => {
                let fields = fields.clone();

                for (name, piece) in fields.iter() {
                    let mark = self.diagnostics.len();

                    match self.field_slot(*piece, &slot, *name) {
                        Some(field) => self.bind_pattern(*piece, field),
                        // A piece of something with no such piece. Typecheck unified
                        // the two shapes first, so this is unreachable — reported
                        // rather than asserted, because it is a data path.
                        None if self.diagnostics.len() == mark => self.report(
                            *piece,
                            Code::RejectTypeMismatch,
                            "this is not a piece of that value",
                        ),
                        None => {}
                    }
                }
            }

            // Typecheck's gate makes this unreachable: a literal leaf is not
            // destructurable, precisely because it would bind nothing and so mean
            // `true` where it means the empty relation.
            _ => self.report(
                pattern,
                Code::NyiBindUnification,
                "matching two patterns against each other is not implemented yet",
            ),
        }
    }

    /// A level's key pattern, field by field in **declared order** — which is
    /// encoding order, and so the order a seek prefix has to be built in.
    fn key(
        &mut self,
        node: NodeId,
        key_ty: &PredicateTy,
        address: Address,
        level: &mut SeekBuilder,
    ) {
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

            (PredicateTy::Record(_), _) => self.whole_key(node, key_ty, address, level),

            (scalar, _) => self.field(node, scalar, address, &FieldPath::field(0), level),
        }
    }

    /// A **whole record key** in one pattern — `test.Foo Y`, or a wildcard.
    ///
    /// A key is not one field, so there is no [`FieldPath`] that names it and no
    /// plan operator that moves it. It does not need one: a stored key is its
    /// top-level fields back to back, so *every* whole-key question decomposes into
    /// the per-field questions [`field`](Self::field) already answers.
    ///
    /// - **A capture** binds the variable to [`Slot::Key`] and closes the seek
    ///   prefix, exactly as a captured field does — the key is an output here.
    /// - **A read** resolves the pattern once and then asks for each of its fields
    ///   in turn, so `test.Bar Y` against a bound `Y` splices field 0, field 1, …
    ///   in declared order, which is byte-for-byte the key `Y` holds.
    ///
    /// The second case is why this cannot go through
    /// [`constant`](Self::constant) for a constant record: that writes the
    /// `MARK_RECORD`-wrapped form, which is right for a record *inside* a field and
    /// wrong for a whole key. Decomposing first means a constant key reaches
    /// `constant` one field at a time, and the wrapper never appears.
    fn whole_key(
        &mut self,
        node: NodeId,
        key_ty: &PredicateTy,
        address: Address,
        level: &mut SeekBuilder,
    ) {
        let PredicateTy::Record(field_tys) = key_ty else {
            return;
        };
        if let ExprKind::Wildcard = self.ast.store().kind(node) {
            level.building = false;
            return;
        }

        if let ExprKind::Var(symbol) = self.ast.store().kind(node)
            && self.lookup(*symbol).is_none()
        {
            level.building = false;
            self.bindings.push((
                *symbol,
                Slot::Key {
                    address,
                    ty: key_ty.clone(),
                },
            ));
            return;
        }

        let Some(slot) = self.resolve(node) else {
            level.building = false;
            return;
        };

        for (idx, (name, field_ty)) in field_tys.clone().iter().enumerate() {
            let Some(field) = self.field_slot(node, &slot, Symbol::Schema(*name)) else {
                level.building = false;
                continue;
            };

            self.matched(
                node,
                &field,
                field_ty,
                address,
                &FieldPath::field(idx),
                level,
            );
        }
    }

    /// One key field: seek, splice, residual, or capture.
    ///
    /// Three of those four are the same question — *where does this value live* —
    /// so they are one arm, asked of [`resolve`](Self::resolve). What is left is the
    /// two shapes a key field can hold that are **not** reads: a variable this level
    /// is the first to mention, which the field *binds*; and a string prefix, which
    /// denotes a range and so is a pattern rather than a value.
    fn field(
        &mut self,
        node: NodeId,
        ty: &PredicateTy,
        address: Address,
        path: &FieldPath,
        level: &mut SeekBuilder,
    ) {
        match self.ast.store().kind(node) {
            ExprKind::Wildcard => level.building = false,

            // First occurrence in this order: the field is an *output*, so it cannot
            // narrow the scan — unless a `X = "a"..` elsewhere in the body says what
            // the output has to look like, which narrows it to a range and is why
            // this arm asks [`constrain`](Self::constrain) rather than closing the
            // prefix itself.
            ExprKind::Var(symbol) if self.lookup(*symbol).is_none() => {
                let symbol = *symbol;
                self.bindings.push((
                    symbol,
                    Slot::Field {
                        address,
                        path: path.clone(),
                        ty: ty.clone(),
                    },
                ));
                self.constrain(symbol, ty, path, level);
            }

            // A **range**, and so the one narrowing that is not a slot: there is no
            // single value for `"a".."` to be, which is also why a variable cannot be
            // bound to one.
            ExprKind::Prefix(_) => match self.constant(node, ty) {
                Some(constant) => Self::narrow_by(constant, path, level),
                // Typecheck rejects a prefix against a non-string field first;
                // reported rather than declined so no path refuses a plan silently.
                None => self.report(
                    node,
                    Code::RejectTypeMismatch,
                    "this prefix is not a pattern for that field's type",
                ),
            },

            // **An alternation inside a pattern.** Distributing it outward — one
            // whole pattern per branch, which is what makes it a level's
            // alternatives — means writing tree nodes the query did not, and the
            // tree is not flatten's to extend. Reported here rather than left to
            // decline quietly below, because typecheck now gives `|` a type and so
            // no longer reports it for us.
            ExprKind::Disjunction(_) => {
                level.building = false;
                self.report(
                    node,
                    Code::NyiDisjunction,
                    "an alternation inside a pattern is not implemented yet; write it as                      whole alternatives — `test.Foo {…} | test.Foo {…}`",
                );
            }

            // `never` as a *field* would make the level match nothing, which is a
            // level with no sources — but this walk builds one seek for the level
            // it is inside, and it has no way to say "and now the whole thing is
            // empty" from a field down.
            ExprKind::Never => {
                level.building = false;
                self.report(
                    node,
                    Code::NyiNever,
                    "`never` inside a pattern is not implemented yet; as a statement or a                      branch of a disjunction it works",
                );
            }

            _ => {
                let mark = self.diagnostics.len();

                match self.resolve(node) {
                    Some(slot) => self.matched(node, &slot, ty, address, path, level),

                    // Not one place. A record giving only some of its fields is a
                    // pattern rather than a value, and its pieces are matched one
                    // step deeper; anything else that reaches here is a read that
                    // resolved to nothing.
                    None if matches!(self.ast.store().kind(node), ExprKind::Record(_)) => {
                        self.partial(node, ty, address, path, level);
                    }

                    None => {
                        level.building = false;

                        // **A quiet decline here is the one outcome worse than
                        // refusing the query.** The field would get no narrowing, no
                        // residual and no error, `building` would stay set, and the
                        // level would match every row — a wrong answer with nothing
                        // to read. `resolve` declines loudly in the cases it knows
                        // about (a read through a reference, say), and typecheck
                        // reported the deferred constructs, so report only where
                        // nothing explained it.
                        let deferred = matches!(
                            self.ast.store().kind(node),
                            ExprKind::Never
                                | ExprKind::Disjunction(_)
                                | ExprKind::Subquery(_)
                                | ExprKind::Error
                        );

                        if self.diagnostics.len() == mark && !deferred {
                            self.report(
                                node,
                                Code::RejectUnresolvedAccess,
                                "this read does not resolve to a value that can match a key \
                                 field",
                            );
                        }
                    }
                }
            }
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
        level: &mut SeekBuilder,
    ) {
        match slot {
            // **A whole key matched into a field.** The two are the same record and
            // not the same bytes: a stored key is flat, while a record *inside* a
            // field keeps its `MARK_RECORD … TERM` wrapper so that it can be skipped
            // as one value. Splicing one where the other belongs compares different
            // encodings and matches nothing, silently — the shape of bug the
            // `FactRef` marker exists to prevent — so this is refused rather than
            // built out of the fields.
            Slot::Key { .. } => {
                level.building = false;
                self.report(
                    node,
                    Code::NyiWholeKey,
                    "matching a whole key against a record field is not implemented \
                     yet; a stored key is flat and a record field is wrapped, so the \
                     two are not the same bytes",
                );
            }

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
                PredicateTy::Fact(referenced) if Some(*referenced) == *predicate => {
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

            // A **constant** at a key field — written there or reached by name,
            // which is now one path: this is the arm that makes
            // `Z = 1; test.Bar {id = Z}` a seek rather than a scan, and it is the
            // same arm `test.Bar {id = 1}` takes.
            //
            // `None` is a constant that does not determine all of the field's bytes
            // — a record giving only some of its fields, `{}` most of all. That is a
            // pattern rather than a value, so it narrows nothing here and its pieces
            // are matched one step deeper.
            Slot::Const(folded) => match self.constant(*folded, ty) {
                Some(constant) => Self::narrow_by(constant, path, level),
                None => self.partial(*folded, ty, address, path, level),
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

    /// A record pattern that gives only **some** of its fields — including the one
    /// that gives none.
    ///
    /// It cannot narrow the scan, because the encoding is positional and the bytes
    /// of the fields it does give are not a prefix of anything. So the field closes
    /// the seek and each piece is matched one path step deeper, where it becomes a
    /// residual or a capture in its own right.
    fn partial(
        &mut self,
        node: NodeId,
        ty: &PredicateTy,
        address: Address,
        path: &FieldPath,
        level: &mut SeekBuilder,
    ) {
        level.building = false;

        let (ExprKind::Record(fields), PredicateTy::Record(field_tys)) =
            (self.ast.store().kind(node), ty)
        else {
            return;
        };

        let fields = fields.clone();

        for (idx, (name, field_ty)) in field_tys.clone().iter().enumerate() {
            if let Some(pattern) = field_pattern(&fields, Symbol::Schema(*name)) {
                self.field(pattern, field_ty, address, &path.then(idx), level);
            }
        }
    }

    /// Narrow this level by a constant: a seek component while the prefix is still
    /// building, a residual once it has closed.
    ///
    /// Shared by the two ways a constant reaches a key field — written there, or
    /// bound to a variable and folded — so that `Z = 1; test.Bar {id = Z}` narrows
    /// exactly as `test.Bar {id = 1}` does rather than by a parallel code path.
    fn narrow_by(constant: Const, path: &FieldPath, level: &mut SeekBuilder) {
        match constant {
            Const::Bytes(bytes) => {
                if level.building {
                    level.parts.push(SeekKeyPart::Bytes(bytes.into()));
                } else {
                    level.residuals.push(Residual {
                        path: path.clone(),
                        op: ResidualOp::EqConst(bytes.into()),
                    });
                }
            }

            // A prefix narrows to a *range*, so it can end a seek but nothing may
            // follow it in one: the bytes after it are not the field's.
            Const::Prefix(bytes) => {
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
        }
    }

    /// Narrow the field that **captures** a variable by every pattern the body
    /// constrains it with — and close the seek prefix, which a capture always does.
    ///
    /// This is what makes `test.Name X; X = "an"..` the same plan as
    /// `test.Name "an"..`: the field is an output *and* a range, and a range is
    /// still a seek. Applied here rather than as a residual after the fact because
    /// after the fact is too late — a seek prefix is built while the level's key is
    /// walked, so a constraint that arrives later can only filter rows the scan has
    /// already produced, which is the difference between a range and a scan.
    ///
    /// A variable may carry several, and each is narrowed in turn: the first ends
    /// the seek prefix (a prefix is the last thing that can be in one) and the rest
    /// filter, which is exactly what "all of them hold" means.
    fn constrain(
        &mut self,
        symbol: Symbol,
        ty: &PredicateTy,
        path: &FieldPath,
        level: &mut SeekBuilder,
    ) {
        let patterns: Vec<NodeId> = self
            .constraints
            .iter()
            .filter(|(constrained, _)| *constrained == symbol)
            .map(|(_, pattern)| *pattern)
            .collect();

        if patterns.is_empty() {
            level.building = false;
            return;
        }

        self.constrained.push(symbol);

        for pattern in patterns {
            match self.constant(pattern, ty) {
                Some(constant) => Self::narrow_by(constant, path, level),
                // Typecheck rejects a prefix against a non-string variable first;
                // reported rather than declined so no path refuses a plan silently.
                None => {
                    level.building = false;
                    self.report(
                        pattern,
                        Code::RejectTypeMismatch,
                        "this prefix is not a pattern for that variable's type",
                    );
                }
            }
        }
    }

    /// The constraints **no capture applied** — every one whose variable is bound
    /// somewhere other than a key field of a level being walked.
    ///
    /// An alias is the case that matters: `Y = X.name; Y = "a"..` constrains a place
    /// inside a row already bound, so there is no seek left to narrow and the answer
    /// is a residual on the level that row belongs to. The rest are reported, and
    /// **reporting is the point** — a constraint silently dropped is a query that
    /// answers with rows it was told to exclude, which is worse than one that
    /// refuses.
    fn apply_constraints(&mut self, body: &mut Body) {
        for (symbol, pattern) in std::mem::take(&mut self.constraints) {
            if self.constrained.contains(&symbol) {
                continue;
            }

            match self.lookup(symbol) {
                Some(Slot::Field { address, path, ty }) => {
                    let Some(constant) = self.constant(pattern, &ty) else {
                        self.report(
                            pattern,
                            Code::RejectTypeMismatch,
                            "this prefix is not a pattern for that variable's type",
                        );
                        continue;
                    };

                    let (Const::Prefix(bytes) | Const::Bytes(bytes)) = constant;
                    let Some(level) = body.level_mut(address) else {
                        continue;
                    };

                    // Every alternative gets it, as a compare does: a variable a
                    // disjunction binds is in the same place in each branch.
                    for source in level.sources.iter_mut() {
                        let residuals = source.residuals_mut();
                        let mut extended = residuals.to_vec();

                        extended.push(Residual {
                            path: path.clone(),
                            op: ResidualOp::Prefix(bytes.clone().into()),
                        });

                        *residuals = extended.into();
                    }
                }

                // **A constant against a pattern**, both known now: `X = "abc"; X =
                // "a".."`. Nothing to check per row, so the answer is the whole
                // query — either the constraint holds and the statement is a
                // tautology, or it does not and the query is the empty relation,
                // which is a level with no source to open. That is `never`'s level,
                // written by a statement that did not say `never`.
                Some(Slot::Const(folded)) => {
                    let (Some(Const::Bytes(value)), Some(Const::Prefix(prefix))) = (
                        self.constant(folded, &PredicateTy::Str),
                        self.constant(pattern, &PredicateTy::Str),
                    ) else {
                        self.report(
                            pattern,
                            Code::RejectTypeMismatch,
                            "this prefix is not a pattern for that variable's type",
                        );
                        continue;
                    };

                    if !value.starts_with(&prefix) {
                        body.push_level(Level {
                            sources: Box::new([]),
                            binds: Box::new([body.next_address()]),
                        });
                    }
                }

                // **A fact's value**, which is the one slot a string prefix can be
                // well typed against and still have nowhere to go: the bytes are in
                // `entities`, and [I6] keeps `entities` out of the scan loop. Same
                // deferral a prefix written at a key field draws.
                //
                // [I6]: ../../../docs/invariants.md#i6
                Some(Slot::Value { .. }) => self.report(
                    pattern,
                    Code::NyiValueMatch,
                    "matching on a fact's value is not implemented yet; a value is fetched \
                     per row, and residuals run inside the scan",
                ),

                // A whole key or a row. Both are a type error against a string
                // prefix, so typecheck has reported it — but a quiet decline here
                // would be a plan missing a constraint, so this says so rather than
                // trusting that.
                Some(_) => self.report(
                    pattern,
                    Code::NyiBindUnification,
                    "matching this against a pattern is not implemented yet",
                ),

                // Nothing binds it, and nothing read it either — the safety check
                // only sees reads, and this statement's read is of a variable no
                // generator offers. Same fault, said the same way.
                None => {
                    let at = self.ast.store().span(pattern);
                    self.unbound(symbol, at);
                }
            }
        }
    }

    /// Whether `node` is a pattern a bind can **fold** — one whose value is known
    /// without running anything.
    ///
    /// A scalar literal, or a record built entirely of foldable things, however
    /// deeply. A record on the *left* of a bind is `pattern = pattern` unification
    /// and typecheck defers it before flatten sees it; a record on the right of a
    /// plain variable bind is not unification at all, just a constant with fields,
    /// and folding it is the same substitution as a scalar.
    ///
    /// Two things are deliberately not foldable. A string **prefix** denotes a
    /// range rather than a value, so there is nothing for a variable bound to one to
    /// be. And a record mentioning a **captured** variable — `{a = 1, b = Y}` — is a
    /// value that differs per row, so it is not a constant at all: that is the
    /// derived bind this phase leaves unlowered, and the nearest thing in the
    /// language to a producer for [`Step::Derive`].
    /// Shared with typecheck, which decides *from the same predicate* that a bind is
    /// a substitution rather than the unification it defers — see
    /// [`Ast::is_constant`].
    fn is_foldable(&self, node: NodeId) -> bool {
        self.ast.is_constant(node)
    }

    /// Bind every variable in `pattern` to its piece of the constant `value`.
    ///
    /// The collect-time entry to [`bind_pattern`](Self::bind_pattern): a constant is
    /// a slot like any other, so folding a constant bind and naming a place are the
    /// same walk over the same left side.
    fn fold_into(&mut self, pattern: NodeId, value: NodeId) {
        self.bind_pattern(pattern, Slot::Const(value));
    }

    /// The bytes a pattern determines, if it determines all of them.
    ///
    /// `None` is "not a constant" — a variable, a wildcard, or a record giving only
    /// part of itself. A record is only constant when the *type's* every field is
    /// given, because the encoding is positional: a missing field would leave the
    /// bytes of the ones after it in the wrong place.
    fn constant(&self, node: NodeId, ty: &PredicateTy) -> Option<Const> {
        // A variable bound to a literal *is* that literal here, which is what makes
        // `Z = 1; test.Bar {id = Z}` seek the same bytes `{id = 1}` does rather than
        // splice a register. Resolved before the match so every arm below — records
        // included — sees through the binding.
        if let ExprKind::Var(symbol) = self.ast.store().kind(node)
            && let Some(Slot::Const(folded)) = self.lookup(*symbol)
        {
            return self.constant(folded, ty);
        }

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

    /// The field `name` of a **folded constant**, as a constant of its own.
    ///
    /// `None` when the constant is not a record, which is a field read on a scalar and
    /// something typecheck has already rejected. The lookup is by exact [`Symbol`], and
    /// works because both sides were interned by the same lowering pass: the record's
    /// field names and the access's come from the same source text through the same
    /// schema-first resolution.
    fn folded_field(&self, folded: NodeId, name: Symbol) -> Option<NodeId> {
        match self.ast.store().kind(folded) {
            ExprKind::Record(fields) => field_pattern(fields, name),
            _ => None,
        }
    }

    /// **Where an expression's value lives** — the one function from a read to a
    /// [`Slot`], and the only place the answer is worked out.
    ///
    /// Every position that can consume a value goes through here: a key field, the
    /// head, an alias's right side, and a record's pieces when it destructures. That
    /// is what keeps a construct from meaning one thing in one position and
    /// something else in another — the failure mode of answering the question six
    /// times ([chapter 7](../../../docs/07-compilation.md)).
    ///
    /// `None` is "this denotes no place". Some of those are reported here (a read
    /// through a reference); the rest were reported by the phase that owns them.
    fn resolve(&mut self, node: NodeId) -> Option<Slot> {
        // A literal, or a record of them to any depth: a constant is a location like
        // any other — the node itself, substituted wherever the name is used. Asked
        // first so every arm below sees a written-out constant and a named one the
        // same way.
        if self.ast.is_constant(node) {
            return Some(Slot::Const(node));
        }

        match self.ast.store().kind(node) {
            ExprKind::Var(symbol) => self.lookup(*symbol),

            // A hoisted generator, which by now is a level with a row of its own.
            ExprKind::Fact(..) => self.hoisted_slot(node),

            ExprKind::Access(FieldRef::Value, base) => {
                // A row, or a reference followed into one: both are a register
                // holding the fact whose value side this is, which is what makes
                // the value one point read away from here.
                let base = self.resolve(*base)?;

                match self.dereference(node, base)? {
                    Slot::Row { address, predicate } => {
                        let ty = self.schema.get(predicate?)?.value()?.ty.clone();
                        Some(Slot::Value { address, ty })
                    }
                    // Typecheck rejects `.value` on anything else: a field's type has
                    // no value side, and a value's is not a fact.
                    _ => None,
                }
            }

            ExprKind::Access(FieldRef::Key(name), base) => {
                let (name, base) = (*name, *base);
                let slot = self.resolve(base)?;
                let slot = self.dereference(node, slot)?;
                self.field_slot(node, &slot, name)
            }

            _ => None,
        }
    }

    /// A **reference, as the row it names** — the substitution that reading
    /// through one comes down to.
    ///
    /// Every other slot passes through unchanged, so this sits between resolving a
    /// base and reading a field or a value out of it, and both readers get the
    /// same answer: the fetched row is an ordinary register, and everything
    /// downstream is the ordinary walk.
    ///
    /// It reads the fetch rather than making one, because a level has to exist
    /// *before* the statement that reads it — see
    /// [`fetch_within`](Self::fetch_within).
    fn dereference(&mut self, node: NodeId, slot: Slot) -> Option<Slot> {
        match &slot {
            Slot::Field {
                address,
                path,
                ty: PredicateTy::Fact(predicate),
            } => match self.fetched_register(*address, path) {
                Some(address) => Some(Slot::Row {
                    address,
                    predicate: Some(*predicate),
                }),

                // Every statement's reads are walked by `fetch_within` before it
                // is emitted, so this is a read in a position that walk does not
                // reach rather than a construct that cannot work. Reported, not
                // declined: `flatten_ordered` promises that a refusal has a
                // reason, and a quiet `None` here would be a query with no plan
                // and no message.
                None => {
                    self.report(
                        node,
                        Code::NyiFactField,
                        "reading through a reference in this position is not implemented \
                         yet; name the reference in a statement of its own first",
                    );
                    None
                }
            },

            // **A reference held in a fact's value.** A fetch reads the id out of
            // a register's *key* bytes, and a value is in the other column family
            // — reaching it would mean a fetch whose reference is itself a fetch,
            // one that no level's register holds.
            Slot::Value {
                ty: PredicateTy::Fact(_),
                ..
            } => {
                self.report(
                    node,
                    Code::NyiFactField,
                    "reading through a reference held in a fact's value is not implemented \
                     yet; a fetch follows a reference in a key",
                );
                None
            }

            _ => Some(slot),
        }
    }

    /// The slot of one **field inside** another slot.
    ///
    /// Reading `X.name` and destructuring `{name = Y} = X` ask this same question,
    /// which is why they answer it with the same function: a field of a place is a
    /// place, reached by extending the path rather than by moving any bytes.
    fn field_slot(&mut self, node: NodeId, slot: &Slot, name: Symbol) -> Option<Slot> {
        match slot {
            Slot::Row { address, predicate } => {
                // `None` is a row of the empty relation: it has no fields to name,
                // and nothing can read one, because no row ever arrives.
                let key_ty = self.schema.get((*predicate)?)?.key().ty.clone();
                let (idx, ty) = field_of(&key_ty, name)?;

                Some(Slot::Field {
                    address: *address,
                    path: FieldPath::field(idx),
                    ty,
                })
            }

            // A field of a whole key is a field of the row it came from — the same
            // answer `Slot::Row` gives, with the key type already to hand.
            Slot::Key { address, ty } => {
                let (idx, field_ty) = field_of(ty, name)?;

                Some(Slot::Field {
                    address: *address,
                    path: FieldPath::field(idx),
                    ty: field_ty,
                })
            }

            Slot::Field { address, path, ty } => match ty {
                PredicateTy::Record(_) => {
                    let (idx, field_ty) = field_of(ty, name)?;

                    Some(Slot::Field {
                        address: *address,
                        path: path.then(idx),
                        ty: field_ty,
                    })
                }
                // A reference reaches here only if it was not dereferenced on the
                // way in, which every path through `resolve` does — so this is the
                // same "a read in a position `fetch_within` does not reach" case,
                // reported the same way rather than declined.
                PredicateTy::Fact(_) => self
                    .dereference(node, slot.clone())
                    .and_then(|slot| self.field_slot(node, &slot, name)),
                _ => None,
            },

            // A field of a **folded constant record** is itself a constant: `A = {x =
            // 2}` makes `A.x` the literal `2`, and the substitution has to reach
            // through the access or it stops at the variable — quietly, and
            // differently wrongly in each position (see the guard test).
            //
            // `None` here is a field read on a *scalar* constant, which typecheck
            // rejects: an integer has no fields.
            Slot::Const(folded) => self.folded_field(*folded, name).map(Slot::Const),

            // A field of a value: typecheck rejects it too, since a value's type has
            // no fields.
            Slot::Value { .. } => None,
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
                    // A whole key projects as the **record it is** — one projection
                    // per field, which is the only way to say it: a key is not one
                    // field, so no single `Project::RegisterField` names it.
                    Slot::Key { address, ty } => {
                        let PredicateTy::Record(fields) = ty else {
                            return None;
                        };

                        Some(Project::Record(
                            fields
                                .iter()
                                .enumerate()
                                .map(|(idx, (name, field_ty))| {
                                    (
                                        Symbol::Schema(*name),
                                        Project::RegisterField {
                                            address,
                                            path: FieldPath::field(idx),
                                            ty: field_ty.clone(),
                                        },
                                    )
                                })
                                .collect(),
                        ))
                    }
                    Slot::Field { address, path, ty } => {
                        Some(Project::RegisterField { address, path, ty })
                    }
                    Slot::Value { address, ty } => Some(Project::Value { address, ty }),
                    // Substitution: project the literal the variable was bound to,
                    // which is the same `Project::Lit` the head would have got had
                    // the literal been written here.
                    Slot::Const(folded) => self.project(folded),
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
        plan::{FactId, Project, Residual, ResidualOp, SeekKey, SeekKeyPart, Source, Test},
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
            // A level says which register it fills; a test fills none, and the rest
            // of the line is the same because the sources are built the same way.
            let (sources, opening) = match step {
                Step::Level(level) => (
                    &level.sources,
                    format!(
                        "{} <- ",
                        level
                            .binds
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                ),
                // A derived bind, which binds a value rather than a level.
                Step::Derive(derived) => {
                    out.push(format!("{} = <computed>", derived.bind));
                    continue;
                }
                // A negation: the rows that must not exist.
                Step::Test(Test::Absent(sources)) => (sources, "absent ".to_owned()),
            };

            // One alternative per source, joined by `|`. A level flatten emits has
            // exactly one, so these renderings read as they always did; zero
            // sources is the empty relation and renders as the keyword for it.
            let alternatives = sources
                .iter()
                .map(|source| {
                    let name = schema
                        .get(source.predicate_id())
                        .and_then(|p| p.name())
                        .unwrap_or("?")
                        .to_owned();

                    let seek = match source {
                        Source::Seek { access, .. } => match &access.seek_key {
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
                                        SeekKeyPart::RegisterFactId(address) =>
                                            format!("{address}#"),
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            ),
                        },
                        // The reference followed, named against the register it is
                        // read out of — `fetch[r0.1]` is "the fact field 1 of r0
                        // points at".
                        Source::Fetch {
                            reference, path, ..
                        } => format!("fetch[{reference}.{path}]"),
                    };

                    let residuals = source
                        .residuals()
                        .iter()
                        .map(|Residual { path, op }| match op {
                            ResidualOp::EqConst(_) => format!("{path} == k"),
                            ResidualOp::Prefix(_) => format!("{path} ^= k"),
                            ResidualOp::EqRegisterField { address, path: at } => {
                                format!("{path} == {address}.{at}")
                            }
                            ResidualOp::EqRegisterFactId(address) => {
                                format!("{path} == {address}#")
                            }
                        })
                        .collect::<Vec<_>>();

                    let residuals = if residuals.is_empty() {
                        String::new()
                    } else {
                        format!(" where {}", residuals.join(" and "))
                    };

                    format!("{name} {seek}{residuals}")
                })
                .collect::<Vec<_>>();

            let rendered = if alternatives.is_empty() {
                "never".to_owned()
            } else {
                alternatives.join(" | ")
            };

            out.push(format!("{opening}{rendered}"));
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
    /// Every code the **whole front end** reports, typecheck's included — for the
    /// queries flatten never gets to see.
    fn front_end_codes(source: &str) -> Vec<String> {
        let schema = corpus::schema();
        let mut compilation = Compilation::new(source, &schema);

        compilation.plan();
        compilation
            .diagnostics()
            .codes()
            .map(str::to_owned)
            .collect()
    }

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
        match &plan
            .level(0)
            .expect("a level")
            .sole_source()
            .expect("one source")
            .seek_key()
            .expect("a seek")
        {
            SeekKey::Prefix(bytes) => assert_eq!(bytes.as_ref(), i64_field(1).as_slice()),
            other => panic!("expected a constant prefix, got {other:?}"),
        }
    }

    /// A scalar key is one field, so a constant against it is the whole seek.
    #[test]
    fn a_scalar_key_constant_is_the_whole_seek() {
        let flattened = compile("X where X = test.Count -42");

        match &flattened
            .plan()
            .level(0)
            .expect("a level")
            .sole_source()
            .expect("one source")
            .seek_key()
            .expect("a seek")
        {
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
        match &plan
            .level(0)
            .expect("a level")
            .sole_source()
            .expect("one source")
            .residuals()[0]
            .op
        {
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
        match &plan
            .level(0)
            .expect("a level")
            .sole_source()
            .expect("one source")
            .seek_key()
            .expect("a seek")
        {
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
        match &plan
            .level(0)
            .expect("a level")
            .sole_source()
            .expect("one source")
            .residuals()[0]
            .op
        {
            ResidualOp::Prefix(bytes) => assert_eq!(bytes.as_ref(), expected.as_slice()),
            other => panic!("expected a prefix residual, got {other:?}"),
        }
    }

    // ---- a pattern a bound variable has to match ----------------------------

    /// **A capture the body constrains is still a seek**, and that is the whole
    /// point of the construct.
    ///
    /// `test.Name X; X = "a"..` is the same question as `test.Name "a"..` with a
    /// name for the answer, and a name must not cost a range scan its range. The
    /// field is an output — ordinarily the thing that *closes* a seek prefix — and a
    /// prefix constraint on it narrows to a range all the same, because a range is
    /// what a seek is.
    ///
    /// Applying it as a residual after the level was built would give the same rows
    /// and read the whole predicate to find them, which is the difference this
    /// asserts by shape rather than by answer.
    #[test]
    fn a_constrained_capture_narrows_the_scan_that_binds_it() {
        let flattened = compile("X where test.Name X; X = \"a\"..");
        let plan = flattened.plan();

        assert_eq!(
            describe(plan, &flattened.interner),
            lines(&["r0 <- test.Name seek[k]", "head r0.0:str"])
        );

        // The same bytes the prefix written at the field seeks — `put_str` without
        // its terminator, which is what every string starting with it begins with.
        let mut expected = str_field("a");
        expected.pop().expect("a terminated string");
        match &plan
            .level(0)
            .expect("a level")
            .sole_source()
            .expect("one source")
            .seek_key()
            .expect("a seek")
        {
            SeekKey::Prefix(bytes) => assert_eq!(bytes.as_ref(), expected.as_slice()),
            other => panic!("expected a prefix seek, got {other:?}"),
        }

        assert_eq!(rows("X where test.Name X; X = \"a\"..").len(), 3);
    }

    /// A constant ahead of the constrained field **extends** the seek, and the
    /// prefix ends it — the rule a prefix written in the key follows, reached from
    /// the other direction.
    #[test]
    fn a_constraint_extends_a_seek_that_is_still_building() {
        assert_eq!(
            shape("X where test.Foo {id = 1, name = X}; X = \"a\".."),
            lines(&["r0 <- test.Foo seek[k]", "head r0.1:str"])
        );

        // ...and behind an open field there is no seek left to extend, so it
        // filters. Same constraint, different level: sargeability is a property of
        // the order, not of the statement.
        assert_eq!(
            shape("X where test.Foo {name = X}; X = \"a\".."),
            lines(&["r0 <- test.Foo scan where 1 ^= k", "head r0.1:str"])
        );

        assert_eq!(rows("X where test.Foo {name = X}; X = \"a\"..").len(), 2);
    }

    // ---- negation (Phase 6b) ------------------------------------------------

    /// **A negation is a test, not a level**: no register, no row, and its key is
    /// built exactly as a scan's is.
    #[test]
    fn a_negation_is_a_test_over_the_seek_a_scan_would_have_used() {
        assert_eq!(
            shape("X where test.Foo {id = X}; !test.Bar {id = X}"),
            lines(&[
                "r0 <- test.Foo scan",
                "absent test.Bar seek[r0.0]",
                "head r0.0:int",
            ])
        );

        assert_eq!(
            rows("X where test.Foo {id = X}; !test.Bar {id = X}"),
            ints(&[3])
        );
    }

    /// **The placement rule, and it is forced rather than likely.**
    ///
    /// Glean's `Note [Reordering negations]` states it as semantics rather than as
    /// a heuristic: a negation must run after everything binding the parent-scope
    /// variables it uses, because an unbound variable inside a negation behaves as
    /// a wildcard — so moving it changes what the query *asks*. Here that rule
    /// needs no mechanism at all: a negation's variables are `reads`, and the
    /// frontier already refuses to run a statement before its reads are bound.
    ///
    /// Asserted as **one plan**, not merely one set of rows. Same rows would also
    /// hold if the negation ran first and matched nothing by accident.
    #[test]
    fn a_negation_runs_after_the_statement_that_binds_it() {
        let expected = shape("X where test.Foo {id = X}; !test.Bar {id = X}");
        assert_eq!(
            shape("X where !test.Bar {id = X}; test.Foo {id = X}"),
            expected
        );

        assert_eq!(
            rows("X where !test.Bar {id = X}; test.Foo {id = X}"),
            rows("X where test.Foo {id = X}; !test.Bar {id = X}")
        );
    }

    /// A negated **disjunction** is one test over both alternatives, exactly as a
    /// disjunction is one level over both.
    #[test]
    fn a_negated_disjunction_is_one_test_with_two_sources() {
        assert_eq!(
            shape("X where test.Foo {id = X}; !(test.Bar {id = X} | test.Node {id = X})"),
            lines(&[
                "r0 <- test.Foo scan",
                "absent test.Bar seek[r0.0] | test.Node seek[r0.0]",
                "head r0.0:int",
            ])
        );

        // `test.Bar` holds 1 and 2, `test.Node` holds 2 and 3 — so nothing of
        // `test.Foo` survives both, and the empty answer is the right one.
        assert_eq!(
            rows("X where test.Foo {id = X}; !(test.Bar {id = X} | test.Node {id = X})"),
            ints(&[])
        );
    }

    /// **`!never` passes every row** — a test with no source to open. The identity
    /// law, arrived at by the same counting a level does.
    #[test]
    fn negating_the_empty_relation_passes_everything() {
        assert_eq!(
            shape("X where test.Bar {id = X}; !never"),
            lines(&["r0 <- test.Bar scan", "absent never", "head r0.0:int"])
        );

        assert_eq!(rows("X where test.Bar {id = X}; !never"), ints(&[1, 2]));
    }

    /// **What a negation refuses, and why each is a refusal rather than a gap.**
    ///
    /// The first two would *answer*, wrongly or surprisingly, if lowered by
    /// analogy; the third needs a shape the machine does not have.
    #[test]
    fn a_negation_refuses_what_it_cannot_mean() {
        // A variable only the negation names is existential — "any `test.Edge`" —
        // which is the opposite of what every other statement here does with a
        // name. `_` says it, so this asks for `_`.
        assert_eq!(
            front_end_codes("X where test.Foo {id = X}; !test.Edge {from = X, to = Y}"),
            ["reject/unbound-variable"]
        );

        // Hoisting a nested generator out of a negation changes the answer when
        // the hoisted level matches nothing.
        assert_eq!(
            front_end_codes("P where P = test.Foo {id = 1}; !test.Ref {of = test.Foo {id = 2}}"),
            ["nyi/negation"]
        );

        // A negated group needs a level inside a test.
        assert_eq!(
            front_end_codes("X where test.Foo {id = X}; !(Y where test.Bar {id = Y})"),
            ["nyi/negation"]
        );

        // A wildcard inside a negation is fine, and is the spelling the first case
        // asks for.
        assert_eq!(
            rows("X where test.Foo {id = X}; !test.Edge {from = X, to = _}"),
            ints(&[3])
        );
    }

    /// **Where the constraint is written does not matter**, which is what makes it a
    /// constraint rather than a step.
    ///
    /// It is collected from the whole body before an order is chosen — as the
    /// constant fold is, and for the same reason: the level that captures the
    /// variable applies it, and which level that is, is the order's answer. So every
    /// order gives one plan, not merely one set of rows.
    #[test]
    fn a_constraint_lands_wherever_it_is_written() {
        let expected = shape("X where test.Edge {from = X}; test.Node {id = X}; X = 1");

        for source in [
            "X where X = 1; test.Edge {from = X}; test.Node {id = X}",
            "X where test.Edge {from = X}; X = 1; test.Node {id = X}",
        ] {
            assert_eq!(shape(source), expected, "for {source:?}");
        }

        let constrained = shape("X where test.Name X; X = \"a\"..");
        assert_eq!(shape("X where X = \"a\"..; test.Name X"), constrained);
    }

    /// **Every constraint on a variable holds.** The first narrows to a range —
    /// nothing can follow a prefix in a seek — and the rest filter, which is exactly
    /// what a conjunction of them means.
    #[test]
    fn two_constraints_on_one_variable_both_hold() {
        assert_eq!(
            shape("X where test.Name X; X = \"a\"..; X = \"an\".."),
            lines(&["r0 <- test.Name seek[k] where 0 ^= k", "head r0.0:str"])
        );

        assert_eq!(
            rows("X where test.Name X; X = \"a\"..; X = \"an\".."),
            strs(&["ann", "anna"]),
        );
    }

    /// A variable an **alias** binds is in a register a level already filled, so
    /// there is no seek left to narrow and the answer is a residual on that level.
    ///
    /// This is the arm no capture reaches, and it has to exist or the constraint
    /// would be dropped: a query answering with rows it was told to exclude is worse
    /// than one that refuses.
    #[test]
    fn a_constraint_on_an_alias_filters_the_row_it_names() {
        assert_eq!(
            shape("Y where X = test.Foo _; Y = X.name; Y = \"a\".."),
            lines(&["r0 <- test.Foo scan where 1 ^= k", "head r0.1:str"])
        );

        assert_eq!(
            rows("Y where X = test.Foo _; Y = X.name; Y = \"a\".."),
            strs(&["ann", "ann"]),
        );
    }

    /// **A constant against a pattern is decided at compile time**, because both
    /// sides are known: either the statement is a tautology and folds away with the
    /// rest of the bind, or the query is the empty relation.
    ///
    /// The second is `never`'s level — a level with no source to open — written by a
    /// statement that did not say `never`. Answering it as "no constraint" instead
    /// would mean *true* where it means the empty relation, which is the same trap
    /// `{a = 1} = {a = 2}` is refused for.
    #[test]
    fn a_constraint_a_constant_meets_folds_and_one_it_cannot_empties_the_query() {
        assert_eq!(
            shape("X where X = \"abc\"; X = \"a\".."),
            lines(&["head \"abc\""])
        );
        assert_eq!(rows("X where X = \"abc\"; X = \"a\".."), strs(&["abc"]));

        assert_eq!(
            shape("X where X = \"abc\"; X = \"z\".."),
            lines(&["r0 <- never", "head \"abc\""])
        );
        assert_eq!(rows("X where X = \"abc\"; X = \"z\".."), vec![]);
    }

    /// A **disjunction**'s branches each bind the variable, so each narrows itself.
    ///
    /// One residual would have been right too and one seek would not: a branch's
    /// seek is its own, and a level's alternatives are two key layouts.
    #[test]
    fn every_branch_of_a_disjunction_is_constrained() {
        assert_eq!(
            shape("X where test.Name X | test.Name X; X = \"a\".."),
            lines(&[
                "r0 <- test.Name seek[k] | test.Name seek[k]",
                "head r0.0:str"
            ])
        );
    }

    /// Constraining a variable **nothing binds** is the missing generator it looks
    /// like, and not a deferral: the statement is fine and the query never says
    /// where the value comes from.
    ///
    /// Said once, though it is read twice — by this statement and by the head.
    #[test]
    fn a_constraint_on_a_variable_nothing_binds_is_unbound() {
        assert_eq!(
            compile("X where X = \"a\"..").codes(),
            ["reject/unbound-variable"]
        );
        assert_eq!(
            compile("Y where test.Name Y; X = \"a\"..").codes(),
            ["reject/unbound-variable"]
        );
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
        let deps = deps_of("P where P = test.Foo {id = 1}; test.Ref {of = P}");

        assert_eq!(deps.antichains(), Some(vec![vec![0], vec![1]]));
        assert!(deps.respects(&[0, 1]));
        assert!(!deps.respects(&[1, 0]));
    }

    /// **The order the query was written in is not the order it runs in.**
    ///
    /// The same read, written the other way round: `of = P` reads `P` in the
    /// statement that comes first, and the statement that comes second is the only
    /// one that can capture it. The dependency graph says so — one order works and
    /// the source is not it — and [`reorder`](crate::focus::reorder::reorder) is what
    /// makes the query legal rather than refused.
    ///
    /// Asserted as *plan equality* against the other spelling rather than against a
    /// literal shape: the claim is that these are one query written two ways, so any
    /// shape either of them has, both have.
    #[test]
    fn a_row_bound_after_the_field_that_reads_it_is_reordered() {
        let deps = deps_of("P where test.Ref {of = P}; P = test.Foo {id = 1}");

        assert_eq!(deps.antichains(), Some(vec![vec![1], vec![0]]));
        assert!(deps.respects(&[1, 0]));
        assert!(
            !deps.respects(&[0, 1]),
            "the premise: the source order is wrong"
        );

        assert_eq!(
            shape("P where test.Ref {of = P}; P = test.Foo {id = 1}"),
            shape("P where P = test.Foo {id = 1}; test.Ref {of = P}"),
            "one query, two spellings, one plan"
        );

        // And the plan is the reordered one, not a scan of `test.Ref` that filters:
        // `test.Foo` is the level the query wrote *second*, and it is `r0` — so the
        // reference seek splices the fact id of a row already bound.
        assert_eq!(
            shape("P where test.Ref {of = P}; P = test.Foo {id = 1}"),
            lines(&[
                "r0 <- test.Foo seek[k]",
                "r1 <- test.Ref seek[r0#]",
                "head r0",
            ]),
        );
    }

    /// **A row variable is bound by at most one statement.**
    ///
    /// Two statements claiming the same row say *these two facts are the same
    /// fact*, which is unification proper and has no engine — unlike binding a row
    /// out of source order, which is only an ordering question and which
    /// [`reorder`](crate::focus::reorder::reorder) now answers.
    ///
    /// Typecheck used to catch this incidentally, by refusing *any* bind whose left
    /// side was already bound. That refusal is now narrowed to the shapes that
    /// really need unification, so the check lives here — where every statement's
    /// row is in hand at once, which is also the only place it can be decided
    /// independently of the order. The proptest generator at
    /// [`proptest`](self::proptest) relies on this holding.
    #[test]
    fn a_row_variable_bound_twice_is_deferred() {
        let schema = corpus::schema();
        let mut compilation = Compilation::new(
            "X where X = test.Foo {id = 1}; X = test.Foo {id = 2}",
            &schema,
        );

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

    // ---- an alias: a second name for a value already somewhere --------------

    /// **Naming a field read is a spelling, not a second way to run a query.**
    ///
    /// `Y = X.name` says where `Y`'s value lives — a field of the row `X` is bound
    /// to — so it substitutes exactly as a constant bind does: no register, no
    /// step, nothing computed. The warrant is plan equality, as it is for hoisting:
    /// if the two agreed only on their answers, the alias would be a second
    /// machine.
    #[test]
    fn naming_a_field_read_compiles_to_the_read() {
        assert_eq!(
            shape("Y where X = test.Foo _; Y = X.name"),
            shape("X.name where X = test.Foo _"),
        );

        assert_eq!(
            rows("Y where X = test.Foo _; Y = X.name"),
            strs(&["ann", "bob", "ann"]),
        );
    }

    /// **An alias reaches every position the read reaches**, because it *is* the
    /// read: at a key field it splices the register it names rather than comparing
    /// a value, which is the whole point of substituting a location.
    ///
    /// This is the query that used to draw `nyi/value-bind`.
    #[test]
    fn an_alias_seeks_where_the_read_would() {
        assert_eq!(
            shape("X.name where X = test.Foo _; Y = X.name; test.Name Y"),
            shape("X.name where X = test.Foo _; test.Name X.name"),
        );

        assert_eq!(
            rows("Y where X = test.Foo _; Y = X.name; test.Name Y"),
            strs(&["ann", "bob", "ann"]),
        );
    }

    /// **An alias must be written after whatever binds its base** — and the limit
    /// is *typecheck's*, not the plan's.
    ///
    /// `reorder` would place either spelling: an alias reads its base and captures
    /// its target, which is all the frontier needs. But inference runs in source
    /// order, so `X`'s type is still an open variable when `Y = X.name` is checked,
    /// and resolving the read would need row polymorphism the type model does not
    /// have (`ty::Checker::unresolved`). The diagnostic is therefore the earlier,
    /// clearer one and comes from typecheck, before flatten sees the query.
    ///
    /// Recorded as a test rather than left implicit because it is the one place an
    /// alias is *not* the ordering-free substitution the rest of these say it is.
    #[test]
    fn an_alias_must_follow_what_binds_its_base() {
        assert_eq!(
            front_end_codes("Y where Y = X.name; X = test.Foo _"),
            ["reject/unresolved-access"],
        );

        // ...and the same query the other way round is an ordinary alias.
        assert_eq!(
            rows("Y where X = test.Foo _; Y = X.name"),
            strs(&["ann", "bob", "ann"]),
        );
    }

    /// `X = Y` with `Y` bound is the same substitution with an empty path.
    #[test]
    fn naming_a_bound_variable_is_an_alias() {
        assert_eq!(
            shape("Y where test.Foo {name = X}; Y = X"),
            shape("X where test.Foo {name = X}"),
        );

        assert_eq!(
            rows("Y where test.Foo {name = X}; Y = X"),
            strs(&["ann", "bob", "ann"]),
        );
    }

    /// **`X = Y` is symmetric, and so is the plan it compiles to** — which side is
    /// written first says nothing about which one the value comes from.
    ///
    /// [`Flattener::orient`] settles that from the body: the side some fact pattern
    /// can bind is where the value is, and the other is a name for it. Written the
    /// other way round the alias used to claim the *bound* side, demoting the key
    /// that offered to capture it to a read — so nothing bound it, the free variable
    /// was unbound as well, and a query with a perfectly good plan drew two
    /// diagnostics.
    #[test]
    fn which_side_of_a_bind_is_written_first_does_not_matter() {
        // One side capturable: the other is the name, whichever way round.
        assert_eq!(
            shape("Y where test.Foo {name = X}; X = Y"),
            shape("Y where test.Foo {name = X}; Y = X"),
        );

        // A row on one side, and the same.
        assert_eq!(
            shape("Y where P = test.Foo _; P = Y"),
            shape("Y where P = test.Foo _; Y = P"),
        );

        // Both sides capturable is the case that must *not* be turned round: it is
        // a compare, belonging to neither side, and the plan is a residual on
        // whichever level binds later.
        assert_eq!(
            shape("X where test.Foo {id = X}; test.Bar {id = Y}; X = Y"),
            shape("X where test.Foo {id = X}; test.Bar {id = Y}; Y = X"),
        );

        // ...including written *above* the statement that binds one of its sides.
        // That order used to be refused outright, because typecheck asked whether a
        // variable had been mentioned yet rather than whether the body binds it.
        assert_eq!(
            shape("X where test.Foo {id = X}; X = Y; test.Bar {id = Y}"),
            shape("X where test.Foo {id = X}; test.Bar {id = Y}; X = Y"),
        );

        assert_eq!(
            rows("Y where test.Foo {name = X}; X = Y"),
            rows("X where test.Foo {name = X}"),
        );
    }

    /// A `.value` alias projects, and still cannot be matched: a value is fetched
    /// per row and never enters the scan ([I6](../../../docs/invariants.md#i6)).
    /// The deferral is the value one, reported where the match is attempted.
    #[test]
    fn a_value_alias_projects_but_does_not_match() {
        assert_eq!(
            rows("Y where X = test.Foo _; Y = X.value"),
            strs(&["one", "two", "three"]),
        );

        assert_eq!(
            compile("Y where X = test.Foo _; Y = X.value; test.Name Y").codes(),
            ["nyi/value-match"],
        );

        // A **constraint** on a value is the same deferral by the same rule — and
        // the one slot a string prefix can be well typed against and still have
        // nowhere to go, which is why it needs an arm of its own rather than falling
        // through to "typecheck already said so".
        assert_eq!(
            compile("Y where X = test.Foo _; Y = X.value; Y = \"a\"..").codes(),
            ["nyi/value-match"],
        );
    }

    /// An alias through a **reference** names the fetched row's field, exactly as
    /// the read written in place does: naming a place changes nothing about where
    /// it is.
    #[test]
    fn an_alias_through_a_reference_names_the_fetched_field() {
        assert_eq!(
            shape("Y where test.Ref {of = P}; Y = P.name"),
            shape("P.name where test.Ref {of = P}"),
        );
    }

    /// **One variable, one claim.** Two statements saying what a name is, is
    /// unification rather than an ordering question — *these two places hold the
    /// same value* — and flatten's claim check ([`Flattener::claims`]) is where it
    /// is seen, because only there is every statement in hand at once.
    ///
    /// It is the only backstop now. Typecheck used to catch it incidentally, by
    /// refusing any bind whose left side was already mentioned; that gate is gone,
    /// because it decided in source order.
    #[test]
    fn a_variable_may_be_claimed_once() {
        // Two names for two different places. The types agree — both are `str` —
        // so it is the second claim being refused and not a mismatch.
        assert_eq!(
            front_end_codes("Z where X = test.Foo _; Y = test.Foo _; Z = X.name; Z = Y.name"),
            ["nyi/bind-unification"],
        );

        // Two rows of one predicate: the same rule, reached in flatten because
        // typecheck's gate lets a repeated fact bind through on purpose — it is
        // the gate that makes `test.Ref {of = P}; P = test.Foo _` an ordering
        // question rather than a rejection.
        assert_eq!(
            compile("X where X = test.Foo _; X = test.Foo {id = 1}").codes(),
            ["nyi/bind-unification"],
        );
    }

    /// **A record pattern destructures against any slot, not just a constant.**
    ///
    /// `{inner = X} = P.outer` names each piece of a place, which is the same
    /// substitution one name for the whole place is — so it compiles to the plan
    /// the chain of reads compiles to, and to the plan the *nested pattern*
    /// spelling of the same query compiles to.
    ///
    /// Glean reaches this by decomposing `T = U` into leaf equations and dropping
    /// the trivial ones (`Opt.hs:592-663`); here the decomposition is by field name
    /// against a slot, so the leaves that would be trivial never exist.
    #[test]
    fn a_record_pattern_destructures_any_slot() {
        assert_eq!(
            shape("X where P = test.Nested _; {inner = X} = P.outer"),
            shape("X where P = test.Nested _; X = P.outer.inner"),
        );

        assert_eq!(
            rows("X where P = test.Nested _; {inner = X} = P.outer"),
            ints(&[1, 7])
        );

        // Every piece at once, from a field with two of them.
        assert_eq!(
            rows("{a = A, b = B} where P = test.Wide _; {extra = A, inner = B} = P.outer"),
            vec![Value::Record(Box::new([
                ("a".to_owned(), Value::Int(1)),
                ("b".to_owned(), Value::Int(2)),
            ]))],
        );
    }

    /// A **wildcard piece** binds nothing and cannot fail, so a pattern carrying one
    /// is the read of the pieces that are left. Glean's expansion drops these as
    /// tautologies; decomposing against a slot means they are never built.
    ///
    /// Note the pattern still has to name *every* field: records unify exactly here,
    /// with no row polymorphism, so `{inner = X}` against a two-field record is a
    /// type error rather than a partial match. That is typecheck's rule and this is
    /// the spelling it leaves for "I only want this piece".
    #[test]
    fn a_wildcard_piece_binds_nothing() {
        assert_eq!(
            shape("X where P = test.Wide _; {extra = _, inner = X} = P.outer"),
            shape("X where P = test.Wide _; X = P.outer.inner"),
        );

        assert_eq!(
            rows("X where P = test.Wide _; {extra = _, inner = X} = P.outer"),
            ints(&[2]),
        );
    }

    /// A **literal** leaf on the left is still refused, and the reason is the one
    /// [`Ast::is_destructurable`] gives: it binds nothing, so flatten would emit no
    /// constraint and the statement would silently mean *true* where it means the
    /// empty relation. Deciding it needs two values compared, which is the hard
    /// half.
    #[test]
    fn a_literal_piece_is_still_unification() {
        assert_eq!(
            front_end_codes("X where P = test.Nested _; {inner = 1} = P.outer"),
            ["nyi/bind-unification"],
        );
    }

    // ---- the deferred constructs -------------------------------------------

    /// What is left of `nyi/value-bind`: a right side that denotes **no location**.
    ///
    /// The code used to cover two different things. A field read names a place in a
    /// register and now substitutes; these do not, and are the derived bind the
    /// machine has a step for and the language still has no producer for.
    #[test]
    fn a_value_no_location_names_is_still_deferred() {
        // A record mentioning a captured variable: its value differs per row, and
        // it is in no register — it would have to be built.
        assert_eq!(
            compile("X where test.Nested {outer = {inner = Y}}; X = {inner = Y}").codes(),
            ["nyi/value-bind"],
        );
    }

    /// The folding rule's edges: what a bind may fold, and what it may not.
    #[test]
    fn a_constant_bind_folds_however_deep_it_is() {
        // A string folds as readily as an int.
        assert_eq!(rows("X where X = \"ann\""), strs(&["ann"]));

        // A *prefix* is not a value — it denotes a range, so there is nothing for a
        // variable bound to it to be, and nothing here binds `X` at all. It is a
        // **constraint** on where `X` lives, and this query gives it nowhere: the
        // fault is the missing generator, which is what it now says.
        assert_eq!(
            compile("X where X = \"a\"..").codes(),
            ["reject/unbound-variable"]
        );

        // A record of constants is a constant. The left side is a plain variable, so
        // this is an ordinary bind and not the `pattern = pattern` unification a
        // record on the *left* would be — that one typecheck defers, and the corpus
        // pins it, before flatten is reached.
        assert_eq!(
            rows("X where X = {inner = 1}"),
            vec![Value::Record(Box::new([(
                "inner".to_owned(),
                Value::Int(1)
            )]))],
        );

        // ...to any depth, and field order is the schema's rather than the source's,
        // since lowering sorts them.
        assert_eq!(
            rows("X where X = {extra = 2, inner = 1}"),
            rows("X where X = {inner = 1, extra = 2}"),
        );

        // A record mentioning a **captured** variable is not a constant: its value
        // differs per row. That is the derived bind this phase leaves unlowered.
        assert_eq!(
            compile("X where test.Nested {outer = {inner = Y}}; X = {inner = Y}").codes(),
            ["nyi/value-bind"],
        );
    }

    /// **A constant may be bound after a field has captured the variable.**
    ///
    /// The fold's counterpart to
    /// [`a_row_bound_after_the_field_that_reads_it_is_reordered`]: `test.Foo {id = N};
    /// N = 1` names `N` before the statement that says what it is, and that is an
    /// ordering artefact and nothing more — the fold is collected from the whole body
    /// before any statement is lowered, so both spellings reach `emit` with the same
    /// bindings and compile to the same plan.
    ///
    /// Unlike the row case this needs no reordering at all: a folded constant takes
    /// no register and no step, so there is no level to move.
    ///
    /// [`a_row_bound_after_the_field_that_reads_it_is_reordered`]: self::tests::a_row_bound_after_the_field_that_reads_it_is_reordered
    #[test]
    fn a_constant_may_be_bound_after_a_field_captures_it() {
        for (written_late, written_first) in [
            // A scalar, at a key field that the constant then narrows to a seek.
            (
                "X where test.Foo {id = N, name = X}; N = 1",
                "X where N = 1; test.Foo {id = N, name = X}",
            ),
            // A record of constants, at a record-typed field — the case whose bytes
            // carry a `MARK_RECORD` wrapper (see the test below), reached the other
            // way round.
            (
                "X where test.Nested {outer = X}; X = {inner = 1}",
                "X where X = {inner = 1}; test.Nested {outer = X}",
            ),
        ] {
            assert_eq!(
                shape(written_late),
                shape(written_first),
                "one query, two spellings, one plan: {written_late:?}"
            );
            assert_eq!(rows(written_late), rows(written_first), "{written_late:?}");
        }
    }

    /// **Destructuring a constant is the same as binding each piece.**
    ///
    /// `{a = X, b = Y} = {a = 1, b = 2}` is sugar, and the test is that it is *exactly*
    /// sugar: the same rows and the same plan as writing the two binds out. Nothing is
    /// compared at runtime, because each variable folds into the piece of the constant
    /// it lines up with.
    #[test]
    fn destructuring_a_constant_is_the_same_as_binding_each_piece() {
        assert_eq!(rows("X where {a = X} = {a = 1}"), rows("X where X = 1"));

        assert_eq!(
            shape("X where {a = X, b = Y} = {a = 1, b = 2}; test.Bar {id = X}"),
            shape("X where X = 1; Y = 2; test.Bar {id = X}"),
            "destructuring is the two binds written out"
        );

        // Nested, and against a field the fold then narrows — so the pieces reach the
        // seek exactly as a literal written in place would.
        assert_eq!(
            shape("X where {a = X} = {a = {inner = 1}}; test.Nested {outer = X}"),
            shape("X where X = {inner = 1}; test.Nested {outer = X}"),
        );
    }

    /// **Reading a field of a folded constant is itself a constant.**
    ///
    /// `A = {x = 2}` makes `A.x` the literal `2`, so the substitution has to go through
    /// the *access* and not stop at the variable. Stopping halfway went wrong in two
    /// different ways, neither of them an error message:
    ///
    /// - in the **head**, `resolve` declined quietly, so flatten returned no plan with
    ///   nothing reported — the "no plan without a reason" assertion firing as a panic;
    /// - at a **key field**, the constraint was dropped altogether, so the level
    ///   matched every row. A silent wrong answer, which is the worse of the two.
    ///
    /// Found from the shell, by hand, on `{x = A.x, y = A.y} where A = {x=2, y=3}`.
    #[test]
    fn a_field_read_through_a_folded_constant_is_folded_too() {
        // The head case: the whole query folds away, so this is one row of literals
        // and no levels at all.
        assert_eq!(
            rows("{x = A.x, y = A.y} where A = {x = 2, y = 3}"),
            vec![Value::Record(Box::new([
                ("x".to_owned(), Value::Int(2)),
                ("y".to_owned(), Value::Int(3)),
            ]))],
        );

        // The key-field case, against the literal written in place: same plan, so the
        // read narrows the seek exactly as `{id = 1}` does.
        assert_eq!(
            shape("A.x where A = {x = 1}; test.Bar {id = A.x}"),
            shape("1 where test.Bar {id = 1}"),
            "a field read through a fold narrows the seek like a literal"
        );

        // And the rows, which is what says the constraint did not go missing: `test.Bar`
        // has more than one fact, so "matched everything" is visible here.
        assert_eq!(
            rows("A.x where A = {x = 1}; test.Bar {id = A.x}"),
            rows("1 where test.Bar {id = 1}"),
        );
        assert!(
            rows("A.x where A = {x = 1}; test.Bar {id = A.x}").len()
                < rows("A.x where A = {x = 1}; test.Bar _").len(),
            "the premise: an unnarrowed scan of test.Bar returns more rows"
        );

        // To any depth.
        assert_eq!(rows("A.a.b where A = {a = {b = 7}}"), vec![Value::Int(7)]);
    }

    /// A field read on a **scalar** constant stays a type error: an integer has no
    /// fields, and typecheck says so before flatten sees it.
    ///
    /// This is the case the buggy arm's comment was written for, and it was right about
    /// — the mistake was assuming it covered records too.
    #[test]
    fn a_field_read_on_a_scalar_constant_is_a_type_error() {
        let schema = corpus::schema();

        for source in ["X where A = 42; X = A.x", "A.x where A = 42"] {
            let mut compilation = Compilation::new(source, &schema);

            assert!(compilation.plan().is_none(), "{source:?}");
            assert_eq!(
                compilation.diagnostics().codes().collect::<Vec<_>>(),
                ["reject/type-mismatch"],
                "{source:?}"
            );
        }
    }

    /// **One variable, one constant.**
    ///
    /// Two constant binds of one variable is unification — the same fault as a row
    /// claimed twice, and worse to get wrong, because `lookup` walks the bindings in
    /// reverse and would silently keep the *last*. Typecheck used to catch this
    /// incidentally by refusing any bind whose left side was already bound; that
    /// refusal is now narrowed, so the check has an owner.
    #[test]
    fn a_constant_bound_to_one_variable_twice_is_deferred() {
        let schema = corpus::schema();

        for source in [
            "Y where Y = 1; Y = 2",
            // Identical values are refused too: deciding they agree would mean
            // comparing encoded bytes, and a query saying it twice is degenerate
            // either way.
            "Y where Y = 1; Y = 1",
            // And with a capture in between, which is the shape that makes it
            // dangerous rather than merely redundant.
            "Y where test.Foo {id = Y}; Y = 1; Y = 2",
        ] {
            let mut compilation = Compilation::new(source, &schema);

            assert!(compilation.plan().is_none(), "{source:?}");
            assert_eq!(
                compilation.diagnostics().codes().collect::<Vec<_>>(),
                ["nyi/bind-unification"],
                "{source:?}"
            );
        }
    }

    /// **The trap a folded record walks past.** A record inside a field keeps its
    /// `MARK_RECORD` wrapper; a *stored key* is flat. Folding reaches
    /// [`constant`](Flattener::constant), whose record arm writes the wrapped form —
    /// which is right here and would be wrong for a whole key.
    ///
    /// It is safe because `key` destructures the top-level record itself and emits
    /// field by field, so a whole key never reaches `constant`; and a bare variable
    /// as a whole key is `nyi/whole-key` from `collect` before any of this. Pinned
    /// because both halves are invisible from the fold's own code, and getting it
    /// wrong reads the wrong bytes with no error — it matches nothing.
    #[test]
    fn a_folded_record_narrows_a_nested_field_and_still_matches() {
        assert_eq!(
            rows("X where X = {inner = 1}; test.Nested {outer = X}").len(),
            1
        );

        // The written form is the oracle: a fold must reach the *same row*, which
        // projecting the matched fact's identity says exactly.
        assert_eq!(
            rows("R where X = {inner = 1}; R = test.Nested {outer = X}"),
            rows("R where R = test.Nested {outer = {inner = 1}}"),
        );
        assert_eq!(
            rows("R where R = test.Nested {outer = {inner = 1}}").len(),
            1,
            "the oracle has to match something for the comparison to mean anything",
        );

        // And it is a seek, not a scan-and-filter: `outer` is the leading key field.
        let flattened = compile("X where X = {inner = 1}; test.Nested {outer = X}");
        match &flattened
            .plan()
            .level(0)
            .expect("a level")
            .sole_source()
            .expect("one source")
            .seek_key()
            .expect("a seek")
        {
            SeekKey::Prefix(bytes) => assert!(!bytes.is_empty(), "a constant prefix"),
            SeekKey::Composite(parts) => assert!(
                matches!(parts.first(), Some(SeekKeyPart::Bytes(_))),
                "the fold must reach the seek prefix, got {parts:?}",
            ),
        }
    }

    /// **Reading through a reference is a level of its own** — the fact the id
    /// names, fetched into a register, and read from there like any other row.
    ///
    /// Both sides of that fact: a key field is bytes in the fetched row, and the
    /// value is one point read further, off the same register.
    ///
    /// **The trap the split guards:** a register holds its own row's key bytes, so
    /// splicing those where a fact id belongs would compare a key against an id and
    /// quietly match nothing. *Following* a reference splices `Register::fact_id`
    /// for exactly that reason; *reading through* one is this fetch, and the two
    /// are still different plans.
    #[test]
    fn reading_through_a_reference_fetches_the_fact_it_names() {
        assert_eq!(
            shape("X.name where test.Ref {of = X}"),
            lines(&[
                "r0 <- test.Ref scan",
                "r1 <- test.Foo fetch[r0.0]",
                "head r1.1:str",
            ]),
        );

        assert_eq!(
            shape("X.value where test.Ref {of = X}"),
            lines(&[
                "r0 <- test.Ref scan",
                "r1 <- test.Foo fetch[r0.0]",
                "head r1.value:str",
            ]),
        );
    }

    /// A reference that is not the leading key field is followed just the same:
    /// the fetch names the field it reads, not a position in the seek.
    #[test]
    fn a_fetch_reads_the_reference_field_wherever_it_sits() {
        assert_eq!(
            shape("Y where test.Link {at = 11, of = P}; Y = P.name"),
            lines(&[
                "r0 <- test.Link seek[k]",
                "r1 <- test.Foo fetch[r0.1]",
                "head r1.1:str",
            ]),
        );
    }

    /// **A chain of references is a chain of fetches**, each reading the register
    /// the one before it bound.
    #[test]
    fn a_reference_to_a_reference_is_two_fetches() {
        assert_eq!(
            shape("N where test.Deep {via = R}; N = R.of.name"),
            lines(&[
                "r0 <- test.Deep scan",
                "r1 <- test.Ref fetch[r0.0]",
                "r2 <- test.Foo fetch[r1.0]",
                "head r2.1:str",
            ]),
        );
    }

    /// **Two reads of one reference are one fetch.** A point read per read would
    /// fetch the same row twice for every row of the level above it, and the
    /// second copy would be a register that can never disagree with the first.
    #[test]
    fn two_reads_of_one_reference_share_a_fetch() {
        assert_eq!(
            shape("{a = X.id, b = X.name} where test.Ref {of = X}"),
            lines(&[
                "r0 <- test.Ref scan",
                "r1 <- test.Foo fetch[r0.0]",
                "head {a = r1.0:int, b = r1.1:str}",
            ]),
        );
    }

    /// A field read through a reference **narrows the level that reads it**, like
    /// any other bound value: the fetch is an outer level, so its register is
    /// spliceable into the seek below it.
    ///
    /// This is what the hoist ordering is for. Were the fetch emitted after the
    /// level that reads it, the splice would name a register bound *inside* itself
    /// — which the executor's field-offset cache is entitled to assume cannot
    /// happen.
    #[test]
    fn a_field_read_through_a_reference_seeks() {
        assert_eq!(
            shape("P.id where test.Ref {of = P}; test.Bar {id = P.id}"),
            lines(&[
                "r0 <- test.Ref scan",
                "r1 <- test.Foo fetch[r0.0]",
                "r2 <- test.Bar seek[r1.0]",
                "head r1.0:int",
            ]),
        );
    }

    /// The **nested spelling is still a join**, not a fetch: a fact pattern
    /// written inside another is a generator, and matching one against a reference
    /// compares ids without reading anything. Only a *read* through a reference
    /// costs a lookup.
    #[test]
    fn a_nested_pattern_is_a_join_rather_than_a_fetch() {
        assert_eq!(
            shape("Y where test.Ref {of = test.Foo {id = 1, name = Y}}"),
            lines(&[
                "r0 <- test.Foo seek[k]",
                "r1 <- test.Ref seek[r0#]",
                "head r0.1:str",
            ]),
        );
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

    // ---- subqueries ---------------------------------------------------------

    /// **A subquery inlines**, so it needs no operator and no nested run: its
    /// statements become the enclosing query's, and its head is the value the
    /// bind names. The plan is the one the same query written flat compiles to.
    #[test]
    fn a_subquery_inlines_into_the_query_around_it() {
        assert_eq!(
            shape("X where X = (Y where test.Foo {id = Y})"),
            shape("X where test.Foo {id = X}"),
        );
    }

    /// Its statements are ordinary statements afterwards, so `reorder` places
    /// them with everything else and a subquery can be joined against.
    #[test]
    fn a_subquery_joins_with_the_statements_around_it() {
        assert_eq!(
            shape("X where test.Bar {id = X}; W = (Y where test.Foo {id = X, name = Y})"),
            lines(&[
                "r0 <- test.Bar scan",
                "r1 <- test.Foo seek[r0.0]",
                "head r0.0:int",
            ]),
        );
    }

    /// A name the subquery binds fresh and a **later** statement binds too is two
    /// variables to typecheck, which scoped the first away, and would be one to
    /// flatten, which inlines. Refused rather than silently conflated.
    ///
    /// Reading an *outer* name is the opposite case and is allowed — that is what
    /// correlation is, and the test above relies on it.
    #[test]
    fn a_subquery_reusing_an_outer_name_is_refused() {
        assert_eq!(
            compile("X where X = (Y where test.Foo {id = Y}); test.Bar {id = Y}").codes(),
            ["nyi/subquery"]
        );
    }

    // ---- comparing two bound values ----------------------------------------

    /// **`X = Y` with both sides bound is a residual on the level that binds
    /// later.** It needs no step of its own and nothing new in the machine: a
    /// residual is checked against the row a level is scanning, against registers
    /// filled outside it, which is exactly the shape of the constraint.
    #[test]
    fn comparing_two_bound_variables_is_a_residual_on_the_inner_level() {
        assert_eq!(
            shape("X where test.Foo {id = X}; test.Bar {id = Y}; X = Y"),
            lines(&[
                "r0 <- test.Foo scan",
                "r1 <- test.Bar scan where 0 == r0.0",
                "head r0.0:int",
            ]),
        );
    }

    /// The comparison is **symmetric**, and the order it is written in does not
    /// change the plan: the residual belongs to whichever level is inner, which is
    /// a fact about the order `reorder` chose rather than about the source.
    #[test]
    fn a_comparison_reads_both_sides_whichever_way_it_is_written() {
        assert_eq!(
            shape("X where test.Foo {id = X}; test.Bar {id = Y}; X = Y"),
            shape("X where test.Foo {id = X}; test.Bar {id = Y}; Y = X"),
        );
    }

    /// A comparison **claims neither side**, so the keys that mention them still
    /// capture them — and it is *read*-only, so it cannot be ordered before the
    /// levels that bind what it compares.
    #[test]
    fn a_comparison_is_ordered_after_both_levels_it_reads() {
        let deps = deps_of("X where test.Bar {id = Y}; X = Y; test.Foo {id = X, name = _}");

        // Written second, and it must not run until both are bound.
        assert!(deps.stmt(1).expect("the comparison").captures.is_empty());
        assert_eq!(deps.stmt(1).expect("the comparison").reads.len(), 2);
    }

    /// Two fields of the **same** row is a different question — an intra-row
    /// repeat, which is its own deferral and its own decision.
    #[test]
    fn comparing_two_fields_of_one_row_is_still_deferred() {
        assert_eq!(
            compile("X where test.Edge {from = X, to = Y}; X = Y").codes(),
            ["nyi/repeated-variable"]
        );
    }

    // ---- disjunction and `never` -------------------------------------------

    /// **A disjunction is one level with an alternative per branch** — not a level
    /// per branch, and not a DNF expansion across the conjuncts around it.
    #[test]
    fn a_disjunction_is_one_level_with_a_source_per_branch() {
        assert_eq!(
            shape("X where test.Foo {id = X} | test.Bar {id = X}"),
            lines(&["r0 <- test.Foo scan | test.Bar scan", "head r0.0:int"]),
        );
    }

    /// **No DNF expansion across conjuncts**, which is the claim that keeps a
    /// disjunction affordable at all.
    ///
    /// Three two-branch disjunctions in conjunction are 2³ = 8 clauses if the
    /// alternation is distributed over the conjunction, and 3 levels of 2 sources if
    /// it is not. The plan is **linear in the branches** — one source per branch
    /// written, no matter how many disjunctions sit beside it — which is what makes
    /// the exponential shape unreachable rather than merely unlikely.
    #[test]
    fn conjoined_disjunctions_do_not_multiply() {
        let flattened = compile(
            "X where test.Foo {id = X} | test.Bar {id = X}; \
             test.Node {id = X} | test.Edge {from = X, to = _}; \
             test.Count X | test.Nested {outer = {inner = X}}",
        );

        let sources: Vec<usize> = flattened
            .plan()
            .body
            .iter()
            .filter_map(|step| match step {
                Step::Level(level) => Some(level.sources.len()),
                Step::Derive(_) | Step::Test(_) => None,
            })
            .collect();

        assert_eq!(
            sources,
            vec![2, 2, 2],
            "one level per statement, one source per branch"
        );
    }

    /// A branch narrows on its own: each alternative builds its own seek, so one
    /// can be a seek while another is a scan.
    #[test]
    fn each_branch_builds_its_own_seek() {
        assert_eq!(
            shape("X where test.Foo {id = 1, name = X} | test.Foo {id = _, name = X}"),
            lines(&["r0 <- test.Foo seek[k] | test.Foo scan", "head r0.1:str"]),
        );
    }

    /// **`never` is a level with no alternative to open** — the empty relation,
    /// which the machine already had a shape for.
    #[test]
    fn never_is_a_level_with_no_sources() {
        assert_eq!(
            shape("X where X = never"),
            lines(&["r0 <- never", "head r0"]),
        );
    }

    /// **`never` is the identity of `|`**, and the implementation says so by
    /// dropping the branch rather than by special-casing it anywhere later.
    #[test]
    fn a_never_branch_drops_out_of_a_disjunction() {
        assert_eq!(
            shape("X where test.Bar {id = X} | never"),
            shape("X where test.Bar {id = X}"),
        );
    }

    /// **A variable only one branch binds is not bound after the statement.** The
    /// captures intersect, so the head's read has nothing behind it — reported at
    /// the read, which is where a person can act on it, rather than as a
    /// run-time read of a register the taken branch never wrote.
    #[test]
    fn a_variable_only_one_branch_binds_does_not_escape() {
        assert_eq!(
            compile("Y where test.Foo {id = X, name = Y} | test.Bar {id = X}").codes(),
            ["reject/unbound-variable"]
        );
    }

    /// A variable **both** branches bind has to be in the same place in each: the
    /// register holds one row and the plan reads it by one path, so a variable at
    /// a different field in another branch would decode the wrong bytes for half
    /// the rows.
    #[test]
    fn a_variable_at_a_different_field_in_two_branches_is_refused() {
        assert_eq!(
            compile("X where test.Edge {from = 1, to = X} | test.Bar {id = X}").codes(),
            ["nyi/disjunction"]
        );
    }

    /// What is left of `nyi/disjunction`: an alternation *inside* a pattern.
    /// Distributing it outward — Glean's "PLAN B" — means rewriting the enclosing
    /// pattern once per branch, which needs tree nodes flatten cannot make.
    #[test]
    fn an_alternation_inside_a_pattern_is_not_implemented_yet() {
        assert_eq!(
            compile("X where test.Bar {id = X}; test.Node {id = 1 | 2}").codes(),
            ["nyi/disjunction"]
        );
    }

    /// **A whole key is its fields**, and that is the whole implementation: a
    /// stored key is flat, so a capture projects as a record built one field at a
    /// time, and a *scalar* key stays the one field it always was.
    #[test]
    fn a_whole_record_key_binds_to_a_record_of_its_fields() {
        assert_eq!(
            shape("Y where test.Foo Y"),
            lines(&[
                "r0 <- test.Foo scan",
                "head {id = r0.0:int, name = r0.1:str}"
            ]),
        );

        assert_eq!(
            shape("Y where test.Count Y"),
            lines(&["r0 <- test.Count scan", "head r0.0:int"]),
            "a scalar key is one field",
        );
    }

    /// **Read as an input, a whole key splices every field in declared order** —
    /// which is byte-for-byte the key the register holds, because the layout is
    /// flat. The point of the test is that it is a *seek* and not a scan with
    /// filters: the fields go into the prefix, in order, from field 0.
    #[test]
    fn a_whole_key_read_back_splices_each_field_in_order() {
        // `test.Bar` and `test.Node` are both `{id : int}`, so one's key is a
        // pattern for the other's.
        assert_eq!(
            shape("Y where test.Bar Y; test.Node Y"),
            lines(&[
                "r0 <- test.Bar scan",
                "r1 <- test.Node seek[r0.0]",
                "head {id = r0.0:int}",
            ]),
        );
    }

    /// A field of a whole key is a field of the row it came from, so naming one
    /// costs no register and no step — the same answer a row gives.
    #[test]
    fn a_field_of_a_whole_key_is_a_field_of_its_row() {
        assert_eq!(
            shape("Y.name where test.Foo Y"),
            lines(&["r0 <- test.Foo scan", "head r0.1:str"]),
        );
    }

    /// What is left of `nyi/whole-key`, and it is a different thing from what the
    /// code used to mean: a whole key matched **into a record field**. The two are
    /// the same record and not the same bytes — flat against wrapped — so building
    /// the match out of the fields would compare the wrong things and match
    /// nothing.
    /// Needs a schema the fixture deliberately does not have — a predicate whose
    /// *whole key* is also some other predicate's *field* type — so it is built
    /// here rather than added to the shared fixture, which every battery and the
    /// corpus would pay for. That is also why this code is the one `nyi/` with no
    /// corpus entry: the corpus can only say what the fixture can express.
    #[test]
    fn matching_a_whole_key_against_a_record_field_is_not_implemented_yet() {
        use crate::focus::schema::Predicate;
        use ::lasso::Rodeo;
        use std::sync::Arc;

        let mut names = Rodeo::new();
        let mut sym = |s: &str| names.get_or_intern(s);

        // `t.Point` is `{x : int}`; `t.Box`'s `at` field is the same record.
        let point = PredicateTy::Record(Arc::from([(sym("x"), PredicateTy::Int)]));
        let predicates = vec![
            Predicate {
                name: sym("t.Point"),
                key: point.clone(),
                value: None,
            },
            Predicate {
                name: sym("t.Box"),
                key: PredicateTy::Record(Arc::from([(sym("at"), point)])),
                value: None,
            },
        ];
        let schema = Schema::new(names.into_reader(), Arc::from(predicates));

        let mut compilation = Compilation::new("Y where t.Point Y; t.Box {at = Y}", &schema);

        assert!(compilation.plan().is_none(), "expected no plan");
        assert_eq!(
            compilation.diagnostics().codes().collect::<Vec<_>>(),
            ["nyi/whole-key"],
        );
    }

    /// **A reference held in a fact's value** is what is left of
    /// `nyi/fact-field`.
    ///
    /// A fetch reads its id out of a register's *key* bytes, and a value is in the
    /// other column family — so following one would mean a fetch whose reference is
    /// itself a fetch, which nothing holds. Needs a bespoke schema: no fixture
    /// predicate has a fact-typed value, which is also why this arm used to decline
    /// **quietly** and why the `flatten_ordered` promise-guard is what would have
    /// caught it.
    #[test]
    fn reading_through_a_reference_in_a_value_is_not_implemented_yet() {
        use crate::focus::schema::Predicate;
        use ::lasso::Rodeo;
        use std::sync::Arc;

        let mut names = Rodeo::new();
        let mut sym = |s: &str| names.get_or_intern(s);

        // `t.Owner`'s *value* is a reference to a `t.Thing`, whose key has a name.
        let predicates = vec![
            Predicate {
                name: sym("t.Thing"),
                key: PredicateTy::Record(Arc::from([(sym("name"), PredicateTy::Str)])),
                value: None,
            },
            Predicate {
                name: sym("t.Owner"),
                key: PredicateTy::Record(Arc::from([(sym("id"), PredicateTy::Int)])),
                value: Some(PredicateTy::Fact(PredicateId(0))),
            },
        ];
        let schema = Schema::new(names.into_reader(), Arc::from(predicates));

        let mut compilation = Compilation::new("O.value.name where O = t.Owner _", &schema);

        assert!(compilation.plan().is_none(), "expected no plan");
        assert_eq!(
            compilation.diagnostics().codes().collect::<Vec<_>>(),
            ["nyi/fact-field"],
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

    /// **A source order that already works is flattened untouched**, verified by
    /// plan equality: flattening these queries is the same as flattening them in the
    /// order they were written.
    ///
    /// Not the identity in general any more — `reorder` moves what has to move, and
    /// [`a_row_bound_after_the_field_that_reads_it_is_reordered`] is that case. This
    /// is the other half of the claim, and the more important one for a reader: a
    /// query that compiled before this module chose anything still compiles to the
    /// very same plan.
    ///
    /// [`a_row_bound_after_the_field_that_reads_it_is_reordered`]: self::tests::a_row_bound_after_the_field_that_reads_it_is_reordered
    #[test]
    fn a_valid_source_order_is_flattened_untouched() {
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
    /// Also the shape with **no generator at all**: the bind folds away entirely,
    /// leaving a plan with no steps, which answers exactly one row.
    #[test]
    fn a_value_bind_returns_the_value() {
        assert_eq!(rows("X where X = 42"), ints(&[42]));
        assert_eq!(rows("X where X = \"ann\""), strs(&["ann"]));
    }

    /// A folded bind **narrowing a scan**: the constant reaches the key field by
    /// substitution, so it seeks or filters exactly as the literal written in place
    /// would — which is the point of folding rather than binding a register.
    ///
    /// Whether it lands in the seek prefix or in a residual is sargeability's
    /// ordinary decision about *that field*, not a fact about the fold.
    #[test]
    fn a_folded_bind_narrows_a_seek() {
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

    /// A fold must leave **resume** exactly as it was.
    ///
    /// It should, by construction: a folded constant is in the plan's bytes and its
    /// head, not in a register, so there is nothing for a cursor to carry and
    /// nothing to recompute. That is the argument, and this is the check on it —
    /// resume == uninterrupted at every cut point over plans built from folded
    /// binds, including one the head reads beside a captured field.
    ///
    /// The purity invariant's own guard is
    /// `iter::a_derive_is_recomputed_across_every_cut_point`, which drives a real
    /// [`Step::Derive`](crate::focus::plan::Step) from a hand-built plan — nothing
    /// in the language lowers one, so nothing here can reach it.
    /// A query whose **every** binding folded has no levels, so it cannot be
    /// suspended past — and reports `Done` when asked to suspend rather than
    /// handing back a cursor that would re-emit its row.
    ///
    /// The rows are the point of the assertion; the zero round-trips are the rule.
    #[test]
    fn a_query_with_no_levels_answers_without_suspending() {
        let flattened = compile("X where X = 42");
        let plan = flattened.plan().clone();

        assert_eq!(plan.levels(), 0, "everything folded, so there is no loop");
        assert!(plan.body.is_empty(), "...and so no step either");

        let mut mk = || (store(), plan.clone());
        let cuts = std::collections::BTreeSet::from([1]);
        let (rows, suspends) = run_with_suspends(&mut mk, &flattened.interner, &cuts).expect("run");

        assert_eq!(rows, ints(&[42]), "the row is still the answer");
        assert_eq!(
            suspends, 0,
            "a plan with no levels reports Done at a suspend request, since an \
             empty cursor would restart it and re-emit the row",
        );
    }

    #[test]
    fn resume_is_unaffected_by_a_folded_bind() {
        for source in [
            // Recomputed before the level whose seek reads it.
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
/// | a **negation** over a captured variable | a `Step::Test`, and a probe that seeks or filters |
/// | a **second branch** on a statement | a level with two `Source`s — and, on a negation, a test with two |
///
/// The last two also carry the **algebraic laws**, which are the properties that use
/// no model at all: a negation and its assertion partition the rows, a disjunction is
/// the concatenation of its branches, and `!(A | B)` is `!A; !B`. Each is
/// `prop_filter`ed onto the draws that can carry it, which costs a redraw rather than
/// a weaker generator.
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

    /// Prefixes to draw for a **constraint** — `V0 = "a"..`. `"b"` is left to the
    /// key-field prefix above on purpose: it matches one string of the three, and a
    /// filter that severe applied to a quarter of the population thins the rows
    /// every other property in this file runs over (the census is what says so).
    const CONSTRAINT_PREFIXES: [&str; 2] = ["", "a"];

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
        /// One field pattern per **branch** — `gen.P {…} | gen.P {…}`. One branch is
        /// an ordinary statement; more is a disjunction.
        ///
        /// Always the **same predicate**, and every branch keeps the *variable*
        /// leaves of the first in the same fields. Both are forced rather than
        /// chosen: a register holds one row and the plan holds one path into it, so
        /// a variable a disjunction binds has to live at the same place in every
        /// branch, and a variable only some branch binds does not escape the
        /// statement at all. Branches therefore differ only in what they *match* —
        /// which is the shape Glean distributes an alternation inside a pattern
        /// into, and the one worth generating.
        branches: Vec<Vec<FieldPat>>,
    }

    impl StmtSpec {
        /// The field variables this pattern names.
        ///
        /// For a generator these are captures-or-reads and the order decides which;
        /// for a **negation** they are reads outright, which is what makes them an
        /// ordering constraint the spec has to know about.
        fn vars(&self) -> Vec<usize> {
            fn of(leaf: &Leaf, out: &mut Vec<usize>) {
                if let Leaf::Var(var) = leaf {
                    out.push(*var);
                }
            }

            let mut out = vec![];
            for field in self.branches.iter().flatten() {
                match field {
                    FieldPat::Leaf(leaf) => of(leaf, &mut out),
                    FieldPat::Nested(subs) => subs.iter().for_each(|leaf| of(leaf, &mut out)),
                }
            }
            out
        }

        /// The first branch — what a single-branch statement *is*, and what the
        /// others are built from.
        fn first(&self) -> &[FieldPat] {
            &self.branches[0]
        }
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
        /// `R.f{k}.f{j}` → a field of the fact `R`'s reference field names: a
        /// `Source::Fetch`, and the only head item that costs a second lookup.
        Deref(usize, usize, usize),
    }

    /// A generated query, the store it runs against, and what it means.
    #[derive(Debug, Clone)]
    pub struct QueryAndStore {
        schema: Vec<PredSpec>,
        /// `facts[p]` — predicate `p`'s facts, deduplicated and sorted by key.
        facts: Vec<Vec<Fact>>,
        stmts: Vec<StmtSpec>,
        /// `V{var} = "prefix"..` — a **constraint** on a variable some statement
        /// captures. At most one, because it is a statement like any other and the
        /// order properties run every permutation of the body.
        ///
        /// Drawn only over variables the query already captures, since a constraint
        /// binds nothing: constraining one nothing binds is
        /// `reject/unbound-variable`, not a query this generator may draw.
        constraints: Vec<(usize, &'static str)>,
        /// `!gen.P{p} {f{k} = V{v}}` — a **negation**, over a variable the query
        /// captures. At most one, for the reason there is at most one constraint.
        ///
        /// The same [`StmtSpec`] a generator is, because that is what a negation
        /// is: a pattern, matched the same way, whose rows must not exist. Drawn
        /// only over captured variables, since one it alone names is a wildcard
        /// flatten refuses to guess at (`nyi/negation`).
        negations: Vec<StmtSpec>,
        head: Vec<HeadItem>,
    }

    /// What a statement index names.
    ///
    /// The body is three lists in one index space — generators, then constraints,
    /// then negations — and every property that orders statements has to agree
    /// about which is which. One function answers that, rather than the same
    /// arithmetic written out at each site.
    enum Which<'a> {
        Gen(&'a StmtSpec),
        Constrain(usize),
        Negate(&'a StmtSpec),
    }

    impl QueryAndStore {
        /// The body's length — generators **and** constraints, since an order names
        /// every statement flatten collected and a constraint is one of them.
        pub fn statements(&self) -> usize {
            self.stmts.len() + self.constraints.len() + self.negations.len()
        }

        /// Which of the three lists a statement index falls in.
        fn which(&self, stmt: usize) -> Which<'_> {
            if let Some(spec) = self.stmts.get(stmt) {
                return Which::Gen(spec);
            }

            let past_generators = stmt - self.stmts.len();

            match self.constraints.get(past_generators) {
                Some(_) => Which::Constrain(past_generators),
                None => Which::Negate(&self.negations[past_generators - self.constraints.len()]),
            }
        }

        /// How many variables this query constrains, for the census: the source
        /// cannot be asked, because a **key field** may be a string prefix too and
        /// the two read alike.
        pub fn constraints(&self) -> usize {
            self.constraints.len()
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
                        HeadItem::Deref(row, field, target) => {
                            format!("R{row}.f{field}.f{target}")
                        }
                    };
                    format!("h{h} = {item}")
                })
                .collect();

            format!("{{{}}}", fields.join(", "))
        }

        fn statement_source(&self, stmt: usize) -> String {
            match self.which(stmt) {
                Which::Gen(spec) => self.pattern_source(spec),
                Which::Constrain(index) => {
                    let (var, prefix) = self.constraints[index];
                    format!("V{var} = {prefix:?}..")
                }
                // `!` prefixes the statement, so it sits outside the bind — which a
                // negation never has anyway, there being no row to name.
                Which::Negate(spec) => format!("!{}", self.pattern_source(spec)),
            }
        }

        /// One statement's pattern, with no `!` — what a negation negates, and what
        /// the same statement would be if it were asserted instead.
        ///
        /// Every branch, joined by `|`. A disjunction is **flat** in the grammar, so
        /// this is the whole of rendering one.
        fn pattern_source(&self, spec: &StmtSpec) -> String {
            let branches: Vec<String> = spec
                .branches
                .iter()
                .map(|fields| self.branch_source(spec, fields))
                .collect();

            format!("{}{}", bind_source(spec), branches.join(" | "))
        }

        /// One branch of a statement: `gen.P{n} {…}`, with no bind and no `|`.
        fn branch_source(&self, spec: &StmtSpec, branch: &[FieldPat]) -> String {
            let fields: Vec<String> = branch
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

            format!("gen.P{} {{{}}}", spec.predicate, fields.join(", "))
        }

        /// Whether this draw carries a negation — the queries the complement law
        /// below is about, and the reason it is `prop_filter`ed rather than assumed.
        pub fn has_negation(&self) -> bool {
            !self.negations.is_empty()
        }

        /// The same query with its negation written **positively** — `!S` becomes
        /// `S`, so the statement generates where it used to filter.
        ///
        /// This and [`source_without_the_negation`](Self::source_without_the_negation)
        /// are the two halves the complement law compares against: a row of the
        /// unfiltered query either has a witness, in which case the positive form
        /// produces it, or it has none, in which case the negation keeps it. Nothing
        /// about that reasoning goes through the model, which is the point of
        /// stating it — the model and the executor could agree about what a negation
        /// means and both be wrong.
        pub fn source_asserting_the_negation(&self) -> String {
            self.source_with(|this, spec| Some(this.pattern_source(spec)))
        }

        /// The same query with the negation **dropped** — the rows it filters.
        pub fn source_without_the_negation(&self) -> String {
            self.source_with(|_, _| None)
        }

        /// The same query with its negation written **twice**, which must answer
        /// exactly as once: a filter is idempotent, and a step that got its
        /// one-bit frame state wrong would not be.
        pub fn source_negating_twice(&self) -> String {
            self.source_with(|this, spec| {
                let pattern = this.pattern_source(spec);
                Some(format!("!{pattern}; !{pattern}"))
            })
        }

        /// Whether some statement is a **disjunction** — the queries the branch laws
        /// below are about.
        pub fn has_disjunction(&self) -> bool {
            self.disjunctive_statement().is_some()
        }

        /// Whether the negation is over more than one branch — `!(A | B)`, which is
        /// what De Morgan's law needs.
        pub fn has_disjunctive_negation(&self) -> bool {
            self.negations.iter().any(|spec| spec.branches.len() > 1)
        }

        /// The first statement with more than one branch.
        ///
        /// The first rather than all of them: the laws rewrite one statement and
        /// leave the rest of the query alone, which is what makes the comparison
        /// about that statement.
        fn disjunctive_statement(&self) -> Option<usize> {
            self.stmts.iter().position(|spec| spec.branches.len() > 1)
        }

        /// The query with the disjunctive statement's branches replaced by `chosen`
        /// — indices into its branch list, written in the order given.
        ///
        /// `&[0]` is that statement with only its first branch, `&[1, 0]` is the
        /// same disjunction with its branches swapped.
        pub fn source_with_branches(&self, chosen: &[usize]) -> String {
            let target = self
                .disjunctive_statement()
                .expect("a disjunctive statement to rewrite");

            let body: Vec<String> = self
                .identity()
                .into_iter()
                .map(|stmt| {
                    if stmt != target {
                        return self.statement_source(stmt);
                    }

                    let spec = &self.stmts[target];
                    let branches: Vec<String> = chosen
                        .iter()
                        .map(|&k| self.branch_source(spec, &spec.branches[k]))
                        .collect();

                    format!("{}{}", bind_source(spec), branches.join(" | "))
                })
                .collect();

            format!("{} where {}", self.head_source(), body.join("; "))
        }

        /// The same query with `!(A | B)` written as `!A; !B` — the other side of
        /// De Morgan's law.
        pub fn source_de_morgan(&self) -> String {
            self.source_with(|this, spec| {
                Some(
                    spec.branches
                        .iter()
                        .map(|branch| format!("!{}", this.branch_source(spec, branch)))
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            })
        }

        /// The query with each negation rewritten by `render`, or dropped where it
        /// returns `None`. Statement order is the written one.
        fn source_with(&self, render: impl Fn(&Self, &StmtSpec) -> Option<String>) -> String {
            let body: Vec<String> = self
                .identity()
                .into_iter()
                .filter_map(|stmt| match self.which(stmt) {
                    Which::Negate(spec) => render(self, spec),
                    _ => Some(self.statement_source(stmt)),
                })
                .collect();

            format!("{} where {}", self.head_source(), body.join("; "))
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
            (0..self.statements()).collect()
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

        /// **Every** order of the body, safe or not.
        ///
        /// What [`orders`](Self::orders) filters out is exactly what a query may not
        /// be *handed* — flatten in a given order refuses a read before its bind —
        /// but it is not what a query may not be *written* in, because `reorder`
        /// chooses. So the source-rewriting property gets all of them, and the
        /// filtered list stays for the properties that pass an order in explicitly.
        pub fn all_orders(&self) -> Vec<Vec<usize>> {
            permutations(&self.identity())
        }

        /// Whether `order` binds every row before a reference field reads it, and
        /// every constrained variable before the constraint reads it.
        fn respects(&self, order: &[usize]) -> bool {
            let mut bound: Vec<usize> = vec![];
            let mut captured: Vec<usize> = vec![];

            for &stmt in order {
                // A **constraint** and a **negation** both bind nothing and read
                // their variables, so an earlier statement has to mention each —
                // the first that does is the one that captures it, whichever it is.
                let spec = match self.which(stmt) {
                    Which::Gen(spec) => spec,

                    Which::Constrain(index) => {
                        let (var, _) = self.constraints[index];

                        if !captured.contains(&var) {
                            return false;
                        }

                        continue;
                    }

                    Which::Negate(spec) => {
                        if spec.vars().iter().any(|var| !captured.contains(var)) {
                            return false;
                        }

                        continue;
                    }
                };

                for pat in spec.branches.iter().flatten() {
                    if let FieldPat::Leaf(Leaf::Row(row)) = pat
                        && !bound.contains(row)
                    {
                        return false;
                    }
                }

                // Captures from the **first** branch: every branch binds the same
                // variables at the same fields by construction, so one of them is
                // the answer and reading all of them would double-count.
                for pat in spec.first() {
                    match pat {
                        FieldPat::Leaf(Leaf::Var(var)) => captured.push(*var),
                        FieldPat::Nested(subs) => {
                            captured.extend(subs.iter().filter_map(|leaf| match leaf {
                                Leaf::Var(var) => Some(*var),
                                _ => None,
                            }))
                        }
                        _ => {}
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
                if self.constrained(env) && !self.witnessed(env) {
                    rows.push(self.project(env));
                }
                return;
            }

            // A **constraint** and a **negation** iterate nothing: both are checked
            // once the whole row is built, which is the same set of rows in the same
            // order as checking either the moment its variables are bound. The model
            // reads them that way because the reading is obviously right — the
            // compiler is the one claiming a seek, or a probe placed mid-plan, is
            // the same thing.
            let Which::Gen(spec) = self.which(order[depth]) else {
                self.walk(order, depth + 1, env, rows);
                return;
            };

            // **Branch-major**, which is what the machine does with a level's
            // sources: it drains the first alternative, then the next. So a
            // disjunction *concatenates* its branches rather than merging them, and
            // a fact matching two branches is answered twice. Written out here as
            // two loops for the same reason the rest of the model is written the
            // slow way — it is the obvious reading, and the compiler is the one
            // making a claim about it.
            for branch in &spec.branches {
                for (index, fact) in self.facts[spec.predicate].iter().enumerate() {
                    let saved = env.clone();

                    if matches(branch, fact, env) {
                        if let Some(row) = spec.row {
                            env.rows[row] = Some((spec.predicate, index));
                        }
                        self.walk(order, depth + 1, env, rows);
                    }

                    *env = saved;
                }
            }
        }

        /// Whether every constraint holds of this row.
        fn constrained(&self, env: &Env) -> bool {
            self.constraints.iter().all(|(var, prefix)| {
                match env.vars[*var]
                    .as_ref()
                    .expect("a constrained variable is captured")
                {
                    FieldVal::Str(text) => text.starts_with(prefix),
                    // Only `Str` positions are drawn a prefix.
                    FieldVal::Int(_) => unreachable!("a prefix constrains a string"),
                }
            })
        }

        /// Whether any negation finds a **witness** — a fact that matches it — in
        /// which case this row is not an answer.
        ///
        /// The obvious reading, as the rest of the model is: look at every fact of
        /// the predicate and ask whether one matches. What the compiler claims
        /// instead is that a seek narrowed by the bound registers finds the same
        /// answer while reading at most one row, which is the thing worth checking.
        fn witnessed(&self, env: &Env) -> bool {
            self.negations.iter().any(|spec| {
                spec.branches.iter().any(|branch| {
                    self.facts[spec.predicate]
                        .iter()
                        .any(|fact| matches(branch, fact, &mut env.clone()))
                })
            })
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

                // Two facts: the row's reference field says which fact of
                // `REFERENCED` to read, and `target` says which of *its* fields.
                HeadItem::Deref(r, field, target) => {
                    let (predicate, index) = row(*r);

                    let GenVal::Ref(sequence) = &self.facts[predicate][index].key[*field] else {
                        panic!("a deref head item names a reference field");
                    };

                    let referenced = &self.facts[REFERENCED.0 as usize];
                    referenced[*sequence as usize - 1].key[*target].to_value()
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

    /// `R0 = `, or nothing — the row a statement binds, rendered.
    fn bind_source(spec: &StmtSpec) -> String {
        match spec.row {
            Some(row) => format!("R{row} = "),
            None => String::new(),
        }
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

    /// Match one **branch** of a statement against one fact, binding what it
    /// captures.
    ///
    /// Leaves partial bindings behind on failure; the caller restores from its own
    /// copy, which is what makes backtracking here trivial and the model easy to
    /// believe.
    fn matches(branch: &[FieldPat], fact: &Fact, env: &mut Env) -> bool {
        for (f, pat) in branch.iter().enumerate() {
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
        /// Whether this statement is a **disjunction**, and which values the second
        /// branch matches — two digits of one draw, as the constraint and negation
        /// draws are.
        branch: u8,
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
        filters: FilterDraw,
    ) -> QueryAndStore {
        let FilterDraw {
            constraint,
            negation,
            negation_branches,
        } = filters;

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

            // A **second branch**, on a quarter of the statements: the same pattern
            // with everything that binds nothing re-matched against another value
            // occurring at that position. Keeping the variable leaves exactly where
            // they are is what makes the branches agree about what they bind, which
            // is the rule flatten enforces and the one a generator has to respect to
            // draw a query at all.
            let branches = match draw.branch.is_multiple_of(4) {
                true => vec![
                    alternative_branch(&fields, &facts[predicate], draw.branch / 4),
                    fields,
                ],
                false => vec![fields],
            };

            resolved.push(StmtSpec {
                predicate,
                row,
                branches,
            });
        }

        // A **constraint**, over a variable the query captures and whose type a
        // prefix is a pattern for. Drawn from `used_vars` rather than from the whole
        // pool because a constraint binds nothing: one on a variable no key mentions
        // is `reject/unbound-variable`, not a query this generator may draw.
        //
        // At most one. It is a statement like any other, so every extra one
        // multiplies the permutations the order properties re-run each case over.
        // Whether and which are separate digits of the draw: half the queries that
        // *can* carry one do, and those spread evenly over the prefixes. A
        // constraint is a filter, so the rate is a trade against the row count the
        // whole battery runs on — the census measures both ends of it.
        let constraints: Vec<(usize, &'static str)> = used_vars
            .iter()
            .filter(|v| var_tys[**v] == FieldTy::Str)
            .find_map(|v| {
                constraint.is_multiple_of(2).then(|| {
                    (
                        *v,
                        CONSTRAINT_PREFIXES[constraint as usize / 2 % CONSTRAINT_PREFIXES.len()],
                    )
                })
            })
            .into_iter()
            .collect();

        // A **negation**, over a variable the query captures and a scalar field of
        // that variable's type. Drawn from `used_vars` for the reason a constraint
        // is, and more strictly: a variable a negation alone names is not a variable
        // at all but a wildcard, which flatten refuses rather than guesses
        // (`nyi/negation`).
        //
        // One scalar field is enough to reach both shapes a test can take. At the
        // leading field the variable narrows the probe's seek — a composite seek
        // spliced from a register — and behind an open one it filters instead, which
        // is the same sargeability the rest of the generator exercises, now inside a
        // step that binds nothing.
        //
        // At most one, and on a **third** of the queries that can carry one: it is a
        // statement like any other, so each extra multiplies the permutations every
        // order property re-runs, and it is the sharpest filter this generator draws
        // — every row it rejects is a row the resume battery does not get to cut
        // through. A half was measurably too many
        // (`the_generator_is_not_degenerate` is what measures it).
        let negations: Vec<StmtSpec> = {
            // Where each variable is already matched, which is what a candidate
            // must **not** be. `test.P {f0 = V}; !test.P {f0 = V}` is a witness by
            // construction — the row that bound `V` is the row the probe finds — so
            // it answers nothing, every time. A battery of queries that return no
            // rows tests nothing about resume, and this is the one shape of that
            // which is not a coincidence but an identity.
            let occurs: Vec<(usize, usize, usize)> = resolved
                .iter()
                .flat_map(|spec| {
                    spec.branches.iter().flat_map(|branch| {
                        branch
                            .iter()
                            .enumerate()
                            .filter_map(|(field, pat)| match pat {
                                FieldPat::Leaf(Leaf::Var(var)) => {
                                    Some((spec.predicate, field, *var))
                                }
                                _ => None,
                            })
                    })
                })
                .collect();

            let candidates: Vec<(usize, usize, usize)> = schema
                .iter()
                .enumerate()
                .flat_map(|(predicate, spec)| {
                    spec.fields
                        .iter()
                        .enumerate()
                        .filter_map(move |(field, ty)| match ty {
                            GenTy::Scalar(scalar) => Some((predicate, field, *scalar)),
                            GenTy::Record(_) | GenTy::Ref => None,
                        })
                })
                .flat_map(|(predicate, field, scalar)| {
                    used_vars
                        .iter()
                        .filter(|var| var_tys[**var] == scalar)
                        .map(|var| (predicate, field, *var))
                        .collect::<Vec<_>>()
                })
                .filter(|candidate| !occurs.contains(candidate))
                .collect();

            match candidates.is_empty() || !negation.is_multiple_of(4) {
                true => vec![],
                false => {
                    let (predicate, field, var) =
                        candidates[negation as usize / 4 % candidates.len()];

                    let branch: Vec<FieldPat> = schema[predicate]
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(f, _)| {
                            FieldPat::Leaf(if f == field {
                                Leaf::Var(var)
                            } else {
                                // Unmentioned rather than a wildcard: the two
                                // mean the same thing to the plan, and this way
                                // a record or reference field needs no pattern
                                // of its own.
                                Leaf::Omitted
                            })
                        })
                        .collect();

                    // A **second branch** for the negation too, on half of them:
                    // `!(A | B)` is the shape De Morgan's law is about, and it is
                    // also the only way a *test* gets more than one source.
                    let branches = match negation_branches.is_multiple_of(2) {
                        true => vec![
                            alternative_branch(&branch, &facts[predicate], negation_branches / 2),
                            branch,
                        ],
                        false => vec![branch],
                    };

                    vec![StmtSpec {
                        predicate,
                        row: None,
                        branches,
                    }]
                }
            }
        };

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

            let item = match draw.kind % 4 {
                0 => HeadItem::Row(row),
                // Only where the predicate has a value to read.
                1 if spec.value.is_some() => HeadItem::Value(row),
                1 => HeadItem::Row(row),

                // A read **through** the reference every other predicate's key
                // ends with — the only head item that costs a `Source::Fetch`,
                // and so the only way this generator reaches one. Drawn rather
                // than left to chance for the reason the reference field itself
                // is not drawn: it needs a row bound over a referring predicate,
                // which two coincidences already have to line up for.
                2 if PredicateId(resolved[stmt].predicate as u32) != REFERENCED => HeadItem::Deref(
                    row,
                    // Where `resolve` puts it: last in the key.
                    spec.fields.len() - 1,
                    draw.field as usize % schema[REFERENCED.0 as usize].fields.len(),
                ),

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
            constraints,
            negations,
            head,
        }
    }

    /// **A second branch of a pattern**: the same leaves, with everything that binds
    /// nothing re-matched against another value occurring at that position.
    ///
    /// Variables and row reads are copied across untouched, and that is the whole
    /// discipline: a disjunction's branches must bind the same variables at the same
    /// fields, because a register holds one row and the plan holds one path into it.
    /// A generator that varied them would draw queries flatten is right to refuse,
    /// and would test the refusal rather than the disjunction.
    ///
    /// A position with no scalar values to draw from — a reference field, a record
    /// matched whole — keeps whatever the first branch had, so the branches differ
    /// only where they can.
    fn alternative_branch(base: &[FieldPat], facts: &[Fact], pick: u8) -> Vec<FieldPat> {
        let other = |leaf: &Leaf, occurring: Vec<FieldVal>| match leaf {
            Leaf::Var(_) | Leaf::Row(_) => leaf.clone(),
            _ if occurring.is_empty() => leaf.clone(),
            _ => Leaf::Const(GenVal::Scalar(
                occurring[pick as usize % occurring.len()].clone(),
            )),
        };

        base.iter()
            .enumerate()
            .map(|(f, pat)| match pat {
                FieldPat::Leaf(leaf) => {
                    FieldPat::Leaf(other(leaf, occurring_scalars(facts, f, None)))
                }
                FieldPat::Nested(subs) => FieldPat::Nested(
                    subs.iter()
                        .enumerate()
                        .map(|(g, leaf)| other(leaf, occurring_scalars(facts, f, Some(g))))
                        .collect(),
                ),
            })
            .collect()
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
            0u8..(4 * PICKS),
        )
            .prop_map(|(predicate, row, fields, branch)| StmtDraw {
                predicate,
                row,
                fields,
                branch,
            })
    }

    /// The two statements that **bind nothing**: a constraint and a negation.
    ///
    /// Drawn together because they are the same kind of thing to every property
    /// that orders statements, and because keeping them out of the tuple above
    /// keeps `resolve`'s parameters countable.
    #[derive(Debug, Clone, Copy)]
    struct FilterDraw {
        /// Whether to constrain, and which prefix — separate digits of one draw.
        constraint: u8,
        /// Whether to negate, and which (predicate, field, variable) triple.
        negation: u8,
        /// Whether the negation is over a **disjunction** — `!(A | B)`, the shape
        /// De Morgan's law is about — and which values its second branch matches.
        negation_branches: u8,
    }

    fn arb_head() -> impl Strategy<Value = HeadDraw> {
        (0u8..4, 0u8..PICKS, 0u8..PICKS).prop_map(|(kind, which, field)| HeadDraw {
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
            // Wider than `PICKS`: each digit carries *whether* to draw the
            // statement and *which* one, so a constraint lands on half the queries
            // that can carry one and a negation on a quarter.
            (
                0u8..(2 * CONSTRAINT_PREFIXES.len() as u8),
                0u8..(4 * PICKS),
                0u8..(2 * PICKS),
            )
                .prop_map(|(constraint, negation, negation_branches)| FilterDraw {
                    constraint,
                    negation,
                    negation_branches,
                }),
        )
            .prop_map(
                |(npredicates, predicates, facts, var_tys, stmts, heads, filters)| {
                    resolve(
                        npredicates,
                        predicates,
                        facts,
                        var_tys,
                        stmts,
                        heads,
                        filters,
                    )
                },
            )
    }
}

#[cfg(test)]
mod battery {
    use super::{
        flatten, flatten_in_order,
        proptest::{QueryAndStore, arb_query_and_store},
    };
    use crate::focus::{
        cst::CstNode,
        diag::Diagnostics,
        fixtures::{collect_rows, run_with_suspends},
        lower::lower,
        parse::parse,
        plan::{
            FactId, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart, Source, Step, Test,
            proptest::{arb_interruption_schedule, cut_points},
        },
        schema::{LocalInterner, PredicateTy, Schema},
        store::FjallDb,
        tuple::Value,
        ty,
    };
    use ::proptest::prelude::*;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    /// Compile `source` against `schema`, in the given loop order.
    ///
    /// Asserts nothing was reported: the generator is valid by construction, so a
    /// diagnostic here is a fault in flatten or in the generator, and either way
    /// the message should say which query.
    fn plan_of(schema: &Schema, source: &str, order: &[usize]) -> (Plan, LocalInterner) {
        plan_of_maybe_ordered(schema, source, Some(order))
    }

    /// The plan for `source` in the order **`reorder` chooses** — what the compiler
    /// does with a query nobody handed an order to.
    fn plan_of_chosen(schema: &Schema, source: &str) -> (Plan, LocalInterner) {
        plan_of_maybe_ordered(schema, source, None)
    }

    fn plan_of_maybe_ordered(
        schema: &Schema,
        source: &str,
        order: Option<&[usize]>,
    ) -> (Plan, LocalInterner) {
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

        let plan = match order {
            Some(order) => flatten_in_order(&ast, schema, &mut interner, &mut diagnostics, order),
            None => flatten(&ast, schema, &mut interner, &mut diagnostics),
        };

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

    /// Run **some other spelling** of a spec's query against the spec's store, in
    /// the order `reorder` picks.
    ///
    /// The order has to be chosen rather than given: the spellings the complement
    /// law compares have different statement *counts*, so there is no one
    /// permutation to hand all of them.
    fn run_source(spec: &QueryAndStore, schema: &Schema, source: &str) -> Vec<Value> {
        let (plan, interner) = plan_of_chosen(schema, source);

        collect_rows(spec.build_store(), plan, &interner).expect("run")
    }

    /// Whether `part` appears within `whole` in order, gaps allowed.
    fn is_subsequence(part: &[Value], whole: &[Value]) -> bool {
        let mut rest = whole.iter();
        part.iter().all(|row| rest.any(|other| other == row))
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

        /// **The complement law: a negation and its assertion partition the rows.**
        ///
        /// Every property above compares the engine against the *model*, and a model
        /// is a second reading of the same specification — which is exactly the
        /// comparison that cannot catch a wrong idea of what negation *means*, since
        /// both readings would share it. This one uses no model at all. It runs one
        /// query three ways — with `!S`, with `S`, and with neither — and asserts the
        /// relations between the three answers that hold whatever negation means, as
        /// long as it means the absence of what the same statement asserts:
        ///
        /// 1. **A filter only removes.** The negated query's rows are a subsequence
        ///    of the unfiltered query's — same rows, same order, some missing.
        /// 2. **Nothing falls between the two halves.** Every row of the unfiltered
        ///    query is answered by `!S` or produced by `S`.
        /// 3. **The half that cannot be produced survives.** A row `S` never yields
        ///    is a row with no witness, so `!S` must keep it.
        /// 4. **And the half that can is gone.** A row both halves answer is only
        ///    possible where the unfiltered query answers that projection *twice* —
        ///    one row with a witness and one without. Without (4) the law is
        ///    one-sided and a negation that never filtered anything would satisfy
        ///    every other clause; the mutation is the argument for its being here.
        /// 5. **Filtering twice is filtering once.** `!S; !S` answers as `!S` —
        ///    which is a claim about the frame's one bit of state as much as about
        ///    the semantics.
        ///
        /// Sets rather than multisets in (2)–(4), and deliberately: the positive
        /// form is a **level**, so it produces one row per witness where the negation
        /// produces one per surviving row, and two rows of the unfiltered query may
        /// project identically when the head does not name what distinguishes them.
        /// That is the exception (4) states rather than assumes; counting would be
        /// asserting something about multiplicity, which is a different law.
        #[test]
        fn a_negation_and_its_assertion_partition_the_rows(
            spec in arb_query_and_store().prop_filter(
                "the draw carries a negation",
                QueryAndStore::has_negation,
            ),
        ) {
            let schema = spec.schema();

            let filtered = run_source(&spec, &schema, &spec.source());
            let matched = run_source(&spec, &schema, &spec.source_asserting_the_negation());
            let unfiltered = run_source(&spec, &schema, &spec.source_without_the_negation());

            prop_assert!(
                is_subsequence(&filtered, &unfiltered),
                "a negation invented or reordered rows: {:?} is not a subsequence of {:?}",
                filtered,
                unfiltered
            );

            let split: BTreeSet<&Value> = filtered.iter().chain(matched.iter()).collect();
            let whole: BTreeSet<&Value> = unfiltered.iter().collect();
            prop_assert_eq!(
                &split,
                &whole,
                "rows answered by neither half, for {:?}",
                spec.source()
            );

            for row in &unfiltered {
                if !matched.contains(row) {
                    prop_assert!(
                        filtered.contains(row),
                        "{:?} has no witness and was dropped anyway, for {:?}",
                        row,
                        spec.source()
                    );
                }
            }

            for row in filtered.iter().filter(|row| matched.contains(row)) {
                prop_assert!(
                    unfiltered.iter().filter(|other| *other == row).count() > 1,
                    "{:?} has a witness and survived the negation anyway, for {:?}",
                    row,
                    spec.source()
                );
            }

            prop_assert_eq!(
                run_source(&spec, &schema, &spec.source_negating_twice()),
                filtered,
                "negating twice is not negating once, for {:?}",
                spec.source()
            );
        }

        /// **A disjunction is the concatenation of its branches**, and nothing else.
        ///
        /// The second law that needs no model. Run the query with `A | B`, then once
        /// with `A` alone and once with `B` alone: every complete answer uses exactly
        /// one branch for that statement, so the disjunction's rows are the two
        /// answers put together — **as a multiset**, which is the whole content of
        /// the claim. Nothing is merged, nothing is deduplicated, and a fact matching
        /// both branches is answered twice.
        ///
        /// A multiset rather than a sequence because the concatenation is per outer
        /// row: a disjunction nested inside another loop yields `A`-rows then
        /// `B`-rows *for each row above it*, where running the branches separately
        /// groups all of `A`'s first. Same rows, different interleaving, and the
        /// interleaving is the nested loop's business rather than the disjunction's.
        #[test]
        fn a_disjunction_is_the_concatenation_of_its_branches(
            spec in arb_query_and_store().prop_filter(
                "the draw carries a disjunction",
                QueryAndStore::has_disjunction,
            ),
        ) {
            let schema = spec.schema();

            let both = run_source(&spec, &schema, &spec.source());
            let first = run_source(&spec, &schema, &spec.source_with_branches(&[0]));
            let second = run_source(&spec, &schema, &spec.source_with_branches(&[1]));

            let mut apart = first;
            apart.extend(second);
            apart.sort();

            let mut together = both;
            together.sort();

            prop_assert_eq!(
                together,
                apart,
                "a disjunction lost, merged or invented rows, for {:?}",
                spec.source()
            );
        }

        /// **Branch order is not part of the answer.** `A | B` and `B | A` differ in
        /// the order rows arrive — the machine drains one source before the next —
        /// and in nothing else.
        #[test]
        fn swapping_two_branches_answers_the_same_multiset(
            spec in arb_query_and_store().prop_filter(
                "the draw carries a disjunction",
                QueryAndStore::has_disjunction,
            ),
        ) {
            let schema = spec.schema();

            let mut written = run_source(&spec, &schema, &spec.source_with_branches(&[0, 1]));
            let mut swapped = run_source(&spec, &schema, &spec.source_with_branches(&[1, 0]));

            written.sort();
            swapped.sort();

            prop_assert_eq!(written, swapped, "for {:?}", spec.source());
        }

        /// **De Morgan: `!(A | B)` is `!A; !B`.**
        ///
        /// The one classical law of this family that focus can *write down*, and it
        /// relates two different machines: a single test over two sources, and two
        /// tests over one each. Compared exactly rather than as a multiset — neither
        /// spelling binds anything, so both filter the same row sequence in place.
        ///
        /// Its partners do not survive the trip. **Double negation is not a law
        /// here**, and not even syntax: `!` prefixes a statement, `!!S` does not
        /// parse, and the reason it should not is that `S` binds and multiplies rows
        /// while `!!S` could only filter — `¬¬` is identity for a proposition and
        /// cannot be for a generator. What *is* the law is idempotence, `!S; !S ≡
        /// !S`, which the complement property asserts. **Distributivity** —
        /// `Q; (A | B) ≡ (Q; A) | (Q; B)` — has no right-hand side to test: `|`
        /// joins patterns, not statement lists, and a disjunction of subqueries is
        /// `nyi/disjunction`.
        #[test]
        fn de_morgan_holds_for_a_negated_disjunction(
            spec in arb_query_and_store().prop_filter(
                "the draw carries a negated disjunction",
                QueryAndStore::has_disjunctive_negation,
            ),
        ) {
            let schema = spec.schema();

            let negated_disjunction = run_source(&spec, &schema, &spec.source());
            let conjoined_negations = run_source(&spec, &schema, &spec.source_de_morgan());

            prop_assert_eq!(
                negated_disjunction,
                conjoined_negations,
                "!(A | B) and !A; !B disagreed, for {:?}",
                spec.source()
            );
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
        ///
        /// Over **every** permutation, not just the safe ones. That is the whole of
        /// what `reorder` buys: the orders this used to skip are the ones where a
        /// reference field reads a row the next statement binds, and they now compile
        /// — to the same plan, and so to the same rows — rather than being refused.
        /// No order is passed in: each rewritten source is compiled the way the
        /// compiler compiles it, so what is under test is the order `reorder` picked.
        #[test]
        fn rewriting_the_body_in_another_order_means_the_same_query(spec in arb_query_and_store()) {
            let mut want = spec.expected();
            want.sort();

            let schema = spec.schema();

            for order in spec.all_orders() {
                let source = spec.source_in_order(&order);
                let (plan, interner) = plan_of_chosen(&schema, &source);

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
        fetch_source: bool,
        negation_test: bool,
        negation_splice: bool,
        negation_residual: bool,
        negation_above_a_scan: bool,
        disjunctive_level: bool,
        disjunctive_negation: bool,
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
                (self.fetch_source, "a `Source::Fetch`"),
                (self.negation_test, "a `Step::Test`"),
                (
                    self.negation_splice,
                    "a negation whose probe seeks by a bound register",
                ),
                (
                    self.negation_residual,
                    "a negation whose probe filters by a bound register",
                ),
                (
                    self.negation_above_a_scan,
                    "a negation placed *above* a scan, where a step that binds \
                     nothing has to be re-entered from below",
                ),
                (self.disjunctive_level, "a level with more than one source"),
                (
                    self.disjunctive_negation,
                    "a negation over more than one source — `!(A | B)`",
                ),
            ] {
                if !present {
                    out.push(what);
                }
            }

            out
        }

        fn observe(&mut self, plan: &Plan) {
            // A test with a level after it — the placement Phase 6's I14 guard
            // showed to be the only one that observes a restore fault, since a step
            // below every scan is re-entered from beneath on the way back up
            // whether or not resume did anything for it.
            self.negation_above_a_scan |= plan
                .body
                .iter()
                .skip_while(|step| !matches!(step, Step::Test(_)))
                .any(|step| matches!(step, Step::Level(_)));

            for step in plan.body.iter() {
                // The census is about the shapes a *scan* can take; a derive step
                // has no seek and no residuals. When the generator learns to draw
                // one, it gets its own census entry rather than being folded in
                // here, since "reached a derive step" is a different claim.
                //
                // A **test** is folded in, because its sources are ordinary sources
                // and the interesting claim is about them: reaching the step says
                // nothing if every probe it drew was a bare scan. So the two ways a
                // bound register can reach a probe — narrowing its seek, or
                // filtering its rows — are counted separately.
                let level = match step {
                    Step::Level(level) => {
                        self.disjunctive_level |= level.sources.len() > 1;
                        level
                    }
                    Step::Derive(_) => continue,
                    Step::Test(Test::Absent(sources)) => {
                        self.negation_test = true;
                        self.disjunctive_negation |= sources.len() > 1;

                        for source in sources.iter() {
                            self.negation_splice |= matches!(
                                source.seek_key(),
                                Some(SeekKey::Composite(parts))
                                    if parts.iter().any(|part| matches!(
                                        part,
                                        SeekKeyPart::RegisterField { .. }
                                    ))
                            );

                            self.negation_residual |= source.residuals().iter().any(|residual| {
                                matches!(residual.op, ResidualOp::EqRegisterField { .. })
                            });
                        }

                        continue;
                    }
                };

                // Every alternative counts: a shape reached by the second source
                // of a disjunction is as reached as one in the first, and the
                // census is what says the battery saw it at all.
                for source in level.sources.iter() {
                    match source {
                        Source::Seek { access, .. } => match &access.seek_key {
                            SeekKey::Prefix(bytes) => self.constant_seek |= !bytes.is_empty(),
                            SeekKey::Composite(parts) => {
                                self.multi_part_seek |= parts.len() > 1;

                                for part in parts.iter() {
                                    match part {
                                        SeekKeyPart::Bytes(_) => self.constant_in_composite = true,
                                        SeekKeyPart::RegisterField { path, .. } => {
                                            self.nested_path |= !path.is_flat();
                                        }
                                        SeekKeyPart::RegisterFactId(_) => {
                                            self.fact_id_splice = true;
                                        }
                                    }
                                }
                            }
                        },
                        Source::Fetch { path, .. } => {
                            self.fetch_source = true;
                            self.nested_path |= !path.is_flat();
                        }
                    }

                    let residuals = source.residuals();
                    self.several_residuals |= residuals.len() > 1;

                    for Residual { path, op } in residuals.iter() {
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

        /// Safe orders to compile each draw in. Six is where the shapes stop
        /// arriving, and every one of them is a whole compile.
        const ORDERS: usize = 6;

        let mut runner = TestRunner::deterministic();
        let mut shapes = Shapes::default();

        for _ in 0..RUNS {
            let spec = arb_query_and_store()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let schema = spec.schema();

            // Several orders, not only the one the query was written in: where a
            // step that binds nothing *sits* is a property of the order, and the
            // identity order always writes a negation last.
            for order in spec.orders().into_iter().take(ORDERS) {
                let (plan, _) = plan_of(&schema, &spec.source(), &order);
                shapes.observe(&plan);
            }
        }

        let missing = shapes.missing();
        assert!(
            missing.is_empty(),
            "{RUNS} generated queries never produced: {}",
            missing.join(", ")
        );
    }

    /// **The rewriting property is not vacuous.**
    ///
    /// `rewriting_the_body_in_another_order_means_the_same_query` runs over every
    /// permutation of the body rather than only the safe ones, which says something
    /// only if the generator draws queries where some permutation *is* unsafe — a
    /// reference field reading a row that a later statement binds. Those are exactly
    /// the orders that used to be skipped and that `reorder` now has to fix, so if
    /// this count were zero the strengthening would be decoration.
    ///
    /// Counted rather than asserted per-case, for the same reason the census is: it
    /// is a claim about the *generator*, and one case proves nothing either way.
    #[test]
    fn the_generator_reaches_a_source_order_reorder_has_to_fix() {
        use ::proptest::{
            strategy::{Strategy, ValueTree},
            test_runner::TestRunner,
        };

        const RUNS: usize = 300;

        let mut runner = TestRunner::deterministic();
        let mut fixable = 0;
        let mut unsafe_orders = 0;

        for _ in 0..RUNS {
            let spec = arb_query_and_store()
                .new_tree(&mut runner)
                .unwrap()
                .current();

            let skipped = spec.all_orders().len() - spec.orders().len();
            unsafe_orders += skipped;
            fixable += usize::from(skipped > 0);
        }

        assert!(
            fixable > 0,
            "{RUNS} generated queries drew no body whose written order needs fixing, \
             so running the rewriting property over every permutation tests nothing \
             beyond the safe ones"
        );
        assert!(unsafe_orders > 0);
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
        ///
        /// **The order is drawn, not the identity**, and that is what puts a step
        /// that binds nothing *above* a scan. Phase 6 learned this the expensive
        /// way: the first [I14](../../docs/invariants.md#i14) guard passed with
        /// resume's recompute deleted, because the derive sat below the scan and
        /// `enumerate` re-entered it from beneath on the way back up. A negation is
        /// the same shape of step and would hide the same fault — written last in
        /// the source, it is the innermost step and every replay of it is a replay
        /// the machine would have done anyway.
        #[test]
        fn resume_of_a_compiled_plan_equals_the_query(
            spec in arb_query_and_store(),
            schedule in arb_interruption_schedule(),
            which in 0usize..8,
        ) {
            resume_matches_the_model(&spec, &schedule, which)?;
        }

        /// **The same claim, over draws that are guaranteed to filter.**
        ///
        /// The property above draws a negation in about one case in fifteen — the
        /// generator keeps the rate low because a probe rejects rows, and rows are
        /// what the resume battery cuts through. One in fifteen is enough to say the
        /// shape is *reached* (the census says so) and too few to call it covered,
        /// so this is the same experiment with the draw forced, which is what
        /// disjunction got for the same reason. The cost is a redraw, not a weaker
        /// generator: `prop_filter` retries until it has a negation, so this runs a
        /// full battery of them.
        #[test]
        fn resume_of_a_negated_plan_equals_the_query(
            spec in arb_query_and_store().prop_filter(
                "the draw carries a negation",
                QueryAndStore::has_negation,
            ),
            schedule in arb_interruption_schedule(),
            which in 0usize..8,
        ) {
            resume_matches_the_model(&spec, &schedule, which)?;
        }
    }

    /// Run `spec` in a drawn loop order, cutting where `schedule` says, and compare
    /// against the model **in that order**.
    ///
    /// The order is drawn rather than the identity because that is what places a
    /// step which binds nothing — a derive, a test — *above* a scan, and Phase 6
    /// established that no other placement observes a restore fault.
    fn resume_matches_the_model(
        spec: &QueryAndStore,
        schedule: &[bool],
        which: usize,
    ) -> Result<(), TestCaseError> {
        let schema = spec.schema();
        let orders = spec.orders();
        let order = &orders[which % orders.len()];

        let (plan, interner) = plan_of(&schema, &spec.source(), order);
        let model = spec.expected_in_order(order);

        let cuts = cut_points(schedule, model.len());
        let (rows, suspends) =
            run_with_suspends(|| (spec.build_store(), plan.clone()), &interner, &cuts).unwrap();

        prop_assert_eq!(
            suspends,
            cuts.len(),
            "expected one suspend per scheduled row"
        );
        prop_assert_eq!(
            rows,
            model,
            "schedule {:?} of order {:?} changed the run",
            cuts,
            order
        );

        Ok(())
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
        let mut with_constraint = 0;
        let mut with_disjunction = 0;
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
            if spec.constraints() > 0 {
                with_constraint += 1;
            }
            if spec.has_disjunction() {
                with_disjunction += 1;
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
        // **The average, and it has moved twice.** 433 rows over these 200 queries
        // before the negation draw landed, 392 after — all of the difference being
        // rows a probe legitimately rejected — and 614 once statements could be
        // disjunctions, which concatenate their branches and so add rows where a
        // negation removes them. The bound stays at 1.5 rows a query, below the
        // lowest of the three, with the room stated rather than shaved to fit: a
        // filter this generator draws on purpose costs rows on purpose, and the
        // assertion that carries the "not degenerate" claim is `with_rows` above —
        // more than half of all queries still answer at least one row.
        assert!(
            rows_total * 2 > RUNS * 3,
            "{rows_total} rows over {RUNS} queries is too thin"
        );
        assert!(
            through_a_reference * 25 > RUNS,
            "only {through_a_reference}/{RUNS} queries follow a reference"
        );
        // A tenth is enough to say the order properties actually re-run a
        // constrained query over every permutation. Oftener than that and the row
        // count above starts to suffer, which is the trade the draw is tuned for.
        assert!(
            with_constraint * 10 > RUNS,
            "only {with_constraint}/{RUNS} queries constrain a variable"
        );
        // A disjunction costs no rows — it adds them — so this one is a third
        // rather than a tenth. The branch laws are `prop_filter`ed onto exactly
        // these draws, and a filter is only cheap while what it keeps is common.
        assert!(
            with_disjunction * 3 > RUNS,
            "only {with_disjunction}/{RUNS} queries draw a disjunction"
        );
    }
}
