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
//! | | must |
//! |---|---|
//! | [`Verdict::Lowers`] | parse, and lower to a schema with no diagnostics |
//! | [`Verdict::Diagnosed`] | **parse**, and then draw exactly that code |
//! | [`Verdict::SyntaxError`] | not parse |
//!
//! The middle row is the whole of permissive-early: a deferred construct is a thing the
//! grammar accepts so that lowering can name it. A gate that only checked *some*
//! diagnostic came out would pass for a compiler that reported the wrong one, which is
//! exactly the drift a code exists to prevent — so the assertion is on the **set** of
//! codes, and an entry that draws a second unexpected one fails too.

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
        "schema src { predicate File : string\n          predicate Module : { file : File, name : string } }",
        Verdict::Lowers,
    ),
    entry(
        "a value side",
        "schema src { predicate Module : string\n          predicate Decl : { module : Module, name : string } -> string }",
        Verdict::Lowers,
    ),
    entry(
        "a reference to another namespace",
        "schema src { predicate Decl : string }\n         schema a { predicate P : { d : src.Decl } }",
        Verdict::Lowers,
    ),
    entry(
        "a named type, which is sugar with no identity of its own",
        "schema src { type Position = { line : int, col : int }\n          predicate At : { where : Position } }",
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
        "schema src { predicate File : string\n          predicate Ref : { at : { line : int, col : int }, file : File } }",
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
        "a named type that expands into itself",
        "schema src { type A = B\n type B = A\n predicate P : A }",
        Verdict::Diagnosed(Code::RejectTypeCycle),
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
    use super::{
        super::{lower::lower, parse::parse},
        *,
    };

    /// Every code a source draws, sorted and deduplicated.
    fn codes(source: &str) -> Vec<String> {
        let mut diags = vec![];

        if let Some(cst) = parse(source, &mut diags) {
            // The schema itself is not the question here — the diagnostics are.
            let _ = lower(&cst, &mut diags);
        }

        let mut codes: Vec<String> = diags.into_iter().filter_map(|d| d.code).collect();
        codes.sort();
        codes.dedup();
        codes
    }

    /// **The gate**: every entry comes to exactly what it says it does.
    #[test]
    fn every_entry_is_classified_as_the_table_says() {
        for Entry {
            about,
            source,
            verdict,
        } in CORPUS
        {
            match verdict {
                Verdict::Lowers => assert!(
                    codes(source).is_empty(),
                    "`{about}` should lower cleanly, and drew {:?}:\n  {source}",
                    codes(source)
                ),
                Verdict::Diagnosed(code) => assert_eq!(
                    codes(source),
                    vec![code.as_str().to_owned()],
                    "`{about}` should draw exactly `{}`:\n  {source}",
                    code.as_str()
                ),
                // Its own gate below: a source that does not parse has no lowering to
                // ask about, and the two claims fail for different reasons.
                Verdict::SyntaxError => {}
            }
        }
    }

    /// The parse half, kept separate: "it parses" is the claim that lets a deferred
    /// construct be reported by name instead of by caret, and it is worth failing on
    /// its own rather than inside a diagnostic mismatch.
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
