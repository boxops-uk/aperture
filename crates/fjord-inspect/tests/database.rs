//! The database table, and the ranges a run walks across it.
//!
//! The table's whole claim is that it shows *the rows the machine will walk, in
//! the order it will meet them* — so a scan's `[lo, hi)` is a band across it.
//! If the order were the insertion order, or the bytes were rendered with
//! separators, or a range were reported after the rows it produced, the band
//! would point at the wrong rows and every screenshot would still look
//! plausible.

use fjord_inspect::{SAMPLES, SCHEMA, database, trace};

#[test]
fn the_table_holds_every_fact_the_database_does() {
    let db = database(SCHEMA);

    assert_eq!(db.predicates.len(), 6, "one entry per declared predicate");
    assert_eq!(
        db.facts,
        db.predicates.iter().map(|p| p.rows.len()).sum::<usize>(),
        "the total does not agree with the rows listed"
    );
    assert!(
        db.facts >= 30,
        "only {} facts — the demo database is meant to answer two or three rows \
         per query, which takes more than that",
        db.facts
    );

    for predicate in &db.predicates {
        assert!(
            !predicate.rows.is_empty(),
            "`{}` is declared and holds nothing, so a query over it demonstrates \
             an empty scan and nothing else",
            predicate.name
        );
    }
}

/// **Key order, which is the order a scan meets them** — not insertion order,
/// and not the order the facts were written in this crate.
#[test]
fn the_table_is_in_the_order_a_scan_meets_them() {
    for predicate in database(SCHEMA).predicates {
        let keys: Vec<_> = predicate.rows.iter().map(|row| row.key.clone()).collect();
        let mut sorted = keys.clone();
        sorted.sort();

        assert_eq!(
            keys, sorted,
            "`{}` is not listed in key order, so a scan range would shade the \
             wrong rows",
            predicate.name
        );
    }
}

/// Every row shows both halves: the bytes as stored, and what they decode to.
#[test]
fn every_row_shows_its_bytes_and_its_fact() {
    for predicate in database(SCHEMA).predicates {
        let has_value = predicate.ty.contains("->");

        for row in &predicate.rows {
            assert!(
                row.key.len() % 2 == 0 && row.key.chars().all(|c| c.is_ascii_hexdigit()),
                "`{}` is not plain hex: {:?} — the page compares these as strings, \
                 and a separator would break `starts with`",
                row.fact,
                row.key
            );
            assert!(
                !row.decoded.is_null(),
                "`{}` shows bytes that did not decode",
                row.fact
            );
            assert_eq!(
                row.value.is_some(),
                has_value,
                "`{}` disagrees with its predicate about having a value side",
                row.fact
            );
        }
    }
}

/// **The band means what it says.** Every row the machine binds came out of the
/// range the level was opened over — so a reader shading `[lo, hi)` across the
/// table is shading exactly the rows the scan can produce.
#[test]
fn every_row_a_register_holds_came_from_the_range_it_was_scanned_over() {
    let mut checked = 0;

    for sample in SAMPLES {
        let traced = trace(SCHEMA, sample.source);
        // The range each depth is currently walking, as the run reports them.
        let mut ranges: std::collections::BTreeMap<usize, (String, Option<String>)> =
            std::collections::BTreeMap::new();

        for step in &traced.steps {
            if let Some(scanning) = &step.scanning {
                if scanning.fetch.is_none() {
                    ranges.insert(scanning.step, (scanning.lo.clone(), scanning.hi.clone()));
                }
            }

            // A row read and dropped is a row the scan produced, so it too must
            // lie inside the range.
            if let Some(rejected) = &step.rejected
                && let (Some(key), Some((lo, hi))) =
                    (rejected.row.key.as_ref(), ranges.get(&rejected.step))
            {
                assert!(
                    key >= lo && hi.as_ref().is_none_or(|hi| key < hi),
                    "`{}` dropped {key} from a scan over [{lo}, {hi:?})",
                    sample.label
                );
                checked += 1;
            }
        }
    }

    assert!(
        checked > 5,
        "only {checked} rows were checked against a range — the ranges are not \
         being reported, and the property holds vacuously"
    );
}

/// A level that follows a reference reads **one row**, and says which.
#[test]
fn a_fetch_says_which_row_it_read() {
    let traced = trace(SCHEMA, "N where code.Ref {from = A, to = B}; N = B.name");

    let fetches: Vec<_> = traced
        .steps
        .iter()
        .filter_map(|step| step.scanning.as_ref())
        .filter_map(|scanning| scanning.fetch.clone())
        .collect();

    assert!(
        !fetches.is_empty(),
        "reading through a reference did not report a fetch: a page would show \
         it as a scan over nothing"
    );
    assert!(
        fetches.iter().all(|fetch| fetch.starts_with('#')),
        "a fetch does not name the row it read: {fetches:?}"
    );
}
