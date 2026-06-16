#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl From<std::ops::Range<usize>> for Span {
    fn from(range: std::ops::Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

impl Into<std::ops::Range<usize>> for Span {
    fn into(self) -> std::ops::Range<usize> {
        self.start..self.end
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Location<FileId = ()> {
    pub file_id: FileId,
    pub span: Span,
}

impl From<std::ops::Range<usize>> for Location {
    fn from(range: std::ops::Range<usize>) -> Self {
        Self {
            file_id: (),
            span: range.into(),
        }
    }
}

impl From<Span> for Location {
    fn from(span: Span) -> Self {
        Self { file_id: (), span }
    }
}

impl<FileId> Location<FileId> {
    pub fn new(file_id: FileId, span: impl Into<Span>) -> Self {
        Self {
            file_id,
            span: span.into(),
        }
    }
}
