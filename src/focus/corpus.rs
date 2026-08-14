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
//! | [`Expectation::Supported`] | parses, typechecks, is implemented, and **returns these rows** against the shared [`fixture`](crate::focus::fixture) |
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
//! # The database these entries run against
//!
//! The shared [`fixture`](crate::focus::fixture) — its schema, its facts, and the
//! same rows the shell serves, so a corpus entry is something a person can type at
//! the prompt. Every `Supported` entry records what it returns, and the gate below
//! runs it against a **real** `FjallDb` to check.
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use crate::focus::{diag::Code, fixture, schema::Schema};
use Expectation::{Diagnosed, ParseError, Supported};

/// The schema the corpus is written against — the shared
/// [`fixture`](crate::focus::fixture), which the shell serves too.
#[must_use]
pub fn schema() -> Schema {
    fixture::schema()
}

/// What the compiler must do with a corpus entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// Parses, typechecks, produces a plan, and running it against the
    /// [`fixture`](crate::focus::fixture) returns exactly these rows.
    ///
    /// The rows are a *rendering* — `1`, `ann`, `{a = ann, b = 1}`,
    /// `test.Foo#1` for a reference — joined by `"; "`, and empty for no rows.
    /// Carried in the variant rather than beside it so that a newly-supported
    /// construct cannot be marked supported without saying what it answers.
    Supported(&'static str),
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
        Supported("test.Foo#1; test.Foo#2; test.Foo#3"),
        "scan a predicate and bind the whole row",
    ),
    entry(
        "X where test.Foo {name = X}",
        Supported("ann; bob; ann"),
        "implicit bind; capture a key field",
    ),
    entry(
        "{a = X, b = Y} where test.Foo {name = X, id = Y}",
        Supported("{a = ann, b = 1}; {a = bob, b = 2}; {a = ann, b = 3}"),
        "record head over two captured fields",
    ),
    entry(
        "X.name where X = test.Foo _",
        Supported("ann; bob; ann"),
        "field access on a bound row",
    ),
    entry(
        "X.value where X = test.Foo _",
        Supported("one; two; three"),
        "`.value` is the fact's value side — Project::Value",
    ),
    entry(
        "X where test.Edge {from = X, to = Y}; test.Node {id = Y}",
        Supported("1; 1; 2"),
        "two-level join through a shared variable",
    ),
    entry(
        "X where test.Nested {outer = {inner = X}}",
        Supported("1; 7"),
        "nested record pattern",
    ),
    entry(
        "X where X = test.Name \"abc\"..",
        Supported("test.Name#1"),
        "string prefix against a scalar string key — ResidualOp::Prefix",
    ),
    entry(
        "X where X = test.Count -42",
        Supported("test.Count#2"),
        "negative integer literal",
    ),
    entry(
        "X where X = test.Count -9223372036854775808",
        Supported("test.Count#1"),
        "i64::MIN — only reachable through the unary minus, so the literal itself \
         does not fit i64",
    ),
    entry(
        "X where X = test.Count 1_000",
        Supported("test.Count#4"),
        "underscore digit separator",
    ),
    entry(
        "Y where Y = test.Foo _; test.Name Y.name",
        Supported("test.Foo#1; test.Foo#2; test.Foo#3"),
        "dot binds tighter than application: this is `test.Name (Y.name)`. The \
         precedence itself is pinned structurally in `parse.rs`; this entry is here \
         to check the well-formed case typechecks",
    ),
    entry(
        "Y where Y = test.Foo _; test.Name (Y.name)",
        Supported("test.Foo#1; test.Foo#2; test.Foo#3"),
        "the same, parenthesised — a group is transparent",
    ),
    entry(
        "X where X = test.Foo _;",
        Supported("test.Foo#1; test.Foo#2; test.Foo#3"),
        "a trailing `;` is permitted by `stmt (';' [stmt])*`",
    ),
    entry(
        "X where X = test.Foo {id = 1}",
        Supported("test.Foo#1"),
        "a constant in the leading key field narrows the scan to a seek",
    ),
    entry(
        "X where test.Foo {id = X, name = \"ann\"}",
        Supported("1; 3"),
        "a capture cannot narrow the scan, so the constant behind it filters — \
         sargeability is order-dependent",
    ),
    entry(
        "Y where test.Count Y",
        Supported("-9223372036854775808; -42; 7; 1000"),
        "a scalar key is one field, so a variable may stand for the whole of it",
    ),
    entry(
        "X where test.Ref {of = X}",
        Supported("test.Foo#1; test.Foo#2"),
        "a fact-typed field may be captured and projected — the row it names is a \
         `Value::FactRef`, and reads no second fact to say so",
    ),
    entry(
        "P where P = test.Foo {id = 1}; test.Ref {of = P}",
        Supported("test.Foo#1"),
        "**a join through a reference**: the bound row's fact id is spliced into the \
         seek, so the reference is followed without a store read",
    ),
    entry(
        "P where test.Ref {of = P}; P = test.Foo {id = 1}",
        Supported("test.Foo#1"),
        "**the same join written in the order that reads before it binds** — the \
         statement that captures `P` is second, so `reorder` moves it first; the \
         same rows as the spelling above, because it is the same plan",
    ),
    entry(
        "P where P = test.Foo {id = 1}; test.Link {at = X, of = P}",
        Supported("test.Foo#1"),
        "the same compare once the seek prefix has closed — a capture at `at` closes \
         it, so `of` filters instead",
    ),
    entry(
        "X where X = test.Ref {of = test.Foo {id = 1}}",
        Supported("test.Ref#1"),
        "**the idiomatic spelling of that join**: a fact pattern inside another is a \
         generator, hoisted into a loop level of its own and matched by id",
    ),
    entry(
        "X where X = test.Deep {via = test.Ref {of = test.Foo {id = 1}}}",
        Supported("test.Deep#1"),
        "hoisting is recursive — innermost first, so each level is bound before the \
         one that names it",
    ),
    entry(
        "test.Bar {id = 1} where test.Foo _",
        Supported("test.Bar#1; test.Bar#1; test.Bar#1"),
        "a fact pattern in the **head** is the same construct: hoisted into the last \
         level, and projected as the fact it names",
    ),
    // ---- deferred constructs: parse, then say so by name ----
    entry(
        "X where test.Foo {id = X} | test.Bar {id = X}",
        Supported("1; 2; 3; 1; 2"),
        "**a disjunction**, which is one level with an alternative per branch — \
         never DNF-expanded across conjuncts, and the rows are the branches' \
         concatenated in order rather than merged or deduplicated",
    ),
    entry(
        "X where test.Foo {id = X}; !test.Bar {id = X}",
        Diagnosed(Code::NyiNegation),
        "statement-level negation; must move after its non-locals are bound",
    ),
    entry(
        "X where X = (Y where test.Foo {id = Y})",
        Supported("1; 2; 3"),
        "**a subquery**, which inlines: its statements become the enclosing \
         query's and its head is the value the bind names",
    ),
    entry(
        "X.alt? where X = test.Foo _",
        Diagnosed(Code::NyiUnionSelect),
        "union select lowers to a DiscriminantEq residual; PredicateTy has no \
         Union variant yet (I10 freezes discriminants when it does)",
    ),
    entry(
        "X where X = never",
        Supported(""),
        "**the empty pattern**: a level with no alternative to open, which is \
         exhausted the moment it is entered",
    ),
    entry(
        "Y where X = test.Foo _; Y = X.name",
        Supported("ann; bob; ann"),
        "an **alias**: a name for a value that is already in a register, so it \
         substitutes exactly as a constant does — no register, no step, and the same \
         plan as projecting the read directly",
    ),
    entry(
        "Y where X = test.Foo _; Y = X.name; test.Name Y",
        Supported("ann; bob; ann"),
        "the alias reaching a **key field**, where it splices the register it names \
         rather than comparing a value — the point of substituting a location",
    ),
    entry(
        "Y where test.Foo {name = X}; Y = X",
        Supported("ann; bob; ann"),
        "`var = var` with only one side bound: the same substitution with an empty path",
    ),
    entry(
        "Y where X = test.Foo _; Y = X.value",
        Supported("one; two; three"),
        "a `.value` alias projects; matching on it stays deferred ([I6](invariants.md))",
    ),
    entry(
        "X where test.Nested {outer = {inner = Y}}; X = {inner = Y}",
        Diagnosed(Code::NyiValueBind),
        "what is left of the value bind: a record mentioning a **captured** variable \
         is in no register and differs per row, so it would have to be *built* — the \
         derived bind the machine has a step for and the language has no producer for",
    ),
    entry(
        "Y where test.Ref {of = P}; Y = P.name",
        Supported("ann; bob"),
        "naming a read *through* a reference is the same substitution: the alias names \
         the fetched row's field, so it is the same plan as writing the read in place",
    ),
    entry(
        "X where test.Foo {id = X}; test.Bar {id = Y}; X = Y",
        Supported("1; 2"),
        "`var = var` with **both** sides already bound: a residual on whichever \
         level binds later, which is where a value already in a register is \
         compared against another",
    ),
    entry(
        "X where test.Foo {id = X} = test.Bar {id = X}",
        Diagnosed(Code::NyiBindUnification),
        "generator = generator — also the hard half",
    ),
    entry(
        "X where P = test.Nested _; {inner = X} = P.outer",
        Supported("1; 7"),
        "a record pattern **destructuring a place** rather than a constant: each \
         piece names a piece of `P`'s row, so this is the same plan as `X = \
         P.outer.inner` and as the nested-pattern spelling",
    ),
    entry(
        "X where P = test.Wide _; {extra = _, inner = X} = P.outer",
        Supported("2"),
        "a **wildcard piece** binds nothing and cannot fail — the tautology Glean's \
         expansion drops, which decomposing against a slot never builds",
    ),
    entry(
        "X where {a = X} = {a = 1}",
        Supported("1"),
        "a record **destructured against a constant**: each variable folds into its \
         piece, so this is exactly the sugar it looks like — the same plan as writing \
         `X = 1`. Sound only because the right side is constant, and only because a \
         literal leaf on the *left* is refused: `{a = 1} = {a = 2}` would bind nothing \
         and so mean `true` where it means the empty relation",
    ),
    entry(
        "X where test.Foo {id = X}; {a = X} = {a = Y}",
        Diagnosed(Code::NyiValueBind),
        "the same shape with a **non-constant** right side. The line between the two \
         deferrals is where a value *is*: `{a = Y}` is in no register and would have \
         to be built, which is the value bind — where two things that are each \
         somewhere would only need comparing, which is the bind unification above",
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
        Supported("42"),
        "a variable bound to a literal is **folded**: substituted at every use, so \
         it takes no register and no plan step. This one folds away entirely, \
         leaving a plan with no levels — the unit relation, exactly one row",
    ),
    entry(
        "Z where Z = 1; test.Bar {id = Z}",
        Supported("1"),
        "the same fold **narrowing a seek**: `{id = Z}` seeks the bytes `{id = 1}` \
         seeks, because the fold is seen through by the same code that encodes a \
         literal written in place",
    ),
    entry(
        "Z where test.Bar {id = Z}; Z = 1",
        Supported("1"),
        "**the same fold written after the field that captures the variable** — the \
         fold is collected from the whole body before any statement is lowered, so \
         this reaches `emit` with the same bindings as the spelling above and is the \
         same plan. No reordering involved: a constant takes no level to move",
    ),
    entry(
        "X where X = {inner = 1}; test.Nested {outer = X}",
        Supported("{inner = 1}"),
        "a **record** of constants folds too, and narrows a nested key field. The \
         wrapped form `constant` writes is right inside a field and would be wrong \
         for a whole key — safe because `key` destructures the top-level record \
         itself, and a bare variable as a whole key is `nyi/whole-key` first",
    ),
    entry(
        "X where test.Nested {outer = X}; X = {inner = 1}",
        Supported("{inner = 1}"),
        "and the record fold the other way round, at a record-typed field — the \
         wrapped-bytes trap above, reached from the spelling that names the variable \
         first",
    ),
    entry(
        "{x = A.x, y = A.y} where A = {x = 2, y = 3}",
        Supported("{x = 2, y = 3}"),
        "**reading a field through a folded constant is folded too**: the substitution \
         goes through the *access*, not just the variable. Stopping at the variable \
         declined quietly here, so flatten returned no plan with nothing reported — \
         found as a panic from the shell",
    ),
    entry(
        "A.x where A = {x = 1}; test.Bar {id = A.x}",
        Supported("1"),
        "the same read at a **key field**, narrowing the seek exactly as the literal in \
         place would. This is the half that was worse: the constraint was dropped with \
         no diagnostic, so the level matched every row",
    ),
    entry(
        "{a = X, b = Z} where test.Edge {from = X, to = _}; Z = 7",
        Supported("{a = 1, b = 7}; {a = 1, b = 7}; {a = 2, b = 7}"),
        "and a fold read by the head beside a captured field — one row per edge, \
         the folded value repeated, which is what says folding did not turn a \
         constant into a level of its own",
    ),
    entry(
        "X.name where test.Ref {of = X}",
        Supported("ann; bob"),
        "**reading through** a reference: `X` holds a fact id and its fields are in \
         another fact's key, so the fact it names is fetched into a level of its own \
         (`Source::Fetch`) and read from there. *Following* a reference still reads \
         nothing — that is the id compare above",
    ),
    entry(
        "X.value where test.Ref {of = X}",
        Supported("one; two"),
        "the same through the value side: one register, and the value one point read \
         further off it — the arm that used to decline *quietly*, which is what makes \
         the `flatten_ordered` promise-guard load-bearing here",
    ),
    entry(
        "N where test.Deep {via = R}; N = R.of.name",
        Supported("ann; bob"),
        "a **chain** of references is a chain of fetches, each reading the register the \
         one before it bound — two hops, three levels, and no join",
    ),
    entry(
        "{a = X.id, b = X.name} where test.Ref {of = X}",
        Supported("{a = 1, b = ann}; {a = 2, b = bob}"),
        "two reads of one reference are **one** fetch: a second level would read the \
         same row again for every row above it, and could never disagree with the first",
    ),
    entry(
        "P.id where test.Ref {of = P}; test.Bar {id = P.id}",
        Supported("1; 2"),
        "a field read through a reference **narrows** the level that reads it — the \
         fetch is an outer level, so its register splices into the seek below it",
    ),
    entry(
        "Y where Y = test.Foo _; test.Name Y.value",
        Diagnosed(Code::NyiValueMatch),
        "a value may be projected but not matched: I6 keeps `entities` out of the \
         scan loop",
    ),
    entry(
        "Y where test.Foo Y",
        Supported("{id = 1, name = ann}; {id = 2, name = bob}; {id = 3, name = ann}"),
        "**a whole key**, which is its fields: a stored key is flat, so the record \
         is built one field at a time and needs no operator of its own",
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
                Supported(_) | Diagnosed(_) if diagnostics.has_errors() => {
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

            if matches!(expect, Supported(_)) && !plan {
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
                Supported(_) => vec![],
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

    /// **The phase's headline gate: every supported entry runs, against a real
    /// database, and returns the rows it says it does.**
    ///
    /// Until now `Supported` meant "produces a plan", which is not the same claim: a
    /// plan that seeks the wrong prefix, filters on the wrong field or projects the
    /// wrong path is still a plan. This runs each one through `enumerate` over a
    /// [`FjallDb`] seeded from the shared fixture and compares the rows.
    ///
    /// One database for the whole corpus, not one per entry: creating a keyspace is
    /// fsync-bound at tens of milliseconds a tree, and the queries only read.
    ///
    /// Rows are compared as a **rendering** rather than as `Value`s, so the expected
    /// answer is something a person can read in the table and check by eye — and so a
    /// reference is written as the fact it names rather than as a snowflake integer.
    #[test]
    fn every_supported_entry_returns_its_rows() {
        use crate::focus::{compile::Compilation, fixture, plan::FactId, store::FjallDb};

        let dir = tempfile::tempdir().expect("a scratch directory");
        let db = FjallDb::open(dir.path()).expect("open");
        let schema = schema();

        for fixture::Fact {
            predicate,
            key,
            value,
            sequence,
        } in fixture::facts()
        {
            let id = db.put_fact(predicate, &key, &value).expect("put");
            assert_eq!(
                id,
                FactId::new(predicate, sequence).expect("a fixture fact id"),
                "the store's allocator diverged from the fixture's numbering",
            );
        }

        let mut wrong = vec![];

        for Entry {
            source,
            expect,
            note,
        } in CORPUS
        {
            let Supported(want) = expect else { continue };

            let mut compilation = Compilation::new(source, &schema);
            let Some(plan) = compilation.plan() else {
                // `every_entry_is_diagnosed_as_classified` owns this failure; saying
                // it twice would only make the other one harder to read.
                continue;
            };

            let got = match run(&db, plan, compilation.interner(), &schema) {
                Ok(rows) => rows,
                Err(error) => {
                    wrong.push(format!(
                        "{source:?}\n      failed to run: {error}  ({note})"
                    ));
                    continue;
                }
            };

            if got != *want {
                wrong.push(format!(
                    "{source:?}\n      want {want:?}\n      got  {got:?}  ({note})"
                ));
            }
        }

        assert!(
            wrong.is_empty(),
            "{} of {} supported entries answer differently than recorded:\n    {}",
            wrong.len(),
            CORPUS
                .iter()
                .filter(|entry| matches!(entry.expect, Supported(_)))
                .count(),
            wrong.join("\n    "),
        );
    }

    /// Run a plan to completion and render its rows.
    fn run(
        db: &crate::focus::store::FjallDb,
        plan: crate::focus::plan::Plan,
        interner: &crate::focus::schema::LocalInterner,
        schema: &Schema,
    ) -> Result<String, crate::focus::error::ApertureError> {
        use crate::focus::iter::{Executor, Iteratee, Stream};
        use tokio_util::sync::CancellationToken;

        let executor = Executor::new(db.reader(), plan);
        let rendered = executor.enumerate(
            Vec::new(),
            |mut rows: Vec<String>, mut row| {
                rows.push(render(&row.to_value(interner)?, schema));
                Ok(Stream::Continue(rows))
            },
            &CancellationToken::new(),
        )?;

        let rows = match rendered {
            Iteratee::Done(rows) | Iteratee::Suspended(rows, _) => rows,
        };

        Ok(rows.join("; "))
    }

    /// A row as the corpus writes it: bare scalars, `{a = …}` for a record, and
    /// `test.Foo#1` for a reference — the predicate it belongs to and its sequence
    /// within it, which is also its position in the fixture.
    fn render(value: &crate::focus::tuple::Value, schema: &Schema) -> String {
        use crate::focus::tuple::Value;

        match value {
            Value::Int(n) => n.to_string(),
            Value::Str(s) => s.clone(),
            Value::FactRef(id) => {
                let name = schema
                    .get(id.predicate())
                    .and_then(|predicate| predicate.name())
                    .unwrap_or("?");

                format!("{name}#{}", id.sequence())
            }
            Value::Record(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(name, field)| format!("{name} = {}", render(field, schema)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            other => format!("{other:?}"),
        }
    }
}
