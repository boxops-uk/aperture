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
