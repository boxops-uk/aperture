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
| .NET SDK 10 | Runs the indexer that seeds the tour's corpus (step 4) | for the corpus |
| `python3` | Serves these docs locally | yes |

There is nothing else — no database to install, no daemon to configure. The storage engine
([fjall](https://github.com/fjall-rs/fjall)) is a Rust dependency, and a database is a
directory.

## 1. Build — or download

:::note Prebuilt binaries
Every [GitHub release](https://github.com/boxops-uk/fjord/releases/latest) carries `fjord`
and `fjord-viewer` for Linux x86_64, with SLSA provenance — verify what you downloaded with
`gh attestation verify ./fjord --repo boxops-uk/fjord`. Downloading skips this whole step.
:::

```bash
cargo build --release --bin fjord
```

That gives you `target/release/fjord`, the one command-line tool. Two other binaries
are worth knowing about:

```bash
cargo build --release --bin fjord-viewer        # the code-search site
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
arrive over the wire, from a producer. The one this tour uses is the repository's own:
`Boxops.Fjord.Indexer` runs a real design-time build per project, asks Roslyn what every
name in the result means, and writes the answers down the socket — pointed, here, at the
.NET code it is itself part of. Three projects: the client library, the demo producer, and
the indexer indexing itself.

```bash
dotnet run --project clients/dotnet/Boxops.Fjord.Indexer --configuration Release -- \
  --source clients/dotnet --at ./db/fjord.sock//code
```

```text
indexing /path/to/fjord/clients/dotnet
  schema fingerprint b08eea634e866a75
  entry point /path/to/fjord/clients/dotnet/Boxops.Fjord.slnx
  3 C# project(s) in the solution
  built Boxops.Fjord.Demo.csproj (net10.0, 4 files, 2.7s)
  built Boxops.Fjord.Indexer.csproj (net10.0, 12 files, 2.7s)
  built Boxops.Fjord.Client.csproj (net10.0, 13 files, 4.9s)
  build layer: 3 project(s), 3 from a design-time build, 20 file(s) attributed exactly

connecting to ./db/fjord.sock//code, 1 writer(s)
  connected: protocol 2, 29 predicates, schema b08eea634e866a75

indexed 20 file(s) in 4.0s
  total                       15,441 facts in 28 blocks
  server                      15,039 created, 40,382 deduped

references: 2,672 resolved, 1,718 to declarations outside the index, 1 unresolved
```

`40,382 deduped` is the interesting number. The indexer holds **no fact ids**: it sends
each declaration with its module nested inside it, and the module with its file nested
inside that. The server interns each nested fact, so a file named a few thousand times is
written once and deduplicated the rest — 55,421 facts touched, 15,039 rows exist.

To write facts from your own program, see
[the client section](clients.html#writing-facts-from-rust); the .NET demo producer is the
same idea at fixture size.

## 5. Ask it something

```bash
fjord --data-dir ./db query code 'F where src.File F' --limit 3
```

```text
VALUE
Boxops.Fjord.Client/Blocks.cs
Boxops.Fjord.Client/Buffers.cs
Boxops.Fjord.Client/Crc32.cs
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
37    Block
56    Block.Encode
44    Block.HeaderLength
41    Block.Magic
49    Block.MaxFacts
5 row(s)
```

Find-references — the question a code index exists to answer — is a join through a
reference, and the schema is laid out so that it seeks. `Crc32` is the client's checksum
type, and every use of it turns out to be in the block encoder:

```bash
fjord --data-dir ./db query code \
  '{f = F, l = L} where src.Ref {to = src.Decl {name = "Crc32"}, file = F, at = {line = L}}' \
  --expand
```

```text
F                              L
Boxops.Fjord.Client/Blocks.cs  98
Boxops.Fjord.Client/Blocks.cs  99
Boxops.Fjord.Client/Blocks.cs  99
Boxops.Fjord.Client/Blocks.cs  99
Boxops.Fjord.Client/Blocks.cs  99
5 row(s)
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
  29 predicate(s) · rows print as jsonl · :help for commands
```

The shell compiles what you type **locally**, against the schema the server said it
serves — so a mistake is a caret under the word rather than a round trip, and `:plan` can
show you the plan without running anything.

```text
sigla> :limit 3
sigla> F where src.File F
"Boxops.Fjord.Client/Blocks.cs"
"Boxops.Fjord.Client/Buffers.cs"
"Boxops.Fjord.Client/Crc32.cs"
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
sealed code: 15039 facts, 2378056 bytes, identity 0xdd0fe1300c88a3fa
```

The identity is `hash(canonical schema, base facts)` — a content hash, so the same inputs
build a byte-identical answer whatever order they were written in. `list` now shows the
database as `complete`, with that number under `CONTENT`, and any writer is refused at the
handshake:

```text
Boxops.Fjord.Client.FjordServerException: ModeRefused: `code` is complete: it takes no more writes
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
