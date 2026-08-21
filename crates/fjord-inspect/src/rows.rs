//! **Running the query** — the answers, and what reading them cost.
//!
//! The executor is the same one the server runs, over a
//! [`MemStore`](fjord_store_mem::MemStore) holding the
//! [demo database](crate::demo). Nothing here is a simulation of execution: the
//! plan the [plan view](crate::plan) shows is the plan this runs.
//!
//! **The profile is beside the rows on purpose.** A query that answers three
//! rows having read three is a different query from one that answers three
//! having read three hundred, and only one of the two numbers is visible in the
//! answer.

use fjord_engine::{
    compile::Compilation,
    iter::{Executor, Iteratee, Profile, Stream},
};
use fjord_schema::schema::Schema;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    schema::compile as compile_schema,
    view::{DiagnosticView, views_of},
};

/// One answer, as the head projects it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowView {
    /// The row's ordinal in the run, from 1.
    pub at: usize,
    /// The projected value, rendered as JSON by the codec's own `Serialize`.
    pub value: serde_json::Value,
}

/// What running the query answered, and what it read to answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rows {
    pub rows: Vec<RowView>,
    /// Rows pulled from a scan at each step of the plan's body — matched or
    /// skipped. Indexed by step, so it lines up with the plan view.
    pub examined: Vec<u64>,
    /// Every row examined, across every step.
    pub examined_total: u64,
    /// Whether the run stopped at the row cap rather than because it was done.
    /// Said rather than shown, because a truncated answer rendered as a whole
    /// one is the failure worth guarding against.
    pub truncated: bool,
    pub diagnostics: Vec<DiagnosticView>,
}

/// How many rows the site will answer before stopping.
///
/// The demo database is small enough that nothing reaches this; a query written
/// to be silly (`X where code.Decl _; code.Decl _; code.Decl _`) can. Stated as
/// a constant and reported as `truncated`, never as a silent stop.
pub const ROW_CAP: usize = 500;

/// Compile `query` against `schema_source` and run it over the demo database.
///
/// Never fails: a query that does not compile answers no rows and its
/// diagnostics, which is what the compiled view says too.
#[must_use]
pub fn rows(schema_source: &str, query: &str) -> Rows {
    let (schema, diagnostics) = compile_schema(schema_source);

    let Some(schema) = schema else {
        return empty(diagnostics);
    };

    run(&schema, query)
}

/// The same view, already JSON.
#[must_use]
pub fn rows_json(schema_source: &str, query: &str) -> String {
    serde_json::to_string(&rows(schema_source, query)).expect("a rows view serialises")
}

fn empty(diagnostics: Vec<DiagnosticView>) -> Rows {
    Rows {
        rows: Vec::new(),
        examined: Vec::new(),
        examined_total: 0,
        truncated: false,
        diagnostics,
    }
}

fn run(schema: &Schema, query: &str) -> Rows {
    let mut compilation = Compilation::new(query, schema);
    let Some(plan) = compilation.plan() else {
        // A query that does not compile answers nothing, and *why* is the only
        // useful thing to say — dropping the diagnostics here is how a page
        // ends up showing an empty table for a query with a typo in it.
        return empty(views_of(compilation.diagnostics()));
    };
    let interner = compilation.interner();

    let render = |value: &fjord_encoding::tuple::Value| crate::value::json(value, schema);

    let store = match crate::demo::store(schema) {
        Ok(store) => store,
        // Only reachable if the schema in the page and the facts in this crate
        // disagree — a bug here rather than a caller's mistake, and one that
        // must not look like "this query answers nothing".
        Err(fault) => return empty(vec![fault_view(&fault.to_string())]),
    };
    let mut profile = Profile::for_plan(&plan);
    let executor = Executor::new(store, plan);

    // The nine-line loop the corpus's own acceptance gate writes, with the cap
    // as its only addition.
    let answered = match executor.enumerate_profiled(
        Vec::new(),
        |mut rows: Vec<RowView>, mut row| {
            let value = row.to_value(interner)?;
            rows.push(RowView {
                at: rows.len() + 1,
                value: render(&value),
            });

            Ok(if rows.len() >= ROW_CAP {
                Stream::Suspend(rows)
            } else {
                Stream::Continue(rows)
            })
        },
        &CancellationToken::new(),
        &mut profile,
    ) {
        Ok(answered) => answered,
        Err(fault) => return empty(vec![fault_view(&fault.to_string())]),
    };

    let (rows, truncated) = match answered {
        Iteratee::Done(rows) => (rows, false),
        // Suspended means the cap stopped it, since nothing else here suspends.
        Iteratee::Suspended(rows, _) => (rows, true),
    };

    Rows {
        rows,
        examined_total: profile.total(),
        examined: profile.examined,
        truncated,
        diagnostics: Vec::new(),
    }
}

/// A fault raised while *running*, as a diagnostic with nothing to point at.
///
/// The executor's errors are about the plan or the store rather than about a
/// span of the query, so there is no span to give: a label pointing at byte
/// zero would be a lie a reader would chase.
fn fault_view(message: &str) -> DiagnosticView {
    DiagnosticView {
        code: None,
        message: message.to_owned(),
        labels: Vec::new(),
    }
}
