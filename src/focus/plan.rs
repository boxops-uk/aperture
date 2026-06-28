use byteview::ByteView;

use crate::focus::{error::StoreError, schema::PredicateId, transport::OutValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FactId(pub(crate) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarId(pub(crate) u32);

#[derive(Debug, Clone, Copy)]
pub enum ScalarTy {
    Int,
    Str,
}

#[derive(Debug)]
pub enum SeekKey {
    Prefix(Box<[u8]>),
    Composite(Box<[SeekKeyPart]>),
}

#[derive(Debug)]
pub enum SeekKeyPart {
    Bytes(Box<[u8]>),
    SlotField { var_id: VarId, field_idx: usize },
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
    EqSlotField { var_id: VarId, field_idx: usize },
}

#[derive(Debug)]
pub struct Residual {
    pub field_idx: usize,
    pub op: ResidualOp,
}

#[derive(Debug)]
pub struct Generator {
    pub access: Access,
    pub binds: Box<[VarId]>,
    pub residuals: Box<[Residual]>,
}

#[derive(Debug)]
pub enum Project {
    Lit(OutValue),
    SlotField {
        var_id: VarId,
        field_idx: usize,
        ty: ScalarTy,
    },
    FactId(VarId),
    Value(VarId),
    Record(Box<[Project]>),
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

pub trait Store {
    type Scan: Iterator<Item = Result<(ByteView, FactId), StoreError>>;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Self::Scan;

    fn point(&self, id: FactId) -> Result<Option<Entity>, StoreError>;
}
