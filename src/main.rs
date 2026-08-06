//! `aperture` — an interactive shell for the focus language.
//!
//! Reads a focus query, highlights it as you type from the same `logos` lexer the
//! compiler uses, **compiles it to a [`Plan`] and runs it** — reporting whatever the
//! front end found on the way: lex and parse errors, names lowering cannot resolve,
//! the type typecheck infers for the head, and then the rows.
//!
//! It runs against a **real** [`FjallDb`] in a scratch directory, seeded at startup
//! from the shared [`fixture`] — the same schema and the same rows the
//! [`corpus`](aperture::focus::corpus) is written against, so anything the corpus
//! classifies `Supported` is something you can type here and get the recorded answer
//! for. `:plan` shows what a query compiled to without running it, which is where its
//! cost is visible: which field narrowed the scan, which one only filters, and which
//! register a level reads.
//!
//! Both are worth having at a prompt rather than only in tests, because the fixture is
//! built around **fact references** — the relations point at facts by `FactId` rather
//! than repeating their keys — and a reference is the one thing whose plan does not
//! read like its source: following one splices an *id*, so `of = r0#` is the answer to
//! "did it follow the reference, or compare the wrong bytes?".
//!
//! # What it does not do
//!
//! **Anything a wire client could not.** The product shell is remote-first
//! ([operations §5](../docs/aperture-cli-design.md)), so this holds no state a wire
//! session cannot reproduce: no cursors kept between lines, no cached compilations.
//! Phase 9 re-points the interactive front at the wire client, and everything here
//! survives that except which store it opens.

use std::{borrow::Cow, path::Path};

use aperture::focus::{
    compile::Compilation,
    error::ApertureError,
    fixture,
    iter::{Address, Executor, Iteratee, Stream},
    lexer::{Token, tokenize},
    plan::{Access, FactId, FactStore, FieldPath, Generator, Plan, Project, SeekKey},
    print,
    schema::{LocalInterner, PredicateId, PredicateTy, Schema, SchemaInterner, Symbol},
    store::{FjallDb, FjallStore},
    syntax::Ty,
    tuple::{Value, decode_key},
};
use codespan_reporting::term::{
    self,
    termcolor::{ColorChoice, StandardStream},
};
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

// ---- the database ----------------------------------------------------------

/// Write the shared fixture's facts, returning how many.
///
/// The order is [`fixture::facts`]'s, and the id the allocator hands out is checked
/// against the one the fixture expects: a reference is a whole [`FactId`], so a fact
/// numbered differently here than in the fixture would be pointed at by nothing
/// ([I11](../docs/invariants.md#i11)). Referential integrity is a consequence of the
/// order, not a check.
fn seed(db: &FjallDb) -> Result<usize, ApertureError> {
    let facts = fixture::facts();

    for fixture::Fact {
        predicate,
        key,
        value,
        sequence,
    } in &facts
    {
        let id = db.put_fact(*predicate, key, value)?;
        let expected = FactId::new(*predicate, *sequence)?;

        assert_eq!(
            id, expected,
            "the store's allocator diverged from the fixture's numbering",
        );
    }

    Ok(facts.len())
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
        Ok(Some(entity)) => match decode_key(interner, &entity.key, predicate.key().ty) {
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

/// **Compile `source` and run it**, printing the rows it answers with.
///
/// The sequencing, the interner and the diagnostic rendering all belong to
/// [`Compilation`] — the shell's job is to decide what to print. A query with a fault
/// has already said so through its diagnostics, so there is nothing to add; a clean
/// one gets its type, then its rows.
///
/// `plan()` returning `None` with an empty sink would be a bug in the front end
/// rather than in the query, and this says so instead of printing nothing.
fn run(source: &str, db: &FjallDb, schema: &Schema) {
    let mut compilation = Compilation::new(source, schema);
    let plan = compilation.plan();

    let writer = StandardStream::stdout(ColorChoice::Auto);
    let _ = compilation.render(&mut writer.lock(), &term::Config::default());

    if compilation.diagnostics().has_errors() {
        return;
    }

    if let Some(head) = compilation.head_ty() {
        println!("  : {}", render_ty(head, schema, compilation.interner()));
    }

    let Some(plan) = plan else {
        println!("  (no plan, and no diagnostic saying why — that is a compiler bug)");
        return;
    };

    print_rows(db, schema, compilation.interner(), plan);
}

/// Run a plan to completion and print each row, resolving references to the facts
/// they name.
fn print_rows(db: &FjallDb, schema: &Schema, interner: &LocalInterner, plan: Plan) {
    let rows = Executor::new(db.reader(), plan).enumerate(
        Vec::<Value>::new(),
        |mut acc, mut row| {
            acc.push(row.to_value(interner)?);
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
                    render_value(&store, schema, interner, row, MAX_REF_DEPTH)
                );
            }
            println!("  {} row(s)", rows.len());
        }
    }
}

/// Show the plan a query compiles to, without running it — where its cost lives.
fn print_plan(source: &str, schema: &Schema) {
    let mut compilation = Compilation::new(source, schema);
    let plan = compilation.plan();

    let writer = StandardStream::stdout(ColorChoice::Auto);
    let _ = compilation.render(&mut writer.lock(), &term::Config::default());

    match plan {
        Some(plan) => println!("{}", print::plan(&plan, schema, compilation.interner())),
        None if compilation.diagnostics().has_errors() => {}
        None => println!("  (no plan, and no diagnostic saying why — that is a compiler bug)"),
    }
}

// ---- commands --------------------------------------------------------------

fn print_help() {
    println!("  <query>          compile and run a focus query, e.g.");
    for example in fixture::EXAMPLES {
        println!("                     {example}");
    }
    println!("  :plan <query>    the plan it compiles to, without running it");
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
/// Hand-built, because the shell cannot compile a query to a plan until Phase 5
/// wires the driver's `plan()` in.
///
/// A stored key is its top-level fields back to back, so projecting the *whole* key
/// is a record over one `RegisterField` per field: there is no single
/// [`FieldPath`] that names all of them. That is the same asymmetry a query meets
/// as `nyi/whole-key` — a scalar key *is* one field and needs no record.
fn scan_plan(
    id: PredicateId,
    key_ty: PredicateTy,
    value_ty: Option<PredicateTy>,
    interner: &mut LocalInterner,
) -> Plan {
    let key = key_projection(&key_ty);

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

/// Project a whole stored key: one field per declared key field, in declaration
/// order — which is also encoding order, since field lists are sorted by name.
fn key_projection(key_ty: &PredicateTy) -> Project {
    match key_ty {
        PredicateTy::Record(fields) => Project::Record(
            fields
                .iter()
                .enumerate()
                .map(|(idx, (name, ty))| {
                    (
                        Symbol::Schema(*name),
                        Project::RegisterField {
                            address: Address::new(0),
                            path: FieldPath::field(idx),
                            ty: ty.clone(),
                        },
                    )
                })
                .collect(),
        ),
        scalar => Project::RegisterField {
            address: Address::new(0),
            path: FieldPath::field(0),
            ty: scalar.clone(),
        },
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
    let schema = fixture::schema();

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
                    Some((":plan", query)) => print_plan(query.trim(), &schema),
                    _ if line == ":facts" => println!("  :facts needs a predicate — try :schema"),
                    _ if line == ":plan" => println!("  :plan needs a query — try :help"),
                    _ if line == ":schema" => print_schema(&schema),
                    _ if line == ":help" => print_help(),
                    _ if line == ":quit" || line == ":q" => return Ok(()),
                    _ if line.starts_with(':') => {
                        println!("  no such command: {line} — try :help");
                    }
                    _ => run(&line, &db, &schema),
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

    /// Run a query the way the prompt does: compile it, execute it against a real
    /// store, and hand back the rows.
    fn ask(db: &FjallDb, schema: &Schema, source: &str) -> Vec<Value> {
        let mut compilation = Compilation::new(source, schema);
        let plan = compilation
            .plan()
            .unwrap_or_else(|| panic!("{source}:\n{}", compilation.render_to_string()));

        let rows = Executor::new(db.reader(), plan)
            .enumerate(
                Vec::<Value>::new(),
                |mut acc, mut row| {
                    acc.push(row.to_value(compilation.interner())?);
                    Ok(Stream::Continue(acc))
                },
                &CancellationToken::new(),
            )
            .expect("run");

        let (Iteratee::Done(rows) | Iteratee::Suspended(rows, _)) = rows;
        rows
    }

    /// A database of this test's own, seeded exactly as the shell seeds its own.
    fn seeded(suffix: &str) -> (FjallDb, std::path::PathBuf) {
        let dir = scratch_dir().with_extension(suffix);
        let _ = std::fs::remove_dir_all(&dir);

        let schema = fixture::schema();
        let db = FjallDb::open(&dir).expect("open");
        db.create_predicates((0..schema.len()).map(|i| PredicateId(i as u32)))
            .expect("create");
        seed(&db).expect("seed");

        (db, dir)
    }

    /// **Phase 5's acceptance criterion, at the prompt's own path: typing a query
    /// returns rows, end to end, through the real compiler and the real executor
    /// against a real store.**
    ///
    /// Everything the shell could do before this stopped at a type. What makes it a
    /// test of the *shell* rather than of the compiler is the store: these rows came
    /// off disk, out of keyspaces the seed wrote, through a plan nobody built by hand.
    #[test]
    fn typing_a_query_returns_rows() {
        let (db, dir) = seeded("test-run");
        let schema = fixture::schema();

        assert_eq!(
            ask(&db, &schema, "X.value where X = test.Foo _"),
            ["one", "two", "three"].map(|s| Value::Str(s.to_owned())),
        );

        // A join **through a reference**, which is what a fact database is for and
        // what this phase made expressible: two `test.Link` facts point at
        // `test.Foo {id = 2}`, so its name comes back once per referrer.
        assert_eq!(
            ask(
                &db,
                &schema,
                "P.name where P = test.Foo {id = 2}; test.Link {at = _, of = P}"
            ),
            ["bob", "bob"].map(|s| Value::Str(s.to_owned())),
        );

        // The same join written the idiomatic way, with the generator nested — which
        // is hoisted into a level of its own.
        assert_eq!(
            ask(
                &db,
                &schema,
                "X where X = test.Ref {of = test.Foo {id = 1}}"
            ),
            [Value::FactRef(
                FactId::new(schema.find_position("test.Ref").expect("test.Ref").0, 1)
                    .expect("a fixture fact id")
            )],
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every example `:help` offers **runs**, against the database the shell seeds.
    ///
    /// It used to be enough that they typechecked, and that was the gap this phase
    /// found: both previous examples were nested generators over fact-typed fields,
    /// so they typechecked and then had no plan. The corpus checks the rows each one
    /// answers with; this checks that the shell's own store answers at all.
    #[test]
    fn every_help_example_runs() {
        let (db, dir) = seeded("test-examples");
        let schema = fixture::schema();

        for source in fixture::EXAMPLES {
            let rows = ask(&db, &schema, source);
            assert!(!rows.is_empty(), "{source} returned nothing");
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line wrong in two ways prints both faults, in the order they were
    /// written.
    ///
    /// The shell is the reason the driver sorts for rendering rather than
    /// emitting in phase order: this is what someone typing at the prompt
    /// actually sees, and the unknown predicate is found by lowering — an earlier
    /// phase than the one that rejects the head — while being written later.
    #[test]
    fn a_line_wrong_twice_prints_both_faults_in_source_order() {
        let schema = fixture::schema();
        let mut compilation = Compilation::new("_ where X = nosuch.Pred _", &schema);
        compilation.check();

        let rendered = compilation.render_to_string();
        let head = rendered.find("wildcard").expect("the head fault");
        let name = rendered.find("nosuch").expect("the unresolved name");

        assert!(
            head < name,
            "the shell printed these out of order:\n{rendered}"
        );
    }

    /// The schema the shell offers has to be the schema it seeded, or a query
    /// resolves against names with no facts behind them.
    #[test]
    fn every_declared_predicate_is_seeded() {
        let dir = scratch_dir().with_extension("test-seed");
        let _ = std::fs::remove_dir_all(&dir);

        let schema = fixture::schema();
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

        let schema = fixture::schema();
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

                    decode_key(&interner, &entity.key, target.key().ty)
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
