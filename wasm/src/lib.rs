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
//! `import`**, because resolution reads files. Everything from lexing to a plan
//! runs here.

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

/// The version of Fjord this module was built from, for a page that wants to
/// say what it is running.
#[wasm_bindgen]
#[must_use]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
