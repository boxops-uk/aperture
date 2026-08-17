//! The schema DSL's front end: lex → parse → (Phase 8.3) lower.
//!
//! [Operations §10](../../../docs/aperture-cli-design.md) puts "parse → AST → canonical
//! model; imports/resolution; fingerprints" in this crate, which is why the bottom of
//! the stack has a grammar in it. The dependency *direction* is unchanged: nothing
//! above needs to know a schema was ever text.
//!
//! # What is here, and what is deliberately not
//!
//! This is Phase 8.2 — the surface parses, and every construct the type model cannot
//! yet hold draws a named code rather than a syntax error
//! ([`diag::Code`]). Lowering to `Schema`, the canonical form and the fingerprints are
//! 8.3; imports are resolved at 8.4.
//!
//! **A derivation's body is not in the grammar, and that is a recorded decision rather
//! than an oversight.** Angle parses queries and schemas with one grammar, so a
//! derivation body is simply a `query` non-terminal. Aperture split the two: `focus`
//! lives in `aperture-engine`, which is *above* this crate, so a schema parser here
//! cannot parse a query without inverting the dependency. The declaration *form* is
//! accepted — `stored` marks a predicate as derived — and how the body is carried
//! across that boundary is settled when Phase 8b needs it. The options are already
//! visible: a delimited raw span this crate never interprets, or moving the derivation
//! into the query language's own file.

pub mod corpus;
pub mod diag;
pub mod lexer;
pub mod lower;
pub mod parse;
pub mod parser;
pub mod print;
pub mod resolve;

use crate::schema::Schema;

/// Read a schema from source: parse, lower, and hand back what it declares.
///
/// The whole front end in one call, for a caller that has text and wants a schema.
/// Diagnostics come back **rendered against the source**, because every caller of this
/// is reporting to a person and the alternative is each of them building a
/// `SimpleFile` of its own.
///
/// # Errors
///
/// The rendered diagnostics, when the source does not parse or does not lower.
pub fn read(name: &str, source: &str) -> Result<Schema, String> {
    read_with(name, source, lower::lower)
}

/// Read a schema **a database already numbered** — see [`lower::recover`].
///
/// The reader of [`print::print`]'s output, and the one call that must not re-assign
/// ids: they are frozen in the tag of every fact the database holds.
///
/// # Errors
///
/// As [`read`].
pub fn recover(name: &str, source: &str) -> Result<Schema, String> {
    read_with(name, source, lower::recover)
}

fn read_with(
    name: &str,
    source: &str,
    lower: fn(&parser::Cst<'_>, &mut Vec<diag::Diagnostic>) -> Option<lower::Lowered>,
) -> Result<Schema, String> {
    let mut diags = vec![];

    let lowered = parse::parse(source, &mut diags).and_then(|cst| lower(&cst, &mut diags));

    match lowered {
        // A schema that lowered *and* complained is still refused: every diagnostic
        // this front end raises is an error, and a schema half of which was dropped is
        // not the schema anybody wrote down.
        Some(lowered) if diags.is_empty() => Ok(lowered.schema),

        _ => {
            let rendered = diag::render(name, source, &diags);

            // A refusal always has a reason. An empty sink here is a compiler bug
            // rather than a bad schema, and one that costs an afternoon: the caller
            // reports "it did not lower" with nothing to point at.
            Err(if rendered.is_empty() {
                format!("{name}: not a schema, and the compiler said nothing — this is a bug")
            } else {
                rendered
            })
        }
    }
}
