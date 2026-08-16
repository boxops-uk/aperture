//! The command tree — [operations §4](../docs/aperture-cli-design.md).
//!
//! Common lifecycle verbs stay top-level because they are the daily drivers; admin
//! tooling nests one level. Every database-taking command is meant to accept any
//! address form from §2, so "local or remote" is a property of the *address* rather
//! than of the command — which is why there is no `--remote` flag anywhere here.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// An immutable, embedded fact database.
#[derive(Debug, Parser)]
#[command(name = "aperture", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// The store root: where databases live, and what the socket path derives from.
    ///
    /// Also `APERTURE_DATA_DIR`. Defaults under `$XDG_DATA_HOME` — see
    /// [`crate::config`].
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Say more. Repeatable.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the server over a store root.
    Serve {
        /// Where to bind. Defaults to `<data-dir>/aperture.sock`.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,

        /// Written once the listener is accepting — a signal, not a race.
        #[arg(long, value_name = "PATH")]
        ready_file: Option<PathBuf>,
    },

    /// Create a Writable database.
    Create { name: String },

    /// Seal a database: Writable → Complete, and immutable thereafter.
    Finish {
        name: String,

        /// Seal a database holding no facts.
        ///
        /// Refused by default because a silently-empty sealed artifact is the classic
        /// CI failure that looks like success.
        #[arg(long)]
        allow_zero_facts: bool,
    },

    /// List the databases in the store root.
    List {
        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },

    /// Show a database's metadata and schema.
    Describe {
        name: String,

        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,
    },

    /// Run a query and print its rows.
    Query {
        name: String,
        query: String,

        #[arg(long, value_enum, default_value_t = RowFormat::Table)]
        format: RowFormat,

        /// Stop after this many rows, cancelling the rest in band.
        ///
        /// Not `LIMIT`: the query is unchanged and the server does the work up to the
        /// point the cancel lands. What it bounds is what crosses the socket.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,

        /// Print rows and elapsed time to stderr, so it survives a pipe.
        #[arg(long)]
        timing: bool,

        /// Report what the query **examined**, per step, to stderr.
        ///
        /// The outcome to a plan's intent: a plan says which field narrowed the scan,
        /// and this says how many rows that came to.
        #[arg(long)]
        profile: bool,
    },

    /// An interactive REPL.
    ///
    /// With a database, it is the product shell: **always over the wire**, so the
    /// format has a permanent exerciser and `\more` holds a real cursor across a real
    /// round trip. With none, it is Phase 5's embedded demo over a scratch database it
    /// seeds itself — which is where `:plan` and `:type` live, a plan being a thing a
    /// client never holds.
    Shell {
        /// The database to connect to.
        database: Option<String>,
    },

    /// Administrative commands.
    #[command(subcommand)]
    Db(DbCommand),
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Delete a database.
    Rm {
        name: String,

        /// Do not ask.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

/// How to render output.
///
/// **Client-side, always.** The wire carries the binary format and the server never
/// produces JSON — a decision from the original brief, and the reason this is a flag
/// on the command rather than a field in a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Aligned columns for a person.
    Table,
    /// One JSON document, for a script.
    Json,
}

/// How to render a query's rows.
///
/// Its own enum rather than [`Format`]'s, because the shapes a *result* wants are not
/// the shapes a listing wants: `raw` and `count` are meaningless for `list`, and the
/// distinction between a shape that streams and one that cannot is a property of
/// results alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RowFormat {
    /// Aligned columns for a person. **The one shape that buffers** — see
    /// [`crate::rows`].
    Table,
    /// One JSON document, written incrementally.
    Json,
    /// Tab-separated fields, one row per line. Streams.
    Raw,
    /// The row count and nothing else.
    ///
    /// For measuring the *server*: rendering is the client's cost, and a throughput
    /// number that includes it is measuring the wrong process.
    Count,
}
