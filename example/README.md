# The example corpus — what the `aperture` shell is an index *of*

The shell (`src/main.rs`) starts with a store full of facts about a small codebase.
This directory is that codebase, the indexer that reads it, and the facts it produced:

| | |
|---|---|
| `src/` | the corpus: a tiny sorted-key store and a query layer over it, in Python |
| `index.py` | the **indexer** — parses `src/` with the standard library's `ast` and emits facts |
| `index.json` | its output, compiled into the shell by `include_str!` |

Regenerate after editing the corpus (the shell picks it up on the next `cargo build`):

```
python3 example/index.py
```

## Why any of this exists

The shell used to seed five files' worth of made-up declarations written out by hand,
which is fine for showing that a reference join works and useless for showing what a
fact database is *for*. Real names share prefixes — `encode_int`, `encode_str`,
`encode_key` — and a search over them is the query a code index answers all day.
So the facts now come from a real parse of real (if small) source, and the corpus is
here to be read alongside the answers: every row the shell prints names a file, a line
and a column you can go and look at.

**Six of the schema's twenty-two predicates, and that is the whole of what `ast` can
answer.** The rest — a type's base and interfaces, what a member overrides, a parameter's
type, which project compiles a file — are questions about *symbols* and about the build,
and Python's `ast` knows neither. They are filled by
[`Aperture.Indexer`](../clients/dotnet/Aperture.Indexer/README.md), which has Roslyn and
MSBuild to ask. A predicate nobody here fills is an empty keyspace in the shell's scratch
database and nothing else; `:schema` lists it with no facts under it.

The corpus is Python because its parser ships with it. The exercise is the facts, not
the front end that finds them. It is deliberately about the same things Aperture is —
a codec, key ranges, a store, a plan, a runner — so a query about it reads as a
question about this repository.

## The facts

Five predicates of the shell's schema come straight out of the parse — `src.File`,
`src.Module`, `src.Decl`, `src.Ref`, `src.Import` — and one is **derived**:

`src.SearchByName {name, to}` is the declaration names again, keyed the other way
round. A declaration's key begins with its module, so a prefix of a *name* cannot
narrow that scan — it can only filter rows the scan already produced. Keyed by the
name instead, the same prefix is a **range**, and the two spellings of one question
are worth comparing in the shell:

```
focus> :plan X where X = src.SearchByName {name = "encode"..}
focus> :plan D where D = src.Decl {name = "encode"..}
```

That is what a derived predicate is for, and writing it by hand here is what a deriver
does until Aperture can declare one (PLAN phase 8b).

`src.Ref` carries the **file the reference is in** alongside the line and column, and that
file is not the one `to` reaches: a reference and the declaration it names are usually in
different files, which is the whole reason to record one. A row is a place you can open —
`:help`'s find-usages example asks for the file and the line together for that reason.

```
focus> {file = F, line = L} where src.Ref {file = F, at = {line = L}, to = src.Decl {name = "encode_str"}}
```

`index.json` names references **by position**, not by id: a fact id is what a write
returns, so an indexer cannot know one. The arrays are in write order and a row may
only point at an array before it, which is what makes referential integrity a
consequence of the order rather than a check.

## What the indexer does not do

Types, scopes, or anything needing either. A reference resolves when a bare name is one
its module declares or imported by name, or when a dotted attribute reaches through an
imported module — `store.engine.store_open`. So:

- `db.put(...)` is **not** a reference, because knowing what `db` is means inferring
  its type;
- a local variable shadowing a declaration's name would be a false hit (the corpus
  avoids doing that, deliberately);
- a class's methods are indexed under a qualified name — `Store.put` — which is both
  what someone searching types and what makes `"Store"` a prefix of six declarations.

A real indexer for a real language does all of this properly. This one is honest about
the line it stops at.

## Running the corpus

It is real Python, and importable with `src` as the root:

```
PYTHONPATH=example/src python3 -m query.run
```
