//! The type model and the identity types every other layer names.
//!
//! The bottom of the workspace: this crate depends on nothing of Aperture's, and
//! everything else depends on it. Two modules, kept apart because they answer
//! different questions — [`schema`] is what a predicate *is*, [`id`] is what a
//! stored row *is called*.
//!
//! Design of record: [chapter 6](../../../docs/06-types-and-schema.md) for the
//! type model, [chapter 3](../../../docs/03-storage-model.md#factid-allocation-i11)
//! for the id.

pub mod id;
pub mod schema;
pub mod syntax;
