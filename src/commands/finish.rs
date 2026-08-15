//! `aperture finish <db>`.

use aperture_store::catalog::Finished;

use crate::{CliError, code_index, commands};

/// # Errors
///
/// [`CliError::RootHeld`] if a server owns the root, or whatever sealing reports —
/// including [`StoreError::EmptyDatabase`](aperture_store::error::StoreError::EmptyDatabase)
/// for a database with no facts.
pub fn run(
    root: &std::path::Path,
    name: &str,
    allow_zero_facts: bool,
) -> Result<Finished, CliError> {
    let (catalog, _lock) = commands::exclusive(root)?;
    Ok(catalog.finish(name, &code_index::schema(), allow_zero_facts)?)
}
