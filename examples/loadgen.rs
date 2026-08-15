//! **A load generator for the server**, driving it over a real socket through
//! `aperture-client`.
//!
//! Not part of the command tree ([operations §4](../docs/aperture-cli-design.md) has no
//! `bench`), and deliberately so: this is a measuring instrument, not a thing anyone
//! should find while looking for how to use the database. It lives here rather than in
//! `aperture-client` because it needs the built-in schema, and there is exactly one
//! statement of that ([`code_index`](aperture_cli::code_index)) — a bench that declared
//! its own would eventually measure a database it could not have written.
//!
//! ```text
//! cargo run --release --example loadgen -- --data-dir /tmp/apbench --files 20000
//! ```
//!
//! It starts nothing: point it at a running server. `scripts/bench.sh` is the whole
//! sequence — create, serve, seed, measure — if you want it in one command.
//!
//! # What it is measuring, and what it is not
//!
//! Every number here is **end to end over a socket**: compile, plan, execute, encode,
//! frame, and decode on this side. That is the number that matters for "is the server
//! fast enough", and it is *not* an executor microbenchmark — the engine's own guards
//! ([I5](../docs/invariants.md#i5), [I6](../docs/invariants.md#i6),
//! [I9](../docs/invariants.md#i9)) cover that ground, and cover it better, because they
//! assert shapes rather than time.
//!
//! Rows are counted and dropped rather than rendered. Rendering is the client's cost,
//! and a throughput number that included it would be measuring this file.

use std::{
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use aperture_cli::code_index;
use aperture_client::{Connection, Mode, WireFact, WireRef, WireValue};
use aperture_schema::schema::{PredicateId, Schema};

/// `src.File`, `src.Module`, `src.Decl` — positions in the built-in schema.
const FILE: PredicateId = PredicateId(0);
const MODULE: PredicateId = PredicateId(1);
const DECL: PredicateId = PredicateId(2);

struct Options {
    socket: PathBuf,
    database: String,
    files: usize,
    decls_per_file: usize,
    connections: usize,
    runs: usize,
    seed: bool,
    block: usize,
}

fn main() {
    let options = match parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("loadgen: {message}");
            eprintln!();
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let schema = Arc::new(code_index::schema());

    if options.seed {
        seed(&options, &schema);
    }

    measure(&options, &schema);
}

const USAGE: &str = "\
usage: loadgen [options]

  --socket PATH        where the server is listening (default <data-dir>/aperture.sock)
  --data-dir PATH      derives the socket path, as the CLI does
  --database NAME      default `code`
  --files N            files to write when seeding (default 10000)
  --decls-per-file K   declarations per file (default 5)
  --block N            facts per block on the wire (default 1000)
  --connections C      concurrent connections for the query phase (default 8)
  --runs R             query executions per workload, spread over the connections
  --no-seed            measure an existing database rather than writing one";

fn parse() -> Result<Options, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let mut data_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut database = "code".to_owned();
    let mut files = 10_000;
    let mut decls_per_file = 5;
    let mut connections = 8;
    let mut runs = 200;
    let mut block = 1000;
    let mut seed = true;

    let mut at = 0;
    while at < argv.len() {
        let flag = argv[at].as_str();

        let mut value = || -> Result<String, String> {
            at += 1;
            argv.get(at)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };

        match flag {
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--database" => database = value()?,
            "--files" => files = value()?.parse().map_err(|_| "--files takes a number")?,
            "--decls-per-file" => {
                decls_per_file = value()?
                    .parse()
                    .map_err(|_| "--decls-per-file takes a number")?;
            }
            "--connections" => {
                connections = value()?
                    .parse()
                    .map_err(|_| "--connections takes a number")?;
            }
            "--runs" => runs = value()?.parse().map_err(|_| "--runs takes a number")?,
            "--block" => block = value()?.parse().map_err(|_| "--block takes a number")?,
            "--no-seed" => seed = false,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag `{other}`")),
        }

        at += 1;
    }

    let socket = socket
        .or_else(|| data_dir.map(|dir| dir.join("aperture.sock")))
        .ok_or("one of --socket or --data-dir is needed")?;

    Ok(Options {
        socket,
        database,
        files,
        decls_per_file,
        connections: connections.max(1),
        runs: runs.max(1),
        seed,
        block: block.max(1),
    })
}

// ---- facts -------------------------------------------------------------------

fn file(index: usize) -> WireFact {
    WireFact {
        predicate: FILE,
        key: WireValue::Str(format!("src/f{index:07}.py")),
        value: None,
    }
}

fn module(index: usize) -> WireFact {
    WireFact {
        predicate: MODULE,
        key: WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Nested(Box::new(file(index)))),
            WireValue::Str(format!("m{index:07}")),
        ])),
        value: None,
    }
}

/// Fields in the schema's sorted order — line, module, name — and the kind on the
/// value side.
///
/// Every declaration nests its module, which nests its file, so the server is doing
/// two levels of **interning** per fact: look the key up, write it if absent. That is
/// the write path a real indexer produces, and it is the reason ingest throughput here
/// is not simply "bytes divided by time".
fn decl(file_index: usize, n: usize) -> WireFact {
    WireFact {
        predicate: DECL,
        key: WireValue::Record(Box::from([
            WireValue::Int((n * 17 + 1) as i64),
            WireValue::Ref(WireRef::Nested(Box::new(module(file_index)))),
            WireValue::Str(format!("symbol_{file_index:07}_{n:03}")),
        ])),
        value: Some(WireValue::Str(
            if n.is_multiple_of(3) { "class" } else { "def" }.to_owned(),
        )),
    }
}

// ---- seeding -----------------------------------------------------------------

fn seed(options: &Options, schema: &Arc<Schema>) {
    let mut connection = connect(options, schema, Mode::ReadWrite);

    let total = options.files * options.decls_per_file;
    println!(
        "seeding {} declarations over {} files, {} facts per block",
        thousands(total as u64),
        thousands(options.files as u64),
        thousands(options.block as u64)
    );

    let started = Instant::now();
    let mut created = 0u64;
    let mut deduped = 0u64;
    let mut batch = Vec::with_capacity(options.block);

    for index in 0..options.files {
        for n in 0..options.decls_per_file {
            batch.push(decl(index, n));

            if batch.len() >= options.block {
                let written = connection.write(DECL, &batch).expect("a block is written");
                created += written.created;
                deduped += written.deduped;
                batch.clear();
            }
        }
    }

    if !batch.is_empty() {
        let written = connection.write(DECL, &batch).expect("a block is written");
        created += written.created;
        deduped += written.deduped;
    }

    let elapsed = started.elapsed();

    // Facts *touched* rather than sent: a declaration nesting a module nesting a file
    // is three facts on the first visit and one on every later one, so `created +
    // deduped` is the work the server actually did.
    let touched = created + deduped;

    println!(
        "  {} created, {} deduped in {} — {} facts/s touched, {} decls/s",
        thousands(created),
        thousands(deduped),
        duration(elapsed),
        thousands(rate(touched, elapsed)),
        thousands(rate(total as u64, elapsed))
    );
    println!();
}

// ---- measuring ---------------------------------------------------------------

struct Workload {
    name: &'static str,
    focus: String,
}

fn workloads(options: &Options) -> Vec<Workload> {
    // Chosen from the middle of the range so a seek has somewhere to seek past.
    let middle = format!("src/f{:07}", options.files / 2);
    let one_file = format!("symbol_{:07}", options.files / 2);

    vec![
        Workload {
            name: "scan files",
            focus: "F where src.File F".to_owned(),
        },
        Workload {
            name: "scan decls",
            focus: "N where src.Decl {name = N}".to_owned(),
        },
        // A **constant fold**, not a filter: `F = "…"` substitutes at every use, so
        // `src.File F` becomes an exact key seek and the head is the constant. One
        // point read, whatever the database holds — which is the number that says
        // whether the index is doing its job.
        Workload {
            name: "seek one file",
            focus: format!("F where src.File F; F = \"{middle}.py\""),
        },
        Workload {
            name: "seek prefix",
            focus: format!("F where src.File F; F = \"{middle}\".."),
        },
        Workload {
            name: "project record",
            focus: "{at = D.line, what = D.name} where D = src.Decl _".to_owned(),
        },
        Workload {
            name: "follow reference",
            focus: "{what = D.name, file = D.module.file} where D = src.Decl _".to_owned(),
        },
        // Deliberately denies almost nothing: paired against `scan decls`, the
        // difference is what a residual costs per row rather than what skipping rows
        // saves. A denial is never a seek — that is the point of the two polarities
        // living in separate collections.
        Workload {
            name: "denial",
            focus: format!("N where src.Decl {{name = N}}; N != \"{one_file}\".."),
        },
    ]
}

fn measure(options: &Options, schema: &Arc<Schema>) {
    println!(
        "measuring: {} connections, {} runs per workload",
        options.connections, options.runs
    );
    println!();

    let mut rows_out = vec![];

    for workload in workloads(options) {
        let Some(result) = run_workload(options, schema, &workload) else {
            rows_out.push(vec![
                workload.name.to_owned(),
                "—".to_owned(),
                "did not compile".to_owned(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ]);
            continue;
        };

        rows_out.push(vec![
            workload.name.to_owned(),
            thousands(result.rows),
            duration(result.percentile(50)),
            duration(result.percentile(95)),
            duration(result.percentile(99)),
            duration(result.max()),
            thousands(rate(result.runs as u64, result.wall)),
            thousands(rate(result.rows * result.runs as u64, result.wall)),
        ]);
    }

    print!(
        "{}",
        table(
            &[
                "workload", "rows", "p50", "p95", "p99", "max", "query/s", "row/s"
            ],
            &rows_out
        )
    );
}

struct Measured {
    rows: u64,
    runs: usize,
    wall: Duration,
    latencies: Vec<Duration>,
}

impl Measured {
    fn percentile(&self, p: usize) -> Duration {
        if self.latencies.is_empty() {
            return Duration::ZERO;
        }
        let at = (self.latencies.len() * p / 100).min(self.latencies.len() - 1);
        self.latencies[at]
    }

    fn max(&self) -> Duration {
        self.latencies.last().copied().unwrap_or(Duration::ZERO)
    }
}

fn run_workload(options: &Options, schema: &Arc<Schema>, workload: &Workload) -> Option<Measured> {
    // One run first, alone, to find out whether it compiles at all and how many rows
    // it answers with — a workload that fails should say so once rather than
    // `connections × runs` times.
    let mut probe = connect(options, schema, Mode::ReadOnly);
    let mut result = match probe.query(&workload.focus) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("loadgen: `{}` did not compile: {error}", workload.name);
            return None;
        }
    };

    let rows = probe.drain(&mut result).expect("its rows").len() as u64;
    drop(probe);

    let per_connection = options.runs.div_ceil(options.connections);
    let started = Instant::now();

    let latencies: Vec<Duration> = thread::scope(|scope| {
        let handles: Vec<_> = (0..options.connections)
            .map(|_| {
                scope.spawn(|| {
                    let mut connection = connect(options, schema, Mode::ReadOnly);
                    let mut mine = Vec::with_capacity(per_connection);

                    for _ in 0..per_connection {
                        let at = Instant::now();
                        let mut result = connection.query(&workload.focus).expect("it compiles");

                        // Pulled and dropped: the rows have to cross the socket and be
                        // decoded — that is the work — but rendering them would be
                        // measuring this program.
                        while connection.next_row(&mut result).expect("a row").is_some() {}

                        mine.push(at.elapsed());
                    }

                    mine
                })
            })
            .collect();

        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a worker finishes"))
            .collect()
    });

    let wall = started.elapsed();
    let mut latencies = latencies;
    latencies.sort_unstable();

    Some(Measured {
        rows,
        runs: latencies.len(),
        wall,
        latencies,
    })
}

// ---- plumbing ----------------------------------------------------------------

fn connect(options: &Options, schema: &Arc<Schema>, mode: Mode) -> Connection {
    Connection::connect(
        &options.socket,
        &options.database,
        Arc::clone(schema),
        mode,
        false,
    )
    .unwrap_or_else(|error| {
        eprintln!(
            "loadgen: cannot connect to {}: {error}",
            options.socket.display()
        );
        eprintln!("  is a server running? `aperture serve --data-dir <dir>`");
        std::process::exit(1);
    })
}

fn rate(count: u64, over: Duration) -> u64 {
    let seconds = over.as_secs_f64();
    if seconds <= 0.0 {
        return 0;
    }
    (count as f64 / seconds) as u64
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
    let micros = elapsed.as_secs_f64() * 1_000_000.0;

    if micros < 1000.0 {
        format!("{micros:.0}µs")
    } else if micros < 1_000_000.0 {
        format!("{:.2}ms", micros / 1000.0)
    } else {
        format!("{:.2}s", micros / 1_000_000.0)
    }
}

/// A right-aligned table. The CLI's is left-aligned and lives in a private module;
/// numbers want the other alignment and this is ten lines.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    use std::fmt::Write as _;

    let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
    }

    let mut out = String::new();

    for (index, header) in headers.iter().enumerate() {
        if index == 0 {
            let _ = write!(out, "{:<width$}", header, width = widths[0]);
        } else {
            let _ = write!(out, "  {:>width$}", header, width = widths[index]);
        }
    }
    out.push('\n');

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            // `chars().count()`, because the em dash a failed workload prints is three
            // bytes and one column.
            let pad = widths[index].saturating_sub(cell.chars().count());
            if index == 0 {
                let _ = write!(out, "{cell}{:pad$}", "", pad = pad);
            } else {
                let _ = write!(out, "  {:pad$}{cell}", "", pad = pad);
            }
        }
        out.push('\n');
    }

    out
}
