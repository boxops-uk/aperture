use lasso::Spur;
use thiserror::Error;

use crate::focus::{iter::Address, schema::Symbol};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("decode error: {0}")]
    DecodeError(#[from] StoreCodecError),

    #[error("variable at address 0x{0:016x} used before it was bound")]
    UseBeforeBind(Address),

    #[error("address 0x{0:016x} out of bounds")]
    AddressOutOfBounds(Address),

    #[error("advance of closed frame")]
    AdvanceAfterClose,

    #[error("resume key not found")]
    BadResumeKey,

    #[error("operation cancelled")]
    Cancelled,

    #[error("unknown symbol: {0:?}")]
    UnknownSymbol(Symbol),
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
