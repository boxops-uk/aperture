//! The plan view, held to the printer it is a structured form of.
//!
//! The guard that matters is `a_plan_view_names_every_step_the_printer_does`.
//! The view exists so a page can *address* a step — number it, hover it, line it
//! up against the query — and the text in each step is the engine's own. If the
//! two ever disagree about how many steps there are, or in what order, the page
//! would show one step's text under another's structure, and every number beside
//! it would be about the wrong thing.

use std::collections::BTreeSet;

use fjord_engine::corpus::{CORPUS, Expectation};
use fjord_inspect::{SCHEMA, lowered};

fn fixture_schema() -> String {
    fjord_schema::syntax::print::print(&fjord_store::fixture::schema())
}

/// **Structure and text come from one walk, so they cannot drift apart.**
///
/// Checked against `print::plan`, which is what `fjord query --plan` shows: the
/// steps joined by newlines, then the head. A view step whose text is not the
/// line at its own index is a view that has renumbered the plan.
#[test]
fn a_plan_view_names_every_step_the_printer_does() {
    let schema_source = fixture_schema();
    let mut planned = 0;

    for entry in CORPUS {
        let Some(plan) = lowered(&schema_source, entry.source).plan else {
            continue;
        };
        planned += 1;

        for (index, step) in plan.steps.iter().enumerate() {
            assert_eq!(step.index, index, "steps are not in body order");
            assert!(
                !step.text.is_empty(),
                "`{}` step {index} has no text, so the page has nothing to show",
                entry.source
            );
        }

        assert_eq!(
            plan.steps_count,
            plan.steps.len(),
            "`{}` counts its steps differently from how many it lists",
            entry.source
        );

        // Levels are not steps: a cursor holds one row per *level*, and the
        // distinction is what `Plan::levels` exists for.
        let levels = plan
            .steps
            .iter()
            .filter(|step| step.kind == "Level")
            .count();
        assert_eq!(
            levels, plan.levels,
            "`{}` says {} levels and lists {levels}",
            entry.source, plan.levels
        );

        // The level numbers a cursor pairs against are 0..levels, in order.
        let numbered: Vec<_> = plan.steps.iter().filter_map(|step| step.level).collect();
        assert_eq!(
            numbered,
            (0..levels).collect::<Vec<_>>(),
            "`{}` numbers its levels out of order",
            entry.source
        );

        assert!(
            !plan.head.is_empty(),
            "`{}` has a plan with no head, so nothing says what it answers",
            entry.source
        );
        assert_eq!(
            plan.fingerprint.len(),
            16,
            "a plan fingerprint is 64 bits of hex: {}",
            plan.fingerprint
        );
    }

    assert!(
        planned > 60,
        "only {planned} corpus entries produced a plan — the walk is not reaching them"
    );
}

/// The view's text **is** the printer's, joined the way the printer joins it.
#[test]
fn the_view_is_the_printer_rendered_apart() {
    let schema = fjord_store::fixture::schema();
    let schema_source = fjord_schema::syntax::print::print(&schema);

    for entry in CORPUS {
        let Expectation::Supported(_) = entry.expect else {
            continue;
        };
        let Some(view) = lowered(&schema_source, entry.source).plan else {
            continue;
        };

        let mut compilation = fjord_engine::compile::Compilation::new(entry.source, &schema);
        let plan = compilation.plan().expect("a supported entry plans");
        let printed = fjord_engine::print::plan(&plan, &schema, compilation.interner());

        let rebuilt = format!(
            "{}\n  head {}",
            view.steps
                .iter()
                .map(|step| step.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            view.head
        );

        assert_eq!(
            rebuilt, printed,
            "the view does not reassemble what `print::plan` shows for `{}`",
            entry.source
        );
    }
}

/// **A plan exists exactly when the sink is clean**, which is the rule the
/// server runs under — so a page cannot show a plan for a query the server
/// would refuse, and cannot hide one for a query it would run.
#[test]
fn a_plan_appears_exactly_when_the_query_compiles() {
    let schema_source = fixture_schema();

    for entry in CORPUS {
        let view = lowered(&schema_source, entry.source);
        let clean = view.diagnostics.is_empty();

        assert_eq!(
            view.plan.is_some(),
            clean,
            "`{}` reports {:?} and {} a plan",
            entry.source,
            view.diagnostics
                .iter()
                .map(|d| d.code.clone())
                .collect::<Vec<_>>(),
            if view.plan.is_some() {
                "still has"
            } else {
                "has no"
            }
        );
    }
}

/// **The census.** A plan view is worth what the corpus reaches: if nothing
/// produced a seek, a fetch, a derive or a test, the fields a reader looks at
/// first would be untested while every property above stayed green.
#[test]
fn the_corpus_reaches_every_shape_a_plan_can_have() {
    let schema_source = fixture_schema();
    let mut access = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    let mut residuals = 0;
    let mut alternatives = 0;

    for entry in CORPUS {
        let Some(plan) = lowered(&schema_source, entry.source).plan else {
            continue;
        };
        for step in &plan.steps {
            kinds.insert(step.kind);
            access.extend(step.access.iter().copied());
            residuals += step.residuals;
            if step.access.len() > 1 {
                alternatives += 1;
            }
        }
    }

    for wanted in ["scan", "seek", "fetch", "absent", "derive", "compare"] {
        assert!(
            access.contains(wanted),
            "no corpus entry plans a `{wanted}`, so nothing tests how it is shown"
        );
    }
    for wanted in ["Level", "Derive", "Test"] {
        assert!(
            kinds.contains(wanted),
            "no corpus entry plans a `{wanted}` step"
        );
    }
    assert!(
        residuals > 10,
        "only {residuals} residuals across the corpus"
    );
    assert!(
        alternatives > 0,
        "no corpus entry plans a level with alternatives, so a disjunction is unshown"
    );
}

/// The JSON a page parses, pinned by example — on the schema the site ships,
/// because that is what a reader will actually be looking at.
#[test]
fn the_json_is_the_shape_the_page_reads() {
    let view = lowered(
        SCHEMA,
        "N where F = code.File \"src/lib.rs\"; code.Decl {file = F, name = N, line = _}",
    );
    let json = serde_json::to_value(&view).expect("serialises");
    let plan = &json["plan"];

    assert_eq!(plan["levels"], 2);
    assert_eq!(plan["steps"][0]["kind"], "Level");
    assert_eq!(plan["steps"][0]["register"], "r0");
    assert_eq!(plan["steps"][0]["level"], 0);
    assert_eq!(plan["steps"][0]["predicates"][0], "code.File");
    assert_eq!(plan["steps"][0]["access"][0], "seek");
    assert_eq!(plan["steps"][1]["access"][0], "seek");
    assert!(
        plan["head"].as_str().is_some_and(|head| !head.is_empty()),
        "the head is empty"
    );
    assert!(
        plan["steps"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("code.File")),
        "a step's text does not name the predicate it reads"
    );
}
