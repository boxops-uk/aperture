use std::borrow::Cow;
use std::cell::RefCell;

use aperture::lens::{
    cst::CstNode,
    lexer::{self, Token},
    lower::Lowering,
    parser::Parser,
    schema::Schema,
    ty::{Ty, TyChecker, TyDisplay, infer_query},
};
use codespan_reporting::{
    files::SimpleFile,
    term::{
        self, Config,
        termcolor::{ColorChoice, StandardStream},
    },
};
use im::HashMap;
use rustyline::{
    Editor, Helper,
    completion::Completer,
    error::ReadlineError,
    highlight::{CmdKind, Highlighter},
    hint::Hinter,
    history::DefaultHistory,
    validate::Validator,
};
use string_interner::DefaultStringInterner as StringInterner;

#[derive(Default)]
struct LensHighlighter;

impl Highlighter for LensHighlighter {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let mut diagnostics = Vec::new();
        let (tokens, spans) = lexer::tokenize(line, &mut diagnostics);

        let mut out = String::with_capacity(line.len() + 16);
        let mut last = 0;

        for (token, span) in tokens.iter().zip(spans.iter()) {
            let start = span.start as usize;
            let end = span.end as usize;

            if start > last {
                out.push_str(&line[last..start]);
            }

            out.push_str("\x1b[");
            out.push_str(ansi_code(token));
            out.push('m');
            out.push_str(&line[start..end]);
            out.push_str("\x1b[0m");

            last = end;
        }

        if last < line.len() {
            out.push_str(&line[last..]);
        }

        Cow::Owned(out)
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        true
    }
}

fn ansi_code(token: &Token) -> &'static str {
    match token {
        Token::Where => "35",
        Token::Nat | Token::String => "36",
        Token::LPar
        | Token::RPar
        | Token::LBrace
        | Token::RBrace
        | Token::Semi
        | Token::Eq
        | Token::Comma
        | Token::Wildcard
        | Token::DotDot => "90",
        _ => "0",
    }
}

#[derive(Default)]
struct LensHelper {
    highlighter: LensHighlighter,
}

impl Highlighter for LensHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        self.highlighter.highlight(line, pos)
    }

    fn highlight_char(&self, line: &str, pos: usize, kind: CmdKind) -> bool {
        self.highlighter.highlight_char(line, pos, kind)
    }
}

impl Completer for LensHelper {
    type Candidate = String;
}

impl Hinter for LensHelper {
    type Hint = String;
}

impl Validator for LensHelper {}

impl Helper for LensHelper {}

fn main() -> rustyline::Result<()> {
    loop {
        let mut schema = Schema::new();
        let strings = RefCell::new(StringInterner::default());

        let test_ns = strings.borrow_mut().get_or_intern("test");
        let foo_sym = strings.borrow_mut().get_or_intern("Foo");

        let id = schema.insert_predicate_ty(
            vec![test_ns].into_boxed_slice(),
            foo_sym,
            Ty::Int,
            Ty::Never,
        );

        let baa_sym = strings.borrow_mut().get_or_intern("Baa");
        let x_sym = strings.borrow_mut().get_or_intern("x");
        let y_sym = strings.borrow_mut().get_or_intern("y");

        schema.insert_predicate_ty(
            vec![test_ns].into_boxed_slice(),
            baa_sym,
            Ty::Record {
                field_tys: HashMap::new().update(
                    x_sym,
                    Ty::Record {
                        field_tys: HashMap::new().update(y_sym, Ty::Fact { predicate_id: id }),
                    },
                ),
            },
            Ty::Never,
        );

        let mut rl: Editor<LensHelper, DefaultHistory> = Editor::new()?;
        rl.set_helper(Some(LensHelper::default()));

        let input = match rl.readline("> ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => return Ok(()),
            Err(err) => return Err(err),
        };

        let mut diagnostics = vec![];
        let parser = Parser::new(input.trim(), &mut diagnostics);
        let cst = parser.parse(&mut diagnostics);

        let writer = StandardStream::stderr(ColorChoice::Auto);
        let config = Config::default();
        let file = SimpleFile::new("<stdin>", &input);

        for diagnostic in diagnostics.iter() {
            term::emit_to_write_style(&mut writer.lock(), &config, &file, diagnostic).unwrap();
        }

        if !diagnostics.is_empty() {
            continue;
        }

        let cst_node = CstNode::new(&cst);
        let lowering = Lowering::new(&strings, &schema, ());
        let query = lowering.lower(&cst_node);

        let mut ty_checker: TyChecker = TyChecker::new();

        let inferred_ty = infer_query(&mut ty_checker, &schema, &query);

        let query_ty = ty_checker.zonk(&inferred_ty);

        println!(
            "{}",
            TyDisplay {
                string_interner: &strings.borrow(),
                schema: &schema,
                ty: query_ty,
            }
        );

        for diagnostic in ty_checker.diagnostics(&strings.borrow(), &schema) {
            term::emit_to_write_style(&mut writer.lock(), &config, &file, &diagnostic).unwrap();
        }
    }
}
