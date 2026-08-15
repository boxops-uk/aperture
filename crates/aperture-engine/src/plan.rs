use std::fmt;

use aperture_encoding::tuple::Value;
use aperture_schema::schema::{PredicateId, PredicateTy, Symbol};

/// Which register a plan reads or binds — an index into the frame stack, named
/// here rather than in the executor because it is part of what a plan *says*.
///
/// The executor is what gives it a meaning at run time, but every producer of a
/// plan writes one, so a plan that could not name a register would not be a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address(pub(crate) usize);

impl Address {
    pub fn new(i: usize) -> Self {
        Self(i)
    }

    /// Which register this is, as an index into the plan's levels — a plan binds one
    /// register per level, so this also says which generator bound it.
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

impl fmt::Display for Address {
    /// A register index, written `r0`, `r1`, … — not a machine address. It used
    /// to render as a 16-digit hex value, so `Address(0)` reached a diagnostic as
    /// `0x0000000000000000`, which reads as a pointer.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
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

/// Where one level's rows come from, and the filters that apply to rows out of
/// **this** source.
///
/// Residuals belong here rather than on the [`Level`] because a residual is a
/// [`FieldPath`] into a row, and two sources of one level are two different key
/// layouts — a path that names a field of one names different bytes, or no bytes
/// at all, in the other.
#[derive(Debug, Clone)]
pub enum Source {
    Seek {
        access: Access,
        residuals: Box<[Residual]>,
    },
    /// **The fact a reference names** — one row, reached by id rather than by
    /// scanning for it.
    ///
    /// `reference` is a register bound at an outer level and `path` a fact-typed
    /// field of its key, so the id is already in hand: this is the second lookup
    /// that [`SeekKeyPart::RegisterFactId`] deliberately avoids, and the reason it
    /// could avoid it is that *following* a reference compares ids while *reading
    /// through* one needs the other fact's key bytes, which live only in
    /// `entities`.
    ///
    /// A source rather than a step, because a point read is a relation of at most
    /// one row and the machine's job over it is a scan's exactly: open, drain, move
    /// on. That is what keeps `enumerate` unchanged
    /// ([the query-surface note](../../../docs/query-surface.md)).
    ///
    /// `predicate_id` is the field's **declared** referent, and is checked against
    /// the id actually stored. It is not redundant with [`FactId::predicate`]: every
    /// `path` in this source's residuals — and every projection off the register it
    /// binds — was compiled against the declared key layout, so a reference that
    /// names another predicate would decode a different type's bytes at that offset
    /// and answer, silently, with whatever was there.
    Fetch {
        reference: Address,
        path: FieldPath,
        predicate_id: PredicateId,
        residuals: Box<[Residual]>,
    },
}

impl Source {
    /// The residuals rows out of this source are filtered by.
    #[must_use]
    pub fn residuals(&self) -> &[Residual] {
        match self {
            Source::Seek { residuals, .. } | Source::Fetch { residuals, .. } => residuals,
        }
    }

    /// The residuals, to add one to.
    pub fn residuals_mut(&mut self) -> &mut Box<[Residual]> {
        match self {
            Source::Seek { residuals, .. } | Source::Fetch { residuals, .. } => residuals,
        }
    }

    /// How this source's scan is narrowed — `None` for a source that does not
    /// scan.
    #[must_use]
    pub fn seek_key(&self) -> Option<&SeekKey> {
        match self {
            Source::Seek { access, .. } => Some(&access.seek_key),
            Source::Fetch { .. } => None,
        }
    }

    /// The predicate this source draws from, when it draws from exactly one.
    #[must_use]
    pub fn predicate_id(&self) -> PredicateId {
        match self {
            Source::Seek { access, .. } => access.predicate_id,
            Source::Fetch { predicate_id, .. } => *predicate_id,
        }
    }
}

/// One **loop level**: the rows it iterates, and the registers it binds them to.
///
/// `sources` are the level's alternatives, tried in order and concatenated — so
/// the count is the construct:
///
/// | sources | what it is |
/// |---|---|
/// | 0 | the **empty relation** — the level is exhausted the moment it is entered |
/// | 1 | an ordinary scan, which is every plan focus compiles today |
/// | N | a **disjunction**, one branch per source |
///
/// They are one node rather than three because the machine's job is identical in
/// all three: open a source, drain it, move to the next, and back up when there is
/// no next. Counting is the only thing that differs, which is why `never` needs no
/// arm of its own and no case in [`enumerate`](crate::iter::Executor::enumerate).
///
/// `binds` is the level's, not a source's: every alternative binds the same
/// variables, which is what makes a register mean one thing whichever branch
/// filled it (see [the query-surface note](../../../docs/query-surface.md)).
#[derive(Debug, Clone)]
pub struct Level {
    pub sources: Box<[Source]>,
    pub binds: Box<[Address]>,
}

impl Level {
    /// A level with a single [`Source::Seek`] — the shape every plan had before
    /// a level could have alternatives, and still the shape of any level flatten
    /// emits.
    #[must_use]
    pub fn seek(access: Access, binds: Box<[Address]>, residuals: Box<[Residual]>) -> Self {
        Self {
            sources: Box::new([Source::Seek { access, residuals }]),
            binds,
        }
    }

    /// A level with a single [`Source::Fetch`] — the fact a reference names,
    /// bound to a register of its own so that everything downstream reads it as
    /// an ordinary row.
    #[must_use]
    pub fn fetch(
        reference: Address,
        path: FieldPath,
        predicate_id: PredicateId,
        binds: Box<[Address]>,
        residuals: Box<[Residual]>,
    ) -> Self {
        Self {
            sources: Box::new([Source::Fetch {
                reference,
                path,
                predicate_id,
                residuals,
            }]),
            binds,
        }
    }

    /// A level that produces nothing — zero sources.
    ///
    /// Not reachable from focus text yet (`never` is [Phase
    /// 6b](../../../PLAN.md)); the machine handles it because it falls out of
    /// counting, and it is guarded so that it stays that way.
    #[must_use]
    pub fn empty(binds: Box<[Address]>) -> Self {
        Self {
            sources: Box::new([]),
            binds,
        }
    }

    /// This level's only source, when it has exactly one.
    ///
    /// Every level flatten emits is single-source, so this is what a caller
    /// reasoning about *the* seek of a level means — and `None` is the honest
    /// answer for a disjunction, where there is no such thing.
    #[must_use]
    pub fn sole_source(&self) -> Option<&Source> {
        match &*self.sources {
            [source] => Some(source),
            _ => None,
        }
    }

    /// The predicate this level's rows come from, when every source agrees.
    ///
    /// `None` for the empty relation, and for a disjunction spanning predicates —
    /// where there is no single answer, and a caller that wants to name a field
    /// has to say which source it means.
    #[must_use]
    pub fn predicate_id(&self) -> Option<PredicateId> {
        let first = self.sources.first()?.predicate_id();

        self.sources
            .iter()
            .all(|source| source.predicate_id() == first)
            .then_some(first)
    }
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
    /// A **derived bind's** output, read out of its value slot.
    ///
    /// Distinct from [`Project::Lit`], which carries a constant the head owns
    /// outright. A literal derived bind could be folded into one and the query
    /// would answer the same — deliberately not done, because folding is what would
    /// leave the recompute-on-restore path with no coverage until the first
    /// non-constant `Computed` arm exists to need it.
    Computed(Address),
    Record(Box<[(Symbol, Project)]>),
}

/// What a **derived bind** computes: a pure expression over already-bound slots.
///
/// The vocabulary is deliberately one arm wide. Without primitives (an
/// [open decision](../../docs/open-decisions.md)) a derived bind can only produce
/// a constant, and inventing arms for arithmetic here would be speculating about a
/// decision that has not been taken. What the enum *is* for is the shape of the
/// seam: every arm must be a pure function of the fact slots, with no iteration and
/// no hidden state, because that purity is what lets a [`Cursor`] save only
/// generator positions and recompute the rest
/// ([chapter 7](../../docs/07-compilation.md#derived-facts)).
///
/// [`Cursor`]: crate::iter::Cursor
#[derive(Debug, Clone)]
pub enum Computed {
    Lit(Value),
}

/// A variable bound to a computed value rather than to a row — chapter 7's
/// *derived bind*.
///
/// It is **not a loop level**: it produces exactly one value, `enumerate` does not
/// iterate it, and the [`Cursor`](crate::iter::Cursor) does not store it.
#[derive(Debug, Clone)]
pub struct DerivedBind {
    pub bind: Address,
    pub value: Computed,
}

/// A **filter**: a step that binds nothing, produces no row of its own, and either
/// passes the row standing when it runs or drops it.
///
/// One arm wide on purpose, exactly as [`Computed`] is. What the type is for is the
/// shape of the seam — a test is a *predicate on the bindings*, so it takes no
/// register, contributes nothing to a [`Cursor`](crate::iter::Cursor), and is
/// re-decided on restore rather than replayed. The positive form (`Exists`, for a
/// subquery whose bindings must not escape) is the additive arm when something needs
/// one; inventing it now would be a guess about a construct that does not exist yet.
#[derive(Debug, Clone)]
pub enum Test {
    /// **Negation** — the row survives iff *no* source produces a row.
    ///
    /// The sources are the negated statement's alternatives, so the count means what
    /// it means for a [`Level`]: one is `!test.Bar {…}`, several is a negated
    /// disjunction, and **none is the negation of the empty relation**, which every
    /// row passes. That last one needs no arm of its own for the same reason `never`
    /// needed none — "no source produced a row" is already true of no sources.
    ///
    /// Each source is drained only until its **first** row: the question is whether a
    /// witness exists, not how many there are. So a negation costs at most one
    /// matching row per row the level above it produces, and reads only `keys`
    /// ([I6](../../../docs/invariants.md#i6) is about values, and a probe fetches
    /// none).
    Absent(Box<[Source]>),
}

/// One position in a plan's body: a level to iterate, a value to compute, or a
/// filter to pass.
///
/// A single ordered sequence, because `reorder` produces a single order — holding
/// the kinds in separate collections joined by an index would mean two sources
/// of truth for one ordering, with nothing to say which wins.
///
/// Only [`Level`] iterates, and that is the distinction the whole machine is built
/// around: [`Plan::levels`] counts loops, `body.len()` counts steps, and a register
/// address counts *levels*, because a derive and a test bind no row.
#[derive(Debug, Clone)]
pub enum Step {
    Level(Level),
    Derive(DerivedBind),
    Test(Test),
}

impl Step {
    /// A body of levels only — every plan's shape before derived binds, and still
    /// the shape of any query without one.
    #[must_use]
    pub fn levels<const N: usize>(levels: [Level; N]) -> Box<[Step]> {
        Box::new(levels.map(Step::Level))
    }

    #[must_use]
    pub fn is_level(&self) -> bool {
        matches!(self, Step::Level(_))
    }
}

/// The compiled query — the fixed contract between the front end and the
/// executor.
#[derive(Debug, Clone)]
pub struct Plan {
    pub nvars: usize,
    pub body: Box<[Step]>,
    pub head: Project,
}

impl Plan {
    /// How many of this plan's steps are **loop levels**.
    ///
    /// Distinct from `body.len()`, which counts steps, and the distinction is
    /// load-bearing: a [`Cursor`](crate::iter::Cursor) holds one row per
    /// *level*, and resume replays it against the levels in order. `body.len()`
    /// used to mean both, so every site that wants one or the other now has to
    /// name which.
    #[must_use]
    pub fn levels(&self) -> usize {
        self.body.iter().filter(|step| step.is_level()).count()
    }

    /// This plan's identity, for a resume cursor to carry
    /// ([`PlanFingerprint`], [chapter 5](../../../docs/05-resume.md)).
    ///
    /// Recomputed on demand rather than cached in the struct: a `Plan` is public
    /// and its fields are `pub`, so a cached value would be a second source of
    /// truth that any construction site could leave stale — and it is computed
    /// twice per query at most, once at suspend and once at resume.
    #[must_use]
    pub fn fingerprint(&self) -> PlanFingerprint {
        let mut fingerprint = Fingerprint::new();
        fingerprint.plan(self);
        PlanFingerprint(fingerprint.0)
    }

    /// The `n`th **loop level**, skipping derive steps.
    ///
    /// The counterpart to [`Plan::levels`], and the accessor anything reasoning
    /// about join order wants: `body[n]` is the `n`th *step*, which is a different
    /// thing as soon as a plan derives anything.
    #[must_use]
    pub fn level(&self, n: usize) -> Option<&Level> {
        self.body
            .iter()
            .filter_map(|step| match step {
                Step::Level(level) => Some(level),
                Step::Derive(_) | Step::Test(_) => None,
            })
            .nth(n)
    }
}

/// A **plan's identity**, as a resume cursor carries it
/// ([chapter 5](../../../docs/05-resume.md)).
///
/// A cursor's entries are paired with the plan's levels *by order*, and until this
/// existed the only thing checked before that pairing was how many there were — so
/// two plans of the same shape over overlapping predicates accepted each other's
/// cursors, and the per-level `fact_id` check was all that stood between that and a
/// wrong answer. It passes whenever the saved key exists in the other plan's scan
/// too.
///
/// Displayed as hex, and never compared for order: it is 64 bits of FNV-1a, so
/// "different" is certain and "same" is a 2⁻⁶⁴ bet — which is why the exact
/// level-count check is kept rather than folded into this one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PlanFingerprint(u64);

impl fmt::Debug for PlanFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plan {:#018x}", self.0)
    }
}

/// FNV-1a over a plan's structure, written out explicitly rather than derived.
///
/// **Stability is the whole requirement**, and it is why this is not a
/// `Hash` impl: `DefaultHasher` is documented as free to change between Rust
/// releases, and a fingerprint that changes under the engine rejects legitimate
/// resumes — strictly worse than the hole it closes. FNV-1a is fixed forever and is
/// what Glean fingerprints its continuations with.
struct Fingerprint(u64);

impl Fingerprint {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    fn byte(&mut self, b: u8) {
        self.0 ^= u64::from(b);
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }

    fn int(&mut self, n: u64) {
        for b in n.to_be_bytes() {
            self.byte(b);
        }
    }

    fn len(&mut self, n: usize) {
        self.int(n as u64);
    }

    /// Length first, then content — so that two adjacent variable-length fields
    /// cannot be re-split into a different pair that hashes the same.
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        for &x in b {
            self.byte(x);
        }
    }

    fn address(&mut self, a: Address) {
        self.len(a.index());
    }

    fn path(&mut self, p: &FieldPath) {
        self.len(p.field_idx());
        self.len(p.steps().len());
        for &step in p.steps() {
            self.len(step);
        }
    }

    /// A type's **shape**: constructors and arity, with the field names skipped —
    /// see [`Fingerprint::project`] for why an interned name cannot be hashed.
    fn ty(&mut self, ty: &PredicateTy) {
        match ty {
            PredicateTy::Int => self.byte(0),
            PredicateTy::Str => self.byte(1),
            PredicateTy::Fact(p) => {
                self.byte(2);
                self.int(u64::from(p.0));
            }
            PredicateTy::Record(fields) => {
                self.byte(3);
                self.len(fields.len());
                for (_name, field) in fields.iter() {
                    self.ty(field);
                }
            }
        }
    }

    fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.byte(0),
            Value::Int(i) => {
                self.byte(1);
                self.int(*i as u64);
            }
            Value::Str(s) => {
                self.byte(2);
                self.bytes(s.as_bytes());
            }
            Value::FactRef(id) => {
                self.byte(3);
                self.int(id.raw());
            }
            Value::Record(fields) => {
                self.byte(4);
                self.len(fields.len());
                for (name, field) in fields.iter() {
                    // A literal's field names are owned strings, not interned
                    // ones, so these *are* hashable and are part of the answer.
                    self.bytes(name.as_bytes());
                    self.value(field);
                }
            }
        }
    }

    /// The head.
    ///
    /// **Record field names are deliberately not hashed.** A [`Symbol`] is an index
    /// into a per-query interner, so the same query compiled twice — in another
    /// process, against a fresh interner — names its fields with different numbers,
    /// and hashing them would make the fingerprint unstable exactly where a wire
    /// cursor needs it most. The consequence is stated rather than hidden: two plans
    /// differing only in what their head fields are *called* share a fingerprint.
    /// Neither positions a scan, which is what a cursor entry is paired with.
    fn project(&mut self, p: &Project) {
        match p {
            Project::Lit(v) => {
                self.byte(0);
                self.value(v);
            }
            Project::RegisterField { address, path, ty } => {
                self.byte(1);
                self.address(*address);
                self.path(path);
                self.ty(ty);
            }
            Project::FactRef(address) => {
                self.byte(2);
                self.address(*address);
            }
            Project::Value { address, ty } => {
                self.byte(3);
                self.address(*address);
                self.ty(ty);
            }
            Project::Computed(address) => {
                self.byte(4);
                self.address(*address);
            }
            Project::Record(fields) => {
                self.byte(5);
                self.len(fields.len());
                for (_name, field) in fields.iter() {
                    self.project(field);
                }
            }
        }
    }

    fn seek_key(&mut self, key: &SeekKey) {
        match key {
            SeekKey::Prefix(bytes) => {
                self.byte(0);
                self.bytes(bytes);
            }
            SeekKey::Composite(parts) => {
                self.byte(1);
                self.len(parts.len());
                for part in parts.iter() {
                    match part {
                        SeekKeyPart::Bytes(bytes) => {
                            self.byte(0);
                            self.bytes(bytes);
                        }
                        SeekKeyPart::RegisterField { address, path } => {
                            self.byte(1);
                            self.address(*address);
                            self.path(path);
                        }
                        SeekKeyPart::RegisterFactId(address) => {
                            self.byte(2);
                            self.address(*address);
                        }
                    }
                }
            }
        }
    }

    fn residuals(&mut self, residuals: &[Residual]) {
        self.len(residuals.len());
        for residual in residuals {
            self.path(&residual.path);
            match &residual.op {
                ResidualOp::EqConst(bytes) => {
                    self.byte(0);
                    self.bytes(bytes);
                }
                ResidualOp::Prefix(bytes) => {
                    self.byte(1);
                    self.bytes(bytes);
                }
                ResidualOp::EqRegisterField { address, path } => {
                    self.byte(2);
                    self.address(*address);
                    self.path(path);
                }
                ResidualOp::EqRegisterFactId(address) => {
                    self.byte(3);
                    self.address(*address);
                }
            }
        }
    }

    fn source(&mut self, source: &Source) {
        match source {
            Source::Seek { access, residuals } => {
                self.byte(0);
                self.int(u64::from(access.predicate_id.0));
                self.seek_key(&access.seek_key);
                self.residuals(residuals);
            }
            Source::Fetch {
                reference,
                path,
                predicate_id,
                residuals,
            } => {
                self.byte(1);
                self.address(*reference);
                self.path(path);
                self.int(u64::from(predicate_id.0));
                self.residuals(residuals);
            }
        }
    }

    fn plan(&mut self, plan: &Plan) {
        self.len(plan.nvars);
        self.len(plan.body.len());

        for step in plan.body.iter() {
            match step {
                Step::Level(level) => {
                    self.byte(0);
                    self.len(level.sources.len());
                    for source in level.sources.iter() {
                        self.source(source);
                    }
                    self.len(level.binds.len());
                    for bind in level.binds.iter() {
                        self.address(*bind);
                    }
                }
                Step::Derive(derived) => {
                    self.byte(1);
                    self.address(derived.bind);
                    match &derived.value {
                        Computed::Lit(v) => {
                            self.byte(0);
                            self.value(v);
                        }
                    }
                }
                Step::Test(test) => {
                    self.byte(2);
                    match test {
                        Test::Absent(sources) => {
                            self.byte(0);
                            self.len(sources.len());
                            for source in sources.iter() {
                                self.source(source);
                            }
                        }
                    }
                }
            }
        }

        self.project(&plan.head);
    }
}

#[cfg(test)]
mod tests {
    use super::{proptest::arb_plan_and_store, *};
    use crate::fixtures::i64_field;
    use ::proptest::prelude::*;
    use aperture_schema::schema::PredicateId;

    /// A base plan touching most of what a fingerprint has to see: a seek whose
    /// prefix splices a register field, a residual, a second alternative, a fetch
    /// through a reference, a derived bind, and a record head.
    fn base() -> Plan {
        Plan {
            nvars: 3,
            body: Box::new([
                Step::Level(Level::seek(
                    Access {
                        predicate_id: PredicateId(1),
                        seek_key: SeekKey::Prefix(Box::new([1, 2])),
                    },
                    Box::new([Address::new(0)]),
                    Box::new([Residual {
                        path: FieldPath::field(0),
                        op: ResidualOp::EqConst(i64_field(7).into_boxed_slice()),
                    }]),
                )),
                Step::Level(Level::fetch(
                    Address::new(0),
                    FieldPath::field(1),
                    PredicateId(2),
                    Box::new([Address::new(1)]),
                    Box::new([]),
                )),
                Step::Derive(DerivedBind {
                    bind: Address::new(2),
                    value: Computed::Lit(Value::Int(42)),
                }),
            ]),
            head: Project::Record(Box::new([])),
        }
    }

    /// **Every part of a plan reaches the fingerprint.** A hand-written structural
    /// hash fails silently in one direction only — a field left out of the walk
    /// makes two different plans agree, and a cursor from one then resumes into the
    /// other, which is the exact failure the fingerprint exists to stop.
    ///
    /// So the guard is a table of single-element mutations, all of which must land
    /// on distinct values. It is also the checklist to extend when a plan grows an
    /// arm: a new `Source`, `ResidualOp` or `Project` variant that nothing here
    /// distinguishes is one the walk may have forgotten.
    #[test]
    fn every_part_of_a_plan_reaches_its_fingerprint() {
        let with_body = |f: &dyn Fn(&mut Vec<Step>)| {
            let mut plan = base();
            let mut body = plan.body.into_vec();
            f(&mut body);
            plan.body = body.into_boxed_slice();
            plan
        };

        let level = |body: &Vec<Step>, n: usize| match &body[n] {
            Step::Level(level) => level.clone(),
            Step::Derive(_) | Step::Test(_) => panic!("step {n} is a level"),
        };

        let mutations: Vec<(&str, Plan)> = vec![
            ("the base plan", base()),
            ("one register fewer", Plan { nvars: 2, ..base() }),
            (
                "a different predicate",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    l.sources = Box::new([Source::Seek {
                        access: Access {
                            predicate_id: PredicateId(9),
                            seek_key: SeekKey::Prefix(Box::new([1, 2])),
                        },
                        residuals: l.sources[0].residuals().to_vec().into_boxed_slice(),
                    }]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "a different seek prefix",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    l.sources = Box::new([Source::Seek {
                        access: Access {
                            predicate_id: PredicateId(1),
                            seek_key: SeekKey::Prefix(Box::new([1, 3])),
                        },
                        residuals: l.sources[0].residuals().to_vec().into_boxed_slice(),
                    }]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "a spliced register rather than a constant prefix",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    l.sources = Box::new([Source::Seek {
                        access: Access {
                            predicate_id: PredicateId(1),
                            seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                                address: Address::new(0),
                                path: FieldPath::field(0),
                            }])),
                        },
                        residuals: Box::new([]),
                    }]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "a different residual constant",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    *l.sources[0].residuals_mut() = Box::new([Residual {
                        path: FieldPath::field(0),
                        op: ResidualOp::EqConst(i64_field(8).into_boxed_slice()),
                    }]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "the same constant at a different field",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    *l.sources[0].residuals_mut() = Box::new([Residual {
                        path: FieldPath::field(1),
                        op: ResidualOp::EqConst(i64_field(7).into_boxed_slice()),
                    }]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "no residual at all",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    *l.sources[0].residuals_mut() = Box::new([]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "a second alternative",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    let sole = l.sources[0].clone();
                    l.sources = Box::new([sole.clone(), sole]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "a different bind",
                with_body(&|body| {
                    let mut l = level(body, 0);
                    l.binds = Box::new([Address::new(2)]);
                    body[0] = Step::Level(l);
                }),
            ),
            (
                "the fetch reading a different field",
                with_body(&|body| {
                    body[1] = Step::Level(Level::fetch(
                        Address::new(0),
                        FieldPath::field(0),
                        PredicateId(2),
                        Box::new([Address::new(1)]),
                        Box::new([]),
                    ));
                }),
            ),
            (
                "the fetch naming a different referent",
                with_body(&|body| {
                    body[1] = Step::Level(Level::fetch(
                        Address::new(0),
                        FieldPath::field(1),
                        PredicateId(3),
                        Box::new([Address::new(1)]),
                        Box::new([]),
                    ));
                }),
            ),
            (
                "a different derived value",
                with_body(&|body| {
                    body[2] = Step::Derive(DerivedBind {
                        bind: Address::new(2),
                        value: Computed::Lit(Value::Int(43)),
                    });
                }),
            ),
            (
                "the steps in the other order",
                with_body(&|body| body.swap(1, 2)),
            ),
            (
                "a different head",
                Plan {
                    head: Project::Computed(Address::new(2)),
                    ..base()
                },
            ),
            (
                "a head reading a different register",
                Plan {
                    head: Project::FactRef(Address::new(1)),
                    ..base()
                },
            ),
        ];

        let mut seen: Vec<(&str, PlanFingerprint)> = Vec::new();
        for (name, plan) in &mutations {
            let fingerprint = plan.fingerprint();

            if let Some((other, _)) = seen.iter().find(|(_, f)| *f == fingerprint) {
                panic!(
                    "`{name}` and `{other}` fingerprint the same ({fingerprint:?}) — \
                     the walk does not distinguish them"
                );
            }

            seen.push((name, fingerprint));
        }
    }

    /// A fingerprint is a **pure function of the plan**: computed twice, it agrees.
    #[test]
    fn a_fingerprint_is_a_function_of_the_plan() {
        assert_eq!(base().fingerprint(), base().fingerprint());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// **A fingerprint does not depend on the interner**, which is what makes it
        /// usable on a cursor at all: the token outlives the process, and the same
        /// query compiled again gets a fresh interner where every name has a
        /// different number. Hashing those numbers would make a legitimate resume
        /// fail — strictly worse than the hole the fingerprint closes.
        ///
        /// The padding is the whole test: interning two names first shifts every
        /// symbol the plan carries, and nothing about the query has changed.
        #[test]
        fn a_fingerprint_does_not_depend_on_the_interner(spec in arb_plan_and_store()) {
            use crate::fixtures::interner_with;

            let names = ["r0", "r1", "r2"];
            let padded: Vec<&str> = ["pad_a", "pad_b"].into_iter().chain(names).collect();

            prop_assert_eq!(
                spec.build_plan(&interner_with(&names)).fingerprint(),
                spec.build_plan(&interner_with(&padded)).fingerprint(),
            );
        }
    }
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
        Access, Address, FieldPath, Level, Plan, Project, Residual, ResidualOp, SeekKey,
        SeekKeyPart, Source, Step,
    };
    use crate::fixtures::{compose, i64_field, interner_with, str_field};
    use aperture_encoding::tuple::Value;
    use aperture_schema::schema::{LocalInterner, PredicateId, PredicateTy};
    use aperture_store::mem_store::MemStore;

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
    /// Public because [`flatten::proptest`](crate::flatten::proptest)
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
        /// One entry per [`Source`], each holding that source's residual — so the
        /// length is the construct: one is a scan, more is a **disjunction**.
        ///
        /// Every alternative reads the same predicate and the same seek, and
        /// differs only in what it filters. That is deliberate rather than a
        /// simplification: sources over *different* predicates would bind one
        /// register to two key layouts, which needs the exported-value rule the
        /// language cannot ask for yet ([the query-surface note]).
        ///
        /// [the query-surface note]: ../../../docs/query-surface.md
        sources: Vec<Option<ResidualSpec>>,
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
                .map(|(level, spec)| {
                    let access = Access {
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
                    };

                    let sources = spec
                        .sources
                        .iter()
                        .map(|residual| Source::Seek {
                            access: access.clone(),
                            residuals: match residual {
                                None => Box::new([]) as Box<[Residual]>,
                                Some(ResidualSpec::EqConst { field, val }) => {
                                    Box::new([Residual {
                                        path: FieldPath::field(*field),
                                        op: ResidualOp::EqConst(val.encode().into_boxed_slice()),
                                    }])
                                }
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
                        .collect();

                    Level {
                        sources,
                        binds: Box::new([Address::new(level)]),
                    }
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
                // Every level the generator draws is a scan; a derive step is a
                // shape it does not yet reach, and the census says so.
                body: body.into_iter().map(Step::Level).collect(),
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
        /// Whether this level gets a second [`Source`] — see [`LevelSpec::sources`].
        alternative: u8,
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

            // A second alternative on some levels, filtering differently, so the
            // battery sees a level whose rows come from more than one source —
            // and, at a cut point, a suspend taken while the *second* one is live.
            let mut sources = vec![residual];

            if draw.alternative.is_multiple_of(3) {
                sources.push(Some(ResidualSpec::EqConst {
                    field,
                    val: constant_for(
                        &facts[predicate],
                        field,
                        fields[field],
                        draw.constant.wrapping_add(1),
                    ),
                }));
            }

            resolved.push(LevelSpec {
                predicate,
                seek,
                sources,
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
            0u8..PICKS,
        )
            .prop_map(
                |(predicate, seek, residual, field, reference, constant, alternative)| LevelDraw {
                    predicate,
                    seek,
                    residual,
                    field,
                    reference,
                    constant,
                    alternative,
                },
            )
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
