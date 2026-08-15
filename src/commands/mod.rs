//! One module per command, and one rule they all follow: **a command never opens a
//! store root a server holds**.
//!
//! `ops-I1` gives one process ownership of a root, and §2 is explicit that there is
//! no silent fallback from "connect" to "open directly" — a bare name always means
//! "ask the local server". Until lifecycle commands route through the server
//! ([9d](../../PLAN.md)), the honest interim is to take the root lock and say so when
//! it is held, rather than to open the directory anyway.

pub mod create;
pub mod describe;
pub mod finish;
pub mod list;
pub mod rm;
pub mod serve;

use std::path::Path;

use aperture_store::{catalog::Catalog, error::StoreError};

use crate::CliError;

/// Open the store root and take exclusive ownership of it.
///
/// # Errors
///
/// [`CliError::RootHeld`] with an actionable message if a server holds it — which is
/// `ops-I1` refusing rather than a lock to wait on.
pub fn exclusive(root: &Path) -> Result<(Catalog, aperture_store::catalog::RootLock), CliError> {
    let catalog = Catalog::open(root)?;

    match catalog.lock() {
        Ok(lock) => Ok((catalog, lock)),
        Err(StoreError::RootHeld { .. }) => Err(CliError::RootHeld {
            root: root.to_path_buf(),
        }),
        Err(other) => Err(other.into()),
    }
}

/// Open the store root for reading only.
///
/// Takes **no lock**: enumeration reads sidecars and never opens fjall (`ops-I7`), so
/// it works perfectly well while a server owns every database under the root. That is
/// the whole point of the filesystem being the catalog.
///
/// # Errors
///
/// [`CliError::Store`] if the root cannot be read.
pub fn readable(root: &Path) -> Result<Catalog, CliError> {
    Ok(Catalog::open(root)?)
}
