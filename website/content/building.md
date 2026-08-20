---
title: Building from source
description: The workspace, the build and test commands, the generated grammar, the .NET client, and what each crate is for.
---

Fjord is a Cargo workspace. There is no build system on top of it, no code generation
step you have to run by hand, and no vendored C.

## Build and test

```bash
cargo build                          # everything, debug
cargo build --release --bin fjord # the tool, optimised

cargo test                           # the green suite
cargo test -- --ignored --list       # the invariant coverage ledger
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all
```

`default-members` is the whole workspace, so `cargo build` and `cargo test` mean
*everything* without `--workspace`. That is deliberate: the coverage ledger silently
narrowing to one package as crates are extracted would be a ledger that had stopped
counting.

:::note The coverage ledger
`cargo test -- --ignored --list` prints any guard that is written but not yet live —
each one pinned to an invariant whose subsystem does not exist yet. Work that touches an
invariant is finished only when its guard is un-ignored and green. The ledger currently
lists **nothing**: every invariant's guard is live. See [Testing method](testing.html).
:::

### Generated code

The two grammars are compiled at build time by [`lelwel`](https://crates.io/crates/lelwel)
from `build.rs`, so nothing is checked in and nothing needs regenerating by hand:

| Grammar | Compiled by | Language |
|---|---|---|
| `crates/fjord-engine/src/grammar.llw` | `fjord-engine/build.rs` | sigla queries |
| `crates/fjord-schema/src/syntax/grammar.llw` | `fjord-schema/build.rs` | the schema DSL |

## The workspace, top to bottom

Each crate depends only on the ones **above** it in this list. That is not a convention
any more — the compiler refuses the other direction, and there is no edge pointing back.

| Crate | Holds |
|---|---|
| `fjord-schema` | The type model (`schema`), the physical row id (`id`), schema identity (`fingerprint`) and the schema DSL's front end (`syntax`: lexer, grammar, parse, lower, print, import resolution). Depends on no Fjord crate. |
| `fjord-encoding` | The order-preserving storage tuple codec (`tuple`) and its error type. |
| `fjord-wire` | The **transport** codec and the protocol vocabulary: `varint`, `value`, `crc`, `block`, `frame`, `protocol`. A sibling of `fjord-encoding`, not a layer on it — it shares no bytes with the storage codec. |
| `fjord-store` | The `FactStore` seam, the fjall backend, the in-memory test store, `fact`, the format stamp, and the lifecycle: `catalog`, `meta`, `schema_doc`, `identity`, `ulid`, `lookup_cache`. |
| `fjord-ingest` | The write funnel: `FactSink` (the write seam) and `intern` — a wire fact in, a `FactId` out, nested references resolved bottom-up. |
| `fjord-engine` | **sigla and the machine**: lex → parse → typecheck → flatten → reorder → `Plan`, and the executor. All new query work lands here. |
| `fjord-client` | The client: `address`, `connection`, `rows` (a result as a bookmark), `expand`. Depends on `fjord-wire` and nothing else. |
| `fjord-server` | The protocol over a Unix socket or TCP: `session`, `registry`, `outbound` (the fair writer), `rows`, `blocking`, `server`, `stats`, `catalogue`. |
| `fjord-viewer` | The code-search site: `query`, `render`, `pool`, and the routes. An ordinary consumer of the client. |
| `fjord-cli` | The tool: `cli`, `config`, `commands/`, `output`, `prompt`, `sample_schema`, `workload`. The binary is `fjord`. |

Two test-support modules span crates, and the split is load-bearing:
`fjord_store::fixtures` holds everything store-shaped (probes, model stores,
scan-contract assertions) because a probe has to be *the same* `FactStore` as the store it
wraps; `fjord_engine::fixtures` holds the plan runners and re-exports the rest.

## Binaries

| Binary | Build | What it is |
|---|---|---|
| `fjord` | `cargo build --release --bin fjord` | The command line tool: create, serve, query, shell, schema, list, describe, finish, db rm |
| `fjord-viewer` | `cargo build --release --bin fjord-viewer` | The code-search site over a database |

## Measuring instruments

They live in `crates/fjord-cli/examples/` and are deliberately examples rather than
subcommands: measuring instruments are not things anyone should find while looking for how to
use the database. Run them from anywhere in the workspace — `cargo run --release --example
loadgen -- …`.

| Example | Rung | What it isolates |
|---|---|---|
| `examples/engine.rs` | S1–S3 | The engine with everything else taken away |
| `examples/breakdown.rs` | S4 | The fixed per-query cost, by subtraction |
| `examples/loadgen.rs` | S5 | One connection, the whole round trip — and the seeder |
| `examples/soak.rs` | S6–S7 | A mixed population, and steady state over hours |
| `examples/codesearch.rs` | S6 | The product's own traffic rather than a generic mix |
| `examples/ingest.rs` | write | The write path per layer: commit, resolve, decode |

```bash
cargo run --release --example loadgen -- --data-dir /tmp/fjbench --files 20000
./scripts/bench.sh          # create, serve, seed, measure — one command
```

## Where a database to work against comes from

There is no bundled corpus. `schemas/code.sigla` describes three layers, and only the first —
files, modules, declarations, references, their spans — is answerable by a syntax walk; the
build layer and the declaration graph need a compiler and a build system, which is what the
.NET indexer has. So the way to get a database worth querying is to point that at a real
checkout:

```bash
./clients/dotnet/index-repo.sh ~/src/OrchardCore
```

`./scripts/bench.sh` is the other way in: it creates, serves, seeds and measures in one
command, from a synthetic corpus sized by `FILES` and `DECLS`.

## The .NET client

A second implementation of the wire protocol, in C#, sharing no constants and no enums
with the Rust side. It exists to answer what the Rust tests cannot: whether the protocol
is implementable from outside. It has already found two faults that way.

```bash
./clients/dotnet/run-demo.sh                  # write a small index and query it back
./clients/dotnet/index-repo.sh <checkout>     # index a real .NET repository
./clients/dotnet/emit-golden.sh               # regenerate the byte-for-byte golden
```

The golden is checked in, and `fjord-client`'s
`byte_identical_with_the_dotnet_client` asserts the Rust encoder produces the same bytes
for the same corpus. The Rust test needs no `dotnet`; regenerating the golden does. See
[Clients & the viewer](clients.html).

## Repository layout

```text
fjord/
├── crates/              the workspace, bottom to top (table above). `fjord-cli` is
│                       the `fjord` binary; its examples/ are the instruments
├── schemas/             code.sigla, the sample schema every client here builds against
├── clients/dotnet/      the C# client, demo producer and real indexer
├── docs/                the design book — chapters 1–7 plus operations and references
├── bench/FINDINGS.md    what has actually been measured
├── PLAN.md              the phase tree and current state
└── website/             this documentation site
```

The design book in `docs/` is the source of record. This site is written from it and links
back into it by chapter where a subject has more depth than a docs site should carry.
