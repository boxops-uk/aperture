//! Typecheck — resolve names against the schema and **annotate, don't mutate**.
//!
//! Types go into a side table indexed by [`NodeId`], never into the tree
//! ([chapter 7]): the tree stays the shared substrate and each phase owns its own
//! annotations. Because the store is append-only and its ids dense, that table is a
//! `Vec`, not a map.
//!
//! This is also where the permissive grammar is narrowed. Every construct the
//! grammar accepts but the engine cannot yet run draws **one specific diagnostic
//! naming it**, rather than a parse error or a confusing type error — that promise
//! is what [`corpus`](crate::focus::corpus) exists to check. Two kinds of narrowing
//! happen here:
//!
//! - **`nyi/…`** — deferred features: disjunction, negation, subqueries, union
//!   select, `never`, and the hard half of `pattern = pattern`
//!   ([open decisions](../../../docs/open-decisions.md)).
//! - **`reject/…`** — constructs that are meaningless and will not be implemented:
//!   a wildcard head, a literal as a bind target, `.value` where the key shadows it
//!   ([conventions](../../../docs/conventions.md)).
//!
//! Inference is Hindley–Milner-shaped — unification over type variables with an
//! occurs check — ported from the superseded `lens` prototype, with records as
//! sorted slices rather than a `HashMap`. Errors accumulate: a failed unification
//! rolls its substitution back so a mistake in one field cannot poison its
//! siblings, and checking continues.
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use crate::focus::{
    diag::{Code, Diagnostics},
    lower::VALUE_FIELD,
    schema::{LocalInterner, PredicateId, PredicateTy, Schema, Symbol},
    syntax::{Ast, ExprKind, FieldRef, Literal, NodeId, Query, QueryStmt, Ty, TyVarId},
};

/// The types a query's nodes were given.
pub struct Typed {
    tys: Vec<Option<Ty>>,
}

impl Typed {
    /// The type of a node, if it was reached. A node under a construct that was
    /// rejected outright is not annotated.
    pub fn ty(&self, id: NodeId) -> Option<&Ty> {
        self.tys.get(id.index()).and_then(Option::as_ref)
    }
}

/// Typecheck a lowered query, reporting into `diagnostics`.
///
/// The sink is lowering's and the parse's: one compilation, one list, read in
/// source order.
pub fn check(
    ast: &Ast,
    schema: &Schema,
    interner: &LocalInterner,
    diagnostics: &mut Diagnostics,
) -> Typed {
    let mut checker = Checker {
        schema,
        interner,
        env: vec![],
        subst: vec![],
        tys: vec![None; ast.store().len()],
        undo: vec![],
        diagnostics,
    };

    checker.query(ast, ast.query());

    // Resolve every annotation before handing the table over. During checking an
    // annotation is whatever was known at the time — usually a type variable — and a
    // side table full of unresolved variables tells a later phase nothing.
    let tys = checker
        .tys
        .iter()
        .map(|slot| slot.as_ref().map(|ty| checker.zonk(ty)))
        .collect();

    Typed { tys }
}

/// Why two types could not be made equal.
enum TyError {
    Mismatch { expected: Ty, got: Ty },
    UnknownField(Symbol),
    Infinite,
}

/// One reversible change, so a failed check leaves no residue.
enum Undo {
    Subst { var: TyVarId, prev: Option<Ty> },
    Annotation { node: NodeId, prev: Option<Ty> },
}

/// Where to roll a scope back to.
///
/// No mark for the substitution: its *values* are restored from the undo log, and
/// its slots are deliberately never reclaimed (see [`Checker::fresh_var_id`]).
struct Snapshot {
    undo: usize,
    env: usize,
}

struct Checker<'a> {
    schema: &'a Schema,
    interner: &'a LocalInterner,
    /// Variable → its type variable. **Append-only** — a variable is introduced at
    /// its first occurrence and never rebound — so rolling back a scope is a
    /// truncation rather than a clone of the whole environment.
    env: Vec<(Symbol, TyVarId)>,
    subst: Vec<Option<Ty>>,
    tys: Vec<Option<Ty>>,
    undo: Vec<Undo>,
    diagnostics: &'a mut Diagnostics,
}

impl Checker<'_> {
    // ---- the walk -------------------------------------------------------------

    fn query(&mut self, ast: &Ast, query: &Query<NodeId>) {
        for stmt in query.body() {
            self.stmt(ast, stmt);
        }

        // The head is inferred *last*: it reads variables the body binds, and
        // capture happens at first occurrence, so any order of the body works but
        // the head must come after all of it.
        let head = *query.head();
        if matches!(ast.store().kind(head), ExprKind::Wildcard) {
            self.reject(
                ast,
                head,
                Code::RejectWildcardInHead,
                "a wildcard head projects nothing",
            );
        }
        self.infer(ast, head);
    }

    fn stmt(&mut self, ast: &Ast, stmt: &QueryStmt<NodeId>) {
        match stmt {
            QueryStmt::Implicit(id) => {
                self.generator(ast, *id);
            }
            // **A negation is a generator, typed as one.** `!test.Bar {id = X}` has
            // to name a predicate and match its key like any other statement — the
            // fields have to exist and the types have to agree — and the only thing
            // that differs is what happens to the rows, which is not a type.
            //
            // The environment is deliberately **not** rolled back afterwards. A
            // negation binds nothing *outward*, which is Glean's `FlatNegation ->
            // mempty` and is the rule for **scope** — flatten's, since it is the
            // phase that decides what captures what. Types are the other question,
            // and there the answer is the opposite: a variable named inside a
            // negation and outside it is one variable and must have one type, so
            // truncating the env here would introduce a second type variable for it
            // and let `test.Foo {name = X}; !test.Bar {id = X}` typecheck.
            QueryStmt::Negation(id) => {
                self.generator(ast, *id);
            }
            QueryStmt::Bind(lhs, rhs) => self.bind(ast, *lhs, *rhs),
        }
    }

    /// A pattern in **generating** position — a statement, or the right side of a
    /// bind that generates.
    ///
    /// The difference from [`infer`](Self::infer) is disjunction, and it is a real
    /// one: as a *value*, `A | B` needs one type, so the branches unify. As a
    /// *statement* each branch generates on its own, so `test.Foo _ | test.Bar _`
    /// is two perfectly good generators whose types could not unify and do not
    /// need to — the statement produces rows, not a value.
    ///
    /// Both still have one type when something reads them, which is why a
    /// generating disjunction that *binds* its row still unifies: that is
    /// [`bind`](Self::bind)'s question, asked of the result rather than here.
    fn generator(&mut self, ast: &Ast, id: NodeId) -> Ty {
        let ExprKind::Disjunction(branches) = ast.store().kind(id) else {
            return self.infer(ast, id);
        };

        let branches = branches.clone();
        let mut last = Ty::Error;

        for branch in branches.iter() {
            last = self.generator(ast, *branch);
        }

        self.annotate(id, last.clone());
        last
    }

    /// `lhs = rhs`.
    ///
    /// What is checked here is the **left side, and only its shape**: a variable, a
    /// wildcard, or a record whose every leaf is one of those. What the right side
    /// has to be — and which of the four things a bind can mean it then is — is
    /// flatten's question, because it is the phase that knows where a value lives
    /// and it is the phase that sees the whole body rather than the statements
    /// above this one.
    ///
    /// What stays deferred here is a left side that is none of the three: a literal
    /// (which can never be a target), a generator, or a field read — each wants
    /// pattern-pushing rather than a bind
    /// ([open decisions](../../../docs/open-decisions.md)).
    fn bind(&mut self, ast: &Ast, lhs: NodeId, rhs: NodeId) {
        match ast.store().kind(lhs) {
            ExprKind::Wildcard => {
                let ty = self.infer(ast, rhs);
                self.annotate(lhs, ty);
            }

            // **A variable on the left, and that is the whole gate.** What the
            // statement *does* — introduce the variable, say what it is, compare it
            // against something else, or constrain where it already lives — is
            // flatten's question, because only flatten knows where a value comes
            // from. `test.Ref {of = P}; P = test.Foo …` says `P` is one variable
            // named twice and `reorder` picks the loop order; `test.Foo {id = N}; N
            // = 1` says `N` is `1`, which flatten substitutes at every use; `X = Y`
            // with both bound is a residual on whichever level binds later; `X =
            // "a"..` narrows the level that captures `X`.
            //
            // This arm used to ask whether the variable was **already mentioned**,
            // which decided all four in *source* order — the one order the query
            // might not have used, and the thing this phase gave up deciding when
            // `reorder` took the question over. `F = G` compiled or did not
            // depending on whether the statement that mentions `G` was written
            // above or below it, and the plans were identical either way.
            //
            // Claiming the same variable *twice* — two rows, or two constants — is
            // unification too, and flatten refuses it: only flatten knows whether a
            // variable is already a row or a constant rather than a capture.
            ExprKind::Var(symbol) => {
                let symbol = *symbol;
                // A fresh variable is introduced *before* the right side is
                // inferred, so that both occurrences in `X = {a = X}` are the same
                // type variable. Inferring first would quietly make two of them, and
                // the occurs check could then never fire. One already introduced
                // keeps the type variable it has — that is what makes the two
                // occurrences of `P` the same `P`.
                let var = match self.lookup(symbol) {
                    Some(var) => var,
                    None => {
                        let var = self.fresh_var_id();
                        self.env.push((symbol, var));
                        var
                    }
                };
                self.annotate(lhs, Ty::Var(var));

                let ty = self.infer(ast, rhs);
                if let Err(err) = self.unify(&Ty::Var(var), &ty) {
                    self.report(ast, rhs, err);
                }
            }

            // A record on the left: a **destructuring**, not unification. Every
            // variable in it is bound to the piece of the right side it lines up
            // with, which is the same substitution a scalar bind gets and needs
            // nothing compared at runtime.
            //
            // The gate is the *left* side only. What the right side has to be is a
            // question about where its pieces live, and flatten answers it: a
            // constant destructures into literals, anything naming a place
            // destructures into pieces of that place, and a value in no register
            // draws `nyi/value-bind` there. What cannot be relaxed is the left side:
            // a *literal* leaf (`{a = 1} = {a = 2}`) would bind nothing and so mean
            // `true` where it means the empty relation — see
            // [`Ast::is_destructurable`].
            ExprKind::Record(_) if ast.is_destructurable(lhs) => {
                // The left side first, so its variables are introduced before the
                // right side is inferred — the discipline the variable arm explains.
                let pattern = self.infer(ast, lhs);
                let value = self.infer(ast, rhs);
                if let Err(err) = self.unify(&pattern, &value) {
                    self.report(ast, rhs, err);
                }
            }

            ExprKind::Lit(_) | ExprKind::Prefix(_) => {
                self.reject(
                    ast,
                    lhs,
                    Code::RejectBindLhs,
                    "a literal cannot be bound to; put the variable on the left",
                );
                self.infer(ast, rhs);
            }

            // Lowering already reported whatever produced the error node.
            ExprKind::Error => {
                self.infer(ast, rhs);
            }

            _ => {
                self.nyi(
                    ast,
                    lhs,
                    Code::NyiBindUnification,
                    "matching two patterns against each other",
                );
                self.infer(ast, lhs);
                self.infer(ast, rhs);
            }
        }
    }

    fn infer(&mut self, ast: &Ast, id: NodeId) -> Ty {
        let ty = match ast.store().kind(id) {
            ExprKind::Lit(Literal::Int(_)) => Ty::Int,
            ExprKind::Lit(Literal::Str(_)) | ExprKind::Prefix(_) => Ty::String,
            ExprKind::Wildcard => self.fresh_var(),
            ExprKind::Error => Ty::Error,

            ExprKind::Var(symbol) => {
                let symbol = *symbol;
                match self.lookup(symbol) {
                    Some(var) => Ty::Var(var),
                    None => {
                        let var = self.fresh_var_id();
                        self.env.push((symbol, var));
                        Ty::Var(var)
                    }
                }
            }

            ExprKind::Record(fields) => Ty::Record(
                fields
                    .iter()
                    .map(|(name, value)| (*name, self.infer(ast, *value)))
                    .collect(),
            ),

            ExprKind::Access(field, base) => {
                let (field, base) = (*field, *base);
                self.access(ast, id, field, base)
            }

            ExprKind::Fact(predicate, key) => {
                let (predicate, key) = (*predicate, *key);
                if let Some(key_ty) = self.predicate_key_ty(predicate) {
                    self.check(ast, key, &key_ty);
                }
                Ty::Fact(predicate)
            }

            // Deferred constructs. Their children are still walked where that keeps
            // the environment honest, but a subquery deliberately is not: its
            // variables are scoped to it, and it has already been reported, so
            // descending would only add diagnostics about a construct we have
            // declined.
            // **`never` is polymorphic**, which is what "the identity of `|`" means
            // in a system with no subtyping: a fresh variable takes whatever type
            // the position demands, so `A | never` is `A`'s type and `never` alone
            // is whatever it is asked to be. Phase 2 declined a `Ty::Never`
            // constructor as speculative; this is the same answer without one, and
            // the empty relation needs nothing else — it matches no rows, so no
            // value of any type ever comes out of it.
            ExprKind::Never => self.fresh_var(),
            ExprKind::Select(..) => {
                self.nyi(
                    ast,
                    id,
                    Code::NyiUnionSelect,
                    "selecting a union alternative",
                );
                Ty::Error
            }
            // **Every branch has the one type the disjunction has.** Unified rather
            // than merely compared, so a branch may *inform* the type — `never | 1`
            // is `int`, and the fresh variable `never` contributed is what makes
            // that work.
            //
            // Variables are deliberately **not** scoped per branch here. A variable
            // some branch does not bind is a *safety* question, not a typing one:
            // flatten takes the intersection of what the branches capture, and range
            // restriction then reports the read that has nothing behind it — at the
            // read, which is where a person can act on it.
            ExprKind::Disjunction(branches) => {
                let branches = branches.clone();
                let result = self.fresh_var();

                for branch in branches.iter() {
                    let branch_ty = self.infer(ast, *branch);

                    if let Err(err) = self.unify(&result, &branch_ty) {
                        self.report(ast, *branch, err);
                    }
                }

                result
            }
            // **A subquery is a query, and its variables are its own.** The body is
            // checked, then the environment is truncated back — so a name used
            // inside says nothing about the same name outside, which is what makes
            // `(Y where …)` writable next to an outer `Y`.
            //
            // Only the environment is rolled back, not the substitution or the
            // annotations: the types the subquery worked out are real and the side
            // table keeps them. That is why this is not [`rollback`](Self::rollback),
            // which is for a scope that *failed*.
            ExprKind::Subquery(query) => {
                let scope = self.env.len();

                for stmt in query.body() {
                    self.stmt(ast, stmt);
                }

                let ty = self.infer(ast, *query.head());
                self.env.truncate(scope);
                ty
            }
        };

        self.annotate(id, ty.clone());
        ty
    }

    /// Check `id` against a known type, rather than inferring it.
    fn check(&mut self, ast: &Ast, id: NodeId, expected: &Ty) {
        let expected = self.repr(expected);

        // A poisoned expectation means something upstream already failed; checking
        // against it would report the same mistake again. At the head, as in
        // `unify`: poison inside a record is the business of the field it is in.
        if matches!(expected, Ty::Error) {
            return;
        }

        match ast.store().kind(id) {
            ExprKind::Wildcard => self.annotate(id, expected),

            ExprKind::Record(fields) => match &expected {
                // Only the fields the pattern *mentions* are checked: an omitted
                // field is a wildcard, so `test.Edge {from = 1}` is "any edge from
                // 1". That is the reading the storage model wants — a mentioned
                // prefix of the key becomes a seek, the rest a scan — and the
                // asymmetry with `unify`, which does require two record *types* to
                // have the same fields, is deliberate: a pattern is a partial
                // description of a value, a type is not.
                Ty::Record(field_tys) => {
                    for (name, value) in fields.iter() {
                        match field_tys.iter().find(|(n, _)| n == name) {
                            Some((_, field_ty)) => {
                                let field_ty = field_ty.clone();
                                // Each field is checked in its own scope, so a bad
                                // field leaves no partial substitution behind to
                                // confuse its siblings.
                                let before = self.diagnostics.len();
                                let snapshot = self.snapshot();
                                self.check(ast, *value, &field_ty);
                                if self.diagnostics.len() > before {
                                    self.rollback(snapshot);
                                }
                            }
                            None => {
                                let name = self.name_of(*name).to_owned();
                                self.reject(
                                    ast,
                                    *value,
                                    Code::RejectUnknownField,
                                    format!("`{name}` is not a field here"),
                                );
                            }
                        }
                    }
                    self.annotate(id, expected.clone());
                }
                _ => self.infer_then_unify(ast, id, &expected),
            },

            _ => self.infer_then_unify(ast, id, &expected),
        }
    }

    fn infer_then_unify(&mut self, ast: &Ast, id: NodeId, expected: &Ty) {
        let snapshot = self.snapshot();
        let inferred = self.infer(ast, id);
        if let Err(err) = self.unify(&inferred, expected) {
            self.rollback(snapshot);
            self.report(ast, id, err);
        }
    }

    /// One `.field` or `.value` step.
    fn access(&mut self, ast: &Ast, id: NodeId, field: FieldRef, base: NodeId) -> Ty {
        let base_ty = self.infer(ast, base);
        let base_ty = self.repr(&base_ty);

        match field {
            FieldRef::Value => match base_ty {
                Ty::Error => Ty::Error,
                Ty::Fact(predicate) => {
                    // A key field also called `value` makes `.value` ambiguous, and
                    // the grammar cannot tell them apart — so the schema decides.
                    if self.key_shadows_value(predicate) {
                        return self.reject_ty(
                            ast,
                            id,
                            Code::RejectValueShadowed,
                            "this predicate has a key field called `value`, so `.value` is ambiguous",
                        );
                    }
                    match self.predicate_value_ty(predicate) {
                        Some(ty) => ty,
                        None => self.reject_ty(
                            ast,
                            id,
                            Code::RejectNoValue,
                            "this predicate has no value",
                        ),
                    }
                }
                Ty::Var(_) => self.unresolved(ast, id),
                other => {
                    let got = self.render(&other);
                    self.reject_ty(
                        ast,
                        id,
                        Code::RejectTypeMismatch,
                        format!("only a fact has a value; this is {got}"),
                    )
                }
            },

            FieldRef::Key(name) => {
                let record = match base_ty {
                    Ty::Error => return Ty::Error,
                    Ty::Fact(predicate) => match self.predicate_key_ty(predicate) {
                        Some(ty) => ty,
                        None => return Ty::Error,
                    },
                    record @ Ty::Record(_) => record,
                    Ty::Var(_) => return self.unresolved(ast, id),
                    other => {
                        let got = self.render(&other);
                        return self.reject_ty(
                            ast,
                            id,
                            Code::RejectTypeMismatch,
                            format!("{got} has no fields"),
                        );
                    }
                };

                match field_of(&record, name) {
                    Some(ty) => ty,
                    None => {
                        let name = self.name_of(name).to_owned();
                        self.reject_ty(
                            ast,
                            id,
                            Code::RejectUnknownField,
                            format!("`{name}` is not a field here"),
                        )
                    }
                }
            }
        }
    }

    /// A field read whose base type is still open.
    ///
    /// Resolving it would need row polymorphism — "some record with a `name` field"
    /// — which the type model does not have. In practice the variable is unbound
    /// because nothing binds it, which Phase 4's range-restriction check rejects
    /// anyway; this is the earlier, clearer diagnostic.
    fn unresolved(&mut self, ast: &Ast, id: NodeId) -> Ty {
        self.reject_ty(
            ast,
            id,
            Code::RejectUnresolvedAccess,
            "the type of this value is not known here, so its field cannot be resolved",
        )
    }

    // ---- unification ----------------------------------------------------------

    fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), TyError> {
        let a = self.repr(a);
        let b = self.repr(b);

        // Poison unifies with anything, so one mistake reports once — and it has to
        // *propagate* into an unbound variable, not just stop here. `X = nosuch.Pred _`
        // binds `X` to an error node; without this, `X` stays unknown and every later
        // `X.field` reports "the type of this value is not known", turning one bad
        // predicate name into a diagnostic per use.
        //
        // Checked at the **head**, not through the whole type. Poison buried inside
        // a record is reached by the structural recursion below, which silences the
        // field it is actually in and leaves the rest of the comparison
        // reportable. That is the rule: report a mismatch only when it is a fact
        // about what the user wrote — an arity or a field name is, and neither can
        // be influenced by a subtree that already failed. Returning early on poison
        // *anywhere* swallowed those too.
        if matches!(a, Ty::Error) || matches!(b, Ty::Error) {
            for ty in [&a, &b] {
                // Both sides are resolved, so a variable here is genuinely unbound.
                if let Ty::Var(var) = ty {
                    self.set_var(*var, Ty::Error);
                }
            }
            return Ok(());
        }

        match (a, b) {
            (Ty::Var(x), Ty::Var(y)) if x == y => Ok(()),
            (Ty::Var(var), ty) | (ty, Ty::Var(var)) => self.bind_var(var, ty),

            (Ty::Int, Ty::Int) | (Ty::String, Ty::String) => Ok(()),
            (Ty::Fact(x), Ty::Fact(y)) if x == y => Ok(()),

            (Ty::Record(xs), Ty::Record(ys)) => {
                if xs.len() != ys.len() {
                    return Err(TyError::Mismatch {
                        expected: Ty::Record(ys),
                        got: Ty::Record(xs),
                    });
                }
                // Looked up by name rather than zipped: both sides are sorted, but
                // the schema's order is Phase 8's to guarantee, not this pass's to
                // assume.
                for (name, x) in xs.iter() {
                    let Some((_, y)) = ys.iter().find(|(n, _)| n == name) else {
                        return Err(TyError::UnknownField(*name));
                    };
                    self.unify(x, y)?;
                }
                Ok(())
            }

            (got, expected) => Err(TyError::Mismatch { expected, got }),
        }
    }

    fn bind_var(&mut self, var: TyVarId, ty: Ty) -> Result<(), TyError> {
        if self.occurs(var, &ty) {
            return Err(TyError::Infinite);
        }
        self.set_var(var, ty);
        Ok(())
    }

    /// Resolve `ty` far enough to see its outermost shape: follow a chain of bound
    /// variables to the first non-variable, or to an unbound one.
    ///
    /// The shallow half of what [`zonk`](Self::zonk) does, and all any caller
    /// during checking needs — each one matches on the head and then recurses, and
    /// every recursion resolves its own head. Deep-resolving at every level
    /// instead re-walked the whole remaining subtree once per level, which is
    /// quadratic in the type's size. A `Ty` clone is a refcount bump
    /// ([`Ty::Record`] is an `Arc`), so this is not.
    ///
    /// Takes `&self`: nothing here writes back. The previous form compressed the
    /// path as it walked, which meant an `Undo` entry per traversal — undo-log
    /// growth during what reads as a pure query.
    fn repr(&self, ty: &Ty) -> Ty {
        let mut current = ty.clone();

        // A chain cannot be longer than the number of variables, and `bind_var`'s
        // occurs check makes a cycle impossible. The bound is a backstop: it turns
        // a broken occurs check into a debug assertion rather than a hang.
        for _ in 0..=self.subst.len() {
            let Ty::Var(var) = current else {
                return current;
            };
            let Some(bound) = self.var_ty(var) else {
                return current;
            };
            current = bound;
        }

        debug_assert!(false, "substitution chain is cyclic at {current:?}");
        current
    }

    /// Resolve a type **fully**, rebuilding every record it walks.
    ///
    /// The expensive one, and so called in exactly one place: the final pass over
    /// the annotation table, which is the only point a completely resolved type is
    /// wanted. Everything during checking uses [`repr`](Self::repr).
    fn zonk(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Error | Ty::Int | Ty::String | Ty::Fact(_) => ty.clone(),

            Ty::Var(var) => match self.var_ty(*var) {
                Some(bound) => self.zonk(&bound),
                None => ty.clone(),
            },

            Ty::Record(fields) => Ty::Record(
                fields
                    .iter()
                    .map(|(name, field)| (*name, self.zonk(field)))
                    .collect(),
            ),
        }
    }

    fn occurs(&self, var: TyVarId, ty: &Ty) -> bool {
        match self.repr(ty) {
            Ty::Error | Ty::Int | Ty::String | Ty::Fact(_) => false,
            Ty::Var(other) => other == var,
            Ty::Record(fields) => fields.iter().any(|(_, field)| self.occurs(var, field)),
        }
    }

    // ---- state ----------------------------------------------------------------

    /// A type variable that has never been used before, and whose id will never be
    /// handed out again.
    ///
    /// A [`TyVarId`] is an index into `subst`, so ids are only fresh while the
    /// substitution grows monotonically — which is why [`Checker::rollback`]
    /// restores its values but does not truncate it. Reclaiming the slots would let
    /// this hand back an id belonging to a rolled-back scope, and a `Ty::Var` that
    /// outlived that scope would then quietly mean a *different* variable. A
    /// handful of unused `Option<Ty>` slots for the length of one query is the
    /// cheaper mistake by a wide margin.
    fn fresh_var_id(&mut self) -> TyVarId {
        self.subst.push(None);
        TyVarId::new(self.subst.len() - 1)
    }

    fn fresh_var(&mut self) -> Ty {
        Ty::Var(self.fresh_var_id())
    }

    fn lookup(&self, symbol: Symbol) -> Option<TyVarId> {
        self.env
            .iter()
            .rev()
            .find(|(name, _)| *name == symbol)
            .map(|(_, var)| *var)
    }

    fn var_ty(&self, var: TyVarId) -> Option<Ty> {
        self.subst.get(var.index()).cloned().flatten()
    }

    fn set_var(&mut self, var: TyVarId, ty: Ty) {
        let prev = self.var_ty(var);
        self.undo.push(Undo::Subst { var, prev });
        // Indexed directly, and sound by construction: every `TyVarId` comes from
        // `fresh_var_id`, which pushes the slot, and nothing removes one.
        self.subst[var.index()] = Some(ty);
    }

    fn annotate(&mut self, node: NodeId, ty: Ty) {
        let prev = self.tys[node.index()].replace(ty);
        self.undo.push(Undo::Annotation { node, prev });
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            undo: self.undo.len(),
            env: self.env.len(),
        }
    }

    /// Undo everything a failed scope did, so a mistake in one field cannot poison
    /// its siblings.
    ///
    /// The undo log restores every substitution and annotation the scope wrote. The
    /// environment *is* truncated, because a variable introduced in the scope must
    /// stop counting as bound; the substitution is not, because its ids must stay
    /// unique for the checker's life ([`Checker::fresh_var_id`]).
    fn rollback(&mut self, at: Snapshot) {
        while self.undo.len() > at.undo {
            match self.undo.pop() {
                Some(Undo::Subst { var, prev }) => self.subst[var.index()] = prev,
                Some(Undo::Annotation { node, prev }) => self.tys[node.index()] = prev,
                None => break,
            }
        }
        self.env.truncate(at.env);
    }

    // ---- diagnostics ----------------------------------------------------------

    fn diagnostic(&mut self, ast: &Ast, id: NodeId, code: Code, message: String) {
        self.diagnostics.error(code, message, ast.store().span(id));
    }

    fn reject(&mut self, ast: &Ast, id: NodeId, code: Code, message: impl Into<String>) {
        self.diagnostic(ast, id, code, message.into());
    }

    fn reject_ty(&mut self, ast: &Ast, id: NodeId, code: Code, message: impl Into<String>) -> Ty {
        self.reject(ast, id, code, message);
        Ty::Error
    }

    /// A construct that parses and will be implemented, but not yet. The message
    /// says so in those words, because "not supported" reads as "never will be".
    fn nyi(&mut self, ast: &Ast, id: NodeId, code: Code, what: &str) {
        self.diagnostic(ast, id, code, format!("{what} is not implemented yet"));
    }

    fn report(&mut self, ast: &Ast, id: NodeId, err: TyError) {
        match err {
            TyError::Mismatch { expected, got } => {
                let (expected, got) = (self.render(&expected), self.render(&got));
                self.reject(
                    ast,
                    id,
                    Code::RejectTypeMismatch,
                    format!("expected {expected}, found {got}"),
                );
            }
            TyError::UnknownField(name) => {
                let name = self.name_of(name).to_owned();
                self.reject(
                    ast,
                    id,
                    Code::RejectUnknownField,
                    format!("`{name}` is not a field here"),
                );
            }
            TyError::Infinite => self.reject(
                ast,
                id,
                Code::RejectInfiniteType,
                "this pattern would have to contain itself",
            ),
        }
    }

    // ---- schema and names -----------------------------------------------------

    fn predicate_key_ty(&self, predicate: PredicateId) -> Option<Ty> {
        let predicate = self.schema.get(predicate)?;
        Some(schema_ty(predicate.key().ty))
    }

    fn predicate_value_ty(&self, predicate: PredicateId) -> Option<Ty> {
        let predicate = self.schema.get(predicate)?;
        predicate.value().map(|value| schema_ty(value.ty))
    }

    fn key_shadows_value(&self, predicate: PredicateId) -> bool {
        self.schema
            .get(predicate)
            .and_then(|p| p.key().find_field(VALUE_FIELD))
            .is_some()
    }

    fn name_of(&self, symbol: Symbol) -> &str {
        self.interner.try_resolve(symbol).unwrap_or("?")
    }

    fn render(&self, ty: &Ty) -> String {
        match ty {
            Ty::Int => "an integer".to_owned(),
            Ty::String => "a string".to_owned(),
            // Only reachable nested inside another type: a bare poison never
            // reaches a message, because `unify` returns `Ok` on it. Named as
            // already-reported rather than as a type — "found an error" reads
            // like a compiler fault.
            Ty::Error => "(already reported)".to_owned(),
            Ty::Var(_) => "an unknown type".to_owned(),
            Ty::Fact(predicate) => match self.schema.get(*predicate).and_then(|p| p.name()) {
                Some(name) => format!("`{name}`"),
                None => "a fact".to_owned(),
            },
            Ty::Record(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(name, ty)| format!("{} = {}", self.name_of(*name), self.render(ty)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// The query-level form of a declared type.
fn schema_ty(ty: &PredicateTy) -> Ty {
    match ty {
        PredicateTy::Int => Ty::Int,
        PredicateTy::Str => Ty::String,
        PredicateTy::Fact(predicate) => Ty::Fact(*predicate),
        PredicateTy::Record(fields) => Ty::Record(
            fields
                .iter()
                .map(|(name, field)| (Symbol::Schema(*name), schema_ty(field)))
                .collect(),
        ),
    }
}

fn field_of(ty: &Ty, name: Symbol) -> Option<Ty> {
    let Ty::Record(fields) = ty else {
        return None;
    };
    fields
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, ty)| ty.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{
        corpus,
        cst::CstNode,
        lower::lower,
        parse::parse,
        schema::{Predicate, PredicateId},
    };
    use lasso::Rodeo;
    use std::sync::Arc;

    struct Checked {
        typed: Typed,
        diagnostics: Diagnostics,
        head: NodeId,
        interner: LocalInterner,
    }

    fn compile(source: &str) -> Checked {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();
        let cst = parse(source, &mut diagnostics).expect("a tree");
        let root = CstNode::new(&cst);
        let ast = lower(&root, &schema, &mut interner, &mut diagnostics);
        assert!(
            diagnostics.is_empty(),
            "{source:?} should lower cleanly: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let typed = check(&ast, &schema, &interner, &mut diagnostics);
        let head = *ast.query().head();
        Checked {
            typed,
            diagnostics,
            head,
            interner,
        }
    }

    fn codes(checked: &Checked) -> Vec<&str> {
        checked
            .diagnostics
            .iter()
            .filter_map(|d| d.code.as_deref())
            .collect()
    }

    /// Every diagnostic code `source` draws, from lowering **and** typecheck, in
    /// order.
    ///
    /// Unlike [`compile`], lowering is allowed to report — which is how poison
    /// gets into a query in the first place, so it is the only way to test what
    /// typecheck does with it.
    fn all_codes(source: &str) -> Vec<String> {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();
        let cst = parse(source, &mut diagnostics).expect("a tree");
        let root = CstNode::new(&cst);
        let ast = lower(&root, &schema, &mut interner, &mut diagnostics);
        let _typed = check(&ast, &schema, &interner, &mut diagnostics);

        diagnostics.codes().map(str::to_owned).collect()
    }

    /// The head's type, rendered.
    fn head_ty(source: &str) -> String {
        let checked = compile(source);
        assert!(
            codes(&checked).is_empty(),
            "{source:?}: {:?}",
            codes(&checked)
        );
        let ty = checked
            .typed
            .ty(checked.head)
            .expect("the head is annotated");
        render(ty, &checked.interner)
    }

    /// A rendering with structure, unlike `Checker::render`'s prose.
    fn render(ty: &Ty, interner: &LocalInterner) -> String {
        match ty {
            Ty::Int => "int".to_owned(),
            Ty::String => "str".to_owned(),
            Ty::Error => "!error".to_owned(),
            Ty::Var(_) => "?".to_owned(),
            Ty::Fact(p) => format!("fact({})", p.0),
            Ty::Record(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(n, t)| format!(
                        "{}={}",
                        interner.try_resolve(*n).unwrap_or("?"),
                        render(t, interner)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Annotations are resolved before the table is handed over — the point of the
    /// final zonk. Without it every one of these would read `?`.
    #[test]
    fn the_side_table_holds_resolved_types() {
        assert_eq!(head_ty("X where X = test.Foo _"), "fact(0)");
        assert_eq!(head_ty("X where test.Foo {id = X}"), "int");
        assert_eq!(head_ty("X where test.Foo {name = X}"), "str");
        assert_eq!(head_ty("X.name where X = test.Foo _"), "str");
        assert_eq!(head_ty("X.value where X = test.Foo _"), "str");
        assert_eq!(head_ty("X where test.Nested {outer = {inner = X}}"), "int");
        assert_eq!(
            head_ty("{a = X, b = Y} where test.Foo {name = X, id = Y}"),
            "{a=str, b=int}"
        );
    }

    /// A variable's type flows between statements, in both directions.
    #[test]
    fn inference_crosses_statements() {
        assert_eq!(
            head_ty("X where test.Edge {from = X, to = Y}; test.Node {id = Y}"),
            "int"
        );
        // The head reads a type the *later* statement determines.
        assert_eq!(
            head_ty("Y where test.Node {id = Y}; test.Edge {to = Y}"),
            "int"
        );
    }

    /// Errors accumulate: one pass reports every mistake it finds, because the
    /// permissive grammar means a query can be wrong in several ways at once.
    #[test]
    fn checking_keeps_going_after_an_error() {
        let checked = compile(
            "X where test.Foo {nosuch = X}; test.Bar {alsonosuch = Y}; test.Foo {name = 42}",
        );
        assert_eq!(
            codes(&checked),
            [
                "reject/unknown-field",
                "reject/unknown-field",
                "reject/type-mismatch"
            ]
        );
    }

    /// A bad field must not poison its siblings — each field is checked in its own
    /// scope and rolled back on failure.
    #[test]
    fn a_bad_field_leaves_its_siblings_alone() {
        let checked = compile("{a = N} where test.Foo {name = 42, id = N}");
        assert_eq!(codes(&checked), ["reject/type-mismatch"]);
        // `id` still resolved, despite `name` failing beside it.
        let ty = checked.typed.ty(checked.head).expect("annotated");
        assert_eq!(render(ty, &checked.interner), "{a=int}");
    }

    /// Both occurrences of `X` are the same type variable, so a self-referential
    /// pattern is caught rather than silently making two variables.
    #[test]
    fn a_self_referential_bind_is_an_infinite_type() {
        let checked = compile("X where X = {a = X}");
        assert_eq!(codes(&checked), ["reject/infinite-type"]);
    }

    #[test]
    fn a_field_read_on_an_unknown_type_is_rejected() {
        let checked = compile("X where test.Foo X.name");
        assert_eq!(codes(&checked), ["reject/unresolved-access"]);
    }

    /// `.value` needs the schema twice over: for the value's type, and to notice the
    /// key already has a field by that name.
    #[test]
    fn value_access_consults_the_schema() {
        assert_eq!(head_ty("X.value where X = test.Foo _"), "str");

        let checked = compile("X.value where X = test.Bar _");
        assert_eq!(codes(&checked), ["reject/no-value"]);

        let checked = compile("X.value where X = test.Shadow _");
        assert_eq!(codes(&checked), ["reject/value-shadowed"]);
    }

    /// Every deferred construct reports itself by name, exactly once.
    ///
    /// **One is left** — union select, which waits on a `PredicateTy::Union` to
    /// exist at all ([Phase 8](../../PLAN.md)). Negation was the other, and is now
    /// a plan; what still carries its code is narrower and is flatten's, so the
    /// corpus is where those are pinned.
    #[test]
    fn deferred_constructs_report_themselves() {
        let checked = compile("X.alt? where X = test.Foo _");
        assert_eq!(codes(&checked), ["nyi/union-select"]);
    }

    /// **A negation is typechecked, and only its types are typecheck's business.**
    ///
    /// The statement is a generator like any other — the predicate has to exist, the
    /// key has to fit, a wrong type is a type error — and what makes it a negation
    /// happens entirely later. The last two cases are the ones the environment rule
    /// is about: a variable named inside a negation and outside it is **one**
    /// variable with one type, so a mismatch between the two occurrences is caught
    /// here rather than compiling into a plan that compares an int against a string.
    #[test]
    fn a_negation_is_typechecked_like_the_generator_it_is() {
        for source in [
            "X where test.Foo {id = X}; !test.Bar {id = X}",
            "X where test.Foo {id = X}; !test.Edge {from = X, to = _}",
            "X where test.Foo {id = X}; !(test.Bar {id = X} | test.Node {id = X})",
            "X where test.Foo {id = X}; !never",
        ] {
            assert_eq!(all_codes(source), [] as [&str; 0], "for {source:?}");
        }

        assert_eq!(
            all_codes("X where test.Name X; !test.Bar {id = X}"),
            ["reject/type-mismatch"],
            "a negation reading a variable at the wrong type is still a type error"
        );

        assert_eq!(
            all_codes("X where test.Foo {id = X}; !test.Bar {nope = X}"),
            ["reject/unknown-field"]
        );
    }

    /// **Binding a row a field has already mentioned is an ordering question**, and
    /// typecheck's business here is types only.
    ///
    /// `test.Ref {of = P}; P = test.Foo …` is the same query as those two statements
    /// the other way round, and compiles to the same plan —
    /// [`reorder`](crate::focus::reorder::reorder) picks the order that binds before
    /// it reads. So `P` is one variable with one type whichever statement mentions
    /// it first, and *which* statement binds it is not typecheck's call to make.
    ///
    /// This is the half of `pattern = pattern` that never needed unification. The
    /// rule it replaces refused every bind whose left side was already bound, which
    /// conflated it with the cases that do — those are still deferred, next test.
    #[test]
    fn a_row_may_be_bound_after_a_field_mentions_it() {
        assert_eq!(
            head_ty("P where test.Ref {of = P}; P = test.Foo {id = 1}"),
            "fact(0)"
        );

        // The bind is symmetric, as Angle's reference says `A = B` is: the same
        // query written either way round has the same type.
        assert_eq!(
            head_ty("P where P = test.Foo {id = 1}; test.Ref {of = P}"),
            "fact(0)"
        );

        // Two statements claiming one row is unification, not ordering — but it is
        // *flatten* that says so, from the whole statement list, because deciding it
        // here would be deciding it in source order again. Typecheck sees only that
        // the types agree, which they do.
        assert!(
            all_codes("X where X = test.Foo {id = 1}; X = test.Foo {id = 2}").is_empty(),
            "the row claim is flatten's to refuse"
        );

        // A second claim of a *different* predicate is a plain type error, caught
        // here because the bind unifies against what the variable already is.
        assert_eq!(
            all_codes("X where X = test.Foo {id = 1}; X = test.Bar {id = 2}"),
            ["reject/type-mismatch"]
        );
    }

    /// **A constant may be bound after a field has captured the variable.**
    ///
    /// `test.Foo {id = N}; N = 1` says what `N` *is*, and a constant is substituted at
    /// every use rather than compared at runtime — so as with the row case above, the
    /// only thing that made this look like unification was the order it was written in.
    /// Flatten does the folding and pins the plan equality; typecheck's part is that
    /// the two types agree.
    #[test]
    fn a_constant_may_be_bound_after_a_field_captures_it() {
        assert_eq!(head_ty("X where test.Foo {id = N, name = X}; N = 1"), "str");
        assert_eq!(
            head_ty("X where test.Nested {outer = X}; X = {inner = 1}"),
            "{inner=int}"
        );

        // A constant of the wrong type is a plain type error, not a deferral.
        assert_eq!(
            all_codes("X where test.Foo {id = N, name = X}; N = \"one\""),
            ["reject/type-mismatch"]
        );

        // Two constants for one variable is unification, and flatten's to refuse —
        // only it knows the variable is already a constant rather than a capture.
        assert!(all_codes("Y where Y = 1; Y = 2").is_empty());
    }

    /// **A record on the left, matched against a constant, is destructuring** — every
    /// variable in it is bound to a piece of a value already known, which is the same
    /// substitution as a scalar constant and needs nothing compared at runtime.
    #[test]
    fn a_constant_may_be_destructured_into_a_record() {
        assert_eq!(head_ty("X where {a = X} = {a = 1}"), "int");

        // To any depth, and beside a wildcard, which binds nothing.
        assert_eq!(head_ty("X where {a = {b = X}} = {a = {b = 1}}"), "int");
        assert_eq!(head_ty("X where {a = X, b = _} = {a = 1, b = 2}"), "int");

        // The shapes still have to agree, and a mismatch is a type error rather than
        // a deferral.
        assert_eq!(
            all_codes("X where {a = X} = {a = 1}; test.Foo {name = X}"),
            ["reject/type-mismatch"]
        );
    }

    /// **A literal on the left of a destructuring stays deferred**, and this is the
    /// trap the feature above walks past.
    ///
    /// `{a = 1} = {a = 2}` *typechecks* — both sides are `int` — and binds nothing. A
    /// flatten that accepted it would emit no constraint at all, so the statement would
    /// silently mean **true** where it means the empty relation. Deciding it needs the
    /// two constants' bytes compared, which is unification; refusing it is what keeps
    /// "binds nothing" from ever reaching flatten.
    #[test]
    fn a_literal_inside_a_destructured_record_is_deferred() {
        for source in [
            "X where test.Foo {id = X}; {a = 1} = {a = 2}",
            // Refused even where the two agree: deciding *that* is the same byte
            // comparison, and a query saying it is degenerate either way.
            "X where test.Foo {id = X}; {a = 1} = {a = 1}",
            // And where only one leaf is a literal, so the rest would have folded.
            "X where {a = X, b = 2} = {a = 1, b = 2}",
        ] {
            assert_eq!(
                all_codes(source),
                ["nyi/bind-unification"],
                "for {source:?}"
            );
        }
    }

    /// **What typecheck still defers is a property of the left side alone.**
    ///
    /// Each of these has something on the left that is not a target: a generator, a
    /// field read, a literal leaf. Pushing a pattern *into* one is a different
    /// operation from binding — there is nothing to give a name to — and it is the
    /// only part of `pattern = pattern` that has no answer here.
    #[test]
    fn a_left_side_that_is_not_a_target_is_still_deferred() {
        for source in [
            // Generator against generator: "these two facts are the same fact", which
            // is also what flatten refuses when two statements claim one row.
            "X where test.Foo {id = X} = test.Bar {id = X}",
            // A field read on the left names a place, but naming it is not binding
            // it: the pattern would have to be pushed into the row it comes from.
            "X where Y = test.Foo _; Y.name = X",
        ] {
            assert_eq!(
                all_codes(source),
                ["nyi/bind-unification"],
                "for {source:?}"
            );
        }
    }

    /// **Source order no longer decides what a bind means.**
    ///
    /// Whether a variable has been *mentioned above* used to gate this arm, so
    /// `X = Y` typechecked when the statement binding `Y` was written first and drew
    /// `nyi/bind-unification` when it was written second — for the same query, with
    /// the same plan. That is the decision `reorder` took over, and typecheck kept a
    /// copy of it.
    ///
    /// Which of the four things a bind can be is flatten's answer now, so all this
    /// checks is that typecheck has stopped answering: the shapes pass, and the
    /// corpus pins what each compiles to.
    #[test]
    fn a_bind_is_typed_the_same_whichever_order_it_is_written_in() {
        for source in [
            "X where test.Foo {id = X}; test.Bar {id = Y}; X = Y",
            "X where test.Foo {id = X}; X = Y; test.Bar {id = Y}",
            "X where X = Y; test.Foo {id = X}; test.Bar {id = Y}",
            // A field read on the right, and a prefix: neither is a value that can
            // be substituted, and both are perfectly well typed.
            "X where test.Foo {name = X}; Y = test.Foo _; X = Y.name",
            "X where test.Foo {name = X}; X = \"a\"..",
        ] {
            assert_eq!(all_codes(source), [] as [&str; 0], "for {source:?}");
        }

        // The types still have to agree, and a mismatch is a mismatch rather than a
        // deferral — the thing the old gate hid.
        assert_eq!(
            all_codes("X where test.Foo {id = X}; X = \"a\".."),
            ["reject/type-mismatch"]
        );
    }

    /// **Where the record gate now is: the left side only.**
    ///
    /// A record on the left destructures against whatever the right side is, and
    /// *what the right side is* is flatten's question — it knows where a value
    /// lives and typecheck does not. So a record against a non-constant record is
    /// no longer refused here; it is refused there, and by the code that says why:
    /// `{a = Y}` names no place, so it would have to be built.
    #[test]
    fn what_a_record_destructures_against_is_flattens_question() {
        // Typecheck passes it through; the corpus pins what flatten then says
        // (`nyi/value-bind` — `{a = Y}` names no place).
        assert_eq!(
            all_codes("X where test.Foo {id = X}; {a = X} = {a = Y}"),
            [] as [&str; 0]
        );

        // ...and a **literal** leaf on the left is still refused here, because that
        // is a property of the left side alone.
        assert_eq!(
            all_codes("X where test.Foo {id = X}; {a = 1} = {a = Y}"),
            ["nyi/bind-unification"],
        );
    }

    /// A deferred construct's message says "not implemented yet", not "unsupported":
    /// the distinction is the whole point of the permissive grammar.
    #[test]
    fn deferred_messages_say_yet() {
        let checked = compile("X.alt? where X = test.Foo _");
        let first = checked.diagnostics.iter().next().expect("a diagnostic");
        assert!(
            first.message.contains("not implemented yet"),
            "got {:?}",
            first.message
        );
    }

    #[test]
    fn a_wildcard_head_is_rejected() {
        let checked = compile("_ where test.Foo _");
        assert_eq!(codes(&checked), ["reject/wildcard-in-head"]);
    }

    #[test]
    fn a_literal_cannot_be_a_bind_target() {
        let checked = compile("X where 42 = test.Foo _");
        assert_eq!(codes(&checked), ["reject/bind-lhs"]);
    }

    /// Scalar-keyed predicates take a scalar pattern, and a mismatch is caught.
    #[test]
    fn scalar_keys_are_checked() {
        assert_eq!(head_ty("X where X = test.Name \"abc\""), "fact(5)");
        assert_eq!(head_ty("X where X = test.Count 42"), "fact(6)");

        let checked = compile("X where X = test.Count \"abc\"");
        assert_eq!(codes(&checked), ["reject/type-mismatch"]);

        let checked = compile("X where X = test.Name 42");
        assert_eq!(codes(&checked), ["reject/type-mismatch"]);
    }

    /// A record *pattern* may name a subset of the key's fields — an omitted field
    /// is a wildcard — while two record *types* must agree on their fields exactly.
    /// Both halves are pinned because the asymmetry is deliberate, and because it
    /// was incidental in the first draft rather than intended.
    #[test]
    fn a_record_pattern_may_name_a_subset_but_a_type_may_not() {
        assert_eq!(head_ty("X where test.Edge {from = X, to = _}"), "int");
        // "any edge from 1" — `to` is unmentioned, so unconstrained.
        assert_eq!(head_ty("X where X = test.Edge {from = 1}"), "fact(2)");

        // Unifying two record *types*, though, is exact: `X` is `{inner}` from the
        // first statement and `{extra, inner}` from the second.
        let checked = compile("X where test.Nested {outer = X}; test.Wide {outer = X}");
        assert_eq!(codes(&checked), ["reject/type-mismatch"]);

        // ...and the same shape twice is fine.
        let checked = compile("X where test.Nested {outer = X}; test.Nested {outer = X}");
        assert!(codes(&checked).is_empty(), "{:?}", codes(&checked));
    }

    /// A rolled-back scope's type variables are never handed out again.
    ///
    /// `rollback` restores the substitution's values from the undo log but does not
    /// reclaim its slots, and that asymmetry is the point. Truncating would make a
    /// later `fresh_var_id` return an id belonging to the abandoned scope, so a
    /// `Ty::Var` that outlived it would silently come to mean a different variable
    /// — a wrong type inferred quietly, where the leak it saves is a few unused
    /// `Option<Ty>` slots for one query.
    #[test]
    fn a_rolled_back_scope_does_not_reuse_its_type_variables() {
        let schema = corpus::schema();
        let interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();
        let mut checker = Checker {
            schema: &schema,
            interner: &interner,
            env: vec![],
            subst: vec![],
            tys: vec![],
            undo: vec![],
            diagnostics: &mut diagnostics,
        };

        let outer = checker.fresh_var_id();
        let at = checker.snapshot();

        let inner = checker.fresh_var_id();
        checker.set_var(inner, Ty::Int);
        checker.rollback(at);

        // The abandoned scope's binding is gone...
        assert_eq!(checker.var_ty(inner), None, "the binding must be undone");

        // ...and its id is not reissued, so no stale `Ty::Var` can alias it.
        let next = checker.fresh_var_id();
        assert_ne!(next, inner, "a rolled-back id was handed out again");
        assert_ne!(next, outer);

        // A variable from *before* the snapshot is still writable — the
        // substitution was not truncated out from under it.
        checker.set_var(outer, Ty::String);
        assert_eq!(checker.var_ty(outer), Some(Ty::String));
    }

    // ---- poison, and how far it reaches --------------------------------------
    //
    // `Ty::Error` is a poison that unifies with anything, so one mistake reports
    // once. The question these pin is *how much* it silences. The rule is:
    //
    //   report a mismatch only when it is a fact about what the user literally
    //   wrote — never an inference from something already reported.
    //
    // So poison silences the comparison it is *part of*, and nothing else.
    // Arities, field names and concrete type constructors are read off the source
    // pattern or the schema, neither of which a poisoned subtree can influence,
    // so they stay reportable.

    /// Poison silences its own comparison while an independent structural error
    /// beside it still reports.
    ///
    /// `nosuch.Pred` poisons the `a` field of `X`'s record type. Unifying that
    /// against `test.Nested`'s `{inner: int}` is two separate facts: the poisoned
    /// field, already reported and to be left alone, and the field *name* `a`,
    /// which the user genuinely wrote where `inner` was needed.
    ///
    /// This used to report only the first — any poison anywhere in either type
    /// returned early and swallowed the whole unification, independent errors
    /// included.
    #[test]
    fn poison_silences_its_own_comparison_but_not_an_independent_one() {
        assert_eq!(
            all_codes("X where X = {a = nosuch.Pred _}; test.Nested {outer = X}"),
            ["reject/unknown-predicate", "reject/unknown-field"]
        );
    }

    /// The other half, and the reason poison exists: a *read* of the poisoned
    /// thing adds nothing, however many times it happens.
    #[test]
    fn reading_a_poisoned_value_never_cascades() {
        for source in [
            "X.a where X = {a = nosuch.Pred _}",
            "{p = X.a, q = X.a} where X = {a = nosuch.Pred _}",
            "X where X = nosuch.Pred _; test.Foo {id = X}",
        ] {
            assert_eq!(
                all_codes(source),
                ["reject/unknown-predicate"],
                "for {source:?}"
            );
        }
    }

    /// A variable bound to a variable resolves through the chain — including when
    /// the far end is a compound type that has to be carried back.
    ///
    /// Resolving used to compress the chain as a side effect of walking it. It no
    /// longer does, so the walk itself has to be right. Two links is as deep as
    /// the implemented subset reaches: a third would need `Y = Z` with `Y`
    /// already bound, which is `nyi/bind-unification`.
    #[test]
    fn a_variable_bound_to_a_variable_resolves_through_the_chain() {
        assert_eq!(head_ty("X where X = Y; test.Foo {id = Y}"), "int");
        assert_eq!(
            head_ty("X where X = Y; test.Nested {outer = Y}"),
            "{inner=int}"
        );
    }

    #[test]
    fn an_unknown_predicate_poisons_without_cascading() {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();
        let cst = parse("X.name where X = nosuch.Pred _", &mut diagnostics).expect("a tree");
        let root = CstNode::new(&cst);
        let ast = lower(&root, &schema, &mut interner, &mut diagnostics);
        // Lowering reported the predicate; typecheck must not add to it.
        assert_eq!(
            diagnostics.codes().collect::<Vec<_>>(),
            ["reject/unknown-predicate"]
        );

        // The sink is shared, so "typecheck said nothing" is "nothing arrived
        // while it ran" rather than "it returned an empty list".
        let mark = diagnostics.len();
        let _typed = check(&ast, &schema, &interner, &mut diagnostics);
        assert!(
            diagnostics.since(mark).is_empty(),
            "typecheck should stay quiet: {:?}",
            diagnostics
                .since(mark)
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    /// A schema with one predicate, `test.Deep`, whose key is `outer` nested
    /// `depth` records deep with an `int` at the bottom.
    fn deep_schema(depth: usize) -> Schema {
        let mut rodeo = Rodeo::new();
        let outer = rodeo.get_or_intern("outer");
        let name = rodeo.get_or_intern("test.Deep");

        let mut key = PredicateTy::Int;
        for _ in 0..depth {
            key = PredicateTy::Record(Arc::from([(outer, key)]));
        }

        Schema::new(
            rodeo.into_reader(),
            Arc::from([Predicate {
                name,
                key,
                value: None,
            }]),
        )
    }

    /// Allocations made by `check` alone (parse and lower excluded) for a query
    /// that unifies two copies of a `depth`-deep record type.
    fn checking_allocations(depth: usize) -> u64 {
        let schema = deep_schema(depth);
        let source = "X where test.Deep {outer = X}; test.Deep {outer = X}";

        let mut interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();
        let cst = parse(source, &mut diagnostics).expect("a tree");
        let root = CstNode::new(&cst);
        let ast = lower(&root, &schema, &mut interner, &mut diagnostics);
        assert!(
            diagnostics.is_empty(),
            "the fixture query must lower cleanly"
        );

        let mut reported = usize::MAX;
        let info = allocation_counter::measure(|| {
            let _typed = check(&ast, &schema, &interner, &mut diagnostics);
            reported = diagnostics.len();
        });

        assert_eq!(
            reported, 0,
            "the fixture query must typecheck cleanly, or this measures diagnostics"
        );
        info.count_total
    }

    /// Checking a deeply nested record type costs **linear** work, not quadratic.
    ///
    /// `unify` resolves both sides at each level and then recurses, and `occurs`
    /// walks the type the same way — so while resolving was *deep*, each level
    /// rebuilt the whole remaining subtree and a type of size n cost O(n²)
    /// allocations. Resolution is now shallow ([`Checker::repr`]) and a `Ty` clone
    /// is a refcount bump, so doubling the depth must roughly double the work.
    ///
    /// Measured rather than asserted, as every non-functional claim here is.
    /// Allocations per `check` when this was written:
    ///
    /// | depth | deep resolve | shallow |
    /// |------:|-------------:|--------:|
    /// |    32 |        1,872 |     227 |
    /// |    64 |        6,816 |     451 |
    /// |   128 |       25,920 |     899 |
    ///
    /// The doubling ratio separates cleanly: 3.6–3.8 against 1.99.
    #[test]
    fn checking_a_deep_type_is_linear_not_quadratic() {
        // The counting allocator ships inside `allocation-counter` and is only
        // linked because it is a dev-dependency. If that wiring ever breaks,
        // `measure` reports zeroes and the ratio below is vacuous — so prove the
        // probe sees a known allocation first.
        let control = allocation_counter::measure(|| {
            std::hint::black_box(Vec::<u8>::with_capacity(4096));
        });
        assert!(
            control.count_total > 0,
            "counting allocator is not installed; this guard would pass vacuously: {control:?}"
        );

        let n = checking_allocations(64);
        let twice = checking_allocations(128);
        assert!(n > 0, "checking a 64-deep type allocated nothing at all");

        // Linear growth is a ratio of 2, or a little under with a constant term;
        // quadratic is 4. The threshold sits between, with room for both.
        assert!(
            twice * 100 <= n * 250,
            "checking is superlinear in the type's depth: {n} allocations at depth 64 \
             and {twice} at 128, a ratio of {:.2} where linear is 2 and quadratic 4",
            twice as f64 / n as f64,
        );
    }

    #[test]
    fn the_head_type_of_a_fact_is_the_predicate() {
        let checked = compile("X where X = test.Shadow _");
        assert!(codes(&checked).is_empty());
        assert_eq!(
            checked.typed.ty(checked.head),
            Some(&Ty::Fact(PredicateId(7)))
        );
    }
}
