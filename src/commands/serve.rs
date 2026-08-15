//! `aperture serve`.
//!
//! Owns the store root (`ops-I1`) and serves every database under it. **Unix socket
//! only** — `ops-I10` is default-closed, and a server that binds a network interface
//! because nobody said not to is the failure that rule exists to prevent.

use std::{path::Path, sync::Arc};

use aperture_server::{Database, protocol, serve_unix};
use aperture_store::store::FjallDb;

use crate::{CliError, code_index, commands};

/// # Errors
///
/// [`CliError::RootHeld`] if another server owns the root, or whatever binding or
/// opening reports.
pub fn run(root: &Path, socket: &Path, ready_file: Option<&Path>) -> Result<(), CliError> {
    // Held for the process's life: the lock *is* the ownership, so it is taken before
    // anything is opened and released only when the server exits.
    let (catalog, _lock) = commands::exclusive(root)?;

    let schema = code_index::schema();
    let fingerprint = protocol::provisional_fingerprint(&schema);

    let listing = catalog.list()?;

    let mut databases = Vec::new();
    for entry in &listing.entries {
        let db = FjallDb::open(&entry.path)?;
        databases.push(Arc::new(Database::new(
            entry.name().to_owned(),
            db,
            code_index::schema(),
        )));
    }

    println!("aperture serve");
    println!("  data dir   {}", root.display());
    println!("  socket     {}", socket.display());
    println!("  protocol   {}", protocol::VERSION);
    println!("  schema     {fingerprint:#018x}  (provisional — see PLAN Phase 8)");

    if databases.is_empty() {
        // Said plainly rather than served silently: a server with nothing to serve is
        // almost always a wrong `--data-dir`, and the fix is one command away.
        println!("  databases  none — `aperture create <name>` makes one");
    } else {
        println!("  databases  {}", databases.len());
        for entry in &listing.entries {
            println!("    {:<20} {}", entry.name(), entry.status());
        }
    }

    for problem in &listing.problems {
        eprintln!("warning: {problem}");
    }

    serve_unix(socket, ready_file, databases)?;
    Ok(())
}
