//! `fjord finish <db>`.

use std::path::Path;

use fjord_store::catalog::Finished;

use crate::{
    CliError,
    commands::{self, Route, Target},
};

/// # Errors
///
/// [`CliError::Refused`] if a running server declines — including a database holding
/// no facts, which takes `--allow-zero-facts` whichever door it is sealed through —
/// [`CliError::RootHeld`] if none is listening and something else holds the root, or
/// whatever sealing reports.
pub fn run(root: &Path, target: &Target, allow_zero_facts: bool) -> Result<Finished, CliError> {
    match commands::route(root, target)? {
        // The server seals through the handle it already holds (`Catalog::finish_held`)
        // rather than opening a second one, and the identity that comes back is the
        // same one this process would have computed. `ops-I4` does not depend on which
        // door a build came through, and there is a store test that says so.
        //
        // Restated as the store's own type rather than passed through: the client does
        // not depend on a storage engine to be told what a fingerprint is, so the two
        // shapes are the same fields under different names, and this is where they meet.
        Route::Server(mut server) => {
            let sealed = server.finish(&target.database, allow_zero_facts)?;
            Ok(Finished {
                fingerprint: sealed.fingerprint,
                facts: sealed.facts,
                bytes: sealed.bytes,
                already_complete: sealed.already_complete,
            })
        }

        Route::Local(catalog, _lock) => {
            // **No schema passed, because this is where the wrong one used to be.** This
            // arm handed `catalog.finish` the tool's built-in schema regardless of what
            // the database embedded, and `identity::compute` looks a predicate up by
            // position — so sealing a database built against any other schema decoded
            // every stored key against whatever type sat at that position and recorded
            // an `ops-I4` identity over the result. `Catalog::finish` reads the embedded
            // copy itself, which is the only statement of it either door can reach.
            let selector = fjord_store::catalog::Selector::parse(&target.database)?;
            Ok(catalog.finish(&selector, allow_zero_facts)?)
        }
    }
}
