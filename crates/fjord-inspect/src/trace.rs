//! **The run, one transition at a time** — the whole of it, in one answer.
//!
//! The executor is a defunctionalised state machine
//! ([I7](../../../website/content/invariants.md#i7)): `depth`, a stack of
//! frames, and a loop whose every iteration is exactly one transition. So a
//! debugger is not a second interpreter — it is that loop, driven a step at a
//! time, with the machine's own state read between steps.
//! `stepping_yields_what_running_yields` is what says the two are the same
//! machine.
//!
//! **The whole trace, not a step at a time.** A page that asked for step *n*
//! would replay the prefix for each one, which is O(n²) for a scrub bar; and a
//! live executor across the WebAssembly boundary would put state — and a
//! `free()` — where two strings and JSON have been enough. So one call runs the
//! query to the end and answers every step, each carrying **what changed**: the
//! registers written, the row yielded, the row dropped and which residual
//! dropped it. The page folds those into cumulative state and scrubs a local
//! array, forwards and backwards, without asking again.
//!
//! **The cap is stated, never silent.** Past [`TRACE_CAP`] transitions the run
//! stops and says so, because a truncated run rendered as a whole one is the
//! failure worth guarding against.

use fjord_encoding::tuple::{Value, decode_key};
use fjord_engine::{
    compile::Compilation,
    iter::{Executor, MachineState, Profile, Register, Slot, Trace as Watch, Transition},
};
use fjord_schema::schema::{LocalInterner, Schema};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    schema::compile as compile_schema,
    view::{DiagnosticView, views_of},
};

/// How many transitions the site will run before stopping.
///
/// A query over the demo database takes tens; one written to be silly takes
/// thousands. Reported as `truncated` rather than quietly stopping.
pub const TRACE_CAP: usize = 10_000;

/// What a register holds, as a page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegisterView {
    pub address: usize,
    /// `fact`, `value`, or `empty`.
    pub kind: &'static str,
    /// The row's identity — `code.Decl#4` — for a register holding a row.
    ///
    /// **Decoded against the predicate of the id actually bound**, not against
    /// the level's: a level with alternatives can bind rows of different
    /// predicates, and reading one through the other's key type decodes
    /// plausible bytes into the wrong answer.
    pub fact: Option<String>,
    /// The row's key, or the computed value.
    pub value: Option<serde_json::Value>,
}

/// A row a residual read and dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejection {
    /// The step of the plan whose scan pulled it.
    pub step: usize,
    /// Which of that step's residuals dropped it, by index into the plan view's
    /// own list.
    pub residual: usize,
    pub row: RegisterView,
}

/// One transition, and what it changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceStep {
    pub at: usize,
    /// `step`, `yield`, `reject`, or `done`.
    pub event: &'static str,
    /// Where the machine is standing after it — an index into the plan's body,
    /// and `steps` exactly when it is on the head with a row to hand back.
    pub depth: usize,
    /// The registers this transition changed, and nothing else: a page folds
    /// them onto what it already has.
    pub registers: Vec<RegisterView>,
    /// The row, on a `yield`.
    pub row: Option<serde_json::Value>,
    /// The row dropped, on a `reject`.
    pub rejected: Option<Rejection>,
    /// Rows examined per plan step, as they stand after this transition.
    pub examined: Vec<u64>,
}

/// A whole run, step by step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trace {
    pub steps: Vec<TraceStep>,
    /// How many rows the run answered.
    pub rows: usize,
    pub examined_total: u64,
    /// Whether the run stopped at [`TRACE_CAP`] rather than because it was done.
    pub truncated: bool,
    pub diagnostics: Vec<DiagnosticView>,
}

/// Compile `query` against `schema_source` and trace it over the demo database.
#[must_use]
pub fn trace(schema_source: &str, query: &str) -> Trace {
    let (schema, diagnostics) = compile_schema(schema_source);

    let Some(schema) = schema else {
        return empty(diagnostics);
    };

    run(&schema, query)
}

/// The same view, already JSON.
#[must_use]
pub fn trace_json(schema_source: &str, query: &str) -> String {
    serde_json::to_string(&trace(schema_source, query)).expect("a trace serialises")
}

fn empty(diagnostics: Vec<DiagnosticView>) -> Trace {
    Trace {
        steps: Vec::new(),
        rows: 0,
        examined_total: 0,
        truncated: false,
        diagnostics,
    }
}

/// What the executor tells us while a transition runs.
///
/// Only rejections: everything else the machine does is visible in `depth` and
/// the registers between steps, and a hook reporting what is already readable
/// would be a second source of truth for it.
#[derive(Default)]
struct Watcher {
    rejected: Vec<(usize, Register, usize)>,
}

impl Watch for Watcher {
    fn rejected(&mut self, depth: usize, register: &Register, residual: usize) {
        self.rejected.push((depth, register.clone(), residual));
    }
}

fn run(schema: &Schema, query: &str) -> Trace {
    let mut compilation = Compilation::new(query, schema);
    let Some(plan) = compilation.plan() else {
        return empty(views_of(compilation.diagnostics()));
    };
    let interner = compilation.interner();

    let store = match crate::demo::store(schema) {
        Ok(store) => store,
        Err(fault) => return empty(vec![fault_view(&fault.to_string())]),
    };

    let body = plan.body.len();
    let mut profile = Profile::for_plan(&plan);
    let mut executor = Executor::new(store, plan);
    let token = CancellationToken::new();

    let mut steps: Vec<TraceStep> = Vec::new();
    let mut watcher = Watcher::default();
    let mut previous: Vec<Option<RegisterView>> = Vec::new();
    let mut rows = 0;
    let mut truncated = false;

    loop {
        if steps.len() >= TRACE_CAP {
            truncated = true;
            break;
        }

        // Standing on the head: the moment a run would call back with a row.
        if let Some(mut row) = executor.row() {
            let value = row
                .to_value(interner)
                .ok()
                .map(|value| crate::value::json(&value, schema));
            rows += 1;

            steps.push(TraceStep {
                at: steps.len(),
                event: "yield",
                depth: body,
                registers: Vec::new(),
                row: value,
                rejected: None,
                examined: profile.examined.clone(),
            });

            if !executor.resume_after_row() {
                break;
            }
            continue;
        }

        let moved = executor.step_watched(&token, &mut profile, &mut watcher);

        // The rows this transition read and dropped, in the order it read them —
        // before the transition itself, because that is when they happened.
        for (depth, register, residual) in watcher.rejected.drain(..) {
            steps.push(TraceStep {
                at: steps.len(),
                event: "reject",
                depth,
                registers: Vec::new(),
                row: None,
                rejected: Some(Rejection {
                    step: depth,
                    residual,
                    row: register_view(usize::MAX, &Slot::Fact(register), schema, interner),
                }),
                examined: profile.examined.clone(),
            });
        }

        let done = match moved {
            Ok(Transition::Stepped) => false,
            Ok(Transition::Done) => true,
            Err(fault) => {
                let mut trace = finish(steps, rows, &profile, truncated);
                trace.diagnostics.push(fault_view(&fault.to_string()));
                return trace;
            }
        };

        let now = registers(executor.state(), schema, interner);
        let changed = changes(&previous, &now);
        previous = now;

        steps.push(TraceStep {
            at: steps.len(),
            event: if done { "done" } else { "step" },
            depth: executor.depth(),
            registers: changed,
            row: None,
            rejected: None,
            examined: profile.examined.clone(),
        });

        if done {
            break;
        }
    }

    finish(steps, rows, &profile, truncated)
}

fn finish(steps: Vec<TraceStep>, rows: usize, profile: &Profile, truncated: bool) -> Trace {
    Trace {
        steps,
        rows,
        examined_total: profile.total(),
        truncated,
        diagnostics: Vec::new(),
    }
}

fn registers(
    state: &MachineState,
    schema: &Schema,
    interner: &LocalInterner,
) -> Vec<Option<RegisterView>> {
    state
        .registers
        .iter()
        .enumerate()
        .map(|(address, slot)| {
            slot.as_ref()
                .map(|slot| register_view(address, slot, schema, interner))
        })
        .collect()
}

/// Only what this transition wrote.
///
/// A page folds these onto the registers it already has, which is what keeps a
/// whole trace small: a run of two hundred steps carries two hundred register
/// writes, not two hundred copies of the register file.
fn changes(before: &[Option<RegisterView>], after: &[Option<RegisterView>]) -> Vec<RegisterView> {
    after
        .iter()
        .enumerate()
        .filter(|(address, now)| before.get(*address).map(Option::as_ref) != Some(now.as_ref()))
        .map(|(address, now)| {
            now.clone().unwrap_or(RegisterView {
                address,
                kind: "empty",
                fact: None,
                value: None,
            })
        })
        .collect()
}

fn register_view(
    address: usize,
    slot: &Slot,
    schema: &Schema,
    interner: &LocalInterner,
) -> RegisterView {
    match slot {
        Slot::Fact(register) => {
            let predicate = register.fact_id.predicate();
            let declared = schema.get(predicate);

            let value = declared
                .map(|declared| declared.key().ty.clone())
                .and_then(|ty| decode_key(interner, &register.key(), &ty).ok())
                .map(|value: Value| crate::value::json(&value, schema));

            RegisterView {
                address,
                kind: "fact",
                fact: Some(crate::value::fact(&register.fact_id, schema)),
                value,
            }
        }
        Slot::Value(value) => RegisterView {
            address,
            kind: "value",
            fact: None,
            value: Some(crate::value::json(value, schema)),
        },
    }
}

fn fault_view(message: &str) -> DiagnosticView {
    DiagnosticView {
        code: None,
        message: message.to_owned(),
        labels: Vec::new(),
    }
}
