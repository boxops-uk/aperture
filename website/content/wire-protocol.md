---
title: Wire protocol
description: Frames, streams, the handshake, the four query kinds, the write stream, block format and the value encoding — everything a second implementation needs.
---

One socket, framed and multiplexed. Everything talks it: the CLI, the shell, the viewer, and a
C# client that shares no code with the server. Protocol version is **2**.

The transport is a Unix socket by default. TCP is an explicit opt-in — `serve --listen-tcp
host:port`, with no config-file entry and no environment variable, so a port can only appear
because somebody typed one.

## The frame layer

```text
  ┌────────┬────────────┬────────────┬───────────────────────────┐
  │ kind:1 │ stream:4   │ length:4   │ payload (length bytes)     │
  └────────┴────────────┴────────────┴───────────────────────────┘
    9-byte header; the two counts are little-endian
```

PostgreSQL-inspired and deliberately **not** PostgreSQL-compatible. PG's header is a type byte
and a length, and every message belongs to the one conversation the connection *is* — strictly
serial, so a long query blocks a short one. The `stream` field is the departure and the reason
for departing: a query is a stream, a write is a stream, several run at once, and every frame
says which.

Two rules the frame layer holds and nothing above it needs to restate:

- **`kind` is a byte with named constants, not a closed enum.** A framing layer delimits
  messages; deciding what a message *means* is the layer above. An unrecognised kind is handed
  up intact, which is also what lets a peer at a newer version be told "I do not know that
  message" rather than "your bytes are malformed".
- **`length` is bounded before it is trusted**, because it sizes a read from a number the peer
  chose. The cap is 64 MiB (`MAX_PAYLOAD`).

## Frame kinds

| Byte | Name | Direction | Carries |
|---|---|---|---|
| `S` | `STARTUP` | → | Version, database, session mode, schema claim |
| `R` | `READY` | ← | Version, the database's schema fingerprint, predicate count |
| `Q` | `QUERY` | → | Query text, on a new stream |
| `P` | `QUERY_PROFILE` | → | The same, and report what it examined |
| `G` | `QUERY_PAGE` | → | The same, stop after N rows and hand back a token |
| `N` | `QUERY_COUNT` | → | The same, report only how many rows |
| `T` | `ROW_DESCRIPTION` | ← | The head's shape, once per query stream |
| `D` | `DATA_ROW` | ← | One row |
| `p` | `PROFILE` | ← | What the query examined, once, just before `C` |
| `n` | `COUNT` | ← | How many rows |
| `r` | `RESUME` | ← | The resume token, when a page was cut short |
| `C` | `COMPLETE` | ← | The stream finished, with counts |
| `E` | `ERROR` | ↔ | This stream failed, with a code and a message |
| `X` | `CANCEL` | → | Stop this stream |
| `W` | `OPEN_WRITE` | → | Open a write stream |
| `G` | `COPY_IN_RESPONSE` | ← | Ready for blocks *(write streams)* |
| `d` | `COPY_DATA` | ↔ | One block of facts |
| `c` | `COPY_DONE` | → | The write stream's blocks are finished |
| `L` | `CONTROL` | → | A lifecycle request: create, finish, remove |
| `M` | `CONTROL_REPLY` | ← | What it came to |
| `H` | `SCHEMA` | → | "What can I ask you?" — no payload |
| `h` | `SCHEMA_REPLY` | ← | The schema this session is served with, as source |
| `F` | `FETCH` | → | A batch of fact ids |
| `f` | `FETCHED` | ← | One key each, positionally, or an absence |

`G` appears twice, and that is not a typo: it is `QUERY_PAGE` from the client and
`COPY_IN_RESPONSE` from the server. A kind is only ever read in one direction, so the byte is
free to mean one thing outbound and another inbound — the same economy PostgreSQL's protocol
makes.

## The handshake

```text
  →  S   { version, database, mode, schema_fingerprint, [(predicate, fingerprint)…] }
  ←  R   { version, schema_fingerprint, predicates }
```

The database may be **empty**, which is a control session — the only kind of session a
`create` could be sent on.

`mode` is `read-only` or `read-write`, and it is resolved **once** against the database's
status: a Complete database is read-only, full stop, and a write session against one is refused
at establishment rather than per fact.

The schema claim has two halves, and both are checks the client chooses:

- **`schema_fingerprint`** is an equality claim, or `0` for "do not check". Zero is a real
  answer rather than a hole: a reader has nothing to assert.
- **`predicates`** is a per-predicate claim, checked by **subset containment** — which is what
  the freeze invariant actually says, and what lets a producer writing six of a database's
  twenty-seven predicates connect without restating the other twenty-one.

:::note There is no credential field
Access control is entirely the transport's job — socket permissions, or a gateway in front of
opted-in TCP. That is only safe because binding is default-closed, and it is stated as an
invariant (`ops-I10`) rather than left as an omission.
:::

## Asking what you may ask

```text
  →  H   (no payload)
  ←  h   the schema, as source
```

A database carries the schema it was created against, so a client's built-in copy is its own
opinion. `h` is the answer, and it is what lets a client describe the right predicates, compile
a query before sending it, and show a plan. **Virtual predicates are included**, because the
question is what may be asked rather than what the database holds.

## A query stream

```text
  →  Q   "{f = F, l = L} where src.Ref {…}"
  ←  T   {f: src.File, l: int}
  ←  D   one row
  ←  D   one row
  …
  ←  C   complete
```

Four query kinds rather than one kind with flags, and the reason is the same each time: `Q`'s
payload is the query text and nothing else, so a leading flag byte would be a silent change of
meaning for every client already sending UTF-8. A client that has never heard of profiling
neither sends `P` nor receives a `p`, and the protocol version does not move.

| Kind | Adds | Answers with |
|---|---|---|
| `Q` | — | `T`, `D`*, `C` |
| `P` | Report what it examined | …plus `p`, once, just before `C` |
| `G` | Stop after N rows | …plus `r`, **only** if the result was cut short and there is more |
| `N` | Count instead of encoding | `n` — no `T`, no `D` |

`G` is what makes paging **stateless**. Without it a result lives in the server's session, keyed
by stream id, and a caller has to hold the connection to see page two — which a web tier cannot
do and cannot work around, because "everything after key K" is not expressible in the language.
A page that reached the end sends **no** token, which is how a caller knows it has seen
everything without asking again to be told nothing.

`N` is the cheapest kind to justify: the plan is the same and the executor is the same, and what
differs is the accumulator — the driver is a fold, so counting is a fold that keeps a number.
It is **not** aggregation in the language.

### Rows

Rows go out **256 at a time**, off the executor's suspend, and the next chunk resumes from the
bytes-only cursor. So a result of any size never buffers in the server and never monopolises the
socket — and between chunks is exactly where a cancel gets its chance to land.

### Cancellation

```text
  →  X   (on the stream it cancels)
```

**In band, on the stream it cancels** — not a connection teardown and not a side channel. That
is the whole reason frames carry a stream id: cancelling a long query has to be possible without
disturbing the other streams sharing the socket, and a second connection could not do it because
the first one's state is not there.

### Errors

```text
  ←  E   { code, message }
```

A stream's failure leaves the connection usable. The code exists so a client can branch without
parsing English; the message exists because a person reads it.

| Code | Means |
|---|---|
| 1 `Protocol` | Malformed or out-of-sequence frames |
| 2 `UnknownDatabase` | No such database under this root |
| 3 `SchemaMismatch` | The claim disagrees with what this database holds |
| 4 `ModeRefused` | A write session against a Complete database |
| 5 `BadFacts` | A block that does not validate against the embedded schema |
| 6 `Conflict` | Same key, different value |
| 7 `BadQuery` | The query did not compile |
| 8 `Internal` | Look at the server's logs |
| 9 `InUse` | Something else holds this database — the one code worth **retrying** |
| 10 `Refused` | A well-formed request the server will not carry out — the answer is in the message |

## Fetching what an id names

```text
  →  F   a batch of ids, batched per predicate
  ←  f   one key each, in the order asked
```

The read-path twin of a nested reference on the way in. Stored, a reference is a `FactId`, so a
row carries a number — and sigla cannot ask what it names, because a query names a fact by its
key. Five properties, each deliberate:

- **The key, not the value side.** A reference names an *identity*, and the identity is the key.
  It is also exactly the logical form the content hash is computed over and a producer nests, so
  one shape covers writing a fact, hashing it and expanding it.
- **Positional against the request**, so the reply does not send the question back with the
  answer. It carries a count, so the one fault positional pairing is exposed to is caught rather
  than mis-paired. Batch size is capped at 4096 in the decoder, so it is a protocol rule rather
  than a handler's caution.
- **Absence, not a failed stream**, for an id naming no fact: the server cannot tell an id
  lifted out of a row from one somebody typed, and the client can. A bad *predicate* id is a
  refusal, because that is a disagreement about a schema both ends hold.
- **Two kinds of absence**, because they mean opposite things: *missing* for a predicate that
  stores its facts, where a dangling id is impossible and corruption is the only explanation;
  and *unstored* for a predicate that is **answered rather than stored**, whose rows are a view
  materialised per query and may simply have moved on. Collapsing them would mean either crying
  corruption at an ordinary `db rm` or staying quiet about a damaged store.
- **Read-only is enough.** It reads facts, which every session may do. One point read each, on
  the blocking pool. It reads the store as it is *now* rather than under the query's snapshot,
  and nothing follows from that: a fact is immutable and an id is never reused, so an id that
  was in a row names the same fact under every later view.

## The write stream

```text
  →  W   open a write stream
  ←  G   ready
  →  d   one block
  →  d   one block
  →  c   done
  ←  C   facts written, facts deduped, the id range per predicate
```

Per block, on that stream's own task and concurrently with every other stream's:

1. **decode** the block (CRC first),
2. **validate** against the embedded schema,
3. **intern** nested references bottom-up,
4. **storage-encode**, and
5. **write both column families atomically**.

The exclusion is per **key**, inside the store, so blocks from different streams proceed
together. What a block holds at the database level is only the shared half of the seal barrier:
writers hold it shared, `finish` takes it exclusive.

**Failure is per stream, not per fact.** A rejected block fails the write stream; the connection
and its other streams survive. A conflict names the offending key.

:::note A fact that interned cleanly and then conflicted stays written
Its target was legitimately named and legitimately defined, facts are immutable, and interning
is idempotent — so retrying the whole message after fixing the conflict dedups against it. What
a transaction would prevent here is a wasted row, not a wrong answer.
:::

### Blocks

The same bytes on the wire and on disk — a `COPY_DATA` payload **is** a block, and that is a
test rather than an intention.

```text
  [sync: FF × 10][magic "FJBK"][name_len u32][count u32][length u32][crc32 u32][name][payload]
   └──────────────────── 30 bytes of framing ────────────────────┘
```

- **The predicate is named, not numbered.** A number would make a fact file meaningful only
  against the database whose numbering wrote it, and make every client keep a table of ids in
  step with a server's. A name costs about six bytes *once per block*.
- **The name sits after the fixed-width fields**, so a splitter still reaches `length` at a fixed
  offset.
- **Header fields are fixed-width and little-endian**, where the payload is varints: a splitter
  must read `length` before it can trust anything else, and nothing here is ordered — the
  big-endian storage codec is answering a different requirement.
- **The CRC covers the header's own fields as well as the payload**, so a corrupted `length` is
  caught rather than used to skip to the wrong place.
- Caps: 16.7 M facts, 64 MiB payload, 64 KiB name.

### Sync markers cannot occur in a payload

Ten `0xFF` bytes are unreachable **by the encoding**, not by luck:

- a string is length-prefixed UTF-8, and UTF-8 never uses `0xF8`–`0xFF` at all;
- a varint's continuation bytes are `0x80`–`0xFF` but its final byte is below `0x80`, so a run
  ends where the varint does — and the longest possible is nine bytes;
- runs cannot join across values, for the same reason;
- the header cannot contribute one either: `count` and `length` are capped to keep a zero top
  byte, so only the checksum is free to be all-ones, and four is not ten.

So a marker appears exactly once per block, at its start, and a scan of a well-formed file finds
boundaries and nothing else. Validation (magic, then CRC) stays load-bearing for the fault it is
actually for: a torn write, a flipped bit, a file cut mid-block.

That is what makes **one file splittable**: seek anywhere, scan to the next sync, hand blocks to
workers. No reliance on per-predicate contiguity in the input, and no requirement that the
producer chose the chunking.

## The value encoding

Both directions use the transport codec, and it differs from the storage codec in every choice:

| | Storage | Transport |
|---|---|---|
| int | Marker carrying the width, big-endian minimal magnitude, negatives ones'-complemented | LEB128 varint over zigzag |
| string | Marker, escaped contents, terminator | Varint length, then the bytes |
| record | Marker, fields, terminator | The fields, concatenated. Nothing else |
| reference | Marker plus fixed 8 bytes | A varint union branch: an id, **or the target fact** |
| names, types, arities | — | **Not sent at all** |

The last row is the design. Both peers have the schema — the handshake compares fingerprints
before data flows, and a database's schema is frozen at create — so names, order, arity and type
are things the reader already has. That is **Avro's** model, and Avro states the consequence
plainly: binary Avro carries no type information or field names, and a record is just the
concatenation of its fields' encodings.

What is *not* borrowed is as deliberate:

- **Protobuf and Thrift** spend one to two bytes per field per message on a tag, and what it buys
  is a reader skipping fields it does not know — schema evolution between peers that never
  agreed. These peers agreed, by fingerprint, before the first byte.
- **Cap'n Proto** spends wire size on fixed-width fields to buy O(1) access with no parse. Every
  inbound fact is parsed regardless — to intern its references and re-encode it as a storage
  tuple — so there is no parse to avoid and the size is worse.

Two properties are load-bearing rather than incidental:

- **Minimal varints are enforced**, so one value has exactly one encoding. A block carries a
  CRC32 and the same encoding is used on the wire and in a file, so "the same facts" has to mean
  "the same bytes" for a checksum to be worth computing.
- **A reference is type-checked against the predicate it names**, both directions, free: a
  `Fact(p)` field can only hold a reference to `p`, and a fact id carries its predicate in its
  top bits. That catches the one corruption a bare id is prone to — an id from the right database
  and the wrong tree — which no length check or checksum would.

## What a client sends: the whole fact

```text
    src.Decl {
      module = src.Module {                    ← a whole fact, not an id
        file = src.File "store/keys.py",       ← nested again
        name = "keys"
      },
      name = "key_of", line = 12
    }
```

A reference field holds the **target fact written inline** — key and value both, to any depth —
or a `FactId` for a producer that already holds one. Ingest interns each nested fact and
substitutes the id.

**The producer keeps no book.** That is the whole reason for the shape: an indexer walking a
syntax tree knows the file when it reaches the declaration, and every id-based alternative asks
it to carry a map from every entity to an assigned identity plus an emission order that respects
one. The cost of the trade, stated: a repeated target is sent repeatedly. Block-local
back-references would compact it and are deliberately not in P0.

The **only** asymmetry between the two directions is this one: a row on its way out was read from
storage, so its references are already ids.

## Control frames

`create`, `finish` and `remove` are frames on an **ordinary stream** rather than on stream 0 —
which keeps a `create` (tens of keyspaces, tens of milliseconds each) off the reader loop, and
gives lifecycle requests the same per-stream error handling everything else has.

`list` and `describe` are deliberately **not** control frames: they read sidecars and never open
the storage engine, so they already work while a server holds every database under the root. The
remote form of `list` is a query over the virtual predicate `fjord.db.List`.

## Server obligations

- **A stream is a task.** The reader loop reads a frame, routes it, and goes back to reading.
- **One fair writer** takes a frame from each stream's queue in turn. Fairness is structural
  rather than a scheduling hope: a single shared output channel is unfair in exactly the way that
  matters, since a million-row query fills it and a second stream's four-frame answer waits
  behind all of them.
- **A chunk boundary is a real resume**, and the snapshot is released at each one.
- **Backpressure** is bounded per-stream queues plus per-connection backpressure. Per-stream
  flow-control windows are deferred.

## What is built, and what is not

| | |
|---|---|
| **Built** | The frame layer; the handshake including the schema check and the mode refusal; write streams; query streams in all four kinds; `H`/`h`; `F`/`f`; in-band per-stream cancellation; a reader task per connection and one fair writer over bounded queues; control frames for create/finish/remove; stream-level failure that leaves the connection usable |
| **Deferred** | Per-stream flow-control windows |
| **Opt-in** | TCP — default-closed, and `--listen-tcp` is the only way to open one |
| **Not built** | Ingesting a fact **file** (the format is defined and the block encoding is shared; the splitter and the pipeline are not wired to a command) |

A second implementation exists and is part of the test surface: `clients/dotnet` speaks this
protocol in C#, shares no constants with the server, and its encoder is held byte-for-byte
against the Rust one over a checked-in golden. See [Clients & the viewer](clients.html).
