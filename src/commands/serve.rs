//! `aperture serve`.
//!
//! Owns the store root (`ops-I1`) and serves every database under it. **Unix socket
//! only** — `ops-I10` is default-closed, and a server that binds a network interface
//! because nobody said not to is the failure that rule exists to prevent.

use std::{path::Path, sync::Arc};

use aperture_server::{Registry, serve_unix};
use aperture_wire::protocol;

use crate::{CliError, code_index, commands};

/// # Errors
///
/// [`CliError::RootHeld`] if another server owns the root, or whatever binding or
/// opening reports.
pub fn run(root: &Path, socket: &Path, ready_file: Option<&Path>) -> Result<(), CliError> {
    // **`--features console`, and a developer's build only.**
    //
    // Turns on `tokio-console`, which shows every task, where it is parked and how long
    // it has been there — the view that finding `bench/FINDINGS.md` §10 needed, and
    // which that investigation reproduced by hand with a counter per await site.
    //
    // Off by default and deliberately not an operator's switch: it serves gRPC on
    // 127.0.0.1:6669, and a listening port that appears because a feature was on is the
    // shape `ops-I10` exists to refuse. It also needs `RUSTFLAGS="--cfg tokio_unstable"`,
    // so it cannot be turned on by accident:
    //
    // ```text
    // RUSTFLAGS="--cfg tokio_unstable" cargo run --release --features console \
    //     --bin aperture -- --data-dir PATH serve
    // tokio-console
    // ```
    #[cfg(feature = "console")]
    console_subscriber::init();

    // Held for the process's life: the lock *is* the ownership, so it is taken before
    // anything is opened and released only when the server exits.
    let (catalog, _lock) = commands::exclusive(root, socket)?;

    let schema = code_index::schema();
    let fingerprint = protocol::provisional_fingerprint(&schema);

    // The registry takes the catalog with it, because owning the root and owning the
    // databases under it are the same ownership: `create` and `remove` arriving over
    // the wire need both, and a server that held only the open handles is exactly the
    // server that had to be stopped before a lifecycle command could run.
    let (registry, listing) = Registry::open(catalog, schema)?;
    let registry = Arc::new(registry);

    println!("aperture serve");
    println!("  data dir   {}", root.display());
    println!("  socket     {}", socket.display());
    println!("  protocol   {}", protocol::VERSION);
    println!("  schema     {fingerprint:#018x}  (provisional — see PLAN Phase 8)");

    if listing.entries.is_empty() {
        // Said plainly rather than served silently: a server with nothing to serve is
        // almost always a wrong `--data-dir`, and the fix is one command away — and
        // now the command works without stopping this process first.
        println!("  databases  none — `aperture create <name>` makes one");
    } else {
        println!("  databases  {}", registry.len());
        for entry in &listing.entries {
            println!("    {:<20} {}", entry.name(), entry.status());
        }
    }

    for problem in &listing.problems {
        eprintln!("warning: {problem}");
    }

    serve_unix(socket, ready_file, registry)?;
    Ok(())
}
