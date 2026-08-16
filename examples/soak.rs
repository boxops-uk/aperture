//! **Many overlapping clients, queries of very different cost, sustained.**
//!
//! `loadgen` answers "how fast is one workload" and `breakdown` answers "where does one
//! query's time go". Neither answers the question that decides whether an architecture
//! survives contact with a user population: what happens to a **cheap** query when it
//! is sharing the server with expensive ones, and does the whole thing degrade
//! gracefully or fall over.
//!
//! ```text
//! cargo run --release --example soak -- --data-dir /tmp/ap-bench --clients 64 --seconds 20
//! ```
//!
//! # What it does
//!
//! Each client is a connection and a thread, issuing queries drawn from a weighted mix
//! and pausing for `--think-ms` between them, the way a person does. The mix is
//! deliberately lopsided — most queries are cheap, a few are ruinous — because that is
//! the shape a real population has, and because a fair-looking average hides exactly
//! the failure being looked for.
//!
//! What is reported is **per class**, never pooled: a p99 over a mix of a point lookup
//! and a hundred-thousand-row scan is a number about the mix, not about the server.
//! The question is whether the point lookup stayed fast, and only a per-class
//! percentile can answer it.
//!
//! # How to read it
//!
//! - **offered vs achieved** — if achieved is below offered, the server is saturated
//!   and every latency below is a queue rather than a service time.
//! - **cheap-query p99** — the number a user notices. Graceful degradation means it
//!   rises with load; falling over means it detaches from p50.
//! - **errors** — anything other than zero is the actual failure signal.
//!
//! The client shares a machine with the server, so at high client counts the
//! measurement is partly of the load generator. Where that starts to bite is visible
//! as achieved rate flattening while CPU is not the server's.

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

/// One kind of query a client might ask, and how often.
struct Class {
    name: &'static str,
    weight: u32,
    focus: String,
}

struct Options {
    socket: PathBuf,
    database: String,
    clients: usize,
    seconds: u64,
    think_ms: u64,
    stalled: usize,
}

const USAGE: &str = "\
usage: soak [options]

  --socket PATH     where the server is listening
  --data-dir PATH   derives the socket path, as the CLI does
  --database NAME   default `code`
  --clients N       concurrent connections, each a thread (default 32)
  --seconds S       how long to sustain the load (default 15)
  --think-ms MS     pause between one client's queries (default 0 — as hard as it can)
  --stalled N       extra clients that ask for everything and then stop reading

A **stalled** client is the classic way a server falls over: it asks for a large
result and then does not read it, so the answer backs up. What should happen is that
it blocks itself and nobody else — the per-stream queues are bounded and the writer
is fair, so a stream that will not drain is a stream that waits. What must not happen
is that it holds a worker, a blocking thread, or the connection's reader.";

fn main() {
    let options = match parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("soak: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let schema = Arc::new(code_index::schema());
    let pivots = sample(&options, &schema);
    let classes = Arc::new(mix(&pivots));

    println!("sampled: {} · {}", pivots.file, pivots.directory);

    println!(
        "soak — {} clients, {}s, {} think, mix of {} classes",
        options.clients,
        options.seconds,
        if options.think_ms == 0 {
            "no".to_owned()
        } else {
            format!("{}ms", options.think_ms)
        },
        classes.len()
    );
    for class in classes.iter() {
        println!("  {:<18} weight {}", class.name, class.weight);
    }
    println!();

    let stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicU64::new(0));

    // Started first, and given a moment to get their results flowing, so the measured
    // clients below are running against a server that is already holding them.
    let stalled = start_stalled(&options, &schema, &stop);
    if !stalled.is_empty() {
        println!(
            "  ...and {} stalled clients holding a result open\n",
            stalled.len()
        );
        thread::sleep(Duration::from_millis(500));
    }

    let started = Instant::now();
    let cpu_before = Cpu::now();

    let samples: Vec<Vec<(usize, Duration)>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..options.clients)
            .map(|client| {
                let schema = Arc::clone(&schema);
                let classes = Arc::clone(&classes);
                let stop = Arc::clone(&stop);
                let errors = Arc::clone(&errors);
                let options = &options;

                scope.spawn(move || run_client(client, options, &schema, &classes, &stop, &errors))
            })
            .collect();

        thread::sleep(Duration::from_secs(options.seconds));
        stop.store(true, Ordering::SeqCst);

        let measured = handles
            .into_iter()
            .map(|handle| handle.join().expect("a client finishes"))
            .collect();

        for handle in stalled {
            let _ = handle.join();
        }

        measured
    });

    let wall = started.elapsed();
    let cpu = cpu_before.since(wall);

    report(
        &classes,
        &samples,
        wall,
        errors.load(Ordering::SeqCst),
        &options,
        &cpu,
    );
}

/// CPU seconds, taken twice and subtracted.
///
/// **The generator shares this machine with the server**, and a synchronous client is
/// one OS thread per user, so at high client counts some of what looks like the server
/// saturating is this process saturating. The only way to tell them apart is to report
/// what each burned: `mine` is this process, `machine` is every core that was not idle.
/// `machine - mine` is the server's share plus whatever else is running, which on an
/// otherwise quiet box is the number that matters.
struct Cpu {
    mine: f64,
    machine: f64,
    /// Wall seconds × cores — the ceiling both of the above are measured against.
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
        let cores = std::thread::available_parallelism().map_or(1.0, |n| n.get() as f64);

        Cpu {
            mine: self_cpu_seconds() - self.mine,
            machine: machine_busy_seconds() - self.machine,
            available: wall.as_secs_f64() * cores,
        }
    }
}

/// The kernel counts in clock ticks, and there are 100 a second on every Linux this
/// runs on. Read rather than assumed would mean linking `libc` for one constant.
const TICKS_PER_SECOND: f64 = 100.0;

fn self_cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();

    // `comm` is arbitrary text in parentheses, so the fields only line up after the
    // last `)`. From there, token 11 is `utime` and token 12 is `stime`.
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

    // `cpu user nice system idle iowait irq softirq steal …` — busy is everything
    // except idle and iowait, because a core waiting on disk is not one this is
    // competing for.
    let fields: Vec<f64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse().ok())
        .collect();

    let total: f64 = fields.iter().sum();
    let idle: f64 = fields.iter().skip(3).take(2).sum();

    (total - idle) / TICKS_PER_SECOND
}

/// The values the cheap classes ask for, **sampled from the database** rather than
/// computed from what it was seeded with.
///
/// Computing them only works against a corpus this file wrote. Against somebody's
/// indexed checkout there is no arithmetic that lands on a key which exists, and a point
/// lookup for a key that is not there measures a miss — which is a different, and
/// cheaper, query than the one the mix is supposed to be weighted around.
struct Pivots {
    file: String,
    directory: String,
}

/// One connection, two queries, before any load starts.
fn sample(options: &Options, schema: &Arc<Schema>) -> Pivots {
    let mut connection = Connection::connect(
        &options.socket,
        &options.database,
        Arc::clone(schema),
        Mode::ReadOnly,
        false,
    )
    .expect("the server is listening");

    let mut rows = connection
        .query("F where src.File F")
        .expect("src.File compiles");

    // Deep enough in that a seek has somewhere to seek past, and the last row taken
    // rather than the first, so a short predicate still answers.
    let page = connection
        .take(&mut rows, 16_000)
        .expect("the scan answers");
    connection.cancel(&mut rows).ok();

    let file = page
        .iter()
        .rev()
        .find_map(|value| match value {
            aperture_wire::WireValue::Str(text) => Some(text.clone()),
            _ => None,
        })
        .expect("src.File holds at least one row");

    let directory = match file.rfind('/') {
        Some(cut) => file[..=cut].to_owned(),
        None => file.clone(),
    };

    Pivots { file, directory }
}

/// A weighted mix, lopsided on purpose.
///
/// Most of a population asks cheap questions; a few ask for everything. The expensive
/// class is what makes the cheap class's p99 mean something — without it the test is
/// "is one query fast", which is already known.
fn mix(pivots: &Pivots) -> Vec<Class> {
    vec![
        Class {
            name: "point lookup",
            weight: 80,
            focus: format!("F where src.File F; F = \"{}\"", pivots.file),
        },
        Class {
            name: "small scan",
            weight: 15,
            focus: format!("F where src.File F; F = \"{}\"..", pivots.directory),
        },
        Class {
            name: "full scan",
            weight: 4,
            focus: "F where src.File F".to_owned(),
        },
        Class {
            name: "join, whole db",
            weight: 1,
            focus: "{what = D.name, file = D.module.file} where D = src.Decl _".to_owned(),
        },
    ]
}

/// Clients that ask for everything and then stop reading.
///
/// Each opens a query, takes a couple of rows, and sleeps until the run is over. The
/// server has a result in flight for every one of them, with nobody draining it.
fn start_stalled<'scope>(
    options: &'scope Options,
    schema: &Arc<Schema>,
    stop: &'scope Arc<AtomicBool>,
) -> Vec<thread::JoinHandle<()>> {
    (0..options.stalled)
        .map(|_| {
            let schema = Arc::clone(schema);
            let stop = Arc::clone(stop);
            let socket = options.socket.clone();
            let database = options.database.clone();

            thread::spawn(move || {
                let Ok(mut connection) =
                    Connection::connect(&socket, &database, schema, Mode::ReadOnly, false)
                else {
                    return;
                };

                let Ok(mut rows) = connection.query("F where src.File F") else {
                    return;
                };

                // Two rows, then nothing: enough that the server is mid-result and
                // filling the queue behind us.
                let _ = connection.take(&mut rows, 2);

                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(50));
                }
            })
        })
        .collect()
}

fn run_client(
    client: usize,
    options: &Options,
    schema: &Arc<Schema>,
    classes: &[Class],
    stop: &AtomicBool,
    errors: &AtomicU64,
) -> Vec<(usize, Duration)> {
    let mut connection = match Connection::connect(
        &options.socket,
        &options.database,
        Arc::clone(schema),
        Mode::ReadOnly,
        false,
    ) {
        Ok(connection) => connection,
        Err(error) => {
            eprintln!("soak: client {client} could not connect: {error}");
            errors.fetch_add(1, Ordering::Relaxed);
            return vec![];
        }
    };

    let total: u32 = classes.iter().map(|class| class.weight).sum();
    let mut samples = Vec::with_capacity(4096);

    // Deterministic and per-client: no `rand` dependency, and a client that starts at
    // a different point in the cycle than its neighbour, so they do not march in step
    // and manufacture a thundering herd the real population would not have.
    let mut tick = (client as u32).wrapping_mul(2_654_435_761);

    while !stop.load(Ordering::Relaxed) {
        tick = tick.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let mut pick = (tick >> 8) % total;

        let index = classes
            .iter()
            .position(|class| {
                if pick < class.weight {
                    true
                } else {
                    pick -= class.weight;
                    false
                }
            })
            .unwrap_or(0);

        let at = Instant::now();
        let answered = connection
            .query(&classes[index].focus)
            .and_then(|mut rows| {
                while connection.next_row(&mut rows)?.is_some() {}
                Ok(())
            });

        match answered {
            Ok(()) => samples.push((index, at.elapsed())),
            Err(error) => {
                if errors.fetch_add(1, Ordering::Relaxed) < 5 {
                    eprintln!("soak: client {client}: {error}");
                }
                // A failed connection cannot be reused: the socket may be mid-frame.
                return samples;
            }
        }

        if options.think_ms > 0 {
            thread::sleep(Duration::from_millis(options.think_ms));
        }
    }

    samples
}

fn report(
    classes: &[Class],
    samples: &[Vec<(usize, Duration)>],
    wall: Duration,
    errors: u64,
    options: &Options,
    cpu: &Cpu,
) {
    let mut per_class: Vec<Vec<Duration>> = classes.iter().map(|_| vec![]).collect();

    for client in samples {
        for (index, elapsed) in client {
            per_class[*index].push(*elapsed);
        }
    }

    let completed: usize = per_class.iter().map(Vec::len).sum();

    let mut rows = vec![];
    for (class, latencies) in classes.iter().zip(per_class.iter_mut()) {
        latencies.sort_unstable();

        if latencies.is_empty() {
            rows.push(vec![class.name.to_owned(), "0".to_owned()]);
            continue;
        }

        rows.push(vec![
            class.name.to_owned(),
            latencies.len().to_string(),
            percentile(latencies, 50),
            percentile(latencies, 95),
            percentile(latencies, 99),
            duration(*latencies.last().expect("not empty")),
            format!("{:.0}", latencies.len() as f64 / wall.as_secs_f64()),
        ]);
    }

    print!(
        "{}",
        table(
            &["class", "count", "p50", "p95", "p99", "max", "q/s"],
            &rows
        )
    );

    let achieved = completed as f64 / wall.as_secs_f64();

    println!();
    println!("  clients        {}", options.clients);
    println!("  completed      {completed} in {:.1}s", wall.as_secs_f64());
    println!("  achieved       {achieved:.0} queries/s");

    if options.think_ms > 0 {
        let offered = options.clients as f64 * 1000.0 / options.think_ms as f64;
        println!(
            "  offered        {offered:.0} queries/s  ({}%)",
            (achieved / offered * 100.0).round()
        );
    }

    println!(
        "  errors         {errors}{}",
        if errors == 0 { "" } else { "   <-- LOOK" }
    );

    // Reported beside every result, per the phase plan's constraint §5: without it a
    // flat achieved rate is ambiguous between "the server is full" and "the generator
    // is". `server side` is everything busy that was not this process.
    println!(
        "  cpu            {:.0}% of {:.0} core-seconds — generator {:.0}%, server side {:.0}%",
        100.0 * cpu.machine / cpu.available.max(1.0),
        cpu.available,
        100.0 * cpu.mine / cpu.available.max(1.0),
        100.0 * (cpu.machine - cpu.mine).max(0.0) / cpu.available.max(1.0),
    );
}

fn percentile(sorted: &[Duration], p: usize) -> String {
    let at = (sorted.len() * p / 100).min(sorted.len() - 1);
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

fn parse() -> Result<Options, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let mut data_dir: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut database = "code".to_owned();
    let mut clients = 32;
    let mut seconds = 15;
    let mut think_ms = 0;
    let mut stalled = 0;

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
            "--clients" => clients = value()?.parse().map_err(|_| "--clients takes a number")?,
            "--seconds" => seconds = value()?.parse().map_err(|_| "--seconds takes a number")?,
            "--think-ms" => think_ms = value()?.parse().map_err(|_| "--think-ms takes a number")?,
            "--stalled" => stalled = value()?.parse().map_err(|_| "--stalled takes a number")?,
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
        clients: clients.max(1),
        seconds: seconds.max(1),
        think_ms,
        stalled,
    })
}
