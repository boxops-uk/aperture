//! `aperture schema check | fingerprint | diff` — [operations §5](../../docs/aperture-cli-design.md).
//!
//! The three questions a schema can be asked **before any database holds one**, which
//! is what makes them worth having: a schema is a build artifact long before it is an
//! artifact's contents, and every one of these answers is cheaper to get from a file
//! than from a database that was created wrongly.
//!
//! Nothing here opens fjall or talks to a server. `diff` reads a database's *sidecar
//! and embedded copy* when given a name, which is `ops-I7` again — the filesystem is
//! the catalog, so a comparison works while a server holds everything under the root.

use std::path::{Path, PathBuf};

use aperture_schema::{
    fingerprint::{self, Compatibility, Identity},
    schema::Schema,
    syntax::resolve,
};

use crate::{CliError, cli::Format, commands, output};

/// Resolve a schema and say what came of it.
///
/// # Errors
///
/// [`CliError::Schema`] with the rendered reason: an import nothing answers, a syntax
/// error in any file, or a redeclaration in the union.
pub fn check(file: &Path, roots: &[PathBuf]) -> Result<String, CliError> {
    let resolved = read(file, roots)?;
    let identity = fingerprint::identity(&resolved.schema);

    let mut out = format!(
        "{} predicate(s) in {} file(s)\n",
        resolved.schema.len(),
        resolved.files.len()
    );

    // **Which files, in the order they were read.** The one question a checker can
    // answer that a compiler cannot: two roots holding a namespace of the same name is
    // a configuration problem, and it is invisible in the schema itself.
    for path in &resolved.files {
        out.push_str(&format!("  {}\n", path.display()));
    }

    out.push_str(&format!("fingerprint {:#018x}\n", identity.schema()));

    Ok(out)
}

/// Print a schema's fingerprint, and each predicate's.
///
/// # Errors
///
/// As [`check`].
pub fn print_fingerprint(
    file: &Path,
    roots: &[PathBuf],
    format: Format,
    canonical: bool,
) -> Result<String, CliError> {
    let resolved = read(file, roots)?;

    // The canonical form is what the number is *of*, and the thing to diff when two
    // implementations disagree about a schema they believe they share — so it is a flag
    // rather than a separate command, and it prints alone.
    if canonical {
        return Ok(fingerprint::identity(&resolved.schema)
            .canonical()
            .to_owned());
    }

    Ok(match format {
        Format::Json => format!(
            "{}\n",
            serde_json::to_string_pretty(&output::schema_json(&resolved.schema))
                .unwrap_or_default()
        ),
        Format::Table => output::schema_table(&resolved.schema),
    })
}

/// Compare two schemas — files, database names, or one of each.
///
/// # Errors
///
/// [`CliError::Schema`] if either side is a schema that does not resolve, or
/// [`CliError::Store`] if a named database cannot be read.
pub fn diff(before: &str, after: &str, root: &Path, roots: &[PathBuf]) -> Result<String, CliError> {
    let (old, old_from) = side(before, root, roots)?;
    let (new, new_from) = side(after, root, roots)?;

    let mut out = format!("{old_from}\n{new_from}\n\n");

    match old.compatibility(&new) {
        Compatibility::Identical => out.push_str("Identical\n"),

        Compatibility::Compatible { added } => {
            out.push_str(&format!("Compatible ({added} added)\n"));
            for name in new.predicates().keys() {
                if old.of(name).is_none() {
                    out.push_str(&format!("  + {name}\n"));
                }
            }
        }

        // **Per-predicate reasons, and the two reasons are different problems.** A
        // removed predicate breaks every query that names it; a modified one breaks
        // every *fact* already written, because a key's fields are positional. Neither
        // is repairable under subset containment, and saying which it is says whether
        // the schema or the data is the thing that moved.
        Compatibility::Breaking { broken } => {
            out.push_str(&format!("Breaking ({} predicate(s))\n", broken.len()));

            for name in &broken {
                match new.of(name) {
                    None => out.push_str(&format!("  - {name}  (removed)\n")),
                    Some(new_fingerprint) => out.push_str(&format!(
                        "  ~ {name}  (modified: {:016x} → {new_fingerprint:016x})\n",
                        old.of(name).unwrap_or_default()
                    )),
                }
            }

            let added = new
                .predicates()
                .keys()
                .filter(|name| old.of(name).is_none())
                .count();

            if added > 0 {
                out.push_str(&format!("  ({added} added, which is not the problem)\n"));
            }
        }
    }

    Ok(out)
}

/// One side of a diff: a schema file, or a database in the store root.
///
/// **A path wins over a name**, and the rule is "does this file exist" rather than
/// anything about the spelling: a database is named, not pathed, so the two cannot
/// collide except by somebody keeping a file named after a database in the working
/// directory — where reading the file is what they meant.
fn side(what: &str, root: &Path, roots: &[PathBuf]) -> Result<(Identity, String), CliError> {
    let path = Path::new(what);

    if path.is_file() {
        let resolved = read(path, roots)?;
        return Ok((
            fingerprint::identity(&resolved.schema),
            format!("{what}  (schema file)"),
        ));
    }

    let catalog = commands::readable(root)?;
    let entry = catalog.get(what)?;

    let Some(schema) = aperture_store::schema_doc::read(&entry.path)? else {
        return Err(CliError::Schema(format!(
            "`{what}` embeds no schema copy — it predates one being kept, and there is \
             nothing to compare"
        )));
    };

    Ok((
        fingerprint::identity(&schema),
        format!("{what}  (database, {})", entry.status()),
    ))
}

fn read(file: &Path, roots: &[PathBuf]) -> Result<resolve::Resolved, CliError> {
    resolve::resolve(file, roots).map_err(CliError::Schema)
}

/// The schema `create --schema <file>` should embed.
///
/// Here rather than in [`create`](super::create) because it is the same resolution the
/// three commands above do, and a `create` that resolved imports differently from
/// `schema check` would be a tool that passes its own checker and builds something else.
///
/// # Errors
///
/// As [`check`].
pub fn resolve_for_create(file: &Path, roots: &[PathBuf]) -> Result<Schema, CliError> {
    Ok(read(file, roots)?.schema)
}
