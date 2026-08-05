//! `aperture` — an interactive shell for the focus language.
//!
//! Reads a focus query, highlights it as you type from the same `logos` lexer the
//! compiler uses, and reports what the front end makes of it: lex and parse
//! errors, names lowering cannot resolve, and the type typecheck infers for the
//! head.
//!
//! It runs against a **real** [`FjallDb`] in a scratch directory, seeded at
//! startup from the schema below — so the names a query resolves against, and the
//! rows `:facts` prints, are the ones actually on disk. The schema is built
//! around **fact references**: the relations point at the entities by `FactId`
//! rather than repeating their keys, which is what `:facts` resolves back to
//! `demo.City "Cambridge"` when it prints a row.
//!
//! # What it cannot do yet
//!
//! **Run your query.** Turning a typed tree into a [`Plan`] is flatten, which is
//! Phase 4 and does not exist — `focus::syntax::FlatPlan` is unwired scaffolding.
//! So a query gets as far as a type and stops, and the shell says so rather than
//! pretending. `:facts` runs a plan this program builds by hand, which is the
//! honest way to show the store, executor and codec underneath all working.

use std::{borrow::Cow, collections::BTreeMap, path::Path, sync::Arc};

use aperture::focus::{
    error::{ApertureError, StoreCodecError},
    iter::{Address, Executor, Iteratee, Stream},
    lexer::{Token, tokenize},
    lower::lower,
    parse::parse,
    plan::{Access, FactId, FactStore, Generator, Plan, Project, SeekKey},
    schema::{LocalInterner, Predicate, PredicateId, PredicateTy, Schema, SchemaInterner},
    store::{FjallDb, FjallStore},
    syntax::Ty,
    tuple::{TupleEncoder, Value, decode_typed},
    ty,
};
use codespan_reporting::{
    files::SimpleFile,
    term::{
        self,
        termcolor::{ColorChoice, StandardStream},
    },
};
use lasso::Rodeo;
use rustyline::{
    Context, Editor, Helper,
    completion::Completer,
    error::ReadlineError,
    highlight::{CmdKind, Highlighter},
    hint::Hinter,
    history::DefaultHistory,
    validate::Validator,
};
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "focus> ";

// ---- the schema, and the facts that back it --------------------------------

/// A key or value field, before encoding.
enum Field<'a> {
    Int(i64),
    Str(&'a str),
    /// A reference to another fact, by the id its write returned.
    Ref(FactId),
}

/// A predicate id **is** its position in the schema — and a `Fact` field names
/// one, so the ids have to be written down before the vector that defines them.
/// `predicate_ids_are_positions` checks each against the name it is meant to be.
const PERSON: PredicateId = PredicateId(0);
const KNOWS: PredicateId = PredicateId(1);
const CITY: PredicateId = PredicateId(2);
const LIVES_IN: PredicateId = PredicateId(3);

/// The schema this shell resolves names against.
///
/// Record fields are listed **sorted by name**, as everywhere: a record's field
/// order is part of its encoding.
///
/// Two predicates are *entities* — `demo.Person` and `demo.City` — and the two
/// relations refer to them by [`PredicateTy::Fact`] rather than by repeating a
/// key. That is the shape a fact database is for: a reference is a `FactId`,
/// encoded under its own marker (`0x51`), so `demo.LivesIn` names the very row
/// `demo.City` wrote, and a reference to a city that was never written cannot be
/// spelled. It is also what gives a query something to walk: the `person` of a
/// `demo.LivesIn` fact *is* a person, so `.value` on it reads that person's name
/// with no join written out by hand.
fn demo_schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let mut sym = |name: &str| rodeo.get_or_intern(name);

    let predicates = vec![
        // A person is identified by id, and their name is the value side — so
        // `X.value` has something to read.
        Predicate {
            name: sym("demo.Person"),
            key: PredicateTy::Record(Arc::from([(sym("id"), PredicateTy::Int)])),
            value: Some(PredicateTy::Str),
        },
        Predicate {
            name: sym("demo.Knows"),
            key: PredicateTy::Record(Arc::from([
                (sym("from"), PredicateTy::Fact(PERSON)),
                (sym("to"), PredicateTy::Fact(PERSON)),
            ])),
            value: None,
        },
        // A bare scalar key, so not every predicate here is a record — and a
        // referable fact all the same: what a reference names is a *fact*, whatever
        // shape its key has.
        Predicate {
            name: sym("demo.City"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: sym("demo.LivesIn"),
            key: PredicateTy::Record(Arc::from([
                (sym("city"), PredicateTy::Fact(CITY)),
                (sym("person"), PredicateTy::Fact(PERSON)),
            ])),
            value: None,
        },
    ];

    Schema::new(rodeo.into_reader(), Arc::from(predicates))
}

fn put_field(enc: &mut TupleEncoder<'_>, field: &Field<'_>) {
    match field {
        Field::Int(value) => enc.put_i64(*value),
        Field::Str(value) => enc.put_str(value),
        Field::Ref(id) => enc.put_fact_id(*id),
    }
}

/// Encode a record key — the shape of every predicate here bar `demo.City`.
fn record_key(fields: &[Field<'_>]) -> Result<Vec<u8>, StoreCodecError> {
    let mut out = Vec::new();
    let mut enc = TupleEncoder::new(&mut out);

    enc.record(|enc| {
        for field in fields {
            put_field(enc, field);
        }
        Ok(())
    })?;

    Ok(out)
}

/// Encode a bare scalar, for a key or a value.
fn scalar(field: &Field<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    put_field(&mut TupleEncoder::new(&mut out), field);
    out
}

/// Write the facts the schema describes, returning how many.
///
/// Every key is distinct: a key is written once ([`FjallDb::put_fact`]).
///
/// The entities go first, because a reference is to a fact and the id that names
/// it is what [`FjallDb::put_fact`] hands back — so `demo.Knows` and
/// `demo.LivesIn` cannot be written until the people and cities they point at
/// exist. Referential integrity here is a consequence of the order, not a check.
fn seed(db: &FjallDb) -> Result<usize, ApertureError> {
    let mut written = 0;
    let mut put = |predicate, key: Vec<u8>, value: Vec<u8>| -> Result<FactId, ApertureError> {
        let id = db.put_fact(predicate, &key, &value)?;
        written += 1;
        Ok(id)
    };

    let mut people = BTreeMap::new();
    for (id, name) in [
        (1, "Ada Lovelace"),
        (2, "Grace Hopper"),
        (3, "Alan Turing"),
        (4, "Edsger Dijkstra"),
    ] {
        let fact = put(
            PERSON,
            record_key(&[Field::Int(id)])?,
            scalar(&Field::Str(name)),
        )?;
        people.insert(id, fact);
    }

    let mut cities = BTreeMap::new();
    for name in ["Amsterdam", "Baltimore", "Cambridge"] {
        let fact = put(CITY, scalar(&Field::Str(name)), vec![])?;
        cities.insert(name, fact);
    }

    for (from, to) in [(1, 2), (1, 3), (2, 3), (3, 4)] {
        put(
            KNOWS,
            record_key(&[Field::Ref(people[&from]), Field::Ref(people[&to])])?,
            vec![],
        )?;
    }

    for (place, id) in [
        ("Cambridge", 1),
        ("Baltimore", 2),
        ("Cambridge", 3),
        ("Amsterdam", 4),
    ] {
        put(
            LIVES_IN,
            record_key(&[Field::Ref(cities[place]), Field::Ref(people[&id])])?,
            vec![],
        )?;
    }

    Ok(written)
}

// ---- highlighting ----------------------------------------------------------

/// ANSI colours, chosen by what a token *means* rather than what it is: someone
/// scanning a query wants predicates, variables and literals to separate.
fn colour(token: Token) -> &'static str {
    match token {
        // A lexer error, marked as it is typed — the earliest diagnostic there is.
        Token::Error => "1;31",
        Token::Where | Token::Never => "1;35",
        Token::QId => "33",
        Token::UId => "1;36",
        Token::LId => "34",
        Token::Nat | Token::String | Token::Minus | Token::DotDot => "32",
        Token::Wildcard | Token::Pipe | Token::Bang | Token::Question => "1;90",
        _ => "90",
    }
}

struct FocusHelper;

impl FocusHelper {
    /// Whether `line` is a shell command rather than a query.
    fn is_command(line: &str) -> bool {
        line.trim_start().starts_with(':')
    }
}

impl Highlighter for FocusHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() || Self::is_command(line) {
            return Cow::Borrowed(line);
        }

        // Diagnostics discarded: they belong to submitting a line, not to typing
        // one. What is live here is the colour — an invalid token turns red under
        // the cursor.
        let (tokens, spans) = tokenize(line, &mut Vec::new());

        let mut out = String::with_capacity(line.len() * 2);
        let mut last = 0;

        for (token, span) in tokens.iter().zip(spans.iter()) {
            if span.start > last {
                out.push_str(&line[last..span.start]);
            }

            // Whitespace carries no colour: painting it would colour the gaps
            // between tokens as well as the tokens.
            if matches!(token, Token::Whitespace) {
                out.push_str(&line[span.clone()]);
            } else {
                out.push_str("\x1b[");
                out.push_str(colour(*token));
                out.push('m');
                out.push_str(&line[span.clone()]);
                out.push_str("\x1b[0m");
            }

            last = span.end;
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

impl Hinter for FocusHelper {
    type Hint = String;

    /// A live hint for the one fault that is unambiguous mid-typing.
    ///
    /// Lexical only. Half-written input is a *parse* error almost continuously —
    /// `X where` is incomplete rather than wrong — so hinting those would be
    /// noise. An invalid token stays wrong however much more is typed.
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if pos < line.len() || Self::is_command(line) {
            return None;
        }

        let (tokens, _) = tokenize(line, &mut Vec::new());
        tokens
            .iter()
            .any(|token| matches!(token, Token::Error))
            .then(|| "   invalid token".to_owned())
    }
}

impl Completer for FocusHelper {
    type Candidate = String;
}

impl Validator for FocusHelper {}
impl Helper for FocusHelper {}

// ---- rendering types -------------------------------------------------------

fn render_ty(ty: &Ty, schema: &Schema, interner: &LocalInterner) -> String {
    match ty {
        Ty::Int => "int".to_owned(),
        Ty::String => "str".to_owned(),
        Ty::Error => "?error".to_owned(),
        Ty::Var(_) => "?".to_owned(),
        Ty::Fact(predicate) => schema
            .get(*predicate)
            .and_then(|p| p.name())
            .map_or_else(|| "fact".to_owned(), str::to_owned),
        Ty::Record(fields) => {
            let rendered = fields
                .iter()
                .map(|(name, field)| {
                    format!(
                        "{} : {}",
                        interner.try_resolve(*name).unwrap_or("?"),
                        render_ty(field, schema, interner)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{rendered}}}")
        }
    }
}

/// How far [`render_value`] follows references before it stops and names the row
/// instead.
///
/// A referenced fact's key can hold references of its own, and nothing stops a
/// schema being cyclic — so rendering is bounded rather than trusting the data to
/// bottom out. This schema is two deep.
const MAX_REF_DEPTH: usize = 4;

/// Render a projected row as focus-flavoured text.
///
/// A reference prints as the fact it names — `demo.City "Cambridge"` — which is
/// both what a reader wants and how that same fact is written in a query. It
/// costs one point read apiece, which is why this runs over rows the executor has
/// already produced: values stay out of the scan loop
/// ([I6](../docs/invariants.md#i6)).
fn render_value(
    store: &FjallStore,
    schema: &Schema,
    interner: &LocalInterner,
    value: &Value,
    depth: usize,
) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Int(n) => n.to_string(),
        // Debug formatting is the quoting and escaping focus source uses.
        Value::Str(s) => format!("{s:?}"),
        Value::FactRef(id) => render_ref(store, schema, interner, *id, depth),
        Value::Record(fields) => {
            let rendered = fields
                .iter()
                .map(|(name, field)| {
                    format!(
                        "{name} = {}",
                        render_value(store, schema, interner, field, depth)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{rendered}}}")
        }
    }
}

/// Resolve one reference through the `entities` column family.
///
/// The id's own tag says which predicate it belongs to, so the schema gives the
/// key's type without anything else being carried alongside. Where the fact
/// cannot be read — depth exhausted, or a dangling id — the reference still names
/// a row, so print the identity rather than nothing: `demo.Person#3`.
fn render_ref(
    store: &FjallStore,
    schema: &Schema,
    interner: &LocalInterner,
    id: FactId,
    depth: usize,
) -> String {
    let Some(predicate) = schema.get(id.predicate()) else {
        return format!("?#{}", id.sequence());
    };
    let name = predicate.name().unwrap_or("?");

    if depth == 0 {
        return format!("{name}#{}", id.sequence());
    }

    match store.point(id) {
        Ok(Some(entity)) => match decode_typed(interner, &entity.key, predicate.key().ty) {
            Ok(key) => format!(
                "{name} {}",
                render_value(store, schema, interner, &key, depth - 1)
            ),
            Err(error) => format!("{name}#{} ({error})", id.sequence()),
        },
        Ok(None) => format!("{name}#{} (dangling)", id.sequence()),
        Err(error) => format!("{name}#{} ({error})", id.sequence()),
    }
}

fn render_predicate_ty(ty: &PredicateTy, schema: &Schema, interner: &SchemaInterner) -> String {
    match ty {
        PredicateTy::Int => "int".to_owned(),
        PredicateTy::Str => "str".to_owned(),
        PredicateTy::Fact(predicate) => schema
            .get(*predicate)
            .and_then(|p| p.name())
            .map_or_else(|| "fact".to_owned(), str::to_owned),
        PredicateTy::Record(fields) => {
            let rendered = fields
                .iter()
                .map(|(name, field)| {
                    format!(
                        "{} : {}",
                        interner.resolve(*name).unwrap_or("?"),
                        render_predicate_ty(field, schema, interner)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{rendered}}}")
        }
    }
}

// ---- the front end ---------------------------------------------------------

/// Parse, lower and typecheck `source`, reporting everything it finds.
///
/// The phases accumulate rather than fail fast, so a query wrong in several ways
/// is reported in one go — except that lowering needs a tree, so a refused parse
/// stops there.
fn check(source: &str, schema: &Schema) {
    let writer = StandardStream::stdout(ColorChoice::Auto);
    let config = term::Config::default();
    let file = SimpleFile::new("<input>", source);

    let emit = |diagnostics: &[_]| {
        for diagnostic in diagnostics {
            let _ = term::emit_to_write_style(&mut writer.lock(), &config, &file, diagnostic);
        }
    };

    let parsed = parse(source);
    emit(parsed.diagnostics());

    let Some(root) = parsed.root() else { return };
    if parsed.has_errors() {
        return;
    }

    let mut interner = LocalInterner::new(schema.interner().clone());
    let (ast, lowering) = lower(&root, schema, &mut interner);
    emit(&lowering);

    let (typed, checking) = ty::check(&ast, schema, &interner);
    emit(&checking);

    if !lowering.is_empty() || !checking.is_empty() {
        return;
    }

    match typed.ty(*ast.query().head()) {
        Some(head) => {
            println!("  : {}", render_ty(head, schema, &interner));
            println!("  (typechecked — running it needs flatten, which is Phase 4)");
        }
        None => println!("  (the head was not annotated)"),
    }
}

// ---- commands --------------------------------------------------------------

/// The queries `:help` offers, which `help_examples_typecheck` checks: a shell
/// that suggests a query it then rejects is worse than one that suggests none.
///
/// Both are about references. A field typed `Fact` is written as the fact itself —
/// `demo.Person {id = 1}` — so a key nests the pattern that names it; and reading
/// through one is `.value`, which needs no join because a reference already *is*
/// the row.
const EXAMPLES: [&str; 2] = [
    "X.value where demo.Knows {from = demo.Person {id = 1}, to = X}",
    "P.value where demo.LivesIn {city = demo.City \"Cambridge\", person = P}",
];

fn print_help() {
    println!("  <query>          typecheck a focus query, e.g.");
    for example in EXAMPLES {
        println!("                     {example}");
    }
    println!("  :schema          the predicates this shell knows");
    println!("  :facts <name>    rows stored for a predicate, read through the executor");
    println!("  :help            this");
    println!("  :quit            leave");
}

fn print_schema(schema: &Schema) {
    for index in 0..schema.len() {
        let Some(predicate) = schema.get(PredicateId(index as u32)) else {
            continue;
        };

        let key = render_predicate_ty(predicate.key().ty, schema, schema.interner());
        let value = predicate.value().map_or_else(String::new, |value| {
            format!(
                " -> {}",
                render_predicate_ty(value.ty, schema, schema.interner())
            )
        });

        println!("  {} : {key}{value}", predicate.name().unwrap_or("?"));
    }
}

/// A whole-predicate scan, projecting the key and — where there is one — the
/// value.
///
/// Hand-built, because flatten does not exist yet. `field_idx` indexes the key's
/// *top-level* fields, and a key is one field however many parts it has, so field
/// 0 is the whole key.
fn scan_plan(
    id: PredicateId,
    key_ty: PredicateTy,
    value_ty: Option<PredicateTy>,
    interner: &mut LocalInterner,
) -> Plan {
    let key = Project::RegisterField {
        address: Address::new(0),
        field_idx: 0,
        ty: key_ty,
    };

    let head = match value_ty {
        None => key,
        Some(value_ty) => {
            // Sorted by name, as record fields are everywhere.
            let key_name = interner.get_or_intern("key");
            let value_name = interner.get_or_intern("value");

            Project::Record(Box::new([
                (key_name, key),
                (
                    value_name,
                    Project::Value {
                        address: Address::new(0),
                        ty: value_ty,
                    },
                ),
            ]))
        }
    };

    Plan {
        nvars: 1,
        body: Box::new([Generator {
            access: Access {
                predicate_id: id,
                seek_key: SeekKey::Prefix(Box::new([])),
            },
            binds: Box::new([Address::new(0)]),
            residuals: Box::new([]),
        }]),
        head,
    }
}

fn print_facts(db: &FjallDb, schema: &Schema, name: &str) {
    let Some((id, predicate)) = schema.find_position(name) else {
        println!("  no predicate called `{name}` — try :schema");
        return;
    };

    let key_ty = predicate.key().ty.clone();
    let value_ty = predicate.value().map(|value| value.ty.clone());

    let mut interner = LocalInterner::new(schema.interner().clone());
    let plan = scan_plan(id, key_ty, value_ty, &mut interner);

    let rows = Executor::new(db.reader(), plan).enumerate(
        Vec::<Value>::new(),
        |mut acc, mut row| {
            acc.push(row.to_value(&interner)?);
            Ok(Stream::Continue(acc))
        },
        &CancellationToken::new(),
    );

    match rows {
        Err(error) => println!("  {error}"),
        Ok(Iteratee::Done(rows) | Iteratee::Suspended(rows, _)) => {
            // A reader of its own, opened after the scan has finished: resolving a
            // reference is a point read, and this is not the scan loop.
            let store = db.reader();

            for row in &rows {
                println!(
                    "  {}",
                    render_value(&store, schema, &interner, row, MAX_REF_DEPTH)
                );
            }
            println!("  {} row(s)", rows.len());
        }
    }
}

// ---- the loop --------------------------------------------------------------

/// A scratch directory of this run's own, so two shells never share a database
/// and re-running never writes a key twice.
fn scratch_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("aperture-shell-{}", std::process::id()))
}

fn main() -> Result<(), ApertureError> {
    let dir = scratch_dir();
    let result = shell(&dir);

    // On every path, including an early `?`: best effort, since failing to tidy
    // up is not a reason to fail the run.
    let _ = std::fs::remove_dir_all(&dir);

    result
}

fn shell(dir: &Path) -> Result<(), ApertureError> {
    let schema = demo_schema();

    let db = FjallDb::open(dir)?;
    db.create_predicates((0..schema.len()).map(|index| PredicateId(index as u32)))?;
    let written = seed(&db)?;

    println!("aperture — a focus shell");
    println!("{written} facts in {}\n", dir.display());
    print_schema(&schema);
    println!();
    print_help();
    println!();

    let mut editor: Editor<FocusHelper, DefaultHistory> =
        Editor::new().map_err(|error| readline_failure(&error))?;
    editor.set_helper(Some(FocusHelper));

    loop {
        match editor.readline(PROMPT) {
            Ok(line) => {
                let line = line.trim().to_owned();
                if line.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(&line);

                match line.split_once(char::is_whitespace) {
                    Some((":facts", name)) => print_facts(&db, &schema, name.trim()),
                    _ if line == ":facts" => println!("  :facts needs a predicate — try :schema"),
                    _ if line == ":schema" => print_schema(&schema),
                    _ if line == ":help" => print_help(),
                    _ if line == ":quit" || line == ":q" => return Ok(()),
                    _ if line.starts_with(':') => {
                        println!("  no such command: {line} — try :help");
                    }
                    _ => check(&line, &schema),
                }
            }

            // Ctrl-C abandons the line; Ctrl-D leaves.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return Ok(()),
            Err(error) => return Err(readline_failure(&error)),
        }
    }
}

/// A terminal that cannot be read is not a store fault, but the shell has one
/// error type to leave by.
fn readline_failure(error: &ReadlineError) -> ApertureError {
    eprintln!("aperture: {error}");
    ApertureError::Cancelled
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Drop the ANSI sequences [`FocusHelper::highlight`] inserts, leaving what
    /// the terminal would actually show.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();

        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            // `\x1b[…m`, which is the only form emitted here.
            for escaped in chars.by_ref() {
                if escaped == 'm' {
                    break;
                }
            }
        }

        out
    }

    /// The ids the `Fact` fields refer to have to be the predicates they are named
    /// for. A predicate id is a *position*, so inserting one into the middle of
    /// `demo_schema` silently repoints every reference — this is what says so.
    #[test]
    fn predicate_ids_are_positions() {
        let schema = demo_schema();

        for (name, id) in [
            ("demo.Person", PERSON),
            ("demo.Knows", KNOWS),
            ("demo.City", CITY),
            ("demo.LivesIn", LIVES_IN),
        ] {
            let found = schema.find_position(name).map(|(id, _)| id);
            assert_eq!(found, Some(id), "{name} is not at {}", id.0);
        }
    }

    /// Every example the shell prints parses, lowers and typechecks against the
    /// schema it seeds — `:help` is advice, and wrong advice is worse than none.
    #[test]
    fn help_examples_typecheck() {
        let schema = demo_schema();

        for source in EXAMPLES {
            let parsed = parse(source);
            assert!(
                !parsed.has_errors(),
                "{source}: {:?}",
                parsed.diagnostics().len()
            );
            let root = parsed.root().expect("a tree, since there are no errors");

            let mut interner = LocalInterner::new(schema.interner().clone());
            let (ast, lowering) = lower(&root, &schema, &mut interner);
            assert!(lowering.is_empty(), "{source}: {lowering:?}");

            let (typed, checking) = ty::check(&ast, &schema, &interner);
            assert!(checking.is_empty(), "{source}: {checking:?}");
            assert!(
                typed.ty(*ast.query().head()).is_some(),
                "{source}: the head was not annotated"
            );
        }
    }

    /// The schema the shell offers has to be the schema it seeded, or a query
    /// resolves against names with no facts behind them.
    #[test]
    fn every_declared_predicate_is_seeded() {
        let dir = scratch_dir().with_extension("test-seed");
        let _ = std::fs::remove_dir_all(&dir);

        let schema = demo_schema();
        let db = FjallDb::open(&dir).expect("open");
        db.create_predicates((0..schema.len()).map(|i| PredicateId(i as u32)))
            .expect("create");
        let written = seed(&db).expect("seed");

        assert!(written > 0);
        for index in 0..schema.len() {
            let id = PredicateId(index as u32);
            let predicate = schema.get(id).expect("declared");
            let name = predicate.name().expect("named");

            let mut interner = LocalInterner::new(schema.interner().clone());
            let plan = scan_plan(
                id,
                predicate.key().ty.clone(),
                predicate.value().map(|v| v.ty.clone()),
                &mut interner,
            );

            let rows = Executor::new(db.reader(), plan)
                .enumerate(
                    0usize,
                    |n, _row| Ok(Stream::Continue(n + 1)),
                    &CancellationToken::new(),
                )
                .expect("scan");
            let (Iteratee::Done(rows) | Iteratee::Suspended(rows, _)) = rows;

            assert!(rows > 0, "{name} is declared but has no facts");
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every reference the seed writes names a fact that is there.
    ///
    /// Nothing on the write path checks this — a `FactId` is eight bytes like any
    /// other field, and referential integrity comes from the *order* `seed` writes
    /// in. So reordering it would leave the shell printing `(dangling)` and a query
    /// reading through a reference failing at the point lookup; this fails first.
    #[test]
    fn every_reference_resolves() {
        let dir = scratch_dir().with_extension("test-refs");
        let _ = std::fs::remove_dir_all(&dir);

        let schema = demo_schema();
        let db = FjallDb::open(&dir).expect("open");
        db.create_predicates((0..schema.len()).map(|i| PredicateId(i as u32)))
            .expect("create");
        seed(&db).expect("seed");

        /// Collect the references anywhere in a projected row.
        fn refs(value: &Value, out: &mut Vec<FactId>) {
            match value {
                Value::FactRef(id) => out.push(*id),
                Value::Record(fields) => {
                    for (_, field) in fields.iter() {
                        refs(field, out);
                    }
                }
                Value::Null | Value::Int(_) | Value::Str(_) => {}
            }
        }

        let store = db.reader();
        let mut seen = 0;

        for index in 0..schema.len() {
            let id = PredicateId(index as u32);
            let predicate = schema.get(id).expect("declared");

            let mut interner = LocalInterner::new(schema.interner().clone());
            let plan = scan_plan(
                id,
                predicate.key().ty.clone(),
                predicate.value().map(|v| v.ty.clone()),
                &mut interner,
            );

            let rows = Executor::new(db.reader(), plan)
                .enumerate(
                    Vec::<Value>::new(),
                    |mut acc, mut row| {
                        acc.push(row.to_value(&interner)?);
                        Ok(Stream::Continue(acc))
                    },
                    &CancellationToken::new(),
                )
                .expect("scan");
            let (Iteratee::Done(rows) | Iteratee::Suspended(rows, _)) = rows;

            for row in &rows {
                let mut ids = Vec::new();
                refs(row, &mut ids);

                for fact in ids {
                    let target = schema.get(fact.predicate()).expect("a declared predicate");
                    let entity = store
                        .point(fact)
                        .expect("point")
                        .unwrap_or_else(|| panic!("dangling reference {}", fact.raw()));

                    decode_typed(&interner, &entity.key, target.key().ty)
                        .expect("the referenced key decodes at the referenced predicate's type");
                    seen += 1;
                }
            }
        }

        assert!(
            seen > 0,
            "the schema has `Fact` fields but nothing wrote one"
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    proptest! {
        /// **Highlighting only adds colour.** Strip the escapes and the line comes
        /// back byte for byte.
        ///
        /// This runs on every keystroke over whatever is half-typed, so it has to
        /// be total: losing a byte would show the wrong text under the cursor, and
        /// panicking would take the shell down mid-edit. Hence arbitrary input —
        /// that is what a line editor gets.
        ///
        /// Bar `ESC` itself, which is a limit of the *oracle*, not of the
        /// highlighter: `strip_ansi` cannot tell an escape this code emitted from
        /// one that was in the line, so it would eat the input's. The highlighter
        /// pushes only slices of `line` and fixed colour codes, so it is
        /// byte-preserving there too — there is just no text-level way to say so.
        #[test]
        fn highlighting_only_adds_colour(line in "[^\u{1b}]{0,120}") {
            let highlighted = FocusHelper.highlight(&line, line.len());
            prop_assert_eq!(strip_ansi(&highlighted), line);
        }

        /// A command is passed through untouched — it is not focus source.
        #[test]
        fn commands_are_not_highlighted(rest in "[^\u{1b}]{0,40}") {
            let line = format!(":{rest}");
            let highlighted = FocusHelper.highlight(&line, line.len());
            prop_assert_eq!(highlighted.as_ref(), line.as_str());
        }
    }
}
