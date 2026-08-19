//! `fjord` — the command-line tool.
//!
//! Parse, resolve where things live, dispatch. The commands themselves are in
//! [`commands`]; this file is deliberately thin, because the interesting decisions
//! are about *ownership* and *addressing* rather than about argument parsing.
//!
//! See [operations §4](../../../docs/fjord-cli-design.md) for the tree and §2 for the
//! addressing rules it is built to obey.

mod cli;
mod commands;
mod config;
mod output;
mod prompt;
mod rows;
#[cfg(test)]
mod testing;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;

use cli::{Cli, Command, DbCommand, SchemaCommand};
pub use fjord_cli::sample_schema;

/// Why a command could not run.
///
/// One taxonomy for the tool, so that every exit goes through one place and no
/// command invents its own wording for "the server has this".
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Store(#[from] fjord_store::error::StoreError),

    /// Writing failed — usually a pipe the reader closed, which is how `| head` ends
    /// a query rather than a fault worth a stack trace.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Server(#[from] fjord_server::ServerError),

    #[error("{0}")]
    Engine(#[from] fjord_engine::error::FjordError),

    /// The client could not do it — a server that said no, or a socket that failed.
    ///
    /// The server's own wording rather than a summary of it: the server is the thing
    /// that knows what happened, and a tool that paraphrased would be one more place
    /// for the two answers to drift apart.
    #[error("{0}")]
    Client(#[from] fjord_client::ClientError),

    /// A config file that was named and could not be read, or that is not a config file.
    ///
    /// A *missing* `./fjord.json` is not this — nobody asked for one. A missing
    /// `--config` is, because somebody did.
    #[error("{path}: {detail}", path = path.display())]
    Config {
        path: std::path::PathBuf,
        detail: String,
    },

    /// Nothing is listening where a database was asked for.
    ///
    /// §2's rule 1, and the message it asks for: a bare name always means "ask the
    /// local server", and there is **no** silent fallback to opening the directory,
    /// because a server may be holding it (`ops-I1`). So the failure has to say what
    /// to do about it rather than quietly doing something else.
    #[error(
        "could not connect to the Fjord server at {target}\n           \
         is one running? `fjord serve` starts one over this data directory"
    )]
    NoServer { target: fjord_client::Endpoint },

    /// A store root held by a process that is **not** listening on this socket.
    ///
    /// The ordinary case no longer reaches here: a running server owns its root, and a
    /// lifecycle command finds it on the socket and routes through it. What is left is
    /// the genuinely confusing case — something holds the root and is not answering —
    /// so the message names both halves, which is what a psql-style actionable error
    /// is for.
    #[error(
        "the store root {} is held by another process, and nothing is listening on {}\n  \
         if a server is running, this is not its data directory — check --data-dir",
        root.display(),
        socket.display()
    )]
    RootHeld { root: PathBuf, socket: PathBuf },

    /// An address that is not one — `fjord://` with nothing after it, or no
    /// database on the end.
    ///
    /// Its own variant so the message can show the form rather than whatever the
    /// resolver failed at: somebody who mistyped an address needs to be told the shape,
    /// not told that a hostname did not resolve.
    #[error("`{address}` is not an address — try fjord://host:port/database")]
    Address { address: String },

    /// A refusal that has **already been rendered**, spans and all.
    ///
    /// Its own variant so that [`main`] can print it alone: everything else here is a
    /// sentence and reads as `fjord: <sentence>`, while this is a codespan block
    /// whose first line a prefix would push out of alignment with its own caret.
    #[error("{0}")]
    Diagnosed(String),

    /// A schema that does not resolve, does not parse, or does not lower.
    ///
    /// Carries the reason **already rendered against its source**, spans and all,
    /// because the schema front end is the thing that knows which file and which line —
    /// and because a resolved schema is several files, so a caret is worth more here
    /// than anywhere else in this tool.
    #[error("{0}")]
    Schema(String),

    /// The terminal, rather than anything Fjord did.
    ///
    /// Its own variant because it is the one failure here that says nothing about the
    /// database: a readline that cannot open a tty is a fact about where the tool was
    /// run, and folding it into [`Io`](CliError::Io) would file it under "a pipe
    /// closed".
    #[error("the shell could not start: {0}")]
    Shell(String),
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match start(&cli).and_then(|context| dispatch(&cli, &context)) {
        Ok(()) => ExitCode::SUCCESS,

        // A diagnostic is printed as it was rendered. Everything else is a sentence,
        // and a sentence about a tool should say which tool.
        Err(CliError::Diagnosed(rendered)) => {
            eprint!("{rendered}");
            ExitCode::FAILURE
        }

        Err(error) => {
            eprintln!("fjord: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Everything a command needs to know about *where*, worked out once.
struct Context {
    /// The store root — for the offline path, and for `serve`.
    root: std::path::PathBuf,
    /// The socket this root's server listens on, and the default target's path.
    socket: std::path::PathBuf,
    /// Where an address that named no target goes.
    default: fjord_client::Endpoint,
    /// Whether that default is this machine's own socket, which is the only case with
    /// an offline half — see [`commands::Target`].
    default_is_local: bool,
}

impl Context {
    /// Resolve an address argument against the default target.
    fn target(&self, address: &str) -> Result<commands::Target, CliError> {
        commands::Target::resolve(address, &self.default, self.default_is_local)
    }
}

/// The layering, applied once: **flag over environment over file over default**.
///
/// The address argument is the layer above all of these and is applied per command, in
/// [`Context::target`], because it is the argument.
fn start(cli: &Cli) -> Result<Context, CliError> {
    let file = config::file(cli.config.as_deref())?;

    let root = config::data_dir(cli.data_dir.clone().or_else(|| file.data_dir.clone()));
    let chosen = config::root_was_chosen(cli.data_dir.as_ref()) || file.data_dir.is_some();
    let socket = config::socket_path(&root, chosen, None);

    let default = config::default_endpoint(&socket, &file)?;

    // Only the plain local socket keeps an offline half. A target from the environment
    // or a config file is one somebody named, and reaching past a named server to open
    // a directory is what §2 forbids.
    let default_is_local = default == fjord_client::Endpoint::Unix(socket.clone());

    Ok(Context {
        root,
        socket,
        default,
        default_is_local,
    })
}

fn dispatch(cli: &Cli, context: &Context) -> Result<(), CliError> {
    let root = context.root.as_path();

    match &cli.command {
        Command::Serve {
            socket: bind,
            listen_tcp,
            ready_file,
            commit_per_block,
        } => {
            // **The same path a client computes**, which is the whole of the
            // server-detection mechanism: a server that listened somewhere else would
            // be invisible to every command that did not name it.
            let socket = bind.clone().unwrap_or_else(|| context.socket.clone());
            commands::serve::run(
                root,
                &socket,
                listen_tcp.as_deref(),
                ready_file.as_deref(),
                *commit_per_block,
            )
        }

        Command::Create { name, schema } => {
            let created = commands::create::run(
                root,
                &context.target(name)?,
                schema,
                &config::schema_path(cli.schema_path.clone()),
            )?;

            println!(
                "created {} ({}) against {}",
                created.name, created.instance, created.schema
            );
            Ok(())
        }

        Command::Finish {
            name,
            allow_zero_facts,
        } => {
            // Sealing merges every tree before it walks them, which on a large database
            // is tens of seconds of rewriting with nothing to show for it yet. Said
            // before the wait rather than explained after it, and on stderr because the
            // line that matters is still the one on stdout.
            eprintln!("sealing {name} — merging trees, then computing identity");

            let sealed = commands::finish::run(root, &context.target(name)?, *allow_zero_facts)?;

            if sealed.already_complete {
                println!("{name} is already complete ({:#018x})", sealed.fingerprint);
            } else {
                println!(
                    "sealed {name}: {} facts, {} bytes, identity {:#018x}",
                    sealed.facts, sealed.bytes, sealed.fingerprint
                );
            }
            Ok(())
        }

        Command::List { format } => {
            print!("{}", commands::list::run(root, *format)?);
            Ok(())
        }

        Command::Describe {
            name,
            format,
            schema,
        } => {
            print!("{}", commands::describe::run(root, name, *format, *schema)?);
            Ok(())
        }

        Command::Query {
            name,
            query,
            format,
            timeout,
            limit,
            timing,
            profile,
            count,
            expand,
        } => {
            if *count {
                let started = std::time::Instant::now();
                let rows = commands::query::count(&context.target(name)?, query)?;

                println!("{rows}");

                if *timing {
                    eprintln!(
                        "counted in {:.3} ms",
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                }

                return Ok(());
            }

            let limits = commands::query::Limits {
                rows: *limit,
                timeout: timeout.map(std::time::Duration::from_secs_f64),
            };

            // **Ctrl-C asks the query to stop; it does not tear the connection down.**
            // The handler only sets a flag, because a signal handler is not a place to
            // speak a protocol from — the query loop notices between rows and sends a
            // per-stream Cancel, which is the difference between the server finishing
            // the stream tidily and discovering a dead socket.
            let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            {
                let interrupted = std::sync::Arc::clone(&interrupted);
                let _ = ctrlc::set_handler(move || {
                    interrupted.store(true, std::sync::atomic::Ordering::Relaxed);
                });
            }

            let rendering = commands::query::Rendering {
                format: *format,
                // `--expand` with no number means all the way down; absent means ids,
                // which is what a row carries.
                expand: expand.unwrap_or(0),
            };

            let summary = commands::query::run(
                &context.target(name)?,
                query,
                rendering,
                limits,
                *profile,
                &interrupted,
            )?;

            if let Some(measured) = &summary.profile {
                eprint!(
                    "{}",
                    commands::query::render_profile(measured, summary.rows)
                );
            }

            match summary.stopped {
                commands::query::Stopped::No => {}
                commands::query::Stopped::Limit => eprintln!(
                    "fjord: stopped at {} rows; raise or drop --limit to see the rest",
                    summary.rows
                ),
                commands::query::Stopped::Timeout => eprintln!(
                    "fjord: gave up after {} rows; raise or drop --timeout to see the rest",
                    summary.rows
                ),
                // Nothing to suggest — they asked. What is worth saying is that the
                // rows above are real and the query was stopped, not that it failed.
                commands::query::Stopped::Interrupt => {
                    eprintln!("fjord: cancelled at {} rows", summary.rows);
                }
            }

            // Where expansion could not go, and why — one line per predicate. On stderr
            // with everything else that is not a row.
            for notice in &summary.notices {
                eprintln!("fjord: {notice}");
            }

            // **Never silent**, and never on stdout: a reference naming no fact is a
            // damaged database, not a row somebody chose not to expand.
            if summary.unresolved > 0 {
                eprintln!(
                    "fjord: {} reference(s) named no fact and were printed as ids; \
                     this database is damaged",
                    summary.unresolved
                );
            }

            if *timing {
                // stderr, so a timing number never lands in a pipe someone is parsing.
                eprintln!(
                    "{} row(s) in {:.3} ms",
                    summary.rows,
                    summary.elapsed.as_secs_f64() * 1000.0
                );

                if summary.fetched > 0 {
                    eprintln!("{} point read(s) to expand", summary.fetched);
                }
            }

            Ok(())
        }

        // **Always over the wire**, and it never silently opens a store root a server
        // might hold: it connects, or says nothing is listening. There used to be a
        // second, embedded shell here for an argument-less invocation, which seeded a
        // scratch database from a corpus compiled into the binary — see `cli.rs`.
        Command::Shell { database } => commands::shell::run(&context.target(database)?),

        // **Files, not databases.** Nothing here opens a store root except `diff`, and
        // that one reads sidecars (`ops-I7`) — so all three work while a server holds
        // everything under it.
        Command::Schema(command) => {
            let roots = config::schema_path(cli.schema_path.clone());

            print!(
                "{}",
                match command {
                    SchemaCommand::Check { file } => commands::schema::check(file, &roots)?,

                    SchemaCommand::Fingerprint {
                        file,
                        format,
                        canonical,
                    } => commands::schema::print_fingerprint(file, &roots, *format, *canonical)?,

                    SchemaCommand::Diff { before, after } =>
                        commands::schema::diff(before, after, root, &roots)?,
                }
            );

            Ok(())
        }

        Command::Db(DbCommand::Rm { name, yes }) => {
            if !*yes {
                // Deleting a database is not undoable and the tool has no trash, so
                // the default is to ask. `--yes` is what a script passes.
                eprintln!("fjord: refusing to delete `{name}` without --yes");
                return Ok(());
            }

            commands::rm::run(root, &context.target(name)?)?;
            println!("removed {name}");
            Ok(())
        }
    }
}
