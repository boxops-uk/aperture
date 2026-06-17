use super::parser::{Diagnostic, Span};
use codespan_reporting::diagnostic::Label;
use logos::Logos;

#[derive(Debug, Clone, PartialEq, Default)]
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
#[derive(Logos, Debug, PartialEq, Copy, Clone)]
#[logos(error = LexerError)]
pub enum Token {
    EOF,
    #[regex(r"([ \t\n\f]|\\\n)+")]
    Whitespace,
    #[token("where")]
    Where,
    #[token("never")]
    Never,
    #[regex(r"[A-Z][a-zA-Z0-9_]*")]
    UId,
    #[regex(r"[a-z][a-zA-Z0-9_]*")]
    LId,
    #[token("_")]
    Wildcard,
    #[regex(r"0|[1-9][0-9_]*")]
    Nat,
    #[regex(r#""(?:\\(?:["\\/bfnrt]|u[a-fA-F0-9]{4})|[^"\\[:cntrl:]]+)*""#)]
    String,
    #[token("..")]
    DotDot,
    #[token(".")]
    Dot,
    #[token("=")]
    Eq,
    #[token(";")]
    Semi,
    #[token(",")]
    Comma,
    #[token("-")]
    Minus,
    #[token("(")]
    LPar,
    #[token(")")]
    RPar,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    Error,
}

// TODO: extend tokenization (e.g. check for mismatched parentheses)
pub fn tokenize(source: &str, diags: &mut Vec<Diagnostic>) -> (Vec<Token>, Vec<Span>) {
    let lexer = Token::lexer(source);
    let mut tokens = vec![];
    let mut spans = vec![];

    for (token, span) in lexer.spanned() {
        match token {
            Ok(token) => {
                tokens.push(token);
            }
            Err(err) => {
                diags.push(err.into_diagnostic(span.clone()));
                tokens.push(Token::Error);
            }
        }
        spans.push(span);
    }
    (tokens, spans)
}
