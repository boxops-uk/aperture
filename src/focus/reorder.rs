//! reorder — choose the loop order. **The runnable frontier, greedily.**
//!
//! Ordering a query's generators is *mostly* a performance choice ([chapter 7]),
//! and correctness needs only a **safety** check: every variable a seek, residual
//! or head reads must be *captured* by some generator's key pattern, before it is
//! read. But "before" is a property of the order, so where the order the query was
//! *written* in reads a variable the statement after it binds, this module is what
//! makes the query legal — and the alternative is refusing a query that has a
//! perfectly good plan.
//!
//! So [`reorder`] emits the **frontier**: repeatedly, the statements whose reads
//! are all bound, lowest-numbered first. Greedy, one pass, no backtracking —
//! because the constraint is monotone (see below). A query whose written order
//! already works is returned unchanged, so this is not a second way to compile
//! anything; it is the same plan, plus the ones that used to be rejected.
//!
//! # Why greedy is complete
//!
//! `reads` is a *structural* property of a statement — the base of an access chain
//! is read whatever the order — and `bound` only ever grows as statements are
//! emitted. So a statement runnable at one step is still runnable at the next, and
//! emitting one can never strand another: if any valid order exists, taking
//! anything runnable leads to one. No search, no give-up branch.
//!
//! Glean's `Reorder` does need a give-up branch, and the difference is *nested*
//! statement groups (from negation and disjunction), whose own reads depend on how
//! their branches are ordered — which is where monotonicity fails. focus has none
//! until [Phase 6b], so this argument is worth re-proving then and not before.
//!
//! [`Deps::antichains`] is kept, but off this path: it answers *whether* an order
//! exists (the exact feasibility question, and the property test's independent
//! check), while a layering is the wrong shape to *choose* one — a layer index is
//! only a lower bound on position, so sorting within a layer can never defer a
//! statement past the layer that would have bound its key. See [`reorder`] on
//! where the selectivity heuristic goes.
//!
//! # Why a variable graph, not an edge list
//!
//! The obvious interface is "statement *i* must precede statement *j*". It is the
//! wrong one, because **which statement captures a variable depends on the order
//! chosen**: in `test.Edge {from = X, to = Y}; test.Node {id = Y}` either statement
//! can capture `Y` — whichever comes first — and reversing them is a valid plan
//! with a different seek. An edge list fixes that choice before the order is
//! picked, and so forbids orders that are perfectly correct.
//!
//! [`Deps`] therefore records, per statement, the variables it *can* capture and
//! the ones it can only *read* (the base of an access chain — `Y.name` reads `Y`
//! and can never bind it). Edges fall out of an order rather than constraining it,
//! and a derived bind — which consumes variables and produces one without
//! iterating — is the same shape: reads it cannot satisfy itself, captures it
//! offers. So is a **negation**, at the extreme: it captures nothing at all, so
//! every variable it names is a read, and the placement rule Datalog states
//! separately — a negation runs after everything binding the variables it uses —
//! is what this graph already says about it.
//!
//! [chapter 7]: ../../../docs/07-compilation.md
//! [derived binds]: ../../../docs/07-compilation.md#derived-facts
//! [Phase 6b]: ../../../PLAN.md

use crate::focus::schema::Symbol;

/// Whether a statement has a **written position** to preserve.
///
/// Glean's `Ordered`/`Floating` (`Flatten/Types.hs:70-77`), and the same
/// distinction: a statement the query wrote sits somewhere in a sequence a person
/// chose, and one flatten invented — a hoisted generator, an alias — does not.
///
/// Nothing in [`reorder`] branches on it, and that is deliberate. Glean's own rule
/// is to run floating statements first, because there they are filters; here the
/// same rule would move a hoisted generator ahead of the statement that named it,
/// and the nested and two-statement spellings of one query would stop compiling to
/// the same plan.
///
/// **The consumer it was kept for did not materialise, and that is the interesting
/// part.** It was held for [Phase 6b](../../../PLAN.md)'s placement rule — a
/// negation may not run before the statements binding its non-locals — which reads
/// like a constraint on *written* order. It is not: give a negation `reads` = the
/// variables it names and `captures` = nothing, and the frontier already refuses to
/// run it early, because refusing to run a statement before its reads are bound is
/// the only thing the frontier does. So negation added no mechanism here at all.
///
/// What the tag is actually for is [`preserves_written_order`]: "the order you
/// wrote is the order you get, unless it could not run" is a claim about the
/// statements a *person* wrote, and a hoisted generator has no written position to
/// jump.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The query wrote this statement, here.
    #[default]
    Written,
    /// Flatten invented it, and it has no position of its own — only the
    /// dependencies that say where it can go.
    Floating,
}

/// What one statement needs bound before it runs, and what it can bind itself.
#[derive(Debug, Default, Clone)]
pub struct StmtDeps {
    /// Variables this statement can bind, by capturing them from a key field it
    /// matches. A variable in more than one statement's `captures` is bound by
    /// whichever runs first.
    pub captures: Box<[Symbol]>,
    /// Variables this statement can only read, so something else must capture
    /// them first — today the base of an access chain or an alias's right side,
    /// tomorrow a derived bind's inputs.
    pub reads: Box<[Symbol]>,
    /// Whether the query wrote this statement — see [`Placement`].
    pub placement: Placement,
}

/// The dependency graph flatten hands to [`reorder`]: one entry per statement, in
/// the order flatten collected them.
#[derive(Debug, Default, Clone)]
pub struct Deps {
    stmts: Box<[StmtDeps]>,
}

impl Deps {
    #[must_use]
    pub fn new(stmts: impl Into<Box<[StmtDeps]>>) -> Self {
        Self {
            stmts: stmts.into(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.stmts.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stmts.is_empty()
    }

    #[must_use]
    pub fn stmt(&self, index: usize) -> Option<&StmtDeps> {
        self.stmts.get(index)
    }

    /// Whether `order` binds every variable before it is read — the one property
    /// an order has to have, and the only thing a reorderer may not get wrong.
    ///
    /// `order` must be a permutation of `0..len`; anything else is not an order of
    /// this graph and is `false`.
    #[must_use]
    pub fn respects(&self, order: &[usize]) -> bool {
        if order.len() != self.stmts.len() {
            return false;
        }

        let mut seen = vec![false; self.stmts.len()];
        let mut bound: Vec<Symbol> = vec![];

        for &stmt in order {
            let Some(deps) = self.stmts.get(stmt) else {
                return false;
            };
            // A repeat is not a permutation, and would otherwise read as an order
            // that binds everything twice and satisfies anything.
            if std::mem::replace(&mut seen[stmt], true) {
                return false;
            }
            if deps.reads.iter().any(|var| !bound.contains(var)) {
                return false;
            }

            bound.extend(deps.captures.iter().copied());
        }

        true
    }

    /// Layers of statements, each independently orderable once the layers before
    /// it have run — Kahn's algorithm, one **antichain** per layer.
    ///
    /// This is what the eventual selectivity heuristic sorts *within*: statements
    /// in one layer can be run in any order relative to each other, so a
    /// reorderer is free there and nowhere else. `None` when no order works —
    /// a variable nothing captures, or a cycle of reads, both of which flatten's
    /// safety check reports before it gets here.
    #[must_use]
    pub fn antichains(&self) -> Option<Vec<Vec<usize>>> {
        let mut scheduled = vec![false; self.stmts.len()];
        let mut bound: Vec<Symbol> = vec![];
        let mut layers: Vec<Vec<usize>> = vec![];
        let mut left = self.stmts.len();

        while left > 0 {
            // Membership is decided against what the *previous* layers bound, which
            // is what makes a layer an antichain: no member can depend on another.
            let layer: Vec<usize> = (0..self.stmts.len())
                .filter(|stmt| {
                    !scheduled[*stmt]
                        && self.stmts[*stmt]
                            .reads
                            .iter()
                            .all(|var| bound.contains(var))
                })
                .collect();

            if layer.is_empty() {
                return None;
            }

            for &stmt in &layer {
                scheduled[stmt] = true;
                left -= 1;
                bound.extend(self.stmts[stmt].captures.iter().copied());
            }

            layers.push(layer);
        }

        Some(layers)
    }
}

/// Choose the order the plan's generators run in: the **runnable frontier**,
/// greedily, lowest-numbered first.
///
/// At each step, take the statements whose reads are all bound — the *frontier* —
/// and emit one. Repeat. That is the whole algorithm, and it needs no
/// backtracking: see [`Deps`]' monotonicity argument, which the completeness
/// property test pins.
///
/// Returns a permutation of `0..deps.len()`, which the caller applies to its
/// statement list. Whatever comes back is checked before it is used — flatten's
/// safety pass runs over the *chosen* order, not over the collection order, so an
/// order violating the reads is reported rather than compiled into a plan that
/// reads an unbound register. That check lives there, once, rather than as an
/// assertion here: it is a data path, and the convention is errors, not panics.
/// It is also why an unorderable query gets a permutation back rather than a
/// `None` — the diagnostic belongs to flatten, which can name the variable.
///
/// # Where the selectivity heuristic goes, and why it is not here
///
/// Picking the **lowest-numbered** member of the frontier is a tie-break, not a
/// cost model, and it is chosen so that a query whose source order already works
/// compiles to exactly the plan it compiled to before (the guard test says so).
/// The real heuristic replaces that `position` with a `min_by_key` over the
/// frontier — and, unlike a layered
/// [`antichains`](Deps::antichains)-then-sort-within-a-layer scheme, it can then
/// weigh a statement against *what is bound at the moment it would run*, which is
/// the only point at which "point match, prefix seek or full scan" has an answer.
/// Layering cannot express that: a layer index is a lower bound on position, so
/// sorting inside a layer can never defer a cheap-looking scan past the selective
/// statement that would have bound its key.
///
/// What blocks it is data, not structure: ranking point-matches before
/// prefix-matches before full scans (Glean's `Reorder`) needs the shape of each
/// statement's key prefix, and [`StmtDeps`] carries variable *occurrences* only.
/// Extending it is a separate change with its own guard; putting a cost function
/// here before it has anything to measure would be a stub pretending to be a
/// heuristic.
// TODO: selectivity — extend `StmtDeps` with the key-prefix shape, then
// `min_by_key` over the frontier; and move negations/conditionals after their
// non-locals are bound (Phase 6b).
#[must_use]
pub fn reorder(deps: &Deps) -> Box<[usize]> {
    let mut order: Vec<usize> = Vec::with_capacity(deps.len());
    let mut emitted = vec![false; deps.len()];
    let mut bound: Vec<Symbol> = vec![];

    while order.len() < deps.len() {
        let runnable = (0..deps.len()).find(|stmt| {
            !emitted[*stmt]
                && deps.stmts[*stmt]
                    .reads
                    .iter()
                    .all(|var| bound.contains(var))
        });

        // An empty frontier with statements left over is a query no order can
        // satisfy: a read nothing captures, or a cycle of reads. Emit the remainder
        // in collection order and let flatten's safety pass name the variable —
        // reporting twice, or reporting here, would be the worse diagnostic.
        let Some(stmt) = runnable else {
            order.extend((0..deps.len()).filter(|stmt| !emitted[*stmt]));
            break;
        };

        emitted[stmt] = true;
        bound.extend(deps.stmts[stmt].captures.iter().copied());
        order.push(stmt);
    }

    order.into()
}

/// Whether `order` ever moves a **written** statement ahead of an earlier written
/// one that could have run instead.
///
/// The stability [`reorder`] gets from taking the *lowest-numbered* runnable
/// statement, stated so that it is checked rather than assumed. It is what makes
/// "the order you wrote is the order you get, unless it could not run" true, and it
/// is the property [`Placement`]'s Phase 6b consumer will rest on: a rule that
/// keeps a negation after the statements binding its non-locals is only meaningful
/// if written order is otherwise preserved.
#[must_use]
pub fn preserves_written_order(deps: &Deps, order: &[usize]) -> bool {
    let mut bound: Vec<Symbol> = vec![];

    for (position, &stmt) in order.iter().enumerate() {
        let Some(this) = deps.stmt(stmt) else {
            return false;
        };

        // A written statement taken here jumps any *earlier* written one still to
        // come that was runnable at this point — which the frontier never does.
        if this.placement == Placement::Written {
            let jumped = order[position + 1..].iter().any(|&later| {
                later < stmt
                    && deps.stmt(later).is_some_and(|other| {
                        other.placement == Placement::Written
                            && other.reads.iter().all(|var| bound.contains(var))
                    })
            });

            if jumped {
                return false;
            }
        }

        bound.extend(this.captures.iter().copied());
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::schema::{LocalInterner, SchemaInterner};
    use ::proptest::prelude::*;
    use lasso::Rodeo;

    /// An interner-free way to name variables in these tests.
    fn vars(names: &[&str]) -> (LocalInterner, Vec<Symbol>) {
        let mut interner = LocalInterner::new(SchemaInterner::new(Rodeo::new().into_reader()));
        let symbols = names.iter().map(|n| interner.get_or_intern(n)).collect();
        (interner, symbols)
    }

    fn deps(stmts: &[(Vec<Symbol>, Vec<Symbol>)]) -> Deps {
        Deps::new(
            stmts
                .iter()
                .map(|(captures, reads)| StmtDeps {
                    captures: captures.clone().into(),
                    reads: reads.clone().into(),
                    placement: Placement::Written,
                })
                .collect::<Vec<_>>(),
        )
    }

    /// The same, with every third statement floating — enough to mix the two
    /// without making the shape depend on the draw.
    fn mixed_deps(stmts: &[(Vec<Symbol>, Vec<Symbol>)]) -> Deps {
        Deps::new(
            stmts
                .iter()
                .enumerate()
                .map(|(index, (captures, reads))| StmtDeps {
                    captures: captures.clone().into(),
                    reads: reads.clone().into(),
                    placement: if index % 3 == 0 {
                        Placement::Floating
                    } else {
                        Placement::Written
                    },
                })
                .collect::<Vec<_>>(),
        )
    }

    /// **A source order that already works is kept exactly**, for every size —
    /// including none.
    ///
    /// This is the identity claim the module used to make unconditionally, now
    /// stated as the property that survives the frontier: picking the
    /// lowest-numbered runnable statement can only ever be the collection order
    /// when the collection order binds before it reads. So every query that
    /// compiled before this module chose anything compiles to the *same plan*, and
    /// reordering is observable only where the old code refused outright.
    #[test]
    fn reorder_keeps_an_order_the_source_already_got_right() {
        let (_i, v) = vars(&["X", "Y"]);

        for stmts in [
            vec![],
            vec![(vec![v[0]], vec![])],
            vec![
                (vec![v[0]], vec![]),
                (vec![v[1]], vec![v[0]]),
                (vec![], vec![v[1]]),
            ],
        ] {
            let deps = deps(&stmts);
            let order = reorder(&deps);

            assert!(deps.respects(&order) || deps.antichains().is_none());
            assert_eq!(
                order.as_ref(),
                (0..stmts.len()).collect::<Vec<_>>().as_slice(),
                "a valid collection order is its own answer"
            );
        }
    }

    /// **A source order that reads before it binds is fixed**, not refused.
    ///
    /// The case this module exists for: `test.Ref {of = P}; P = test.Foo …` reads
    /// `P` in the statement written first, and the statement written second is the
    /// only one that can capture it. One order works and the source is not it.
    #[test]
    fn reorder_fixes_an_order_the_source_got_wrong() {
        let (_i, v) = vars(&["X", "Y", "Z"]);

        // 0 reads X; only 1 captures it.
        let graph = deps(&[(vec![], vec![v[0]]), (vec![v[0]], vec![])]);
        assert!(
            !graph.respects(&[0, 1]),
            "the premise: the source order is wrong"
        );
        assert_eq!(reorder(&graph).as_ref(), &[1, 0]);

        // A chain the source wrote backwards end to end: 0 reads Y, 1 reads X and
        // captures Y, 2 captures X. Only [2, 1, 0] works.
        let chain = deps(&[
            (vec![], vec![v[1]]),
            (vec![v[1]], vec![v[0]]),
            (vec![v[0]], vec![]),
        ]);
        assert!(!chain.respects(&[0, 1, 2]));
        assert_eq!(reorder(&chain).as_ref(), &[2, 1, 0]);

        // Only the statements that *have* to move do. 0 is runnable immediately and
        // stays first; 1 has to wait for 2, which is the only thing reordered.
        let partial = deps(&[
            (vec![v[2]], vec![]),
            (vec![], vec![v[0]]),
            (vec![v[0]], vec![]),
        ]);
        assert_eq!(reorder(&partial).as_ref(), &[0, 2, 1]);
        assert!(partial.respects(&reorder(&partial)));
    }

    /// An unorderable query still gets a **permutation** back.
    ///
    /// `reorder` does not report: flatten's safety pass runs over the chosen order
    /// and names the variable nothing binds (module docs). Returning a short order,
    /// or an `Option`, would move that diagnostic here and make every caller
    /// handle a case it already handles.
    #[test]
    fn reorder_is_a_permutation_when_no_order_works() {
        let (_i, v) = vars(&["X", "Y", "Z"]);

        // Nothing captures Z.
        let orphan = deps(&[(vec![v[0]], vec![]), (vec![], vec![v[2]])]);
        assert_eq!(orphan.antichains(), None);
        assert_eq!(reorder(&orphan).len(), 2);
        assert!(!orphan.respects(&reorder(&orphan)));

        // A cycle of reads, and a statement that reads what only it captures —
        // neither can start, and the whole remainder comes back in source order.
        let cycle = deps(&[(vec![v[0]], vec![v[1]]), (vec![v[1]], vec![v[0]])]);
        assert_eq!(reorder(&cycle).as_ref(), &[0, 1]);

        let selfish = deps(&[(vec![v[0]], vec![v[0]])]);
        assert_eq!(reorder(&selfish).as_ref(), &[0]);
    }

    /// An order is respected exactly when every read is captured earlier.
    ///
    /// The three cases are the whole of the property: a read after its capture, a
    /// read before it, and a read nothing captures at all.
    #[test]
    fn respects_is_bound_before_read() {
        let (_i, v) = vars(&["X", "Y"]);

        // 0 captures X; 1 reads X.
        let graph = deps(&[(vec![v[0]], vec![]), (vec![v[1]], vec![v[0]])]);
        assert!(graph.respects(&[0, 1]));
        assert!(!graph.respects(&[1, 0]), "1 reads X before 0 binds it");

        // Either statement can capture X, so either order works — the reason this
        // is a variable graph and not an edge list.
        let either = deps(&[(vec![v[0]], vec![]), (vec![v[0]], vec![])]);
        assert!(either.respects(&[0, 1]));
        assert!(either.respects(&[1, 0]));

        // A read nothing captures cannot be ordered at all.
        let orphan = deps(&[(vec![], vec![v[1]])]);
        assert!(!orphan.respects(&[0]));

        // Not a permutation, so not an order of this graph.
        assert!(!graph.respects(&[0]));
        assert!(!graph.respects(&[0, 0]));
    }

    /// Antichains layer the statements by what their reads need: everything
    /// runnable now, then everything that becomes runnable, and so on.
    #[test]
    fn antichains_layer_by_what_is_runnable() {
        let (_i, v) = vars(&["X", "Y", "Z"]);

        // 0 and 2 need nothing; 1 reads X (from 0); 3 reads Y (from 1).
        let graph = deps(&[
            (vec![v[0]], vec![]),
            (vec![v[1]], vec![v[0]]),
            (vec![v[2]], vec![]),
            (vec![], vec![v[1]]),
        ]);

        assert_eq!(
            graph.antichains(),
            Some(vec![vec![0, 2], vec![1], vec![3]]),
            "one layer per round of what the previous layers bound"
        );

        // A single layer is the shape a query of plain fact patterns has, which is
        // why P0 can order them however it likes.
        let independent = deps(&[(vec![v[0]], vec![]), (vec![v[1]], vec![])]);
        assert_eq!(independent.antichains(), Some(vec![vec![0, 1]]));

        // Nothing captures Z, so no layering exists.
        let stuck = deps(&[(vec![v[0]], vec![]), (vec![], vec![v[2]])]);
        assert_eq!(stuck.antichains(), None);

        // A cycle of reads: each needs what the other binds. Unreachable from
        // flatten today — typecheck rejects a read before its binding — and the
        // case Phase 6's derived binds make possible, so the interface answers it
        // now.
        let cycle = deps(&[(vec![v[0]], vec![v[1]]), (vec![v[1]], vec![v[0]])]);
        assert_eq!(cycle.antichains(), None);

        assert_eq!(Deps::default().antichains(), Some(vec![]));
    }

    /// Every antichain layering is an order the graph respects, and the identity
    /// is one whenever the graph came from a source-ordered query.
    #[test]
    fn a_layering_is_an_order_that_respects_the_graph() {
        let (_i, v) = vars(&["X", "Y", "Z"]);
        let graph = deps(&[
            (vec![v[0]], vec![]),
            (vec![v[1]], vec![v[0]]),
            (vec![v[2]], vec![]),
            (vec![], vec![v[1]]),
        ]);

        let flattened: Vec<usize> = graph
            .antichains()
            .expect("layerable")
            .into_iter()
            .flatten()
            .collect();

        assert!(graph.respects(&flattened));
        assert!(graph.respects(&reorder(&graph)));
    }

    proptest! {
        /// **Whenever an order exists, `reorder` finds one** — the completeness
        /// claim, and the only thing a greedy frontier could plausibly get wrong.
        ///
        /// Greedy needs no backtracking here because the constraint is *monotone*:
        /// `reads` is a structural property of a statement (fixed whatever the
        /// order) and `bound` only grows, so a statement runnable at one step is
        /// still runnable at the next, and emitting one can never strand another.
        /// This is the property that argument buys, and it is checked against
        /// [`Deps::antichains`] — an independent greedy layering — so the two have
        /// to agree on *which* graphs are orderable as well.
        ///
        /// Glean's `reorderStmts` cannot claim this: it queues and retries, and
        /// gives up if it gets all the way round. The difference is nested
        /// statement groups, which break monotonicity and which focus does not have
        /// until disjunction and negation land (Phase 6b) — so re-prove this then.
        #[test]
        fn reorder_finds_an_order_whenever_one_exists(
            stmts in prop::collection::vec(
                (
                    prop::collection::vec(0usize..4, 0..3),
                    prop::collection::vec(0usize..4, 0..3),
                ),
                0..7,
            ),
        ) {
            let (_i, pool) = vars(&["A", "B", "C", "D"]);
            let graph = deps(
                &stmts
                    .iter()
                    .map(|(captures, reads)| {
                        (
                            captures.iter().map(|&i| pool[i]).collect::<Vec<_>>(),
                            reads.iter().map(|&i| pool[i]).collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>(),
            );

            let order = reorder(&graph);

            // A permutation, always: the caller applies it to its statement list.
            let mut sorted = order.to_vec();
            sorted.sort_unstable();
            prop_assert_eq!(sorted, (0..stmts.len()).collect::<Vec<_>>());

            // Complete, and no more than complete: an order comes back respected
            // exactly when the graph admits one at all.
            prop_assert_eq!(graph.respects(&order), graph.antichains().is_some());
        }

        /// **Written order survives reordering**, whatever else moves.
        ///
        /// A statement the query wrote is never taken ahead of an earlier written
        /// one that could have run in its place — the stability that comes free
        /// from a lowest-numbered-first frontier. It holds with floating statements
        /// mixed in, which is what says the tag changes nothing about the order.
        ///
        /// This is what makes negation's placement legible rather than lucky: a
        /// negation moves only because its `reads` are not bound yet, never because
        /// the frontier felt like reordering.
        #[test]
        fn reorder_keeps_the_written_statements_in_written_order(
            stmts in prop::collection::vec(
                (
                    prop::collection::vec(0usize..4, 0..3),
                    prop::collection::vec(0usize..4, 0..3),
                ),
                0..7,
            ),
        ) {
            let (_i, pool) = vars(&["A", "B", "C", "D"]);
            let drawn = stmts
                .iter()
                .map(|(captures, reads)| {
                    (
                        captures.iter().map(|&i| pool[i]).collect::<Vec<_>>(),
                        reads.iter().map(|&i| pool[i]).collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();

            for graph in [deps(&drawn), mixed_deps(&drawn)] {
                let order = reorder(&graph);
                prop_assert!(preserves_written_order(&graph, &order));
            }
        }
    }
}
