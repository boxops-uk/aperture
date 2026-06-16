use crate::lens::{location::Location, schema::PredicateId};
use im::HashMap;
use string_interner::DefaultSymbol as Symbol;

#[derive(Debug)]
pub struct Query<FileId = ()> {
    pub location: Location<FileId>,
    pub head: Pattern<FileId>,
    pub body: Box<[Statement<FileId>]>,
}

#[derive(Debug)]
pub struct Pattern<FileId = ()> {
    pub location: Location<FileId>,
    pub kind: PatternKind<Pattern<FileId>, FileId>,
}

#[derive(Debug)]
pub enum Statement<FileId = ()> {
    Bind {
        left: Pattern<FileId>,
        right: Pattern<FileId>,
    },
    ImplicitBind(Pattern<FileId>),
}

#[derive(Debug)]
pub enum PatternKind<T, FileId = ()> {
    Wildcard,
    Int(i64),
    String(Symbol),
    StringPrefix(Symbol),
    Var(Symbol),
    Subquery(Box<Query<FileId>>),
    Record {
        field_patterns: HashMap<Symbol, T>,
    },
    Fact {
        predicate_id: PredicateId,
        key_pattern: Box<[T]>,
    },
}
