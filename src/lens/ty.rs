use crate::lens::{
    location::Location,
    query::{Pattern, Query},
    schema::PredicateId,
};
use im::HashMap;
use string_interner::DefaultSymbol as Symbol;

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
    pub diagnostics: Vec<(Location<FileId>, TyError)>,
}

impl<FileId> TyChecker<FileId> {
    pub fn new() -> Self {
        Self {
            env: HashMap::new(),
            subst: vec![],
            undo_log: vec![],
            diagnostics: vec![],
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
}

pub fn check_pattern(ty_checker: &mut TyChecker, pattern: &Pattern, expected_ty: &Ty) {
    todo!()
}

pub fn check_record(
    ty_checker: &mut TyChecker,
    field_patterns: &HashMap<Symbol, Pattern>,
    expected_tys: &HashMap<Symbol, Ty>,
) {
    for (field_name, field_pattern) in field_patterns.iter() {
        match expected_tys.get(field_name) {
            Some(expected_ty) => {
                check_pattern(ty_checker, field_pattern, expected_ty);
            }
            None => {
                ty_checker.diagnostics.push((
                    field_pattern.location,
                    TyError::UnknownField { field: *field_name },
                ));
            }
        }
    }
}

pub fn infer_query(ty_checker: &mut TyChecker, query: &Query) -> Ty {
    todo!()
}

pub fn infer_subquery(ty_checker: &mut TyChecker, subquery: &Query) -> Ty {
    todo!()
}

pub fn infer_pattern(ty_checker: &mut TyChecker, pattern: &Pattern) -> Ty {
    todo!()
}
