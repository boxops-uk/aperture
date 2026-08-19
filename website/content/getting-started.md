---
title: Getting started
description: Build the binaries, create a database, start a server, write some facts, and ask it a question — in about five minutes.
---

Everything below was run against the repository as it stands. Output is quoted as it was
printed.

## Prerequisites

| You need | Why | Optional? |
|---|---|---|
| A Rust toolchain (edition 2024, stable) | Builds `fjord`, the server and the viewer | no |
| `python3` | Regenerates the example index, and serves these docs | for the example corpus |
| .NET SDK 8+ | The C# client, the demo producer and the real indexer | yes |

There is nothing else — no database to install, no daemon to configure. The storage engine
([fjall](https://github.com/fjall-rs/fjall)) is a Rust dependency, and a database is a
directory.

## 1. Build

```bash
cargo build --release --bin fjord
```

That gives you `target/release/fjord`, the one command-line tool. Two other binaries
are worth knowing about:

```bash
cargo build --release --bin fjord-viewer        # the code-search site
cargo build --release --example loadgen         # a producer that can fill a database
```

For a first run, `--release` matters more than usual: a debug build of the executor is
several times slower and is not the thing you want a first impression of.

## 2. Create a database

A database is created **against a schema**, and that schema is frozen and embedded in it for
its lifetime. There is no default: the schema decides what every stored row means, so a
database whose schema nobody chose is one nobody can describe.

`schemas/code.sigla` in the repository is a worked example — twenty-seven predicates
describing files, declarations, references, a build graph and a declaration graph. It is what
the .NET indexer writes, what the viewer reads and what every benchmark here measures, so it
is the one to start from.

```bash
fjord --data-dir ./db create code --schema schemas/code.sigla
```

```text
created code (01M0BN4HG1W821VK1R7R9E26P1) against schemas/code.sigla
```

The name is `code`; the ULID is the **instance**. `--data-dir` is the **store root** — the
directory databases live under, and the thing a server owns.

```bash
fjord --data-dir ./db list
```

```text
NAME  INSTANCE                    STATUS    SCHEMA        CONTENT  FACTS  BYTES  CREATED
code  01M0BN4HG1W821VK1R7R9E26P1  writable  b08eea634e86  -        -      -      2026-08-19 00:01:04Z
```

`list` reads sidecar files and never opens the storage engine, so it works while a server
holds every database under the root.

## 3. Start a server

Every client talks to a server — there is no "open the directory directly" path for
readers. Locally that is a Unix socket.

```bash
fjord --data-dir ./db serve --ready-file ./ready &
while [ ! -e ./ready ]; do sleep 0.1; done
```

```text
fjord serve
  data dir   ./db
  socket     ./db/fjord.sock
  protocol   2
  schema     0xb08eea634e866a75  (the built-in one; each database is served with its own)
  databases  1
    code                 writable
```

`--ready-file` appears **after** the listener is accepting, so waiting on it is a signal
rather than a race. The socket path is derived from the store root, which is how a client
finds a server without being told where the data is.

:::warn Socket paths are short for a reason
A Unix socket path has a hard length limit of about 100 bytes. If your store root is deep,
pass `--socket /tmp/fjord.sock` explicitly and name it in the address —
`/tmp/fjord.sock//code`.
:::

## 4. Put some facts in it

There is no `fjord write` command yet — [file ingestion](status.html) is unbuilt. Facts
arrive over the wire, from a producer. Three exist today:

```bash
# a synthetic index: 200 files x 5 declarations, plus lines, refs and modules
cargo run --release --example loadgen -- --data-dir ./db --files 200 --decls-per-file 5
```

```text
seeding 1,000 declarations over 200 files, 1,000 facts per block
  5,200 created, 11,000 deduped in 46.87ms — 345,662 facts/s touched, 21,337 decls/s
```

The other two are the .NET client's demo (a tiny hand-written index) and
`Fjord.Indexer`, which runs a real design-time build with Roslyn over a checkout — see
[Clients & the viewer](clients.html). To write facts from your own program, see
[the client section](clients.html#writing-facts-from-rust).

`11,000 deduped` is the interesting number. The producer holds **no fact ids**: it sends
each declaration with its module nested inside it, and the module with its file nested
inside that. The server interns each nested fact, so a file named a thousand times is
written once and deduplicated 999 times.

## 5. Ask it something

```bash
fjord --data-dir ./db query code 'F where src.File F' --limit 3
```

```text
VALUE
src/f0000000.py
src/f0000001.py
src/f0000002.py
3 row(s)
fjord: stopped at 3 rows; raise or drop --limit to see the rest
```

A query is a **head pattern**, the word `where`, and statements. Capture fields by name
and shape the output with a record head:

```bash
fjord --data-dir ./db query code \
  '{name = N, line = L} where src.Decl {module = M, name = N, line = L}' --limit 5
```

```text
LINE  NAME
1     symbol_0000000_000
18    symbol_0000000_001
35    symbol_0000000_002
52    symbol_0000000_003
69    symbol_0000000_004
5 row(s)
```

Find-references — the question a code index exists to answer — is a join through a
reference, and the schema is laid out so that it seeks:

```bash
fjord --data-dir ./db query code \
  '{f = F, l = L} where src.Ref {to = src.Decl {name = "symbol_0000000_000"}, file = F, at = {line = L}}' \
  --expand
```

```text
F                L
src/f0000001.py  2
1 row(s)
```

Without `--expand`, `F` prints as `#9:2` — a fact id, because that is what a reference is
once stored. Expansion is the client asking the server *what fact does this id name*, and
it is off unless you ask for it: it costs one point read per distinct reference.

## 6. Use the shell

```bash
fjord --data-dir ./db shell code
```

```text
fjord shell — `code` on ./db/fjord.sock
  28 predicate(s) · rows print as jsonl · :help for commands
```

The shell compiles what you type **locally**, against the schema the server said it
serves — so a mistake is a caret under the word rather than a round trip, and `:plan` can
show you the plan without running anything.

```text
sigla> :limit 3
sigla> F where src.File F
"src/f0000000.py"
"src/f0000001.py"
"src/f0000002.py"
  :more for the next 3 — 3 so far
sigla> :more
```

`:more` holds a real resume token across a real round trip. Full command list:
[Shell reference](shell.html).

## 7. Seal it

A database is an **artifact**. Sealing flushes and merges every tree, hashes the content,
records the identity, and flips the status — after which every write is refused, forever.

```bash
fjord --data-dir ./db finish code
```

```text
sealing code — merging trees, then computing identity
sealed code: 5200 facts, 849350 bytes, identity 0xf2c2e86612f579e0
```

The identity is `hash(canonical schema, base facts)` — a content hash, so the same inputs
build a byte-identical answer whatever order they were written in. `list` now shows the
database as `complete`, with that number under `CONTENT`, and any writer is refused at the
handshake:

```text
loadgen: cannot connect to ./db/fjord.sock: `code` is complete: it takes no more writes
```

Merging at `finish` is not cosmetic: an unmerged tree was measured seeking at up to 180×
a merged one, and the artifact roughly halves on disk. See
[Performance](performance.html).

## 8. Browse it

```bash
fjord-viewer ./db/fjord.sock//code --bind 127.0.0.1:8088
```

A code-search site — browse, file view with line-level cross-references, prefix search,
symbol pages — built entirely out of ordinary queries through the ordinary client. See
[Clients & the viewer](clients.html#the-viewer).

## What to read next

- [Walkthrough](walkthrough.html) — the same path with more of the interesting corners.
- [Concepts](concepts.html) — facts, predicates, keys, values, lifecycle.
- [sigla query language](query-language.html) — the whole language, construct by construct.
- [CLI reference](cli.html) — every command, flag, address form and config key.
