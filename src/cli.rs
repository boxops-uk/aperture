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

    /// An interactive REPL.
    Shell,

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
