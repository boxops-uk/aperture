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

Four things, in the order they matter:

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
4. **The rest of the schema.** Sixteen of the built-in schema's twenty-two predicates —
   the build layer and the declaration graph — cannot be answered by a syntax walk at
   all, and `example/index.py` fills none of them. This program is where they come from,
   which makes it part of the schema rather than a consumer of it.

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
  server                  18,176,899 created, 44,422,889 deduped
```

Five million references naming nine hundred thousand declarations *is* that dedup count.
It is interning working, and it is the number this whole exercise exists to measure.

## How C# maps onto the code index

The schema is the server's — twenty-two predicates, hardcoded until Phase 8 — so the
question is not what to declare but what to put in it. `CodeIndex.cs` states it a third
time, independently, because that is what the handshake fingerprint is for.

**The source layer**, which every indexer here fills:

| predicate | what it holds | how it is decided |
|---|---|---|
| `src.File` | a path, relative to `--root` | every syntax tree with a file behind it, minus `bin/` and `obj/` |
| `src.Module` | `{file, name}` | the **namespace**, per file. A file declaring two namespaces is two modules; the C# analogue of a Python module is not the project, because a project spans namespaces and a namespace spans projects |
| `src.Decl` | `{module, name, line}`, value = kind | every symbol with syntax of its own: types, methods, constructors, operators, properties, indexers, events, fields, enum members, delegates, local functions. The name is qualified by its containing *types* — `Store.Cursor.Next` — and the line is the identifier's, not the first attribute's |
| `src.SearchByName` | `{name, to}` | the same declaration keyed by its **short** name, which is what someone searching types |
| `src.Ref` | `{to, file, at: {line, col}}` | every identifier that binds to a declaration this index holds |
| `src.Import` | `{from, to}` | module → module, deduped, implied by where the references actually resolved |
| `src.Line` | `{file, line}`, value = the text | every line of every file walked, blanks included |

**The build layer** — what compiled a file, and into what:

| predicate | what it holds | how it is decided |
|---|---|---|
| `src.Project` | a `.csproj` path | every project file under `--source`, whether or not it built |
| `src.Assembly` | an assembly name | `AssemblyName` if the build or the project file states one, else the project file's base name — which is MSBuild's own default |
| `src.Compilation` | `{assembly, framework, project}` | one per project per target framework: the resolved TFM after a design-time build, the TFM as the project file spells it otherwise |
| `src.ProjectSource` | `{file, project}` | MSBuild's source list where there is one; the nearest enclosing project otherwise; nothing at all where neither answers |
| `src.ProjectRef` | `{from, to}` | `<ProjectReference>`, resolved to a project this index holds |
| `src.Package` · `src.PackageRef` | `{name, version}` · `{package, project}` | `<PackageReference>`, with the version MSBuild resolved where it was asked |

**The declaration graph** — what a syntax walk cannot see:

| predicate | what it holds | how it is decided |
|---|---|---|
| `src.Member` | `{container, member}` | every declaration with a containing type, including nested types |
| `src.Extends` | `{base, type}` | the base type, unless it is `System.Object` |
| `src.Implements` | `{iface, type}` | **`AllInterfaces`** — the closure, not the list the declaration writes |
| `src.Override` | `{base, member}` | `override`, plus implicit and explicit interface implementation, which are the same question |
| `src.Param` | `{decl, index, name}`, value = the type | methods, constructors, operators, indexers, and a delegate's invoke signature |
| `src.TypeOf` | `{decl}`, value = the type | a field's or property's type, a method's return type, an event's handler type |
| `src.Doc` | `{decl}`, value = the text | the `///` comment above the declaration, slashes stripped, tags kept |
| `src.Attribute` | `{attribute, target}` | every attribute applied to a declaration, by its full type name |

Several of those deserve their reasoning stated.

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

**`src.Implements` stores the closure, and that is a decision.** A type that says
`: List<T>` *is* an `IEnumerable`, and someone asking for every enumerable in a
repository is asking the semantic question, not the syntactic one. focus has no
recursion to close a transitive relation with at query time, so the closure is written
down — the same trade `src.SearchByName` makes, and the same one Glean makes for its
`InheritedMembers`: more facts on the way in, one seek on the way out.

**`src.Param` and `src.TypeOf` hold a type as a *spelling*, not as an identity.**
`ReadOnlySpan<byte>` is what a signature renders as and what a person reads; the identity
is already in the index, because the type name in a parameter list is an ordinary
identifier that the walk resolves into a `src.Ref` like any other. Storing the display
string as a *value* rather than a key field says exactly that: read it, do not join on
it.

**`src.Line` is the line table, and it is complete on purpose.** There are no arrays in
the type model yet ([open decisions](../../../docs/open-decisions.md)), so a sequence is
said the only way this schema can say one — a fact per element with the position in the
key. Blank lines are included: a table whose gaps mean "empty" is indistinguishable from
one whose gaps mean "not indexed". It is the largest predicate by bytes, the widest row
in the database, and the reason a search hit can be rendered with its context without
opening a file — and `--no-lines` leaves it out when what is being measured is the
semantic index.

**The build layer degrades rather than disappears.** A design-time build knows the
resolved framework, the assembly name MSBuild computed, versions after central package
management, and the exact source list. Without one, the project files are still on disk
and still say what they reference — so `ProjectIndex` reads their XML, attributes each
source file to the nearest enclosing project, and records an unexpanded
`$(NetCoreAppCurrent)` as exactly that rather than inventing a framework. Two things it
will not do: guess a version a project file does not state (the empty string means "not
stated"), and attribute *shared* source — `src/libraries/Common` in dotnet/runtime lives
under no project and is compiled into a hundred assemblies by explicit `<Compile
Include>`, so it gets no edge at all rather than a plausible one. The run says how many
files that was.

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
- **Generic instantiations are collapsed to their definition** — `List<int>.Add` and
  `List<T>.Add` are one declaration — and a type parameter is not a declaration at all.
  The hierarchy itself *is* indexed now, as `src.Extends` and `src.Implements`.
- **A type the compiler could not resolve is not recorded as one.** `src.TypeOf`,
  `src.Param` and `src.Attribute` skip an error type, because an unresolved type
  displays as whatever the source wrote — so the same declaration reached from a run
  that resolved it and one that did not would be a same-key-different-value conflict for
  the first two, and two spellings of one key for the third.
- **The extras are written once per declaration key.** Two symbols landing on one
  (module, name, line) — overloads written on a single line — give the first one's kind,
  type, parameters and doc comment, and the run counts the collision.

## Two things that had to be got right

**`Compile`, not `Build`.** Buildalyzer's default targets are `Clean;Build`, and both
delete things — `Build` because it depends on `IncrementalClean`, which removes what the
last build wrote and this one did not, and a design-time build writes nothing. Pointing
the default at a checkout empties every `bin` in it. It did that here, to this program's
own output, while it was running out of it.

**One declaration key, one kind.** A `src.Decl` key is (module, name, line) and its value
is the kind, so two declarations agreeing on the key and differing on the kind are a
same-key-different-value conflict — which the server rejects deterministically and by
name (`ops-I5`), failing the stream carrying it. Right for a database, and the wrong way
to lose an hour of indexing. So a constructor is `Store.ctor` rather than `Store` —
otherwise a type and its constructor written on one line collide — and where two symbols
still land on one key, the first kind wins and the run counts it.

That rule now guards more than the kind. A declaration's type, its parameters' types and
its doc comment are values too, so **everything carrying one is emitted once per key**:
the first symbol to reach a key describes it and later ones do not. Two overloads written
on a single line therefore give one signature rather than a failed stream — which is the
right trade, because the alternative is losing the other eighteen million facts to a
collision nobody would have predicted.

## The flags

```
--source <path>       a .sln, .slnx, .csproj, or a directory holding one (required)
--root <path>         paths are reported relative to this (default: the solution's directory)
--dotnet <path>       the dotnet host to build with (default: <root>/.dotnet/dotnet if present)
--socket <path>       the server's socket (default: /tmp/aperture.sock)
--database <name>     the database to write to (default: code)
--batch <n>           facts per block (default: 4096)
--max-files <n>       stop after n source files
--skip-files <n>      skip the first n files, in path order (--syntax-only)
--max-projects <n>    stop after n projects
--jobs <n>            builds, and files walked, at once (default: 4, or fewer cores)
--no-refs             declarations only: no src.Ref, no src.Import
--no-lines            do not write the line table (src.Line)
--no-docs             do not write doc comments (src.Doc)
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

**`--skip-files` is what makes a checkout too big for one compilation indexable.** A
syntax-only run holds every tree of its `--source` at once, and the memory that costs is
measurable: on this corpus, roughly 1.4 GB per ten thousand files parsed, plus another
0.23 GB for every thousand files *walked* as the symbol tables fill. dotnet/runtime's
`src/` is 32,710 files, which is more than most machines will give. Sliced, it is nine
runs of four thousand:

```sh
for i in $(seq 0 8); do
    dotnet run --project clients/dotnet/Aperture.Indexer -c Release -- \
        --source ~/runtime/src --root ~/runtime --syntax-only --no-smoke \
        --skip-files $((i * 4000)) --max-files 4000 \
        --socket /tmp/ap-runtime/db/aperture.sock --database code
done
```

The slices are a partition because the order is the path order, and the facts accumulate
in one database because **nothing here holds an id**: a later slice naming a declaration
an earlier one wrote sends the same nested fact and the server dedups it. What it costs
is stated rather than hidden — a reference from one slice to a declaration in another
binds against the framework's metadata rather than against source, so it is dropped as
external. Slices that fall on a library boundary keep nearly all of it; slices that cut
one in half do not.

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

This is a real one: **all of dotnet/runtime's `src/`** — 32,710 C# files, 430 MB of
source — indexed `--syntax-only` into a release server on an eight-core machine, one
compilation, `--jobs 8`.

```
indexed 32,710 file(s) in 4613.4s
  src.File                    32,710
  src.Module                  36,192
  src.Decl                   888,292
  src.SearchByName           888,292
  src.Ref                  4,879,151
  src.Import                 271,553
  src.Project                  5,607
  src.Assembly                 5,607
  src.Compilation              6,906
  src.ProjectSource           26,685
  src.ProjectRef               1,281
  src.Package                    437
  src.PackageRef                 437
  src.Member                 835,542
  src.Extends                 16,446
  src.Implements              32,581
  src.Override                98,651
  src.Param                  578,271
  src.TypeOf                 761,197
  src.Doc                     73,975
  src.Attribute              175,405
  src.Line                 8,583,810
  total                   18,199,028 facts in 4,454 blocks
  server                  18,176,899 created, 44,422,889 deduped
  writing                     1827.5s of 4613.4s
  throughput                   3,944 facts/s

references: 4,879,151 resolved, 190,888 to declarations outside the index, 3,130,214 unresolved
  6,025 file(s) no project compiles (shared source, or outside every project directory)
```

**1.8 GB on disk**, about a hundred bytes a fact — which is what a fact costs once its
references are ids and its strings are stored once.

Five of those numbers are worth reading twice.

**`src.Line` is 8.6 million of the 18.2 million.** A line table is a fact per line and
this repository has eight and a half million lines of C#; it is also most of the bytes,
since its value is the line. `--no-lines` halves the index for the runs where the
semantic half is the point.

**The build layer is the whole repository, not the walk.** 5,607 projects, because a
project is a fact about the checkout and a run stopped early by `--max-files` should
still say which projects exist.

**`src.ProjectSource` is 26,685 against 32,710 files, and the gap is the honest part.**
Six thousand files have no project the containment rule can name: shared source under
`src/libraries/Common`, and directories like `src/tests/JIT/CodeGenBringUpTests` that
hold **645 project files beside their sources**, one per test. Containment says "one of
these 645", so the run says nothing and counts it. A design-time build answers all of
them exactly.

**3.13 million names went unresolved — one in three.** That is not the usual
`--syntax-only` figure (one in eight) and the reason is instructive: this is a whole
repository compiled as *one* compilation, and dotnet/runtime defines the same type many
times over — a reference assembly, an implementation, a per-platform variant. Roslyn
cannot pick, so the name binds to nothing. Slicing per project, or a design-time build,
resolves it; indexing the whole tree at once is what buys the cross-library edges that
do resolve.

**Writing was 1,827 of 4,613 seconds — 40%.** The rest is Roslyn: five million names
asked what they mean, and every declaration asked what it extends, implements,
overrides, takes as parameters and says in its doc comment.

**`created` counts every fact written, nested targets included; `deduped` those already
there.** Eighteen million facts *sent* were sixty-two million facts *touched* — a factor
of 3.4, which is what it costs to send each reference with its declaration, that
declaration's module and that module's file nested inside it. Forty-four million of them
were already in the database. That number is interning working, and producing it is the
whole point of the exercise; `--dry-run` is the honest way to measure the client without
it.

Then it asks the database a handful of questions, chosen for what they cost rather than
for what they mean. These are `--profile` runs against the whole index above, and the
`examined` column is the point of printing them:

```
  every type implementing IDisposable — the closure, so one seek
  focus> {type = T.name} where src.SearchByName {name = "IDisposable", to = I};
         src.Implements {iface = I, type = T}
    src.SearchByName        2
    src.Implements      3,879
    fetch src.Decl      3,879
    3,879 row(s) in 39 ms

  everything marked [Obsolete] — a string leading the key, so also one seek
  focus> {name = D.name} where src.Attribute {attribute = "System.ObsoleteAttribute", target = D}
    src.Attribute       1,960
    fetch src.Decl      1,960
    1,960 row(s) in 17 ms

  the parameters of every `TryParse`, in order
  focus> {at = P.index, name = P.name, type = P.value}
         where src.SearchByName {name = "TryParse", to = D}; P = src.Param {decl = D}
    {at = 0, name = "s", type = "string?"}
    {at = 2, name = "provider", type = "System.IFormatProvider?"}
    {at = 3, name = "result", type = "TSelf"}
    6 row(s) in 3 ms

  what compiles the file that declares `Utf8JsonReader`
  focus> {assembly = Y, framework = F, project = X}
         where src.SearchByName {name = "Utf8JsonReader", to = D};
               src.ProjectSource {file = D.module.file, project = src.Project X};
               src.ProjectSource {file = D.module.file, project = P};
               src.Compilation {assembly = src.Assembly Y, framework = F, project = P}
    {assembly = "System.Text.Json", framework = "netstandard2.0",
     project = "src/libraries/System.Text.Json/ref/System.Text.Json.csproj"}
    {assembly = "System.Text.Json", framework = "$(NetFrameworkMinimum)", ...}

  uses of `SyntaxKind`, which is a join and still a scan
  focus> {line = R.at.line, col = R.at.col} where R = src.Ref {to = D};
         src.SearchByName {name = "SyntaxKind", to = D}
```

Four things those show, in the order they matter.

**The container-first keys work.** Every one of the 3,879 rows `src.Implements`
examined it also produced, out of a predicate holding 32,581 — that is a seek into
`iface`, not a scan filtered afterwards. `src.Attribute` says the same for
`[Obsolete]`: 1,960 examined, 1,960 produced, out of 175,405. Both are what the field
*names* bought, since sorted field order is the key order.

**A scalar key is read back with a nested pattern, not a field access.**
`src.Project`'s key is a bare string, so `project = P` binds a *reference* and prints as
one; `project = src.Project X` matches the target and binds its key. It is the same
spelling the write side uses, which is the point — but it is worth knowing before
writing the query the other way and getting `#7:675`.

**The unexpanded `$(NetFrameworkMinimum)` in that answer is the degradation, visible in
the data.** No design-time build ran here, so the framework is what the project file
literally says. A run with MSBuild answers `net462` for the same row.

**`src.Ref` is still the one that scans**, and it is a finding rather than a
disappointment: its key begins with a *position*, so "every use of this declaration"
reads the predicate rather than narrowing into it — five million rows on this index.
The shape of the argument is exactly the one `src.SearchByName` settles at the
declaration level, and the answer is the same: a predicate keyed by `to` would make it a
seek. That is a derived predicate nobody can declare yet
([Phase 8b](../../../PLAN.md)), which is a thing worth knowing from a measurement rather
than from an opinion.
