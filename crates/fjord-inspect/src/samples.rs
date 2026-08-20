//! What the interactive site opens with: a schema, and queries over it.
//!
//! **Here rather than in the page, because here they are tested.** The first
//! version of the site invented its own sample queries in TypeScript, and every
//! one of them was missing the head a query requires — the lexer tokenised them
//! happily and nobody noticed until a parser was wired up. A sample that ships
//! is a claim about the language, so it is data in a crate with a suite, and
//! `every_sample_compiles_clean` is the claim.
//!
//! The schema is the repository's own `schemas/code.sigla` — the one the CLI's
//! demo creates a database from and the .NET indexer writes against — rather
//! than one written for the site. A second schema would be a second thing to
//! keep true, and this one is already exercised end to end.

/// A query worth opening with, and what it is an example of.
pub struct Sample {
    pub label: &'static str,
    pub source: &'static str,
}

/// The schema the samples are written against.
///
/// Embedded rather than fetched: the module has no filesystem and the page has
/// no server, and a copy in the page would be a second statement of a schema
/// that already exists.
pub const SCHEMA: &str = include_str!("../../../schemas/code.sigla");

/// The queries the site opens with, in the order a reader should meet them.
pub const SAMPLES: &[Sample] = &[
    Sample {
        label: "a scan",
        source: "P where src.File P",
    },
    Sample {
        label: "a join",
        source: "D where M = src.Module {file = _, name = \"Fjord.Client\"}; \
                 src.Decl {module = M, name = D, line = _}",
    },
    Sample {
        label: "a record head",
        source: "{name = N, line = L} where src.Decl {module = _, name = N, line = L}",
    },
    Sample {
        label: "a constraint",
        source: "P where src.File P; P = \"src/\"..",
    },
    Sample {
        label: "a comparison",
        source: "N where src.Decl {module = _, name = N, line = L}; L > 100",
    },
    Sample {
        label: "a negation",
        source: "N where src.Module {file = _, name = N}; \
                 !src.Decl {module = _, name = N, line = _}",
    },
    Sample {
        label: "reading a reference",
        source: "P where src.Module {file = F, name = _}; F = src.File P",
    },
    Sample {
        label: "an unknown predicate",
        source: "X where src.Nonesuch X",
    },
    Sample {
        label: "junk",
        source: "X where X = }",
    },
];

/// The samples as JSON, for a page that renders them.
#[must_use]
pub fn samples_json() -> String {
    let listed: Vec<_> = SAMPLES
        .iter()
        .map(|sample| serde_json::json!({ "label": sample.label, "source": sample.source }))
        .collect();
    serde_json::to_string(&listed).expect("samples serialise")
}
