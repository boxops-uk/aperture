use std::fmt;
use std::sync::Arc;

use itertools::Itertools;
use lasso::{Rodeo, RodeoReader, Spur};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredicateId(pub(crate) u32);

pub const PREDICATE_ID_SIZE: usize = std::mem::size_of::<u32>();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Symbol {
    Schema(Spur),
    Local(Spur),
}

#[derive(Debug, Clone)]
pub enum PredicateTy {
    Int,
    Str,
    Fact(PredicateId),
    Record(Arc<[(Spur, PredicateTy)]>),
}

#[derive(Debug, Clone)]
pub struct Predicate {
    pub name: Spur,
    pub key: PredicateTy,
    pub value: Option<PredicateTy>,
}

pub struct PredicateTyRef<'a> {
    interner: &'a SchemaInterner,
    pub ty: &'a PredicateTy,
}

impl<'a> PredicateTyRef<'a> {
    pub fn find_field(&self, name: &str) -> Option<(usize, PredicateTyRef<'a>)> {
        let PredicateTy::Record(fields) = self.ty else {
            return None;
        };
        let spur = self.interner.get_spur(name)?;
        fields
            .iter()
            .enumerate()
            .find(|(_, (s, _))| *s == spur)
            .map(|(i, (_, ty))| {
                (
                    i,
                    PredicateTyRef {
                        interner: self.interner,
                        ty,
                    },
                )
            })
    }
}

pub struct PredicateRef<'a> {
    interner: &'a SchemaInterner,
    inner: &'a Predicate,
}

impl<'a> PredicateRef<'a> {
    pub fn name(&self) -> &str {
        self.interner
            .0
            .try_resolve(&self.inner.name)
            .unwrap_or_default()
    }

    pub fn key(&self) -> PredicateTyRef<'a> {
        PredicateTyRef {
            interner: self.interner,
            ty: &self.inner.key,
        }
    }

    pub fn value(&self) -> Option<PredicateTyRef<'a>> {
        self.inner.value.as_ref().map(|ty| PredicateTyRef {
            interner: self.interner,
            ty,
        })
    }

    pub fn predicate(&self) -> &'a Predicate {
        self.inner
    }
}

#[derive(Clone)]
pub struct SchemaInterner(Arc<RodeoReader>);

impl SchemaInterner {
    pub fn new(reader: RodeoReader) -> Self {
        SchemaInterner(Arc::new(reader))
    }

    pub fn get(&self, s: &str) -> Option<Symbol> {
        self.0.get(s).map(Symbol::Schema)
    }

    fn get_spur(&self, s: &str) -> Option<Spur> {
        self.0.get(s)
    }

    pub fn try_resolve(&self, symbol: Symbol) -> Option<&str> {
        match symbol {
            Symbol::Schema(spur) => self.0.try_resolve(&spur),
            Symbol::Local(_) => None,
        }
    }
}

pub struct LocalInterner {
    schema: SchemaInterner,
    local: Rodeo,
}

impl LocalInterner {
    pub fn new(schema: SchemaInterner) -> Self {
        LocalInterner {
            schema,
            local: Rodeo::new(),
        }
    }

    pub fn schema(&self) -> &SchemaInterner {
        &self.schema
    }

    pub fn get(&self, s: &str) -> Option<Symbol> {
        if let Some(symbol) = self.schema.get(s) {
            return Some(symbol);
        }
        self.local.get(s).map(Symbol::Local)
    }

    pub fn get_or_intern(&mut self, s: &str) -> Symbol {
        if let Some(symbol) = self.schema.get(s) {
            return symbol;
        }
        Symbol::Local(self.local.get_or_intern(s))
    }

    pub fn try_resolve(&self, symbol: Symbol) -> Option<&str> {
        match symbol {
            Symbol::Schema(spur) => self.schema.0.try_resolve(&spur),
            Symbol::Local(spur) => self.local.try_resolve(&spur),
        }
    }
}

#[derive(Clone)]
pub struct Schema {
    interner: SchemaInterner,
    predicates: Arc<[Predicate]>,
}

impl Schema {
    pub fn new(reader: RodeoReader, predicates: Arc<[Predicate]>) -> Self {
        Schema {
            interner: SchemaInterner::new(reader),
            predicates,
        }
    }

    pub fn interner(&self) -> &SchemaInterner {
        &self.interner
    }

    pub fn get(&self, id: PredicateId) -> Option<PredicateRef<'_>> {
        self.predicates
            .get(id.0 as usize)
            .map(|inner| PredicateRef {
                interner: &self.interner,
                inner,
            })
    }

    pub fn find_position(&self, name: &str) -> Option<(PredicateId, PredicateRef<'_>)> {
        let spur = self.interner.get_spur(name)?;
        let (idx, inner) = self.predicates.iter().find_position(|p| p.name == spur)?;
        Some((
            PredicateId(idx as u32),
            PredicateRef {
                interner: &self.interner,
                inner,
            },
        ))
    }
}

struct PredicateTyDebug<'a> {
    interner: &'a SchemaInterner,
    ty: &'a PredicateTy,
}

impl fmt::Debug for PredicateTyDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            PredicateTy::Int => write!(f, "int"),
            PredicateTy::Str => write!(f, "str"),
            PredicateTy::Fact(id) => write!(f, "fact({})", id.0),
            PredicateTy::Record(fields) => {
                let mut ds = f.debug_struct("");
                for (spur, ty) in fields.iter() {
                    let name = self.interner.0.try_resolve(spur).unwrap_or("<unknown>");
                    ds.field(
                        name,
                        &PredicateTyDebug {
                            interner: self.interner,
                            ty,
                        },
                    );
                }
                ds.finish()
            }
        }
    }
}

struct PredicateDebug<'a> {
    interner: &'a SchemaInterner,
    predicate: &'a Predicate,
}

impl fmt::Debug for PredicateDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("");

        s.field(
            "key",
            &PredicateTyDebug {
                interner: self.interner,
                ty: &self.predicate.key,
            },
        );

        if let Some(value) = &self.predicate.value {
            s.field(
                "value",
                &PredicateTyDebug {
                    interner: self.interner,
                    ty: value,
                },
            );
        }

        s.finish()
    }
}

impl fmt::Debug for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ds = f.debug_struct("Schema");
        for predicate in self.predicates.iter() {
            let name = self
                .interner
                .0
                .try_resolve(&predicate.name)
                .unwrap_or("<unknown>");
            ds.field(
                name,
                &PredicateDebug {
                    interner: &self.interner,
                    predicate,
                },
            );
        }
        ds.finish()
    }
}
