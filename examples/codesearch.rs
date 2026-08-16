//! **The traffic a code-search UI actually makes**: search by name, paged, never unbounded.
//!
//! ```text
//! cargo run --release --example codesearch -- --data-dir /path/to/db --users 256
//! cargo run --release --example codesearch -- --data-dir … --page 100 --think-ms 2000
//! ```
//!
//! `soak` measures a deliberately lopsided *generic* population — most queries cheap, one
//! in a hundred asking for the whole database — because that shape is what finds
//! head-of-line blocking. This one measures a **product**, and the two disagree about
//! almost everything that matters:
//!
//! | | `soak` | here |
//! |---|---|---|
//! | the expensive class | a whole-database join, 1% of queries, 94% of the CPU | there isn't one |
//! | how a query ends | drained to the last row | **paged**: take 50, cancel the rest |
//! | what it searches for | one pivot, every time | a **pool of real names**, prefix-truncated |
//! | an empty search term | — | **never issued**: no prefix, no query |
//!
//! Each of those changes the answer, and the last one is a product decision rather than a
//! measurement choice: a UI that refuses to search for nothing never asks this database for
//! an unbounded scan, which is what makes the numbers below look nothing like `soak`'s.
//!
//! # The model
//!
//! Every query is a prefix seek into `src.SearchByName`, whose key leads with `name`
//! precisely so that this is a range rather than a filter (`code_index.rs` says as much).
//! Terms are **sampled from the corpus** and truncated, so a search-as-you-type burst is
//! short prefixes matching thousands and a considered search is a long one matching a few —
//! which is the selectivity spread a real population has and a single pivot cannot show.
//!
//! Results are **paged**: the client takes `--page` rows and cancels the rest, which is
//! what a UI backend does when it renders fifty hits.
//!
//! It is worth knowing what that costs, because the obvious guess is wrong. The executor
//! computes a whole `CHUNK_ROWS = 256` chunk whatever the page size, so a 50-row page looks
//! like it should be free next to a 256-row one — and it is not. Measured at 32 users:
//!
//! ```text
//!   page  rows returned  typeahead p50  achieved
//!     50             48         6.1 ms  5,118 q/s
//!    100             93         7.1 ms  4,489 q/s
//!    256            226        10.5 ms  3,274 q/s
//!    500            417        14.4 ms  2,508 q/s
//! ```
//!
//! Roughly linear in rows *delivered*, because the per-row cost that dominates is not the
//! executor's — it is one frame each through the outbound mutex, the socket and the client's
//! decoder ([findings §9](../bench/FINDINGS.md)), and a cancel stops paying it. Bigger pages
//! are cheaper per row (245k row/s at page 50, 1.05M at page 500) and dearer per query, so
//! the page size is a latency/throughput dial rather than a free choice.
//!
//! # What is deliberately missing
//!
//! **Find-references.** It is the second thing anyone wants from a code-search tool and it
//! cannot be served: `src.Ref`'s key is `{at, file, to}`, so a lookup by `to` cannot seek
//! and scans all 4,879,151 references — 2.21 s for a *single* declaration, multiplied by
//! every declaration sharing the name. It is left out of the mix because including it would
//! make this a benchmark of one unanswerable query; it is in `bench/FINDINGS.md` as the
//! blocker it is.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use aperture_cli::code_index;
use aperture_client::{Connection, Mode};
use aperture_schema::schema::Schema;

/// What the server computes per turn. A page smaller than this is not a cheaper query.
const CHUNK_ROWS: usize = 256;

/// How many names to keep for the term pool, and how far apart to take them.
///
/// Spread through the keyspace rather than adjacent, so a run does not measure one warm
/// block of the index over and over.
const TERM_POOL: usize = 4_000;
const TERM_STRIDE: usize = 23;

fn main() {
    let options = match parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("codesearch: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    // **The idle baseline comes first**, before this process has asked the server for
    // anything. Taken after sampling the term pool it measured the tail of that scan, which
    // is precisely the mistake this correction exists to avoid.
    let idle_rate = idle_cpu_rate();

    let schema = Arc::new(code_index::schema());
    let terms = Arc::new(sample_terms(&options, &schema));

    println!(
        "codesearch — {} users, {}s, {} think, page {}, {} terms sampled",
        options.users,
        options.seconds,
        if options.think_ms == 0 {
            "no".to_owned()
        } else {
            format!("{}ms", options.think_ms)
        },
        options.page,
        terms.len()
    );

    let classes = classes();
    for class in &classes {
        println!(
            "  {:<22} weight {:<3} prefix {}–{} chars",
            class.name, class.weight, class.min_prefix, class.max_prefix
        );
    }
    if options.page < CHUNK_ROWS {
        println!(
            "\n  note: the executor still computes a whole {CHUNK_ROWS}-row chunk, but only the\n  \
             {} rows asked for are framed, sent and decoded — which is the part that costs.",
            options.page
        );
    }
    println!();

    let classes = Arc::new(classes);
    let stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicU64::new(0));
    let empty = Arc::new(AtomicU64::new(0));

    // This box runs other servers, and the benchmark is meant to be run *below* saturation
    // where machine-wide busy time is mostly not ours. Everything reported as attributable
    // is net of this.
    println!("  idle baseline  {idle_rate:.2} cores busy before the run\n");

    let started = Instant::now();
    let cpu_before = Cpu::now();

    let samples: Vec<Vec<Sample>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..options.users)
            .map(|user| {
                let schema = Arc::clone(&schema);
                let classes = Arc::clone(&classes);
                let terms = Arc::clone(&terms);
                let stop = Arc::clone(&stop);
                let errors = Arc::clone(&errors);
                let empty = Arc::clone(&empty);
                let options = &options;

                scope.spawn(move || {
                    run_user(
                        user, options, &schema, &classes, &terms, &stop, &errors, &empty,
                    )
                })
            })
            .collect();

        thread::sleep(Duration::from_secs(options.seconds));
        stop.store(true, Ordering::SeqCst);

        handles
            .into_iter()
            .map(|handle| handle.join().expect("a user finishes"))
            .collect()
    });

    let wall = started.elapsed();
    let cpu = cpu_before.since(wall);

    report(
        &classes,
        &samples,
        wall,
        errors.load(Ordering::SeqCst),
        empty.load(Ordering::SeqCst),
        &options,
        &cpu,
        idle_rate,
    );
}

const USAGE: &str = "\
usage: codesearch [options]

  --socket PATH     where the server is listening
  --data-dir PATH   derives the socket path, as the CLI does
  --database NAME   default `code`
  --users N         concurrent users, each a connection and a thread (default 64)
  --seconds S       how long to sustain the load (default 30)
  --think-ms MS     pause between one user's queries (default 3000)
  --page N          rows a search returns before cancelling the rest (default 50)
";

struct Options {
    socket: PathBuf,
    database: String,
    users: usize,
    seconds: u64,
    think_ms: u64,
    page: usize,
}

/// One kind of thing a person does to a search box.
struct Class {
    name: &'static str,
    weight: u32,
    /// Prefix length, in characters, drawn uniformly in this range.
    min_prefix: usize,
    max_prefix: usize,
    /// Whether the result list renders locations, which costs a fetch per row.
    with_detail: bool,
    /// Whether the term is used whole, as clicking a result does.
    exact: bool,
}

/// The mix, and the reasoning for each weight.
fn classes() -> Vec<Class> {
    vec![
        // Search-as-you-type: one query per keystroke once there is enough to search for,
        // so it is most of the traffic by a wide margin and its prefixes are short — which
        // is also when they match the most rows.
        Class {
            name: "typeahead",
            weight: 70,
            min_prefix: 2,
            max_prefix: 4,
            with_detail: false,
            exact: false,
        },
        // The search someone stopped typing at: longer, more selective, and the one whose
        // latency a person actually judges the product by.
        Class {
            name: "considered search",
            weight: 20,
            min_prefix: 5,
            max_prefix: 12,
            with_detail: false,
            exact: false,
        },
        // The same search rendering *where* each hit is, which is a fetch per row.
        Class {
            name: "search + locations",
            weight: 8,
            min_prefix: 5,
            max_prefix: 12,
            with_detail: true,
            exact: false,
        },
        // Clicking a hit.
        Class {
            name: "open symbol",
            weight: 2,
            min_prefix: 0,
            max_prefix: 0,
            with_detail: true,
            exact: true,
        },
    ]
}

/// The focus text for one search.
fn focus_for(class: &Class, term: &str, prefix: usize) -> String {
    let escaped = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");

    if class.exact {
        return format!(
            "{{n = S.name, at = S.to.line}} where S = src.SearchByName {{name = \"{}\", to = _}}",
            escaped(term)
        );
    }

    // Truncate on a character boundary: sampled names are somebody else's identifiers.
    let cut: String = term.chars().take(prefix).collect();

    if class.with_detail {
        format!(
            "{{n = S.name, at = S.to.line}} where S = src.SearchByName {{name = \"{}\".., to = _}}",
            escaped(&cut)
        )
    } else {
        format!(
            "D where src.SearchByName {{name = \"{}\".., to = D}}",
            escaped(&cut)
        )
    }
}

/// One measured query.
struct Sample {
    class: usize,
    elapsed: Duration,
    rows: usize,
}

#[allow(clippy::too_many_arguments)]
fn run_user(
    user: usize,
    options: &Options,
    schema: &Arc<Schema>,
    classes: &[Class],
    terms: &[String],
    stop: &AtomicBool,
    errors: &AtomicU64,
    empty: &AtomicU64,
) -> Vec<Sample> {
    let Ok(mut connection) = Connection::connect(
        &options.socket,
        &options.database,
        Arc::clone(schema),
        Mode::ReadOnly,
        false,
    ) else {
        errors.fetch_add(1, Ordering::Relaxed);
        return vec![];
    };

    let total: u32 = classes.iter().map(|class| class.weight).sum();
    let mut samples = Vec::new();

    // Deterministic per user, different between users: a benchmark that cannot be re-run
    // to the same sequence is one whose outliers cannot be chased.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ (user as u64).wrapping_mul(0x0100_0000_01B3);
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    while !stop.load(Ordering::Relaxed) {
        let pick = (next() % u64::from(total)) as u32;
        let mut running = 0;
        let index = classes
            .iter()
            .position(|class| {
                running += class.weight;
                pick < running
            })
            .unwrap_or(0);
        let class = &classes[index];

        let term = &terms[(next() % terms.len() as u64) as usize];
        let prefix = if class.max_prefix > class.min_prefix {
            class.min_prefix + (next() as usize % (class.max_prefix - class.min_prefix + 1))
        } else {
            class.max_prefix
        };

        let focus = focus_for(class, term, prefix);
        let started = Instant::now();

        match connection.query(&focus) {
            Ok(mut rows) => {
                match connection.take(&mut rows, options.page) {
                    Ok(page) => {
                        // The page is rendered; the rest of the result is not wanted. This
                        // is the *graceful* cancel — the one a UI backend should make, and
                        // the one that does not leak (`bench/FINDINGS.md` §10).
                        if !rows.finished() {
                            let _ = connection.cancel(&mut rows);
                        }

                        if page.is_empty() {
                            empty.fetch_add(1, Ordering::Relaxed);
                        }

                        samples.push(Sample {
                            class: index,
                            elapsed: started.elapsed(),
                            rows: page.len(),
                        });
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => {
                errors.fetch_add(1, Ordering::Relaxed);
            }
        }

        if options.think_ms > 0 {
            thread::sleep(Duration::from_millis(options.think_ms));
        }
    }

    samples
}

/// Real declaration names, spread through the index.
fn sample_terms(options: &Options, schema: &Arc<Schema>) -> Vec<String> {
    let mut connection = Connection::connect(
        &options.socket,
        &options.database,
        Arc::clone(schema),
        Mode::ReadOnly,
        false,
    )
    .expect("the server is listening");

    let mut rows = connection
        .query("N where src.SearchByName {name = N, to = _}")
        .expect("the search index compiles");

    let page = connection
        .take(&mut rows, TERM_POOL * TERM_STRIDE)
        .expect("the search index answers");
    let _ = connection.cancel(&mut rows);

    let terms: Vec<String> = page
        .iter()
        .step_by(TERM_STRIDE)
        .filter_map(|value| match value {
            aperture_wire::WireValue::Str(text) if text.chars().count() >= 2 => Some(text.clone()),
            _ => None,
        })
        .collect();

    assert!(
        !terms.is_empty(),
        "no search terms sampled — is this database indexed?"
    );

    terms
}

// ---- reporting ----------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn report(
    classes: &[Class],
    samples: &[Vec<Sample>],
    wall: Duration,
    errors: u64,
    empty: u64,
    options: &Options,
    cpu: &Cpu,
    idle_rate: f64,
) {
    let mut per_class: Vec<Vec<Sample>> = classes.iter().map(|_| vec![]).collect();
    for user in samples {
        for sample in user {
            per_class[sample.class].push(Sample {
                class: sample.class,
                elapsed: sample.elapsed,
                rows: sample.rows,
            });
        }
    }

    let completed: usize = per_class.iter().map(Vec::len).sum();
    let mut rows_out = vec![row(&[
        "class", "count", "p50", "p95", "p99", "p99.9", "max", "rows", "q/s",
    ])];

    for (class, samples) in classes.iter().zip(per_class.iter_mut()) {
        if samples.is_empty() {
            rows_out.push(row(&[class.name, "0", "—", "—", "—", "—", "—", "—", "—"]));
            continue;
        }

        let mean_rows =
            samples.iter().map(|sample| sample.rows).sum::<usize>() as f64 / samples.len() as f64;
        samples.sort_by_key(|sample| sample.elapsed);
        let latencies: Vec<Duration> = samples.iter().map(|sample| sample.elapsed).collect();

        rows_out.push(row(&[
            class.name,
            &thousands(samples.len() as u64),
            &percentile(&latencies, 500),
            &percentile(&latencies, 950),
            &percentile(&latencies, 990),
            &percentile(&latencies, 999),
            &duration(*latencies.last().expect("not empty")),
            &format!("{mean_rows:.0}"),
            &format!("{:.0}", samples.len() as f64 / wall.as_secs_f64()),
        ]));
    }

    print!("{}", table(&rows_out));

    let achieved = completed as f64 / wall.as_secs_f64();
    println!();
    println!("  users          {}", options.users);
    println!("  completed      {completed} in {:.1}s", wall.as_secs_f64());
    println!("  achieved       {achieved:.0} queries/s");

    if options.think_ms > 0 {
        let offered = options.users as f64 * 1000.0 / options.think_ms as f64;
        println!(
            "  offered        {offered:.0} queries/s  ({}%)",
            (achieved / offered * 100.0).round()
        );
    }

    println!(
        "  empty results  {empty}{}",
        if empty == 0 {
            ""
        } else {
            "   <-- a sampled term matched nothing, which should be impossible"
        }
    );
    println!(
        "  errors         {errors}{}",
        if errors == 0 { "" } else { "   <-- LOOK" }
    );
    println!(
        "  cpu            {:.0}% of {:.0} core-seconds — generator {:.0}%, server side {:.0}%",
        100.0 * cpu.machine / cpu.available.max(1.0),
        cpu.available,
        100.0 * cpu.mine / cpu.available.max(1.0),
        100.0 * (cpu.machine - cpu.mine).max(0.0) / cpu.available.max(1.0),
    );

    // The number the question is actually about, stated rather than left to arithmetic —
    // and net of whatever else this box was doing before the run started.
    let cores = cpu.available / wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let attributable = (cpu.machine - idle_rate * wall.as_secs_f64()).max(0.0);

    if attributable > 0.0 && completed > 0 {
        let per_query = attributable / completed as f64;
        let saturated = cores / per_query;

        println!(
            "\n  {:.2} ms of CPU per query, net of the idle baseline",
            per_query * 1000.0
        );
        println!("  → ~{saturated:.0} q/s to saturate {cores:.0} cores");
        if options.think_ms > 0 {
            println!(
                "  → ~{:.0} users at {}s think, ~{:.0} at 10s",
                saturated * (options.think_ms as f64 / 1000.0),
                options.think_ms as f64 / 1000.0,
                saturated * 10.0
            );
        }
        println!(
            "  (extrapolation from {:.0}% utilisation — trust it least when that is lowest)",
            100.0 * cpu.machine / cpu.available.max(1.0)
        );
    }
}

// ---- the shared odds and ends --------------------------------------------------------

/// CPU seconds, taken twice and subtracted — the same accounting `soak` does, and for the
/// same reason: a synchronous generator is one thread per user on the server's own cores.
struct Cpu {
    mine: f64,
    machine: f64,
    available: f64,
}

impl Cpu {
    fn now() -> Cpu {
        Cpu {
            mine: self_cpu_seconds(),
            machine: machine_busy_seconds(),
            available: 0.0,
        }
    }

    fn since(&self, wall: Duration) -> Cpu {
        let cores = thread::available_parallelism().map_or(1.0, |n| n.get() as f64);
        Cpu {
            mine: self_cpu_seconds() - self.mine,
            machine: machine_busy_seconds() - self.machine,
            available: wall.as_secs_f64() * cores,
        }
    }
}

const TICKS_PER_SECOND: f64 = 100.0;

/// Cores busy while nothing of ours is running, sampled over two seconds.
fn idle_cpu_rate() -> f64 {
    let before = machine_busy_seconds();
    thread::sleep(Duration::from_secs(2));
    (machine_busy_seconds() - before) / 2.0
}

fn self_cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return 0.0;
    };

    let fields: Vec<&str> = rest.split_whitespace().collect();
    let at = |index: usize| -> f64 {
        fields
            .get(index)
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0)
    };

    (at(11) + at(12)) / TICKS_PER_SECOND
}

fn machine_busy_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let Some(line) = stat.lines().next() else {
        return 0.0;
    };

    let fields: Vec<f64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse().ok())
        .collect();

    let total: f64 = fields.iter().sum();
    let idle: f64 = fields.iter().skip(3).take(2).sum();

    (total - idle) / TICKS_PER_SECOND
}

/// Permille, because p99.9 is the percentile a search box is judged on and p99 hides it.
fn percentile(sorted: &[Duration], per_mille: usize) -> String {
    let at = (sorted.len() * per_mille / 1000).min(sorted.len() - 1);
    duration(sorted[at])
}

fn duration(elapsed: Duration) -> String {
    let micros = elapsed.as_secs_f64() * 1_000_000.0;
    if micros < 1000.0 {
        format!("{micros:.0}µs")
    } else if micros < 1_000_000.0 {
        format!("{:.1}ms", micros / 1000.0)
    } else {
        format!("{:.2}s", micros / 1_000_000.0)
    }
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

fn row(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|cell| (*cell).to_owned()).collect()
}

fn table(rows: &[Vec<String>]) -> String {
    use std::fmt::Write as _;

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

fn parse() -> Result<Options, String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut options = Options {
        socket: PathBuf::new(),
        database: "code".to_owned(),
        users: 64,
        seconds: 30,
        think_ms: 3000,
        page: 50,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));

        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--database" => options.database = value()?,
            "--users" => {
                options.users = value()?.parse().map_err(|_| "--users takes a number")?;
            }
            "--seconds" => {
                options.seconds = value()?.parse().map_err(|_| "--seconds takes a number")?;
            }
            "--think-ms" => {
                options.think_ms = value()?.parse().map_err(|_| "--think-ms takes a number")?;
            }
            "--page" => {
                options.page = value()?.parse().map_err(|_| "--page takes a number")?;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }

    options.socket = match (socket, data_dir) {
        (Some(socket), _) => socket,
        (None, Some(dir)) => dir.join("aperture.sock"),
        (None, None) => return Err("one of --socket or --data-dir is required".to_owned()),
    };

    if options.page == 0 {
        return Err("--page must be at least 1".to_owned());
    }

    Ok(options)
}
