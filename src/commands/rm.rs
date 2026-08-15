//! `aperture db rm <db>`.

use crate::{CliError, commands};

/// # Errors
///
/// [`CliError::RootHeld`] if a server owns the root, or whatever the catalog reports.
pub fn run(root: &std::path::Path, name: &str) -> Result<(), CliError> {
    let (catalog, _lock) = commands::exclusive(root)?;
    Ok(catalog.remove(name)?)
}
