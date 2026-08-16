//! **The queries the instruments measure, stated once.**
//!
//! Phase 10's S0. Every rung of the ladder — the in-process executor bench, the load
//! generator, the soak, the code-search mix — needs the same questions asked of the same
//! data, or a number from one rung cannot be compared with a number from another. They
//! each stated their own, which is how `loadgen` came to seek `files / 2` — a key that
//! exists only in a corpus it seeded itself, and exists in no real index at all.
//!
//! # Pivots are sampled, never computed
//!
//! A workload that seeks needs a key that is *there*. Against somebody's checkout there
//! is no arithmetic that lands on one, so [`Pivots`] carries values taken out of whatever
//! corpus is loaded. Sampling is deliberately **not** in this module: an in-process bench
//! has a `FjallDb` and a load generator has a socket, and pretending those are the same
//! would mean this module depending on both. What is shared is the shape and the
//! queries — which is the part that has to agree.
//!
//! # A workload states what it answers
//!
//! `aperture_engine::corpus` makes a `Supported` entry carry the rows it returns, for the
//! reason this needs too: a run that returned a different count did not measure what it
//! thought it did, and should say so rather than print a throughput figure. The rows a
//! given corpus answers with are not knowable here, so an instrument fixes them with one
//! unmeasured probe and holds every timed run to it — which is what
//! `examples/engine.rs` does.

/// The values a workload seeks for, taken out of the corpus that is loaded.
#[derive(Debug, Clone)]
pub struct Pivots {
    /// A file path that exists.
    pub file: String,
    /// Its directory, so a prefix seek covers a real run of adjacent keys.
    pub directory: String,
    /// A declaration name, for a denial that denies almost nothing.
    pub decl: String,
    /// A name `src.SearchByName` actually holds.
    pub search: String,
}

impl Pivots {
    /// Pivots from a sampled path and two sampled names.
    ///
    /// The directory is derived rather than sampled because it is not an independent
    /// fact: it has to be a prefix of a key that exists, and the only way to be sure of
    /// that is to cut it off one.
    #[must_use]
    pub fn new(
        file: impl Into<String>,
        decl: impl Into<String>,
        search: impl Into<String>,
    ) -> Pivots {
        let file = file.into();
        let directory = match file.rfind('/') {
            Some(cut) => file[..=cut].to_owned(),
            None => file.clone(),
        };

        Pivots {
            file,
            directory,
            decl: decl.into(),
            search: search.into(),
        }
    }

    /// Pivots for a corpus nothing could be sampled from — an empty database, or a
    /// probe that found no rows.
    ///
    /// Every value is one no real index holds, on purpose: a workload built on these
    /// answers zero rows, and zero rows against a corpus that has some is a loud
    /// failure rather than a quiet mis-measurement.
    #[must_use]
    pub fn unsampled() -> Pivots {
        Pivots::new("\u{0}none/\u{0}none", "\u{0}none", "\u{0}none")
    }
}

/// One question, and what asking it is meant to show.
#[derive(Debug, Clone)]
pub struct Workload {
    pub name: &'static str,
    pub focus: String,
    /// What it is here to exercise, printed beside the number so a table row says what
    /// it means without this file open next to it.
    pub about: &'static str,
    /// Stop after this many rows.
    ///
    /// For the workloads that cannot be run to completion: a join whose key field cannot
    /// be sought degenerates to a scan of the inner predicate *per outer row*. The point
    /// of such a workload is the `examined` column, which is legible from a capped run —
    /// provided the cap is printed, which is what this field is for.
    pub stop_at: Option<u64>,
}

impl Workload {
    fn new(name: &'static str, focus: String, about: &'static str) -> Workload {
        Workload {
            name,
            focus,
            about,
            stop_at: None,
        }
    }
}

/// The catalogue, in the order a ladder reads it: the control first, then seeks, then
/// scans by size, then the joins that price a key's field order.
#[must_use]
pub fn catalogue(pivots: &Pivots) -> Vec<Workload> {
    vec![
        // The vacuous-pass control, and the executor's own floor. Every binding folds,
        // so this is a plan with no steps: no scan, no seek, no store read, exactly one
        // row, and exactly zero rows examined. If this one ever reports work, the
        // instrument is lying about everything below it.
        Workload::new(
            "no data (control)",
            "X where X = 42".to_owned(),
            "a folded plan — no steps",
        ),
        Workload::new(
            "seek one file",
            format!("F where src.File F; F = \"{}\"", escape(&pivots.file)),
            "constant fold → one point",
        ),
        Workload::new(
            "seek prefix",
            format!(
                "F where src.File F; F = \"{}\"..",
                escape(&pivots.directory)
            ),
            "range seek, one directory",
        ),
        Workload::new(
            "search by name",
            format!(
                "D where src.SearchByName {{name = \"{}\", to = D}}",
                escape(&pivots.search)
            ),
            "the query a person types",
        ),
        Workload::new(
            "scan files",
            "F where src.File F".to_owned(),
            "smallest full scan",
        ),
        Workload::new(
            "scan modules",
            "N where src.Module {name = N}".to_owned(),
            "a key field off a record",
        ),
        Workload::new(
            "scan decls",
            "N where src.Decl {name = N}".to_owned(),
            "the mid-sized scan",
        ),
        Workload::new(
            "project record",
            "{at = D.line, what = D.name} where D = src.Decl _".to_owned(),
            "two fields, one row",
        ),
        // Reading *through* a reference is a `Source::Fetch` — one point read per row of
        // the level above. Both of these fetch, and that is the point of the pair:
        // projecting the fetched fact's own reference field costs exactly what
        // projecting its string costs, so the fetch is the whole price and what you take
        // off it afterwards is free.
        Workload::new(
            "fetch, project a ref",
            "{what = D.name, file = D.module.file} where D = src.Decl _".to_owned(),
            "fetch: a point read per row",
        ),
        Workload::new(
            "fetch, project a string",
            "{what = D.name, module = D.module.name} where D = src.Decl _".to_owned(),
            "the same fetch, read further",
        ),
        // **The pair that prices key field order.** A predicate's seekable prefix is its
        // key's leading fields, in the order the schema *declares* them, and nothing
        // about the query distinguishes a join that seeks from one that rescans the
        // whole predicate per outer row. The ratio between these is the price of the
        // declaration.
        //
        // This pair is why `src.Decl` is declared `{module, name, line}`. It used to be
        // `{line, module, name}` — alphabetical, by a convention `code_index` imposed on
        // itself — so the ordinary "declarations in this module" join was the *slow* arm
        // here, at 56,274 rows examined per row produced. It is the fast arm now, and
        // the slow one is a real query that still cannot narrow: `src.SearchByName` is
        // keyed for lookup *by name*, so reaching it by `to` is the same trap on a
        // predicate whose own order is right ([findings §2](../bench/FINDINGS.md)).
        Workload::new(
            "join on a leading field",
            "L where F = src.File _; src.Line {file = F, line = L}".to_owned(),
            "seekable: the reference leads the key",
        ),
        Workload::new(
            "join on a leading reference",
            "D where M = src.Module _; src.Decl {module = M, name = D}".to_owned(),
            "seekable since the reorder: the module leads the key",
        ),
        Workload {
            name: "join on a trailing field",
            focus: "N where D = src.Decl _; src.SearchByName {to = D, name = N}".to_owned(),
            about: "not seekable: `name` leads the key, and this joins on `to`",
            stop_at: Some(2_000),
        },
        Workload::new(
            "denial",
            format!(
                "N where src.Decl {{name = N}}; N != \"{}\"..",
                escape(&pivots.decl)
            ),
            "a residual per row, never a seek",
        ),
        Workload::new(
            "scan refs",
            "F where src.Ref {file = F}".to_owned(),
            "seven figures, nested key",
        ),
        Workload::new(
            "wide row",
            "{f = R.file, l = R.at.line, c = R.at.col} where R = src.Ref _".to_owned(),
            "three fields off a nested key",
        ),
        Workload::new(
            "join through two references",
            "{from = I.from.name, to = I.to.name} where I = src.Import _".to_owned(),
            "two fetches per row",
        ),
        Workload::new(
            "scan lines",
            "L where src.Line {line = L}".to_owned(),
            "the largest predicate",
        ),
    ]
}

/// Pivots sampled **over the wire**, for the instruments that have a connection rather
/// than a store.
///
/// One place rather than three, because "which key does this seek for" is exactly the
/// question the instruments were each answering differently: `loadgen` computed one from
/// `--files`, which lands on a real key only in a corpus it seeded itself, and against
/// somebody's checkout measures a miss. Taking the *last* row of a bounded page rather
/// than the first is deliberate — deep enough that a seek has somewhere to seek past, and
/// still answering when the predicate is shorter than the page.
///
/// # Errors
///
/// Whatever the connection reports. A corpus with no files is not an error here — it
/// answers [`Pivots::unsampled`], which makes every seek workload return nothing rather
/// than seek for something plausible that is not there.
pub fn sample(
    connection: &mut aperture_client::Connection,
) -> Result<Pivots, aperture_client::ClientError> {
    fn first_string(
        connection: &mut aperture_client::Connection,
        focus: &str,
        depth: usize,
    ) -> Result<Option<String>, aperture_client::ClientError> {
        let mut rows = connection.query(focus)?;
        let page = connection.take(&mut rows, depth)?;
        connection.cancel(&mut rows).ok();

        Ok(page.iter().rev().find_map(|value| match value {
            aperture_wire::WireValue::Str(text) => Some(text.clone()),
            _ => None,
        }))
    }

    let file = first_string(connection, "F where src.File F", 16_000)?;
    let decl = first_string(connection, "N where src.Decl {name = N}", 400_000)?;
    let search = first_string(connection, "N where src.SearchByName {name = N}", 400_000)?;

    Ok(match (file, decl) {
        (None, None) => Pivots::unsampled(),
        (file, decl) => {
            let decl = decl.unwrap_or_else(|| "\u{0}none".to_owned());
            let search = search.unwrap_or_else(|| decl.clone());
            Pivots::new(file.unwrap_or_else(|| "\u{0}none".to_owned()), decl, search)
        }
    })
}

/// One workload by name, for an instrument that draws a mix rather than a ladder.
///
/// Panics rather than answering `None`: the names are literals in this file, a mix that
/// asks for one that is gone is a mix that will silently measure a different population,
/// and there is no useful thing to do with the absence at run time.
#[must_use]
pub fn named(pivots: &Pivots, name: &str) -> Workload {
    catalogue(pivots)
        .into_iter()
        .find(|workload| workload.name == name)
        .unwrap_or_else(|| panic!("no workload named `{name}` — the catalogue has moved"))
}

/// `"` and `\` are the two characters a focus string literal cannot carry raw, and a
/// sampled path is somebody else's data.
#[must_use]
pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every workload in the catalogue **compiles**, which is the one thing this file
    /// can check without a corpus.
    ///
    /// It is worth checking here rather than leaving to the instruments: a bench that
    /// fails to compile its own query reports that as a run failure hours into a
    /// measurement, and the fault is a typo in a string literal.
    #[test]
    fn every_workload_compiles() {
        let schema = crate::code_index::schema();
        let pivots = Pivots::new("a/b.py", "encode", "encode");

        for workload in catalogue(&pivots) {
            let mut compilation =
                aperture_engine::compile::Compilation::new(&workload.focus, &schema);
            let plan = compilation.plan();

            assert!(
                !compilation.diagnostics().has_errors(),
                "`{}` does not compile:\n{}\n{}",
                workload.name,
                workload.focus,
                compilation.render_to_string()
            );
            assert!(plan.is_some(), "`{}` has no plan", workload.name);
        }
    }

    /// A sampled path's directory is a **prefix of it**, so a prefix seek built from one
    /// covers keys that exist.
    #[test]
    fn a_directory_is_a_prefix_of_the_file_it_came_from() {
        let pivots = Pivots::new("src/store/keys.py", "k", "k");
        assert_eq!(pivots.directory, "src/store/");
        assert!(pivots.file.starts_with(&pivots.directory));

        // A path with no directory is its own prefix, which is still true and still
        // seeks — rather than an empty string, which would seek the whole predicate and
        // quietly turn a seek workload into a scan.
        let flat = Pivots::new("keys.py", "k", "k");
        assert_eq!(flat.directory, "keys.py");
        assert!(!flat.directory.is_empty());
    }

    /// Escaping covers exactly the two characters a focus literal cannot carry.
    #[test]
    fn escaping_covers_quotes_and_backslashes() {
        assert_eq!(escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape(r"a\b"), r"a\\b");
    }
}
