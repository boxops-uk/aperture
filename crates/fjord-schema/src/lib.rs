//! The type model and the identity types every other layer names.
//!
//! The bottom of the workspace: this crate depends on nothing of Fjord's, and
//! everything else depends on it. Two modules, kept apart because they answer
//! different questions — [`schema`] is what a predicate *is*, [`id`] is what a
//! stored row *is called*.
//!
//! Design of record: [chapter 6](../../../docs/06-types-and-schema.md) for the
//! type model, [chapter 3](../../../docs/03-storage-model.md#factid-allocation-i11)
//! for the id.

/// The string interner this crate's public API is built over.
///
/// **Re-exported because it leaks.** [`Schema::new`](schema::Schema::new) takes a
/// `lasso::RodeoReader` and [`SchemaInterner::resolve`](schema::SchemaInterner::resolve)
/// takes a `Spur`, so a program that builds a schema by hand needs the *same* version of
/// `lasso` that this crate links — and a program that added its own would compile until
/// the two versions diverged, then stop, with an error about two types of the same name.
/// Taking it from here makes that impossible rather than merely documented.
///
/// Most callers want [`syntax::read`] and never name this: reading a schema from `.sigla`
/// source needs no interner of the caller's at all.
pub use lasso;

pub mod fingerprint;
pub mod id;
pub mod schema;
pub mod syntax;

/// **The README, compiled.**
///
/// `cfg(doctest)` so it costs an ordinary build nothing and appears in no documentation:
/// what it buys is that the examples on the crate's front page are run by `cargo test`
/// like any other, rather than being prose that compiled once when it was written.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct Readme;
