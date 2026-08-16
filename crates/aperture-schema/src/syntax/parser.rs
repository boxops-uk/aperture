//! Glue between the generated parser and this crate's lexer and diagnostics.
//!
//! `lelwel` generates the table-driven parser into `OUT_DIR`; what lives here is the
//! two callbacks it needs and the `Lexed` hand-off that keeps the source lexed exactly
//! once. Mirrors `aperture_engine::parser`, which does the same job for `focus`.

use super::lexer::{Token, tokenize};

use codespan_reporting::diagnostic::Label;

pub use super::diag::Diagnostic;

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

/// Tokens lexed before the parse starts, handed to the parser rather than lexed again.
///
/// Both passes would push their lexer diagnostics into the same sink, so lexing twice
/// does not merely waste work — it reports every invalid token twice.
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
