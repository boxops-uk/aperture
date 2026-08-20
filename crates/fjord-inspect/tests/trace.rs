//! The trace, held to the run it is a step-by-step account of.
//!
//! The claim the whole interactive site rests on is that what a reader watches
//! is what the server does. The engine's own
//! `stepping_yields_what_running_yields` says the *machine* is the same one;
//! these say the **view** of it does not drift — same rows, same order, same
//! counts, and every rejection naming a residual the plan actually has.

use fjord_inspect::{SAMPLES, SCHEMA, lowered, lowered::UNRESOLVED, rows, trace};

/// **A trace answers the rows the run answers.**
///
/// The `yield` steps are the rows, in order. If they ever differed, a reader
/// stepping through a query would be watching a run nobody else would get.
#[test]
fn a_trace_yields_the_rows_the_run_does() {
    for sample in SAMPLES {
        let traced = trace(SCHEMA, sample.source);
        let ran = rows(SCHEMA, sample.source);

        let yielded: Vec<_> = traced
            .steps
            .iter()
            .filter(|step| step.event == "yield")
            .filter_map(|step| step.row.clone())
            .collect();
        let answered: Vec<_> = ran.rows.iter().map(|row| row.value.clone()).collect();

        assert_eq!(
            yielded, answered,
            "`{}` yields different rows stepped than run",
            sample.label
        );
        assert_eq!(
            traced.rows,
            answered.len(),
            "`{}` counts its rows differently from how many it yielded",
            sample.label
        );
        assert_eq!(
            traced.examined_total, ran.examined_total,
            "`{}` examined a different number of rows stepped than run",
            sample.label
        );
        assert!(!traced.truncated, "`{}` hit the trace cap", sample.label);
    }
}

/// **Every rejection names a residual the plan has**, at a step the plan has.
///
/// The index is into that step's residual list, which is how a page can say
/// *dropped by `where line > 15`* rather than *dropped*. An index the plan does
/// not have would point at the wrong filter, or at none.
#[test]
fn every_rejection_names_a_residual_the_plan_has() {
    let mut seen = 0;

    for sample in SAMPLES {
        let traced = trace(SCHEMA, sample.source);
        let Some(plan) = lowered(SCHEMA, sample.source).plan else {
            continue;
        };

        for step in traced.steps.iter().filter(|step| step.event == "reject") {
            let rejection = step.rejected.as_ref().expect("a reject carries its row");
            let at = plan
                .steps
                .get(rejection.step)
                .unwrap_or_else(|| panic!("`{}` rejects at a step the plan has not", sample.label));

            assert!(
                rejection.residual < at.residuals,
                "`{}` names residual {} of a step with {}",
                sample.label,
                rejection.residual,
                at.residuals
            );
            assert_eq!(
                rejection.row.kind, "fact",
                "a rejected row is a row, not a computed value"
            );
            seen += 1;
        }
    }

    assert!(
        seen > 5,
        "only {seen} rejections across every sample — the hook is not firing, and \
         every property above would hold vacuously"
    );
}

/// A register view resolves every name it shows: the predicate a row belongs to
/// comes from the id actually bound, and a name that did not resolve is written
/// loudly rather than as a plausible other name.
#[test]
fn every_name_in_a_register_resolves() {
    for sample in SAMPLES {
        for step in trace(SCHEMA, sample.source).steps {
            for register in step.registers {
                if let Some(fact) = &register.fact {
                    assert!(
                        !fact.contains(UNRESOLVED),
                        "`{}` shows a register as {fact:?}",
                        sample.label
                    );
                }
            }
        }
    }
}

/// **The registers a page folds are the registers the machine had.**
///
/// Each step carries only what changed, so a page reconstructs the register
/// file by folding. This is that fold, checked against the one thing that must
/// come out of it: at every `yield`, the registers hold what the row was
/// projected from — so a register the trace never mentioned would leave a hole
/// where a page shows a value.
#[test]
fn folding_the_changes_fills_every_register_a_row_was_projected_from() {
    let source = "N where F = code.File \"src/lib.rs\"; code.Decl {file = F, name = N, line = _}";
    let traced = trace(SCHEMA, source);
    let plan = lowered(SCHEMA, source).plan.expect("it plans");

    let mut held: std::collections::BTreeMap<usize, String> = std::collections::BTreeMap::new();
    let mut yields = 0;

    for step in &traced.steps {
        for register in &step.registers {
            match &register.fact {
                Some(fact) => {
                    held.insert(register.address, fact.clone());
                }
                None if register.kind == "empty" => {
                    held.remove(&register.address);
                }
                None => {}
            }
        }

        if step.event == "yield" {
            yields += 1;
            assert_eq!(
                held.len(),
                plan.registers,
                "at row {yields} the fold holds {} of {} registers",
                held.len(),
                plan.registers
            );
        }
    }

    assert_eq!(yields, 3, "the sample answers three rows");
}

/// **The cap is said, not silent.** A query the demo database can make
/// enormous stops, and the view says it stopped.
#[test]
fn a_run_past_the_cap_says_it_was_cut_off() {
    // Five unconstrained scans over seven declarations: 16,807 rows, and a
    // transition or three apiece.
    let traced = trace(
        SCHEMA,
        "N where code.Decl {file = _, name = N, line = _}; \
         code.Decl _; code.Decl _; code.Decl _; code.Decl _",
    );

    assert!(
        traced.truncated,
        "a run of {} steps did not reach the cap",
        traced.steps.len()
    );
    assert_eq!(
        traced.steps.len(),
        fjord_inspect::TRACE_CAP,
        "a truncated trace stops at the cap"
    );
}

/// The JSON a page parses, pinned by example.
#[test]
fn the_json_is_the_shape_the_page_reads() {
    let json = serde_json::to_value(trace(
        SCHEMA,
        "N where code.Decl {file = _, name = N, line = L}; L > 15",
    ))
    .expect("serialises");

    assert_eq!(json["rows"], 3);
    assert_eq!(json["truncated"], false);

    let steps = json["steps"].as_array().expect("steps");
    let rejected = steps
        .iter()
        .find(|step| step["event"] == "reject")
        .expect("this query drops rows");
    assert_eq!(rejected["rejected"]["residual"], 0);
    assert!(
        rejected["rejected"]["row"]["fact"]
            .as_str()
            .is_some_and(|fact| fact.starts_with("code.Decl#")),
        "a dropped row does not say which fact it was"
    );

    let yielded = steps
        .iter()
        .find(|step| step["event"] == "yield")
        .expect("this query answers");
    assert!(
        yielded["row"].is_string(),
        "a yielded row carries its value"
    );
}

/// **A reference reads as the fact it names.**
///
/// `Value`'s own serialiser writes a `FactRef` as the `u64` it is — a predicate
/// tag and a sequence packed together — which is right for a wire and
/// unreadable in a panel. A register holding a `code.Decl` shows the file it is
/// in as `code.File#2`, not as 1099511627778.
#[test]
fn a_reference_reads_as_the_fact_it_names() {
    let traced = trace(
        SCHEMA,
        "N where F = code.File \"src/lib.rs\"; code.Decl {file = F, name = N, line = _}",
    );

    let shown: Vec<String> = traced
        .steps
        .iter()
        .flat_map(|step| step.registers.iter())
        .filter_map(|register| register.value.as_ref())
        .map(std::string::ToString::to_string)
        .collect();

    assert!(
        shown.iter().any(|value| value.contains("code.File#")),
        "no register shows a reference by name: {shown:?}"
    );
    assert!(
        !shown.iter().any(|value| value.contains("109951162")),
        "a reference is still showing as its raw id: {shown:?}"
    );
}
