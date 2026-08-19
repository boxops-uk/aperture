---
title: Walkthrough
description: One session from an empty directory to a sealed artifact — with the plans, the profiles, the paging and the refusals that make the design visible.
---

This is the tour. Every command was run and every block of output is what it printed. The
database is a synthetic code index — 200 files, five declarations each, plus modules,
references and a line table — because it is large enough for the interesting answers and
small enough to seed in 50 ms.

Set up once:

```bash
cd /tmp && mkdir fj-tour && cd fj-tour
AP=/path/to/fjord/target/release/fjord
```

## 1. A schema you can read

The sample schema is a file, `schemas/code.sigla`, and it parses like any other. Ask it
what it thinks it is:

```bash
$AP schema check schemas/code.sigla
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
$AP schema fingerprint schemas/code.sigla
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
$AP --data-dir ./db create code --schema schemas/code.sigla
$AP --data-dir ./db serve --ready-file ./ready &
while [ ! -e ./ready ]; do sleep 0.1; done
```

```text
created code (01M0BNMTQ3RWQFMM755NV1MWA3) against schemas/code.sigla
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

```bash
cargo run --release --example loadgen -- --data-dir ./db --files 200 --decls-per-file 5
```

```text
seeding 1,000 declarations over 200 files, 1,000 facts per block
  5,200 created, 11,000 deduped in 46.76ms — 346,440 facts/s touched, 21,385 decls/s
```

Read the two counts together. The producer sent 16,200 facts and 5,200 rows exist, because
every reference it wrote was **the target fact nested inline** rather than an id:

```text
src.Decl {
  module = src.Module {                    ← a whole fact, not an id
    file = src.File "src/f0000000.py",     ← nested again
    name = "m0000000"
  },
  name = "symbol_0000000_000", line = 1
}
```

The server interns each nested fact bottom-up — a parent's key has no bytes until its
children have ids — and substitutes the id. A file named a thousand times is written once
and deduplicated 999 times. That is why an indexer needs no map from entities to
identities and no emission order: it emits what it holds where it stands.

## 4. Ask the first questions

```bash
$AP --data-dir ./db query code 'F where src.File F' --limit 3
```

```text
VALUE
src/f0000000.py
src/f0000001.py
src/f0000002.py
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
1     symbol_0000000_000
18    symbol_0000000_001
35    symbol_0000000_002
52    symbol_0000000_003
69    symbol_0000000_004
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
"#23:1"
"#23:2"
```

`#23:1` is a `FactId`: predicate 23, sequence 1. sigla cannot ask what it names — a query
names a fact by its key, never by its number, and putting an id in the language would put a
storage detail in a query. So the question goes to the **protocol**, and the client asks it:

```bash
$AP --data-dir ./db query code 'R where R = src.Ref _' --format jsonl --limit 2 --expand
```

```json
{"to": {"module": {"file": "src/f0000000.py", "name": "m0000000"}, "name": "symbol_0000000_000", "line": 1}, "file": "src/f0000001.py", "at": {"line": 2, "col": 4, "length": 12}}
{"to": {"module": {"file": "src/f0000000.py", "name": "m0000000"}, "name": "symbol_0000000_001", "line": 18}, "file": "src/f0000001.py", "at": {"line": 15, "col": 4, "length": 12}}
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
sigla> :plan {f = F, l = L} where src.Ref {to = src.Decl {name = "symbol_0000000_000"}, file = F, at = {line = L}}
  r0 <- src.Decl scan
       where name == "symbol_0000000_000"
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
  '{f = F, l = L} where src.Ref {to = src.Decl {name = "symbol_0000000_000"}, file = F, at = {line = L}}' \
  --profile
```

```text
F     L
#9:2  2
1 row(s)
STEP      EXAMINED
src.Decl  1000      full scan
src.Ref   1
1001 examined, 1 produced
```

1,000 rows examined to find one declaration, then exactly one row for its references. The
fix is not a query change; it is the schema — and it is what `src.SearchByName` exists for:

```text
sigla> :plan D where D = src.SearchByName {name = "symbol"..}
  r0 <- src.SearchByName seek[name = "symbol".., to = _]
  head r0#
```

The same names, keyed the other way round, so a name prefix is a **range** rather than a
filter. That is what a derived predicate is: data a query could compute, stored keyed the
way the query wants to read it.

## 7. The other plan shapes, in one place

Each of these is `:plan` output, and each is a different piece of the machine.

**Reading through a reference** — a fetch level, one point read per row above it:

```text
sigla> :plan N where src.Ref {to = D}; N = D.name
  r0 <- src.Ref scan
  r1 <- src.Decl fetch[r0.to]
  head r1.name
```

**Arithmetic** — a derived bind, one value per row, in a register of its own. Not a loop
level: the cursor stores nothing for it, because it is recomputed on resume.

```text
sigla> :plan Y where src.Decl {line = L}; Y = L + 1
  r0 <- src.Decl scan
  r1 = r0.line + 1
  head r1=
```

**Negation** — a test, not a level. It binds nothing, takes no cursor entry, and each
source is drained only to its first row, because the question is whether a witness exists:

```text
sigla> :plan F where F = src.File _; !src.Module {file = F, name = "m0000000"}
  r0 <- src.File scan
  absent src.Module seek[file = r0#, name = "m0000000"]
  head r0#
```

**A denial** — a residual on the level that holds the field. Never a seek, however it is
written: "does not start with X" is the two ranges either side of one, and a seek walks one.

```text
sigla> :plan N where src.Decl {name = N}; N != "symbol_0000000"..
  r0 <- src.Decl scan
       where name does not start with "symbol_0000000"
  head r0.name
```

**A disjunction** — one level with an alternative per source, tried in order and
concatenated. Never DNF-expanded across conjuncts:

```text
sigla> :plan X where src.Decl {module = M, name = X} | src.Module {file = _, name = X}
  r0 <- src.Decl scan
     | src.Module scan
  head r0.1
```

## 8. Paging that holds a real cursor

```text
sigla> :limit 3
  3 row(s) per page
sigla> F where src.File F
  : str
"src/f0000000.py"
"src/f0000001.py"
"src/f0000002.py"
  :more for the next 3 — 3 so far
sigla> :more
"src/f0000003.py"
"src/f0000004.py"
"src/f0000005.py"
  :more for the next 3 — 6 so far
```

`:more` is not a re-run with an offset. The server suspended the query, encoded one
detached row per open loop level into a **bytes-only token**, and handed it over; the next
page resumes from those bytes. Nothing is held server-side between pages, which is what
makes paging stateless — a web tier can page without holding a connection.

## 9. A mistake is a caret, not a round trip

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

## 10. Your own schema

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

## 11. Seal it, and watch it refuse

```bash
$AP --data-dir ./db finish code
```

```text
sealing code — merging trees, then computing identity
sealed code: 5200 facts, 849350 bytes, identity 0xf2c2e86612f579e0
```

`finish` makes the data durable, **merges every tree**, computes
`hash(canonical schema, base facts)`, records it, and flips the status as the last durable
act. Now the database is an artifact:

```text
NAME  INSTANCE                    STATUS    SCHEMA        CONTENT       FACTS  BYTES   CREATED
code  01M0BNMTQ3RWQFMM755NV1MWA3  complete  b08eea634e86  f2c2e86612f5  5200   849350  2026-08-19 00:09:58Z
```

and every writer is refused at the handshake, structurally rather than per fact:

```text
loadgen: cannot connect to ./db/fjord.sock: `code` is complete: it takes no more writes
```

## What the tour showed

| You saw | The rule behind it |
|---|---|
| 11,000 facts deduped | Interning **is** the dedup; a nested reference resolves to one row |
| A name that filtered and an id that seeked | Field order is key order, and key order is the index design |
| `#23:1` in a row, expanded on request | Stored, a reference is a `FactId`; expansion is a protocol question, not a query one |
| `absent`, `r1 = …`, a second source on one level | Three step kinds and no more: a level, a test, a derive |
| `:more` returning the next three | A resume token is bytes, so paging holds nothing open |
| A caret with no round trip | The client compiles; the server decides what runs |
| `complete`, and a refused writer | `Writable → Complete` is one way, and enforced at session establishment |
