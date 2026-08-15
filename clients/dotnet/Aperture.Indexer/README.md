# The .NET indexer

A real indexer for a real language, writing into Aperture over the wire protocol:
**Buildalyzer** runs each project's design-time build out of process, **Roslyn** answers
what every name in the resulting compilation means, and the facts go straight down the
socket to a running server.

`Aperture.Demo` shows the protocol works by writing six declarations somebody typed out.
This writes however many a checkout of .NET source contains, which is the other thing a
database needs shown: that it holds up when the facts were not chosen to be convenient.

```sh
# a fresh database, a server, and a checkout indexed into it
./clients/dotnet/index-repo.sh ~/src/OrchardCore

# or by hand, against a server already running
dotnet run --project clients/dotnet/Aperture.Indexer -- \
    --source ~/src/OrchardCore --socket /tmp/ap-index/db/aperture.sock --database code
```

## What it is for

Three things, in the order they matter:

1. **Volume.** A million facts, written the way a producer really writes them, is the
   only way to find out what interning costs, what a scan costs when the predicate is
   not six rows, and whether a plan that looks fine on the fixture still looks fine when
   `src.Ref` has seven figures in it.
2. **A second implementation, doing something harder.** The demo proved the protocol is
   implementable from outside. This proves it is *usable* from outside — that a producer
   with a real workload, emitting in the order a syntax walk reaches things, needs
   nothing the protocol does not offer.
3. **Something to query.** An index of code someone knows is a database whose answers
   can be checked by opening the file.

## The shape of the run

**A producer that holds no fact ids.** Roslyn hands this program a symbol; it turns the
symbol into the `src.Decl` fact that names it and nests *that whole fact* wherever a
reference to it goes — through the module that holds it, down to the file that holds
that. It keeps no map from entities to identities, and it emits in whatever order the
walk reaches things.

At six declarations that is an elegance argument. At a million facts it is the only
tractable option: the alternative is a second pass over an index that no longer fits in
memory, ordered so that every target is written before every reference to it.

The cost lands on the server, deliberately, and the run reports it:

```
  server                     672,481 created, 4,318,904 deduped
```

A million references naming forty thousand declarations *is* that dedup count. It is
interning working, and it is the number this whole exercise exists to measure.

## How C# maps onto the code index

The schema is the server's — six predicates, hardcoded until Phase 8 — so the question
is not what to declare but what to put in it. `CodeIndex.cs` states it a third time,
independently, because that is what the handshake fingerprint is for.

| predicate | what it holds | how it is decided |
|---|---|---|
| `src.File` | a path, relative to `--root` | every syntax tree with a file behind it, minus `bin/` and `obj/` |
| `src.Module` | `{file, name}` | the **namespace**, per file. A file declaring two namespaces is two modules; the C# analogue of a Python module is not the project, because a project spans namespaces and a namespace spans projects |
| `src.Decl` | `{line, module, name}`, value = kind | every symbol with syntax of its own: types, methods, constructors, operators, properties, indexers, events, fields, enum members, delegates, local functions. The name is qualified by its containing *types* — `Store.Cursor.Next` — and the line is the identifier's, not the first attribute's |
| `src.SearchByName` | `{name, to}` | the same declaration keyed by its **short** name, which is what someone searching types |
| `src.Ref` | `{at: {line, col}, file, to}` | every identifier that binds to a declaration this index holds |
| `src.Import` | `{from, to}` | module → module, deduped, implied by where the references actually resolved |

Three of those deserve their reasoning stated.

**`src.Decl`'s value side is the kind** — `class`, `method`, `ctor`, `property` — because
a value cannot be matched on ([I6](../../../docs/invariants.md#i6)), which makes it
exactly right for something a query wants to *read* and never to filter by.

**`src.SearchByName` earns itself here in a way the fixture cannot show.** A declaration's
key begins with its module, so `src.Decl {name = "Parse"..}` reaches the name only after
the scan has opened — a filter over every declaration in the database. Keyed by the name
instead, the same prefix is a range. On six declarations that is a debating point; on
several hundred thousand it is the difference the `--profile` flag prints.

**`src.Import` is not the `using` directives.** In C# a `using` names a namespace, and a
namespace is declared across many files in many projects — it says nothing about which
file this one needs. What carries that is where the names actually resolved to, so the
edge here is "a name in this file resolved to a declaration in that module", deduped.
That is a dependency graph a question can be asked of; the `using` list is not.

## What it resolves, and what it does not

Everything here is a **symbol** question rather than a syntax question, which is the
whole difference between this and `example/index.py` — that one is honest about stopping
at the line where types would be needed, and this one is on the other side of it.
An extension method invoked as an instance method, a member reached through a type
inferred from a lambda's parameter, a partial class continued in another file: Roslyn has
already answered all of it, and the walk asks.

What it still does not do, each for a reason:

- **A reference to something outside the index is dropped**, not recorded. A symbol from
  a NuGet package or the framework has no source location to point at, and inventing a
  declaration for it would put file facts in the database naming paths that do not exist.
  The run reports how many: on a typical repository it is a third of all names.
- **A partial type is filed at its first declaration.** One symbol, one declaration fact.
- **A multi-targeted project is indexed once**, at the newest .NET it builds for. The
  other target frameworks are the same files and would dedup on the way in; the work
  would not.
- **Types are not indexed as a hierarchy** and generic instantiations are collapsed to
  their definition — `List<int>.Add` and `List<T>.Add` are one declaration. What the
  schema can express is what gets expressed.

## Two things that had to be got right

**`Compile`, not `Build`.** Buildalyzer's default targets are `Clean;Build`, and both
delete things — `Build` because it depends on `IncrementalClean`, which removes what the
last build wrote and this one did not, and a design-time build writes nothing. Pointing
the default at a checkout empties every `bin` in it. It did that here, to this program's
own output, while it was running out of it.

**One declaration key, one kind.** A `src.Decl` key is (module, line, name) and its value
is the kind, so two declarations agreeing on the key and differing on the kind are a
same-key-different-value conflict — which the server rejects deterministically and by
name (`ops-I5`), failing the stream carrying it. Right for a database, and the wrong way
to lose an hour of indexing. So a constructor is `Store.ctor` rather than `Store` —
otherwise a type and its constructor written on one line collide — and where two symbols
still land on one key, the first kind wins and the run counts it.

## The flags

```
--source <path>       a .sln, .slnx, .csproj, or a directory holding one (required)
--root <path>         paths are reported relative to this (default: the solution's directory)
--socket <path>       the server's socket (default: /tmp/aperture.sock)
--database <name>     the database to write to (default: code)
--batch <n>           facts per block (default: 4096)
--max-files <n>       stop after n source files
--max-projects <n>    stop after n projects
--jobs <n>            builds, and files walked, at once (default: 4, or fewer cores)
--no-refs             declarations only: no src.Ref, no src.Import
--no-restore          do not let the design-time build restore first
--syntax-only         skip MSBuild; glob *.cs and parse them
--dry-run             index and encode, but connect to nothing
--emit <path>         also write every block to a file
--no-smoke            do not query the index afterwards
--verbose             let MSBuild's output through
```

**`--batch` is a flag because finding out what it should be is the point of having
something to measure with.** A flush is a write stream, and the server interns a block
inside its per-database writer lock: bigger means fewer round trips and a longer hold.

**`--dry-run` is how to measure the volume**, since it encodes every block and writes
none. A connected run hands its facts to the client, which encodes them on the way out,
so the byte count is reported only when this program does the encoding itself.

**`--syntax-only` is the honest degraded mode.** No MSBuild, no NuGet, no project graph:
every `.cs` file under `--source`, parsed against the framework this program is running
on. Declarations are all still found — they are in the syntax. References into a package's
types are not, because the type is an error type and the member on it binds to nothing.
It exists so that a repository which will not restore on this machine still produces an
index, and so that a run measuring *the database* need not wait for MSBuild first. The
loader falls back to it on its own if every project fails.

The cost is measurable, and worth knowing before choosing it. FluentValidation indexed
through the design-time build leaves **13** names unresolved out of six and a half
thousand; Roslyn's compilers indexed `--syntax-only` leave **310,525** out of two and a
half million. Those are different repositories, so it is not a controlled comparison —
but one name in five hundred against one in eight is the right order of difference, and
it is what asking MSBuild buys.

**`--emit <path>`** writes the same blocks to a file — sync marker, header, CRC and
payload, byte for byte what the wire carries. That is the fact-file format Phase 7b
ingests, so a large index can be captured once and replayed without Roslyn in the loop.

## Something big to point it at

Any .NET checkout will do, and the interesting ones are the ones nobody wrote for this.

```sh
git clone --depth 1 --filter=blob:limit=1m https://github.com/dotnet/roslyn.git
git clone --depth 1 https://github.com/OrchardCMS/OrchardCore.git
git clone --depth 1 https://github.com/JamesNK/Newtonsoft.Json.git
```

**A repository that pins its SDK will not design-time build here**, and that is not a bug
in either party: `global.json` names a version, `dotnet` refuses to substitute another,
and Roslyn's own repository pins an SDK preview that this machine does not have. Every
project then fails, and the loader falls back to parsing the `.cs` files — which for a
corpus that exists to be *large* costs the cross-assembly edges and nothing else. Delete
or relax the `global.json` in the checkout to get the full semantic index.

## What a run prints

This is a real one: Roslyn's own compilers, `src/Compilers` at 3,374 files, indexed
`--syntax-only` into a release server on a four-core machine.

```
indexed 3,374 file(s) in 381.0s
  src.File                     3,374
  src.Module                   3,587
  src.Decl                   161,553
  src.SearchByName           161,553
  src.Ref                  2,016,062
  src.Import                  56,507
  total                    2,402,636 facts in 589 blocks
  server                   2,402,616 created, 9,101,648 deduped
  writing                       74.1s of 381.0s
  throughput                     6,306 facts/s

references: 2,016,062 resolved, 235,190 to declarations outside the index, 310,525 unresolved
```

**`created` counts every fact written, nested targets included; `deduped` those already
there.** Two and a half million facts *sent* were eleven and a half million facts
*touched* — a factor of 4.7, which is what it costs to send each reference with its
declaration, that declaration's module and that module's file nested inside it. Nine
million of those were already in the database. That number is interning working, and
producing it is the whole point of the exercise.

**Writing was 74 of 381 seconds.** The bottleneck in this run is Roslyn, not Aperture:
asking two and a half million names what they mean is most of what an indexer does, which
is why the walk runs on every core the machine will give it and why `--dry-run` is the
honest way to measure the client alone.

Then it asks the database three questions, chosen for what they cost rather than for what
they mean:

```
  every namespace, which is a scan
  focus> N where src.Module {name = N}
    3,587 row(s) in 0.02s

  declarations named `SyntaxKind`, which is a seek
  focus> {kind = D.value, line = D.line, name = D.name} where src.SearchByName {name = "SyntaxKind", to = D}
    {kind = "enum", line = 12, name = "SyntaxKind"}
    {kind = "field", line = 29, name = "TrackingDiagnosticAnalyzer.Entry.SyntaxKind"}
    {kind = "field", line = 16, name = "SourceWithMarkedNodes.MarkedSpan.SyntaxKind"}
    3 row(s) in 0.00s

  uses of `SyntaxKind`, which is a join
  focus> {line = R.at.line, col = R.at.col} where R = src.Ref {to = D}; src.SearchByName {name = "SyntaxKind", to = D}
    109,394 row(s) in 4.95s
```

The third is the interesting one, and it is a finding rather than a disappointment.
`src.Ref`'s key begins with a **position**, so "every use of this declaration" cannot
narrow into the predicate — it reads all two million rows and keeps the hundred thousand
that match. Five seconds is what a scan of that size costs; the shape of the
argument is exactly the one `src.SearchByName` settles at the declaration level, and it
is the same answer here: a predicate keyed by `to` would make it a seek. That is a
derived predicate nobody can declare yet ([Phase 8b](../../../PLAN.md)), which is a thing
worth knowing from a measurement rather than from an opinion.
