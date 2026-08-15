//! `aperture describe <db>`.
//!
//! Metadata and the schema, both read without opening fjall — the sidecar for one and
//! the embedded copy for the other. A server holding the database is no obstacle.

use crate::{CliError, cli::Format, commands, output};

/// # Errors
///
/// [`CliError::Store`] if there is no such database or its sidecar cannot be read.
pub fn run(root: &std::path::Path, name: &str, format: Format) -> Result<String, CliError> {
    let catalog = commands::readable(root)?;
    let entry = catalog.get(name)?;
    let schema = aperture_store::schema_doc::read(&entry.path).ok();

    Ok(match format {
        Format::Json => {
            let mut value = output::entry_json(&entry);
            if let (serde_json::Value::Object(map), Some(doc)) = (&mut value, &schema) {
                map.insert(
                    "schema".to_owned(),
                    serde_json::to_value(doc).unwrap_or(serde_json::Value::Null),
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

            if let Some(doc) = &schema {
                out.push('\n');
                out.push_str(&output::schema_table(doc));
            }

            out
        }
    })
}
