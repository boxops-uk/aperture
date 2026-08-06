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
//! predicate test.Wide   : { outer : { extra : int, inner : int } }
//! predicate test.Ref    : { of : test.Foo }         // a fact-typed field
//! ```
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use std::sync::Arc;

use lasso::Rodeo;

use crate::focus::{
    diag::Code,
    schema::{Predicate, PredicateId, PredicateTy, Schema},
};
use Expectation::{Diagnosed, ParseError, Supported};

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
        // Deliberately `test.Nested`'s field name carrying a differently-shaped
        // record: the only way a query in the implemented subset can make two record
        // *types* meet, which is what exercises unification's exact-arity rule.
        // Appended last — a predicate's position is its id, and the tests assert on
        // those, so inserting anywhere else renumbers them.
        Predicate {
            name: sym("test.Wide"),
            key: PredicateTy::Record(Arc::from([(
                sym("outer"),
                PredicateTy::Record(Arc::from([
                    (sym("extra"), PredicateTy::Int),
                    (sym("inner"), PredicateTy::Int),
                ])),
            )])),
            value: None,
        },
        // A **fact-typed key field**, which nothing else here has: it is what makes
        // the deferred cross-fact cases reachable at all. A reference is a `FactId`
        // and a register holds its own row, so matching or capturing one needs
        // navigation the `Plan` IR does not have (`nyi/fact-field`), and a fact
        // pattern written in the field is a nested generator. Also appended last.
        Predicate {
            name: sym("test.Ref"),
            key: PredicateTy::Record(Arc::from([(sym("of"), PredicateTy::Fact(PredicateId(0)))])),
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
    /// be reworded without churning the corpus. [`Code::kind`] says which sort of
    /// fault it is: deferred to a later phase, meaningless and rejected for good,
    /// or a malformed literal.
    Diagnosed(Code),
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
        "Y where Y = test.Foo _; test.Name Y.name",
        Supported,
        "dot binds tighter than application: this is `test.Name (Y.name)`. The \
         precedence itself is pinned structurally in `parse.rs`; this entry is here \
         to check the well-formed case typechecks",
    ),
    entry(
        "Y where Y = test.Foo _; test.Name (Y.name)",
        Supported,
        "the same, parenthesised — a group is transparent",
    ),
    entry(
        "X where X = test.Foo _;",
        Supported,
        "a trailing `;` is permitted by `stmt (';' [stmt])*`",
    ),
    entry(
        "X where X = test.Foo {id = 1}",
        Supported,
        "a constant in the leading key field narrows the scan to a seek",
    ),
    entry(
        "X where test.Foo {id = X, name = \"a\"}",
        Supported,
        "a capture cannot narrow the scan, so the constant behind it filters — \
         sargeability is order-dependent",
    ),
    entry(
        "Y where test.Count Y",
        Supported,
        "a scalar key is one field, so a variable may stand for the whole of it",
    ),
    // ---- deferred constructs: parse, then say so by name ----
    entry(
        "X where test.Foo {id = X} | test.Bar {id = X}",
        Diagnosed(Code::NyiDisjunction),
        "disjunction survives flattening as a node (never DNF-expanded); the \
         union-of-streams operator is a deferred feature",
    ),
    entry(
        "X where test.Foo {id = X}; !test.Bar {id = X}",
        Diagnosed(Code::NyiNegation),
        "statement-level negation; must move after its non-locals are bound",
    ),
    entry(
        "X where X = (Y where test.Foo {id = Y})",
        Diagnosed(Code::NyiSubquery),
        "subquery as a pattern",
    ),
    entry(
        "X.alt? where X = test.Foo _",
        Diagnosed(Code::NyiUnionSelect),
        "union select lowers to a DiscriminantEq residual; PredicateTy has no \
         Union variant yet (I10 freezes discriminants when it does)",
    ),
    entry(
        "X where X = never",
        Diagnosed(Code::NyiNever),
        "the empty pattern",
    ),
    entry(
        "X where test.Foo {id = X}; test.Bar {id = Y}; X = Y",
        Diagnosed(Code::NyiBindUnification),
        "`var = var` with both sides already bound — the hard half of \
         `pattern = pattern` (docs/open-decisions.md)",
    ),
    entry(
        "X where test.Foo {id = X} = test.Bar {id = X}",
        Diagnosed(Code::NyiBindUnification),
        "generator = generator — also the hard half",
    ),
    entry(
        "X where {a = X} = {a = 1}",
        Diagnosed(Code::NyiBindUnification),
        "anonymous record = anonymous record — also the hard half",
    ),
    // ---- meaningless: parses, rejected with a clear diagnostic ----
    entry(
        "_ where test.Foo _",
        Diagnosed(Code::RejectWildcardInHead),
        "a wildcard head projects nothing",
    ),
    entry(
        "X where 42 = test.Foo _",
        Diagnosed(Code::RejectBindLhs),
        "a literal cannot be a bind target",
    ),
    entry(
        "X.value where X = test.Shadow _",
        Diagnosed(Code::RejectValueShadowed),
        "the predicate's key has a field named `value`, so `.value` is ambiguous",
    ),
    entry(
        "X where test.Foo {name = X, name = Y}",
        Diagnosed(Code::RejectDuplicateField),
        "record fields are a sorted set; a duplicate is an error, not a \
         last-one-wins overwrite",
    ),
    entry(
        "X where X = nosuch.Pred _",
        Diagnosed(Code::RejectUnknownPredicate),
        "not in the schema",
    ),
    entry(
        "X where test.Foo {nosuch = X}",
        Diagnosed(Code::RejectUnknownField),
        "not a field of the predicate's key",
    ),
    entry(
        "X where test.Foo {name = 42}",
        Diagnosed(Code::RejectTypeMismatch),
        "`name` is a string",
    ),
    entry(
        "X where test.Foo X.name",
        Diagnosed(Code::RejectUnresolvedAccess),
        "nothing binds `X`, so there is no type to read `name` from. Resolving it \
         would need row polymorphism; Phase 4's range-restriction check would reject \
         the query anyway",
    ),
    entry(
        "X.value where X = test.Bar _",
        Diagnosed(Code::RejectNoValue),
        "`test.Bar` is key-only",
    ),
    // ---- malformed literals: lexed permissively, rejected in lowering ----
    entry(
        "X where X = test.Count 1__0",
        Diagnosed(Code::LitIntUnderscore),
        "repeated separator",
    ),
    entry(
        "X where X = test.Count 1_",
        Diagnosed(Code::LitIntUnderscore),
        "trailing separator",
    ),
    entry(
        "X where X = test.Count 007",
        Diagnosed(Code::LitIntLeadingZero),
        "leading zero",
    ),
    entry(
        "X where X = test.Count 99999999999999999999",
        Diagnosed(Code::LitIntRange),
        "does not fit i64 — an error, never a panicking parse",
    ),
    entry(
        "X where X = test.Count 9223372036854775808",
        Diagnosed(Code::LitIntRange),
        "one past i64::MAX; only reachable with a minus in front of it",
    ),
    entry(
        r#"X where X = test.Name "\uD800""#,
        Diagnosed(Code::LitStringEscape),
        "an unpaired surrogate. The lexer's regex accepts the escape, so this is only \
         catchable when the string is decoded",
    ),
    // ---- deferred at flatten: parse, typecheck, then say so by name ----
    entry(
        "X where X = 42",
        Diagnosed(Code::NyiValueBind),
        "binding a variable to a value no generator produced is a derived bind \
         (PLAN Phase 6), which needs the `Slot` value variant",
    ),
    entry(
        "test.Bar {id = 1} where test.Foo _",
        Diagnosed(Code::NyiNestedGenerator),
        "a fact pattern away from the top level of a statement is a generator that \
         has to be hoisted into its own loop level",
    ),
    entry(
        "X where test.Ref {of = X}",
        Diagnosed(Code::NyiFactField),
        "a fact-typed field holds a reference; reading through it is cross-fact \
         navigation (`Access::Fetch`), and matching one against a bound row needs a \
         fact-id splice",
    ),
    entry(
        "Y where Y = test.Foo _; test.Name Y.value",
        Diagnosed(Code::NyiValueMatch),
        "a value may be projected but not matched: I6 keeps `entities` out of the \
         scan loop",
    ),
    entry(
        "Y where test.Foo Y",
        Diagnosed(Code::NyiWholeKey),
        "a stored key is its fields with no wrapper, so a record key is not one \
         field and has no path to project; name its fields instead",
    ),
    // ---- meaningless at flatten ----
    entry(
        "X where test.Foo _",
        Diagnosed(Code::RejectUnboundVariable),
        "range restriction: nothing captures `X`, so there are no values for it to \
         range over",
    ),
    entry(
        "X where test.Edge {from = X, to = X}",
        Diagnosed(Code::NyiRepeatedVariable),
        "an intra-row repeat needs a same-row `EqField` residual; the Phase 4 \
         decision is to reject it for now rather than add an operator nothing else \
         uses (docs/open-decisions.md)",
    ),
    entry(
        "X where X = test.Foo _; 42",
        Diagnosed(Code::RejectNotAGenerator),
        "a statement that is not a fact pattern generates nothing and constrains \
         nothing",
    ),
    entry(
        "\"abc\".. where test.Foo _",
        Diagnosed(Code::RejectNotProjectable),
        "a string prefix is a pattern, not a value, so it cannot be a head",
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
    use crate::focus::{diag::Diagnostics, parse::parse};

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
            let mut diagnostics = Diagnostics::new();
            let _cst = parse(source, &mut diagnostics);
            let diags: Vec<&String> = diagnostics.iter().map(|d| &d.message).collect();

            match expect {
                Supported | Diagnosed(_) if diagnostics.has_errors() => {
                    gaps.push(format!("{source:?} must parse ({note}) — got {diags:?}"))
                }
                ParseError if !diagnostics.has_errors() => {
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

    /// The other half, and the phase's headline claim: each `Diagnosed` entry draws
    /// exactly the code it claims — and nothing else — while each `Supported` entry
    /// draws nothing at all **and produces a runnable plan**.
    ///
    /// Asserting the *set* of codes rather than "contains" is deliberate. A
    /// construct reported as not-yet-implemented must not also produce a type error
    /// about itself; cascading is the failure mode this pass rolls back its
    /// substitutions to avoid.
    #[test]
    fn every_entry_is_diagnosed_as_classified() {
        let mut wrong = vec![];

        for Entry {
            source,
            expect,
            note,
        } in CORPUS
        {
            // Not focus at all — the parse half of the gate owns these.
            if matches!(expect, ParseError) {
                continue;
            }

            let (got, plan) = compile(source);
            let got: Vec<&str> = got.iter().map(String::as_str).collect();

            if matches!(expect, Supported) && !plan {
                wrong.push(format!(
                    "{source:?}\n      is Supported but produced no plan  ({note})"
                ));
                continue;
            }
            // Compared as rendered strings, not as `Code`s: reading `got` back into
            // a `Code` would have to do something with a string that resolves to no
            // variant, and every choice there hides an unexpected diagnostic — which
            // is the one thing this gate exists to catch.
            let want: Vec<&str> = match expect {
                Supported => vec![],
                Diagnosed(code) => vec![code.as_str()],
                ParseError => unreachable!(),
            };

            if got != want {
                wrong.push(format!(
                    "{source:?}\n      want {want:?}, got {got:?}  ({note})"
                ));
            }
        }

        assert!(
            wrong.is_empty(),
            "{} of {} entries are diagnosed differently than classified:\n    {}",
            wrong.len(),
            CORPUS.len(),
            wrong.join("\n    ")
        );
    }

    /// Every distinct diagnostic code `source` draws, in first-seen order, and
    /// whether it produced a plan.
    ///
    /// Driven through [`Compilation`] rather than by calling the phases by hand, so
    /// the gate covers the **whole** front end — lowering, typecheck *and* flatten.
    /// That is what makes `Supported` mean "runs" rather than "typechecks": every
    /// entry in the implemented subset has to come out the far end as a plan.
    ///
    /// Codes are collected as they come, *not* filtered against the ones the corpus
    /// knows about: a code nobody expected has to be able to fail this gate, which is
    /// the whole point of comparing sets.
    fn compile(source: &str) -> (Vec<String>, bool) {
        use crate::focus::compile::Compilation;

        let schema = schema();
        let mut compilation = Compilation::new(source, &schema);

        // A refused parse is the parse gate's business, and reports nothing here.
        let plan = compilation.plan().is_some();

        let mut codes: Vec<String> = vec![];
        for diag in compilation.diagnostics() {
            // Parse diagnostics carry no code, and the parse gate owns them.
            if let Some(code) = diag.code.as_deref()
                && !codes.iter().any(|seen| seen == code)
            {
                codes.push(code.to_owned());
            }
        }

        (codes, plan)
    }
}
