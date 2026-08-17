//! **The bottom of the [capacity ladder](../docs/phase-10-capacity.md): the executor,
//! the compiler and the store, each measured alone.**
//!
//! ```text
//! cargo run --release --example engine -- --store /path/to/db/<name>/<ulid>
//! cargo run --release --example engine -- --store … --layer store
//! cargo run --release --example engine -- --store … --only scan --iterations 5
//! ```
//!
//! `loadgen` measures the whole round trip and `breakdown` takes its fixed cost apart.
//! Both stop at the socket. This one starts under it: an in-process [`FjallDb`], a plan
//! compiled once *outside* the loop, and [`Executor::enumerate_profiled`] driven
//! directly — no tokio, no wire, no session, no server. Whatever it reports is the
//! floor everything above it is paying on top of.
//!
//! # It runs against a real index, and that is the point
//!
//! `--store` names a **fjall instance directory** — the ULID under a database, the
//! thing that actually holds `keyspaces/`. Point it at a `.NET` checkout indexed by
//! `clients/dotnet/Aperture.Indexer`, because uniform synthetic rows flatter seeks and
//! understate cache pressure, and because the question this ladder exists to answer is
//! about a corpus somebody would really keep.
//!
//! **The server must not be holding that directory** — fjall takes its own lock, and
//! `ops-I1` means one process owns a root. Copy the instance directory and point at the
//! copy; a copy taken while a server is up is torn, so stop it for the copy.
//!
//! # Every number is checked against what it claims to have measured
//!
//! A throughput figure for a query that did something other than what you think it did
//! is worse than no figure. So each workload is run once **unmeasured** to establish
//! its row count and its per-step examined counts, and every timed run afterwards must
//! reproduce both exactly — a mismatch aborts with the discrepancy rather than printing
//! a rate. That is [`docs/testing.md`][testing]'s rule about vacuous passes, applied to
//! a measurement instead of an assertion, and it is why the catalogue leads with a
//! query that examines exactly zero rows: an instrument that reports no work for a real
//! query is broken, not fast.
//!
//! Pivots — the file path a seek seeks, the name `SearchByName` searches for — are
//! **sampled from the store**, never computed. `loadgen` can say `files / 2` because it
//! wrote the corpus itself. Against somebody's checkout there is no such arithmetic,
//! and a seek for a key that is not there measures a miss.
//!
//! # Read the shapes, not the microseconds
//!
//! Same caveat as `breakdown`: absolute numbers belong to the box that produced them.
//! What travels is ns/row against predicate size, the ratio between examined and
//! produced, and what paging costs as a fraction of the run it interrupts.
//!
//! [testing]: ../docs/testing.md

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use aperture_cli::{
    code_index,
    workload::{Pivots, Workload, catalogue},
};
use aperture_encoding::tuple::Value;
use aperture_engine::{
    compile::Compilation,
    iter::{Cursor, Executor, Iteratee, Profile, Stream},
    plan::{Plan, SeekKey, Source, Step, Test},
};
use aperture_schema::schema::{LocalInterner, PredicateId, Schema};
use aperture_store::{fact_store::FactStore, store::FjallDb};

/// What the server pages at (`session.rs`), and therefore the interval this file
/// suspends at when it is measuring what paging costs. Stated as its own constant
/// because the number being *the server's* is the whole reason the comparison means
/// anything.
const CHUNK_ROWS: u64 = 256;

/// How many rows the paging comparison runs over, per arm.
///
/// Both arms are bounded to the same count, so the difference between them is the
/// suspend/resume machinery and nothing else. Bounded at all because the alternative is
/// paying for 8.6M rows twice to learn a per-page number that 100k rows already says.
const PAGING_ROWS: u64 = 100_000;

fn main() {
    let options = match parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("engine: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let store = match resolve_instance(&options.store) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("engine: {message}");
            std::process::exit(2);
        }
    };

    let schema = Arc::new(code_index::schema());
    let db = match FjallDb::open(&store) {
        Ok(db) => db,
        Err(error) => {
            eprintln!(
                "engine: {} did not open: {error}\n  \
                 a server holding this root keeps its own lock on it — stop it, or measure a copy",
                store.display()
            );
            std::process::exit(1);
        }
    };

    println!("store    {}", store.display());
    println!("host     {}", host());

    match options.layer {
        Layer::Executor => {
            let pivots = sample(&db, &schema);
            println!("{}", describe(&pivots));
            executor(&db, &schema, &options, &pivots);
        }
        Layer::Compile => {
            let pivots = sample(&db, &schema);
            compile_layer(&schema, &options, &pivots);
        }
        Layer::Encode => {
            let pivots = sample(&db, &schema);
            encode_layer(&db, &schema, &options, &pivots);
        }
        Layer::Store => store_layer(&db, &schema, &options),
    }
}

const USAGE: &str = "\
usage: cargo run --release --example engine -- --store PATH [options]

  --store PATH         a fjall instance directory (…/<database>/<ulid>), or a store
                       root holding exactly one database
  --layer LAYER        executor (default) | encode | compile | store
  --iterations N       timed runs per workload, after one unmeasured probe (default 3)
  --only SUBSTR        run only the workloads whose name contains SUBSTR
  --no-paging          skip the suspend/resume comparison (S1's F7)
  --paging-rows N      rows per arm of that comparison (default 100000)
";

struct Options {
    store: PathBuf,
    layer: Layer,
    iterations: usize,
    only: Option<String>,
    paging: bool,
    paging_rows: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Executor,
    Encode,
    Compile,
    Store,
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        store: PathBuf::new(),
        layer: Layer::Executor,
        iterations: 3,
        only: None,
        paging: true,
        paging_rows: PAGING_ROWS,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));

        match arg.as_str() {
            "--store" => options.store = PathBuf::from(value()?),
            "--layer" => {
                options.layer = match value()?.as_str() {
                    "executor" => Layer::Executor,
                    "encode" => Layer::Encode,
                    "compile" => Layer::Compile,
                    "store" => Layer::Store,
                    other => return Err(format!("no such layer: {other}")),
                }
            }
            "--iterations" => {
                options.iterations = value()?
                    .parse()
                    .map_err(|_| "--iterations wants a number")?;
            }
            "--only" => options.only = Some(value()?),
            "--no-paging" => options.paging = false,
            "--paging-rows" => {
                options.paging_rows = value()?
                    .parse()
                    .map_err(|_| "--paging-rows wants a number")?;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if options.store.as_os_str().is_empty() {
        return Err("--store is required".to_owned());
    }
    if options.iterations == 0 {
        return Err("--iterations must be at least 1".to_owned());
    }

    Ok(options)
}

/// Accept either the instance directory or the store root above it.
///
/// A store root is `<root>/<database>/<ulid>` ([`ops-I7`][ops], the filesystem as the
/// catalogue), and the fjall keyspace is the deepest of those. Handing `FjallDb::open` a
/// root would stamp a *new, empty* database inside it and report zero facts for
/// everything — a plausible-looking run measuring nothing, which is exactly the failure
/// this ladder is supposed to make impossible.
///
/// [ops]: ../docs/aperture-cli-design.md
fn resolve_instance(path: &Path) -> Result<PathBuf, String> {
    if path.join("keyspaces").is_dir() {
        return Ok(path.to_path_buf());
    }

    let mut found: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for database in entries.flatten() {
            if let Ok(instances) = std::fs::read_dir(database.path()) {
                for instance in instances.flatten() {
                    if instance.path().join("keyspaces").is_dir() {
                        found.push(instance.path());
                    }
                }
            }
        }
    }

    match found.len() {
        1 => {
            let instance = found.remove(0);
            println!("note     resolved to the one instance under that root");
            Ok(instance)
        }
        0 => Err(format!(
            "{} holds no fjall instance — point --store at …/<database>/<ulid>",
            path.display()
        )),
        n => Err(format!(
            "{} holds {n} instances — name the one to measure",
            path.display()
        )),
    }
}

// ---- the catalogue ------------------------------------------------------------------

/// The values a workload seeks for, taken out of the corpus that is loaded.
/// Sample rather than compute: against somebody's checkout there is no arithmetic that
/// lands on a key which exists.
///
/// The shape and the queries live in [`aperture_cli::workload`] — S0 — so that this
/// bench, the load generator and the soak ask the same questions of the same data.
/// *Where the values come from* stays here, because an in-process bench has a `FjallDb`
/// and a load generator has a socket, and those are not the same act.
fn sample(db: &FjallDb, schema: &Schema) -> Pivots {
    let file = sample_str(db, schema, "F where src.File F", 16_000);
    let decl = sample_str(db, schema, "N where src.Decl {name = N}", 400_000);
    let search = sample_str(db, schema, "N where src.SearchByName {name = N}", 400_000);

    match (file, decl) {
        // A corpus with neither a file nor a declaration is not one to measure, and
        // pivots invented here would make every seek workload answer zero rows and look
        // fast. `unsampled` makes that loud instead.
        (None, None) => Pivots::unsampled(),
        (file, decl) => {
            let decl = decl.unwrap_or_else(|| "\u{0}none".to_owned());
            let search = search.unwrap_or_else(|| decl.clone());
            Pivots::new(file.unwrap_or_else(|| "\u{0}none".to_owned()), decl, search)
        }
    }
}

/// What the run printed, so a reader can see which keys the numbers below are about.
fn describe(pivots: &Pivots) -> String {
    format!(
        "pivots   file      {}\n         directory {}\n         decl      {}\n         search    {}",
        pivots.file, pivots.directory, pivots.decl, pivots.search
    )
}

/// Run `focus` and return the string at `index`, or the last row if the scan is shorter.
///
/// Stops there rather than draining: a pivot from row 400,000 of `src.Decl` should cost
/// 400,000 rows, not 888,292.
fn sample_str(db: &FjallDb, schema: &Schema, focus: &str, index: u64) -> Option<String> {
    let plan = compiled(focus, schema);
    let interner = LocalInterner::new(schema.interner().clone());
    let cancel = CancellationToken::new();
    let mut profile = Profile::for_plan(&plan);

    let out = Executor::new(db.reader(), plan)
        .enumerate_profiled(
            (None, 0u64),
            |(last, n), mut row| {
                let value = row.to_value(&interner)?;
                let n = n + 1;
                let last = first_str(&value).or(last);

                if n >= index {
                    Ok(Stream::Suspend((last, n)))
                } else {
                    Ok(Stream::Continue((last, n)))
                }
            },
            &cancel,
            &mut profile,
        )
        .expect("a sampling scan runs");

    match out {
        Iteratee::Done((last, _)) | Iteratee::Suspended((last, _), _) => last,
    }
}

/// The first string anywhere in a projected row, so one sampler serves a bare binding
/// and a record alike.
fn first_str(value: &Value) -> Option<String> {
    match value {
        Value::Str(s) => Some(s.clone()),
        Value::Record(fields) => fields.iter().find_map(|(_, v)| first_str(v)),
        _ => None,
    }
}

// ---- S1: the executor ---------------------------------------------------------------

fn executor(db: &FjallDb, schema: &Schema, options: &Options, pivots: &Pivots) {
    println!(
        "\nS1 — the executor alone: {} timed run(s) after one unmeasured probe\n",
        options.iterations
    );

    let mut rows_out = vec![row(&[
        "workload",
        "rows",
        "examined",
        "per row",
        "rows/s",
        "best",
        "plan",
        "exercises",
    ])];

    let mut paging: Vec<Vec<String>> = Vec::new();

    for workload in catalogue(pivots) {
        if !options.selects(workload.name) {
            continue;
        }

        let plan = compiled(&workload.focus, schema);
        let stop_at = workload.stop_at.unwrap_or(u64::MAX);
        let probe = run(db, &plan, stop_at);

        let mut best = Duration::MAX;
        for _ in 0..options.iterations {
            let started = Instant::now();
            let measured = run(db, &plan, stop_at);
            let elapsed = started.elapsed();

            // The self-check. Two runs of one plan over one immutable store must agree
            // on both counts; if they do not, the number about to be printed describes
            // neither run.
            if measured.rows != probe.rows || measured.examined != probe.examined {
                eprintln!(
                    "engine: `{}` did not reproduce its probe\n  \
                     probe    {} rows, examined {:?}\n  \
                     measured {} rows, examined {:?}",
                    workload.name, probe.rows, probe.examined, measured.rows, measured.examined
                );
                std::process::exit(1);
            }

            best = best.min(elapsed);
        }

        let examined: u64 = probe.examined.iter().sum();
        let counted = if workload.stop_at.is_some() {
            format!("{} (capped)", thousands(probe.rows))
        } else {
            thousands(probe.rows)
        };

        rows_out.push(row(&[
            workload.name,
            &counted,
            &thousands(examined),
            &per_row(best, probe.rows),
            &thousands(rate(probe.rows, best)),
            &duration(best),
            &plan_shape(&plan, schema),
            workload.about,
        ]));

        if options.paging && workload.stop_at.is_none() && probe.rows >= 2 * CHUNK_ROWS {
            paging.push(page_comparison(db, &plan, &workload, options));
        }
    }

    print!("{}", table(&rows_out));
    println!("\n  `examined` is per-step and summed: what the scan pulled, matched or not.");
    println!("  `per row` and `rows/s` count rows *produced*, so a filtering workload");
    println!("  reports the cost of the answer rather than the cost of the search.");

    if !paging.is_empty() {
        let mut out = vec![row(&[
            "workload",
            "rows",
            "straight",
            "paged",
            "overhead",
            "pages",
            "per page",
            "· snapshot",
            "· resume",
            "· rows",
        ])];
        out.extend(paging);

        println!(
            "\nF7 — what paging costs: the same plan straight through, against the same \n\
             plan suspended every {CHUNK_ROWS} rows and resumed from a bytes-only cursor,\n\
             which is what the server does on every query.\n"
        );
        print!("{}", table(&out));
        println!(
            "\n  The three dotted columns are the paged arm taken apart, per page: a fresh\n  \
             snapshot, the rebuilt executor replaying one seek per level, and the rows —\n  \
             which the straight arm pays too. They sum to the paged total, so whatever\n  \
             `overhead` is, one of them has to hold it.\n\n  \
             A resume's replayed seeks are deliberately uncounted by `Profile`, so both\n  \
             arms report identical `examined` figures: the work paging adds is invisible\n  \
             to a profile by construction."
        );
    }
}

/// One run's outcome — the two things a timed run must reproduce.
struct Run {
    rows: u64,
    examined: Vec<u64>,
}

/// Drive `plan` to `limit` rows, counting only.
///
/// No projection: `to_value` decodes and allocates at the escape boundary, which is a
/// cost of *reading a row out*, not of finding it. S4 is where that lands, beside the
/// framing it exists to feed.
fn run(db: &FjallDb, plan: &Plan, limit: u64) -> Run {
    let cancel = CancellationToken::new();
    let mut profile = Profile::for_plan(plan);

    let out = Executor::new(db.reader(), plan.clone())
        .enumerate_profiled(
            0u64,
            |n, _row| {
                let n = n + 1;
                if n >= limit {
                    Ok(Stream::Suspend(n))
                } else {
                    Ok(Stream::Continue(n))
                }
            },
            &cancel,
            &mut profile,
        )
        .expect("the plan runs");

    let rows = match out {
        Iteratee::Done(n) | Iteratee::Suspended(n, _) => n,
    };

    Run {
        rows: std::hint::black_box(rows),
        examined: profile.examined,
    }
}

/// Drive `plan` to `limit` rows **suspending every `CHUNK_ROWS`**, rebuilding the
/// executor from the cursor each time.
///
/// The store is re-read per page on purpose: `db.reader()` takes a fresh snapshot, which
/// is [`I8`][i8] — the snapshot is released at suspend, so a paged run pays for a new one
/// per page and that cost belongs in this number.
///
/// [i8]: ../docs/invariants.md#i8
fn run_paged(db: &FjallDb, plan: &Plan, limit: u64) -> (Run, Paging) {
    let cancel = CancellationToken::new();
    let mut profile = Profile::for_plan(plan);
    let mut cursor: Option<Cursor> = None;
    let mut total = 0u64;
    let mut paging = Paging::default();

    loop {
        // Timed apart, because "paging is slow" is not an answer anyone can act on.
        // A page is three things — a fresh snapshot, a rebuilt executor replaying one
        // seek per level, and the rows themselves — and only the first two are the
        // price of having stopped.
        let started = Instant::now();
        let store = db.reader();
        paging.snapshot += started.elapsed();

        let started = Instant::now();
        let executor = match cursor.take() {
            None => Executor::new(store, plan.clone()),
            Some(cursor) => {
                paging.resumes += 1;
                Executor::resume(store, plan.clone(), cursor).expect("the cursor resumes")
            }
        };
        paging.resume += started.elapsed();

        let started = Instant::now();
        let out = executor
            .enumerate_profiled(
                total,
                |n, _row| {
                    let n = n + 1;
                    if n >= limit || n.is_multiple_of(CHUNK_ROWS) {
                        Ok(Stream::Suspend(n))
                    } else {
                        Ok(Stream::Continue(n))
                    }
                },
                &cancel,
                &mut profile,
            )
            .expect("the plan runs");
        paging.rows += started.elapsed();

        match out {
            Iteratee::Done(n) => {
                total = n;
                break;
            }
            Iteratee::Suspended(n, next) => {
                total = n;
                if n >= limit {
                    break;
                }
                cursor = Some(next);
            }
        }
    }

    (
        Run {
            rows: std::hint::black_box(total),
            examined: profile.examined,
        },
        paging,
    )
}

/// Where a paged run's time went, summed over its pages.
#[derive(Default, Clone)]
struct Paging {
    /// `db.reader()` — a fresh immutable snapshot per page (I8).
    snapshot: Duration,
    /// `Executor::resume` — the cursor checked, then one seek replayed per level.
    resume: Duration,
    /// The rows themselves, which the straight-through arm pays too.
    rows: Duration,
    resumes: u64,
}

fn page_comparison(
    db: &FjallDb,
    plan: &Plan,
    workload: &Workload,
    options: &Options,
) -> Vec<String> {
    let limit = options.paging_rows;

    // Warm both arms once, then take the best of `iterations` each. Best rather than
    // mean because the thing being isolated is a per-page cost, and a scheduler
    // hiccup adds to one arm without belonging to it.
    let _ = run(db, plan, limit);
    let mut straight = Duration::MAX;
    for _ in 0..options.iterations {
        let started = Instant::now();
        let measured = run(db, plan, limit);
        straight = straight.min(started.elapsed());
        debug_assert_eq!(measured.rows, limit.min(measured.rows));
    }

    let _ = run_paged(db, plan, limit);
    let mut paged = Duration::MAX;
    let mut split = Paging::default();
    for _ in 0..options.iterations {
        let started = Instant::now();
        let (_measured, measured_split) = run_paged(db, plan, limit);
        let elapsed = started.elapsed();

        if elapsed < paged {
            paged = elapsed;
            split = measured_split;
        }
    }

    let pages = split.resumes.max(1);
    let per_page = |total: Duration| duration(total / u32::try_from(pages).unwrap_or(u32::MAX));
    let overhead = paged.saturating_sub(straight);

    row(&[
        workload.name,
        &thousands(limit),
        &duration(straight),
        &duration(paged),
        &format!("{:+.0}%", percent(overhead, straight)),
        &thousands(split.resumes),
        &per_page(overhead),
        &per_page(split.snapshot),
        &per_page(split.resume),
        &per_page(split.rows),
    ])
}

// ---- S4's missing half: what a row costs to hand out ---------------------------------

/// **The per-row cost the executor does not pay** — projection, wire conversion, encoding.
///
/// S1 counts rows and touches no field ([I5](../docs/invariants.md#i5) is why that is
/// cheap: a register holds the whole row and decodes lazily). The server cannot: for every
/// row it calls `to_value`, then [`rows::to_wire`], then `encode_value` into **a fresh
/// `Vec` per row** (`session.rs:863`). That is what F4 predicts dominates above ~100k
/// row/s, and it is measurable without a socket, a session or a runtime — which is the
/// only way to tell it apart from the transport it is usually bundled with.
///
/// The three calls are the server's own, in the server's order, rather than a model of
/// them: an approximation here would be measuring this file.
fn encode_layer(db: &FjallDb, schema: &Schema, options: &Options, pivots: &Pivots) {
    println!(
        "\nS4a — projecting and encoding each row, no socket: {} timed run(s)\n",
        options.iterations
    );

    let mut out = vec![row(&[
        "workload",
        "rows",
        "counted",
        "encoded",
        "per row",
        "bytes/row",
        "tax",
    ])];

    for workload in catalogue(pivots) {
        if !options.selects(workload.name) || workload.stop_at.is_some() {
            continue;
        }

        let plan = compiled(&workload.focus, schema);

        // Exactly what the server's `prepare` builds, so the type a row is encoded
        // against is the one it would really be encoded against.
        let mut compilation = Compilation::new(&workload.focus, schema);
        let _ = compilation.plan().expect("it compiled a moment ago");
        let head = compilation.head_ty().expect("a head type").clone();
        let desc = aperture_server::rows::desc_of(&head, compilation.interner())
            .expect("the head has a descriptor");
        let mut interner = compilation.into_interner();
        let ty = desc.to_ty(&mut interner);

        let counted = best_of_runs(options.iterations, || run(db, &plan, u64::MAX).rows);

        let mut bytes = 0u64;
        let encoded = best_of_runs(options.iterations, || {
            let cancel = CancellationToken::new();
            let mut buffer = Vec::new();
            bytes = 0;

            let out = Executor::new(db.reader(), plan.clone())
                .enumerate(
                    0u64,
                    |n, mut row| {
                        let value = row.to_value(&interner)?;

                        // `expect` rather than `?`: a row this build cannot hand out is a
                        // bug in the instrument or the server, not a slow path, and the
                        // closure's error type is the executor's.
                        let wire = aperture_server::rows::to_wire(&ty, &value)
                            .expect("a row converts to wire");

                        buffer.clear();
                        aperture_wire::value::encode_value(&mut buffer, schema, &ty, &wire)
                            .expect("a row encodes");

                        bytes += buffer.len() as u64;
                        std::hint::black_box(&buffer);
                        Ok(Stream::Continue(n + 1))
                    },
                    &cancel,
                )
                .expect("the plan runs");

            match out {
                Iteratee::Done(n) | Iteratee::Suspended(n, _) => n,
            }
        });

        if counted.rows == 0 {
            continue;
        }

        out.push(row(&[
            workload.name,
            &thousands(counted.rows),
            &thousands(rate(counted.rows, counted.best)),
            &thousands(rate(encoded.rows, encoded.best)),
            &per_row(encoded.best, encoded.rows),
            &(bytes / counted.rows.max(1)).to_string(),
            &format!(
                "{:.1}×",
                encoded.best.as_secs_f64() / counted.best.as_secs_f64().max(f64::MIN_POSITIVE)
            ),
        ]));
    }

    print!("{}", table(&out));
    println!(
        "\n  `counted` is S1 — rows found, no field touched. `encoded` adds exactly what the\n  \
         server adds per row: `to_value`, `to_wire`, `encode_value` into a fresh buffer.\n  \
         `tax` is the ratio, and it is the part of the wire path that is not the wire."
    );
}

/// The best of `iterations` runs of something that returns a row count.
struct Measured {
    rows: u64,
    best: Duration,
}

fn best_of_runs(iterations: usize, mut run: impl FnMut() -> u64) -> Measured {
    let mut rows = run();
    let mut best = Duration::MAX;

    for _ in 0..iterations {
        let started = Instant::now();
        rows = run();
        best = best.min(started.elapsed());
    }

    Measured { rows, best }
}

// ---- S2: the compiler ---------------------------------------------------------------

fn compile_layer(schema: &Schema, options: &Options, pivots: &Pivots) {
    let iterations = options.iterations.max(200);
    println!("\nS2 — the compiler alone: {iterations} compilations per query\n");

    let mut out = vec![row(&["query", "compile", "steps"])];

    for workload in catalogue(pivots) {
        if !options.selects(workload.name) {
            continue;
        }

        let best = best_of(iterations, || {
            let mut compilation = Compilation::new(&workload.focus, schema);
            std::hint::black_box(compilation.plan().is_some());
        });

        let plan = compiled(&workload.focus, schema);
        out.push(row(&[
            workload.name,
            &duration(best),
            &plan.body.len().to_string(),
        ]));
    }

    print!("{}", table(&out));

    // The question `breakdown` does not ask: the floor it measured is for *one* query,
    // and a plan cache is worth what a compile costs at the size people actually write.
    println!("\n  against query size — the same predicate, k conjuncts:\n");
    let mut out = vec![row(&[
        "conjuncts",
        "query bytes",
        "compile",
        "per conjunct",
    ])];

    for k in [1usize, 2, 4, 8, 16, 32] {
        let focus = generated(k);
        let best = best_of(iterations, || {
            let mut compilation = Compilation::new(&focus, schema);
            std::hint::black_box(compilation.plan().is_some());
        });

        out.push(row(&[
            &k.to_string(),
            &focus.len().to_string(),
            &duration(best),
            &duration(best / u32::try_from(k).unwrap_or(1)),
        ]));
    }

    print!("{}", table(&out));
}

/// `k` conjuncts over one predicate, each binding its own variable — a query that grows
/// without changing shape, so what moves is the compiler's cost in the number of
/// statements rather than in what they mean.
fn generated(k: usize) -> String {
    let mut focus = String::from("X0 where ");
    for i in 0..k {
        if i > 0 {
            focus.push_str("; ");
        }
        let _ = write!(focus, "src.File X{i}");
    }
    focus
}

// ---- S3: the store ------------------------------------------------------------------

fn store_layer(db: &FjallDb, schema: &Schema, options: &Options) {
    println!("\nS3 — the store alone: a full keyspace scan per predicate\n");

    let mut out = vec![row(&[
        "predicate",
        "facts",
        "key bytes",
        "per row",
        "rows/s",
        "elapsed",
        "seek",
    ])];

    let mut total_facts = 0u64;
    let mut widest: Option<(String, u64)> = None;

    for id in 0..schema.len() {
        let predicate = PredicateId(u32::try_from(id).expect("a predicate id fits"));
        let name = schema
            .get(predicate)
            .and_then(|p| p.name())
            .map_or_else(|| format!("predicate {id}"), ToOwned::to_owned);

        if !options.selects(&name) {
            continue;
        }

        let reader = db.reader();
        let started = Instant::now();
        let mut facts = 0u64;
        let mut bytes = 0u64;

        // Every SAMPLE_STRIDE'th key, kept for the seek measurement below: taken on the
        // way past rather than in a second pass, because a second pass would read them
        // back out of a cache the first one warmed.
        let mut sampled: Vec<Vec<u8>> = Vec::new();

        for entry in reader
            .scan(&predicate.0.to_be_bytes(), None)
            .expect("a predicate scan opens")
        {
            let (key, _id) = entry.expect("a row decodes");
            if facts.is_multiple_of(SAMPLE_STRIDE) && sampled.len() < SAMPLE_KEYS {
                sampled.push(key.to_vec());
            }
            facts += 1;
            bytes += key.len() as u64;
        }

        let elapsed = started.elapsed();
        total_facts += facts;

        if facts > 0 {
            let per_key = bytes / facts;
            if widest.as_ref().is_none_or(|(_, w)| per_key > *w) {
                widest = Some((name.clone(), per_key));
            }
        }

        out.push(row(&[
            &name,
            &thousands(facts),
            &thousands(bytes),
            &per_row(elapsed, facts),
            &thousands(rate(facts, elapsed)),
            &duration(elapsed),
            &seek_cost(db, &sampled),
        ]));
    }

    print!("{}", table(&out));
    println!("\n  {} facts in the index.", thousands(total_facts));
    if let Some((name, per_key)) = widest {
        println!("  widest key: {name}, {per_key} bytes/row.");
    }
    println!(
        "  `seek` opens a scan at a key sampled from the middle of the keyspace and takes\n  \
         one row — which is precisely what a resume does per page, with nothing above it."
    );

    point_reads(db, schema);
}

/// Keys kept per predicate for the seek measurement, and how far apart to take them.
///
/// Spread rather than adjacent: a hundred seeks to neighbouring keys measure one block
/// of cache, which is the one thing a resume never gets to do.
const SAMPLE_KEYS: usize = 100;
const SAMPLE_STRIDE: u64 = 4_099;

/// **The seek floor** — what it costs to open a scan at a given key and read one row.
///
/// This is [`Executor::resume`]'s per-level work with the executor taken away, so the
/// difference between this and S1's per-page `resume` column is what the machine adds
/// to what the store charges.
fn seek_cost(db: &FjallDb, keys: &[Vec<u8>]) -> String {
    if keys.is_empty() {
        return "—".to_owned();
    }

    let reader = db.reader();

    // Warm, then measure: the first pass over a hundred cold keys measures the disk it
    // read them from, which is a different question.
    for key in keys {
        let _ = reader
            .scan(key, None)
            .expect("a seek opens")
            .next()
            .transpose()
            .expect("a row decodes");
    }

    let started = Instant::now();
    let mut found = 0u64;
    for key in keys {
        if reader
            .scan(key, None)
            .expect("a seek opens")
            .next()
            .transpose()
            .expect("a row decodes")
            .is_some()
        {
            found += 1;
        }
    }
    let elapsed = started.elapsed();

    std::hint::black_box(found);
    per_row(elapsed, keys.len() as u64)
}

/// The floor under a `Source::Fetch`: one `point` per row of the level above it.
fn point_reads(db: &FjallDb, schema: &Schema) {
    // Modules, because that is what `read through reference` fetches — the same
    // predicate, so the two numbers subtract.
    let Some(module) = (0..schema.len())
        .map(|id| PredicateId(id as u32))
        .find(|id| {
            schema
                .get(*id)
                .and_then(|p| p.name())
                .is_some_and(|name| name == "src.Module")
        })
    else {
        return;
    };

    let reader = db.reader();
    let ids: Vec<_> = reader
        .scan(&module.0.to_be_bytes(), None)
        .expect("a scan opens")
        .filter_map(Result::ok)
        .map(|(_key, id)| id)
        .take(50_000)
        .collect();

    if ids.is_empty() {
        return;
    }

    let reader = db.reader();
    let started = Instant::now();
    let mut hits = 0u64;
    for id in &ids {
        if reader.point(*id).expect("a point read answers").is_some() {
            hits += 1;
        }
    }
    let elapsed = started.elapsed();

    println!(
        "\n  point reads: {} of {} hit, {} each, {}/s",
        thousands(hits),
        thousands(ids.len() as u64),
        per_row(elapsed, ids.len() as u64),
        thousands(rate(ids.len() as u64, elapsed))
    );
}

// ---- shared -------------------------------------------------------------------------

impl Options {
    fn selects(&self, name: &str) -> bool {
        self.only.as_ref().is_none_or(|only| name.contains(only))
    }
}

/// Compile, or say why not and stop.
///
/// A catalogue entry that does not compile is a broken instrument, not a slow query, so
/// it fails loudly here rather than being skipped into a table with an em dash in it.
fn compiled(focus: &str, schema: &Schema) -> Plan {
    let mut compilation = Compilation::new(focus, schema);
    match compilation.plan() {
        Some(plan) => plan,
        None => {
            eprintln!("engine: `{focus}` did not compile:");
            eprint!("{}", compilation.render_to_string());
            std::process::exit(1);
        }
    }
}

/// The plan's steps, named — the same naming the server does for `query --profile`,
/// which is deliberately re-stated here rather than imported: S1 is the rung that must
/// link no server at all.
fn plan_shape(plan: &Plan, schema: &Schema) -> String {
    let name = |id: PredicateId| {
        schema
            .get(id)
            .and_then(|p| p.name())
            .map_or_else(|| format!("predicate {}", id.0), ToOwned::to_owned)
    };

    let mut parts = Vec::new();
    for step in &plan.body {
        match step {
            Step::Level(level) => {
                for source in &level.sources {
                    match source {
                        Source::Seek { access, .. } => {
                            let full = match &access.seek_key {
                                SeekKey::Prefix(bytes) => bytes.is_empty(),
                                SeekKey::Composite(parts) => parts.is_empty(),
                            };
                            parts.push(if full {
                                format!("{}*", name(access.predicate_id))
                            } else {
                                name(access.predicate_id)
                            });
                        }
                        Source::Fetch { predicate_id, .. } => {
                            parts.push(format!("fetch {}", name(*predicate_id)));
                        }
                    }
                }
            }
            Step::Derive(_) => parts.push("derive".to_owned()),
            Step::Test(Test::Compare { .. }) => parts.push("compare".to_owned()),
            Step::Test(Test::Absent(_)) => parts.push("!".to_owned()),
        }
    }

    if parts.is_empty() {
        "no steps".to_owned()
    } else {
        parts.join(" → ")
    }
}

fn best_of(iterations: usize, mut f: impl FnMut()) -> Duration {
    for _ in 0..iterations / 10 {
        f();
    }

    let mut best = Duration::MAX;
    for _ in 0..iterations {
        let started = Instant::now();
        f();
        best = best.min(started.elapsed());
    }
    best
}

fn host() -> String {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();

    let kernel = read("/proc/sys/kernel/osrelease").trim().to_owned();
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let memory = read("/proc/meminfo")
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemTotal:")
                .map(str::trim)
                .map(str::to_owned)
        })
        .unwrap_or_default();

    format!("{cores} cores, {memory}, kernel {kernel}, release build")
}

fn rate(count: u64, over: Duration) -> u64 {
    let seconds = over.as_secs_f64();
    if seconds <= 0.0 {
        return 0;
    }
    (count as f64 / seconds) as u64
}

fn per_row(elapsed: Duration, rows: u64) -> String {
    if rows == 0 {
        return "—".to_owned();
    }

    let nanos = elapsed.as_nanos() as f64 / rows as f64;
    if nanos >= 1_000_000.0 {
        format!("{:.1} ms", nanos / 1_000_000.0)
    } else if nanos >= 1_000.0 {
        format!("{:.1} µs", nanos / 1_000.0)
    } else {
        format!("{nanos:.0} ns")
    }
}

fn percent(part: Duration, whole: Duration) -> f64 {
    if whole.is_zero() {
        return 0.0;
    }
    100.0 * part.as_secs_f64() / whole.as_secs_f64()
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds >= 1.0 {
        format!("{seconds:.2} s")
    } else if seconds >= 0.001 {
        format!("{:.2} ms", seconds * 1_000.0)
    } else {
        format!("{:.1} µs", seconds * 1_000_000.0)
    }
}

fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|cell| (*cell).to_owned()).collect()
}

/// Column-aligned, header row first — `loadgen`'s renderer, kept local because an
/// example cannot import another example.
fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            let pad = widths[index] - cell.chars().count();
            let _ = write!(out, "  {cell}{:pad$}", "");
        }
        out.push('\n');
    }
    out
}
