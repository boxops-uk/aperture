use std::{fmt, ops::Range};

use byteview::ByteView;
use tinyvec::ArrayVec;
use tokio_util::sync::CancellationToken;

use crate::focus::{
    error::{ApertureError, StoreCodecError, StoreError},
    plan::{
        Computed, FactId, FactStore, FieldPath, Plan, Project, Residual, ResidualOp, SeekKey,
        SeekKeyPart, Source, Step,
    },
    schema::{LocalInterner, PREDICATE_ID_SIZE},
    tuple::{
        MARK_ESCAPE, MARK_RECORD, MARK_TERM, Value, decode_typed, fact_ref_bytes, skip, strinc,
    },
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    pub fact_id: FactId,
    pub bytes: ByteView,
}

impl Register {
    pub fn key(&self) -> ByteView {
        self.bytes.slice(PREDICATE_ID_SIZE..)
    }

    pub fn to_detached(&self) -> Register {
        Register {
            fact_id: self.fact_id,
            bytes: self.bytes.to_detached(),
        }
    }
}

/// What a register holds: a **stored row**, or a **computed value**.
///
/// The fact case is the original register and the one
/// [I5](../../docs/invariants.md#i5) is about — the whole row, fields decoded
/// lazily at a read site. The value case is a *derived bind*'s output
/// ([chapter 7](../../docs/07-compilation.md#derived-facts)): a pure function of
/// the fact slots, which is exactly why the [`Cursor`] does not store one and a
/// resume recomputes it instead.
///
/// The two are kept apart at the type level rather than unified behind "some
/// bytes" because splicing a value where an id belongs — or the reverse — compares
/// the wrong encoding and quietly matches nothing, which is the same class of
/// silent fault the `FactRef` marker split guards against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Fact(Register),
    Value(Value),
}

pub struct MachineState {
    pub registers: Box<[Option<Slot>]>,
}

impl MachineState {
    pub fn new(nvars: usize) -> Self {
        Self {
            registers: vec![None; nvars].into_boxed_slice(),
        }
    }

    /// The row bound to `address`.
    ///
    /// Reading a *value* slot here is a malformed plan, not a data condition: the
    /// compiler knows which addresses derived binds write, so a seek splicing one
    /// as a row is a compiler fault. It still reports rather than panics, because
    /// a plan can also arrive off the wire.
    pub fn fact(&self, address: Address) -> Result<&Register, ApertureError> {
        match self.get(address)? {
            Slot::Fact(register) => Ok(register),
            Slot::Value(_) => Err(ApertureError::SlotKindMismatch {
                address,
                wanted: "a fact row",
                held: "a computed value",
            }),
        }
    }

    /// The computed value bound to `address`, for a plan step reading a derived
    /// bind's output.
    pub fn value(&self, address: Address) -> Result<&Value, ApertureError> {
        match self.get(address)? {
            Slot::Value(value) => Ok(value),
            Slot::Fact(_) => Err(ApertureError::SlotKindMismatch {
                address,
                wanted: "a computed value",
                held: "a fact row",
            }),
        }
    }

    fn get(&self, address: Address) -> Result<&Slot, ApertureError> {
        self.registers
            .get(address.0)
            .ok_or(ApertureError::AddressOutOfBounds(address))?
            .as_ref()
            .ok_or(ApertureError::UseBeforeBind(address))
    }
}

/// Skip-counting probe for the D2 guard (`exec::projection_walks_each_field_once`).
///
/// Every `skip` performed to fill a field-offset cache bumps a thread-local
/// counter; the guard asserts that projecting k fields of one row costs k skips
/// rather than k(k+1)/2. Same shape as `tuple::decode_probe`. See
/// `docs/testing.md`.
#[cfg(any(test, feature = "proptest"))]
pub mod skip_probe {
    use std::cell::Cell;

    thread_local! {
        static SKIPS: Cell<u64> = const { Cell::new(0) };
    }

    /// Reset the skip counter to zero.
    pub fn reset() {
        SKIPS.with(|c| c.set(0));
    }

    /// Rows-worth of `skip` calls since the last [`reset`].
    pub fn count() -> u64 {
        SKIPS.with(Cell::get)
    }

    pub(crate) fn bump() {
        SKIPS.with(|c| c.set(c.get() + 1));
    }
}

const FIELD_OFFSETS_CAPACITY: usize = 16;

/// Where each leading key field of **one specific row** ends.
///
/// `ends[k]` is the offset one past field `k`, so field `k` spans
/// `ends[k - 1]..ends[k]`. Filled lazily, left to right — the encoding is
/// self-delimiting ([I2](../../docs/invariants.md#i2)), so finding field `k`
/// means skipping the `k` before it, and caching the boundaries is what stops a
/// seek splice and a residual on the same register re-walking the row.
///
/// # The reuse invariant
///
/// **The offsets describe the row they were filled from, and nothing else.** A
/// cache read against a *different* row silently truncates or overruns the field
/// — a wrong seek prefix (wrong join results) or an out-of-range slice. Reuse is
/// therefore sound only while the row is fixed, which rests on three links:
///
/// 1. Caches are indexed by **register address** — the frame's for seek splices
///    and residuals, the executor's for projection — so each one only ever
///    describes the row held by that one register.
/// 2. A generator only names registers bound at **strictly outer** levels, so
///    none of them can change while its own level is open.
/// 3. Every cache is cleared when the row beneath it may have moved:
///    [`StackFrame::open`] for the frame's, since a level is re-opened whenever an
///    outer level advances, and [`Row::to_value`] for projection's, once a row.
///
/// Link 2 is a property of the *plan*, which the executor does not verify, and
/// link 3 was once missing — the regression is
/// `seek_splice_rereads_field_when_outer_row_width_changes`. So the chain is also
/// checked mechanically: in debug builds a cache remembers the row it was filled
/// from and [`FieldOffsets::get`] asserts every later read presents the same one.
/// That turns every executor test, including the generated resume battery, into a
/// check of this invariant. The witness costs nothing in release, and nothing on
/// the hot path either way — a `ByteView` clone is a refcount bump
/// ([I9](../../docs/invariants.md#i9)).
#[derive(Debug, Clone)]
pub struct FieldOffsets {
    ends: ArrayVec<[usize; FIELD_OFFSETS_CAPACITY]>,
    /// The row the offsets were derived from; `None` until the first fill. Debug
    /// builds only — this is the witness for the reuse invariant above, not state
    /// the cache needs to work.
    #[cfg(debug_assertions)]
    row: Option<ByteView>,
}

impl Default for FieldOffsets {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldOffsets {
    pub fn new() -> Self {
        Self {
            ends: ArrayVec::new(),
            #[cfg(debug_assertions)]
            row: None,
        }
    }

    /// Drop every cached offset. Called when the row a cache describes may have
    /// changed — which is what makes reusing one safe.
    pub fn clear(&mut self) {
        self.ends.clear();
        #[cfg(debug_assertions)]
        {
            self.row = None;
        }
    }

    /// The span of field `idx` within `key`, skipping only as far as it must.
    ///
    /// `key` must be the same row every time until [`clear`](Self::clear) — see
    /// the type's reuse invariant.
    pub fn get(&mut self, key: &ByteView, idx: usize) -> Result<Range<usize>, StoreCodecError> {
        self.witness_row(key);

        if let Some(&end) = self.ends.get(idx) {
            return Ok(if idx == 0 {
                0..end
            } else {
                self.ends[idx - 1]..end
            });
        }
        let mut i = self.ends.len();
        let mut start = if i == 0 { 0 } else { self.ends[i - 1] };
        loop {
            #[cfg(any(test, feature = "proptest"))]
            skip_probe::bump();

            let end = skip(key, start, false)?;
            if i < FIELD_OFFSETS_CAPACITY {
                self.ends.push(end);
            }
            if i == idx {
                return Ok(start..end);
            }
            i += 1;
            start = end;
        }
    }

    /// Record the row on first fill, and check every later read against it.
    ///
    /// Compares by *content*: two registers holding equal bytes yield equal
    /// offsets, so equal bytes are exactly the right notion of "the same row".
    #[cfg(debug_assertions)]
    fn witness_row(&mut self, key: &ByteView) {
        match &self.row {
            None => self.row = Some(key.clone()),
            Some(filled) => assert!(
                filled == key,
                "field-offset cache reused against a different row: filled from \
                 {:02x?}, now read against {:02x?}. A cache must be cleared whenever \
                 the row it describes changes — `StackFrame::open` for the frame's, \
                 `Row::to_value` for projection's.",
                filled.as_ref(),
                key.as_ref(),
            ),
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    fn witness_row(&mut self, _key: &ByteView) {}
}

/// How many rows the executor examines between cancellation polls.
///
/// Polling costs an atomic load, which is cheap but not free next to the per-row
/// work, so it happens on a stride rather than per row. The consequence is that a
/// run shorter than the stride can complete despite a cancelled token — a bounded
/// overrun, which is the trade the stride exists to make.
/// The span of `path` within `key`: the cache resolves the top-level field, and
/// any nested steps are walked inside it.
///
/// Only the top level is cached — the **depth-1 fast path**
/// ([`FieldPath`](crate::focus::plan::FieldPath)). A nested step re-derives its
/// offsets on every read, which is the trade a cache per record would have to
/// earn; flat keys are what the hot loop sees.
fn field_span(
    offsets: &mut FieldOffsets,
    key: &ByteView,
    path: &FieldPath,
) -> Result<Range<usize>, ApertureError> {
    let mut span = offsets
        .get(key, path.field_idx())
        .map_err(ApertureError::Decode)?;

    for &step in path.steps() {
        span = nested_field_span(key, span, step)?;
    }

    Ok(span)
}

/// The span of field `step` of the record occupying `outer` of `key`.
///
/// A record is `MARK_RECORD <element>… MARK_TERM` ([chapter 2]), so the walk is:
/// step over the marker, then skip `step` elements — in *nested* mode, where a
/// null element is escaped and a bare terminator ends the record. Bounded to
/// `outer`, so a malformed row cannot walk into the field that follows.
///
/// [chapter 2]: ../../docs/02-tuple-codec.md
fn nested_field_span(
    key: &[u8],
    outer: Range<usize>,
    step: usize,
) -> Result<Range<usize>, ApertureError> {
    if key.get(outer.start) != Some(&MARK_RECORD) {
        return Err(ApertureError::NotARecord { step });
    }

    // Bounded to the field, so a malformed row cannot walk out of this record and
    // into the one that follows it.
    let bytes = key
        .get(..outer.end)
        .ok_or(ApertureError::Decode(StoreCodecError::UnexpectedEof))?;

    let mut start = outer.start + 1;

    for _ in 0..step {
        if at_record_end(bytes, start) {
            return Err(ApertureError::NestedFieldOutOfRange { step });
        }
        start = skip(bytes, start, true).map_err(ApertureError::Decode)?;
    }

    if at_record_end(bytes, start) {
        return Err(ApertureError::NestedFieldOutOfRange { step });
    }

    let end = skip(bytes, start, true).map_err(ApertureError::Decode)?;

    Ok(start..end)
}

/// Whether `at` is a record's terminator rather than a null element — the one
/// place `0x00` is ambiguous, resolved by the `0x00 0xFF` escape ([chapter 2]).
///
/// [chapter 2]: ../../docs/02-tuple-codec.md
fn at_record_end(bytes: &[u8], at: usize) -> bool {
    bytes.get(at) == Some(&MARK_TERM) && bytes.get(at + 1) != Some(&MARK_ESCAPE)
}

/// The span of `path` in the row held by register `var`, through that register's
/// slot in `field_offsets`.
///
/// Shared by the frame (seek splices and residuals) and by projection, so both
/// index the cache by address and bounds-check it the same way.
fn get_field_span(
    field_offsets: &mut [FieldOffsets],
    key: &ByteView,
    var: Address,
    path: &FieldPath,
) -> Result<Range<usize>, ApertureError> {
    let offsets = field_offsets
        .get_mut(var.0)
        .ok_or(ApertureError::AddressOutOfBounds(var))?;

    field_span(offsets, key, path)
}

pub const CANCELLATION_STRIDE: usize = 4096;

/// Polls the cancellation token every [`CANCELLATION_STRIDE`] rows examined.
///
/// **Rows examined, not rows produced.** The two shapes fail differently: a
/// residual that rejects a million rows does a million rows of work without
/// producing one, while a scan whose rows all match produces a row — and returns
/// from [`StackFrame::next`] — after a single iteration. Counting only the first
/// (which is what a counter local to one `next()` call does) leaves a query that
/// matches everything unable to observe cancellation at all, however long it
/// runs. So the count lives here, above any single `next()`, and one tick means
/// one row pulled from a scan, whichever way it goes.
struct Deadline<'a> {
    token: &'a CancellationToken,
    since_poll: usize,
}

impl<'a> Deadline<'a> {
    fn new(token: &'a CancellationToken) -> Self {
        Self {
            token,
            since_poll: 0,
        }
    }

    /// Count one examined row, polling the token on the stride.
    #[inline]
    fn tick(&mut self) -> Result<(), ApertureError> {
        self.since_poll += 1;

        if self.since_poll >= CANCELLATION_STRIDE {
            self.since_poll = 0;

            if self.token.is_cancelled() {
                return Err(ApertureError::Cancelled);
            }
        }

        Ok(())
    }
}

struct StackFrame<S: FactStore> {
    scan: Option<S::Scan>,
    /// Which of the level's [`Source`]s is being drained.
    ///
    /// Alternatives are concatenated, so this only ever moves forward while the
    /// level is open, and is reset when it closes — a level re-entered from an
    /// outer level's next row starts at its first source again. Saved into the
    /// [`Cursor`] beside the row, because "which branch produced this" is not
    /// recoverable from the row itself.
    source: usize,
    current: Option<Register>,
    field_offsets: Box<[FieldOffsets]>,
    /// Whether a [`Step::Derive`] at this position has produced its one value —
    /// unused by scan steps, which read the same thing off `scan`.
    ///
    /// This is the whole state a derived bind needs, and it has to live somewhere
    /// the loop can read: arriving at a step from below and from above must do
    /// different things, and `enumerate` carries no direction.
    derived_produced: bool,
}

impl<S: FactStore> StackFrame<S> {
    fn closed(nvars: usize) -> Self {
        Self {
            scan: None,
            source: 0,
            current: None,
            field_offsets: vec![FieldOffsets::new(); nvars].into_boxed_slice(),
            derived_produced: false,
        }
    }

    /// Close the level: no live scan, no row, and back to its first source.
    ///
    /// Resetting `source` is what makes a level re-entered from an outer row
    /// produce all of its alternatives again rather than resuming where the last
    /// pass through it happened to stop.
    fn close(&mut self) {
        self.scan = None;
        self.source = 0;
        self.current = None;
    }

    fn open(
        &mut self,
        store: &S,
        source: &Source,
        state: &MachineState,
        resume_at: Option<&[u8]>,
    ) -> Result<(), ApertureError> {
        // The field-offset caches hold offsets into whichever row each register
        // held when they were filled. Re-opening this level means an outer
        // register has advanced, so they must be cleared *before* `build_prefix`
        // reads them: a stale span silently truncates or overruns the spliced
        // field, giving a wrong seek prefix (wrong join results) or an
        // out-of-range slice.
        self.field_offsets.iter_mut().for_each(|fo| fo.clear());

        let prefix = self.build_prefix(state, source)?;
        let hi = strinc(&prefix);
        let lo = resume_at.unwrap_or(&prefix);

        self.scan = Some(store.scan(lo, hi.as_deref())?);
        self.current = None;

        Ok(())
    }

    fn build_prefix(
        &mut self,
        state: &MachineState,
        source: &Source,
    ) -> Result<Vec<u8>, ApertureError> {
        let Source::Seek { access, .. } = source;
        let mut prefix = access.predicate_id.0.to_be_bytes().to_vec();

        match &access.seek_key {
            SeekKey::Prefix(bytes) => prefix.extend_from_slice(bytes.as_ref()),
            SeekKey::Composite(parts) => {
                for part in parts.iter() {
                    match part {
                        SeekKeyPart::Bytes(bytes) => prefix.extend_from_slice(bytes.as_ref()),
                        SeekKeyPart::RegisterField {
                            address: var_address,
                            path,
                        } => {
                            let key = state.fact(*var_address)?.key();
                            let span =
                                get_field_span(&mut self.field_offsets, &key, *var_address, path)?;
                            prefix.extend_from_slice(&key[span]);
                        }
                        // The register's *identity*, encoded as a fact-typed field
                        // holds it — never its key bytes (see the variant).
                        SeekKeyPart::RegisterFactId(var_address) => {
                            let fact_id = state.fact(*var_address)?.fact_id;
                            prefix.extend_from_slice(&fact_ref_bytes(fact_id));
                        }
                    }
                }
            }
        }
        Ok(prefix)
    }

    /// Advance to the next row satisfying this level's residuals.
    ///
    /// `deadline` is the run's, not this call's: it counts every row pulled here
    /// — matched or skipped — so the poll interval holds however the plan filters
    /// (see [`Deadline`]).
    fn next(
        &mut self,
        state: &MachineState,
        source: &Source,
        deadline: &mut Deadline<'_>,
    ) -> Result<Option<Register>, ApertureError> {
        let scan = self.scan.as_mut().ok_or(ApertureError::AdvanceAfterClose)?;

        for row in scan {
            deadline.tick()?;

            let (key_bytes, fact_id) = row?;

            // Every `keys` row begins with its predicate id, and `Register::key`
            // slices those bytes off to reach the key fields — on a shorter row
            // that slice panics. This is the one point where store output becomes
            // machine state, so checking here covers every `FactStore` impl at
            // once, including ones written later.
            if key_bytes.len() < PREDICATE_ID_SIZE {
                return Err(StoreError::ShortKeyRow {
                    len: key_bytes.len(),
                    expected: PREDICATE_ID_SIZE,
                }
                .into());
            }

            let current = Register {
                fact_id,
                bytes: key_bytes,
            };

            if Self::check_residuals(&mut self.field_offsets, state, source.residuals(), &current)?
            {
                self.current = Some(current.clone());
                return Ok(Some(current));
            }
        }

        Ok(None)
    }

    fn check_residuals(
        frame_field_offsets: &mut [FieldOffsets],
        state: &MachineState,
        residuals: &[Residual],
        register: &Register,
    ) -> Result<bool, ApertureError> {
        let key = register.key();
        let mut row_field_offsets = FieldOffsets::new();

        for residual in residuals.iter() {
            let span = field_span(&mut row_field_offsets, &key, &residual.path)?;
            let field = &key[span];

            let ok = match &residual.op {
                ResidualOp::EqConst(const_bytes) => field == const_bytes.as_ref(),
                ResidualOp::Prefix(prefix_bytes) => field.starts_with(prefix_bytes.as_ref()),
                ResidualOp::EqRegisterField {
                    address: var_address,
                    path,
                } => {
                    let other = state.fact(*var_address)?;
                    let other_key = other.key();
                    let other_span =
                        get_field_span(frame_field_offsets, &other_key, *var_address, path)?;
                    field == &other_key[other_span]
                }
                // The bound row's id *encoded*, rather than the field decoded: a
                // reference is a marker and eight fixed bytes, so this is a nine-byte
                // compare against a stack buffer — no decode, and no allocation in
                // the scan loop ([I9]).
                ResidualOp::EqRegisterFactId(var_address) => {
                    field == fact_ref_bytes(state.fact(*var_address)?.fact_id)
                }
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub struct Executor<S: FactStore> {
    store: S,
    plan: Plan,
    state: MachineState,
    stack: Box<[StackFrame<S>]>,
    depth: usize,
    /// One field-offset cache per register, for projection.
    ///
    /// Owned here rather than made per row: a fresh `Box<[_]>` for each row would
    /// allocate on the hot path ([I9](../../docs/invariants.md#i9)). Cleared at
    /// the top of [`Row::to_value`], which is the scope over which it is valid —
    /// no register can change while `step` holds the row.
    projection_offsets: Box<[FieldOffsets]>,
}

pub struct Cursor(Vec<Register>);

pub struct Row<'a, S: FactStore> {
    store: &'a S,
    state: &'a MachineState,
    plan: &'a Plan,
    offsets: &'a mut [FieldOffsets],
}

impl<S: FactStore> Row<'_, S> {
    /// Project this row through the plan's head.
    ///
    /// Clearing is done here, where the cache is used, so the precondition
    /// belongs to the function that depends on it — and so calling this twice on
    /// one row refills rather than reads another row's offsets.
    pub fn to_value(&mut self, interner: &LocalInterner) -> Result<Value, ApertureError> {
        for offsets in self.offsets.iter_mut() {
            offsets.clear();
        }

        project(
            interner,
            &self.plan.head,
            self.state,
            self.store,
            self.offsets,
        )
    }
}

fn project<S: FactStore>(
    interner: &LocalInterner,
    p: &Project,
    state: &MachineState,
    store: &S,
    offsets: &mut [FieldOffsets],
) -> Result<Value, ApertureError> {
    match p {
        Project::Lit(v) => Ok(v.clone()),

        Project::FactRef(address) => Ok(Value::FactRef(state.fact(*address)?.fact_id)),

        // A derived bind's output. Already a `Value` — computed, not decoded — so
        // there is no row to walk and no type to decode against.
        Project::Computed(address) => Ok(state.value(*address)?.clone()),

        Project::RegisterField { address, path, ty } => {
            let reg = state.fact(*address)?;
            let key = reg.key();

            // Through the row's cache, so a head reading several fields of one
            // register walks the row once between them all.
            let span = get_field_span(offsets, &key, *address, path)?;

            decode_typed(interner, &key[span], ty)
        }

        Project::Value { address, ty } => {
            // The value lives in the `entities` CF, not in the register (which
            // holds `predicate_id ++ key`). Fetch it by fact id — the one place
            // a value is read (I6) — and decode the value bytes.
            let reg = state.fact(*address)?;
            let entity = store
                .point(reg.fact_id)?
                .ok_or(ApertureError::DanglingFactId(reg.fact_id))?;
            decode_typed(interner, &entity.value, ty)
        }

        Project::Record(fields) => {
            let mut out = Vec::with_capacity(fields.len());

            for (field_name, field_proj) in fields.iter() {
                let field_name = interner
                    .try_resolve(*field_name)
                    .ok_or(ApertureError::UnknownSymbol(*field_name))?
                    .to_owned();

                let value = project(interner, field_proj, state, store, offsets)?;

                out.push((field_name, value));
            }

            Ok(Value::Record(out.into_boxed_slice()))
        }
    }
}

pub enum Stream<A> {
    Continue(A),
    Suspend(A),
}

/// How a run stopped. Every variant is reached by *consuming* the executor, which
/// is what enforces [I8](../../docs/invariants.md#i8): the store handle, its
/// snapshot and every open scan are dropped before the caller gets the answer.
///
/// A resumable stop carries only a bytes-only [`Cursor`]
/// ([chapter 5](../../docs/05-resume.md)); to continue, rebuild with
/// [`Executor::resume`] against a fresh snapshot.
pub enum Iteratee<A> {
    Done(A),
    Suspended(A, Cursor),
}

impl<S: FactStore> Executor<S> {
    pub fn new(store: S, plan: Plan) -> Self {
        let nvars = plan.nvars;
        let nframes = plan.body.len();
        let state = MachineState::new(nvars);
        let stack = std::iter::repeat_with(|| StackFrame::closed(nvars))
            .take(nframes)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            store,
            plan,
            state,
            stack,
            depth: 0,
            projection_offsets: vec![FieldOffsets::new(); nvars].into_boxed_slice(),
        }
    }

    /// The bytes-only resume point: one detached row per **level**.
    ///
    /// Called at a suspend, where every step up to and including `depth` has
    /// produced — so the cursor names every scan step among them, and nothing for
    /// the derive steps, which are recomputed instead. Asserted rather than
    /// assumed: collecting whatever happened to be set would quietly renumber the
    /// levels if a frame in the middle were ever empty, and `resume` pairs cursor
    /// entries with scan steps **by order**.
    pub fn build_cursor(&self) -> Cursor {
        let saved: Vec<Register> = self
            .stack
            .iter()
            .filter_map(|f| f.current.as_ref().map(Register::to_detached))
            .collect();

        debug_assert_eq!(
            saved.len(),
            self.plan.body[..=self.depth]
                .iter()
                .filter(|step| step.is_level())
                .count(),
            "a suspend cursor must name every level up to `depth`, contiguously"
        );

        Cursor(saved)
    }

    pub fn resume(store: S, plan: Plan, cursor: Cursor) -> Result<Self, ApertureError> {
        let mut ex = Executor::new(store, plan);

        if cursor.0.is_empty() {
            return Ok(ex);
        }

        // A `Cursor` is bytes-only and rebuilt from the wire, so it is untrusted:
        // checked here rather than left to index `plan.body` out of bounds below.
        //
        // Compared against the **level** count, not the step count: a cursor holds
        // one row per level and a suspend always happens at a full row, so anything
        // other than exactly that many is a cursor this plan did not produce. It was
        // `>` while the two counts were the same number, which let a short cursor
        // half-replay a plan and carry on from the wrong place.
        if cursor.0.len() != ex.plan.levels() {
            return Err(ApertureError::CursorPlanMismatch {
                cursor: cursor.0.len(),
                plan: ex.plan.levels(),
            });
        }

        // Replaying a cursor re-reads one row per level, so it cannot run long
        // enough to reach a poll; the token is here only to satisfy `next`.
        let cancel = CancellationToken::new();
        let mut deadline = Deadline::new(&cancel);

        // One forward walk over the steps, which is the design's sentence made
        // literal: **re-bind the fact-slots, recompute the value-slots**. A scan
        // consumes the next cursor entry in order; a derive recomputes, because the
        // cursor deliberately carries nothing for it.
        let mut saved_rows = cursor.0.iter();

        for index in 0..ex.plan.body.len() {
            let frame = &mut ex.stack[index];

            match &ex.plan.body[index] {
                Step::Level(level) => {
                    // Cannot run out: the length check above pinned the cursor to
                    // exactly this plan's level count.
                    let saved = saved_rows.next().ok_or(ApertureError::BadResumeKey)?;

                    let source = level.sources.first().ok_or(ApertureError::BadResumeKey)?;

                    frame.open(&ex.store, source, &ex.state, Some(&saved.bytes))?;

                    let row = frame
                        .next(&ex.state, source, &mut deadline)?
                        .ok_or(ApertureError::BadResumeKey)?;

                    if row.fact_id != saved.fact_id {
                        return Err(ApertureError::BadResumeKey);
                    }

                    for var_address in level.binds.iter() {
                        ex.state.registers[var_address.0] = Some(Slot::Fact(row.clone()));
                    }
                    frame.current = Some(row);
                }

                Step::Derive(derived) => {
                    ex.state.registers[derived.bind.0] = Some(Slot::Value(compute(&derived.value)));
                    frame.derived_produced = true;
                }
            }
        }

        // A suspend only ever happens at a full row, so every step had produced —
        // which is why the walk above replays all of them and lands here.
        ex.depth = ex.plan.body.len() - 1;
        Ok(ex)
    }

    /// Run the plan, handing each row to `step`, until the plan is exhausted or
    /// `step` asks to suspend.
    ///
    /// **Takes `self` by value, and that is load-bearing**
    /// ([I8](../../docs/invariants.md#i8)). A fjall scan pins a read snapshot, and
    /// a pinned snapshot keeps LSM blocks — and a whole superseded generation —
    /// alive; an idle portal must hold neither. Consuming the executor makes that
    /// structural instead of a discipline: *every* exit path from here (done,
    /// suspend, cancel, error unwind) drops the frame stack and the store handle,
    /// so there is no shape of caller that can park a live iterator across a
    /// suspend. Resuming is `Executor::resume` with the returned [`Cursor`] and a
    /// fresh snapshot, which is exactly what the wire path does when a portal
    /// wakes up ([chapter 5](../../docs/05-resume.md)).
    pub fn enumerate<A>(
        mut self,
        init: A,
        mut step: impl FnMut(A, Row<'_, S>) -> Result<Stream<A>, ApertureError>,
        cancellation_token: &CancellationToken,
    ) -> Result<Iteratee<A>, ApertureError> {
        // One deadline for the whole run: the poll interval is a property of the
        // run, not of any single level's scan.
        let mut deadline = Deadline::new(cancellation_token);
        let mut acc = init;

        loop {
            if self.depth == self.plan.body.len() {
                let row = Row {
                    store: &self.store,
                    state: &self.state,
                    plan: &self.plan,
                    offsets: &mut self.projection_offsets,
                };
                match step(acc, row)? {
                    Stream::Continue(next) => {
                        acc = next;

                        // No steps at all — a query whose every binding folded at
                        // compile time, `X where X = 42`. It has produced its one
                        // row and there is no level to back into; `depth -= 1` here
                        // is what used to underflow, and is why an empty body was an
                        // error. It is safe now for the same reason the suspend arm
                        // below is: a plan with no levels is *exactly one row*, so
                        // "done" is the truth rather than a guess.
                        if self.plan.body.is_empty() {
                            return Ok(Iteratee::Done(acc));
                        }

                        self.depth -= 1;
                        continue;
                    }
                    Stream::Suspend(next) => {
                        acc = next;

                        // A plan with no levels produces **exactly one row** — every
                        // step is a derived bind, and a derived bind is one value —
                        // so its cursor would be empty, and an empty cursor means
                        // "start from the beginning". Suspending here would re-emit
                        // that row on resume. Reporting `Done` instead is not a
                        // half-answer: the run genuinely is complete, which is what
                        // a resume would have discovered one round-trip later
                        // anyway.
                        if self.plan.levels() == 0 {
                            return Ok(Iteratee::Done(acc));
                        }

                        // Back off the head before saving, so `depth` names the
                        // innermost step holding a row — which is what the cursor
                        // is checked against.
                        self.depth -= 1;
                        return Ok(Iteratee::Suspended(acc, self.build_cursor()));
                    }
                }
            }

            let frame = &mut self.stack[self.depth];

            // Descending or backtracking is not a variable the loop carries — it is
            // read off the frame, which is what keeps this a defunctionalised state
            // machine ([I7](../../docs/invariants.md#i7)). A scan reads it from
            // whether its iterator is open; a derive step, having no iterator, needs
            // the one bit below.
            match &self.plan.body[self.depth] {
                Step::Level(level) => {
                    // No alternative left to open — which is both "every source
                    // has been drained" and, for a level with no sources at all,
                    // "the empty relation". One arm, because the machine's answer
                    // to the two is the same: close and back up.
                    let Some(source) = level.sources.get(frame.source) else {
                        frame.close();
                        if self.depth == 0 {
                            return Ok(Iteratee::Done(acc));
                        }
                        self.depth -= 1;
                        continue;
                    };

                    if frame.scan.is_none() {
                        frame.open(&self.store, source, &self.state, None)?;
                    }

                    match frame.next(&self.state, source, &mut deadline)? {
                        Some(register) => {
                            for var_address in level.binds.iter() {
                                let slot = self
                                    .state
                                    .registers
                                    .get_mut(var_address.0)
                                    .ok_or(ApertureError::AddressOutOfBounds(*var_address))?;
                                *slot = Some(Slot::Fact(register.clone()));
                            }
                            frame.current = Some(register);
                            self.depth += 1;
                        }
                        // This alternative is drained; the next round of the loop
                        // opens the one after it, or backs out above if there is
                        // none. Backtracking lives in one place for both.
                        None => {
                            frame.scan = None;
                            frame.source += 1;
                        }
                    }
                }

                // A derived bind produces exactly one value, so as a step it is a
                // one-row generator: compute and ascend the first time, report
                // exhausted the second. That is the whole of "a derived bind is not
                // a loop level" as the machine sees it — the difference from a scan
                // is that it contributes nothing to the cursor and is recomputed on
                // resume rather than replayed.
                Step::Derive(derived) => {
                    if frame.derived_produced {
                        frame.derived_produced = false;
                        if self.depth == 0 {
                            return Ok(Iteratee::Done(acc));
                        }
                        self.depth -= 1;
                    } else {
                        let slot = self
                            .state
                            .registers
                            .get_mut(derived.bind.0)
                            .ok_or(ApertureError::AddressOutOfBounds(derived.bind))?;
                        *slot = Some(Slot::Value(compute(&derived.value)));
                        frame.derived_produced = true;
                        self.depth += 1;
                    }
                }
            }
        }
    }
}

/// Evaluate a derived bind.
///
/// Total and pure by construction — no store, no state, no iteration — which is
/// the invariant the resume path depends on: this is called again after a restore
/// and must produce what it produced before
/// ([chapter 7](../../docs/07-compilation.md#derived-facts)).
fn compute(value: &Computed) -> Value {
    match value {
        Computed::Lit(v) => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{
        fixtures::{
            FrozenStore, PointSpy, collect_rows, compose, count_rows, fact_ref_field, i64_field,
            interner_with, run_with_suspends, str_field,
        },
        mem_store::MemStore,
        plan::{
            Access, DerivedBind, Entity, FactId, FieldPath, Level, Plan, Project, Residual,
            ResidualOp, SeekKey, SeekKeyPart,
            proptest::{PlanAndStore, arb_interruption_schedule, arb_plan_and_store, cut_points},
        },
        schema::{PredicateId, PredicateTy},
        store::FjallDb,
        tuple::{MARK_NULL, Value, decode_probe},
    };
    use ::proptest::prelude::*;
    use std::{collections::BTreeSet, sync::atomic::Ordering};
    use tempfile::TempDir;

    /// Run a plan whose head projects only scalars (no record field names to
    /// resolve). Record-head tests call [`collect_rows`] with their own interner.
    fn run(store: MemStore, plan: Plan) -> Vec<Value> {
        collect_rows(store, plan, &interner_with(&[])).unwrap()
    }

    // ---- the field-offset cache -------------------------------------------
    //
    // The cache is what stops a seek splice and a residual on the same register
    // re-walking the row, and it is sound only while the row it describes is
    // fixed (see [`FieldOffsets`]). These pin that contract directly, at the unit
    // it lives in; `seek_splice_rereads_field_when_outer_row_width_changes` is
    // the same invariant asserted through the executor.

    /// A composite key as the register would hold it.
    fn key_of(fields: &[&[u8]]) -> ByteView {
        ByteView::from(compose(fields))
    }

    /// Offsets are filled left to right however they are asked for, and each span
    /// is exactly its field — including when the first read skips ahead.
    #[test]
    fn field_offsets_span_each_field_and_fill_lazily() {
        let key = key_of(&[&i64_field(1), &str_field("abc"), &i64_field(2)]);
        let mut offsets = FieldOffsets::new();

        // Asked out of order: reaching field 2 has to fill 0 and 1 on the way.
        let third = offsets.get(&key, 2).unwrap();
        let first = offsets.get(&key, 0).unwrap();
        let second = offsets.get(&key, 1).unwrap();

        assert_eq!(&key[first.clone()], i64_field(1).as_slice());
        assert_eq!(&key[second.clone()], str_field("abc").as_slice());
        assert_eq!(&key[third.clone()], i64_field(2).as_slice());

        // Contiguous and covering: fields abut, and the last one ends the key.
        assert_eq!(first.start, 0);
        assert_eq!(first.end, second.start);
        assert_eq!(second.end, third.start);
        assert_eq!(third.end, key.len());
    }

    /// A key with more fields than the cache can hold: the tail past the cap is
    /// re-derived on each read rather than cached, and must still be right — both
    /// the first time and on a repeat read.
    #[test]
    fn field_offsets_resolve_fields_past_the_cache_capacity() {
        let fields: Vec<Vec<u8>> = (0..FIELD_OFFSETS_CAPACITY as i64 + 4)
            .map(i64_field)
            .collect();
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        let key = key_of(&refs);
        let mut offsets = FieldOffsets::new();

        for (idx, field) in fields.iter().enumerate() {
            let span = offsets.get(&key, idx).unwrap();
            assert_eq!(&key[span], field.as_slice(), "field {idx}");
        }

        let last = fields.len() - 1;
        let span = offsets.get(&key, last).unwrap();
        assert_eq!(
            &key[span],
            fields[last].as_slice(),
            "re-read of field {last}"
        );
    }

    /// After a clear the cache describes whatever row it is next given. The two
    /// rows have deliberately different field widths, so a surviving offset could
    /// not go unnoticed.
    #[test]
    fn field_offsets_reread_the_new_row_after_clear() {
        let short = key_of(&[&str_field("a"), &i64_field(7)]);
        let long = key_of(&[&str_field("abcdef"), &i64_field(7)]);

        let mut offsets = FieldOffsets::new();
        let span = offsets.get(&short, 1).unwrap();
        assert_eq!(&short[span], i64_field(7).as_slice());

        offsets.clear();
        let span = offsets.get(&long, 1).unwrap();
        assert_eq!(&long[span], i64_field(7).as_slice());
    }

    /// The witness is not decorative. Without the clear above, the cached
    /// boundaries of `"a"` applied to `"abcdef"` name bytes in the middle of the
    /// string rather than the integer that follows it — a wrong seek prefix, or a
    /// residual comparing the wrong bytes. That must be caught, not answered.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "field-offset cache reused against a different row")]
    fn field_offsets_reject_a_stale_row() {
        let filled = key_of(&[&str_field("a"), &i64_field(7)]);
        let other = key_of(&[&str_field("abcdef"), &i64_field(7)]);

        let mut offsets = FieldOffsets::new();
        offsets.get(&filled, 1).unwrap();
        let _ = offsets.get(&other, 1);
    }

    // ---- nested field paths ------------------------------------------------
    //
    // A stored key is its top-level fields back to back, so those are reached by
    // the cache alone. A *record-typed* field keeps its own `MARK_RECORD … TERM`
    // wrapper ([chapter 2]), and a plan reaches inside it with a
    // [`FieldPath`](crate::focus::plan::FieldPath). These pin that walk, including
    // both ways it can be asked for something the row does not have — which are
    // plan faults, and so must be errors rather than bytes that happen to sit
    // there (conventions: errors, not panics, on data paths).

    /// A record field as a key holds it: `{outer = {inner = …, extra = …}}`.
    fn record_field(fields: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![MARK_RECORD];
        out.extend_from_slice(&fields.concat());
        out.push(MARK_TERM);
        out
    }

    /// Each step of a path lands on exactly the field it names, at any depth, and
    /// beside a flat field that is reached by the fast path.
    #[test]
    fn a_path_walks_into_a_nested_record() {
        // key: field 0 = int, field 1 = {a = str, b = {c = int}}
        let inner = record_field(&[&i64_field(9)]);
        let nested = record_field(&[&str_field("x"), &inner]);
        let key = key_of(&[&i64_field(7), &nested]);

        let mut offsets = FieldOffsets::new();

        let flat = field_span(&mut offsets, &key, &FieldPath::field(0)).expect("flat field");
        assert_eq!(&key[flat], i64_field(7).as_slice());

        let whole =
            field_span(&mut offsets, &key, &FieldPath::field(1)).expect("the record field whole");
        assert_eq!(
            &key[whole],
            nested.as_slice(),
            "a record field keeps its wrapper"
        );

        let one = field_span(&mut offsets, &key, &FieldPath::nested(1, [0])).expect("1.0");
        assert_eq!(&key[one], str_field("x").as_slice());

        let two = field_span(&mut offsets, &key, &FieldPath::nested(1, [1])).expect("1.1");
        assert_eq!(&key[two], inner.as_slice());

        let deep = field_span(&mut offsets, &key, &FieldPath::nested(1, [1, 0])).expect("1.1.0");
        assert_eq!(&key[deep], i64_field(9).as_slice());
    }

    /// A null *element* inside a record is `0x00 0xFF`, and a bare `0x00` is the
    /// terminator — so the walk has to read the escape rather than stop at the
    /// first zero byte, or every field after a null would be unreachable.
    #[test]
    fn a_path_walks_past_an_escaped_null_element() {
        let nested = record_field(&[&[MARK_NULL, MARK_ESCAPE], &i64_field(5)]);
        let key = key_of(&[&nested]);

        let mut offsets = FieldOffsets::new();
        let second = field_span(&mut offsets, &key, &FieldPath::nested(0, [1])).expect("0.1");

        assert_eq!(&key[second], i64_field(5).as_slice());
    }

    /// Stepping into a field that is not a record is a plan disagreeing with the
    /// schema, and says so.
    #[test]
    fn a_path_into_a_scalar_field_is_an_error() {
        let key = key_of(&[&i64_field(7)]);
        let mut offsets = FieldOffsets::new();

        assert!(matches!(
            field_span(&mut offsets, &key, &FieldPath::nested(0, [0])),
            Err(ApertureError::NotARecord { step: 0 })
        ));
    }

    /// A step past the record's last field stops at the terminator rather than
    /// reading the bytes of whatever follows the field.
    #[test]
    fn a_path_past_the_last_nested_field_is_an_error() {
        let nested = record_field(&[&i64_field(1)]);
        // A second top-level field, so an overrun would find real bytes to decode.
        let key = key_of(&[&nested, &i64_field(2)]);
        let mut offsets = FieldOffsets::new();

        assert!(matches!(
            field_span(&mut offsets, &key, &FieldPath::nested(0, [1])),
            Err(ApertureError::NestedFieldOutOfRange { step: 1 })
        ));
    }

    /// The whole machine through a nested path: a residual filters on a field
    /// inside a record, a seek splices one, and the head projects one.
    ///
    /// The unit tests above pin the walk; this pins that every place a plan can
    /// name a field passes the path through rather than only the flat index it
    /// used to carry.
    #[test]
    fn a_plan_seeks_filters_and_projects_through_a_nested_path() {
        let (nested, ints) = (PredicateId(0), PredicateId(1));

        let mut store = MemStore::new();
        // `nested`: one field, a record `{inner = i, tag = str}`.
        for (i, tag) in [(1i64, "a"), (2, "b"), (3, "a")] {
            store.insert(
                nested,
                record_field(&[&i64_field(i), &str_field(tag)]),
                i as u64,
            );
        }
        // `ints`: a scalar key, joined against the nested `inner` field.
        for i in [2i64, 3] {
            store.insert(ints, i64_field(i), i as u64);
        }

        let interner = interner_with(&["n"]);
        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                Level::seek(
                    Access {
                        predicate_id: nested,
                        seek_key: SeekKey::Prefix(Box::new([])),
                    },
                    Box::new([Address::new(0)]), // `tag = "a"`, one step inside the record.
                    Box::new([Residual {
                        path: FieldPath::nested(0, [1]),
                        op: ResidualOp::EqConst(str_field("a").into_boxed_slice()),
                    }]),
                ),
                Level::seek(
                    Access {
                        predicate_id: ints,
                        // ...seeking on `inner`, also one step inside.
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            path: FieldPath::nested(0, [0]),
                        }])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
            ]),
            head: Project::Record(Box::new([(
                interner.get("n").expect("interned above"),
                Project::RegisterField {
                    address: Address::new(0),
                    path: FieldPath::nested(0, [0]),
                    ty: PredicateTy::Int,
                },
            )])),
        };

        let rows = collect_rows(store, plan, &interner).expect("run");

        // Of the three nested rows, `tag = "a"` keeps 1 and 3; of those, only 3
        // has an `ints` fact to join with.
        assert_eq!(
            rows,
            vec![Value::Record(Box::new([("n".to_owned(), Value::Int(3))]))]
        );
    }

    /// A path renders as the field it names, which is what a plan reads as.
    #[test]
    fn a_path_renders_as_its_steps() {
        assert_eq!(FieldPath::field(2).to_string(), "2");
        assert_eq!(FieldPath::nested(1, [0, 3]).to_string(), "1.0.3");
        assert!(FieldPath::field(0).is_flat());
        assert!(!FieldPath::nested(0, [0]).is_flat());
        assert_eq!(FieldPath::field(1).then(2), FieldPath::nested(1, [2]));
    }

    /// A register renders as an index, not a machine address. `Address(0)` used to
    /// reach a diagnostic as `0x0000000000000000`.
    #[test]
    fn an_address_reads_as_a_register() {
        assert_eq!(Address::new(0).to_string(), "r0");
        assert_eq!(
            ApertureError::UseBeforeBind(Address::new(2)).to_string(),
            "r2 was read before anything was bound to it"
        );
        assert_eq!(
            ApertureError::AddressOutOfBounds(Address::new(7)).to_string(),
            "r7 is not a register in this plan"
        );
    }

    /// Projection walks a row **once**, not once per field.
    ///
    /// A record head reading k fields off one register built a fresh offset cache
    /// for each and skipped from field 0 every time — k(k+1)/2 skips for k fields,
    /// where the frame's own cache had long since stopped doing that for seeks and
    /// residuals. Reading fields 0..=3 of one row must cost 4 skips, not 10.
    #[test]
    fn projection_walks_each_field_once() {
        const FIELDS: usize = 4;

        let p = PredicateId(0);
        let mut store = MemStore::new();
        store.insert(
            p,
            compose(&[&i64_field(1), &i64_field(2), &i64_field(3), &i64_field(4)]),
            1,
        );

        let names = ["a", "b", "c", "d"];
        let interner = interner_with(&names);
        let head = Project::Record(
            names
                .iter()
                .enumerate()
                .map(|(idx, name)| {
                    (
                        interner.get(name).expect("interned above"),
                        Project::RegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(idx),
                            ty: PredicateTy::Int,
                        },
                    )
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );

        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head,
        };

        // Nothing else in this plan reads a field: the seek is a bare prefix and
        // there are no residuals, so every skip counted here is projection's.
        skip_probe::reset();
        let rows = collect_rows(store, plan, &interner).expect("run");
        let skips = skip_probe::count();

        assert_eq!(rows.len(), 1, "the plan must produce a row to measure");
        assert_eq!(
            skips,
            FIELDS as u64,
            "projecting {FIELDS} fields of one row took {skips} skips; walking the \
             row once costs {FIELDS}, and once per field costs {}",
            FIELDS * (FIELDS + 1) / 2
        );
    }

    // ---- malformed plans and cursors --------------------------------------
    //
    // Both cross into the executor from outside — a plan from the compiler, a
    // `Cursor` from the wire — so neither may panic it (conventions: errors, not
    // panics, on data paths).

    /// **A plan with no steps is the unit relation: exactly one row.**
    ///
    /// It used to be `EmptyPlan`, and the reason was sound at the time — the first
    /// row backed into `depth -= 1` and underflowed, and emitting a row anyway would
    /// have been worse than a panic, because an empty `Cursor` restarts a run and so
    /// the row would come back twice across a suspend. Both halves are now answered:
    /// the head backs out to `Done` instead of decrementing, and a plan with no
    /// levels reports `Done` when asked to suspend rather than handing back a cursor
    /// that cannot express "already emitted".
    ///
    /// What produces this shape is a query whose every binding **folded** —
    /// `X where X = 42` compiles to no steps and a literal head.
    #[test]
    fn a_plan_with_no_steps_yields_exactly_one_row() {
        let plan = Plan {
            nvars: 0,
            body: Step::levels([]),
            head: Project::Lit(Value::Int(1)),
        };

        assert_eq!(
            collect_rows(MemStore::new(), plan, &interner_with(&[])).expect("run"),
            vec![Value::Int(1)],
        );
    }

    /// Cancellation is observed on a scan whose rows **all match**.
    ///
    /// The token is polled every `CANCELLATION_STRIDE` rows examined. While the
    /// counter lived inside a single `next()` call it only ever counted rows a
    /// residual *skipped*: a plan with no residual returns after one iteration
    /// each time, so the counter reset before it could reach the stride and the
    /// token was never read. A long-running query that matched everything could
    /// not be cancelled at all — the one shape most likely to need it.
    ///
    /// The companion positive control is `snapshot_released_at_suspend` in
    /// `store`, which covers the skipped-row path and the snapshot release.
    #[test]
    fn a_matching_scan_observes_cancellation() {
        let p = PredicateId(0);

        let mut store = MemStore::new();
        for i in 0..(CANCELLATION_STRIDE as i64 * 2) {
            store.insert(p, i64_field(i), i as u64 + 1);
        }

        // No residual: every row matches, so every `next()` returns immediately.
        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        let cancelled = CancellationToken::new();
        cancelled.cancel();

        let mut seen = 0usize;
        let out = Executor::new(store, plan).enumerate(
            0usize,
            |n, _row| {
                seen = n + 1;
                Ok(Stream::Continue(n + 1))
            },
            &cancelled,
        );

        assert!(
            matches!(out, Err(ApertureError::Cancelled)),
            "a matching scan ran to completion under a cancelled token"
        );
        assert!(
            seen < CANCELLATION_STRIDE * 2,
            "cancellation must stop the run early, not after every row ({seen})"
        );
    }

    /// A `FactStore` yielding one malformed row: three bytes, too few to carry
    /// the predicate-id prefix every `keys` row begins with.
    struct ShortRowStore;

    impl FactStore for ShortRowStore {
        type Scan = std::vec::IntoIter<Result<(ByteView, FactId), ApertureError>>;

        fn scan(&self, _lo: &[u8], _hi: Option<&[u8]>) -> Result<Self::Scan, ApertureError> {
            Ok(vec![Ok((
                ByteView::from(vec![0u8; PREDICATE_ID_SIZE - 1]),
                FactId::from_raw(1),
            ))]
            .into_iter())
        }

        fn point(&self, _id: FactId) -> Result<Option<Entity>, ApertureError> {
            Ok(None)
        }
    }

    /// A corrupt `keys` row is a surfaced error, not a panicking slice. The read
    /// path decodes bytes this process did not write — a reopened DB, a file
    /// copied between machines — so a malformed row is a data condition.
    #[test]
    fn a_short_keys_row_is_an_error_not_a_panic() {
        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(PredicateId(0), 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        assert!(matches!(
            collect_rows(ShortRowStore, plan, &interner_with(&[])),
            Err(ApertureError::Store(StoreError::ShortKeyRow {
                len: 3,
                expected: 4
            }))
        ));
    }

    /// A cursor naming more levels than the plan has must be rejected, not used
    /// to index the plan's body.
    ///
    /// The cursor is a real one — taken from a two-level run and offered to a
    /// one-level plan, which is the shape a stale portal on the wire has.
    #[test]
    fn resume_rejects_a_cursor_deeper_than_the_plan() {
        let (person, knows) = (PredicateId(0), PredicateId(1));

        let seed = || {
            let mut store = MemStore::new();
            store.insert(person, i64_field(1), 1);
            store.insert(knows, compose(&[&i64_field(1), &i64_field(2)]), 1);
            store
        };

        let two_level = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: knows,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(0),
                        }])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
            ]),
            head: Project::FactRef(Address::new(1)),
        };

        let suspended = Executor::new(seed(), two_level)
            .enumerate(
                0usize,
                |n, _row| Ok(Stream::Suspend(n + 1)),
                &CancellationToken::new(),
            )
            .expect("run");

        let Iteratee::Suspended(_, cursor) = suspended else {
            panic!("the plan was supposed to suspend");
        };
        assert_eq!(cursor.0.len(), 2, "the cursor must name both levels");

        let one_level = Plan {
            nvars: 1,
            body: Step::levels([scan_all(person, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        assert!(matches!(
            Executor::resume(seed(), one_level, cursor),
            Err(ApertureError::CursorPlanMismatch { cursor: 2, plan: 1 })
        ));
    }

    // ---- the register file and the cursor, at the seams --------------------
    //
    // These pin the three contracts the `Register → Slot` promotion (PLAN Phase
    // 6) rewrites: what [`MachineState::get`] does when a register is not there,
    // what `resume` does when a saved row is not the row it saved, and that a
    // [`Cursor`] is exactly one **detached** row per level. Each was reachable
    // only by inspection before — `an_address_reads_as_a_register` asserts how
    // the two register faults *render*, which is a different claim from the
    // machine producing them.

    /// Reading a register no generator binds must come back as `UseBeforeBind`,
    /// not unwrap a `None`.
    ///
    /// Flatten cannot emit this — range-restriction rejects it first — which is
    /// precisely why it needs a guard here rather than there: the plan this
    /// protects against arrives from somewhere else, hand-built today and
    /// wire-decoded later, and `MachineState::get` is the one funnel both go
    /// through.
    #[test]
    fn reading_an_unbound_register_is_an_error_not_a_panic() {
        let p = PredicateId(0);
        let mut store = MemStore::new();
        store.insert(p, i64_field(1), 1);

        // Two registers, one generator binding r0: nothing ever binds r1.
        let plan = Plan {
            nvars: 2,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(1)),
        };

        assert!(matches!(
            collect_rows(store, plan, &interner_with(&[])),
            Err(ApertureError::UseBeforeBind(a)) if a == Address::new(1)
        ));
    }

    /// Reading a register the plan does not have at all is `AddressOutOfBounds` —
    /// the arm above it in `get`, and a different fault: out of range rather than
    /// in range and empty.
    #[test]
    fn reading_a_register_past_the_plan_is_an_error_not_a_panic() {
        let p = PredicateId(0);
        let mut store = MemStore::new();
        store.insert(p, i64_field(1), 1);

        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(7)),
        };

        assert!(matches!(
            collect_rows(store, plan, &interner_with(&[])),
            Err(ApertureError::AddressOutOfBounds(a)) if a == Address::new(7)
        ));
    }

    /// Reading a register as the **wrong kind of slot** reports rather than
    /// panics, in both directions.
    ///
    /// This is the fault the [`Slot`] split exists to make impossible to ignore:
    /// a value spliced where a row's bytes belong (or the reverse) compares two
    /// different encodings and would quietly match nothing — the same silent shape
    /// as the `FactRef` marker trap. A plan from the compiler cannot do this,
    /// since flatten knows which addresses a derived bind writes; a plan off the
    /// wire can, which is why it is an error and not a `debug_assert`.
    #[test]
    fn reading_a_register_as_the_wrong_kind_of_slot_is_an_error() {
        let mut state = MachineState::new(2);
        state.registers[0] = Some(Slot::Value(Value::Int(42)));
        state.registers[1] = Some(Slot::Fact(Register {
            fact_id: FactId::new(PredicateId(0), 1).expect("id"),
            bytes: ByteView::from(vec![0, 0, 0, 0]),
        }));

        assert!(matches!(
            state.fact(Address::new(0)),
            Err(ApertureError::SlotKindMismatch {
                address,
                wanted: "a fact row",
                held: "a computed value",
            }) if address == Address::new(0)
        ));
        assert!(matches!(
            state.value(Address::new(1)),
            Err(ApertureError::SlotKindMismatch {
                wanted: "a computed value",
                held: "a fact row",
                ..
            })
        ));

        // ...and reads the right kind without complaint.
        assert_eq!(
            state.value(Address::new(0)).expect("a value"),
            &Value::Int(42)
        );
        assert!(state.fact(Address::new(1)).is_ok());

        // The two faults above are distinct from *absence*, which the addresses
        // beyond these two still report as before.
        assert!(matches!(
            state.fact(Address::new(9)),
            Err(ApertureError::AddressOutOfBounds(_))
        ));
    }

    /// A one-level plan suspended after its first row, as the cursor tests below
    /// need it. Returns the cursor and the model rows.
    fn suspend_after_first_row(store: MemStore, plan: Plan) -> Cursor {
        let out = Executor::new(store, plan)
            .enumerate(
                (),
                |(), _row| Ok(Stream::Suspend(())),
                &CancellationToken::new(),
            )
            .expect("run");

        match out {
            Iteratee::Suspended((), cursor) => cursor,
            Iteratee::Done(()) => panic!("the plan was supposed to suspend"),
        }
    }

    /// **The resume integrity check.** A cursor's saved key must still resolve to
    /// the *same fact*, and when it does not, resume must refuse rather than carry
    /// on against a row it never saw.
    ///
    /// This is what [I11](../../docs/invariants.md#i11) buys the executor: ids are
    /// never reused, so a key that now names a different id means the cursor and
    /// the store disagree about the world — a stale portal against a rebuilt DB.
    /// Resuming anyway would emit a row the uninterrupted run never produced,
    /// which is exactly the failure [I4](../../docs/invariants.md#i4) forbids and
    /// the one the row-sequence comparison cannot see, because the run it is
    /// compared against no longer exists.
    #[test]
    fn resume_refuses_a_cursor_whose_key_now_names_another_fact() {
        let p = PredicateId(0);

        let plan = || Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        let mut original = MemStore::new();
        original.insert(p, i64_field(1), 1);
        let cursor = suspend_after_first_row(original, plan());

        // The same key, a different id — what a rebuilt DB looks like from the
        // outside. The bytes resume seeks by are byte-identical, so only the id
        // check can catch this.
        let mut rebuilt = MemStore::new();
        rebuilt.insert(p, i64_field(1), 99);

        assert!(matches!(
            Executor::resume(rebuilt, plan(), cursor),
            Err(ApertureError::BadResumeKey)
        ));
    }

    /// The other arm of the same check: the saved key is gone entirely, so the
    /// replay scan yields nothing where it must yield the saved row.
    #[test]
    fn resume_refuses_a_cursor_whose_key_is_gone() {
        let p = PredicateId(0);

        let plan = || Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        let mut original = MemStore::new();
        original.insert(p, i64_field(1), 1);
        let cursor = suspend_after_first_row(original, plan());

        assert!(matches!(
            Executor::resume(MemStore::new(), plan(), cursor),
            Err(ApertureError::BadResumeKey)
        ));
    }

    /// A cursor is **one row per level, every level, and it owns its bytes**.
    ///
    /// Two claims in one test because they are the same claim from either end.
    /// *One per level:* `build_cursor` collects whichever frames hold a row and
    /// `debug_assert`s that this is `depth + 1` of them; a suspend only ever
    /// happens at a full row, so the count is the level count — which is what
    /// makes `resume`'s replay-by-position sound. Phase 6 must keep this exact
    /// number: a derived bind is not a loop level and adds no cursor entry.
    /// *Owns its bytes:* the store is dropped here before the cursor is read, so
    /// a view still pointing into it would be reading freed memory — the whole
    /// reason [`Register::to_detached`] exists on the suspend path.
    #[test]
    fn a_cursor_holds_one_detached_row_per_level() {
        let interner = interner_with(&["a", "b", "c"]);

        for (levels, mk) in [
            (1usize, &one_level_scan as &dyn Fn() -> (MemStore, Plan)),
            (2, &|| two_level_seek_join(&interner)),
            (3, &|| three_level_seek_join(&interner)),
        ] {
            let (store, plan) = mk();
            let cursor = suspend_after_first_row(store, plan);

            assert_eq!(
                cursor.0.len(),
                levels,
                "a {levels}-level plan suspended with {} cursor entr(ies)",
                cursor.0.len()
            );

            // Every entry names a real fact and carries its whole row —
            // `predicate_id ++ key`, so at least the id is present. Read *after*
            // the store that produced the bytes has been dropped.
            for (level, saved) in cursor.0.iter().enumerate() {
                assert!(
                    saved.bytes.len() > PREDICATE_ID_SIZE,
                    "level {level}'s saved row is {} byte(s) — no key follows the \
                     predicate id",
                    saved.bytes.len()
                );
                assert_ne!(
                    saved.fact_id.raw(),
                    0,
                    "level {level} saved the reserved fact id, which is never a fact"
                );
            }
        }
    }

    // A residual on a key field is evaluated against the field's value (the
    // stripped key, predicate-id prefix removed), consistently with seek splices
    // and projection — so it filters on the field, not on the prefix bytes.
    #[test]
    fn residual_eq_const_on_key_field_filters_correctly() {
        let pred = PredicateId(0);

        let mut store = MemStore::new();
        store.insert(pred, str_field("alpha"), 1);
        store.insert(pred, str_field("beta"), 2);
        store.insert(pred, str_field("gamma"), 3);

        let plan = Plan {
            nvars: 1,
            body: Step::levels([Level::seek(
                Access {
                    predicate_id: pred,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                Box::new([Address::new(0)]),
                Box::new([Residual {
                    path: FieldPath::field(0),
                    op: ResidualOp::EqConst(str_field("beta").into_boxed_slice()),
                }]),
            )]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(0),
                ty: PredicateTy::Str,
            },
        };

        assert_eq!(run(store, plan), vec![Value::Str("beta".to_string())]);
    }

    // `Project::Value` reads the fact's value from the `entities` CF (via a
    // point lookup by fact id), not the key bytes held in the register. Here the
    // key is a string ("alpha") and the value is an integer (42); projecting the
    // value must yield the integer. Regression for the latent bug where
    // `Project::Value` decoded `reg.bytes` (predicate_id ++ key) as the value.
    #[test]
    fn project_value_decodes_entity_value_not_register_key() {
        let pred = PredicateId(0);

        let mut store = MemStore::new();
        store.insert_valued(pred, str_field("alpha"), 1, i64_field(42));

        let plan = Plan {
            nvars: 1,
            body: Step::levels([Level::seek(
                Access {
                    predicate_id: pred,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                Box::new([Address::new(0)]),
                Box::new([]),
            )]),
            head: Project::Value {
                address: Address::new(0),
                ty: PredicateTy::Int,
            },
        };

        assert_eq!(run(store, plan), vec![Value::Int(42)]);
    }

    // ---- Happy-path battery (0b) ------------------------------------------
    //
    // Hand-built plans over hand-built stores, checked against hand-computed
    // rows. The model is "run to completion, collect rows" (`collect_rows`).
    // These exercise the executor mechanics — scan order, seek splices, the
    // three residual ops, backtracking, and every projection head — before the
    // schema-first generator (0c) drives the same machine at scale.

    /// Build an expected `Value::Record` from `(name, value)` pairs in slice
    /// order (matching the order the plan's `Project::Record` lists its fields).
    fn record(fields: &[(&str, Value)]) -> Value {
        Value::Record(
            fields
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        )
    }

    /// A single generator that scans a whole predicate and binds one register.
    fn scan_all(predicate_id: PredicateId, bind: usize) -> Level {
        Level::seek(
            Access {
                predicate_id,
                seek_key: SeekKey::Prefix(Box::new([])),
            },
            Box::new([Address::new(bind)]),
            Box::new([]),
        )
    }

    // A one-level scan projects a key field, in ascending key order regardless
    // of insert order (the codec is order-preserving, I1).
    #[test]
    fn scan_projects_scalar_field_in_key_order() {
        let p = PredicateId(0);

        let mut store = MemStore::new();
        store.insert(p, i64_field(30), 1);
        store.insert(p, i64_field(10), 2);
        store.insert(p, i64_field(20), 3);

        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(0),
                ty: PredicateTy::Int,
            },
        };

        assert_eq!(
            run(store, plan),
            vec![Value::Int(10), Value::Int(20), Value::Int(30)]
        );
    }

    // A `Prefix` residual on a string key field keeps only rows whose field
    // starts with the given (encoded, terminator-stripped) prefix.
    #[test]
    fn residual_prefix_on_string_field() {
        let p = PredicateId(0);

        let mut store = MemStore::new();
        store.insert(p, str_field("alpha"), 1);
        store.insert(p, str_field("altair"), 2);
        store.insert(p, str_field("beta"), 3);

        // Encoded "al" is [MARK_STRING, 'a', 'l', MARK_TERM]; a field prefix is
        // that without the terminator so it matches "al…" strings.
        let mut prefix = str_field("al");
        prefix.pop();

        let plan = Plan {
            nvars: 1,
            body: Step::levels([Level::seek(
                Access {
                    predicate_id: p,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                Box::new([Address::new(0)]),
                Box::new([Residual {
                    path: FieldPath::field(0),
                    op: ResidualOp::Prefix(prefix.into_boxed_slice()),
                }]),
            )]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(0),
                ty: PredicateTy::Str,
            },
        };

        assert_eq!(
            run(store, plan),
            vec![
                Value::Str("alpha".to_string()),
                Value::Str("altair".to_string()),
            ]
        );
    }

    // A two-level join: for each Person(id), seek Knows(id, other) by splicing
    // the bound id into the inner scan prefix. Person 3 has no Knows row, so it
    // contributes nothing (the inner scan is empty and the machine backtracks).
    #[test]
    fn two_level_join_via_seek_splice() {
        let person = PredicateId(0);
        let knows = PredicateId(1);

        let mut store = MemStore::new();
        store.insert(person, i64_field(1), 1);
        store.insert(person, i64_field(2), 2);
        store.insert(person, i64_field(3), 3);
        store.insert(knows, compose(&[&i64_field(1), &i64_field(2)]), 10);
        store.insert(knows, compose(&[&i64_field(1), &i64_field(3)]), 11);
        store.insert(knows, compose(&[&i64_field(2), &i64_field(3)]), 12);

        let interner = interner_with(&["a", "b"]);
        let a = interner.get("a").unwrap();
        let b = interner.get("b").unwrap();

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: knows,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(0),
                        }])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
            ]),
            head: Project::Record(Box::new([
                (
                    a,
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    b,
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        let rows = collect_rows(store, plan, &interner).unwrap();
        assert_eq!(
            rows,
            vec![
                record(&[("a", Value::Int(1)), ("b", Value::Int(2))]),
                record(&[("a", Value::Int(1)), ("b", Value::Int(3))]),
                record(&[("a", Value::Int(2)), ("b", Value::Int(3))]),
            ]
        );
    }

    // A level's field-offset cache is keyed by register and holds offsets into
    // whichever row that register held when it was filled. Re-opening the level
    // must not read it: the outer register has advanced, and the *cached* row's
    // field widths need not match the current one's.
    //
    // The trap needs a residual as well as a seek on the same register — the
    // residual fills the cache while scanning, and the next `open` then builds its
    // seek prefix from it. The outer keys have deliberately different byte widths
    // ("a", "abc", "b"), so a stale offset truncates the spliced field: seeking
    // "abc" with the width of "a" widens the range to every "ab…" row, and the
    // inner rows here are chosen so the residual can't filter the intruder back
    // out — `("ab", 3)` satisfies it just as `("abc", 3)` does. With equal-width
    // keys, or without the extra row, the bug is invisible.
    #[test]
    fn seek_splice_rereads_field_when_outer_row_width_changes() {
        let outer = PredicateId(0);
        let inner = PredicateId(1);

        let mut store = MemStore::new();
        for (i, (s, n)) in [("a", 1i64), ("abc", 3), ("b", 4)].into_iter().enumerate() {
            store.insert(
                outer,
                compose(&[&str_field(s), &i64_field(n)]),
                i as u64 + 1,
            );
        }
        for (i, (s, n)) in [("a", 1i64), ("ab", 3), ("abc", 3), ("b", 4)]
            .into_iter()
            .enumerate()
        {
            store.insert(
                inner,
                compose(&[&str_field(s), &i64_field(n)]),
                10 + i as u64,
            );
        }

        let interner = interner_with(&["a", "b", "c"]);

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(outer, 0),
                Level::seek(
                    Access {
                        predicate_id: inner,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(0),
                        }])),
                    },
                    Box::new([Address::new(1)]), // Fills this frame's offset cache for register 0 mid-scan.
                    Box::new([Residual {
                        path: FieldPath::field(1),
                        op: ResidualOp::EqRegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(1),
                        },
                    }]),
                ),
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Str,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Str,
                    },
                ),
                (
                    interner.get("c").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        let str_row = |outer: &str, inner: &str, n: i64| {
            record(&[
                ("a", Value::Str(outer.to_string())),
                ("b", Value::Str(inner.to_string())),
                ("c", Value::Int(n)),
            ])
        };

        let rows = collect_rows(store, plan, &interner).unwrap();
        assert_eq!(
            rows,
            vec![
                str_row("a", "a", 1),
                str_row("abc", "abc", 3),
                str_row("b", "b", 4),
            ]
        );
    }

    // ---- following a reference ---------------------------------------------

    /// A fact-typed field holds an **id**, not a key, so the splice is off
    /// `Register::fact_id`.
    ///
    /// The fixture separates the two on purpose: the outer keys are 10, 20, 30 while
    /// its fact ids are 1, 2, 3, and an integer field and a fact reference differ
    /// only in their leading marker byte (`0x48` against `0x51`). Splicing the key
    /// bytes therefore seeks a well-formed prefix that matches nothing — a silently
    /// empty answer, which is the trap this operator exists to close.
    #[test]
    fn seek_splices_a_bound_rows_fact_id() {
        let (person, refs) = (PredicateId(0), PredicateId(1));
        let person_fact = |sequence| FactId::new(person, sequence).unwrap();

        let mut store = MemStore::new();
        for (i, key) in [10i64, 20, 30].into_iter().enumerate() {
            store.insert(person, i64_field(key), i as u64 + 1);
        }
        // Keyed `(of, tag)`, so the splice is a *prefix* of a longer key and one
        // outer row can match several inner ones.
        for (i, (of, tag)) in [(1u64, 7i64), (1, 8), (3, 9)].into_iter().enumerate() {
            store.insert(
                refs,
                compose(&[&fact_ref_field(person_fact(of)), &i64_field(tag)]),
                i as u64 + 1,
            );
        }

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: refs,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterFactId(
                            Address::new(0),
                        )])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
            ]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(0),
                ty: PredicateTy::Int,
            },
        };

        let interner = interner_with(&[]);
        assert_eq!(
            collect_rows(store, plan, &interner).unwrap(),
            vec![Value::Int(10), Value::Int(10), Value::Int(30)],
            "two references name fact 1 (key 10) and one names fact 3 (key 30); \
             nothing names fact 2",
        );
    }

    /// The same compare once the seek prefix has closed: the field's bytes are
    /// checked against the bound row's id as the rows come.
    #[test]
    fn residual_compares_a_bound_rows_fact_id() {
        let (person, links) = (PredicateId(0), PredicateId(1));
        let person_fact = |sequence| FactId::new(person, sequence).unwrap();

        let mut store = MemStore::new();
        for (i, key) in [10i64, 20, 30].into_iter().enumerate() {
            store.insert(person, i64_field(key), i as u64 + 1);
        }
        for (i, (at, of)) in [(7i64, 1u64), (8, 1), (9, 99)].into_iter().enumerate() {
            store.insert(
                links,
                compose(&[&i64_field(at), &fact_ref_field(person_fact(of))]),
                i as u64 + 1,
            );
        }

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: links,
                        seek_key: SeekKey::Prefix(Box::new([])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([Residual {
                        path: FieldPath::field(1),
                        op: ResidualOp::EqRegisterFactId(Address::new(0)),
                    }]),
                ),
            ]),
            head: Project::RegisterField {
                address: Address::new(1),
                path: FieldPath::field(0),
                ty: PredicateTy::Int,
            },
        };

        let interner = interner_with(&[]);
        assert_eq!(
            collect_rows(store, plan, &interner).unwrap(),
            vec![Value::Int(7), Value::Int(8)],
            "only fact 1 (key 10) is referenced; `of = 99` dangles and matches nobody",
        );
    }

    /// A reference splice reads no second fact — [I6](../../../docs/invariants.md#i6)
    /// stays structural. The id is already in the register, so following a reference
    /// costs the scan it narrows and nothing else.
    #[test]
    fn following_a_reference_fetches_no_entity() {
        let (person, refs) = (PredicateId(0), PredicateId(1));

        let mut store = MemStore::new();
        store.insert(person, i64_field(10), 1);
        store.insert(refs, fact_ref_field(FactId::new(person, 1).unwrap()), 1);

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: refs,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterFactId(
                            Address::new(0),
                        )])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
            ]),
            head: Project::FactRef(Address::new(1)),
        };

        let (spy, point_calls) = PointSpy::new(store);
        let interner = interner_with(&[]);

        assert_eq!(
            collect_rows(spy, plan, &interner).unwrap(),
            vec![Value::FactRef(FactId::new(refs, 1).unwrap())],
        );
        assert_eq!(
            point_calls.load(Ordering::Relaxed),
            0,
            "a fact-id splice must not read `entities`",
        );
    }

    // A three-level join (friends-of-friends): Person(a) → Knows(a, b) →
    // Knows(b, c). Only 1→2→3 completes all three levels; every other path dead-
    // ends and backtracks, so exactly one row survives.
    #[test]
    fn three_level_join_friends_of_friends() {
        let person = PredicateId(0);
        let knows = PredicateId(1);

        let mut store = MemStore::new();
        for id in [1, 2, 3] {
            store.insert(person, i64_field(id), id as u64);
        }
        store.insert(knows, compose(&[&i64_field(1), &i64_field(2)]), 10);
        store.insert(knows, compose(&[&i64_field(1), &i64_field(3)]), 11);
        store.insert(knows, compose(&[&i64_field(2), &i64_field(3)]), 12);

        let interner = interner_with(&["a", "b", "c"]);
        let a = interner.get("a").unwrap();
        let b = interner.get("b").unwrap();
        let c = interner.get("c").unwrap();

        let seek_first_on = |reg: usize| {
            SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                address: Address::new(reg),
                path: FieldPath::field(0),
            }]))
        };

        let plan = Plan {
            nvars: 3,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: knows,
                        seek_key: seek_first_on(0),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
                Level::seek(
                    Access {
                        predicate_id: knows,
                        // splice r1's second field (b) into the inner prefix.
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(1),
                            path: FieldPath::field(1),
                        }])),
                    },
                    Box::new([Address::new(2)]),
                    Box::new([]),
                ),
            ]),
            head: Project::Record(Box::new([
                (
                    a,
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    b,
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    c,
                    Project::RegisterField {
                        address: Address::new(2),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        let rows = collect_rows(store, plan, &interner).unwrap();
        assert_eq!(
            rows,
            vec![record(&[
                ("a", Value::Int(1)),
                ("b", Value::Int(2)),
                ("c", Value::Int(3)),
            ])]
        );
    }

    // An `EqRegisterField` residual expresses a cross-loop equality that is not a
    // seek prefix: a self-join of R(x, y) on `inner.x == outer.y`. The inner
    // level scans the whole predicate and the residual filters it.
    #[test]
    fn residual_eq_register_field_cross_loop() {
        let r = PredicateId(0);

        let mut store = MemStore::new();
        store.insert(r, compose(&[&i64_field(1), &i64_field(2)]), 1);
        store.insert(r, compose(&[&i64_field(2), &i64_field(3)]), 2);
        store.insert(r, compose(&[&i64_field(3), &i64_field(1)]), 3);

        let interner = interner_with(&["a", "b"]);
        let a = interner.get("a").unwrap();
        let b = interner.get("b").unwrap();

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(r, 0),
                Level::seek(
                    Access {
                        predicate_id: r,
                        seek_key: SeekKey::Prefix(Box::new([])),
                    },
                    Box::new([Address::new(1)]), // inner.field0 == outer(r0).field1
                    Box::new([Residual {
                        path: FieldPath::field(0),
                        op: ResidualOp::EqRegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(1),
                        },
                    }]),
                ),
            ]),
            head: Project::Record(Box::new([
                (
                    a,
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    b,
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        let rows = collect_rows(store, plan, &interner).unwrap();
        assert_eq!(
            rows,
            vec![
                record(&[("a", Value::Int(1)), ("b", Value::Int(3))]),
                record(&[("a", Value::Int(2)), ("b", Value::Int(1))]),
                record(&[("a", Value::Int(3)), ("b", Value::Int(2))]),
            ]
        );
    }

    // A `FactRef` head projects each matched row's fact id, in key-scan order.
    #[test]
    fn factref_head_yields_fact_ids_in_scan_order() {
        let p = PredicateId(0);

        let mut store = MemStore::new();
        store.insert(p, i64_field(20), 7);
        store.insert(p, i64_field(10), 5);

        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        assert_eq!(
            run(store, plan),
            vec![
                Value::FactRef(FactId::new(p, 5).expect("id")),
                Value::FactRef(FactId::new(p, 7).expect("id")),
            ]
        );
    }

    // A scan over an empty predicate yields no rows.
    #[test]
    fn empty_predicate_yields_no_rows() {
        let p = PredicateId(0);
        let store = MemStore::new();

        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        assert_eq!(run(store, plan), Vec::<Value>::new());
    }

    // ---- derive steps (Phase 6) -------------------------------------------
    //
    // A [`Step::Derive`] is a one-row generator: it computes its value on the way
    // down and reports exhausted on the way back up. These drive it from
    // hand-built plans, because flatten does not emit one yet — so without them
    // the arm would be code with no coverage.

    /// A derive step binding `r0`, for plans that want one.
    fn derive(bind: usize, value: Value) -> Step {
        Step::Derive(DerivedBind {
            bind: Address::new(bind),
            value: Computed::Lit(value),
        })
    }

    /// **A plan with no levels answers exactly one row**, and its head reads the
    /// computed slot.
    ///
    /// The shape `X where X = 42` compiles to, and the one that made the empty-body
    /// rule need revisiting: `body.is_empty()` is still an error, but a body of
    /// derive steps is not empty and is not a loop.
    #[test]
    fn a_plan_of_only_derives_yields_one_row() {
        let plan = Plan {
            nvars: 1,
            body: Box::new([derive(0, Value::Int(42))]),
            head: Project::Computed(Address::new(0)),
        };

        assert_eq!(plan.levels(), 0, "no scan steps, so no loop levels");
        assert_eq!(run(MemStore::new(), plan), vec![Value::Int(42)]);
    }

    /// Two derives in a row, so the head sees both slots — and the machine walks
    /// back down through both to finish.
    #[test]
    fn derives_compose_and_the_run_terminates() {
        let interner = interner_with(&["a", "b"]);
        let plan = Plan {
            nvars: 2,
            body: Box::new([derive(0, Value::Int(1)), derive(1, Value::Int(2))]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").expect("interned"),
                    Project::Computed(Address::new(0)),
                ),
                (
                    interner.get("b").expect("interned"),
                    Project::Computed(Address::new(1)),
                ),
            ])),
        };

        let rows = collect_rows(MemStore::new(), plan, &interner).expect("run");
        assert_eq!(rows.len(), 1, "two one-row steps are still one row");
    }

    /// A derive **above** a scan: computed once, then read on every row the scan
    /// produces. The row count is the scan's, which is what says the derive did not
    /// multiply the answer.
    #[test]
    fn a_derive_above_a_scan_holds_for_every_row() {
        let p = PredicateId(0);
        let mut store = MemStore::new();
        for (i, v) in [10i64, 20, 30].into_iter().enumerate() {
            store.insert(p, i64_field(v), i as u64 + 1);
        }

        let interner = interner_with(&["got", "want"]);
        let plan = Plan {
            nvars: 2,
            body: Box::new([derive(1, Value::Int(7)), Step::Level(scan_all(p, 0))]),
            head: Project::Record(Box::new([
                (
                    interner.get("got").expect("interned"),
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("want").expect("interned"),
                    Project::Computed(Address::new(1)),
                ),
            ])),
        };

        assert_eq!(plan.levels(), 1, "one scan among two steps");
        assert_eq!(plan.body.len(), 2, "...and two steps in the body");

        let rows = collect_rows(store, plan, &interner).expect("run");
        assert_eq!(rows.len(), 3, "one row per scanned fact, not more");
    }

    /// **Recompute-on-restore.** A derive step contributes nothing to the cursor,
    /// so a resume has to recompute it — and the rows either side of every cut
    /// point must be identical.
    ///
    /// This is the purity invariant's guard in the form chapter 7 specifies, and
    /// **the step order is the whole test**. With the derive *below* the scan,
    /// `enumerate` re-enters it from below on the way back up and recomputes it
    /// itself — so a `resume` that skipped its recompute still passed. Deleting the
    /// recompute is only observable when a derive sits *above* a scan: there the
    /// machine backtracks into the scan and ascends through the head without ever
    /// re-entering the derive, so the slot `resume` left behind is the one the head
    /// reads. Both orders are run, because the masking order is the one a careless
    /// change would leave as the only coverage.
    #[test]
    fn a_derive_is_recomputed_across_every_cut_point() {
        let p = PredicateId(0);
        let interner = interner_with(&["n", "z"]);

        // `above` puts the derive before the scan — the order that actually depends
        // on `resume` recomputing.
        for above in [true, false] {
            let where_ = if above { "above" } else { "below" };

            let mk = || {
                let mut store = MemStore::new();
                for (i, v) in [1i64, 2, 3].into_iter().enumerate() {
                    store.insert(p, i64_field(v), i as u64 + 1);
                }

                let scan = Step::Level(scan_all(p, 0));
                let computed = derive(1, Value::Int(99));
                let body: Box<[Step]> = if above {
                    Box::new([computed, scan])
                } else {
                    Box::new([scan, computed])
                };

                let plan = Plan {
                    nvars: 2,
                    body,
                    head: Project::Record(Box::new([
                        (
                            interner.get("n").expect("interned"),
                            Project::RegisterField {
                                address: Address::new(0),
                                path: FieldPath::field(0),
                                ty: PredicateTy::Int,
                            },
                        ),
                        (
                            interner.get("z").expect("interned"),
                            Project::Computed(Address::new(1)),
                        ),
                    ])),
                };

                (store, plan)
            };

            // The structural half: the cursor names levels, not steps.
            let cursor = suspend_after_first_row(mk().0, mk().1);
            assert_eq!(
                cursor.0.len(),
                1,
                "derive {where_} the scan: a two-step plan with one level must save \
                 one row, not two"
            );

            // The behavioural half, at every cut point.
            let context = format!("MemStore, derive {where_} scan");
            let model = assert_resume_equals_uninterrupted(mk, &interner, &context);
            assert_rows(&model, 3);
        }
    }

    // ---- Resume battery (0c) ----------------------------------------------
    //
    // I4 — resume == uninterrupted run. The model is `collect_rows` ("run to
    // completion, collect rows"); the system under test is `run_with_suspends`,
    // which rebuilds the executor from a bytes-only `Cursor` at each cut point.
    // These cases pin the 1-/2-/3-level shapes at *every* cut point
    // deterministically; the schema-first generator drives the same property
    // over generated `(plan, store)` pairs.

    /// Assert resume == uninterrupted for **every** cut point of `mk`'s run:
    /// suspending once after row `k` for each `k` in turn, then suspending after
    /// every row at once. Returns the model rows, so a caller can pin the run's
    /// size (the property must not pass by exercising nothing) or check further
    /// schedules against the same model.
    ///
    /// Generic over the store: the battery is the same against `MemStore` and
    /// against fjall, which is the point — a `Cursor` is bytes-only, so a store
    /// that yields the same rows must resume the same way (PLAN 1d). `context`
    /// names the store in failure messages.
    fn assert_resume_equals_uninterrupted<S: FactStore>(
        mut mk: impl FnMut() -> (S, Plan),
        interner: &LocalInterner,
        context: &str,
    ) -> Vec<Value> {
        let (store, plan) = mk();
        let model = collect_rows(store, plan, interner).unwrap();

        for k in 1..=model.len() {
            let schedule = BTreeSet::from([k]);
            let (rows, suspends) = run_with_suspends(&mut mk, interner, &schedule).unwrap();

            assert_eq!(suspends, 1, "{context}: schedule {{{k}}} never suspended");
            assert_eq!(
                rows,
                model,
                "{context}: suspending after row {k} of {} changed the run",
                model.len()
            );
        }

        // The maximal schedule: a suspend/resume round-trip at every row.
        let every: BTreeSet<usize> = (1..=model.len()).collect();
        let (rows, suspends) = run_with_suspends(&mut mk, interner, &every).unwrap();

        assert_eq!(
            suspends,
            model.len(),
            "{context}: expected one suspend per row"
        );
        assert_eq!(
            rows, model,
            "{context}: suspending after every row changed the run"
        );

        model
    }

    /// The number of rows a deterministic case must produce, asserted separately
    /// so a shape that silently stops matching cannot pass vacuously.
    fn assert_rows(model: &[Value], expected: usize) {
        assert_eq!(
            model.len(),
            expected,
            "model produced {} row(s), expected {expected}",
            model.len()
        );
    }

    /// 1 level: a full scan of one predicate, scalar head.
    fn one_level_scan() -> (MemStore, Plan) {
        let p = PredicateId(0);

        let mut store = MemStore::new();
        for (i, v) in [30i64, 10, 20].into_iter().enumerate() {
            store.insert(p, i64_field(v), i as u64 + 1);
        }

        let plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(0),
                ty: PredicateTy::Int,
            },
        };

        (store, plan)
    }

    /// 2 levels: Person(a) → Knows(a, b) by seek splice. Person 3 has no `Knows`
    /// row, so the inner scan is empty there and the machine backtracks — a
    /// cut point either side of that boundary must still resume exactly.
    fn two_level_seek_join(interner: &LocalInterner) -> (MemStore, Plan) {
        let person = PredicateId(0);
        let knows = PredicateId(1);

        let mut store = MemStore::new();
        for id in [1i64, 2, 3] {
            store.insert(person, i64_field(id), id as u64);
        }
        store.insert(knows, compose(&[&i64_field(1), &i64_field(2)]), 10);
        store.insert(knows, compose(&[&i64_field(1), &i64_field(3)]), 11);
        store.insert(knows, compose(&[&i64_field(2), &i64_field(3)]), 12);

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: knows,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(0),
                        }])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        (store, plan)
    }

    /// 3 levels: Person(a) → Knows(a, b) → Knows(b, c), over a `Knows` relation
    /// with a cycle so several `a` values fan out to more than one row — the run
    /// crosses join cross-product boundaries repeatedly.
    fn three_level_seek_join(interner: &LocalInterner) -> (MemStore, Plan) {
        let person = PredicateId(0);
        let knows = PredicateId(1);

        let mut store = MemStore::new();
        for id in [1i64, 2, 3] {
            store.insert(person, i64_field(id), id as u64);
        }
        for (i, (from, to)) in [(1i64, 2i64), (1, 3), (2, 3), (3, 1)]
            .into_iter()
            .enumerate()
        {
            store.insert(
                knows,
                compose(&[&i64_field(from), &i64_field(to)]),
                10 + i as u64,
            );
        }

        let seek_on = |reg: usize, field_idx: usize| {
            SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                address: Address::new(reg),
                path: FieldPath::field(field_idx),
            }]))
        };

        let plan = Plan {
            nvars: 3,
            body: Step::levels([
                scan_all(person, 0),
                Level::seek(
                    Access {
                        predicate_id: knows,
                        seek_key: seek_on(0, 0),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([]),
                ),
                Level::seek(
                    Access {
                        predicate_id: knows,
                        seek_key: seek_on(1, 1),
                    },
                    Box::new([Address::new(2)]),
                    Box::new([]),
                ),
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("c").unwrap(),
                    Project::RegisterField {
                        address: Address::new(2),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        (store, plan)
    }

    /// 2 levels joined by a cross-loop `EqRegisterField` residual rather than a
    /// seek: resume must restore the outer binding well enough for the *residual*
    /// to keep filtering identically, not just for the scan range to be right.
    fn two_level_residual_join(interner: &LocalInterner) -> (MemStore, Plan) {
        let r = PredicateId(0);

        let mut store = MemStore::new();
        for (i, (x, y)) in [(1i64, 2i64), (2, 3), (3, 1)].into_iter().enumerate() {
            store.insert(r, compose(&[&i64_field(x), &i64_field(y)]), i as u64 + 1);
        }

        let plan = Plan {
            nvars: 2,
            body: Step::levels([
                scan_all(r, 0),
                Level::seek(
                    Access {
                        predicate_id: r,
                        seek_key: SeekKey::Prefix(Box::new([])),
                    },
                    Box::new([Address::new(1)]),
                    Box::new([Residual {
                        path: FieldPath::field(0),
                        op: ResidualOp::EqRegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(1),
                        },
                    }]),
                ),
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        path: FieldPath::field(0),
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        path: FieldPath::field(1),
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        (store, plan)
    }

    #[test]
    fn resume_equals_uninterrupted_one_level() {
        let interner = interner_with(&[]);
        let model = assert_resume_equals_uninterrupted(one_level_scan, &interner, "MemStore");
        assert_rows(&model, 3);
    }

    #[test]
    fn resume_equals_uninterrupted_two_level_seek() {
        let interner = interner_with(&["a", "b"]);
        let model = assert_resume_equals_uninterrupted(
            || two_level_seek_join(&interner),
            &interner,
            "MemStore",
        );
        assert_rows(&model, 3);
    }

    #[test]
    fn resume_equals_uninterrupted_three_level_seek() {
        let interner = interner_with(&["a", "b", "c"]);
        let model = assert_resume_equals_uninterrupted(
            || three_level_seek_join(&interner),
            &interner,
            "MemStore",
        );
        assert_rows(&model, 5);
    }

    #[test]
    fn resume_equals_uninterrupted_cross_loop_residual() {
        let interner = interner_with(&["a", "b"]);
        let model = assert_resume_equals_uninterrupted(
            || two_level_residual_join(&interner),
            &interner,
            "MemStore",
        );
        assert_rows(&model, 3);
    }

    proptest! {
        // This is the executor's headline gate, and a case is cheap (the whole
        // battery runs in well under a second), so take four times the default.
        #![proptest_config(ProptestConfig::with_cases(1024))]

        // I4 — resume == uninterrupted run. **The executor's headline acceptance
        // gate.** Over schema-first `(plan, store)` pairs (1-/2-/3-level, seeks,
        // constant and cross-loop residuals): the row sequence is invariant under
        // suspension at every single cut point, under a generated interruption
        // schedule, and under suspending after every row — no duplicates, no
        // skips, including across join cross-product boundaries.
        #[test]
        fn resume_equals_uninterrupted(
            spec in arb_plan_and_store(),
            schedule in arb_interruption_schedule(),
        ) {
            let interner = spec.interner();
            let mut mk = || spec.build(&interner);

            // Every single cut point, and the maximal schedule.
            let context = format!("MemStore, {} level(s)", spec.levels());
            let model = assert_resume_equals_uninterrupted(&mut mk, &interner, &context);

            // Then the generated schedule.
            let cuts = cut_points(&schedule, model.len());
            let (rows, suspends) = run_with_suspends(&mut mk, &interner, &cuts).unwrap();

            assert_eq!(suspends, cuts.len(), "expected one suspend per scheduled row");
            assert_eq!(rows, model, "schedule {cuts:?} changed the run");
        }
    }

    // ---- The same battery, against fjall (1d) -----------------------------
    //
    // I4 is only half-tested on `MemStore`: a `Cursor` is bytes-only and a resume
    // re-seeks by exactly those bytes, so what matters is that a *real* store —
    // LSM iterators, a snapshot per segment, rows arriving as `Slice`s rather
    // than cloned `Vec`s — reproduces the run identically. This is also the only
    // place [I8](../../docs/invariants.md#i8) is testable at all; its guard lives
    // in `store` alongside the drop probe.

    /// Seed a fjall DB with the spec's facts, in the spec's order.
    ///
    /// The returned ids are asserted to be exactly what the spec numbers them,
    /// which pins that the real per-predicate allocator and the generator's
    /// deterministic order agree — without that, the two stores would hold the
    /// same rows under different ids and a `FactRef` head would diverge.
    fn seed_fjall(spec: &PlanAndStore, path: &std::path::Path) -> FjallDb {
        let db = FjallDb::open(path).expect("open");

        for (predicate, key, sequence) in spec.facts() {
            let id = db.put_fact(predicate, &key, &[]).expect("put");
            assert_eq!(
                id,
                FactId::new(predicate, sequence).expect("spec fact id"),
                "the allocator diverged from the spec's fact order"
            );
        }

        db
    }

    proptest! {
        // A case builds a real DB — keyspace creation is fsync-bound at ~30 ms a
        // tree, and a spec has up to three predicates, so a case costs ~100 ms
        // against the MemStore battery's microseconds. Enough cases to be a real
        // battery, not 1024 of them; the shapes themselves are already covered
        // exhaustively above, and what is under test here is the store beneath
        // them.
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// I4 — resume == uninterrupted run, **against fjall**, at every cut point
        /// and under a generated schedule.
        ///
        /// Also differential: the fjall run must equal the `MemStore` run for the
        /// same spec, row for row and id for id. That is what licenses every other
        /// executor battery to be written against `MemStore` alone.
        #[test]
        fn resume_equals_uninterrupted_on_fjall(
            spec in arb_plan_and_store(),
            schedule in arb_interruption_schedule(),
        ) {
            let interner = spec.interner();
            let dir = TempDir::new().expect("tempdir");
            let db = seed_fjall(&spec, dir.path());

            let mut mk = || (db.reader(), spec.build_plan(&interner));

            let context = format!("fjall, {} level(s)", spec.levels());
            let model = assert_resume_equals_uninterrupted(&mut mk, &interner, &context);

            let cuts = cut_points(&schedule, model.len());
            let (rows, suspends) = run_with_suspends(&mut mk, &interner, &cuts).unwrap();

            assert_eq!(suspends, cuts.len(), "expected one suspend per scheduled row");
            assert_eq!(rows, model, "schedule {cuts:?} changed the run on fjall");

            // The differential: the same spec on the in-memory model.
            let (mem, plan) = spec.build(&interner);
            let mem_rows = collect_rows(mem, plan, &interner).unwrap();

            assert_eq!(
                model, mem_rows,
                "fjall and MemStore disagree on the same spec ({} level(s))",
                spec.levels()
            );
        }
    }

    // ---- NFR guards (0a) --------------------------------------------------
    //
    // Non-functional invariants are tested mechanically, not eyeballed: a
    // decode counter (I5), a `point()` spy (I6), and an allocation-counting
    // allocator (I9). See `docs/testing.md`.

    // I5 — a register holds the whole row; fields decode lazily. Binding N
    // variables is N refcount bumps and *zero* field decodes; decoding happens
    // only at a read site (projection).
    #[test]
    fn bind_is_refcount_not_decode() {
        let p = PredicateId(0);

        let mut store = MemStore::new();
        store.insert(p, compose(&[&i64_field(1), &i64_field(2)]), 1);
        store.insert(p, compose(&[&i64_field(3), &i64_field(4)]), 2);

        // Three variables bind to each whole row; no residuals; no projection.
        let bind_plan = Plan {
            nvars: 3,
            body: Step::levels([Level::seek(
                Access {
                    predicate_id: p,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                Box::new([Address::new(0), Address::new(1), Address::new(2)]),
                Box::new([]),
            )]),
            head: Project::FactRef(Address::new(0)),
        };

        decode_probe::reset();
        let n = count_rows(store, bind_plan).unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            decode_probe::count(),
            0,
            "binding decoded {} field(s); binding must be refcount-only (I5)",
            decode_probe::count()
        );

        // Positive control: projecting a key field *does* decode.
        let mut store2 = MemStore::new();
        store2.insert(p, compose(&[&i64_field(1), &i64_field(2)]), 1);
        let proj_plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(1),
                ty: PredicateTy::Int,
            },
        };

        decode_probe::reset();
        let rows = collect_rows(store2, proj_plan, &interner_with(&[])).unwrap();
        assert_eq!(rows, vec![Value::Int(2)]);
        assert!(
            decode_probe::count() > 0,
            "projecting a field must decode (I5 positive control)"
        );
    }

    // I6 — values never enter the scan hot loop. A key-only query (scan +
    // key-field residual + key-field projection) never fetches from `entities`.
    #[test]
    fn no_value_fetch_in_scan() {
        let p = PredicateId(0);

        let mut store = MemStore::new();
        store.insert_valued(p, i64_field(1), 1, i64_field(100));
        store.insert_valued(p, i64_field(2), 2, i64_field(200));

        let (spy, calls) = PointSpy::new(store);
        let plan = Plan {
            nvars: 1,
            body: Step::levels([Level::seek(
                Access {
                    predicate_id: p,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                Box::new([Address::new(0)]),
                Box::new([Residual {
                    path: FieldPath::field(0),
                    op: ResidualOp::EqConst(i64_field(2).into_boxed_slice()),
                }]),
            )]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(0),
                ty: PredicateTy::Int,
            },
        };

        let rows = collect_rows(spy, plan, &interner_with(&[])).unwrap();
        assert_eq!(rows, vec![Value::Int(2)]);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "point() (value fetch) called during a key-only query (I6)"
        );

        // Positive control: a `Value` head fetches from `entities` via point().
        let mut store2 = MemStore::new();
        store2.insert_valued(p, i64_field(1), 1, i64_field(100));
        let (spy2, calls2) = PointSpy::new(store2);
        let value_plan = Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, 0)]),
            head: Project::Value {
                address: Address::new(0),
                ty: PredicateTy::Int,
            },
        };

        let rows2 = collect_rows(spy2, value_plan, &interner_with(&[])).unwrap();
        assert_eq!(rows2, vec![Value::Int(100)]);
        assert!(
            calls2.load(Ordering::Relaxed) > 0,
            "a Value head must fetch via point() (I6 positive control)"
        );
    }

    // I9 — the hot path is allocation-free per row. Scanning N rows and 2N rows
    // (over the alloc-free `FrozenStore`, without projecting) allocates the same
    // amount: the difference is only the per-row scan work, so equal counts mean
    // zero allocations per row. Non-row-scaling costs (frame open, executor
    // setup) are constant and cancel.
    //
    // Bytes are asserted alongside counts: a single buffer sized by the row count
    // (materialising the result set — the anti-pattern I9 exists to forbid) is one
    // allocation either way, and only the volume gives it away.
    #[test]
    fn scan_is_alloc_free_per_row() {
        // The counting allocator ships inside `allocation-counter` and is only
        // linked because it is a dev-dependency. If that wiring ever breaks,
        // `measure` reports zeroes and every comparison below holds vacuously —
        // so prove the probe sees a known allocation first.
        let control = allocation_counter::measure(|| {
            std::hint::black_box(Vec::<u8>::with_capacity(4096));
        });
        assert!(
            control.count_total > 0 && control.bytes_total >= 4096,
            "counting allocator is not installed; the I9 guard would pass vacuously: {control:?}"
        );

        let p = PredicateId(0);

        // Sequences are 1-based: sequence 0 is reserved, so `FactId::new` rejects
        // it ([I11](../../docs/invariants.md#i11)).
        let store_n = FrozenStore::from_keys(p, (1..=64u64).map(|i| (i64_field(i as i64), i)));
        let store_2n = FrozenStore::from_keys(p, (1..=128u64).map(|i| (i64_field(i as i64), i)));

        let plan = |bind| Plan {
            nvars: 1,
            body: Step::levels([scan_all(p, bind)]),
            head: Project::FactRef(Address::new(0)),
        };

        let mut n1 = 0;
        let mut n2 = 0;
        let info_n = allocation_counter::measure(|| n1 = count_rows(store_n, plan(0)).unwrap());
        let info_2n = allocation_counter::measure(|| n2 = count_rows(store_2n, plan(0)).unwrap());

        assert_eq!(n1, 64);
        assert_eq!(n2, 128);
        let (allocs_n, allocs_2n) = (info_n.count_total, info_2n.count_total);
        assert_eq!(
            allocs_n, allocs_2n,
            "hot path is not alloc-free per row: {allocs_n} allocs for 64 rows vs {allocs_2n} for 128"
        );
        let (bytes_n, bytes_2n) = (info_n.bytes_total, info_2n.bytes_total);
        assert_eq!(
            bytes_n, bytes_2n,
            "hot path allocates per row by volume: {bytes_n} bytes for 64 rows vs {bytes_2n} for 128"
        );
    }
}
