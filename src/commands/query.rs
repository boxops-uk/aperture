//! `aperture query <db> <QUERY>`.
//!
//! **Always over the wire**, and that is §2's rule 1 rather than a simplification: a
//! bare name means "ask the local server", and there is no silent fallback to opening
//! the directory because a server may be holding it (`ops-I1`). With none listening
//! the answer is a psql-style actionable error, not a directory read.
//!
//! Rows are **streamed**: pulled one at a time and written as they arrive, so a result
//! of any size crosses this process without being held in it. The one exception is the
//! aligned table, which cannot know its column widths until the last row — see
//! [`crate::rows`], and use `--format raw` or `--format count` when that matters.

use std::{path::Path, sync::Arc, time::Instant};

use aperture_client::{ClientError, Connection, Expander, Mode};

use crate::{CliError, cli::RowFormat, code_index, rows::Sink};

/// §2's remote address form: `aperture://host:port/database`.
pub(crate) const ADDRESS_SCHEME: &str = "aperture://";

/// Why a query stopped before the server said it was done.
///
/// One enum rather than three flags, because the three are mutually exclusive and the
/// message a person needs is different for each: a `--limit` names a knob to raise, a
/// timeout names one to extend, and an interrupt names nothing at all — they asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The server ran out of rows, which is the only way a query *completes*.
    No,
    Limit,
    Timeout,
    Interrupt,
}

/// What a query came to.
#[derive(Debug)]
pub struct Summary {
    pub rows: u64,
    pub elapsed: std::time::Duration,
    /// Whether anything cut it short, and what.
    pub stopped: Stopped,
    /// What the server said it examined, when asked.
    pub profile: Option<aperture_client::QueryProfile>,
    /// Point reads spent expanding references — distinct ids, so the difference between
    /// this and the references printed is what the cache saved. Zero when not expanding.
    pub fetched: u64,
    /// References that named no fact. Not an absence but a **damaged database**, since
    /// both column families are written together ([I12](../../docs/invariants.md#i12))
    /// and ids are never reused ([I11](../../docs/invariants.md#i11)) — so it is reported
    /// rather than left looking like a field somebody chose not to expand.
    pub unresolved: u64,
    /// What expansion could not do, in words — a predicate this server will not resolve
    /// an id of, because it answers that predicate rather than storing it. One line per
    /// predicate, not per row.
    pub notices: Vec<String>,
}

/// How rows are to be shown.
///
/// Two display choices travelling together, because they are decided in the same place
/// and neither is a question about the *query*: one is the shape a row prints in, the
/// other how far a reference in it is followed.
#[derive(Debug, Clone, Copy)]
pub struct Rendering {
    pub format: RowFormat,
    /// Hops to follow a reference. Zero prints ids, which is what a row carries.
    pub expand: usize,
}

impl Rendering {
    /// One shape, references left as the ids a row carries.
    ///
    /// Test-only: every real caller has an `--expand` to pass, and a default that meant
    /// "off" would be a second place for that decision to live.
    #[cfg(test)]
    #[must_use]
    pub fn plain(format: RowFormat) -> Rendering {
        Rendering { format, expand: 0 }
    }
}

/// What the caller is prepared to wait for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub rows: Option<u64>,
    pub timeout: Option<std::time::Duration>,
}

/// # Errors
///
/// [`CliError::NoServer`] if nothing is listening, [`CliError::Client`] if the server
/// refuses the session or the query does not compile — carrying the compiler's own
/// diagnostics.
/// Answer **how many rows**, and none of them.
///
/// A different entry point rather than a flag on [`run`], because nothing after the
/// first line is shared: there is no descriptor, no sink, no paging and no cancel
/// loop — the server counts and sends a number.
///
/// # Errors
///
/// As [`run`].
pub fn count(socket: &Path, name: &str, query: &str) -> Result<u64, CliError> {
    let mut connection = connect(socket, name, Mode::ReadOnly)?;
    Ok(connection.count(query)?)
}

pub fn run(
    socket: &Path,
    name: &str,
    query: &str,
    rendering: Rendering,
    limits: Limits,
    profile: bool,
    interrupted: &std::sync::atomic::AtomicBool,
) -> Result<Summary, CliError> {
    // **Read-only, and asserting nothing.** A reader has no claim to make about the
    // schema: the database's is the one that matters, it is frozen at create
    // ([I13](../../docs/invariants.md#i13)), and a tool that refused to *read* a
    // database because its own built-in copy had moved on would be refusing the one
    // thing that still works.
    let mut connection = connect(socket, name, Mode::ReadOnly)?;

    // **Asked for only when expanding**, and it is the *served* schema rather than this
    // tool's built-in one. A fetch reply is schema-driven — each key encoded against its
    // own predicate's key type, with no descriptor to carry the shape — so it can only be
    // read against the schema the database was created with ([I13]). It also names the
    // fields of an expanded reference on the way out.
    //
    // [I13]: ../../docs/invariants.md#i13
    let schema = if rendering.expand > 0 {
        Some(Arc::new(connection.served_schema()?))
    } else {
        None
    };

    let started = Instant::now();
    let opened = if profile {
        connection.query_profiled(query)
    } else {
        connection.query(query)
    };

    let mut result = match opened {
        Ok(result) => result,
        Err(refusal) => return Err(diagnosed(&mut connection, query, refusal)),
    };

    let stdout = std::io::stdout();
    let mut sink = match &schema {
        Some(schema) => Sink::naming(
            stdout.lock(),
            rendering.format,
            result.desc(),
            Arc::clone(schema),
        ),
        None => Sink::new(stdout.lock(), rendering.format, result.desc()),
    }?;

    let mut expander = schema.map(Expander::new);
    let mut stopped = Stopped::No;

    loop {
        // **Three ways to stop, and all of them cancel in band.** The server completes
        // the stream with what it sent, the connection stays usable, and the rows
        // already in flight are drained rather than left in the socket for the next
        // stream to trip over. A `--limit` is not a `LIMIT`, and neither of the others
        // is a promise about the server: each is a bound on what this command waits
        // for, and the cancel lands between rows.
        let reason = if limits.rows.is_some_and(|limit| result.seen() >= limit) {
            Some(Stopped::Limit)
        } else if limits
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            Some(Stopped::Timeout)
        } else if interrupted.load(std::sync::atomic::Ordering::Relaxed) {
            Some(Stopped::Interrupt)
        } else {
            None
        };

        if let Some(reason) = reason {
            connection.cancel(&mut result)?;
            stopped = reason;
            break;
        }

        match connection.next_row(&mut result)? {
            Some(row) => {
                // A row at a time, on the way to the sink, so the whole result is still
                // streamed: expansion adds reads per row, never a buffer.
                match &mut expander {
                    Some(expander) => {
                        let expanded = expander.expand(&mut connection, &row, rendering.expand)?;
                        sink.row(&expanded)?;
                    }
                    None => sink.row(&row)?,
                }
            }
            None => break,
        }
    }

    let rows = sink.end()?;

    Ok(Summary {
        rows,
        elapsed: started.elapsed(),
        stopped,
        fetched: expander.as_ref().map_or(0, Expander::fetched),
        unresolved: expander.as_ref().map_or(0, Expander::unresolved),
        // Whatever expansion could not do — a predicate the server answers rather than
        // stores, say. Carried out rather than printed here, because this function writes
        // *rows* to stdout and a notice is not one.
        notices: expander
            .as_mut()
            .map(Expander::take_notices)
            .unwrap_or_default(),
        // Absent after any of the three early stops, and that is honest rather than a
        // gap: the server reports what it examined when the query *ends*, and a
        // cancelled one ended early — a tally taken then would describe a different
        // query than the one asked.
        profile: result.profile().cloned(),
    })
}

/// A refusal the *compiler* can put a caret under, rendered here rather than read as a
/// sentence.
///
/// The server sends its diagnostics as text, which is right — it is the thing that
/// knows what happened, and paraphrasing would be one more place for two answers to
/// drift. What it cannot know is whether this end has a terminal, so what comes back is
/// plain. Given the schema the server serves (one request, on the failure path only,
/// where a round trip costs nothing), the same query compiles here and the same
/// diagnostics render **in colour, under a caret**.
///
/// Anything that is not the query's fault, or any schema this cannot fetch, falls
/// straight through as the server's own words.
fn diagnosed(connection: &mut Connection, query: &str, refusal: ClientError) -> CliError {
    use aperture_engine::compile::Compilation;

    if refusal.code() != Some(aperture_client::ErrorCode::BadQuery) {
        return refusal.into();
    }

    let Ok(schema) = connection.served_schema() else {
        return refusal.into();
    };

    let mut compilation = Compilation::new(query, &schema);
    let _ = compilation.plan();

    if !compilation.diagnostics().has_errors() {
        // The server refused something this compiler accepts, which is a disagreement
        // worth seeing as the server stated it rather than as a local guess.
        return refusal.into();
    }

    let mut rendered = Vec::new();
    let config = codespan_reporting::term::Config::default();

    if crate::prompt::colours_enabled_on_stderr() {
        let _ = compilation.render(
            &mut codespan_reporting::term::termcolor::Ansi::new(&mut rendered),
            &config,
        );
    } else {
        let _ = compilation.render(
            &mut codespan_reporting::term::termcolor::NoColor::new(&mut rendered),
            &config,
        );
    }

    match String::from_utf8(rendered) {
        Ok(text) => CliError::Diagnosed(text),
        Err(_) => refusal.into(),
    }
}

/// The profile, as a person reads it.
///
/// `(full scan)` is the line worth having: it is the one that names something to go
/// and fix, and Glean prints it for the same reason.
#[must_use]
pub fn render_profile(profile: &aperture_client::QueryProfile, rows: u64) -> String {
    let steps: Vec<Vec<String>> = profile
        .steps
        .iter()
        .map(|step| {
            vec![
                step.label.clone(),
                step.examined.to_string(),
                if step.full_scan { "full scan" } else { "" }.to_owned(),
            ]
        })
        .collect();

    let examined = profile.examined();
    let mut out = crate::output::table(&["step", "examined", ""], &steps);

    // The ratio is the whole point of the table: a query that read a hundred thousand
    // rows to answer with three has a plan problem, and no per-step number says that
    // as plainly as the two totals side by side.
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!("{examined} examined, {rows} produced\n"),
    );

    out
}

/// Connect, turning "nothing is listening" into the error §2 asks for.
///
/// **§2's address resolution for readers, stated once.** A bare name means the local
/// server on the derived socket; `aperture://host:port/db` means that server, over TCP.
/// Both `query` and `shell` come through here, so neither can invent its own rule — and
/// there is still no silent fallback to opening a directory, because a server may be
/// holding it (`ops-I1`).
pub(crate) fn connect(socket: &Path, database: &str, mode: Mode) -> Result<Connection, CliError> {
    use std::io::ErrorKind;

    let opened = match database.strip_prefix(ADDRESS_SCHEME) {
        Some(rest) => {
            let (authority, name) = rest.split_once('/').ok_or_else(|| CliError::Address {
                address: database.to_owned(),
            })?;

            if authority.is_empty() || name.is_empty() {
                return Err(CliError::Address {
                    address: database.to_owned(),
                });
            }

            Connection::connect_tcp(authority, name, Arc::new(code_index::schema()), mode, false)
        }

        None => Connection::connect(
            socket,
            database,
            Arc::new(code_index::schema()),
            mode,
            false,
        ),
    };

    match opened {
        Ok(connection) => Ok(connection),

        Err(ClientError::Io(error))
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Err(CliError::NoServer {
                socket: socket.to_path_buf(),
            })
        }

        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicBool, time::Duration};

    use super::{Limits, Rendering, Stopped, run};
    use crate::{cli::RowFormat, testing::serving};

    const FILES: usize = 600;

    /// Run the query command against a seeded server, counting what came out.
    fn query(
        serving: &crate::testing::Serving,
        limits: Limits,
        interrupted: &AtomicBool,
    ) -> super::Summary {
        run(
            &serving.socket,
            "code",
            "F where src.File F",
            // Counted rather than rendered: what these tests are about is *how many*
            // rows crossed the socket before the cancel landed, and a table would put
            // six hundred lines through the harness to say it.
            Rendering::plain(RowFormat::Count),
            limits,
            false,
            interrupted,
        )
        .expect("the query runs")
    }

    /// Nothing asked to stop it, so it ends the only way a query completes.
    #[test]
    fn an_unbounded_query_reports_that_nothing_stopped_it() {
        let serving = serving(FILES);
        let quiet = AtomicBool::new(false);

        let summary = query(&serving, Limits::default(), &quiet);

        assert_eq!(summary.rows, FILES as u64);
        assert_eq!(summary.stopped, Stopped::No);
    }

    /// `--limit` stops at the row, and says which knob did it.
    ///
    /// The count is exact rather than approximate because the limit is checked before
    /// each row is asked for: a `--limit` is the client's bound on what it reads, not
    /// a `LIMIT` the server was told about.
    #[test]
    fn a_limit_stops_at_the_row_it_names() {
        let serving = serving(FILES);
        let quiet = AtomicBool::new(false);

        let summary = query(
            &serving,
            Limits {
                rows: Some(37),
                timeout: None,
            },
            &quiet,
        );

        assert_eq!(summary.rows, 37);
        assert_eq!(summary.stopped, Stopped::Limit);
    }

    /// A deadline already past stops it before the first row.
    ///
    /// Zero rather than "something small": a timeout that has to *elapse* is a test
    /// that fails on a slow machine and passes on a fast one, and what is being tested
    /// is the wiring — that the deadline is checked, that the cancel goes in band, and
    /// that the reason survives to the caller.
    #[test]
    fn a_deadline_already_past_stops_before_the_first_row() {
        let serving = serving(FILES);
        let quiet = AtomicBool::new(false);

        let summary = query(
            &serving,
            Limits {
                rows: None,
                timeout: Some(Duration::ZERO),
            },
            &quiet,
        );

        assert_eq!(summary.rows, 0);
        assert_eq!(summary.stopped, Stopped::Timeout);
    }

    /// An interrupt cancels the stream, and the connection is still usable afterwards.
    ///
    /// The second query is the point. Ctrl-C is a *stream* cancel rather than a
    /// connection teardown, and the only way to say that is to keep using the thing
    /// afterwards — which here means a second query on a second connection reaching a
    /// server that was never left holding a half-answered stream.
    #[test]
    fn an_interrupt_cancels_the_stream_and_leaves_the_server_working() {
        let serving = serving(FILES);
        let pressed = AtomicBool::new(true);

        let summary = query(&serving, Limits::default(), &pressed);

        assert_eq!(summary.rows, 0, "it was already pressed");
        assert_eq!(summary.stopped, Stopped::Interrupt);

        let quiet = AtomicBool::new(false);
        let after = query(&serving, Limits::default(), &quiet);
        assert_eq!(after.rows, FILES as u64, "the server is still answering");
        assert_eq!(after.stopped, Stopped::No);
    }

    /// A stop of any kind means no profile, because the tally would describe a
    /// different query than the one asked.
    ///
    /// **The query is a three-way cross join, and that is what makes this
    /// deterministic.** The first version asked for a plain scan of 600 files and was
    /// racy under a loaded machine: a cancel is only *observed* if it reaches the
    /// server's reader before the query finishes, and 600 rows can finish first — after
    /// which a profile is correct and the assertion below is simply wrong. 600³ is
    /// 216 million rows, so completion is not a thing that can happen while a test
    /// waits, and the cancel wins by construction rather than by luck.
    #[test]
    fn a_cancelled_query_reports_no_profile() {
        let serving = serving(FILES);
        let quiet = AtomicBool::new(false);

        let summary = run(
            &serving.socket,
            "code",
            "X where src.File X; src.File _; src.File _",
            Rendering::plain(RowFormat::Count),
            Limits {
                rows: Some(5),
                timeout: None,
            },
            true,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(summary.stopped, Stopped::Limit);
        assert_eq!(summary.rows, 5, "it stopped where it was told to");
        assert!(
            summary.profile.is_none(),
            "a cancelled tally is not this query's"
        );

        // And the same query, allowed to finish, does report one — otherwise the
        // assertion above would hold for a build that never sent a profile at all.
        let whole = run(
            &serving.socket,
            "code",
            "F where src.File F",
            Rendering::plain(RowFormat::Count),
            Limits::default(),
            true,
            &quiet,
        )
        .expect("the query runs");

        assert!(whole.profile.is_some(), "an uncancelled one does");
    }
}

#[cfg(test)]
mod catalogue {
    use std::sync::atomic::AtomicBool;

    use super::{Limits, Rendering, Stopped, run};
    use crate::{cli::RowFormat, testing::serving};

    /// Run a query against the seeded server and capture what it printed.
    ///
    /// Through the real command, because what is being tested is that a virtual
    /// predicate is *ordinary*: the same connect, the same compile on the server, the
    /// same DESC and DATA_ROW frames, the same renderer.
    fn ask(serving: &crate::testing::Serving, query: &str) -> (super::Summary, String) {
        // `run` writes rows to stdout, which a test cannot capture per-thread — so the
        // count is what is asserted on, and the shell's own tests cover the rendering.
        let quiet = AtomicBool::new(false);
        let summary = run(
            &serving.socket,
            "code",
            query,
            Rendering::plain(RowFormat::Count),
            Limits::default(),
            false,
            &quiet,
        )
        .expect("the query runs");

        (summary, query.to_owned())
    }

    /// **The catalogue answers as a predicate, not as a special case.**
    ///
    /// One database exists, so one row comes back — over the ordinary query path, with
    /// no control message anywhere in it.
    #[test]
    fn the_catalogue_is_queryable() {
        let serving = serving(0);

        let (summary, _) = ask(&serving, "N where aperture.db.List {name = N}");

        assert_eq!(summary.rows, 1, "the one database this server holds");
        assert_eq!(summary.stopped, Stopped::No);
    }

    /// **The point of putting it behind the store seam**: it filters, and the filter is
    /// the plan's rather than the client's.
    ///
    /// `writable` matches and `complete` does not, which is what says the row's own
    /// bytes were read and compared rather than a listing being handed over whole.
    #[test]
    fn the_catalogue_filters_like_any_other_predicate() {
        let serving = serving(0);

        let (writable, _) = ask(
            &serving,
            "N where aperture.db.List {name = N, status = \"writable\"}",
        );
        assert_eq!(writable.rows, 1, "created, and not yet sealed");

        let (complete, _) = ask(
            &serving,
            "N where aperture.db.List {name = N, status = \"complete\"}",
        );
        assert_eq!(complete.rows, 0, "nothing has been sealed");
    }

    /// A name that leads the key **seeks**, and the profile is what says so.
    ///
    /// This is the claim a bespoke `LIST` frame could not make: the listing is read
    /// through a plan, so narrowing it costs what narrowing a keyspace costs, and
    /// `--profile` reports it in the same table as everything else.
    #[test]
    fn a_lookup_by_name_narrows_rather_than_scanning() {
        let serving = serving(0);
        let quiet = AtomicBool::new(false);

        let summary = run(
            &serving.socket,
            "code",
            "I where aperture.db.List {name = \"code\", instance = I}",
            Rendering::plain(RowFormat::Count),
            Limits::default(),
            true,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(summary.rows, 1);

        let profile = summary
            .profile
            .expect("a profile, the query having finished");
        assert!(
            profile.steps.iter().any(|step| !step.full_scan),
            "a leading-field lookup should not be a full scan: {:?}",
            profile.steps
        );
    }

    /// A query that never mentions the catalogue does not pay for one.
    ///
    /// Not a performance nicety: building it walks the store root and reads a sidecar
    /// per database, and doing that on every query about `src.File` would make every
    /// read cost a directory listing. The assertion is indirect — the query answers —
    /// but the guard it protects is the `reads` check, and deleting that check makes
    /// this test slower rather than red, so the comment is the guard's real home.
    #[test]
    fn an_ordinary_query_still_answers_with_the_catalogue_declared() {
        let serving = serving(4);

        let (summary, _) = ask(&serving, "F where src.File F");

        assert_eq!(summary.rows, 4);
    }
}

#[cfg(test)]
mod over_tcp {
    use std::sync::atomic::AtomicBool;

    use super::{Limits, Rendering, Stopped, run};
    use crate::{CliError, cli::RowFormat, testing::serving_on_tcp};

    /// **The same protocol, over a different pipe** — which is the whole claim
    /// `--listen-tcp` makes, and the reason the client is one enum rather than a second
    /// implementation.
    ///
    /// Asked through both doors of one server, so a difference would be the transport's
    /// and could be nothing else.
    #[test]
    fn the_same_question_answers_the_same_over_either_door() {
        let (serving, address) = serving_on_tcp(9);
        let quiet = AtomicBool::new(false);

        let ask = |name: &str| {
            run(
                &serving.socket,
                name,
                "F where src.File F",
                Rendering::plain(RowFormat::Count),
                Limits::default(),
                false,
                &quiet,
            )
            .expect("the query runs")
        };

        let unix = ask("code");
        let tcp = ask(&format!("aperture://{address}/code"));

        assert_eq!(unix.rows, 9);
        assert_eq!(
            tcp.rows, unix.rows,
            "the transport is not part of the answer"
        );
        assert_eq!(tcp.stopped, Stopped::No);
    }

    /// An address that is not one is refused by shape, not by whatever failed to
    /// resolve.
    #[test]
    fn a_malformed_address_says_what_an_address_looks_like() {
        let (serving, _address) = serving_on_tcp(0);
        let quiet = AtomicBool::new(false);

        for bad in ["aperture://", "aperture://host:1234", "aperture:///code"] {
            let error = run(
                &serving.socket,
                bad,
                "F where src.File F",
                Rendering::plain(RowFormat::Count),
                Limits::default(),
                false,
                &quiet,
            )
            .expect_err("it cannot connect to that");

            assert!(
                matches!(error, CliError::Address { .. }),
                "`{bad}` should be refused as an address: {error}"
            );
        }
    }
}

#[cfg(test)]
mod mixed {
    use std::sync::atomic::AtomicBool;

    use super::{Limits, Rendering, Stopped, run};
    use crate::{cli::RowFormat, testing::serving};

    /// Rows a query answers with, counted server-side.
    fn count(serving: &crate::testing::Serving, query: &str) -> u64 {
        let quiet = AtomicBool::new(false);

        run(
            &serving.socket,
            "code",
            query,
            Rendering::plain(RowFormat::Count),
            Limits::default(),
            false,
            &quiet,
        )
        .expect("the query runs")
        .rows
    }

    /// **One plan, two stores.** A level whose rows come from the registry and a level
    /// whose rows come from fjall, in the same query.
    ///
    /// This is what `Catalogued` actually claims: it dispatches on the predicate each
    /// scan names, so being wrapped costs a stored predicate nothing and a virtual one
    /// no special handling. A single-predicate test cannot see that — it would pass just
    /// as well against a wrapper that answered *everything* from memory, or one that
    /// only worked when the catalogue was the outermost level.
    #[test]
    fn a_query_joins_the_catalogue_to_a_stored_predicate() {
        const FILES: usize = 20;
        let serving = serving(FILES);

        // No shared variable, so this is a cross product: one database × twenty files.
        // Contrived as a question, and exactly right as an exercise — the two levels
        // have to interleave, which means the outer one is re-entered and the inner one
        // re-opened per outer row.
        let rows = count(
            &serving,
            "{db = D.name, file = F} where D = aperture.db.List _; src.File F",
        );
        assert_eq!(rows, FILES as u64, "one database × {FILES} files");
    }

    /// The same join with the levels written the other way round.
    ///
    /// `reorder` chooses the loop order, so the written order is not the executed one —
    /// but the *store* is asked in whichever order it picks, and a wrapper that only
    /// handled the catalogue at one nesting depth would answer one of these two and not
    /// the other.
    #[test]
    fn the_join_answers_the_same_written_either_way() {
        const FILES: usize = 20;
        let serving = serving(FILES);

        let catalogue_first = count(
            &serving,
            "{db = D.name, file = F} where D = aperture.db.List _; src.File F",
        );
        let files_first = count(
            &serving,
            "{db = D.name, file = F} where src.File F; D = aperture.db.List _",
        );

        assert_eq!(catalogue_first, files_first);
        assert_eq!(catalogue_first, FILES as u64);
    }

    /// **A negation over the catalogue**, which is the arm that decides whether the
    /// listing gets built at all.
    ///
    /// The listing is materialised only when the plan is found to read it, and a
    /// negation is a `Step::Test` rather than a level — a separate branch of that check.
    /// If it were missed, the catalogue would not be built, the query would look for a
    /// keyspace that does not exist, and the failure would be a fault rather than a
    /// wrong answer. Both polarities are asserted because only the pair distinguishes
    /// "the negation ran" from "the negation matched nothing, whatever it looked at".
    #[test]
    fn a_negation_over_the_catalogue_is_answered() {
        const FILES: usize = 5;
        let serving = serving(FILES);

        let absent = count(
            &serving,
            "F where src.File F; !aperture.db.List {name = \"no-such-database\"}",
        );
        assert_eq!(
            absent, FILES as u64,
            "nothing is named that, so every row passes"
        );

        let present = count(
            &serving,
            "F where src.File F; !aperture.db.List {name = \"code\"}",
        );
        assert_eq!(
            present, 0,
            "`code` is there, so the negation fails every row"
        );
    }

    /// **A fetch beside the catalogue** — the path that makes the wrapped store answer a
    /// *point read* rather than a scan.
    ///
    /// `D.module.name` reads **through** a reference: one `point()` per row of the level
    /// above, which is the second of `FactStore`'s two methods and the one no other test
    /// here reaches — a file is a bare string, so a query over files alone never asks for
    /// one. `Catalogued::point` has to delegate an id belonging to a stored predicate,
    /// and the failure if it did not would be a fetch answering with catalogue bytes:
    /// not an error, a wrong name.
    #[test]
    fn a_fetch_through_a_reference_works_beside_the_catalogue() {
        const FILES: usize = 6;
        let serving = serving(FILES);

        // The fetch alone, first, so the count below has something to be compared with.
        let alone = count(
            &serving,
            "{d = D.name, m = D.module.name} where D = src.Decl _",
        );
        assert_eq!(alone, FILES as u64);

        let beside = count(
            &serving,
            "{db = C.name, d = D.name, m = D.module.name} \
             where C = aperture.db.List _; D = src.Decl _",
        );
        assert_eq!(
            beside, FILES as u64,
            "one database × {FILES} declarations, each one fetching its module"
        );
    }

    /// **Paging across a mixed plan**, which is [I4](../../docs/invariants.md#i4) over a
    /// cursor whose levels came from two different row sources.
    ///
    /// The corpus is sized to cross the server's 256-row chunk several times, so the
    /// result really is suspended and resumed rather than answered in one go — and the
    /// comparison is against the same query taken whole, because a count would pass for
    /// a resume that dropped one row and repeated another.
    #[test]
    fn a_mixed_plan_pages_to_the_same_answer() {
        const FILES: usize = 1000;
        const SERVER_CHUNK: usize = 256;
        const _: () = assert!(FILES > 3 * SERVER_CHUNK);

        let serving = serving(FILES);
        let query = "{db = D.name, file = F} where D = aperture.db.List _; src.File F";

        let whole = count(&serving, query);
        assert_eq!(whole, FILES as u64);

        // The same query, stopped early and resumed by the ordinary paging path: what
        // `--limit` proves here is that the cursor is honoured mid-result over a plan
        // one of whose levels is not backed by a keyspace at all.
        let quiet = AtomicBool::new(false);
        let paged = run(
            &serving.socket,
            "code",
            query,
            Rendering::plain(RowFormat::Count),
            Limits {
                rows: Some(600),
                timeout: None,
            },
            false,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(paged.rows, 600, "it stopped where it was told");
        assert_eq!(paged.stopped, Stopped::Limit);
    }

    /// A field of the catalogue used as an ordinary value: read, projected, and
    /// compared against a stored row's.
    ///
    /// The point is that nothing downstream of the store knows the difference — the
    /// residual comparing these two strings is the same `ResidualOp` it would be over
    /// two stored predicates, decoding bytes that came from a `Vec`.
    #[test]
    fn a_catalogue_field_narrows_a_query_that_also_reads_a_keyspace() {
        const FILES: usize = 4;
        let serving = serving(FILES);

        // A constant on the catalogue's **leading** key field, so it folds into a seek,
        // beside a scan of a real keyspace. Both levels are asked of the same wrapped
        // store, and the answer is a non-zero count — which is what makes the negative
        // case below evidence rather than an absence.
        let matched = count(
            &serving,
            "{db = N, file = F} where aperture.db.List {name = N}; src.File F; N = \"code\"",
        );
        assert_eq!(matched, FILES as u64, "the one database × every file");

        // The same query with a name no database has. Zero, and it means something
        // *because* the identical shape above is four: a store that had stopped serving
        // either side would answer zero to both, and the pair separates them.
        let unmatched = count(
            &serving,
            "{db = N, file = F} where aperture.db.List {name = N}; src.File F; N = \"nope\"",
        );
        assert_eq!(unmatched, 0, "no database is called `nope`");
    }
}

#[cfg(test)]
mod surface {
    use std::sync::atomic::AtomicBool;

    use super::{Limits, Rendering, run};
    use crate::{
        cli::RowFormat,
        testing::{create_database, serving},
    };

    const FILES: usize = 6;

    fn count(serving: &crate::testing::Serving, query: &str) -> u64 {
        let quiet = AtomicBool::new(false);

        run(
            &serving.socket,
            "code",
            query,
            Rendering::plain(RowFormat::Count),
            Limits::default(),
            false,
            &quiet,
        )
        .expect("the query runs")
        .rows
    }

    /// **`--expand` resolves the references in a result, and changes nothing else.**
    ///
    /// What is checked here is the *cost and the shape of the run*, since `run` writes to
    /// stdout: the same query answers the same rows, and expansion shows up as point
    /// reads. That it renders the fact rather than the id is
    /// [`crate::rows`]' test, and that a person sees it is the shell's.
    ///
    /// The corpus is `declaration → module → file`, so each row holds one reference and
    /// each module holds one more: **two** distinct ids per declaration, and one file per
    /// module. Six declarations therefore cost twelve reads and not one more — which is
    /// the cache being asserted, since every row is read at two levels and none twice.
    #[test]
    fn expanding_resolves_every_reference_once() {
        let serving = serving(FILES);
        let quiet = AtomicBool::new(false);

        let expanded = run(
            &serving.socket,
            "code",
            "D where src.Decl D",
            Rendering {
                format: RowFormat::Count,
                expand: aperture_client::FULL_DEPTH,
            },
            Limits::default(),
            false,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(expanded.rows, FILES as u64, "the same rows either way");
        assert_eq!(
            expanded.fetched,
            (FILES * 2) as u64,
            "one module and one file per declaration, each read once"
        );
        assert_eq!(
            expanded.unresolved, 0,
            "every reference in a database this tool wrote resolves"
        );

        // Off, nothing is read at all — the flag is the whole difference.
        let plain = run(
            &serving.socket,
            "code",
            "D where src.Decl D",
            Rendering::plain(RowFormat::Count),
            Limits::default(),
            false,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(plain.rows, FILES as u64);
        assert_eq!(plain.fetched, 0, "no expansion, no point reads");

        // One hop reads the modules and not their files.
        let shallow = run(
            &serving.socket,
            "code",
            "D where src.Decl D",
            Rendering {
                format: RowFormat::Count,
                expand: 1,
            },
            Limits::default(),
            false,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(shallow.fetched, FILES as u64, "one level, one read per row");
    }

    /// **A reference into a virtual predicate expands, like any other reference.**
    ///
    /// `X where X = aperture.db.List _` heads on the fact type rather than on its key, so
    /// the row is a *reference to a catalogue row* — the shape that has to work if
    /// "virtual predicates behave like facts" is to mean anything on the read path. It
    /// does, because `Catalogued` answers `point` as well as `scan`, and the fetch handler
    /// wraps the store in it exactly as the query path does.
    ///
    /// Nothing is spent when no id asks for it, which is the other half: materialising a
    /// listing walks the store root and reads a sidecar per database, so it happens only
    /// when an id names the catalogue.
    #[test]
    fn a_reference_into_a_virtual_predicate_expands() {
        let serving = serving(FILES);
        let quiet = AtomicBool::new(false);

        let summary = run(
            &serving.socket,
            "code",
            "X where X = aperture.db.List _",
            Rendering {
                format: RowFormat::Count,
                expand: aperture_client::FULL_DEPTH,
            },
            Limits::default(),
            false,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(summary.rows, 1, "the row survives: {summary:?}");
        assert_eq!(
            summary.fetched, 1,
            "the catalogue row was read: {summary:?}"
        );
        assert_eq!(
            summary.unresolved, 0,
            "and it resolved, so nothing looks like damage"
        );
        assert!(
            summary.notices.is_empty(),
            "nothing to apologise for: {:?}",
            summary.notices
        );
    }

    /// **A virtual predicate answers a fetch like any other**, and an id past its end is
    /// an *unstored* absence rather than a missing fact.
    ///
    /// The whole claim `aperture.db.List` makes is that a virtual predicate is ordinary:
    /// `Catalogued` answers both halves of the store seam for it, so a `point` read finds
    /// a catalogue row exactly as it finds a stored one, and nothing above the seam knows
    /// the difference. Refusing here — which is what this test used to assert — made the
    /// seam's own promise false one layer up, and broke an ordinary query.
    ///
    /// The second half is the distinction that replaced the refusal. A stored fact that is
    /// not there is corruption ([I11](../../docs/invariants.md#i11),
    /// [I12](../../docs/invariants.md#i12)); a *catalogue* row that is not there is a
    /// listing that has moved on, since these ids are positions in a view materialised per
    /// query rather than durable identities. Only the server can tell those apart, so it
    /// says which.
    #[test]
    fn a_virtual_predicate_resolves_an_id_and_says_when_one_has_moved_on() {
        use aperture_client::{Mode, WireValue};
        use aperture_schema::id::FactId;
        use aperture_wire::protocol::Found;

        let serving = serving(FILES);
        let mut connection =
            super::connect(&serving.socket, "code", Mode::ReadOnly).expect("a connection");

        let schema = std::sync::Arc::new(connection.served_schema().expect("the schema"));
        let catalogue = crate::code_index::catalogue_id();

        // Sequence 1 is the first listed database, since the catalogue hands sequences out
        // from 1 as a real allocator does.
        let first = FactId::new(catalogue, 1).expect("a fact id");
        let answered = connection.fetch(&schema, &[first]).expect("it resolves");

        let Some(Found::Key(WireValue::Record(fields))) = answered.first() else {
            panic!("a catalogue row is a record: {answered:?}");
        };
        assert_eq!(
            fields[0],
            WireValue::Str("code".to_owned()),
            "the row the listing holds, name first: {fields:?}"
        );

        // Past the end of the listing: nothing there, and **not** a claim of damage.
        let past = FactId::new(catalogue, 9_999).expect("a fact id");
        assert_eq!(
            connection.fetch(&schema, &[past]).expect("it is answered"),
            vec![Found::Unstored],
            "a listing that has no such row is not a missing fact"
        );

        // And the session still answers about the catalogue the ordinary way.
        let mut rows = connection
            .query("N where aperture.db.List {name = N}")
            .expect("it compiles");
        assert_eq!(
            connection.drain(&mut rows).expect("the rows arrive").len(),
            1
        );
    }

    /// **A disjunction with one branch in memory and one in fjall.**
    ///
    /// The sharpest of these, because a disjunction is *one level with several sources*
    /// — the same frame, opened against a different store each time it moves to the next
    /// alternative. Nothing else makes `Catalogued::scan` answer two ways inside one
    /// loop, and a wrapper that decided per *query* rather than per *scan* would pass
    /// every other test here and fail this one.
    #[test]
    fn a_disjunction_draws_one_branch_from_each_store() {
        let serving = serving(FILES);

        let both = count(&serving, "N where aperture.db.List {name = N} | src.File N");
        assert_eq!(
            both,
            FILES as u64 + 1,
            "every file, and the one database, concatenated"
        );

        // Each branch alone, so the sum above is a sum of two known things rather than a
        // number that happens to be right.
        assert_eq!(count(&serving, "N where aperture.db.List {name = N}"), 1);
        assert_eq!(count(&serving, "N where src.File N"), FILES as u64);
    }

    /// A negation of a **stored** predicate, over a variable the **catalogue** bound.
    ///
    /// The direction matters: the negation's probe seeks a keyspace using bytes that
    /// came out of the listing, so the register it reads was filled by the in-memory
    /// side and spliced into a real seek.
    #[test]
    fn a_negation_of_a_keyspace_reads_a_catalogue_binding() {
        let serving = serving(FILES);

        // No file is named `code`, so the database survives the negation.
        let survives = count(&serving, "N where aperture.db.List {name = N}; !src.File N");
        assert_eq!(survives, 1, "no file is called `code`");

        // The assertion's partner: with the negation the other way up, the same row is
        // excluded — so the pair says the probe ran rather than that it matched nothing.
        let excluded = count(&serving, "N where aperture.db.List {name = N}; src.File N");
        assert_eq!(excluded, 0, "and asserting it instead answers nothing");
    }

    /// A negation of the **catalogue**, over a variable a *stored* level bound — the
    /// reverse direction, where the probe is the one answered from memory.
    #[test]
    fn a_negation_of_the_catalogue_reads_a_stored_binding() {
        let serving = serving(FILES);

        let all = count(&serving, "F where src.File F; !aperture.db.List {name = F}");
        assert_eq!(all, FILES as u64, "no file shares a name with a database");
    }

    /// **A denial on a catalogue field** — `!=`, which is a residual and never a seek.
    ///
    /// Worth its own test because a denial is decided by `apply_compares` over the row's
    /// own bytes, so it is the plainest check that a listing row decodes exactly as a
    /// stored one does.
    #[test]
    fn a_denial_filters_the_listing_by_its_own_bytes() {
        let serving = serving(0);
        create_database(&serving, "alpha");
        create_database(&serving, "zulu");

        // Denying a prefix one of the three carries: the other two survive. A filter
        // picking rows apart says more than an all-or-nothing answer, which would hold
        // just as well for a listing that was never read.
        let kept = count(
            &serving,
            "N where aperture.db.List {name = N}; N != \"co\"..",
        );
        assert_eq!(kept, 2, "alpha and zulu, but not code");

        let all = count(
            &serving,
            "N where aperture.db.List {name = N}; N != \"q\"..",
        );
        assert_eq!(all, 3, "nothing starts with q");
    }

    /// **A prefix constraint on the catalogue's leading field**, answered over three
    /// databases so the range has one below it, one inside it and one above.
    ///
    /// **What this does not do**, stated because the obvious reading is wrong and I
    /// believed it for a while: it does not pin `Catalogued::scan`'s upper bound.
    /// Deleting that bound outright leaves this test green — sargeability compiles a
    /// string prefix into a seek **and** a residual, and the residual re-checks every
    /// row the range let through. The bound is defence in depth and no query can
    /// isolate it, which is why it has a unit test of its own next to the code
    /// (`aperture_server::catalogue`).
    #[test]
    fn a_prefix_constraint_bounds_the_listing() {
        let serving = serving(0);
        create_database(&serving, "alpha");
        create_database(&serving, "zulu");

        assert_eq!(
            count(&serving, "N where aperture.db.List {name = N}"),
            3,
            "alpha, code, zulu"
        );

        assert_eq!(
            count(
                &serving,
                "N where aperture.db.List {name = N}; N = \"co\".."
            ),
            1,
            "`code` is in the range; `alpha` is below it and `zulu` above"
        );
        assert_eq!(
            count(
                &serving,
                "N where aperture.db.List {name = N}; N = \"cp\".."
            ),
            0,
            "and one letter later it is not"
        );
    }

    /// A **subquery** over the catalogue, inlined into an enclosing query that also
    /// reads a keyspace.
    #[test]
    fn a_subquery_over_the_catalogue_inlines() {
        let serving = serving(FILES);

        let rows = count(
            &serving,
            "{db = N, file = F} where N = (M where aperture.db.List {name = M}); src.File F",
        );
        assert_eq!(rows, FILES as u64, "the subquery's one row × every file");
    }
}
