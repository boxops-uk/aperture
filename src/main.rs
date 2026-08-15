//! `aperture` — an interactive shell for the focus language.
//!
//! Reads a focus query, highlights it as you type from the same `logos` lexer the
//! compiler uses, **compiles it to a [`Plan`] and runs it** — reporting whatever the
//! front end found on the way: lex and parse errors, names lowering cannot resolve,
//! the type typecheck infers for the head, and then the rows.
//!
//! A command's **argument** is highlighted too, when the command takes focus source:
//! `:plan X where …` reads exactly as the same query does at the bare prompt, because
//! it is the same text going to the same compiler. Colour arriving as you type it is
//! also the shell saying it recognised the command — a typo stays grey. That is keyed
//! on [`COMMANDS`], which is the one table `:help`, the highlighter and the hinter all
//! read.
//!
//! It runs against a **real** [`FjallDb`] in a scratch directory, seeded at startup
//! with the index of a real (if small) codebase — the Python under `example/src`,
//! parsed by `example/index.py` into the files, modules, declarations, references and
//! imports of [`INDEX`] — so the names a query resolves against and the rows it returns
//! are the ones actually on disk, and every row names a file, a line and a column
//! someone can go and look at. `:plan` shows what a query compiled to without running
//! it, which is where its cost is visible: which field narrowed the scan, which one only
//! filters, and which register a level reads.
//!
//! The schema is a **code index** because that is the canonical shape for a fact
//! database: one fact per thing, and everything about a thing pointing at it by
//! [`FactId`](aperture::focus::plan::FactId) rather than repeating it. A reference is
//! also the one thing whose plan does not read like its source — following one splices
//! an *id*, so `to = r0#` is the answer to "did it follow the reference, or compare the
//! wrong bytes?".
//!
//! The facts themselves are written as **well-typed Rust values** through
//! [`focus::fact`](aperture::focus::fact), which is what a hand-written deriver would
//! do: a plain struct, named fields, and the schema deciding the encoding order. The
//! `Fact` impls below deliberately list their fields in the order that reads well rather
//! than the order the schema declares, because getting that wrong by hand writes a fact
//! nobody can find.
//!
//! # What it does not do
//!
//! **Anything a wire client could not.** The product shell is remote-first
//! ([operations §5](../docs/aperture-cli-design.md)), so this holds no state a wire
//! session cannot reproduce: no cursors kept between lines, no cached compilations.
//! Phase 9 re-points the interactive front at the wire client, and everything here
//! survives that except which store it opens.

use std::{
    borrow::Cow,
    io::IsTerminal,
    path::Path,
    sync::{Arc, OnceLock},
};

use aperture::focus::{
    compile::Compilation,
    error::{ApertureError, StoreError},
    fact::{Fact, ToValue, record},
    iter::{Executor, Iteratee, Stream},
    lexer::{Token, tokenize},
    plan::{Access, Address, FactId, FactStore, FieldPath, Level, Plan, Project, SeekKey, Step},
    print,
    schema::{LocalInterner, Predicate, PredicateId, PredicateTy, Schema, SchemaInterner, Symbol},
    store::{FjallDb, FjallStore},
    syntax::Ty,
    tuple::{Value, decode_key},
};
use lasso::Rodeo;

use codespan_reporting::term::{
    self,
    termcolor::{ColorChoice, StandardStream},
};
use serde::Deserialize;

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

/// A predicate id **is** its position in the schema, and a `Fact` field names one — so
/// the ids of the predicates that are *pointed at* have to be written down before the
/// vector that defines them. Nothing checks it — this shell is a scaffold Phase 9
/// re-points at the wire client — so a wrong id here writes facts under the wrong
/// predicate and the queries below quietly return nothing.
const FILE: PredicateId = PredicateId(0);
const MODULE: PredicateId = PredicateId(1);
const DECL: PredicateId = PredicateId(2);

/// The schema this shell resolves names against: **a code index**, which is the
/// canonical shape for a fact database — one fact per thing, and everything about a
/// thing pointing at it rather than repeating it.
///
/// Record fields are listed **sorted by name**, as everywhere: a record's field order
/// is part of its encoding. The `Fact` impls below deliberately do *not* list them in
/// that order, because a hand-written deriver has no reason to know it — see
/// [`focus::fact`](aperture::focus::fact).
///
/// What each predicate is here to show:
///
/// | predicate | shows |
/// |---|---|
/// | `src.File` | a **scalar** key — a path is one string, and needs no record |
/// | `src.Module` | a **reference**, so a module names its file rather than repeating the path |
/// | `src.Decl` | a **value side**, so `D.value` has something to read, plus a second reference |
/// | `src.SearchByName` | **key order is the index**: the same names keyed so a prefix narrows |
/// | `src.Ref` | a **nested record** key field, and two references to two predicates, reached through an open pattern |
/// | `src.Import` | two references to one predicate, which is what a graph edge is |
fn demo_schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let mut sym = |name: &str| rodeo.get_or_intern(name);

    let predicates = vec![
        Predicate {
            name: sym("src.File"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: sym("src.Module"),
            key: PredicateTy::Record(Arc::from([
                (sym("file"), PredicateTy::Fact(FILE)),
                (sym("name"), PredicateTy::Str),
            ])),
            value: None,
        },
        // The value side is the declaration's *kind* — `def`, `class`, `method`,
        // `const` — because it is the one thing a query would want without matching on
        // it, and a value cannot be matched ([I6](../docs/invariants.md#i6)).
        Predicate {
            name: sym("src.Decl"),
            key: PredicateTy::Record(Arc::from([
                (sym("line"), PredicateTy::Int),
                (sym("module"), PredicateTy::Fact(MODULE)),
                (sym("name"), PredicateTy::Str),
            ])),
            value: Some(PredicateTy::Str),
        },
        // **The search index over declaration names**, and the one predicate here that
        // exists for a reason about *keys* rather than about the code: a declaration's
        // key begins with its module, so `src.Decl {name = "encode"..}` reaches the name
        // only after the scan has opened, and the prefix can filter rows but not narrow
        // to them. Keyed with `name` leading — which is also the encoding order, since
        // field lists are sorted — the same prefix is a range, and `:plan` shows the
        // difference as `seek[name = "encode".., to = _]` against a `scan` whose
        // `where name starts with "encode"` is all it has.
        //
        // It is the same names twice over, which is what a *derived* predicate is: data
        // a query could compute, stored keyed the way the query wants to read it.
        // Written by hand here because nothing can declare one yet
        // ([Phase 8b](../PLAN.md)) — `example/index.py` emits it exactly as a deriver
        // would.
        Predicate {
            name: sym("src.SearchByName"),
            key: PredicateTy::Record(Arc::from([
                (sym("name"), PredicateTy::Str),
                (sym("to"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        // A location is the **file and the position together**, and the file is not
        // derivable from the rest of the row: `to` reaches the file the *declaration*
        // is in, which for most references is a different one — that being what a
        // reference is for. So the file is a key field of its own, and the row names
        // somewhere someone can go and look.
        Predicate {
            name: sym("src.Ref"),
            key: PredicateTy::Record(Arc::from([
                (
                    sym("at"),
                    PredicateTy::Record(Arc::from([
                        (sym("col"), PredicateTy::Int),
                        (sym("line"), PredicateTy::Int),
                    ])),
                ),
                (sym("file"), PredicateTy::Fact(FILE)),
                (sym("to"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        Predicate {
            name: sym("src.Import"),
            key: PredicateTy::Record(Arc::from([
                (sym("from"), PredicateTy::Fact(MODULE)),
                (sym("to"), PredicateTy::Fact(MODULE)),
            ])),
            value: None,
        },
    ];

    Schema::new(rodeo.into_reader(), Arc::from(predicates))
}

// ---- the facts, as well-typed values ---------------------------------------
//
// Each of these is what a hand-written deriver emits: a plain struct, named fields,
// and `db.put` doing the rest. None of them lists its fields in the schema's sorted
// order, and none of them has to — that is what `focus::fact` checks and reorders,
// and getting it wrong by hand writes a fact nobody can find.
//
// Each is *also* the JSON row `example/index.py` wrote for it, because the only thing
// that differs between the two is what a **reference** is: an indexer names a fact by
// position, a store by the id its write returned. That is the `Ref` parameter — a
// `Decl<Idx>` is what came out of the indexer and a `Decl` is what goes in — so one
// struct is both shapes and the two cannot drift apart.

/// A reference as an *indexer* can express one: a **position** in the array of rows
/// for the predicate it points at.
///
/// It cannot be a [`FactId`], because an id is what a write returns
/// ([I11](../docs/invariants.md#i11)) — it does not exist until the fact does. [`seed`]
/// resolves each position against the ids it has already written, which is the whole
/// reason the arrays are in write order.
#[derive(Clone, Copy, Deserialize)]
struct Idx(usize);

/// A file needs no `Ref` parameter and no resolving: it is the one predicate here whose
/// key points at nothing, so the indexer's row *is* the fact.
#[derive(Clone, Deserialize)]
struct File(String);

impl Fact for File {
    const PREDICATE: &'static str = "src.File";

    /// A scalar key is one value, not a record of one.
    fn key(&self) -> Value {
        self.0.to_value()
    }
}

#[derive(Deserialize)]
struct Module<Ref = FactId> {
    name: String,
    file: Ref,
}

impl Fact for Module {
    const PREDICATE: &'static str = "src.Module";

    fn key(&self) -> Value {
        record([
            ("name", self.name.to_value()),
            ("file", self.file.to_value()),
        ])
    }
}

#[derive(Deserialize)]
struct Decl<Ref = FactId> {
    module: Ref,
    name: String,
    line: i64,
    kind: String,
}

impl Fact for Decl {
    const PREDICATE: &'static str = "src.Decl";

    fn key(&self) -> Value {
        record([
            ("module", self.module.to_value()),
            ("name", self.name.to_value()),
            ("line", self.line.to_value()),
        ])
    }

    fn value(&self) -> Option<Value> {
        Some(self.kind.to_value())
    }
}

/// A declaration's name, keyed by the name — see the schema above.
#[derive(Deserialize)]
struct SearchByName<Ref = FactId> {
    name: String,
    to: Ref,
}

impl Fact for SearchByName {
    const PREDICATE: &'static str = "src.SearchByName";

    fn key(&self) -> Value {
        record([("name", self.name.to_value()), ("to", self.to.to_value())])
    }
}

/// A position in a file — a **nested record**, so it implements [`ToValue`] rather
/// than [`Fact`]: it is part of a key, not a fact of its own.
#[derive(Clone, Copy, Deserialize)]
struct Pos {
    line: i64,
    col: i64,
}

impl ToValue for Pos {
    fn to_value(&self) -> Value {
        record([("line", self.line.to_value()), ("col", self.col.to_value())])
    }
}

/// A resolved reference: where it is — the file and the position in it — and what it
/// names.
///
/// The two references are to *different* predicates, which the shared `Ref` parameter
/// does not distinguish and does not need to: a position indexes the array it was
/// written against, and [`seed`] resolves each against its own.
#[derive(Deserialize)]
struct Reference<Ref = FactId> {
    file: Ref,
    at: Pos,
    to: Ref,
}

impl Fact for Reference {
    const PREDICATE: &'static str = "src.Ref";

    fn key(&self) -> Value {
        record([
            ("file", self.file.to_value()),
            ("at", self.at.to_value()),
            ("to", self.to.to_value()),
        ])
    }
}

#[derive(Deserialize)]
struct Import<Ref = FactId> {
    from: Ref,
    to: Ref,
}

impl Fact for Import {
    const PREDICATE: &'static str = "src.Import";

    fn key(&self) -> Value {
        record([("from", self.from.to_value()), ("to", self.to.to_value())])
    }
}

// ---- ingesting the indexer's output ----------------------------------------

/// The indexer's output: `example/index.json`, one array per predicate, in **write
/// order** — a row may only point at an array before it, by position.
///
/// `deny_unknown_fields` is what keeps the two sides of the format from drifting
/// apart quietly: a section renamed in the JSON is then a fault at startup rather
/// than a predicate that silently gets no facts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    files: Vec<File>,
    modules: Vec<Module<Idx>>,
    decls: Vec<Decl<Idx>>,
    names: Vec<SearchByName<Idx>>,
    refs: Vec<Reference<Idx>>,
    imports: Vec<Import<Idx>>,
}

/// The indexer's output, **compiled in** — see [`example/README.md`].
///
/// `include_str!` rather than a read at startup, for two reasons: the shell has no
/// working directory it can rely on, and this way `cargo build` reruns when the file
/// changes. Aperture has no ingestion path yet ([Phase 7](../PLAN.md)), so a
/// checked-in artefact and a loader written by hand is what stands in for one — and
/// what Phase 7 replaces, on both sides.
///
/// [`example/README.md`]: ../example/README.md
const INDEX: &str = include_str!("../example/index.json");

/// What to say when the compiled-in index is not the one this shell expects.
const MALFORMED: &str = "example/index.json is not the index this shell knows how to write — \
     rerun `python3 example/index.py`";

/// Write the example corpus's index, returning how many facts.
///
/// **The id a write returns is what a reference to that fact is**, which is why this
/// reads as it does: the files, then the modules in them, then the declarations in
/// those, then everything that points at a declaration. Referential integrity is a
/// consequence of the order rather than a check — nothing can point at a fact that
/// has not been written, because there is no id for it yet — and resolving the
/// indexer's positions against the ids so far is the same argument from the other end.
fn seed(db: &FjallDb, schema: &Schema) -> Result<usize, ApertureError> {
    let index: Index =
        serde_json::from_str(INDEX).unwrap_or_else(|error| panic!("{MALFORMED} ({error})"));

    let mut loader = Loader {
        db,
        schema,
        written: 0,
    };

    let files = loader.put_all(&index.files, File::clone)?;

    let modules = loader.put_all(&index.modules, |module| Module {
        name: module.name.clone(),
        file: id_at(&files, module.file),
    })?;

    let decls = loader.put_all(&index.decls, |decl| Decl {
        module: id_at(&modules, decl.module),
        name: decl.name.clone(),
        line: decl.line,
        kind: decl.kind.clone(),
    })?;

    loader.put_all(&index.names, |name| SearchByName {
        name: name.name.clone(),
        to: id_at(&decls, name.to),
    })?;

    loader.put_all(&index.refs, |reference| Reference {
        file: id_at(&files, reference.file),
        at: reference.at,
        to: id_at(&decls, reference.to),
    })?;

    loader.put_all(&index.imports, |import| Import {
        from: id_at(&modules, import.from),
        to: id_at(&modules, import.to),
    })?;

    Ok(loader.written)
}

/// The id of the fact the indexer named by position.
///
/// The one place this shell panics on data, and deliberately: [`INDEX`] is a build
/// artefact compiled in, not input, so a position past the end of an array is a bug in
/// `example/index.py` — and this says so at startup on every run, rather than leaving a
/// query to answer nothing and no one to know why.
fn id_at(written: &[FactId], at: Idx) -> FactId {
    *written
        .get(at.0)
        .unwrap_or_else(|| panic!("{MALFORMED} (row {} of {} written)", at.0, written.len()))
}

/// The load in progress: what a write needs, plus a count for `:schema`.
///
/// A struct rather than a closure because [`put`](Loader::put) is generic over the
/// fact, and a closure cannot be.
struct Loader<'a> {
    db: &'a FjallDb,
    schema: &'a Schema,
    written: usize,
}

impl Loader<'_> {
    fn put<F: Fact>(&mut self, fact: &F) -> Result<FactId, StoreError> {
        self.written += 1;
        self.db.put(self.schema, fact)
    }

    /// Write one fact per row, returning the ids **in the same order** — which is
    /// exactly what the positions in the arrays written after this one index into.
    fn put_all<Row, F: Fact>(
        &mut self,
        rows: &[Row],
        resolve: impl Fn(&Row) -> F,
    ) -> Result<Vec<FactId>, StoreError> {
        rows.iter().map(|row| self.put(&resolve(row))).collect()
    }
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
    /// Where this line's **focus source** begins, if any.
    ///
    /// A query is source from the first byte. A command word is not — but the
    /// *argument* of a command that takes one is, so `:plan X where …` and
    /// `:facts src.Decl` colour everything past the word. `None` means there is no
    /// source in the line at all: a bare command, a command that takes no
    /// argument, or one the shell does not know.
    ///
    /// Keyed on [`COMMANDS`] rather than on "everything after the first word",
    /// which is the point rather than an implementation detail: colour appearing
    /// as you type the argument is also the shell saying it **recognised the
    /// command**. A typo stays grey.
    fn source_offset(line: &str) -> Option<usize> {
        let trimmed = line.trim_start();

        if !trimmed.starts_with(':') {
            return Some(0);
        }

        // No whitespace yet ⇒ the command word is still being typed, and there is
        // no argument to colour.
        let word_end = trimmed.find(char::is_whitespace)?;
        let indent = line.len() - trimmed.len();

        COMMANDS
            .iter()
            .any(|command| command.name == &trimmed[..word_end] && command.argument.is_some())
            .then_some(indent + word_end)
    }

    /// Paint `source` onto `out`, one colour per token.
    ///
    /// Pushes only slices of `source` and fixed colour codes, which is what keeps
    /// highlighting byte-preserving — it runs on every keystroke over half-typed
    /// input, so losing a byte would show the wrong text under the cursor.
    fn paint(source: &str, out: &mut String) {
        // Diagnostics discarded: they belong to submitting a line, not to typing
        // one. What is live here is the colour — an invalid token turns red under
        // the cursor.
        let (tokens, spans) = tokenize(source, &mut Vec::new());
        let mut last = 0;

        for (token, span) in tokens.iter().zip(spans.iter()) {
            if span.start > last {
                out.push_str(&source[last..span.start]);
            }

            // Whitespace carries no colour: painting it would colour the gaps
            // between tokens as well as the tokens.
            if matches!(token, Token::Whitespace) {
                out.push_str(&source[span.clone()]);
            } else {
                out.push_str("\x1b[");
                out.push_str(colour(*token));
                out.push('m');
                out.push_str(&source[span.clone()]);
                out.push_str("\x1b[0m");
            }

            last = span.end;
        }

        if last < source.len() {
            out.push_str(&source[last..]);
        }
    }
}

impl Highlighter for FocusHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let Some(at) = Self::source_offset(line) else {
            return Cow::Borrowed(line);
        };

        let (command, source) = line.split_at(at);
        if source.trim().is_empty() {
            return Cow::Borrowed(line);
        }

        let mut out = String::with_capacity(line.len() * 2);
        out.push_str(command);
        Self::paint(source, &mut out);
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
        if pos < line.len() {
            return None;
        }

        // The same span the highlighter paints, for the same reason: an invalid
        // token in `:plan …`'s argument is as wrong as one at the bare prompt.
        let source = &line[Self::source_offset(line)?..];

        let (tokens, _) = tokenize(source, &mut Vec::new());
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
                        "{}: {}",
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
/// bottom out. This schema is three references deep: a name in the search index
/// points at a declaration, which points at its module, which points at its file.
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

/// Whether to emit colour: stdout is a terminal, and `NO_COLOR` is unset.
///
/// Mirrors what `ColorChoice::Auto` decides for the diagnostics, so a rendered
/// schema and a rendered error agree about whether a person or a pipe is reading.
/// Resolved once — it cannot change under a running shell.
fn colours_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

/// What a fragment of a rendered schema *is*, for colouring.
///
/// **The schema has no lexer** — the DSL is Phase 8 — so `:schema` is coloured by
/// hand, and this is that hand-written tokenisation. What it deliberately is not is a
/// second palette: every role resolves through [`colour`] with the token a schema
/// grammar would emit, so the two renderings can drift in *what* they colour but
/// never in the colours themselves, and the mapping is already written down for
/// whoever writes that grammar.
///
/// [`Role::Keyword`] is the one outright guess, and worth knowing about: under the
/// *query* lexer `int` comes out an `LId`, indistinguishable from a field name. A
/// schema grammar would almost certainly make the primitive type names reserved
/// words, and separating types from field names is the whole reason to colour this at
/// all — so they are painted as keywords on that assumption.
#[derive(Clone, Copy)]
enum Role {
    /// A predicate name, or a reference to one — `src.Decl`.
    Predicate,
    /// A record field name — `line`.
    Field,
    /// A primitive type name — `int`, `str`.
    Keyword,
    /// Structure: `:`, `,`, `{`, `}`, `->`.
    Punctuation,
}

impl Role {
    /// The token a schema lexer would produce for this role.
    fn token(self) -> Token {
        match self {
            Role::Predicate => Token::QId,
            Role::Field => Token::LId,
            Role::Keyword => Token::Where,
            Role::Punctuation => Token::Comma,
        }
    }

    fn paint(self, text: &str) -> String {
        if !colours_enabled() {
            return text.to_owned();
        }
        format!("\x1b[{}m{text}\x1b[0m", colour(self.token()))
    }
}

fn render_predicate_ty(ty: &PredicateTy, schema: &Schema, interner: &SchemaInterner) -> String {
    match ty {
        PredicateTy::Int => Role::Keyword.paint("int"),
        PredicateTy::Str => Role::Keyword.paint("str"),
        // A reference is painted as the predicate it names, because that is what it
        // is: the same yellow `src.Decl` gets at the top of its own line.
        PredicateTy::Fact(predicate) => Role::Predicate.paint(
            schema
                .get(*predicate)
                .and_then(|p| p.name())
                .unwrap_or("fact"),
        ),
        PredicateTy::Record(fields) => {
            let rendered = fields
                .iter()
                .map(|(name, field)| {
                    format!(
                        "{}{} {}",
                        Role::Field.paint(interner.resolve(*name).unwrap_or("?")),
                        Role::Punctuation.paint(":"),
                        render_predicate_ty(field, schema, interner)
                    )
                })
                .collect::<Vec<_>>()
                .join(&Role::Punctuation.paint(", "));

            format!(
                "{}{rendered}{}",
                Role::Punctuation.paint("{"),
                Role::Punctuation.paint("}")
            )
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

/// Show the type of a query's head, without planning or running it.
///
/// Stops at [`check`](Compilation::check) rather than at a plan, which is both cheaper
/// and *more* answerable: a type is known for every query that lowers and type-checks,
/// including the ones flatten defers. `X where D = src.Decl {…}` has a type whether or
/// not there is an engine for it yet.
fn print_type(source: &str, schema: &Schema) {
    let mut compilation = Compilation::new(source, schema);
    compilation.check();

    let writer = StandardStream::stdout(ColorChoice::Auto);
    let _ = compilation.render(&mut writer.lock(), &term::Config::default());

    if compilation.diagnostics().has_errors() {
        return;
    }

    match compilation.head_ty() {
        Some(ty) => println!("  : {}", render_ty(ty, schema, compilation.interner())),
        None => println!("  (no type, and no diagnostic saying why — that is a compiler bug)"),
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

/// Queries `:help` offers.
///
/// A shell that advertises a query it cannot answer is worse than one that advertises
/// none, and that is exactly what this was doing before a query could be run at all.
/// **Nothing checks these strings**, and it is worth being plain about that: they are
/// written against the code index, and `focus::corpus` — which does run every entry
/// against a real store — is written against the shared fixture. What the corpus
/// pins is that each *construct* used here returns rows; that these particular
/// queries do is checked by typing them, which is what a scaffold is for.
///
/// Between them they reach everything a reference-shaped schema needs — a scalar key,
/// a value side, a nested record field, a captured reference, and a chain of
/// generators written nested, which is how one actually writes a traversal.
///
/// The fifth is find-usages, and it answers with the **file and the line together**,
/// because either alone names nowhere anyone can go and look. Both come out of the one
/// row — a reference's file is its own, not the file of the declaration it names, which
/// for a reference worth having is a different one.
///
/// The first two are the same question twice, and the pair is the point: **prefix
/// search**, asked of the predicate keyed for it and of the one that is not.
/// `src.SearchByName` leads with the name, so the prefix is a
/// `seek[name = "encode".., to = _]` — a range of the index. `src.Decl` leads with
/// the module, so by the time the scan reaches the name it can only filter what it
/// has already read. Same rows, and `:plan` shows what it cost to get them: which key
/// fields the seek pinned, and which it left `_` for the scan to walk.
///
/// The fourth is the **other** pair, and the other half of navigation: it *reads
/// through* the reference the third *joins* on. `D.module.name` is a point read per
/// declaration (`fetch[r0.module]`), where the join spelling —
/// `src.Decl {module = src.Module {name = M}}` — is a second scan whose rows are
/// matched by id. Same answer, and which one is right is a property of the
/// question: a fetch suits a reference each row has exactly one of, and a join
/// suits one where the *other* side is what narrows.
///
/// The last two are **negations**, and they are the query a code index is asked at
/// review time: what is here that nothing uses. Both are a `Step::Test` — no
/// register, no row of its own — and the pair shows the same sargeability the seek
/// examples do, one loop deeper. `!src.Import {from = M}` narrows to
/// `seek[from = r0#, to = _]`, because `from` leads that key; `!src.Ref {to = D}`
/// cannot, `to` being last, so it scans and filters. Same shape of answer, and
/// `:plan` says which one paid for it.
///
/// The one before them is prefix search again, and the thing it shows is that **naming the
/// answer costs nothing**. `F` is the file path, captured — ordinarily the thing that
/// closes a seek prefix, because an output cannot narrow a scan — and `F = "query"..`
/// says what that output has to look like, so the level that binds it seeks the range
/// instead. `:plan` shows `src.File seek["query"..]`, the same range the prefix written
/// in the pattern reaches, and then the join into `src.Module` by id
/// ([chapter 7](../docs/07-compilation.md#what-a-bind-can-mean)).
const EXAMPLES: [&str; 10] = [
    "X where X = src.SearchByName {name = \"encode\"..}",
    "D where D = src.Decl {name = \"encode\"..}",
    "D.name where D = src.Decl {module = src.Module {file = src.File \"store/codec.py\"}}",
    "{decl = D.name, module = D.module.name} where D = src.Decl {name = \"encode\"..}",
    "D.value where D = src.Decl {name = \"encode_key\"}",
    "{file = F, line = L} where src.Ref {file = F, at = {line = L}, to = src.Decl {name = \"encode_str\"}}",
    "M where src.Import {from = M, to = src.Module {name = \"store.codec\"}}",
    "F where src.Module {file = src.File F}; F = \"query\"..",
    "M.name where M = src.Module _; !src.Import {from = M}",
    "D.name where D = src.Decl _; !src.Ref {to = D}",
];

/// One shell command.
///
/// `argument` is `Some` when the command takes **focus source** — a query, or a
/// predicate name. That is the field the highlighter reads, and it is why this is
/// a table rather than three lists: the help text, the highlighter and the hinter all
/// need to know the same thing. The dispatch in [`shell`] is the fourth reader and the
/// one that can drift — a command added here without an arm there is advertised and
/// highlighted but does nothing.
struct Command {
    name: &'static str,
    argument: Option<&'static str>,
    help: &'static str,
}

/// The commands, in the order `:help` lists them: what a query *is*, then what it
/// costs, then what is stored, then the shell itself.
const COMMANDS: [Command; 7] = [
    Command {
        name: ":type",
        argument: Some("<query>"),
        help: "the type of its head, without planning or running it",
    },
    Command {
        name: ":plan",
        argument: Some("<query>"),
        help: "the plan it compiles to, without running it",
    },
    Command {
        name: ":facts",
        argument: Some("<name>"),
        help: "rows stored for a predicate, read through the executor",
    },
    Command {
        name: ":schema",
        argument: None,
        help: "the predicates this shell knows",
    },
    Command {
        name: ":clear",
        argument: None,
        help: "clear the screen",
    },
    Command {
        name: ":help",
        argument: None,
        help: "this, and some queries to try",
    },
    Command {
        name: ":quit",
        argument: None,
        help: "leave (or Ctrl-D)",
    },
];

fn print_help() {
    println!("  <query>          compile and run a focus query, e.g.");
    for example in EXAMPLES {
        println!("                     {example}");
    }

    for Command {
        name,
        argument,
        help,
    } in &COMMANDS
    {
        let invocation = match argument {
            Some(argument) => format!("{name} {argument}"),
            None => (*name).to_owned(),
        };
        println!("  {invocation:<16} {help}");
    }
}

fn print_schema(schema: &Schema) {
    for index in 0..schema.len() {
        let Some(predicate) = schema.get(PredicateId(index as u32)) else {
            continue;
        };

        let key = render_predicate_ty(predicate.key().ty, schema, schema.interner());
        let value = predicate.value().map_or_else(String::new, |value| {
            format!(
                "{}{}",
                Role::Punctuation.paint(" -> "),
                render_predicate_ty(value.ty, schema, schema.interner())
            )
        });

        println!(
            "  {}{} {key}{value}",
            Role::Predicate.paint(predicate.name().unwrap_or("?")),
            Role::Punctuation.paint(":"),
        );
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
        body: Step::levels([Level::seek(
            Access {
                predicate_id: id,
                seek_key: SeekKey::Prefix(Box::new([])),
            },
            Box::new([Address::new(0)]),
            Box::new([]),
        )]),
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
    let schema = demo_schema();

    let db = FjallDb::open(dir)?;
    db.create_predicates((0..schema.len()).map(|index| PredicateId(index as u32)))?;
    let written = seed(&db, &schema)?;

    // Straight to the prompt. Everything a banner used to print is a command away —
    // the predicates and the store behind `:schema`, the commands and some example
    // queries behind `:help` — and the pointer to it appears where someone is
    // actually looking for it: after typing a command that does not exist.

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
                    Some((":type", query)) => print_type(query.trim(), &schema),
                    _ if line == ":facts" => println!("  :facts needs a predicate — try :schema"),
                    _ if line == ":plan" => println!("  :plan needs a query — try :help"),
                    _ if line == ":type" => println!("  :type needs a query — try :help"),
                    // Best effort, and silent off a tty: the next `readline` reprints
                    // the prompt at the top of the cleared screen either way.
                    _ if line == ":clear" => {
                        let _ = editor.clear_screen();
                    }
                    _ if line == ":schema" => {
                        print_schema(&schema);
                        // Where the facts came from, how many, and where they went,
                        // which start-up used to print: it belongs with the predicates
                        // rather than nowhere.
                        println!(
                            "\n  {written} facts from example/index.json in {}",
                            dir.display()
                        );
                    }
                    _ if line == ":help" => print_help(),
                    _ if line == ":quit" || line == ":q" => return Ok(()),
                    _ if line.starts_with(':') => {
                        println!("  no such command: {line}");
                        println!("  :help for commands and example queries");
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
