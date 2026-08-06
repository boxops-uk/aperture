//! reorder — choose the loop order. **The identity, and that is correct.**
//!
//! Ordering a query's generators is a *performance* choice, not a correctness one
//! ([chapter 7]). Correctness needs only a **safety** check: every variable a
//! seek, residual or head reads must be *captured* by some generator's key
//! pattern. Because a variable is captured at its first occurrence, "bound before
//! use" then holds in any order in which the capturing statement is free to move —
//! so a linear order that satisfies the reads is all the executor needs, and
//! picking a *better* one is selectivity, which P0 does not do.
//!
//! So this module ships `reorder = identity`. What it does *not* ship is an
//! identity with no interface: [`Deps`] is the shape the real algorithm (Kahn's
//! topological sort, layered into antichains, with a selectivity heuristic inside
//! each layer) and Phase 6's [derived binds] both need, built now so dropping
//! either in does not reshape the caller.
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
//! offers.
//!
//! [chapter 7]: ../../../docs/07-compilation.md
//! [derived binds]: ../../../docs/07-compilation.md#derived-facts

use crate::focus::schema::Symbol;

/// What one statement needs bound before it runs, and what it can bind itself.
#[derive(Debug, Default, Clone)]
pub struct StmtDeps {
    /// Variables this statement can bind, by capturing them from a key field it
    /// matches. A variable in more than one statement's `captures` is bound by
    /// whichever runs first.
    pub captures: Box<[Symbol]>,
    /// Variables this statement can only read, so something else must capture
    /// them first — today the base of an access chain, tomorrow a derived bind's
    /// inputs.
    pub reads: Box<[Symbol]>,
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

/// Choose the order the plan's generators run in: **the identity**.
///
/// Not a stub — see the module docs. The whole of P0's claim is that any order
/// satisfying [`Deps::respects`] is *correct*, so the cheapest such order is a
/// legitimate choice, and the collection order is one by construction (a read is
/// only reachable after typecheck has seen the variable bound, which is source
/// order).
///
/// Returns a permutation of `0..deps.len()`, which the caller applies to its
/// statement list. Whatever comes back is checked before it is used — flatten's
/// safety pass runs over the *chosen* order, not over the collection order, so a
/// future reorderer that returned an order violating the reads is reported rather
/// than compiled into a plan that reads an unbound register. That check lives
/// there, once, rather than as an assertion here: it is a data path, and the
/// convention is errors, not panics.
// TODO: Kahn + antichain + selectivity — sort `antichains()` layer by layer,
// point-matches before prefix-matches before full scans (Glean's `Reorder`), and
// move negations/conditionals after their non-locals are bound.
#[must_use]
pub fn reorder(deps: &Deps) -> Box<[usize]> {
    (0..deps.len()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::schema::{LocalInterner, SchemaInterner};
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
                })
                .collect::<Vec<_>>(),
        )
    }

    /// The identity, for every size — including none.
    #[test]
    fn reorder_is_the_identity() {
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

            assert_eq!(
                order.as_ref(),
                (0..stmts.len()).collect::<Vec<_>>().as_slice(),
                "reorder is the identity in P0"
            );
        }
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
}
