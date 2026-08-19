---
title: Operations
description: The lifecycle, the ten operational invariants, ownership and locking, deployment shapes, and the operational gaps this design names rather than hides.
---

Operational rules live in their own namespace — always written `ops-Ix`, never confused with the
engine invariants. Individual commands reference them by number.

## The lifecycle

```text
   create ──▶ Writable ──▶ finish ──▶ Complete        (and Broken, for a failed one)
                  │                       │
            ingest, derive           read only, forever
```

| State | Means |
|---|---|
| **Writable** | Ingestion happens here. Many writers, one process |
| **Complete** | Sealed. Every open-for-write is refused at session establishment |
| **Broken** | A database whose build failed. Named so it is not mistaken for either of the above |

`create → ingest base → derive → finish` is the phase order (`ops-I8`). A deriver reads the frozen
base through a sealed snapshot and writes only derived predicates; prefix-disjointness makes
read/write disjointness **structural** rather than a convention.

## The ten operational invariants

| # | Statement |
|---|---|
| `ops-I1` | **Single-process store ownership.** A storage directory is opened by exactly one process; a running server owns every database under its root; no silent connect→open fallback. **A process, not a thread** — and that is what makes per-key exclusion inside the process sufficient |
| `ops-I2` | **Complete = immutable.** Once Complete, every open-for-write is refused at session establishment — structural, not per write |
| `ops-I3` | **Finish ordering.** Durable first, then flip the status via an atomic sidecar write as the **last** durable action. Never observable that metadata says Complete while data is not durable |
| `ops-I4` | **Reproducibility.** A database built twice from identical inputs is identical; identity is `hash(canonical schema, base facts)`. Timestamps and random ids are descriptive, never identity. Conflict handling is order-independent — strict reject, neither first- nor last-writer-wins |
| `ops-I5` | **One write funnel.** Every writer passes the same pipeline: schema-validate → dedup identical → reject same-key-different-value. **One pipeline, not one thread** |
| `ops-I6` | **Session modes.** A session declares `read-only` or `read-write` at open, resolved once against the database's status |
| `ops-I7` | **The filesystem is the catalog.** No manifest of databases; enumeration is a walk of the store root plus sidecar reads. Any index is rebuildable and never authoritative |
| `ops-I8` | **Derivation is phased.** create → ingest base → derive → finish |
| `ops-I9` | **No cross-database anything in P0.** No cross-database queries, stacking or ownership |
| `ops-I10` | **No in-database auth; the transport is the trust boundary.** No RBAC; authentication is the transport's job. Safe only because binding is default-closed |

Two of them are worth reading together, because a chain of reasoning that once connected them has
been explicitly **cut**: `ops-I4` (reproducibility) and `ops-I5` (one funnel) were both once read as
requiring a single writer thread. Neither does. `ops-I4`'s hash is a **multiset** over each fact's
logical form, so write order and writer count cannot move it, and `ops-I5` asks that there be no
path around the rules, not that one core apply them. What actually needed serialising was the
key-to-fact bijection, and it now has a mechanism of its own — see
[Storage](storage.html#the-other-half-one-key-names-exactly-one-fact).

## Ownership and locking

- A **root lock** gives one process the store root. A root held by something that is not listening
  on the socket is refused **by name** rather than opened — the message says both halves, because
  "something holds this and is not answering" is the genuinely confusing case.
- The **socket is the server-detection mechanism**, and there is no other autodetect. Its presence
  is what a client uses; its absence is what an offline lifecycle command uses.
- Inside the server, writers are excluded **per key**, not per database. What remains at the
  database level is the **seal barrier**: writers hold it shared, `finish` takes it exclusive, and
  that is `ops-I2`.
- `list` and `describe` take no lock and open no store (`ops-I7`), so they work while a server
  holds everything under the root.

## On disk

```text
<data_dir>/
├── fjord.sock                     # the socket; its presence ⇒ a server is here
└── <name>/<instance>/             # instance: a ULID
    ├── FJORD_META                 # the sidecar: temp + fsync + rename (ops-I3)
    │     name, instance, status, format version, schema fingerprint,
    │     content fingerprint (at finish), counts, size, created_at
    ├── schema/                    # the embedded canonical schema
    └── <storage files>            # keys.<id> / entities.<id> per predicate
```

The sidecar is the fast enumeration path; the embedded schema copy is the durable fallback **and**
the source a server reads a database's schema back from. The field list is fixed and deliberately
has no "externally modified" flag — there is no such concept (`ops-I6`) — and no provenance field
yet. Both are additions the versioned format can take later, which is what the format version is
for.

## Running a server

```bash
fjord --data-dir /var/lib/fjord serve --ready-file /run/fjord.ready
```

- **Binds a Unix socket only, by default** (`ops-I10`). TCP is `--listen-tcp host:port`, with no
  config-file entry and no environment variable, so a port can only appear because somebody typed
  one. It is an opt-in to *reachability*, not to access control: the handshake accepts anonymous,
  so whoever passes it is taking on the gateway in front of it.
- **`--ready-file` appears after the listener accepts.** The socket path is derived rather than
  chosen, so the file only has to appear — but it has to appear *after*, or it is a race dressed as
  a signal.
- **A stream is a task, and one fair writer drains them round-robin**, so a long query does not
  delay a short one on the same connection.
- **Blocking work runs on a blocking pool**, never on the reactor.

### Systemd, roughly

```text
[Service]
ExecStart=/usr/local/bin/fjord --data-dir /var/lib/fjord serve \
         --ready-file /run/fjord/ready
Restart=on-failure
```

There is nothing to configure beyond the root: the socket derives from it, and clients find it
without being told where the data is. Access control is the socket's permissions.

## Deployment shapes

### Build in CI, serve elsewhere

The workflow the design assumes is **a fresh sealed artifact per build**:

```bash
fjord --data-dir ./out create code --schema ./schemas/code.sigla
fjord --data-dir ./out serve --ready-file ./ready &
# … a producer writes facts over the socket …
fjord --data-dir ./out finish code
```

Lifecycle commands work with **no server running** — that is the amendment the offline path exists
for. A reader always goes through a server.

Then the artifact is a directory: `tar` it, publish it, and untar it into a store root on the
serving side. There is no registration step, because the filesystem is the catalog.

### Scaling readers

A server owns its snapshot. Horizontal scaling is **more processes, each with its own copy** of an
immutable Complete database. That is what `ops-I2` plus tar-able directories buy, and it is the
place the design is most exposed: a copy per reader multiplies the artifact rather than amortising
it, and shipping bytes is a real cost. It is recorded as the honest counterweight to the "no
cross-database anything" rule.

### Freshness

A Complete database is immutable, so "freshness" is **how often a new one is built**. That makes it
an indexing-throughput question rather than a serving one, and it is why write throughput is
measured but not yet targeted.

## Backup and restore

Not built as commands, and mostly not needed as ones:

- **Backup is a tar of the directory, Complete databases only.** A file-level copy of a Writable
  database is unsafe under single-process ownership and explicitly out of scope. Include the
  sidecar.
- **Restore is an untar into the store root.** No registration step exists. Validate that the
  sidecar parses, the status is Complete, and — if you recorded it — the content fingerprint
  matches.
- The same mechanism serves the future copy-on-start reader-scaling mode.

## Gaps this design names

Each of these is a *specified* absence rather than an oversight. They are listed here so an
operator is not surprised by one.

| Gap | Where it stands |
|---|---|
| `fjord write` from files | Unbuilt. The file format, the block encoding and the sync-marker splitting rule are all defined; the pipeline is not wired to a command |
| `db verify` | Unbuilt. Recomputing the content fingerprint is cheap and specified. Structural at-rest validation (the two maps agreeing, scan order holding) is guarded at *write* time and nowhere after a crash-and-recover |
| Per-predicate statistics | Not recorded. `finish` is the natural place, and nothing feeds the reorderer's selectivity heuristic today — which is why it does not have one |
| Retention | No policy engine, and the workflow generates the problem: "a fresh artifact per run" means rebuilds accumulate and `db rm` is a manual verb. "Keep the newest *n* Complete instances" is the shape to build |
| Provenance and properties | No field records what a database was built from. Both are safe as descriptive metadata; neither must ever become a *functional* input, because the schema is embedded and identity is content |
| Per-stream flow control | Deferred. Bounded per-stream queues plus per-connection backpressure in the meantime |
| A resumable deadline | A timeout unwinds terminally rather than handing back a cursor — a mid-descent position is not representable in the token as it stands |
| Authentication | None, by design (`ops-I10`). The handshake has no credential field at all |

## Two operational rules worth internalising

**Never reach past a server.** A bare name always means "ask the local server", and a failure says
what to do about it rather than quietly opening the directory — because a server may be holding it.
This is why there is no `--embedded` flag: under the address grammar, a path in the `where` slot is
a socket.

**Sealing is where the artifact's shape is decided.** `finish` merges every tree before it walks
them, which on a large database is tens of seconds of rewriting with nothing to show for it. That
is stated *before* the wait rather than explained after it — and it is the difference between a
reader seeking once per plan level per page and seeking up to 180× that.
