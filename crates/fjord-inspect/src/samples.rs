//! What the interactive site opens with: a schema, and queries over it.
//!
//! **Here rather than in the page, because here they are tested.** The first
//! version of the site invented its own sample queries in TypeScript, and every
//! one of them was missing the head a query requires — the lexer tokenised them
//! happily and nobody noticed until a parser was wired up. A sample that ships
//! is a claim about the language, so it is data in a crate with a suite, and
//! `every_sample_compiles_clean` is the claim.
//!
//! The schema is [`schemas/demo.sigla`](crate::demo) — the site's own, small
//! enough to read in a screen and chosen so every shape the language has
//! appears exactly once. `code.sigla` is the real one and cannot do this job:
//! it has no union and no nested record, so a select, a union pattern and a
//! discriminant residual would have nothing to bind against.

/// A query worth opening with, what it is an example of, and how many rows it
/// answers.
///
/// The count is part of the sample rather than a fact about it: a demo query
/// that answers nothing demonstrates nothing, and one that answers a single row
/// shows no backtracking. `every_sample_answers_what_it_says` holds each to its
/// number.
pub struct Sample {
    pub label: &'static str,
    pub source: &'static str,
    /// How many rows it answers over the [demo database](crate::demo), or
    /// `None` when the sample is there to be refused.
    pub rows: Option<usize>,
}

/// The queries the site opens with, in the order a reader should meet them.
///
/// Between them they reach every construct the language has: a scan and a seek,
/// a join through a reference in both key positions, a record head, a
/// constraint, a comparison, arithmetic, a denial, a negation, a disjunction, a
/// subquery, a nested record, a union matched as a seek and the same union
/// matched as a residual, a select, and the value side.
pub const SAMPLES: &[Sample] = &[
    Sample {
        label: "a scan",
        source: "P where code.File P; P = \"src/\"..",
        rows: Some(3),
    },
    Sample {
        label: "a join",
        source: "N where F = code.File \"src/lib.rs\"; code.Decl {file = F, name = N, line = _}",
        rows: Some(3),
    },
    Sample {
        label: "a record head",
        source: "{name = N, line = L} where code.Decl {file = _, name = N, line = L}; L > 15",
        rows: Some(3),
    },
    Sample {
        label: "the value side",
        source: "D.value where D = code.Decl {file = _, name = _, line = L}; L < 6",
        rows: Some(3),
    },
    Sample {
        label: "arithmetic",
        source: "E where code.Span {decl = _, at = {line = L, col = 4}}; E = L + 1",
        rows: Some(3),
    },
    Sample {
        label: "a denial",
        source: "N where F = code.File \"src/lib.rs\"; \
                 code.Decl {file = F, name = N, line = _}; N != \"Error\"",
        rows: Some(2),
    },
    Sample {
        label: "a negation",
        source: "N where D = code.Decl {file = _, name = N, line = _}; \
                 !code.Ref {from = _, to = D}",
        rows: Some(2),
    },
    Sample {
        label: "following a reference",
        source: "N where M = code.Decl {file = _, name = \"main\", line = _}; \
                 code.Ref {from = M, to = T}; N = T.name",
        rows: Some(2),
    },
    Sample {
        label: "a union, seeking",
        source: "D where code.KindOf {what = {func = 1}, decl = D}",
        rows: Some(3),
    },
    Sample {
        label: "a union, filtering",
        source: "D where code.Kind {decl = D, what = {func = 1}}",
        rows: Some(3),
    },
    Sample {
        label: "a select",
        source: "A where K = code.Kind {decl = _, what = _}; A = K.what.func?; A > 0",
        rows: Some(4),
    },
    Sample {
        label: "a nested record",
        source: "{line = L, col = C} where code.Span {decl = _, at = {line = L, col = C}}; C = 4",
        rows: Some(3),
    },
    Sample {
        label: "a disjunction",
        source: "D where code.Kind {decl = D, what = {data = _}} \
                 | code.Kind {decl = D, what = {func = 2}}",
        rows: Some(3),
    },
    Sample {
        label: "a subquery",
        source: "X where X = (Y where code.File Y; Y = \"src/\"..)",
        rows: Some(3),
    },
    Sample {
        label: "an unknown predicate",
        source: "X where code.Nonesuch X",
        rows: None,
    },
    Sample {
        label: "junk",
        source: "X where X = }",
        rows: None,
    },
];

/// The samples as JSON, for a page that renders them.
#[must_use]
pub fn samples_json() -> String {
    let listed: Vec<_> = SAMPLES
        .iter()
        .map(|sample| {
            serde_json::json!({
                "label": sample.label,
                "source": sample.source,
                "rows": sample.rows,
            })
        })
        .collect();
    serde_json::to_string(&listed).expect("samples serialise")
}
