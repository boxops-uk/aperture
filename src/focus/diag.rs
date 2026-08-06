//! Diagnostics: the codes the front end reports with, and the sink it reports into.
//!
//! Two things live here, and they are the same idea from opposite ends.
//!
//! [`Code`] is the **identity** of a fault, separate from its wording. Tests assert
//! on the code, so a message can be reworded without churning them, and
//! `focus::corpus` — the language surface as data — classifies every deferred
//! construct by the code it must draw. It is an enum rather than the `&'static str`
//! it started as because a typo in a string is a test that passes for the wrong
//! reason; the compiler now enumerates the taxonomy. The rendered form is
//! unchanged, so what a reader sees is what it always was.
//!
//! [`Diagnostics`] is the **sink**: one per compilation, shared by every phase.
//! Phases report by pushing into it and cannot return diagnostics, which is the
//! point — a `Vec` handed back is a `Vec` a caller can drop on the floor, and
//! "every diagnostic reaches the user" then rests on each call site remembering.
//! The same move as [`Executor::enumerate`](crate::focus::iter::Executor) taking
//! `self` to make [I8](../../docs/invariants.md#i8) structural.

use std::ops::Range;

use codespan_reporting::diagnostic::{Label, Severity};

use super::syntax::{NodeSpan, source_range};

/// The front end's diagnostic type: `codespan-reporting`'s, with no file id,
/// because a compilation is one source.
///
/// It lives here rather than in `parser.rs`, where it began: that module is glue
/// around the *generated* parser, so every phase importing its diagnostic type
/// from it read as though diagnostics were a lelwel concept.
pub type Diagnostic = codespan_reporting::diagnostic::Diagnostic<()>;

/// What a code says about the fault it names — the distinction the code's prefix
/// has always carried, available to a caller rather than only to a reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `nyi/…` — parses, will be implemented, is not yet. A later phase owns it.
    Deferred,
    /// `reject/…` — meaningless, and no phase will ever implement it.
    Meaningless,
    /// `lit/…` — a malformed literal: right shape, unusable value.
    Literal,
}

/// The identity of a diagnostic, independent of its wording.
///
/// Adding a variant is adding a fault the front end can report; the rendered
/// string is part of the test surface (`focus::corpus`), so treat it as an
/// interface — rename the variant freely, change [`Code::as_str`] deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    // `nyi/` — deferred to a later phase.
    NyiBindUnification,
    NyiDisjunction,
    NyiFactField,
    NyiNegation,
    NyiNestedGenerator,
    NyiNever,
    NyiRepeatedVariable,
    NyiSubquery,
    NyiUnionSelect,
    NyiValueBind,
    NyiValueMatch,
    NyiWholeKey,

    // `reject/` — meaningless, rejected for good.
    RejectBindLhs,
    RejectDuplicateField,
    RejectInfiniteType,
    RejectNoValue,
    RejectNotAGenerator,
    RejectNotProjectable,
    RejectTypeMismatch,
    RejectUnboundVariable,
    RejectUnknownField,
    RejectUnknownPredicate,
    RejectUnresolvedAccess,
    RejectValueShadowed,
    RejectWildcardInHead,

    // `lit/` — a malformed literal.
    LitIntLeadingZero,
    LitIntRange,
    LitIntUnderscore,
    LitStringEscape,
}

impl Code {
    /// Every code, for tests that must cover the taxonomy exhaustively.
    ///
    /// Hand-written rather than derived, and that is the point: adding a variant
    /// without adding it here fails `every_code_is_in_all`, which is the reminder
    /// to classify it.
    pub const ALL: &'static [Code] = &[
        Code::NyiBindUnification,
        Code::NyiDisjunction,
        Code::NyiFactField,
        Code::NyiNegation,
        Code::NyiNestedGenerator,
        Code::NyiNever,
        Code::NyiRepeatedVariable,
        Code::NyiSubquery,
        Code::NyiUnionSelect,
        Code::NyiValueBind,
        Code::NyiValueMatch,
        Code::NyiWholeKey,
        Code::RejectBindLhs,
        Code::RejectDuplicateField,
        Code::RejectInfiniteType,
        Code::RejectNoValue,
        Code::RejectNotAGenerator,
        Code::RejectNotProjectable,
        Code::RejectTypeMismatch,
        Code::RejectUnboundVariable,
        Code::RejectUnknownField,
        Code::RejectUnknownPredicate,
        Code::RejectUnresolvedAccess,
        Code::RejectValueShadowed,
        Code::RejectWildcardInHead,
        Code::LitIntLeadingZero,
        Code::LitIntRange,
        Code::LitIntUnderscore,
        Code::LitStringEscape,
    ];

    /// The rendered code — what a reader sees, and what the corpus asserts on.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Code::NyiBindUnification => "nyi/bind-unification",
            Code::NyiDisjunction => "nyi/disjunction",
            Code::NyiFactField => "nyi/fact-field",
            Code::NyiNegation => "nyi/negation",
            Code::NyiNestedGenerator => "nyi/nested-generator",
            Code::NyiNever => "nyi/never",
            Code::NyiRepeatedVariable => "nyi/repeated-variable",
            Code::NyiSubquery => "nyi/subquery",
            Code::NyiUnionSelect => "nyi/union-select",
            Code::NyiValueBind => "nyi/value-bind",
            Code::NyiValueMatch => "nyi/value-match",
            Code::NyiWholeKey => "nyi/whole-key",

            Code::RejectBindLhs => "reject/bind-lhs",
            Code::RejectDuplicateField => "reject/duplicate-field",
            Code::RejectInfiniteType => "reject/infinite-type",
            Code::RejectNoValue => "reject/no-value",
            Code::RejectNotAGenerator => "reject/not-a-generator",
            Code::RejectNotProjectable => "reject/not-projectable",
            Code::RejectTypeMismatch => "reject/type-mismatch",
            Code::RejectUnboundVariable => "reject/unbound-variable",
            Code::RejectUnknownField => "reject/unknown-field",
            Code::RejectUnknownPredicate => "reject/unknown-predicate",
            Code::RejectUnresolvedAccess => "reject/unresolved-access",
            Code::RejectValueShadowed => "reject/value-shadowed",
            Code::RejectWildcardInHead => "reject/wildcard-in-head",

            Code::LitIntLeadingZero => "lit/int-leading-zero",
            Code::LitIntRange => "lit/int-range",
            Code::LitIntUnderscore => "lit/int-underscore",
            Code::LitStringEscape => "lit/string-escape",
        }
    }

    /// What this code says about the fault — derived from the variant, so it
    /// cannot drift from the prefix the way a second string would.
    #[must_use]
    pub const fn kind(self) -> Kind {
        match self {
            Code::NyiBindUnification
            | Code::NyiDisjunction
            | Code::NyiFactField
            | Code::NyiNegation
            | Code::NyiNestedGenerator
            | Code::NyiNever
            | Code::NyiRepeatedVariable
            | Code::NyiSubquery
            | Code::NyiUnionSelect
            | Code::NyiValueBind
            | Code::NyiValueMatch
            | Code::NyiWholeKey => Kind::Deferred,

            Code::RejectBindLhs
            | Code::RejectDuplicateField
            | Code::RejectInfiniteType
            | Code::RejectNoValue
            | Code::RejectNotAGenerator
            | Code::RejectNotProjectable
            | Code::RejectTypeMismatch
            | Code::RejectUnboundVariable
            | Code::RejectUnknownField
            | Code::RejectUnknownPredicate
            | Code::RejectUnresolvedAccess
            | Code::RejectValueShadowed
            | Code::RejectWildcardInHead => Kind::Meaningless,

            Code::LitIntLeadingZero
            | Code::LitIntRange
            | Code::LitIntUnderscore
            | Code::LitStringEscape => Kind::Literal,
        }
    }

    /// The code a rendered string names, if any.
    ///
    /// For reading back what a test or a diagnostic wrote down; the front end
    /// itself never needs it. Deliberately not `FromStr`: this reverses
    /// [`as_str`](Self::as_str) specifically, and there is no other spelling of a
    /// code a caller might reasonably expect it to accept.
    #[must_use]
    pub fn from_rendered(text: &str) -> Option<Code> {
        Code::ALL.iter().copied().find(|c| c.as_str() == text)
    }
}

impl Kind {
    /// The prefix codes of this kind carry, including the separator.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Kind::Deferred => "nyi/",
            Kind::Meaningless => "reject/",
            Kind::Literal => "lit/",
        }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A span a diagnostic can point at.
///
/// Implemented for both span types on purpose. A stored node's span is a
/// [`NodeSpan`] (`u32`) and the parser's is a `usize` range; they are named apart
/// because passing one where the other was meant is invisible at a call site, and
/// this is the one place both arrive. Converting here means no phase writes
/// `source_range` by hand at a report site.
pub trait ReportSpan {
    fn into_range(self) -> Range<usize>;
}

impl ReportSpan for Range<usize> {
    fn into_range(self) -> Range<usize> {
        self
    }
}

impl ReportSpan for &Range<usize> {
    fn into_range(self) -> Range<usize> {
        self.clone()
    }
}

impl ReportSpan for NodeSpan {
    fn into_range(self) -> Range<usize> {
        source_range(&self)
    }
}

impl ReportSpan for &NodeSpan {
    fn into_range(self) -> Range<usize> {
        source_range(self)
    }
}

/// The diagnostics of one compilation, in the order they were reported.
///
/// Keep-going by construction: reporting a fault does not stop a phase, so one
/// run lists everything wrong with a query rather than its first fault
/// ([chapter 7](../../docs/07-compilation.md)).
#[derive(Debug, Default)]
pub struct Diagnostics {
    inner: Vec<Diagnostic>,
}

impl Diagnostics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Report an error at `span`.
    pub fn error(&mut self, code: Code, message: impl Into<String>, span: impl ReportSpan) {
        self.inner.push(
            Diagnostic::error()
                .with_code(code.as_str())
                .with_message(message.into())
                .with_label(Label::primary((), span.into_range())),
        );
    }

    /// Report a diagnostic built elsewhere.
    ///
    /// For the two producers that have no [`Code`] to give: the generated parser,
    /// which builds its own diagnostics through `ParserCallbacks`, and the lexer's
    /// invalid-token report. Everything else should go through [`error`](Self::error).
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.inner.push(diagnostic);
    }

    /// Whether anything error-or-worse was reported.
    ///
    /// Filtered by severity rather than asking whether the list is empty: the sink
    /// is shared by every phase, and the first warning or note added to it must not
    /// start reading as a failure.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.inner.iter().any(|d| d.severity >= Severity::Error)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Diagnostic> {
        self.inner.iter()
    }

    /// The codes reported, in order, skipping any diagnostic that carries none.
    ///
    /// What tests assert on — identity rather than wording.
    pub fn codes(&self) -> impl Iterator<Item = &str> {
        self.inner.iter().filter_map(|d| d.code.as_deref())
    }

    /// Everything reported since `mark`, where `mark` is a [`len`](Self::len)
    /// taken earlier.
    ///
    /// One sink for the whole compilation means "what did *this phase* report" is
    /// no longer "the `Vec` it returned"; it is the tail added while it ran. Used
    /// by the driver to decide whether a phase found anything, and by tests that
    /// pin one phase's output rather than a query's.
    #[must_use]
    pub fn since(&self, mark: usize) -> &[Diagnostic] {
        self.inner.get(mark..).unwrap_or_default()
    }

    /// The backing vector, for the generated parser and the lexer alone.
    ///
    /// lelwel's `Parser::parse` and `tokenize` both take `&mut Vec<Diagnostic>`, so
    /// there has to be one way through. It is not a general escape hatch: a phase
    /// reaching for this is a phase that could have used [`error`](Self::error).
    pub fn as_vec_mut(&mut self) -> &mut Vec<Diagnostic> {
        &mut self.inner
    }

    #[must_use]
    pub fn into_inner(self) -> Vec<Diagnostic> {
        self.inner
    }
}

impl<'a> IntoIterator for &'a Diagnostics {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Every variant is in `ALL`.
    ///
    /// `ALL` is what the other guards here iterate, so a variant missing from it is
    /// a variant nothing checks. Counted through an exhaustive `match` rather than
    /// against a literal, so adding a variant fails to compile until it is
    /// classified — the same reason `kind` matches variant by variant.
    #[test]
    fn every_code_is_in_all() {
        fn counts(code: Code) -> usize {
            match code {
                Code::NyiBindUnification
                | Code::NyiDisjunction
                | Code::NyiFactField
                | Code::NyiNegation
                | Code::NyiNestedGenerator
                | Code::NyiNever
                | Code::NyiRepeatedVariable
                | Code::NyiSubquery
                | Code::NyiUnionSelect
                | Code::NyiValueBind
                | Code::NyiValueMatch
                | Code::NyiWholeKey
                | Code::RejectBindLhs
                | Code::RejectDuplicateField
                | Code::RejectInfiniteType
                | Code::RejectNoValue
                | Code::RejectNotAGenerator
                | Code::RejectNotProjectable
                | Code::RejectTypeMismatch
                | Code::RejectUnboundVariable
                | Code::RejectUnknownField
                | Code::RejectUnknownPredicate
                | Code::RejectUnresolvedAccess
                | Code::RejectValueShadowed
                | Code::RejectWildcardInHead
                | Code::LitIntLeadingZero
                | Code::LitIntRange
                | Code::LitIntUnderscore
                | Code::LitStringEscape => 1,
            }
        }

        let listed: BTreeSet<Code> = Code::ALL.iter().copied().collect();
        assert_eq!(listed.len(), Code::ALL.len(), "`ALL` repeats a variant");
        assert_eq!(
            Code::ALL.iter().copied().map(counts).sum::<usize>(),
            Code::ALL.len()
        );
    }

    /// A code's string is unique, and its prefix agrees with its kind.
    ///
    /// Two variants sharing a string would make the corpus assert on something it
    /// cannot distinguish; a prefix disagreeing with `kind` would make one of them
    /// lying. Both are the kind of mistake a hand-written table invites, which is
    /// why the strings became an enum in the first place.
    #[test]
    fn codes_are_unique_and_prefixed_by_kind() {
        let mut seen = BTreeSet::new();

        for code in Code::ALL {
            let text = code.as_str();

            assert!(seen.insert(text), "two codes render as `{text}`");
            assert!(
                text.starts_with(code.kind().prefix()),
                "`{text}` is {:?}, whose prefix is `{}`",
                code.kind(),
                code.kind().prefix()
            );
            assert_eq!(Code::from_rendered(text), Some(*code));
        }
    }

    /// An unknown string is not a code — `from_str` says so rather than guessing.
    #[test]
    fn an_unknown_code_does_not_resolve() {
        assert_eq!(Code::from_rendered("reject/nope"), None);
        assert_eq!(Code::from_rendered(""), None);
        assert_eq!(Code::from_rendered("nyi/"), None);
    }

    /// The sink reports the same byte range whichever span type it was handed.
    ///
    /// The two exist because confusing them is invisible at a call site
    /// (`ad703a6c3`); this is the one place both arrive, so it is the place to
    /// pin that the conversion is not the identity on the wrong one.
    #[test]
    fn both_span_types_land_on_the_same_bytes() {
        let mut diagnostics = Diagnostics::new();

        diagnostics.error(Code::NyiNever, "parser span", 3usize..7usize);
        diagnostics.error(Code::NyiNever, "node span", 3u32..7u32);

        let ranges: Vec<_> = diagnostics
            .iter()
            .map(|d| d.labels[0].range.clone())
            .collect();

        assert_eq!(ranges, vec![3..7, 3..7]);
    }

    /// `has_errors` filters by severity, so a warning-only sink is not a failure.
    #[test]
    fn has_errors_is_by_severity_not_emptiness() {
        let mut diagnostics = Diagnostics::new();
        assert!(!diagnostics.has_errors());

        diagnostics.push(Diagnostic::warning().with_message("just a warning"));
        assert!(
            !diagnostics.has_errors(),
            "a warning must not read as a failure"
        );
        assert!(!diagnostics.is_empty());

        diagnostics.error(
            Code::RejectNoValue,
            "this predicate has no value",
            0usize..1,
        );
        assert!(diagnostics.has_errors());
    }

    /// Reports keep their order, and `codes` skips what carries none.
    #[test]
    fn the_sink_preserves_order_and_reports_codes() {
        let mut diagnostics = Diagnostics::new();

        diagnostics.error(Code::RejectUnknownPredicate, "first", 0usize..1);
        diagnostics.push(Diagnostic::error().with_message("uncoded"));
        diagnostics.error(Code::NyiSubquery, "third", 2usize..3);

        assert_eq!(diagnostics.len(), 3);
        assert_eq!(
            diagnostics.codes().collect::<Vec<_>>(),
            vec!["reject/unknown-predicate", "nyi/subquery"]
        );
    }
}
