//! The **fixture database** the tests share: the schema queries are written against
//! and the facts they run against.
//!
//! Shared, and that is the point. The engine's `corpus` says what the
//! compiler must do with a snippet *and what it answers*, and the flatten batteries
//! assert plan shapes over the same rows — so a shape asserted in one place and an
//! answer asserted in the other are about one database rather than two that have
//! drifted.
//!
//! The **shell** has its own (`src/main.rs`): a demo wants a schema someone recognises,
//! and this one is built out of the compiler's awkward cases rather than out of
//! anything.
//!
//! Phase 8 parses schemas; until then this is hand-built.
//!
//! # The schema
//!
//! ```text
//! predicate test.Foo    : { id : int, name : string } -> string
//! predicate test.Bar    : { id : int }
//! predicate test.Edge   : { from : int, to : int }
//! predicate test.Node   : { id : int }
//! predicate test.Nested : { outer : { inner : int } }
//! predicate test.Name   : string
//! predicate test.Count  : int
//! predicate test.Shadow : { value : int }              // `.value` is ambiguous on it
//! predicate test.Wide   : { outer : { extra : int, inner : int } }
//! predicate test.Ref    : { of : test.Foo }            // a fact-typed field
//! predicate test.Link   : { at : int, of : test.Foo }  // ...not in the leading field
//! predicate test.Deep   : { via : test.Ref }           // ...and a chain of two hops
//! ```
//!
//! Four of those are deliberate awkward cases rather than data: `test.Shadow` has a
//! key field literally named `value`, `test.Wide` carries `test.Nested`'s field name
//! with a differently-shaped record, and `test.Ref`/`test.Link` differ only in
//! whether the reference is the *leading* key field — which is what decides whether
//! a fact-id compare narrows the scan or filters it.
//!
//! # The facts
//!
//! A **sequence is per predicate and 1-based** ([I11]), and a reference carries the
//! whole [`FactId`] — predicate tag included — so the facts have to know their own
//! numbering. [`facts`] therefore yields the sequence alongside each fact, in the
//! order it must be written, and every store built from it agrees fact for fact,
//! ids included: an in-memory one takes the sequence directly, and a real
//! [`FjallDb`](crate::store::FjallDb) is checked against what its allocator
//! hands out.
//!
//! [I11]: ../../../docs/invariants.md#i11

use std::sync::Arc;

use lasso::Rodeo;

use aperture_encoding::tuple::{MARK_RECORD, MARK_TERM, fact_ref_bytes, put_i64, put_str};
use aperture_schema::{
    id::FactId,
    schema::{Predicate, PredicateId, PredicateTy, Schema},
};

/// Predicate ids **are** positions in the schema, and a `Fact` field names one — so
/// the ids of the referenced predicates have to be written down before the vector
/// that defines them. `predicate_ids_are_positions` checks each against its name.
const FOO: PredicateId = PredicateId(0);
const BAR: PredicateId = PredicateId(1);
const EDGE: PredicateId = PredicateId(2);
const NODE: PredicateId = PredicateId(3);
const NESTED: PredicateId = PredicateId(4);
const NAME: PredicateId = PredicateId(5);
const COUNT: PredicateId = PredicateId(6);
const SHADOW: PredicateId = PredicateId(7);
const WIDE: PredicateId = PredicateId(8);
const REF: PredicateId = PredicateId(9);
const LINK: PredicateId = PredicateId(10);
const DEEP: PredicateId = PredicateId(11);

/// The schema, hand-built.
///
/// Field lists happen to be sorted by name here, which is this fixture's habit and not a
/// rule — a record's field order is part of its encoding, and a schema's is whatever it
/// declares ([chapter 6]). The built-in code index declares two of its keys otherwise, on
/// purpose.
///
/// [chapter 6]: ../../../docs/06-types-and-schema.md
#[must_use]
pub fn schema() -> Schema {
    let mut names = Rodeo::new();
    let mut sym = |s: &str| names.get_or_intern(s);

    let predicates = vec![
        Predicate {
            name: sym("test.Foo"),
            key: PredicateTy::Record(Arc::from([
                (sym("id"), PredicateTy::Int),
                (sym("name"), PredicateTy::Str),
            ])),
            value: Some(PredicateTy::Str),
        },
        Predicate {
            name: sym("test.Bar"),
            key: PredicateTy::Record(Arc::from([(sym("id"), PredicateTy::Int)])),
            value: None,
        },
        Predicate {
            name: sym("test.Edge"),
            key: PredicateTy::Record(Arc::from([
                (sym("from"), PredicateTy::Int),
                (sym("to"), PredicateTy::Int),
            ])),
            value: None,
        },
        Predicate {
            name: sym("test.Node"),
            key: PredicateTy::Record(Arc::from([(sym("id"), PredicateTy::Int)])),
            value: None,
        },
        Predicate {
            name: sym("test.Nested"),
            key: PredicateTy::Record(Arc::from([(
                sym("outer"),
                PredicateTy::Record(Arc::from([(sym("inner"), PredicateTy::Int)])),
            )])),
            value: None,
        },
        Predicate {
            name: sym("test.Name"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: sym("test.Count"),
            key: PredicateTy::Int,
            value: None,
        },
        // A key field literally named `value`, so `.value` is ambiguous on it — the
        // `reject/value-shadowed` case.
        Predicate {
            name: sym("test.Shadow"),
            key: PredicateTy::Record(Arc::from([(sym("value"), PredicateTy::Int)])),
            value: None,
        },
        // Deliberately `test.Nested`'s field name carrying a differently-shaped
        // record: the only way a query in the implemented subset can make two record
        // *types* meet, which is what exercises unification's exact-arity rule.
        Predicate {
            name: sym("test.Wide"),
            key: PredicateTy::Record(Arc::from([(
                sym("outer"),
                PredicateTy::Record(Arc::from([
                    (sym("extra"), PredicateTy::Int),
                    (sym("inner"), PredicateTy::Int),
                ])),
            )])),
            value: None,
        },
        // A **fact-typed key field**, which is what makes a reference join
        // expressible at all: a reference is a `FactId`, so following one splices an
        // id rather than any key bytes.
        Predicate {
            name: sym("test.Ref"),
            key: PredicateTy::Record(Arc::from([(sym("of"), PredicateTy::Fact(FOO))])),
            value: None,
        },
        // A reference that is **not** the leading key field, so a fact-id compare
        // lands after the seek prefix has closed. `at` sorts before `of`, so a
        // capture at `at` is what closes it.
        Predicate {
            name: sym("test.Link"),
            key: PredicateTy::Record(Arc::from([
                (sym("at"), PredicateTy::Int),
                (sym("of"), PredicateTy::Fact(FOO)),
            ])),
            value: None,
        },
        // A reference to a *referrer*, so a chain of them is two hops long — which is
        // what makes hoisting's recursion reachable.
        Predicate {
            name: sym("test.Deep"),
            key: PredicateTy::Record(Arc::from([(sym("via"), PredicateTy::Fact(REF))])),
            value: None,
        },
    ];

    // Field and predicate names queries use but that no declaration interns, so
    // `LocalInterner`'s schema-first lookup can still resolve them.
    for name in ["a", "b", "alt", "nosuch", "value"] {
        sym(name);
    }

    Schema::new(names.into_reader(), Arc::from(predicates))
}

/// One fact, ready to write: its predicate, key bytes, value bytes, and the
/// **sequence** it must be given within its predicate.
pub struct Fact {
    pub predicate: PredicateId,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub sequence: u64,
}

/// Every fact, in the order it must be written.
///
/// Grouped by predicate and ascending within each group, so the sequence a fact is
/// given is also its scan position — which is what lets an expected row be written
/// as `test.Foo#1` and read as "the first `test.Foo`".
///
/// The rows are chosen so that each construct the corpus exercises actually *matches*
/// something: `test.Name` holds an `"abc"` so a prefix pattern is not empty,
/// `test.Count` holds `i64::MIN` and `1000` so the literal edge cases have a fact to
/// find, and two `test.Link` rows point at one `test.Foo` so a join through a
/// reference can return more than one row. Nothing references `test.Foo {id = 3}`, so
/// it can return none.
#[must_use]
pub fn facts() -> Vec<Fact> {
    let mut out = Vec::new();

    // `test.Foo` first: everything referencing a fact references one of these, and a
    // reference needs the whole id, sequence included.
    let a_foo = |sequence| fact_ref_bytes(fact(FOO, sequence)).to_vec();
    let reference = |sequence| fact_ref_bytes(fact(REF, sequence)).to_vec();

    for (sequence, (id, name, value)) in
        [(1i64, "ann", "one"), (2, "bob", "two"), (3, "ann", "three")]
            .into_iter()
            .enumerate()
    {
        out.push(Fact {
            predicate: FOO,
            key: [int(id), string(name)].concat(),
            value: string(value),
            sequence: sequence as u64 + 1,
        });
    }

    push(&mut out, BAR, [1i64, 2].map(int));
    push(
        &mut out,
        EDGE,
        [(1i64, 2i64), (1, 3), (2, 3)].map(|(from, to)| [int(from), int(to)].concat()),
    );
    push(&mut out, NODE, [2i64, 3].map(int));
    push(
        &mut out,
        NESTED,
        [1i64, 7].map(|inner| record(&[int(inner)])),
    );
    push(&mut out, NAME, ["abc", "ann", "anna", "bob"].map(string));
    push(&mut out, COUNT, [i64::MIN, -42, 7, 1_000].map(int));
    push(&mut out, SHADOW, [5i64].map(int));
    push(
        &mut out,
        WIDE,
        [(1i64, 2i64)].map(|(extra, inner)| record(&[int(extra), int(inner)])),
    );
    push(&mut out, REF, [1u64, 2].map(a_foo));
    push(
        &mut out,
        LINK,
        [(10i64, 1u64), (11, 2), (12, 2)].map(|(at, of)| [int(at), a_foo(of)].concat()),
    );
    push(&mut out, DEEP, [1u64, 2].map(reference));

    out
}

/// An id in this fixture: a predicate and a 1-based sequence.
fn fact(predicate: PredicateId, sequence: u64) -> FactId {
    FactId::new(predicate, sequence).expect("a fixture fact id")
}

/// Append keys for one predicate, numbered from 1 in the order given.
fn push<const N: usize>(out: &mut Vec<Fact>, predicate: PredicateId, keys: [Vec<u8>; N]) {
    for (sequence, key) in keys.into_iter().enumerate() {
        out.push(Fact {
            predicate,
            key,
            value: Vec::new(),
            sequence: sequence as u64 + 1,
        });
    }
}

fn int(value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    put_i64(&mut out, value);
    out
}

fn string(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, value);
    out
}

/// A **record-typed field**, which keeps its wrapper: inside a key it is one value
/// among others and has to be skippable as one. A key itself is flat
/// ([chapter 3](../../../docs/03-storage-model.md#a-stored-key-is-flat)).
fn record(fields: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![MARK_RECORD];
    for field in fields {
        out.extend_from_slice(field);
    }
    out.push(MARK_TERM);
    out
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    /// A `Fact` field names a predicate by id, and an id is a *position* — so a
    /// predicate inserted anywhere but the end silently re-points every reference to
    /// it. Checked by name so that mistake fails here rather than in a query.
    #[test]
    fn predicate_ids_are_positions() {
        let schema = schema();

        for (name, expected) in [
            ("test.Foo", FOO),
            ("test.Bar", BAR),
            ("test.Edge", EDGE),
            ("test.Node", NODE),
            ("test.Nested", NESTED),
            ("test.Name", NAME),
            ("test.Count", COUNT),
            ("test.Shadow", SHADOW),
            ("test.Wide", WIDE),
            ("test.Ref", REF),
            ("test.Link", LINK),
            ("test.Deep", DEEP),
        ] {
            assert_eq!(
                schema.find_position(name).map(|(id, _)| id),
                Some(expected),
                "{name}"
            );
        }
    }

    /// Every fact's key is distinct within its predicate — a key is an identity, so
    /// two facts sharing one are one fact, and a fixture that did that would quietly
    /// have fewer rows than it reads as having.
    #[test]
    fn every_key_is_distinct_within_its_predicate() {
        let mut seen: BTreeSet<(u32, Vec<u8>)> = BTreeSet::new();

        for Fact {
            predicate,
            key,
            sequence,
            ..
        } in facts()
        {
            assert!(
                seen.insert((predicate.0, key)),
                "{predicate:?} sequence {sequence} repeats a key"
            );
        }
    }

    /// Sequences are per predicate, 1-based and gap-free, in the order [`facts`]
    /// yields them — the property a real allocator has, and the one a reference
    /// written as `test.Foo#1` depends on.
    #[test]
    fn sequences_are_dense_and_in_order() {
        let mut next: BTreeMap<u32, u64> = BTreeMap::new();

        for fact in facts() {
            let expected = next.entry(fact.predicate.0).or_insert(1);
            assert_eq!(fact.sequence, *expected, "{:?}", fact.predicate);
            *expected += 1;
        }
    }
}
