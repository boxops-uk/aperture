//! `aperture-viewer` — a code-search site over an Aperture database.
//!
//! ```text
//! aperture --data-dir ~/ap-cs/db serve &
//! aperture-viewer ~/ap-cs/db/aperture.sock//code
//! ```
//!
//! Everything it does is in [`aperture_viewer`]; this file is the argument parsing and
//! the listener, and is deliberately thin for the same reason `aperture`'s `main.rs`
//! is.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use aperture_client::{Address, Endpoint};
use clap::Parser;

#[derive(Parser)]
#[command(name = "aperture-viewer", about = "Browse an Aperture code index")]
struct Args {
    /// Where to read from: `[where//]name[@instance]`.
    ///
    /// The same address grammar `aperture` takes — a bare name means the default
    /// socket, `/path/to.sock//code` names one, and `box:7280//code` is TCP. An address
    /// naming no target uses `$XDG_RUNTIME_DIR/aperture.sock`, which is where a server
    /// started with no `--data-dir` listens.
    #[arg(default_value = "code")]
    address: String,

    /// Where to listen.
    #[arg(long, default_value = "127.0.0.1:8088")]
    bind: String,

    /// How many connections to keep open to the server.
    ///
    /// Idle ones, that is: a burst opens more and closes them on return, so this is
    /// the floor rather than the ceiling. See `pool` for why a pool has a policy.
    #[arg(long, default_value_t = 8)]
    pool: usize,
}

/// Resolve an address argument, defaulting the target the way the CLI does.
///
/// A viewer has no configuration layer of its own, so the one default is the well-known
/// socket — `$XDG_RUNTIME_DIR/aperture.sock`, where a server started with no `--data-dir`
/// listens. Anything else is named in the address.
fn address(text: &str) -> Result<Address, aperture_client::ClientError> {
    let socket = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join("aperture.sock");

    Ok(Address::parse(text)?.or_endpoint(Endpoint::Unix(socket)))
}

fn main() -> ExitCode {
    let args = Args::parse();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("aperture-viewer: no runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async move {
        // **The built-in schema, and no assertion made with it.** A reader has no
        // claim to make about the schema — the database's is the one that matters,
        // and it is frozen at create (I13).
        let schema = Arc::new(aperture_cli_schema());

        let address = match address(&args.address) {
            Ok(address) => address,
            Err(error) => {
                eprintln!("aperture-viewer: {error}");
                return ExitCode::FAILURE;
            }
        };

        let app = match aperture_viewer::App::open(&address, schema, args.pool) {
            Ok(app) => Arc::new(app),
            Err(error) => {
                eprintln!("aperture-viewer: could not read `{address}`: {error}");
                return ExitCode::FAILURE;
            }
        };

        println!("aperture-viewer");
        println!("  reading   {address}");
        println!("  files     {}", app.files());
        println!("  listening http://{}", args.bind);

        let listener = match tokio::net::TcpListener::bind(&args.bind).await {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("aperture-viewer: cannot listen on {}: {error}", args.bind);
                return ExitCode::FAILURE;
            }
        };

        let served = axum::serve(listener, app.router())
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await;

        match served {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("aperture-viewer: {error}");
                ExitCode::FAILURE
            }
        }
    })
}

/// The code index schema, parsed from the same file the server reads.
///
/// **Compiled in rather than fetched**, which is the same position every other client
/// is in: the transport codec sends no field names and no types, so both ends supply
/// them. What a client cannot do yet is *ask* — see `docs/phase-8-schemas.md` on why
/// generating a client from the schema is recorded rather than scheduled.
fn aperture_cli_schema() -> aperture_schema::schema::Schema {
    const SOURCE: &str = include_str!("../../../schemas/code.aps");

    let mut diagnostics = vec![];

    let cst = aperture_schema::syntax::parse::parse(SOURCE, &mut diagnostics)
        .expect("the built-in schema parses");

    aperture_schema::syntax::lower::lower(&cst, &mut diagnostics)
        .expect("the built-in schema lowers")
        .schema
}
