---
title: Shell reference
description: The two REPLs — the wire shell over a server, and the embedded demo that seeds its own database — with every command and what each one is for.
---

`fjord shell` is two things depending on whether you name a database.

| Invocation | What it is |
|---|---|
| `fjord shell <db>` | The **product shell**. Always over the wire, even against a local server — so the protocol has a permanent exerciser and `:more` holds a real cursor across a real round trip |
| `fjord shell` | The **embedded demo**. A scratch database it seeds itself with a real index of a small Python corpus — the one thing no wire client can do |

Both share the input layer: syntax highlighting from the compiler's own lexer, history,
completion, and the rule that a line with an unclosed `{` or `(` continues on the next one.

Commands are `:`-prefixed. The psql spellings (`\d`, `\l`, `\c`, `\timing`, `\more`) are accepted
as aliases, because neither prefix can begin a sigla query — so a hand trained on psql costs
nothing.

## The wire shell

```bash
fjord --data-dir ./db shell code
```

```text
fjord shell — `code` on ./db/fjord.sock
  28 predicate(s) · rows print as jsonl · :help for commands
```

| Command | Aliases | Does |
|---|---|---|
| `<query>` | | Compile it **locally**, send it, print the rows |
| `:type <query>` | | The type of its head, without planning or running it |
| `:plan <query>` | | The plan it compiles to, without running it |
| `:facts <predicate>` | | Every row of one predicate — sugar for `X where <predicate> X` |
| `:schema [name]` | `\d`, `:d` | The schema this database is served with, or one predicate, or a prefix |
| `:more` | `\more`, `:m` | The next page of the last result |
| `:limit [n]` | | Rows per page; bare, it says what the page is |
| `:format <f>` | | How a row prints: `jsonl`, `json`, `table`, `raw` |
| `:expand [hops]` | | Show the fact a reference names, not its id. Bare toggles it |
| `:cancel` | `\cancel` | Stop the last result early |
| `:timing` | `\timing` | Toggle how long a page took |
| `:profile` | `\profile` | Toggle what a query examined, per step of its plan |
| `:list` | `\l`, `:l` | The databases on this server — a query over `fjord.db.List` |
| `:connect <db>` | `\c`, `:c` | The same session against another database |
| `:clear` | | Clear the screen |
| `:help` | `\?`, `:?`, `:h` | The table above, generated from the table itself |
| `:quit` | Ctrl-D | Leave |

`:help` is generated from the command table, so a command that exists is a command that is
listed.

### The shell compiles what you type

It holds the schema the server said it serves — the `H`/`h` exchange — so a query is compiled
**here** before it is sent. Three things follow:

- A mistake is the compiler's own diagnostic, with the code, the caret and colour, and **no round
  trip**.
- `:plan` and `:type` can be answered at all. A client never holds a plan otherwise; what was
  missing was the *schema*.
- Where the two compilers could disagree, the **server** decides what runs.

### Paging holds a real cursor

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

`:more` is not a re-run with an offset. The server suspended the query, encoded one detached row
per open loop level into a bytes-only token, and handed it over. Nothing is held server-side
between pages.

The line before the first row (`: str`) is the **row descriptor** — the shape of the head, sent
once per query.

### Rows print as JSON Lines

One value per line, shaped by the descriptor at every level, so a nested record is a nested
object. A page is not a document and three pages of one query are not three documents, which is
why it is line-per-row rather than an array. `:format table` is there for reading rather than
piping.

### `:expand` — show the fact a reference names

```text
sigla> R where R = src.Ref _
{"to": "#4:1", "file": "#9:2", "at": {"line": 2, "col": 4, "length": 12}}

sigla> :expand
sigla> R where R = src.Ref _
{"to": {"module": {"file": "src/f0000000.py", "name": "m0000000"}, "name": "symbol_0000000_000", "line": 1}, "file": "src/f0000001.py", "at": {"line": 2, "col": 4, "length": 12}}
```

A row carries a reference as a fact id, because that is what one is once stored — and sigla
cannot ask what it names. So the question goes on the protocol, and the client walks the answer:
breadth-first, one round trip per level of depth, one point read per distinct id, cached across
pages because a page of references into one file names that file forty times.

A reference that resolves to nothing is **reported**, not hidden: for an id out of a row it
cannot happen, so it means a damaged database — and a row printing the id instead would look like
a field somebody chose not to expand.

### `:profile` — what it examined

```text
sigla> :profile
sigla> {f = F, l = L} where src.Ref {to = src.Decl {name = "symbol_0000000_000"}, file = F, at = {line = L}}
STEP      EXAMINED
src.Decl  1000      full scan
src.Ref   1
1001 examined, 1 produced
```

Per **step of the plan's body**, which is what the machine counts — so a fetch, a disjunction and
a negation each get a line. Read it against `:plan`: the plan is the intent, this is the outcome.

A profile arrives once, just before the result ends, because the tally is not final until the
last chunk has run. A `:limit` that cancels early therefore reports none rather than reporting a
different query's numbers.

### `:schema` — and prefixes

```text
sigla> :schema src.Decl
sigla> :schema src.
```

An exact name describes one predicate; anything that does not resolve exactly falls back to
**prefix matching**, so `:schema src.` dumps a namespace rather than failing.

Virtual predicates are printed like any other, because the served schema is what may be *asked*
about. `fjord.db.List` is there, and `:list` is a query over it.

## The embedded demo

```bash
fjord shell
```

No server, no store root, no setup: it seeds a scratch database from a **real** index of the
Python corpus in `example/` — files, modules, declarations, references, imports, and one derived
search predicate — written through the fact API at startup.

| Command | Does |
|---|---|
| `<query>` | Compile and run it against the scratch database |
| `:type <query>` | The type of its head |
| `:plan <query>` | The plan, without running it |
| `:facts <name>` | Rows stored for a predicate, read through the executor |
| `:schema` | The predicates this shell knows |
| `:clear`, `:help`, `:quit` | As above |

Its `:help` also prints ten queries worth trying, and they answer with real names:

```text
sigla> D.name where D = src.Decl {name = "encode"..}
  : str
  "encode_int"
  "encode_key"
  "encode_str"
  3 row(s)

sigla> {file = F, line = L} where src.Ref {file = F, at = {line = L}, to = src.Decl {name = "encode_str"}}
  : {file: src.File, line: int}
  {file = src.File "store/codec.py", line = 38}
  {file = src.File "store/keys.py", line = 17}
  2 row(s)

sigla> M.name where M = src.Module _; !src.Import {from = M}
  : str
  "store.codec"
  1 row(s)
```

Read those against the corpus itself: every row names a file and a line you can go and look at.

Rows here print in sigla's own value syntax rather than as JSON, because this shell renders from
the engine's values directly — it is the one place in the tool that is not a wire client.

:::note Why both survive
The wire shell exists so the protocol has a permanent exerciser and so paging holds a real
cursor. The embedded one exists because it **seeds its own database** — which no wire client can
do, since writing needs a producer — and because it is the fastest possible way to see the
system answer a real question.
:::

## Things worth trying in either

```sigla
:plan D where D = src.Decl {name = "encode"..}
:plan D where D = src.SearchByName {name = "encode"..}
```

The same question twice, and the plans are the argument for what a derived predicate is: one
scans and filters, the other seeks a range. Run both with `:profile` on and read the
`EXAMINED` column.

```sigla
D.name where D = src.Decl _; !src.Ref {to = D}
```

Unused declarations — a negation, which is a test rather than a level: it binds nothing and each
source is drained to its first row.

```sigla
{decl = D.name, module = D.module.name} where D = src.Decl {name = "encode"..}
```

Reading **through** a reference: `D.module` is a fact id, so its name is in another fact's key and
the plan grows a fetch level.
