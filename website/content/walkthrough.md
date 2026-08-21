---
title: A guided tour
description: One session from an empty directory to a sealed database — writing facts, asking questions, reading a plan, paging, and watching a finished database refuse a writer.
---

One session, start to finish: an empty directory in, a sealed database out, with a look at
the interesting things along the way. It takes about ten minutes to follow, and it assumes
you have read [Getting started](getting-started.html) — this is the same path with more of
the corners in it.

The data is a **real code index of real code**: the repository's own .NET solution — the
client library, the demo producer, and the indexer — indexed by that same indexer, with
Roslyn answering what every name means. Every command below was run, and every block of
output is what it printed.

Set up once (`FJ` is a Fjord checkout with `cargo build --release` done and the .NET SDK
on the path):

```bash
cd /tmp && mkdir fj-tour && cd fj-tour
FJ=/path/to/fjord
AP=$FJ/target/release/fjord
```

## 1. A schema you can read

The sample schema is a file, `schemas/code.sigla`, and it parses like any other. Ask it
what it thinks it is:

```bash
$AP schema check $FJ/schemas/code.sigla
```

```text
27 predicate(s) in 1 file(s)
  schemas/code.sigla
fingerprint 0xb08eea634e866a75
```

The fingerprint is computed over the **canonical form** — fully-qualified names, no
comments, no whitespace, no declaration order. Two files that mean the same thing have the
same number. Per-predicate fingerprints come out too:

```bash
$AP schema fingerprint $FJ/schemas/code.sigla
```

```text
ID  PREDICATE       TYPE                                                         FINGERPRINT
0   src.Assembly    string                                                       36525ff21049
1   src.Attribute   { attribute: string, target: src.Decl }                       44271aed92ee
2   src.AttributeOf { target: src.Decl, attribute: string }                       3917b590f90a
3   src.Compilation { assembly: src.Assembly, framework: string, project: … }     a1f1156c4e18
4   src.Decl        { module: src.Module, name: string, line: int } -> string     54a21901f27e
…
```

Two of those predicates are the same data twice: `src.Attribute` leads with the attribute
and `src.AttributeOf` leads with the target. That is not redundancy, it is the index
design — and [step 6](#6-read-the-plan) is where it becomes visible.

## 2. Create, and serve

```bash
$AP --data-dir ./db create code --schema $FJ/schemas/code.sigla
$AP --data-dir ./db serve --ready-file ./ready &
while [ ! -e ./ready ]; do sleep 0.1; done
```

```text
created code (01M0G64F9Q2YYKDAG6459JGZJ5) against schemas/code.sigla
fjord serve
  data dir   ./db
  socket     ./db/fjord.sock
  protocol   2
  databases  1
    code                 writable
```

**No schema is printed, because a server has none of its own.** Each database is served with
its own embedded copy, so one store root can hold artifacts built from different declarations
— and one that embedded no copy is listed rather than served, since the only alternative is to
guess how its rows decode.

## 3. Write facts, holding no ids

The producer is `Boxops.Fjord.Indexer`, pointed at the .NET code it is itself part of:

```bash
dotnet run --project $FJ/clients/dotnet/Boxops.Fjord.Indexer --configuration Release -- \
  --source $FJ/clients/dotnet --at ./db/fjord.sock//code
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
  src.File                        20
  src.Decl                       617
  src.Ref                      2,672
  src.Project                      3
  src.ProjectRef                   2
  src.Line                     6,183
  …
  total                       15,441 facts in 28 blocks
  server                      15,039 created, 40,382 deduped

references: 2,672 resolved, 1,718 to declarations outside the index, 1 unresolved
```

Read the two server counts together: 55,421 facts touched, 15,039 rows exist — because
every reference the walk wrote was **the target fact nested inline** rather than an id:

```text
src.Decl {
  module = src.Module {                              ← a whole fact, not an id
    file = src.File "Boxops.Fjord.Client/Crc32.cs",  ← nested again
    name = "Boxops.Fjord.Client"
  },
  name = "Crc32", line = 7
}
```

The server interns each nested fact bottom-up — a parent's key has no bytes until its
children have ids — and substitutes the id. A file named a few thousand times is written
once and deduplicated the rest. That is why an indexer needs no map from entities to
identities and no emission order: it emits what it holds where the syntax walk stands. (The
`1,718 to declarations outside the index` are references to the BCL and packages — real
code points at code nobody walked, and the indexer says so rather than inventing targets.)

## 4. Ask the first questions

```bash
$AP --data-dir ./db query code 'F where src.File F' --limit 3
```

```text
VALUE
Boxops.Fjord.Client/Blocks.cs
Boxops.Fjord.Client/Buffers.cs
Boxops.Fjord.Client/Crc32.cs
3 row(s)
fjord: stopped at 3 rows; raise or drop --limit to see the rest
```

`--limit` is **not** `LIMIT`: the query is unchanged, the server does the work up to the
point the in-band cancel lands, and what it bounds is what crosses the socket.

A record head names the output fields:

```bash
$AP --data-dir ./db query code \
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

The columns came back alphabetically because a *query's* record fields are sorted by name
when it is lowered — so `{a = 1, b = 2}` and `{b = 2, a = 1}` are one type and one set of
bytes. A *schema's* fields are never sorted; that order is the key order.

## 5. A reference is an id, until you ask

```bash
$AP --data-dir ./db query code 'R where R = src.Ref _' --format jsonl --limit 2
```

```json
"#23:60"
"#23:62"
```

`#23:60` is a `FactId`: predicate 23, sequence 60. sigla cannot ask what it names — a query
names a fact by its key, never by its number, and putting an id in the language would put a
storage detail in a query. So the question goes to the **protocol**, and the client asks it:

```bash
$AP --data-dir ./db query code 'R where R = src.Ref _' --format jsonl --limit 2 --expand
```

```json
{"to": {"module": {"file": "Boxops.Fjord.Client/Crc32.cs", "name": "Boxops.Fjord.Client"}, "name": "Crc32", "line": 7}, "file": "Boxops.Fjord.Client/Blocks.cs", "at": {"line": 98, "col": 24, "length": 5}}
{"to": {"module": {"file": "Boxops.Fjord.Client/Crc32.cs", "name": "Boxops.Fjord.Client"}, "name": "Crc32", "line": 7}, "file": "Boxops.Fjord.Client/Blocks.cs", "at": {"line": 99, "col": 13, "length": 5}}
```

That is the **logical form**: the same shape a producer sends, and the same shape the
content hash is computed over. The recursion, the depth bound and the cache are the
client's, because how deep to expand is a display decision. The server does one point read
per distinct id.

## 6. Read the plan

The shell holds the schema the server serves, so it compiles locally and can show a plan
without running anything.

```bash
$AP --data-dir ./db shell code
```

Find-references, which is the question this schema is shaped for:

```text
sigla> :plan {f = F, l = L} where src.Ref {to = src.Decl {name = "Crc32"}, file = F, at = {line = L}}
  r0 <- src.Decl scan
       where name == "Crc32"
  r1 <- src.Ref seek[to = r0#, file = _, at = _]
  head {f = r1.file, l = r1.at.line}
```

Two levels, and the plan says exactly what each costs. `src.Decl`'s key is
`{module, name, line}`, so a constraint on `name` cannot narrow the scan — the leading
field is open, and the name can only **filter** rows the scan already produced. Then
`src.Ref`'s key leads with `to`, so the declaration's fact id **seeks**: `r0#` is spliced
into the seek key, and only the references to that declaration are read.

Ask for the outcome as well as the intent:

```bash
$AP --data-dir ./db query code \
  '{f = F, l = L} where src.Ref {to = src.Decl {name = "Crc32"}, file = F, at = {line = L}}' \
  --profile
```

```text
F     L
#9:2  98
#9:2  99
#9:2  99
#9:2  99
#9:2  99
5 row(s)
STEP      EXAMINED
src.Decl  483       full scan
src.Ref   5
488 examined, 5 produced
```

Every declaration in the index examined to find one, then exactly five rows for its
references. The fix is not a query change; it is the schema — and it is what
`src.SearchByName` exists for:

```text
sigla> :plan D where D = src.SearchByName {name = "Fjord"..}
  r0 <- src.SearchByName seek[name = "Fjord".., to = _]
  head r0#
```

The same names, keyed the other way round, so a name prefix is a **range** rather than a
filter. That is what a derived predicate is: data a query could compute, stored keyed the
way the query wants to read it.

Every other shape the language compiles to — reading through a reference, arithmetic, a
negation, a denial, a disjunction — is laid out side by side in
[Executor & resume](executor.html#every-construct-as-a-plan).

## 7. Paging that holds a real cursor

```text
sigla> :limit 3
  3 row(s) per page
sigla> F where src.File F
  : str
"Boxops.Fjord.Client/Blocks.cs"
"Boxops.Fjord.Client/Buffers.cs"
"Boxops.Fjord.Client/Crc32.cs"
  :more for the next 3 — 3 so far
sigla> :more
"Boxops.Fjord.Client/Errors.cs"
"Boxops.Fjord.Client/FjordAddress.cs"
"Boxops.Fjord.Client/FjordConnection.cs"
  :more for the next 3 — 6 so far
```

`:more` is not a re-run with an offset. The server suspended the query, encoded one
detached row per open loop level into a **bytes-only token**, and handed it over; the next
page resumes from those bytes. Nothing is held server-side between pages, which is what
makes paging stateless — a web tier can page without holding a connection.

## 8. A mistake is a caret, not a round trip

```bash
$AP --data-dir ./db query code 'X where src.Nope X'
```

```text
error[reject/unknown-predicate]: `src.Nope` is not a predicate in this schema
  ┌─ <input>:1:9
  │
1 │ X where src.Nope X
  │         ^^^^^^^^^^
```

The client compiled it against the schema the server serves, so the diagnostic arrived
without asking the server anything. Where the two compilers could disagree, the **server**
decides what runs.

## 9. Your own schema

A schema is a file, and creating a database against one freezes it there:

```schema
# people.sigla
schema demo {

  # A scalar key: the whole key is one string.
  predicate Person : string

  # A record key. Field order is key order, so this is fast at
  # "who does this person know" and only filters the other way.
  predicate Knows : { from : Person, to : Person }

  # A value side (`-> T`) is fetched only when a query asks for it.
  predicate Age : { person : Person } -> int
}
```

```bash
$AP schema fingerprint people.sigla
```

```text
ID  PREDICATE    TYPE                                    FINGERPRINT
0   demo.Age     { person: demo.Person } -> int          a3b1b02ea361
1   demo.Knows   { from: demo.Person, to: demo.Person }  080f8e02ff95
2   demo.Person  string                                  34b7f70464c8
```

Adding a predicate is compatible. Changing one — including **reordering its fields** — is
not, because field order is encoding order:

```bash
$AP schema diff people.sigla people2.sigla    # a fourth predicate added
$AP schema diff people.sigla people3.sigla    # `Knows` fields swapped
```

```text
Compatible (1 added)
  + demo.Employer

Breaking (1 predicate(s))
  ~ demo.Knows  (modified: 080f8e02ff957601 → c1779584fe40b587)
```

Create a database against it, through the running server:

```bash
$AP create './db/fjord.sock//people' --schema people.sigla
```

```text
created people (01M0BN8AG2APYZB3B5YXGY58VW) against people.sigla
```

The address is `[where//]name[@instance]`, and it is the same grammar every client takes —
the CLI, the viewer, and the .NET indexer. See [CLI reference](cli.html#addressing).

## 10. Seal it, and watch it refuse

```bash
$AP --data-dir ./db finish code
```

```text
sealing code — merging trees, then computing identity
sealed code: 15039 facts, 2378056 bytes, identity 0xdd0fe1300c88a3fa
```

`finish` makes the data durable, **merges every tree**, computes
`hash(canonical schema, base facts)`, records it, and flips the status as the last durable
act. Now the database is an artifact:

```text
NAME  INSTANCE                    STATUS    SCHEMA        CONTENT       FACTS  BYTES    CREATED
code  01M0G64F9Q2YYKDAG6459JGZJ5  complete  b08eea634e86  dd0fe1300c88  15039  2378056  2026-08-20 18:15:05Z
```

and every writer is refused at the handshake, structurally rather than per fact:

```text
Boxops.Fjord.Client.FjordServerException: ModeRefused: `code` is complete: it takes no more writes
```

## What the tour showed

| You saw | The rule behind it |
|---|---|
| 40,382 facts deduped | Interning **is** the dedup; a nested reference resolves to one row |
| A name that filtered and an id that seeked | Field order is key order, and key order is the index design |
| `#23:60` in a row, expanded on request | Stored, a reference is a `FactId`; expansion is a protocol question, not a query one |
| A scan, then a seek spliced with its id | A plan is a nested loop, and the order of its steps *is* the nesting |
| `:more` returning the next three | A resume token is bytes, so paging holds nothing open |
| A caret with no round trip | The client compiles; the server decides what runs |
| `complete`, and a refused writer | `Writable → Complete` is one way, and enforced at session establishment |
