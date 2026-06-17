use im::HashMap;
use std::cell::RefCell;
use string_interner::DefaultStringInterner as StringInterner;
use string_interner::DefaultSymbol as Symbol;

use crate::lens::cst::CstKind;
use crate::lens::cst::CstNode;
use crate::lens::lexer::Token;
use crate::lens::location::Location;
use crate::lens::parser::Rule;
use crate::lens::query::PatternKind;
use crate::lens::query::{Pattern, Query, Statement};
use crate::lens::schema::PredicateId;
use crate::lens::schema::Schema;

#[derive(Debug)]
pub enum Lowered<FileId> {
    Query(Query<FileId>),
    StmtList(Box<[Statement<FileId>]>),
    Statement(Statement<FileId>),
    Pattern(Pattern<FileId>),
    FieldList(HashMap<Symbol, Pattern<FileId>>),
    Field((Symbol, Pattern<FileId>)),
    IntermediateFact {
        predicate_id: PredicateId,
        key_pattern: Option<Box<Pattern<FileId>>>,
    },
    UId(Symbol),
    LId(Symbol),
    Nat(u32),
    Str(Symbol),
    Trivia,
}

pub struct Lowering<'a, FileId> {
    interner: &'a RefCell<StringInterner>,
    schema: &'a Schema,
    file_id: FileId,
}

impl<'a, FileId> Lowering<'a, FileId>
where
    FileId: Copy + std::fmt::Debug,
{
    pub fn new(interner: &'a RefCell<StringInterner>, schema: &'a Schema, file_id: FileId) -> Self {
        Self {
            interner,
            schema,
            file_id,
        }
    }

    pub fn lower<'s>(&self, cst: &'s CstNode<'s>) -> Query<FileId> {
        let Lowered::Query(q) = cst.para(&|kind| self.algebra(kind)) else {
            unreachable!()
        };

        q
    }

    fn intern<'s>(&self, s: &'s str) -> Symbol {
        self.interner.borrow_mut().get_or_intern(s)
    }

    fn loc(&self, span: super::parser::Span) -> Location<FileId> {
        Location::new(self.file_id, span)
    }

    fn algebra<'s>(&self, kind: CstKind<'s, (CstNode<'s>, Lowered<FileId>)>) -> Lowered<FileId> {
        match kind {
            CstKind::Rule {
                rule: Rule::Root,
                children,
                ..
            } => children
                .into_iter()
                .find_map(|(_, lowered)| match lowered {
                    Lowered::Query(query) => Some(Lowered::Query(query)),
                    _ => None,
                })
                .expect("root has a query"),

            CstKind::Rule {
                rule: Rule::Query,
                span,
                children,
            } => {
                let mut head = None;
                let mut body = None;

                for (_, lowered) in children {
                    match lowered {
                        Lowered::Pattern(p) => head = Some(p),
                        Lowered::StmtList(s) => body = Some(s),
                        _ => {}
                    }
                }

                Lowered::Query(Query {
                    location: self.loc(span),
                    head: head.expect("query has a head pattern"),
                    body: body.expect("query has a body"),
                })
            }

            CstKind::Rule {
                rule: Rule::StmtList,
                children,
                ..
            } => Lowered::StmtList(
                children
                    .into_iter()
                    .filter_map(|(_, lowered)| match lowered {
                        Lowered::Statement(stmt) => Some(stmt),
                        _ => None,
                    })
                    .collect(),
            ),

            CstKind::Rule {
                rule: Rule::BindStmt,
                children,
                ..
            } => {
                let mut pats = children.into_iter().filter_map(|(_, l)| match l {
                    Lowered::Pattern(p) => Some(p),
                    _ => None,
                });
                Lowered::Statement(Statement::Bind {
                    left: pats.next().expect("bind statement has a lhs pattern"),
                    right: pats.next().expect("bind statement has a rhs pattern"),
                })
            }

            CstKind::Rule {
                rule: Rule::ImplicitBindStmt,
                children,
                ..
            } => {
                let p = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::Pattern(p) => Some(p),
                        _ => None,
                    })
                    .expect("implicit bind statement has a pattern");
                Lowered::Statement(Statement::ImplicitBind(p))
            }

            CstKind::Rule {
                rule: Rule::WildcardApattern,
                span,
                ..
            } => Lowered::Pattern(Pattern {
                location: self.loc(span),
                kind: PatternKind::Wildcard,
            }),

            CstKind::Rule {
                rule: Rule::VarApattern,
                span,
                children,
            } => {
                let sym = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::UId(s) => Some(s),
                        _ => None,
                    })
                    .expect("var apattern has a uid");
                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::Var(sym),
                })
            }

            CstKind::Rule {
                rule: Rule::NatApattern,
                span,
                children,
            } => {
                let n = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::Nat(n) => Some(n),
                        _ => None,
                    })
                    .expect("nat apattern has a nat");
                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::Int(n.into()),
                })
            }

            CstKind::Rule {
                rule: Rule::IntApattern,
                span,
                children,
            } => {
                let n = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::Nat(n) => Some(n),
                        _ => None,
                    })
                    .expect("int apattern has a nat");
                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::Int(-(n as i64)),
                })
            }

            CstKind::Rule {
                rule: Rule::StringApattern,
                span,
                children,
            } => {
                let s = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::Str(s) => Some(s),
                        _ => None,
                    })
                    .expect("string apattern has a str");
                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::String(s),
                })
            }

            CstKind::Rule {
                rule: Rule::StringPrefixApattern,
                span,
                children,
            } => {
                let s = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::Str(s) => Some(s),
                        _ => None,
                    })
                    .expect("string prefix apattern has a str");
                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::StringPrefix(s),
                })
            }

            CstKind::Rule {
                rule: Rule::AnonRecordApattern,
                span,
                children,
            } => {
                let field_patterns = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::FieldList(fl) => Some(fl),
                        _ => None,
                    })
                    .unwrap_or_default();
                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::Record { field_patterns },
                })
            }

            CstKind::Rule {
                rule: Rule::FactApattern,
                span,
                children,
            } => {
                let (predicate_id, key_pattern) = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::IntermediateFact {
                            predicate_id,
                            key_pattern,
                        } => Some((predicate_id, key_pattern)),
                        _ => None,
                    })
                    .expect("fact apattern has an intermediate fact");

                let key_pattern = key_pattern.unwrap_or_else(|| {
                    Box::new(Pattern {
                        location: self.loc(span.clone()),
                        kind: PatternKind::Record {
                            field_patterns: HashMap::new(),
                        },
                    })
                });

                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::Fact {
                        predicate_id,
                        key_pattern,
                    },
                })
            }

            CstKind::Rule {
                rule: Rule::SubqueryApattern,
                span,
                children,
            } => {
                let q = children
                    .into_iter()
                    .find_map(|(_, l)| match l {
                        Lowered::Query(q) => Some(q),
                        _ => None,
                    })
                    .expect("subquery apattern has a query");
                Lowered::Pattern(Pattern {
                    location: self.loc(span),
                    kind: PatternKind::Subquery(Box::new(q)),
                })
            }

            CstKind::Rule {
                rule: Rule::FactPattern,
                children,
                ..
            } => {
                let mut namespace = vec![];
                let mut name = None;
                let mut key_pattern = None;
                for (_, l) in children {
                    match l {
                        Lowered::LId(s) => namespace.push(s),
                        Lowered::UId(s) => name = Some(s),
                        Lowered::Pattern(p) => key_pattern = Some(Box::new(p)),
                        _ => {}
                    }
                }
                let name = name.expect("fact pattern has a name");

                Lowered::IntermediateFact {
                    predicate_id: self
                        .schema
                        .get_predicate_id(&namespace, name)
                        .expect("fact pattern has a valid predicate"),
                    key_pattern,
                }
            }

            CstKind::Rule {
                rule: Rule::FieldList,
                children,
                ..
            } => Lowered::FieldList(
                children
                    .into_iter()
                    .filter_map(|(_, l)| match l {
                        Lowered::Field(f) => Some(f),
                        _ => None,
                    })
                    .collect(),
            ),

            CstKind::Rule {
                rule: Rule::Field,
                children,
                ..
            } => {
                let mut name = None;
                let mut value = None;
                for (_, l) in children {
                    match l {
                        Lowered::LId(s) => name = Some(s),
                        Lowered::Pattern(p) => value = Some(p),
                        _ => {}
                    }
                }
                Lowered::Field((
                    name.expect("field has a name"),
                    value.expect("field has a value pattern"),
                ))
            }

            CstKind::Token {
                token: Token::UId,
                text,
                ..
            } => Lowered::UId(self.intern(text)),
            CstKind::Token {
                token: Token::LId,
                text,
                ..
            } => Lowered::LId(self.intern(text)),
            CstKind::Token {
                token: Token::Nat,
                text,
                ..
            } => Lowered::Nat(text.parse().unwrap()),
            CstKind::Token {
                token: Token::String,
                text,
                ..
            } => Lowered::Str(self.intern(text)),

            CstKind::Rule { .. } | CstKind::Token { .. } => Lowered::Trivia,
        }
    }
}
