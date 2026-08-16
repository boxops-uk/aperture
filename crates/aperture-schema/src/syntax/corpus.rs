//! **The schema surface as data** — the audit table, executable so it cannot drift.
//!
//! `aperture_engine::corpus` does this for `focus` and earned its keep immediately:
//! running it before touching the grammar gave the audit empirically, and six entries
//! that did not parse turned out to be exactly the six constructs that step added. This
//! is the same table for the schema DSL.
//!
//! # What an entry claims, and when
//!
//! Each entry carries the classification it should *end up* with, and the gate checks
//! as much of that as the compiler can currently answer:
//!
//! | | at 8.2 (now) | at 8.3, when lowering lands |
//! |---|---|---|
//! | [`Verdict::Lowers`] | must parse | must lower with no diagnostics |
//! | [`Verdict::Diagnosed`] | must **parse** | must draw exactly that code |
//! | [`Verdict::SyntaxError`] | must not parse | unchanged |
//!
//! So the deferred constructs are already pinned as *parsing* — which is the whole of
//! permissive-early, and the half that is checkable before there is anything to defer
//! them to. Writing the expected code down now is what stops 8.3 inventing a different
//! one and calling it done.

use super::diag::Code;

/// What a source is expected to come to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Accepted, all the way to a `Schema`.
    Lowers,
    /// Parses, and is then refused by name.
    Diagnosed(Code),
    /// Not in the language at all.
    SyntaxError,
}

/// One row of the table.
#[derive(Debug, Clone, Copy)]
pub struct Entry {
    /// What the construct is called, in prose.
    pub about: &'static str,
    pub source: &'static str,
    pub verdict: Verdict,
}

const fn entry(about: &'static str, source: &'static str, verdict: Verdict) -> Entry {
    Entry {
        about,
        source,
        verdict,
    }
}

/// The table.
pub const CORPUS: &[Entry] = &[
    // ---- the surface that works -------------------------------------------------
    entry(
        "a scalar key",
        "schema src { predicate File : string }",
        Verdict::Lowers,
    ),
    entry(
        "a record key, whose field order is the key order",
        "schema src { predicate Module : { file : File, name : string } }",
        Verdict::Lowers,
    ),
    entry(
        "a value side",
        "schema src { predicate Decl : { module : Module, name : string } -> string }",
        Verdict::Lowers,
    ),
    entry(
        "a reference to another namespace",
        "schema a { predicate P : { d : src.Decl } }",
        Verdict::Lowers,
    ),
    entry(
        "a named type, which is sugar with no identity of its own",
        "schema src { type Position = { line : int, col : int } }",
        Verdict::Lowers,
    ),
    entry(
        "an import, which names a namespace and never a path",
        "schema a { import lang.rust }",
        Verdict::Lowers,
    ),
    entry(
        "several blocks in one file — a namespace is open across files",
        "schema a { predicate P : string }\nschema b { predicate Q : string }",
        Verdict::Lowers,
    ),
    entry(
        "a comment",
        "# what this is\nschema src { predicate File : string }",
        Verdict::Lowers,
    ),
    entry(
        "an empty record",
        "schema src { type Unit = {} }",
        Verdict::Lowers,
    ),
    entry(
        "a nested record inside a key",
        "schema src { predicate Ref : { at : { line : int, col : int }, file : File } }",
        Verdict::Lowers,
    ),
    // ---- deferred: parses now, refused by name ----------------------------------
    entry(
        "an array — the multiplicity decision, settled as not yet",
        "schema src { predicate P : [string] }",
        Verdict::Diagnosed(Code::NyiArray),
    ),
    entry(
        "a set",
        "schema src { predicate P : set string }",
        Verdict::Diagnosed(Code::NyiSet),
    ),
    entry(
        "maybe, which is sugar over a union",
        "schema src { predicate P : maybe string }",
        Verdict::Diagnosed(Code::NyiMaybe),
    ),
    entry(
        "an enumeration",
        "schema src { type Colour = enum { red | green } }",
        Verdict::Diagnosed(Code::NyiEnum),
    ),
    entry(
        "a union, with the explicit discriminants I10 requires",
        "schema src { type T = { a : int = 0 | b : string = 1 } }",
        Verdict::Diagnosed(Code::NyiUnion),
    ),
    entry(
        "evolves, which P0 does not have",
        "schema a evolves b",
        Verdict::Diagnosed(Code::NyiEvolves),
    ),
    entry(
        "a stored derivation",
        "schema src { predicate P : string -> string stored }",
        Verdict::Diagnosed(Code::NyiDerivation),
    ),
    entry(
        "a standalone derive",
        "schema src { derive P stored }",
        Verdict::Diagnosed(Code::NyiDerivation),
    ),
    // ---- meaningless, rather than deferred ---------------------------------------
    entry(
        "a discriminant on a record field, where it means nothing",
        "schema src { type R = { a : int = 0, b : string } }",
        Verdict::Diagnosed(Code::RejectDiscriminantOnRecordField),
    ),
    entry(
        "two definitions of one name",
        "schema src { predicate P : string\n predicate P : int }",
        Verdict::Diagnosed(Code::RejectRedeclaration),
    ),
    entry(
        "a type that names nothing",
        "schema src { predicate P : Nowhere }",
        Verdict::Diagnosed(Code::RejectUnknownName),
    ),
    // ---- not in the language ------------------------------------------------------
    entry(
        "a declaration outside any block",
        "predicate P : string",
        Verdict::SyntaxError,
    ),
    entry(
        "a versioned schema name, which Angle has and this does not",
        "schema src.1 { predicate P : string }",
        Verdict::SyntaxError,
    ),
    entry(
        "an import naming a path rather than a namespace",
        "schema a { import \"lang/rust.aps\" }",
        Verdict::SyntaxError,
    ),
    entry(
        "a lowercase predicate name",
        "schema src { predicate file : string }",
        Verdict::SyntaxError,
    ),
];

#[cfg(test)]
mod tests {
    use super::{super::parse::parse, *};

    /// **The gate**: every entry parses, or fails to, exactly as it says.
    ///
    /// The half of each verdict that is checkable before lowering exists — and the half
    /// that matters most for permissive-early, since "it parses" is the claim that lets
    /// a deferred construct be reported by name instead of by caret.
    #[test]
    fn every_entry_parses_as_classified() {
        for Entry {
            about,
            source,
            verdict,
        } in CORPUS
        {
            let mut diags = vec![];
            let tree = parse(source, &mut diags);
            let parsed = tree.is_some() && diags.is_empty();

            match verdict {
                Verdict::Lowers | Verdict::Diagnosed(_) => assert!(
                    parsed,
                    "`{about}` should parse, and did not:\n  {source}\n  {diags:?}"
                ),
                Verdict::SyntaxError => {
                    assert!(!parsed, "`{about}` should not parse, and did:\n  {source}")
                }
            }
        }
    }

    /// **Every code has a worked example.** A code with no entry is a refusal nobody
    /// has written down the shape of, which is how a diagnostic comes to name a
    /// construct that cannot actually be written.
    #[test]
    fn every_code_is_reachable_from_the_corpus() {
        for code in Code::ALL {
            assert!(
                CORPUS
                    .iter()
                    .any(|entry| entry.verdict == Verdict::Diagnosed(*code)),
                "no corpus entry expects `{}`",
                code.as_str()
            );
        }
    }

    /// The table is not all one answer — a gate over a corpus of a single verdict
    /// would pass for a parser that did nothing else.
    #[test]
    fn the_corpus_covers_every_verdict() {
        let has = |f: fn(&Verdict) -> bool| CORPUS.iter().any(|e| f(&e.verdict));

        assert!(has(|v| matches!(v, Verdict::Lowers)));
        assert!(has(|v| matches!(v, Verdict::Diagnosed(_))));
        assert!(has(|v| matches!(v, Verdict::SyntaxError)));
    }
}
