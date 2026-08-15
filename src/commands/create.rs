//! `aperture create <name>`.

use std::path::Path;

use crate::{
    CliError, code_index,
    commands::{self, Route},
};

/// A database that now exists, however it was made.
///
/// One type for both doors, so the caller does not have to know which one answered —
/// which is [operations §5](../../docs/aperture-cli-design.md)'s rule that local and
/// remote are a property of the *address*, seen from the printing end.
pub struct Created {
    pub name: String,
    pub instance: String,
}

/// # Errors
///
/// [`CliError::Refused`] if a running server declines, [`CliError::RootHeld`] if none
/// is listening and something else holds the root, or whatever the catalog reports.
pub fn run(root: &Path, socket: &Path, name: &str) -> Result<Created, CliError> {
    let instance = match commands::route(root, socket)? {
        Route::Server(mut server) => server.create(name)?,

        Route::Local(catalog, _lock) => {
            let schema = code_index::schema();
            let fingerprint = aperture_wire::provisional_fingerprint(&schema);
            catalog.create(name, &schema, fingerprint)?.meta.instance
        }
    };

    Ok(Created {
        name: name.to_owned(),
        instance,
    })
}
