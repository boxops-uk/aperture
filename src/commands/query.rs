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

use aperture_client::{ClientError, Connection, Mode};

use crate::{CliError, cli::RowFormat, code_index, rows::Sink};

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
pub struct Summary {
    pub rows: u64,
    pub elapsed: std::time::Duration,
    /// Whether anything cut it short, and what.
    pub stopped: Stopped,
    /// What the server said it examined, when asked.
    pub profile: Option<aperture_client::QueryProfile>,
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
pub fn run(
    socket: &Path,
    name: &str,
    query: &str,
    format: RowFormat,
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

    let started = Instant::now();
    let mut result = if profile {
        connection.query_profiled(query)?
    } else {
        connection.query(query)?
    };

    let stdout = std::io::stdout();
    let mut sink = Sink::new(stdout.lock(), format, result.desc())?;

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
            Some(row) => sink.row(&row)?,
            None => break,
        }
    }

    let rows = sink.end()?;

    Ok(Summary {
        rows,
        elapsed: started.elapsed(),
        stopped,
        // Absent after any of the three early stops, and that is honest rather than a
        // gap: the server reports what it examined when the query *ends*, and a
        // cancelled one ended early — a tally taken then would describe a different
        // query than the one asked.
        profile: result.profile().cloned(),
    })
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
pub(crate) fn connect(socket: &Path, database: &str, mode: Mode) -> Result<Connection, CliError> {
    use std::io::ErrorKind;

    match Connection::connect(
        socket,
        database,
        Arc::new(code_index::schema()),
        mode,
        false,
    ) {
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

    use super::{Limits, Stopped, run};
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
            RowFormat::Count,
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
    #[test]
    fn a_cancelled_query_reports_no_profile() {
        let serving = serving(FILES);
        let quiet = AtomicBool::new(false);

        let summary = run(
            &serving.socket,
            "code",
            "F where src.File F",
            RowFormat::Count,
            Limits {
                rows: Some(5),
                timeout: None,
            },
            true,
            &quiet,
        )
        .expect("the query runs");

        assert_eq!(summary.stopped, Stopped::Limit);
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
            RowFormat::Count,
            Limits::default(),
            true,
            &quiet,
        )
        .expect("the query runs");

        assert!(whole.profile.is_some(), "an uncancelled one does");
    }
}
