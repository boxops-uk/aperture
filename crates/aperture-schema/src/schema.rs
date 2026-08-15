use std::{collections::BTreeMap, sync::Arc};

use lasso::{Rodeo, RodeoReader, Spur};

/// A predicate's position in the schema, which **is** its id.
///
/// The field stays public, unlike [`FactId`](crate::focus::id::FactId)'s, because
/// there is no invariant here to protect: an id *is* a position, so building one
/// from an index is the ordinary thing to do. The check that matters — that the id
/// fits the fact-id tag — belongs where the tag is composed, and lives there.
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
    /// This predicate's name, or `None` if the schema's own interner cannot
    /// resolve it.
    ///
    /// `None` is a broken schema, not a predicate without a name — it used to
    /// come back as `""`, which reads as a valid empty name and travels on into
    /// diagnostics. Both callers already have a "no such predicate" path to fold
    /// it into.
    pub fn name(&self) -> Option<&'a str> {
        self.interner.resolve(self.inner.name)
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

    /// The text of a name interned in the schema.
    ///
    /// Takes a [`Spur`] rather than a [`Symbol`]: this tier holds schema names and
    /// nothing else, so a `Symbol::Local` is not a question it can answer, and a
    /// signature accepting one has to reply `None` to something it was never
    /// asked. The two-tier resolve is [`LocalInterner::try_resolve`], which
    /// delegates here for the schema half instead of reaching past this type into
    /// the reader it wraps.
    pub fn resolve(&self, spur: Spur) -> Option<&str> {
        self.0.try_resolve(&spur)
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

    /// The text behind a symbol, from whichever tier interned it.
    pub fn try_resolve(&self, symbol: Symbol) -> Option<&str> {
        match symbol {
            Symbol::Schema(spur) => self.schema.resolve(spur),
            Symbol::Local(spur) => self.local.try_resolve(&spur),
        }
    }
}

#[derive(Clone)]
pub struct Schema {
    interner: SchemaInterner,
    predicates: Arc<[Predicate]>,
    /// `name → id`, built once at construction.
    ///
    /// A predicate's *position* is its id, so `predicates` is in id order and
    /// cannot be searched by name. Lowering resolves a name for every fact
    /// pattern in a query, and scanning every predicate in the schema for each one
    /// is the wrong shape for something built once and then queried repeatedly.
    by_name: Arc<BTreeMap<Spur, PredicateId>>,
}

impl Schema {
    pub fn new(reader: RodeoReader, predicates: Arc<[Predicate]>) -> Self {
        let mut by_name = BTreeMap::new();

        for (idx, predicate) in predicates.iter().enumerate() {
            // First wins, as the linear scan this replaces did. Two predicates
            // sharing a name is a schema error for Phase 8 to reject; until then,
            // indexing them must not silently start preferring the other one.
            by_name
                .entry(predicate.name)
                .or_insert(PredicateId(idx as u32));
        }

        Schema {
            interner: SchemaInterner::new(reader),
            predicates,
            by_name: Arc::new(by_name),
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

    /// How many predicates the schema declares. A predicate's position **is** its
    /// id, so this is also one past the largest valid [`PredicateId`].
    pub fn len(&self) -> usize {
        self.predicates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty()
    }

    /// The predicate called `name`, and its id.
    pub fn find_position(&self, name: &str) -> Option<(PredicateId, PredicateRef<'_>)> {
        let spur = self.interner.get_spur(name)?;
        let id = *self.by_name.get(&spur)?;
        Some((id, self.get(id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_of(names: &[&str]) -> Schema {
        let mut rodeo = Rodeo::new();
        let predicates: Vec<Predicate> = names
            .iter()
            .map(|name| Predicate {
                name: rodeo.get_or_intern(name),
                key: PredicateTy::Int,
                value: None,
            })
            .collect();

        Schema::new(rodeo.into_reader(), Arc::from(predicates))
    }

    /// The two-tier resolve reaches both tiers, and the schema tier resolves a
    /// `Spur` rather than being asked about a `Symbol` it cannot own.
    #[test]
    fn the_two_tiers_resolve_their_own_names() {
        let mut rodeo = Rodeo::new();
        rodeo.get_or_intern("declared");
        let schema = SchemaInterner::new(rodeo.into_reader());

        let mut interner = LocalInterner::new(schema.clone());

        // A name the schema declares resolves through the schema tier...
        let declared = interner.get_or_intern("declared");
        assert!(matches!(declared, Symbol::Schema(_)));
        assert_eq!(interner.try_resolve(declared), Some("declared"));

        // ...and one it does not resolves through the local tier, which the schema
        // tier on its own could never have answered.
        let local = interner.get_or_intern("query-only");
        assert!(matches!(local, Symbol::Local(_)));
        assert_eq!(interner.try_resolve(local), Some("query-only"));

        let Symbol::Schema(spur) = declared else {
            unreachable!("checked above")
        };
        assert_eq!(schema.resolve(spur), Some("declared"));
    }

    /// A name lookup is an index built at construction rather than a scan, and a
    /// predicate's position is still its id.
    #[test]
    fn find_position_returns_the_declared_position() {
        let schema = schema_of(&["a.One", "b.Two", "c.Three"]);

        for (expected, name) in ["a.One", "b.Two", "c.Three"].iter().enumerate() {
            let (id, found) = schema.find_position(name).expect(name);
            assert_eq!(id, PredicateId(expected as u32));
            assert_eq!(found.name(), Some(*name));
        }

        assert!(schema.find_position("nosuch.Pred").is_none());
    }

    /// Two predicates sharing a name resolve to the **first**, as the linear scan
    /// this index replaced did. A duplicate is a schema error for Phase 8 to
    /// reject; indexing them must not quietly change which one a query gets.
    #[test]
    fn find_position_prefers_the_first_of_a_duplicated_name() {
        let schema = schema_of(&["a.One", "dup.Pred", "b.Two", "dup.Pred"]);

        let (id, _) = schema.find_position("dup.Pred").expect("dup.Pred");
        assert_eq!(id, PredicateId(1));
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
    // predicates permuted and split across files differently — and assert the
    // canonical forms are byte-identical and the fingerprints equal. A genuine
    // semantic change must still change the fingerprint (the negative control, or
    // the property passes trivially for a constant function).
    //
    // **Field order is not source layout.** A record's field order *is* its
    // encoding order (`focus::fact`), and it decides the seek prefix — so
    // permuting fields is a semantic change and belongs in the negative control,
    // never in the permuted-input arm. Asserting otherwise would certify two
    // schemas as identical whose facts have incompatible bytes. Glean draws the
    // line in the same place: field order is inside its per-predicate
    // fingerprint, and a field reorder is handled as a *transform* rather than as
    // identity.
    #[test]
    #[ignore = "I13 — pending Phase 8 (needs canonical form + fingerprints, PLAN 8)"]
    fn fingerprint_is_order_independent() {
        unimplemented!(
            "Phase 8: assert two source orderings of one schema share a fingerprint, and a semantic change does not"
        );
    }
}
