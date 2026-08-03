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
            .get(address.0 as usize)
            .ok_or(ApertureError::AddressOutOfBounds(address))?
            .as_ref()
            .ok_or(ApertureError::UseBeforeBind(address))
    }
}

const FIELD_OFFSETS_CAPACITY: usize = 16;

#[derive(Debug, Clone)]
pub struct FieldOffsets(ArrayVec<[usize; FIELD_OFFSETS_CAPACITY]>);

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
            let end = skip(&key, start, false)?;
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
        let prefix = self.build_prefix(state, generator)?;
        let hi = strinc(&prefix);
        let lo = resume_at.unwrap_or(&prefix);

        self.scan = Some(store.scan(lo, hi.as_deref()));
        self.current = None;
        self.field_offsets.iter_mut().for_each(|fo| fo.clear());

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
            .map_err(|e| ApertureError::DecodeError(e))
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
            let reg = state.get(*address)?;
            decode_typed(interner, &reg.bytes, ty)
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
        mem_store::MemStore,
        plan::{Access, Generator, Plan, Project, Residual, ResidualOp, SeekKey},
        schema::{LocalInterner, PredicateId, PredicateTy, SchemaInterner},
        tuple::{Value, put_str},
    };
    use lasso::Rodeo;
    use tokio_util::sync::CancellationToken;

    fn enc_str(s: &str) -> Vec<u8> {
        let mut b = Vec::new();
        put_str(&mut b, s);
        b
    }

    fn run(store: MemStore, plan: Plan) -> Vec<Value> {
        let interner = LocalInterner::new(SchemaInterner::new(Rodeo::new().into_reader()));
        let cancel = CancellationToken::new();
        let mut ex = Executor::new(store, plan);

        let out = ex
            .enumerate(
                Vec::<Value>::new(),
                |mut acc, row| {
                    acc.push(row.to_value(&interner)?);
                    Ok(Stream::Continue(acc))
                },
                &cancel,
            )
            .expect("query failed");

        match out {
            Iteratee::Done(v) | Iteratee::Suspended(v, _) => v,
        }
    }

    // A residual on a key field is evaluated against the field's value (the
    // stripped key, predicate-id prefix removed), consistently with seek splices
    // and projection — so it filters on the field, not on the prefix bytes.
    #[test]
    fn residual_eq_const_on_key_field_filters_correctly() {
        let pred = PredicateId(0);

        let mut store = MemStore::new();
        store.insert(pred, enc_str("alpha"), 1);
        store.insert(pred, enc_str("beta"), 2);
        store.insert(pred, enc_str("gamma"), 3);

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
                    op: ResidualOp::EqConst(enc_str("beta").into_boxed_slice()),
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
}
