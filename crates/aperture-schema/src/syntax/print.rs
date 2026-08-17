//! [`Schema`] → source — the form a database **embeds**.
//!
//! [I13](../../../../docs/invariants.md#i13) asks a database to carry its own schema,
//! and this is the half that makes the copy worth carrying: text in the same language
//! `aperture create --schema` takes, so a reader needs no second format and the parser
//! that already exists is the one that reads it back.
//!
//! # Why not the canonical form
//!
//! [`fingerprint`](crate::fingerprint) already renders a schema as a byte string, and
//! embedding *that* would need a second parser for a second grammar — one whose only
//! reader would be this crate. The canonical form's job is to be hashed; this one's is
//! to be read, diffed by a person, and handed back to `create`. They are different jobs
//! and the fingerprint keeps them honest: an embedded copy that lowers to a different
//! schema has a different fingerprint from the one the sidecar recorded.
//!
//! # Order is the artifact, which is why reading it back is [`recover`]
//!
//! Predicates are printed **in id order**, because an id is a position and every
//! [`FactId`](crate::id::FactId) a database holds carries one as its tag. Reading the
//! copy back with [`lower`](super::lower::lower) would re-assign ids by sorted name
//! ([D1](../../../../docs/phase-8-schemas.md)) — right for a schema being *declared*,
//! and wrong for one being *recovered*, where the numbering is already frozen on disk.
//! For every schema written down as text the two agree, since lowering sorted them in
//! the first place; for a hand-built one they need not, and the difference is a database
//! whose keyspace names no longer match its predicates.

use std::fmt::Write;

use crate::schema::{PredicateId, PredicateTy, Schema};

/// Write `schema` back out as source.
///
/// Virtual predicates are skipped: one is answered by whoever runs the query rather
/// than stored, so an artifact must not claim to hold a kind of fact nothing can write
/// to it — the same rule the fingerprint and the keyspaces follow.
///
/// A predicate whose name has no namespace is not expressible (`schema { … }` is not a
/// schema), and rather than invent one this prints an empty namespace, which does not
/// parse. That is deliberate: the check that matters is
/// [`equivalent`] over the round trip, and a name that cannot be written down should
/// fail it loudly rather than come back as something else.
#[must_use]
pub fn print(schema: &Schema) -> String {
    let mut out = String::new();
    let mut open: Option<&str> = None;

    for index in 0..schema.len() {
        let id = PredicateId(index as u32);

        if schema.is_virtual(id) {
            continue;
        }

        let Some(predicate) = schema.get(id) else {
            continue;
        };
        let Some(qualified) = predicate.name() else {
            continue;
        };

        let (namespace, name) = split(qualified);

        if open != Some(namespace) {
            if open.is_some() {
                out.push_str("}\n\n");
            }
            let _ = writeln!(out, "schema {namespace} {{");
            open = Some(namespace);
        }

        out.push_str("  predicate ");
        out.push_str(name);
        out.push_str(" : ");
        ty(&mut out, schema, &predicate.predicate().key);

        if let Some(value) = predicate.predicate().value.as_ref() {
            out.push_str(" -> ");
            ty(&mut out, schema, value);
        }

        out.push('\n');
    }

    if open.is_some() {
        out.push_str("}\n");
    }

    out
}

/// Whether two schemas hold the same predicates, **at the same positions**.
///
/// Not `PartialEq`: a `Schema` holds interned symbols, so two built from the same text
/// by different interners are unequal as values and identical as schemas. Position is
/// part of the claim rather than a detail — it is what a `FactId`'s tag names — so this
/// compares by id and not by name lookup.
#[must_use]
pub fn equivalent(left: &Schema, right: &Schema) -> bool {
    let stored = |schema: &Schema| {
        (0..schema.len())
            .map(|index| PredicateId(index as u32))
            .filter(|id| !schema.is_virtual(*id))
            .collect::<Vec<_>>()
    };

    let (ours, theirs) = (stored(left), stored(right));

    if ours.len() != theirs.len() {
        return false;
    }

    ours.iter().zip(&theirs).all(|(&ours, &theirs)| {
        let (Some(a), Some(b)) = (left.get(ours), right.get(theirs)) else {
            return false;
        };

        ours == theirs
            && a.name() == b.name()
            && same_ty(left, &a.predicate().key, right, &b.predicate().key)
            && match (a.predicate().value.as_ref(), b.predicate().value.as_ref()) {
                (None, None) => true,
                (Some(a), Some(b)) => same_ty(left, a, right, b),
                _ => false,
            }
    })
}

fn same_ty(left: &Schema, ours: &PredicateTy, right: &Schema, theirs: &PredicateTy) -> bool {
    match (ours, theirs) {
        (PredicateTy::Int, PredicateTy::Int) | (PredicateTy::Str, PredicateTy::Str) => true,

        // **By id.** A reference is a position, and two schemas that name the same
        // target from different positions are not the same schema to anything that
        // decodes a stored row.
        (PredicateTy::Fact(ours), PredicateTy::Fact(theirs)) => ours == theirs,

        (PredicateTy::Record(ours), PredicateTy::Record(theirs)) => {
            ours.len() == theirs.len()
                && ours
                    .iter()
                    .zip(theirs.iter())
                    .all(|((a, ours), (b, theirs))| {
                        left.interner().resolve(*a) == right.interner().resolve(*b)
                            && same_ty(left, ours, right, theirs)
                    })
        }

        _ => false,
    }
}

/// `src.Decl` → (`src`, `Decl`); a name with no dot has no namespace.
fn split(qualified: &str) -> (&str, &str) {
    match qualified.rsplit_once('.') {
        Some((namespace, name)) => (namespace, name),
        None => ("", qualified),
    }
}

fn ty(out: &mut String, schema: &Schema, shape: &PredicateTy) {
    match shape {
        PredicateTy::Int => out.push_str("int"),
        PredicateTy::Str => out.push_str("string"),

        // **Fully qualified, always.** A bare name resolves in the block's own
        // namespace, so printing one would make a cross-namespace reference come back
        // pointing at a predicate in the wrong one — or at nothing.
        PredicateTy::Fact(target) => {
            out.push_str(schema.get(*target).and_then(|p| p.name()).unwrap_or(""));
        }

        PredicateTy::Record(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }

            out.push_str("{ ");
            for (index, (name, field)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(schema.interner().resolve(*name).unwrap_or(""));
                out.push_str(" : ");
                ty(out, schema, field);
            }
            out.push_str(" }");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lasso::Rodeo;

    use super::*;
    use crate::{
        schema::Predicate,
        syntax::{
            corpus::{CORPUS, Verdict},
            lower::{lower, recover},
            parse::parse,
        },
    };

    fn read(source: &str, declared_order: bool) -> Schema {
        let mut diags = vec![];
        let cst = parse(source, &mut diags).expect("parses");
        let lowered = if declared_order {
            recover(&cst, &mut diags)
        } else {
            lower(&cst, &mut diags)
        }
        .expect("lowers");

        assert!(diags.is_empty(), "{source}\n{diags:?}");
        lowered.schema
    }

    /// **The round trip over the whole surface that lowers.** Printing is only worth
    /// anything if what comes back is the same schema, and the corpus is the widest
    /// statement of "the surface" this crate has.
    #[test]
    fn every_corpus_schema_survives_being_written_back() {
        for entry in CORPUS {
            if entry.verdict != Verdict::Lowers {
                continue;
            }

            let schema = read(entry.source, false);
            let printed = print(&schema);
            let back = read(&printed, true);

            assert!(
                equivalent(&schema, &back),
                "`{}` did not survive:\n{printed}",
                entry.about
            );
        }
    }

    /// **A schema nobody sorted still comes back at the same positions**, which is the
    /// whole reason [`recover`] exists. Written `Zebra` before `Apple` on purpose: under
    /// [`lower`]'s sorted assignment the two swap, and every `FactId` already written
    /// would then name the other one.
    #[test]
    fn declaration_order_is_recovered_and_sorting_would_lose_it() {
        let mut rodeo = Rodeo::new();
        let mut sym = |s: &str| rodeo.get_or_intern(s);
        let (zebra, apple) = (sym("test.Zebra"), sym("test.Apple"));
        let field = sym("of");

        let schema = Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: zebra,
                    key: PredicateTy::Str,
                    value: None,
                },
                Predicate {
                    name: apple,
                    key: PredicateTy::Record(Arc::from([(
                        field,
                        PredicateTy::Fact(PredicateId(0)),
                    )])),
                    value: Some(PredicateTy::Int),
                },
            ]),
        );

        let printed = print(&schema);

        assert!(
            equivalent(&schema, &read(&printed, true)),
            "recovered:\n{printed}"
        );
        assert!(
            !equivalent(&schema, &read(&printed, false)),
            "if sorting kept the positions there would be nothing for `recover` to do"
        );
    }

    /// A virtual predicate belongs to the server, so an artifact's copy must not name
    /// one — the same rule the fingerprint and the keyspaces follow.
    #[test]
    fn a_virtual_predicate_is_not_written_down() {
        let schema = read(
            "schema src { predicate File : string }\n\
             schema aperture.db { predicate List : string }",
            false,
        );

        let (id, _) = schema.find_position("aperture.db.List").expect("declared");
        let served = schema.with_virtual([id]);

        let printed = print(&served);

        assert!(!printed.contains("List"), "{printed}");
        assert!(printed.contains("File"), "{printed}");
        assert!(equivalent(&served, &read(&printed, true)));
    }

    /// The printed form is what a person reads, so it is asserted literally once —
    /// a round trip alone would pass for output nobody could stand to look at.
    #[test]
    fn what_it_looks_like() {
        let schema = read(
            "schema src { predicate File : string\n\
             predicate Ref : { at : { line : int, col : int }, file : File } -> string }",
            false,
        );

        assert_eq!(
            print(&schema),
            "schema src {\n  \
               predicate File : string\n  \
               predicate Ref : { at : { line : int, col : int }, file : src.File } -> string\n\
             }\n"
        );
    }

    /// A reference across namespaces stays pointed where it was written.
    #[test]
    fn a_cross_namespace_reference_is_printed_qualified() {
        let schema = read(
            "schema src { predicate Decl : string }\n\
             schema a { predicate P : { d : src.Decl } }",
            false,
        );

        let printed = print(&schema);
        assert!(printed.contains("d : src.Decl"), "{printed}");
        assert!(equivalent(&schema, &read(&printed, true)));
    }
}
