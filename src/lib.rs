//! The `aperture` package's library half: the things both binaries need.
//!
//! There are two — the shell (`src/main.rs`) and the server
//! (`src/bin/aperture-serve.rs`) — and until [Phase 8](../PLAN.md) parses schemas
//! they both need the same one written down in Rust. It was written down twice, which
//! is exactly the drift a shared definition prevents: a server serving one shape and
//! a shell querying another is a mismatch nothing would report until a query returned
//! nothing.
//!
//! This is where the package becomes `aperture-cli`: Phase 9's command tree lands
//! here, and `main.rs` becomes one command among several.

pub mod code_index;
