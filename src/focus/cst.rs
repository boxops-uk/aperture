//! The **CST façade** — an untyped, lossless, grammar-shaped view of a parse.
//!
//! This is the first of the three tree representations ([chapter 7]). Its job is
//! *fidelity*: every node carries its span and the source text it covers, so
//! diagnostics can point at exactly the right bytes and a parse round-trips back
//! to text. It is deliberately **permissive** — it happily represents constructs
//! that are meaningless, which are rejected later at typecheck/flatten with a
//! clear diagnostic rather than contorted into the grammar.
//!
//! The generated parser produces a flat, index-addressed [`Cst`]. This module
//! wraps it as a recursive tree with two folds — [`CstNode::cata`] (bottom-up)
//! and [`CstNode::para`] (bottom-up, keeping the node beside each result) — which
//! is what lowering runs on.
//!
//! [chapter 7]: ../../../docs/07-compilation.md

use crate::focus::{
    lexer::Token,
    parser::{Cst, Node, NodeRef, Rule, Span},
};

/// One layer of the façade: a grammar rule with its children, or a token leaf.
///
/// The child type is a parameter so the folds can replace children with their
/// results — the functor the `cata`/`para` algebras are written against.
pub enum CstKind<'s, T> {
    Rule {
        rule: Rule,
        span: Span,
        children: Box<[T]>,
    },
    Token {
        token: Token,
        text: &'s str,
        span: Span,
    },
}

impl<'s, T> CstKind<'s, T> {
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> CstKind<'s, U> {
        match self {
            CstKind::Rule {
                rule,
                span,
                children,
            } => CstKind::Rule {
                rule,
                span,
                children: children.into_iter().map(f).collect(),
            },
            CstKind::Token { token, text, span } => CstKind::Token { token, text, span },
        }
    }
}

/// A cursor at one node of a parsed [`Cst`].
///
/// Cheap to copy — a shared reference plus an index.
#[derive(Clone, Copy)]
pub struct CstNode<'s> {
    cst: &'s Cst<'s>,
    node_ref: NodeRef,
}

impl<'s> CstNode<'s> {
    /// The root of `cst`.
    pub fn new(cst: &'s Cst<'s>) -> Self {
        Self {
            cst,
            node_ref: NodeRef::ROOT,
        }
    }

    /// This node, with its children as cursors.
    pub fn kind(&self) -> CstKind<'s, CstNode<'s>> {
        let span = self.cst.span(self.node_ref);
        match self.cst.get(self.node_ref) {
            Node::Rule(rule, _) => CstKind::Rule {
                rule,
                span,
                children: self
                    .cst
                    .children(self.node_ref)
                    .map(|node_ref| CstNode {
                        cst: self.cst,
                        node_ref,
                    })
                    .collect(),
            },
            Node::Token(token, _) => CstKind::Token {
                token,
                text: &self.cst.source()[span.clone()],
                span,
            },
        }
    }

    /// Fold the subtree bottom-up.
    pub fn cata<R>(&self, f: &mut impl FnMut(CstKind<'s, R>) -> R) -> R {
        let kind = self.kind().map(|child| child.cata(f));
        f(kind)
    }

    /// Fold the subtree bottom-up, keeping each child's cursor beside its result.
    ///
    /// Lowering needs the cursor as well as the result — a node's span and token
    /// text are only reachable through it.
    pub fn para<R>(&self, f: &mut impl FnMut(CstKind<'s, (CstNode<'s>, R)>) -> R) -> R {
        let kind = self.kind().map(|child| {
            let result = child.para(f);
            (child, result)
        });
        f(kind)
    }

    pub fn span(&self) -> Span {
        self.cst.span(self.node_ref)
    }
}
