//! `aperture describe <db>`.
//!
//! Metadata and the schema, both read without opening fjall — the sidecar for one and
//! the embedded copy for the other. A server holding the database is no obstacle.

use aperture_store::catalog::{Intent, Selector};

use crate::{CliError, cli::Format, commands, output};

/// # Errors
///
/// [`CliError::Store`] if there is no such database or its sidecar cannot be read.
pub fn run(
    root: &std::path::Path,
    name: &str,
    format: Format,
    dump_schema: bool,
) -> Result<String, CliError> {
    let catalog = commands::readable(root)?;
    let entry = catalog.resolve(&Selector::parse(name)?, Intent::Read)?;

    // **`--schema` dumps the copy verbatim**, comments and all, because the thing worth
    // having is the text `create --schema` would take back — not this command's idea of
    // how to lay it out.
    if dump_schema {
        return Ok(
            aperture_store::schema_doc::source(&entry.path)?.unwrap_or_else(|| {
                format!("# `{name}` embeds no schema copy — it predates one being kept.\n")
            }),
        );
    }

    let embedded = aperture_store::schema_doc::read(&entry.path);

    Ok(match format {
        Format::Json => {
            let mut value = output::entry_json(&entry);

            if let serde_json::Value::Object(map) = &mut value {
                map.insert(
                    "schema".to_owned(),
                    match &embedded {
                        Ok(Some(schema)) => output::schema_json(schema),
                        Ok(None) => serde_json::Value::Null,
                        Err(problem) => serde_json::Value::String(problem.to_string()),
                    },
                );
            }

            format!(
                "{}\n",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            )
        }

        Format::Table => {
            let meta = &entry.meta;

            let facts = output::measured(meta.facts);
            let bytes = output::measured(meta.bytes);
            let content = meta.content_fingerprint.map_or_else(
                || "-  (recorded at finish)".to_owned(),
                |f| format!("{f:#018x}"),
            );

            let mut out = output::table(
                &["field", "value"],
                &[
                    vec!["name".into(), meta.name.clone()],
                    vec!["instance".into(), meta.instance.clone()],
                    vec!["status".into(), meta.status.to_string()],
                    vec!["path".into(), entry.path.display().to_string()],
                    vec![
                        "format".into(),
                        format!(
                            "codec {} / storage {}",
                            meta.format_codec, meta.format_storage
                        ),
                    ],
                    vec![
                        "schema".into(),
                        format!("{:#018x}", meta.schema_fingerprint),
                    ],
                    vec!["content".into(), content],
                    vec!["facts".into(), facts],
                    vec!["bytes".into(), bytes],
                    vec!["created".into(), output::timestamp(meta.created_at_ms)],
                ],
            );

            out.push('\n');

            // A copy that cannot be read is worth saying out loud rather than leaving
            // as an absence: this database can no longer be served, and the metadata
            // above gives no hint of it.
            match &embedded {
                Ok(Some(schema)) => out.push_str(&output::schema_table(schema)),
                Ok(None) => out.push_str("(no embedded schema copy)\n"),
                Err(problem) => {
                    out.push_str(&format!(
                        "the embedded schema copy is unreadable: {problem}\n"
                    ));
                }
            }

            out
        }
    })
}
