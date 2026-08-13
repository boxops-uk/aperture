//! [`Ast`] → text, in two renderings that must not be confused.
//!
//! - [`print`] emits **focus source**: text that parses and lowers back to the tree
//!   it came from. That makes lowering invertible, which is what lets the front end
//!   be property-tested — generate a tree, print it, parse it, compare — rather than
//!   only checked against hand-written snippets.
//! - [`canonical`] emits an **s-expression**, which is deliberately *not* focus
//!   syntax. It is the structural identity of a tree: two trees built by different
//!   routes have different `NodeId`s and different spans but the same canonical
//!   form, so this is what a round-trip compares. Keeping it a separate rendering is
//!   what stops the round-trip property being circular.
//!
//! Printing is **not** the inverse of parsing in the other direction: whitespace,
//! redundant parens and the choice of string escapes are all lost. Only
//! `parse ∘ print == id` on trees is claimed, and only that is tested.
//!
//! The hard part is parentheses. The grammar has three precedence levels, and a
//! child looser than its position allows has to be wrapped — see [`Prec`].

use std::fmt::Write as _;

use crate::focus::{
    iter::Address,
    plan::{FieldPath, Plan, Project, Residual, ResidualOp, SeekKey, SeekKeyPart, Source, Step},
    schema::{LocalInterner, PredicateRef, PredicateTy, Schema, Symbol},
    syntax::{Ast, ExprKind, FieldRef, Literal, NodeId, NodeSpan, Query, QueryStmt, narrow_offset},
};

/// How loosely a pattern binds, from the grammar:
///
/// ```text
/// pattern := branch ('|' branch)*                        -- Disjunction
/// branch  := fact_pattern | primary ('.' LId ['?'])*     -- Application | Chain
/// primary := '_' | UId | Nat | … | '(' pattern … ')'     -- Primary
/// ```
///
/// A child is parenthesised exactly when its level is *greater* than the level its
/// position permits. `Application` and `Chain` are siblings in the grammar but must
/// be ordered here, because an access chain's base may be a chain (`X.a.b`) while an
/// application in that position needs wrapping (`(test.Foo X).name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Prec {
    Primary,
    Chain,
    Application,
    Disjunction,
}

/// Render `ast` as focus source.
pub fn print(ast: &Ast, schema: &Schema, interner: &LocalInterner) -> String {
    spanned(ast, schema, interner).text
}

/// Render a compiled [`Plan`] for a person to read: one line per loop level, then
/// the head.
///
/// A **third** rendering, and the only one that is not about a tree. It exists
/// because the plan is where a query's cost lives — which field narrowed the scan,
/// which one only filters, which register a level reads — and none of that is visible
/// in the source it came from. Fields are named from the schema rather than shown as
/// indices, since `of = r0#` is the answer to "did it follow the reference?" and
/// `1 = r0#` is not.
///
/// Constants render as `<const>`: what matters here is *where* a constant went, and
/// decoding one back to a literal would need the field's type threaded through every
/// arm to say something the query already says.
#[must_use]
pub fn plan(plan: &Plan, schema: &Schema, interner: &LocalInterner) -> String {
    let mut out = String::new();

    // Numbered over *scan* steps: a level is a loop, and a derived bind is not one.
    let mut level = 0;

    for step in plan.body.iter() {
        let generator = match step {
            Step::Level(generator) => generator,
            // A derived bind names the register it computes, since it has no
            // predicate to be about and no scan to narrow.
            Step::Derive(derived) => {
                let _ = writeln!(out, "  {} = <computed>", derived.bind);
                continue;
            }
        };

        // A path read out of some *other* register is named against the key of
        // whatever predicate that register holds, which is a different predicate
        // with different field names. Naming it against this level's key gave
        // `r0.module` for a register holding a `src.Module`, whose key has no
        // `module` field at all.
        let register_field = |address: &Address, path: &FieldPath| {
            let predicate = register_key(plan, address, schema);
            let key_ty = predicate.as_ref().map(|p| p.key().ty);

            format!("{address}.{}", field_name(key_ty, path, schema))
        };

        let _ = write!(out, "  {} <-", Address::new(level));

        // A level with no sources produces nothing. Rendered as the keyword for
        // it rather than as a blank, because "this level answers nothing" is the
        // most important thing a plan can say about itself.
        if generator.sources.is_empty() {
            out.push_str(" never");
        }

        for (alternative, Source::Seek { access, residuals }) in
            generator.sources.iter().enumerate()
        {
            // Alternatives after the first are stacked under the level, so a
            // single-source level — every level focus compiles today — reads
            // exactly as it did before there was more than one.
            if alternative > 0 {
                let _ = write!(out, "\n     |");
            }

            let predicate = schema.get(access.predicate_id);
            let name = predicate.as_ref().and_then(|p| p.name()).unwrap_or("?");
            let key_ty = predicate.as_ref().map(|p| p.key().ty);
            let field = |path: &FieldPath| field_name(key_ty, path, schema);

            let _ = write!(out, " {name}");

            match &access.seek_key {
                SeekKey::Prefix(bytes) if bytes.is_empty() => out.push_str(" scan"),
                SeekKey::Prefix(_) => out.push_str(" seek[<const>]"),
                SeekKey::Composite(parts) => {
                    let parts: Vec<String> = parts
                        .iter()
                        .map(|part| match part {
                            SeekKeyPart::Bytes(_) => "<const>".to_owned(),
                            SeekKeyPart::RegisterField { address, path } => {
                                register_field(address, path)
                            }
                            SeekKeyPart::RegisterFactId(address) => format!("{address}#"),
                        })
                        .collect();

                    let _ = write!(out, " seek[{}]", parts.join(" "));
                }
            }

            for Residual { path, op } in residuals.iter() {
                let at = field(path);

                let _ = match op {
                    ResidualOp::EqConst(_) => write!(out, "\n       where {at} == <const>"),
                    ResidualOp::Prefix(_) => {
                        write!(out, "\n       where {at} starts with <const>")
                    }
                    ResidualOp::EqRegisterField { address, path } => {
                        write!(
                            out,
                            "\n       where {at} == {}",
                            register_field(address, path)
                        )
                    }
                    ResidualOp::EqRegisterFactId(address) => {
                        write!(out, "\n       where {at} == {address}#")
                    }
                };
            }
        }

        out.push('\n');
        level += 1;
    }

    let _ = write!(
        out,
        "  head {}",
        projection(plan, &plan.head, schema, interner)
    );
    out
}

/// One projection, as the row it produces reads.
///
/// Takes the whole plan because a projection names a field of a *register*, and which
/// predicate that register holds is recorded by the level that binds it — so the field
/// has a name here as much as in a seek, just one indirection further away.
fn projection(plan: &Plan, project: &Project, schema: &Schema, interner: &LocalInterner) -> String {
    let key_of = |address: &Address| register_key(plan, address, schema);

    match project {
        Project::Lit(value) => format!("{value:?}"),
        // A row's identity, which is what a reference to it holds.
        Project::FactRef(address) => format!("{address}#"),
        Project::RegisterField { address, path, .. } => {
            let predicate = key_of(address);
            let key_ty = predicate.as_ref().map(|p| p.key().ty);

            format!("{address}.{}", field_name(key_ty, path, schema))
        }
        Project::Value { address, .. } => format!("{address}.value"),
        // A computed value, which no predicate's field names.
        Project::Computed(address) => format!("{address}="),
        Project::Record(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, field)| format!(
                    "{} = {}",
                    interner.try_resolve(*name).unwrap_or("?"),
                    projection(plan, field, schema, interner)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The predicate whose key a register holds.
///
/// Found by asking which level *binds* this register, rather than by indexing the
/// body with the register number. Those were the same number while every register
/// came from a level of its own; a derived bind writes a register with no level
/// behind it, so indexing would name an unrelated predicate's fields.
fn register_key<'a>(
    plan: &Plan,
    address: &Address,
    schema: &'a Schema,
) -> Option<PredicateRef<'a>> {
    plan.body
        .iter()
        .filter_map(|step| match step {
            Step::Level(generator) => Some(generator),
            Step::Derive(_) => None,
        })
        .find(|level| level.binds.contains(address))
        // `None` for a disjunction spanning predicates: there is no single key to
        // name a field against, and falling back to the index says so.
        .and_then(|level| level.predicate_id())
        .and_then(|predicate| schema.get(predicate))
}

/// A field path as the schema names it — `of`, or `outer.inner` for a nested step.
///
/// Falls back to the indices when the type is not to hand — a malformed plan naming a
/// register no level binds, or a field past the key's arity. Naming what can be named
/// is worth more than naming nothing.
fn field_name(key_ty: Option<&PredicateTy>, path: &FieldPath, schema: &Schema) -> String {
    let Some(mut ty) = key_ty else {
        return path.to_string();
    };

    let mut names = vec![];

    for index in std::iter::once(path.field_idx()).chain(path.steps().iter().copied()) {
        let PredicateTy::Record(fields) = ty else {
            // A scalar key is one field and has no name of its own.
            return if names.is_empty() {
                path.to_string()
            } else {
                names.join(".")
            };
        };

        let Some((name, field_ty)) = fields.get(index) else {
            return path.to_string();
        };

        names.push(schema.interner().resolve(*name).unwrap_or("?").to_owned());
        ty = field_ty;
    }

    names.join(".")
}

/// Render `ast` as focus source, keeping the range each node's text occupies.
///
/// Printing is where a span can be *predicted*: the printer knows what it emitted
/// and where, so lowering the result must hand back exactly these ranges. That is
/// what makes spans property-testable at all — a generated tree has no source to
/// compare against, and re-deriving one by slicing and re-parsing would only ever
/// check that a span looks plausible.
pub fn spanned(ast: &Ast, schema: &Schema, interner: &LocalInterner) -> Spanned {
    let mut out = Spanned {
        text: String::new(),
        spans: vec![0..0; ast.store().len()],
    };
    Printer {
        ast,
        schema: Some(schema),
        interner,
    }
    .query(&mut out, ast.query());
    out
}

/// Focus source under construction, with the span each node was printed at.
pub struct Spanned {
    text: String,
    /// By `NodeId`, which indexes the store densely.
    spans: Vec<NodeSpan>,
}

impl Spanned {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Where `id`'s own text landed.
    ///
    /// **Parentheses the printer wrapped around `id` are excluded**, because that is
    /// lowering's convention: a `paren_primary` is a pass-through to its child
    /// (`lower.rs`), so the child keeps the span it was pushed with. A subquery's
    /// parens *are* included, since there the parens belong to the node's own rule.
    /// The two conventions must agree, or `spans_are_where_the_text_was_printed`
    /// would be pinning the printer's rather than lowering's.
    pub fn span(&self, id: NodeId) -> NodeSpan {
        self.spans[id.index()].clone()
    }

    fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// Record `id` as covering exactly what `f` emits.
    fn node(&mut self, id: NodeId, f: impl FnOnce(&mut Self)) {
        let start = narrow_offset(self.text.len());
        f(self);
        self.spans[id.index()] = start..narrow_offset(self.text.len());
    }

    /// Emit `items` separated by `sep`.
    fn join<T>(
        &mut self,
        sep: &str,
        items: impl IntoIterator<Item = T>,
        mut f: impl FnMut(&mut Self, T),
    ) {
        for (index, item) in items.into_iter().enumerate() {
            if index > 0 {
                self.push(sep);
            }
            f(self, item);
        }
    }
}

/// Render `ast` as an s-expression: its structure, with no `NodeId`s or spans.
///
/// Not focus syntax, and not parseable. This is what two trees are compared by.
pub fn canonical(ast: &Ast, interner: &LocalInterner) -> String {
    let printer = Printer {
        ast,
        // Predicates are named by id here, so no schema is needed — which is also
        // why a canonical form survives being compared across two schemas.
        schema: None,
        interner,
    };

    let mut out = String::new();
    printer.canonical_query(&mut out, ast.query());
    out
}

struct Printer<'a> {
    ast: &'a Ast,
    schema: Option<&'a Schema>,
    interner: &'a LocalInterner,
}

impl Printer<'_> {
    // ---- focus source ---------------------------------------------------------

    fn query(&self, out: &mut Spanned, query: &Query<NodeId>) {
        self.pattern(out, *query.head(), Prec::Disjunction);
        out.push(" where ");
        out.join("; ", query.body(), |out, stmt| self.stmt(out, stmt));
    }

    fn stmt(&self, out: &mut Spanned, stmt: &QueryStmt<NodeId>) {
        match stmt {
            QueryStmt::Implicit(id) => self.pattern(out, *id, Prec::Disjunction),
            QueryStmt::Bind(lhs, rhs) => {
                self.pattern(out, *lhs, Prec::Disjunction);
                out.push(" = ");
                self.pattern(out, *rhs, Prec::Disjunction);
            }
            QueryStmt::Negation(id) => {
                out.push("!");
                self.pattern(out, *id, Prec::Disjunction);
            }
        }
    }

    /// Print the node at `id`, wrapping it if it binds more loosely than `permitted`.
    ///
    /// The wrapping parens are emitted *outside* the recorded span — see
    /// [`Spanned::span`] for why that is lowering's convention and not a choice.
    fn pattern(&self, out: &mut Spanned, id: NodeId, permitted: Prec) {
        let wrapped = self.level(id) > permitted;
        if wrapped {
            out.push("(");
        }
        out.node(id, |out| self.bare(out, id));
        if wrapped {
            out.push(")");
        }
    }

    fn level(&self, id: NodeId) -> Prec {
        match self.ast.store().kind(id) {
            ExprKind::Disjunction(_) => Prec::Disjunction,
            ExprKind::Fact(..) => Prec::Application,
            ExprKind::Access(..) | ExprKind::Select(..) => Prec::Chain,
            _ => Prec::Primary,
        }
    }

    fn bare(&self, out: &mut Spanned, id: NodeId) {
        match self.ast.store().kind(id) {
            ExprKind::Wildcard => out.push("_"),
            ExprKind::Never => out.push("never"),
            ExprKind::Var(symbol) => out.push(self.name(*symbol)),

            ExprKind::Lit(Literal::Int(value)) => {
                // `i64::MIN`'s magnitude does not fit an `i64`, and the grammar's
                // negative literal is `'-' Nat`, so the sign is printed separately
                // from an unsigned magnitude.
                if *value < 0 {
                    out.push(&format!("-{}", value.unsigned_abs()));
                } else {
                    out.push(&value.to_string());
                }
            }
            ExprKind::Lit(Literal::Str(symbol)) => out.push(&escape(self.name(*symbol))),
            ExprKind::Prefix(symbol) => {
                out.push(&escape(self.name(*symbol)));
                out.push("..");
            }

            ExprKind::Record(fields) => {
                out.push("{");
                out.join(", ", fields.iter(), |out, (name, value)| {
                    out.push(self.name(*name));
                    out.push(" = ");
                    self.pattern(out, *value, Prec::Disjunction);
                });
                out.push("}");
            }

            // An access chain's base is a primary or another chain; anything looser
            // is wrapped.
            ExprKind::Access(FieldRef::Key(name), base) => {
                self.pattern(out, *base, Prec::Chain);
                out.push(".");
                out.push(self.name(*name));
            }
            ExprKind::Access(FieldRef::Value, base) => {
                self.pattern(out, *base, Prec::Chain);
                out.push(".value");
            }
            ExprKind::Select(alt, base) => {
                self.pattern(out, *base, Prec::Chain);
                out.push(".");
                out.push(self.name(*alt));
                out.push("?");
            }

            ExprKind::Fact(predicate, key) => {
                // Unreachable from a lowered tree — lowering only builds a `Fact`
                // for a predicate it resolved, under a schema that could name it —
                // but printing must not panic on a hand-built one.
                let name = self
                    .schema
                    .and_then(|s| s.get(*predicate))
                    .and_then(|p| p.name())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("unknown.Predicate{}", predicate.0));
                out.push(&name);
                out.push(" ");
                self.pattern(out, *key, Prec::Application);
            }

            ExprKind::Disjunction(branches) => {
                out.join(" | ", branches.iter(), |out, branch| {
                    self.pattern(out, *branch, Prec::Application)
                });
            }

            // Unlike a precedence paren, these belong to the subquery's own rule, so
            // they are emitted inside the node's span — which is where lowering puts
            // them too.
            ExprKind::Subquery(query) => {
                out.push("(");
                self.query(out, query);
                out.push(")");
            }

            // Deliberately not valid focus: a tree with an error node has no source,
            // and emitting something plausible would hide that.
            ExprKind::Error => out.push("!error"),
        }
    }

    // ---- canonical form -------------------------------------------------------

    fn canonical_query(&self, out: &mut String, query: &Query<NodeId>) {
        out.push_str("(query ");
        self.canonical_body(out, query);
        out.push(')');
    }

    /// `head stmt stmt …` — the inside a query and a subquery share.
    fn canonical_body(&self, out: &mut String, query: &Query<NodeId>) {
        self.canonical_pattern(out, *query.head());
        out.push(' ');

        for (index, stmt) in query.body().iter().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            match stmt {
                QueryStmt::Implicit(id) => {
                    out.push_str("(implicit ");
                    self.canonical_pattern(out, *id);
                    out.push(')');
                }
                QueryStmt::Bind(lhs, rhs) => {
                    out.push_str("(bind ");
                    self.canonical_pattern(out, *lhs);
                    out.push(' ');
                    self.canonical_pattern(out, *rhs);
                    out.push(')');
                }
                QueryStmt::Negation(id) => {
                    out.push_str("(not ");
                    self.canonical_pattern(out, *id);
                    out.push(')');
                }
            }
        }
    }

    /// Written into one buffer rather than folded up as a `String` per node: the
    /// fold concatenated whole subtrees at every level, so a tree of n nodes cost
    /// O(n²) copying to render.
    fn canonical_pattern(&self, out: &mut String, id: NodeId) {
        /// A `String` is an infallible sink; `write!` returns `Result` regardless.
        const SINK: &str = "writing to a String cannot fail";

        match self.ast.store().kind(id) {
            ExprKind::Wildcard => out.push_str("(wild)"),
            ExprKind::Never => out.push_str("(never)"),
            ExprKind::Error => out.push_str("(error)"),

            ExprKind::Var(symbol) => {
                out.push_str("(var ");
                out.push_str(self.name(*symbol));
                out.push(')');
            }

            ExprKind::Lit(Literal::Int(value)) => write!(out, "(int {value})").expect(SINK),
            ExprKind::Lit(Literal::Str(symbol)) => {
                write!(out, "(str {:?})", self.name(*symbol)).expect(SINK);
            }
            ExprKind::Prefix(symbol) => {
                write!(out, "(prefix {:?})", self.name(*symbol)).expect(SINK);
            }

            ExprKind::Record(fields) => {
                out.push_str("(record");
                for (name, value) in fields.iter() {
                    out.push_str(" (");
                    out.push_str(self.name(*name));
                    out.push(' ');
                    self.canonical_pattern(out, *value);
                    out.push(')');
                }
                out.push(')');
            }

            ExprKind::Access(FieldRef::Key(name), base) => {
                out.push_str("(field ");
                out.push_str(self.name(*name));
                out.push(' ');
                self.canonical_pattern(out, *base);
                out.push(')');
            }
            ExprKind::Access(FieldRef::Value, base) => {
                out.push_str("(value ");
                self.canonical_pattern(out, *base);
                out.push(')');
            }
            ExprKind::Select(alt, base) => {
                out.push_str("(select ");
                out.push_str(self.name(*alt));
                out.push(' ');
                self.canonical_pattern(out, *base);
                out.push(')');
            }

            ExprKind::Fact(predicate, key) => {
                write!(out, "(fact {} ", predicate.0).expect(SINK);
                self.canonical_pattern(out, *key);
                out.push(')');
            }

            ExprKind::Disjunction(branches) => {
                out.push_str("(or ");
                for (index, branch) in branches.iter().enumerate() {
                    if index > 0 {
                        out.push(' ');
                    }
                    self.canonical_pattern(out, *branch);
                }
                out.push(')');
            }

            ExprKind::Subquery(query) => {
                out.push_str("(subquery ");
                self.canonical_body(out, query);
                out.push(')');
            }
        }
    }

    fn name(&self, symbol: Symbol) -> &str {
        self.interner.try_resolve(symbol).unwrap_or("?")
    }
}

/// Quote and escape a string so the lexer accepts it and `unescape_str` inverts it.
///
/// The lexer's `String` regex admits `\" \\ \/ \b \f \n \r \t \uXXXX` and any other
/// character that is neither a quote, a backslash, nor a control character — so
/// control characters *must* be escaped, and everything else may be literal.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            // Every other control character, DEL included — the regex's `[:cntrl:]`
            // covers 0x00–0x1F and 0x7F — has no short escape.
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{
        compile::Compilation,
        corpus,
        cst::CstNode,
        diag::Diagnostics,
        lower::lower,
        parse::parse,
        plan::{Access, Level as PlanLevel},
        schema::PredicateId,
        syntax::{proptest::arb_query_spec, source_range},
    };
    use ::proptest::prelude::*;

    /// **A register's field is named against that register's predicate.**
    ///
    /// The rendering exists to answer "which field narrowed this scan", so a wrong
    /// name is worse than an index: both `from` and `id` are real field names here,
    /// and naming `r0`'s path against *this* level's key silently swapped one for
    /// the other. Nothing caught it because this renderer had no test and only the
    /// shell called it.
    #[test]
    fn a_spliced_register_is_named_against_its_own_predicate() {
        let schema = corpus::schema();

        // `r0` holds a `test.Edge` (key `{from, to}`); the level seeking on it is a
        // `test.Foo` (key `{id, name}`). Path 0 is `from` there and `id` here.
        let mut compilation = Compilation::new(
            "X where test.Edge {from = X, to = _}; test.Foo {id = X, name = _}",
            &schema,
        );
        let compiled = compilation.plan().expect("a plan");
        let rendered = plan(&compiled, &schema, compilation.interner());

        assert!(
            rendered.contains("seek[r0.from]"),
            "expected the register's own field name, got:\n{rendered}"
        );

        // And a scalar-keyed level reading a record-keyed register: the field has a
        // name on one side and none on the other.
        let mut compilation =
            Compilation::new("X where test.Foo {name = X, id = _}; test.Name X", &schema);
        let compiled = compilation.plan().expect("a plan");
        let rendered = plan(&compiled, &schema, compilation.interner());

        assert!(
            rendered.contains("seek[r0.name]"),
            "expected the register's own field name, got:\n{rendered}"
        );
    }

    /// The id of the predicate `name`, found by asking the schema rather than by
    /// hardcoding a number the fixture is free to renumber.
    fn predicate_id(schema: &Schema, name: &str) -> PredicateId {
        (0..64)
            .map(PredicateId)
            .find(|id| schema.get(*id).and_then(|p| p.name()) == Some(name))
            .unwrap_or_else(|| panic!("no predicate called {name}"))
    }

    /// The **residual** arm names the other register against its own predicate
    /// too — the same fault as the seek, one arm along, and the arm a fix to the
    /// seek alone would have left behind.
    #[test]
    fn a_residual_against_a_register_is_named_against_its_own_predicate() {
        let schema = corpus::schema();

        // `r0` holds a `test.Foo` (key `{id, name}`); the level filtering against
        // it is a `test.Edge` (key `{from, to}`). `from` is a wildcard, so the
        // seek prefix closes and `to = X` becomes a residual reading `r0`'s field
        // 0 — which is `id` there and `from` here.
        let mut compilation = Compilation::new(
            "X where test.Foo {id = X, name = _}; test.Edge {from = _, to = X}",
            &schema,
        );
        let compiled = compilation.plan().expect("a plan");
        let rendered = plan(&compiled, &schema, compilation.interner());

        assert!(
            rendered.contains("where to == r0.id"),
            "expected `to == r0.id` — this level's field against the register's \
             own — got:\n{rendered}"
        );
    }

    /// The **head** names a register against the predicate of the level that
    /// binds it, not against the last level or the level whose number matches.
    #[test]
    fn a_projected_register_is_named_against_its_own_predicate() {
        let schema = corpus::schema();

        // `r0` is a `test.Foo` (`{id, name}`) and `r1` a `test.Link` (`{at, of}`).
        // Projecting `r0`'s field 1 is `name`; against `test.Link` it would read
        // `of`, which is a real field name and so fails silently.
        let mut compilation = Compilation::new(
            "Y where test.Foo {id = _, name = Y}; test.Link {at = 1, of = _}",
            &schema,
        );
        let compiled = compilation.plan().expect("a plan");
        let rendered = plan(&compiled, &schema, compilation.interner());

        assert!(
            rendered.contains("head r0.name"),
            "expected the head to name `r0`'s own field, got:\n{rendered}"
        );
    }

    /// A register bound by a **disjunction spanning predicates** has no single key
    /// to be named against, and the renderer says so by falling back to the index
    /// rather than picking one of the alternatives.
    ///
    /// Reachable only from a hand-built plan today, since flatten emits
    /// single-source levels — the same standing as the derive steps `projection`
    /// already has to handle.
    #[test]
    fn a_register_bound_by_a_disjunction_across_predicates_falls_back_to_the_index() {
        let schema = corpus::schema();
        let interner = LocalInterner::new(schema.interner().clone());

        let foo = predicate_id(&schema, "test.Foo");
        let edge = predicate_id(&schema, "test.Edge");

        let source = |predicate| Source::Seek {
            access: Access {
                predicate_id: predicate,
                seek_key: SeekKey::Prefix(Box::new([])),
            },
            residuals: Box::new([]),
        };

        let compiled = Plan {
            nvars: 1,
            body: Box::new([Step::Level(PlanLevel {
                sources: Box::new([source(foo), source(edge)]),
                binds: Box::new([Address::new(0)]),
            })]),
            head: Project::RegisterField {
                address: Address::new(0),
                path: FieldPath::field(0),
                ty: PredicateTy::Int,
            },
        };

        let rendered = plan(&compiled, &schema, &interner);

        assert!(
            rendered.contains("head r0.0"),
            "expected the index, since `id` and `from` are both wrong for half the \
             rows, got:\n{rendered}"
        );
        // Both alternatives are still named, each against its own key.
        assert!(
            rendered.contains("test.Foo scan") && rendered.contains("test.Edge scan"),
            "expected both alternatives, got:\n{rendered}"
        );
    }

    /// Parse and lower `source`, requiring both to be clean.
    fn tree(source: &str) -> (Ast, LocalInterner, Schema) {
        let schema = corpus::schema();
        let mut interner = LocalInterner::new(schema.interner().clone());
        let mut diagnostics = Diagnostics::new();
        let cst = parse(source, &mut diagnostics).expect("a tree");
        assert!(!diagnostics.has_errors(), "{source:?} must parse");

        let ast = lower(
            &CstNode::new(&cst),
            &schema,
            &mut interner,
            &mut diagnostics,
        );
        assert!(diagnostics.is_empty(), "{source:?} must lower cleanly");
        (ast, interner, schema)
    }

    fn printed(source: &str) -> String {
        let (ast, interner, schema) = tree(source);
        print(&ast, &schema, &interner)
    }

    /// Printing puts parens exactly where the grammar needs them — no more, no less.
    #[test]
    fn parentheses_go_where_precedence_requires() {
        // Dot is tighter than application, so the access needs none.
        assert_eq!(
            printed("Y where test.Name Y.name"),
            "Y where test.Name Y.name"
        );
        // ...and a redundant pair is dropped.
        assert_eq!(
            printed("Y where test.Name (Y.name)"),
            "Y where test.Name Y.name"
        );

        // An application *under* an access does need them.
        assert_eq!(
            printed("(test.Bar {id = 1}).value where test.Foo _"),
            "(test.Bar {id = 1}).value where test.Foo _"
        );

        // `|` is looser than application: as a fact's key it is wrapped, as a
        // statement it is not.
        assert_eq!(
            printed("X where test.Foo (A | B)"),
            "X where test.Foo (A | B)"
        );
        assert_eq!(printed("X where A | B"), "X where A | B");

        // A disjunction branch that is itself a disjunction keeps its parens, or it
        // would re-parse as one flat three-branch node.
        assert_eq!(printed("X where (A | B) | C"), "X where (A | B) | C");
    }

    #[test]
    fn literals_and_names_survive_printing() {
        assert_eq!(
            printed("X where X = test.Count -42"),
            "X where X = test.Count -42"
        );
        assert_eq!(
            printed("X where X = test.Count -9223372036854775808"),
            "X where X = test.Count -9223372036854775808"
        );
        // Separators are not part of the value.
        assert_eq!(
            printed("X where X = test.Count 1_000"),
            "X where X = test.Count 1000"
        );
        assert_eq!(
            printed(r#"X where X = test.Name "a\nb""#),
            r#"X where X = test.Name "a\nb""#
        );
        assert_eq!(
            printed(r#"X where X = test.Name "abc".."#),
            r#"X where X = test.Name "abc".."#
        );
    }

    #[test]
    fn every_construct_prints() {
        assert_eq!(printed("X where X = never"), "X where X = never");
        assert_eq!(
            printed("X.alt? where test.Foo _"),
            "X.alt? where test.Foo _"
        );
        assert_eq!(
            printed("X.value where test.Foo _"),
            "X.value where test.Foo _"
        );
        assert_eq!(printed("_ where test.Foo {}"), "_ where test.Foo {}");
        assert_eq!(
            printed("X where !test.Bar {id = 1}"),
            "X where !test.Bar {id = 1}"
        );
        assert_eq!(
            printed("X where X = (Y where test.Foo {id = Y})"),
            "X where X = (Y where test.Foo {id = Y})"
        );
    }

    /// The property the printer exists for, over the hand-written corpus:
    /// **parse ∘ print is the identity on trees.** Printing then re-lowering must
    /// give a structurally identical tree.
    ///
    /// Entries whose lowering reports something are skipped — an error node has no
    /// source text, by design.
    #[test]
    fn printing_and_reparsing_the_corpus_is_the_identity() {
        let mut checked = 0;

        for entry in corpus::CORPUS {
            let schema = corpus::schema();
            let mut interner = LocalInterner::new(schema.interner().clone());

            let mut diagnostics = Diagnostics::new();
            let Some(cst) = parse(entry.source, &mut diagnostics) else {
                continue;
            };
            if diagnostics.has_errors() {
                continue;
            }
            let ast = lower(
                &CstNode::new(&cst),
                &schema,
                &mut interner,
                &mut diagnostics,
            );
            if !diagnostics.is_empty() {
                continue;
            }

            let text = print(&ast, &schema, &interner);

            // Re-parse with a *fresh* interner, so the comparison cannot accidentally
            // depend on interning order.
            let mut reinterner = LocalInterner::new(schema.interner().clone());
            let mut rediagnostics = Diagnostics::new();
            let recst = parse(&text, &mut rediagnostics);
            assert!(
                !rediagnostics.has_errors(),
                "printing {:?} gave {text:?}, which does not parse: {:?}",
                entry.source,
                rediagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            );

            let reast = lower(
                &CstNode::new(&recst.expect("a tree")),
                &schema,
                &mut reinterner,
                &mut rediagnostics,
            );
            assert!(
                rediagnostics.is_empty(),
                "printing {:?} gave {text:?}, which does not lower cleanly",
                entry.source
            );

            assert_eq!(
                canonical(&ast, &interner),
                canonical(&reast, &reinterner),
                "{:?} printed to {text:?}, which lowered to a different tree",
                entry.source
            );
            checked += 1;
        }

        assert!(checked > 20, "only {checked} entries were round-tripped");
    }

    /// Printing is idempotent: the second printing is byte-identical, which is what
    /// makes the output a normal form rather than merely valid.
    #[test]
    fn printing_is_idempotent() {
        for entry in corpus::CORPUS {
            let schema = corpus::schema();
            let mut interner = LocalInterner::new(schema.interner().clone());
            let mut diagnostics = Diagnostics::new();
            let Some(cst) = parse(entry.source, &mut diagnostics) else {
                continue;
            };
            if diagnostics.has_errors() {
                continue;
            }
            let ast = lower(
                &CstNode::new(&cst),
                &schema,
                &mut interner,
                &mut diagnostics,
            );
            if !diagnostics.is_empty() {
                continue;
            }

            let once = print(&ast, &schema, &interner);
            let (reast, reinterner, _) = tree(&once);
            let twice = print(&reast, &schema, &reinterner);
            assert_eq!(once, twice, "for {:?}", entry.source);
        }
    }

    proptest! {
        /// **`parse ∘ print == id` on trees.** Generate a tree, print it, parse and
        /// lower the text, and the tree must come back structurally identical.
        ///
        /// Only that direction is claimed. `print ∘ parse` is not the identity on
        /// *text* — whitespace, redundant parens and the choice of escapes are all
        /// normalised away — which is why the comparison is between canonical forms
        /// of trees rather than between strings.
        ///
        /// This is what turns the hand-written corpus from the whole specification of
        /// the surface into a set of worked examples: the corpus says which syntax is
        /// acceptable, and this says the front end is faithful across all of it.
        #[test]
        fn lowering_a_printed_tree_gives_the_same_tree(spec in arb_query_spec()) {
            let schema = corpus::schema();
            let (ast, interner) = spec.build(&schema);
            let text = print(&ast, &schema, &interner);

            let mut diagnostics = Diagnostics::new();
            let cst = parse(&text, &mut diagnostics);
            prop_assert!(
                !diagnostics.has_errors(),
                "printed {text:?}, which does not parse: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
            let cst = cst.expect("a tree");

            // A fresh interner: the comparison must not depend on interning order.
            let mut reinterner = LocalInterner::new(schema.interner().clone());
            let reast = lower(
                &CstNode::new(&cst),
                &schema,
                &mut reinterner,
                &mut diagnostics,
            );
            prop_assert!(
                diagnostics.is_empty(),
                "printed {text:?}, which does not lower cleanly: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            );

            prop_assert_eq!(
                canonical(&ast, &interner),
                canonical(&reast, &reinterner),
                "printed {:?}", text
            );
        }

        /// **A node's span is where its text was printed.** The printer records the
        /// range it emitted each node at; parsing and lowering that text must give
        /// back exactly those ranges.
        ///
        /// This is the half of the front end the tree round-trip is blind to. Spans
        /// carry no structure, so every one of them could be off by a byte, name a
        /// sibling, or swallow a precedence paren while the tree comparison stayed
        /// green — and spans are what every diagnostic points with.
        ///
        /// It is testable only because printing *predicts* the spans. A generated
        /// tree has no source of its own (`QuerySpec::build` pushes `0..0`), and
        /// re-deriving one by slicing a span and re-parsing it would only ever check
        /// that the span looks plausible, not that it is right.
        #[test]
        fn spans_are_where_the_text_was_printed(spec in arb_query_spec()) {
            let schema = corpus::schema();
            let (ast, interner) = spec.build(&schema);
            let printed = spanned(&ast, &schema, &interner);

            let mut diagnostics = Diagnostics::new();
            let cst = parse(printed.text(), &mut diagnostics);
            prop_assert!(
                !diagnostics.has_errors(),
                "printed {:?}, which does not parse: {:?}",
                printed.text(),
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
            let cst = cst.expect("a tree");

            let mut reinterner = LocalInterner::new(schema.interner().clone());
            let reast = lower(
                &CstNode::new(&cst),
                &schema,
                &mut reinterner,
                &mut diagnostics,
            );
            prop_assert!(
                diagnostics.is_empty(),
                "printed {:?}, which does not lower cleanly: {:?}",
                printed.text(),
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            );

            // The walk pairs nodes positionally, which only means anything if the two
            // trees have the same shape to begin with.
            prop_assert_eq!(
                canonical(&ast, &interner),
                canonical(&reast, &reinterner),
                "printed {:?}", printed.text()
            );

            spans_agree_in_query(&printed, (&ast, ast.query()), (&reast, reast.query()))?;
        }
    }

    /// The text a span covers, for a failure message.
    fn slice(text: &str, span: &NodeSpan) -> String {
        match text.get(source_range(span)) {
            Some(text) => format!("{text:?}"),
            None => "<not a valid range>".to_owned(),
        }
    }

    /// Walk two same-shaped trees together, checking each printed span against the
    /// one lowering recovered.
    fn spans_agree(
        printed: &Spanned,
        (ast, id): (&Ast, NodeId),
        (reast, reid): (&Ast, NodeId),
    ) -> Result<(), TestCaseError> {
        let expected = printed.span(id);
        let found = reast.store().span(reid);
        prop_assert_eq!(
            expected.clone(),
            found.clone(),
            "printed at {:?} = {}, lowered back at {:?} = {} — in {:?}",
            expected,
            slice(printed.text(), &expected),
            found,
            slice(printed.text(), &found),
            printed.text()
        );

        // Leaves have no children, and a variant mismatch is impossible: the caller
        // has already compared canonical forms.
        match (ast.store().kind(id), reast.store().kind(reid)) {
            (ExprKind::Record(fields), ExprKind::Record(refields)) => {
                for ((_, value), (_, revalue)) in fields.iter().zip(refields.iter()) {
                    spans_agree(printed, (ast, *value), (reast, *revalue))?;
                }
            }
            (ExprKind::Access(_, base), ExprKind::Access(_, rebase))
            | (ExprKind::Select(_, base), ExprKind::Select(_, rebase))
            | (ExprKind::Fact(_, base), ExprKind::Fact(_, rebase)) => {
                spans_agree(printed, (ast, *base), (reast, *rebase))?;
            }
            (ExprKind::Disjunction(branches), ExprKind::Disjunction(rebranches)) => {
                for (branch, rebranch) in branches.iter().zip(rebranches.iter()) {
                    spans_agree(printed, (ast, *branch), (reast, *rebranch))?;
                }
            }
            (ExprKind::Subquery(query), ExprKind::Subquery(requery)) => {
                spans_agree_in_query(printed, (ast, query), (reast, requery))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn spans_agree_in_query(
        printed: &Spanned,
        (ast, query): (&Ast, &Query<NodeId>),
        (reast, requery): (&Ast, &Query<NodeId>),
    ) -> Result<(), TestCaseError> {
        spans_agree(printed, (ast, *query.head()), (reast, *requery.head()))?;
        for (stmt, restmt) in query.body().iter().zip(requery.body()) {
            match (stmt, restmt) {
                (QueryStmt::Implicit(id), QueryStmt::Implicit(reid))
                | (QueryStmt::Negation(id), QueryStmt::Negation(reid)) => {
                    spans_agree(printed, (ast, *id), (reast, *reid))?;
                }
                (QueryStmt::Bind(lhs, rhs), QueryStmt::Bind(relhs, rerhs)) => {
                    spans_agree(printed, (ast, *lhs), (reast, *relhs))?;
                    spans_agree(printed, (ast, *rhs), (reast, *rerhs))?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod generator {
    use crate::focus::{corpus, print::print, syntax::proptest::arb_query_spec};
    use proptest::{
        strategy::{Strategy, ValueTree},
        test_runner::TestRunner,
    };

    /// The round-trip property is only as good as what it is handed, and a
    /// generator can degenerate silently — a change to a `prop_recursive` weight or
    /// a leaf set can quietly reduce it to variables and wildcards, leaving the
    /// property green and vacuous.
    ///
    /// So the shape of the generated population is itself asserted: mostly
    /// non-trivial trees, and every construct reached.
    #[test]
    fn the_generator_is_not_degenerate() {
        const RUNS: usize = 400;

        let schema = corpus::schema();
        let mut runner = TestRunner::deterministic();
        let mut sizes = vec![];
        let mut text = String::new();

        for _ in 0..RUNS {
            let spec = arb_query_spec().new_tree(&mut runner).unwrap().current();
            let (ast, interner) = spec.build(&schema);
            sizes.push(ast.store().len());
            text.push_str(&print(&ast, &schema, &interner));
            text.push('\n');
        }

        sizes.sort_unstable();
        let median = sizes[RUNS / 2];
        assert!(median >= 8, "median tree is only {median} nodes");

        let trivial = sizes.iter().filter(|n| **n <= 3).count();
        assert!(
            trivial * 10 < RUNS,
            "{trivial} of {RUNS} trees are trivial (<= 3 nodes)"
        );

        // Every construct on the surface must actually be reached, including the ones
        // whose *printing* is the interesting part.
        for (what, needle) in [
            ("disjunction", " | "),
            ("subquery", " where "),
            ("negation", "!"),
            ("record", "{"),
            ("empty record", "{}"),
            ("field access", "."),
            ("value access", ".value"),
            ("union select", "?"),
            ("never", "never"),
            ("wildcard", "_"),
            ("string prefix", ".."),
            ("negative literal", "-"),
            ("i64::MIN", "-9223372036854775808"),
            ("escaped quote", "\\\""),
            ("escaped control char", "\\u00"),
            ("parenthesised group", "("),
        ] {
            assert!(
                text.contains(needle),
                "the generator never produced a {what}"
            );
        }
    }
}
