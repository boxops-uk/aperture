use im_rc::HashMap;
use string_interner::DefaultStringInterner as StringInterner;
use string_interner::DefaultSymbol as Symbol;

use codespan_reporting::diagnostic::{Diagnostic, Label};

use crate::lens::cst::CstKind;
use crate::lens::cst::CstNode;
use crate::lens::diag::{Diag, IntoDiagnostic};
use crate::lens::lexer::Token;
use crate::lens::location::Location;
use crate::lens::parser::Rule;
use crate::lens::query::NodeIdGen;
use crate::lens::query::PatternKind;
use crate::lens::query::{Pattern, Query, Statement};
use crate::lens::schema::PredicateId;
use crate::lens::schema::Schema;

#[derive(Debug)]
pub enum LowerError {
    UnknownPredicate {
        namespace: Box<[Symbol]>,
        name: Symbol,
    },
}

impl<FileId: Copy> IntoDiagnostic<FileId> for LowerError {
    fn into_diagnostic(
        &self,
        location: Location<FileId>,
        interner: &StringInterner,
        _schema: &Schema,
    ) -> Diagnostic<FileId> {
        match self {
            LowerError::UnknownPredicate { namespace, name } => {
                let name_str = interner.resolve(*name).unwrap_or("?");
                let full_name = if namespace.is_empty() {
                    name_str.to_owned()
                } else {
                    let ns = namespace
                        .iter()
                        .map(|s| interner.resolve(*s).unwrap_or("?"))
                        .collect::<Vec<_>>()
                        .join("::");
                    format!("{}::{}", ns, name_str)
                };
                Diagnostic::error()
                    .with_message(format!("unknown predicate '{}'", full_name))
                    .with_labels(vec![
                        Label::primary(location.file_id, location.span)
                            .with_message(format!("'{}' is not defined in the schema", full_name)),
                    ])
            }
        }
    }
}

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
    Error,
}

pub struct LensLowering<'a, FileId> {
    node_id_gen: &'a mut NodeIdGen,
    interner: &'a mut StringInterner,
    schema: &'a Schema,
    file_id: FileId,
    errors: Vec<Diag<FileId>>,
}

impl<'a, FileId> LensLowering<'a, FileId>
where
    FileId: Copy + std::fmt::Debug + 'static,
{
    pub fn new(
        node_id_gen: &'a mut NodeIdGen,
        interner: &'a mut StringInterner,
        schema: &'a Schema,
        file_id: FileId,
    ) -> Self {
        Self {
            node_id_gen,
            interner,
            schema,
            file_id,
            errors: vec![],
        }
    }

    pub fn lower<'s>(&mut self, cst: &'s CstNode<'s>) -> Query<FileId> {
        let Lowered::Query(q) = cst.para(&mut |kind| self.algebra(kind)) else {
            unreachable!()
        };
        q
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn drain_into(&mut self, diags: &mut Vec<Diag<FileId>>) {
        diags.extend(self.errors.drain(..));
    }

    fn intern(&mut self, s: &str) -> Symbol {
        self.interner.get_or_intern(s)
    }

    fn loc(&self, span: super::parser::Span) -> Location<FileId> {
        Location::new(self.file_id, span)
    }

    fn pattern(
        &mut self,
        span: super::parser::Span,
        kind: PatternKind<Pattern<FileId>, FileId>,
    ) -> Pattern<FileId> {
        Pattern {
            id: self.node_id_gen.next(),
            location: self.loc(span),
            kind,
        }
    }

    fn algebra<'s>(
        &mut self,
        kind: CstKind<'s, (CstNode<'s>, Lowered<FileId>)>,
    ) -> Lowered<FileId> {
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
            } => Lowered::Pattern(self.pattern(span, PatternKind::Wildcard)),

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
                Lowered::Pattern(self.pattern(span, PatternKind::Var(sym)))
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
                Lowered::Pattern(self.pattern(span, PatternKind::Int(n.into())))
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
                Lowered::Pattern(self.pattern(span, PatternKind::Int(-(n as i64))))
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
                Lowered::Pattern(self.pattern(span, PatternKind::String(s)))
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
                Lowered::Pattern(self.pattern(span, PatternKind::StringPrefix(s)))
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
                Lowered::Pattern(self.pattern(span, PatternKind::Record { field_patterns }))
            }

            CstKind::Rule {
                rule: Rule::FactApattern,
                span,
                children,
            } => {
                let mut intermediate = None;
                let mut has_lower_error = false;
                for (_, l) in children {
                    match l {
                        Lowered::IntermediateFact {
                            predicate_id,
                            key_pattern,
                        } => intermediate = Some((predicate_id, key_pattern)),
                        Lowered::Error => has_lower_error = true,
                        _ => {}
                    }
                }

                if has_lower_error && intermediate.is_none() {
                    return Lowered::Pattern(self.pattern(span, PatternKind::Error));
                }

                let (predicate_id, key_pattern) =
                    intermediate.expect("fact apattern has an intermediate fact");

                let key_pattern = key_pattern.unwrap_or_else(|| {
                    Box::new(self.pattern(
                        span.clone(),
                        PatternKind::Record {
                            field_patterns: HashMap::new(),
                        },
                    ))
                });

                Lowered::Pattern(self.pattern(
                    span,
                    PatternKind::Fact {
                        predicate_id,
                        key_pattern,
                    },
                ))
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
                Lowered::Pattern(self.pattern(span, PatternKind::Subquery(Box::new(q))))
            }

            CstKind::Rule {
                rule: Rule::FactPattern,
                span,
                children,
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
                let namespace: Box<[Symbol]> = namespace.into();

                match self.schema.get_predicate_id(&namespace, name) {
                    Some(predicate_id) => Lowered::IntermediateFact {
                        predicate_id,
                        key_pattern,
                    },
                    None => {
                        self.errors.push(Diag::unrendered(
                            self.loc(span),
                            LowerError::UnknownPredicate { namespace, name },
                        ));
                        Lowered::Error
                    }
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
