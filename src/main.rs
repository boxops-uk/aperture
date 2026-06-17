use std::borrow::Cow;

use aperture::lens::{
    diag::Diag,
    lower::LensLowering,
    parse::{LensParser, Token, tokenize},
    query::NodeIdGen,
    schema::Schema,
    ty::{LensTyChecker, Ty, TyDisplay, infer_query},
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
        let (tokens, spans) = tokenize(line);

        let mut out = String::with_capacity(line.len() + 16);
        let mut last = 0;

        for (token, span) in tokens.iter().zip(spans.iter()) {
            let start = span.start;
            let end = span.end;

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
        let mut strings = StringInterner::default();
        let mut schema = Schema::new();

        let test_ns = strings.get_or_intern("test");
        let foo_sym = strings.get_or_intern("Foo");

        let id = schema.insert_predicate_ty(
            vec![test_ns].into_boxed_slice(),
            foo_sym,
            Ty::Int,
            Ty::Never,
        );

        let baa_sym = strings.get_or_intern("Baa");
        let x_sym = strings.get_or_intern("x");
        let y_sym = strings.get_or_intern("y");

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

        let writer = StandardStream::stderr(ColorChoice::Auto);
        let config = Config::default();
        let file = SimpleFile::new("<stdin>", &input);

        let mut diags: Vec<Diag> = vec![];

        let mut parser = LensParser::parse(input.trim());
        parser.drain_into(&mut diags);

        if !diags.is_empty() {
            render_diags(&diags, &strings, &schema, &writer, &config, &file);
            continue;
        }

        let mut node_id_gen = NodeIdGen::default();
        let mut lowering = LensLowering::new(&mut node_id_gen, &mut strings, &schema, ());
        let query = lowering.lower(&parser.cst());
        lowering.drain_into(&mut diags);

        if !diags.is_empty() {
            render_diags(&diags, &strings, &schema, &writer, &config, &file);
            continue;
        }

        let mut ty_checker = LensTyChecker::new();
        let ty = infer_query(&mut ty_checker, &schema, &query);
        ty_checker.drain_into(&mut diags);

        println!(
            "{}",
            TyDisplay {
                ty: ty_checker.zonk(&ty),
                schema: &schema,
                string_interner: &strings
            }
        );

        render_diags(&diags, &strings, &schema, &writer, &config, &file);
    }
}

fn render_diags(
    diags: &[Diag],
    strings: &StringInterner,
    schema: &Schema,
    writer: &StandardStream,
    config: &Config,
    file: &SimpleFile<&str, &String>,
) {
    for diag in diags {
        term::emit_to_write_style(
            &mut writer.lock(),
            config,
            file,
            &diag.render(strings, schema),
        )
        .unwrap();
    }
}
