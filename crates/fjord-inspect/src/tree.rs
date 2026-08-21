//! The parse, as data: the **lossless, untyped, grammar-shaped tree** the book's
//! phase table promises, with every node's span.
//!
//! This is the tree the parser actually produces — grammar rules and token
//! leaves — not the lowered one. Two things follow from that, and both are why
//! it is worth showing:
//!
//! - **It is permissive.** The grammar accepts constructs that mean nothing, and
//!   they are refused later by name at typecheck or flatten. A reader sees the
//!   shape before anything has judged it.
//! - **It needs no schema.** Lowering resolves names against one; parsing does
//!   not, so a browser can show this for any text at all, including text that is
//!   half-typed.
//!
//! Every node carries its span, so a page can highlight both ways: source to
//! node, and node to source.
//!
//! **Two properties a page leans on, and both are asserted.** The leaves
//! reassemble the source exactly — the grammar's `skip Whitespace` keeps trivia
//! out of the parser's *matching*, not out of the tree — and a node's span
//! contains every child's. A view that quietly widened a span would still look
//! plausible in a tree and would highlight the wrong bytes; `tree_spans_nest`
//! is what catches it.

use fjord_engine::{
    cst::{CstKind, CstNode},
    diag::Diagnostics,
    parse::parse,
    parser::Rule,
};
use serde::Serialize;

use crate::{
    tokens::kind as token_kind,
    view::{DiagnosticView, Span, views_of},
};

/// A node of the parse tree, in a flat arena.
///
/// Dense ids rather than nesting, because a page needs to address a node — to
/// select it, to scroll to it, to say which one the cursor is in — and an index
/// is the cheapest name there is. Children are ids into the same `Vec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeNode {
    pub id: usize,
    /// The grammar rule (`Stmt`, `FactPattern`) or the token (`QId`, `LBrace`).
    pub kind: &'static str,
    /// Whether this is a leaf. A page indents rules and prints tokens.
    pub token: bool,
    /// A token's text. Absent for a rule, whose text is its span of the source.
    pub label: Option<String>,
    pub span: Span,
    pub children: Vec<usize>,
}

/// A parsed source: the tree, if there is one, and what was reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tree {
    /// The root, or `None` when the parse was refused outright — too long, too
    /// deeply nested, or unparseable from the first token.
    pub root: Option<usize>,
    pub nodes: Vec<TreeNode>,
    pub diagnostics: Vec<DiagnosticView>,
}

/// Parse `source` and describe the tree it produced.
///
/// Never fails: a refusal is `root: None` plus the diagnostics that say why. A
/// parse that recovered has both a tree *and* diagnostics, and the tree carries
/// `Error` nodes where recovery happened — which is the state an interactive
/// view spends most of its time in.
#[must_use]
pub fn tree(source: &str) -> Tree {
    let mut diagnostics = Diagnostics::new();
    let parsed = parse(source, &mut diagnostics);

    let mut nodes = Vec::new();
    let root = parsed
        .as_ref()
        .map(|cst| push(&CstNode::new(cst), &mut nodes));

    Tree {
        root,
        nodes,
        diagnostics: views_of(&diagnostics),
    }
}

/// The same view, already JSON — see [`crate::tokens::tokens_json`] for why the
/// encoder lives on this side of the WebAssembly boundary.
#[must_use]
pub fn tree_json(source: &str) -> String {
    serde_json::to_string(&tree(source)).expect("a tree view serialises")
}

/// Flatten one node into the arena, depth first, and answer its id.
///
/// Written as an explicit recursion rather than through `CstNode::cata`, because
/// a node needs its id *before* its children are pushed — the arena's order is
/// parent-then-children, which is what lets a page render it without a second
/// pass.
fn push(node: &CstNode<'_>, nodes: &mut Vec<TreeNode>) -> usize {
    let id = nodes.len();

    match node.kind() {
        CstKind::Rule {
            rule,
            span,
            children,
        } => {
            nodes.push(TreeNode {
                id,
                kind: rule_name(rule),
                token: false,
                label: None,
                span: Span {
                    start: span.start,
                    end: span.end,
                },
                children: Vec::new(),
            });

            let child_ids: Vec<_> = children.iter().map(|child| push(child, nodes)).collect();
            nodes[id].children = child_ids;
        }
        CstKind::Token { token, text, span } => nodes.push(TreeNode {
            id,
            kind: token_kind(token),
            token: true,
            label: Some(text.to_owned()),
            span: Span {
                start: span.start,
                end: span.end,
            },
            children: Vec::new(),
        }),
    }

    id
}

/// The rule's name, as a string a page can key on.
///
/// Exhaustive for the same reason the token table is: the grammar is generated
/// from `grammar.llw`, so a rule added there does not compile until somebody
/// says what it is called here — which is the only thing that keeps a view from
/// silently omitting a construct.
const fn rule_name(rule: Rule) -> &'static str {
    match rule {
        Rule::AccessPattern => "AccessPattern",
        Rule::AnonRecordPrimary => "AnonRecordPrimary",
        Rule::Arith => "Arith",
        Rule::BindStmt => "BindStmt",
        Rule::Branch => "Branch",
        Rule::DenyStmt => "DenyStmt",
        Rule::Disjunction => "Disjunction",
        Rule::Error => "Error",
        Rule::Fact => "Fact",
        Rule::FactPattern => "FactPattern",
        Rule::Field => "Field",
        Rule::FieldList => "FieldList",
        Rule::GeStmt => "GeStmt",
        Rule::GtStmt => "GtStmt",
        Rule::ImplicitBindStmt => "ImplicitBindStmt",
        Rule::IntPrimary => "IntPrimary",
        Rule::LeStmt => "LeStmt",
        Rule::LtStmt => "LtStmt",
        Rule::NatPrimary => "NatPrimary",
        Rule::NegationStmt => "NegationStmt",
        Rule::NeverPrimary => "NeverPrimary",
        Rule::ParenPrimary => "ParenPrimary",
        Rule::Pattern => "Pattern",
        Rule::Primary => "Primary",
        Rule::Query => "Query",
        Rule::Root => "Root",
        Rule::Stmt => "Stmt",
        Rule::StmtList => "StmtList",
        Rule::StringPrefixPrimary => "StringPrefixPrimary",
        Rule::StringPrimary => "StringPrimary",
        Rule::SubqueryPrimary => "SubqueryPrimary",
        Rule::Sum => "Sum",
        Rule::VarPrimary => "VarPrimary",
        Rule::WildcardPrimary => "WildcardPrimary",
    }
}
