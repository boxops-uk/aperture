//! **The write rung** — Phase 12a, and the first instrument in this project that
//! measures a database being *written* rather than read.
//!
//! Every rung S1–S7 measures reading, against a sealed corpus. That left the one number
//! the write path is judged by — facts per second — attributable to nothing:
//! [findings §12](../bench/FINDINGS.md) had to be read off the .NET indexer's own report,
//! which bundles Roslyn, a socket, the server's framing and the funnel into one figure.
//! This separates them, in process, with no tokio and no wire.
//!
//! # The ladder, and what each rung subtracts
//!
//! | layer | resolves | reads the LSM | commits | decodes |
//! |---|---|---|---|---|
//! | `commit` | no | no | yes | no |
//! | `intern` → `dedup:warm` | yes | **no** — every key is cached | no | no |
//! | `intern` → `dedup:cold` | yes | yes | no | no |
//! | `intern` → `create` | yes | yes | yes | no |
//! | `block` | yes | yes | yes | **yes** |
//!
//! Read the differences rather than the rows:
//!
//! - `create` − `dedup:cold` is what **committing** costs inside interning, which is the
//!   term [12f](../PLAN.md) proposes to cut by batching per block instead of per fact.
//! - `dedup:cold` − `dedup:warm` is what the **cache removes** — the same claim
//!   `interning_reads_a_key_once_however_many_references_name_it` makes as a count, priced.
//! - `block` − `create` is the transport codec, which is the only part of the write path
//!   the read ladder already had an opinion about.
//!
//! # Why a write bench needs a database per iteration
//!
//! Ingest is not idempotent in the way a query is: the second run of the same facts
//! creates nothing. So an iteration cannot re-run against its own output, and every timed
//! run gets a fresh directory. Two consequences worth stating because they are easy to
//! get wrong and invisible when you do:
//!
//! - **Keyspace creation is excluded.** A pair of trees costs ~30 ms and the source layer
//!   needs four pairs, so lazily creating them inside the timed region would put ~120 ms
//!   of setup into a one-second measurement. [`FjallDb::create_predicates`] exists for
//!   this and is called before the clock starts.
//! - **The counts are checked against a closed form, not against the previous run.**
//!   [`Corpus`] states what it costs ([`Corpus::facts`], [`Corpus::interns`]) and a test
//!   proves that statement against a real store, so a run that writes a different number
//!   of facts aborts — including the first one, which a probe-and-hold discipline cannot
//!   check.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aperture_cli::{code_index, workload::Corpus};
use aperture_encoding::tuple::{Value, encode_key};
use aperture_ingest::{intern_block, intern_fact};
use aperture_schema::schema::{PredicateId, PredicateTy, Schema};
use aperture_store::store::FjallDb;
use aperture_wire::encode_block;

fn main() {
    let options = match parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("ingest: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let schema = code_index::schema();
    let corpus = options.corpus;

    println!("host     {}", host());
    println!("scratch  {}", options.scratch.display());
    println!("corpus   {}", corpus.describe());
    println!(
        "runs     best of {}, keyspaces created before the clock starts",
        options.iterations
    );
    println!(
        "commits  {}",
        if options.per_block {
            "once per block"
        } else {
            "once per fact"
        }
    );
    println!();
    println!(
        "{:<16}{:>12}{:>14}{:>14}{:>12}{:>10}",
        "layer", "ms", "facts/s", "interns/s", "reads/fact", "cache"
    );

    let mut rows = Vec::new();
    if options.layer.runs(Layer::Commit) {
        rows.push(commit(&options, &schema));
    }
    if options.layer.runs(Layer::Intern) {
        rows.extend(intern(&options, &schema, false));
    }
    if options.layer.runs(Layer::Block) {
        rows.extend(intern(&options, &schema, true));
    }

    for row in &rows {
        println!("{}", row.render());
    }

    println!();
    println!("{}", differences(&rows));
}

const USAGE: &str = "\
usage: cargo run --release --example ingest -- [options]

  --layer LAYER        all (default) | commit | intern | block
  --iterations N       timed runs per layer, best reported (default 3)
  --scratch PATH       where the throwaway databases go (default: a temp dir)
  --files N            files in the corpus (default 100)
  --modules N          modules per file (default 2)
  --decls N            declarations per module (default 20)
  --refs N             references per declaration (default 5)
  --per-block          commit once per block, as `serve --commit-per-block` does

The four fanouts set the interns-per-fact ratio, which is what interning's cost is
decided by. The real index sits at 3.8; the default here is 4.6.
";

struct Options {
    layer: Which,
    iterations: usize,
    scratch: PathBuf,
    corpus: Corpus,
    /// Commit once per block, as `serve --commit-per-block` does.
    per_block: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Commit,
    Intern,
    Block,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    All,
    One(Layer),
}

impl Which {
    fn runs(self, layer: Layer) -> bool {
        match self {
            Which::All => true,
            Which::One(only) => only == layer,
        }
    }
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        layer: Which::All,
        iterations: 3,
        scratch: std::env::temp_dir().join("aperture-ingest-rung"),
        corpus: Corpus::standard(),
        per_block: false,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        let count = |arg: &str, got: String| -> Result<u64, String> {
            got.parse().map_err(|_| format!("{arg} wants a number"))
        };

        match arg.as_str() {
            "--layer" => {
                options.layer = match value()?.as_str() {
                    "all" => Which::All,
                    "commit" => Which::One(Layer::Commit),
                    "intern" => Which::One(Layer::Intern),
                    "block" => Which::One(Layer::Block),
                    other => return Err(format!("no such layer: {other}")),
                }
            }
            "--iterations" => {
                options.iterations = value()?
                    .parse()
                    .map_err(|_| "--iterations wants a number")?;
            }
            "--scratch" => options.scratch = PathBuf::from(value()?),
            "--files" => options.corpus.files = count("--files", value()?)?,
            "--modules" => options.corpus.modules_per_file = count("--modules", value()?)?,
            "--decls" => options.corpus.decls_per_module = count("--decls", value()?)?,
            "--refs" => options.corpus.refs_per_decl = count("--refs", value()?)?,
            "--per-block" => options.per_block = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if options.iterations == 0 {
        return Err("--iterations must be at least 1".to_owned());
    }
    if options.corpus.facts() == 0 {
        return Err("that corpus holds no facts".to_owned());
    }

    Ok(options)
}

/// One measured row.
struct Row {
    layer: &'static str,
    elapsed: Duration,
    /// Facts written, so a rung that writes nothing reports no rate rather than zero.
    created: u64,
    /// Resolve-or-create calls.
    interns: u64,
    /// Live `keys` reads per fact created, or per intern where nothing was created.
    reads: Option<f64>,
    hit_rate: Option<f64>,
}

impl Row {
    fn render(&self) -> String {
        let seconds = self.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let rate = |n: u64| {
            if n == 0 {
                "-".to_owned()
            } else {
                thousands((n as f64 / seconds) as u64)
            }
        };

        format!(
            "{:<16}{:>12.1}{:>14}{:>14}{:>12}{:>10}",
            self.layer,
            self.elapsed.as_secs_f64() * 1000.0,
            rate(self.created),
            rate(self.interns),
            self.reads
                .map_or_else(|| "-".to_owned(), |reads| format!("{reads:.2}")),
            self.hit_rate
                .map_or_else(|| "-".to_owned(), |hit| format!("{hit:.1}%")),
        )
    }
}

/// A throwaway database, its keyspaces already built.
///
/// The directory is removed first rather than after: a run that panicked leaves one
/// behind, and adopting it would measure an ingest that dedups against a previous run's
/// facts — which looks like a very fast write path.
fn scratch(options: &Options, name: &str, schema: &Schema) -> (PathBuf, FjallDb) {
    let path = options.scratch.join(name);
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch directory");

    let db = FjallDb::open(&path).expect("a database");
    // Every declared predicate, not only the four this corpus writes: an id is a
    // *position* in the schema, so asking for all of them is the same thing `create`
    // does and keeps the setup independent of which fanouts a run chose.
    let declared = (0..schema.len())
        .map(|n| PredicateId(u32::try_from(n).expect("a schema id fits")))
        .filter(|id| !schema.is_virtual(*id));
    db.create_predicates(declared).expect("the trees");
    (path, db)
}

/// **The floor**: `put_fact` on distinct keys, resolving nothing.
///
/// A different corpus by necessity — `put_fact` takes encoded bytes, and encoding a key
/// that holds a reference presupposes the interning this layer is defined by not doing.
/// So it writes `src.File` keys, one string apiece, as many as the corpus has facts. What
/// it prices is the part every other row also pays: an id from the allocator, two tree
/// inserts, one batch commit through fjall's journal.
fn commit(options: &Options, schema: &Schema) -> Row {
    let predicate = code_index::id("src.File");
    let facts = options.corpus.facts();
    let keys: Vec<Vec<u8>> = (0..facts)
        .map(|n| {
            encode_key(
                &PredicateTy::Str,
                &Value::Str(format!("src/dir{}/file{n}.cs", n % 16)),
            )
            .expect("a key")
        })
        .collect();

    let mut best: Option<Duration> = None;
    for iteration in 0..options.iterations {
        let (path, db) = scratch(options, &format!("commit-{iteration}"), schema);

        let started = Instant::now();
        for key in &keys {
            db.put_fact(predicate, key, &[]).expect("a write");
        }
        let elapsed = started.elapsed();

        // `put_fact` is the primitive below the funnel, so nothing here should have
        // resolved anything. A read would mean this layer is measuring the one above it.
        assert_eq!(
            db.intern_read_counters(),
            (0, 0),
            "the commit floor must not read"
        );
        best = Some(best.map_or(elapsed, |had: Duration| had.min(elapsed)));

        drop(db);
        let _ = std::fs::remove_dir_all(&path);
    }

    Row {
        layer: "commit",
        elapsed: best.expect("at least one iteration"),
        created: facts,
        interns: 0,
        reads: Some(0.0),
        hit_rate: None,
    }
}

/// The corpus through the funnel: create, then dedup warm, then dedup cold.
///
/// All three in one function against one database because they are only meaningful in
/// that order — a dedup pass needs a store that already holds the facts, and a *cold*
/// one needs a handle that has not seen them.
fn intern(options: &Options, schema: &Schema, through_blocks: bool) -> Vec<Row> {
    let corpus = options.corpus;
    let emissions = corpus.emit(schema);
    let blocks: Vec<Vec<u8>> = emissions
        .iter()
        .map(|emission| {
            let mut bytes = Vec::new();
            encode_block(&mut bytes, schema, emission.predicate, &emission.facts).expect("a block");
            bytes
        })
        .collect();

    let label = |what: &'static str| {
        if through_blocks {
            match what {
                "create" => "block:create",
                "dedup:warm" => "block:dedup:warm",
                _ => "block:dedup:cold",
            }
        } else {
            what
        }
    };

    // One pass over the whole corpus, however it is being fed in. A free function
    // rather than a closure because it has to serve two different sinks — the database
    // itself, and a `Staged` writer standing in front of it.
    fn send<S: aperture_ingest::FactSink>(
        sink: &S,
        schema: &Schema,
        emissions: &[aperture_cli::workload::Emission],
        blocks: &[Vec<u8>],
        through_blocks: bool,
    ) -> (u64, u64) {
        let (mut created, mut interns) = (0u64, 0u64);
        if through_blocks {
            for bytes in blocks {
                let out = intern_block(sink, schema, bytes).expect("the block ingests");
                created += out.created as u64;
                interns += out.seen() as u64;
            }
        } else {
            for emission in emissions {
                for fact in &emission.facts {
                    let out = intern_fact(sink, schema, fact).expect("it ingests");
                    created += out.created as u64;
                    interns += out.seen() as u64;
                }
            }
        }
        (created, interns)
    }

    let pass = |sink: &FjallDb| send(sink, schema, &emissions, &blocks, through_blocks);

    let mut best: Option<[Measured; 3]> = None;
    for iteration in 0..options.iterations {
        let name = format!("intern-{}-{iteration}", u8::from(through_blocks));
        let (path, db) = scratch(options, &name, schema);

        // The commit is *inside* the timed region, since it is the cost being measured.
        let create = if options.per_block {
            measure(&db, || {
                let staged = db.staged();
                let out = send(&staged, schema, &emissions, &blocks, through_blocks);
                staged.commit().expect("the block commits");
                out
            })
        } else {
            measure(&db, || pass(&db))
        };
        check(&create, corpus.facts(), corpus.interns() - corpus.facts());

        // The dedup passes create nothing, so they commit nothing either way — which is
        // what makes `create` the only row the flag can move.
        let warm = measure(&db, || pass(&db));
        check(&warm, 0, corpus.interns());

        drop(db);
        let db = FjallDb::open(&path).expect("reopen");
        let cold = measure(&db, || pass(&db));
        check(&cold, 0, corpus.interns());

        let run = [create, warm, cold];
        // Best-of by the *creating* pass, so the three rows come from one run and stay
        // comparable with each other rather than being three different runs' minima.
        if best
            .as_ref()
            .is_none_or(|had| run[0].elapsed < had[0].elapsed)
        {
            best = Some(run);
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&path);
    }

    let best = best.expect("at least one iteration");
    let [create, warm, cold] = best;
    vec![
        create.row(label("create")),
        warm.row(label("dedup:warm")),
        cold.row(label("dedup:cold")),
    ]
}

/// One pass's outcome: the clock, and every counter that moved during it.
struct Measured {
    elapsed: Duration,
    created: u64,
    interns: u64,
    key_reads: u64,
    hits: u64,
    misses: u64,
}

impl Measured {
    fn row(&self, layer: &'static str) -> Row {
        let per = if self.created > 0 {
            self.created
        } else {
            self.interns
        };

        Row {
            layer,
            elapsed: self.elapsed,
            created: self.created,
            interns: self.interns,
            reads: (per > 0).then(|| self.key_reads as f64 / per as f64),
            hit_rate: (self.hits + self.misses > 0)
                .then(|| 100.0 * self.hits as f64 / (self.hits + self.misses) as f64),
        }
    }
}

/// Run one pass, taking the counters as deltas so a second pass against the same handle
/// reports its own reads rather than the run's total.
fn measure(db: &FjallDb, pass: impl FnOnce() -> (u64, u64)) -> Measured {
    let before_reads = db.intern_read_counters();
    let before_cache = db.lookup_counters();

    let started = Instant::now();
    let (created, interns) = pass();
    let elapsed = started.elapsed();

    let after_reads = db.intern_read_counters();
    let after_cache = db.lookup_counters();

    Measured {
        elapsed,
        created,
        interns,
        key_reads: after_reads.0 - before_reads.0,
        hits: after_cache.0 - before_cache.0,
        misses: after_cache.1 - before_cache.1,
    }
}

/// **Reproduce or abort.** A pass that wrote a different number of facts than the corpus
/// says it holds did not measure the corpus, and the rate printed for it would be a real
/// number describing nothing.
fn check(pass: &Measured, created: u64, deduped: u64) {
    assert_eq!(
        (pass.created, pass.interns - pass.created),
        (created, deduped),
        "the pass wrote {} and deduped {}, against a corpus stating {created} and {deduped}",
        pass.created,
        pass.interns - pass.created,
    );
}

/// The subtractions the table exists for, spelled out so a reader is not doing
/// arithmetic on two columns to find the answer.
fn differences(rows: &[Row]) -> String {
    let find = |name: &str| rows.iter().find(|row| row.layer == name);
    let mut out = Vec::new();

    if let (Some(create), Some(cold)) = (find("create"), find("dedup:cold")) {
        out.push(format!(
            "committing        {:>8.1} ms of create's {:.1} ms  (create − dedup:cold)",
            (create.elapsed.as_secs_f64() - cold.elapsed.as_secs_f64()) * 1000.0,
            create.elapsed.as_secs_f64() * 1000.0,
        ));
    }
    if let (Some(cold), Some(warm)) = (find("dedup:cold"), find("dedup:warm")) {
        out.push(format!(
            "the cache saves   {:>8.1} ms per pass          (dedup:cold − dedup:warm)",
            (cold.elapsed.as_secs_f64() - warm.elapsed.as_secs_f64()) * 1000.0,
        ));
    }
    if let (Some(block), Some(create)) = (find("block:create"), find("create")) {
        out.push(format!(
            "block decode      {:>8.1} ms per pass          (block:create − create)",
            (block.elapsed.as_secs_f64() - create.elapsed.as_secs_f64()) * 1000.0,
        ));
    }

    if out.is_empty() {
        "differences need more than one layer — run --layer all".to_owned()
    } else {
        out.join("\n")
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

fn host() -> String {
    format!(
        "{} cores, {}",
        std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
        std::env::consts::OS,
    )
}
