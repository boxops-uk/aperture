use std::sync::Arc;

use byteview::ByteView;

use crate::focus::{
    error::StoreError,
    plan::{FactId, Generator, Plan, Residual, ResidualOp, SeekKey, SeekKeyPart, Store, VarId},
    schema::PREDICATE_ID_SIZE,
    transport::{field_range, strinc},
};

// ── trace helpers ────────────────────────────────────────────────────────────

const RST: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";

fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "(empty)".to_string();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn fmt_slot(slot: &Slot) -> String {
    match slot {
        Slot::Fact { id, key } => {
            let b = key.as_ref();
            if b.len() >= PREDICATE_ID_SIZE {
                let pred = u32::from_be_bytes(b[..PREDICATE_ID_SIZE].try_into().unwrap());
                let payload = &b[PREDICATE_ID_SIZE..];
                if payload.is_empty() {
                    format!("fact#{} pred={pred}", id.0)
                } else {
                    format!("fact#{} pred={pred} │ {}", id.0, hex(payload))
                }
            } else {
                format!("fact#{} {}", id.0, hex(b))
            }
        }
        Slot::Value(v) => hex(v.as_ref()),
    }
}

fn print_env_table(env: &Env) {
    let slots = &env.slots;
    if slots.is_empty() {
        return;
    }
    let rows: Vec<(String, String)> = slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let label = format!("v{i}");
            let value = match s {
                None => "(unbound)".to_string(),
                Some(slot) => fmt_slot(slot),
            };
            (label, value)
        })
        .collect();
    let lw = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(3).max(3);
    let vw = rows.iter().map(|(_, v)| v.len()).max().unwrap_or(5).max(5);
    let top = format!("┌─{:─<lw$}─┬─{:─<vw$}─┐", "", "");
    let mid = format!("├─{:─<lw$}─┼─{:─<vw$}─┤", "", "");
    let bot = format!("└─{:─<lw$}─┴─{:─<vw$}─┘", "", "");
    eprintln!("  {DIM}{top}{RST}");
    eprintln!(
        "  {DIM}│{RST} {BOLD}{:<lw$}{RST} {DIM}│{RST} {BOLD}{:<vw$}{RST} {DIM}│{RST}",
        "var", "bytes"
    );
    eprintln!("  {DIM}{mid}{RST}");
    for (label, value) in &rows {
        eprintln!(
            "  {DIM}│{RST} {:<lw$} {DIM}│{RST} {:<vw$} {DIM}│{RST}",
            label, value
        );
    }
    eprintln!("  {DIM}{bot}{RST}");
}

// ── slot / env ───────────────────────────────────────────────────────────────

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

// ── frame ────────────────────────────────────────────────────────────────────

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

    fn next(
        &mut self,
        generator: &Generator,
        env: &Env,
        trace: bool,
    ) -> Result<Option<Slot>, StoreError> {
        let cursor = self.cursor.as_mut().ok_or(StoreError::AdvanceAfterClose)?;

        for row in cursor {
            let (full_key, fact_id) = row?;
            let slot = Slot::Fact {
                id: fact_id,
                key: full_key,
            };

            if trace {
                eprintln!("    {DIM}scan{RST}  {}", fmt_slot(&slot));
            }

            if check_residuals(&generator.residuals, &slot, env, trace)? {
                self.current = Some(slot.clone());
                return Ok(Some(slot));
            }
        }

        Ok(None)
    }
}

// ── residuals ────────────────────────────────────────────────────────────────

fn check_residuals(
    residuals: &[Residual],
    slot: &Slot,
    env: &Env,
    trace: bool,
) -> Result<bool, StoreError> {
    for (i, residual) in residuals.iter().enumerate() {
        let field = slot.field(residual.field_idx)?;

        let (pass, op_label) = match &residual.op {
            ResidualOp::EqConst(const_bytes) => (
                field.as_ref() == const_bytes.as_ref(),
                format!("EqConst({})", hex(const_bytes)),
            ),
            ResidualOp::Prefix(prefix_bytes) => (
                field.as_ref().starts_with(prefix_bytes.as_ref()),
                format!("Prefix({})", hex(prefix_bytes)),
            ),
            ResidualOp::EqSlotField { var_id, field_idx } => {
                let other_field = env.get(*var_id)?.field(*field_idx)?;
                (
                    field.as_ref() == other_field.as_ref(),
                    format!("EqSlotField(v{}.field[{}])", var_id.0, field_idx),
                )
            }
        };

        if trace {
            let mark = if pass {
                format!("{GREEN}✓{RST}")
            } else {
                format!("{YELLOW}✗{RST}")
            };
            eprintln!(
                "    {DIM}residual[{i}]{RST} field[{}]={} {} {mark}",
                residual.field_idx,
                hex(field.as_ref()),
                op_label,
            );
            if !pass {
                eprintln!("    {YELLOW}→ SKIP{RST}");
            }
        }

        if !pass {
            return Ok(false);
        }
    }

    Ok(true)
}

// ── executor ─────────────────────────────────────────────────────────────────

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
    trace: bool,
}

impl<S: Store> Executor<S> {
    pub fn new(store: Arc<S>, plan: Arc<Plan>) -> Self {
        let n = plan.body.len();
        let env = Env::new(plan.nvars);
        let frames = (0..n).map(|_| Frame::closed()).collect();
        let trace = std::env::var("APERTURE_PUMP_TRACE").is_ok();
        Self {
            store,
            plan,
            env,
            frames,
            level: 0,
            started: false,
            done: false,
            trace,
        }
    }

    fn bind(&mut self, level: usize, slot: Slot) {
        if self.trace {
            let vars: Vec<String> = self.plan.body[level]
                .binds
                .iter()
                .map(|v| format!("v{}", v.0))
                .collect();
            eprintln!(
                "  [{level}] {MAGENTA}BIND{RST}   {} ← {}",
                vars.join(", "),
                fmt_slot(&slot),
            );
        }
        for var_id in self.plan.body[level].binds.iter() {
            self.env.slots[var_id.0 as usize] = Some(slot.clone());
        }
        self.frames[level].current = Some(slot);
        if self.trace {
            print_env_table(&self.env);
        }
    }

    pub fn pump(&mut self) -> Result<Step, StoreError> {
        if self.done {
            return Ok(Step::Done);
        }

        self.started = true;

        if self.trace {
            eprintln!(
                "{BOLD}{CYAN}━━━ pump(){RST} level={} generators={} nvars={}",
                self.level,
                self.plan.body.len(),
                self.plan.nvars,
            );
        }

        loop {
            if self.frames[self.level].cursor.is_none() {
                let generator = &self.plan.body[self.level];

                if self.trace {
                    let prefix = Frame::<S>::build_prefix(generator, &self.env)?;
                    eprintln!(
                        "  [{level}] {BLUE}OPEN{RST}   pred={} prefix=[{}]",
                        generator.access.predicate_id.0,
                        hex(&prefix),
                        level = self.level,
                    );
                }

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
                self.frames[self.level].next(generator, env, self.trace)?
            };

            match next {
                Some(slot) => {
                    if self.trace {
                        eprintln!(
                            "  [{level}] {GREEN}MATCH{RST}  {}",
                            fmt_slot(&slot),
                            level = self.level,
                        );
                    }

                    self.bind(self.level, slot);

                    if self.level == self.plan.body.len() - 1 {
                        if self.trace {
                            eprintln!("  [{level}] {BOLD}{GREEN}YIELD{RST}", level = self.level);
                            eprintln!();
                        }
                        return Ok(Step::Continue);
                    }

                    if self.trace {
                        eprintln!(
                            "  [{level}→{next_level}] {CYAN}ADVANCE{RST}",
                            level = self.level,
                            next_level = self.level + 1,
                        );
                    }
                    self.level += 1;
                }
                None => {
                    if self.trace {
                        eprintln!(
                            "  [{level}] {RED}EXHAUSTED{RST}",
                            level = self.level,
                        );
                    }

                    self.frames[self.level].cursor = None;
                    self.frames[self.level].current = None;

                    if self.level == 0 {
                        if self.trace {
                            eprintln!("  {BOLD}{RED}DONE{RST}");
                            eprintln!();
                        }
                        self.done = true;
                        return Ok(Step::Done);
                    }

                    if self.trace {
                        eprintln!(
                            "  [{prev_level}←{level}] {RED}BACKTRACK{RST}",
                            prev_level = self.level - 1,
                            level = self.level,
                        );
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
                executor.frames[level].next(generator, env, false)?
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
