use codespan_reporting::diagnostic::Diagnostic;
use string_interner::DefaultStringInterner as StringInterner;

use crate::lens::{location::Location, schema::Schema};

pub trait IntoDiagnostic<FileId> {
    fn into_diagnostic(
        &self,
        location: Location<FileId>,
        interner: &StringInterner,
        schema: &Schema,
    ) -> Diagnostic<FileId>;
}

pub enum Diag<FileId = ()> {
    Rendered(Diagnostic<FileId>),
    Unrendered {
        location: Location<FileId>,
        error: Box<dyn IntoDiagnostic<FileId>>,
    },
}

impl<FileId: Copy> Diag<FileId> {
    pub fn unrendered(
        location: Location<FileId>,
        error: impl IntoDiagnostic<FileId> + 'static,
    ) -> Self {
        Diag::Unrendered {
            location,
            error: Box::new(error),
        }
    }

    pub fn render(&self, interner: &StringInterner, schema: &Schema) -> Diagnostic<FileId> {
        match self {
            Diag::Rendered(d) => d.clone(),
            Diag::Unrendered { location, error } => {
                error.into_diagnostic(*location, interner, schema)
            }
        }
    }
}
