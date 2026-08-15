//! `aperture db rm <db>`.

use std::path::Path;

use crate::{
    CliError,
    commands::{self, Route},
};

/// # Errors
///
/// [`CliError::Refused`] if a running server declines — including a database a session
/// still holds, which is contention rather than a state and ends when the session does
/// — [`CliError::RootHeld`] if none is listening and something else holds the root, or
/// whatever the catalog reports.
pub fn run(root: &Path, socket: &Path, name: &str) -> Result<(), CliError> {
    match commands::route(root, socket)? {
        // The server closes the store before deleting the directory it was holding;
        // this process has nothing open to close.
        Route::Server(mut server) => Ok(server.remove(name)?),
        Route::Local(catalog, _lock) => Ok(catalog.remove(name)?),
    }
}
