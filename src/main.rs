//! `aperture` — the command-line tool.
//!
//! Parse, resolve where things live, dispatch. The commands themselves are in
//! [`commands`]; this file is deliberately thin, because the interesting decisions
//! are about *ownership* and *addressing* rather than about argument parsing.
//!
//! See [operations §4](../docs/aperture-cli-design.md) for the tree and §2 for the
//! addressing rules it is built to obey.

mod cli;
mod code_index;
mod commands;
mod config;
mod output;
mod rows;
mod shell;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;

use cli::{Cli, Command, DbCommand};

/// Why a command could not run.
///
/// One taxonomy for the tool, so that every exit goes through one place and no
/// command invents its own wording for "the server has this".
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Store(#[from] aperture_store::error::StoreError),

    /// Writing failed — usually a pipe the reader closed, which is how `| head` ends
    /// a query rather than a fault worth a stack trace.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Server(#[from] aperture_server::ServerError),

    #[error("{0}")]
    Engine(#[from] aperture_engine::error::ApertureError),

    /// The client could not do it — a server that said no, or a socket that failed.
    ///
    /// The server's own wording rather than a summary of it: the server is the thing
    /// that knows what happened, and a tool that paraphrased would be one more place
    /// for the two answers to drift apart.
    #[error("{0}")]
    Client(#[from] aperture_client::ClientError),

    /// Nothing is listening where a database was asked for.
    ///
    /// §2's rule 1, and the message it asks for: a bare name always means "ask the
    /// local server", and there is **no** silent fallback to opening the directory,
    /// because a server may be holding it (`ops-I1`). So the failure has to say what
    /// to do about it rather than quietly doing something else.
    #[error(
        "could not connect to the Aperture server on socket {}\n           is one running? `aperture serve` starts one over this data directory",
        socket.display()
    )]
    NoServer { socket: PathBuf },

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = config::data_dir(cli.data_dir.clone());

    // Derived from the root rather than chosen (§2), which is what makes it the
    // server-detection mechanism: a command that knows the data directory knows where
    // to look, with nothing to configure and nothing to get out of step.
    let socket = config::socket_path(&root, None);

    match dispatch(&cli, &root, &socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("aperture: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: &Cli, root: &std::path::Path, socket: &std::path::Path) -> Result<(), CliError> {
    match &cli.command {
        Command::Serve {
            socket: bind,
            ready_file,
        } => {
            let socket = config::socket_path(root, bind.clone());
            commands::serve::run(root, &socket, ready_file.as_deref())
        }

        Command::Create { name } => {
            let created = commands::create::run(root, socket, name)?;
            println!("created {} ({})", created.name, created.instance);
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

            let sealed = commands::finish::run(root, socket, name, *allow_zero_facts)?;

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

        Command::Describe { name, format } => {
            print!("{}", commands::describe::run(root, name, *format)?);
            Ok(())
        }

        Command::Query {
            name,
            query,
            format,
            limit,
            timing,
            profile,
        } => {
            let summary = commands::query::run(socket, name, query, *format, *limit, *profile)?;

            if let Some(measured) = &summary.profile {
                eprint!(
                    "{}",
                    commands::query::render_profile(measured, summary.rows)
                );
            }

            if summary.truncated {
                eprintln!(
                    "aperture: stopped at {} rows; raise or drop --limit to see the rest",
                    summary.rows
                );
            }

            if *timing {
                // stderr, so a timing number never lands in a pipe someone is parsing.
                eprintln!(
                    "{} row(s) in {:.3} ms",
                    summary.rows,
                    summary.elapsed.as_secs_f64() * 1000.0
                );
            }

            Ok(())
        }

        Command::Shell => Ok(shell::main()?),

        Command::Db(DbCommand::Rm { name, yes }) => {
            if !*yes {
                // Deleting a database is not undoable and the tool has no trash, so
                // the default is to ask. `--yes` is what a script passes.
                eprintln!("aperture: refusing to delete `{name}` without --yes");
                return Ok(());
            }

            commands::rm::run(root, socket, name)?;
            println!("removed {name}");
            Ok(())
        }
    }
}
