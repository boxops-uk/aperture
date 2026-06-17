use crate::lens::{
    lexer::Token,
    parser::{Cst, Node, NodeRef, Rule, Span},
};

pub enum CstKind<'s, T> {
    Rule {
        span: Span,
        rule: Rule,
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
                span,
                rule,
                children,
            } => CstKind::Rule {
                span,
                rule,
                children: children
                    .into_iter()
                    .map(f)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            },
            CstKind::Token { token, text, span } => CstKind::Token { token, text, span },
        }
    }
}

pub struct CstNode<'s> {
    cst: &'s Cst<'s>,
    node_ref: NodeRef,
}

impl<'s> CstNode<'s> {
    pub fn new(cst: &'s Cst<'s>) -> Self {
        Self {
            cst,
            node_ref: NodeRef::ROOT,
        }
    }

    pub fn kind(&self) -> CstKind<'s, CstNode<'s>> {
        match self.cst.get(self.node_ref) {
            Node::Rule(rule, _) => CstKind::Rule {
                span: self.cst.span(self.node_ref),
                rule: rule,
                children: self
                    .cst
                    .children(self.node_ref)
                    .map(|child_ref| CstNode {
                        cst: self.cst,
                        node_ref: child_ref,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            },
            Node::Token(token, _) => CstKind::Token {
                token: token,
                text: self.cst.source()[self.cst.span(self.node_ref)].into(),
                span: self.cst.span(self.node_ref),
            },
        }
    }

    pub fn cata<R>(&self, f: &mut impl FnMut(CstKind<'s, R>) -> R) -> R {
        let kind = self.kind().map(|child| child.cata(f));
        f(kind)
    }

    pub fn para<R>(&self, f: &mut impl FnMut(CstKind<'s, (CstNode<'s>, R)>) -> R) -> R {
        let kind = self.kind().map(|child| {
            let r = child.para(f);
            (child, r)
        });
        f(kind)
    }

    pub fn span(&self) -> Span {
        self.cst.span(self.node_ref)
    }
}
