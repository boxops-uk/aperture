//! **The shell, and nothing else.**
//!
//! Every export takes a `&str` and returns a `String` of JSON, and there is no
//! logic here — a function that needs a branch belongs in [`fjord_inspect`],
//! where the host suite covers it. What cannot be covered there is exactly what
//! is left here: the `wasm_bindgen` boundary.
//!
//! The boundary is `serde_json` in [`fjord_inspect`] and `JSON.parse` on the
//! other side — the encoder is deliberately *not* here, so the JSON a browser
//! receives is byte for byte the JSON the host suite asserts on. A string
//! because payloads are query-sized and a string is debuggable: a failing view
//! can be pasted into a terminal. `serde-wasm-bindgen` is the upgrade if
//! profiling ever asks for it; pre-empting it buys nothing.
//!
//! What a browser cannot do, stated so it is not filed as a gap: **ingest**,
//! because interning needs a real backend and durable id claims, and **schema
//! `import`**, because resolution reads files — so a browser schema is
//! single-file until a virtual resolver exists. Everything else runs here,
//! lexing to executing: the queries answer against a `MemStore` holding the
//! demo database, through the same executor the server runs.

use wasm_bindgen::prelude::wasm_bindgen;

/// Lex `source` as sigla and answer the [token view](fjord_inspect::Tokens) as
/// JSON.
///
/// Never fails: an unreadable byte is a token plus a diagnostic, so a page
/// gets an answer for every keystroke including the half-typed ones.
#[wasm_bindgen]
#[must_use]
pub fn tokens(source: &str) -> String {
    fjord_inspect::tokens_json(source)
}

/// Parse `source` as sigla and answer the [tree view](fjord_inspect::Tree) as
/// JSON.
///
/// Never fails either: a refusal is a tree with no root and the diagnostics
/// that say why, and a recovered parse carries both a tree and the faults it
/// recovered from — which is what a half-typed query looks like.
#[wasm_bindgen]
#[must_use]
pub fn tree(source: &str) -> String {
    fjord_inspect::tree_json(source)
}

/// Lex `source` as a **schema** and answer the token view as JSON.
///
/// A second lexer, not a second reading of the first: the schema language has
/// comments and namespaces where sigla has neither.
#[wasm_bindgen]
#[must_use]
pub fn schema_tokens(source: &str) -> String {
    fjord_inspect::schema_tokens_json(source)
}

/// Read `source` as a schema and answer the
/// [schema view](fjord_inspect::SchemaView) as JSON.
#[wasm_bindgen]
#[must_use]
pub fn schema(source: &str) -> String {
    fjord_inspect::schema_json(source)
}

/// Compile `query` against `schema` through the whole front end — lex, parse,
/// lower, typecheck, flatten, reorder — and answer the
/// [lowered view](fjord_inspect::Lowered) as JSON.
///
/// Two strings in, because the module holds no state: a browser has no
/// filesystem to keep a schema in, and a handle would be a lifetime to manage
/// across a boundary that cannot express one. Schemas are small and compiling
/// one is microseconds.
#[wasm_bindgen]
#[must_use]
pub fn compile(schema: &str, query: &str) -> String {
    fjord_inspect::lowered_json(schema, query)
}

/// Run `query` against the demo database and answer the rows, with what
/// reading them cost.
#[wasm_bindgen]
#[must_use]
pub fn run(schema: &str, query: &str) -> String {
    fjord_inspect::rows_json(schema, query)
}

/// **Trace `query`** — the whole run, one transition at a time.
///
/// The executor is a state machine whose every loop iteration is one
/// transition, so this is that loop driven a step at a time, with the machine's
/// registers read between steps. The whole run comes back at once: a page folds
/// the changes and scrubs a local array, forwards and backwards, rather than
/// asking again per step.
#[wasm_bindgen]
#[must_use]
pub fn trace(schema: &str, query: &str) -> String {
    fjord_inspect::trace_json(schema, query)
}

/// **Every stored row of the demo database**, as bytes and as a fact, in the
/// order a scan meets them.
///
/// The bytes are the point: a seek is a byte prefix and a scan is a range over
/// the same order, so a scan's bounds mean nothing against decoded values and
/// everything against these.
#[wasm_bindgen]
#[must_use]
pub fn database(schema: &str) -> String {
    fjord_inspect::database_json(schema)
}

/// The schema the site opens with — the repository's own `schemas/code.sigla`.
#[wasm_bindgen]
#[must_use]
pub fn sample_schema() -> String {
    fjord_inspect::SCHEMA.to_owned()
}

/// The queries the site opens with, as JSON.
///
/// From the module rather than the page because they are *tested* there:
/// `every_sample_compiles_clean` is what stops the site shipping an example the
/// language would refuse.
#[wasm_bindgen]
#[must_use]
pub fn samples() -> String {
    fjord_inspect::samples_json()
}

/// The version of Fjord this module was built from, for a page that wants to
/// say what it is running.
#[wasm_bindgen]
#[must_use]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
