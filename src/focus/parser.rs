use super::lexer::{Token, tokenize};

use codespan_reporting::diagnostic::Label;
pub type Diagnostic = codespan_reporting::diagnostic::Diagnostic<()>;

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

/// Tokens lexed before the parse starts, handed to the parser so it does not lex
/// the source a second time.
///
/// `parse` has to tokenize up front to bound the nesting depth, and the generated
/// parser calls [`ParserCallbacks::create_tokens`] for itself. Lexing twice was
/// not only wasted work: both passes push their lexer diagnostics into the same
/// sink, so every invalid token was reported twice.
#[derive(Default)]
pub struct Lexed {
    pub tokens: Option<(Vec<Token>, Vec<Span>)>,
}

impl<'a> ParserCallbacks<'a> for Parser<'a> {
    type Diagnostic = Diagnostic;
    type Context = Lexed;

    fn create_tokens(
        context: &mut Self::Context,
        source: &'a str,
        diags: &mut Vec<Self::Diagnostic>,
    ) -> (Vec<Token>, Vec<Span>) {
        // Already lexed, with its diagnostics already in `diags` — taking them
        // here is what keeps the source lexed exactly once.
        context
            .tokens
            .take()
            .unwrap_or_else(|| tokenize(source, diags))
    }
    fn create_diagnostic(&self, span: Span, message: String) -> Self::Diagnostic {
        Self::Diagnostic::error()
            .with_message(message)
            .with_label(Label::primary((), span))
    }
}
