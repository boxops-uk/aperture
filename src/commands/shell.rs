//! `aperture shell <db>` — the REPL, always over the wire.
//!
//! **Remote-first, and that is the point rather than a limitation**
//! ([operations §5](../../docs/aperture-cli-design.md)). The shell is the permanent
//! exerciser of the wire format: every query a person types here is a real handshake, a
//! real stream and a real page of `DATA_ROW` frames, so a format change that the tests
//! happen not to cover still cannot survive somebody using the tool. `aperture shell`
//! with no database is the *other* shell — [`crate::shell`], Phase 5's embedded demo
//! over a scratch database it seeds itself, which is where `:plan` and `:type` live
//! because a plan is a thing a client never holds.
//!
//! # `\more` is what this was built for
//!
//! A result here is a **bookmark**, not a buffer. `\more` reads the next page and stops;
//! nothing is held at either end, because the place is kept by the *stream* staying open
//! — server-side, parked on a full outbound queue with a bytes-only cursor whose
//! snapshot was released at the chunk boundary ([I8](../../docs/invariants.md#i8)). A
//! pause of a millisecond and a pause of an hour cost the server the same thing.
//!
//! Until this existed, [I4](../../docs/invariants.md#i4) — resume equals an
//! uninterrupted run, the most heavily tested machinery in this project — had **no
//! interactive exerciser at all**: Phase 5's REPL discards the resume token at both of
//! its call sites. `\more` is a person holding a cursor across a round trip, and
//! [`pages_concatenate_to_an_uninterrupted_run`](tests) is that claim as a test.
//!
//! # Errors do not end the session, and which ones do is a rule
//!
//! A [`ClientError::Server`] is the server refusing one *stream* — a query that does not
//! compile, a database that has gone — and psql's answer is right: print it and keep the
//! prompt. Anything else (`Io`, `Wire`, `Protocol`) means the conversation itself is
//! broken, and continuing would be pretending otherwise.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use aperture_client::{ClientError, Connection, Mode, Rows};
use aperture_schema::schema::{PredicateId, Schema};

use crate::{
    CliError,
    cli::RowFormat,
    commands::query::{connect, render_profile},
    rows::Sink,
    shell::{Role, render_predicate_ty},
};

/// Rows delivered per page, and what `\more` continues.
///
/// Deliberately unrelated to the server's `CHUNK_ROWS`: a page is what a person can
/// read, a chunk is what the executor computes between suspends, and tying them
/// together would make a display choice into a protocol one.
const PAGE: usize = 40;

/// Whether the loop should keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continue,
    Quit,
}

/// The result `\more` would continue.
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
    database: String,
    socket: PathBuf,
    held: Option<Held>,
    timing: bool,
    profiling: bool,
    page: usize,
}

impl Repl {
    /// Connect, read-only.
    ///
    /// # Errors
    ///
    /// [`CliError::NoServer`] if nothing is listening, or whatever the handshake says.
    pub fn connect(socket: &Path, database: &str) -> Result<Repl, CliError> {
        // Read-only, for the reason `query` is: a reader has no claim to make about the
        // schema, and the database's is the one that counts.
        let connection = connect(socket, database, Mode::ReadOnly)?;

        Ok(Repl {
            connection,
            database: database.to_owned(),
            socket: socket.to_path_buf(),
            held: None,
            timing: false,
            profiling: false,
            page: PAGE,
        })
    }

    /// What the prompt says, which is which database is answering.
    #[must_use]
    pub fn prompt(&self) -> String {
        format!("{}=> ", self.database)
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

        if let Some(meta) = line.strip_prefix('\\') {
            return self.meta(meta.trim(), out);
        }

        match self.query(line, out) {
            Ok(()) => Ok(Control::Continue),
            Err(error) => refused(error, out).map(|()| Control::Continue),
        }
    }

    fn meta(&mut self, meta: &str, out: &mut impl Write) -> Result<Control, CliError> {
        let (command, argument) = match meta.split_once(char::is_whitespace) {
            Some((command, argument)) => (command, argument.trim()),
            None => (meta, ""),
        };

        match command {
            "q" | "quit" => return Ok(Control::Quit),

            "more" | "m" => match self.query_error(|repl| repl.more(out)) {
                Ok(()) => {}
                Err(error) => refused(error, out)?,
            },

            "cancel" => match self.query_error(Repl::cancel) {
                Ok(Some(sent)) => writeln!(out, "  cancelled after {sent} row(s)")?,
                Ok(None) => writeln!(out, "  nothing to cancel")?,
                Err(error) => refused(error, out)?,
            },

            "timing" => {
                self.timing = !self.timing;
                writeln!(out, "  timing is {}", on_off(self.timing))?;
            }

            "profile" => {
                self.profiling = !self.profiling;
                writeln!(out, "  profile is {}", on_off(self.profiling))?;
                if self.profiling {
                    writeln!(
                        out,
                        "  what the next query examines is reported when it ends"
                    )?;
                }
            }

            "d" => self.describe(argument, out)?,

            "c" | "connect" => {
                if argument.is_empty() {
                    writeln!(out, "  \\c needs a database name")?;
                } else {
                    self.reconnect(argument, out)?;
                }
            }

            "?" | "h" | "help" => help(out)?,

            other => writeln!(
                out,
                "  no such command: \\{other} — \\? lists what there is"
            )?,
        }

        Ok(Control::Continue)
    }

    /// Run a query and show its first page.
    fn query(&mut self, source: &str, out: &mut impl Write) -> Result<(), ClientError> {
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

        let page = self.connection.take(&mut held.rows, self.page)?;

        // A page at a time through the same renderer `query` uses, so the shell cannot
        // drift from the non-interactive tool in how a row reads.
        let mut sink =
            Sink::new(&mut *out, RowFormat::Table, held.rows.desc()).map_err(ClientError::Io)?;
        for row in &page {
            sink.row(row).map_err(ClientError::Io)?;
        }
        let shown = sink.end().map_err(ClientError::Io)?;

        held.delivered += shown;

        let finished = held.rows.finished();
        let delivered = held.delivered;
        let profile = held.rows.profile().cloned();
        let elapsed = started.elapsed();

        if finished {
            // Only now, and only if asked: the tally is not final until the last chunk
            // has run, which is why the server sends it once, just before the end.
            if let Some(profile) = profile.as_ref() {
                write!(out, "{}", render_profile(profile, delivered)).map_err(ClientError::Io)?;
            }
            self.held = None;
        } else {
            writeln!(
                out,
                "  \\more for the next {} — {delivered} so far",
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

    /// `\d` — the schema, or one predicate, or a namespace.
    fn describe(&mut self, name: &str, out: &mut impl Write) -> Result<(), CliError> {
        let schema = Arc::clone(self.connection.schema());

        if name.is_empty() {
            for index in 0..schema.len() {
                writeln!(out, "{}", predicate_line(&schema, index))?;
            }
            return Ok(());
        }

        let exact = (0..schema.len()).find(|index| named(&schema, *index) == Some(name));

        if let Some(index) = exact {
            writeln!(out, "{}", predicate_line(&schema, index))?;
            return Ok(());
        }

        // **Prefix fallback, so `\d src.` dumps a namespace rather than failing.** A
        // name that does not resolve is much more often a namespace someone is
        // exploring than a typo, and psql and Glean's shell both read it that way.
        let matching: Vec<usize> = (0..schema.len())
            .filter(|index| named(&schema, *index).is_some_and(|it| it.starts_with(name)))
            .collect();

        if matching.is_empty() {
            writeln!(out, "  no predicate matches `{name}`")?;
        } else {
            for index in matching {
                writeln!(out, "{}", predicate_line(&schema, index))?;
            }
        }

        Ok(())
    }

    /// `\c` — the same session against another database.
    fn reconnect(&mut self, database: &str, out: &mut impl Write) -> Result<(), CliError> {
        // The old result goes with the old connection, and saying so is better than
        // leaving a `\more` that would silently continue a result from a database the
        // person has stopped looking at.
        let had = self.held.is_some();

        match Repl::connect(&self.socket, database) {
            Ok(fresh) => {
                let (timing, profiling, page) = (self.timing, self.profiling, self.page);
                *self = fresh;
                self.timing = timing;
                self.profiling = profiling;
                self.page = page;

                writeln!(out, "  now connected to `{database}`")?;
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

    /// Run something that may be refused, keeping the borrow checker out of the way.
    fn query_error<T>(
        &mut self,
        f: impl FnOnce(&mut Repl) -> Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        f(self)
    }
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

fn named(schema: &Schema, index: usize) -> Option<&str> {
    schema.get(PredicateId(index as u32))?.name()
}

/// One predicate, as `\d` prints it: name, key, and a value side when there is one.
fn predicate_line(schema: &Schema, index: usize) -> String {
    let Some(predicate) = schema.get(PredicateId(index as u32)) else {
        return String::new();
    };

    let key = render_predicate_ty(predicate.key().ty, schema, schema.interner());
    let value = predicate.value().map_or_else(String::new, |value| {
        format!(
            "{}{}",
            Role::Punctuation.paint(" -> "),
            render_predicate_ty(value.ty, schema, schema.interner())
        )
    });

    format!(
        "  {}{} {key}{value}",
        Role::Predicate.paint(predicate.name().unwrap_or("?")),
        Role::Punctuation.paint(":"),
    )
}

fn help(out: &mut impl Write) -> Result<(), CliError> {
    writeln!(out, "  <query>          run a focus query")?;
    writeln!(out, "  \\more            the next page of the last result")?;
    writeln!(out, "  \\cancel          stop the last result early")?;
    writeln!(
        out,
        "  \\d [name]        the schema, one predicate, or a prefix"
    )?;
    writeln!(out, "  \\c <db>          connect to another database")?;
    writeln!(out, "  \\timing          toggle how long a page took")?;
    writeln!(out, "  \\profile         toggle what a query examined")?;
    writeln!(out, "  \\q               leave (or Ctrl-D)")?;
    Ok(())
}

/// The readline loop.
///
/// Thin on purpose: everything worth testing is in [`Repl::handle`], and what is left
/// here is a terminal.
///
/// # Errors
///
/// Whatever ends the session — a broken connection, or a readline failure that is not
/// an interrupt.
pub fn run(socket: &Path, database: &str) -> Result<(), CliError> {
    use rustyline::{Editor, error::ReadlineError, history::DefaultHistory};

    let mut repl = Repl::connect(socket, database)?;
    let mut editor: Editor<crate::shell::FocusHelper, DefaultHistory> =
        Editor::new().map_err(|error| CliError::Shell(error.to_string()))?;
    editor.set_helper(Some(crate::shell::FocusHelper));

    let stdout = std::io::stdout();

    loop {
        match editor.readline(&repl.prompt()) {
            Ok(line) => {
                let _ = editor.add_history_entry(&line);

                let mut out = stdout.lock();
                if repl.handle(&line, &mut out)? == Control::Quit {
                    return Ok(());
                }
            }

            // Ctrl-C abandons the line, as it does everywhere; Ctrl-D leaves. A
            // held result goes with the session either way, and the server notices
            // the socket closing.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return Ok(()),
            Err(error) => return Err(CliError::Shell(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, thread};

    use aperture_server::{Registry, server::Listener};
    use aperture_store::catalog::Catalog;
    use aperture_wire::{WireFact, WireValue, protocol::provisional_fingerprint};

    use super::{Control, Repl};
    use crate::code_index;

    /// A server on a socket, in this process, with `count` files in it.
    struct Serving {
        _dir: tempfile::TempDir,
        socket: PathBuf,
    }

    fn serving(count: usize) -> Serving {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let socket = dir.path().join("aperture.sock");

        let schema = code_index::schema();
        let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
        catalog
            .create("code", &schema, provisional_fingerprint(&schema))
            .expect("a database");

        let (registry, _listing) = Registry::open(catalog, schema).expect("a registry");
        let listener = Listener::bind(&socket).expect("a socket");

        thread::spawn(move || {
            let _ = listener.run_blocking(Arc::new(registry));
        });

        let serving = Serving {
            _dir: dir,
            socket: socket.clone(),
        };

        seed(&serving, count);
        serving
    }

    fn seed(serving: &Serving, count: usize) {
        let mut writer = aperture_client::Connection::connect(
            &serving.socket,
            "code",
            Arc::new(code_index::schema()),
            aperture_client::Mode::ReadWrite,
            true,
        )
        .expect("a writer");

        let facts: Vec<WireFact> = (0..count)
            .map(|n| WireFact {
                predicate: code_index::FILE,
                key: WireValue::Str(format!("f{n:05}.py")),
                value: None,
            })
            .collect();

        writer.write(code_index::FILE, &facts).expect("written");
    }

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
    /// `\more` holds a bytes-only cursor across a round trip and resumes it, which is
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
                all.extend(paths(&typed(&mut repl, "\\more")));
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
        assert!(first.contains("\\more"), "{first}");

        while repl.held.is_some() {
            let page = typed(&mut repl, "\\more");
            if repl.held.is_none() {
                assert!(!page.contains("\\more"), "the last page invites nothing");
            }
        }

        let after = typed(&mut repl, "\\more");
        assert!(after.contains("no result to continue"), "{after}");
    }

    /// A query that does not compile is the server refusing one stream, and the
    /// session goes on answering.
    #[test]
    fn a_bad_query_does_not_end_the_session() {
        let serving = serving(3);
        let mut repl = repl(&serving);

        let refused = typed(&mut repl, "this is not focus");
        assert!(!refused.is_empty(), "it says something");

        let after = typed(&mut repl, "F where src.File F");
        assert_eq!(paths(&after).len(), 3, "and the next query still runs");
    }

    /// `\d` resolves a name exactly, falls back to a prefix, and says so when neither
    /// answers.
    #[test]
    fn describe_resolves_a_name_then_a_prefix() {
        let serving = serving(1);
        let mut repl = repl(&serving);

        let one = typed(&mut repl, "\\d src.File");
        assert!(one.contains("src.File"), "{one}");
        assert!(!one.contains("src.Decl"), "an exact name is not a prefix");

        let namespace = typed(&mut repl, "\\d src.");
        assert!(namespace.contains("src.File") && namespace.contains("src.Decl"));

        let nothing = typed(&mut repl, "\\d nope.");
        assert!(nothing.contains("no predicate matches"), "{nothing}");
    }

    /// A new query ends the old result rather than leaving `\more` pointed at it.
    #[test]
    fn a_second_query_replaces_the_first_result() {
        let serving = serving(220);
        let mut repl = repl(&serving);

        let _ = typed(&mut repl, "F where src.File F");
        assert!(repl.held.is_some());

        let second = typed(&mut repl, "F where src.File F");
        assert!(second.contains("\\more"), "the new result is the held one");

        let cancelled = typed(&mut repl, "\\cancel");
        assert!(cancelled.contains("cancelled"), "{cancelled}");
        assert!(repl.held.is_none());
    }

    /// `\c` to a database that is not there keeps the one that is.
    ///
    /// The old connection is still open and still answering, so losing the session
    /// over a typo would be throwing away a working thing to report a broken one.
    #[test]
    fn connecting_to_a_database_that_is_not_there_keeps_the_session() {
        let serving = serving(3);
        let mut repl = repl(&serving);

        let refused = typed(&mut repl, "\\c nope");
        assert!(!refused.contains("now connected"), "{refused}");

        let after = typed(&mut repl, "F where src.File F");
        assert_eq!(paths(&after).len(), 3, "the old database still answers");
        assert_eq!(repl.database, "code", "and it is still the one named");
    }

    /// `\q` is the one thing that stops the loop.
    #[test]
    fn quit_is_the_only_way_the_loop_ends() {
        let serving = serving(1);
        let mut repl = repl(&serving);
        let mut out = Vec::new();

        assert_eq!(
            repl.handle("\\timing", &mut out).expect("handled"),
            Control::Continue
        );
        assert_eq!(
            repl.handle("\\q", &mut out).expect("handled"),
            Control::Quit
        );
    }
}
