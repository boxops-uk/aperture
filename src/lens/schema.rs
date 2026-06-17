use im::HashMap;
use string_interner::DefaultSymbol as Symbol;

use crate::lens::ty::Ty;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PredicateId(pub(crate) usize);

#[derive(Debug)]
pub struct PredicateTy {
    pub id: PredicateId,
    pub namespace: Box<[Symbol]>,
    pub name: Symbol,
    pub key_ty: Ty,
    pub value_ty: Ty,
}

#[derive(Debug)]
pub struct Schema {
    next_predicate_id: usize,
    predicate_tys: Vec<PredicateTy>,
    predicate_ids: HashMap<Box<[Symbol]>, HashMap<Symbol, PredicateId>>,
}

impl Schema {
    pub fn new() -> Self {
        Self {
            next_predicate_id: 0,
            predicate_tys: vec![],
            predicate_ids: HashMap::new(),
        }
    }

    fn next_predicate_id(&mut self) -> PredicateId {
        let id = self.next_predicate_id;
        self.next_predicate_id += 1;
        PredicateId(id)
    }

    pub fn predicate_tys(&self) -> impl Iterator<Item = &PredicateTy> {
        self.predicate_tys.iter()
    }

    pub fn get_predicate_ty(&self, PredicateId(i): PredicateId) -> Option<&PredicateTy> {
        self.predicate_tys.get(i)
    }

    pub fn get_predicate_key_ty(&self, predicate_id: PredicateId) -> Option<&Ty> {
        self.get_predicate_ty(predicate_id)
            .map(|predicate_ty| &predicate_ty.key_ty)
    }

    pub fn get_predicate_value_ty(&self, predicate_id: PredicateId) -> Option<&Ty> {
        self.get_predicate_ty(predicate_id)
            .map(|predicate_ty| &predicate_ty.value_ty)
    }

    pub fn get_predicate_id(&self, namespace: &[Symbol], name: Symbol) -> Option<PredicateId> {
        self.predicate_ids.get(namespace)?.get(&name).copied()
    }

    pub fn predicates_in_namespace(
        &self,
        namespace: &[Symbol],
    ) -> impl Iterator<Item = (Symbol, PredicateId)> + '_ {
        self.predicate_ids
            .get(namespace)
            .into_iter()
            .flat_map(|inner| inner.iter().map(|(&name, &id)| (name, id)))
    }

    pub fn insert_predicate_ty(
        &mut self,
        namespace: Box<[Symbol]>,
        name: Symbol,
        key_ty: Ty,
        value_ty: Ty,
    ) -> PredicateId {
        let predicate_id = self.next_predicate_id();
        let predicate_ty = PredicateTy {
            id: predicate_id,
            namespace: namespace.clone(),
            name,
            key_ty,
            value_ty,
        };
        self.predicate_tys.push(predicate_ty);

        let mut inner = self
            .predicate_ids
            .get(&namespace)
            .cloned()
            .unwrap_or_default();
        inner.insert(name, predicate_id);
        self.predicate_ids.insert(namespace, inner);

        predicate_id
    }
}
