//! **sigla** — the query language — and the machine that runs it.
//!
//! Two halves meeting at one fixed contract, the [`Plan`](plan::Plan) IR:
//! `lex → parse → typecheck → flatten → reorder` compiles sigla text to a plan,
//! and [`iter`] runs that plan as a nested loop over a store, able to suspend to
//! a bytes-only cursor and resume exactly.
//!
//! Everything physical is below this crate — the schema, the id, the codec, the
//! store — so what is left here is a description of a query and a machine that
//! executes one. Design of record:
//! [chapter 4](../../../docs/04-executor.md) for the machine,
//! [chapter 5](../../../docs/05-resume.md) for resume, and
//! [chapter 7](../../../docs/07-compilation.md) for the compiler.

pub mod compile;
pub mod cst;
pub mod diag;
pub mod error;
pub mod flatten;
pub mod iter;
pub mod lower;
pub mod parse;
pub mod plan;
pub mod print;
pub mod reorder;
pub mod syntax;
pub mod ty;

// The generated-parser glue and its `logos` lexer. Public because the façade
// (`cst`) hands out `Rule` and `Token` values, and because the compilation
// driver parses from outside this module.
pub mod lexer;
pub mod parser;

// Test-support surface: the plan runners the executor batteries import, plus a
// re-export of the store-shaped half, which lives in `fjord-store`. Gated so
// `--features proptest` exposes them to consumers outside `cfg(test)` too (see
// `docs/testing.md`).
#[cfg(any(test, feature = "proptest"))]
pub mod fixtures;

// The target-feature corpus — Phase 2's audit table as data, and the acceptance
// gate for the grammar surface.
#[cfg(any(test, feature = "proptest"))]
pub mod corpus;
