//! The superseded first-attempt front end, kept only as a reference to
//! re-implement into `focus` and then delete file-by-file (see
//! [chapter 1](../docs/01-concepts.md)). **Not compiled** — this module is not
//! declared in `lib.rs`.
//!
//! Subsumed and deleted so far: the grammar, lexer, parser glue, parse entry
//! point, CST façade and lowering, all of which now live in `src/focus/` with
//! tests. What remains is the reference for work not yet done:
//!
//! - `hoist.rs` — flatten, the reference for **Phase 4**;
//! - `query.rs` — the boxed ergonomic AST, the third tree representation, which
//!   `focus` has not built yet;
//! - `ty.rs` — typecheck, being re-implemented into `focus`;
//! - `schema.rs`, `location.rs`, `diag.rs` — what those three depend on.

pub mod diag;
pub mod hoist;
pub mod location;
pub mod query;
pub mod schema;
pub mod ty;
