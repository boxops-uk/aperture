# Aperture DB — CLI Design & Operational Requirements

Design record capturing the decisions made during operational design. Each command section
lists the behavioural requirements agreed so far, so this doc doubles as the build checklist.

**Nothing in this file is implemented yet** — the whole operational surface is
[Phase 9](../PLAN.md#phase-9--operations--production-ready). So where a decision below is
weighed against Glean (source read at commit `95c0fb6`), it weighs Glean's **shipped code**
against Aperture's **design**, and that asymmetry is worth keeping in view: Glean's costs are
measured and admitted in its own source, Aperture's are predicted. The ledger of what is
adopted, what diverges and what is undecided is
[`glean-comparison.md`](glean-comparison.md); it is the single source of truth for the
comparison, and this file cites it rather than restating it.

---

## 1. System invariants (every command obeys these)

These are the cross-cutting rules; individual commands reference them by number.

- **ops-I1 — Single-process store ownership.** A fjall database directory may be opened by exactly
  one process. A running server *owns* every DB under its store root. The CLI never opens a
  directory a server holds; there is no silent fallback from "connect" to "open directly."
  **Stricter than Glean, not different in kind.** Glean spawns 48 writer threads by default
  (`glean/config/server/server_config.thrift:236-237`) over per-repo queues, and excludes them
  per DB with a *try*-mutex whose loser deduplicates and then writes anyway
  (`glean/db/Glean/Database/Write/Batch.hs:221-234` — with an open `TODO` asking whether it
  should), while the storage layer beneath simply asserts the property it needs: *"We do **not**
  support concurrent writes"* (`glean/rocksdb/database-impl.cpp:482`). `ops-I1` plus the per-DB
  single writer task (§5 `serve`) make structural what Glean leaves to lock discipline — and
  `ops-I4` is what needs it that way.
- **ops-I2 — Complete = immutable.** Lifecycle is `Writable → Complete` (plus `Broken`). Once
  Complete, every open-for-write is refused at session establishment — immutability is
  structural (no writable handle exists), not defended per-write. This is the one rule never
  delegated to user judgment.
- **ops-I3 — Finish ordering.** `finish` makes all data durable first (fjall
  `PersistMode::SyncAll`), then flips status via an atomic sidecar write (temp file → fsync →
  rename) as the **last durable action**. It must never be observable that metadata says Complete
  while data is not durable. A crash mid-finish leaves a Writable DB (resume or discard).
  **Why Glean needs a third state and Aperture does not.** Glean's lifecycle is
  `Incomplete → Finalizing → Complete` (+ `Broken`) internally
  (`glean/if/internal.thrift:39-49`), and `Finalizing` exists because *its* finish is long work:
  complete-predicates, optimize/compact, cache ownership, re-merge the schema, stamp a wall-clock
  time (`glean/db/Glean/Database/Backup.hs:436-472`) — and its storage layer stays read-write
  throughout, **because compaction mutates the DB**
  (`glean/db/Glean/Database/Open.hs:105-111`). Glean *does* drop the write handle atomically at
  `finish` (`glean/db/Glean/Database/Finish.hs:64-84`), so on immutability it is closer to
  `ops-I2` than a divergence framing implies; what it cannot do is call the status flip its last
  durable act. `ops-I8` is what buys that — hoisting the finalization work out of `finish` into
  an operator-visible phase leaves `finish` with nothing to do but sync and flip.
  (`unfinishDatabase` exists — `glean/db/Glean/Database/Finish.hs:87-110` — but is header-marked
  testing-only. Aperture cannot offer it at all: a recorded content hash cannot survive an
  append, so `ops-I2` is downstream of `ops-I4`, not an independent choice.)
- **ops-I4 — Reproducibility.** A DB built twice from identical inputs is identical.
  Identity = `hash(canonical schema, base facts)`; derived facts are implied by identity, not
  folded into it. Wall-clock timestamps and random IDs are descriptive metadata, never identity.
  Conflict handling must be order-independent to preserve this — a strict reject, never a
  pick-one rule of *either* polarity (see `ops-I5`: Glean's escape hatch keeps the fact already
  in the set, i.e. first-writer-wins, which is order-dependent just the same).
  **The chain, written out, because it is one link longer than it reads:** reproducibility ⇒
  deterministic reject + one write funnel (`ops-I5`) ⇒ no concurrent independent writers to a DB
  (`ops-I1`) ⇒ no incremental append (`ops-I9`). Every arrow is forced, so `ops-I9` is a
  consequence of this invariant rather than an independent preference.
  **What Glean pays instead.** A Glean DB is not reproducible, for six independent reasons —
  arrival-order fact ids, a wall-clock completion stamp, `ignoreRedef`, a bounded `LookupCache`
  whose eviction depends on timing, a load-bearing random `glean.guid`, and RocksDB compaction —
  and it costs Glean little, because its identity is **provenance**, not content: `Repo{name,
  hash}` (`glean/if/glean.thrift:106-109`) where the hash is "just an arbitrary string" and not
  even guaranteed unique, which is precisely why a random GUID had to be added per instance
  (`glean/db/Glean/Database/Create.hs:185-187`). A content hash gives that uniqueness for free;
  what it gives up is the answer to "what was I built from?" (§9). Note that Glean pays
  Aperture's price wherever it wants determinism too: `glean merge` processes its input files
  serially (`glean/tools/gleancli/GleanCLI/Merge.hs:115-117`), so serialisation is the cost of a
  reproducible merge in either system.
- **ops-I5 — One write funnel.** All writes — bulk ingest, wire COPY, tool/deriver sessions — pass
  the same pipeline: schema validation → sort/merge → dedup byte-identical facts silently →
  **reject** same-key-different-value conflicts. Structural guarantees (schema-valid,
  well-encoded, deduped, conflict-rejected) hold for *every* writer regardless of trust — a bad
  tool's blast radius is "wrong facts," never "broken DB." What manual writers are trusted to
  provide (purity / idempotence, so re-derivation reproduces their output) is a *semantic*
  guarantee the implementor owns; the DB does not record or check it (see ops-I6, and `db verify`'s
  changed meaning in §5).
  **The reject rule is adopted from Glean, not a divergence from it.** It is Glean's own default:
  `Define::define` returns `Id::INVALID` on same-key-different-value
  (`glean/rts/define.h:20-30`), `defineBatch` raises `"invalid fact redefinition"`
  (`glean/rts/define.cpp:91-102`), and a test pins the message. What Glean actually does
  differently is that it must **switch the rule off** — `ignoreRedef = True` — on three paths:
  the concurrent rebase queue, `FactSet::rebase`, and `glean merge`. Its own source says why, and
  the confession is the strongest argument available for a single funnel: *"this might mean that
  we are ignoring actual errors and silently picking one of the two facts… That's bad, but I
  don't see an alternative"* (`glean/write/Glean/Write/SendAndRebaseQueue.hs:408-426`). On those
  paths the fact that survives is the one already in the set, so the behaviour is
  first-writer-wins and order-dependent. The cause is concurrent rebasing against per-writer
  caches — so the claim to make is not "we reject and Glean doesn't", it is **"one funnel is what
  makes rejecting affordable"**: one canonical encoder and identical keys colocated at a single
  merge frontier remove both causes.
- **ops-I6 — Session modes.** A session declares at open: `read-only` | `read-write`. Mode is
  resolved against DB status **once** at establishment (Complete ⇒ read-only, full stop; ops-I2).
  No provenance/dirty-flag tracking in P0: manual tool writes are fully trusted to be pure, so
  there is no `externally_modified` marker and no identity downgrade — identity is always the
  content hash from ops-I4. (The sidecar format is versioned so a dirty-flag can return as a pure
  addition if CI ever needs to *enforce* reproducibility rather than *trust* it; and future
  schema-driven derivation is simply another read-write writer on the same funnel.)
  **One reading, and this is it:** there is no `externally_modified` field in P0 — not in the
  sidecar (§9), not as a `list` column (§5), and not as an input to identity at `finish` (§5),
  because a marker that downgraded identity would contradict `ops-I4`'s "identity is always the
  content hash". Where §5 and §9 mention it they say it is absent, and §11 lists the dirty-flag
  as deferred; nothing below treats it as live. A session's declared type
  (`ingest` | `tool` | deriver) is a diagnostic label on the same funnel, not a provenance
  record.
- **ops-I7 — Filesystem is the catalog.** No manifest of DBs. Local enumeration = walk store root +
  read sidecars (readable even while a server holds the DBs, since sidecars don't require
  opening fjall). Any index/cache must be rebuildable from a scan and never authoritative.
- **ops-I8 — Derivation is phased.** Strictly: create → ingest base → derive → finish. Derivers
  read the frozen base only (never their own or each other's writes), write only derived
  predicates. Prefix-disjointness (predicate ID is the key prefix) makes read/write disjointness
  structural. Derivers are embarrassingly parallel; no stratification in P0. Deriver input goes
  behind a "sealed snapshot" abstraction (not hardcoded "the base keyspaces") so sealed-round
  stratification can be added later without deriver rewrites. This phasing is also what makes
  `ops-I3` statable: the work Glean has to do *inside* `finish`, and needs a whole `Finalizing`
  state for, happens here instead — in a phase an operator can see, order and re-run.
- **ops-I9 — No cross-DB anything in P0.** No cross-DB queries, no stacking, no ownership sets.
  **Two mechanisms, priced apart, because only one of them is expensive.** *Stacking* composes
  two DBs by fact-id range (`glean/rts/stacked.h:20-144`) and needs **no ownership at all** — a
  delta can only add. *Ownership* is a per-fact set expression that lets a delta **hide** base
  facts, and it is the half that costs. Glean's own code prices it: the visibility check is a
  per-row filter wrapping every iterator (`glean/rts/ownership/slice.h:167-233`) — an indirect
  call, a lock, an interval binary search and possibly a page read per candidate row — and its
  `TODO` records that it does "2 DB lookups (to get the value) even for facts that we skip"
  (`glean/rts/lookup.cpp:152-159`) — which is [I6](invariants.md#i6) and [I9](invariants.md#i9)
  seen as anti-patterns rather than as invariants. Measured overhead is ≤10%
  on typical queries but **~3× on search-heavy** ones; propagation is **O(facts) in time and
  space**, with the space fix listed as future work; it **bans negation in stored derived
  predicates** purely because invalidation would cost as much as re-deriving; it renumbers facts
  to keep the owner interval map compact, which collides with [I11](invariants.md#i11); and it
  carries a derived-fact visibility hole Glean's own docs say "isn't implemented yet". The whole
  apparatus costs about **7%** of DB size. Aperture declines both halves, but only the second is
  a hard divergence — the full cost breakdown is in [the ledger](glean-comparison.md).
  **The seam actually kept is `FactStore::{scan, point}`** (`crates/aperture-engine/src/plan.rs`) — the direct
  analogue of Glean's `Lookup`, and exactly where Glean puts `Stacked` and `Sliced`. It is *not*
  "don't hardcode predicate + key as the whole address in planner layers", because
  `Access { predicate_id, seek_key }` (`crates/aperture-engine/src/plan.rs`) **is** that address, with no
  section/layer dimension, and `Cursor` (`crates/aperture-engine/src/iter.rs`) carries no layer tag: those two are
  what a stack would have to change, which is the honest statement of the seam. The good news is
  real. Per-predicate keyspaces make a stacked scan a **two-way merge** — strictly better than
  Glean's arrangement, whose sectioned seek filters the base's whole prefix range and discards.
  Carving the snowflake's per-predicate sequence space is easy, and `recover_high_water`
  (`crates/aperture-store/src/store.rs`) already derives each per-predicate boundary from the data rather than
  from a counter. And two *frozen* snapshots make [I8](invariants.md#i8) **easier** than Glean's,
  whose upper layer can still be written. **The wall is invalidation:** per-fact visibility would
  have to move inside the scan iterator — below the register, off the id in the `keys` row, which
  is the option Glean calls "ugly" and did not take — or
  [I6](invariants.md#i6)/[I9](invariants.md#i9) get amended.
  **The counter-argument, stated and then answered.** "A fresh sealed artifact per run is cheap
  because a Complete DB is a tar-able file" is the same sentence Glean gives as a motivation
  *for* incrementality with the sign flipped: its stated problems are indexing jobs "taking
  multiple hours" and "a monolithic indexing job produces a large DB that can be slow to ship
  around, for example to replicate across a fleet". Tar-ability is a build virtue and a
  distribution liability. From Glean's one published datum — 21,735,709 facts ≈ 1.06 GiB, so
  ~51 B/fact — a billion-fact index is ~50 GiB per rebuild, and copy-on-start readers (§5
  `serve`) multiply that rather than amortising it, so **the bet fails on distribution, not on
  build time**. Treat the arithmetic as an estimate from a single demo database: Glean publishes
  no production sizes, indexing times or churn rates anywhere. And the real blocker is on this
  side of the comparison — **Aperture states no target corpus size, no churn rate and no
  freshness budget anywhere**, so `ops-I9` is ultimately a *requirements* question this repo
  cannot settle on its own. It is recorded as a deliberate bet whose premise is untested, not as
  a finding.
- **ops-I10 — No in-DB auth; the transport is the trust boundary.** There is no RBAC and no
  differentiated capability, so the DB does no authn/authz — authentication is delegated to the
  transport (Unix-socket filesystem permissions locally; an authenticated gateway / mTLS
  terminator / tunnel for TCP). This is safe *only because binding is default-closed*: the
  server binds the **Unix socket only** by default, and TCP is an explicit operator opt-in
  expected to sit behind a gateway. The failure mode to prevent is a server that binds a network
  interface by default and becomes an unauthenticated DB open to the world. The handshake keeps
  a **reserved credential slot** (accepted-as-anonymous in P0) so transport/identity auth can
  later be a handshake extension, not a wire redesign.
  **`ops-I9` is the mechanism this one would need, so the two are a single decision.** Per-fact
  authorization in Glean *is* ownership with different units: ACL groups are allocated as
  ownership units and ANDed into the same slices (`glean/rts/ownership/slice.h:78-86`).
  Declining ownership therefore also declines the only
  mechanism Glean has for per-fact visibility control — and if `ops-I10` is ever reopened for
  per-fact authz, `ops-I9` is reopened with it, at `ops-I9`'s price and not this one's.

---

## 2. Addressing & connection resolution

Postgres-shaped: the client **always connects to a server** (Unix socket locally, TCP remotely);
direct directory access is a distinct, explicit, opt-in mode.

**Address forms**

| Form | Meaning |
|---|---|
| `mydb` | DB `mydb` via the local server's Unix socket (socket path derived from store root / config) |
| `aperture://[user@]host:port/mydb` | explicit remote over TCP |
| `--embedded <path>` (or a path-shaped address) | open the DB directory in-process, no server |

**Resolution rules**

1. Bare name → local socket. If nothing is listening: fail with a psql-style actionable error
   ("could not connect: is the server running on socket …?"). **Never** fall back to opening
   the directory (ops-I1).
2. URI → connect as specified.
3. Embedded → allowed only when the fjall lock is free (no server holds it). Embedded
   **read-only** requires status Complete. Embedded **read-write** is the offline
   ingest/derivation path (CI merge step) and requires exclusive access to a Writable DB.
   Attempting embedded access to a held DB fails with a clear message, never a lock fight.
4. The socket *is* the server-detection mechanism. No other autodetect.

**Least-surprise contract:** a bare name always means "ask the local server"; a path or URI
means exactly what it says; the tool never guesses between them.

---

## 3. Configuration

Layered, .NET-style, using the figment pattern from the CLI groundwork
(defaults → config file → env (`APERTURE_` prefix) → CLI flags; every clap field `Option<T>` +
`#[serde(skip_serializing_if = "Option::is_none")]` so unset flags don't clobber lower layers).

| Key | Used by | Notes |
|---|---|---|
| `data_dir` | server, embedded | store root; also determines default socket path |
| `listen` | server | **default-closed (ops-I10): Unix socket only.** TCP (`tcp = host:port`) is an explicit opt-in expected behind an authenticated gateway. Never binds a network interface by default. |
| `cache_size` | server, embedded | fjall unified cache |
| `max_connections` | server | |
| `host` / `port` / `default_db` | client commands | assembled into an address when no explicit one given |
| `schema_path` | schema commands, `create` | ordered roots used to resolve a *named* schema to its entry file; also `APERTURE_SCHEMA_PATH`, first-match-wins. `mod` edges within a schema resolve relative to the referencing file (§7), not via this path. |
| `format` | `query`, `shell` | client-side rendering (see I/O note in §5 `query`) |

Global flags on every command: `--config <file>`, `--verbose/-v` (repeatable), and where
relevant `--data-dir`, `--host/--port` overrides. All `#[arg(global = true)]`.

---

## 4. Command tree

```
aperture
├── serve                          # run the server over a store root
├── create   <name>                # new Writable DB (schema fixed here)
├── write    <db> [FILE...]        # ingest fact files / stdin (wire COPY or embedded bulk)
├── finish   <db>                  # seal: Writable → Complete
├── query    <db> <QUERY>          # one-shot query
├── shell    [<db>]                # interactive REPL (always speaks the wire protocol)
├── list                           # enumerate DBs (local scan or remote virtual predicate)
├── describe <db>                  # metadata + schema info
├── db
│   ├── backup   <db> <dest>       # tar a Complete DB
│   ├── restore  <archive> [name]  # untar into store root
│   ├── verify   <db>              # integrity check (fingerprint / CRCs)
│   └── rm       <db>              # delete
├── schema
│   ├── check       [ROOT|FILE...] # resolve + canonicalize + report errors
│   ├── diff        <a> <b>        # Identical / Compatible / Breaking (+ reasons)
│   └── fingerprint [ROOT|FILE...] # print schema + per-predicate fingerprints
└── completions <shell>            # clap_complete
```

Design notes: common lifecycle verbs stay top-level (they're the daily drivers); admin and
schema tooling nest one level. Every DB-taking command accepts any address form from §2, so
"local vs remote" is a property of the *address*, not of the command — the server implements
each operation on the same core code the embedded path uses (two front doors, one implementation).

---

## 5. Per-command requirements

### `aperture serve`

Run the server owning a store root.

- Acquires exclusive ownership of `data_dir` (ops-I1): refuse to start if another server holds the
  root (detect via lock); create the Unix socket at the derived path. **Binds the socket only by
  default (ops-I10);** TCP is an explicit opt-in (`--listen-tcp host:port` / `listen.tcp` config)
  and the operator is responsible for putting it behind an authenticated gateway. Never binds a
  network interface implicitly.
- No authn/authz in the server (ops-I10): the handshake accepts the reserved credential slot as
  anonymous. Access control is entirely the transport's job (socket permissions, or the gateway
  in front of opted-in TCP).
- Implements the wire protocol (§6): PG-shaped handshake, then framed messages
  `[type:u8][stream_id:u32][len:u32][payload]` with **stream-level multiplexing**.
- **Per-connection single writer task** that fairly interleaves ready streams (round-robin over
  per-stream output queues) — without this, one chatty stream starves the socket even when the
  executor has capacity.
- Results are chunked DataRow frames tagged by stream, flushed incrementally; the executor's
  resumable iteratee yields between chunks.
- In-band per-stream cancellation (Cancel frame carrying `stream_id`).
- Per-stream flow-control windows: **deferred past P0**; start with per-connection backpressure
  + bounded per-stream queues.
- Session establishment enforces ops-I6/ops-I2: mode declared in the handshake, resolved once
  against DB status.
- Per-DB wire writes funnel through a **single writer task** (fjall's non-transactional path
  loses updates on concurrent read-modify-write; a Writable DB is single-server-owned anyway,
  so serialization is free and the transactional keyspace is unnecessary).
- Exposes enumeration as the virtual predicate `aperture.db.List` through the normal query
  machinery — no bespoke control message.
- Reader scaling model: a server owns its snapshot; horizontal scaling = more processes each
  with their own **copy** of an immutable Complete DB (copy-on-start read mode is a future
  deployment feature enabled by ops-I2 + tar-able directories). This is also where `ops-I9`'s
  bet is most exposed: a copy per reader multiplies the artifact rather than amortising it, and
  shipping bytes is where Glean's incrementality argument actually bites.
- **A readiness signal.** `--write-port FILE` (Glean's
  `glean/server/Glean/Server/Config.hs:19-49`) writes the bound address to a file once the
  server is serving, which is the standard answer to "how does a test or an init script know the
  server is up". Aperture's socket path is derived, not chosen, so the file only has to appear —
  but it has to appear *after* the listener is accepting, or it is a race dressed as a signal.

### `aperture create <name>`

Create a Writable DB.

- Requires a schema: `--schema <entry-file>` (or a named schema resolved to its entry file via
  `schema_path`). Walks the `mod` tree from that root (§7), canonicalizes, computes
  fingerprints, and **embeds the canonical schema + fingerprint in the DB** at creation — the
  schema travels with the data; the DB is self-describing. The schema is fixed for the DB's
  lifetime (no in-place schema change; P0 has no `evolves`).
- Creates directory `<store>/<name>/<instance>/` where `<instance>` is a **provisional**
  ULID during the Writable phase. Content-derived identity can only exist at `finish`
  (it hashes the base facts); see `finish` for how identity is recorded.
- Writes the initial sidecar (status = Writable) atomically per ops-I3's write discipline.
- Routed like any other command: through the server if addressed by name, embedded (offline)
  if addressed by path with no server holding the root.
- **Two descriptive fields the design is missing, both cheap and both genuine gaps.**
  *Provenance:* nothing in the sidecar (§9) records **what the DB was built from**, which is the
  first thing every consumer of a code index asks and is the primary key Glean's whole fleet is
  organised around — its identity *is* provenance (`Repo{name, hash}`, conventionally
  name/revision, plus `glean.scm.*` properties). Content identity is stronger for verification
  and says nothing about origin; the two are complements, not alternatives. *Properties:* Glean
  carries a freeform `map<string,string>` with a reserved `glean.` prefix, set at create
  (`--property NAME=VALUE`) and readable back (`glean/if/glean.thrift:116`). Both are safe under
  `ops-I4`'s existing "descriptive metadata, never identity" carve-out, and a properties map
  subsumes the provenance field. What must **not** be copied is Glean's use of properties as
  *functional* inputs (schema selection, ACL mode, dependency identity) —
  [I13](invariants.md#i13) embeds the schema and `ops-I4` derives identity from content; these
  stay descriptive-only.

### `aperture write <db> [FILE...]`

Ingest facts from fact files or stdin.

- **Two transports, one funnel (ops-I5):**
  - *Wire:* connect with a `read-write` session, open a write stream; payload is CopyData-framed
    fact blocks (CopyInResponse → CopyData\* → CopyDone borrowed from PG COPY). Writes are
    **just another stream** — a deriver/tool interleaves read streams and write streams on one
    connection; no separate sub-channel, no second code path.
  - *Embedded:* the offline CI merge path. Requires exclusive access to a Writable DB (ops-I1).
- **Bulk pipeline (embedded, and server-side for file ingest):**
  1. Split input into chunks via sync-marker scan (§8) — no serial parse from byte zero.
  2. Workers in parallel: wire-decode → storage-tuple-encode (order-preserving key) → sort.
  3. K-way merge across workers per predicate. At the merge frontier: identical keys are
     colocated ⇒ dedup byte-identical facts silently; **reject the batch** on
     same-key-different-value (deterministic, order-independent — required by ops-I4).
     `--on-conflict=reject` is the default; any override must be commutative, never LWW.
  4. Feed the sorted, deduped, conflict-free ascending stream to fjall bulk `ingest()`
     (the "hidden unchecked write" — it needs no per-key reads because the merge already
     established the invariants). One keyspace per predicate ⇒ per-predicate ingests are
     independent trees and may overlap.
- **Steps 2 and 3 are not yet consistent, and one open decision is why.** A key may contain a
  fact **reference**, and a reference is a final DB-local `FactId` — which step 3 assigns. So a
  key holding one has no bytes, and therefore no sort position, at step 2. Dedup in step 3 makes
  it worse in the same direction: collapsing two ids into one means redirecting every reference
  to the loser. What a fact file's reference actually is —
  [open decision](open-decisions.md#what-a-reference-is-in-a-fact-file) — determines whether this
  is a pre-pass, a stratified ingest, or a substitution table. Do not implement the pipeline
  before answering it.
- Schema validation against the DB's embedded schema on every path; a fact file's header
  fingerprint (§8) is checked for compatibility (subset containment, §7) before ingest.
- Session typing per ops-I6: file ingestion is `ingest`, arbitrary tool sessions are `tool`. Both
  contribute to base identity and neither sets a marker — the label is for diagnostics and
  future per-writer provenance (§11), not a dirty flag.
- Refused outright on a Complete DB (ops-I2).

### `aperture finish <db>`

Seal a Writable DB.

- Ordering per ops-I3: flush + `SyncAll` everything → compute content identity
  `hash(canonical schema, base facts)` → record it in the sidecar → atomically flip status to
  Complete as the final durable act.
- Identity recording: the fingerprint lives in the sidecar; the directory keeps its provisional
  instance name (renaming under a live server is not worth it). DBs are addressable by name and
  by fingerprint. Identity is *always* the content hash (ops-I4) — there is no path by which a
  finished DB carries a random ID instead, because P0 has no dirty flag to trigger one (ops-I6).
- Idempotent-ish: finishing a Complete DB is a no-op with a notice; a crash mid-finish leaves
  Writable and the command can be re-run.
- **Refuse to seal a DB with no facts** unless `--allow-zero-facts` is given. A silently-empty
  sealed artifact is the classic CI failure that looks like success; Glean guards it exactly this
  way, and dies with a message naming the flag
  (`glean/tools/gleancli/GleanCLI/Finish.hs:44-51`).
- After finish: every write-mode open is refused at establishment, forever (ops-I2).

### `aperture query <db> <QUERY>`

One-shot query.

- Opens a read-only session (any address form; embedded read-only requires Complete + lock
  free, per §2.3).
- Streams results incrementally — never buffers a full result set; large results must not
  monopolize the connection (chunked DataRows + fair writer interleaving on the server side).
- **Rendering is client-side.** The wire carries the binary format; the CLI optionally
  serializes to JSON/text/raw (`--format`). The server never produces JSON (decided in the
  original brief).
- `--timeout`, and Ctrl-C maps to a per-stream Cancel frame (not connection teardown).

### `aperture shell [<db>]`

Interactive psql-like REPL.

- **Remote-first: always speaks the wire protocol**, even against a local server — the shell is
  the permanent exerciser of the wire format. (Embedded shell over a Complete DB is a
  fast-follow convenience, same executor library.)
- Meta-commands mirroring psql: `\l` (list — issues `aperture.db.List`), `\d [pred]`
  (describe DB / predicate), `\c <db>` (reconnect), `\timing`, `\q`. Readline editing + history.
- Multiplexing means a long-running query doesn't block issuing another (`\cancel <n>` or
  Ctrl-C cancels the active stream in-band).
- **`\more` — hold the cursor and resume it. This is the highest-value item on the page, by a
  distance.** The Phase 5 REPL discards the resume token at both call sites
  (`Iteratee::Suspended(rows, _)`, `src/main.rs`), so [I4](invariants.md#i4) and
  [I8](invariants.md#i8) — a bytes-only cursor, an entire resume battery, the most heavily tested
  machinery in the project — have **no interactive exerciser at all**. A wire client *can* hold a
  `Cursor`; that is the whole point of a bytes-only continuation, and Glean's shell threads this
  (`glean/shell/Glean/Shell.hs:892-909`, resumed by `:more` at `:644-649`). Pair it with a
  truncation footer that names the knob ("use `\more` to see more results"), the way Glean's does.
- **`\profile` — facts searched per predicate, with a full-scan flag.** `\d`/`:plan` shows
  *intent* — which field narrowed the scan, which one only filters; this shows the *outcome*, and
  closes the loop the plan renderer opens. Glean prints a per-predicate table and appends
  `" (full scan)"` for predicates it scanned whole (`glean/shell/Glean/Shell.hs:1013-1024`). The
  executor already counts rows examined for cancellation, so the counter exists and is simply not
  surfaced.
- **`\d <prefix>` falls back to prefix matching** when a name doesn't resolve exactly, so
  `\d src.` dumps a namespace rather than failing (`glean/shell/Glean/Shell.hs:273-281`).

### `aperture list`

- Local (no address / `--local`): walk store root, read sidecars only (ops-I7) — works while a
  server holds the DBs, never opens fjall.
- Remote (address given): query `aperture.db.List`; output identical either way.
- Columns: name, instance, status, schema fingerprint (short), fact count, size, created
  (descriptive only, ops-I4). No `externally_modified` column — there is no such field (ops-I6).

### `aperture describe <db>`

- Full sidecar metadata + embedded schema summary (predicates, per-predicate fingerprints,
  counts). `--schema` dumps the canonical schema. Same local/remote duality as `list`.
- **Per-predicate stats are a gap with real leverage, and cheap here.** The design lists counts
  but no per-predicate count or size, and nothing feeds `reorder`'s selectivity heuristic, which
  therefore has no data at all. Glean maintains its stats **incrementally on every commit** in
  their own column family and loads them once at open, so reading them is an in-memory snapshot
  rather than a scan (`glean/rocksdb/database-impl.cpp:485-540`, loaded at `:119, 137-156`) — and
  then spends them on query planning, on "that predicate has no facts in this DB" diagnostics, and
  on sizing derivation batches. Aperture's per-predicate keyspaces make a per-predicate count
  nearly free, and `finish` is the natural place to record it.

### `aperture db backup <db> <dest>` / `db restore <archive> [name]`

- **Backup = tar of the directory. Complete DBs only** (ops-I2 makes them frozen artifacts;
  file-level backup of a Writable DB is both unsafe under ops-I1 and explicitly out of scope).
  Include the sidecar; optionally verify content fingerprint before archiving.
- Restore = untar into the store root; no registration step exists (ops-I7 — the filesystem is the
  catalog). Validate: sidecar parses, status is Complete, fingerprint matches `--verify`.
- The same mechanism serves the future copy-on-start reader-scaling mode.
- Independent convergence worth one line: Glean's backup **is** a tarball too — of a RocksDB
  checkpoint or a BackupEngine backup (`glean/db/Glean/Database/Storage/RocksDB.hs:208-221`),
  with restore sniffing which it got. The archive format is not where these systems differ.

### `aperture db verify <db>`

- Recompute content fingerprint over base facts + schema and compare to the sidecar; check
  block CRCs where applicable. Meaningful only for Complete DBs. Reproducibility is checkable for
  every Complete DB, since identity is always the content hash (ops-I4/ops-I6).
- **At-rest structural validation is a genuine gap, and the shape to copy exists.** A fingerprint
  match proves the bytes are the ones that were hashed; it proves nothing about the two column
  families agreeing or about scan order. Glean's `Validate` walks every fact and checks six
  things (`glean/rts/validate.cpp:109-145`), two of which are literally Aperture's engine
  invariants seen from the other side: **enumeration order** is
  [I1](invariants.md#i1) at rest, and **key→id agreement** (`idByKey` mismatch) is
  [I12](invariants.md#i12) at rest. Both are guarded at *write* time here and nowhere after a
  crash-and-recover. Glean's `--limit` sampling is worth copying with it, so the check is
  affordable on a large DB.

### `aperture db rm <db>`

- Routed through the server if it holds the DB (server closes + deletes); embedded/offline
  deletes require the lock to be free. Refuse ambiguous bare names that match multiple
  instances unless `--all-instances`.
- **No retention policy is a small genuine gap, and the workflow generates the problem it
  solves.** "A fresh sealed artifact per run" means rebuilds accumulate, and `db rm` is a manual
  verb. A policy engine is service-inherent and rightly absent, but "keep the newest *n*
  Complete instances" is not. Three of Glean's rules are worth reading before designing one
  (`glean/db/Glean/Database/Retention.hs:228-317`): the retain-at-least count considers
  *complete* DBs only, a time floor protects an in-progress DB from being reaped mid-write, and
  nothing old is deleted until a newer one is actually available.

### `aperture schema check [ROOT|FILE...]`

- Resolve imports: build the transitive closure of import edges, **dedup by file identity**,
  union all namespace blocks (imports are explicit edges with concatenation semantics —
  cycles are therefore harmless by construction; diamonds dedup for free).
- Errors: unresolved import (search `schema_path` roots, first-match-wins), **genuine
  redeclaration** (two different definitions of the same fully-qualified name — the real error,
  as opposed to the same file reached twice), name-resolution failures.
- Transitive visibility is accepted (import isn't an encapsulation boundary) — document, don't
  fight it.

### `aperture schema fingerprint [ROOT|FILE...]`

- Canonicalize (resolve to fully-qualified names; strip comments/whitespace/file
  provenance/declaration order) and print: the schema fingerprint plus the per-predicate
  fingerprint map. Fingerprints are computed over the resolved union, so file layout and
  declaration order never affect identity.

### `aperture schema diff <a> <b>`

- Inputs: schema roots/files **or** DBs (compare embedded schemas) in any combination.
- P0 compatibility rule (deliberately collapsed): schema identity is the map
  `qualified_name → predicate_fingerprint`, and
  `compatible(old → new) ⇔ old_map ⊆ new_map`.
  **The only compatible change is adding a new predicate.** Any in-place modification —
  key or value fields, since values are queryable and positionally encoded — is Breaking
  until `evolves` exists. No field-level diffing needed in P0.
- Output: `Identical` | `Compatible (n added)` | `Breaking` with per-predicate reasons
  (removed / modified: old-fp ≠ new-fp).
- Future note recorded here so it isn't lost: when unions land, "add an alternative" is the
  common safe evolution **only if** discriminant tags are append-only (declaration-order or
  explicit tags) — sorted-name discriminants would silently renumber and stealth-break.

### `aperture completions <shell>`

- `clap_complete` output for bash/zsh/fish/powershell.

---

## 6. Wire protocol summary (as it constrains commands)

- PG-inspired, **not** PG-compatible. Startup/handshake PG-shaped (version, DB name, session
  mode per ops-I6); thereafter framed: `[type:u8][stream_id:u32][len:u32][payload]`.
- Streams are the multiplexing unit: a query is a stream; a write is a stream carrying
  CopyData-framed fact blocks. Short queries complete while long ones run on the same
  connection (the head-of-line-blocking fix that motivated leaving PG's strictly-serial model).
- Server obligations: fair per-connection writer task; chunked results; in-band per-stream
  cancel; flow-control windows deferred (bounded queues + connection backpressure in P0).
- Handshake compares the client's expected schema fingerprint against the DB's — cheap early
  mismatch detection, enabled by self-describing DBs.

## 7. Schema system summary

- **Files:** every file starts with a namespace declaration; namespaces are open across files;
  a file may contain multiple namespace blocks. Names must be unambiguous within a namespace
  after the resolved union.
- **Imports:** explicit Go-style edges (no exhaustive root scanning; disjoint schemas coexist
  in one tree), concatenation semantics (transitive closure → dedup by file → union). Cycles
  harmless; redeclaration is the error. Roots via `schema_path`, first-match-wins.
- **Identity:** canonical form → per-predicate fingerprints → schema fingerprint. Independent
  of file layout and declaration order by construction.
- **Compatibility (P0):** subset containment; add-predicate only. No `evolves`.
- **Deferred, with seams kept:** `evolves` + query-time projection; union alternative
  extension (append-only discriminants); field-level compatibility lattice.

## 8. Fact file / ingestion format summary

- **Envelope:** header (magic, format version, producing-schema fingerprint) → blocks → optional
  footer (block offsets, per-predicate grouping) for O(1) split assignment when the file was
  finalized under our control.
- **How a fact in a block names another fact is undefined**, and is the one thing here that
  cannot be filled in later: it decides whether a block is sortable in isolation (see §5) and
  whether a file can carry a self-contained subgraph at all.
  [Open decision](open-decisions.md#what-a-reference-is-in-a-fact-file).
- **Block = `[sync marker][block header: magic, predicate id, n, length, CRC32][n facts]`.**
  RLE of the predicate ID: indexers writing in visitation order emit small blocks (bursts);
  the post-merge writer emits huge ones; blocks coalesce monotonically through k-merges until
  fully ordered. Same format on-wire (a CopyData frame carries a block) and on disk.
- **Sync markers:** a *reserved, structurally-illegal* byte sequence (unused type-tag run the
  encoder never emits) — minimizes false candidates — but every hit is only a *candidate*:
  values carry arbitrary bytes (blobs/source text), so a marker can occur inside one. The
  validated block header (magic + length + CRC) after the marker is the load-bearing safety;
  SIMD `memchr`-style scan finds candidates, header validation rejects coincidences in a few
  instructions. (Avro's random-marker + decode-validate pattern, with a reserved sequence.)
- Splitting for parallel ingest: seek anywhere → scan to next validated sync → hand blocks to
  workers. No reliance on per-predicate contiguity in the input.
- **This is a real advantage over Glean, and it is the splittability, not the encoding.** A Glean
  binary `Batch` is one opaque sequential blob: `firstId`, `count`, and `facts` where "facts may
  only refer to facts which occur before them in this sequence" with ids assumed sequential from
  `firstId` (`glean/if/glean.thrift:159-181`). No sync marker, no block header, no CRC, no footer
  index — so a batch **cannot be split**. Glean parallelises *across* batches, which pushes the
  chunking decision onto the producer; Aperture splits *one file* at validated sync markers, so a
  single indexer's output parallelises without the indexer knowing anything about it.

## 9. On-disk layout

```
<data_dir>/
├── aperture.sock                  # (or per-config) server socket; presence ⇒ server detection
└── <name>/<instance>/             # instance: provisional ULID; identity fp lives in sidecar
    ├── APERTURE_META              # sidecar: atomic temp+fsync+rename writes (ops-I3)
    │     name, instance, status(Writable|Complete|Broken), format version,
    │     schema fingerprint, content fingerprint (at finish), counts, size,
    │     created_at (descriptive only)
    ├── schema/                    # embedded canonical schema (belt & suspenders vs lost sidecar)
    └── <fjall database files>     # one keyspace per predicate; fjall's own lock ⇒ ops-I1 detection
```

- Sidecar is the fast enumeration path (ops-I7); the embedded schema copy inside the DB is the
  durable fallback.
- The field list is **fixed and has no `externally_modified`** (ops-I6) and **no provenance**
  (§5 `create`). Both of those are additions the versioned format can take later; the format
  version is what makes them additions rather than migrations.
- One fjall keyspace per predicate **per column family** (`keys.<id>`, `entities.<id>`): gives
  independent bulk-ingest trees, keeps prefix-disjointness aligned with physical isolation,
  and makes dropping a re-derived predicate an O(1) tree delete rather than range tombstones.
  A keyspace costs ~30 ms to create, so `create` should materialise every predicate's trees
  from the schema up front — see
  [chapter 3](03-storage-model.md#one-keyspace-per-predicate--for-both-column-families) for
  the measurements and the `max_memtable_size` obligation that comes with the split.

## 10. Project structure

Cargo workspace. **The bottom four crates exist** — `aperture-schema`, `-encoding`, `-store`,
`-engine` — extracted ahead of Phase 7, since ingestion is the first thing that needs a real
store/encoding boundary ([`PLAN.md`](../PLAN.md) cross-cutting note). The rest are unbuilt, and
the root package is still the shell. The seam decided
during the embedded-mode discussion is structural: **the executor consumes
`(storage handle, sealed snapshot)` and never assumes a connection** — that single cut yields
embedded offline ingest/derivation (P0-required by the CI merge path) and embedded read-only
query (fast-follow) from the same library.

```
aperture/
├── Cargo.toml                     # [workspace]
├── crates/
│   ├── aperture-schema/           # parse → AST → canonical model; imports/resolution;
│   │                              # fingerprints; subset-containment compatibility
│   ├── aperture-encoding/         # order-preserving storage tuple codec; wire value codec;
│   │                              # reserved sync-marker constants
│   ├── aperture-store/            # Store trait: fjall backend + in-memory B-tree (tests);
│   │                              # snapshot/sealed-snapshot abstraction (ops-I8 seam);
│   │                              # keyspace-per-predicate mapping; lifecycle (ops-I2/ops-I3);
│   │                              # sidecar read/write; store-root enumeration (ops-I7)
│   ├── aperture-engine/           # existing query engine; resumable iteratee executor;
│   │                              # consumes (store handle, snapshot) — no server assumptions
│   ├── aperture-ingest/           # fact-file format (§8); sync-scan splitter;
│   │                              # parallel decode/sort; k-way merge w/ dedup + conflict-reject;
│   │                              # fjall bulk-ingest sink
│   ├── aperture-wire/             # frame codec, message types, stream state machine —
│   │                              # shared by server and client, no I/O policy
│   ├── aperture-client/           # connection, handshake, session modes, stream mux,
│   │                              # COPY writer — used by CLI, shell, and external tools/derivers
│   ├── aperture-server/           # listener, per-conn fair writer task, session enforcement
│   │                              # (ops-I6), per-DB single writer, aperture.db.List
│   └── aperture-cli/              # bin
│       └── src/
│           ├── main.rs            # parse → layered config → logging → dispatch
│           ├── cli.rs             # clap tree (§4); global args flattened + global=true
│           ├── config.rs          # figment layering (§3)
│           ├── address.rs         # §2 resolution (bare name / URI / embedded)
│           ├── output.rs          # client-side rendering: table/json/raw
│           └── commands/
│               ├── mod.rs         # top-level dispatch
│               ├── serve.rs  create.rs  write.rs  finish.rs
│               ├── query.rs  shell.rs  list.rs  describe.rs
│               ├── db/            # mod.rs backup.rs restore.rs verify.rs rm.rs
│               └── schema/        # mod.rs check.rs diff.rs fingerprint.rs
└── tests/                         # assert_cmd + trycmd end-to-end; figment Jail for config
```

Dependency direction: `cli → {client, ingest, store, schema, engine}`;
`server → {wire, store, engine, ingest, schema}`; `client → wire → encoding`;
`engine → {store, encoding, schema}`. Nothing depends on `cli` or `server`.

## 11. Deliberately deferred (with the seam that keeps each cheap)

| Deferred | Seam kept |
|---|---|
| `evolves` + query-time projection | compatibility checker structured around canonical model diff, not just hashes |
| Union types + append-only discriminants | noted in `schema diff`; discriminant rule recorded before unions exist |
| Derived-on-derived (sealed rounds) | derivers read a "sealed snapshot" abstraction, not raw base keyspaces (ops-I8); round boundaries computable from the derivation graph rather than declared, per Glean's topological `derive` (`glean/tools/gleancli/GleanCLI/Derive.hs:86-132`) |
| Schema-driven derivation | third session type in ops-I6, same funnel, no marker |
| Per-stream flow-control windows | bounded per-stream queues + connection backpressure in P0 |
| Cross-DB query + **additive** stacking | `FactStore::{scan, point}` is the seam (ops-I9) — the analogue of Glean's `Lookup`; `Access` and `Cursor` are the two types that change, and per-predicate keyspaces make a stacked scan a two-way merge |
| **Ownership / invalidation** (hiding base facts) | none, deliberately: it is the half that amends [I6](invariants.md#i6)/[I9](invariants.md#i9), adds a fallible lifecycle phase, and constrains the query language (ops-I9, ops-I10) |
| Embedded read-only `query`/`shell` | executor already consumes (handle, snapshot); CLI address form reserved |
| Copy-on-start reader scaling | Complete DBs are tar-able artifacts (backup mechanism reused) — and the row where `ops-I9`'s distribution cost lands |
| At-rest structural validation | `db verify` exists as a command; the two checks to add are [I1](invariants.md#i1)/[I12](invariants.md#i12) at rest (§5) |
| Per-predicate stats | recorded at `finish` into the versioned sidecar; the consumer (`reorder`'s selectivity) already has the seam |
| Retention policy | `db rm` exists and the filesystem is the catalog (ops-I7), so a policy is a caller, not a mechanism |
| Provenance / freeform properties | sidecar format is versioned; both are descriptive-only under ops-I4 (§5 `create`) |
| Per-predicate write provenance | the session type is already labelled (§5 `write`); no field exists yet, and the sidecar format is versioned |