---
title: Clients & the viewer
description: The .NET client and real indexer, the code-search viewer, and what it takes to write a client of your own.
---

Everything that talks to Fjord is a client of one protocol. There is no privileged path: the
CLI, the shell, the viewer and a C# program written outside the repository all use the same
frames.

## The .NET client

`clients/dotnet` is a C# implementation of the wire protocol, plus two programs that use it.

| Project | What it is |
|---|---|
| `Fjord.Client` | The library: varints, CRC-32, the value codec, blocks, frames, the handshake, a connection |
| `Fjord.Demo` | A console program that writes a small code index and queries it back |
| `Fjord.Indexer` | A real indexer: Buildalyzer and Roslyn over a .NET checkout, at whatever size the checkout is |

It exists to answer a question the Rust tests cannot: **is the protocol implementable from
outside?** A client written in the same repository, against the same types, can agree with the
server by accident — sharing a constant, sharing an enum, sharing an assumption nobody wrote
down. This one shares nothing but the specification.

It has already earned that twice: it found a block header length stated in one place and
miscounted in another, and a fingerprint that would have depended on the client's byte order.

```bash
./clients/dotnet/run-demo.sh
```

```text
writing 6 declarations, every reference nested
  created 12, deduped 6 (of 18 facts touched)
  6 declarations + 3 modules + 3 files = 12 distinct facts

a reference with a nested-record key: created 1, deduped 4
the same declarations again: created 0, deduped 18
```

Read the last line: sending the same declarations again writes **nothing**. Interning is
idempotent, which is what makes retrying after a dropped connection safe.

### What a client has to know

**It has to have the schema.** The value codec sends no field names, no type markers and no
record arities — the server has them and so does the client. So a client states the schema it
uses, and asserts it at the handshake with a fingerprint, which turns "we disagree about the
schema" from a corrupted database into a refused connection.

That has caught things too. When the server moved from a cut-down three-predicate schema to the
real code index, the demo was refused at the handshake with both fingerprints named, before a
byte of data flowed.

**It does not have to know how facts are stored.** The storage codec is order-preserving,
self-delimiting and frozen on disk; none of that is on the wire, and a client never sees it.

### The byte-for-byte golden

```bash
./clients/dotnet/emit-golden.sh
```

That writes the blocks the C# client encodes for a fixed corpus, and `fjord-client`'s
`byte_identical_with_the_dotnet_client` asserts the Rust encoder produces the same bytes. The
corpus and the schema are stated **independently on each side on purpose** — a shared statement
would make the two agree by construction, which is the agreement being tested. The Rust test
needs no `dotnet`; regenerating the golden does.

## The real indexer

```bash
./clients/dotnet/index-repo.sh ~/src/OrchardCore
```

The demo's argument made at scale: the same library, the same nested references and the same
handshake, driven by a design-time build per project and a compiler that answers what every name
means. It is where a database large enough to be worth measuring comes from — and where twenty-one
of the built-in schema's predicates come from, because the build layer and the declaration graph
cannot be answered by a syntax walk at all.

The run reports what interning cost:

```text
  server                  18,176,899 created, 44,422,889 deduped
```

Five million references naming nine hundred thousand declarations **is** that dedup count. A
producer holding no fact ids is an elegance argument at six declarations; at eighteen million
facts it is the only tractable option, because the alternative is a second pass over an index that
no longer fits in memory, ordered so that every target is written before every reference to it.

It can also write the same facts into **Glean's** own JSON batch format
(`--glean-out <dir>`, `./clients/dotnet/index-repo-glean.sh`) against a predicate-, field- and
field-order-preserving translation of the built-in schema. One walk, two sinks — so a comparison
of the two systems is a comparison of the two systems and not of two indexers. Two honesty
conditions are recorded with it: references stay nested on that path as well, and **emitting is
not writing** — the load is a second phase with its own clock, so the honest total for Glean is
emit plus load.

### What the .NET client does not do

It mirrors the server, so it stops where the server does. Streams are issued sequentially — the
ids are real and the server tags every reply — but it sends a stream's frames and reads its
replies before starting the next. There is no cancellation and no flow control on that side.

There is no test project either, deliberately: the console program *is* the test, and a unit test
of this codec against constants copied from the Rust would only prove the constants were copied.

## The viewer

```bash
fjord --data-dir ./db serve &
fjord-viewer ./db/fjord.sock//code --bind 127.0.0.1:8088
```

A code-search site over a Fjord database: HTML written by hand, no assets, no framework.

| Route | Page |
|---|---|
| `/` | Browse — the file tree |
| `/file/{path}` | A file, with line-level cross-references |
| `/search` | Prefix search over declaration names |
| `/symbol/{name}` | A symbol: where it is declared, and where it is used |
| `/health` | Liveness |

| Flag | Default | Means |
|---|---|---|
| *(positional)* | `code` | The address, in the usual grammar |
| `--bind` | `127.0.0.1:8088` | Where to listen |
| `--pool` | `8` | Idle connections to keep open to the server |

Three things about it are the point rather than the implementation:

- **It is an ordinary consumer of the protocol.** It depends on the client crate and nothing
  below it, which is the claim: a viewer needs no privileged access to a database.
- **Every question a page asks is in one place** (`query.rs`), and each says which key order
  answers it. That is where the schema's index design becomes visible as product behaviour —
  find-references is a seek because `src.Ref` leads with its target, and a file's cross-references
  are a seek because `src.FileXRef` leads with the file.
- **The pool exists because the client is blocking and a web server is not.** A recycled pool with
  a floor rather than a ceiling: a burst opens more connections and closes them on return.

The viewer is also what Phase 11 was built from, and building it is what found the two predicates
that had to be added — a file's cross-references keyed by file, and a case-folded search index —
because the questions a UI asks turned out not to be the questions the schema answered.

## Writing a client

The protocol is [documented frame by frame](wire-protocol.html). A minimal read-only client is:

1. Connect to the socket (or TCP).
2. Send `S` with the protocol version, the database name, `read-only`, and `0` for the schema
   fingerprint — meaning "do not check", which is right for a reader.
3. Read `R`.
4. Send `H`, read `h`, and you have the schema as source. (A Rust client can parse it; a
   hand-written one may skip this and carry the shapes it needs.)
5. Send `Q` with query text on a fresh stream id.
6. Read `T` once, then `D` frames, then `C`.

A producer differs in three places: `read-write` in the handshake, a **non-zero** schema claim,
and `W` → `d`* → `c` instead of `Q`.

### Writing facts from Rust

The client crate is the shortest path. A fact is a predicate, a key and an optional value, and a
reference is either an id or **the whole target fact**:

```rust
use std::sync::Arc;
use fjord_client::{Connection, Mode};
use fjord_wire::{WireFact, WireRef, WireValue};

let mut connection = Connection::connect(
    socket,          // &Path
    "code",          // the database
    Arc::clone(&schema),
    Mode::ReadWrite,
    true,            // assert the schema fingerprint — right for a producer
)?;

// A file fact: a scalar key.
let file = |path: &str| WireFact {
    predicate: FILE,
    key: WireValue::Str(path.to_owned()),
    value: None,
};

// A declaration whose module names a file — every reference nested, no ids held.
let decl = WireFact {
    predicate: DECL,
    key: WireValue::Record(
        vec![
            WireValue::Ref(WireRef::Nested(Box::new(module("store/keys.py", "keys")))),
            WireValue::Str("key_of".to_owned()),
            WireValue::Int(12),
        ]
        .into(),
    ),
    value: Some(WireValue::Str("def".to_owned())),
};

let written = connection.write(DECL, &[decl])?;
println!("created {}, deduped {}", written.created, written.deduped);
```

Two rules to hold on to:

- **A record's fields are in schema order, and the names are not sent.** The bytes are positional
  against the schema both ends hold.
- **A nested reference is the whole target**, and the server interns it bottom-up. Send the same
  target under a thousand parents and one row exists.

Reading is the mirror image:

```rust
let mut rows = connection.query("F where src.File F")?;
while let Some(row) = connection.next_row(&mut rows)? {
    println!("{row:?}");
}
```

`Rows` is a **bookmark**, not a borrow of the connection — so several results can be open at once,
and `take` is the page `:more` is built on.

### Things a client should get right

| | Why |
|---|---|
| Bound `length` before allocating | It sizes a read from a number the peer chose |
| Treat an unknown frame kind as unknown, not malformed | That is what lets a newer peer be answered rather than rejected |
| Emit **minimal** varints | One value must have exactly one encoding, or the block CRC means nothing |
| Cover the header in the block CRC | A corrupted `length` must be caught, not used to skip to the wrong place |
| Nest references rather than inventing ids | An id you did not receive from the server names some other fact |
| Expect ids in rows | A reference comes back as `#p:n`; expand with `F`/`f` if you need the key |
