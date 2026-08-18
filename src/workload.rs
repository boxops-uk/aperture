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

use aperture_schema::schema::{PredicateId, Schema};
use aperture_wire::{WireFact, WireRef, WireValue};

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

    /// **[`Corpus`]'s closed form is what interning actually costs** — checked against a
    /// real store, because otherwise the write rung's reproduce-or-abort check is
    /// circular: it would be holding the run to a number derived from the same reasoning
    /// that produced the run.
    ///
    /// Both halves matter and they fail differently. `created` wrong means the corpus does
    /// not contain what it says (a duplicate key generated twice, or a fanout that
    /// collides), and every facts/s number would then be divided by the wrong count.
    /// `interns` wrong means the nesting is not the depth claimed, which is precisely the
    /// quantity the cache is judged on.
    #[test]
    fn the_corpus_costs_exactly_what_it_says_it_does() {
        let corpus = Corpus {
            files: 4,
            modules_per_file: 2,
            decls_per_module: 3,
            refs_per_decl: 2,
        };

        let dir = tempfile::tempdir().expect("a scratch directory");
        let db = aperture_store::store::FjallDb::open(dir.path()).expect("a database");
        let schema = crate::code_index::schema();

        let (mut created, mut seen) = (0, 0);
        for emission in corpus.emit(&schema) {
            let mut interns = 0;
            for fact in &emission.facts {
                let out = aperture_ingest::intern_fact(&db, &schema, fact).expect("it ingests");
                interns += out.seen();
            }

            assert_eq!(
                interns as u64, emission.interns,
                "{} costs {interns} interns, not the {} it claims",
                emission.name, emission.interns,
            );
            created += emission.facts.len();
            seen += interns;
        }

        // Every fact is distinct, so a first ingest creates one per emitted fact — and
        // the interns above it are the repeats the cache exists for.
        assert_eq!(created as u64, corpus.facts(), "distinct facts");
        assert_eq!(seen as u64, corpus.interns(), "resolve-or-create calls");

        // The claim the ratio is quoted for: a `keys` read per *distinct* key, not per
        // intern. Same arithmetic as the ingest crate's guard, one layer up and over a
        // shape with four levels of nesting rather than two.
        assert_eq!(
            db.intern_read_counters().0,
            corpus.facts(),
            "one live `keys` read per distinct key"
        );
    }
}

// ---------------------------------------------------------------------------------
// The write side: what an ingest instrument sends
// ---------------------------------------------------------------------------------

/// **The facts a write instrument sends, stated once** — S0 for the write path.
///
/// Everything above this line describes *questions asked of a corpus that exists*. A
/// write rung has the opposite problem: it needs facts that do **not** exist yet, and
/// the thing under measurement is not their content but their **shape**. Interning's
/// cost is decided by how many references name the same target — 94.9M interns produced
/// 25.0M facts on the real index, a ratio of 3.8
/// ([findings §12](../../bench/FINDINGS.md)) — so a corpus that does not reproduce that
/// ratio measures a write path nobody has.
///
/// The four fanouts below are what set it. They are the source layer of the built-in
/// schema, nested exactly as [`aperture_cli::code_index`](crate::code_index) declares
/// it: a reference names a declaration, which names a module, which names a file. So one
/// `src.Ref` carries a four-deep subgraph, and the thousandth reference to a declaration
/// re-sends the whole chain — which is the redundancy, and is *not* a flaw in the
/// producer. It is what a syntax walk has in hand.
///
/// # Why the counts are predicted rather than probed
///
/// `examples/engine.rs` fixes its row counts with one unmeasured run and holds every
/// timed run to them, because what a real corpus answers is not knowable in advance.
/// Here it is: the arithmetic below is a closed form, so an instrument can assert that
/// the store agrees with the corpus's own statement of itself. A run whose `created`
/// differs from [`Corpus::facts`] did not write the corpus described — a stronger check
/// than reproducibility between runs, because the first run is checked too.
#[derive(Debug, Clone, Copy)]
pub struct Corpus {
    pub files: u64,
    pub modules_per_file: u64,
    pub decls_per_module: u64,
    pub refs_per_decl: u64,
}

/// One predicate's worth of facts, ready to send.
///
/// A block is a run of **one** predicate ([`aperture_wire::block`]), and a producer
/// emits per predicate, so this is the unit both the in-process rung and a wire client
/// hand onward.
pub struct Emission {
    pub predicate: PredicateId,
    pub name: &'static str,
    pub facts: Vec<WireFact>,
    /// Interns this run of facts costs — itself plus every nested target, counted with
    /// repeats. What the cache is judged against.
    pub interns: u64,
}

impl Corpus {
    /// The default shape: 24,300 facts at 4.6 interns each.
    ///
    /// Chosen so the interns-per-fact ratio brackets the real index's 3.8 rather than
    /// matching it exactly — a corpus that reproduced the number by construction could
    /// not be used to ask what moves it.
    #[must_use]
    pub fn standard() -> Corpus {
        Corpus {
            files: 100,
            modules_per_file: 2,
            decls_per_module: 20,
            refs_per_decl: 5,
        }
    }

    /// Distinct facts, which is what a first ingest **creates**.
    #[must_use]
    pub fn facts(&self) -> u64 {
        self.files + self.modules() + self.decls() + self.refs()
    }

    /// Resolve-or-create calls a first ingest makes, repeats included.
    ///
    /// A file costs 1; a module 2 (itself and its file); a declaration 3; a reference 5
    /// — itself, the declaration chain of three, and the file it also names directly.
    #[must_use]
    pub fn interns(&self) -> u64 {
        self.files + self.modules() * 2 + self.decls() * 3 + self.refs() * 5
    }

    fn modules(&self) -> u64 {
        self.files * self.modules_per_file
    }

    fn decls(&self) -> u64 {
        self.modules() * self.decls_per_module
    }

    fn refs(&self) -> u64 {
        self.decls() * self.refs_per_decl
    }

    /// A one-line statement of the shape, for the run's header.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} files × {} modules × {} decls × {} refs\n         \
             {} facts, {} interns ({:.2} per fact)",
            self.files,
            self.modules_per_file,
            self.decls_per_module,
            self.refs_per_decl,
            self.facts(),
            self.interns(),
            self.interns() as f64 / self.facts() as f64,
        )
    }

    /// The corpus as blocks, in the order a producer would reach them.
    ///
    /// Emitted parents-first because that is what a walk does, **not** because interning
    /// needs it: every fact here carries its targets nested, so any order works and the
    /// last block would create the whole graph on its own. Reversing this is a legitimate
    /// thing for an instrument to try, and the created count must not change.
    ///
    /// # Panics
    ///
    /// If `schema` does not declare the source layer this describes.
    #[must_use]
    pub fn emit(&self, schema: &Schema) -> Vec<Emission> {
        let id = |name: &str| {
            schema
                .find_position(name)
                .map(|(id, _)| id)
                .unwrap_or_else(|| panic!("the schema declares no `{name}`"))
        };
        let (file_id, module_id, decl_id, ref_id) = (
            id("src.File"),
            id("src.Module"),
            id("src.Decl"),
            id("src.Ref"),
        );

        // `src.File : string`
        let file = |f: u64| WireFact {
            predicate: file_id,
            key: WireValue::Str(format!("src/dir{}/file{f}.cs", f % 16)),
            value: None,
        };
        // `src.Module : { file : File, name : string }`
        let module = |f: u64, m: u64| WireFact {
            predicate: module_id,
            key: WireValue::Record(
                vec![
                    WireValue::Ref(WireRef::Nested(Box::new(file(f)))),
                    WireValue::Str(format!("Ns{m}")),
                ]
                .into(),
            ),
            value: None,
        };
        // `src.Decl : { module : Module, name : string, line : int } -> string`
        let decl = |f: u64, m: u64, d: u64| WireFact {
            predicate: decl_id,
            key: WireValue::Record(
                vec![
                    WireValue::Ref(WireRef::Nested(Box::new(module(f, m)))),
                    WireValue::Str(format!("Member{d}")),
                    WireValue::Int(i64::try_from(d * 7 + 1).unwrap_or(i64::MAX)),
                ]
                .into(),
            ),
            value: Some(WireValue::Str("method".to_owned())),
        };
        // `src.Ref : { to : Decl, file : File, at : { line, col, length } }` — the target
        // leads, which is findings §2's key order and the reason find-references seeks.
        // `at` is a **record**, not an int: a span has the three fields the schema says
        // it has, and getting that wrong here is what the test below caught.
        let reference = |f: u64, m: u64, d: u64, r: u64| WireFact {
            predicate: ref_id,
            key: WireValue::Record(
                vec![
                    WireValue::Ref(WireRef::Nested(Box::new(decl(f, m, d)))),
                    WireValue::Ref(WireRef::Nested(Box::new(file((f + r) % self.files)))),
                    WireValue::Record(
                        vec![
                            WireValue::Int(i64::try_from(r * 13 + 2).unwrap_or(i64::MAX)),
                            WireValue::Int(i64::try_from(r + 4).unwrap_or(i64::MAX)),
                            WireValue::Int(8),
                        ]
                        .into(),
                    ),
                ]
                .into(),
            ),
            value: None,
        };

        let each = |count: u64, mut build: Box<dyn FnMut(u64) -> WireFact + '_>| {
            (0..count).map(&mut *build).collect::<Vec<_>>()
        };

        vec![
            Emission {
                predicate: file_id,
                name: "src.File",
                facts: each(self.files, Box::new(&file)),
                interns: self.files,
            },
            Emission {
                predicate: module_id,
                name: "src.Module",
                facts: each(
                    self.modules(),
                    Box::new(|n| module(n / self.modules_per_file, n % self.modules_per_file)),
                ),
                interns: self.modules() * 2,
            },
            Emission {
                predicate: decl_id,
                name: "src.Decl",
                facts: each(
                    self.decls(),
                    Box::new(|n| {
                        let d = n % self.decls_per_module;
                        let module = n / self.decls_per_module;
                        decl(
                            module / self.modules_per_file,
                            module % self.modules_per_file,
                            d,
                        )
                    }),
                ),
                interns: self.decls() * 3,
            },
            Emission {
                predicate: ref_id,
                name: "src.Ref",
                facts: each(
                    self.refs(),
                    Box::new(|n| {
                        let r = n % self.refs_per_decl;
                        let decl = n / self.refs_per_decl;
                        let d = decl % self.decls_per_module;
                        let module = decl / self.decls_per_module;
                        reference(
                            module / self.modules_per_file,
                            module % self.modules_per_file,
                            d,
                            r,
                        )
                    }),
                ),
                interns: self.refs() * 5,
            },
        ]
    }
}
