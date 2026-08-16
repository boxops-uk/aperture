//! Parsing a schema file into a lossless CST.
//!
//! The entry point, and the two bounds that make a refusal a refusal rather than a
//! crash. Mirrors `aperture_engine::parse` — including the reasons, which are the same
//! reasons.

use codespan_reporting::diagnostic::Label;

use super::{
    diag::Diagnostic,
    lexer::{Token, tokenize},
    parser::{Cst, Lexed, Parser},
};

/// The longest source this crate will address.
///
/// A span is a byte offset, and everything downstream narrows one; refusing a source
/// nobody can point into is what keeps that narrowing lossless rather than silently
/// wrapping past the limit.
const MAX_SOURCE_LEN: usize = u32::MAX as usize;

/// How deeply a type may nest before the recursive-descent parser is refused.
///
/// The generated parser is recursive descent and `ty` recurses through records, arrays,
/// `maybe` and `set`, so deep input would overflow the stack — a panic on a data path,
/// which [conventions](../../../../docs/conventions.md) does not allow. A schema is
/// written by a person; 64 is far past anything anyone means and far below what would
/// exhaust a stack.
const MAX_NEST_DEPTH: usize = 64;

/// Parse `source`, reporting into `diags`.
///
/// `None` is a **refusal** — an unaddressable source, or one nested past
/// [`MAX_NEST_DEPTH`] — and means there is no tree at all. A tree *with errors in it* is
/// the ordinary case and comes back as `Some` with diagnostics beside it, because
/// permissive-early needs a reader to get every complaint at once rather than the first.
pub fn parse<'src>(source: &'src str, diags: &mut Vec<Diagnostic>) -> Option<Cst<'src>> {
    if source.len() > MAX_SOURCE_LEN {
        // No label: there is no span a renderer could address, which is the whole
        // reason for the limit.
        diags.push(Diagnostic::error().with_message(format!(
            "schema source is {} bytes; the limit is {MAX_SOURCE_LEN}",
            source.len()
        )));
        return None;
    }

    // Lexed once, here, because the nesting bound reads the token stream before the
    // parse; the tokens are then handed over rather than lexed again.
    let (tokens, spans) = tokenize(source, diags);

    if let Some(at) = nesting_overflow(&tokens) {
        let span = spans.get(at).cloned().unwrap_or(0..source.len());
        diags.push(
            Diagnostic::error()
                .with_message(format!("a type nested deeper than {MAX_NEST_DEPTH}"))
                .with_label(Label::primary((), span)),
        );
        return None;
    }

    let lexed = Lexed {
        tokens: Some((tokens, spans)),
    };

    Some(Parser::new_with_context(source, diags, lexed).parse(diags))
}

/// The index of the first token at which nesting exceeds [`MAX_NEST_DEPTH`].
///
/// Read straight off the token stream: every unclosed bracket opens a nested `ty`, and
/// that is the whole of it — a schema has no application to count, which is what made
/// `focus`'s version of this fiddly. `{a: {…}, b: {…}}` is a record of two siblings and
/// counts as one level, because a closed brace pops.
fn nesting_overflow(tokens: &[Token]) -> Option<usize> {
    let mut depth = 0usize;

    for (at, token) in tokens.iter().enumerate() {
        match token {
            Token::LBrace | Token::LBrack | Token::LPar => {
                depth += 1;
                if depth > MAX_NEST_DEPTH {
                    return Some(at);
                }
            }
            // Saturating, because a stray closer is a parse error to report rather
            // than an underflow to panic on.
            Token::RBrace | Token::RBrack | Token::RPar => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(source: &str) -> Vec<String> {
        let mut diags = vec![];
        parse(source, &mut diags);
        diags.into_iter().map(|d| d.message).collect()
    }

    fn parses(source: &str) {
        let mut diags = vec![];
        let tree = parse(source, &mut diags);
        assert!(tree.is_some(), "no tree for `{source}`");
        assert!(
            diags.is_empty(),
            "`{source}` did not parse cleanly: {diags:?}"
        );
    }

    /// The shape a real schema has, top to bottom.
    #[test]
    fn a_schema_block_parses() {
        parses(
            "\
# the source layer
schema src {
  import lang.rust

  type Position = { line : int, col : int }

  predicate File : string
  predicate Module : { file : File, name : string }
  predicate Decl : { module : Module, name : string, line : int } -> string
}",
        );
    }

    /// Several blocks in one file, which operations §7 requires: namespaces are open
    /// across files, so a file cannot be one namespace by construction.
    #[test]
    fn a_file_may_hold_several_blocks() {
        parses("schema a { predicate P : string }\nschema b { predicate Q : string }");
    }

    /// **Everything deferred still parses.** This is permissive-early stated as a
    /// test: none of these is a syntax error, so each can be reported by name later.
    #[test]
    fn the_deferred_surface_parses() {
        parses("schema s { predicate P : [string] }");
        parses("schema s { predicate P : set string }");
        parses("schema s { predicate P : maybe string }");
        parses("schema s { type T = enum { red | green } }");
        parses("schema s { type T = { a : int = 0 | b : string = 1 } }");
        parses("schema a evolves b");
        parses("schema s { predicate P : string -> string stored }");
        parses("schema s { derive P stored }");
    }

    /// A record and a sum share their braces and are told apart by the separator, so
    /// both spellings have to reach the same rule and come out different.
    #[test]
    fn a_record_and_a_sum_both_parse() {
        parses("schema s { type R = { a : int, b : string } }");
        parses("schema s { type S = { a : int = 0 | b : string = 1 } }");
        parses("schema s { type One = { a : int } }");
        parses("schema s { type Empty = {} }");
    }

    /// Qualified and unqualified references, and a trailing comma.
    #[test]
    fn names_and_trailing_punctuation() {
        parses("schema s { predicate P : { a : src.Decl, b : Local } }");
    }

    /// A genuine syntax error is still a syntax error — permissive-early widens the
    /// grammar, it does not stop it complaining.
    #[test]
    fn nonsense_is_refused() {
        assert!(!errors("schema s { predicate }").is_empty());
        assert!(!errors("predicate P : string").is_empty(), "no block");
        assert!(!errors("schema s {").is_empty(), "unclosed");
    }

    /// Nesting past the bound is a diagnostic, never a stack overflow.
    ///
    /// The positive control matters as much as the refusal: a bound that refused
    /// *everything* would pass the first assertion and be useless.
    #[test]
    fn deep_nesting_is_refused_rather_than_overflowing() {
        let deep = format!(
            "schema s {{ type T = {}int{} }}",
            "[".repeat(MAX_NEST_DEPTH + 2),
            "]".repeat(MAX_NEST_DEPTH + 2)
        );

        let mut diags = vec![];
        assert!(parse(&deep, &mut diags).is_none());
        assert!(diags.iter().any(|d| d.message.contains("nested deeper")));

        let shallow = format!(
            "schema s {{ type T = {}int{} }}",
            "[".repeat(8),
            "]".repeat(8)
        );
        parses(&shallow);
    }

    /// A stray closing bracket is a parse error, not an arithmetic underflow in the
    /// depth counter — which is what `saturating_sub` is there for.
    #[test]
    fn a_stray_closer_does_not_underflow() {
        assert!(!errors("schema s { type T = ] }").is_empty());
    }
}
