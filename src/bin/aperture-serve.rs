//! `aperture-serve` — a server over a Unix socket, for a client in any language.
//!
//! What [operations §5 `serve`](../../docs/aperture-cli-design.md) will become, at the
//! size Phase 7a needs: bind a socket, open a store, serve the wire protocol. The
//! command tree, configuration layering and the rest of §5 are Phase 9's; this is a
//! binary so that a non-Rust producer has something to connect to, which is the only
//! way to find out whether the protocol is actually implementable from the outside.
//!
//! ```text
//! aperture-serve --socket /tmp/aperture.sock --data-dir /tmp/aperture-db [--ready-file F]
//! ```
//!
//! **The schema is hardcoded, and it has to be.** Schemas are not parsed until
//! [Phase 8](../../PLAN.md) and a database does not carry one until
//! [I13](../../docs/invariants.md#i13) lands with it, so somebody has to write this
//! down in Rust. It is the code index the example indexer emits
//! (`example/README.md`) and the shell serves, because a demo of "an indexer writes
//! facts" wants a schema an indexer would actually produce. When Phase 8 lands, this
//! function is deleted rather than ported.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_server::{Database, protocol, serve_unix};
use aperture_store::store::FjallDb;
use lasso::Rodeo;

/// Predicate ids **are** positions in the schema, so the ones pointed at have to be
/// named before the vector that defines them.
const FILE: PredicateId = PredicateId(0);
const MODULE: PredicateId = PredicateId(1);

/// The code index: files, modules that name a file, declarations that name a module.
///
/// Field lists are sorted by name, as everywhere — a record's field order is part of
/// its encoding ([chapter 6](../../docs/06-types-and-schema.md)).
///
/// ```text
/// predicate src.File   : string
/// predicate src.Module : { file : src.File, name : string }
/// predicate src.Decl   : { kind : string, line : int, module : src.Module, name : string }
/// ```
fn code_index() -> Schema {
    let mut rodeo = Rodeo::new();
    let mut name = |text: &str| rodeo.get_or_intern(text);

    let (file, module, decl) = (name("src.File"), name("src.Module"), name("src.Decl"));
    let (f_file, f_kind, f_line, f_module, f_name) = (
        name("file"),
        name("kind"),
        name("line"),
        name("module"),
        name("name"),
    );

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![
            Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: module,
                key: PredicateTy::Record(
                    vec![
                        (f_file, PredicateTy::Fact(FILE)),
                        (f_name, PredicateTy::Str),
                    ]
                    .into(),
                ),
                value: None,
            },
            Predicate {
                name: decl,
                key: PredicateTy::Record(
                    vec![
                        (f_kind, PredicateTy::Str),
                        (f_line, PredicateTy::Int),
                        (f_module, PredicateTy::Fact(MODULE)),
                        (f_name, PredicateTy::Str),
                    ]
                    .into(),
                ),
                value: None,
            },
        ]),
    )
}

struct Args {
    socket: PathBuf,
    data_dir: PathBuf,
    ready_file: Option<PathBuf>,
    database: String,
}

fn parse() -> Result<Args, String> {
    let mut socket = PathBuf::from("/tmp/aperture.sock");
    let mut data_dir = PathBuf::from("/tmp/aperture-db");
    let mut ready_file = None;
    let mut database = "code".to_owned();

    let mut args = std::env::args().skip(1);

    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("`{flag}` needs a value"));

        match flag.as_str() {
            "--socket" => socket = PathBuf::from(value()?),
            "--data-dir" => data_dir = PathBuf::from(value()?),
            "--ready-file" => ready_file = Some(PathBuf::from(value()?)),
            "--database" => database = value()?,
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown flag `{other}`")),
        }
    }

    Ok(Args {
        socket,
        data_dir,
        ready_file,
        database,
    })
}

const USAGE: &str = "\
aperture-serve — serve a database over the wire protocol

    --socket PATH       where to bind          (default /tmp/aperture.sock)
    --data-dir PATH     the store root         (default /tmp/aperture-db)
    --database NAME     the name clients ask for  (default `code`)
    --ready-file PATH   written once the listener is accepting
";

fn main() -> ExitCode {
    let args = match parse() {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("aperture-serve: {message}\n");
            }
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let schema = code_index();
    let fingerprint = protocol::provisional_fingerprint(&schema);

    let db = match FjallDb::open(&args.data_dir) {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "aperture-serve: cannot open {}: {error}",
                args.data_dir.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let database = Arc::new(Database::new(args.database.clone(), db, schema));

    // Printed rather than only computed: a client that wants the handshake to *check*
    // the schema has to be told what to expect, and until Phase 8 there is no schema
    // file to read it out of.
    println!("aperture-serve");
    println!("  socket       {}", args.socket.display());
    println!("  data dir     {}", args.data_dir.display());
    println!("  database     {}", args.database);
    println!("  protocol     {}", protocol::VERSION);
    println!("  schema       {fingerprint:#018x}  (provisional — see PLAN Phase 8)");

    if let Err(error) = serve_unix(&args.socket, args.ready_file.as_deref(), vec![database]) {
        eprintln!("aperture-serve: {error}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
