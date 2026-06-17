pub use super::lexer::Token;
pub use super::parser::Rule;

use super::lexer::tokenize as lex_raw;
use super::parser::{Cst, Diagnostic, Parser};
use crate::lens::{cst::CstNode, diag::Diag, location::Span};

/// Tokenizes `source`, returning tokens and their spans. Lex errors are discarded.
pub fn tokenize(source: &str) -> (Vec<Token>, Vec<Span>) {
    let (tokens, ranges) = lex_raw(source, &mut vec![]);
    let spans = ranges.into_iter().map(Span::from).collect();
    (tokens, spans)
}

pub struct LensParser<'src> {
    cst: Cst<'src>,
    errors: Vec<Diag>,
}

impl<'src> LensParser<'src> {
    pub fn parse(source: &'src str) -> Self {
        let mut raw_diags: Vec<Diagnostic> = vec![];
        let cst = Parser::new(source, &mut raw_diags).parse(&mut raw_diags);
        let errors = raw_diags.into_iter().map(Diag::Rendered).collect();
        Self { cst, errors }
    }

    pub fn cst(&self) -> CstNode<'_> {
        CstNode::new(&self.cst)
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn drain_into(&mut self, diags: &mut Vec<Diag>) {
        diags.extend(self.errors.drain(..));
    }
}
