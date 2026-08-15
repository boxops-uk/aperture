//! The compilation driver: one context the phases run through.
//!
//! Before this, `parse`, `lower` and `ty::check` were three functions a caller
//! sequenced by hand, each needing the schema and an interner threaded to it and
//! each handing back diagnostics to be collected. Every caller wrote the same
//! fifteen lines, and each was free to write them differently — `src/main.rs`
//! stopped after a parse error, `focus::corpus` carried on.
//!
//! [`Compilation`] owns that plumbing instead: the source, the schema, the
//! per-query interner, the diagnostics sink, and the trees and side tables the
//! phases produce ([chapter 7]). Phase 4's flatten becomes another pass over the
//! same state rather than a fourth function with a fourth `Vec`.
//!
//! **Deliberately not a query engine.** No memoization, no dependency graph, no
//! incremental recomputation — a compilation is one query, compiled once, and
//! designing for incrementality before anything needs it would buy a rewrite
//! later ([`PLAN.md`] Phase 3).
//!
//! # What it does not store
//!
//! The **CST**. It borrows the source, and holding it here would make this a
//! self-referential struct for the sake of a tree that lowering consumes and
//! nothing reads again. `Ast` and `Typed` are owned outright, so the context is a
//! plain struct parameterised by the source's lifetime.
//!
//! [chapter 7]: ../../../docs/07-compilation.md
//! [`PLAN.md`]: ../../../PLAN.md

use codespan_reporting::{diagnostic::LabelStyle, files, files::SimpleFile, term};

use super::{
    cst::CstNode,
    diag::{Diagnostic, Diagnostics},
    flatten::flatten,
    lower::lower,
    parse::parse,
    plan::Plan,
    syntax::{Ast, Ty},
    ty::{self, Typed},
};
use aperture_schema::schema::{LocalInterner, Schema};

/// One query, compiled.
///
/// Built with [`new`](Self::new), driven with [`check`](Self::check) or
/// [`plan`](Self::plan), and read for diagnostics either way — the phases keep
/// going through faults, so a result and diagnostics are not alternatives.
pub struct Compilation<'src> {
    source: &'src str,
    schema: &'src Schema,
    interner: LocalInterner,
    diagnostics: Diagnostics,
    ast: Option<Ast>,
    typed: Option<Typed>,
    /// Whether [`check`](Self::check) has run, so it runs once. Distinct from
    /// `ast.is_some()`, because a refused parse produces no tree and must not
    /// re-run the phases on every call.
    checked: bool,
    /// Whether [`plan`](Self::plan) has run — see there for why it runs once.
    planned: bool,
}

impl<'src> Compilation<'src> {
    #[must_use]
    pub fn new(source: &'src str, schema: &'src Schema) -> Self {
        Self {
            source,
            schema,
            interner: LocalInterner::new(schema.interner().clone()),
            diagnostics: Diagnostics::new(),
            ast: None,
            typed: None,
            checked: false,
            planned: false,
        }
    }

    /// Run `parse → lower → typecheck`, reporting everything into one sink.
    ///
    /// `None` means the parse was **refused** — no tree, so nothing downstream can
    /// run (see [`parse`]). Anything else returns `Some`, *including* a query with
    /// errors in it: the phases are keep-going by design, a tree with holes is
    /// lowered to error nodes rather than abandoned, and poison stops those
    /// spreading into a cascade. So a caller decides validity by asking
    /// [`diagnostics`](Self::diagnostics), not by the `Option`.
    ///
    /// Runs once. Calling it again returns the first run's result rather than
    /// reporting everything a second time.
    pub fn check(&mut self) -> Option<&Typed> {
        if self.checked {
            return self.typed.as_ref();
        }
        self.checked = true;

        // Destructured, so the phases can borrow the interner and the sink at once
        // — the whole borrow story of the context, and the reason these are fields
        // rather than something behind a `&mut self` method chain.
        let Self {
            source,
            schema,
            interner,
            diagnostics,
            ..
        } = self;

        let cst = parse(source, diagnostics)?;
        let ast = lower(&CstNode::new(&cst), schema, interner, diagnostics);
        let typed = ty::check(&ast, schema, interner, diagnostics);

        self.ast = Some(ast);
        self.typed = Some(typed);
        self.typed.as_ref()
    }

    /// Compile all the way to a [`Plan`] — the driver's terminal product.
    ///
    /// Type-checks first, and **stops if anything was reported**: flatten handles
    /// the implemented subset, so a query with a deferred construct or a type error
    /// in it has no plan to be missing, and running flatten over it would report
    /// consequences of a fault the user already has.
    ///
    /// `None` means no plan, always with a reason in the sink
    /// ([`flatten`](crate::flatten)).
    ///
    /// **Produced once.** Flatten reports into the shared sink, so a second run
    /// would report every fault it found a second time; a caller wanting the plan
    /// twice keeps the one it was given. Asking again yields `None`.
    pub fn plan(&mut self) -> Option<Plan> {
        self.check()?;

        if self.diagnostics.has_errors() || self.planned {
            return None;
        }
        self.planned = true;

        // Destructured for the same reason `check` is: flatten reads the tree and
        // the interner while reporting into the sink.
        let Self {
            schema,
            interner,
            diagnostics,
            ast,
            ..
        } = self;

        flatten(ast.as_ref()?, schema, interner, diagnostics)
    }

    /// The type of the query's head, once [`check`](Self::check) has run.
    #[must_use]
    pub fn head_ty(&self) -> Option<&Ty> {
        let ast = self.ast.as_ref()?;
        self.typed.as_ref()?.ty(*ast.query().head())
    }

    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    #[must_use]
    pub fn ast(&self) -> Option<&Ast> {
        self.ast.as_ref()
    }

    #[must_use]
    pub fn interner(&self) -> &LocalInterner {
        &self.interner
    }

    /// Take the interner, consuming the compilation.
    ///
    /// What a caller that outlives the compilation needs: a `Plan`'s projections hold
    /// `Symbol`s minted here, so anything decoding a row later has to resolve them
    /// against **this** interner. A second one built from the same schema would agree
    /// about schema names and disagree about every local one, which is a row whose
    /// head fields cannot be named.
    #[must_use]
    pub fn into_interner(self) -> LocalInterner {
        self.interner
    }

    #[must_use]
    pub fn source(&self) -> &'src str {
        self.source
    }

    /// The diagnostics in the order a reader wants them: by where they point.
    ///
    /// The sink itself keeps **arrival** order, which is phase order — every
    /// lowering fault precedes every typecheck fault, whatever part of the query
    /// each is about. That is right for the sink, which is a log, and what
    /// [`Diagnostics::since`] slices by phase. It is wrong for a person, who reads
    /// the query top to bottom: a fault at the head reported *after* one in the
    /// body reads as though the head were fine.
    ///
    /// So presentation sorts and the log does not. Stably, so two diagnostics
    /// about the same span stay in the order the phases found them, and by the
    /// earliest primary label — a diagnostic with no label (the parse refusals,
    /// which have nothing to point at) sorts first, and is the only diagnostic
    /// there is in those cases.
    fn in_source_order(&self) -> Vec<&Diagnostic> {
        let mut ordered: Vec<&Diagnostic> = self.diagnostics.iter().collect();

        ordered.sort_by_key(|diagnostic| {
            diagnostic
                .labels
                .iter()
                .filter(|label| label.style == LabelStyle::Primary)
                .map(|label| label.range.start)
                .min()
                .unwrap_or(0)
        });

        ordered
    }

    /// Render every diagnostic against the source, styled, in source order.
    ///
    /// Rendering lives here because the context is what holds the source a
    /// diagnostic's spans point into; a caller that had to build its own
    /// `SimpleFile` could build one over different text.
    pub fn render<W>(&self, writer: &mut W, config: &term::Config) -> Result<(), files::Error>
    where
        W: term::WriteStyle + ?Sized,
    {
        let file = SimpleFile::new("<input>", self.source);

        for diagnostic in self.in_source_order() {
            term::emit_to_write_style(writer, config, &file, diagnostic)?;
        }

        Ok(())
    }

    /// Every diagnostic rendered as plain text, in source order.
    ///
    /// What a test asserts on, and what a non-terminal caller wants.
    #[must_use]
    pub fn render_to_string(&self) -> String {
        let file = SimpleFile::new("<input>", self.source);
        let config = term::Config::default();
        let mut out = String::new();

        for diagnostic in self.in_source_order() {
            // A `String` sink cannot fail to be written to; a diagnostic naming a
            // file this one doesn't have would, and there is exactly one file.
            let _ = term::emit_to_string(&mut out, &config, &file, diagnostic);
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{corpus, print::canonical};
    use proptest::prelude::*;

    #[test]
    fn a_clean_query_typechecks_and_reports_nothing() {
        let schema = corpus::schema();
        let mut compilation = Compilation::new("X where X = test.Foo _", &schema);

        assert!(compilation.check().is_some());
        assert!(
            compilation.diagnostics().is_empty(),
            "{:?}",
            compilation.render_to_string()
        );
        assert!(compilation.head_ty().is_some());
    }

    /// **One sink, every phase — and two orders, deliberately.**
    ///
    /// A query wrong in lowering *and* in typecheck reports both. The sink holds
    /// them in arrival order, which is phase order, because that is what makes
    /// `since(mark)` mean "what this phase reported". Rendering sorts them by
    /// where they point, because a reader works down the query and a fault at the
    /// head reported after one in the body reads as though the head were fine.
    ///
    /// Both halves are pinned here: they are the same two diagnostics in opposite
    /// orders, so a change that collapsed the distinction fails one or the other.
    #[test]
    fn one_sink_holds_every_phase_and_rendering_sorts_it() {
        let schema = corpus::schema();

        // `nosuch.Pred` is lowering's (an unresolvable name) and comes second in
        // the text; the wildcard head is typecheck's and comes first.
        let mut compilation = Compilation::new("_ where X = nosuch.Pred _", &schema);
        compilation.check();

        assert_eq!(
            compilation.diagnostics().codes().collect::<Vec<_>>(),
            ["reject/unknown-predicate", "reject/wildcard-in-head"],
            "the sink is a log: phase order, so `since` can slice it",
        );

        let rendered = compilation.render_to_string();
        let head = rendered.find("wildcard").expect("the head diagnostic");
        let name = rendered.find("nosuch").expect("the name diagnostic");
        assert!(
            head < name,
            "rendering is for a reader: source order, whatever phase found what\n{rendered}"
        );
    }

    /// A refused parse yields diagnostics and no tree — and does not panic.
    ///
    /// The one case where `check` returns `None`: there is nothing to lower, as
    /// opposed to something with holes in it.
    #[test]
    fn a_refused_parse_yields_diagnostics_and_no_tree() {
        let schema = corpus::schema();
        let deep = format!("X where X = {}", "{a = ".repeat(300));
        let mut compilation = Compilation::new(&deep, &schema);

        assert!(compilation.check().is_none());
        assert!(compilation.ast().is_none());
        assert!(compilation.head_ty().is_none());
        assert!(compilation.diagnostics().has_errors());
        assert!(
            compilation
                .render_to_string()
                .contains("nested deeper than"),
            "the refusal must say why"
        );
    }

    /// `check` runs once: asking twice does not report twice.
    #[test]
    fn checking_twice_does_not_report_twice() {
        let schema = corpus::schema();
        let mut compilation = Compilation::new("X.alt? where X = test.Foo _", &schema);

        compilation.check();
        let after_first = compilation.diagnostics().len();
        compilation.check();

        assert_eq!(compilation.diagnostics().len(), after_first);
        assert_eq!(after_first, 1);
    }

    /// `plan` runs the whole pipeline: a clean query comes back as a runnable plan
    /// with nothing reported.
    #[test]
    fn planning_a_clean_query_produces_a_plan() {
        let schema = corpus::schema();
        let mut compilation = Compilation::new("X where X = test.Foo _", &schema);

        let plan = compilation.plan();
        assert!(
            compilation.diagnostics().is_empty(),
            "{}",
            compilation.render_to_string()
        );

        let plan = plan.expect("a plan");
        assert_eq!(plan.body.len(), 1);
        assert_eq!(plan.nvars, 1);
    }

    /// A query that does not typecheck has no plan, and flatten is not run over it
    /// — so the only diagnostic is the one the user can act on.
    #[test]
    fn a_query_that_does_not_typecheck_is_not_flattened() {
        let schema = corpus::schema();

        for (source, code) in [
            ("X where X = nosuch.Pred _", "reject/unknown-predicate"),
            ("X.alt? where X = test.Foo _", "nyi/union-select"),
            ("_ where test.Foo _", "reject/wildcard-in-head"),
        ] {
            let mut compilation = Compilation::new(source, &schema);
            assert!(compilation.plan().is_none(), "{source:?}");
            assert_eq!(
                compilation.diagnostics().codes().collect::<Vec<_>>(),
                [code],
                "{source:?}",
            );
        }
    }

    /// `plan` produces once. Flatten reports into the shared sink, so a second run
    /// would say everything twice — the same reason the source is lexed once.
    #[test]
    fn planning_twice_does_not_report_twice() {
        let schema = corpus::schema();

        // A query flatten rejects, so there is a diagnostic that could be doubled.
        let mut compilation = Compilation::new("X where test.Foo _", &schema);

        assert!(compilation.plan().is_none());
        let after_first = compilation.diagnostics().len();
        assert!(compilation.plan().is_none());

        assert_eq!(compilation.diagnostics().len(), after_first);
        assert_eq!(after_first, 1);
    }

    proptest! {
        /// **Compiling is deterministic.** The same source twice gives an identical
        /// tree and identical rendered diagnostics.
        ///
        /// What this catches is output that depends on something other than the
        /// input: a `HashMap`'s iteration order in some later phase, an address, a
        /// clock. The front end has none of those today — record fields are sorted
        /// slices by convention — and this is what says so as flatten and the rest
        /// arrive.
        ///
        /// **What it does not catch**, stated because the distinction is easy to
        /// get wrong: a dependence on *interning* order. Two runs of the same source
        /// intern the same names in the same order, so both would leak it
        /// identically and agree. `print`'s round-trip is the guard for that — it
        /// re-parses with a deliberately fresh interner and compares canonical
        /// forms. Checked rather than assumed: collapsing every name in the
        /// canonical form to a per-tier constant leaves *this* property green and
        /// fails that one.
        #[test]
        fn compiling_is_deterministic(source in arb_source()) {
            let schema = corpus::schema();

            let mut first = Compilation::new(&source, &schema);
            first.check();
            let mut second = Compilation::new(&source, &schema);
            second.check();

            prop_assert_eq!(first.render_to_string(), second.render_to_string());

            match (first.ast(), second.ast()) {
                (Some(a), Some(b)) => prop_assert_eq!(
                    canonical(a, first.interner()),
                    canonical(b, second.interner()),
                ),
                (None, None) => {}
                _ => prop_assert!(false, "one run produced a tree and the other did not"),
            }
        }

        /// The composed pipeline never panics, whatever it is handed.
        ///
        /// Each phase has this property already; the driver is where they meet, and
        /// a phase that is fine alone can still be handed something impossible by
        /// the one before it.
        #[test]
        fn compiling_arbitrary_sources_never_panics(source in ".{0,120}") {
            let schema = corpus::schema();
            let mut compilation = Compilation::new(&source, &schema);
            compilation.check();

            // Rendering walks every label's span against the source, so it is part
            // of what must not panic: a span past the end would take the shell down
            // at the moment it tried to explain the fault.
            let _ = compilation.render_to_string();
        }
    }

    /// Sources with enough structure to reach the later phases, mixed with junk.
    fn arb_source() -> impl Strategy<Value = String> {
        prop_oneof![
            proptest::sample::select(
                corpus::CORPUS
                    .iter()
                    .map(|e| e.source.to_owned())
                    .collect::<Vec<_>>()
            ),
            ".{0,60}".prop_map(|s| s),
        ]
    }
}
