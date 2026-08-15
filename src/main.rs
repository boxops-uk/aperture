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

    #[error("{0}")]
    Server(#[from] aperture_server::ServerError),

    #[error("{0}")]
    Engine(#[from] aperture_engine::error::ApertureError),

    /// A store root a server owns.
    ///
    /// **Never a fallback to opening it directly** (`ops-I1`, §2): the message names
    /// the root and says what to do, which is what a psql-style actionable error is
    /// for.
    #[error(
        "the store root {} is held by a running server\n  \
         lifecycle commands route through the server from PLAN 9d; until then, stop it first",
        root.display()
    )]
    RootHeld { root: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = config::data_dir(cli.data_dir.clone());

    match dispatch(&cli, &root) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("aperture: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: &Cli, root: &std::path::Path) -> Result<(), CliError> {
    match &cli.command {
        Command::Serve { socket, ready_file } => {
            let socket = config::socket_path(root, socket.clone());
            commands::serve::run(root, &socket, ready_file.as_deref())
        }

        Command::Create { name } => {
            let entry = commands::create::run(root, name)?;
            println!("created {} ({})", entry.name(), entry.meta.instance);
            Ok(())
        }

        Command::Finish {
            name,
            allow_zero_facts,
        } => {
            let sealed = commands::finish::run(root, name, *allow_zero_facts)?;

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

        Command::Shell => Ok(shell::main()?),

        Command::Db(DbCommand::Rm { name, yes }) => {
            if !*yes {
                // Deleting a database is not undoable and the tool has no trash, so
                // the default is to ask. `--yes` is what a script passes.
                eprintln!("aperture: refusing to delete `{name}` without --yes");
                return Ok(());
            }

            commands::rm::run(root, name)?;
            println!("removed {name}");
            Ok(())
        }
    }
}
