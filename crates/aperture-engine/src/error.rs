use thiserror::Error;

use aperture_encoding::error::StoreCodecError;
use aperture_store::error::StoreError;

use aperture_schema::schema::PredicateId;

use crate::plan::{Address, PlanFingerprint};

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApertureError {
    #[error("decode error: {0}")]
    Decode(#[from] StoreCodecError),

    #[error("{0} was read before anything was bound to it")]
    UseBeforeBind(Address),

    #[error("{address} holds {held} where the plan wanted {wanted}")]
    SlotKindMismatch {
        address: Address,
        wanted: &'static str,
        held: &'static str,
    },

    #[error("{0} is not a register in this plan")]
    AddressOutOfBounds(Address),

    #[error("advance of closed frame")]
    AdvanceAfterClose,

    /// A resume cursor naming more levels than the plan has. A
    /// [`Cursor`](crate::iter::Cursor) is bytes-only and rebuilt from the
    /// wire, so a cursor that does not match the plan it is resumed against is
    /// untrusted input, not an impossibility.
    #[error("resume cursor names {cursor} level(s) but the plan has {plan}")]
    CursorPlanMismatch { cursor: usize, plan: usize },

    /// A resume cursor built by a **different build** of the engine.
    ///
    /// Checked before anything is read out of the cursor, because what the version
    /// governs is how to read it: a cursor whose layout this build does not know is
    /// not a cursor it can look inside to find a better diagnostic.
    #[error("resume cursor is version {cursor}; this build reads version {executor}")]
    CursorVersion { cursor: u16, executor: u16 },

    /// A resume cursor built from a **different plan** — the hole the level count
    /// leaves open ([chapter 5](../../docs/05-resume.md)).
    ///
    /// A cursor's entries are paired with the plan's levels *by order*, so two
    /// plans of the same shape over overlapping predicates would accept each
    /// other's cursors and answer from the wrong rows, with only the per-level
    /// `fact_id` check between that and a wrong answer — and that check passes
    /// whenever the saved key exists in the other plan's scan too.
    #[error("resume cursor was built from a different plan ({cursor:?}, not {plan:?})")]
    CursorPlan {
        cursor: PlanFingerprint,
        plan: PlanFingerprint,
    },

    /// A resume cursor naming an alternative the level it is replayed against
    /// does not have — the same untrusted-input case as
    /// [`CursorPlanMismatch`](Self::CursorPlanMismatch), one level down. The
    /// level count matching does not make the sources match, since two plans of
    /// the same shape can disagree about how many alternatives a level has.
    #[error("resume cursor names source {index} of a level with {sources}")]
    CursorSourceOutOfRange { index: usize, sources: usize },

    #[error("resume key not found")]
    BadResumeKey,

    /// A plan stepping *into* a key field that is not a record. The field's own
    /// marker says what it is, so this is a plan disagreeing with the schema the
    /// row was written under — reported rather than read as bytes that happen to
    /// sit there.
    #[error("a plan reads nested field {step} of a key field that is not a record")]
    NotARecord { step: usize },

    /// A plan naming a nested field the record does not have: its terminator came
    /// first. A [`FieldPath`](crate::plan::FieldPath) is checked against the
    /// schema when the plan is built, so this is a malformed plan, not a query
    /// answering nothing.
    #[error("a plan reads nested field {step} of a record with fewer fields than that")]
    NestedFieldOutOfRange { step: usize },

    /// A stored reference naming a **different predicate** than the field it sits
    /// in is declared to reference.
    ///
    /// Reported rather than followed, because the row it names would be read
    /// against the declared predicate's key layout: every path in the fetching
    /// level's residuals, and every projection off the register it binds, was
    /// compiled from that layout. Following it anyway decodes another type's bytes
    /// at those offsets and answers with whatever is there.
    #[error(
        "a reference declared to name {expected:?} names {found:?}, whose key has a different shape"
    )]
    ReferenceCrossesPredicate {
        expected: PredicateId,
        found: PredicateId,
    },

    #[error("operation cancelled")]
    Cancelled,

    #[error("store error: {0}")]
    Store(#[from] StoreError),
}
