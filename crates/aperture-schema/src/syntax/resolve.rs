//! **Imports** — an entry file, everything it names, and the union they make.
//!
//! [Operations §7](../../../../docs/aperture-cli-design.md) settles the rules and this
//! is the transcription:
//!
//! - **An import names a namespace, never a path** (`import lang.rust`). How a namespace
//!   is found is a resolver's business, and this resolver's answer is the obvious one:
//!   `lang.rust` is `lang/rust.aps` under a root.
//! - **Roots are searched in order, first match wins.** The entry file's own directory
//!   is searched first, so a self-contained directory of schemas needs nothing
//!   configured; `schema_path` supplies the rest.
//! - **Imports are edges with concatenation semantics** — take the transitive closure,
//!   dedup by file identity, union the blocks. A namespace is open across files, so the
//!   union is simply the text put end to end.
//! - **Cycles are harmless by construction.** Dedup by identity means a file already
//!   read is not read again, so `a` importing `b` importing `a` terminates with two
//!   files. Diamonds dedup for free, and there is nothing to detect and nothing to
//!   refuse.
//! - **The real error is genuine redeclaration**: two *different* definitions of one
//!   fully-qualified name, as against the same file reached twice. Lowering the union
//!   already reports that by name — the dedup above is what makes it mean what it says.
//! - **Transitive visibility is accepted rather than fought.** An import is not an
//!   encapsulation boundary: what `a` imports, anything importing `a` can see. Angle
//!   works this way too, and documenting it is cheaper than a scoping rule nobody asked
//!   for.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::{
    schema::Schema,
    syntax::{diag, lower, parse},
};

/// The extension a namespace's file has.
pub const EXTENSION: &str = "aps";

/// An entry file, everything it imports, and what they come to together.
pub struct Resolved {
    /// Every file that went into it, in the order they were read — the entry first.
    ///
    /// What `schema check` prints, and what says *where* a schema came from when two
    /// roots hold a namespace of the same name.
    pub files: Vec<PathBuf>,
    /// Their union, as one source — what was lowered.
    ///
    /// **Not what a database embeds**: that is [`print`](super::print::print) of the
    /// schema below, which is the same declarations with the comments, the file
    /// boundaries and the writing order taken out. This is what a diagnostic points
    /// into, and what a person is shown when they ask what resolution came to.
    pub source: String,
    pub schema: Schema,
}

/// Resolve `entry` against `roots`.
///
/// # Errors
///
/// A rendered reason: a file that cannot be read, an import nothing resolves, a syntax
/// error in any file, or anything lowering refuses about the union — a redeclaration
/// most of all.
pub fn resolve(entry: &Path, roots: &[PathBuf]) -> Result<Resolved, String> {
    // The entry file's own directory first, then the configured roots. A schema that
    // sits beside the ones it imports is the common case and should need no setup.
    let mut search: Vec<PathBuf> = entry
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    search.extend(roots.iter().cloned());

    let mut files: Vec<PathBuf> = vec![];
    let mut sources: Vec<String> = vec![];
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

    // The frontier, as (what to read, who asked for it). The second half is the whole
    // of a useful "unresolved import" message: a namespace with no file is only ever a
    // problem in the file that named it.
    let mut pending: Vec<(PathBuf, Option<(PathBuf, String)>)> = vec![(entry.to_owned(), None)];

    while let Some((path, asked_by)) = pending.pop() {
        let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

        // **Dedup by file identity, not by name.** Two roots may spell one file two
        // ways, and a diamond reaches it twice; reading it twice would turn every
        // declaration in it into a redeclaration of itself.
        if !seen.insert(identity) {
            continue;
        }

        let text = std::fs::read_to_string(&path).map_err(|source| match &asked_by {
            Some((by, namespace)) => format!(
                "{}: cannot read `{namespace}` from {}: {source}",
                by.display(),
                path.display()
            ),
            None => format!("{}: {source}", path.display()),
        })?;

        let name = path.display().to_string();
        let mut diags = vec![];

        let Some(cst) = parse::parse(&text, &mut diags) else {
            return Err(diag::render(&name, &text, &diags));
        };
        if !diags.is_empty() {
            return Err(diag::render(&name, &text, &diags));
        }

        for namespace in lower::imports(&cst) {
            let found = find(&namespace, &search).ok_or_else(|| {
                format!(
                    "{name}: nothing on the schema path declares `{namespace}` — looked for \
                     `{}` in {}",
                    relative(&namespace).display(),
                    if search.is_empty() {
                        "no roots at all (set `schema_path`)".to_owned()
                    } else {
                        search
                            .iter()
                            .map(|root| root.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )
            })?;

            pending.push((found, Some((path.clone(), namespace))));
        }

        sources.push(text);
        files.push(path);
    }

    // **One file is itself; several are a union**, and the difference is what a
    // diagnostic can honestly point at. A schema with no imports is lowered under its
    // own name with its own line numbers — the common case, and the one where a caret
    // is worth most. Several files have to be lowered together (that is what makes a
    // cross-file reference resolve and a cross-file redeclaration an error), so they
    // get a header apiece and a name that says the union is what was read.
    let (name, source) = if files.len() == 1 {
        (files[0].display().to_string(), sources.concat())
    } else {
        let name = format!(
            "<resolved schema: {} and {} more>",
            files[0].display(),
            files.len() - 1
        );

        let text = files
            .iter()
            .zip(&sources)
            .map(|(path, source)| format!("# ---- {}\n{source}\n", path.display()))
            .collect::<String>();

        (name, text)
    };

    let schema = super::read(&name, &source)?;

    Ok(Resolved {
        files,
        source,
        schema,
    })
}

/// `lang.rust` → `lang/rust.aps`.
fn relative(namespace: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for segment in namespace.split('.') {
        path.push(segment);
    }
    path.set_extension(EXTENSION);
    path
}

/// The first root holding `namespace`'s file.
fn find(namespace: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let relative = relative(namespace);

    roots
        .iter()
        .map(|root| root.join(&relative))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `files` into a scratch directory and resolve the first of them.
    fn resolving(files: &[(&str, &str)]) -> (tempfile::TempDir, Result<Resolved, String>) {
        let dir = tempfile::tempdir().expect("a scratch directory");

        for (name, source) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("a directory");
            }
            std::fs::write(path, source).expect("it writes");
        }

        let entry = dir.path().join(files[0].0);
        let resolved = resolve(&entry, &[]);

        (dir, resolved)
    }

    fn names(schema: &Schema) -> Vec<String> {
        (0..schema.len())
            .filter_map(|index| {
                schema
                    .get(crate::schema::PredicateId(index as u32))?
                    .name()
                    .map(str::to_owned)
            })
            .collect()
    }

    /// An import is an edge, and the union is what lowers — including a reference that
    /// crosses the file boundary, which is the reason resolution exists at all.
    #[test]
    fn an_import_brings_in_what_it_names() {
        let (_dir, resolved) = resolving(&[
            (
                "main.aps",
                "schema app { import src\n predicate Use : { of : src.File } }",
            ),
            ("src.aps", "schema src { predicate File : string }"),
        ]);

        let resolved = resolved.expect("it resolves");
        assert_eq!(resolved.files.len(), 2);
        assert_eq!(names(&resolved.schema), ["app.Use", "src.File"]);
    }

    /// A namespace of several segments is a path of several segments.
    #[test]
    fn a_dotted_namespace_is_a_directory() {
        let (_dir, resolved) = resolving(&[
            ("main.aps", "schema app { import lang.rust }"),
            (
                "lang/rust.aps",
                "schema lang.rust { predicate Crate : string }",
            ),
        ]);

        assert_eq!(
            names(&resolved.expect("it resolves").schema),
            ["lang.rust.Crate"]
        );
    }

    /// **A cycle is harmless**, because dedup is by file identity: `a` imports `b`
    /// imports `a` terminates with two files and no complaint. There is no cycle check
    /// here, and this test is what says one is not needed.
    #[test]
    fn a_cycle_of_imports_terminates() {
        let (_dir, resolved) = resolving(&[
            ("a.aps", "schema a { import b\n predicate A : { b : b.B } }"),
            ("b.aps", "schema b { import a\n predicate B : string }"),
        ]);

        let resolved = resolved.expect("it resolves");
        assert_eq!(resolved.files.len(), 2);
        assert_eq!(names(&resolved.schema), ["a.A", "b.B"]);
    }

    /// A diamond reads the shared file once. Reading it twice would make every
    /// declaration in it a redeclaration of itself, which is the error this dedup
    /// exists to *not* raise.
    #[test]
    fn a_diamond_reads_the_shared_file_once() {
        let (_dir, resolved) = resolving(&[
            ("main.aps", "schema app { import left\n import right }"),
            (
                "left.aps",
                "schema left { import base\n predicate L : string }",
            ),
            (
                "right.aps",
                "schema right { import base\n predicate R : string }",
            ),
            ("base.aps", "schema base { predicate B : string }"),
        ]);

        let resolved = resolved.expect("it resolves");
        assert_eq!(resolved.files.len(), 4, "base is read once, not twice");
        assert_eq!(names(&resolved.schema), ["base.B", "left.L", "right.R"]);
    }

    /// **Genuine redeclaration is the real error** — two different definitions of one
    /// fully-qualified name, which no dedup can excuse.
    #[test]
    fn two_definitions_of_one_name_are_refused() {
        let (_dir, resolved) = resolving(&[
            (
                "main.aps",
                "schema app { import other\n predicate P : string }",
            ),
            ("other.aps", "schema app { predicate P : int }"),
        ]);

        let Err(failed) = resolved else {
            panic!("one name, two definitions");
        };
        assert!(failed.contains("app.P"), "{failed}");
    }

    /// An import nothing answers says which file asked and where it looked.
    #[test]
    fn an_unresolved_import_says_what_it_looked_for() {
        let (_dir, resolved) = resolving(&[("main.aps", "schema app { import lang.rust }")]);

        let Err(failed) = resolved else {
            panic!("there is no such namespace");
        };
        assert!(failed.contains("lang.rust"), "{failed}");
        assert!(failed.contains("lang/rust.aps"), "{failed}");
    }

    /// A syntax error is reported against the **file** it is in, not against the union,
    /// which is the reason each file is parsed on its own first.
    #[test]
    fn a_syntax_error_names_the_file_it_is_in() {
        let (_dir, resolved) = resolving(&[
            ("main.aps", "schema app { import broken }"),
            ("broken.aps", "schema broken { predicate }"),
        ]);

        let Err(failed) = resolved else {
            panic!("it does not parse");
        };
        assert!(failed.contains("broken.aps"), "{failed}");
    }
}
