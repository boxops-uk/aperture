//! Tokens of the schema DSL.
//!
//! Its own lexer rather than a share of `focus`'s, and the reason is the dependency
//! direction rather than taste: `focus`'s lexer lives in `aperture-engine`, which sits
//! *above* this crate. Two lexers for two languages is the honest arrangement — and
//! they are genuinely different languages, one of which has no expressions at all.
//!
//! # Names, and why there are four kinds
//!
//! A schema distinguishes what a name *is* by how it is written, exactly as `focus`
//! does, so the parser never needs a symbol table to know what it is looking at:
//!
//! | token | shape | what it names |
//! |---|---|---|
//! | [`Token::UId`] | `Decl` | a predicate or type in this namespace |
//! | [`Token::QId`] | `src.Decl` | one in another namespace |
//! | [`Token::LId`] | `file` | a field, a builtin type, or a one-segment namespace |
//! | [`Token::NsId`] | `lang.rust` | a namespace of several segments |
//!
//! `logos` takes the longest match, which is what keeps these apart without ordering
//! rules: `src.Decl` is a `QId` (8 characters) rather than an `NsId` of `src` (3),
//! and `lang.rust` is an `NsId` rather than an `LId` of `lang`.

use codespan_reporting::diagnostic::Label;
use logos::Logos;

use super::{diag::Diagnostic, parser::Span};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LexerError {
    #[default]
    Invalid,
}

impl LexerError {
    pub fn into_diagnostic(self, span: Span) -> Diagnostic {
        match self {
            Self::Invalid => Diagnostic::error()
                .with_message("invalid token")
                .with_label(Label::primary((), span)),
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Logos, Debug, PartialEq, Eq, Copy, Clone)]
#[logos(error = LexerError)]
pub enum Token {
    EOF,

    #[regex(r"([ \t\n\f\r]|\\\n)+")]
    Whitespace,

    /// `# like this`, to the end of the line — Angle's comment, and every schema
    /// anyone has written in this family uses it.
    // `allow_greedy` because that is exactly the intent: a comment runs to the end
    // of its line, and `logos` warns about `[^\n]*` on the assumption it was reached
    // for by accident.
    #[regex(r"#[^\n]*", allow_greedy = true)]
    Comment,

    #[token("schema")]
    Schema,
    #[token("import")]
    Import,
    #[token("predicate")]
    Predicate,
    #[token("type")]
    Type,
    #[token("derive")]
    Derive,
    #[token("stored")]
    Stored,
    #[token("evolves")]
    Evolves,
    #[token("enum")]
    Enum,
    #[token("maybe")]
    Maybe,
    #[token("set")]
    Set,

    /// `src.Decl` — a qualified name, ending in an uppercase segment.
    #[regex(r"[a-z][a-zA-Z0-9_]*(\.[a-z][a-zA-Z0-9_]*)*\.[A-Z][a-zA-Z0-9_]*")]
    QId,
    /// `lang.rust` — a namespace of two or more segments, all lowercase.
    #[regex(r"[a-z][a-zA-Z0-9_]*(\.[a-z][a-zA-Z0-9_]*)+")]
    NsId,
    /// `Decl` — a predicate or type name.
    #[regex(r"[A-Z][a-zA-Z0-9_]*")]
    UId,
    /// `file` — a field name, a builtin type, or a one-segment namespace.
    #[regex(r"[a-z][a-zA-Z0-9_]*")]
    LId,

    /// A discriminant. Digits only: a discriminant is a tag rather than a number to
    /// compute with, so there is no sign and no separator to validate.
    #[regex(r"[0-9]+")]
    Nat,

    #[token("->")]
    Arrow,
    #[token(":")]
    Colon,
    #[token(",")]
    Comma,
    #[token("=")]
    Eq,
    #[token("|")]
    Pipe,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBrack,
    #[token("]")]
    RBrack,
    #[token("(")]
    LPar,
    #[token(")")]
    RPar,

    Error,
}

/// Lex `source`, reporting invalid tokens into `diags`.
///
/// An invalid token becomes [`Token::Error`] and lexing carries on, so one stray
/// character does not cost the reader every diagnostic after it.
pub fn tokenize(source: &str, diags: &mut Vec<Diagnostic>) -> (Vec<Token>, Vec<Span>) {
    let lexer = Token::lexer(source);
    let mut tokens = vec![];
    let mut spans = vec![];

    for (token, span) in lexer.spanned() {
        match token {
            Ok(token) => tokens.push(token),
            Err(err) => {
                diags.push(err.into_diagnostic(span.clone()));
                tokens.push(Token::Error);
            }
        }
        spans.push(span);
    }

    (tokens, spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(source: &str) -> Vec<Token> {
        let mut diags = vec![];
        let (tokens, _) = tokenize(source, &mut diags);
        assert!(diags.is_empty(), "unexpected lex errors in `{source}`");
        tokens
            .into_iter()
            .filter(|t| !matches!(t, Token::Whitespace | Token::Comment))
            .collect()
    }

    /// **The four name shapes, told apart by longest match alone.**
    ///
    /// This is the pinned claim: no ordering rule and no symbol table, just `logos`
    /// preferring the longer match. Break it and `src.Decl` starts lexing as a
    /// namespace followed by something, which parses as nothing recognisable.
    #[test]
    fn a_name_is_classified_by_its_shape() {
        assert_eq!(lex("Decl"), [Token::UId]);
        assert_eq!(lex("file"), [Token::LId]);
        assert_eq!(lex("lang.rust"), [Token::NsId]);
        assert_eq!(lex("src.Decl"), [Token::QId]);
        assert_eq!(lex("codemarkup.types.Visibility"), [Token::QId]);
    }

    /// A keyword is a keyword, and a name that merely starts with one is not.
    #[test]
    fn a_keyword_does_not_swallow_a_name_beginning_with_it() {
        assert_eq!(lex("set"), [Token::Set]);
        assert_eq!(lex("settings"), [Token::LId]);
        assert_eq!(lex("type"), [Token::Type]);
        assert_eq!(lex("typename"), [Token::LId]);
    }

    /// Comments and whitespace are skipped, and a comment runs to the newline only.
    #[test]
    fn a_comment_ends_at_the_line() {
        let mut diags = vec![];
        let (tokens, _) = tokenize("# a note\npredicate", &mut diags);
        assert!(diags.is_empty());
        assert!(tokens.contains(&Token::Comment));
        assert!(tokens.contains(&Token::Predicate));
    }

    /// A whole declaration, to show the pieces fit together.
    #[test]
    fn a_predicate_declaration_lexes() {
        assert_eq!(
            lex("predicate Decl : { module : Module, name : string } -> string"),
            [
                Token::Predicate,
                Token::UId,
                Token::Colon,
                Token::LBrace,
                Token::LId,
                Token::Colon,
                Token::UId,
                Token::Comma,
                Token::LId,
                Token::Colon,
                Token::LId,
                Token::RBrace,
                Token::Arrow,
                Token::LId,
            ]
        );
    }

    /// An invalid character is one `Error` token and lexing continues past it.
    #[test]
    fn an_invalid_character_does_not_end_the_lex() {
        let mut diags = vec![];
        let (tokens, _) = tokenize("predicate ^ Decl", &mut diags);

        assert_eq!(diags.len(), 1, "one complaint, not one per token after it");
        assert!(tokens.contains(&Token::Error));
        assert!(tokens.contains(&Token::UId), "it kept going");
    }
}
