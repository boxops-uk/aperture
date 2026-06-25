use crate::lens::{location::Location, schema::PredicateId};
use im_rc::HashMap;
use string_interner::DefaultSymbol as Symbol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(usize);

#[derive(Debug, Default)]
pub struct NodeIdGen {
    next_id: usize,
}

impl NodeIdGen {
    pub fn next(&mut self) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        NodeId(id)
    }
}

#[derive(Debug, Clone)]
pub struct Query<FileId = ()> {
    pub location: Location<FileId>,
    pub head: Pattern<FileId>,
    pub body: Box<[Statement<FileId>]>,
}

#[derive(Debug, Clone)]
pub struct Pattern<FileId = ()> {
    pub id: NodeId,
    pub location: Location<FileId>,
    pub kind: PatternKind<Pattern<FileId>, FileId>,
}

#[derive(Debug, Clone)]
pub enum Statement<FileId = ()> {
    Bind {
        left: Pattern<FileId>,
        right: Pattern<FileId>,
    },
    ImplicitBind(Pattern<FileId>),
}

#[derive(Debug, Clone)]
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
        key_pattern: Box<T>,
    },
    Error,
}
