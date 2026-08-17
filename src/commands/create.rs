//! `aperture create <name> [--schema <file>]`.

use std::path::{Path, PathBuf};

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
    /// Where its schema came from — what the line this prints says, so a database
    /// built against the wrong file is visible at the moment it is made rather than at
    /// the first query that reads nothing.
    pub schema: String,
}

/// # Errors
///
/// [`CliError::Schema`] if `--schema` names something that does not resolve,
/// [`CliError::RootHeld`] if no server is listening and something else holds the root,
/// or whatever the server or the catalog reports.
pub fn run(
    root: &Path,
    socket: &Path,
    name: &str,
    schema: Option<&Path>,
    schema_path: &[PathBuf],
) -> Result<Created, CliError> {
    // **Resolved here, on the machine holding the files**, whichever door answers. A
    // server asked to read a path would be a server asked to have the caller's
    // filesystem, and "no such file" on a host the caller cannot see is a worse error
    // than any this avoids.
    let resolved = schema
        .map(|file| commands::schema::resolve_for_create(file, schema_path))
        .transpose()?;

    let described = schema.map_or_else(
        || "the built-in schema".to_owned(),
        |file| file.display().to_string(),
    );

    let instance = match commands::route(root, socket)? {
        Route::Server(mut server) => {
            // The resolved schema as source rather than the entry file's text: what
            // the server embeds must be the union, or a database built through the
            // server would hold less than the same command built locally.
            let source = resolved
                .as_ref()
                .map(aperture_schema::syntax::print::print)
                .unwrap_or_default();

            server.create(name, &source)?
        }

        Route::Local(catalog, _lock) => {
            let schema = resolved.unwrap_or_else(code_index::schema);
            catalog.create(name, &schema)?.meta.instance
        }
    };

    Ok(Created {
        name: name.to_owned(),
        instance,
        schema: described,
    })
}
