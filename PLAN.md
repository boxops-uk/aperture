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

**Built, end to end through compilation.** The store split is done,
`fjord-inspect` holds the token, parse-tree, lowered and plan views, `wasm/`
builds a 280 KB module (117 KB over the wire), and `web/` is a React site that
lexes, parses, lowers, typechecks, flattens and reorders on every keystroke
against a schema the reader can edit — ending in the plan the executor would
run. What is left is *running* it.

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
| `Lowered` — `{id, kind, label, ty, span, children}` plus the statement list | `Ast`, walked from the head and the body, with `Typed::ty` beside it | ✅ |
| `SchemaView` — predicates and their declared types | `syntax::{parse, lower}`, typed by `print::signature` | ✅ enough for the page; canonical form and compatibility are not shown |
| `Tokens`, for the **schema** language | `fjord_schema::syntax::lexer` — a second lexer, not a second reading of the first | ✅ |
| `Types` — per node, and the head | `Typed::ty`, `Compilation::head_ty` | ✅ folded into `Lowered` — a type is an annotation *on* a node, and a second panel would make a reader align two lists by hand |
| `Diagnostics` — code, message, labels | the sink, through `Diagnostics::in_source_order` | ✅ for every phase that reports without a schema |
| `PlanView` — steps, levels, seek keys, residuals, projections, fingerprint | `print::steps` and `print::head`, with structure around the engine's own text | ✅ |
| `Rows` and `ProfileView` | `fixtures::collect_rows` and `iter::enumerate_profiled` over a `MemStore` from `fixture::facts()` | to build |
| `WireView` — frames, blocks, and a hex dump annotated by offset | `fjord_wire::{frame, block, value, protocol}` | to build |
| `SchemaView` — predicates, canonical form, identity, compatibility | `fjord_schema::{syntax, print, fingerprint}` | to build |

**The schema is text, and the page holds it.** `syntax::read` builds a schema
from a string with no filesystem in reach, so the second editor was all it
took. Two consequences worth keeping: the module stays **stateless** — two
strings in, JSON out, no handle to a compiled schema that a page would have to
free — and a reader can *break* the schema and watch the query stop
typechecking, which is the clearest statement that these are the same phases
the server runs.

**The samples moved into the crate.** `fjord_inspect::SAMPLES` and `SCHEMA`
(the repository's own `schemas/code.sigla`, embedded) are what the page opens
with, and `every_sample_compiles_clean` is what makes them claims rather than
decoration. The page invented its own examples once; all of them were missing
the head a query requires.

**The lowered view runs the whole front end, not just typecheck.** Several
refusals a reader meets first are flatten's (`nyi/value-field`,
`reject/not-a-generator`), and a page that showed "no errors" for a query
`flatten` would refuse would be lying. The plan it produces is now shown beside
it, and **a plan exists exactly when the sink is clean** — the same rule the
server runs under, asserted rather than assumed.

**The plan view does not render the plan.** `print::plan` was split into
`print::steps` (one string per step) and `print::head`, with `plan` becoming the
join of them — so the text a page shows is byte for byte what
`fjord query --plan` shows, and `the_view_is_the_printer_rendered_apart`
reassembles one from the other to prove it. What the view adds is *structure*
around that text: the step's kind, the register it fills, whether each source
scans or seeks, how many residuals filter it. A second renderer would decode
stored bytes a second way, and the places it would differ — a constant's type, a
union alternative's name, which field a path names — are exactly the ones worth
reading.

**Levels are not steps, and the view says both.** A resume cursor holds one row
per *level*; a derive and a test bind nothing and take no cursor entry. Carrying
one number would make the other wrong somewhere a reader could not see.

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
`kind`, and the class is decided in Rust. It paid off when the schema pane
needed highlighting: the schema language is a *second* lexer with tokens sigla
does not have (comments, namespaces), and what the page needed was two more
arms on one shared vocabulary — one stylesheet, one set of classes, and a
reader meeting one idea rather than two. A page styles what the language says a
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

### Movement 4 — stepping the executor: a query debugger ✅

**Built.** The site runs queries against a database in the page and steps
through the run one transition at a time: registers as they fill, the row each
`yield` answers, and the rows a residual read and dropped. What follows is the
design as it was argued before it was built, amended where building it found
something.

**Goal.** Not "run a query in the page" but *step* one: see the registers as
they fill, where the machine is in the plan, what it has yielded so far, and
what it has read to get there. A reader who has watched a nested loop backtrack
understands the executor in a way no amount of prose achieves.

**It needed no new machine, which was the bet.** `iter.rs` is a
**defunctionalised state machine** ([I7](website/content/invariants.md#i7)):
`depth`, a stack of frames, and one loop whose every iteration is exactly one
transition. Stepping is therefore *exposing one iteration at a time* — not a
second interpreter in the view crate, which would be the very thing this whole
exercise exists to avoid. The nine transitions are already there to be named:
**open** a source, **produce** a row into a register, **drain** an alternative,
**close** a level, **yield** a row, **compute** a derived bind, **pass** a test,
**fail** a test, **done**.

**Rows dropped by a residual are the point, not a detail.** They never reach the
loop — `frame.next` filters them inside the scan — and they are exactly what
makes a scan cost more than a seek, so the debugger shows each one and *which
residual rejected it* (`check_residuals` knows). The scan loop is where
[I6](website/content/invariants.md#i6) and
[I9](website/content/invariants.md#i9) live, so this is paid for the way this
repository already pays for instrumentation: `FieldOffsets::witness_row` has a
real implementation under `cfg(debug_assertions)` and an empty `#[inline]` one
otherwise.

**A Cargo feature, `fjord-engine/trace`, off by default** — not
`debug_assertions`, which is on for every dev build and off in release, and the
browser wants a *release* build with tracing in it. The hook goes exactly where
`Profile.examined` is already incremented, because that increment is why skipped
rows are counted at all: the trace point and the counter are the same site. It
rides on the `Deadline`, which is already the per-run instrumentation carrier
threaded into the row loop — and which will want a better name once it carries
two things.

**Plus a runtime `Option`, even in the traced build.** That is what keeps
[I9](website/content/invariants.md#i9) honest: the allocation guard runs with
the sink switched off and must still count zero per row. Compile-time gating
alone would leave the guard measuring code that no longer resembles what ships.

**Both configurations are tested, which is the part that matters.**
`fjord-inspect` enables the feature, so `cargo test --workspace` builds the
engine *traced* and every existing guard — alloc-free per row, no value fetch in
the scan, resume equals uninterrupted — runs against the traced build.
`cargo test -p fjord-engine` has no `fjord-inspect` in its graph and builds it
*untraced*. CI runs both. A feature nobody tests is an aspiration.

#### The database in the page

`MemStore` is wasm-clean already; what is missing is facts — and, it turns out,
a schema. `schemas/code.sigla` has **no union and no nested record**, so a
select (`.what.func?`), a union pattern, a discriminant residual and a nested
record key have nothing to bind against. A union in a *leading* key field is a
seek and behind another field is a residual — the same query shape, two costs —
which is one of the sharpest things the plan view can show, so a database that
cannot demonstrate it is not a demonstration of the language.

**So the site gets a schema of its own: `schemas/demo.sigla`.** Code-search
shaped, because that is what Fjord is for, but small enough to read in one
screen and chosen so every shape the language has appears exactly once:

| Predicate | Shape it is there for |
|---|---|
| `code.File : string` | a **scalar** key — and a prefix constraint (`"src/"..`) that excludes something |
| `code.Decl { file : File, name : string, line : int } -> string` | a **record** key with a **leading reference**, a three-field prefix a seek can pin part of, an `int` for comparisons and arithmetic, and a **value side** |
| `code.Ref { from : Decl, to : Decl }` | a reference that is **not** leading (a fact-id compare as a residual rather than a seek), and a **two-hop chain** through `from.file` |
| `code.Span { decl : Decl, at : { line : int, col : int } }` | a **nested record** inside a key |
| `code.Kind { decl : Decl, what : kind }` | a **union behind** another field — matched by a residual on the discriminant |
| `code.KindOf { what : kind, decl : Decl }` | the same fact in the other key order, so the union **leads** and the tag is a seek. The pattern `code.sigla` already uses for `Attribute`/`AttributeOf`, and for the same reason: the leading run is what a query narrows on |

with `kind` declared as `{ type : string = 5 | func : int = 2 }` — two
alternatives, tags **neither contiguous, nor starting at zero, nor in
declaration order**, so nothing that read a discriminant as a position can pass
([I10](website/content/invariants.md#i10)).

A real file rather than a string in a crate, so the same database can be built
outside the browser — `fjord create demo --schema schemas/demo.sigla` — and the
queries a reader tried in the page can be run against a real one.

**The facts: around fifteen, and each one earns its place.** Three files (one
outside `src/`, so a prefix excludes it); three declarations, two of them in one
file, so a join returns two rows for one outer row and **none** for another —
backtracking a reader can watch; two reference edges forming a chain, with one
declaration referenced by nothing, so a negation has something to be true about;
one span; and a kind per declaration in both key orders.

They are authored through `fjord_store::fact::encode` — the path the .NET golden
and the server's catalogue take — so there is one encoder, with name resolution
and field reordering included, and references are written as the fixture writes
them: sequences chosen up front so `Decl`'s `file` names a `File` that exists.
**Hand-encoding a key is the anti-pattern `AGENTS.md` names**, and its three
silent preconditions apply here exactly.

Guard: `every_sample_answers_what_it_says` — each sample query's rows asserted
in the host suite, which is the corpus's discipline applied to the demo. A
sample that answers nothing is a sample that demonstrates nothing, so the guard
also refuses an empty answer unless the sample says it expects one.

#### `Executor::advance`

The loop body of `enumerate_profiled` becomes

```rust
fn advance(&mut self, deadline: &mut Deadline<'_>) -> Result<Transition, FjordError>
```

with `enumerate_profiled` looping over it. **The yield policy stays where it
is**: what to do with a row — `Stream::Continue` or `Stream::Suspend`, and the
`depth -= 1` after — is the streaming caller's business, so `advance` reports
`Yielded` and leaves the machine standing on the head. Accessors follow for what
a debugger reads between transitions: `depth`, `state` (already public types),
and a `row` that is `Some` exactly when the machine is standing on the head.

**The trap to name up front:** `advance` must not become the place where
*policy* lives. Descending or backtracking is read off the frame rather than
carried as a variable, which is what keeps the machine defunctionalised, and a
`Transition` return value must not become a second way of saying the same thing.

The safety net for the extraction is the strongest battery in the repository:
resume-equals-uninterrupted on both stores, the corpus suspending at every cut
point from 1 to 64, alloc-free per row under a counting allocator, no value
fetch in the scan, and the I8 drop probe. New guard:
`stepping_yields_what_running_yields` — drive `advance` to completion over every
corpus entry and compare the rows with `enumerate`'s.

#### `fjord_inspect::trace`

**The whole trace in one call.** `trace(schema, query)` runs the query to
completion and answers the entire run as a list of steps, each carrying only
what *changed*: the transition, the depth, the register written, the row
examined or rejected and by which residual, the row yielded. The page folds that
into cumulative state and scrubs a local array — instant in both directions, one
round trip, no state on the boundary, nothing for JavaScript to free, and no
O(n²) from replaying a prefix per step. "Step over" is then a client-side search
for the next `Yielded` entry, and costs nothing.

A run over a fifteen-fact database is tens of transitions; a deliberately silly
one is thousands. **The cap is stated rather than silent**: past a bound the
trace stops and says it stopped, because a truncated run rendered as a whole one
is the exact failure this repository keeps guarding against.

The escape hatch, if the browser database ever stops being a toy, is a
`#[wasm_bindgen]` struct owning a live `Executor` — O(1) per step, at the cost
of state on the boundary and a `free()` for JavaScript to forget.

The view, per step: the transition and the plan step it happened at, the depth,
every register (empty, a decoded row, or a computed value), the rows yielded so
far, and `Profile.examined` as it stands. **A register is decoded against
`fact_id.predicate()`** — the predicate of the row actually bound, not the
level's — because a level with alternatives can bind rows of different
predicates, and decoding against the wrong one reads plausible bytes.

#### The page

A **run** tab: transport controls (start, back a row, back one transition, one
transition, on to the next row, play, end), a scrub bar over the whole run, the
register panel, and the rows yielded so far. Stepping back is free, because the
trace is already in hand.

**Under it, the database as a table** — every stored row, in key order, as bytes
*and* as a fact, with the range the current scan is walking **shaded across
it**. That is the panel the plan's numbers are about: a seek is a byte prefix
and a scan is a range over the same order, so `[lo, hi)` means nothing against
decoded values and everything against stored keys. The pinned bytes are marked
off from the ones the scan walks, which is the cost model in one place —
everything left of the boundary the seek jumped to, everything right of it the
scan reads.

Four states a row can be in, and between them they are the whole story of a
query: outside the range and never read; inside it and not yet reached; **read
and dropped** by a residual; and **held**, which is where a register is
standing.

The bounds come from `open`, which is where they are computed — recorded on the
frame under the feature and reported by the caller that holds the deadline, so
no signature changes for a feature that is off. `Trace` grew `scanning` and
`fetching` beside `rejected` for it. The hex is unseparated because the page
compares it as a string: `"0000000104"` starts with `"00000001"` and
`"00 00 00 01 04"` does not.

#### What building it found

**A silently wrong answer, in `flatten`.** A constraint written inside a
subquery — `X = (Y where code.File Y; Y = "src/"..)` — was *dropped*, so the
query answered rows the constraint excludes; and a generator bind inside one
(`X = (Y where Y = test.Foo _)`) declined to plan at all, tripping flatten's own
"no plan without a reason" assertion in a debug build and refusing with an empty
sink in a release one. The cause was two paths for one thing: the subquery
inliner carried its own copy of the bind walk that handled only the *alias*
case. Both now go through one `Flattener::bind`, and four corpus entries pin the
combinations that were missing — which is why it survived: the corpus is how the
language surface is specified, and nothing had written these down.

**A reference reads as the fact it names.** `Value`'s serialiser writes a
`FactRef` as the `u64` it is, which is right for a wire and unreadable in a
panel, so the view renders one as `code.File#2`. Not a second codec: nothing
there decodes bytes.

### What is left

- **The `WireView`** — frames, blocks and a hex dump annotated by offset — which
  is the one view in the original list nothing has needed yet.
- **Retiring the hand-written highlighter** in `website/assets/app.js`, which
  waits on the new site carrying the book's code samples.
- **A schema handle, if a bigger schema ever makes it hurt.** `compile` re-reads
  the schema on every keystroke, because the module holds no state — two strings
  in, JSON out, and no handle a page has to free. Measured on
  `schemas/code.sigla`: 700–800 µs warm for the whole round trip, which is a
  tenth of a frame, so the statelessness is worth keeping until it is not.
- **Size.** 258 KB is the whole front end plus the schema language; `wasm-opt
  -Oz` takes 34 KB off it and `web/`'s dev-dependencies now carry binaryen so
  the build script finds one. If it matters more later, the lever is splitting
  the module per segment rather than shrinking this one.
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
