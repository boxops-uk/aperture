use std::sync::Arc;

use itertools::Itertools;
use lasso::{Rodeo, RodeoReader, Spur};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredicateId(pub u32);

pub const PREDICATE_ID_SIZE: usize = std::mem::size_of::<u32>();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Symbol {
    Schema(Spur),
    Local(Spur),
}

#[derive(Debug, Clone)]
pub enum PredicateTy {
    Int,
    Str,
    Fact(PredicateId),
    Record(Arc<[(Spur, PredicateTy)]>),
}

#[derive(Debug, Clone)]
pub struct Predicate {
    pub name: Spur,
    pub key: PredicateTy,
    pub value: Option<PredicateTy>,
}

pub struct PredicateTyRef<'a> {
    interner: &'a SchemaInterner,
    pub ty: &'a PredicateTy,
}

impl<'a> PredicateTyRef<'a> {
    pub fn find_field(&self, name: &str) -> Option<(usize, PredicateTyRef<'a>)> {
        let PredicateTy::Record(fields) = self.ty else {
            return None;
        };
        let spur = self.interner.get_spur(name)?;
        fields
            .iter()
            .enumerate()
            .find(|(_, (s, _))| *s == spur)
            .map(|(i, (_, ty))| {
                (
                    i,
                    PredicateTyRef {
                        interner: self.interner,
                        ty,
                    },
                )
            })
    }
}

pub struct PredicateRef<'a> {
    interner: &'a SchemaInterner,
    inner: &'a Predicate,
}

impl<'a> PredicateRef<'a> {
    pub fn name(&self) -> &str {
        self.interner
            .0
            .try_resolve(&self.inner.name)
            .unwrap_or_default()
    }

    pub fn key(&self) -> PredicateTyRef<'a> {
        PredicateTyRef {
            interner: self.interner,
            ty: &self.inner.key,
        }
    }

    pub fn value(&self) -> Option<PredicateTyRef<'a>> {
        self.inner.value.as_ref().map(|ty| PredicateTyRef {
            interner: self.interner,
            ty,
        })
    }

    pub fn predicate(&self) -> &'a Predicate {
        self.inner
    }
}

#[derive(Clone)]
pub struct SchemaInterner(Arc<RodeoReader>);

impl SchemaInterner {
    pub fn new(reader: RodeoReader) -> Self {
        SchemaInterner(Arc::new(reader))
    }

    pub fn get(&self, s: &str) -> Option<Symbol> {
        self.0.get(s).map(Symbol::Schema)
    }

    fn get_spur(&self, s: &str) -> Option<Spur> {
        self.0.get(s)
    }

    pub fn try_resolve(&self, symbol: Symbol) -> Option<&str> {
        match symbol {
            Symbol::Schema(spur) => self.0.try_resolve(&spur),
            Symbol::Local(_) => None,
        }
    }
}

pub struct LocalInterner {
    schema: SchemaInterner,
    local: Rodeo,
}

impl LocalInterner {
    pub fn new(schema: SchemaInterner) -> Self {
        LocalInterner {
            schema,
            local: Rodeo::new(),
        }
    }

    pub fn schema(&self) -> &SchemaInterner {
        &self.schema
    }

    pub fn get(&self, s: &str) -> Option<Symbol> {
        if let Some(symbol) = self.schema.get(s) {
            return Some(symbol);
        }
        self.local.get(s).map(Symbol::Local)
    }

    pub fn get_or_intern(&mut self, s: &str) -> Symbol {
        if let Some(symbol) = self.schema.get(s) {
            return symbol;
        }
        Symbol::Local(self.local.get_or_intern(s))
    }

    pub fn try_resolve(&self, symbol: Symbol) -> Option<&str> {
        match symbol {
            Symbol::Schema(spur) => self.schema.0.try_resolve(&spur),
            Symbol::Local(spur) => self.local.try_resolve(&spur),
        }
    }
}

#[derive(Clone)]
pub struct Schema {
    interner: SchemaInterner,
    predicates: Arc<[Predicate]>,
}

impl Schema {
    pub fn new(reader: RodeoReader, predicates: Arc<[Predicate]>) -> Self {
        Schema {
            interner: SchemaInterner::new(reader),
            predicates,
        }
    }

    pub fn interner(&self) -> &SchemaInterner {
        &self.interner
    }

    pub fn get(&self, id: PredicateId) -> Option<PredicateRef<'_>> {
        self.predicates
            .get(id.0 as usize)
            .map(|inner| PredicateRef {
                interner: &self.interner,
                inner,
            })
    }

    pub fn find_position(&self, name: &str) -> Option<(PredicateId, PredicateRef<'_>)> {
        let spur = self.interner.get_spur(name)?;
        let (idx, inner) = self.predicates.iter().find_position(|p| p.name == spur)?;
        Some((
            PredicateId(idx as u32),
            PredicateRef {
                interner: &self.interner,
                inner,
            },
        ))
    }
}

/// Phase-8 invariant guards: [I10](../../docs/invariants.md#i10) stable union
/// discriminants and [I13](../../docs/invariants.md#i13) the embedded, frozen
/// schema.
///
/// Both need the schema DSL — a parser, a canonical form, and fingerprints — which
/// arrives in **Phase 8** ([`PLAN.md`](../../PLAN.md)); today's schema is
/// hardcoded fixtures with no unions and no identity. The guards are written up
/// front as the specification, `#[ignore]`d until their subject exists, and named
/// under `pending_phase_8` so `cargo test -- --ignored --list` (the coverage
/// ledger) shows the phase that owns them.
#[cfg(test)]
mod pending_phase_8 {
    // I10 — union alternative discriminants are explicit, assigned once, and
    // append-only. They are frozen the moment union data is written, because a
    // discriminant is part of the on-disk encoding of every value of that type.
    //
    // Procedure: load a schema declaring a union, then load an edited version that
    // renumbers an existing alternative, and one that reuses a retired
    // discriminant for a new alternative — both must be rejected at load with a
    // specific diagnostic, not silently accepted. Appending a fresh alternative
    // with an unused discriminant must be accepted, since that is the one
    // permitted evolution.
    #[test]
    #[ignore = "I10 — pending Phase 8 (needs the schema DSL + unions, PLAN 8)"]
    fn discriminants_append_only() {
        unimplemented!(
            "Phase 8: assert renumbered and reused union discriminants are rejected at schema load"
        );
    }

    // I13 — the DB's schema is embedded and frozen at create, and every ingest is
    // validated against it by subset containment, so the DB stays
    // self-describing.
    //
    // Procedure: create a DB with schema S (embedding its canonical form and
    // fingerprint), then attempt to ingest a fact file whose schema fingerprint is
    // not subset-compatible with S — a renamed predicate, a changed key type, a
    // dropped field — and assert each is rejected. A fact file whose schema is a
    // compatible subset must be accepted.
    #[test]
    #[ignore = "I13 — pending Phase 8 (needs parsed schemas + ingest validation, PLAN 8)"]
    fn ingest_rejects_incompatible_schema() {
        unimplemented!(
            "Phase 8: assert ingest rejects a fact file whose schema is not subset-compatible"
        );
    }

    // I13 — schema identity is a property of the schema, not of its source
    // layout. The fingerprint is taken over the *canonical* form, so how the
    // declarations happen to be spread across files and orderings cannot change
    // it; otherwise a reformatting would invalidate every existing fact file
    // (and `ops-I4` reproducibility with it).
    //
    // Procedure: build the same schema from two different source orderings — the
    // predicates and their fields permuted, split across files differently — and
    // assert the canonical forms are byte-identical and the fingerprints equal.
    // A genuine semantic change must still change the fingerprint (the negative
    // control, or the property passes trivially for a constant function).
    #[test]
    #[ignore = "I13 — pending Phase 8 (needs canonical form + fingerprints, PLAN 8)"]
    fn fingerprint_is_order_independent() {
        unimplemented!(
            "Phase 8: assert two source orderings of one schema share a fingerprint, and a semantic change does not"
        );
    }
}
