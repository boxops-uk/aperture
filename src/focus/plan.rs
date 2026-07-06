use byteview::ByteView;
use lasso::Spur;

use crate::focus::{
    error::StoreError,
    iter::Address,
    schema::{PredicateId, PredicateTy, Symbol},
    transport::Value,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarId(pub u32);

#[derive(Debug)]
pub enum SeekKey {
    Prefix(Box<[u8]>),
    Composite(Box<[SeekKeyPart]>),
}

#[derive(Debug)]
pub enum SeekKeyPart {
    Bytes(Box<[u8]>),
    RegisterField { address: Address, field_idx: usize },
}

#[derive(Debug)]
pub struct Access {
    pub predicate_id: PredicateId,
    pub seek_key: SeekKey,
}

#[derive(Debug)]
pub enum ResidualOp {
    EqConst(Box<[u8]>),
    Prefix(Box<[u8]>),
    EqRegisterField { address: Address, field_idx: usize },
}

#[derive(Debug)]
pub struct Residual {
    pub field_idx: usize,
    pub op: ResidualOp,
}

#[derive(Debug)]
pub struct Generator {
    pub access: Access,
    pub binds: Box<[Address]>,
    pub residuals: Box<[Residual]>,
}

#[derive(Debug)]
pub enum Project {
    Lit(Value),
    RegisterField {
        address: Address,
        field_idx: usize,
        ty: PredicateTy,
    },
    FactRef(Address),
    Value {
        address: Address,
        ty: PredicateTy,
    },
    Record(Box<[(Symbol, Project)]>),
}

pub struct Plan {
    pub nvars: usize,
    pub body: Box<[Generator]>,
    pub head: Project,
}

#[derive(Debug)]
pub struct Entity {
    pub key: ByteView,
    pub value: ByteView,
}

pub trait FactStore {
    type Scan: Iterator<Item = Result<(ByteView, FactId), StoreError>>;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Self::Scan;

    fn point(&self, id: FactId) -> Result<Option<Entity>, StoreError>;
}
