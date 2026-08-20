# Fjord — the working contract

**Fjord** (the product: *Fjord DB*) is an embedded, immutable **fact database**; **sigla** is
its typed, Datalog-flavoured query and schema language. This file is the contract for working
in this repository: where the truth lives, the invariants by number, how work is proven done,
and the traps. It is deliberately tight — the *why* behind every rule lives in the design
book, not here.

## Where the truth lives

| What | Where |
|---|---|
| **The design book** (for humans — architecture, rationale, reference) | [`website/content/`](website/README.md) — published at <https://boxops-uk.github.io/fjord/> on every push to main; browse locally with `python3 website/serve.py`. The reading order is `NAV` in `build.py` |
| **The invariant registry** (statement · why · guard · status) | [`website/content/invariants.md`](website/content/invariants.md) — know these by number |
| **The roadmap** — what is unbuilt, its acceptance criteria, the settled decisions | [`PLAN.md`](PLAN.md) |
| Where we stand against Glean — read **before proposing a feature Glean has** | [`docs/glean.md`](docs/glean.md) |
| What has been measured, and the method | [`bench/FINDINGS.md`](bench/FINDINGS.md) · [performance](website/content/performance.md) |

## Module map — a workspace, bottom to top

Each crate depends only on the ones above it in this list; the compiler enforces the
direction. `fjord-schema`, `fjord-wire`, `fjord-client` and `fjord-db` are **published** —
their rustdoc is built with `-D warnings` in CI.

| Crate | Holds |
|---|---|
| `fjord-schema` | the type model (`schema`), the physical row id (`id`), schema identity (`fingerprint`) and the schema DSL front end (`syntax`: lexer, grammar, parse, lower, print, resolve, corpus). Depends on no Fjord crate |
| `fjord-encoding` | the order-preserving storage tuple codec (`tuple`) and `StoreCodecError` |
| `fjord-wire` | the **transport** codec and protocol vocabulary: `varint`, schema-driven `value`, `crc`, `block`, `frame`, `protocol`. A sibling of `fjord-encoding`, not a layer on it — shares no bytes with the storage codec |
| `fjord-store` | **the seam and nothing else**: the `FactStore` trait, `fact` (a fact written by hand), `keys` (the predicate a bound names), the format stamp, and `StoreError` — whose `Backend` variant is a boxed source, because *the backend failed* is the trait's business and *which* backend is not. Plus the shared test support: `fixture` (the database every battery queries) and `fixtures` (the probes, generic over `FactStore`). Links no fjall, no filesystem, no threads |
| `fjord-store-mem` | `MemStore` — an implementation, not test machinery: the differential oracle, and the store an engine compiled to WebAssembly runs on |
| `fjord-store-fjall` | the fjall backend (`store`, `lookup_cache`) and the lifecycle (`catalog`, `meta`, `schema_doc`, `identity`, `ulid`), with `CatalogError` for what is about a database as an *artifact* — a sidecar path, a held root lock, a database that is Complete |
| `fjord-ingest` | the write funnel: `FactSink` (the write seam, as `FactStore` is the read seam) and `intern` — a `WireFact` in, a `FactId` out, nested references resolved bottom-up |
| `fjord-engine` | **sigla** and the machine: lex → parse → typecheck → flatten → reorder → `Plan`, and the executor. All new query work lands here. `lib.rs` is the module list and nothing else. Depends on the **seam**, never on a backend — that edge is what makes a browser build possible, and `dependency_closure` is its guard |
| `fjord-inspect` | the **JSON view** of every construct: view models that derive `Serialize`, and the mapping from the engine's internals onto them. Never `Serialize` on the internals — a `Symbol` means nothing without the interner that minted it. The precedent is `fjord_wire::desc`, and a browser is one more peer with no interner |
| `fjord-client` | the client: `address`, `connection` (Unix socket or TCP, one `Transport` enum), `rows` (a result as a bookmark), `expand`. Depends on `wire` and nothing else |
| `fjord-server` | the protocol over a socket: `session`, `registry`, `outbound` (the fair writer), `rows`, `blocking`, `server`, `stats`, `catalogue` (the virtual `fjord.db.*` predicates — the reserved namespace is marked virtual so a stored predicate can never collide with it) |
| `fjord-viewer` | the code-search site. Depends on `fjord-client` and nothing below it — the claim being that a viewer is an ordinary consumer of the protocol. Binary: `fjord-viewer` |
| `fjord-cli` | the tool: `cli`, `config`, `commands/`, `output`, `prompt`, `sample_schema` (a **fixture**, not a default), `workload`. Binary: `fjord` |
| `fjord-db` | the published facade crate |

**Test support spans three crates now, and which one a thing lives in is decided
by what it must *be*.** `fjord-store::fixtures` holds the probes (`DropProbe`,
`PointSpy`, `FrozenStore`) because a probe has to be **the same** `FactStore` as
the store it wraps; `fjord-store::fixture` holds the shared database because
facts are backend-agnostic data; `fjord-engine::fixtures` holds the plan
runners. A guard that must see both implementations at once is a test in
`fjord-store-fjall`, the only crate that can — and a test that reaches back
through the engine goes in `tests/`, never `src/`, or it compiles a second copy
of its own crate.

**`wasm/` is outside the workspace, and so is `web/`.** The WebAssembly shell is
a `cdylib` that only builds for `wasm32-unknown-unknown`; as a workspace member
it would break `cargo build` on the host and quietly narrow the coverage ledger.
It is built by `scripts/build-wasm.sh` and consumed by `web/`, the interactive
site — both consumers of the tree, in the way `clients/dotnet` is.

**A non-Rust client is part of the test surface.** `clients/dotnet` implements the protocol
from outside — no shared constants, no shared enums — and is a checked-in golden:
`byte_identical_with_the_dotnet_client` asserts the Rust encoder produces the same bytes, with
corpus and schema stated independently on each side *on purpose* (a shared statement would
make the two agree by construction). `Boxops.Fjord.Indexer` is that client pointed at real
source via Roslyn, and it also writes Glean's batch format so the two systems can be measured
over one producer.

## How to work here

- **Test-driven, property-first, verification mandatory.** Reasoning is not evidence — nearly
  every bug here (codec off-by-ones, a residual short-circuit, resume duplicating a row) was
  invisible to inspection and caught only by a generated case. Write the property first, watch
  it fail, then fill the impl. "It compiles" is not done.
- **Every invariant owns a guard test, written up front.** A guard whose subsystem does not
  exist yet is `#[ignore]`d with the invariant in the message; `cargo test -- --ignored --list`
  is the coverage ledger and currently lists nothing pending. Work that touches an invariant is
  done only when its guard is un-ignored and green.
- **Non-functional criteria are part of *done*, and are tested, not asserted.** No per-row
  allocation, no value fetch in the scan loop, no snapshot held across suspend — each has a
  mechanical guard (a counting allocator, a store spy, a drop probe cross-checked against
  fjall's own snapshot count). An NFR with no mechanical guard is an aspiration.
- **Every error state is demonstrated by a test that provokes it** — at its contract layer
  (variant, wire `ErrorCode`, or rendered message). The exception is errors that merely bubble
  up from fjall or the OS. A variant no test can provoke is a variant to delete.
- **Keep diffs reviewable in one sitting.** The dominant failure mode here is a large,
  mostly-correct diff whose 10%-wrong part is expensive to find.
- **Respect the invariants absolutely.** Several look like implementation detail but are
  load-bearing or frozen on disk. If a change seems to require breaking one, stop and flag it —
  don't "simplify" past it.

## Build / test

```bash
cargo build
cargo test                          # the green suite — default-members is the whole workspace
cargo test -- --ignored --list      # the invariant coverage ledger
cargo +1.97.1 clippy --all-targets --workspace -- -D warnings
cargo +1.97.1 fmt --all
python3 website/build.py --strict   # the design book builds clean (CI runs this)

cargo check -p fjord-engine --target wasm32-unknown-unknown   # the browser build
./scripts/build-wasm.sh             # the module the interactive site imports
(cd web && npm run smoke)           # that demo, driven in a real browser
```

**The `+1.97.1` is not decoration.** CI's lint gate runs on that pinned toolchain and the
suite runs on `stable`, because a required check that can go red because an upstream released
is a check that blocks merges for a reason nobody chose. Clippy and rustfmt change between
versions; bumping the pin is a commit. Run them without the `+` and you may see lints CI does
not, or miss lints it does.

The sigla and schema grammars are `lelwel` grammars compiled by `build.rs` — nothing is
checked in, nothing regenerates by hand.

## Comments

A comment exists to guard a **trap**: something that bit us, or that fails silently, stated as
the present risk — what would go wrong and why it would be invisible — never as the history of
how the code got here. Everything else the code, the tests or the book should say.

- **Keep:** the risk a guard protects ("reading X here would silently Y"), and citations of
  invariants by number, linking the [registry](website/content/invariants.md).
- **Don't write:** narrative ("this used to…", "at first we…"), design essays, comparisons to
  other systems, or references to build phases — design rationale belongs in the book,
  history belongs in git.
- **A comment that explains logic is a smell**: prefer a clearer name, a smaller function, or
  a test that demonstrates the behaviour (the corpus tests are the model — an audit table as
  data, with gates asserting every entry behaves as classified).
- **Module docs are a short charter**: what the module holds and the one or two traps, not an
  essay.
- **Test doc comments** only where the sentence-style name cannot carry the why — the trap
  being guarded, not a restatement of the name.
- `unwrap`/`expect` only where an invariant makes failure impossible, **with the comment
  saying which invariant**.

## Code conventions

- **Errors, not panics, on data paths.** Corrupt bytes surface as an error variant, never
  `unwrap`/`panic`. A bad byte must not take down a connection.
- **A front-end phase reports by pushing into the `Diagnostics` sink, never by returning** —
  a returned `Vec<Diagnostic>` is one a caller can quietly drop. Report a `Code`, not a
  string: the enum is the taxonomy.
- **Record fields are ordered `[(Symbol, T)]` slices everywhere** (`Box<[…]>` owned,
  `Arc<[…]>` shared) — never `HashMap`. Deterministic order is a codec requirement. **Which
  order differs by which record:** a query's fields are sorted by name at lowering; a schema's
  are in declaration order, and that is the physical key order — it decides what a query can
  seek on.
- **Ownership signals sharing:** `Box<[T]>` owned-once; `Arc` only at genuine sharing
  boundaries; `Arc<str>`/`Arc<[u8]>` for content deduplicated across owners; `ByteView`
  clones are refcount bumps.
- **Symbols interned; runtime is interner-free.** Two-tier interning (frozen `SchemaInterner`
  + per-query `Rodeo`), schema-first resolution — a local name cannot shadow a schema name.
  Resolve to `&str`/`Arc<str>` at plan-build time.
- **Permissive grammar, narrow later.** Meaningless constructs are rejected at
  typecheck/flatten with a named diagnostic, not contorted into the grammar. A construct
  deferred for later must be reported **by name** (`nyi/…`), never as a parse error or a
  panic.

## Anti-patterns — look reasonable, are wrong here

Each breaks a specific invariant; the registry holds the rationale.

- Materialising a full result set (streaming/backpressure; aggregation cannot be suspend-free)
- Eager field decode at bind (I5/I9) · value fetch in the scan loop (I6)
- Holding an iterator across a suspend (I8) · rewriting `enumerate` as recursion (I7)
- Writing one column family without the other, or outside a batch (I12)
- Hand-encoding a key to reach `put_fact` — three preconditions fail **silently** (a stored
  key is flat, field order is the schema's, only the schema says if there is a value side);
  write a `fjord_store::fact::Fact` and use `FjallDb::put`. `encode_typed` is *not* the key
  encoder: it keeps a record's wrapper
- Renumbering markers or discriminants after data exists (I3/I10)
- DNF-expanding disjunction across conjuncts · reshaping the machine for an "additive"
  feature (a construct may add a `Source`, a `Test`, a residual op or a `Computed` arm —
  **never a `Step`**)
- `HashMap` record fields · `unwrap` on decoded data
- "Restoring" the single writer to fix an ordering problem — I12's write-once half is held by
  per-key striped exclusion now; and a conflict rule that picks a winner breaks `ops-I4`
- Adding an invariant-critical feature without its guard written first

## Traps that are easy to walk into

Distilled from what actually went wrong; the book carries each in full.

- **The write path.** The stripe is held across the LSM read *and* the commit — a fjall batch
  is atomic on recovery but **not isolated** from a concurrent reader, so a lock-free CAS
  would not do. Ids are claimed durably ahead of use (chunks in `meta`); a crash must never
  reissue an id a surviving reference resolves through. `serve --commit-per-block` trades
  exactly one thing, stated the same way everywhere: *a crash during ingest may cost the
  index, never its correctness.*
- **The resume token.** Every residual operator carries its **own fingerprint tag** — two
  plans differing only in polarity must not accept each other's cursors. Interned names stay
  **outside** the fingerprint (a `Symbol` is per-query; hashing one fails a legitimate
  resume). Anything that changes what a cursor entry means re-proves I4.
- **A `FieldPath` step at a union position is the expected discriminant, checked** — flatten
  owes the executor the tag's check *before* any read through that payload.
- **A constant bind folds** — no register, no step; a plan whose every bind folded is the
  unit relation. Its resume behaviour is the expensive thing to get wrong; don't "simplify"
  it into a step.
- **A constraint (`X = "a"..`) is applied by the level that captures the variable** —
  collected from the whole body before an order is chosen. Applying it afterwards as a
  residual answers the same rows and reads the whole predicate to find them.
- **A denial (`!=`) is never a seek.** The two polarities are held in separate collections
  precisely so a capture cannot be handed one to narrow itself by.
- **A negation is a `Step::Test`** — binds nothing, takes no cursor entry, re-decided on
  restore. Its variables are *reads*, which is the whole placement rule; `reorder` needs no
  new constraint kind for it.
- **A lower crate's test that runs a query goes in `tests/`, not `src/`** — a unit test
  reaching back through the engine compiles a second copy of its own crate, and the two
  `FactStore`s are then different types.

## Testing method (the distilled version)

The full method is [testing](website/content/testing.md); the parts that shape a change:

- **Generators are a first-class, co-owned artifact.** Every domain type owns a canonical
  strategy in a `proptest` support module (`tuple::proptest`, `plan::proptest`,
  `syntax::proptest`); tests import strategies, never define them inline. Compose with
  combinators — never imperative `Rng` sampling — so shrinking yields a minimal
  counterexample. Inject known edge cases explicitly.
- **Three tiers:** round-trip (`decode ∘ encode == id`), metamorphic against an
  **independent oracle** (never the code under test), and model-based (write the obviously
  correct model *first*; it is the permanent oracle). Generate **schema-first,
  valid-by-construction**; reject-sampling only for flat, mostly-valid domains.
- **A generator's population is asserted** (a census), because a strategy that degenerates
  leaves its property green and vacuous.
- **Trait contracts are asserted per implementation, not differentially** — two stores that
  leak identically satisfy a differential and are both wrong.
- **The corpus is how the language surface is specified**: an audit table as data
  (`fjord_engine::corpus`, `fjord_schema::syntax::corpus`), every entry classified
  `Supported(rows)` / `Diagnosed(code)` / `ParseError`, with gates asserting each entry
  behaves as classified and every diagnostic code is reachable or excused. Rows live *in*
  the classification so nothing can be marked supported without saying what it answers.
- Regression examples pin; properties explore. A test comment states the property, not the
  bug's history.

## Repository rules

Server-side rulesets, no bypass actors — they apply to admins too. **Enforced:** signed
commits everywhere; no force-push; linear history, PR-required, no deletion on `main` and
`release/*`; the `test` check (pinned-toolchain fmt + clippy, the suite, the ledger, the
website building clean) and the `build` check gate those branches; `attest` gates
`release/*`; tags cannot be re-pointed, `v*` tags cannot be deleted. Merges are squash or
rebase only. `GITHUB_TOKEN` is read-only and workflows may not approve PRs.

**Not enforced, recorded so nobody mistakes them for guarantees:** a tag object may be
unsigned (GitHub stores the ruleset and never applies it — but an unsigned commit cannot
enter by any route, so a tag can only point at a verified commit); an admin can weaken any
ruleset (audit-logged, not prevented); a PR can edit its own gate; a hand-uploaded binary can
be added to a Release after the fact — the guarantee stays the consumer's:

```bash
gh attestation verify ./fjord --repo boxops-uk/fjord
```

Registry uploads are deliberately manual: a Release can be deleted, a registry version
cannot, so publishing is a person's decision and not a tag's. Review is not enforced — GitHub
cannot require approvals without deadlocking a sole maintainer; the rule is written and
parked disabled until a second account has write access.
