//! The `aperture` command-line tool, as a library.
//!
//! The binary is a thin dispatcher over this; what lives here is the built-in schema
//! — hardcoded until [Phase 8](../PLAN.md) parses them — which both the tool and its
//! tests need one statement of.
//!
//! This is `aperture-cli` in [operations §10](../docs/aperture-cli-design.md)'s
//! layout, and the package is named for that.

pub mod code_index;
