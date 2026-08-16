//! `aperture serve`.
//!
//! Owns the store root (`ops-I1`) and serves every database under it.
//!
//! **A Unix socket, and TCP only when asked.** `ops-I10` is default-closed, and a server
//! that binds a network interface because nobody said not to is the failure that rule
//! exists to prevent — so `--listen-tcp` has no config-file entry and no environment
//! variable, and a port can appear only because somebody typed one. It is an opt-in to
//! reachability rather than to access control: the handshake accepts anonymous, and the
//! gateway in front is the operator's.

use std::{path::Path, sync::Arc};

use aperture_server::{Registry, server::serve_on};
use aperture_wire::protocol;

use crate::{CliError, code_index, commands};

/// # Errors
///
/// [`CliError::RootHeld`] if another server owns the root, or whatever binding or
/// opening reports.
pub fn run(
    root: &Path,
    socket: &Path,
    listen: Option<&str>,
    ready_file: Option<&Path>,
) -> Result<(), CliError> {
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

    // **The served schema, not the stored one**: the same predicates a client declares,
    // plus `aperture.db.List`, which this process can answer out of the root it owns.
    // The fingerprint is unchanged by that — a virtual predicate is not part of what
    // two ends have to agree about — so a client that has never heard of it still
    // connects ([`code_index::with_catalogue`]).
    let schema = code_index::with_catalogue();
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

    if let Some(address) = listen {
        // Said out loud, every time, because `ops-I10`'s argument is that this never
        // happens by accident — and a line in the startup banner is what makes an
        // accident visible to whoever is looking at the logs.
        println!("  tcp        {address}  (opted in — access control is the gateway's)");
    }

    serve_on(socket, listen, ready_file, registry)?;
    Ok(())
}
