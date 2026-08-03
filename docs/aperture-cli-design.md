# Aperture DB — CLI Design & Operational Requirements

Design record capturing the decisions made during operational design. Each command section
lists the behavioural requirements agreed so far, so this doc doubles as the build checklist.

---

## 1. System invariants (every command obeys these)

These are the cross-cutting rules; individual commands reference them by number.

- **I1 — Single-process store ownership.** A fjall database directory may be opened by exactly
  one process. A running server *owns* every DB under its store root. The CLI never opens a
  directory a server holds; there is no silent fallback from "connect" to "open directly."
- **I2 — Complete = immutable.** Lifecycle is `Writable → Complete` (plus `Broken`). Once
  Complete, every open-for-write is refused at session establishment — immutability is
  structural (no writable handle exists), not defended per-write. This is the one rule never
  delegated to user judgment.
- **I3 — Finish ordering.** `finish` makes all data durable first (fjall `PersistMode::SyncAll`),
  then flips status via an atomic sidecar write (temp file → fsync → rename) as the **last
  durable action**. It must never be observable that metadata says Complete while data is not
  durable. A crash mid-finish leaves a Writable DB (resume or discard).
- **I4 — Reproducibility.** A DB built twice from identical inputs is identical.
  Identity = `hash(canonical schema, base facts)`; derived facts are implied by identity, not
  folded into it. Wall-clock timestamps and random IDs are descriptive metadata, never identity.
  Conflict handling must be order-independent (strict reject, not last-writer-wins) to preserve
  this.
- **I5 — One write funnel.** All writes — bulk ingest, wire COPY, tool/deriver sessions — pass
  the same pipeline: schema validation → sort/merge → dedup byte-identical facts silently →
  **reject** same-key-different-value conflicts. Structural guarantees (schema-valid,
  well-encoded, deduped, conflict-rejected) hold for *every* writer regardless of trust — a bad
  tool's blast radius is "wrong facts," never "broken DB." What manual writers are trusted to
  provide (purity / idempotence, so re-derivation reproduces their output) is a *semantic*
  guarantee the implementor owns; the DB does not record or check it (see I6, and `db verify`'s
  changed meaning in §5).
- **I6 — Session modes.** A session declares at open: `read-only` | `read-write`. Mode is
  resolved against DB status **once** at establishment (Complete ⇒ read-only, full stop; I2).
  No provenance/dirty-flag tracking in P0: manual tool writes are fully trusted to be pure, so
  there is no `externally_modified` marker and no identity downgrade — identity is always the
  content hash from I4. (The sidecar format is versioned so a dirty-flag can return as a pure
  addition if CI ever needs to *enforce* reproducibility rather than *trust* it; and future
  schema-driven derivation is simply another read-write writer on the same funnel.)
- **I7 — Filesystem is the catalog.** No manifest of DBs. Local enumeration = walk store root +
  read sidecars (readable even while a server holds the DBs, since sidecars don't require
  opening fjall). Any index/cache must be rebuildable from a scan and never authoritative.
- **I8 — Derivation is phased.** Strictly: create → ingest base → derive → finish. Derivers
  read the frozen base only (never their own or each other's writes), write only derived
  predicates. Prefix-disjointness (predicate ID is the key prefix) makes read/write disjointness
  structural. Derivers are embarrassingly parallel; no stratification in P0. Deriver input goes
  behind a "sealed snapshot" abstraction (not hardcoded "the base keyspaces") so sealed-round
  stratification can be added later without deriver rewrites.
- **I9 — No cross-DB anything in P0.** No cross-DB queries, no stacking, no ownership sets.
  Seam kept open: do not hardcode "a fact is fully identified by predicate + key, forever" in
  layers that would later need an owner/visibility dimension.
- **I10 — No in-DB auth; the transport is the trust boundary.** There is no RBAC and no
  differentiated capability, so the DB does no authn/authz — authentication is delegated to the
  transport (Unix-socket filesystem permissions locally; an authenticated gateway / mTLS
  terminator / tunnel for TCP). This is safe *only because binding is default-closed*: the
  server binds the **Unix socket only** by default, and TCP is an explicit operator opt-in
  expected to sit behind a gateway. The failure mode to prevent is a server that binds a network
  interface by default and becomes an unauthenticated DB open to the world. The handshake keeps
  a **reserved credential slot** (accepted-as-anonymous in P0) so transport/identity auth can
  later be a handshake extension, not a wire redesign.

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
   the directory (I1).
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
| `listen` | server | **default-closed (I10): Unix socket only.** TCP (`tcp = host:port`) is an explicit opt-in expected behind an authenticated gateway. Never binds a network interface by default. |
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

- Acquires exclusive ownership of `data_dir` (I1): refuse to start if another server holds the
  root (detect via lock); create the Unix socket at the derived path. **Binds the socket only by
  default (I10);** TCP is an explicit opt-in (`--listen-tcp host:port` / `listen.tcp` config)
  and the operator is responsible for putting it behind an authenticated gateway. Never binds a
  network interface implicitly.
- No authn/authz in the server (I10): the handshake accepts the reserved credential slot as
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
- Session establishment enforces I6/I2: mode declared in the handshake, resolved once against
  DB status.
- Per-DB wire writes funnel through a **single writer task** (fjall's non-transactional path
  loses updates on concurrent read-modify-write; a Writable DB is single-server-owned anyway,
  so serialization is free and the transactional keyspace is unnecessary).
- Exposes enumeration as the virtual predicate `aperture.db.List` through the normal query
  machinery — no bespoke control message.
- Reader scaling model: a server owns its snapshot; horizontal scaling = more processes each
  with their own **copy** of an immutable Complete DB (copy-on-start read mode is a future
  deployment feature enabled by I2 + tar-able directories).

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
- Writes the initial sidecar (status = Writable) atomically per I3's write discipline.
- Routed like any other command: through the server if addressed by name, embedded (offline)
  if addressed by path with no server holding the root.

### `aperture write <db> [FILE...]`

Ingest facts from fact files or stdin.

- **Two transports, one funnel (I5):**
  - *Wire:* connect with a `read-write` session, open a write stream; payload is CopyData-framed
    fact blocks (CopyInResponse → CopyData\* → CopyDone borrowed from PG COPY). Writes are
    **just another stream** — a deriver/tool interleaves read streams and write streams on one
    connection; no separate sub-channel, no second code path.
  - *Embedded:* the offline CI merge path. Requires exclusive access to a Writable DB (I1).
- **Bulk pipeline (embedded, and server-side for file ingest):**
  1. Split input into chunks via sync-marker scan (§8) — no serial parse from byte zero.
  2. Workers in parallel: wire-decode → storage-tuple-encode (order-preserving key) → sort.
  3. K-way merge across workers per predicate. At the merge frontier: identical keys are
     colocated ⇒ dedup byte-identical facts silently; **reject the batch** on
     same-key-different-value (deterministic, order-independent — required by I4).
     `--on-conflict=reject` is the default; any override must be commutative, never LWW.
  4. Feed the sorted, deduped, conflict-free ascending stream to fjall bulk `ingest()`
     (the "hidden unchecked write" — it needs no per-key reads because the merge already
     established the invariants). One keyspace per predicate ⇒ per-predicate ingests are
     independent trees and may overlap.
- Schema validation against the DB's embedded schema on every path; a fact file's header
  fingerprint (§8) is checked for compatibility (subset containment, §7) before ingest.
- Session typing per I6: file ingestion is `ingest` (contributes to base identity, no flag);
  arbitrary tool sessions are `tool` (sets `externally_modified`).
- Refused outright on a Complete DB (I2).

### `aperture finish <db>`

Seal a Writable DB.

- Ordering per I3: flush + `SyncAll` everything → compute content identity
  `hash(canonical schema, base facts)` → record it in the sidecar → atomically flip status to
  Complete as the final durable act.
- Identity recording: the fingerprint lives in the sidecar; the directory keeps its provisional
  instance name (renaming under a live server is not worth it). DBs are addressable by name and
  by fingerprint. If `externally_modified` is set, identity stays a random ID — the DB is
  honestly marked non-reproducible (I6).
- Idempotent-ish: finishing a Complete DB is a no-op with a notice; a crash mid-finish leaves
  Writable and the command can be re-run.
- After finish: every write-mode open is refused at establishment, forever (I2).

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

### `aperture list`

- Local (no address / `--local`): walk store root, read sidecars only (I7) — works while a
  server holds the DBs, never opens fjall.
- Remote (address given): query `aperture.db.List`; output identical either way.
- Columns: name, instance, status, schema fingerprint (short), fact count, size,
  `externally_modified`, created (descriptive only, I4).

### `aperture describe <db>`

- Full sidecar metadata + embedded schema summary (predicates, per-predicate fingerprints,
  counts). `--schema` dumps the canonical schema. Same local/remote duality as `list`.

### `aperture db backup <db> <dest>` / `db restore <archive> [name]`

- **Backup = tar of the directory. Complete DBs only** (I2 makes them frozen artifacts;
  file-level backup of a Writable DB is both unsafe under I1 and explicitly out of scope).
  Include the sidecar; optionally verify content fingerprint before archiving.
- Restore = untar into the store root; no registration step exists (I7 — the filesystem is the
  catalog). Validate: sidecar parses, status is Complete, fingerprint matches `--verify`.
- The same mechanism serves the future copy-on-start reader-scaling mode.

### `aperture db verify <db>`

- Recompute content fingerprint over base facts + schema and compare to the sidecar; check
  block CRCs where applicable. Meaningful only for Complete DBs; `externally_modified` DBs can
  only be checked for structural integrity, not reproducibility (I6).

### `aperture db rm <db>`

- Routed through the server if it holds the DB (server closes + deletes); embedded/offline
  deletes require the lock to be free. Refuse ambiguous bare names that match multiple
  instances unless `--all-instances`.

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
  mode per I6); thereafter framed: `[type:u8][stream_id:u32][len:u32][payload]`.
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

## 9. On-disk layout

```
<data_dir>/
├── aperture.sock                  # (or per-config) server socket; presence ⇒ server detection
└── <name>/<instance>/             # instance: provisional ULID; identity fp lives in sidecar
    ├── APERTURE_META              # sidecar: atomic temp+fsync+rename writes (I3)
    │     name, instance, status(Writable|Complete|Broken), format version,
    │     schema fingerprint, content fingerprint (at finish), counts, size,
    │     externally_modified, created_at (descriptive only)
    ├── schema/                    # embedded canonical schema (belt & suspenders vs lost sidecar)
    └── <fjall database files>     # one keyspace per predicate; fjall's own lock ⇒ I1 detection
```

- Sidecar is the fast enumeration path (I7); the embedded schema copy inside the DB is the
  durable fallback.
- One fjall keyspace per predicate: gives independent bulk-ingest trees and keeps
  prefix-disjointness aligned with physical isolation.

## 10. Project structure

Cargo workspace; the existing query engine slots in as a library crate. The seam decided
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
│   │                              # snapshot/sealed-snapshot abstraction (I8 seam);
│   │                              # keyspace-per-predicate mapping; lifecycle (I2/I3);
│   │                              # sidecar read/write; store-root enumeration (I7)
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
│   │                              # (I6), per-DB single writer, aperture.db.List
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
| Derived-on-derived (sealed rounds) | derivers read a "sealed snapshot" abstraction, not raw base keyspaces (I8) |
| Schema-driven derivation | third session type in I6 that doesn't set `externally_modified`; same funnel |
| Per-stream flow-control windows | bounded per-stream queues + connection backpressure in P0 |
| Cross-DB query / stacking / ownership | I9: no "predicate+key is the whole address" assumption in planner layers |
| Embedded read-only `query`/`shell` | executor already consumes (handle, snapshot); CLI address form reserved |
| Copy-on-start reader scaling | Complete DBs are tar-able artifacts (backup mechanism reused) |
| Per-predicate write provenance | `externally_modified` is a boolean today; sidecar format versioned |