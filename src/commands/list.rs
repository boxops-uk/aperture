//! `aperture list`.
//!
//! Reads sidecars and nothing else (`ops-I7`), so it works while a server holds every
//! database under the root — which is the one thing this command must never fail to
//! do, since it is what someone runs when they are trying to find out what is going
//! on.

use aperture_store::catalog::Listing;

use crate::{CliError, cli::Format, commands, output};

/// # Errors
///
/// [`CliError::Store`] if the root cannot be read. A single unreadable database is a
/// *problem* in the listing rather than a failure of it.
pub fn run(root: &std::path::Path, format: Format) -> Result<String, CliError> {
    let listing = commands::readable(root)?.list()?;
    Ok(render(&listing, format))
}

fn render(listing: &Listing, format: Format) -> String {
    match format {
        Format::Json => {
            let value = serde_json::json!({
                "databases": listing.entries.iter().map(output::entry_json).collect::<Vec<_>>(),
                "problems": listing.problems.iter().map(ToString::to_string).collect::<Vec<_>>(),
            });
            format!(
                "{}\n",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            )
        }

        Format::Table => {
            let rows: Vec<Vec<String>> = listing
                .entries
                .iter()
                .map(|entry| {
                    vec![
                        entry.name().to_owned(),
                        entry.meta.instance.clone(),
                        entry.status().to_string(),
                        output::short_fingerprint(Some(entry.meta.schema_fingerprint)),
                        output::short_fingerprint(entry.meta.content_fingerprint),
                        output::measured(entry.meta.facts),
                        output::measured(entry.meta.bytes),
                        output::timestamp(entry.meta.created_at_ms),
                    ]
                })
                .collect();

            let mut out = if rows.is_empty() {
                "no databases\n".to_owned()
            } else {
                // No `externally_modified` column: there is no such field (`ops-I6`).
                output::table(
                    &[
                        "name", "instance", "status", "schema", "content", "facts", "bytes",
                        "created",
                    ],
                    &rows,
                )
            };

            for problem in &listing.problems {
                out.push_str(&format!("warning: {problem}\n"));
            }

            out
        }
    }
}
