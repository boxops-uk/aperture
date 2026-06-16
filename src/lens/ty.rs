use std::fmt::Display;

use crate::lens::{
    location::Location,
    query::{Pattern, PatternKind, Query, Statement},
    schema::{PredicateId, Schema},
};
use codespan_reporting::diagnostic::{Diagnostic, Label};
use im::HashMap;
use string_interner::{DefaultStringInterner, DefaultSymbol as Symbol};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TyVarId(usize);

#[derive(Debug, Clone)]
pub enum Ty {
    Error,
    Never,
    Int,
    String,
    Var(TyVarId),
    Record { field_tys: HashMap<Symbol, Ty> },
    Fact { predicate_id: PredicateId },
}

impl Ty {
    pub fn has_error(&self) -> bool {
        match self {
            Ty::Error => true,
            Ty::Never | Ty::Int | Ty::String | Ty::Var(_) => false,
            Ty::Record { field_tys } => field_tys.values().any(|ty| ty.has_error()),
            Ty::Fact { .. } => false,
        }
    }
}

type Env = HashMap<Symbol, TyVarId>;
type Subst = Vec<Option<Ty>>;

#[derive(Debug)]
struct UndoEntry {
    ty_var_id: TyVarId,
    prev: Option<Ty>,
}

#[derive(Debug)]
pub struct Snapshot {
    undo_log_len: usize,
    subst_len: usize,
}

#[derive(Debug, Clone)]
pub enum TyError {
    InfiniteTy { ty_var_id: TyVarId },
    Mismatch { expected: Ty, got: Ty },
    UnknownPredicate { predicate_id: PredicateId },
    UnknownField { field: Symbol },
}

#[derive(Debug)]
pub struct TyChecker<FileId = ()> {
    env: Env,
    subst: Subst,
    undo_log: Vec<UndoEntry>,
    errors: Vec<(Location<FileId>, TyError)>,
}

pub struct TyDisplay<'a> {
    pub string_interner: &'a DefaultStringInterner,
    pub schema: &'a Schema,
    pub ty: Ty,
}

impl Display for TyDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ty.clone() {
            Ty::Error => write!(f, "Error"),
            Ty::Never => write!(f, "Never"),
            Ty::Int => write!(f, "Int"),
            Ty::String => write!(f, "String"),
            Ty::Var(TyVarId(id)) => write!(f, "{}", u32_to_base26(id as u32)),
            Ty::Record { field_tys } => {
                let fields = field_tys
                    .iter()
                    .map(|(name, ty)| {
                        format!(
                            "{}: {}",
                            self.string_interner.resolve(*name).unwrap(),
                            TyDisplay {
                                string_interner: self.string_interner,
                                schema: self.schema,
                                ty: ty.clone(),
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{{ {} }}", fields)
            }
            Ty::Fact { predicate_id } => {
                let resolved = self
                    .schema
                    .get_predicate_ty(predicate_id)
                    .and_then(|pred_ty| self.string_interner.resolve(pred_ty.name));

                match resolved {
                    Some(name) => write!(f, "{}", name),
                    None => write!(f, "UnknownFact({})", predicate_id.0),
                }
            }
        }
    }
}

impl<FileId> TyChecker<FileId>
where
    FileId: Copy,
{
    pub fn new() -> Self {
        Self {
            env: HashMap::new(),
            subst: vec![],
            undo_log: vec![],
            errors: vec![],
        }
    }

    fn fresh_ty_var_id(&mut self) -> TyVarId {
        let id = self.subst.len();
        self.subst.push(None);
        TyVarId(id)
    }

    pub fn fresh_ty_var(&mut self) -> Ty {
        Ty::Var(self.fresh_ty_var_id())
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            undo_log_len: self.undo_log.len(),
            subst_len: self.subst.len(),
        }
    }

    pub fn rollback(&mut self, snapshot: Snapshot) {
        while self.undo_log.len() > snapshot.undo_log_len {
            let undo_entry = self.undo_log.pop().unwrap();
            self.subst[undo_entry.ty_var_id.0] = undo_entry.prev;
        }

        self.subst.truncate(snapshot.subst_len);
    }

    pub fn get_var_ty(&self, id: TyVarId) -> Option<Ty> {
        self.subst.get(id.0).cloned().flatten()
    }

    pub fn get_symbol_ty(&self, symbol: Symbol) -> Option<Ty> {
        let ty_var_id = self.env.get(&symbol)?;
        self.get_var_ty(*ty_var_id)
    }

    fn set_var_ty(&mut self, id: TyVarId, ty: Ty) {
        let prev = self.get_var_ty(id);
        self.undo_log.push(UndoEntry {
            ty_var_id: id,
            prev,
        });
        self.subst[id.0] = Some(ty);
    }

    pub fn zonk(&mut self, ty: &Ty) -> Ty {
        match ty {
            Ty::Error | Ty::Never | Ty::Int | Ty::String | Ty::Fact { .. } => ty.clone(),

            Ty::Var(id) => {
                let Some(bound_to) = self.get_var_ty(*id) else {
                    return ty.clone();
                };

                let bound_to = self.zonk(&bound_to);
                self.set_var_ty(*id, bound_to.clone());
                bound_to
            }

            Ty::Record { field_tys } => Ty::Record {
                field_tys: field_tys
                    .iter()
                    .map(|(field_name, field_ty)| (*field_name, self.zonk(field_ty)))
                    .collect(),
            },
        }
    }

    pub fn occurs(&mut self, id: TyVarId, ty: &Ty) -> bool {
        match self.zonk(ty) {
            Ty::Error | Ty::Never | Ty::Int | Ty::String | Ty::Fact { .. } => false,

            Ty::Var(other_id) => other_id == id,

            Ty::Record { field_tys } => {
                field_tys.values().any(|field_ty| self.occurs(id, field_ty))
            }
        }
    }

    fn bind_var(&mut self, id: TyVarId, ty: Ty) -> Result<(), TyError> {
        if self.occurs(id, &ty) {
            return Err(TyError::InfiniteTy { ty_var_id: id });
        }
        self.set_var_ty(id, ty);
        Ok(())
    }

    pub fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), TyError> {
        let a = self.zonk(a);
        let b = self.zonk(b);

        // 1. If either side has an error, silently succeed to prevent cascading errors.
        if a.has_error() || b.has_error() {
            return Ok(());
        }

        match (a, b) {
            // 2. Identical variables unify trivially.
            (Ty::Var(a_id), Ty::Var(b_id)) if a_id == b_id => Ok(()),

            // 3. Variables unify by binding to the other type.
            (Ty::Var(id), ty) | (ty, Ty::Var(id)) => self.bind_var(id, ty),

            // 4. Ground types unify trivially
            (Ty::Never, Ty::Never) | (Ty::Int, Ty::Int) | (Ty::String, Ty::String) => Ok(()),

            // 5. Predicate types unify if their predicate ids match.
            (Ty::Fact { predicate_id: a_id }, Ty::Fact { predicate_id: b_id }) if a_id == b_id => {
                Ok(())
            }

            // 6. Record types unify if they have the same set of fields and corresponding field types unify.
            (
                Ty::Record {
                    field_tys: a_fields,
                },
                Ty::Record {
                    field_tys: b_fields,
                },
            ) => {
                if a_fields.len() != b_fields.len() {
                    return Err(TyError::Mismatch {
                        expected: Ty::Record {
                            field_tys: a_fields,
                        },
                        got: Ty::Record {
                            field_tys: b_fields,
                        },
                    });
                }

                for (field_name, a_field_ty) in a_fields.iter() {
                    let Some(b_field_ty) = b_fields.get(field_name) else {
                        return Err(TyError::UnknownField { field: *field_name });
                    };

                    self.unify(a_field_ty, b_field_ty)?
                }

                Ok(())
            }

            (a, b) => Err(TyError::Mismatch {
                expected: a,
                got: b,
            }),
        }
    }

    pub fn diagnostics(
        &self,
        string_interner: &DefaultStringInterner,
        schema: &Schema,
    ) -> impl Iterator<Item = Diagnostic<FileId>> {
        self.errors
            .iter()
            .map(|(location, error)| to_diagnostic(string_interner, schema, *location, error))
    }
}

pub fn to_diagnostic<FileId>(
    string_interner: &DefaultStringInterner,
    schema: &Schema,
    location: Location<FileId>,
    error: &TyError,
) -> Diagnostic<FileId> {
    match error {
        TyError::InfiniteTy { ty_var_id } => Diagnostic::error()
            .with_message("infinite type")
            .with_labels(vec![
                Label::primary(location.file_id, location.span).with_message(format!(
                    "type variable {} occurs inside its own type",
                    u32_to_base26(ty_var_id.0 as u32)
                )),
            ]),
        TyError::Mismatch { expected, got } => Diagnostic::error()
            .with_message("type mismatch")
            .with_labels(vec![
                Label::primary(location.file_id, location.span).with_message(format!(
                    "expected type {}, got {}",
                    TyDisplay {
                        string_interner,
                        schema,
                        ty: expected.clone(),
                    },
                    TyDisplay {
                        string_interner,
                        schema,
                        ty: got.clone(),
                    }
                )),
            ]),
        TyError::UnknownPredicate { predicate_id } => Diagnostic::error()
            .with_message("unknown predicate")
            .with_labels(vec![
                Label::primary(location.file_id, location.span).with_message(format!(
                    "predicate with id {} is not defined in the schema",
                    predicate_id.0
                )),
            ]),
        TyError::UnknownField { field } => Diagnostic::error()
            .with_message("unknown field")
            .with_labels(vec![
                Label::primary(location.file_id, location.span).with_message(format!(
                    "field {} is not defined in the record type",
                    string_interner.resolve(*field).unwrap()
                )),
            ]),
    }
}

pub fn check_pattern(
    ty_checker: &mut TyChecker,
    schema: &Schema,
    pattern: &Pattern,
    expected_ty: &Ty,
) {
    let expected_ty = ty_checker.zonk(expected_ty);

    // Silently succeed when the expected type is already poisoned to prevent cascading errors.
    if expected_ty.has_error() {
        return;
    }

    match &pattern.kind {
        PatternKind::Wildcard => {}

        PatternKind::Record { field_patterns } => match &expected_ty {
            Ty::Record { field_tys } => {
                check_record(ty_checker, schema, field_patterns, field_tys);
            }

            // Implicit predicate name: `{ field = ... }` where a Fact type is expected
            // means the record is the key pattern for that predicate.
            Ty::Fact { predicate_id } => match schema.get_predicate_key_ty(*predicate_id) {
                None => {
                    ty_checker.errors.push((
                        pattern.location,
                        TyError::UnknownPredicate {
                            predicate_id: *predicate_id,
                        },
                    ));
                }
                Some(key_ty) => {
                    let key_ty = key_ty.clone();
                    let key_ty = ty_checker.zonk(&key_ty);
                    if let Ty::Record { field_tys } = key_ty {
                        check_record(ty_checker, schema, field_patterns, &field_tys);
                    } else {
                        ty_checker.errors.push((
                            pattern.location,
                            TyError::Mismatch {
                                expected: expected_ty,
                                got: Ty::Record {
                                    field_tys: HashMap::new(),
                                },
                            },
                        ));
                    }
                }
            },

            _ => {
                ty_checker.errors.push((
                    pattern.location,
                    TyError::Mismatch {
                        expected: expected_ty,
                        got: Ty::Record {
                            field_tys: HashMap::new(),
                        },
                    },
                ));
            }
        },

        // For all other patterns: infer then unify.
        // Snapshot subst and save env so a failed unification is fully undone.
        _ => {
            let snapshot = ty_checker.snapshot();
            let saved_env = ty_checker.env.clone();
            let inferred = infer_pattern(ty_checker, schema, pattern);
            if let Err(e) = ty_checker.unify(&inferred, &expected_ty) {
                ty_checker.rollback(snapshot);
                ty_checker.env = saved_env;
                ty_checker.errors.push((pattern.location, e));
            }
        }
    }
}

pub fn check_record(
    ty_checker: &mut TyChecker,
    schema: &Schema,
    field_patterns: &HashMap<Symbol, Pattern>,
    expected_tys: &HashMap<Symbol, Ty>,
) {
    for (field_name, field_pattern) in field_patterns.iter() {
        match expected_tys.get(field_name) {
            None => {
                ty_checker.errors.push((
                    field_pattern.location,
                    TyError::UnknownField { field: *field_name },
                ));
            }
            Some(expected_ty) => {
                // Snapshot before each field so a failed field check doesn't
                // leave partial unification residue that pollutes sibling fields.
                let snapshot = ty_checker.snapshot();
                let saved_env = ty_checker.env.clone();
                let diag_len = ty_checker.errors.len();
                check_pattern(ty_checker, schema, field_pattern, expected_ty);
                if ty_checker.errors.len() > diag_len {
                    ty_checker.rollback(snapshot);
                    ty_checker.env = saved_env;
                }
            }
        }
    }
}

pub fn infer_query(ty_checker: &mut TyChecker, schema: &Schema, query: &Query) -> Ty {
    for statement in query.body.iter() {
        check_statement(ty_checker, schema, statement);
    }
    infer_pattern(ty_checker, schema, &query.head)
}

fn check_statement(ty_checker: &mut TyChecker, schema: &Schema, statement: &Statement) {
    match statement {
        Statement::Bind { left, right } => {
            // Infer the left side first, then check the right against that type.
            // This makes the "implicit predicate name" feature work naturally when
            // the right side is a record and the left side resolves to a Fact type.
            let left_ty = infer_pattern(ty_checker, schema, left);
            check_pattern(ty_checker, schema, right, &left_ty);
        }
        // An implicit bind is like `pattern = _`: just process the pattern for
        // its side effects (variable introduction, key-pattern checking).
        Statement::ImplicitBind(pattern) => {
            infer_pattern(ty_checker, schema, pattern);
        }
    }
}

pub fn infer_subquery(ty_checker: &mut TyChecker, schema: &Schema, subquery: &Query) -> Ty {
    // Variables introduced inside a subquery must not leak into the outer scope.
    let saved_env = ty_checker.env.clone();
    let result_ty = infer_query(ty_checker, schema, subquery);
    ty_checker.env = saved_env;
    result_ty
}

pub fn infer_pattern(ty_checker: &mut TyChecker, schema: &Schema, pattern: &Pattern) -> Ty {
    match &pattern.kind {
        PatternKind::Wildcard => ty_checker.fresh_ty_var(),
        PatternKind::Int(_) => Ty::Int,
        PatternKind::String(_) | PatternKind::StringPrefix(_) => Ty::String,

        PatternKind::Var(symbol) => {
            if let Some(&ty_var_id) = ty_checker.env.get(symbol) {
                // Already bound — return the existing type variable.
                Ty::Var(ty_var_id)
            } else {
                // Fresh introduction: allocate a type variable and commit the env
                // binding unconditionally. The caller is responsible for snapshotting
                // env if the introduction should be conditional on later success.
                let id = ty_checker.fresh_ty_var_id();
                ty_checker.env.insert(*symbol, id);
                Ty::Var(id)
            }
        }

        PatternKind::Subquery(query) => infer_subquery(ty_checker, schema, query),

        PatternKind::Record { field_patterns } => {
            let field_tys = field_patterns
                .iter()
                .map(|(field_name, field_pattern)| {
                    (
                        *field_name,
                        infer_pattern(ty_checker, schema, field_pattern),
                    )
                })
                .collect();
            Ty::Record { field_tys }
        }

        PatternKind::Fact {
            predicate_id,
            key_pattern,
        } => {
            let Some(key_ty) = schema.get_predicate_key_ty(*predicate_id) else {
                ty_checker.errors.push((
                    pattern.location,
                    TyError::UnknownPredicate {
                        predicate_id: *predicate_id,
                    },
                ));
                return Ty::Error;
            };

            check_pattern(ty_checker, schema, key_pattern, &key_ty);

            Ty::Fact {
                predicate_id: *predicate_id,
            }
        }
    }
}

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
