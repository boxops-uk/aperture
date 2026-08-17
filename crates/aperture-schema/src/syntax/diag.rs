//! Diagnostics for the schema DSL, and the codes they carry.
//!
//! The same arrangement `focus` has: a **code** is an enum rather than a string, so a
//! test asserts on identity instead of wording, and every construct the grammar accepts
//! but the type model cannot yet hold has one of its own. A code is what makes
//! permissive-early legible — the reader is told *which* feature is missing, at the
//! bytes where they wrote it.
//!
//! Kinds, by prefix:
//!
//! - **`nyi/…`** — accepted by the grammar, deferred by design. Every one of these is a
//!   thing the schema corpus has an entry for.
//! - **`reject/…`** — meaningless rather than deferred; it will never be accepted in
//!   this shape.

use codespan_reporting::diagnostic::Label;

use super::parser::Span;

/// A diagnostic over one file's source.
pub type Diagnostic = codespan_reporting::diagnostic::Diagnostic<()>;

/// Why a schema was refused, as an identity a test can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Code {
    /// `[T]` — the multiplicity decision, settled as *not yet*
    /// ([open decisions](../../../../docs/open-decisions.md)).
    NyiArray,
    /// `set T`.
    NyiSet,
    /// `maybe T` — sugar over a union, so it waits on one.
    NyiMaybe,
    /// `enum { a | b }` — likewise.
    NyiEnum,
    /// `{ a : t = 0 | b : t = 1 }` — a union, which lands with `PredicateTy::Union`.
    NyiUnion,
    /// `schema a evolves b`.
    NyiEvolves,
    /// `stored`, and `derive` — a derived predicate, which needs the query language
    /// this crate sits underneath.
    NyiDerivation,
    /// A discriminant written on a record field, where it means nothing.
    RejectDiscriminantOnRecordField,
    /// Two definitions of one fully-qualified name — operations §7's *genuine*
    /// redeclaration, as against the same file reached twice.
    RejectRedeclaration,
    /// A name that resolves to nothing.
    RejectUnknownName,
    /// A named type that expands into itself.
    ///
    /// Distinct from a *predicate* reference cycle, which is fine: a reference is an id
    /// and may point anywhere, while a named type is substituted where it is used and a
    /// cycle among those has no base case.
    RejectTypeCycle,
}

impl Code {
    /// Every code, so a test can assert the corpus covers them all.
    pub const ALL: &'static [Code] = &[
        Code::NyiArray,
        Code::NyiSet,
        Code::NyiMaybe,
        Code::NyiEnum,
        Code::NyiUnion,
        Code::NyiEvolves,
        Code::NyiDerivation,
        Code::RejectDiscriminantOnRecordField,
        Code::RejectRedeclaration,
        Code::RejectUnknownName,
        Code::RejectTypeCycle,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Code::NyiArray => "nyi/array",
            Code::NyiSet => "nyi/set",
            Code::NyiMaybe => "nyi/maybe",
            Code::NyiEnum => "nyi/enum",
            Code::NyiUnion => "nyi/union",
            Code::NyiEvolves => "nyi/evolves",
            Code::NyiDerivation => "nyi/derivation",
            Code::RejectDiscriminantOnRecordField => "reject/discriminant-on-record-field",
            Code::RejectRedeclaration => "reject/redeclaration",
            Code::RejectUnknownName => "reject/unknown-name",
            Code::RejectTypeCycle => "reject/type-cycle",
        }
    }

    /// A diagnostic carrying this code, pointing at `span`.
    #[must_use]
    pub fn at(self, span: Span, message: impl std::fmt::Display) -> Diagnostic {
        Diagnostic::error()
            .with_code(self.as_str())
            .with_message(message)
            .with_label(Label::primary((), span))
    }
}

/// Every diagnostic rendered against the source it points into, in file order.
///
/// Here rather than in each caller because a `SimpleFile` built somewhere else could
/// be built over *different* text, and a diagnostic rendered against the wrong source
/// points a caret at a line nobody wrote. Both callers — a `schema check` reporting to
/// a person, and a store refusing an embedded copy it cannot read — want the same
/// thing.
#[must_use]
pub fn render(name: &str, source: &str, diags: &[Diagnostic]) -> String {
    use codespan_reporting::{files::SimpleFile, term};

    let file = SimpleFile::new(name, source);
    let config = term::Config::default();
    let mut out = String::new();

    let mut ordered: Vec<&Diagnostic> = diags.iter().collect();
    ordered.sort_by_key(|diagnostic| {
        diagnostic
            .labels
            .iter()
            .map(|label| label.range.start)
            .min()
            .unwrap_or(usize::MAX)
    });

    for diagnostic in ordered {
        // A `String` sink cannot fail to be written to; a diagnostic naming a file
        // this one does not have would, and there is exactly one file.
        let _ = term::emit_to_string(&mut out, &config, &file, diagnostic);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code renders, and no two render alike — a duplicate would make two
    /// different refusals indistinguishable to the tests that assert on them.
    #[test]
    fn every_code_has_its_own_string() {
        let mut seen: Vec<&str> = Code::ALL.iter().map(|code| code.as_str()).collect();
        let count = seen.len();

        seen.sort_unstable();
        seen.dedup();

        assert_eq!(seen.len(), count, "two codes render the same way");
    }

    /// A code's prefix says which kind of refusal it is, and the two kinds mean
    /// different things to a reader: one is "not yet", the other is "not ever, like
    /// this".
    #[test]
    fn a_code_is_prefixed_by_its_kind() {
        for code in Code::ALL {
            let text = code.as_str();
            assert!(
                text.starts_with("nyi/") || text.starts_with("reject/"),
                "`{text}` has no kind prefix"
            );
        }
    }
}
