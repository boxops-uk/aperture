use byteview::ByteView;

use crate::focus::{
    error::StoreError,
    plan::{FactId, Generator, Residual, SeekKey, SeekKeyPart, Store, VarId},
    schema::PREDICATE_ID_SIZE,
    transport::{field_range, strinc},
};

#[derive(Debug, Clone)]
pub enum Slot {
    Fact { id: FactId, key: ByteView },
    Value(ByteView),
}

impl Slot {
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
    todo!()
}
