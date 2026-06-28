use std::sync::Arc;

use aperture::focus::schema::{Predicate, PredicateTy, Schema};
use lasso::Rodeo;

fn main() {
    let mut schema_rodeo = Rodeo::new();

    let edge_predicate = Predicate {
        name: schema_rodeo.get_or_intern("Edge"),
        key: PredicateTy::Record(Arc::from(vec![
            (schema_rodeo.get_or_intern("from"), PredicateTy::Int),
            (schema_rodeo.get_or_intern("to"), PredicateTy::Int),
        ])),
        value: None,
    };

    let has_name_predicate = Predicate {
        name: schema_rodeo.get_or_intern("HasName"),
        key: PredicateTy::Record(Arc::from(vec![
            (schema_rodeo.get_or_intern("id"), PredicateTy::Int),
            (schema_rodeo.get_or_intern("name"), PredicateTy::Str),
        ])),
        value: None,
    };

    let schema_reader = schema_rodeo.into_reader();

    let schema = Schema::new(
        schema_reader,
        Arc::from(vec![edge_predicate, has_name_predicate]),
    );

    print!("{:#?}", schema);
}
