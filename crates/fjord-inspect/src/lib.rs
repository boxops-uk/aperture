//! **A JSON view of every construct sigla compiles.**
//!
//! The mapping from the engine's internals onto plain, serialisable shapes — so
//! a page can lay a construct out rather than print a string somebody else
//! rendered. What crosses this boundary is *structure*: a page that receives
//! `{kind, span, text}` per token can align a highlight, a hover and a caret
//! against the source; a page that receives HTML can only insert it.
//!
//! **The internals do not derive `Serialize`, and that is the design.** A
//! `Symbol` is a per-query arena index that means nothing without the
//! `LocalInterner` that minted it; deriving would make internal shapes a JSON
//! contract; and today nothing in `engine`, `schema`, `encoding` or `wire`
//! derives `Serialize` but `FactId`, so a derive would be a commitment made by
//! accident. The precedent is `fjord_wire::desc`, which carries a predicate's
//! field names as *text* because a peer has no interner — a browser is one more
//! peer with no interner.
//!
//! **This crate is not the WebAssembly shell.** It is an ordinary workspace
//! member, covered by the ordinary suite, so that the only thing the `wasm/`
//! crate adds is `#[wasm_bindgen]` and `String` in, `String` out. Its
//! dependency direction is the load-bearing claim: `fjord-engine`,
//! `fjord-schema`, and **never** `fjord-store-fjall` — checked by
//! `dependency_closure` in `fjord-store`.

pub mod tokens;

pub use tokens::{DiagnosticView, Label, Span, TokenClass, TokenView, Tokens, tokens, tokens_json};
