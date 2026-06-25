use std::ops::Range;

#[derive(Clone, Copy)]
pub struct PredicateId(u32);

#[derive(Clone)]
pub enum Value {
    Int(i64),
    String(String),
    Prefix(String),
}

pub enum GroundPatternKind<T> {
    Never,
    Wildcard,
    Value(Value),
    Var(String),
    Record {
        field_patterns: im_rc::HashMap<String, T>,
    },
}

impl<T> GroundPatternKind<T> {
    pub fn map<U: Clone, F: FnMut(&T) -> U>(&self, mut f: F) -> GroundPatternKind<U> {
        match self {
            GroundPatternKind::Never => GroundPatternKind::Never,
            GroundPatternKind::Wildcard => GroundPatternKind::Wildcard,
            GroundPatternKind::Value(value) => GroundPatternKind::Value(value.clone()),
            GroundPatternKind::Var(name) => GroundPatternKind::Var(name.clone()),
            GroundPatternKind::Record { field_patterns } => GroundPatternKind::Record {
                field_patterns: field_patterns
                    .iter()
                    .map(|(field, pattern)| (field.clone(), f(pattern)))
                    .collect(),
            },
        }
    }
}

pub enum GeneratorPatternKind<T> {
    Fact {
        predicate_id: PredicateId,
        key_pattern: Box<T>,
    },
}

impl<T> GeneratorPatternKind<T> {
    pub fn map<U: Clone, F: FnMut(&T) -> U>(&self, mut f: F) -> GeneratorPatternKind<U> {
        match self {
            GeneratorPatternKind::Fact {
                predicate_id,
                key_pattern,
            } => GeneratorPatternKind::Fact {
                predicate_id: *predicate_id,
                key_pattern: Box::new(f(key_pattern)),
            },
        }
    }
}

pub enum PatternKind<T> {
    Ground(GroundPatternKind<T>),
    Generator(GeneratorPatternKind<T>),
}

impl<T> PatternKind<T> {
    pub fn map<U: Clone, F: FnMut(&T) -> U>(&self, mut f: F) -> PatternKind<U> {
        match self {
            PatternKind::Ground(ground) => PatternKind::Ground(ground.map(&mut f)),
            PatternKind::Generator(generator) => PatternKind::Generator(generator.map(&mut f)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternId(u32);

#[derive(Default)]
pub struct PatternIdGen {
    next_id: u32,
}

impl PatternIdGen {
    pub fn next(&mut self) -> PatternId {
        let id = self.next_id;
        self.next_id += 1;
        PatternId(id)
    }
}

pub struct Location<FileId: Clone + PartialEq = ()> {
    pub file_id: FileId,
    pub span: Range<usize>,
}

impl From<Range<usize>> for Location<()> {
    fn from(span: Range<usize>) -> Self {
        Location { file_id: (), span }
    }
}

pub struct Located<T, FileId: Clone + PartialEq = ()> {
    pub value: T,
    pub location: Location<FileId>,
}

pub trait IsPattern {
    type Kind;

    fn id(&self) -> PatternId;

    fn kind(&self) -> &Self::Kind;
}

impl<K> IsPattern for Pattern<K> {
    type Kind = K;

    fn id(&self) -> PatternId {
        self.id
    }

    fn kind(&self) -> &Self::Kind {
        &self.kind
    }
}

impl<K> IsPattern for Located<Pattern<K>> {
    type Kind = K;

    fn id(&self) -> PatternId {
        self.value.id
    }

    fn kind(&self) -> &Self::Kind {
        &self.value.kind
    }
}

pub struct Pattern<K> {
    id: PatternId,
    kind: K,
}

pub enum StatementKind<K> {
    Bind { left: K, right: K },
    ImplicitBind(K),
}

pub struct Statement<K> {
    pub kind: StatementKind<K>,
}

pub struct Query<K> {
    pub head: Pattern<K>,
    pub body: Box<[Statement<K>]>,
}

pub fn is_ground<P>(pattern: &P) -> bool
where
    P: IsPattern<Kind = PatternKind<P>>,
{
    match pattern.kind() {
        PatternKind::Ground(ground) => match ground {
            GroundPatternKind::Never => true,
            GroundPatternKind::Wildcard => true,
            GroundPatternKind::Value(_) => true,
            GroundPatternKind::Var(_) => false,
            GroundPatternKind::Record { field_patterns } => {
                field_patterns.values().all(|child| is_ground(child))
            }
        },
        PatternKind::Generator(_) => false,
    }
}

pub fn reduce<P, R, F>(pattern: &P, f: F) -> R
where
    P: IsPattern<Kind = PatternKind<P>>,
    F: Fn(PatternId, PatternKind<R>) -> R,
    R: Clone,
{
    reduce_go(pattern, &f)
}

fn reduce_go<P, R, F>(pattern: &P, f: &F) -> R
where
    P: IsPattern<Kind = PatternKind<P>>,
    F: Fn(PatternId, PatternKind<R>) -> R,
    R: Clone,
{
    let folded_kind = pattern.kind().map(|child| reduce_go(child, f));
    f(pattern.id(), folded_kind)
}

pub struct AstPattern<FileId: Clone + PartialEq = ()>(
    pub Located<Pattern<PatternKind<AstPattern<FileId>>>, FileId>,
);

impl<FileId: Clone + PartialEq> AstPattern<FileId> {
    pub fn new(
        id: PatternId,
        kind: PatternKind<AstPattern<FileId>>,
        location: Location<FileId>,
    ) -> Self {
        AstPattern(Located {
            value: Pattern { id, kind },
            location,
        })
    }

    pub fn location(&self) -> &Location<FileId> {
        &self.0.location
    }
}

impl<FileId: Clone + PartialEq> IsPattern for AstPattern<FileId> {
    type Kind = PatternKind<AstPattern<FileId>>;

    fn id(&self) -> PatternId {
        self.0.value.id
    }

    fn kind(&self) -> &Self::Kind {
        &self.0.value.kind
    }
}
