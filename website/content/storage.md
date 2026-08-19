---
title: Storage model
description: Two column families, one keyspace per predicate, an order-preserving codec, snowflake fact ids, the atomic two-map write, and how a nested reference becomes an id.
---

Fjord stores every fact twice, in two sorted key–value maps, and the split is the reason
queries are fast at what they are fast at.

## Two column families

```text
  keys      predicate_id ++ encoded_key  →  fact_id
  entities  fact_id                      →  encoded_key + value
```

| | `keys` | `entities` |
|---|---|---|
| Answers | *Which facts match?* | *What is this fact?* |
| Read by | The scan hot loop, exclusively | A point lookup — projection, a fetch through a reference |
| Access | Prefix and range scans over sorted bytes | One key, one row |
| Contains | No values, ever | The key again, plus the value side |

A query over a predicate is a **prefix scan** of `keys`, because the predicate id is the
prefix of every one of its keys. Narrowing on leading key fields extends that prefix. This
only works because the encoding is order-preserving.

### Why two, and not one

Because a value must never enter the scan loop ([I6](invariants.html#i6)). A single map
holding key *and* value would put value bytes in the way of every scan: more bytes read per
row, worse cache behaviour, and a hot loop whose cost depends on data a query did not ask
for. Splitting them makes "seek and filter on keys" and "read a value" two different
operations with two different costs — and makes the second one visible in a profile.

The price is that a fact is written in two places, which is why the write is atomic
([I12](invariants.html#i12)).

## One keyspace per predicate — for both maps

Each predicate gets its own **pair of physical trees**, `keys.<id>` and `entities.<id>`, and
the predicate id is still the key prefix in `keys`. Three consequences, all load-bearing:

- **Physical isolation.** Facts of different predicates cannot affect each other's storage,
  which is what makes ingest parallel across predicates. The storage engine's bulk path
  requires strictly ascending keys; a single shared tree would force one globally ordered
  serial sink.
- **Prefix-disjointness aligns with isolation.** "Predicate id is the key prefix" and "one
  tree per predicate" say the same thing at two levels, so a deriver reading predicate A and
  writing predicate B is structurally read/write-disjoint.
- **A predicate can be dropped wholesale in O(1)** by deleting its two trees — which is what
  re-deriving a derived predicate needs. In a shared tree that would mean range tombstones in
  a store whose premise is that nothing is ever deleted.

Splitting `entities` is possible **only** because a fact id carries its predicate: `point()`
is handed a bare id and no predicate, so an untagged id would turn identity lookup into a
search across every tree.

:::note What it costs
Creating a keyspace costs about **30 ms** (directory create plus fsyncs), so `create`
materialises every predicate's trees up front from the schema rather than lazily — a
twenty-seven-predicate schema is a measurable but one-off cost at create, not a surprise on
first write. The split also means memtable budgets are per tree rather than global, which is
an obligation the design records rather than a free win.
:::

## The tuple codec

The storage codec has three properties, and each is an invariant.

### 1. Order-preserving ([I1](invariants.html#i1))

```text
memcmp(encode(a), encode(b)) == compare(a, b)
```

That is what buys a **range** scan (not a filter) and rows in semantic order with no sort. It
is why `"ann" < "anna"` falls out of the bytes, and why a negative integer sorts below a
positive one.

### 2. Self-delimiting ([I2](invariants.html#i2))

The **marker byte** alone says how to advance past a value. `skip` needs no schema: a scan
can walk to the *n*th field of a key it has never seen the type of. Record nesting is
bounded, so malformed bytes are an error rather than a stack overflow.

### 3. Frozen on disk ([I3](invariants.html#i3))

Marker values and their relative order are semantic — a marker is the most significant part
of a value's sort key. Once data exists they cannot change. New types go into a **reserved
band** in the correct skip family, chosen so the type sorts where it should. A golden-bytes
test pins every marker so a renumber breaks loudly.

### The marker table

| Byte(s) | Marker | Meaning | Skip family |
|---|---|---|---|
| `0x00` | `MARK_NULL` / `MARK_TERM` | null · record terminator | (see below) |
| `0x01–0x20` | *reserved* | future types sorting below `String` | |
| `0x21` | `MARK_STRING` | UTF-8 string | terminator walk |
| `0x22` | `MARK_RECORD` | record start, paired with `MARK_TERM` | nested |
| `0x23–0x3F` | *reserved* | | |
| `0x40–0x47` | `MARK_INT_NEG` | negative integers, width 8 … 1 | width in marker |
| `0x48` | `MARK_INT_ZERO` | zero | fixed, 0 bytes |
| `0x49–0x50` | `MARK_INT_POS` | positive integers, width 1 … 8 | width in marker |
| `0x51` | `MARK_FACT_REF` | a reference to a fact | fixed, 8 bytes |
| `0x52–0xFE` | *reserved* | | |
| `0xFF` | `MARK_ESCAPE` | escapes a null element | — |

Reading the integer band: zero is the centre at `0x48`; positives climb as width grows, so
larger positives sort higher; negatives fall as width grows, so more-negative sorts lower.
The type ordering `null < string < record < integers < fact-ref` falls out of the table.

A fact reference has its **own** fixed-width marker rather than sharing the integer encoding,
so a value's bytes are self-describing without the schema and the `Int`/`Fact` distinction is
enforced at the byte level.

:::warn Every value marker stays below `MARK_ESCAPE`
That is why the reserved band stops at `0xFE`. A new type numbered `0xFF` would silently
invert string ordering across a record boundary — so the ceiling is part of what I3 freezes,
not a spare byte.
:::

## Fact ids are snowflakes ([I11](invariants.html#i11))

```text
   63          40 39                                    0
  ┌──────────────┬───────────────────────────────────────┐
  │ predicate id │ sequence within that predicate        │
  │   (24 bits)  │              (40 bits)                │
  └──────────────┴───────────────────────────────────────┘
    ≤ 16.7 M predicates      ≤ 1.1 T facts per predicate
```

The split is byte-aligned on purpose: the tag is the top three bytes of the big-endian
encoding, so routing a lookup to a predicate's tree is a slice rather than arithmetic.

Three things follow, and they are the reason for the design:

- **`entities` can be split per predicate** while `point()` stays one lookup from a bare id.
- **There is no global allocator.** Each predicate counts its own facts, so two workers on
  different predicates share no counter and write disjoint ascending ranges.
- **Uniqueness across predicates is structural**, not enforced: the tag partitions the space,
  so two predicates cannot collide however their sequences are allocated.

Sequences are per predicate and 1-based, which is what makes `#4:1` readable as "predicate 4,
first fact". Ids are **never reused within a database**, and they are not cross-database
identity: two databases built from the same inputs agree on *content*, not on numbering.

Ids are also claimed **durably ahead of use**, in chunks, because handing one out before its
fact is committed would let a crash reissue it to a different fact — turning a surviving
reference into one that resolves to the wrong target, silently.

## Interning a nested fact

A producer does not send ids. It sends **the target fact**, written inline where the
reference belongs, to any depth:

```text
    src.Decl {
      module = src.Module {                    ← a whole fact, not an id
        file = src.File "store/keys.py",       ← nested again
        name = "keys"
      },
      name = "key_of", line = 12
    }
```

**Interning is resolve-or-create, bottom-up.** At each nested fact: encode its key with the
storage codec, look that key up in the target predicate's `keys` tree, take the id if it is
there, allocate and write if it is not — then substitute the id into the parent and carry on.

The walk is bottom-up because it has to be: a parent's key has no bytes until its children
have ids. Three properties make it safe, and none of them is a new rule:

- **It terminates, and it is well founded.** A nested fact is a finite tree, and a reference
  *in a key* cannot be part of a cycle — the target must be fully identified before the
  referring key has any bytes at all. (Cycles are reachable only through *values*, where a
  reference is not implemented.)
- **"Already there" is dedup**, silently: a target nested under a thousand parents resolves to
  one row.
- **"There with a different value" is a conflict**, rejected deterministically — never
  last-writer-wins and never first-writer-wins, because either is order-dependent and
  reproducibility forbids it.

A consequence worth knowing before you hit it: **a single message can contradict itself.** A
nested fact both names and *defines* its target, so one message naming a target twice with two
different value sides is a producer disagreeing with itself, and is refused as an ordinary
conflict. Both orders reject, so the answer does not depend on the order the walk took.

This is why a producer needs no book: an indexer walking a syntax tree knows the file when it
reaches the declaration, and every id-based alternative would ask it to carry a map from every
entity to an assigned identity, plus an emission order that respects one.

## The atomic two-map write ([I12](invariants.html#i12))

`keys` and `entities` are written in a **single write batch**. A fact is never half-present:
never a key without its entity, never an entity without its key. The two failure directions
fail differently, which is why the guard checks both:

- a `keys` row with no `entities` row makes a value projection return nothing, surfacing as a
  dangling-id error;
- an `entities` row with no `keys` row is **invisible to every query** — silent, and
  undetectable without checking both directions.

### The other half: one key names exactly one fact

Atomicity is the easy half, and a batch delivers it whoever writes. The half a batch is
innocent of is **write-once**: writing the same key twice overwrites the `keys` row and
strands the first fact's entity — exactly the invisible orphan above.

That is the whole of what a parallel writer conflicts with. Two workers interning the same
nested target both find the key absent, both allocate, both write; one `keys` row survives and
the other entity is stranded.

**The mechanism is per-key exclusion**, striped by `hash(predicate_id ++ key_fields)` across
64 stripes, each holding its own slice of the "what does this key already name" cache behind
the same mutex — because the two want exactly the same critical section: *what this key
already names*, and *the right to decide it names nothing yet*.

It needs no lock-ordering discipline, and the reason is the bottom-up rule above: a worker's
critical sections run strictly leaf-then-parent and are never nested — it holds a child's
stripe, releases it, then takes the parent's. The property that makes interning total is the
property that makes it parallelisable.

:::warn Per-key exclusion is the weakest thing that works
A lock-free compare-and-swap on the `keys` tree would **not** be enough, and neither would
anything that lets a resolve proceed without excluding the writer of the same key. A write
batch is atomic on recovery but **not isolated** from a concurrent reader: a reader can see the
`keys` insert before the `entities` insert of the same batch. With one writer that question
never arose. So the stripe is held across the read *and* the commit.
:::

The guard is `store::concurrent_interning_of_one_key_creates_one_fact` — N threads racing on
one key, one id handed to all of them, the bijection intact. Single-threaded, the striping
costs one hash of the key: measured at 1–4%.

## The order a scan is promised in

A scan yields rows in **lexicographic key order**, and that is a commitment rather than an
implementation detail: resume re-seeks against it. It is also what makes rows come back in a
useful order for free — a file's references arrive in the order a renderer wants to splice
them, because position leads the rest of that predicate's key.

## Snapshots

A query reads a **snapshot**. For an immutable database that is nearly free, but a storage
iterator pins one, and a paused query holding a pinned snapshot is a paused query holding
storage open. So the snapshot is **released at every suspend** ([I8](invariants.html#i8)) and
re-taken on resume — which is sound precisely because the data cannot have changed.

The guard for that one cross-checks a drop probe against the storage engine's own count of
open snapshots, because "we dropped our handle" and "the engine considers it closed" are two
different claims.

## The format stamp ([I15](invariants.html#i15))

Every database says which format wrote it: a twelve-byte stamp in a metadata keyspace, with
`codec` and `storage` versioned **separately**, checked at open. An unreadable one is refused
rather than adopted, and a database holding facts with no stamp at all is refused rather than
assumed to be version 1.

Read the gain narrowly. It makes nothing migratable — I3 still binds every database stamped
`codec 1` exactly as before. What it buys is that a *future* codec is a different number
rather than an impossibility.

## On disk

```text
<data_dir>/
├── fjord.sock                     # the server's socket; its presence is server detection
└── <name>/<instance>/             # instance: a ULID
    ├── FJORD_META                 # the sidecar, written temp + fsync + rename
    │     name, instance, status (Writable|Complete|Broken), format version,
    │     schema fingerprint, content fingerprint (at finish), counts, size, created_at
    ├── schema/                    # the embedded canonical schema — the durable copy
    └── <storage files>            # keys.<id> / entities.<id> per predicate
```

The sidecar is the fast enumeration path — `list` and `describe` read it and **never open the
storage engine**, which is what lets them work while a server holds every database under the
root. The embedded schema copy inside the database is the durable fallback, and since the
schema DSL landed it is also the *source* a server reads a database's schema back from.

## Writing a fact by hand

Inside the process there is a second way in, used by the tests and the benchmarks:
`FjallDb::put(&schema, &fact)` takes a **well-typed value** whose key fields are named and
resolved against the schema.

It exists because the low-level `put_fact` takes bytes, and three of its preconditions fail
**silently**: a stored key is flat, field order is the schema's declaration order, and only the
schema says whether a predicate has a value side at all. Each mistake writes a fact that is
simply never found. `put` returns the id, so the fact you write next names this one by a value
you already hold — referential integrity as a consequence of write order rather than a check.

This is deliberately *not* bulk ingestion and deliberately not a `serde` derive. The wire
path is built **beside** it, not on top of it.

## Storage codec versus transport codec

They are siblings. Neither is a layer on the other, and they share no bytes.

| | Storage | Transport |
|---|---|---|
| int | Marker carrying the width, then big-endian minimal magnitude; negatives ones'-complemented | LEB128 varint over zigzag |
| string | Marker, **escaped** contents, terminator — a NUL costs two bytes | Varint length, then the bytes, unescaped |
| record | Marker, fields, terminator | The fields, concatenated. Nothing else |
| reference | Marker plus a fixed 8 bytes, so it sorts as a band of its own | A varint union branch: an id, **or the target fact** |
| field names, types, arities | Never present — self-delimiting by construction | **Not sent at all** — both peers hold the schema |

Measured on the shapes a code index holds, the transport encoding is **40% smaller** than the
storage one — and that comparison is a test rather than a claim. It is not a pointwise law:
a varint is longer at the extremes, and a length prefix passes three bytes at 16 KiB where a
terminator stays at one. The win is over the data, not over every value.
