//! Rendering, on the client side.
//!
//! The server never produces JSON — it carries the binary format and nothing else —
//! so every human- or script-readable shape is made here. That is a decision from the
//! original brief and it is why `--format` is a flag on a command rather than a field
//! in a request.

use std::fmt::Write as _;

use aperture_store::{catalog::Entry, schema_doc::SchemaDoc};

/// A table with a header, aligned to its widest cell.
///
/// Hand-rolled rather than a crate: it is thirty lines, and the alternative is a
/// dependency in a tool whose whole output surface is six columns.
#[must_use]
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.len());
            }
        }
    }

    let mut out = String::new();

    for (index, header) in headers.iter().enumerate() {
        let _ = write!(
            out,
            "{:<width$}  ",
            header.to_uppercase(),
            width = widths[index]
        );
    }
    out.push('\n');

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let _ = write!(out, "{:<width$}  ", cell, width = widths[index]);
        }
        out.push('\n');
    }

    out
}

/// A fact count, a byte count, or `-` for one nobody has measured.
///
/// The dash is load-bearing: a Writable database's counts are genuinely unknown until
/// `finish` walks it, and printing `0` would be a claim rather than an absence.
#[must_use]
pub fn measured(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |n| n.to_string())
}

/// A fingerprint, short enough to read and long enough to tell two apart.
#[must_use]
pub fn short_fingerprint(value: Option<u64>) -> String {
    value.map_or_else(
        || "-".to_owned(),
        |n| format!("{:016x}", n)[..12].to_owned(),
    )
}

/// Epoch milliseconds as a UTC timestamp.
///
/// Civil-date arithmetic rather than a dependency, and the arithmetic is the standard
/// days-from-civil inverse. It is worth the thirty lines because this is the one field
/// a person reads and cannot compute in their head.
#[must_use]
pub fn timestamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));

    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`, which is the standard way to do this without a
/// calendar library.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;

    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// One database as JSON — the sidecar, plus where it is.
#[must_use]
pub fn entry_json(entry: &Entry) -> serde_json::Value {
    let mut value = serde_json::to_value(&entry.meta).unwrap_or(serde_json::Value::Null);

    if let serde_json::Value::Object(map) = &mut value {
        map.insert(
            "path".to_owned(),
            serde_json::Value::String(entry.path.display().to_string()),
        );
    }

    value
}

/// A schema as a person reads it: one predicate per line, in id order.
#[must_use]
pub fn schema_table(doc: &SchemaDoc) -> String {
    use aperture_store::schema_doc::TypeDoc;

    fn render(ty: &TypeDoc) -> String {
        match ty {
            TypeDoc::Int => "int".to_owned(),
            TypeDoc::Str => "string".to_owned(),
            TypeDoc::Fact { name, .. } => name.clone(),
            TypeDoc::Record { fields } => format!(
                "{{ {} }}",
                fields
                    .iter()
                    .map(|field| format!("{} : {}", field.name, render(&field.ty)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    let rows: Vec<Vec<String>> = doc
        .predicates
        .iter()
        .map(|predicate| {
            let signature = match &predicate.value {
                Some(value) => format!("{} -> {}", render(&predicate.key), render(value)),
                None => render(&predicate.key),
            };
            vec![predicate.id.to_string(), predicate.name.clone(), signature]
        })
        .collect();

    table(&["id", "predicate", "type"], &rows)
}
