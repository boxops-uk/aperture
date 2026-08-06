use std::fmt;

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
pub struct FactId(u64);

impl FactId {
    /// The raw eight bytes, for storing or comparing.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Wrap an id that is **already known to be valid** — decoded from a stored
    /// row that the decode boundary has checked, or handed back by a model store
    /// that got it from [`FactId::new`].
    ///
    /// The field is private so that [`FactId::new`]'s checks are the only way to
    /// *mint* an id: the tag has to fit and sequence 0 is reserved, which is what
    /// makes a zeroed eight bytes detectably not a fact
    /// ([I11](../../../docs/invariants.md#i11)). Named rather than a tuple
    /// constructor so the places that bypass those checks are greppable.
    #[must_use]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

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

/// Where a field lives inside a stored key: a **top-level field**, then one step
/// per record it is nested inside.
///
/// A stored key is the concatenation of the key type's top-level fields
/// ([chapter 3]) — so a flat key's field is reached by the leading `field` alone,
/// which is the **depth-1 fast path** the executor's field-offset cache holds.
/// `nested` is the walk *inside* a record-typed field, which no cache covers and
/// which re-derives its offsets per read.
///
/// The leading field is a separate component rather than the first element of one
/// slice, so an *empty* path — a plan naming no field at all — cannot be spelled.
///
/// [chapter 3]: ../../../docs/03-storage-model.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    field: usize,
    nested: Box<[usize]>,
}

impl FieldPath {
    /// A top-level field of the key — the flat case.
    #[must_use]
    pub fn field(field: usize) -> Self {
        Self {
            field,
            nested: Box::new([]),
        }
    }

    /// A field of the record at top-level field `field`, `nested` steps down.
    #[must_use]
    pub fn nested(field: usize, nested: impl Into<Box<[usize]>>) -> Self {
        Self {
            field,
            nested: nested.into(),
        }
    }

    /// This path, then one step further in — reading a field of a record field.
    #[must_use]
    pub fn then(&self, step: usize) -> Self {
        let mut nested = self.nested.to_vec();
        nested.push(step);
        Self::nested(self.field, nested)
    }

    /// The top-level key field this path starts at.
    #[must_use]
    pub fn field_idx(&self) -> usize {
        self.field
    }

    /// The steps inside that field; empty on the fast path.
    #[must_use]
    pub fn steps(&self) -> &[usize] {
        &self.nested
    }

    /// Whether this names a top-level field directly.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.nested.is_empty()
    }
}

impl fmt::Display for FieldPath {
    /// `1`, or `0.1` for a nested step — how a plan reads in a diagnostic or a
    /// test's rendering of it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.field)?;
        for step in self.nested.iter() {
            write!(f, ".{step}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum SeekKey {
    Prefix(Box<[u8]>),
    Composite(Box<[SeekKeyPart]>),
}

#[derive(Debug, Clone)]
pub enum SeekKeyPart {
    Bytes(Box<[u8]>),
    RegisterField {
        address: Address,
        path: FieldPath,
    },
    /// A **fact-typed** field, filled from the register's identity: the encoding of
    /// the row's [`FactId`], not of its key.
    ///
    /// That distinction is the whole point. A register holds the key bytes of the row
    /// it is bound to, and a field declared `Fact(p)` holds a reference to a row —
    /// so splicing the key where the reference belongs compares two different things
    /// and matches nothing, silently. The id is already in the register, so following
    /// a reference reads nothing from `entities` and [I6] stays structural.
    ///
    /// [I6]: ../../../docs/invariants.md#i6
    RegisterFactId(Address),
}

#[derive(Debug, Clone)]
pub struct Access {
    pub predicate_id: PredicateId,
    pub seek_key: SeekKey,
}

#[derive(Debug, Clone)]
pub enum ResidualOp {
    EqConst(Box<[u8]>),
    Prefix(Box<[u8]>),
    EqRegisterField {
        address: Address,
        path: FieldPath,
    },
    /// The [`SeekKeyPart::RegisterFactId`] compare, once the seek prefix has closed.
    EqRegisterFactId(Address),
}

#[derive(Debug, Clone)]
pub struct Residual {
    pub path: FieldPath,
    pub op: ResidualOp,
}

#[derive(Debug, Clone)]
pub struct Generator {
    pub access: Access,
    pub binds: Box<[Address]>,
    pub residuals: Box<[Residual]>,
}

#[derive(Debug, Clone)]
pub enum Project {
    Lit(Value),
    RegisterField {
        address: Address,
        path: FieldPath,
        ty: PredicateTy,
    },
    FactRef(Address),
    Value {
        address: Address,
        ty: PredicateTy,
    },
    Record(Box<[(Symbol, Project)]>),
}

/// The compiled query — the fixed contract between the front end and the
/// executor.
#[derive(Debug, Clone)]
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

    /// Open a scan of `lo..hi`, bounded to the predicate named by `lo`'s first
    /// [`PREDICATE_ID_SIZE`](crate::focus::schema::PREDICATE_ID_SIZE) bytes.
    ///
    /// Fallible, because opening genuinely can fail: a `lo` too short to name a
    /// predicate names nothing, and that is a fault in the *call*, not in a row.
    /// While this returned the iterator directly there was nowhere to say so, and
    /// each implementation invented an answer — one smuggled the error out as a
    /// first row, the others scanned across the predicate boundary and reported
    /// nothing.
    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Result<Self::Scan, ApertureError>;

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

    use super::{
        Access, FieldPath, Generator, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart,
    };
    use crate::focus::{
        fixtures::{compose, i64_field, interner_with, str_field},
        iter::Address,
        mem_store::MemStore,
        schema::{LocalInterner, PredicateId, PredicateTy},
        tuple::Value,
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
    ///
    /// `"a"` and `"ab"` are deliberately in a **prefix relationship**: the front
    /// end's generator writes string-prefix patterns, and a domain of
    /// mutually-distinct one-character strings would make every prefix match
    /// either exactly one value or all of them — never the interesting middle.
    const INTS: [i64; 4] = [0, 1, 2, 3];
    const STRS: [&str; 3] = ["a", "ab", "b"];

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
        pub fn of(pick: u8) -> Self {
            if pick.is_multiple_of(2) {
                FieldTy::Int
            } else {
                FieldTy::Str
            }
        }

        pub fn predicate_ty(self) -> PredicateTy {
            match self {
                FieldTy::Int => PredicateTy::Int,
                FieldTy::Str => PredicateTy::Str,
            }
        }
    }

    /// One key field's value, drawn from the tiny domain above.
    ///
    /// Public because [`flatten::proptest`](crate::focus::flatten::proptest)
    /// generates `(query, store)` pairs over the same vocabulary: the same field
    /// types, the same value domain and the same encoding, so a query generator and
    /// a plan generator cannot drift apart in what they consider a fact.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub enum FieldVal {
        Int(i64),
        Str(&'static str),
    }

    impl FieldVal {
        pub fn of(ty: FieldTy, pick: u8) -> Self {
            match ty {
                FieldTy::Int => FieldVal::Int(INTS[pick as usize % INTS.len()]),
                FieldTy::Str => FieldVal::Str(STRS[pick as usize % STRS.len()]),
            }
        }

        pub fn encode(&self) -> Vec<u8> {
            match self {
                FieldVal::Int(i) => i64_field(*i),
                FieldVal::Str(s) => str_field(s),
            }
        }

        /// This value as a projected row would carry it — what a model oracle
        /// compares the executor's output against.
        #[must_use]
        pub fn to_value(&self) -> Value {
            match self {
                FieldVal::Int(i) => Value::Int(*i),
                FieldVal::Str(s) => Value::Str((*s).to_owned()),
            }
        }

        /// This value as a **focus literal**, for writing the query that matches
        /// it. The string domain holds no character needing an escape, so
        /// quoting is all this has to do.
        #[must_use]
        pub fn source(&self) -> String {
            match self {
                FieldVal::Int(i) => i.to_string(),
                FieldVal::Str(s) => format!("{s:?}"),
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

        /// The spec's facts in insertion order: `(predicate, encoded key, sequence
        /// within that predicate)`.
        ///
        /// One deterministic order, walked by every store this spec seeds. That is
        /// what makes a rebuilt store identical — resume's integrity check compares
        /// a re-read row's `fact_id` against the saved one ([I4]) — and what makes
        /// a fjall store and a `MemStore` built from the same spec agree fact for
        /// fact, ids included, since the numbering matches what the real
        /// per-predicate allocator hands out ([I11]).
        ///
        /// [I4]: ../../../docs/invariants.md#i4
        /// [I11]: ../../../docs/invariants.md#i11
        pub fn facts(&self) -> impl Iterator<Item = (PredicateId, Vec<u8>, u64)> + '_ {
            self.facts.iter().enumerate().flat_map(|(predicate, keys)| {
                keys.iter().enumerate().map(move |(i, key)| {
                    (PredicateId(predicate as u32), encode_key(key), i as u64 + 1)
                })
            })
        }

        pub fn build_store(&self) -> MemStore {
            let mut store = MemStore::new();

            for (predicate, key, sequence) in self.facts() {
                store.insert(predicate, key, sequence);
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
                                    path: FieldPath::field(ref_field),
                                }]))
                            }
                        },
                    },
                    binds: Box::new([Address::new(level)]),
                    residuals: match &spec.residual {
                        None => Box::new([]),
                        Some(ResidualSpec::EqConst { field, val }) => Box::new([Residual {
                            path: FieldPath::field(*field),
                            op: ResidualOp::EqConst(val.encode().into_boxed_slice()),
                        }]),
                        Some(ResidualSpec::EqRegisterField {
                            field,
                            level: ref_level,
                            ref_field,
                        }) => Box::new([Residual {
                            path: FieldPath::field(*field),
                            op: ResidualOp::EqRegisterField {
                                address: Address::new(*ref_level),
                                path: FieldPath::field(*ref_field),
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
                            path: FieldPath::field(*field),
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
    /// self-delimiting (I2), so no lengths or separators are needed, and no record
    /// wrapper of its own ([chapter 3](../../../docs/03-storage-model.md)).
    pub fn encode_key(key: &[FieldVal]) -> Vec<u8> {
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
