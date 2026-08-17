//! CST → [`Schema`]: the type model, built from text.
//!
//! Two passes, and the split is forced rather than stylistic. A predicate may name one
//! declared later in the file — or in another block entirely — so **every declaration is
//! collected before any type is resolved**. The first pass also assigns ids, because a
//! `PredicateTy::Fact` holds the referent's id and cannot be built until every id exists.
//!
//! # Ids come from the sorted name, and that is D1
//!
//! [`phase-8-schemas.md`](../../../../docs/phase-8-schemas.md) settles this: an id is a
//! property of the *database*, assigned by sorted qualified name and then persisted and
//! append-only, never a function of where a declaration sits in a file. This module does
//! the assigning half — sort, then enumerate — which is what makes two orderings of one
//! schema produce identical ids and therefore identical databases.
//!
//! # What is refused, and what is merely absent
//!
//! Everything the grammar accepts and the type model cannot hold is refused **by name**
//! here rather than in the parser ([`Code`]), which is the whole of permissive-early.
//! Imports are *collected and not yet acted on*: resolving them is 8.4, and a schema that
//! imports nothing is the only kind this step can fully answer.

use std::collections::BTreeMap;

use lasso::Rodeo;

use crate::schema::{Predicate, PredicateId, PredicateTy, Schema};

use super::{
    diag::{Code, Diagnostic},
    parser::{Cst, Node, NodeRef, Rule, Span},
};

/// How deep a chain of type aliases may go before it is called a cycle.
///
/// An alias is *expanded*, so `type A = B` / `type B = A` is genuinely infinite — unlike
/// a predicate reference, which is an id and may point wherever it likes. A depth bound
/// is the cheap way to catch it, and it doubles as a bound on absurd-but-finite nesting.
const MAX_ALIAS_DEPTH: usize = 32;

/// The namespace reserved for predicates a **server** answers rather than stores.
///
/// Everything under it is numbered after every stored predicate, so that serving one
/// cannot move an id a database has already written into its keyspace names and into
/// every `FactId` it holds. Glean reserves `builtin` for the same kind of reason.
pub const RESERVED_NAMESPACE: &str = "aperture.";

/// What one source lowered to.
pub struct Lowered {
    pub schema: Schema,
    /// Namespaces this source imports, in the order written.
    ///
    /// Collected rather than resolved: 8.4 owns resolution, and recording them now is
    /// what lets that step be about *finding* files rather than about parsing again.
    pub imports: Vec<String>,
}

/// A declaration, before its types are resolved.
struct Declared<'s> {
    /// `src.File` — the name a query writes, and the name identity is keyed by.
    qualified: String,
    namespace: &'s str,
    ty: NodeRef,
    value: Option<NodeRef>,
}

/// Where a predicate's id comes from.
///
/// Two callers, and the difference between them is [D1](../../../../docs/phase-8-schemas.md)
/// stated as code: a schema being **declared** is numbered by sorted name, so that two
/// orderings of one schema build the same database; a schema being **recovered** from
/// the copy a database embedded already has its numbering, frozen in the tag of every
/// [`FactId`](crate::id::FactId) it holds, and re-assigning it would rename its
/// keyspaces underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Numbering {
    /// Sort by qualified name, then enumerate.
    Assigned,
    /// Take the order the file lists them in.
    Declared,
}

/// Lower `cst` to a schema, reporting into `diags`.
///
/// `None` means nothing usable came back — every predicate was refused, or the file
/// declared none. Diagnostics accumulate rather than stopping at the first, because a
/// reader fixing a schema wants every complaint at once.
#[must_use]
pub fn lower(cst: &Cst<'_>, diags: &mut Vec<Diagnostic>) -> Option<Lowered> {
    build(cst, diags, Numbering::Assigned)
}

/// Lower a schema **a database already numbered**: ids come from the order written.
///
/// The reader of [`print`](super::print::print)'s output, and nothing else should call
/// it. For any schema that was declared as text the two agree — lowering is what sorted
/// it — so the difference only shows for a hand-built schema, where it is the difference
/// between recovering a database's numbering and inventing a new one.
#[must_use]
pub fn recover(cst: &Cst<'_>, diags: &mut Vec<Diagnostic>) -> Option<Lowered> {
    build(cst, diags, Numbering::Declared)
}

fn build(cst: &Cst<'_>, diags: &mut Vec<Diagnostic>, numbering: Numbering) -> Option<Lowered> {
    let mut predicates: Vec<Declared> = vec![];
    let mut aliases: BTreeMap<String, (NodeRef, &str)> = BTreeMap::new();
    let mut imports = vec![];
    let mut seen: BTreeMap<String, Span> = BTreeMap::new();

    // ---- pass one: collect every declaration -------------------------------------
    for decl in kids(cst, NodeRef::ROOT) {
        match rule(cst, decl) {
            Some(Rule::EvolvesDecl) => diags.push(Code::NyiEvolves.at(
                cst.span(decl),
                "`evolves` is not available: P0 freezes a schema at create, so there is \
                 no second schema to evolve into",
            )),

            Some(Rule::SchemaDecl) => {
                let Some(namespace) = first_text(cst, decl, Rule::Ns) else {
                    continue;
                };

                for item in kids(cst, decl) {
                    collect(
                        cst,
                        item,
                        namespace,
                        &mut predicates,
                        &mut aliases,
                        &mut imports,
                        &mut seen,
                        diags,
                    );
                }
            }

            _ => {}
        }
    }

    // ---- ids: sorted by qualified name, then enumerated ---------------------------
    //
    // The sort is what makes this independent of where a declaration was written, which
    // is requirement (1) of D1 — reproducibility — and it is why two files listing the
    // same predicates in different orders build the same database.
    //
    // **`aperture.*` sorts last, and that is load-bearing rather than tidy.** A server
    // serves the stored schema *plus* the predicates it answers itself, while a database
    // creates its keyspaces from the stored one alone. If a reserved name could sort into
    // the middle, adding one would shift every stored id — and every query would then
    // read a keyspace belonging to a different predicate. Reserving the namespace to the
    // end makes "adding a virtual predicate renumbers nothing" true by construction,
    // which is the same rule D1 gives additions in general.
    if numbering == Numbering::Assigned {
        predicates.sort_by(|a, b| {
            let key = |d: &Declared| {
                (
                    d.qualified.starts_with(RESERVED_NAMESPACE),
                    d.qualified.clone(),
                )
            };
            key(a).cmp(&key(b))
        });
    }

    let ids: BTreeMap<&str, PredicateId> = predicates
        .iter()
        .enumerate()
        .map(|(index, declared)| (declared.qualified.as_str(), PredicateId(index as u32)))
        .collect();

    // ---- pass two: resolve types --------------------------------------------------
    let mut rodeo = Rodeo::new();
    let mut cache: BTreeMap<String, Option<PredicateTy>> = BTreeMap::new();

    // **Every alias, whether anything uses it or not.** A declaration that would be
    // refused where it is used should be refused where it is written — otherwise
    // `type C = enum { … }` sits in a schema looking accepted until the day somebody
    // names it.
    for (name, (node, namespace)) in &aliases {
        if cache.contains_key(name) {
            continue;
        }

        let resolved = Resolver {
            cst,
            nodes: &aliases,
            cache: &mut cache,
            ids: &ids,
            namespace,
            rodeo: &mut rodeo,
            diags,
        }
        .ty(*node, 0);

        cache.insert(name.clone(), resolved);
    }

    let mut built = Vec::with_capacity(predicates.len());

    for declared in &predicates {
        let mut resolve = Resolver {
            cst,
            nodes: &aliases,
            cache: &mut cache,
            ids: &ids,
            namespace: declared.namespace,
            rodeo: &mut rodeo,
            diags,
        };

        let key = resolve.ty(declared.ty, 0);
        let value = declared.value.map(|node| resolve.ty(node, 0));

        // A key that did not resolve is a predicate that cannot be written or read, so
        // it is dropped rather than embedded half-formed — the diagnostic already said
        // why, and carrying a hole forward would produce a second, worse one later.
        let (Some(key), value) = (key, value.flatten()) else {
            continue;
        };

        built.push(Predicate {
            name: rodeo.get_or_intern(&declared.qualified),
            key,
            value,
        });
    }

    if built.len() != predicates.len() {
        // Some predicate was refused. The ids above were assigned over *all* of them, so
        // continuing would hand back a schema whose positions no longer match its ids —
        // the one thing a `PredicateId` may not do.
        //
        // **And a refusal always has a reason.** Returning `None` with an empty sink is
        // a compiler bug rather than a bad schema, and it is one that costs an afternoon:
        // the caller reports "it did not lower" and has nothing to point at. This caught
        // exactly that when a grammar rule turned out to be inlined away.
        debug_assert!(
            !diags.is_empty(),
            "lowering refused {} of {} predicates and said nothing",
            predicates.len() - built.len(),
            predicates.len()
        );
        return None;
    }

    Some(Lowered {
        schema: Schema::new(rodeo.into_reader(), built.into()),
        imports,
    })
}

/// One item of a schema block.
#[allow(clippy::too_many_arguments)]
fn collect<'s>(
    cst: &Cst<'s>,
    item: NodeRef,
    namespace: &'s str,
    predicates: &mut Vec<Declared<'s>>,
    aliases: &mut BTreeMap<String, (NodeRef, &'s str)>,
    imports: &mut Vec<String>,
    seen: &mut BTreeMap<String, Span>,
    diags: &mut Vec<Diagnostic>,
) {
    match rule(cst, item) {
        Some(Rule::ImportItem) => {
            if let Some(ns) = first_text(cst, item, Rule::Ns) {
                imports.push(ns.to_owned());
            }
        }

        Some(Rule::DeriveItem) => diags.push(Code::NyiDerivation.at(
            cst.span(item),
            "a derived predicate needs the query language, which is not available to a \
             schema yet — see PLAN Phase 8b",
        )),

        Some(Rule::TypeItem) => {
            let Some(name) = token_text(cst, item, |t| matches!(t, super::lexer::Token::UId))
            else {
                return;
            };
            let qualified = format!("{namespace}.{name}");

            if let Some(ty) = kids(cst, item).find(|node| is_ty(cst, *node)) {
                declare(&qualified, cst.span(item), seen, diags);
                aliases.insert(qualified, (ty, namespace));
            }
        }

        Some(Rule::PredicateItem) => {
            let Some(name) = token_text(cst, item, |t| matches!(t, super::lexer::Token::UId))
            else {
                return;
            };
            let qualified = format!("{namespace}.{name}");

            if has_token(cst, item, super::lexer::Token::Stored) {
                diags.push(Code::NyiDerivation.at(
                    cst.span(item),
                    "`stored` marks a derived predicate, which needs the query language \
                     a schema cannot reach yet — see PLAN Phase 8b",
                ));
                return;
            }

            let mut types = kids(cst, item).filter(|node| is_ty(cst, *node));
            let (Some(key), value) = (types.next(), types.next()) else {
                return;
            };

            declare(&qualified, cst.span(item), seen, diags);
            predicates.push(Declared {
                qualified,
                namespace,
                ty: key,
                value,
            });
        }

        _ => {}
    }
}

/// Record a name, complaining if it is the second definition of one.
///
/// Operations §7's **genuine** redeclaration: two different definitions of one
/// fully-qualified name, as against the same file reached twice, which 8.4's dedup by
/// file identity handles and which is not an error at all.
fn declare(
    qualified: &str,
    span: Span,
    seen: &mut BTreeMap<String, Span>,
    diags: &mut Vec<Diagnostic>,
) {
    if seen.contains_key(qualified) {
        diags.push(Code::RejectRedeclaration.at(
            span,
            format!("`{qualified}` is already declared in this schema"),
        ));
        return;
    }

    seen.insert(qualified.to_owned(), span);
}

/// Resolving a type needs the whole declaration environment, so it travels together.
struct Resolver<'a, 's> {
    cst: &'a Cst<'s>,
    nodes: &'a BTreeMap<String, (NodeRef, &'s str)>,
    /// What each alias resolved to, filled in as they are reached.
    ///
    /// **Memoised so a diagnostic is reported once.** An alias used by three predicates
    /// would otherwise be walked three times and complain three times, and an alias used
    /// by *none* would never be walked at all — which is how `type C = enum {…}` came to
    /// pass silently until the corpus caught it.
    cache: &'a mut BTreeMap<String, Option<PredicateTy>>,
    ids: &'a BTreeMap<&'a str, PredicateId>,
    namespace: &'s str,
    rodeo: &'a mut Rodeo,
    diags: &'a mut Vec<Diagnostic>,
}

impl Resolver<'_, '_> {
    fn ty(&mut self, node: NodeRef, depth: usize) -> Option<PredicateTy> {
        if depth > MAX_ALIAS_DEPTH {
            self.diags.push(Code::RejectTypeCycle.at(
                self.cst.span(node),
                "this type expands into itself — a named type is substituted where it is \
                 used, so a cycle among them has no base case",
            ));
            return None;
        }

        match rule(self.cst, node)? {
            Rule::GroupTy => {
                let inner = kids(self.cst, node).find(|n| is_ty(self.cst, *n))?;
                self.ty(inner, depth)
            }

            Rule::ArrayTy => self.refuse(
                node,
                Code::NyiArray,
                "an array type is not available: a one-to-many is written as one fact \
                 per element (see open-decisions.md)",
            ),
            Rule::SetTy => self.refuse(node, Code::NyiSet, "a set type is not available"),
            Rule::MaybeTy => self.refuse(
                node,
                Code::NyiMaybe,
                "`maybe` is sugar over a union, and waits on one",
            ),
            Rule::EnumTy => self.refuse(
                node,
                Code::NyiEnum,
                "an enumeration is sugar over a union, and waits on one",
            ),

            Rule::BuiltinTy => {
                let name = self.text(node)?;
                match name {
                    "int" => Some(PredicateTy::Int),
                    "string" => Some(PredicateTy::Str),
                    other => self.refuse(
                        node,
                        Code::RejectUnknownName,
                        format!("there is no type called `{other}`"),
                    ),
                }
            }

            // `Decl` — a name in this namespace. A predicate first, then a named type,
            // because a predicate is the thing a reference can *point at*: an alias
            // resolving to a record would silently make a copy where the author meant a
            // reference.
            Rule::RefTy => {
                let name = self.text(node)?;
                let qualified = format!("{}.{name}", self.namespace);
                self.named(node, &qualified, depth)
            }

            // `src.Decl` — already qualified.
            Rule::QrefTy => {
                let qualified = self.text(node)?.to_owned();
                self.named(node, &qualified, depth)
            }

            Rule::BracedTy => self.braced(node, depth),

            _ => None,
        }
    }

    /// A name that is either a predicate (a reference) or an alias (an expansion).
    fn named(&mut self, node: NodeRef, qualified: &str, depth: usize) -> Option<PredicateTy> {
        if let Some(id) = self.ids.get(qualified) {
            return Some(PredicateTy::Fact(*id));
        }

        if let Some(resolved) = self.cache.get(qualified) {
            return resolved.clone();
        }

        if let Some((node, namespace)) = self.nodes.get(qualified).copied() {
            // The alias resolves in *its own* namespace, not the one referring to it.
            let outer = std::mem::replace(&mut self.namespace, namespace);
            let resolved = self.ty(node, depth + 1);
            self.namespace = outer;

            self.cache.insert(qualified.to_owned(), resolved.clone());
            return resolved;
        }

        self.refuse(
            node,
            Code::RejectUnknownName,
            format!("nothing named `{qualified}` is declared"),
        )
    }

    /// `{ … }` — a record, or a union if its fields are separated by `|`.
    fn braced(&mut self, node: NodeRef, depth: usize) -> Option<PredicateTy> {
        let Some(list) = kids(self.cst, node).find(|n| {
            matches!(
                rule(self.cst, *n),
                Some(Rule::RecordFields | Rule::SumFields)
            )
        }) else {
            // `{}` — the empty record, which is a real type and Angle's `Unit`.
            return Some(PredicateTy::Record(Vec::new().into()));
        };

        if rule(self.cst, list) == Some(Rule::SumFields) {
            return self.refuse(
                node,
                Code::NyiUnion,
                "a union is not available until `PredicateTy` has one — its discriminants \
                 are frozen the moment a union fact is written (I10)",
            );
        }

        let mut fields = Vec::new();

        for field in kids(self.cst, list).filter(|n| rule(self.cst, *n) == Some(Rule::Field)) {
            // A discriminant belongs to an alternative, and a record has none — accepted
            // by the grammar so this can be a sentence rather than a caret.
            if has_token(self.cst, field, super::lexer::Token::Nat) {
                self.diags.push(Code::RejectDiscriminantOnRecordField.at(
                    self.cst.span(field),
                    "a discriminant tags a union's alternative; a record field has no tag",
                ));
                return None;
            }

            let name = field_name(self.cst, field)?;
            let ty = kids(self.cst, field).find(|n| is_ty(self.cst, *n))?;
            let ty = self.ty(ty, depth)?;

            fields.push((self.rodeo.get_or_intern(name), ty));
        }

        // **Declaration order, kept.** A record's field order is its encoding order and
        // decides the seek prefix, so this is the one place in the pipeline where *not*
        // sorting is the requirement (chapter 6).
        Some(PredicateTy::Record(fields.into()))
    }

    fn refuse<T>(
        &mut self,
        node: NodeRef,
        code: Code,
        message: impl std::fmt::Display,
    ) -> Option<T> {
        self.diags.push(code.at(self.cst.span(node), message));
        None
    }

    fn text(&self, node: NodeRef) -> Option<&'_ str> {
        kids(self.cst, node)
            .find_map(|child| match self.cst.get(child) {
                Node::Token(_, _) => Some(&self.cst.source()[self.cst.span(child)]),
                Node::Rule(_, _) => None,
            })
            .or_else(|| Some(&self.cst.source()[self.cst.span(node)]))
    }
}

// ---- walking ------------------------------------------------------------------------

/// A node's children, with trivia dropped.
///
/// The CST is lossless — whitespace and comments are nodes — and every walker here wants
/// the structure rather than the layout.
fn kids<'a>(cst: &'a Cst<'a>, node: NodeRef) -> impl Iterator<Item = NodeRef> + 'a {
    cst.children(node).filter(move |child| {
        !matches!(
            cst.get(*child),
            Node::Token(
                super::lexer::Token::Whitespace | super::lexer::Token::Comment,
                _
            )
        )
    })
}

fn rule(cst: &Cst<'_>, node: NodeRef) -> Option<Rule> {
    match cst.get(node) {
        Node::Rule(rule, _) => Some(rule),
        Node::Token(_, _) => None,
    }
}

fn is_ty(cst: &Cst<'_>, node: NodeRef) -> bool {
    matches!(
        rule(cst, node),
        Some(
            Rule::ArrayTy
                | Rule::BracedTy
                | Rule::BuiltinTy
                | Rule::EnumTy
                | Rule::GroupTy
                | Rule::MaybeTy
                | Rule::QrefTy
                | Rule::RefTy
                | Rule::SetTy
                | Rule::Ty
        )
    )
}

fn has_token(cst: &Cst<'_>, node: NodeRef, token: super::lexer::Token) -> bool {
    kids(cst, node).any(|child| matches!(cst.get(child), Node::Token(t, _) if t == token))
}

fn token_text<'a>(
    cst: &'a Cst<'a>,
    node: NodeRef,
    want: impl Fn(super::lexer::Token) -> bool,
) -> Option<&'a str> {
    kids(cst, node).find_map(|child| match cst.get(child) {
        Node::Token(t, _) if want(t) => Some(&cst.source()[cst.span(child)]),
        _ => None,
    })
}

/// A field's name — its first token, whatever kind.
///
/// **Not a rule lookup.** `field_name` in the grammar is an alternation of single
/// tokens, and `lelwel` inlines such a rule rather than emitting a node for it, so
/// asking for a `FieldName` node finds nothing and answers `None` — which is how this
/// silently refused the whole built-in schema once. A field's first token *is* its
/// name, keyword or not.
fn field_name<'a>(cst: &'a Cst<'a>, field: NodeRef) -> Option<&'a str> {
    // The first child either way: `lelwel` wraps the alternation in a `field_name` node,
    // and reading it as *text* rather than by rule name means this keeps working whether
    // it wraps or inlines. Asking for a node by name is what silently refused the whole
    // built-in schema once — the lookup missed, `ty` answered `None`, and nothing said so.
    let first = kids(cst, field).next()?;
    Some(&cst.source()[cst.span(first)])
}

/// The text of a rule child, for rules that wrap a single token (`ns`).
fn first_text<'a>(cst: &'a Cst<'a>, node: NodeRef, of: Rule) -> Option<&'a str> {
    let child = kids(cst, node).find(|n| rule(cst, *n) == Some(of))?;
    Some(&cst.source()[cst.span(child)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parse::parse;

    fn schema_of(source: &str) -> Schema {
        let mut diags = vec![];
        let cst = parse(source, &mut diags).expect("it parses");
        let lowered = lower(&cst, &mut diags).expect("it lowers");

        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        lowered.schema
    }

    /// The names a schema holds, in id order.
    fn names(schema: &Schema) -> Vec<&str> {
        (0..schema.len())
            .filter_map(|index| schema.get(PredicateId(index as u32))?.name())
            .collect()
    }

    /// **Ids come from the sorted qualified name** — D1's assigning half.
    ///
    /// A position in the file decides nothing, which is what makes two spellings of one
    /// schema build the same database.
    #[test]
    fn ids_are_assigned_by_sorted_qualified_name() {
        let schema = schema_of(
            "schema src { predicate Zebra : string\n predicate Apple : string\n \
             predicate Middle : string }",
        );

        assert_eq!(names(&schema), ["src.Apple", "src.Middle", "src.Zebra"]);
    }

    /// **Two orderings of one schema are the same schema** — the precursor to
    /// [I13](../../../../docs/invariants.md#i13)'s fingerprint guard, at the level this
    /// step can already answer.
    ///
    /// Both the ids and the types have to match: ids alone would hold for a lowering
    /// that dropped every type.
    #[test]
    fn two_orderings_lower_to_the_same_schema() {
        let one = schema_of(
            "schema src { predicate File : string\n \
             predicate Module : { file : File, name : string } }",
        );
        let other = schema_of(
            "schema src { predicate Module : { file : File, name : string }\n \
             predicate File : string }",
        );

        assert_eq!(names(&one), names(&other));

        // Compared by rendering rather than by `==`: `PredicateTy` carries no
        // `PartialEq`, and adding one to a core type so a test can be terser is the
        // wrong way round.
        for index in 0..one.len() {
            let id = PredicateId(index as u32);
            assert_eq!(
                format!("{:?}", one.get(id).map(|p| p.predicate().key.clone())),
                format!("{:?}", other.get(id).map(|p| p.predicate().key.clone())),
                "predicate {index} differs between orderings"
            );
        }
    }

    /// **A record's field order is declaration order, and is never sorted.**
    ///
    /// The load-bearing one. A record's order is its encoding order and decides the seek
    /// prefix, so sorting here would silently re-index every database built from a
    /// schema — which is the failure `bench/FINDINGS.md` §2 measured at 56,274 rows
    /// examined per row produced. Written `z` before `a` on purpose: sorted output would
    /// be the alphabetical one, and only an out-of-order declaration can tell them apart.
    #[test]
    fn a_records_fields_keep_the_order_they_were_declared_in() {
        let schema = schema_of("schema src { predicate P : { zebra : int, apple : string } }");
        let predicate = schema.get(PredicateId(0)).expect("the one predicate");

        let PredicateTy::Record(fields) = &predicate.predicate().key else {
            panic!("expected a record");
        };

        let order: Vec<&str> = fields
            .iter()
            .map(|(name, _)| schema.interner().resolve(*name).expect("a field name"))
            .collect();

        assert_eq!(order, ["zebra", "apple"], "declaration order, not sorted");
    }

    /// A reference lowers to the id of the predicate it names, across namespaces too.
    #[test]
    fn a_reference_names_the_predicate_it_points_at() {
        let schema = schema_of(
            "schema src { predicate File : string }\n\
             schema a { predicate P : { f : src.File } }",
        );

        let file = schema.find_position("src.File").expect("src.File").0;
        let p = schema.find_position("a.P").expect("a.P").1;

        let PredicateTy::Record(fields) = &p.predicate().key else {
            panic!("expected a record");
        };

        assert!(matches!(fields[0].1, PredicateTy::Fact(id) if id == file));
    }

    /// A named type is **sugar**: it is expanded where it is used and has no identity of
    /// its own, so it is not a predicate and takes no id.
    #[test]
    fn a_named_type_is_expanded_rather_than_declared() {
        let schema = schema_of(
            "schema src { type Position = { line : int, col : int }\n \
             predicate At : { at : Position } }",
        );

        assert_eq!(names(&schema), ["src.At"], "the alias is not a predicate");

        let PredicateTy::Record(fields) =
            &schema.get(PredicateId(0)).expect("src.At").predicate().key
        else {
            panic!("expected a record");
        };

        assert!(
            matches!(&fields[0].1, PredicateTy::Record(inner) if inner.len() == 2),
            "the alias expanded in place"
        );
    }

    /// A value side is carried, and its absence is carried too.
    #[test]
    fn a_value_side_is_optional_and_kept() {
        let schema = schema_of(
            "schema src { predicate WithValue : string -> string\n \
             predicate WithNone : string }",
        );

        let with = schema.find_position("src.WithValue").expect("declared").1;
        let without = schema.find_position("src.WithNone").expect("declared").1;

        assert!(matches!(with.predicate().value, Some(PredicateTy::Str)));
        assert!(without.predicate().value.is_none());
    }

    /// Imports are collected and not resolved — 8.4's job, recorded so that step is
    /// about finding files rather than parsing again.
    #[test]
    fn imports_are_collected_in_the_order_written() {
        let mut diags = vec![];
        let cst = parse(
            "schema a { import lang.rust\n import src\n predicate P : string }",
            &mut diags,
        )
        .expect("parses");
        let lowered = lower(&cst, &mut diags).expect("lowers");

        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(lowered.imports, ["lang.rust", "src"]);
    }
}
