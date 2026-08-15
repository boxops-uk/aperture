//! **The built-in schema** — a code index, hardcoded until schemas are parsed.
//!
//! One fact per thing, and everything about a thing pointing at it by
//! [`FactId`](aperture_schema::id::FactId) rather than repeating it. It is the shape
//! `example/index.py` emits ([`example/README.md`](../../example/README.md)), the
//! shape the shell queries, and the shape a client writes against — which is the
//! point of it living here rather than in either binary.
//!
//! **This function is deleted, not ported, when [Phase 8](../../PLAN.md) lands.** A
//! database will carry its own canonical schema then
//! ([I13](../../docs/invariants.md#i13)), and nothing will need a schema compiled
//! into a binary.

use std::sync::Arc;

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use lasso::Rodeo;

/// A predicate id **is** its position in the schema, and a `Fact` field names one — so
/// the ids of the predicates that are *pointed at* have to be written down before the
/// vector that defines them. Nothing checks it — this shell is a scaffold Phase 9
/// re-points at the wire client — so a wrong id here writes facts under the wrong
/// predicate and the queries below quietly return nothing.
pub const FILE: PredicateId = PredicateId(0);
pub const MODULE: PredicateId = PredicateId(1);
pub const DECL: PredicateId = PredicateId(2);

/// The schema this shell resolves names against: **a code index**, which is the
/// canonical shape for a fact database — one fact per thing, and everything about a
/// thing pointing at it rather than repeating it.
///
/// Record fields are listed **sorted by name**, as everywhere: a record's field order
/// is part of its encoding. The `Fact` impls below deliberately do *not* list them in
/// that order, because a hand-written deriver has no reason to know it — see
/// [`focus::fact`](aperture_store::fact).
///
/// What each predicate is here to show:
///
/// | predicate | shows |
/// |---|---|
/// | `src.File` | a **scalar** key — a path is one string, and needs no record |
/// | `src.Module` | a **reference**, so a module names its file rather than repeating the path |
/// | `src.Decl` | a **value side**, so `D.value` has something to read, plus a second reference |
/// | `src.SearchByName` | **key order is the index**: the same names keyed so a prefix narrows |
/// | `src.Ref` | a **nested record** key field, and two references to two predicates, reached through an open pattern |
/// | `src.Import` | two references to one predicate, which is what a graph edge is |
pub fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let mut sym = |name: &str| rodeo.get_or_intern(name);

    let predicates = vec![
        Predicate {
            name: sym("src.File"),
            key: PredicateTy::Str,
            value: None,
        },
        Predicate {
            name: sym("src.Module"),
            key: PredicateTy::Record(Arc::from([
                (sym("file"), PredicateTy::Fact(FILE)),
                (sym("name"), PredicateTy::Str),
            ])),
            value: None,
        },
        // The value side is the declaration's *kind* — `def`, `class`, `method`,
        // `const` — because it is the one thing a query would want without matching on
        // it, and a value cannot be matched ([I6](../../docs/invariants.md#i6)).
        Predicate {
            name: sym("src.Decl"),
            key: PredicateTy::Record(Arc::from([
                (sym("line"), PredicateTy::Int),
                (sym("module"), PredicateTy::Fact(MODULE)),
                (sym("name"), PredicateTy::Str),
            ])),
            value: Some(PredicateTy::Str),
        },
        // **The search index over declaration names**, and the one predicate here that
        // exists for a reason about *keys* rather than about the code: a declaration's
        // key begins with its module, so `src.Decl {name = "encode"..}` reaches the name
        // only after the scan has opened, and the prefix can filter rows but not narrow
        // to them. Keyed with `name` leading — which is also the encoding order, since
        // field lists are sorted — the same prefix is a range, and `:plan` shows the
        // difference as `seek[name = "encode".., to = _]` against a `scan` whose
        // `where name starts with "encode"` is all it has.
        //
        // It is the same names twice over, which is what a *derived* predicate is: data
        // a query could compute, stored keyed the way the query wants to read it.
        // Written by hand here because nothing can declare one yet
        // ([Phase 8b](../../PLAN.md)) — `example/index.py` emits it exactly as a deriver
        // would.
        Predicate {
            name: sym("src.SearchByName"),
            key: PredicateTy::Record(Arc::from([
                (sym("name"), PredicateTy::Str),
                (sym("to"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        // A location is the **file and the position together**, and the file is not
        // derivable from the rest of the row: `to` reaches the file the *declaration*
        // is in, which for most references is a different one — that being what a
        // reference is for. So the file is a key field of its own, and the row names
        // somewhere someone can go and look.
        Predicate {
            name: sym("src.Ref"),
            key: PredicateTy::Record(Arc::from([
                (
                    sym("at"),
                    PredicateTy::Record(Arc::from([
                        (sym("col"), PredicateTy::Int),
                        (sym("line"), PredicateTy::Int),
                    ])),
                ),
                (sym("file"), PredicateTy::Fact(FILE)),
                (sym("to"), PredicateTy::Fact(DECL)),
            ])),
            value: None,
        },
        Predicate {
            name: sym("src.Import"),
            key: PredicateTy::Record(Arc::from([
                (sym("from"), PredicateTy::Fact(MODULE)),
                (sym("to"), PredicateTy::Fact(MODULE)),
            ])),
            value: None,
        },
    ];

    Schema::new(rodeo.into_reader(), Arc::from(predicates))
}
