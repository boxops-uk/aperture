//! `aperture create <name>`.

use aperture_store::catalog::Entry;

use crate::{CliError, code_index, commands};

/// # Errors
///
/// [`CliError::RootHeld`] if a server owns the root, or whatever the catalog reports.
pub fn run(root: &std::path::Path, name: &str) -> Result<Entry, CliError> {
    let (catalog, _lock) = commands::exclusive(root)?;

    let schema = code_index::schema();
    let fingerprint = aperture_server::protocol::provisional_fingerprint(&schema);

    Ok(catalog.create(name, &schema, fingerprint)?)
}
