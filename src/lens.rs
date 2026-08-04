//! The superseded first-attempt front end, kept only as a reference to
//! re-implement into `focus` and then delete file-by-file (see
//! [chapter 1](../docs/01-concepts.md)). **Not compiled** — this module is not
//! declared in `lib.rs`.
//!
//! Subsumed and deleted so far: the grammar, lexer, parser glue, parse entry
//! point, CST façade, lowering and typecheck, all of which now live in
//! `src/focus/` with tests. What remains is the reference for work not yet done:
//!
//! - `hoist.rs` — flatten, the reference for **Phase 4**. For the type model it
//!   reads, see `focus::ty` rather than the deleted `lens/ty.rs`: the focus
//!   version is the one Phase 4 will actually consume, and unlike this one it uses
//!   sorted-slice records and a `NodeId` side table.
//! - `query.rs` — the boxed ergonomic AST, the third tree representation, which
//!   `focus` has not built yet;
//! - `schema.rs`, `location.rs`, `diag.rs` — what those two depend on.

pub mod diag;
pub mod hoist;
pub mod location;
pub mod query;
pub mod schema;
