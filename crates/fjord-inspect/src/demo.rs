//! **The database the site queries** — the schema, the facts, and a `MemStore`
//! holding them.
//!
//! Small on purpose, and *complete* on purpose: every shape sigla has appears
//! exactly once in [`SCHEMA`], and the facts are sized so that the queries a
//! reader tries answer **two or three rows** rather than one. A single-row
//! answer shows nothing about backtracking, which is most of what there is to
//! watch.
//!
//! **The facts are written the way any fact is written**, through
//! [`fjord_store::fact::encode`]: named fields, resolved and reordered against
//! the schema, one encoder. Hand-encoding a key to reach a store is the
//! anti-pattern `AGENTS.md` names — three preconditions fail silently — and the
//! fact that this store is a model rather than a database does not make the
//! bytes any less real.
//!
//! Sequences are chosen here rather than allocated, which is what lets a
//! reference name a target that does not exist yet: `Decl #4` can name
//! `File #2` because both numbers are decided before either fact is written.
//! That is the fixture's discipline, and the reason a reference is a `FactId`
//! and not a name.

use fjord_encoding::tuple::Value;
use fjord_schema::{
    id::FactId,
    schema::{PredicateId, Schema},
};
use fjord_store::{
    error::FactError,
    fact::{Fact, ToValue, record},
};
use fjord_store_mem::MemStore;

/// The schema the site opens with, as text.
///
/// A real file, so `fjord create demo --schema schemas/demo.sigla` builds the
/// same database outside the browser.
pub const SCHEMA: &str = include_str!("../../../schemas/demo.sigla");

/// Which predicate a demo fact belongs to, by the sequence numbers below.
mod at {
    /// `code.File` — three under `src/`, one that a `"src/"..` prefix excludes.
    pub const MAIN_RS: u64 = 1;
    pub const LIB_RS: u64 = 2;
    pub const UTIL_RS: u64 = 3;
    pub const GUIDE_MD: u64 = 4;

    /// `code.Decl` — two files with two or three declarations, and one with none.
    pub const MAIN: u64 = 1;
    pub const RUN: u64 = 2;
    pub const CONFIG: u64 = 3;
    pub const LOAD: u64 = 4;
    pub const ERROR: u64 = 5;
    pub const SLUG: u64 = 6;
    pub const PARSE: u64 = 7;
}

struct File(&'static str);

impl Fact for File {
    const PREDICATE: &'static str = "code.File";
    fn key(&self) -> Value {
        self.0.to_value()
    }
}

struct Decl {
    file: FactId,
    name: &'static str,
    line: i64,
    signature: &'static str,
}

impl Fact for Decl {
    const PREDICATE: &'static str = "code.Decl";
    fn key(&self) -> Value {
        record([
            ("file", self.file.to_value()),
            ("name", self.name.to_value()),
            ("line", self.line.to_value()),
        ])
    }
    fn value(&self) -> Option<Value> {
        Some(self.signature.to_value())
    }
}

struct Ref {
    from: FactId,
    to: FactId,
}

impl Fact for Ref {
    const PREDICATE: &'static str = "code.Ref";
    fn key(&self) -> Value {
        record([("from", self.from.to_value()), ("to", self.to.to_value())])
    }
}

struct Span {
    decl: FactId,
    line: i64,
    col: i64,
}

impl Fact for Span {
    const PREDICATE: &'static str = "code.Span";
    fn key(&self) -> Value {
        record([
            ("decl", self.decl.to_value()),
            (
                "at",
                record([("line", self.line.to_value()), ("col", self.col.to_value())]),
            ),
        ])
    }
}

/// What a declaration is: a function of some arity, or a data type of some
/// flavour. Written as the one-field record a union alternative is.
#[derive(Clone, Copy)]
enum What {
    Func(i64),
    Data(&'static str),
}

impl ToValue for What {
    /// A `Value::Union`, not a one-field record.
    ///
    /// The one-field record is how a *query* names an alternative; a fact says
    /// which alternative it is, and the discriminant is filled in by
    /// [`fact::encode`](fjord_store::fact::encode) from the schema — which is
    /// the point of writing facts through it rather than by hand, since a
    /// discriminant written here would be a number this file could get wrong.
    fn to_value(&self) -> Value {
        let (alt, payload) = match self {
            What::Func(arity) => ("func", arity.to_value()),
            What::Data(flavour) => ("data", flavour.to_value()),
        };

        Value::Union {
            disc: 0,
            alt: alt.to_owned(),
            value: Box::new(payload),
        }
    }
}

struct Kind {
    decl: FactId,
    what: What,
}

impl Fact for Kind {
    const PREDICATE: &'static str = "code.Kind";
    fn key(&self) -> Value {
        record([
            ("decl", self.decl.to_value()),
            ("what", self.what.to_value()),
        ])
    }
}

/// The same fact the other way round, so the tag leads the key.
struct KindOf {
    what: What,
    decl: FactId,
}

impl Fact for KindOf {
    const PREDICATE: &'static str = "code.KindOf";
    fn key(&self) -> Value {
        record([
            ("what", self.what.to_value()),
            ("decl", self.decl.to_value()),
        ])
    }
}

/// A store holding the demo database, built against `schema`.
///
/// # Errors
///
/// A fact that does not fit the schema — which can only happen if
/// [`SCHEMA`] and the facts below disagree, and is a bug here rather than a
/// caller's mistake.
pub fn store(schema: &Schema) -> Result<MemStore, FactError> {
    let mut store = MemStore::new();
    type Encoded = Result<(PredicateId, Vec<u8>, Vec<u8>), fjord_store::error::FactError>;
    let mut write = |predicate: &str, sequence: u64, encoded: Encoded| -> Result<(), FactError> {
        let (id, key, value) = encoded?;
        debug_assert_eq!(
            schema.find_position(predicate).map(|(id, _)| id),
            Some(id),
            "a demo fact was written under the wrong predicate"
        );
        store.insert_valued(id, key, sequence, value);
        Ok(())
    };

    let file = |sequence| fact(schema, "code.File", sequence);
    let decl = |sequence| fact(schema, "code.Decl", sequence);

    for (sequence, path) in [
        (at::MAIN_RS, "src/main.rs"),
        (at::LIB_RS, "src/lib.rs"),
        (at::UTIL_RS, "src/util.rs"),
        (at::GUIDE_MD, "docs/guide.md"),
    ] {
        write("code.File", sequence, encode(schema, &File(path)))?;
    }

    // Two declarations in `main.rs`, three in `lib.rs`, two in `util.rs` — and
    // none at all in `docs/guide.md`, so a join over files has an outer row
    // whose inner loop finds nothing and backs straight out.
    for (sequence, file_seq, name, line, signature) in [
        (at::MAIN, at::MAIN_RS, "main", 1, "fn main()"),
        (at::RUN, at::MAIN_RS, "run", 12, "fn run(cfg: Config)"),
        (at::CONFIG, at::LIB_RS, "Config", 3, "struct Config"),
        (
            at::LOAD,
            at::LIB_RS,
            "load",
            20,
            "fn load(path: &str) -> Config",
        ),
        (at::ERROR, at::LIB_RS, "Error", 40, "enum Error"),
        (
            at::SLUG,
            at::UTIL_RS,
            "slug",
            5,
            "fn slug(s: &str) -> String",
        ),
        (at::PARSE, at::UTIL_RS, "parse", 18, "fn parse(s: &str)"),
    ] {
        write(
            "code.Decl",
            sequence,
            encode(
                schema,
                &Decl {
                    file: file(file_seq),
                    name,
                    line,
                    signature,
                },
            ),
        )?;
    }

    // `main` and `Error` are referenced by nothing, so a negation has two rows
    // to be true about; `Config` is referenced twice, so a join through `to`
    // returns more than one.
    for (sequence, from, to) in [
        (1, at::MAIN, at::RUN),
        (2, at::MAIN, at::LOAD),
        (3, at::RUN, at::CONFIG),
        (4, at::LOAD, at::CONFIG),
        (5, at::LOAD, at::PARSE),
        (6, at::PARSE, at::SLUG),
    ] {
        write(
            "code.Ref",
            sequence,
            encode(
                schema,
                &Ref {
                    from: decl(from),
                    to: decl(to),
                },
            ),
        )?;
    }

    // Five of the seven declarations have a span, so a join over them misses
    // twice.
    for (sequence, decl_seq, line, col) in [
        (1, at::MAIN, 1, 0),
        (2, at::RUN, 12, 4),
        (3, at::CONFIG, 3, 0),
        (4, at::LOAD, 20, 4),
        (5, at::PARSE, 18, 4),
    ] {
        write(
            "code.Span",
            sequence,
            encode(
                schema,
                &Span {
                    decl: decl(decl_seq),
                    line,
                    col,
                },
            ),
        )?;
    }

    // Three functions of arity 1 and two data types, so either alternative
    // answers more than one row — and the two key orders hold the same facts,
    // which is what makes the seek and the residual comparable.
    let kinds = [
        (at::MAIN, What::Func(0)),
        (at::RUN, What::Func(1)),
        (at::CONFIG, What::Data("struct")),
        (at::LOAD, What::Func(2)),
        (at::ERROR, What::Data("enum")),
        (at::SLUG, What::Func(1)),
        (at::PARSE, What::Func(1)),
    ];

    for (sequence, (decl_seq, what)) in kinds.iter().enumerate() {
        let sequence = sequence as u64 + 1;
        write(
            "code.Kind",
            sequence,
            encode(
                schema,
                &Kind {
                    decl: decl(*decl_seq),
                    what: *what,
                },
            ),
        )?;
        write(
            "code.KindOf",
            sequence,
            encode(
                schema,
                &KindOf {
                    what: *what,
                    decl: decl(*decl_seq),
                },
            ),
        )?;
    }

    Ok(store)
}

/// The id of the `sequence`th fact of `predicate`.
///
/// Sequences are decided here rather than allocated, so a reference can name a
/// target written later — which is what a store with an allocator would not
/// allow and a model does.
fn fact(schema: &Schema, predicate: &str, sequence: u64) -> FactId {
    let id = schema
        .find_position(predicate)
        .map_or(PredicateId(0), |(id, _)| id);
    FactId::new(id, sequence).expect("a demo fact id is in range")
}

fn encode<F: Fact>(
    schema: &Schema,
    fact: &F,
) -> Result<(PredicateId, Vec<u8>, Vec<u8>), FactError> {
    fjord_store::fact::encode(schema, fact)
}
