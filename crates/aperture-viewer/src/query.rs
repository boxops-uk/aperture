//! **Every question this viewer asks, in one place.**
//!
//! One module for the same reason `aperture_cli::workload` is one module: a query
//! written where it is used is a query nobody can cost, and the whole argument of
//! [phase 11](../../../docs/phase-11-code-search.md) is about which of these seek and
//! which scan. Each one below says which, and a `:plan` against the built-in schema
//! is what settles it.
//!
//! # The one thing that is not a query
//!
//! A reference's *file* comes back as a `FactId`, not a path, because `src.File`'s
//! key is a bare string and a reference has no field to read through — `R.to.name`
//! works because `src.Decl` has a field called `name`. So find-references answers ids
//! and [`Paths`] resolves them, from one pass over `src.File` at startup.
//!
//! That is a language gap rather than a schema one, and it is small: a fetch through
//! a reference already exists (`Source::Fetch`), and what is missing is a way to
//! *name* the whole key of the fetched fact. Recorded here because the workaround is
//! invisible from the outside and would otherwise look like a design.

use std::collections::HashMap;

use aperture_client::{ClientError, Connection};
use aperture_wire::{Desc, WireRef, WireValue};

/// A row, **read by field name**.
///
/// Not by position, and that distinction cost an afternoon: a query's head record has
/// its fields **sorted by name at lowering** (`docs/conventions.md`), so
/// `{line = …, col = …, length = …}` arrives as `col, length, line`. Reading
/// positionally works until somebody renames a field, and then it silently reads the
/// wrong column — which is exactly the failure a row that carries no names invites.
///
/// The descriptor carries the names, so the answer is to use them.
pub struct Row {
    fields: Vec<(String, WireValue)>,
}

impl Row {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&WireValue> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }

    #[must_use]
    pub fn int(&self, name: &str) -> i64 {
        as_int(self.get(name))
    }

    #[must_use]
    pub fn str(&self, name: &str) -> &str {
        as_str(self.get(name))
    }

    #[must_use]
    pub fn id(&self, name: &str) -> Option<u64> {
        as_id(self.get(name))
    }
}

/// Pair a result's values with the names its descriptor gives them.
fn named(desc: &Desc, values: Vec<WireValue>) -> Vec<Row> {
    let names: Vec<String> = match desc {
        Desc::Record(fields) => fields.iter().map(|(name, _)| name.clone()).collect(),
        // A head that is not a record is one unnamed value — `X where …`. Nothing here
        // asks for one, but a row is still a row.
        _ => vec![String::new()],
    };

    values
        .into_iter()
        .map(|value| {
            let values = match value {
                WireValue::Record(fields) => fields.into_vec(),
                other => vec![other],
            };

            Row {
                fields: names.iter().cloned().zip(values).collect(),
            }
        })
        .collect()
}

/// Escape a string for a focus literal.
///
/// Only the two characters the grammar's string rule cares about. A path or an
/// identifier out of a real index contains neither, so this is a guard against
/// input rather than a transformation of it — but the guard has to exist, because
/// the alternative is a caller choosing what the query says.
#[must_use]
pub fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // Control characters cannot appear in a focus string at all, so they are
            // dropped rather than escaped: a path containing one is not a path.
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every file in the index, `FactId` and path together.
///
/// **One query, at startup**, because every page needs it: the browser lists paths,
/// and find-references answers file *ids* that have to become paths before anyone can
/// click them. On `dotnet/runtime` this is ~32,000 rows.
pub struct Paths {
    by_id: HashMap<u64, String>,
    sorted: Vec<String>,
}

impl Paths {
    /// `{id = X, path = P} where X = src.File P` — a scan of the smallest predicate
    /// in the source layer, projecting each row twice: as the identity a reference
    /// names, and as the string the key holds.
    ///
    /// # Errors
    ///
    /// As any query.
    pub fn load(connection: &mut Connection) -> Result<Paths, ClientError> {
        let rows = drain(connection, "{id = X, path = P} where X = src.File P")?;

        let mut by_id = HashMap::with_capacity(rows.len());
        let mut sorted = Vec::with_capacity(rows.len());

        for row in rows {
            let (Some(id), path) = (row.id("id"), row.str("path")) else {
                continue;
            };

            by_id.insert(id, path.to_owned());
            sorted.push(path.to_owned());
        }

        sorted.sort();
        Ok(Paths { by_id, sorted })
    }

    #[must_use]
    pub fn path_of(&self, id: u64) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// The paths under `prefix`, at most `limit` of them, and whether there are more.
    ///
    /// Answered from memory rather than by a query, and that is a deliberate
    /// exception to this module's rule: the whole listing is already here, a
    /// directory listing wants it grouped rather than flat, and a query per keystroke
    /// of a path box would be a query that reads what is in front of it.
    #[must_use]
    pub fn under(&self, prefix: &str, limit: usize) -> (Vec<String>, bool) {
        let matched: Vec<String> = self
            .sorted
            .iter()
            .filter(|path| path.starts_with(prefix))
            .take(limit + 1)
            .cloned()
            .collect();

        let more = matched.len() > limit;
        (matched.into_iter().take(limit).collect(), more)
    }
}

/// A file's source text, one row per line.
///
/// `src.Line` is keyed `{file, line}`, so this **seeks**: one `src.File` seek for the
/// path, then a range over that file's lines. It is the largest predicate in the
/// index by bytes, and reading a file's worth of it is the one query here whose cost
/// is proportional to the page rather than to the database.
#[must_use]
pub fn file_text(path: &str) -> String {
    format!(
        "{{line = L.line, text = L.value}} where L = src.Line {{file = src.File {}}}",
        literal(path)
    )
}

/// Every cross-reference in a file, in the order a renderer splices them.
///
/// **The query the whole phase is about.** Against `src.Ref` — keyed `{to, file, at}`
/// so that find-references seeks — this is a scan of 4.9M rows. `src.FileXRef` is the
/// same references keyed `{file, at, to}`, so it seeks, and position leads the rest
/// of the key so the rows arrive sorted by line and column.
#[must_use]
pub fn file_xrefs(path: &str) -> String {
    format!(
        "{{line = R.at.line, col = R.at.col, length = R.at.length, name = R.to.name, \
         target_line = R.to.line, target_file = R.to.module.file}} \
         where R = src.FileXRef {{file = src.File {}}}",
        literal(path)
    )
}

/// What a file declares, for an outline.
///
/// Three seeks: the file, its modules, their declarations. `src.Module` is keyed
/// `{file, name}` and `src.Decl` `{module, name, line}`, so each level narrows on the
/// one above it.
#[must_use]
pub fn file_outline(path: &str) -> String {
    format!(
        "{{name = D.name, line = D.line, kind = D.value}} \
         where D = src.Decl {{module = src.Module {{file = src.File {}}}}}",
        literal(path)
    )
}

/// Symbols whose name starts with `term`, case-insensitively.
///
/// A prefix seek into `src.SearchByLowerName`, which exists because focus has no
/// `toLower` to apply at read time. The caller lower-cases the term; the rows carry
/// the declaration's real name.
///
/// **Everything after the seek is a fetch, and that is not a style choice.** This was
/// written `…, to = D}}; D = src.Decl {{module = M}}` — read the declaration by
/// binding it — and it cost **30 seconds** against the 25M-fact index where the shape
/// below costs 2 ms.
///
/// The reason is worth knowing, because it is sharper than "statement order decides
/// the plan": a row bind **claims** its variable ([`flatten`]'s `Claims`), so the
/// statement saying what `D` *is* has to run before anything that reads it — and no
/// reordering can rescue that, because it is not an ordering question. `src.Decl`
/// scanned its 888,177 rows and the seek became a residual on each one.
///
/// Reading `D.name` instead makes `D` a reference the seek binds and each read a point
/// read through it, which is the plan the query obviously means:
///
/// ```text
/// src.SearchByLowerName seek[name = "…".., to = _]
/// fetch src.Decl
/// fetch src.Module
/// ```
///
/// [`flatten`]: aperture_client
#[must_use]
pub fn search(term: &str) -> String {
    format!(
        "{{name = D.name, line = D.line, kind = D.value, file = D.module.file}} \
         where src.SearchByLowerName {{name = {}.., to = D}}",
        literal(&term.to_lowercase())
    )
}

/// Everywhere a named declaration is used.
///
/// Two seeks: the name, then `src.Ref` by target — which is what leading with `to`
/// bought, and what made this answerable at all (`bench/FINDINGS.md` §11).
///
/// The `file` field comes back as an id; [`Paths`] turns it into a path.
#[must_use]
pub fn references(name: &str) -> String {
    format!(
        "{{file = R.file, line = R.at.line, col = R.at.col, length = R.at.length}} \
         where src.SearchByName {{name = {}, to = D}}; R = src.Ref {{to = D}}",
        literal(name)
    )
}

/// Where a named declaration is defined, and what it is.
///
/// Fetches rather than binds, for the reason [`search`] spells out at length: binding
/// `D` as a row claims it, which forces `src.Decl`'s level first and turns the seek
/// into a residual over every declaration in the database.
#[must_use]
pub fn definition(name: &str) -> String {
    format!(
        "{{name = D.name, line = D.line, kind = D.value, file = D.module.file}} \
         where src.SearchByName {{name = {}, to = D}}",
        literal(name)
    )
}

/// A declaration's span — where its name starts, and where it ends.
#[must_use]
pub fn definition_span(name: &str) -> String {
    format!(
        "{{col = S.col, end_line = S.endLine, end_col = S.endCol}} \
         where src.SearchByName {{name = {}, to = D}}; S = src.DeclSpan {{decl = D}}",
        literal(name)
    )
}

/// **Every query shape this viewer can ask, with sample arguments.**
///
/// The census `no_page_reads_a_predicate_whole` profiles. Hand-written, and that is
/// deliberate for the reason `aperture_engine::diag::Code::ALL` is hand-written:
/// adding a query and not adding it here is meant to be visible, and the count below
/// is what makes it visible rather than merely possible to notice.
///
/// The arguments are samples, and they matter less than they look: what the guard
/// checks is whether a step reads a predicate **whole**, which is a property of the
/// plan rather than of the arguments or of how many rows are in the database.
#[must_use]
pub fn census() -> Vec<(&'static str, String)> {
    let census = vec![
        ("file_text", file_text("a.cs")),
        ("file_xrefs", file_xrefs("a.cs")),
        ("file_outline", file_outline("a.cs")),
        ("search", search("x")),
        ("references", references("X")),
        ("definition", definition("X")),
        ("definition_span", definition_span("X")),
    ];

    // Every query builder in this module except `Paths::load`, which is a scan on
    // purpose — see the guard, which names it.
    assert_eq!(
        census.len(),
        7,
        "a query was added to this module without being added to the census"
    );

    census
}

/// Run a query and take every row.
///
/// # Errors
///
/// As any query.
pub fn drain(connection: &mut Connection, query: &str) -> Result<Vec<Row>, ClientError> {
    let mut rows = connection.query(query)?;
    let desc = rows.desc().clone();
    let values = connection.drain(&mut rows)?;

    Ok(named(&desc, values))
}

/// Run a query and take one page, with the token to continue it.
///
/// # Errors
///
/// As any query.
pub fn page(
    connection: &mut Connection,
    query: &str,
    limit: u64,
    cursor: Option<&[u8]>,
) -> Result<(Vec<Row>, Option<Vec<u8>>), ClientError> {
    let mut rows = connection.query_page(query, limit, cursor)?;
    let desc = rows.desc().clone();
    let values = connection.drain(&mut rows)?;
    let token = rows.resume_token().map(<[u8]>::to_vec);

    Ok((named(&desc, values), token))
}

// ---- reading a row ---------------------------------------------------------

#[must_use]
pub fn as_int(value: Option<&WireValue>) -> i64 {
    match value {
        Some(WireValue::Int(n)) => *n,
        _ => 0,
    }
}

#[must_use]
pub fn as_str(value: Option<&WireValue>) -> &str {
    match value {
        Some(WireValue::Str(s)) => s,
        _ => "",
    }
}

#[must_use]
pub fn as_id(value: Option<&WireValue>) -> Option<u64> {
    match value {
        Some(WireValue::Ref(WireRef::Id(id))) => Some(id.raw()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path with a quote in it cannot end the string it is in.
    ///
    /// Nothing in a real index contains one, which is exactly why this is worth a
    /// test: the case that never happens is the case nobody notices is wrong, and a
    /// caller choosing what the query *says* rather than what it asks is the whole
    /// of the injection question.
    #[test]
    fn a_literal_cannot_be_escaped_from() {
        assert_eq!(literal(r#"a"b"#), r#""a\"b""#);
        assert_eq!(literal(r"a\b"), r#""a\\b""#);
        assert_eq!(literal("plain"), r#""plain""#);

        // A closing quote followed by a second statement is the shape that would
        // matter. What has to hold is that the only **unescaped** quotes are the two
        // delimiters — checking for a substring is not the same claim, since `\";`
        // contains `";` and is perfectly safe.
        let hostile = literal(r#"x"; src.File _; F = ""#);

        assert!(hostile.starts_with('"') && hostile.ends_with('"'));

        let inside = &hostile[1..hostile.len() - 1];
        let mut escaped = false;
        for c in inside.chars() {
            assert!(
                c != '"' || escaped,
                "an unescaped quote survived: {hostile}"
            );
            escaped = c == '\\' && !escaped;
        }
    }

    /// Control characters are dropped rather than escaped.
    #[test]
    fn control_characters_do_not_reach_the_query() {
        assert_eq!(literal("a\nb\tc"), r#""abc""#);
    }
}
