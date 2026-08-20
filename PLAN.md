# Fjord — the roadmap

What is not built, what building each piece requires, and the record of decisions already
taken so they are not re-litigated. The design of record is the
[design book](website/README.md) — **published** at <https://boxops-uk.github.io/fjord/> on
every push to main, and shipped with each release as an attested `fjord-docs-site.tar.gz`
beside the binaries; the working contract is [`AGENTS.md`](AGENTS.md); what has been
measured is [`bench/FINDINGS.md`](bench/FINDINGS.md). The history of how the system was
built lives in git, where it can be cited by commit.

**Definition of done, everywhere:** a task ends in a green test (prefer a property), and
every invariant a piece of work touches has its guard un-ignored and passing before the work
is done. When picking up an item, decompose it into task-sized leaves *at pickup* — early
decomposition is always wrong — each ending green, ordered by dependency and de-risking.

## What remains

| Work | State | Gated on |
|---|---|---|
| [File ingestion](#file-ingestion--fjord-write) | designed; format built and shared with the wire | nothing — the interning primitive it needed exists |
| [Stored derivation](#stored-derivation) | designed; two rules banked | the [re-derivation decision](#the-open-decision-re-derivation-vs-i11) |
| [The read-path benchmark](#the-read-path-benchmark-against-glean) | planned, with predictions | a quiet machine and the indexed corpus |
| [Authentication](#authentication) | design of record below; nothing built | wanting it |
| [The engine in a browser](#the-engine-in-a-browser--webassembly) | **the store split, `fjord-inspect`, `wasm/` and the lexer segment are built**; the remaining views are not | nothing |
| [Operational gaps](#operational-gaps) | each named with the seam that keeps it cheap | — |
| [Language backlog](#language-backlog) | additive; none reshapes the machine | — |

---

## File ingestion — `fjord write`

**Goal.** Facts writable from files, in parallel, against one database:
`fjord write <db> [FILE...]`. A throughput feature — nothing in the lifecycle, the CLI or the
runtime waits on it.

**The design is already built where it matters.** The block format is shared with the wire
(`fjord-wire::block` — sync marker, magic, fixed-width header fields, CRC over header and
payload, the predicate **named** rather than numbered) and *a file and a socket carry the
same bytes* is a test, not an intention (`tests/one_encoding.rs`). The ten-`0xFF` sync marker
is unreachable inside a payload **by the encoding rather than by luck** — UTF-8 never uses
`0xF8`–`0xFF`, a varint's final byte is below `0x80`, and the header's `count`/`length` are
capped to keep a zero top byte — so a scan from any offset finds boundaries and nothing else,
and validation (magic, then CRC) is for torn writes and flipped bits, not disambiguation.
This splittability is a real advantage over Glean, whose binary `Batch` is one opaque
sequential blob that cannot be split, so it parallelises across batches and pushes the
chunking decision onto the producer.

**What is left is the pipeline:** the file envelope (header: magic, format version,
producing-schema fingerprint; optional footer of block offsets), the splitter (seek anywhere
→ scan to next sync → hand blocks to workers, checked from *every* offset of a multi-block
file), and a pool of workers that decode blocks and `intern_block` them concurrently.

**Two acceptance criteria are inherited rather than owed** — shuffle-invariance
(`writer_count_and_write_order_do_not_change_the_database`) and deterministic rejection under
any interleaving (`a_conflict_between_concurrent_writers_fails_exactly_one_of_them`) are
proven on the wire path, one layer down.

**Do not build the stratum optimisation first.** Intern-as-the-decode-reaches-each-fact is
correct under any number of workers; the sort-into-strata design survives only as a plausible
optimisation of fjall's bulk `ingest()`, untested against a pipeline nobody has written.
Reach for it if `examples/ingest.rs` says the write path is the ceiling, not before.

**Acceptance:**
- [ ] Facts are writable from files in parallel, and queried back.
- [ ] Ingest is order-independent: shuffling input chunks yields the same DB *or* the same
      deterministic rejection (tier-2 metamorphic).
- [ ] Same-key-different-value is deterministically rejected regardless of chunking and
      worker interleaving.
- [ ] One fact encoding, not two: a block is byte-identical on the wire and in a file.

---

## Stored derivation

**Goal.** `predicate P : … = KEY where <query>` as **facts written at build time**. The half
of "derived facts" that never reaches the executor — at query time `P` is facts in a
keyspace, scanned like any other predicate — so this is a *writer* and a *lifecycle* piece,
not a machine change. The schema grammar reserves `stored` and `derive`; the derivation body
is deliberately not in the grammar yet (`nyi/derivation`).

**Lifecycle design of record:** `ops-I8` — create → ingest base → derive → finish; a deriver
reads the frozen base via a sealed snapshot and writes only derived predicates;
prefix-disjointness makes read/write disjointness structural; embarrassingly parallel, no
stratification at first. Derived-on-derived comes later via sealed rounds — the shape to copy
is a per-predicate completion list in the sidecar plus a topological sort of the derivation
graph, computing round boundaries from the schema instead of asking the operator.

**Identity:** `ops-I4`'s hash is over the canonical schema and the **base** facts, so derived
facts are implied by identity, never part of it — re-deriving must be reproducible.

**Two rules are banked in advance, both learned from Glean's source and expensive to
rediscover:**

- **Write the query's *results*, never the body's output.** The reorderer is free to place
  the fact-producing statement above a later filter, and the fact set accumulated while the
  query runs then contains facts that are not true — the results are still correct, which is
  why the fix is to write the results and filter the fact set by them. A deriver implemented
  as "run the plan and dump what the body produced" is wrong in exactly this way, and the
  acceptance test below is shaped so it fails.
- **Negation in a stored derivation is legal here, and that is a one-way door.** Glean
  forbids it because facts derived from the *absence* of facts can be invalidated when an
  incremental database grows. Nothing can be added to a Complete database, so nothing can
  invalidate a derived fact — the ban is a cost of incrementality, not of derivation. Pinned
  by a test so a later reader does not "restore" the ban by analogy. The door: if `ops-I9`
  (no cross-database anything) ever reopens into stacking or incrementality, every stored
  derivation containing `!` becomes unsound.

### The open decision: re-derivation vs I11

**The one genuinely open decision in the project, and it gates this work.** Dropping a
predicate's two trees is O(1) and is what the physical layout was chosen for — but the
allocator's high-water mark is recovered from the last key of the very `entities` tree being
deleted, so the next write to that predicate restarts at sequence 1 and **reuses ids that
dependent predicates still reference**. The failure is a silently wrong answer, not an error,
so the rule must be decided before this work writes a derived fact. Two coherent answers:

- **Re-derivation produces a new DB.** Matches the immutable-artifact philosophy, needs no
  new machinery, and means a one-predicate fix rebuilds everything.
- **In-place, but bounded.** Legal only on a Writable DB, and only with the dependent subtree
  dropped alongside — which additionally needs the high-water mark to survive the drop, and
  the data-recovered mark cannot.

Anything more permissive — re-deriving under live readers — needs generation metadata,
dependent invalidation and generation-aware cursors, and "an O(1) tree delete" should never
be read as promising it.

**Acceptance:**
- [ ] A schema declaring a derived predicate parses, derives, and the derived facts are
      queryable exactly as base facts are — indistinguishable to the executor.
- [ ] `ops-I8` enforced and tested: a deriver cannot read its own writes or another
      deriver's, and cannot write a predicate it does not own.
- [ ] Deriving twice from the same base gives the same facts (`ops-I4`); re-deriving one
      predicate drops and rebuilds only its trees — under whichever re-derivation rule is
      chosen above.
- [ ] **Only the query's results are written** — tested with a plan the reorderer is free to
      schedule the derive above the filter in.
- [ ] A stored derivation containing `!` derives and is queryable.
- [ ] Ingest is refused after `derive` in a way the lifecycle defines, not by accident.

---

## The read-path benchmark against Glean

The write paths are measured and within 8% on equal footing
([findings §15–§17](bench/FINDINGS.md)); the read paths are not. The suite is
[`bench/glean-read-path.md`](bench/glean-read-path.md): sixteen query families over two rungs
(in-process, and over each system's wire), the same 18.3M-fact corpus both systems already
hold from one Roslyn walk, reporting **work done beside every timing** — Glean's
`facts_searched` against our `Profile.examined` — because that separates *did more work* from
*did the same work slower*.

Three predictions it exists to check, each from a design document rather than a hope: the
scan curve against database size (2.4 GB against 886 MB for the same facts); what a value
read costs us (a second point read per row, I6, against Glean's inline value — the sharpest
prediction); and what a missing feature costs (transitive closure as one recursive Angle
query against a client-side loop of round trips — the strongest argument for building
recursion).

It also closes a long-carried item: `bench/baselines/<host>.json` and a `--json` flag on the
instruments, so a number can be re-run rather than re-argued.

---

## Authentication

**Nothing here is built.** This is the design of record, condensed so the shape is argued
once; the guards are named so they can be written up front.

**The current state, honestly:** `ops-I10` — no in-database auth; the transport is the trust
boundary — holds by being default-closed: a Unix socket unless somebody types `--listen-tcp`,
and whoever opens TCP takes on the gateway in front of it. What it does not answer is *who is
at the other end and what may they do*.

**The rule that shapes everything** (proposed `ops-I11`): **a principal is never content.**
No principal, credential, role or grant is stored as a fact, in a sidecar, or anywhere inside
a database directory. Authorization is *configuration* held by the server process; identity
is *attested*, held by the peer. The Vault-against-Postgres lease pattern cannot port —
`CREATE ROLE` needs a mutable principal namespace, and principals-as-facts would enter
`ops-I4`'s identity while `ops-I2` makes a Complete database unwritable — but what the lease
*buys* (short-lived, automatic, never at rest) is delivered by attested identity, where the
issuer is external and the server only verifies: a trust bundle, a clock, a policy. Testable
as: ingest under a policy and the content identity is byte-identical to the same ingest with
no policy.

**One `Principal`, three attestors:** `Peer { uid, gid, pid }` from `SO_PEERCRED` on the Unix
socket (kernel-attested, no crypto — the server already receives and discards this);
`Spiffe { id, expires_at }` from the URI SAN of a verified X.509-SVID; `Token { subject }`
verified against a JWKS; and `Anonymous`, in the enum deliberately, so "the port is reachable
by whoever can route to it" is a value a policy can refuse rather than an absence nothing can
express. Because the verifier is written against a *bundle*, SPIRE and an OpenBao PKI engine
fill the same socket — the issuer is replaceable by construction.

**mTLS costs zero protocol bytes, and that is the argument.** The identity is settled by the
TLS handshake before the first frame; `protocol::VERSION` does not move, and
`decode_startup` keeps refusing trailing bytes. The server must be the terminator — a gateway
that terminates TLS has consumed the certificate, and a forwarded identity is one the server
takes on trust from a hop it cannot verify — so `ops-I10` is not reversed but made real: the
trust boundary moves into the process that enforces it. Bearer tokens, if ever wanted, are a
**new frame kind on stream 0 before `STARTUP`** — the precedent every protocol extension has
followed — never a field appended to the startup payload.

**Authorization at `(database, mode)` and no finer**, evaluated **once at handshake** — the
same place `ops-I6` resolves the mode and `ops-I2` refuses a write to a sealed database — so
nothing enters the executor. Operator configuration, reloadable, never inside a database. Two
consequences taken with it: a database a principal may not see answers `UnknownDatabase`, not
a distinguishable refusal (anything else enumerates the catalogue); and `fjord.db.List` must
filter by principal — cheap, because the catalogue is answered at the `FactStore` seam before
anything the hot loop can see. Per-predicate authorization is priced (a principal enters
query compilation) and not taken; per-fact reopens `ops-I9` at ownership's price and is
**not a gap**.

**Revocation, honestly:** an SVID lives an hour and a session may live longer; authorization
is decided once, so a live session outlives the credential that opened it. The design offers
a maximum connection lifetime and a time bound on the viewer's pool, and states the residual
as **a bounded staleness window, not revocation**.

**What this contradicts today** (amendments to make when built): `ops-I10`'s "reserved
credential slot in the handshake" is retired — mTLS needs no bytes and a token needs a frame
kind — and the "accepts anonymous" notes in `fjord-server` and the CLI become
`Principal::Anonymous`, a value rather than an absence.

**Build order, if built:** a principal exists (SO_PEERCRED, nothing refuses anything) → a
policy that can refuse (loaded at `serve`, evaluated once, `UnknownDatabase` for the
invisible, the catalogue filtered) → mTLS (`--listen-tls`, default-closed on `--listen-tcp`'s
terms; acceptance: `VERSION` does not move and the .NET client still connects) → the Workload
API (rotate in place at half TTL) → connection lifetime. Guards to write up front:
`a_principal_is_never_written_to_a_database`,
`an_unauthorised_database_is_indistinguishable_from_a_missing_one`,
`the_catalogue_lists_only_what_the_principal_may_see`,
`the_dotnet_client_still_connects_at_protocol_version_2`.

---

## The engine in a browser — WebAssembly

**Built, as far as the front end's first two phases.** The store split is done,
`fjord-inspect` holds the token and parse-tree views, `wasm/` builds a 72 KB
module (29 KB over the wire), and `web/` is a React site that lexes *and*
parses on every keystroke, with one span highlighting across every view. What
is left is the phases that need a schema, and the decisions only they can
settle.

**The goal, unchanged.** The design book's interactive segments run the real
lexer, parser, typechecker, planner, executor and transport codec, compiled to
`wasm32-unknown-unknown` — not a JavaScript imitation of them. The boundary
carries **JSON of the constructs, not a rendered string**, because a page that
receives structure can lay it out and a page that receives text can only print
it.

### Movement 1 — the seam becomes a crate, and each implementation its own ✅

Three crates replace `fjord-store`. It keeps its name and becomes **the
abstraction**; each implementation has its own crate, which is what makes a
third backend additive rather than a refactor.

| Crate | Holds |
|---|---|
| `fjord-store` | the seam: `fact_store`, `error`, `fact`, `format`, `keys`, and the shared test support (`fixture`, `fixtures`) |
| `fjord-store-mem` | `MemStore`, no longer test-gated |
| `fjord-store-fjall` | `FjallDb`, `FjallStore`, `Staged`, `FjallScan`, `lookup_cache`, and the lifecycle: `catalog`, `meta`, `schema_doc`, `identity`, `ulid` |

**The error split went as designed, with one addition.** Eleven variants stayed
on the seam and ten moved to `CatalogError` in `fjord-store-fjall`, which
carries `Store(#[from] StoreError)` so a seam fault still bubbles through one
`?`. `StoreError::Backend` is now `Box<dyn Error + Send + Sync>`, constructed
through `StoreError::backend` — `#[from]` cannot do that job, because a blanket
`From<E: Error>` would swallow every other error in the crate. The addition:
two sites that used `StoreError::Meta` for something that was never a sidecar —
a malformed id reservation in the `meta` keyspace, and a virtual catalogue
predicate that disagrees with its declaration — now box a **local** error
through `Backend`. That is the seam-correct reading: from the trait's side,
this backend failing to be what it wrote *is* the backend failing.

**What the ripple actually cost**, against the estimate of seventy-five
references: about that, and nothing surprising in them. `ServerError` and
`CliError` each gained a `Catalog` arm, and the server's `code()` — the one
place the split is visible on the wire — routes lifecycle refusals from
`CatalogError` and delegates `CatalogError::Store` to the same function the
seam's own arm uses, so no client is told anything different.

**The trap the extraction found, exactly where AGENTS.md says it lives:**
`fjord-store`'s own unit tests could no longer use `MemStore`, because a
dev-dependency on `fjord-store-mem` links a *second copy* of `fjord-store` and
the two `FactStore`s are then different types. Those tests moved with `store.rs`
into `fjord-store-fjall`, where both implementations are ordinary dependencies.
One consequence to remember: `fjord-store-fjall` dev-depends on **itself** with
`features = ["proptest"]`, because the witnesses its guards need
(`open_snapshots`, `table_counts`, `flush_to_tables`) are feature-gated and an
integration test is a separate crate. While those guards lived in `fjord-store`
the feature arrived by unification from the engine's dev-dependency.

### Movement 2 — `fjord-inspect`: a JSON view of every construct ◐

The crate exists, with `Tokens` built and the rest to come.

| View | Built from | State |
|---|---|---|
| `Tokens` — `{kind, class, span, text}` + diagnostics | `lexer::tokenize` | ✅ |
| `Tree` — a dense `{id, kind, token, label, span, children}` | the **CST**, through `cst::CstNode` | ✅ |
| `Lowered` — the same shape, one tree in | `Ast` with spans from `print::spanned` | to build; **needs a schema**, which is the next decision below |
| `Types` — per node, and the head | `Typed::ty`, `Compilation::head_ty` | to build |
| `Diagnostics` — code, message, labels | the sink, through `Diagnostics::in_source_order` | ✅ for every phase that reports without a schema |
| `PlanView` — steps, levels, seek keys, residuals, projections, fingerprint | mirrors the walk in `print::plan` | to build |
| `Rows` and `ProfileView` | `fixtures::collect_rows` and `iter::enumerate_profiled` over a `MemStore` from `fixture::facts()` | to build |
| `WireView` — frames, blocks, and a hex dump annotated by offset | `fjord_wire::{frame, block, value, protocol}` | to build |
| `SchemaView` — predicates, canonical form, identity, compatibility | `fjord_schema::{syntax, print, fingerprint}` | to build |

**The split between the two trees is the thing to keep straight.** The parse
view is the *concrete* tree — the "lossless, untyped, grammar-shaped tree with
spans and text" the book's phase table promises — and it needs **no schema**,
which is why it could ship now: lowering resolves names against one, parsing
does not. So a browser can show it for any text at all, including the
half-typed text an interactive view spends most of its time on. The lowered
tree, the types and the plan all wait on the same thing: a schema in the page.

**Two properties, and the second was a surprise.** A node's span contains every
child's — a view that widened one by a byte would still look plausible and
would highlight the wrong text. And the leaves *do* reassemble the source: the
grammar's `skip Whitespace` keeps trivia out of what the parser matches on, not
out of the tree, so the same view drives both panes.

**One decision made while building the first view, worth keeping.** A token
carries a `class` (keyword, predicate, variable, field, …) as well as its
`kind`, and the class is decided in Rust. A page styles what the language says a
token *is* and never re-decides it — which is what stops the highlighter growing
back in TypeScript. Both mappings are exhaustive `match`es with no wildcard, so
a token added to sigla does not compile until somebody says what it is called
and what it is.

**`tokens_json` lives in `fjord-inspect`, not in the shell.** The JSON a browser
receives is then the string the host suite asserts on, and
`a_view_is_the_same_json_on_the_host_and_in_wasm` is a consequence of there
being one encoder rather than a claim needing a test.

### Movement 3 — `fjord-wasm`: the shell, and nothing else ✅

`wasm/` at the repository root with its own `[workspace]` table, a `cdylib`
whose every export takes a `&str` and returns a `String` of JSON, and no logic —
`tokens` is one forwarding call. `scripts/build-wasm.sh` runs cargo, then
`wasm-bindgen --target web`, then `wasm-opt -Oz` if binaryen is installed, and
prints the byte size. It refuses to run if the `wasm-bindgen` CLI and crate
versions differ, because that mismatch fails with a message about a section
rather than about versions.

**How the artifact reaches the site, decided:** built, not committed.
`web/src/wasm/` is gitignored, the page says so when the module is absent, and
it does **not** degrade to a JavaScript highlighter — the highlighter is the
thing being replaced, and a fallback would hide exactly the failure that
matters.

### What is left

- **A schema in the page**, which is what everything left waits on. `Compilation`
  takes `(&str, &Schema)` and `fjord_schema::syntax::read` builds one from text
  with no filesystem in reach, so the shape is a second editor holding a schema
  — the sample schema by default. That unblocks the lowered tree, the types,
  the diagnostics with codes, and the plan.
- **The remaining views**, in the order a reader meets them: `Lowered` and
  `Types`, then `PlanView`, which is the argument for the whole exercise.
- **Retire the hand-written highlighter** in `website/assets/app.js` once the
  new site carries the book's code samples. Until then two highlighters exist,
  which is the state this work is meant to end.
- **CI.** Nothing in `.github/workflows/release.yml` builds `wasm/` or `web/`
  yet, and Pages accepts one artifact per run — so publishing the interactive
  site means either building it into a subdirectory of `website/site/` after
  `build.py` (which `rmtree`s that directory first, so order is load-bearing) or
  staging both. The cheap first step is a `test`-job step running
  `cargo check -p fjord-engine --target wasm32-unknown-unknown`, which is what
  `dependency_closure` cannot prove on its own.
- **A virtual import resolver**, so browser schemas are not single-file:
  `syntax::resolve` reads files, and everything else in `fjord-schema` is clean.
- **`ts-rs` behind a feature**, so `web/src/wasm.ts`'s types are generated from
  the view structs instead of stated a second time.
- **Ingest stays impossible in a browser**, and that is not a gap: interning
  needs a real backend and durable id claims.


## Operational gaps

Each is a *specified* absence with the seam that keeps it cheap — none is an oversight.

| Gap | The seam kept |
|---|---|
| `db backup` / `restore` | A Complete database is a tar-able directory; the commands would wrap what operations already documents. Also the row where copy-on-start reader scaling lands |
| `db verify` | Recomputing the content fingerprint is cheap and specified; the two structural at-rest checks to add are I1 and I12 after a crash-and-recover |
| Per-predicate stats, and `:stat` | An **exact** O(1) count per predicate exists unread — per-predicate keyspaces plus insert-only make fjall's `approximate_len()` reliable. Surface as a virtual predicate (the `fjord.db.List` shape); record at `finish` into the versioned sidecar. Spend it on pruning, not join ordering |
| Server-side reference expansion | A **flag on the query message, not a fourth query kind** — expansion stays orthogonal to paging, profiling, counting. Collapses depth-many round trips into one, which is what makes `--expand` usable over TCP; the predicate allowlist is the better dial than depth. The client-side path stays (it is what makes `:expand` retroactive) |
| A deadline on the cancellation stride, and a byte budget in the chunk accumulator | The cost model charges one budget (rows per page); a pathological query can only be stopped by whoever holds the token, which on a shared server is an availability hole. A coarse monotonic read every 4096 rows is free at our row costs |
| Per-stream flow-control windows | Bounded per-stream queues + connection backpressure in the meantime |
| Retention | `db rm` exists and the filesystem is the catalog, so a policy is a caller, not a mechanism. "Keep the newest *n* Complete instances" is the shape |
| Provenance / freeform properties | The sidecar format is versioned; both are descriptive-only under `ops-I4` |
| Shell completions | `fjord completions <shell>` is specified in the CLI design |
| Fair cross-database write scheduling, fair fan-out merge | Both arrive with multi-database work; the fairness that exists is the other axis (`outbound` interleaves streams within a connection) |
| fjall keyspace tuning | **Measure, do not assume.** Options are fixed at creation, so a comparison builds a database per setting; needs a real-scale corpus. Until then fjall's defaults are the answer |
| `hasRefs` precomputed per predicate | Consulted before walking a fact's references; prerequisite for cheap expansion, not an alternative to it |

---

## Language backlog

Additive — each is an enum arm, a token, or a compile rule; none reshapes the machine. A
construct may add a `Source`, a `Test`, a residual op or a `Computed` arm — never a `Step`.
**Additive is not the same as small**: anything that touches the resume token or freezes
bytes on disk gets acceptance criteria, not a bullet.

- **Recursion / transitive closure.** The largest capability gap against Glean; today it is a
  client-side loop of round trips. The read-path benchmark prices it.
- **Aggregation.** `count` exists as a query kind (`--count`) without entering the language;
  aggregation proper materialises, which is the one thing that cannot be made suspend-free.
- **`distinct` via adjacency.** Deduplicating on the witness tuple is provably a no-op;
  deduplicating on the projected row needs O(distinct) cursor state — except when the
  projected fields are a **prefix of the output order**, where duplicates are adjacent and
  one row of state suffices. Compile under that condition, refuse with a named diagnostic
  otherwise. `--count` with distinct is the same mechanism.
- **A sargeable order comparison.** `<`/`>` on a leading key field denotes one contiguous run
  of the key order — unlike a denial there *is* a seek form. Filters today.
- **if-then-else.** `(C; T) | (!C; E)` is the desugaring and needs no machinery.
- **`maybe` / `enum`.** Sugar over a union (built); each waits on a *naming* decision, since
  what they desugar to enters the fingerprint.
- **Arrays / sets.** The multiplicity decision (below) stands: one fact per element until
  stored derivation exists to explode an array into a seekable index. The codec reserves the
  band; length-prefix vs terminator is the real one-way door.
- **`evolves` + query-time projection.** The compatibility checker is structured around a
  canonical-model diff, not just hashes, so field-level compatibility has its seam.
- **Pattern-pushing** — what is left of `pattern = pattern`: a left side that is not a target
  (`gen = gen`, `Y.name = X`). If unification ever lands after disjunction, import Glean's
  rule that it must be branch-local.
- **Intra-row repeated variables** (`{from = X, to = X}`) — needs a same-row `EqField`
  residual; rejected by name until something else wants the operator.
- **Row polymorphism** in the typechecker — an inference capability with no invariant
  attached and nothing waiting on it.
- **Block-local back-references** in the wire format — a pure encoding win (naming a fact by
  ordinal within a block) over a semantics that is decided; deliberately not first.

---

## Settled decisions — recorded so they are not reopened

Each entry is the decision and the reason it went that way. Reopening one means arguing with
the reason, not rediscovering it.

- **Parallel writes to a Writable database — yes, behind a striped merge frontier.** The
  serial writer was never required: `ops-I4`'s identity is a multiset hash over each fact's
  logical form (order-independent by construction), and `ops-I5` asks for one *pipeline*, not
  one thread. What the single thread actually held was I12's write-once half, and that now
  has a mechanism — per-key exclusion striped by `hash(predicate ++ key)`, no lock ordering
  needed because interning is bottom-up and critical sections are never nested. The stripe is
  held across the read *and* the commit (a batch is atomic on recovery, not isolated from
  readers), so per-key exclusion is the weakest sufficient mechanism and a lock-free CAS
  would not do. Commits do not parallelise (fjall's journal mutex) — accepted, because the
  expensive half was the redundant point reads. Do not restore the single writer, and do not
  add a conflict rule that picks a winner — that is the one thing `ops-I4` really forbids.
- **Per-block commits — a `serve` flag, off by default, gated on the durable id claim.** A
  durability trade should be something somebody typed, not a config entry or a create-time
  property (two identical databases must not differ in metadata because of how fast somebody
  wrote them). The nearly-missed failure: a lost batch let the allocator resume *below* a
  stranded id and reissue it, so a surviving reference resolved to the wrong target through a
  `finish` that looked like it checked this — hence ids are claimed in `meta` before use, and
  the worst outcome is back to "cannot seal". The honest statement, everywhere it appears: *a
  crash during ingest may cost the index, never its correctness.*
- **Multiplicity — one fact per element; `nyi/array` names the decision.** Glean's array
  story works *because* stored derivation explodes an array into a seekable index; arrays
  before stored derivation ship the storage win and none of the query mitigation.
- **A client never computes a fingerprint — it carries the number as a constant.** The
  fingerprint is a provenance tag; the byte-identical golden is what actually guards the
  shapes, and it is the stronger check. Generating clients from the schema is the proper end
  state and would change the golden's role in the same breath.
- **Predicate ids belong to the database, not the schema text.** No assignment that is a
  function of the text satisfies reproducibility, layout-independent identity and
  "adding a predicate is compatible" at once — so the map is assigned at create, embedded,
  append-only for life, and **the wire carries names** (once per block), so the numbering
  never leaves the database and a fact file is portable to any database declaring those
  names.
- **A reference on the way in is the target fact, written inline** (or an id the producer
  already holds); stored, a reference is a `FactId` and nothing else. Every id-based
  alternative makes the *producer* keep a map from every entity to its assigned identity plus
  an emission order respecting it. Interning is resolve-or-create, bottom-up, total because a
  reference in a key cannot be cyclic; "already there" is `ops-I5`'s silent dedup and
  "disagrees" is its same-key-different-value reject — no new rules. The cost is stated: a
  reference costs the target's whole fact on the wire, per occurrence.
- **Storage codec and transport codec are distinct, and the transport is bidirectional.**
  They share no bytes; only the inbound direction has a reference that is not an id.
- **Schema compatibility is subset containment.** The only compatible change is adding a
  predicate; any in-place change is Breaking until `evolves` exists — which is what makes the
  check unable to fail the way a richer one can.
- **Primitives — comparisons and arithmetic are built.** A comparison is a byte-compare
  residual (I1 makes encoded order value order); arithmetic is the first producer of
  `Step::Derive`. Angle's primitive surface is narrower than it sounds (15 ops); what sigla
  still lacks is if-then-else and element iteration, and the second is the multiplicity
  decision, not this one.
- **`pattern = pattern` — the gate is the left side's shape alone.** Most of what was filed
  as unification was not: reading before binding is *ordering* (`reorder`), a constant is
  *substitution* (the fold), a place is an *alias*, a prefix is a *constraint* applied by the
  capturing level, a record of targets is *destructuring*. Both spellings of each compile to
  the same plan, pinned by paired corpus entries. Typecheck no longer asks "was this
  mentioned above" — that decided in source order, the one order the query might not have
  used. A literal leaf on the left of a destructuring is refused because it binds nothing and
  would mean `true` where it means the empty relation.
- **Intra-row repeated variables — rejected by name** (`nyi/repeated-variable`). Repeated
  *reads* of an outer variable are ordinary splices; only a repeated *capture* is refused.
- **Cancellation counts rows examined, and the counter belongs to the run.** As a local it
  reset per call and a plan whose rows all matched never polled the token. The bounded
  overrun a stride buys is the intended trade, documented on the constant.
- **`FactRef` has its own fixed-width marker** (`0x51`) — a value's bytes are
  self-describing without the schema and the `Int`/`Fact` distinction is byte-level.
- **The on-disk format version is two numbers** (`codec`, `storage`) in a metadata keyspace,
  checked for **equality** at open; an unstamped database holding facts is refused rather
  than adopted. It makes nothing migratable — a future codec is a different number rather
  than an impossibility. The resume cursor versions separately, against the build.
- **Union types (the one-way doors, taken together):** explicit append-only discriminants
  (I10); a union is a **terminated group** in the codec so `skip` needs no notion of a value
  still owed; a `FieldPath` step at a union position is the expected discriminant, checked
  before any payload read; every union edit is Breaking in `schema diff`, appending included.
- **Negation in a stored derivation will be legal** — see
  [stored derivation](#stored-derivation); the ban Glean carries is a cost of incrementality
  this design does not pay, and reopening `ops-I9` is what would make it unsound.
