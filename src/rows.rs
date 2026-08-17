//! Query rows, rendered for a person or a script.
//!
//! **Client-side, always.** The wire carries the binary format and the server never
//! produces JSON — a decision from the original brief, and the reason `--format` is a
//! flag on a command rather than a field in a request.
//!
//! # Four of the five shapes stream, and the fifth says why it does not
//!
//! A [`Sink`] is handed rows one at a time, as they arrive, and writes as it goes.
//! [`RowFormat::Table`] is the exception: aligning columns needs the widest cell, and
//! the widest cell is not known until the last row. It buffers, and a result too large
//! to hold is a result to ask for in another shape — which is what `raw` and `count`
//! are for, and why `count` exists at all: a measurement of the *server* should not be
//! paying for this file.
//!
//! # JSON is shaped like the head, all the way down
//!
//! A row's structure is the head's, and the [`Desc`] the server sent describes it —
//! **recursively**, field names and all. So a nested record comes out as a nested
//! object rather than as a positional array: `{at = {line = L, col = C}}` reads as
//! `{"at": {"line": 4, "col": 19}}`, which is the shape somebody asked for when they
//! wrote the query. Rendering only the top level by name and the rest by position was
//! the easy half of the same job, and it made every reference-and-span row — the
//! interesting ones — arrive as numbers in an array.
//!
//! A **reference** renders as `"#predicate:sequence"`, the same string the table
//! shows. A raw `u64` would be the id's bytes rather than the id: nothing takes one as
//! input (focus names a fact by its key, not by its number), so the useful thing is the
//! one a person can read and a script can compare.

use std::io::Write;

use aperture_client::{Desc, WireValue};

use crate::cli::RowFormat;

/// Somewhere to put rows as they arrive.
pub struct Sink<W: Write> {
    out: W,
    format: RowFormat,
    /// What a row looks like, as the server described it.
    ///
    /// Held whole rather than reduced to column names, because JSON needs it whole:
    /// names live at every level of a record, not only the top one.
    desc: Desc,
    /// Column names, when the head is a record. Empty for a scalar head, which is one
    /// unnamed column.
    columns: Vec<String>,
    /// `Table` only — see the module docs for why this one shape holds on.
    buffered: Vec<Vec<String>>,
    rows: u64,
}

impl<W: Write> Sink<W> {
    /// Start a result, having seen its descriptor.
    ///
    /// Whatever has to be written before the first row is written **here**, rather
    /// than in a `begin` the caller has to remember: a two-step opening is a step
    /// somebody eventually skips, and the JSON that comes out of a skipped one is
    /// malformed in a way no row-level test would catch.
    ///
    /// # Errors
    ///
    /// Whatever writing reports.
    pub fn new(mut out: W, format: RowFormat, desc: &Desc) -> std::io::Result<Sink<W>> {
        let columns = match desc {
            Desc::Record(fields) => fields.iter().map(|(name, _)| name.clone()).collect(),
            _ => vec![],
        };

        if format == RowFormat::Json {
            write!(out, "[")?;
        }

        Ok(Sink {
            out,
            format,
            desc: desc.clone(),
            columns,
            buffered: vec![],
            rows: 0,
        })
    }

    /// One row.
    ///
    /// # Errors
    ///
    /// Whatever writing reports — including a closed pipe, which is how `| head`
    /// ends a query rather than a fault to report.
    pub fn row(&mut self, value: &WireValue) -> std::io::Result<()> {
        match self.format {
            RowFormat::Count => {}

            RowFormat::Raw => {
                let cells = self.cells(value);
                writeln!(self.out, "{}", cells.join("\t"))?;
            }

            RowFormat::Json => {
                if self.rows > 0 {
                    write!(self.out, ",")?;
                }
                write!(self.out, "\n  {}", json(value, &self.desc))?;
            }

            RowFormat::Jsonl => writeln!(self.out, "{}", json(value, &self.desc))?,

            RowFormat::Table => {
                let cells = self.cells(value);
                self.buffered.push(cells);
            }
        }

        self.rows += 1;
        Ok(())
    }

    /// Finish, and answer with the number of rows written.
    ///
    /// # Errors
    ///
    /// Whatever writing reports.
    pub fn end(mut self) -> std::io::Result<u64> {
        match self.format {
            RowFormat::Count => writeln!(self.out, "{}", self.rows)?,

            RowFormat::Json => {
                if self.rows > 0 {
                    writeln!(self.out)?;
                }
                writeln!(self.out, "]")?;
            }

            RowFormat::Table => {
                let headers: Vec<&str> = if self.columns.is_empty() {
                    vec!["value"]
                } else {
                    self.columns.iter().map(String::as_str).collect()
                };

                write!(
                    self.out,
                    "{}",
                    crate::output::table(&headers, &self.buffered)
                )?;
                writeln!(self.out, "{} row(s)", self.rows)?;
            }

            RowFormat::Raw | RowFormat::Jsonl => {}
        }

        self.out.flush()?;
        Ok(self.rows)
    }

    /// A row's cells: a record's fields spread across columns, anything else in one.
    fn cells(&self, value: &WireValue) -> Vec<String> {
        match value {
            WireValue::Record(fields) if !self.columns.is_empty() => {
                fields.iter().map(render).collect()
            }
            other => vec![render(other)],
        }
    }
}

/// One value, as a person reads it.
///
/// A reference prints as `#predicate:sequence` rather than as its raw `u64`, because
/// that is what a [`FactId`](aperture_schema::id::FactId) *is* — a snowflake, the
/// owning predicate in the high bits and a per-predicate sequence in the low
/// ([I11](../docs/invariants.md#i11)) — and a sixteen-digit number hides both halves.
#[must_use]
pub fn render(value: &WireValue) -> String {
    match value {
        WireValue::Int(n) => n.to_string(),
        WireValue::Str(text) => text.clone(),

        WireValue::Ref(aperture_client::WireRef::Id(id)) => {
            format!("#{}:{}", id.predicate().0, id.sequence())
        }

        // A nested reference is what a *producer* sends, never what a server answers
        // with — stored, a reference is a `FactId` and nothing else. Rendered anyway
        // rather than panicked on: a client that cannot print what it decoded is a
        // worse bug report than one that prints something odd.
        WireValue::Ref(aperture_client::WireRef::Nested(fact)) => {
            format!("<{}>", render(&fact.key))
        }

        WireValue::Record(fields) => {
            let cells: Vec<String> = fields.iter().map(render).collect();
            format!("{{{}}}", cells.join(", "))
        }
    }
}

/// One value as JSON, named by the descriptor that came with it.
///
/// The descriptor is what makes a record an object rather than an array, at every
/// level. Where the two disagree — a record the descriptor does not describe as one,
/// or one of a different width — the value wins and comes out positionally: a row that
/// arrived is a row worth printing, and a mismatch is a server bug better reported by
/// odd-looking output than by a panic on a data path.
fn json(value: &WireValue, desc: &Desc) -> String {
    match (value, desc) {
        (WireValue::Int(n), _) => n.to_string(),
        (WireValue::Str(text), _) => json_string(text),

        (WireValue::Ref(aperture_client::WireRef::Id(id)), _) => {
            json_string(&format!("#{}:{}", id.predicate().0, id.sequence()))
        }

        // What a producer sends and a server never answers with; printed rather than
        // refused, for the reason `render` gives.
        (WireValue::Ref(aperture_client::WireRef::Nested(fact)), _) => json(&fact.key, desc),

        (WireValue::Record(fields), Desc::Record(named)) if fields.len() == named.len() => {
            let pairs: Vec<String> = fields
                .iter()
                .zip(named.iter())
                .map(|(field, (name, desc))| {
                    format!("{}: {}", json_string(name), json(field, desc))
                })
                .collect();

            format!("{{{}}}", pairs.join(", "))
        }

        (WireValue::Record(fields), _) => {
            let cells: Vec<String> = fields.iter().map(|field| json(field, &Desc::Str)).collect();
            format!("[{}]", cells.join(", "))
        }
    }
}

/// A JSON string, escaped by hand.
///
/// `serde_json` is already here and would do it — but it would mean building a `Value`
/// per row on a path whose whole point is not to, and the escape rules for a string are
/// six characters and a control range.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');

    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }

    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: Vec<WireValue>) -> WireValue {
        WireValue::Record(fields.into())
    }

    #[test]
    fn a_reference_prints_as_the_snowflake_it_is() {
        // Predicate 3, sequence 7 — the two halves an id is made of, and the reason a
        // raw `u64` would be the wrong thing to show.
        let id = aperture_schema::id::FactId::new(aperture_schema::schema::PredicateId(3), 7)
            .expect("a fact id");

        assert_eq!(
            render(&WireValue::Ref(aperture_client::WireRef::Id(id))),
            "#3:7"
        );
    }

    #[test]
    fn a_scalar_head_is_one_unnamed_column() {
        let mut out = vec![];
        let mut sink = Sink::new(&mut out, RowFormat::Table, &Desc::Str).unwrap();

        sink.row(&WireValue::Str("a.py".to_owned())).unwrap();
        sink.row(&WireValue::Str("b.py".to_owned())).unwrap();
        let rows = sink.end().unwrap();

        let text = String::from_utf8(out).unwrap();
        assert_eq!(rows, 2);
        assert!(text.contains("VALUE"), "{text}");
        assert!(text.contains("a.py"), "{text}");
        assert!(text.contains("2 row(s)"), "{text}");
    }

    #[test]
    fn a_record_head_spreads_across_columns() {
        let desc = Desc::Record(Box::from([
            ("at".to_owned(), Desc::Int),
            ("what".to_owned(), Desc::Str),
        ]));

        let mut out = vec![];
        let mut sink = Sink::new(&mut out, RowFormat::Raw, &desc).unwrap();

        sink.row(&record(vec![
            WireValue::Int(12),
            WireValue::Str("key_of".to_owned()),
        ]))
        .unwrap();
        sink.end().unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "12\tkey_of\n");
    }

    /// The JSON is a **document**, not a line per row: it opens, separates and closes,
    /// so a script can pipe it straight into a parser — and it still streams, because
    /// nothing has to be known about row *n+1* to write row *n*.
    #[test]
    fn json_is_one_document_written_incrementally() {
        let desc = Desc::Record(Box::from([
            ("at".to_owned(), Desc::Int),
            ("what".to_owned(), Desc::Str),
        ]));

        let mut out = vec![];
        let mut sink = Sink::new(&mut out, RowFormat::Json, &desc).unwrap();

        sink.row(&record(vec![
            WireValue::Int(1),
            WireValue::Str("a\"b".to_owned()),
        ]))
        .unwrap();
        sink.row(&record(vec![
            WireValue::Int(2),
            WireValue::Str("c".to_owned()),
        ]))
        .unwrap();
        sink.end().unwrap();

        let text = String::from_utf8(out).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");

        assert_eq!(parsed[0]["at"], 1);
        assert_eq!(parsed[0]["what"], "a\"b");
        assert_eq!(parsed[1]["what"], "c");
    }

    /// **A row is shaped like its head, all the way down.**
    ///
    /// The interesting rows in a code index are nested — a span inside a reference,
    /// a file inside a declaration — and rendering only the top level by name left
    /// exactly those arriving as anonymous arrays. The descriptor names every level,
    /// so this uses it at every level.
    #[test]
    fn a_nested_record_is_a_nested_object() {
        let desc = Desc::Record(Box::from([
            ("file".to_owned(), Desc::Str),
            (
                "at".to_owned(),
                Desc::Record(Box::from([
                    ("line".to_owned(), Desc::Int),
                    ("col".to_owned(), Desc::Int),
                ])),
            ),
        ]));

        let mut out = vec![];
        let mut sink = Sink::new(&mut out, RowFormat::Json, &desc).unwrap();

        sink.row(&record(vec![
            WireValue::Str("store/codec.py".to_owned()),
            record(vec![WireValue::Int(4), WireValue::Int(19)]),
        ]))
        .unwrap();
        sink.end().unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).expect("valid JSON");

        assert_eq!(parsed[0]["at"]["line"], 4);
        assert_eq!(parsed[0]["at"]["col"], 19);
        assert_eq!(parsed[0]["file"], "store/codec.py");
    }

    /// A reference is the string the table shows, not the number underneath it.
    ///
    /// Nothing takes a raw id as input — focus names a fact by its key — so the useful
    /// rendering is the one that shows which predicate it belongs to.
    #[test]
    fn a_reference_is_readable_in_json_too() {
        let id = aperture_schema::id::FactId::new(aperture_schema::schema::PredicateId(3), 7)
            .expect("a fact id");

        let mut out = vec![];
        let mut sink = Sink::new(&mut out, RowFormat::Jsonl, &Desc::Int).unwrap();
        sink.row(&WireValue::Ref(aperture_client::WireRef::Id(id)))
            .unwrap();
        sink.end().unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "\"#3:7\"\n");
    }

    /// **JSON Lines is what a page is**: each row stands alone, so three pages of one
    /// query concatenate into something a reader can still parse — which an array per
    /// page does not.
    #[test]
    fn jsonl_is_one_value_per_line() {
        let desc = Desc::Record(Box::from([
            ("at".to_owned(), Desc::Int),
            ("what".to_owned(), Desc::Str),
        ]));

        let mut out = vec![];
        let mut sink = Sink::new(&mut out, RowFormat::Jsonl, &desc).unwrap();

        sink.row(&record(vec![
            WireValue::Int(1),
            WireValue::Str("a".to_owned()),
        ]))
        .unwrap();
        sink.row(&record(vec![
            WireValue::Int(2),
            WireValue::Str("b".to_owned()),
        ]))
        .unwrap();
        let rows = sink.end().unwrap();

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(rows, 2);
        assert_eq!(lines.len(), 2, "one line each, and nothing around them");

        for (n, line) in lines.iter().enumerate() {
            let parsed: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(parsed["at"], n + 1);
        }
    }

    /// An empty result is still a valid document rather than nothing at all.
    #[test]
    fn an_empty_result_is_still_well_formed() {
        let mut out = vec![];
        let sink = Sink::new(&mut out, RowFormat::Json, &Desc::Str).unwrap();
        sink.end().unwrap();

        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!([])
        );
    }

    /// `count` writes the tally and nothing else — the shape a measurement of the
    /// *server* wants, since rendering is the client's cost and not the thing under
    /// test.
    #[test]
    fn count_renders_no_rows() {
        let mut out = vec![];
        let mut sink = Sink::new(&mut out, RowFormat::Count, &Desc::Str).unwrap();

        for n in 0..1000 {
            sink.row(&WireValue::Int(n)).unwrap();
        }
        sink.end().unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "1000\n");
    }
}
