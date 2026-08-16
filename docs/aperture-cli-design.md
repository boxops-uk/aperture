# Aperture DB — CLI Design & Operational Requirements

Design record capturing the decisions made during operational design. Each command section
lists the behavioural requirements agreed so far, so this doc doubles as the build checklist.

**Most of this file is now built** — §1's lifecycle and its invariants, §2's resolution, §3's
layering bar the file layer, §4's tree bar `query`/`shell`/`write`/`db backup`/`restore`/`verify`/
`schema`/`completions`, §5's `serve`/`create`/`finish`/`list`/`describe`/`db rm`, and §6 entire.
[Phase 9](../PLAN.md#phase-9--operations-a-usable-tool) tracks what is where. So where a decision below is
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
  **Aperture does compact inside `finish`, and still needs no third state**, which is worth being
  exact about now that the premise has changed: `finish` is long work here too (minutes on an
  18M-fact index, most of it the identity walk). What `Finalizing` buys Glean is a *state machine
  entry for long work*; what `ops-I3` requires is only that no observer sees Complete before the
  data is durable. A merge before the flip cannot violate that — a crash during it leaves the
  pre-merge tree and a Writable database, which is the same answer, and the same re-runnable
  command, as a crash during the sync.
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

**Amended at 9d, and the amendment is to rule 1 for *lifecycle* commands only.** `create`,
`finish` and `db rm` resolve as: a server listening on the derived socket takes the command;
nothing listening means this process does the work itself, under the root lock. Rule 1 as
written would refuse them outright with no server up, which would make the tool unusable
offline for the one job — building an artifact in CI — the offline path exists for.

It does not weaken `ops-I1`, and the ordering is why. What that rule forbids is trying the
server, failing, and opening the directory *anyway*, because a server might be holding it.
Here nothing is opened until the socket has already answered that none is — rule 4's "the
socket *is* the server-detection mechanism", used as the detection it says it is — and the
root lock remains the authority behind it: a root held by something not listening is refused
by name rather than opened. Reads (`list`, `describe`) never faced the question, since
`ops-I7` means they take no lock and open no store.

Query and write sessions are unchanged: those bind a database, and rule 1 governs them as
written.

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
- **Steps 2 and 3 are still not consistent, and that is now a scheduling problem rather than an
  unanswered one.** A key may contain a fact **reference**; a reference is interned to a final
  `FactId` ([chapter 3](03-storage-model.md#interning-a-nested-fact)), and until it is, the key
  holding it has no bytes and so no sort position at step 2. What a reference *is* is
  [settled](open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline)
  — the target fact, nested — so what is left is *where interning happens in a parallel
  pipeline*: a pre-pass over each chunk, or a stratum boundary in the merge. That is a Phase 7b
  question, and it is asked with the primitive already built and tested by the write stream
  (§6), which has no such conflict: one writer, one stream, interning as it arrives.
- Schema validation against the DB's embedded schema on every path; a fact file's header
  fingerprint (§8) is checked for compatibility (subset containment, §7) before ingest.
- Session typing per ops-I6: file ingestion is `ingest`, arbitrary tool sessions are `tool`. Both
  contribute to base identity and neither sets a marker — the label is for diagnostics and
  future per-writer provenance (§11), not a dirty flag.
- Refused outright on a Complete DB (ops-I2).

### `aperture finish <db>`

Seal a Writable DB.

- Ordering per ops-I3: flush + `SyncAll` everything → **merge every tree** → compute content
  identity `hash(canonical schema, base facts)` → record it in the sidecar → atomically flip
  status to Complete as the final durable act.
- **The merge is a major compaction, and `finish` is the only place it belongs.** Ingestion
  leaves each tree in whatever shape the write order produced and nothing reclaims that
  afterwards; a Writable database might be written again in a moment, so merging it then would
  be merging it twice. Sealing is where the shape becomes final, and the shape is what every
  future reader pays: the executor re-seeks once per plan level per 256-row page, and an
  unmerged tree was measured seeking at up to 180× a merged one on an 18M-fact index, with the
  artifact also halving on disk ([findings](../bench/FINDINGS.md)). Before the identity walk, so
  the fingerprint is computed over the tree that ships; before the sidecar, so the byte count it
  records is the artifact's rather than the ingest's.
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
  **Built:** `--format table|json|raw|count`, `--limit`, `--timing`, `--profile`. Three of the
  four shapes stream; the aligned table buffers, because column widths are not known until the
  last row, and `count` exists so a measurement of the *server* is not paying for the client's
  rendering. `--limit` is not a `LIMIT` — the query is unchanged and the cancel is in band, so
  what it bounds is what crosses the socket. `--timeout` and Ctrl-C are still to come.

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
  **Built, and reachable before the shell is**: `aperture query --profile` prints the same table,
  because the instrument is what performance work needs and a prompt is not. Two details differ
  from the sentence above and are worth stating. It is per **step of the plan's body** rather than
  per predicate — that is what the machine counts, and it is what gives a fetch, a disjunction and
  a negation each a line of their own. And a profile arrives **once, just before the result ends**,
  in its own frame on the stream: the tally is not final until the last chunk has run, and a
  `--limit` that cancels early therefore reports none rather than reporting a different query's.
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

<a id="6-wire-protocol--the-write-stream"></a>
## 6. Wire protocol, and the write stream

**This is the primary ingestion API.** Phase 7 builds it before the file pipeline, and §8's file
format inherits its fact encoding rather than defining a second one.

**Built** — `aperture-server`, over a Unix socket, with `src/bin/aperture-serve.rs` to run it and
a C# client under `clients/dotnet` that speaks it from outside the repository. What is *not* built
is listed at the end of this section rather than implied by its absence.

### The frame layer

- PG-inspired, **not** PG-compatible. Startup/handshake PG-shaped (version, DB name, session
  mode per ops-I6); thereafter framed: `[kind:u8][stream:u32][length:u32][payload]` — **built**
  (`aperture-wire::frame`), 9 bytes. `kind` is a byte with named constants rather than a closed
  enum, deliberately: a framing layer delimits messages and does not interpret them, so an
  unrecognised kind is handed up intact rather than failing the decode — which is also what
  lets a peer at a newer protocol version be told "I do not know that message" instead of
  "your bytes are malformed". `length` is bounded before it is trusted, because it sizes a read
  from a number a peer chose.
- Streams are the multiplexing unit: a query is a stream; a write is a stream carrying
  CopyData-framed fact blocks. Short queries complete while long ones run on the same
  connection (the head-of-line-blocking fix that motivated leaving PG's strictly-serial model).
- Server obligations: fair per-connection writer task; chunked results; in-band per-stream
  cancel; flow-control windows deferred (bounded queues + connection backpressure in P0).
- Handshake compares the client's expected schema fingerprint against the DB's — cheap early
  mismatch detection, enabled by self-describing DBs.

### The value encoding

**Built** — `aperture-wire` (`crates/aperture-wire/src/`), whose module docs are the design of
record for the encoding itself. The shape of it, and what each choice is *not* paying for:

| | storage (`aperture-encoding`) | transport (`aperture-wire`) |
|---|---|---|
| int | marker byte carrying the width, then a big-endian minimal magnitude, negatives ones'-complemented (I1) | LEB128 varint over zigzag |
| string | marker, **escaped** contents, terminator — so a NUL costs two bytes | varint length, then the bytes, unescaped |
| record | marker, fields, terminator | the fields, concatenated. Nothing else |
| reference | marker + fixed 8 bytes, so it sorts as a band of its own | a varint union branch: an id, or the target fact |
| field names / types / arities | — (schema-free by construction, I2) | **not sent at all** |

The last row is the design. Both peers have the schema — the handshake compares fingerprints
before data flows, and I13 freezes a DB's at create — so names, order, arity and type are things
the reader already has. That is **Avro's** model, and Avro states the consequence plainly:
*"Binary encoded Avro data does not include type information or field names"*, so *"a schema must
always be used in order to read Avro data correctly"*, and a record is *"just the concatenation
of the encodings of its fields"*.

What is *not* borrowed is as deliberate. **Protobuf and Thrift** spend one to two bytes per field
per message on a tag, and what it buys is a reader skipping fields it does not know — schema
evolution between peers that never agreed. These peers have agreed, by fingerprint, before the
first byte. **Cap'n Proto** spends wire size on fixed-width fields to buy O(1) access with no
parse; every inbound fact is parsed regardless, to intern its references and re-encode it as a
storage tuple, so there is no parse to avoid and the size is worse.

Two properties are load-bearing rather than incidental:

- **Minimal varints are enforced**, so one value has exactly one encoding. A block carries a
  CRC32 and the same fact encoding is used on the wire and in a file (§8), so "the same facts"
  has to mean "the same bytes" for a checksum to be worth computing.
- **A reference is type-checked against the predicate it names**, both directions, free: a
  `Fact(p)` field can only hold a reference to `p`, and the [snowflake
  tag](03-storage-model.md#factid-allocation-i11) carries a fact's predicate in its top bits. That
  catches the one corruption a bare id is prone to — an id from the right DB and the wrong tree —
  which no length check or checksum would.

Measured on the shapes a code index holds — `{ file : ref, line : int, col : int, name : str }` —
the transport encoding is **40% smaller** than the storage one, and that comparison is a test.
It is not a pointwise law and is not claimed as one: a varint is longer at the extremes
(`i64::MIN` costs ten bytes here and nine in storage) and a length prefix passes three bytes at
16 KiB where a terminator stays at one. The win is over the data, not over every value.

### What a client sends: the whole fact, references included

A fact on the wire is `predicate + key + optional value`, and its **reference fields hold the
target fact written inline** — key and value both, to any depth — or a `FactId` for a producer
that already holds one. Ingest interns each nested fact and substitutes the id
([chapter 3](03-storage-model.md#interning-a-nested-fact)). Stored, a reference is a `FactId`
and only that; nesting is a transport form, not a storage one.

```text
    src.Decl {
      module = src.Module {                    ← a whole fact, not an id
        file = src.File "store/keys.py",       ← nested again
        name = "keys"
      },
      name = "key_of", line = 12
    }
```

**The producer keeps no book.** That is the whole reason for the shape: an indexer walking a
syntax tree knows the file when it reaches the declaration, and every id-based alternative asks
it to carry a map from every entity to an assigned identity, plus an emission order that
respects one. Cost of the trade, stated: a repeated target is sent repeatedly. Block-local
back-references would compact it and are deliberately not in P0 — a pure encoding win over a
semantics that is now decided.

### The write stream, end to end

1. `CopyInResponse` — the server accepts the stream on a `read-write` session against a Writable
   DB (ops-I1, ops-I2 refuse otherwise, at establishment rather than per fact).
2. `CopyData*` — each frame carries one **block**: the same
   `[block header][n facts]` §8 puts in a file, so on-wire and on-disk are one encoding.
3. Per block, in the DB's single writer task: decode → **validate against the embedded schema**
   (I13) → **intern** nested references bottom-up → storage-encode → write both column families
   atomically (I12).
4. `CopyDone` → the server replies with facts written, facts deduped, and the id range per
   predicate. A conflict instead **fails the stream** with the offending key named.

**Dedup and reject are ops-I5's, unchanged.** Interning *is* the dedup at this granularity: a
target nested under a thousand parents resolves to one row. A nested fact whose value disagrees
with one already stored under that key is the same-key-different-value conflict, rejected
deterministically — never last-writer-wins, never first-writer-wins, because either is
order-dependent and ops-I4 forbids it.

**Interning — built** (`aperture-ingest`), against a real store, with `FjallDb::intern` as the
resolve-or-create primitive shared with `put` so there is one implementation of the rule. The
walk is bottom-up because it has to be: a parent's key holds its child's *id*, so until the child
exists the parent has no bytes and therefore no identity to be written under. That is the same
fact that makes a reference in a key impossible to put in a cycle, and so the same fact that
makes the walk terminate.

One consequence is worth knowing before a client hits it: **a single fact can contradict itself.**
A nested fact both names and *defines* its target, so one message naming a target twice with two
different value sides is a producer disagreeing with itself, and is refused as an ordinary
conflict. It is not a special case in the walk — the second occurrence simply finds what the
first wrote — and both orders reject, so the answer does not depend on the order the walk takes.

**Failure is per stream, not per fact.** A rejected block fails the write stream; the connection
and its other streams survive. Whether a failed stream's already-written facts are rolled back is
a P0 decision recorded with the transaction story, not here — a fact whose nested target interned
cleanly and which then conflicted *has* written the target. That is close to harmless and the
reason is worth stating: the target was legitimately named and legitimately defined, facts are
immutable, and interning is idempotent, so retrying the whole message after fixing the conflict
dedups against it. What a transaction would prevent here is a wasted row, not a wrong answer.

### What is built, and what §5 still owes

| | |
|---|---|
| **built** | the frame layer; the handshake, including the schema-fingerprint check and `ops-I2` at establishment; write streams (open → blocks → done → counts); query streams (descriptor → chunked rows → complete); in-band per-stream cancellation; a reader task per connection and one fair writer over bounded per-stream queues; control frames for `create`/`finish`/`remove`; stream-level failure that leaves the connection usable |
| **deferred, named in §5** | per-stream flow-control windows; TCP (`ops-I10` default-closed) |

Three worth being precise about, because each is easy to mistake for something it is not:

- **A stream is a task, so a long query does not delay a short one.** The reader loop reads, routes
  to that stream's task and goes back to reading; one writer task takes a frame from each stream's
  queue in turn. Fairness is structural rather than a scheduling hope — a single shared output
  channel is unfair in exactly the way that matters, since a million-row query fills it and a second
  stream's four-frame answer waits behind all of them.
- **A chunk boundary is a real resume.** Rows go out 256 at a time off the executor's `Suspended`,
  and the next chunk resumes from the bytes-only cursor. This is the first thing in the project to
  use resume for what it is *for* rather than to test it.
- **A control frame is a stream like any other.** `create`, `finish` and `remove` are frames on an
  ordinary stream rather than on stream 0, which is what keeps a `create` — tens of keyspaces, tens
  of milliseconds each — off the reader loop, and gives lifecycle requests the same per-stream error
  handling everything else has. A session may name **no** database, which is the only kind of
  session `create` could be sent on. `list` and `describe` are deliberately not control frames:
  `ops-I7` means they read sidecars and never open fjall, so they already work while a server holds
  every database under the root, and §5's remote branch answers them through `aperture.db.List`.
  All of it is additive — the protocol version does not move, and the .NET client passes unchanged.

### What this direction does *not* share with the read direction

Both directions use the transport codec, which is not the storage codec
([chapter 3](03-storage-model.md#storage-codec-vs-transport-codec)). They differ in exactly one
thing: **only inbound has a reference that is not an id.** A row on its way out was read from
storage and its references are `FactId`s already.

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
- **How a fact in a block names another fact is §6's answer, not a second one.** A reference is
  the target fact written inline, or a `FactId` the producer holds
  ([settled](open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline)).
  So a file **can** carry a self-contained subgraph, which is what an indexer emitting one
  artifact needs. A block is *not* sortable in isolation, though — a key holding a nested
  reference has no bytes until interning has run — and that is the §5 scheduling question, not a
  format one.
- **Block — built** (`aperture-wire::block`), 30 bytes of framing:

  ```text
  [sync: FF × 10][magic "APBK"][predicate u32][count u32][length u32][crc32 u32][payload]
  ```

  RLE of the predicate ID: indexers writing in visitation order emit small blocks (bursts);
  the post-merge writer emits huge ones; blocks coalesce monotonically through k-merges until
  fully ordered. Same bytes on-wire (a CopyData frame's payload *is* a block) and on disk, and
  that is a test rather than an intention (`tests/one_encoding.rs`). Header fields are
  fixed-width where the payload is varints, because a splitter must read `length` before it can
  trust anything else; little-endian, because nothing here is ordered — big-endian in the
  storage codec is an I1 requirement this file does not inherit. The CRC covers the header's
  own fields as well as the payload, so a corrupted `length` is caught rather than used to skip
  to the wrong place.
- **Sync markers — amended, and the amendment is in our favour.** This section specified a
  *reserved, structurally-illegal* sequence ("unused type-tag run the encoder never emits") and
  then conceded that every hit is only a *candidate*, since "values carry arbitrary bytes
  (blobs/source text), so a marker can occur inside one". **Both halves were wrong for the codec
  that got built, in opposite directions**, and the result is stronger than either:

  - There are no type tags to reserve a run of — the value encoding is schema-driven and emits
    none (§6).
  - But the marker genuinely *cannot* occur in a payload, by the encoding rather than by luck.
    Ten `0xFF` bytes are unreachable: a string is length-prefixed UTF-8 and **UTF-8 never uses
    `0xF8`–`0xFF` at all**; a varint's continuation bytes are `0x80`–`0xFF` but its final byte
    is below `0x80`, so a run ends where the varint does and the longest one possible is
    `u64::MAX` at nine; and runs cannot join across values for the same reason. The header
    cannot contribute one either — `count` and `length` are capped to keep a zero top byte, so
    only the checksum is free to be all-ones, and four is not ten.

  **A marker therefore appears exactly once per block, at its start**, and for a well-formed
  file a scan finds boundaries and nothing else. Validation (magic, then CRC) remains
  load-bearing — for the fault it is actually for: a torn write, a flipped bit, a file cut
  mid-block. Not for disambiguating data that looked like a header. SIMD `memchr`-style scan
  finds the marker's first byte at memory bandwidth and confirms the run behind it. (This is
  Avro's marker-and-validate pattern with the collision case designed out rather than accepted.)
- Splitting for parallel ingest: seek anywhere → scan to next sync → hand blocks to
  workers. No reliance on per-predicate contiguity in the input. Checked from *every* offset of
  a multi-block file, not a sampled few.
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

Cargo workspace. **Every crate below now exists**, and so does the dependency direction stated
at the end of this section — `client → wire`, with nothing depending on `cli` or `server`. The
bottom four came first, extracted ahead of Phase 7 since ingestion is the first thing that needs
a real store/encoding boundary ([`PLAN.md`](../PLAN.md) cross-cutting note); `-wire`, `-ingest`
and `-server` followed at 7a, and `-client` at 9e — which is also when the message vocabulary
moved out of the server into `-wire`, where the line below always said it belonged. The seam decided
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
│   │                              # COPY writer, and a query result as a *bookmark*
│   │                              # (`take` is the page `\more` resumes) — used by
│   │                              # CLI, shell, and external tools/derivers
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

Dependency direction, as built: `cli → {client, server, store, schema, engine, wire}`;
`server → {wire, store, engine, ingest, schema}`; `client → {wire, schema}`;
`wire → schema`; `engine → {store, encoding, schema}`; `store → {encoding, schema}`.

Two footnotes, because the shape is not quite the one first written down. The CLI depends on
`server` for the single reason that `aperture serve` is a subcommand — the tool is where the
server is *hosted*, so nothing else does, and that is the rule's substance. And `client → wire`
rather than `client → wire → encoding`: `wire` is a sibling of `encoding`, not a layer on it,
because the transport and storage codecs share no bytes.

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