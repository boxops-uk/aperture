use thiserror::Error;

use crate::focus::plan::VarId;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("malformed key: {0}")]
    MalformedKey(#[from] StoreCodecError),

    #[error("var {0:?} used before it was bound")]
    UseBeforeBind(VarId),

    #[error("var {var_id:?} index out of bounds (nvars={nvars})")]
    BadSlotIndex { var_id: VarId, nvars: usize },

    #[error("advance of closed frame")]
    AdvanceAfterClose,
}

#[derive(Debug, Error)]
pub enum StoreCodecError {
    #[error("unexpected end of input")]
    UnexpectedEof,

    #[error("unexpected mark: {0:#x}")]
    UnexpectedMark(u8),

    #[error("unexpected terminator")]
    UnexpectedTerminator,

    #[error("{0}")]
    BadString(#[from] std::str::Utf8Error),

    #[error("bad integer")]
    BadInteger,

    #[error("bad record")]
    BadRecord,

    #[error("integer overflow")]
    Overflow,

    #[error("integer underflow")]
    Underflow,
}
