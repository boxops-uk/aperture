//! `aperture finish <db>`.

use std::path::Path;

use aperture_store::catalog::Finished;

use crate::{
    CliError, code_index,
    commands::{self, Route},
};

/// # Errors
///
/// [`CliError::Refused`] if a running server declines — including a database holding
/// no facts, which takes `--allow-zero-facts` whichever door it is sealed through —
/// [`CliError::RootHeld`] if none is listening and something else holds the root, or
/// whatever sealing reports.
pub fn run(
    root: &Path,
    socket: &Path,
    name: &str,
    allow_zero_facts: bool,
) -> Result<Finished, CliError> {
    match commands::route(root, socket)? {
        // The server seals through the handle it already holds (`Catalog::finish_held`)
        // rather than opening a second one, and the identity that comes back is the
        // same one this process would have computed. `ops-I4` does not depend on which
        // door a build came through, and there is a store test that says so.
        //
        // Restated as the store's own type rather than passed through: the client does
        // not depend on a storage engine to be told what a fingerprint is, so the two
        // shapes are the same fields under different names, and this is where they meet.
        Route::Server(mut server) => {
            let sealed = server.finish(name, allow_zero_facts)?;
            Ok(Finished {
                fingerprint: sealed.fingerprint,
                facts: sealed.facts,
                bytes: sealed.bytes,
                already_complete: sealed.already_complete,
            })
        }

        Route::Local(catalog, _lock) => {
            Ok(catalog.finish(name, &code_index::schema(), allow_zero_facts)?)
        }
    }
}
