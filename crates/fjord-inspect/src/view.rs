//! What every view says the same way: where something is, and what went wrong.
//!
//! One definition each, because a page keys on these field names and a second
//! `Span` shape — even an identical one — is a second thing to keep true. The
//! diagnostic view is deliberately flat: a code, a message, and the spans it
//! points at, with the primary one marked. Everything a terminal renders around
//! that (the gutter, the underline, the snippet) is the page's business.

use codespan_reporting::diagnostic::LabelStyle;
use fjord_engine::diag::{Diagnostic, Diagnostics};
use serde::Serialize;

/// A byte range into the source, as the lexer reports it.
///
/// Byte offsets rather than character positions, because that is what a `Span`
/// *is* — the page slices the same string the lexer read. A UTF-16 view would
/// be a conversion this crate cannot check and the page can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// One span a diagnostic points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Label {
    pub span: Span,
    /// Whether this is the span being reported, as against one shown for
    /// context. A page that renders one label renders this one.
    pub primary: bool,
}

/// A diagnostic, flattened to what a page can render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticView {
    /// The taxonomy entry, where the phase reported one (`nyi/negation`).
    ///
    /// Absent for the lexer, which has one fault and no code for it: a byte it
    /// cannot read is not a construct anybody deferred.
    pub code: Option<String>,
    pub message: String,
    pub labels: Vec<Label>,
}

/// One diagnostic, flattened.
///
/// Labels keep the order the phase built them in rather than being sorted here:
/// which one is primary is already marked, and a phase that points at two spans
/// meant the second one to follow the first.
#[must_use]
pub fn view_of(diagnostic: &Diagnostic) -> DiagnosticView {
    DiagnosticView {
        code: diagnostic.code.clone(),
        message: diagnostic.message.clone(),
        labels: diagnostic
            .labels
            .iter()
            .map(|label| Label {
                span: Span {
                    start: label.range.start,
                    end: label.range.end,
                },
                primary: label.style == LabelStyle::Primary,
            })
            .collect(),
    }
}

/// Every diagnostic a phase reported, in the order a reader meets them.
///
/// Through [`Diagnostics::in_source_order`] rather than a sort of its own: a
/// second ordering here is one that can disagree with what the terminal prints
/// for the same query.
#[must_use]
pub fn views_of(diagnostics: &Diagnostics) -> Vec<DiagnosticView> {
    diagnostics
        .in_source_order()
        .into_iter()
        .map(view_of)
        .collect()
}
