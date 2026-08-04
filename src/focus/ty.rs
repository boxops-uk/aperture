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

use codespan_reporting::diagnostic::Label;

use crate::focus::{
    lower::VALUE_FIELD,
    parser::Diagnostic,
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

/// Typecheck a lowered query.
pub fn check(ast: &Ast, schema: &Schema, interner: &LocalInterner) -> (Typed, Vec<Diagnostic>) {
    let mut checker = Checker {
        schema,
        interner,
        env: vec![],
        subst: vec![],
        tys: vec![None; ast.store().len()],
        undo: vec![],
        diagnostics: vec![],
    };

    checker.query(ast, ast.query());

    // Resolve every annotation before handing the table over. During checking an
    // annotation is whatever was known at the time — usually a type variable — and a
    // side table full of unresolved variables tells a later phase nothing.
    for id in 0..checker.tys.len() {
        if let Some(ty) = checker.tys[id].take() {
            checker.tys[id] = Some(checker.zonk(&ty));
        }
    }

    (Typed { tys: checker.tys }, checker.diagnostics)
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

struct Snapshot {
    undo: usize,
    subst: usize,
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
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Checker<'a> {
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
                "reject/wildcard-in-head",
                "a wildcard head projects nothing",
            );
        }
        self.infer(ast, head);
    }

    fn stmt(&mut self, ast: &Ast, stmt: &QueryStmt<NodeId>) {
        match stmt {
            QueryStmt::Implicit(id) => {
                self.infer(ast, *id);
            }
            QueryStmt::Negation(id) => {
                self.nyi(ast, *id, "nyi/negation", "negation");
                self.infer(ast, *id);
            }
            QueryStmt::Bind(lhs, rhs) => self.bind(ast, *lhs, *rhs),
        }
    }

    /// `lhs = rhs`.
    ///
    /// Only the easy half is implemented — the left side is a variable being bound
    /// for the first time, or a wildcard. Everything structural is full unification
    /// and deferred ([open decisions](../../../docs/open-decisions.md)); a literal
    /// on the left can never be a target at all.
    fn bind(&mut self, ast: &Ast, lhs: NodeId, rhs: NodeId) {
        match ast.store().kind(lhs) {
            ExprKind::Wildcard => {
                let ty = self.infer(ast, rhs);
                self.annotate(lhs, ty);
            }

            ExprKind::Var(symbol) if self.lookup(*symbol).is_none() => {
                let symbol = *symbol;
                // Introduced *before* the right side is inferred, so that both
                // occurrences in `X = {a = X}` are the same type variable. Inferring
                // first would quietly make two of them, and the occurs check could
                // then never fire.
                let var = self.fresh_var_id();
                self.env.push((symbol, var));
                self.annotate(lhs, Ty::Var(var));

                let ty = self.infer(ast, rhs);
                if let Err(err) = self.unify(&Ty::Var(var), &ty) {
                    self.report(ast, rhs, err);
                }
            }

            ExprKind::Lit(_) | ExprKind::Prefix(_) => {
                self.reject(
                    ast,
                    lhs,
                    "reject/bind-lhs",
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
                    "nyi/bind-unification",
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
            ExprKind::Never => {
                self.nyi(ast, id, "nyi/never", "the empty pattern `never`");
                Ty::Error
            }
            ExprKind::Select(..) => {
                self.nyi(ast, id, "nyi/union-select", "selecting a union alternative");
                Ty::Error
            }
            ExprKind::Disjunction(branches) => {
                self.nyi(ast, id, "nyi/disjunction", "disjunction");
                for branch in branches.iter() {
                    self.infer(ast, *branch);
                }
                Ty::Error
            }
            ExprKind::Subquery(_) => {
                self.nyi(ast, id, "nyi/subquery", "a subquery");
                Ty::Error
            }
        };

        self.annotate(id, ty.clone());
        ty
    }

    /// Check `id` against a known type, rather than inferring it.
    fn check(&mut self, ast: &Ast, id: NodeId, expected: &Ty) {
        let expected = self.zonk(expected);

        // A poisoned expectation means something upstream already failed; checking
        // against it would report the same mistake again.
        if has_error(&expected) {
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
                                    "reject/unknown-field",
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
        let base_ty = self.zonk(&base_ty);

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
                            "reject/value-shadowed",
                            "this predicate has a key field called `value`, so `.value` is ambiguous",
                        );
                    }
                    match self.predicate_value_ty(predicate) {
                        Some(ty) => ty,
                        None => self.reject_ty(
                            ast,
                            id,
                            "reject/no-value",
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
                        "reject/type-mismatch",
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
                            "reject/type-mismatch",
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
                            "reject/unknown-field",
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
            "reject/unresolved-access",
            "the type of this value is not known here, so its field cannot be resolved",
        )
    }

    // ---- unification ----------------------------------------------------------

    fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), TyError> {
        let a = self.zonk(a);
        let b = self.zonk(b);

        // Poison unifies with anything, so one mistake reports once — and it has to
        // *propagate* into an unbound variable, not just stop here. `X = nosuch.Pred _`
        // binds `X` to an error node; without this, `X` stays unknown and every later
        // `X.field` reports "the type of this value is not known", turning one bad
        // predicate name into a diagnostic per use.
        if has_error(&a) || has_error(&b) {
            for ty in [&a, &b] {
                // Both sides are zonked, so a variable here is genuinely unbound.
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

    /// Resolve a type as far as the substitution allows, compressing the path.
    fn zonk(&mut self, ty: &Ty) -> Ty {
        match ty {
            Ty::Error | Ty::Int | Ty::String | Ty::Fact(_) => ty.clone(),

            Ty::Var(var) => {
                let Some(bound) = self.var_ty(*var) else {
                    return ty.clone();
                };
                let bound = self.zonk(&bound);
                self.set_var(*var, bound.clone());
                bound
            }

            Ty::Record(fields) => Ty::Record(
                fields
                    .iter()
                    .map(|(name, field)| (*name, self.zonk(field)))
                    .collect(),
            ),
        }
    }

    fn occurs(&mut self, var: TyVarId, ty: &Ty) -> bool {
        match self.zonk(ty) {
            Ty::Error | Ty::Int | Ty::String | Ty::Fact(_) => false,
            Ty::Var(other) => other == var,
            Ty::Record(fields) => fields.iter().any(|(_, field)| self.occurs(var, field)),
        }
    }

    // ---- state ----------------------------------------------------------------

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
        self.subst[var.index()] = Some(ty);
    }

    fn annotate(&mut self, node: NodeId, ty: Ty) {
        let prev = self.tys[node.index()].replace(ty);
        self.undo.push(Undo::Annotation { node, prev });
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            undo: self.undo.len(),
            subst: self.subst.len(),
            env: self.env.len(),
        }
    }

    fn rollback(&mut self, at: Snapshot) {
        while self.undo.len() > at.undo {
            match self.undo.pop() {
                Some(Undo::Subst { var, prev }) => self.subst[var.index()] = prev,
                Some(Undo::Annotation { node, prev }) => self.tys[node.index()] = prev,
                None => break,
            }
        }
        self.subst.truncate(at.subst);
        self.env.truncate(at.env);
    }

    // ---- diagnostics ----------------------------------------------------------

    fn diagnostic(&mut self, ast: &Ast, id: NodeId, code: &str, message: String) {
        let span = ast.store().span(id);
        self.diagnostics.push(
            Diagnostic::error()
                .with_code(code)
                .with_message(message)
                .with_label(Label::primary((), span.start as usize..span.end as usize)),
        );
    }

    fn reject(&mut self, ast: &Ast, id: NodeId, code: &str, message: impl Into<String>) {
        self.diagnostic(ast, id, code, message.into());
    }

    fn reject_ty(&mut self, ast: &Ast, id: NodeId, code: &str, message: impl Into<String>) -> Ty {
        self.reject(ast, id, code, message);
        Ty::Error
    }

    /// A construct that parses and will be implemented, but not yet. The message
    /// says so in those words, because "not supported" reads as "never will be".
    fn nyi(&mut self, ast: &Ast, id: NodeId, code: &str, what: &str) {
        self.diagnostic(ast, id, code, format!("{what} is not implemented yet"));
    }

    fn report(&mut self, ast: &Ast, id: NodeId, err: TyError) {
        match err {
            TyError::Mismatch { expected, got } => {
                let (expected, got) = (self.render(&expected), self.render(&got));
                self.reject(
                    ast,
                    id,
                    "reject/type-mismatch",
                    format!("expected {expected}, found {got}"),
                );
            }
            TyError::UnknownField(name) => {
                let name = self.name_of(name).to_owned();
                self.reject(
                    ast,
                    id,
                    "reject/unknown-field",
                    format!("`{name}` is not a field here"),
                );
            }
            TyError::Infinite => self.reject(
                ast,
                id,
                "reject/infinite-type",
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
            Ty::Error => "an error".to_owned(),
            Ty::Var(_) => "an unknown type".to_owned(),
            Ty::Fact(predicate) => match self.schema.get(*predicate) {
                Some(p) => format!("`{}`", p.name()),
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

fn has_error(ty: &Ty) -> bool {
    match ty {
        Ty::Error => true,
        Ty::Int | Ty::String | Ty::Var(_) | Ty::Fact(_) => false,
        Ty::Record(fields) => fields.iter().any(|(_, field)| has_error(field)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{corpus, lower::lower, parse::parse, schema::PredicateId};

    struct Checked {
        typed: Typed,
        diagnostics: Vec<Diagnostic>,
        head: NodeId,
        interner: LocalInterner,
    }

    fn compile(source: &str) -> Checked {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let parsed = parse(source);
        let root = parsed.root().expect("a tree");
        let (ast, lowering) = lower(&root, &schema, &mut interner);
        assert!(
            lowering.is_empty(),
            "{source:?} should lower cleanly: {:?}",
            lowering.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let (typed, diagnostics) = check(&ast, &schema, &interner);
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
    #[test]
    fn deferred_constructs_report_themselves() {
        for (source, code) in [
            ("X where X = never", "nyi/never"),
            ("X.alt? where X = test.Foo _", "nyi/union-select"),
            (
                "X where test.Foo {id = X} | test.Bar {id = X}",
                "nyi/disjunction",
            ),
            (
                "X where test.Foo {id = X}; !test.Bar {id = X}",
                "nyi/negation",
            ),
            ("X where X = (Y where test.Foo {id = Y})", "nyi/subquery"),
            (
                "X where test.Foo {id = X}; test.Bar {id = Y}; X = Y",
                "nyi/bind-unification",
            ),
        ] {
            let checked = compile(source);
            assert_eq!(codes(&checked), [code], "for {source:?}");
        }
    }

    /// A deferred construct's message says "not implemented yet", not "unsupported":
    /// the distinction is the whole point of the permissive grammar.
    #[test]
    fn deferred_messages_say_yet() {
        let checked = compile("X where X = never");
        assert!(
            checked.diagnostics[0]
                .message
                .contains("not implemented yet"),
            "got {:?}",
            checked.diagnostics[0].message
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

    #[test]
    fn an_unknown_predicate_poisons_without_cascading() {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let parsed = parse("X.name where X = nosuch.Pred _");
        let root = parsed.root().expect("a tree");
        let (ast, lowering) = lower(&root, &schema, &mut interner);
        // Lowering reported the predicate; typecheck must not add to it.
        assert_eq!(
            lowering
                .iter()
                .filter_map(|d| d.code.as_deref())
                .collect::<Vec<_>>(),
            ["reject/unknown-predicate"]
        );
        let (_typed, diags) = check(&ast, &schema, &interner);
        assert!(
            diags.is_empty(),
            "typecheck should stay quiet: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
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
