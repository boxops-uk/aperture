//! The `fjord` command-line tool, as a library.
//!
//! The binary is a thin dispatcher over this; what lives here is what more than one
//! target needs **one** statement of: the sample code index — `schemas/code.sigla`, parsed
//! by [`sample_schema`] — and the workload catalogue every instrument in `examples/`
//! measures.
//!
//! **Neither is reached by any command.** They are fixtures for the tests, the two
//! integration suites and the six instruments; the tool itself takes a schema from
//! wherever the caller says.
//!
//! The binary re-exports [`sample_schema`] rather than declaring a second `mod` of it, so
//! `crate::sample_schema` means the same module in both targets. Compiling it twice would
//! also parse the schema twice and hand out two `Schema`s that compare equal and are not
//! the same `Arc`.
//!
//! This is `fjord-cli` in [operations §10](../../../website/content/operations.md)'s
//! layout, and the package is named for that.

pub mod sample_schema;
pub mod workload;
