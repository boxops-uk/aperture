//! **Where a query's fixed cost goes.**
//!
//! `loadgen` measures a query end to end and finds a floor: `X where X = 42` folds to
//! a plan with no steps and no store read, and still costs ~200µs. That number caps
//! query *rate* whatever the query does, so it is worth knowing what it is made of
//! before anyone tries to make it smaller.
//!
//! ```text
//! cargo run --release --example breakdown
//! ```
//!
//! # The method is subtraction, and the parts are measured apart from each other
//!
//! Each layer is timed on its own — the transport with no server behind it, the
//! compiler with no socket in front of it — and the parts are then added up and
//! compared against the end-to-end number. What the comparison is for is the
//! *residual*: if the parts account for the whole, the breakdown is complete, and if
//! they do not, the gap is the interesting thing and this file is not finished.
//!
//! Everything runs in one process against a real server on a real socket, so nothing
//! here is a model of the system — it is the system, with a stopwatch in more places.
//!
//! # Read the ratios, not the microseconds
//!
//! Every number below is a **thread handoff on the machine it ran on**, and handoff
//! latency is the most machine-dependent thing there is: a loaded four-vCPU box and an
//! idle sixty-four-core one will disagree by a factor, and the run-to-run spread here
//! is tens of percent. What does not move is the *shape* — which line is two thirds of
//! the total and which is two percent — and that is the thing to act on.
//!
//! These are also **best-case** handoffs: measured with nothing else on the runtime. A
//! busy server pays less per handoff, because the threads stay warm, and more in
//! queueing. Both are visible in the gap between p50 and p95.

use std::{
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use aperture_cli::code_index;
use aperture_client::{Connection, Mode};
use aperture_engine::{
    compile::Compilation,
    iter::{Executor, Iteratee, Stream},
    plan::Plan,
};
use aperture_schema::schema::Schema;
use aperture_server::{Registry, server::Listener};
use aperture_store::{catalog::Catalog, store::FjallDb};
use aperture_wire::{FrameKind, StreamId, encode_desc, encode_frame, frame};

/// The query whose cost is being taken apart: every binding folds, so it compiles to
/// no steps and means exactly one row.
const BASELINE: &str = "X where X = 42";

/// A query with a level, for contrast — the same fixed cost plus one scan's setup.
const ONE_LEVEL: &str = "F where src.File F";

const ITERATIONS: usize = 2000;
const WARMUP: usize = 200;

fn main() {
    let dir = std::env::temp_dir().join(format!("ap-breakdown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    let schema = Arc::new(code_index::schema());
    let socket = dir.join("s.sock");
    let root = dir.join("store");

    let catalog = Catalog::open(&root).expect("a store root");
    catalog.create("code", &schema).expect("a database");

    // A **second store root**, which no server touches: `ops-I1` gives the running
    // server every database under its own, so the direct-to-the-store measurements
    // below cannot share one with it. That is the invariant working, not an obstacle
    // to route around — the alternative would be a second handle on a held directory.
    let bare = Catalog::open(dir.join("bare")).expect("a second store root");
    bare.create("solo", &schema)
        .expect("a database nobody serves");
    let store_path = bare.get("solo").expect("it is there").path.clone();

    let (registry, _listing) = Registry::open(catalog, (*schema).clone()).expect("a registry");
    let listener = Listener::bind(&socket).expect("a socket");
    thread::spawn(move || {
        let _ = listener.run_blocking(Arc::new(registry));
    });

    println!("breakdown of a query's fixed cost — {ITERATIONS} iterations each\n");

    let mut rows = vec![];

    // ---- the transport, with no server behind it ---------------------------
    rows.push(measure("socket round trip", echo_roundtrip));

    // ---- what the runtime charges for moving work between tasks -------------
    //
    // The server's small-query path is mostly *hops*: the reader routes a frame to a
    // per-stream task, that task hops to the blocking pool and back twice, and every
    // frame it answers with goes through a channel to the one writer task. None of
    // that is work; all of it is scheduling, and this is what scheduling costs here.
    rows.extend(measure_runtime());

    // ---- the compiler, with no socket in front of it -----------------------
    {
        let schema = Arc::clone(&schema);
        rows.push(measure("compile (baseline)", move || {
            compile(BASELINE, &schema)
        }));
    }
    {
        let schema = Arc::clone(&schema);
        rows.push(measure("compile (one level)", move || {
            compile(ONE_LEVEL, &schema)
        }));
    }
    {
        let schema = Arc::clone(&schema);
        rows.push(measure("prepare (compile + desc + interner)", move || {
            prepare(BASELINE, &schema)
        }));
    }

    // ---- the store, which the baseline never reads but still opens ---------
    {
        let db = FjallDb::open(&store_path).expect("the database opens");
        rows.push(measure("fjall snapshot", move || {
            std::hint::black_box(db.reader());
        }));
    }
    {
        let db = FjallDb::open(&store_path).expect("the database opens");
        let plan = plan_of(BASELINE, &schema);
        rows.push(measure("execute (no steps)", move || {
            run(&db, &plan);
        }));
    }

    // ---- and the whole thing ------------------------------------------------
    {
        let mut connection = connect(&socket, &schema);
        rows.push(measure("END TO END (baseline)", move || {
            let mut result = connection.query(BASELINE).expect("it compiles");
            while connection.next_row(&mut result).expect("a row").is_some() {}
        }));
    }
    {
        let mut connection = connect(&socket, &schema);
        rows.push(measure("END TO END (one level, empty)", move || {
            let mut result = connection.query(ONE_LEVEL).expect("it compiles");
            while connection.next_row(&mut result).expect("a row").is_some() {}
        }));
    }

    // **A query that does not compile**, which is the same path one frame and one
    // blocking hop shorter: the stream task is spawned, hops to the pool to compile,
    // fails, and answers with a single error frame. Subtracting it from the baseline
    // leaves exactly one hop and two frames, which is how the residual gets named.
    {
        let mut connection = connect(&socket, &schema);
        rows.push(measure("END TO END (bad query)", move || {
            let error = connection.query("this is not focus");
            std::hint::black_box(error.is_err());
        }));
    }

    report(&rows);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the parts ---------------------------------------------------------------

fn compile(source: &str, schema: &Schema) {
    let mut compilation = Compilation::new(source, schema);
    std::hint::black_box(compilation.plan());
}

/// Exactly what the server's `prepare` does, so the number is that function's and not
/// an approximation of it.
fn prepare(source: &str, schema: &Schema) {
    let mut compilation = Compilation::new(source, schema);
    let plan = compilation.plan().expect("it compiles");

    let head = compilation.head_ty().expect("a head type");
    let desc = aperture_server::rows::desc_of(head, compilation.interner()).expect("a descriptor");

    let mut descriptor = vec![];
    encode_desc(&mut descriptor, &desc);

    let mut interner = compilation.into_interner();
    let ty = desc.to_ty(&mut interner);

    std::hint::black_box((plan, descriptor, ty));
}

fn plan_of(source: &str, schema: &Schema) -> Plan {
    Compilation::new(source, schema)
        .plan()
        .expect("it compiles")
}

fn run(db: &FjallDb, plan: &Plan) {
    let outcome = Executor::new(db.reader(), plan.clone())
        .enumerate(
            0usize,
            |acc, _row| Ok(Stream::Continue(acc + 1)),
            &tokio_util::sync::CancellationToken::new(),
        )
        .expect("it runs");

    let (Iteratee::Done(rows) | Iteratee::Suspended(rows, _)) = outcome;
    std::hint::black_box(rows);
}

/// A frame there and back against a socket that does nothing but echo.
///
/// The floor the transport imposes, with no session, no compiler and no store — so
/// whatever is left over after subtracting it belongs to this project rather than to
/// the kernel.
fn echo_roundtrip() {
    thread_local! {
        static PAIR: std::cell::RefCell<Option<UnixStream>> = const { std::cell::RefCell::new(None) };
    }

    PAIR.with(|pair| {
        let mut borrowed = pair.borrow_mut();

        let stream = borrowed.get_or_insert_with(|| {
            let path = std::env::temp_dir().join(format!("ap-echo-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);

            let listener = UnixListener::bind(&path).expect("an echo socket");
            thread::spawn(move || {
                let Ok((mut server, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0u8; 256];
                while let Ok(read) = server.read(&mut buffer) {
                    if read == 0 || server.write_all(&buffer[..read]).is_err() {
                        return;
                    }
                }
            });

            UnixStream::connect(&path).expect("the echo connects")
        });

        let mut out = vec![];
        encode_frame(
            &mut out,
            FrameKind::DATA_ROW,
            StreamId(1),
            BASELINE.as_bytes(),
        )
        .expect("a frame");

        stream.write_all(&out).expect("a write");

        let mut back = vec![0u8; out.len()];
        stream.read_exact(&mut back).expect("a read");
    });
}

/// The three things the runtime charges the server for on every query, measured on a
/// runtime of the same shape and with nothing else running on it.
///
/// That last part matters: these are the *best case*. Under load every one of them is
/// a queue rather than a handoff, which is why the end-to-end p95 pulls away from its
/// p50 far harder than any of the work below does.
fn measure_runtime() -> Vec<Row> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let mut out = vec![];

    // Off the reactor and back — twice per query, once to compile and once per chunk.
    {
        let mut samples = Vec::with_capacity(ITERATIONS);
        runtime.block_on(async {
            for index in 0..ITERATIONS + WARMUP {
                let at = Instant::now();
                tokio::task::spawn_blocking(|| std::hint::black_box(0u8))
                    .await
                    .expect("it finishes");

                if index >= WARMUP {
                    samples.push(at.elapsed());
                }
            }
        });
        out.push(finish("  spawn_blocking hop", samples));
    }

    // A task per stream, and a stream per query: every query the client issues claims
    // a fresh id, so the reader spawns a task for it.
    {
        let mut samples = Vec::with_capacity(ITERATIONS);
        runtime.block_on(async {
            for index in 0..ITERATIONS + WARMUP {
                let at = Instant::now();
                tokio::spawn(async { std::hint::black_box(0u8) })
                    .await
                    .expect("it finishes");

                if index >= WARMUP {
                    samples.push(at.elapsed());
                }
            }
        });
        out.push(finish("  tokio::spawn + join", samples));
    }

    // A frame handed to another task and acknowledged — the shape of the reader
    // routing to a stream, and of a stream queueing to the writer.
    {
        let mut samples = Vec::with_capacity(ITERATIONS);
        runtime.block_on(async {
            let (to_worker, mut inbox) =
                tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<()>>(2);

            tokio::spawn(async move {
                while let Some(reply) = inbox.recv().await {
                    let _ = reply.send(());
                }
            });

            for index in 0..ITERATIONS + WARMUP {
                let at = Instant::now();
                let (reply, wait) = tokio::sync::oneshot::channel();
                to_worker.send(reply).await.expect("it queues");
                wait.await.expect("it answers");

                if index >= WARMUP {
                    samples.push(at.elapsed());
                }
            }
        });
        out.push(finish("  mpsc hop to a task and back", samples));
    }

    out
}

// ---- measuring ---------------------------------------------------------------

struct Row {
    name: &'static str,
    p50: Duration,
    p95: Duration,
    mean: Duration,
}

fn measure(name: &'static str, mut work: impl FnMut()) -> Row {
    for _ in 0..WARMUP {
        work();
    }

    let mut samples = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let at = Instant::now();
        work();
        samples.push(at.elapsed());
    }

    finish(name, samples)
}

fn finish(name: &'static str, mut samples: Vec<Duration>) -> Row {
    samples.sort_unstable();

    let total: Duration = samples.iter().sum();
    let mean = total / samples.len().max(1) as u32;

    Row {
        name,
        p50: samples[samples.len() / 2],
        p95: samples[samples.len() * 95 / 100],
        mean,
    }
}

fn report(rows: &[Row]) {
    let width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);

    println!(
        "{:<width$}  {:>10}  {:>10}  {:>10}",
        "", "p50", "p95", "mean"
    );
    for row in rows {
        println!(
            "{:<width$}  {:>10}  {:>10}  {:>10}",
            row.name,
            micros(row.p50),
            micros(row.p95),
            micros(row.mean)
        );
    }

    let get = |name: &str| rows.iter().find(|row| row.name == name).map(|row| row.p50);

    let (
        Some(transport),
        Some(hop),
        Some(spawn),
        Some(channel),
        Some(prepare),
        Some(execute),
        Some(whole),
        Some(bad),
    ) = (
        get("socket round trip"),
        get("  spawn_blocking hop"),
        get("  tokio::spawn + join"),
        get("  mpsc hop to a task and back"),
        get("prepare (compile + desc + interner)"),
        get("execute (no steps)"),
        get("END TO END (baseline)"),
        get("END TO END (bad query)"),
    )
    else {
        return;
    };

    // **The subtraction that names the residual.** A bad query walks the same path one
    // blocking hop and two outbound frames shorter — it is spawned, hops to the pool to
    // compile, fails, and answers with a single error frame. So the difference between
    // the two end-to-end numbers is exactly that hop plus those two frames, and what is
    // left after taking the hop out is what one frame costs to queue.
    let difference = whole.saturating_sub(bad);
    let per_frame = difference.saturating_sub(hop) / 2;

    println!("\nwhat one frame costs, derived rather than assumed:");
    println!("  baseline - bad query   {:>10}", micros(difference));
    println!("  less one blocking hop  {:>10}", micros(hop));
    println!("  = two frames, so one   {:>10}", micros(per_frame));

    // A query answers with three frames: the descriptor, one row, and the complete.
    let frames = per_frame * 3;

    // Half a round trip: the reader hands the frame to the stream's task and does not
    // wait for it.
    let route = channel / 2;

    let accounted = transport + spawn + route + hop * 2 + frames + prepare + execute;

    println!("\naccounting for the baseline, at p50:");
    let line = |what: &str, cost: Duration| {
        println!(
            "  {what:<24} {:>10}   {:>5.1}%",
            micros(cost),
            cost.as_secs_f64() / whole.as_secs_f64() * 100.0
        );
    };

    line("socket round trip", transport);
    line("spawn the stream's task", spawn);
    line("route the frame to it", route);
    line("spawn_blocking x2", hop * 2);
    line("queue 3 frames out", frames);
    line("compile and plan", prepare);
    line("execute", execute);

    println!("  {:-<24} {:->10}", "", "");
    line("= accounted", accounted);
    line("end to end", whole);

    let residual = whole.saturating_sub(accounted);
    line("unaccounted", residual);

    println!(
        "\n{:.0}% of a small query is the runtime moving it between threads.",
        (spawn + route + hop * 2 + frames).as_secs_f64() / whole.as_secs_f64() * 100.0
    );
    println!(
        "{:.1}% is the work — compiling the query and running the plan.",
        (prepare + execute).as_secs_f64() / whole.as_secs_f64() * 100.0
    );
}

fn micros(elapsed: Duration) -> String {
    let micros = elapsed.as_secs_f64() * 1_000_000.0;

    if micros < 10.0 {
        format!("{micros:.2}µs")
    } else {
        format!("{micros:.1}µs")
    }
}

// ---- plumbing ----------------------------------------------------------------

fn connect(socket: &Path, schema: &Arc<Schema>) -> Connection {
    Connection::connect(socket, "code", Arc::clone(schema), Mode::ReadOnly, false)
        .expect("a connection")
}

/// Silence the unused-import warning for a re-export the frame helper needs.
const _: fn(&[u8]) -> Result<frame::FrameHeader, aperture_wire::WireError> = frame::decode_header;
