use std::{fmt, ops::Range};

use byteview::ByteView;
use tinyvec::ArrayVec;
use tokio_util::sync::CancellationToken;

use crate::focus::{
    error::{ApertureError, StoreCodecError},
    plan::{
        FactId, FactStore, Generator, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart,
    },
    schema::{LocalInterner, PREDICATE_ID_SIZE},
    tuple::{Value, decode_typed, skip, strinc},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address(pub(crate) usize);

impl Address {
    pub fn new(i: usize) -> Self {
        Self(i)
    }
}

impl fmt::LowerHex for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
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

pub struct MachineState {
    pub registers: Box<[Option<Register>]>,
}

impl MachineState {
    pub fn new(nvars: usize) -> Self {
        Self {
            registers: vec![None; nvars].into_boxed_slice(),
        }
    }

    pub fn get(&self, address: Address) -> Result<&Register, ApertureError> {
        self.registers
            .get(address.0)
            .ok_or(ApertureError::AddressOutOfBounds(address))?
            .as_ref()
            .ok_or(ApertureError::UseBeforeBind(address))
    }
}

const FIELD_OFFSETS_CAPACITY: usize = 16;

#[derive(Debug, Clone)]
pub struct FieldOffsets(ArrayVec<[usize; FIELD_OFFSETS_CAPACITY]>);

impl Default for FieldOffsets {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldOffsets {
    pub fn new() -> Self {
        Self(ArrayVec::new())
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn get(&mut self, key: &ByteView, idx: usize) -> Result<Range<usize>, StoreCodecError> {
        if let Some(&end) = self.0.get(idx) {
            return Ok(if idx == 0 {
                0..end
            } else {
                self.0[idx - 1]..end
            });
        }
        let mut i = self.0.len();
        let mut start = if i == 0 { 0 } else { self.0[i - 1] };
        loop {
            let end = skip(key, start, false)?;
            if i < FIELD_OFFSETS_CAPACITY {
                self.0.push(end);
            }
            if i == idx {
                return Ok(start..end);
            }
            i += 1;
            start = end;
        }
    }
}

pub const CANCELLATION_STRIDE: usize = 4096;

pub struct StackFrame<S: FactStore> {
    scan: Option<S::Scan>,
    current: Option<Register>,
    field_offsets: Box<[FieldOffsets]>,
}

impl<S: FactStore> StackFrame<S> {
    pub fn closed(nvars: usize) -> Self {
        Self {
            scan: None,
            current: None,
            field_offsets: vec![FieldOffsets::new(); nvars].into_boxed_slice(),
        }
    }

    pub fn open(
        &mut self,
        store: &S,
        generator: &Generator,
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

        let prefix = self.build_prefix(state, generator)?;
        let hi = strinc(&prefix);
        let lo = resume_at.unwrap_or(&prefix);

        self.scan = Some(store.scan(lo, hi.as_deref()));
        self.current = None;

        Ok(())
    }

    fn get_field_span(
        field_offsets: &mut Box<[FieldOffsets]>,
        key: &ByteView,
        var: Address,
        idx: usize,
    ) -> Result<Range<usize>, ApertureError> {
        field_offsets
            .get_mut(var.0)
            .ok_or(ApertureError::AddressOutOfBounds(var))?
            .get(key, idx)
            .map_err(ApertureError::DecodeError)
    }

    pub fn build_prefix(
        &mut self,
        state: &MachineState,
        generator: &Generator,
    ) -> Result<Vec<u8>, ApertureError> {
        let mut prefix = generator.access.predicate_id.0.to_be_bytes().to_vec();

        match &generator.access.seek_key {
            SeekKey::Prefix(bytes) => prefix.extend_from_slice(bytes.as_ref()),
            SeekKey::Composite(parts) => {
                for part in parts.iter() {
                    match part {
                        SeekKeyPart::Bytes(bytes) => prefix.extend_from_slice(bytes.as_ref()),
                        SeekKeyPart::RegisterField {
                            address: var_address,
                            field_idx,
                        } => {
                            let key = state.get(*var_address)?.key();
                            let span = Self::get_field_span(
                                &mut self.field_offsets,
                                &key,
                                *var_address,
                                *field_idx,
                            )?;
                            prefix.extend_from_slice(&key[span]);
                        }
                    }
                }
            }
        }
        Ok(prefix)
    }

    pub fn next(
        &mut self,
        state: &MachineState,
        generator: &Generator,
        cancellation_token: &CancellationToken,
    ) -> Result<Option<Register>, ApertureError> {
        let scan = self.scan.as_mut().ok_or(ApertureError::AdvanceAfterClose)?;
        let mut since_check: usize = 0;

        for row in scan {
            since_check += 1;
            if since_check == CANCELLATION_STRIDE {
                if cancellation_token.is_cancelled() {
                    return Err(ApertureError::Cancelled);
                }
                since_check = 0;
            }

            let (key_bytes, fact_id) = row?;
            let current = Register {
                fact_id,
                bytes: key_bytes,
            };

            if Self::check_residuals(
                &mut self.field_offsets,
                state,
                &generator.residuals,
                &current,
            )? {
                self.current = Some(current.clone());
                return Ok(Some(current));
            }
        }

        Ok(None)
    }

    pub fn check_residuals(
        frame_field_offsets: &mut Box<[FieldOffsets]>,
        state: &MachineState,
        residuals: &[Residual],
        register: &Register,
    ) -> Result<bool, ApertureError> {
        let key = register.key();
        let mut row_field_offsets = FieldOffsets::new();

        for residual in residuals.iter() {
            let span = row_field_offsets
                .get(&key, residual.field_idx)
                .map_err(ApertureError::DecodeError)?;
            let field = &key[span];

            let ok = match &residual.op {
                ResidualOp::EqConst(const_bytes) => field == const_bytes.as_ref(),
                ResidualOp::Prefix(prefix_bytes) => field.starts_with(prefix_bytes.as_ref()),
                ResidualOp::EqRegisterField {
                    address: var_address,
                    field_idx,
                } => {
                    let other = state.get(*var_address)?;
                    let other_key = other.key();
                    let other_span = Self::get_field_span(
                        frame_field_offsets,
                        &other_key,
                        *var_address,
                        *field_idx,
                    )?;
                    field == &other_key[other_span]
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
}

pub struct Cursor(Vec<Register>);

pub struct Row<'a, S: FactStore> {
    store: &'a S,
    state: &'a MachineState,
    plan: &'a Plan,
}

impl<'a, S: FactStore> Row<'a, S> {
    pub fn to_value(&self, interner: &LocalInterner) -> Result<Value, ApertureError> {
        project(interner, &self.plan.head, self.state, self.store)
    }
}

fn project<S: FactStore>(
    interner: &LocalInterner,
    p: &Project,
    state: &MachineState,
    store: &S,
) -> Result<Value, ApertureError> {
    match p {
        Project::Lit(v) => Ok(v.clone()),

        Project::FactRef(address) => Ok(Value::FactRef(state.get(*address)?.fact_id)),

        Project::RegisterField {
            address,
            field_idx,
            ty,
        } => {
            let reg = state.get(*address)?;
            let key = reg.key();

            let mut offsets = FieldOffsets::new();

            let span = offsets
                .get(&key, *field_idx)
                .map_err(ApertureError::DecodeError)?;

            let field = &key[span];

            decode_typed(interner, field, ty)
        }

        Project::Value { address, ty } => {
            // The value lives in the `entities` CF, not in the register (which
            // holds `predicate_id ++ key`). Fetch it by fact id — the one place
            // a value is read (I6) — and decode the value bytes.
            let reg = state.get(*address)?;
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

                let value = project(interner, field_proj, state, store)?;

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
        }
    }

    pub fn build_cursor(&self) -> Cursor {
        Cursor(
            self.stack
                .iter()
                .filter_map(|f| f.current.as_ref().map(|r| r.to_detached()))
                .collect(),
        )
    }

    pub fn resume(store: S, plan: Plan, cursor: Cursor) -> Result<Self, ApertureError> {
        let mut ex = Executor::new(store, plan);

        if cursor.0.is_empty() {
            return Ok(ex);
        }

        let cancel = CancellationToken::new();

        for (level, saved) in cursor.0.iter().enumerate() {
            let generator = &ex.plan.body[level];
            let frame = &mut ex.stack[level];

            frame.open(&ex.store, generator, &ex.state, Some(&saved.bytes))?;

            let row = frame
                .next(&ex.state, generator, &cancel)?
                .ok_or(ApertureError::BadResumeKey)?;

            if row.fact_id != saved.fact_id {
                return Err(ApertureError::BadResumeKey);
            }

            for var_address in generator.binds.iter() {
                ex.state.registers[var_address.0] = Some(row.clone());
            }
            frame.current = Some(row);
        }

        ex.depth = cursor.0.len() - 1;
        Ok(ex)
    }

    pub fn enumerate<A>(
        &mut self,
        init: A,
        mut step: impl FnMut(A, Row<'_, S>) -> Result<Stream<A>, ApertureError>,
        cancellation_token: &CancellationToken,
    ) -> Result<Iteratee<A>, ApertureError> {
        let mut acc = init;

        loop {
            if self.depth == self.plan.body.len() {
                let row = Row {
                    store: &self.store,
                    state: &self.state,
                    plan: &self.plan,
                };
                match step(acc, row)? {
                    Stream::Continue(next) => {
                        acc = next;
                        self.depth -= 1;
                        continue;
                    }
                    Stream::Suspend(next) => {
                        acc = next;
                        self.depth -= 1;
                        return Ok(Iteratee::Suspended(acc, self.build_cursor()));
                    }
                }
            }

            let generator = &self.plan.body[self.depth];
            let frame = &mut self.stack[self.depth];

            if frame.scan.is_none() {
                frame.open(&self.store, generator, &self.state, None)?;
            }

            match frame.next(&self.state, generator, cancellation_token)? {
                Some(register) => {
                    for var_address in generator.binds.iter() {
                        let slot = self
                            .state
                            .registers
                            .get_mut(var_address.0)
                            .ok_or(ApertureError::AddressOutOfBounds(*var_address))?;
                        *slot = Some(register.clone());
                    }
                    frame.current = Some(register);
                    self.depth += 1;
                }
                None => {
                    frame.scan = None;
                    frame.current = None;
                    if self.depth == 0 {
                        return Ok(Iteratee::Done(acc));
                    }
                    self.depth -= 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{
        fixtures::{
            FrozenStore, PointSpy, collect_rows, compose, count_rows, i64_field, interner_with,
            run_with_suspends, str_field,
        },
        mem_store::MemStore,
        plan::{
            Access, FactId, Generator, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart,
            proptest::{arb_interruption_schedule, arb_plan_and_store, cut_points},
        },
        schema::{PredicateId, PredicateTy},
        tuple::{Value, decode_probe},
    };
    use ::proptest::prelude::*;
    use std::{collections::BTreeSet, sync::atomic::Ordering};

    /// Run a plan whose head projects only scalars (no record field names to
    /// resolve). Record-head tests call [`collect_rows`] with their own interner.
    fn run(store: MemStore, plan: Plan) -> Vec<Value> {
        collect_rows(store, plan, &interner_with(&[])).unwrap()
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
            body: Box::new([Generator {
                access: Access {
                    predicate_id: pred,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                binds: Box::new([Address::new(0)]),
                residuals: Box::new([Residual {
                    field_idx: 0,
                    op: ResidualOp::EqConst(str_field("beta").into_boxed_slice()),
                }]),
            }]),
            head: Project::RegisterField {
                address: Address::new(0),
                field_idx: 0,
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
            body: Box::new([Generator {
                access: Access {
                    predicate_id: pred,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                binds: Box::new([Address::new(0)]),
                residuals: Box::new([]),
            }]),
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
    fn scan_all(predicate_id: PredicateId, bind: usize) -> Generator {
        Generator {
            access: Access {
                predicate_id,
                seek_key: SeekKey::Prefix(Box::new([])),
            },
            binds: Box::new([Address::new(bind)]),
            residuals: Box::new([]),
        }
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
            body: Box::new([scan_all(p, 0)]),
            head: Project::RegisterField {
                address: Address::new(0),
                field_idx: 0,
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
            body: Box::new([Generator {
                access: Access {
                    predicate_id: p,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                binds: Box::new([Address::new(0)]),
                residuals: Box::new([Residual {
                    field_idx: 0,
                    op: ResidualOp::Prefix(prefix.into_boxed_slice()),
                }]),
            }]),
            head: Project::RegisterField {
                address: Address::new(0),
                field_idx: 0,
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
            body: Box::new([
                scan_all(person, 0),
                Generator {
                    access: Access {
                        predicate_id: knows,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            field_idx: 0,
                        }])),
                    },
                    binds: Box::new([Address::new(1)]),
                    residuals: Box::new([]),
                },
            ]),
            head: Project::Record(Box::new([
                (
                    a,
                    Project::RegisterField {
                        address: Address::new(0),
                        field_idx: 0,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    b,
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 1,
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
            body: Box::new([
                scan_all(outer, 0),
                Generator {
                    access: Access {
                        predicate_id: inner,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            field_idx: 0,
                        }])),
                    },
                    binds: Box::new([Address::new(1)]),
                    // Fills this frame's offset cache for register 0 mid-scan.
                    residuals: Box::new([Residual {
                        field_idx: 1,
                        op: ResidualOp::EqRegisterField {
                            address: Address::new(0),
                            field_idx: 1,
                        },
                    }]),
                },
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        field_idx: 0,
                        ty: PredicateTy::Str,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 0,
                        ty: PredicateTy::Str,
                    },
                ),
                (
                    interner.get("c").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 1,
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
                field_idx: 0,
            }]))
        };

        let plan = Plan {
            nvars: 3,
            body: Box::new([
                scan_all(person, 0),
                Generator {
                    access: Access {
                        predicate_id: knows,
                        seek_key: seek_first_on(0),
                    },
                    binds: Box::new([Address::new(1)]),
                    residuals: Box::new([]),
                },
                Generator {
                    access: Access {
                        predicate_id: knows,
                        // splice r1's second field (b) into the inner prefix.
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(1),
                            field_idx: 1,
                        }])),
                    },
                    binds: Box::new([Address::new(2)]),
                    residuals: Box::new([]),
                },
            ]),
            head: Project::Record(Box::new([
                (
                    a,
                    Project::RegisterField {
                        address: Address::new(0),
                        field_idx: 0,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    b,
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 1,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    c,
                    Project::RegisterField {
                        address: Address::new(2),
                        field_idx: 1,
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
            body: Box::new([
                scan_all(r, 0),
                Generator {
                    access: Access {
                        predicate_id: r,
                        seek_key: SeekKey::Prefix(Box::new([])),
                    },
                    binds: Box::new([Address::new(1)]),
                    // inner.field0 == outer(r0).field1
                    residuals: Box::new([Residual {
                        field_idx: 0,
                        op: ResidualOp::EqRegisterField {
                            address: Address::new(0),
                            field_idx: 1,
                        },
                    }]),
                },
            ]),
            head: Project::Record(Box::new([
                (
                    a,
                    Project::RegisterField {
                        address: Address::new(0),
                        field_idx: 0,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    b,
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 1,
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
            body: Box::new([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        assert_eq!(
            run(store, plan),
            vec![Value::FactRef(FactId(5)), Value::FactRef(FactId(7))]
        );
    }

    // A scan over an empty predicate yields no rows.
    #[test]
    fn empty_predicate_yields_no_rows() {
        let p = PredicateId(0);
        let store = MemStore::new();

        let plan = Plan {
            nvars: 1,
            body: Box::new([scan_all(p, 0)]),
            head: Project::FactRef(Address::new(0)),
        };

        assert_eq!(run(store, plan), Vec::<Value>::new());
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
    /// every row at once. `expected_rows` pins the run's size so the property
    /// can't pass by exercising nothing.
    fn assert_resume_equals_uninterrupted(
        mut mk: impl FnMut() -> (MemStore, Plan),
        interner: &LocalInterner,
        expected_rows: usize,
    ) {
        let (store, plan) = mk();
        let model = collect_rows(store, plan, interner).unwrap();

        assert_eq!(
            model.len(),
            expected_rows,
            "model produced {} row(s), expected {expected_rows}",
            model.len()
        );

        for k in 1..=model.len() {
            let schedule = BTreeSet::from([k]);
            let (rows, suspends) = run_with_suspends(&mut mk, interner, &schedule).unwrap();

            assert_eq!(suspends, 1, "schedule {{{k}}} never suspended");
            assert_eq!(rows, model, "suspending after row {k} changed the run");
        }

        // The maximal schedule: a suspend/resume round-trip at every row.
        let every: BTreeSet<usize> = (1..=model.len()).collect();
        let (rows, suspends) = run_with_suspends(&mut mk, interner, &every).unwrap();

        assert_eq!(suspends, model.len(), "expected one suspend per row");
        assert_eq!(rows, model, "suspending after every row changed the run");
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
            body: Box::new([scan_all(p, 0)]),
            head: Project::RegisterField {
                address: Address::new(0),
                field_idx: 0,
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
            body: Box::new([
                scan_all(person, 0),
                Generator {
                    access: Access {
                        predicate_id: knows,
                        seek_key: SeekKey::Composite(Box::new([SeekKeyPart::RegisterField {
                            address: Address::new(0),
                            field_idx: 0,
                        }])),
                    },
                    binds: Box::new([Address::new(1)]),
                    residuals: Box::new([]),
                },
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        field_idx: 0,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 1,
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
                field_idx,
            }]))
        };

        let plan = Plan {
            nvars: 3,
            body: Box::new([
                scan_all(person, 0),
                Generator {
                    access: Access {
                        predicate_id: knows,
                        seek_key: seek_on(0, 0),
                    },
                    binds: Box::new([Address::new(1)]),
                    residuals: Box::new([]),
                },
                Generator {
                    access: Access {
                        predicate_id: knows,
                        seek_key: seek_on(1, 1),
                    },
                    binds: Box::new([Address::new(2)]),
                    residuals: Box::new([]),
                },
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        field_idx: 0,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 1,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("c").unwrap(),
                    Project::RegisterField {
                        address: Address::new(2),
                        field_idx: 1,
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
            body: Box::new([
                scan_all(r, 0),
                Generator {
                    access: Access {
                        predicate_id: r,
                        seek_key: SeekKey::Prefix(Box::new([])),
                    },
                    binds: Box::new([Address::new(1)]),
                    residuals: Box::new([Residual {
                        field_idx: 0,
                        op: ResidualOp::EqRegisterField {
                            address: Address::new(0),
                            field_idx: 1,
                        },
                    }]),
                },
            ]),
            head: Project::Record(Box::new([
                (
                    interner.get("a").unwrap(),
                    Project::RegisterField {
                        address: Address::new(0),
                        field_idx: 0,
                        ty: PredicateTy::Int,
                    },
                ),
                (
                    interner.get("b").unwrap(),
                    Project::RegisterField {
                        address: Address::new(1),
                        field_idx: 1,
                        ty: PredicateTy::Int,
                    },
                ),
            ])),
        };

        (store, plan)
    }

    #[test]
    fn resume_equals_uninterrupted_one_level() {
        assert_resume_equals_uninterrupted(one_level_scan, &interner_with(&[]), 3);
    }

    #[test]
    fn resume_equals_uninterrupted_two_level_seek() {
        let interner = interner_with(&["a", "b"]);
        assert_resume_equals_uninterrupted(|| two_level_seek_join(&interner), &interner, 3);
    }

    #[test]
    fn resume_equals_uninterrupted_three_level_seek() {
        let interner = interner_with(&["a", "b", "c"]);
        assert_resume_equals_uninterrupted(|| three_level_seek_join(&interner), &interner, 5);
    }

    #[test]
    fn resume_equals_uninterrupted_cross_loop_residual() {
        let interner = interner_with(&["a", "b"]);
        assert_resume_equals_uninterrupted(|| two_level_residual_join(&interner), &interner, 3);
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

            let (store, plan) = mk();
            let model = collect_rows(store, plan, &interner).unwrap();

            // Every single cut point.
            for k in 1..=model.len() {
                let cut = BTreeSet::from([k]);
                let (rows, suspends) = run_with_suspends(&mut mk, &interner, &cut).unwrap();

                assert_eq!(suspends, 1, "schedule {{{k}}} never suspended");
                assert_eq!(
                    rows, model,
                    "suspending after row {k} of {} changed the run ({} level(s))",
                    model.len(),
                    spec.levels()
                );
            }

            // The generated schedule, then the maximal one (suspend at every row).
            let every: BTreeSet<usize> = (1..=model.len()).collect();

            for cuts in [cut_points(&schedule, model.len()), every] {
                let (rows, suspends) = run_with_suspends(&mut mk, &interner, &cuts).unwrap();

                assert_eq!(suspends, cuts.len(), "expected one suspend per scheduled row");
                assert_eq!(rows, model, "schedule {cuts:?} changed the run");
            }
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
            body: Box::new([Generator {
                access: Access {
                    predicate_id: p,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                binds: Box::new([Address::new(0), Address::new(1), Address::new(2)]),
                residuals: Box::new([]),
            }]),
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
            body: Box::new([scan_all(p, 0)]),
            head: Project::RegisterField {
                address: Address::new(0),
                field_idx: 1,
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
            body: Box::new([Generator {
                access: Access {
                    predicate_id: p,
                    seek_key: SeekKey::Prefix(Box::new([])),
                },
                binds: Box::new([Address::new(0)]),
                residuals: Box::new([Residual {
                    field_idx: 0,
                    op: ResidualOp::EqConst(i64_field(2).into_boxed_slice()),
                }]),
            }]),
            head: Project::RegisterField {
                address: Address::new(0),
                field_idx: 0,
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
            body: Box::new([scan_all(p, 0)]),
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

        let store_n = FrozenStore::from_keys(p, (0..64u64).map(|i| (i64_field(i as i64), i)));
        let store_2n = FrozenStore::from_keys(p, (0..128u64).map(|i| (i64_field(i as i64), i)));

        let plan = |bind| Plan {
            nvars: 1,
            body: Box::new([scan_all(p, bind)]),
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
