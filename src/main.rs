use aperture::lens::{
    location::Span,
    query::{Pattern, PatternKind, Query, Statement},
    schema::Schema,
    ty::{Ty, TyChecker, TyDisplay, infer_query},
};
use codespan_reporting::{
    files::SimpleFile,
    term::{
        self, Config,
        termcolor::{ColorChoice, StandardStream},
    },
};
use im::HashMap;
use string_interner::DefaultStringInterner as StringInterner;

fn main() {
    let mut schema = Schema::new();
    let mut string_interner: StringInterner = StringInterner::new();

    let baa_name = string_interner.get_or_intern("Baa");
    let field_y = string_interner.get_or_intern("y");
    let baa_key_ty = Ty::Record {
        field_tys: HashMap::new().update(field_y, Ty::String),
    };
    schema.insert_predicate_ty(baa_name, baa_key_ty.clone(), Ty::Never);

    let foo_name = string_interner.get_or_intern("Foo");
    let field_x = string_interner.get_or_intern("x");
    let field_z = string_interner.get_or_intern("z");
    let foo_key_ty = Ty::Record {
        field_tys: HashMap::new()
            .update(field_x, baa_key_ty)
            .update(field_z, Ty::Int),
    };
    let foo_id = schema.insert_predicate_ty(foo_name, foo_key_ty, Ty::Never);

    // Y where test.Foo { x = { y = Y }, z = 42 }
    // 0         1         2         3         4
    // 0123456789012345678901234567890123456789012
    let source = "Y where test.Foo { x = { y = Y }, z = 42 }";

    let var_y = string_interner.get_or_intern("Y");

    let mut ty_checker: TyChecker = TyChecker::new();

    let query: Query = Query {
        location: Span { start: 0, end: 42 }.into(),
        head: Pattern {
            location: Span { start: 0, end: 2 }.into(),
            kind: PatternKind::Var(var_y),
        },
        body: vec![Statement::ImplicitBind(Pattern {
            location: Span { start: 8, end: 42 }.into(),
            kind: PatternKind::Fact {
                predicate_id: foo_id,
                key_pattern: Box::new(Pattern {
                    location: Span { start: 17, end: 42 }.into(),
                    kind: PatternKind::Record {
                        field_patterns: HashMap::new()
                            .update(
                                field_x,
                                Pattern {
                                    location: Span { start: 23, end: 32 }.into(),
                                    kind: PatternKind::Record {
                                        field_patterns: HashMap::new().update(
                                            field_y,
                                            Pattern {
                                                location: Span { start: 29, end: 30 }.into(),
                                                kind: PatternKind::Var(var_y),
                                            },
                                        ),
                                    },
                                },
                            )
                            .update(
                                field_z,
                                Pattern {
                                    location: Span { start: 38, end: 40 }.into(),
                                    kind: PatternKind::String(
                                        string_interner.get_or_intern("42").into(),
                                    ),
                                },
                            ),
                    },
                }),
            },
        })]
        .into_boxed_slice(),
    };

    let inferred_ty = infer_query(&mut ty_checker, &schema, &query);
    let query_ty = ty_checker.zonk(&inferred_ty);
    println!(
        "query type: {}",
        TyDisplay {
            string_interner: &string_interner,
            schema: &schema,
            ty: query_ty,
        }
    );

    let writer = StandardStream::stderr(ColorChoice::Auto);
    let config = Config::default();
    let file = SimpleFile::new("<query>", &source);
    for diagnostic in ty_checker.diagnostics(&string_interner, &schema) {
        term::emit_to_write_style(&mut writer.lock(), &config, &file, &diagnostic).unwrap();
    }
}
