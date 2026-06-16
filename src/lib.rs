pub mod lens;

use codespan_reporting::diagnostic::{Diagnostic, Label};
use std::{
    collections::BTreeMap,
    fmt::{Display, Formatter},
    ops::Range,
    sync::Arc,
};
use string_interner::{DefaultStringInterner as StringInterner, DefaultSymbol as Symbol};

pub type Span = Range<usize>;

#[derive(Debug)]
pub struct Query {
    pub span: Span,
    pub head: Pattern,
    pub body: Box<[Statement]>,
}

#[derive(Debug, Clone)]
pub struct Pattern {
    pub span: Span,
    pub kind: PatternKind<Pattern>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PredicateId(pub u32);

#[derive(Debug, Clone)]
pub enum PatternKind<T> {
    Wildcard,
    Int(i64),
    String(Symbol),
    StringPrefix(Symbol),
    Var(Symbol),
    Subquery(Arc<Query>),
    Record {
        fields: BTreeMap<Symbol, T>,
    },
    Fact {
        predicate_id: PredicateId,
        key_pattern: Box<T>,
    },
}

#[derive(Debug)]
pub enum Statement {
    Bind { left: Pattern, right: Pattern },
    ImplicitBind(Pattern),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TyVarId(u32);

fn u32_to_base26(mut n: u32) -> String {
    let mut buf = [0u8, 7];
    let mut i = buf.len();

    loop {
        let rem = n % 26;
        i -= 1;
        buf[i] = b'a' + rem as u8;
        n /= 26;

        if n == 0 {
            break;
        }

        n -= 1;
    }

    unsafe { String::from_utf8_unchecked(buf[i..].to_vec()) }
}

impl Display for TyVarId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", u32_to_base26(self.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ty {
    Error,
    Never,
    Int,
    String,
    Var(TyVarId),
    Record { field_tys: BTreeMap<Symbol, Ty> },
    Fact { predicate_id: PredicateId },
}

impl Ty {
    pub fn has_error(&self) -> bool {
        match self {
            Ty::Error => true,
            Ty::Record { field_tys } => field_tys.values().any(Ty::has_error),
            Ty::Never | Ty::Int | Ty::String | Ty::Var(_) | Ty::Fact { .. } => false,
        }
    }
}

#[derive(Debug)]
pub struct Schema {
    pub predicate_tys: BTreeMap<PredicateId, PredicateTy>,
}

#[derive(Debug)]
pub struct PredicateTy {
    pub name: Symbol,
    pub namespace: Box<[Symbol]>,
    pub key_ty: Ty,
    pub value_ty: Ty,
}

impl PredicateTy {
    pub fn full_name(&self, string_interner: &StringInterner) -> Option<String> {
        let mut parts = self
            .namespace
            .iter()
            .map(|s| string_interner.resolve(*s).map(|s| s.to_string()))
            .collect::<Option<Vec<_>>>()?;
        parts.push(string_interner.resolve(self.name).map(|s| s.to_string())?);
        Some(parts.join("."))
    }
}

#[derive(Debug, Clone)]
pub enum TyError {
    InfiniteTy { tid: TyVarId },
    Mismatch { expected: Ty, got: Ty },
    UnknownPredicate { predicate_id: PredicateId },
    UnknownField { field: Symbol },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiagnosticMark(usize);

pub struct TyChecker<'s> {
    string_interner: &'s StringInterner,
    bindings: BTreeMap<TyVarId, Ty>,
    locals: BTreeMap<Symbol, Ty>,
    schema: Schema,
    diagnostics: Vec<Diagnostic<()>>,
    next_tid: TyVarId,
}

#[derive(Debug, Clone)]
struct TypeSnapshot {
    bindings: BTreeMap<TyVarId, Ty>,
    locals: BTreeMap<Symbol, Ty>,
}

impl<'s> TyChecker<'s> {
    pub fn new(string_interner: &'s StringInterner, schema: Schema) -> Self {
        Self {
            string_interner,
            bindings: BTreeMap::new(),
            locals: BTreeMap::new(),
            schema,
            diagnostics: Vec::new(),
            next_tid: TyVarId(1),
        }
    }

    fn snapshot_type_state(&self) -> TypeSnapshot {
        TypeSnapshot {
            bindings: self.bindings.clone(),
            locals: self.locals.clone(),
        }
    }

    fn rollback_type_state(&mut self, snapshot: TypeSnapshot) {
        self.bindings = snapshot.bindings;
        self.locals = snapshot.locals;
    }

    fn fresh_tid(&mut self) -> TyVarId {
        let tid = self.next_tid;
        self.next_tid.0 += 1;
        tid
    }

    fn mark_diagnostic(&self) -> DiagnosticMark {
        DiagnosticMark(self.diagnostics.len())
    }

    fn diagnostic_emitted_since(&self, mark: DiagnosticMark) -> bool {
        self.diagnostics.len() > mark.0
    }

    fn format_ty(&self, ty: &Ty) -> String {
        match ty {
            Ty::Error => "Error".to_string(),
            Ty::Never => "Never".to_string(),
            Ty::Int => "Int".to_string(),
            Ty::String => "String".to_string(),
            Ty::Var(tid) => format!("{}", tid),
            Ty::Record { field_tys } => {
                let fields = field_tys
                    .iter()
                    .map(|(name, ty)| {
                        format!(
                            "{}: {}",
                            self.string_interner.resolve(*name).unwrap(),
                            self.format_ty(ty)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {} }}", fields)
            }
            Ty::Fact { predicate_id } => self.schema.predicate_tys.get(predicate_id).map_or_else(
                || format!("UnknownFact({})", predicate_id.0),
                |pred_ty| pred_ty.full_name(self.string_interner).unwrap(),
            ),
        }
    }

    fn emit_diagnostic(&mut self, span: Span, err: TyError) -> DiagnosticMark {
        let diagnostic = match err {
            TyError::InfiniteTy { tid } => Diagnostic::error()
                .with_message("infinite type")
                .with_labels(vec![Label::primary((), span).with_message(format!(
                    "type variable {} occurs inside its own type",
                    tid
                ))]),
            TyError::Mismatch { expected, got } => Diagnostic::error()
                .with_message("type mismatch")
                .with_labels(vec![Label::primary((), span).with_message(format!(
                    "expected type {}, got {}",
                    self.format_ty(&expected),
                    self.format_ty(&got)
                ))]),
            TyError::UnknownPredicate { predicate_id } => Diagnostic::error()
                .with_message("unknown predicate")
                .with_labels(vec![Label::primary((), span).with_message(format!(
                    "predicate with id {} is not defined in the schema",
                    predicate_id.0
                ))]),
            TyError::UnknownField { field } => Diagnostic::error()
                .with_message("unknown field")
                .with_labels(vec![Label::primary((), span).with_message(format!(
                    "field {} is not defined in the record type",
                    self.string_interner.resolve(field).unwrap()
                ))]),
        };

        self.diagnostics.push(diagnostic);
        self.mark_diagnostic()
    }

    fn zonk(&mut self, ty: &Ty) -> Ty {
        match ty {
            Ty::Error | Ty::Never | Ty::Int | Ty::String | Ty::Fact { .. } => ty.clone(),

            Ty::Var(tid) => {
                let Some(bound_to) = self.bindings.get(tid).cloned() else {
                    return ty.clone();
                };

                let zonked = self.zonk(&bound_to);
                self.bindings.insert(*tid, zonked.clone());
                zonked
            }

            Ty::Record { field_tys } => Ty::Record {
                field_tys: field_tys.iter().map(|(k, v)| (*k, self.zonk(v))).collect(),
            },
        }
    }

    fn occurs(&mut self, tid: TyVarId, ty: &Ty) -> bool {
        match self.zonk(ty) {
            Ty::Error | Ty::Never | Ty::Int | Ty::String | Ty::Fact { .. } => false,

            Ty::Var(tid2) => tid2 == tid,

            Ty::Record { field_tys } => field_tys.values().any(|v| self.occurs(tid, v)),
        }
    }

    fn bind(&mut self, tid: TyVarId, ty: &Ty) -> Result<(), TyError> {
        let zonked = self.zonk(ty);

        if zonked.has_error() {
            return Ok(());
        }

        if matches!(zonked, Ty::Var(tid2) if tid2 == tid) {
            return Ok(());
        }

        if self.occurs(tid, &zonked) {
            return Err(TyError::InfiniteTy { tid });
        }

        self.bindings.insert(tid, zonked);
        Ok(())
    }

    fn unify_atomic(&mut self, a: &Ty, b: &Ty) -> Result<(), TyError> {
        let snapshot = self.bindings.clone();

        let result = self.unify(a, b);

        if result.is_err() {
            self.bindings = snapshot;
        }

        result
    }

    fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), TyError> {
        let a = self.zonk(a);
        let b = self.zonk(b);

        match (a, b) {
            (Ty::Error, _) | (_, Ty::Error) => Ok(()),

            (Ty::Var(tid), ty) | (ty, Ty::Var(tid)) => self.bind(tid, &ty),

            (
                Ty::Record {
                    field_tys: a_field_tys,
                },
                Ty::Record {
                    field_tys: b_field_tys,
                },
            ) => {
                if a_field_tys.keys().ne(b_field_tys.keys()) {
                    return Err(TyError::Mismatch {
                        expected: Ty::Record {
                            field_tys: a_field_tys,
                        },
                        got: Ty::Record {
                            field_tys: b_field_tys,
                        },
                    });
                }

                for (name, a_ty) in &a_field_tys {
                    let b_ty = b_field_tys
                        .get(name)
                        .expect("keys already checked to be equal");

                    self.unify(a_ty, b_ty)?;
                }

                Ok(())
            }

            (a, b) if a == b => Ok(()),

            (expected, got) => Err(TyError::Mismatch { expected, got }),
        }
    }

    fn expect_unify(&mut self, span: Span, expected: &Ty, found: &Ty) -> bool {
        let expected = self.zonk(expected);
        let found = self.zonk(found);

        if expected.has_error() || found.has_error() {
            return true;
        }

        match self.unify_atomic(&expected, &found) {
            Ok(()) => true,

            Err(err) => {
                self.emit_diagnostic(span, err);
                false
            }
        }
    }

    pub fn infer_query_root(&mut self, q: &Query) -> Ty {
        let saved_locals = std::mem::take(&mut self.locals);
        let ty = self.infer_query(q);
        self.locals = saved_locals;
        ty
    }

    pub fn infer_query(&mut self, q: &Query) -> Ty {
        for stmt in &*q.body {
            self.check_statement(stmt);
        }

        let head_ty = self.infer_pattern(&q.head);
        self.zonk(&head_ty)
    }

    pub fn infer_subquery(&mut self, q: &Query) -> Ty {
        let saved_locals = std::mem::take(&mut self.locals);
        let ty = self.infer_query(q);
        self.locals = saved_locals;
        ty
    }

    pub fn check_statement(&mut self, s: &Statement) -> bool {
        match s {
            Statement::ImplicitBind(p) => {
                let mark = self.mark_diagnostic();
                let ty = self.infer_pattern(p);

                !ty.has_error() && !self.diagnostic_emitted_since(mark)
            }

            Statement::Bind { left, right } => {
                let mark = self.mark_diagnostic();
                let left_ty = self.infer_pattern(left);

                if left_ty.has_error() || self.diagnostic_emitted_since(mark) {
                    // Still inspect the RHS so we can emit diagnostics inside it too.
                    let _ = self.infer_pattern(right);
                    return false;
                }

                self.check_pattern(right, &left_ty)
            }
        }
    }

    pub fn infer_pattern(&mut self, p: &Pattern) -> Ty {
        match &p.kind {
            PatternKind::Wildcard => Ty::Var(self.fresh_tid()),

            PatternKind::Int(_) => Ty::Int,

            PatternKind::String(_) | PatternKind::StringPrefix(_) => Ty::String,

            PatternKind::Var(s) => {
                if let Some(existing) = self.locals.get(s).cloned() {
                    self.zonk(&existing)
                } else {
                    let fresh = Ty::Var(self.fresh_tid());
                    self.locals.insert(*s, fresh.clone());
                    fresh
                }
            }

            PatternKind::Record { fields } => {
                let mut field_tys = BTreeMap::new();

                for (k, v) in fields {
                    field_tys.insert(*k, self.infer_pattern(v));
                }

                Ty::Record { field_tys }
            }

            PatternKind::Fact {
                predicate_id,
                key_pattern,
            } => {
                let Some(def) = self.schema.predicate_tys.get(predicate_id) else {
                    self.emit_diagnostic(
                        p.span.clone(),
                        TyError::UnknownPredicate {
                            predicate_id: *predicate_id,
                        },
                    );

                    return Ty::Error;
                };

                let key_ty = def.key_ty.clone();

                self.check_pattern(key_pattern, &key_ty);

                Ty::Fact {
                    predicate_id: *predicate_id,
                }
            }

            PatternKind::Subquery(q) => self.infer_subquery(q),
        }
    }

    pub fn check_pattern(&mut self, p: &Pattern, expected: &Ty) -> bool {
        let expected = self.zonk(expected);

        if expected.has_error() {
            return true;
        }

        match (&p.kind, &expected) {
            (PatternKind::Wildcard, _) => true,

            (PatternKind::Var(s), _) => self.bind_pattern_var(p.span.clone(), *s, &expected),

            (
                PatternKind::Record { fields },
                Ty::Record {
                    field_tys: expected_fields,
                },
            ) => self.check_record_pattern(fields, expected_fields),

            (_, Ty::Fact { predicate_id }) => {
                let Some(def) = self.schema.predicate_tys.get(predicate_id) else {
                    self.emit_diagnostic(
                        p.span.clone(),
                        TyError::UnknownPredicate {
                            predicate_id: *predicate_id,
                        },
                    );

                    return false;
                };

                let key_ty = def.key_ty.clone();
                self.check_pattern(p, &key_ty)
            }

            _ => {
                let snapshot = self.snapshot_type_state();
                let mark = self.mark_diagnostic();

                let found = self.infer_pattern(p);

                if found.has_error() || self.diagnostic_emitted_since(mark) {
                    self.rollback_type_state(snapshot);
                    return false;
                }

                let ok = self.expect_unify(p.span.clone(), &expected, &found);

                if !ok {
                    self.rollback_type_state(snapshot);
                }

                ok
            }
        }
    }

    fn bind_pattern_var(&mut self, span: Span, s: Symbol, expected: &Ty) -> bool {
        let expected = self.zonk(expected);

        if expected.has_error() {
            return true;
        }

        if let Some(existing) = self.locals.get(&s).cloned() {
            self.expect_unify(span, &expected, &existing)
        } else {
            self.locals.insert(s, expected);
            true
        }
    }

    fn check_record_pattern(
        &mut self,
        fields: &BTreeMap<Symbol, Pattern>,
        expected_fields: &BTreeMap<Symbol, Ty>,
    ) -> bool {
        let mut ok = true;

        for (name, pat) in fields {
            if !expected_fields.contains_key(name) {
                self.emit_diagnostic(pat.span.clone(), TyError::UnknownField { field: *name });

                ok = false;
            }
        }

        for (name, pat) in fields {
            let Some(expected) = expected_fields.get(name) else {
                continue;
            };

            if !self.check_pattern(pat, expected) {
                ok = false;
            }
        }

        ok
    }
}
