//! The `aperture` command-line tool, as a library.
//!
//! The binary is a thin dispatcher over this; what lives here is what more than one
//! target needs **one** statement of: the built-in schema — hardcoded until
//! [Phase 8](../PLAN.md) parses them — and the workload catalogue every instrument in
//! `examples/` measures, which is [Phase 10](../PLAN.md)'s S0.
//!
//! This is `aperture-cli` in [operations §10](../docs/aperture-cli-design.md)'s
//! layout, and the package is named for that.

pub mod code_index;
pub mod workload;
