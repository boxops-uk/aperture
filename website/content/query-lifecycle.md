---
title: A query, step by step
description: From the text on the client to the rows coming back — the compile, the frames, the plan, the nested loop, the chunk boundary and the resume token, in order.
---

This page follows one query all the way through. Nothing here is a simplification of a
layer described elsewhere; where a subject has more depth, there is a link.

The query:

```sigla
{f = F, l = L} where src.Ref {to = src.Decl {name = "encode_str"}, file = F, at = {line = L}}
```

## The whole journey

```text
  ┌─ client ──────────────────────────────────────────────────────────────┐
  │                                                                        │
  │  1  you type it                                                        │
  │  2  the client holds the served schema (frame H → h)                   │
  │  3  the shell compiles it locally  ── a mistake stops here             │
  │  4  QUERY frame on a new stream:  [Q][stream][len][the text]           │
  └──────────────────────────────┬─────────────────────────────────────────┘
                                 │  one socket, many streams
  ┌─ server ────────────────────▼─────────────────────────────────────────┐
  │  5  the reader task routes the frame to that stream's own task         │
  │  6  hop off the reactor onto the blocking pool                         │
  │  7  compile: lex → parse → typecheck → flatten → reorder → Plan        │
  │  8  ROW_DESCRIPTION frame (T) — the head's shape, once                 │
  │  9  open a snapshot; enumerate 256 rows                                │
  │ 10  encode each row with the transport codec → DATA_ROW frames (D)     │
  │ 11  suspend to a bytes-only Cursor; drop the snapshot                  │
  │     … 9–11 again per chunk, while the previous chunk is being written   │
  │ 12  PROFILE (p) if asked, then COMPLETE (C) with counts                │
  └──────────────────────────────┬─────────────────────────────────────────┘
                                 │
  ┌─ client ────────────────────▼─────────────────────────────────────────┐
  │ 13  decode each row against the descriptor                             │
  │ 14  expand references, if asked — a FETCH round trip per level         │
  │ 15  render: table, json, jsonl, raw, or a count                        │
  └────────────────────────────────────────────────────────────────────────┘
```

## 1–2. The client knows what it may ask

A client connects with a **startup frame** naming the database, a session mode, and the
predicates it claims — each with its own fingerprint. The server checks the claim by subset
containment: a producer that writes six of a database's twenty-seven predicates connects
without restating the other twenty-one.

Then it can ask a question that has nothing to do with data:

```text
  →  H   "what can I ask you?"        (no payload)
  ←  h   the schema this session is served with, as source
```

That matters because a database carries **its own** schema, frozen at create. A client's
built-in idea of the schema is its own opinion; `h` is the truth. Everything a client can do
locally follows from it: describe the right predicates, compile before sending, show a plan.

## 3. The client compiles it

The **shell** compiles every query locally before sending it, so a mistake is the compiler's
own diagnostic — code, caret, colour — with no round trip:

```text
error[reject/unknown-predicate]: `src.Nope` is not a predicate in this schema
  ┌─ <input>:1:9
  │
1 │ X where src.Nope X
  │         ^^^^^^^^^^
```

`fjord query` works the other way round: it sends the text, and *if* the server refuses
with a bad-query code, it fetches the schema and recompiles locally purely to render the
caret. Same diagnostic, one extra round trip only on the failure path.

:::note Two compilers, one authority
The rule where they could disagree: **the server decides what runs.** A client-side compile
is there to answer fast and to show a plan — never to be the thing whose verdict counts.
:::

## 4. One frame, on a new stream

```text
  ┌────────┬────────────┬────────────┬───────────────────────────┐
  │ kind:1 │ stream:4   │ length:4   │ payload                    │
  │  'Q'   │  7         │  92        │ "{f = F, l = L} where …"   │
  └────────┴────────────┴────────────┴───────────────────────────┘
       9-byte header, then `length` bytes
```

The `stream` field is why this is not PostgreSQL's protocol: there, every message belongs to
the one conversation the connection *is*, so a long query blocks a short one behind it. Here
a query is a stream and a write is a stream, several run at once on one connection, and every
frame says which one it belongs to.

There are four query kinds rather than one query kind with flags — `Q` plain, `P` profiled,
`G` paged, `N` counted — because `Q`'s payload is the query text and nothing else. A leading
flag byte would have been a silent change of meaning for every client already sending UTF-8,
and a client that has never heard of profiling neither sends `P` nor receives a `p`.

## 5–6. The server routes it, then gets off the reactor

The reader loop reads a frame, hands it to that stream's own task, and goes straight back to
reading. One **fair writer** task drains each stream's output queue in turn — round-robin,
not a shared channel, because a million-row result would fill a shared channel and a second
stream's four-frame answer would wait behind all of it.

Compilation and execution are **blocking** work: they read an LSM tree. They run on a
blocking pool rather than on the async reactor, so a scan cannot stall the event loop that is
serving every other connection.

## 7. Compilation

`lex → parse → typecheck → flatten → reorder → Plan`.

| Phase | Does |
|---|---|
| lex / parse | Produce a lossless, untyped, grammar-shaped tree with spans and text |
| lower | Turn it into a typed, `NodeId`-indexed tree the later phases run on |
| typecheck | Annotate, don't mutate: types go in side tables indexed by `NodeId` |
| flatten | Collect statements; fold constants; collect constraints and denials; check range restriction; **hoist nested generators**; then decide sargeability per key field |
| reorder | Choose the loop order — the greedy *runnable frontier* |

The tree the first two phases build is lossless and grammar-shaped — one node per rule, one
leaf per token, spans throughout. That is what lets an editor light up the source from a
node, and what lets a recovered parse point at the byte that went wrong:

:::demo parse
N where F = code.File "src/lib.rs"; code.Decl {file = F, name = N, line = _}
:::

Two things are worth pulling out.

**`reorder` is load-bearing for acceptance, not just speed.** It works over a graph of
*variables*, not edges between statements — because which statement captures a shared
variable depends on the order chosen, so edges would forbid correct orders. A query that
reads a variable the next statement binds has a perfectly good plan, and this is what stops
it being refused. Greedy is complete here because the constraint is monotone: `bound` only
grows.

**Constants and constraints are collected before an order is chosen.** `X = 42` is
substituted at every use and takes no register and no step. `X = "a"..` is applied by
whichever level captures `X`, so writing it first or last is the same plan.

The output is the contract between the two halves of the system:

```rust
struct Plan {
    nvars: usize,       // the size of the register file
    body: Box<[Step]>,  // ordered: [0] outermost … [n-1] innermost
    head: Project,      // how to build each output row
}
```

For our query:

```plan
  r0 <- src.Decl scan
       where name == "encode_str"
  r1 <- src.Ref seek[to = r0#, file = _, at = _]
  head {f = r1.file, l = r1.at.line}
```

Read it as two nested loops. The outer one scans `src.Decl` and filters on `name` — because
`src.Decl`'s key is `{module, name, line}` and the leading field is open, so the name cannot
narrow the scan. The inner one **seeks**: `src.Ref`'s key leads with `to`, so the
declaration's fact id is spliced into the seek key and only its references are read.

The same compilation, over the demo schema, with the plan the engine actually emits — its
levels, its registers, and the access each step uses:

:::demo plan
N where code.Decl {file = F, name = N, line = _}; F = code.File P; P = "src/u"..
:::

## 8. The row descriptor, once

```text
  ←  T   {f: src.File, l: int}
```

A query's row shape comes from its **head**, not from a predicate, so unlike a fact it cannot
be read off the schema alone. It is sent once per stream, and every row after it is
positional against it. That is the same bargain the value encoding strikes everywhere here:
both peers know the shape, so the bytes carry no names.

## 9. The nested loop

The executor is a pull-based machine, and its driver is one loop over a `depth` cursor:

```text
loop:
  if depth == body.len():          # past the innermost step — a full row is bound
      hand the row to the consumer
      on Continue: depth -= 1      # backtrack, look for the next row
      on Suspend:  depth -= 1; return Suspended{cursor}

  else match body[depth]:
      Level: open the next source if needed; pull the next matching row
             matched   → bind into registers; depth += 1
             drained   → next source, or back up a level
      Derive: compute the value once, descending; report exhausted, ascending
      Test:   probe each source for one row; pass (depth += 1) or drop the row
```

Three properties of that loop are invariants rather than implementation details:

- **A register holds the whole row, and fields decode lazily.** Binding a variable is a
  refcount bump on a byte buffer, not a decode ([I5](invariants.html#i5)). Field boundaries
  are cached in an inline array that never spills to the heap
  ([I9](invariants.html#i9)).
- **Values never enter the scan loop.** The hot loop touches only the index map; a value is a
  point read, and only when a projection asks ([I6](invariants.html#i6)).
- **It is a state machine, not recursion.** What a recursive implementation would keep on the
  call stack is an explicit frame stack here — which is the only reason the next step is
  possible ([I7](invariants.html#i7)).

Full detail: [Executor & resume](executor.html).

The driver, one transition at a time — the registers as the machine fills them, the rows as
they are yielded, and the rows a residual read and dropped, which are invisible in the answer
and are most of what a query costs:

:::demo run
N where code.Decl {file = _, name = N, line = L}; L > 15
:::

## 10. Encoding a row

A row goes out through **exactly the codec a fact's key comes in through** — no fourth
encoder appears on the way out:

```text
   Ty (the head's inferred type) → Desc (names resolved, sent once)
        → PredicateTy → WireValue → bytes
```

The wire format carries no field names, no arities and no types, because both peers have the
schema and the descriptor. That is Avro's model rather than protobuf's: a tag per field buys
schema evolution between peers that never agreed, and these peers agreed by fingerprint
before the first byte.

```text
  ←  D   one row
  ←  D   one row
  …
```

## 11. The chunk boundary is a real resume

Rows go out **256 at a time**. At each boundary the executor suspends, which does three
things at once:

1. The frame stack is serialised into a `Cursor` — **one detached row per open level, and
   nothing else**. No iterator, no closure, no plan.
2. The **snapshot is dropped** ([I8](invariants.html#i8)). An LSM iterator pins one, and a
   paused query that pins a snapshot is a paused query holding storage open.
3. A cancel gets its chance to land. `--limit`, `:cancel` and Ctrl-C are all in-band cancels
   on that stream, and they take effect between chunks.

The next chunk resumes from those bytes and reproduces the run exactly
([I4](invariants.html#i4)) — the same rows in the same order as if nothing had interrupted
it. The token carries a layout version and a **plan fingerprint**, checked before any entry
is read, because entries are paired with levels by order: without them, two same-shaped plans
over overlapping predicates would accept each other's cursors and answer short, silently.

This is also what makes **paging stateless**. A `G` (paged) query returns rows plus an `r`
(resume) frame, and page two is a fresh request carrying that token. Nothing lives in the
server's session between pages, so a web tier can page without holding a connection —
and "everything after key K" is not expressible in the language, so there is no
offset-shaped workaround.

## 12. The end of the stream

```text
  ←  p   the profile, if the query was a `P`      (once, just before the end)
  ←  C   complete, with counts
```

A profile arrives once, just before the result ends, because the tally is not final until the
last chunk has run. A `--limit` that cancels early therefore reports **none** rather than
reporting a different query's numbers.

```text
STEP      EXAMINED
src.Decl  1000      full scan
src.Ref   1
1001 examined, 1 produced
```

It is per **step of the plan's body** rather than per predicate, which is what the machine
counts — and what gives a fetch, a disjunction and a negation each a line of their own. Read
it against the plan: the plan is the *intent* (which field narrowed, which only filtered) and
this is the *outcome*.

## 13–15. The client renders

Rendering is **always** client-side. The wire carries the binary format and the server never
produces JSON — so `--format` is a flag on the command rather than a field in a request.

| Format | Streams? | Shape |
|---|---|---|
| `jsonl` | yes | One JSON value per line — what a paged result has to be, since a page is not a document |
| `json` | yes | One document, written incrementally |
| `raw` | yes | Tab-separated fields |
| `table` | **no** | Aligned columns — buffers, because column widths are not known until the last row |
| `count` | n/a | The number only, from a `N` query: the server counts instead of encoding |

### Expanding a reference

A row carries a reference as a `FactId`, because that is what a reference is once stored:

```json
{"to": "#4:1", "file": "#9:2", "at": {"line": 2, "col": 4, "length": 12}}
```

sigla cannot ask what `#4:1` names — a query names a fact by its key, never by its number. So
the question goes on the **protocol**:

```text
  →  F   a batch of ids, per predicate
  ←  f   one key each, positionally, or an absence
```

and the client walks it: breadth-first, one round trip per level of depth, one point read per
distinct id, cached across pages because a page of references into one file names that file
forty times.

```json
{"to": {"module": {"file": "Boxops.Fjord.Client/Crc32.cs", "name": "Boxops.Fjord.Client"}, "name": "Crc32", "line": 7}, "file": "Boxops.Fjord.Client/Blocks.cs", "at": {"line": 98, "col": 24, "length": 5}}
```

Four things about that answer are deliberate:

- **It is the key, not the value side**, because a reference names an *identity* and the
  identity is the key. It is also exactly the shape a producer nests on the way in and the
  shape the content hash is computed over — so writing a fact, hashing it and expanding it
  are one shape rather than three.
- **The recursion, the depth bound and the cache are the client's**, because how deep to
  expand is a display decision. The server does one point read per id and nothing else.
- **It composes with everything**, because it is a question asked after the row arrived
  rather than a flag on the query. A fifth query kind would have needed one per combination of
  paging, profiling and counting.
- **Expansion never costs a row.** If the server refuses a fetch, the rows still print with
  ids in them and one line says which predicate could not be expanded. Losing a page of good
  rows to a *display* feature would be the wrong failure.

## The whole thing, timed

From the S4 rung of the measurement ladder, on the reference box: compilation is 4–14 µs and
linear in the query's size; the executor's own floor is ~330 ns per row and flat in database
size; encoding a row costs about 1.5× the executor's work, and the wire above it another
3.6×. Which is why `--count` exists: it runs the same plan and the same executor and throws
away the part that costs.

See [Performance](performance.html) for the method and the numbers; `bench/FINDINGS.md` in
the repository is the register of what was measured, at what size, and what acting on it
would cost.

## Where a write goes instead

The same connection, a different stream kind:

```text
  →  W   open a write stream
  ←  G   ready for blocks
  →  d   one block: [sync][magic][name][count][length][crc][facts]
  →  d   …
  →  c   done
  ←  C   complete: facts written, facts deduped, id range per predicate
```

Per block: decode → validate against the embedded schema → **intern nested references
bottom-up** → storage-encode → write both column families atomically. Blocks from different
streams are processed concurrently, because the exclusion is per **key**, not per database.
See [Wire protocol](wire-protocol.html#the-write-stream) and
[Storage model](storage.html#interning-a-nested-fact).
