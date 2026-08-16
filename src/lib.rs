//! The `aperture` command-line tool, as a library.
//!
//! The binary is a thin dispatcher over this; what lives here is what more than one
//! target needs **one** statement of: the built-in schema — `schemas/code.aps`, parsed
//! by [`code_index`] since [Phase 8](../PLAN.md) — and the workload catalogue every
//! instrument in `examples/` measures, which is [Phase 10](../PLAN.md)'s S0.
//!
//! The binary re-exports [`code_index`] rather than declaring a second `mod` of it, so
//! `crate::code_index` means the same module in both targets. Compiling it twice would
//! also parse the schema twice and hand out two `Schema`s that compare equal and are not
//! the same `Arc`.
//!
//! This is `aperture-cli` in [operations §10](../docs/aperture-cli-design.md)'s
//! layout, and the package is named for that.

pub mod code_index;
pub mod workload;
