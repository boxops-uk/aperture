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

    /// Where a schema's imports are looked for. Repeatable; first match wins.
    ///
    /// Also `APERTURE_SCHEMA_PATH`, separated the way `PATH` is. An entry file's own
    /// directory is always searched first, so a directory of schemas that import each
    /// other needs none of this.
    #[arg(long, global = true, value_name = "PATH")]
    pub schema_path: Option<Vec<PathBuf>>,

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

        /// Also listen on TCP, at `host:port`.
        ///
        /// **Default-closed** (`ops-I10`): there is no config-file entry and no
        /// environment variable for this, so a network port can only appear because
        /// somebody typed one. It is an opt-in to *reachability*, not to access
        /// control — the handshake accepts anonymous, so whoever passes this is taking
        /// on the gateway in front of it.
        #[arg(long, value_name = "HOST:PORT")]
        listen_tcp: Option<String>,

        /// Written once the listener is accepting — a signal, not a race.
        #[arg(long, value_name = "PATH")]
        ready_file: Option<PathBuf>,
    },

    /// Create a Writable database.
    Create {
        name: String,

        /// The schema to create it against — an entry file, whose imports are
        /// resolved from its own directory and then `--schema-path`.
        ///
        /// The schema is **frozen for the database's lifetime** (I13) and embedded in
        /// it, so this is the one moment it can be chosen. Without it, the built-in
        /// code index is used, which is what every database held before there was a
        /// way to say otherwise.
        #[arg(long, value_name = "FILE")]
        schema: Option<PathBuf>,
    },

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

        /// Dump the embedded schema itself — the text `create --schema` would take.
        #[arg(long)]
        schema: bool,
    },

    /// Run a query and print its rows.
    Query {
        name: String,
        query: String,

        #[arg(long, value_enum, default_value_t = RowFormat::Table)]
        format: RowFormat,

        /// Give up after this many seconds, cancelling in band.
        ///
        /// A deadline on the *client's* patience, not a promise about the server: the
        /// cancel lands between rows, so a query stuck inside one chunk is stopped when
        /// that chunk ends. What it bounds is how long this command waits.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<f64>,

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

        /// Print **how many rows**, and none of them.
        ///
        /// The same plan and the same executor; what differs is that the server
        /// counts instead of encoding. That is the part that costs, so this is a
        /// different order of expense from piping the rows to `wc -l`.
        #[arg(long, conflicts_with_all = ["limit", "profile", "format"])]
        count: bool,

        /// Show the fact a reference names, instead of its id — recursively.
        ///
        /// A row carries a reference as an id (`#3:7`), because that is what one is once
        /// stored. This replaces each with the fact it names, and each reference in
        /// *that* with the fact *it* names: `{"to": "#3:7"}` becomes
        /// `{"to": {"module": {…}, "name": "encode", "line": 12}}`, which is the same
        /// nested shape a producer sends when it writes one.
        ///
        /// Bare, it follows every reference to the end of the chain; with a number, that
        /// many hops. **It costs a point read per distinct reference**, answered from a
        /// cache within the run, so it is off unless asked for.
        #[arg(
            long,
            value_name = "HOPS",
            num_args = 0..=1,
            default_missing_value = "16",
            conflicts_with = "count"
        )]
        expand: Option<usize>,
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

    /// Read schemas as files, before any database holds one.
    #[command(subcommand)]
    Schema(SchemaCommand),

    /// Administrative commands.
    #[command(subcommand)]
    Db(DbCommand),
}

/// The three questions a schema can be asked away from a database
/// ([operations §5](../docs/aperture-cli-design.md)).
///
/// All three take **files**, and `diff` takes a database name just as happily: what is
/// being compared is a schema, and where it was read from is the caller's business.
#[derive(Debug, Subcommand)]
pub enum SchemaCommand {
    /// Resolve a schema and report what it does not like.
    ///
    /// Walks the import closure, unions the blocks, and lowers the result — so this
    /// answers unresolved imports, syntax errors and genuine redeclarations, which are
    /// the three things a schema can be wrong about before anything writes a fact.
    Check {
        /// The entry file. Its imports are resolved from its own directory, then
        /// `--schema-path`.
        file: PathBuf,
    },

    /// Print a schema's fingerprint, and each predicate's.
    ///
    /// **This is the number a client carries.** A client never computes one
    /// ([open decisions](../docs/open-decisions.md)); it holds what this prints, and a
    /// stale constant is refused at the handshake by name.
    Fingerprint {
        file: PathBuf,

        #[arg(long, value_enum, default_value_t = Format::Table)]
        format: Format,

        /// Print the canonical form the fingerprint is taken over.
        ///
        /// What a second implementation is written against, and what to diff when two
        /// ends disagree about a schema they believe they share.
        #[arg(long)]
        canonical: bool,
    },

    /// Compare two schemas: `Identical`, `Compatible (n added)`, or `Breaking`.
    ///
    /// Each side is a schema file or the name of a database in the store root, in any
    /// combination — comparing what a build *would* produce against what an artifact
    /// already holds is the question this is for.
    Diff { before: String, after: String },
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
    /// One JSON value per line — [JSON Lines](https://jsonlines.org).
    ///
    /// The same values `json` writes without the array around them, which is what a
    /// consumer reading row by row wants (`jq -c`, a `for line in` loop) and what a
    /// **paged** result has to be: a page is not a document, and three pages of one
    /// query are not three documents either.
    Jsonl,
    /// Tab-separated fields, one row per line. Streams.
    Raw,
    /// The row count and nothing else.
    ///
    /// For measuring the *server*: rendering is the client's cost, and a throughput
    /// number that includes it is measuring the wrong process.
    Count,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`--expand`'s bare depth is [`aperture_client::FULL_DEPTH`]**, restated as a
    /// literal.
    ///
    /// clap needs `default_missing_value` as a string at attribute position, so the
    /// number is written twice: once in `aperture-client`, where the walk is, and once
    /// above. This is the check that they agree — the sort of drift nothing else would
    /// notice, since a wrong number here still expands, just not as far as the flag's own
    /// help says.
    ///
    /// Parsed rather than read off the `Arg`, so what is asserted is the value the
    /// command actually receives.
    #[test]
    fn the_bare_expand_depth_is_the_clients_full_depth() {
        let parsed = Cli::parse_from([
            "aperture",
            "query",
            "code",
            "F where src.File F",
            "--expand",
        ]);

        let Command::Query { expand, .. } = parsed.command else {
            panic!("that is a query");
        };

        assert_eq!(
            expand,
            Some(aperture_client::FULL_DEPTH),
            "`--expand` with no number should follow a chain as far as the expander does"
        );

        // And with a number it is that number, which is the form the bare one defaults.
        let parsed = Cli::parse_from([
            "aperture",
            "query",
            "code",
            "F where src.File F",
            "--expand",
            "2",
        ]);
        let Command::Query { expand, .. } = parsed.command else {
            panic!("that is a query");
        };
        assert_eq!(expand, Some(2));

        // Absent is absent: ids, and no point reads.
        let parsed = Cli::parse_from(["aperture", "query", "code", "F where src.File F"]);
        let Command::Query { expand, .. } = parsed.command else {
            panic!("that is a query");
        };
        assert_eq!(expand, None);
    }
}
