use byteview::ByteView;
use serde::{Serialize, Serializer};

use crate::focus::{
    error::{ApertureError, StoreError},
    iter::Address,
    schema::{PredicateId, PredicateTy, Symbol},
    tuple::Value,
};

/// Bits of a [`FactId`] holding the predicate tag — the high three bytes.
///
/// Byte-aligned on purpose: the tag is a *slice* of the big-endian encoding, not a
/// shift, so routing a `point()` to a predicate's tree costs nothing.
pub const FACT_ID_PREDICATE_BITS: u32 = 24;

/// Bits of a [`FactId`] holding the per-predicate sequence — the low five bytes.
pub const FACT_ID_SEQUENCE_BITS: u32 = u64::BITS - FACT_ID_PREDICATE_BITS;

/// Largest predicate id representable in a [`FactId`] tag (~16.7 M predicates).
pub const MAX_TAGGABLE_PREDICATE: u32 = (1 << FACT_ID_PREDICATE_BITS) - 1;

/// Largest per-predicate sequence (~1.1 T facts per predicate).
pub const MAX_FACT_SEQUENCE: u64 = (1 << FACT_ID_SEQUENCE_BITS) - 1;

/// A fact's physical row id: a **snowflake** — the owning predicate in the high
/// [`FACT_ID_PREDICATE_BITS`] bits, a per-predicate sequence in the low
/// [`FACT_ID_SEQUENCE_BITS`] ([I11], [chapter 3]).
///
/// The tag is what lets `entities` be split per predicate exactly as `keys` is:
/// [`FactStore::point`] is handed a bare id and no predicate, so an untagged id
/// would make identity lookup a search across every predicate's tree. Tagged, it
/// is one lookup in one tree. It also removes the global allocator: each predicate
/// counts its own facts, so two ingest workers on different predicates share no
/// counter and write disjoint, ascending id ranges.
///
/// **Sequence 0 is reserved**, so no valid id is `FactId(0)` and a zeroed or
/// corrupt eight bytes is detectably not a fact — worth having on a path where
/// [I11] is what makes a bytes-only resume cursor safe.
///
/// Uniqueness is structural rather than enforced: the tag partitions the id space,
/// so two predicates cannot collide however their sequences are allocated.
///
/// [I11]: ../../../docs/invariants.md#i11
/// [chapter 3]: ../../../docs/03-storage-model.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FactId(pub u64);

impl FactId {
    /// Compose an id from its predicate and sequence.
    ///
    /// # Errors
    ///
    /// [`StoreError::PredicateIdTooWide`] if the predicate does not fit the tag,
    /// [`StoreError::FactIdSequence`] if the sequence is 0 (reserved) or past
    /// [`MAX_FACT_SEQUENCE`].
    pub fn new(predicate: PredicateId, sequence: u64) -> Result<Self, StoreError> {
        if predicate.0 > MAX_TAGGABLE_PREDICATE {
            return Err(StoreError::PredicateIdTooWide {
                predicate: predicate.0,
                max: MAX_TAGGABLE_PREDICATE,
            });
        }
        if sequence == 0 || sequence > MAX_FACT_SEQUENCE {
            return Err(StoreError::FactIdSequence {
                sequence,
                max: MAX_FACT_SEQUENCE,
            });
        }

        Ok(Self(
            (u64::from(predicate.0) << FACT_ID_SEQUENCE_BITS) | sequence,
        ))
    }

    /// The predicate that owns this fact.
    #[must_use]
    pub fn predicate(self) -> PredicateId {
        // The shift leaves 24 bits, so the narrowing cannot truncate.
        PredicateId((self.0 >> FACT_ID_SEQUENCE_BITS) as u32)
    }

    /// This fact's sequence within its predicate.
    #[must_use]
    pub fn sequence(self) -> u64 {
        self.0 & MAX_FACT_SEQUENCE
    }
}

impl Serialize for FactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

#[derive(Debug)]
pub enum SeekKey {
    Prefix(Box<[u8]>),
    Composite(Box<[SeekKeyPart]>),
}

#[derive(Debug)]
pub enum SeekKeyPart {
    Bytes(Box<[u8]>),
    RegisterField { address: Address, field_idx: usize },
}

#[derive(Debug)]
pub struct Access {
    pub predicate_id: PredicateId,
    pub seek_key: SeekKey,
}

#[derive(Debug)]
pub enum ResidualOp {
    EqConst(Box<[u8]>),
    Prefix(Box<[u8]>),
    EqRegisterField { address: Address, field_idx: usize },
}

#[derive(Debug)]
pub struct Residual {
    pub field_idx: usize,
    pub op: ResidualOp,
}

#[derive(Debug)]
pub struct Generator {
    pub access: Access,
    pub binds: Box<[Address]>,
    pub residuals: Box<[Residual]>,
}

#[derive(Debug)]
pub enum Project {
    Lit(Value),
    RegisterField {
        address: Address,
        field_idx: usize,
        ty: PredicateTy,
    },
    FactRef(Address),
    Value {
        address: Address,
        ty: PredicateTy,
    },
    Record(Box<[(Symbol, Project)]>),
}

pub struct Plan {
    pub nvars: usize,
    pub body: Box<[Generator]>,
    pub head: Project,
}

#[derive(Debug)]
pub struct Entity {
    pub key: ByteView,
    pub value: ByteView,
}

pub trait FactStore {
    type Scan: Iterator<Item = Result<(ByteView, FactId), ApertureError>>;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Self::Scan;

    fn point(&self, id: FactId) -> Result<Option<Entity>, ApertureError>;
}

/// Schema-first `(plan, store)` generator — the executor's hard generation case.
///
/// A randomly-built [`Plan`] is almost always invalid (it names predicates that
/// don't exist, splices unbound registers, indexes fields past a key's arity) and
/// so tests only the error path. This generator is **valid by construction**
/// instead: draw a small schema (predicates × key field types) → draw facts
/// conforming to it → draw a plan valid *against that schema*, introducing
/// registers in dependency order and only ever splicing an already-bound one.
///
/// Draws are unconstrained small numbers **resolved modulo the legal options**,
/// so no case is wasted and shrinking yields a *minimal valid* counterexample
/// rather than garbage — the generator is the type checker in reverse. See
/// [`docs/testing.md`](../../../docs/testing.md).
#[cfg(any(test, feature = "proptest"))]
pub mod proptest {
    use std::collections::BTreeSet;

    use ::proptest::prelude::*;

    use super::{Access, Generator, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart};
    use crate::focus::{
        fixtures::{compose, i64_field, interner_with, str_field},
        iter::Address,
        mem_store::MemStore,
        schema::{LocalInterner, PredicateId, PredicateTy},
    };

    /// Bounds are deliberately tight: the resume battery re-runs a plan once per
    /// cut point, so the work per case is quadratic in the row count.
    const MAX_PREDICATES: usize = 2;
    const MAX_ARITY: usize = 2;
    const MAX_LEVELS: usize = 3;
    const MAX_FACTS: usize = 6;

    /// Field values come from a deliberately tiny domain so joins actually match.
    /// Drawn from the full `i64`/`String` range, every join would be empty and the
    /// battery would exercise nothing but backtracking.
    const INTS: [i64; 4] = [0, 1, 2, 3];
    const STRS: [&str; 3] = ["a", "b", "c"];

    /// Upper bound (exclusive) on every "pick" draw; resolution takes it modulo
    /// however many options are legal in context.
    const PICKS: u8 = 4;

    /// One head record field per level, so the projected row shows every binding.
    /// Listed in level order, which is also sorted order — record fields are
    /// sorted slices everywhere (a codec requirement).
    const FIELD_NAMES: [&str; MAX_LEVELS] = ["r0", "r1", "r2"];

    /// A key field's type. Scalars only: nested records in keys are the codec's
    /// business and are covered by `codec::proptest`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FieldTy {
        Int,
        Str,
    }

    impl FieldTy {
        fn of(pick: u8) -> Self {
            if pick.is_multiple_of(2) {
                FieldTy::Int
            } else {
                FieldTy::Str
            }
        }

        fn predicate_ty(self) -> PredicateTy {
            match self {
                FieldTy::Int => PredicateTy::Int,
                FieldTy::Str => PredicateTy::Str,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    enum FieldVal {
        Int(i64),
        Str(&'static str),
    }

    impl FieldVal {
        fn of(ty: FieldTy, pick: u8) -> Self {
            match ty {
                FieldTy::Int => FieldVal::Int(INTS[pick as usize % INTS.len()]),
                FieldTy::Str => FieldVal::Str(STRS[pick as usize % STRS.len()]),
            }
        }

        fn encode(&self) -> Vec<u8> {
            match self {
                FieldVal::Int(i) => i64_field(*i),
                FieldVal::Str(s) => str_field(s),
            }
        }
    }

    #[derive(Debug, Clone)]
    struct PredicateSpec {
        fields: Vec<FieldTy>,
    }

    #[derive(Debug, Clone)]
    enum ResidualSpec {
        EqConst {
            field: usize,
            val: FieldVal,
        },
        EqRegisterField {
            field: usize,
            level: usize,
            ref_field: usize,
        },
    }

    #[derive(Debug, Clone)]
    struct LevelSpec {
        predicate: usize,
        /// The `(level, field)` spliced into this level's scan prefix; `None` is a
        /// full scan of the predicate.
        seek: Option<(usize, usize)>,
        residual: Option<ResidualSpec>,
    }

    #[derive(Debug, Clone)]
    enum HeadSpec {
        Field { level: usize, field: usize },
        FactRef { level: usize },
    }

    /// A valid `(plan, store)` pair, materialisable as often as needed.
    ///
    /// `Executor::new`/`resume` consume both the store and the plan, so an
    /// interrupted run needs a *fresh, equivalent* pair per segment — hence a
    /// rebuildable spec rather than one built pair.
    #[derive(Debug, Clone)]
    pub struct PlanAndStore {
        schema: Vec<PredicateSpec>,
        /// `facts[p]` — the key tuples of predicate `p`, deduplicated and sorted.
        facts: Vec<Vec<Vec<FieldVal>>>,
        levels: Vec<LevelSpec>,
        head: Vec<HeadSpec>,
    }

    impl PlanAndStore {
        /// The plan's loop-nest depth (one register, one generator per level).
        pub fn levels(&self) -> usize {
            self.levels.len()
        }

        /// An interner holding the head's record field names, so projection can
        /// resolve them.
        pub fn interner(&self) -> LocalInterner {
            interner_with(&FIELD_NAMES[..self.levels.len()])
        }

        pub fn build(&self, interner: &LocalInterner) -> (MemStore, Plan) {
            (self.build_store(), self.build_plan(interner))
        }

        /// Fact ids come from a monotonic counter walked in a deterministic order,
        /// so every rebuild yields an identical store — resume's integrity check
        /// compares a re-read row's `fact_id` against the saved one.
        pub fn build_store(&self) -> MemStore {
            let mut store = MemStore::new();
            let mut next_id = 1u64;

            for (predicate, keys) in self.facts.iter().enumerate() {
                for key in keys {
                    store.insert(PredicateId(predicate as u32), encode_key(key), next_id);
                    next_id += 1;
                }
            }

            store
        }

        pub fn build_plan(&self, interner: &LocalInterner) -> Plan {
            let body = self
                .levels
                .iter()
                .enumerate()
                .map(|(level, spec)| Generator {
                    access: Access {
                        predicate_id: PredicateId(spec.predicate as u32),
                        seek_key: match spec.seek {
                            None => SeekKey::Prefix(Box::new([])),
                            Some((ref_level, ref_field)) => {
                                SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                                    address: Address::new(ref_level),
                                    field_idx: ref_field,
                                }]))
                            }
                        },
                    },
                    binds: Box::new([Address::new(level)]),
                    residuals: match &spec.residual {
                        None => Box::new([]),
                        Some(ResidualSpec::EqConst { field, val }) => Box::new([Residual {
                            field_idx: *field,
                            op: ResidualOp::EqConst(val.encode().into_boxed_slice()),
                        }]),
                        Some(ResidualSpec::EqRegisterField {
                            field,
                            level: ref_level,
                            ref_field,
                        }) => Box::new([Residual {
                            field_idx: *field,
                            op: ResidualOp::EqRegisterField {
                                address: Address::new(*ref_level),
                                field_idx: *ref_field,
                            },
                        }]),
                    },
                })
                .collect::<Vec<_>>()
                .into_boxed_slice();

            let head = self
                .head
                .iter()
                .map(|item| {
                    let level = match item {
                        HeadSpec::Field { level, .. } | HeadSpec::FactRef { level } => *level,
                    };

                    // `interner()` interns exactly these names, so the lookup
                    // cannot fail.
                    let name = interner
                        .get(FIELD_NAMES[level])
                        .expect("head field name is interned by `PlanAndStore::interner`");

                    let projection = match item {
                        HeadSpec::FactRef { level } => Project::FactRef(Address::new(*level)),
                        HeadSpec::Field { level, field } => Project::RegisterField {
                            address: Address::new(*level),
                            field_idx: *field,
                            ty: self.field_ty(*level, *field).predicate_ty(),
                        },
                    };

                    (name, projection)
                })
                .collect::<Vec<_>>()
                .into_boxed_slice();

            Plan {
                nvars: self.levels.len(),
                body,
                head: Project::Record(head),
            }
        }

        fn field_ty(&self, level: usize, field: usize) -> FieldTy {
            self.schema[self.levels[level].predicate].fields[field]
        }
    }

    /// A composite key is its encoded fields back-to-back — the encoding is
    /// self-delimiting (I2), so no lengths or separators are needed.
    fn encode_key(key: &[FieldVal]) -> Vec<u8> {
        let fields: Vec<Vec<u8>> = key.iter().map(FieldVal::encode).collect();
        let fields: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();

        compose(&fields)
    }

    #[derive(Debug, Clone)]
    struct PredicateDraw {
        arity: usize,
        field_tys: Vec<u8>,
    }

    #[derive(Debug, Clone)]
    struct LevelDraw {
        predicate: u8,
        seek: bool,
        residual: u8,
        field: u8,
        reference: u8,
        constant: u8,
    }

    #[derive(Debug, Clone)]
    struct HeadDraw {
        fact_ref: bool,
        field: u8,
    }

    /// Choose the constant for an `EqConst` residual from a value that actually
    /// occurs in this predicate's facts at that field.
    ///
    /// Drawn from the abstract domain instead, the constant matches nothing most
    /// of the time — the residual filters the whole predicate away and the case
    /// exercises no rows. Falls back to the domain for an empty predicate.
    fn constant_for(facts: &[Vec<FieldVal>], field: usize, ty: FieldTy, pick: u8) -> FieldVal {
        match facts.len() {
            0 => FieldVal::of(ty, pick),
            len => facts[pick as usize % len][field].clone(),
        }
    }

    /// Pick an already-bound `(level, field)` whose type is `want`.
    ///
    /// Only levels *before* the one being built are considered — that's what makes
    /// "every variable is bound before use" structural rather than checked. Fields
    /// of a different type are skipped because their encodings can never compare
    /// equal, which would silently generate a plan that matches nothing.
    /// `None` means no earlier level offers one, and the caller falls back to a
    /// construct needing no reference.
    fn pick_reference(
        bound: &[LevelSpec],
        schema: &[PredicateSpec],
        want: FieldTy,
        pick: u8,
    ) -> Option<(usize, usize)> {
        let mut candidates = Vec::new();

        for (level, spec) in bound.iter().enumerate() {
            for (field, ty) in schema[spec.predicate].fields.iter().enumerate() {
                if *ty == want {
                    candidates.push((level, field));
                }
            }
        }

        if candidates.is_empty() {
            None
        } else {
            Some(candidates[pick as usize % candidates.len()])
        }
    }

    fn resolve(
        npredicates: usize,
        predicates: Vec<PredicateDraw>,
        facts: Vec<Vec<Vec<u8>>>,
        levels: Vec<LevelDraw>,
        heads: Vec<HeadDraw>,
    ) -> PlanAndStore {
        let schema: Vec<PredicateSpec> = predicates
            .iter()
            .take(npredicates)
            .map(|draw| PredicateSpec {
                fields: draw
                    .field_tys
                    .iter()
                    .take(draw.arity)
                    .map(|&pick| FieldTy::of(pick))
                    .collect(),
            })
            .collect();

        // Facts conform to their predicate's key types by construction. Each draw
        // carries `MAX_ARITY` picks and the predicate uses the first `arity`.
        let facts: Vec<Vec<Vec<FieldVal>>> = schema
            .iter()
            .zip(facts)
            .map(|(predicate, drawn)| {
                let mut keys: Vec<Vec<FieldVal>> = drawn
                    .iter()
                    .map(|picks| {
                        predicate
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(field, &ty)| FieldVal::of(ty, picks[field]))
                            .collect()
                    })
                    .collect();

                // One key, one fact: the `keys` index maps a key to a single fact
                // id, so a repeated draw would otherwise shadow an earlier fact.
                keys.sort();
                keys.dedup();
                keys
            })
            .collect();

        let mut resolved: Vec<LevelSpec> = Vec::with_capacity(levels.len());

        for draw in &levels {
            let predicate = draw.predicate as usize % schema.len();
            let fields = &schema[predicate].fields;
            let field = draw.field as usize % fields.len();

            // A seek splices a bound register's field into this level's scan
            // prefix, which only means anything against the key's *first* field.
            let seek = if draw.seek {
                pick_reference(&resolved, &schema, fields[0], draw.reference)
            } else {
                None
            };

            let constant = ResidualSpec::EqConst {
                field,
                val: constant_for(&facts[predicate], field, fields[field], draw.constant),
            };

            let residual = match draw.residual % 3 {
                0 => None,
                1 => Some(constant),
                // A cross-loop equality against a bound register — the residual
                // form a seek can't express. Falls back to a constant when no
                // earlier level offers a type-matching field.
                _ => Some(
                    match pick_reference(&resolved, &schema, fields[field], draw.reference) {
                        Some((level, ref_field)) => ResidualSpec::EqRegisterField {
                            field,
                            level,
                            ref_field,
                        },
                        None => constant,
                    },
                ),
            };

            resolved.push(LevelSpec {
                predicate,
                seek,
                residual,
            });
        }

        let head = resolved
            .iter()
            .enumerate()
            .zip(heads)
            .map(|((level, spec), draw)| {
                if draw.fact_ref {
                    HeadSpec::FactRef { level }
                } else {
                    HeadSpec::Field {
                        level,
                        field: draw.field as usize % schema[spec.predicate].fields.len(),
                    }
                }
            })
            .collect();

        PlanAndStore {
            schema,
            facts,
            levels: resolved,
            head,
        }
    }

    fn arb_predicate() -> impl Strategy<Value = PredicateDraw> {
        (1..=MAX_ARITY, prop::collection::vec(0u8..PICKS, MAX_ARITY))
            .prop_map(|(arity, field_tys)| PredicateDraw { arity, field_tys })
    }

    /// Every predicate gets at least one fact: an empty predicate at level 0 makes
    /// the whole run empty, and "the scan finds nothing" is already reached
    /// constantly by seeks and residuals that match no row.
    fn arb_predicate_facts() -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(prop::collection::vec(0u8..PICKS, MAX_ARITY), 1..=MAX_FACTS)
    }

    fn arb_level() -> impl Strategy<Value = LevelDraw> {
        (
            0u8..PICKS,
            any::<bool>(),
            0u8..3,
            0u8..PICKS,
            0u8..PICKS,
            0u8..PICKS,
        )
            .prop_map(|(predicate, seek, residual, field, reference, constant)| {
                LevelDraw {
                    predicate,
                    seek,
                    residual,
                    field,
                    reference,
                    constant,
                }
            })
    }

    fn arb_head() -> impl Strategy<Value = HeadDraw> {
        (any::<bool>(), 0u8..PICKS).prop_map(|(fact_ref, field)| HeadDraw { fact_ref, field })
    }

    /// A valid `(plan, store)` pair: 1-, 2- or 3-level plans with seeks,
    /// constant and cross-loop residuals, and scalar/fact-ref projections, over a
    /// conforming store.
    pub fn arb_plan_and_store() -> impl Strategy<Value = PlanAndStore> {
        (
            1..=MAX_PREDICATES,
            prop::collection::vec(arb_predicate(), MAX_PREDICATES),
            prop::collection::vec(arb_predicate_facts(), MAX_PREDICATES),
            prop::collection::vec(arb_level(), 1..=MAX_LEVELS),
            prop::collection::vec(arb_head(), MAX_LEVELS),
        )
            .prop_map(|(npredicates, predicates, facts, levels, heads)| {
                resolve(npredicates, predicates, facts, levels, heads)
            })
    }

    /// An **interruption schedule**: whether to suspend after each row, cycled
    /// over however many rows a run turns out to produce (the count isn't known
    /// until it runs). Drawn independently of the `(plan, store)` pair so
    /// shrinking can simplify the schedule and the plan separately.
    pub fn arb_interruption_schedule() -> impl Strategy<Value = Vec<bool>> {
        prop::collection::vec(any::<bool>(), 1..=8)
    }

    /// Resolve a schedule against a run of `rows` rows: the 1-based row indices
    /// after which to suspend.
    pub fn cut_points(schedule: &[bool], rows: usize) -> BTreeSet<usize> {
        (1..=rows)
            .filter(|k| schedule[(k - 1) % schedule.len()])
            .collect()
    }
}
