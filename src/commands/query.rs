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

/// What a query came to.
pub struct Summary {
    pub rows: u64,
    pub elapsed: std::time::Duration,
    /// Whether `--limit` stopped it short of the end.
    pub truncated: bool,
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
    limit: Option<u64>,
) -> Result<Summary, CliError> {
    // **Read-only, and asserting nothing.** A reader has no claim to make about the
    // schema: the database's is the one that matters, it is frozen at create
    // ([I13](../../docs/invariants.md#i13)), and a tool that refused to *read* a
    // database because its own built-in copy had moved on would be refusing the one
    // thing that still works.
    let mut connection = connect(socket, name, Mode::ReadOnly)?;

    let started = Instant::now();
    let mut result = connection.query(query)?;

    let stdout = std::io::stdout();
    let mut sink = Sink::new(stdout.lock(), format, result.desc())?;

    let mut truncated = false;

    loop {
        if limit.is_some_and(|limit| result.seen() >= limit) {
            // In band, on the stream: the server completes with what it sent, the
            // connection stays usable, and the rows already in flight are drained
            // rather than left in the socket. A `--limit` is not a `LIMIT`.
            connection.cancel(&mut result)?;
            truncated = true;
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
        truncated,
    })
}

/// Connect, turning "nothing is listening" into the error §2 asks for.
fn connect(socket: &Path, database: &str, mode: Mode) -> Result<Connection, CliError> {
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
