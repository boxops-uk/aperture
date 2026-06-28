use std::sync::Arc;

use byteview::ByteView;

use crate::focus::{
    error::StoreError,
    plan::{FactId, Generator, Plan, Residual, ResidualOp, SeekKey, SeekKeyPart, Store, VarId},
    schema::PREDICATE_ID_SIZE,
    transport::{field_range, strinc},
};

#[derive(Debug, Clone)]
pub enum Slot {
    Fact { id: FactId, key: ByteView },
    Value(ByteView),
}

impl Slot {
    pub fn bytes(&self) -> ByteView {
        match self {
            Slot::Fact { key, .. } => key.clone(),
            Slot::Value(value) => value.clone(),
        }
    }

    pub fn value(&self) -> ByteView {
        match self {
            Slot::Fact { key, .. } => key.slice(PREDICATE_ID_SIZE..),
            Slot::Value(value) => value.clone(),
        }
    }

    pub fn field(&self, idx: usize) -> Result<ByteView, StoreError> {
        let value = self.value();
        let range = field_range(value.as_ref(), idx)?;
        Ok(value.slice(range))
    }
}

pub struct Env {
    slots: Box<[Option<Slot>]>,
}

impl Env {
    pub fn new(nvars: usize) -> Self {
        Self {
            slots: vec![None; nvars].into_boxed_slice(),
        }
    }

    pub fn get(&self, var_id: VarId) -> Result<&Slot, StoreError> {
        self.slots
            .get(var_id.0 as usize)
            .ok_or(StoreError::BadSlotIndex {
                var_id,
                nvars: self.slots.len(),
            })
            .map(|slot| slot.as_ref())?
            .ok_or(StoreError::UseBeforeBind(var_id))
    }
}

pub struct Frame<S: Store> {
    cursor: Option<S::Scan>,
    current: Option<Slot>,
}

impl<S: Store> Frame<S> {
    pub fn closed() -> Self {
        Self {
            cursor: None,
            current: None,
        }
    }

    fn build_prefix(generator: &Generator, env: &Env) -> Result<Vec<u8>, StoreError> {
        let mut prefix = generator.access.predicate_id.0.to_be_bytes().to_vec();

        match &generator.access.seek_key {
            SeekKey::Prefix(bytes) => {
                prefix.extend_from_slice(bytes.as_ref());
            }
            SeekKey::Composite(parts) => {
                for part in parts.iter() {
                    match part {
                        SeekKeyPart::Bytes(bytes) => {
                            prefix.extend_from_slice(bytes.as_ref());
                        }
                        SeekKeyPart::SlotField { var_id, field_idx } => {
                            prefix.extend_from_slice(env.get(*var_id)?.field(*field_idx)?.as_ref());
                        }
                    }
                }
            }
        }

        Ok(prefix)
    }

    fn open(
        &mut self,
        store: &S,
        generator: &Generator,
        env: &Env,
        resume_at: Option<&[u8]>,
    ) -> Result<(), StoreError> {
        let prefix = Self::build_prefix(generator, env)?;
        let hi = strinc(&prefix);
        let lo = resume_at.unwrap_or(&prefix);

        self.cursor = Some(store.scan(lo, hi.as_deref()));
        self.current = None;

        Ok(())
    }

    fn next(&mut self, generator: &Generator, env: &Env) -> Result<Option<Slot>, StoreError> {
        let cursor = self.cursor.as_mut().ok_or(StoreError::AdvanceAfterClose)?;

        for row in cursor {
            let (full_key, fact_id) = row?;
            let slot = Slot::Fact {
                id: fact_id,
                key: full_key,
            };
            if check_residuals(&generator.residuals, &slot, env)? {
                self.current = Some(slot.clone());
                return Ok(Some(slot));
            }
        }

        Ok(None)
    }
}

fn check_residuals(residuals: &[Residual], slot: &Slot, env: &Env) -> Result<bool, StoreError> {
    for residual in residuals {
        let field = slot.field(residual.field_idx)?;

        match &residual.op {
            ResidualOp::EqConst(const_bytes) => {
                if field.as_ref() != const_bytes.as_ref() {
                    return Ok(false);
                }
            }
            ResidualOp::Prefix(prefix_bytes) => {
                if !field.as_ref().starts_with(prefix_bytes.as_ref()) {
                    return Ok(false);
                }
            }
            ResidualOp::EqSlotField { var_id, field_idx } => {
                let other_field = env.get(*var_id)?.field(*field_idx)?;
                if field.as_ref() != other_field.as_ref() {
                    return Ok(false);
                }
            }
        }
    }

    Ok(true)
}

pub struct Resume {
    keys: Vec<Slot>,
    done: bool,
    started: bool,
}

pub enum Step {
    Continue,
    Done,
}

pub struct Executor<S: Store> {
    store: Arc<S>,
    plan: Arc<Plan>,
    env: Env,
    frames: Vec<Frame<S>>,
    level: usize,
    started: bool,
    done: bool,
}

impl<S: Store> Executor<S> {
    pub fn new(store: Arc<S>, plan: Arc<Plan>) -> Self {
        let n = plan.body.len();
        let env = Env::new(plan.nvars);
        let frames = (0..n).map(|_| Frame::closed()).collect();
        Self {
            store,
            plan,
            env,
            frames,
            level: 0,
            started: false,
            done: false,
        }
    }

    fn bind(&mut self, level: usize, slot: Slot) {
        for var_id in self.plan.body[level].binds.iter() {
            self.env.slots[var_id.0 as usize] = Some(slot.clone());
        }
        self.frames[level].current = Some(slot);
    }

    pub fn pump(&mut self) -> Result<Step, StoreError> {
        if self.done {
            return Ok(Step::Done);
        }

        self.started = true;

        loop {
            if self.frames[self.level].cursor.is_none() {
                let generator = &self.plan.body[self.level];

                let prefix_open = {
                    let store = &*self.store;
                    let env = &self.env;
                    self.frames[self.level].open(store, generator, env, None)
                };

                prefix_open?;
            }

            let next = {
                let generator = &self.plan.body[self.level];
                let env = &self.env;
                self.frames[self.level].next(generator, env)?
            };

            match next {
                Some(slot) => {
                    self.bind(self.level, slot);

                    if self.level == self.plan.body.len() - 1 {
                        return Ok(Step::Continue);
                    }

                    self.level += 1;
                }
                None => {
                    self.frames[self.level].cursor = None;
                    self.frames[self.level].current = None;

                    if self.level == 0 {
                        self.done = true;
                        return Ok(Step::Done);
                    }

                    self.level -= 1;
                }
            }
        }
    }

    pub fn suspend(&self) -> Resume {
        if self.done {
            return Resume {
                keys: vec![],
                done: true,
                started: true,
            };
        }

        if !self.started {
            return Resume {
                keys: vec![],
                done: false,
                started: false,
            };
        }

        let keys = self
            .frames
            .iter()
            .filter_map(|frame| frame.current.as_ref().map(|slot| slot.clone()))
            .collect();

        Resume {
            keys,
            done: false,
            started: true,
        }
    }

    pub fn resume(store: Arc<S>, plan: Arc<Plan>, token: Resume) -> Result<Self, StoreError> {
        let mut executor = Self::new(store, plan);

        if token.done {
            executor.done = true;
            return Ok(executor);
        }

        if !token.started {
            return Ok(executor);
        }

        executor.started = true;
        for level in 0..token.keys.len() {
            let saved = token.keys[level].clone();
            {
                let generator = &executor.plan.body[level];
                let store = &*executor.store;
                let env = &executor.env;

                executor.frames[level].open(
                    store,
                    generator,
                    env,
                    Some(&saved.bytes().as_ref()),
                )?;
            }

            let slot = {
                let generator = &executor.plan.body[level];
                let env = &executor.env;
                executor.frames[level].next(generator, env)?
            };

            match slot {
                Some(slot) => executor.bind(level, slot),
                None => {
                    return Err(StoreError::BadResumeKey);
                }
            }
        }

        executor.level = token.keys.len() - 1;
        Ok(executor)
    }
}
