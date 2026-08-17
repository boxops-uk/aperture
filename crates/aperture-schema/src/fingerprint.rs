//! **Schema identity** — the canonical form, and the fingerprints taken over it.
//!
//! [Chapter 6](../../../docs/06-types-and-schema.md) asks for two things: a form that is
//! independent of how a schema was written down, and a hash over it — one per predicate
//! and one for the whole schema. What a database embeds and a handshake compares is the
//! result.
//!
//! # The canonical form is a **byte string**, and that is deliberate
//!
//! Not "a hash over the schema" but a grammar, written out below, because a second
//! implementation has to produce it character by character. The .NET client computes a
//! schema fingerprint of its own today, and chapter 6 names this as the load-bearing
//! decision of the phase: changing *how* a fingerprint is computed silently rejects every
//! artifact and every client already built. So the form is specified, the algorithm is
//! versioned ([`VERSION`]), and the number a database records stays authoritative.
//!
//! ```text
//! schema     := "aperture-schema-v" version "\n" (predicate "\n")*     -- sorted by name
//! predicate  := name ":" type ("->" type)?
//! type       := "int"
//!             | "string"
//!             | "{" (field ("," field)*)? "}"                          -- declaration order
//!             | "@" name "#" hex16
//! field      := name ":" type
//! name       := a fully-qualified name, e.g. src.Decl
//! hex16      := 16 lowercase hex digits
//! ```
//!
//! Two properties of that grammar are the whole point:
//!
//! - **A record's fields are in declaration order, never sorted.** Field order is
//!   encoding order and decides the seek prefix, so permuting fields is a *semantic*
//!   change and must move the fingerprint. Glean draws the line in the same place.
//! - **A reference is spelled as the referent's name *and its fingerprint*.** Not the
//!   `PredicateId`, which is a position and would make identity depend on declaration
//!   order — the very thing this exists to be free of — and not a bare name, which would
//!   not propagate a change in the referent. Chapter 6 specifies exactly this, and the
//!   price is the cycle handling below.
//!
//! # Cycles: the two-pass hash, transcribed rather than invented
//!
//! Spelling a reference with the referent's fingerprint has no base case when two
//! predicates reference each other. Glean's `computeIds` answers it and this is that
//! answer: find the strongly-connected components; within a cyclic one, render every
//! **back-edge into the group** as `#0000000000000000` and hash each member against
//! that; hash the group's individual hashes together into a *cycle hash*; then give each
//! member `hash(individual, cycle)`. Every member comes out distinct, and any change to
//! any of them moves all of them — which is what a Merkle-ish identity is for.

use std::collections::{BTreeMap, BTreeSet};

use crate::schema::{PredicateId, PredicateTy, Schema};

/// The version of *this algorithm*.
///
/// Recorded beside every fingerprint a database keeps, so that changing how a
/// fingerprint is computed is a visible migration rather than a silent rejection of
/// every artifact already produced. Glean added exactly this after being bitten
/// (`glean/if/internal.thrift:24-33`), and the rule it learned is the one to copy: the
/// **stored** fingerprint is authoritative.
pub const VERSION: u32 = 1;

/// What a reference to a predicate inside its own cycle is rendered as, before the
/// group's hash is known.
const HASH0: u64 = 0;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a, 64-bit.
///
/// Small, dependency-free and trivial to reproduce in another language, which is the
/// property that matters here — this is a "did we mean the same schema" check rather
/// than a security boundary.
#[must_use]
pub fn hash(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// A schema's identity: one fingerprint per predicate, and one for the whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `qualified name → fingerprint`. Sorted, so it renders and compares stably.
    predicates: BTreeMap<String, u64>,
    schema: u64,
    /// The canonical form the fingerprints were taken over, kept for `schema
    /// fingerprint` to print and for a person to diff.
    canonical: String,
}

impl Identity {
    /// The whole-schema fingerprint.
    #[must_use]
    pub fn schema(&self) -> u64 {
        self.schema
    }

    /// One predicate's fingerprint, by fully-qualified name.
    #[must_use]
    pub fn of(&self, name: &str) -> Option<u64> {
        self.predicates.get(name).copied()
    }

    /// The map identity *is* — `qualified name → fingerprint`.
    #[must_use]
    pub fn predicates(&self) -> &BTreeMap<String, u64> {
        &self.predicates
    }

    /// The canonical form, as bytes were taken over it.
    #[must_use]
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// Whether `self` may be replaced by `newer` — chapter 6's **subset containment**.
    ///
    /// `compatible(old → new) ⇔ old_map ⊆ new_map`, so the only compatible change is
    /// *adding* a predicate. Any in-place modification of a key or a value is breaking,
    /// because values are queryable and positionally encoded and a field change shifts
    /// stored bytes. No field-level diffing is needed, and none is done.
    #[must_use]
    pub fn compatibility(&self, newer: &Identity) -> Compatibility {
        if self.predicates == newer.predicates {
            return Compatibility::Identical;
        }

        let broken: Vec<String> = self
            .predicates
            .iter()
            .filter(|(name, fingerprint)| newer.predicates.get(*name) != Some(fingerprint))
            .map(|(name, _)| name.clone())
            .collect();

        if broken.is_empty() {
            Compatibility::Compatible {
                added: newer.predicates.len() - self.predicates.len(),
            }
        } else {
            Compatibility::Breaking { broken }
        }
    }
}

/// What `schema diff` answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    Identical,
    /// Every old predicate survives unchanged, and `added` new ones appeared.
    Compatible {
        added: usize,
    },
    /// Named predicates were removed or modified.
    Breaking {
        broken: Vec<String>,
    },
}

/// A schema's whole fingerprint, for a caller that does not want the map.
///
/// The number a handshake compares and a sidecar records. It is the same one
/// `aperture schema fingerprint` prints, which is what lets a client
/// [carry it rather than derive it](../../../docs/open-decisions.md).
#[must_use]
pub fn of(schema: &Schema) -> u64 {
    identity(schema).schema()
}

/// Compute a schema's identity.
///
/// **Virtual predicates are not in it.** One is answered by whoever runs the query
/// rather than stored, so it is not part of what a database holds and must not be part
/// of what a client has to agree with — otherwise a server that grew
/// `aperture.db.List` would stop every existing client until each declared a predicate
/// it can never write to.
#[must_use]
pub fn identity(schema: &Schema) -> Identity {
    let names = names_of(schema);
    let groups = components(schema, &names);

    // Each component is emitted after everything it references, so a fingerprint is
    // always known by the time something spells it.
    let mut fingerprints: BTreeMap<PredicateId, u64> = BTreeMap::new();

    for group in &groups {
        let cyclic = group.len() > 1 || self_referencing(schema, group[0]);
        let members: BTreeSet<PredicateId> = group.iter().copied().collect();

        // Inside a cycle, a back-edge is rendered as `#0` — which is what gives the
        // recursion a base case.
        let elide = if cyclic { &members } else { &BTreeSet::new() };

        let individual: Vec<(PredicateId, u64)> = group
            .iter()
            .map(|id| {
                let form = predicate_form(schema, *id, &names, &fingerprints, elide);
                (*id, hash(form.as_bytes()))
            })
            .collect();

        if !cyclic {
            fingerprints.insert(individual[0].0, individual[0].1);
            continue;
        }

        // Sorted by name, so the group's hash does not depend on the order the SCC
        // algorithm happened to visit its members in.
        let mut ordered: Vec<(PredicateId, u64)> = individual.clone();
        ordered.sort_by_key(|(id, _)| names.get(id).cloned().unwrap_or_default());

        let mut cycle_bytes = Vec::new();
        for (_, individual) in &ordered {
            cycle_bytes.extend_from_slice(&individual.to_be_bytes());
        }
        let cycle = hash(&cycle_bytes);

        for (id, individual) in individual {
            let mut bytes = individual.to_be_bytes().to_vec();
            bytes.extend_from_slice(&cycle.to_be_bytes());
            fingerprints.insert(id, hash(&bytes));
        }
    }

    // The canonical form: every predicate, sorted by name, over a versioned header. The
    // sort is what makes declaration order and file layout invisible to identity.
    let mut by_name: BTreeMap<String, u64> = BTreeMap::new();
    let mut lines: Vec<String> = Vec::with_capacity(names.len());

    for (id, name) in &names {
        let fingerprint = fingerprints.get(id).copied().unwrap_or(HASH0);
        by_name.insert(name.clone(), fingerprint);
        lines.push(predicate_form(
            schema,
            *id,
            &names,
            &fingerprints,
            &BTreeSet::new(),
        ));
    }

    lines.sort();

    let mut canonical = format!("aperture-schema-v{VERSION}\n");
    for line in &lines {
        canonical.push_str(line);
        canonical.push('\n');
    }

    Identity {
        schema: hash(canonical.as_bytes()),
        predicates: by_name,
        canonical,
    }
}

/// `name ":" key ("->" value)?`
fn predicate_form(
    schema: &Schema,
    id: PredicateId,
    names: &BTreeMap<PredicateId, String>,
    fingerprints: &BTreeMap<PredicateId, u64>,
    elide: &BTreeSet<PredicateId>,
) -> String {
    let Some(predicate) = schema.get(id) else {
        return String::new();
    };

    let mut out = names.get(&id).cloned().unwrap_or_default();
    out.push(':');
    type_form(
        &mut out,
        &predicate.predicate().key,
        schema,
        names,
        fingerprints,
        elide,
    );

    if let Some(value) = predicate.predicate().value.as_ref() {
        out.push_str("->");
        type_form(&mut out, value, schema, names, fingerprints, elide);
    }

    out
}

fn type_form(
    out: &mut String,
    ty: &PredicateTy,
    schema: &Schema,
    names: &BTreeMap<PredicateId, String>,
    fingerprints: &BTreeMap<PredicateId, u64>,
    elide: &BTreeSet<PredicateId>,
) {
    match ty {
        PredicateTy::Int => out.push_str("int"),
        PredicateTy::Str => out.push_str("string"),

        // **Name and fingerprint, never the id.** A position would make identity depend
        // on declaration order; a bare name would not carry a change in the referent.
        PredicateTy::Fact(target) => {
            let fingerprint = if elide.contains(target) {
                HASH0
            } else {
                fingerprints.get(target).copied().unwrap_or(HASH0)
            };

            out.push('@');
            out.push_str(names.get(target).map_or("?", String::as_str));
            out.push('#');
            let _ = std::fmt::Write::write_fmt(out, format_args!("{fingerprint:016x}"));
        }

        // Declaration order, kept — see the module docs.
        PredicateTy::Record(fields) => {
            out.push('{');
            for (index, (name, field)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(schema.interner().resolve(*name).unwrap_or("?"));
                out.push(':');
                type_form(out, field, schema, names, fingerprints, elide);
            }
            out.push('}');
        }
    }
}

/// Every **stored** predicate, by id — the ones identity is over.
fn names_of(schema: &Schema) -> BTreeMap<PredicateId, String> {
    (0..schema.len())
        .filter_map(|index| {
            let id = PredicateId(index as u32);
            if schema.is_virtual(id) {
                return None;
            }
            Some((id, schema.get(id)?.name()?.to_owned()))
        })
        .collect()
}

fn self_referencing(schema: &Schema, id: PredicateId) -> bool {
    references(schema, id).contains(&id)
}

/// Every predicate a predicate's types mention.
fn references(schema: &Schema, id: PredicateId) -> BTreeSet<PredicateId> {
    fn walk(ty: &PredicateTy, into: &mut BTreeSet<PredicateId>) {
        match ty {
            PredicateTy::Fact(target) => {
                into.insert(*target);
            }
            PredicateTy::Record(fields) => {
                for (_, field) in fields.iter() {
                    walk(field, into);
                }
            }
            PredicateTy::Int | PredicateTy::Str => {}
        }
    }

    let mut out = BTreeSet::new();
    if let Some(predicate) = schema.get(id) {
        walk(&predicate.predicate().key, &mut out);
        if let Some(value) = predicate.predicate().value.as_ref() {
            walk(value, &mut out);
        }
    }
    out
}

/// Strongly-connected components, **dependencies first**.
///
/// Tarjan's, written iteratively: a schema is a data path, and a recursive walk over one
/// deep enough would be a stack overflow where
/// [conventions](../../../docs/conventions.md) requires an error. Tarjan emits each
/// component only after everything it reaches, which is exactly the order the
/// fingerprints need.
fn components(schema: &Schema, names: &BTreeMap<PredicateId, String>) -> Vec<Vec<PredicateId>> {
    #[derive(Default, Clone)]
    struct Node {
        index: Option<usize>,
        low: usize,
        on_stack: bool,
    }

    let ids: Vec<PredicateId> = names.keys().copied().collect();
    let edges: BTreeMap<PredicateId, Vec<PredicateId>> = ids
        .iter()
        .map(|id| {
            let mut targets: Vec<PredicateId> = references(schema, *id)
                .into_iter()
                .filter(|target| names.contains_key(target))
                .collect();
            targets.sort();
            (*id, targets)
        })
        .collect();

    let mut state: BTreeMap<PredicateId, Node> =
        ids.iter().map(|id| (*id, Node::default())).collect();
    let mut stack: Vec<PredicateId> = vec![];
    let mut out: Vec<Vec<PredicateId>> = vec![];
    let mut next = 0usize;

    for root in &ids {
        if state[root].index.is_some() {
            continue;
        }

        // (node, how many of its edges have been taken)
        let mut work: Vec<(PredicateId, usize)> = vec![(*root, 0)];

        while let Some((node, edge)) = work.pop() {
            if edge == 0 {
                let entry = state.get_mut(&node).expect("known node");
                entry.index = Some(next);
                entry.low = next;
                entry.on_stack = true;
                next += 1;
                stack.push(node);
            }

            let targets = &edges[&node];

            if edge < targets.len() {
                let target = targets[edge];
                work.push((node, edge + 1));

                match state[&target].index {
                    None => work.push((target, 0)),
                    Some(index) => {
                        if state[&target].on_stack {
                            let low = state[&node].low.min(index);
                            state.get_mut(&node).expect("known node").low = low;
                        }
                    }
                }
                continue;
            }

            // Every edge taken: close the node, and propagate its low-link upward.
            if state[&node].low == state[&node].index.expect("visited") {
                let mut group = vec![];
                while let Some(member) = stack.pop() {
                    state.get_mut(&member).expect("known node").on_stack = false;
                    group.push(member);
                    if member == node {
                        break;
                    }
                }
                group.sort();
                out.push(group);
            }

            if let Some((parent, _)) = work.last().copied() {
                let low = state[&parent].low.min(state[&node].low);
                state.get_mut(&parent).expect("known node").low = low;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{lower::lower, parse::parse};

    fn schema_of(source: &str) -> Schema {
        let mut diags = vec![];
        let cst = parse(source, &mut diags).expect("parses");
        let lowered = lower(&cst, &mut diags).expect("lowers");
        assert!(diags.is_empty(), "{diags:?}");
        lowered.schema
    }

    fn identity_of(source: &str) -> Identity {
        identity(&schema_of(source))
    }

    /// **The canonical form is the specification**, so it is asserted literally rather
    /// than by round-trip. A round-trip would pass for any self-consistent encoding,
    /// including one the .NET client cannot reproduce — which is the whole risk D2 names.
    #[test]
    fn the_canonical_form_is_exactly_what_is_documented() {
        let identity = identity_of(
            "schema src { predicate File : string\n \
             predicate Decl : { module : string, line : int } -> string }",
        );

        let file = identity.of("src.File").expect("src.File");

        assert_eq!(
            identity.canonical(),
            format!(
                "aperture-schema-v1\n\
                 src.Decl:{{module:string,line:int}}->string\n\
                 src.File:string\n"
            ),
            "the form drifted from its own grammar"
        );

        // And a reference spells the referent's name *and* its fingerprint.
        let with_ref = identity_of(
            "schema src { predicate File : string\n \
             predicate Module : { file : File } }",
        );
        assert!(
            with_ref
                .canonical()
                .contains(&format!("@src.File#{file:016x}")),
            "a reference carries the referent's fingerprint:\n{}",
            with_ref.canonical()
        );
    }

    /// **Layout is not identity.** Declaration order and how blocks are split across a
    /// file cannot move a fingerprint — otherwise reformatting would invalidate every
    /// fact file, and `ops-I4` with it.
    #[test]
    fn declaration_order_and_file_layout_do_not_move_the_fingerprint() {
        let one = identity_of(
            "schema src { predicate File : string\n \
             predicate Module : { file : File, name : string } }",
        );
        let permuted = identity_of(
            "schema src { predicate Module : { file : File, name : string } }\n\
             schema src { predicate File : string }",
        );

        assert_eq!(one.schema(), permuted.schema());
        assert_eq!(one.predicates(), permuted.predicates());
        assert_eq!(one.canonical(), permuted.canonical());
    }

    /// **Field order *is* identity**, which is the negative control the guard needs: a
    /// property that only ever answers "the same" is a property that says nothing.
    #[test]
    fn permuting_a_records_fields_moves_the_fingerprint() {
        let one = identity_of("schema src { predicate P : { a : int, b : string } }");
        let other = identity_of("schema src { predicate P : { b : string, a : int } }");

        assert_ne!(
            one.of("src.P"),
            other.of("src.P"),
            "a field permutation is a semantic change — it decides the seek prefix"
        );
        assert_ne!(one.schema(), other.schema());
    }

    /// A change in a referent reaches everything that references it — the Merkle
    /// property, and the reason a reference carries a fingerprint rather than a name.
    #[test]
    fn changing_a_referent_changes_its_referrers() {
        let before = identity_of(
            "schema src { predicate File : string\n \
             predicate Module : { file : File } }",
        );
        let after = identity_of(
            "schema src { predicate File : int\n \
             predicate Module : { file : File } }",
        );

        assert_ne!(before.of("src.File"), after.of("src.File"), "it changed");
        assert_ne!(
            before.of("src.Module"),
            after.of("src.Module"),
            "and the change propagated to what points at it"
        );
    }

    /// **The cycle case.** Two predicates referencing each other have no base case under
    /// "a reference carries the referent's fingerprint", and this is Glean's answer:
    /// every member distinct, and a change to either moving both.
    #[test]
    fn a_reference_cycle_is_hashed_as_a_group() {
        let cyclic = identity_of(
            "schema src { predicate A : { b : B }\n \
             predicate B : { a : A } }",
        );

        let a = cyclic.of("src.A").expect("src.A");
        let b = cyclic.of("src.B").expect("src.B");
        assert_ne!(a, b, "members of a cycle are distinct");

        // Changing one member moves both, because the group is hashed together.
        let changed = identity_of(
            "schema src { predicate A : { b : B, extra : int }\n \
             predicate B : { a : A } }",
        );

        assert_ne!(a, changed.of("src.A").expect("src.A"));
        assert_ne!(
            b,
            changed.of("src.B").expect("src.B"),
            "a cycle's hash covers the whole group"
        );
    }

    /// A predicate that references itself is a cycle of one, and must not fall into the
    /// acyclic path — where it would spell its own not-yet-known fingerprint as zero and
    /// collide with anything else that did.
    #[test]
    fn a_self_reference_is_a_cycle_of_one() {
        let identity = identity_of("schema src { predicate Node : { parent : Node } }");
        assert!(identity.of("src.Node").is_some());
        assert!(identity.schema() != 0);
    }

    /// Subset containment: adding a predicate is the only compatible change.
    #[test]
    fn compatibility_is_subset_containment() {
        let old = identity_of("schema src { predicate A : string }");
        let same = identity_of("schema src { predicate A : string }");
        let added = identity_of("schema src { predicate A : string\n predicate B : int }");
        let changed = identity_of("schema src { predicate A : int }");
        let removed = identity_of("schema src { predicate B : int }");

        assert_eq!(old.compatibility(&same), Compatibility::Identical);
        assert_eq!(
            old.compatibility(&added),
            Compatibility::Compatible { added: 1 }
        );
        assert_eq!(
            old.compatibility(&changed),
            Compatibility::Breaking {
                broken: vec!["src.A".to_owned()]
            }
        );
        assert_eq!(
            old.compatibility(&removed),
            Compatibility::Breaking {
                broken: vec!["src.A".to_owned()]
            }
        );
    }

    /// **Two schemas that number their predicates differently are still one schema.**
    ///
    /// The text-level claim is above; this is the same one where it can actually bite,
    /// over hand-built schemas whose positions differ. A reference is spelled by name
    /// and fingerprint rather than by id, so the two below point at the same predicate
    /// through two different ids and must come out alike — the guard that a position
    /// never leaks into identity. It moved here when the provisional fingerprint it was
    /// written against was deleted; the failure it records is real, and was a .NET demo
    /// refused at the handshake by a server it agreed with predicate for predicate.
    #[test]
    fn positions_are_not_part_of_identity() {
        use std::sync::Arc;

        use lasso::Rodeo;

        use crate::schema::Predicate;

        fn built(swapped: bool) -> Schema {
            let mut rodeo = Rodeo::new();
            let (a, b, field) = (
                rodeo.get_or_intern("t.A"),
                rodeo.get_or_intern("t.B"),
                rodeo.get_or_intern("a"),
            );

            let at = u32::from(swapped);

            let first = Predicate {
                name: a,
                key: PredicateTy::Str,
                value: None,
            };
            let second = Predicate {
                name: b,
                key: PredicateTy::Record(Arc::from([(
                    field,
                    PredicateTy::Fact(PredicateId(1 - at)),
                )])),
                value: None,
            };

            let predicates = if swapped {
                vec![second, first]
            } else {
                vec![first, second]
            };

            Schema::new(rodeo.into_reader(), Arc::from(predicates))
        }

        assert_eq!(
            of(&built(false)),
            of(&built(true)),
            "the same schema written in two orders is the same schema"
        );

        // The control: order not mattering must not have made *content* stop mattering.
        let mut rodeo = Rodeo::new();
        let name = rodeo.get_or_intern("t.A");
        let different = Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![Predicate {
                name,
                key: PredicateTy::Int,
                value: None,
            }]),
        );

        assert_ne!(of(&built(false)), of(&different));
    }

    /// **A virtual predicate is invisible to identity**, which is the property the
    /// whole virtual/stored split rests on: a server that grows one must not stop every
    /// client that has never heard of it, and no artifact may claim to hold a kind of
    /// fact nothing can write to it.
    #[test]
    fn a_virtual_predicate_is_not_part_of_a_schemas_identity() {
        let stored = identity_of("schema src { predicate File : string }");

        let served = schema_of(
            "schema src { predicate File : string }\n\
             schema aperture.db { predicate List : string }",
        );
        let (id, _) = served.find_position("aperture.db.List").expect("declared");
        let served = identity(&served.with_virtual([id]));

        assert_eq!(stored.schema(), served.schema());
        assert_eq!(stored.canonical(), served.canonical());
        assert_eq!(served.of("aperture.db.List"), None);
    }

    /// The version is inside the hash, so changing the algorithm is visible rather than
    /// silent — which is the half of D2 this module can enforce on its own.
    #[test]
    fn the_version_is_part_of_the_canonical_form() {
        let identity = identity_of("schema src { predicate A : string }");
        assert!(identity.canonical().starts_with("aperture-schema-v1\n"));
    }
}
