//! `aperture shell <db>` — the REPL, always over the wire.
//!
//! **Remote-first, and that is the point rather than a limitation**
//! ([operations §5](../../docs/aperture-cli-design.md)). The shell is the permanent
//! exerciser of the wire format: every query a person types here is a real handshake, a
//! real stream and a real page of `DATA_ROW` frames, so a format change that the tests
//! happen not to cover still cannot survive somebody using the tool. `aperture shell`
//! with no database is the *other* shell — [`crate::shell`], Phase 5's embedded demo
//! over a scratch database it seeds itself.
//!
//! # It compiles what you type, and that is why the errors look like the demo's
//!
//! This shell used to hand every line to the server and print whatever came back:
//! plain text, no colour, no caret, and a round trip to be told about a typo. It
//! compiles the line **here** now, against the schema the server said it serves
//! ([`Connection::served_schema`]) — so a mistake is a caret under the word, in colour,
//! before anything crosses the socket, and `:plan` and `:type` are answerable at all.
//!
//! Fetching the schema is what makes that honest rather than hopeful. A database
//! carries the schema it was created against ([I13](../../docs/invariants.md#i13)), so
//! compiling against this tool's *built-in* one would be checking a query against a
//! schema nobody is using. The one assumption left is that the server's compiler is
//! this compiler: against a server of a different build the local answer can differ,
//! and the rule is that the **server** decides what runs — a query it refuses is
//! refused with its own message, whatever this one thought.
//!
//! # `:more` is what this was built for
//!
//! A result here is a **bookmark**, not a buffer. `:more` reads the next page and
//! stops; nothing is held at either end, because the place is kept by the *stream*
//! staying open — server-side, parked on a full outbound queue with a bytes-only cursor
//! whose snapshot was released at the chunk boundary
//! ([I8](../../docs/invariants.md#i8)). A pause of a millisecond and a pause of an hour
//! cost the server the same thing.
//!
//! Until this existed, [I4](../../docs/invariants.md#i4) — resume equals an
//! uninterrupted run, the most heavily tested machinery in this project — had **no
//! interactive exerciser at all**: Phase 5's REPL discards the resume token at both of
//! its call sites. `:more` is a person holding a cursor across a round trip, and
//! [`pages_concatenate_to_an_uninterrupted_run`](tests) is that claim as a test.
//!
//! # Errors do not end the session, and which ones do is a rule
//!
//! A [`ClientError::Server`] is the server refusing one *stream* — a query it will not
//! run, a database that has gone — and psql's answer is right: print it and keep the
//! prompt. Anything else (`Io`, `Wire`, `Protocol`) means the conversation itself is
//! broken; the loop says so and opens a new one, because a server restarted under a
//! shell is a hiccup rather than the end of an afternoon.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use aperture_client::{ClientError, Connection, Mode, Rows};
use aperture_engine::{compile::Compilation, print};
use aperture_schema::{
    schema::{PredicateId, Schema},
    syntax::print as schema_print,
};
use codespan_reporting::term::{
    self,
    termcolor::{Ansi, NoColor},
};

use crate::{
    CliError,
    cli::RowFormat,
    commands::query::{connect, render_profile},
    prompt::{self, Command, FocusHelper},
    rows::Sink,
};

/// Rows delivered per page, and what `:more` continues.
///
/// Deliberately unrelated to the server's `CHUNK_ROWS`: a page is what a person can
/// read, a chunk is what the executor computes between suspends, and tying them
/// together would make a display choice into a protocol one. `:limit` moves it.
const PAGE: usize = 40;

/// What `:list` runs.
///
/// Every column the design's listing names, and it is ordinary focus — a whole-row bind
/// and six field reads, no different from anything a person types. `facts` and `bytes`
/// read `-1` until a database is sealed, which is when they are counted.
const LISTING: &str = "{name = D.name, status = D.status, facts = D.facts, \
                       bytes = D.bytes, instance = D.instance} \
                       where D = aperture.db.List _";

/// The commands, in the order `:help` lists them: what a query *is*, then what it
/// costs, then what is stored, then the session, then the shell itself.
///
/// **Two prefixes, one meaning.** `:` is this tool's, and the `\` spellings are what a
/// hand trained on psql types without thinking; neither can begin a focus query, so
/// accepting both costs nothing. Aliases are not advertised — a help screen that lists
/// every spelling twice is one nobody reads to the end.
pub const COMMANDS: [Command; 14] = [
    Command {
        name: ":type",
        aliases: &[],
        argument: Some("<query>"),
        help: "the type of its head, without planning or running it",
    },
    Command {
        name: ":plan",
        aliases: &[],
        argument: Some("<query>"),
        help: "the plan it compiles to, without running it",
    },
    Command {
        name: ":facts",
        aliases: &[],
        argument: Some("<predicate>"),
        help: "every row of one predicate — sugar for `X where <predicate> X`",
    },
    Command {
        name: ":schema",
        aliases: &["\\d", ":d"],
        argument: Some("[name]"),
        help: "the schema this database is served with, or one predicate, or a prefix",
    },
    Command {
        name: ":more",
        aliases: &["\\more", ":m"],
        argument: None,
        help: "the next page of the last result",
    },
    Command {
        name: ":limit",
        aliases: &[],
        argument: None,
        help: "rows per page (bare, it says what the page is)",
    },
    Command {
        name: ":format",
        aliases: &[],
        argument: None,
        help: "how a row prints: jsonl, json, table, raw",
    },
    Command {
        name: ":cancel",
        aliases: &["\\cancel"],
        argument: None,
        help: "stop the last result early",
    },
    Command {
        name: ":timing",
        aliases: &["\\timing"],
        argument: None,
        help: "toggle how long a page took",
    },
    Command {
        name: ":profile",
        aliases: &["\\profile"],
        argument: None,
        help: "toggle what a query examined, per step of its plan",
    },
    Command {
        name: ":list",
        aliases: &["\\l", ":l"],
        argument: None,
        help: "the databases on this server — a query over aperture.db.List",
    },
    Command {
        name: ":connect",
        aliases: &["\\c", ":c"],
        argument: Some("<database>"),
        help: "the same session against another database",
    },
    Command {
        name: ":clear",
        aliases: &[],
        argument: None,
        help: "clear the screen",
    },
    Command {
        name: ":help",
        aliases: &["\\?", ":?", ":h"],
        argument: None,
        help: "this",
    },
];

/// `:quit` is not in the table, because the table is what the *shell* answers and this
/// is what ends it — it is handled before dispatch, beside Ctrl-D.
const QUIT: [&str; 4] = [":quit", ":q", "\\q", "\\quit"];

/// Whether the loop should keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continue,
    Quit,
}

/// The result `:more` would continue.
struct Held {
    rows: Rows,
    /// Rows handed to the person so far, across every page of this result.
    delivered: u64,
}

/// One interactive session against one database.
///
/// Split from the readline loop on purpose: everything below is driven by
/// [`handle`](Repl::handle) taking a line and writing to a sink, so the tests drive a
/// real session against a real server without a terminal anywhere in it.
pub struct Repl {
    connection: Connection,
    /// The address this session was opened with — a bare name, or §2's
    /// `aperture://host:port/database`.
    ///
    /// Kept whole because it is what a *reconnect* needs: a shell started against a
    /// remote server and then told `:connect other` means the other database on that
    /// server, and remembering only the name would quietly answer with the local one.
    address: String,
    /// The database's own name, which is what the prompt says. For a bare address the
    /// two are the same string.
    database: String,
    socket: PathBuf,
    /// **The schema the server said it serves**, not the one this tool was built with.
    ///
    /// Everything local is compiled and described against it: a query before it is
    /// sent, `:plan`, `:type`, `:schema`, and the names tab-completion offers.
    schema: Arc<Schema>,
    held: Option<Held>,
    timing: bool,
    profiling: bool,
    page: usize,
    format: RowFormat,
    /// Set by Ctrl-C while a page is being read, and cleared before every line.
    ///
    /// A page is read a row at a time so this can be *noticed*: the alternative is a
    /// shell that ignores Ctrl-C until the page it is midway through has finished
    /// arriving, which on a scan of a large predicate is exactly when somebody presses
    /// it.
    interrupt: Arc<AtomicBool>,
}

impl Repl {
    /// Connect, read-only.
    ///
    /// # Errors
    ///
    /// [`CliError::NoServer`] if nothing is listening, or whatever the handshake says.
    pub fn connect(socket: &Path, address: &str) -> Result<Repl, CliError> {
        // Read-only, for the reason `query` is: a reader has no claim to make about the
        // schema, and the database's is the one that counts.
        let mut connection = connect(socket, address, Mode::ReadOnly)?;

        // **Asked, not assumed.** Everything this shell does locally is against this
        // schema, and a database created with `--schema` has one this tool has never
        // seen.
        let schema = Arc::new(connection.served_schema()?);

        Ok(Repl {
            connection,
            address: address.to_owned(),
            database: named(address).to_owned(),
            socket: socket.to_path_buf(),
            schema,
            held: None,
            timing: false,
            profiling: false,
            page: PAGE,
            format: RowFormat::Jsonl,
            interrupt: Arc::new(AtomicBool::new(false)),
        })
    }

    /// What the prompt says, which is which database is answering.
    #[must_use]
    pub fn prompt(&self) -> String {
        format!("{}=> ", self.database)
    }

    /// The predicate names tab-completion should offer.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        (0..self.schema.len())
            .filter_map(|index| predicate_named(&self.schema, index).map(str::to_owned))
            .collect()
    }

    /// One line from the person.
    ///
    /// # Errors
    ///
    /// Only what ends the session: a broken connection, or a write that fails. A
    /// refusal from the server is printed and swallowed — see the module docs.
    pub fn handle(&mut self, line: &str, out: &mut impl Write) -> Result<Control, CliError> {
        let line = line.trim();

        if line.is_empty() {
            return Ok(Control::Continue);
        }

        self.interrupt.store(false, Ordering::Relaxed);

        if prompt::starts_a_command(line) {
            return self.meta(line, out);
        }

        match self.query(line, out) {
            Ok(()) => Ok(Control::Continue),
            Err(error) => refused(error, out).map(|()| Control::Continue),
        }
    }

    fn meta(&mut self, line: &str, out: &mut impl Write) -> Result<Control, CliError> {
        let (word, argument) = prompt::split_command(line);

        if QUIT.contains(&word) {
            return Ok(Control::Quit);
        }

        let Some(command) = COMMANDS.iter().find(|command| command.answers_to(word)) else {
            writeln!(out, "  no such command: {word} — :help lists what there is")?;
            return Ok(Control::Continue);
        };

        match command.name {
            ":help" => help(out)?,

            ":clear" => {
                // The escape a terminal understands, and nothing at all when this is
                // not one: a `Vec<u8>` in a test should not collect a screen-clear.
                if prompt::colours_enabled() {
                    write!(out, "\x1b[2J\x1b[H")?;
                }
            }

            ":schema" => self.describe(argument, out)?,

            ":type" => self.explain(argument, out, Explain::Type)?,
            ":plan" => self.explain(argument, out, Explain::Plan)?,

            ":facts" => {
                if argument.is_empty() {
                    writeln!(out, "  :facts needs a predicate — :schema lists them")?;
                } else {
                    // Shown rather than hidden, because it is the query somebody would
                    // have written and the shell is also a way to learn the language.
                    let query = format!("X where {argument} X");
                    writeln!(out, "  {query}")?;
                    self.run_or_report(&query, out)?;
                }
            }

            ":more" => match self.more(out) {
                Ok(()) => {}
                Err(error) => refused(error, out)?,
            },

            ":limit" => self.limit(argument, out)?,
            ":format" => self.set_format(argument, out)?,

            ":cancel" => match self.cancel() {
                Ok(Some(sent)) => writeln!(out, "  cancelled after {sent} row(s)")?,
                Ok(None) => writeln!(out, "  nothing to cancel")?,
                Err(error) => refused(error, out)?,
            },

            ":timing" => {
                self.timing = !self.timing;
                writeln!(out, "  timing is {}", on_off(self.timing))?;
            }

            ":profile" => {
                self.profiling = !self.profiling;
                writeln!(out, "  profile is {}", on_off(self.profiling))?;
                if self.profiling {
                    writeln!(
                        out,
                        "  what the next query examines is reported when it ends"
                    )?;
                }
            }

            // **`:list` is a query, and nothing here makes it a special case.** It is
            // written out rather than hidden behind a control message precisely so that
            // it can be edited: the text is a starting point a person can paste, narrow
            // with a `status =`, or page with `:more`, which is what
            // [operations §5](../../docs/aperture-cli-design.md) means by putting
            // enumeration through the normal machinery.
            ":list" => self.run_or_report(LISTING, out)?,

            ":connect" => {
                if argument.is_empty() {
                    writeln!(out, "  :connect needs a database name")?;
                } else {
                    let wanted = sibling(&self.address, argument);
                    self.reconnect(&wanted, out)?;
                }
            }

            other => writeln!(out, "  {other} is in the table and has no arm — a bug")?,
        }

        Ok(Control::Continue)
    }

    /// Run a query and show its first page, reporting a refusal rather than raising it.
    fn run_or_report(&mut self, source: &str, out: &mut impl Write) -> Result<(), CliError> {
        match self.query(source, out) {
            Ok(()) => Ok(()),
            Err(error) => refused(error, out),
        }
    }

    /// Compile a line here, and run it there.
    ///
    /// The compile is not a formality: it is where a person's mistake is answered, in
    /// colour and under a caret, without a round trip. What it must not do is decide
    /// more than it knows — so a query that compiles is sent as typed, and the server's
    /// answer is the one that counts.
    fn query(&mut self, source: &str, out: &mut impl Write) -> Result<(), ClientError> {
        let mut compilation = Compilation::new(source, &self.schema);
        let plan = compilation.plan();

        render_diagnostics(&compilation, out).map_err(ClientError::Io)?;

        if compilation.diagnostics().has_errors() {
            return Ok(());
        }

        if let Some(head) = compilation.head_ty() {
            writeln!(
                out,
                "  : {}",
                prompt::render_ty(head, &self.schema, compilation.interner())
            )
            .map_err(ClientError::Io)?;
        }

        if plan.is_none() {
            writeln!(
                out,
                "  (no plan, and no diagnostic saying why — that is a compiler bug)"
            )
            .map_err(ClientError::Io)?;
            return Ok(());
        }

        // A new query ends the old result. Cancelling rather than dropping is what
        // keeps the server's side tidy in band: the stream completes with what it
        // sent, instead of being abandoned for the connection to clean up later.
        self.cancel()?;

        let started = Instant::now();
        let rows = if self.profiling {
            self.connection.query_profiled(source)?
        } else {
            self.connection.query(source)?
        };

        self.held = Some(Held { rows, delivered: 0 });

        self.page(started, out)
    }

    /// The next page of the held result.
    fn more(&mut self, out: &mut impl Write) -> Result<(), ClientError> {
        if self.held.is_none() {
            let _ = writeln!(out, "  no result to continue — run a query first");
            return Ok(());
        }

        let started = Instant::now();
        self.page(started, out)
    }

    /// Read one page of whatever is held, render it, and say what comes next.
    fn page(&mut self, started: Instant, out: &mut impl Write) -> Result<(), ClientError> {
        let Some(held) = self.held.as_mut() else {
            return Ok(());
        };

        // A page at a time through the same renderer `query` uses, so the shell cannot
        // drift from the non-interactive tool in how a row reads.
        let mut sink =
            Sink::new(&mut *out, self.format, held.rows.desc()).map_err(ClientError::Io)?;

        // **A row at a time, so Ctrl-C is noticed.** `take(page)` would block until the
        // whole page had arrived, which on a scan that produces few rows is precisely
        // where somebody reaches for it.
        let mut shown = 0;
        let mut stopped = false;

        while shown < self.page {
            if self.interrupt.load(Ordering::Relaxed) {
                stopped = true;
                break;
            }

            let row = self.connection.take(&mut held.rows, 1)?;
            if row.is_empty() {
                break;
            }

            for value in &row {
                sink.row(value).map_err(ClientError::Io)?;
            }
            shown += row.len();
        }

        let written = sink.end().map_err(ClientError::Io)?;
        held.delivered += written;

        let delivered = held.delivered;
        let profile = held.rows.profile().cloned();
        let elapsed = started.elapsed();

        if stopped {
            let sent = self.cancel()?.unwrap_or(delivered);
            writeln!(out, "  interrupted after {sent} row(s)").map_err(ClientError::Io)?;
        } else if held.rows.finished() {
            // Only now, and only if asked: the tally is not final until the last chunk
            // has run, which is why the server sends it once, just before the end.
            if let Some(profile) = profile.as_ref() {
                write!(out, "{}", render_profile(profile, delivered)).map_err(ClientError::Io)?;
            }

            // **Not twice.** A table counts its own rows as it closes, and a shell that
            // added a total underneath would print two numbers that agree, which reads
            // as a bug in whichever one you did not expect. The shapes that count
            // nothing get the total here, where it is the only one.
            if !matches!(self.format, RowFormat::Table | RowFormat::Count) {
                writeln!(out, "  {delivered} row(s)").map_err(ClientError::Io)?;
            }

            self.held = None;
        } else {
            writeln!(
                out,
                "  :more for the next {} — {delivered} so far",
                self.page
            )
            .map_err(ClientError::Io)?;
        }

        if self.timing {
            writeln!(out, "  {:.3} ms", elapsed.as_secs_f64() * 1000.0).map_err(ClientError::Io)?;
        }

        Ok(())
    }

    /// Stop the held result, in band, and answer with what it had sent.
    fn cancel(&mut self) -> Result<Option<u64>, ClientError> {
        match self.held.take() {
            Some(mut held) => self.connection.cancel(&mut held.rows).map(Some),
            None => Ok(None),
        }
    }

    /// `:limit` — how many rows a page is.
    fn limit(&mut self, argument: &str, out: &mut impl Write) -> Result<(), CliError> {
        if argument.is_empty() {
            writeln!(out, "  {} row(s) per page", self.page)?;
            return Ok(());
        }

        match argument.parse::<usize>() {
            Ok(rows) if rows > 0 => {
                self.page = rows;
                writeln!(out, "  {rows} row(s) per page")?;
            }
            // Zero is refused rather than read as "no limit": a page of nothing would
            // hand back an empty result and a `:more` that never ends, and "all of it"
            // is a number somebody can type.
            _ => writeln!(out, "  :limit takes a row count — `:limit 100`")?,
        }

        Ok(())
    }

    /// `:format` — how a row prints.
    fn set_format(&mut self, argument: &str, out: &mut impl Write) -> Result<(), CliError> {
        if argument.is_empty() {
            writeln!(out, "  rows print as {}", format_name(self.format))?;
            return Ok(());
        }

        let chosen = match argument {
            "jsonl" => Some(RowFormat::Jsonl),
            "json" => Some(RowFormat::Json),
            "table" => Some(RowFormat::Table),
            "raw" => Some(RowFormat::Raw),
            _ => None,
        };

        match chosen {
            Some(format) => {
                self.format = format;
                writeln!(out, "  rows print as {}", format_name(format))?;
            }
            None => writeln!(out, "  :format takes jsonl, json, table or raw")?,
        }

        Ok(())
    }

    /// `:type` and `:plan` — the two questions answered without running anything.
    fn explain(
        &mut self,
        source: &str,
        out: &mut impl Write,
        what: Explain,
    ) -> Result<(), CliError> {
        if source.is_empty() {
            writeln!(out, "  {} needs a query — :help", what.name())?;
            return Ok(());
        }

        let mut compilation = Compilation::new(source, &self.schema);

        // `:type` stops at typecheck on purpose: a query can have a perfectly good head
        // type and no plan (a variable nothing binds), and being told the type is the
        // more useful of the two answers when that happens.
        let plan = match what {
            Explain::Type => {
                compilation.check();
                None
            }
            Explain::Plan => compilation.plan(),
        };

        render_diagnostics(&compilation, out)?;

        if compilation.diagnostics().has_errors() {
            return Ok(());
        }

        match what {
            Explain::Type => match compilation.head_ty() {
                Some(ty) => writeln!(
                    out,
                    "  : {}",
                    prompt::render_ty(ty, &self.schema, compilation.interner())
                )?,
                None => writeln!(
                    out,
                    "  (no type, and no diagnostic saying why — that is a compiler bug)"
                )?,
            },

            Explain::Plan => match plan {
                Some(plan) => writeln!(
                    out,
                    "{}",
                    print::plan(&plan, &self.schema, compilation.interner())
                )?,
                None => writeln!(
                    out,
                    "  (no plan, and no diagnostic saying why — that is a compiler bug)"
                )?,
            },
        }

        Ok(())
    }

    /// `:schema` — the whole thing, one predicate, or a namespace.
    ///
    /// **The source the server sent**, painted by the schema language's own lexer.
    /// Printing the text rather than a rendering of the type model is what makes it
    /// something a person can copy into a file and hand back to `create --schema`.
    fn describe(&mut self, name: &str, out: &mut impl Write) -> Result<(), CliError> {
        if name.is_empty() {
            let source = schema_print::served(&self.schema);
            write!(out, "{}", prompt::paint_schema(&source))?;
            writeln!(out, "  {} predicate(s)", self.schema.len())?;
            return Ok(());
        }

        let exact = (0..self.schema.len())
            .find(|index| predicate_named(&self.schema, *index) == Some(name));

        if let Some(index) = exact {
            writeln!(out, "{}", predicate_line(&self.schema, index))?;
            return Ok(());
        }

        // **Prefix fallback, so `:schema src.` dumps a namespace rather than failing.**
        // A name that does not resolve is much more often a namespace someone is
        // exploring than a typo, and psql and Glean's shell both read it that way.
        let matching: Vec<usize> = (0..self.schema.len())
            .filter(|index| {
                predicate_named(&self.schema, *index).is_some_and(|it| it.starts_with(name))
            })
            .collect();

        if matching.is_empty() {
            writeln!(out, "  no predicate matches `{name}`")?;
        } else {
            for index in matching {
                writeln!(out, "{}", predicate_line(&self.schema, index))?;
            }
        }

        Ok(())
    }

    /// `:connect` — the same session against another database.
    fn reconnect(&mut self, database: &str, out: &mut impl Write) -> Result<(), CliError> {
        // The old result goes with the old connection, and saying so is better than
        // leaving a `:more` that would silently continue a result from a database the
        // person has stopped looking at.
        let had = self.held.is_some();

        match Repl::connect(&self.socket, database) {
            Ok(fresh) => {
                let (timing, profiling, page, format) =
                    (self.timing, self.profiling, self.page, self.format);
                let interrupt = Arc::clone(&self.interrupt);

                *self = fresh;
                self.timing = timing;
                self.profiling = profiling;
                self.page = page;
                self.format = format;
                // The Ctrl-C handler holds this one: a fresh flag would be a flag
                // nothing sets.
                self.interrupt = interrupt;

                writeln!(out, "  now connected to `{}`", self.database)?;
                if had {
                    writeln!(out, "  the previous result is gone")?;
                }
            }

            // A database that is not there is not a reason to lose the session — the
            // old connection is still open and still answering.
            Err(error) => writeln!(out, "  {error}")?,
        }

        Ok(())
    }
}

/// Which of the two no-run questions is being asked.
#[derive(Debug, Clone, Copy)]
enum Explain {
    Type,
    Plan,
}

impl Explain {
    fn name(self) -> &'static str {
        match self {
            Explain::Type => ":type",
            Explain::Plan => ":plan",
        }
    }
}

/// Render whatever the compiler found, **in colour when there is a terminal**.
///
/// The reason this is worth the two arms: a diagnostic's value is the caret and the
/// span, and codespan draws both — but it draws them through a style-aware writer, so a
/// shell that renders to a string first gets the text and loses the emphasis. Off a
/// terminal it is plain, which is what a test asserts on and what a pipe wants.
fn render_diagnostics(compilation: &Compilation, out: &mut impl Write) -> std::io::Result<()> {
    let config = term::Config::default();

    if prompt::colours_enabled() {
        let _ = compilation.render(&mut Ansi::new(&mut *out), &config);
    } else {
        let _ = compilation.render(&mut NoColor::new(&mut *out), &config);
    }

    Ok(())
}

/// A refusal is the server declining one stream, and the session survives it.
fn refused(error: ClientError, out: &mut impl Write) -> Result<(), CliError> {
    match error {
        ClientError::Server { message, .. } => {
            writeln!(out, "{message}")?;
            Ok(())
        }
        other => Err(other.into()),
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn format_name(format: RowFormat) -> &'static str {
    match format {
        RowFormat::Table => "table",
        RowFormat::Json => "json",
        RowFormat::Jsonl => "jsonl",
        RowFormat::Raw => "raw",
        RowFormat::Count => "count",
    }
}

/// The address of another database **on the same server** as `address`.
///
/// A bare name stays a bare name; a remote address keeps its host and swaps the
/// database, because `:connect` means "the one next to this" and somebody who reached a
/// server over TCP did not stop meaning that server.
fn sibling(address: &str, database: &str) -> String {
    match address.rsplit_once('/') {
        Some((prefix, _)) if address.starts_with(crate::commands::query::ADDRESS_SCHEME) => {
            format!("{prefix}/{database}")
        }
        _ => database.to_owned(),
    }
}

/// The database an address names — everything after the last `/`, or the whole of a
/// bare name.
fn named(address: &str) -> &str {
    address.rsplit_once('/').map_or(address, |(_, name)| name)
}

fn predicate_named(schema: &Schema, index: usize) -> Option<&str> {
    schema.get(PredicateId(index as u32))?.name()
}

/// One predicate, as `:schema <name>` prints it — in the schema's own syntax, painted
/// by the schema's own lexer.
fn predicate_line(schema: &Schema, index: usize) -> String {
    let id = PredicateId(index as u32);

    let Some(name) = predicate_named(schema, index) else {
        return String::new();
    };
    let Some(signature) = schema_print::signature(schema, id) else {
        return String::new();
    };

    let virtual_note = if schema.is_virtual(id) {
        "   (virtual — the server answers it)"
    } else {
        ""
    };

    format!(
        "  {}{virtual_note}",
        prompt::paint_schema(&format!("predicate {name} : {signature}"))
    )
}

fn help(out: &mut impl Write) -> Result<(), CliError> {
    writeln!(out, "  <query>          run a focus query, e.g.")?;
    writeln!(out, "                     X where src.File X")?;
    writeln!(out, "                     {{file = F, line = L}} where …")?;

    for command in &COMMANDS {
        let spelling = match command.argument {
            Some(argument) => format!("{} {argument}", command.name),
            None => command.name.to_owned(),
        };

        writeln!(out, "  {spelling:<16} {}", command.help)?;
    }

    writeln!(out, "  :quit            leave (or Ctrl-D)")?;
    writeln!(
        out,
        "  a line with an unclosed {{ or ( continues on the next one"
    )?;

    Ok(())
}

/// The readline loop.
///
/// Thin on purpose: everything worth testing is in [`Repl::handle`], and what is left
/// here is a terminal — plus the two things only a terminal has, a Ctrl-C that means
/// "stop reading rows" and a history that outlives the process.
///
/// # Errors
///
/// Whatever ends the session and cannot be reconnected around, or a readline failure
/// that is not an interrupt.
pub fn run(socket: &Path, database: &str) -> Result<(), CliError> {
    use rustyline::{Editor, error::ReadlineError, history::DefaultHistory};

    let mut repl = Repl::connect(socket, database)?;

    // **Two lines, and the second one is the only reason for the first.** A shell that
    // says nothing leaves a person to guess whether `\?` or `:help` or `help` is the
    // one — and this shell has commands the last one did not, prints rows in a shape
    // the last one did not, and pages. Each of those is a sentence long.
    println!(
        "aperture shell — `{}` on {}",
        repl.database,
        socket.display()
    );
    println!(
        "  {} predicate(s) · rows print as jsonl · :help for commands",
        repl.schema.len()
    );

    let mut editor: Editor<FocusHelper, DefaultHistory> =
        Editor::new().map_err(|error| CliError::Shell(error.to_string()))?;
    editor.set_helper(Some(FocusHelper::new(&COMMANDS)));

    if let Some(helper) = editor.helper() {
        helper.knows(repl.names());
    }

    // History is a person's, not a database's: it survives the session, and a shell
    // that forgets what was typed a minute ago is one nobody explores with.
    let history = prompt::history_path();
    if let Some(path) = history.as_ref() {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let _ = editor.load_history(path);
    }

    // **Ctrl-C stops the rows, not the shell.** Set here and read a row at a time by
    // the pager; rustyline handles the keystroke itself while a *line* is being typed,
    // so the two never contend for it.
    {
        let interrupt = Arc::clone(&repl.interrupt);
        let _ = ctrlc::set_handler(move || interrupt.store(true, Ordering::Relaxed));
    }

    let stdout = std::io::stdout();

    loop {
        let line = match editor.readline(&repl.prompt()) {
            Ok(line) => line,

            // Ctrl-C abandons the line, as it does everywhere; Ctrl-D leaves. A held
            // result goes with the session either way, and the server notices the
            // socket closing.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(error) => return Err(CliError::Shell(error.to_string())),
        };

        let _ = editor.add_history_entry(&line);

        let outcome = {
            let mut out = stdout.lock();
            repl.handle(&line, &mut out)
        };

        match outcome {
            Ok(Control::Quit) => break,
            Ok(Control::Continue) => {}

            // The conversation is broken rather than the request refused — see the
            // module docs for which is which. One attempt to open a new one, because a
            // server restarted under a shell should cost a line rather than a session;
            // if that fails too, there is nothing left to talk to.
            Err(error) => {
                eprintln!("aperture: {error}");

                match Repl::connect(&repl.socket.clone(), &repl.address.clone()) {
                    Ok(fresh) => {
                        eprintln!("aperture: reconnected to `{}`", fresh.database);
                        let interrupt = Arc::clone(&repl.interrupt);
                        repl = fresh;
                        repl.interrupt = interrupt;

                        if let Some(helper) = editor.helper() {
                            helper.knows(repl.names());
                        }
                    }
                    Err(again) => {
                        eprintln!("aperture: {again}");
                        return Err(error);
                    }
                }
            }
        }
    }

    if let Some(path) = history.as_ref() {
        let _ = editor.save_history(path);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Control, Repl};
    use crate::testing::{Serving, serving};

    fn repl(serving: &Serving) -> Repl {
        Repl::connect(&serving.socket, "code").expect("a session")
    }

    fn typed(repl: &mut Repl, line: &str) -> String {
        let mut out = Vec::new();
        repl.handle(line, &mut out).expect("the session survives");
        String::from_utf8(out).expect("utf-8")
    }

    /// The paths a row can travel: one page at a time, and all at once.
    fn paths(rows: &str) -> Vec<String> {
        rows.lines()
            .filter(|line| line.contains(".py"))
            .map(|line| line.trim().to_owned())
            .collect()
    }

    /// **The acceptance criterion of Phase 9f, and of this whole shell.**
    ///
    /// `:more` holds a bytes-only cursor across a round trip and resumes it, which is
    /// [I4](../../docs/invariants.md#i4) — resume equals an uninterrupted run —
    /// exercised interactively for the first time. The battery has proved this over
    /// generated plans since Phase 0; what it has never had is a person's hand on it.
    ///
    /// The check is the *concatenation*: pages that each look plausible can still drop
    /// a row at a boundary or repeat one, and only the whole sequence against the whole
    /// answer says otherwise.
    /// The server computes at most this many rows before suspending to a cursor.
    ///
    /// `aperture_server`'s own `CHUNK_ROWS`, restated because it is private — and the
    /// number is load-bearing *here* rather than incidental: a result smaller than one
    /// chunk is answered without the executor ever suspending, so a paging test that
    /// stayed under it would exercise the client's arithmetic and nothing else.
    const SERVER_CHUNK: usize = 256;

    #[test]
    fn pages_concatenate_to_an_uninterrupted_run() {
        // Four chunks, so at least three real suspend-and-resume cycles happen behind
        // the pages — and 25 pages over them, so a page boundary lands inside a chunk
        // and a chunk boundary lands inside a page. Those are the two places a row
        // gets dropped or repeated.
        const ROWS: usize = 1000;

        // Compile-time, because both sides are constants: the claim is about how this
        // test is *built*, and a runtime assertion on two literals is one somebody
        // eventually deletes as noise. The corpus must span several chunks or no cursor
        // is ever resumed and the test below proves nothing.
        const _: () = assert!(ROWS > 3 * SERVER_CHUNK);

        let serving = serving(ROWS);

        let paged = {
            let mut repl = repl(&serving);
            let mut all = paths(&typed(&mut repl, "F where src.File F"));
            let mut pages = 1;

            while repl.held.is_some() {
                all.extend(paths(&typed(&mut repl, ":more")));
                pages += 1;
            }

            assert!(pages > ROWS / super::PAGE, "it really was paged: {pages}");
            all
        };

        // The same query on a fresh session, taken in one go — which is the run the
        // pages have to equal. Comparing against a *recomputed* answer rather than
        // against a count is the whole point: a resume that dropped one row and
        // repeated another would still be 1000 rows.
        let whole = {
            let mut repl = repl(&serving);
            repl.page = ROWS * 2;
            paths(&typed(&mut repl, "F where src.File F"))
        };

        assert_eq!(
            whole.len(),
            ROWS,
            "the uninterrupted run is the whole answer"
        );
        assert_eq!(paged, whole, "resume == uninterrupted run, in order");
    }

    /// A first page says how to get the next one, and the last page does not.
    #[test]
    fn the_footer_names_the_knob_only_while_there_is_more() {
        let serving = serving(220);
        let mut repl = repl(&serving);

        let first = typed(&mut repl, "F where src.File F");
        assert!(first.contains(":more"), "{first}");

        while repl.held.is_some() {
            let page = typed(&mut repl, ":more");
            if repl.held.is_none() {
                assert!(!page.contains(":more"), "the last page invites nothing");
            }
        }

        let after = typed(&mut repl, ":more");
        assert!(after.contains("no result to continue"), "{after}");
    }

    /// **A query that does not compile never leaves the machine**, and what is printed
    /// is the compiler's own diagnostic — code, message and the span it points at.
    #[test]
    fn a_bad_query_is_answered_here_and_the_session_goes_on() {
        let serving = serving(3);
        let mut repl = repl(&serving);

        let refused = typed(&mut repl, "F where src.Nope F");
        assert!(refused.contains("src.Nope"), "{refused}");
        assert!(
            refused.contains("reject/unknown-predicate"),
            "the code, not a paraphrase: {refused}"
        );
        assert!(refused.contains('^'), "and the caret: {refused}");

        let after = typed(&mut repl, "F where src.File F");
        assert_eq!(paths(&after).len(), 3, "and the next query still runs");
    }

    /// `:schema` resolves a name exactly, falls back to a prefix, and says so when
    /// neither answers — and with no argument prints the schema the *server* serves.
    #[test]
    fn schema_resolves_a_name_then_a_prefix() {
        let serving = serving(1);
        let mut repl = repl(&serving);

        let all = typed(&mut repl, ":schema");
        assert!(all.contains("schema src {"), "it is source: {all}");
        assert!(
            all.contains("aperture.db"),
            "including what the server answers itself: {all}"
        );

        let one = typed(&mut repl, ":schema src.File");
        assert!(one.contains("src.File"), "{one}");
        assert!(!one.contains("src.Decl"), "an exact name is not a prefix");

        let namespace = typed(&mut repl, ":schema src.");
        assert!(namespace.contains("src.File") && namespace.contains("src.Decl"));

        let nothing = typed(&mut repl, ":schema nope.");
        assert!(nothing.contains("no predicate matches"), "{nothing}");

        // The psql spelling reaches the same place.
        assert_eq!(typed(&mut repl, "\\d src.File"), one);
    }

    /// **`:plan` and `:type` answer without running anything**, which is what a client
    /// could not do at all until it could ask the server for the schema.
    #[test]
    fn plan_and_type_are_answered_locally() {
        let serving = serving(3);
        let mut repl = repl(&serving);

        let plan = typed(&mut repl, ":plan F where src.File F");
        assert!(plan.contains("src.File"), "{plan}");
        assert!(plan.contains("scan") || plan.contains("seek"), "{plan}");

        let ty = typed(&mut repl, ":type F where src.File F");
        assert!(ty.contains(": str"), "{ty}");

        // Neither ran: the result the pager holds is still nothing.
        assert!(repl.held.is_none());

        // And a bad one is diagnosed rather than sent.
        let bad = typed(&mut repl, ":plan F where src.Nope F");
        assert!(bad.contains("src.Nope"), "{bad}");

        assert!(typed(&mut repl, ":plan").contains("needs a query"));
    }

    /// The page size is a knob, and it takes effect on the next query.
    #[test]
    fn limit_sets_the_page_size() {
        let serving = serving(10);
        let mut repl = repl(&serving);

        assert!(typed(&mut repl, ":limit").contains("40 row(s) per page"));
        assert!(typed(&mut repl, ":limit 3").contains("3 row(s) per page"));
        assert!(typed(&mut repl, ":limit 0").contains("takes a row count"));
        assert!(typed(&mut repl, ":limit lots").contains("takes a row count"));

        let first = typed(&mut repl, "F where src.File F");
        assert_eq!(paths(&first).len(), 3, "three rows, then an invitation");
        assert!(first.contains(":more for the next 3"), "{first}");
    }

    /// **Rows are JSON**, one value per line, and the shape follows the head.
    #[test]
    fn rows_are_json_by_default() {
        let serving = serving(2);
        let mut repl = repl(&serving);

        let rows = typed(&mut repl, "{path = F} where src.File F");

        let objects: Vec<serde_json::Value> = rows
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).expect("valid JSON"))
            .collect();

        assert_eq!(objects.len(), 2, "{rows}");
        assert!(
            objects[0]["path"]
                .as_str()
                .is_some_and(|p| p.ends_with(".py"))
        );

        // And the table is still a command away, for a person reading rather than
        // piping.
        assert!(typed(&mut repl, ":format table").contains("table"));
        let table = typed(&mut repl, "{path = F} where src.File F");
        assert!(table.contains("PATH"), "{table}");

        assert!(typed(&mut repl, ":format sideways").contains("takes jsonl"));
    }

    /// A new query ends the old result rather than leaving `:more` pointed at it.
    #[test]
    fn a_second_query_replaces_the_first_result() {
        let serving = serving(220);
        let mut repl = repl(&serving);

        let _ = typed(&mut repl, "F where src.File F");
        assert!(repl.held.is_some());

        let second = typed(&mut repl, "F where src.File F");
        assert!(second.contains(":more"), "the new result is the held one");

        let cancelled = typed(&mut repl, ":cancel");
        assert!(cancelled.contains("cancelled"), "{cancelled}");
        assert!(repl.held.is_none());
    }

    /// `:connect` to a database that is not there keeps the one that is.
    ///
    /// The old connection is still open and still answering, so losing the session
    /// over a typo would be throwing away a working thing to report a broken one.
    #[test]
    fn connecting_to_a_database_that_is_not_there_keeps_the_session() {
        let serving = serving(3);
        let mut repl = repl(&serving);

        let refused = typed(&mut repl, ":connect nope");
        assert!(!refused.contains("now connected"), "{refused}");

        let after = typed(&mut repl, "F where src.File F");
        assert_eq!(paths(&after).len(), 3, "the old database still answers");
        assert_eq!(repl.database, "code", "and it is still the one named");
    }

    /// **`:list` is a query**, and the row that comes back is this server's own root.
    ///
    /// The point of the test is the last assertion: the same listing is *filterable*,
    /// because it went through a plan rather than a bespoke frame. A `LIST` message
    /// would have answered the first half and would have had to grow a where-clause of
    /// its own for the second.
    #[test]
    fn the_listing_is_a_query_and_can_be_narrowed() {
        let serving = serving(1);
        let mut repl = repl(&serving);

        let listed = typed(&mut repl, ":list");
        assert!(listed.contains("code"), "{listed}");
        assert!(listed.contains("writable"), "{listed}");

        let narrowed = typed(
            &mut repl,
            "N where aperture.db.List {name = N, status = \"complete\"}",
        );
        assert!(
            narrowed.contains("0 row(s)"),
            "nothing is sealed: {narrowed}"
        );
    }

    /// `:facts` is sugar, and it shows the query it is sugar for.
    #[test]
    fn facts_runs_a_scan_and_says_what_it_ran() {
        let serving = serving(2);
        let mut repl = repl(&serving);

        let rows = typed(&mut repl, ":facts src.File");
        assert!(rows.contains("X where src.File X"), "{rows}");
        assert_eq!(paths(&rows).len(), 2, "{rows}");

        assert!(typed(&mut repl, ":facts").contains("needs a predicate"));
    }

    /// `:help` lists every command in the table, so one added without help text is
    /// visible rather than silent.
    #[test]
    fn help_lists_the_table() {
        let serving = serving(1);
        let mut repl = repl(&serving);

        let help = typed(&mut repl, ":help");
        for command in &super::COMMANDS {
            assert!(help.contains(command.name), "{} is missing", command.name);
        }
        assert!(help.contains(":quit"), "and the one that is not in it");
    }

    /// `:quit` is the one thing that stops the loop.
    #[test]
    fn quit_is_the_only_way_the_loop_ends() {
        let serving = serving(1);
        let mut repl = repl(&serving);
        let mut out = Vec::new();

        assert_eq!(
            repl.handle(":timing", &mut out).expect("handled"),
            Control::Continue
        );
        assert_eq!(
            repl.handle(":quit", &mut out).expect("handled"),
            Control::Quit
        );
        assert_eq!(
            repl.handle("\\q", &mut out).expect("handled"),
            Control::Quit
        );
    }

    /// **`:connect` means the database next to this one**, which for a session opened
    /// over TCP is on that server rather than on this machine.
    #[test]
    fn a_sibling_keeps_the_server_it_was_reached_through() {
        use super::{named, sibling};

        assert_eq!(sibling("code", "other"), "other");
        assert_eq!(
            sibling("aperture://box:7000/code", "other"),
            "aperture://box:7000/other"
        );

        assert_eq!(named("code"), "code");
        assert_eq!(named("aperture://box:7000/code"), "code");
    }

    /// An unknown command says so and points at the one that lists them.
    #[test]
    fn an_unknown_command_points_at_help() {
        let serving = serving(1);
        let mut repl = repl(&serving);

        let nope = typed(&mut repl, ":nonesuch");
        assert!(nope.contains("no such command"), "{nope}");
        assert!(nope.contains(":help"), "{nope}");
    }
}
