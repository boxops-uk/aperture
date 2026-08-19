---
title: CLI reference
description: Every command the `fjord` binary has, the address grammar every client shares, and the configuration layering — plus what is deliberately not there yet.
---

One binary, `fjord`. Every database-taking command accepts any address form, so "local or
remote" is a property of the **address**, not of the command — which is why there is no
`--remote` flag anywhere.

```text
fjord
├── serve                          run the server over a store root
├── create   <name>                a new Writable database (the schema is fixed here)
├── finish   <name>                seal: Writable → Complete
├── list                           enumerate databases
├── describe <name>                metadata and schema
├── query    <name> <QUERY>         one-shot query
├── shell    [<name>]              interactive REPL
├── schema
│   ├── check       <file>          resolve, canonicalise, report
│   ├── fingerprint <file>          the schema's number, and each predicate's
│   └── diff        <a> <b>         Identical | Compatible (n added) | Breaking
└── db
    └── rm  <name>                 delete a database
```

## Global options

Available on every command.

| Flag | Also | Means |
|---|---|---|
| `--data-dir <PATH>` | `FJORD_DATA_DIR` | The store root: where databases live, and what the socket path derives from |
| `--config <PATH>` | — | A config file. Without it, `./fjord.json` is read if it happens to be there |
| `--schema-path <PATH>` | `FJORD_SCHEMA_PATH` | Where a schema's imports are looked for. Repeatable, first match wins |
| `-v`, `--verbose` | — | Say more. Repeatable |

**There are no addressing flags.** `--host` and `--port` were specified once and dropped: the
address says where, and a flag that said it too would be a second way to disagree. Nothing but a
server needs `--data-dir` at all — a client finds the default socket without being told where the
data is.

## Addressing

One grammar, used by every client — the CLI, the viewer, and the .NET tools:

```text
[where//]name[@instance]
```

| Address | Means |
|---|---|
| `code` | The default target |
| `code@01M0B3D…` | A particular instance of it |
| `//code` | The same thing, said explicitly |
| `box:7280//code` | TCP |
| `box//code` | TCP, port **7280** |
| `/run/user/1000/fjord.sock//code` | A Unix socket |
| `./dev.sock//code` | A relative socket path |
| `box:7280//` | That server, no database — a control session |

Two rules carry it, and both are derived rather than invented:

- **Split at the *last* `//`**, because a database name may not contain `/` and a socket path
  may. That is what makes `/tmp//sockets//code` parse instead of misread, and why there is no
  "everything before the final slash" rule to learn.
- **A relative socket path needs `./`**, because `dev.sock//code` is otherwise
  indistinguishable from a host called `dev.sock`. It is the rule a shell already imposes on
  `./script`.

What is deliberately absent: **no scheme** (`//` announces where the target is, and nothing needs
to announce that a Fjord address is one); **no names to look up** (a `where` is always
literal, never an alias resolved through a registry — a named target whose meaning lives in
ambient machine state is how `kubectl delete` reaches the wrong cluster); **no credentials** (the
handshake has no credential field, so `user@host` would have been syntax with nothing behind it).

### Resolution

1. An address naming **no** target goes to the default one. If nothing is listening: an
   actionable error naming the target. **Never** a silent fallback to opening the directory —
   a server may be holding it.
2. An address naming a target goes there, and **has no offline half**. `FJORD_TARGET` and a
   config file's `target` count as naming one.
3. **The socket is the server-detection mechanism.** There is no other autodetect.

**One amendment, for lifecycle commands only.** `create`, `finish` and `db rm` resolve as: a
server listening on the default socket takes the command; nothing listening means this process
does the work itself, under the root lock. Rule 1 as written would refuse them with no server up,
which would make the tool unusable offline for the one job — building an artifact in CI — that the
offline path exists for. It does not weaken single-process ownership, and the ordering is why:
nothing is opened until the socket has already answered that no server is there, and a root held
by something that is not listening is refused **by name** rather than opened.

Reads (`list`, `describe`) never faced the question: they read sidecars, take no lock and open no
store.

## Configuration

Layered, every field optional so an unset layer cannot clobber a lower one:

```text
default  →  config file  →  environment (FJORD_…)  →  CLI flag  →  the address argument
```

The address is the top layer, because it is the argument.

The file is `./fjord.json`, or `--config <path>` — **the working directory only, with no walk
upwards**. Cargo and git search parents, and a connection target inherited from a directory
nobody was thinking about is the same invisible state a global registry would be, only harder to
notice. CI writes the file where it runs.

```json
{
  "target": "/run/user/1000/fjord.sock",
  "data_dir": "/var/lib/fjord"
}
```

| Key | Used by | Notes |
|---|---|---|
| `target` | Client commands | Where a server is — a host, a `host:port`, or a socket path. Also `FJORD_TARGET`. **Never a database name** |
| `data_dir` | Server, offline lifecycle | The store root. Also `FJORD_DATA_DIR` |

A file may say *where*, never *which database*: that would be the same ambient-state problem one
level down, where it would decide what a command operates on.

### Where things default to

| | Default |
|---|---|
| Store root | `$FJORD_DATA_DIR`, else `$XDG_DATA_HOME/fjord`, else `$HOME/.local/share/fjord`, else `./fjord-data` |
| Socket, root **not** chosen | `$XDG_RUNTIME_DIR/fjord.sock` |
| Socket, root chosen (`--data-dir`) | `<root>/fjord.sock` |

The runtime-directory default is short **on purpose**: a Unix socket path has a hard limit of 108
bytes on Linux, and one derived from a deep data directory is a path the kernel refuses.

---

## `fjord serve`

Run the server owning a store root. It acquires exclusive ownership of the root and refuses to
start if another server holds it.

| Flag | Means |
|---|---|
| `--socket <PATH>` | Where to bind. Defaults to the derived path above |
| `--listen-tcp <HOST:PORT>` | **Also** listen on TCP. Flag only — no config entry, no environment variable |
| `--ready-file <PATH>` | Written once the listener is accepting |
| `--commit-per-block` | Commit a write stream's facts once per block instead of once per fact |

```bash
fjord --data-dir ./db serve --ready-file ./ready &
while [ ! -e ./ready ]; do sleep 0.1; done
```

`--ready-file` appears **after** the listener accepts, so waiting on it is a signal rather than a
race. That matters because the socket path is derived rather than chosen, so the file only has to
appear.

:::warn `--commit-per-block` trades exactly one thing
Committing per fact is 41% of interning, so a bulk load pays a large fixed tax for a guarantee it
may not need. With this on, a fact's id is handed out before its bytes are durable — so a crash
mid-ingest may leave a database holding a reference to a fact that was never written. That is
caught at `finish`, which walks every reference, and the database **refuses to seal**. The cost is
re-running the index, never a wrong answer from one that sealed.
:::

## `fjord create <name>`

```bash
fjord --data-dir ./db create code
fjord --data-dir ./db create people --schema ./people.sigla
```

| Flag | Means |
|---|---|
| `--schema <FILE>` | The entry file to create it against. Without it, the built-in code index |

The schema is resolved (imports and all), canonicalised, fingerprinted and **embedded**. It is
frozen for the database's lifetime, so this is the one moment it can be chosen. Every
predicate's storage trees are materialised up front from the schema.

The directory is `<root>/<name>/<instance>/`, where the instance is a ULID. Content identity does
not exist yet — it hashes the base facts, so it can only be computed at `finish`.

## `fjord finish <name>`

```bash
fjord --data-dir ./db finish code
```

| Flag | Means |
|---|---|
| `--allow-zero-facts` | Seal a database holding no facts |

In order: flush and sync everything → **merge every tree** → compute
`hash(canonical schema, base facts)` → record it in the sidecar → flip the status, as the last
durable act. It is never observable that the metadata says Complete while the data is not
durable.

The merge is a major compaction, and `finish` is the only place it belongs: ingestion leaves each
tree in whatever shape the write order produced, and a Writable database might be written again
in a moment. Sealing is where the shape becomes final — and the shape is what every future reader
pays. An unmerged tree was measured seeking at up to **180×** a merged one, with the artifact
also roughly halving on disk.

Sealing an **empty** database is refused without the flag, because a silently-empty sealed
artifact is the classic CI failure that looks like success. Finishing a Complete database is a
no-op with a notice; a crash mid-finish leaves it Writable and the command can be re-run.

## `fjord list`

```bash
fjord --data-dir ./db list
fjord --data-dir ./db list --format json
```

```text
NAME  INSTANCE                    STATUS    SCHEMA        CONTENT       FACTS  BYTES   CREATED
code  01M0BNMTQ3RWQFMM755NV1MWA3  complete  b08eea634e86  f2c2e86612f5  5200   849350  2026-08-19 00:09:58Z
```

Walks the store root and reads **sidecars only** — it never opens the storage engine, so it works
while a server holds every database under the root. There is no manifest: the filesystem is the
catalog, and any index would be rebuildable and never authoritative.

## `fjord describe <name>`

```bash
fjord --data-dir ./db describe code
fjord --data-dir ./db describe code --schema      # dump the embedded schema itself
fjord --data-dir ./db describe code --format json
```

Prints the sidecar metadata, then every predicate with its type and fingerprint. `--schema` dumps
the canonical schema source — which is the text `create --schema` would take, so it round-trips.

## `fjord query <name> <query>`

```bash
fjord query code 'F where src.File F' --limit 20
fjord query code 'D where D = src.Decl _' --format jsonl --expand
fjord query code 'R where R = src.Ref _' --count --timing
fjord query '/tmp/fjord.sock//code' 'F where src.File F'
```

| Flag | Means |
|---|---|
| `--format table\|json\|jsonl\|raw\|count` | How rows print. Everything but `table` streams |
| `--limit <N>` | Stop after N rows, cancelling the rest in band |
| `--timeout <SECONDS>` | Give up after this long, cancelling in band |
| `--timing` | Rows and elapsed time, to stderr, so it survives a pipe |
| `--profile` | What the query examined, per step of its plan, to stderr |
| `--count` | How many rows, and none of them |
| `--expand[=HOPS]` | Show the fact a reference names instead of its id — bare follows the chain to the end |

Three things are worth knowing:

- **`--limit` is not `LIMIT`.** The query is unchanged and the server does the work up to the
  point the cancel lands; what it bounds is what crosses the socket.
- **`--count` is a different order of expense from `| wc -l`.** The plan and the executor are the
  same; what differs is that the server counts instead of encoding, and encoding plus framing is
  the majority of the cost.
- **`--expand` costs a point read per distinct reference**, answered from a cache within the run.
  That is why it is off unless asked for.

Rendering is **always** client-side: the wire carries the binary format and the server never
produces JSON.

## `fjord shell [<name>]`

```bash
fjord --data-dir ./db shell code     # the product shell, over the wire
fjord shell                          # the embedded demo, over a scratch database
```

With a database it is the product shell and always speaks the protocol, even against a local
server — so the format has a permanent exerciser and `:more` holds a real cursor across a real
round trip. With none, it is the embedded demo over a database it seeds itself, which is the one
thing no wire client can do.

Full command list: [Shell reference](shell.html).

## `fjord schema …`

```bash
fjord schema check ./code.sigla
fjord schema fingerprint ./code.sigla [--format json] [--canonical]
fjord schema diff before.sigla after.sigla
fjord schema diff before.sigla code           # a file against a database
```

`check` walks the import closure, unions the blocks and lowers the result — so it answers
unresolved imports, syntax errors and genuine redeclarations, which are the three things a schema
can be wrong about before anything writes a fact.

`fingerprint` prints the schema's number and each predicate's. `--canonical` prints the form the
number is taken over, which is what a second implementation is written against and what to diff
when two ends disagree about a schema they believe they share.

`diff` takes schema files **or database names** in any combination.

## `fjord db rm <name>`

```bash
fjord --data-dir ./db db rm scratch -y
```

Routed through the server if it holds the database (the server closes and deletes); offline, it
requires the lock to be free.

## Exit codes and errors

Every failure is one sentence naming the tool, except a compiler diagnostic, which is printed as
it was rendered — with the code, the caret and colour when stderr is a terminal.

```text
fjord: could not connect to the Fjord server at /run/user/1000/fjord.sock
           is one running? `fjord serve` starts one over this data directory
```

```text
error[reject/unknown-predicate]: `src.Nope` is not a predicate in this schema
  ┌─ <input>:1:9
  │
1 │ X where src.Nope X
  │         ^^^^^^^^^^
```

## Not built yet

Named here rather than left to be discovered. The operations design specifies all of them:

| Command | Status |
|---|---|
| `fjord write <db> [FILE…]` | **Not built.** Facts arrive over the wire from a producer; the file format and block encoding exist, the splitter and pipeline are not wired to a command |
| `fjord db backup` / `db restore` | Not built. A Complete database is a directory — `tar` it |
| `fjord db verify` | Not built. Identity is recorded at `finish`; recomputing and comparing it is specified |
| `fjord completions <shell>` | Not built |

Two more are specified and absent from the sidecar rather than from the CLI: **provenance** (what
the database was built from) and a freeform **properties** map. Both are safe under the
reproducibility rule as descriptive metadata, and both are additions the versioned format can
take later.
