//! The **target-feature corpus** — the Phase 2 audit table, executable.
//!
//! Phase 2's job is a grammar that already parses the *full* intended feature
//! surface, so that later phases add meaning to constructs that already parse
//! rather than reshaping the grammar ([chapter 7]). That claim needs a written
//! target, and a written target in prose drifts — so the table lives here, as
//! data, next to the tests that check it.
//!
//! Each entry says what the compiler must do with a snippet:
//!
//! | Classification | Meaning |
//! |---|---|
//! | [`Expectation::Supported`] | parses, typechecks, and is implemented |
//! | [`Expectation::Diagnosed`] | parses, then draws **one specific diagnostic code** — either "not yet implemented" or a rejection of something meaningless |
//! | [`Expectation::ParseError`] | not focus at all; a parse diagnostic is the correct answer |
//!
//! The headline acceptance gate for the phase is that **no entry panics and no
//! `Diagnosed` entry is a parse error** — an unimplemented feature must be
//! reported by name, not by a syntax error.
//!
//! # The audit: `focus` as it stood at the start of Phase 2
//!
//! | Construct | Before | Phase 2 |
//! |---|---|---|
//! | `pattern where stmt; stmt`, `_`, vars, `Nat`, `-Nat`, `"s"`, `"s"..`, records, nesting | parses | unchanged |
//! | `QId pattern` fact pattern | parses; key **mandatory** | unchanged — a whole-predicate scan is `test.Foo _` |
//! | `p.lid` access chain | parses | plus `.value`, the fact's value side |
//! | `p = p` bind | parses | unchanged; the hard cases are rejected at typecheck |
//! | `( p )` group, `( p where … )` subquery | **no paren token at all** | added |
//! | union select `p.alt?` | not representable | added |
//! | disjunction `p \| p` | not representable | added, flat n-ary |
//! | negation `!` | not representable | added, statement prefix |
//! | `never` | not representable | added |
//! | `1__0`, `1_`, `007`, overflow | lexed silently | lexed permissively, rejected in lowering by code |
//! | string escapes | lexed, never decoded | decoded in lowering |
//!
//! # The schema these entries are written against
//!
//! Phase 8 parses schemas; until then this is a hand-built fixture (see
//! `focus::ty`). The corpus needs exactly:
//!
//! ```text
//! predicate test.Foo    : { id : int, name : string } -> string
//! predicate test.Bar    : { id : int }
//! predicate test.Edge   : { from : int, to : int }
//! predicate test.Node   : { id : int }
//! predicate test.Nested : { outer : { inner : int } }
//! predicate test.Name   : string
//! predicate test.Count  : int
//! predicate test.Shadow : { value : int }   // the `.value` shadowing case
//! ```
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use std::sync::Arc;

use lasso::Rodeo;

use crate::focus::schema::{Predicate, PredicateTy, Schema};

/// The schema the corpus is written against.
///
/// Hand-built: Phase 8 parses schemas. Field lists are sorted by name, as they are
/// everywhere ([chapter 6]) — a record's field order is part of its encoding.
///
/// [chapter 6]: ../../../docs/06-types-and-schema.md
pub fn schema() -> Schema {
    let mut names = Rodeo::new();
    let mut sym = |s: &str| names.get_or_intern(s);

    // Predicate order is predicate-id order; ids are what a `keys` row is prefixed
    // with, so this is the one place the corpus fixes them.
    let predicates = vec![
        Predicate {
            name: sym("test.Foo"),
            key: PredicateTy::Record(Arc::from([
                (sym("id"), PredicateTy::Int),
                (sym("name"), PredicateTy::Str),
            ])),
            value: Some(PredicateTy::Str),
        },
        Predicate {
            name: sym("test.Bar"),
            key: PredicateTy::Record(Arc::from([(sym("id"), PredicateTy::Int)])),
            value: None,
        },
        Predicate {
            name: sym("test.Edge"),
            key: PredicateTy::Record(Arc::from([
                (sym("from"), PredicateTy::Int),
                (sym("to"), PredicateTy::Int),
            ])),
            value: None,
        },
        Predicate {
            name: sym("test.Node"),
            key: PredicateTy::Record(Arc::from([(sym("id"), PredicateTy::Int)])),
            value: None,
        },
        Predicate {
            name: sym("test.Nested"),
            key: PredicateTy::Record(Arc::from([(
                sym("outer"),
                PredicateTy::Record(Arc::from([(sym("inner"), PredicateTy::Int)])),
            )])),
            value: None,
        },
        Predicate {
            name: sym("test.Name"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: sym("test.Count"),
            key: PredicateTy::Int,
            value: None,
        },
        // A key field literally named `value`, so `.value` is ambiguous on it —
        // the `reject/value-shadowed` case.
        Predicate {
            name: sym("test.Shadow"),
            key: PredicateTy::Record(Arc::from([(sym("value"), PredicateTy::Int)])),
            value: None,
        },
    ];

    // Field and predicate names the corpus uses but that no declaration interns,
    // so `LocalInterner`'s schema-first lookup can still resolve them.
    for name in ["a", "b", "alt", "nosuch", "value"] {
        sym(name);
    }

    Schema::new(names.into_reader(), Arc::from(predicates))
}

/// What the compiler must do with a corpus entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// Parses, typechecks, and is implemented end to end.
    Supported,
    /// Parses, then draws exactly this diagnostic code.
    ///
    /// The code — not the wording — is what tests assert on, so diagnostics can
    /// be reworded without churning the corpus. Prefixes: `nyi/` for a construct
    /// deferred to a later phase, `reject/` for one that is meaningless and never
    /// will be implemented, `lit/` for a malformed literal.
    Diagnosed(&'static str),
    /// Not valid focus; a parse diagnostic is correct.
    ParseError,
}

/// One snippet, its classification, and why it is in the corpus.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub source: &'static str,
    pub expect: Expectation,
    pub note: &'static str,
}

const fn entry(source: &'static str, expect: Expectation, note: &'static str) -> Entry {
    Entry {
        source,
        expect,
        note,
    }
}

use Expectation::{Diagnosed, ParseError, Supported};

pub const CORPUS: &[Entry] = &[
    // ---- the implemented subset: parses, typechecks, and Phase 4 flattens ----
    entry(
        "X where X = test.Foo _",
        Supported,
        "scan a predicate and bind the whole row",
    ),
    entry(
        "X where test.Foo {name = X}",
        Supported,
        "implicit bind; capture a key field",
    ),
    entry(
        "{a = X, b = Y} where test.Foo {name = X, id = Y}",
        Supported,
        "record head over two captured fields",
    ),
    entry(
        "X.name where X = test.Foo _",
        Supported,
        "field access on a bound row",
    ),
    entry(
        "X.value where X = test.Foo _",
        Supported,
        "`.value` is the fact's value side — Project::Value",
    ),
    entry(
        "X where test.Edge {from = X, to = Y}; test.Node {id = Y}",
        Supported,
        "two-level join through a shared variable",
    ),
    entry(
        "X where test.Nested {outer = {inner = X}}",
        Supported,
        "nested record pattern",
    ),
    entry(
        "X where X = test.Name \"abc\"..",
        Supported,
        "string prefix against a scalar string key — ResidualOp::Prefix",
    ),
    entry(
        "X where X = test.Count -42",
        Supported,
        "negative integer literal",
    ),
    entry(
        "X where X = test.Count -9223372036854775808",
        Supported,
        "i64::MIN — only reachable through the unary minus, so the literal itself \
         does not fit i64",
    ),
    entry(
        "X where X = test.Count 1_000",
        Supported,
        "underscore digit separator",
    ),
    entry(
        "X where test.Foo X.name",
        Supported,
        "dot binds tighter than application: this is `test.Foo (X.name)`",
    ),
    entry(
        "X where test.Foo (X.name)",
        Supported,
        "the same, parenthesised — a group is transparent",
    ),
    entry(
        "X where test.Foo _;",
        Supported,
        "a trailing `;` is permitted by `stmt (';' [stmt])*`",
    ),
    // ---- deferred constructs: parse, then say so by name ----
    entry(
        "X where test.Foo {id = X} | test.Bar {id = X}",
        Diagnosed("nyi/disjunction"),
        "disjunction survives flattening as a node (never DNF-expanded); the \
         union-of-streams operator is a deferred feature",
    ),
    entry(
        "X where test.Foo {id = X}; !test.Bar {id = X}",
        Diagnosed("nyi/negation"),
        "statement-level negation; must move after its non-locals are bound",
    ),
    entry(
        "X where X = (Y where test.Foo {id = Y})",
        Diagnosed("nyi/subquery"),
        "subquery as a pattern",
    ),
    entry(
        "X.alt? where X = test.Foo _",
        Diagnosed("nyi/union-select"),
        "union select lowers to a DiscriminantEq residual; PredicateTy has no \
         Union variant yet (I10 freezes discriminants when it does)",
    ),
    entry(
        "X where X = never",
        Diagnosed("nyi/never"),
        "the empty pattern",
    ),
    entry(
        "X where test.Foo {id = X}; test.Bar {id = Y}; X = Y",
        Diagnosed("nyi/bind-unification"),
        "`var = var` with both sides already bound — the hard half of \
         `pattern = pattern` (docs/open-decisions.md)",
    ),
    entry(
        "X where test.Foo {id = X} = test.Bar {id = X}",
        Diagnosed("nyi/bind-unification"),
        "generator = generator — also the hard half",
    ),
    entry(
        "X where {a = X} = {a = 1}",
        Diagnosed("nyi/bind-unification"),
        "anonymous record = anonymous record — also the hard half",
    ),
    // ---- meaningless: parses, rejected with a clear diagnostic ----
    entry(
        "_ where test.Foo _",
        Diagnosed("reject/wildcard-in-head"),
        "a wildcard head projects nothing",
    ),
    entry(
        "X where 42 = test.Foo _",
        Diagnosed("reject/bind-lhs"),
        "a literal cannot be a bind target",
    ),
    entry(
        "X.value where X = test.Shadow _",
        Diagnosed("reject/value-shadowed"),
        "the predicate's key has a field named `value`, so `.value` is ambiguous",
    ),
    entry(
        "X where test.Foo {name = X, name = Y}",
        Diagnosed("reject/duplicate-field"),
        "record fields are a sorted set; a duplicate is an error, not a \
         last-one-wins overwrite",
    ),
    entry(
        "X where X = nosuch.Pred _",
        Diagnosed("reject/unknown-predicate"),
        "not in the schema",
    ),
    entry(
        "X where test.Foo {nosuch = X}",
        Diagnosed("reject/unknown-field"),
        "not a field of the predicate's key",
    ),
    entry(
        "X where test.Foo {name = 42}",
        Diagnosed("reject/type-mismatch"),
        "`name` is a string",
    ),
    // ---- malformed literals: lexed permissively, rejected in lowering ----
    entry(
        "X where X = test.Count 1__0",
        Diagnosed("lit/int-underscore"),
        "repeated separator",
    ),
    entry(
        "X where X = test.Count 1_",
        Diagnosed("lit/int-underscore"),
        "trailing separator",
    ),
    entry(
        "X where X = test.Count 007",
        Diagnosed("lit/int-leading-zero"),
        "leading zero",
    ),
    entry(
        "X where X = test.Count 99999999999999999999",
        Diagnosed("lit/int-range"),
        "does not fit i64 — an error, never a panicking parse",
    ),
    // ---- not focus ----
    entry("where", ParseError, "no head, no body"),
    entry("X where", ParseError, "no statements"),
    entry(
        "X where test.Foo",
        ParseError,
        "a fact pattern's key is mandatory; the whole-predicate scan is \
         `test.Foo _`",
    ),
    entry("X where X = }", ParseError, "junk"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::parse::parse;

    /// The phase's headline gate: every construct on the target surface parses,
    /// and only the entries that are genuinely not focus draw a parse error. An
    /// unimplemented feature must be reported by name later, never as a syntax
    /// error here.
    #[test]
    fn every_entry_parses_as_classified() {
        // Accumulate rather than assert per entry: one run then reports every
        // remaining gap, which is what makes this readable as a ledger.
        let mut gaps = vec![];

        for Entry {
            source,
            expect,
            note,
        } in CORPUS
        {
            let parsed = parse(source);
            let diags: Vec<&String> = parsed.diagnostics().iter().map(|d| &d.message).collect();

            match expect {
                Supported | Diagnosed(_) if parsed.has_errors() => {
                    gaps.push(format!("{source:?} must parse ({note}) — got {diags:?}"))
                }
                ParseError if !parsed.has_errors() => {
                    gaps.push(format!("{source:?} must be a parse error ({note})"))
                }
                _ => {}
            }
        }

        assert!(
            gaps.is_empty(),
            "{} of {} corpus entries are not yet on the surface:\n  {}",
            gaps.len(),
            CORPUS.len(),
            gaps.join("\n  ")
        );
    }

    /// The other half: each `Diagnosed` entry draws exactly the code it claims,
    /// and each `Supported` entry draws nothing.
    #[test]
    #[ignore = "Phase 2 — pending typecheck (2.7, 2.8)"]
    fn every_entry_is_diagnosed_as_classified() {
        unimplemented!("2.8: compile each entry through typecheck and assert the diagnostic codes");
    }
}
