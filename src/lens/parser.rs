use super::lexer::{Token, tokenize};

use codespan_reporting::diagnostic::Label;
pub type Diagnostic = codespan_reporting::diagnostic::Diagnostic<()>;

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

impl<'a> ParserCallbacks<'a> for Parser<'a> {
    type Diagnostic = Diagnostic;
    type Context = ();

    fn create_tokens(_context: &mut Self::Context, source: &'a str, diags: &mut Vec<Self::Diagnostic>) -> (Vec<Token>, Vec<Span>) {
        tokenize(source, diags)
    }
    fn create_diagnostic(&self, span: Span, message: String) -> Self::Diagnostic {
        Self::Diagnostic::error()
            .with_message(message)
            .with_label(Label::primary((), span))
    }
}
