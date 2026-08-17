# Phase 11 — a code-search site, and what Aperture was missing to serve it

> [Aperture design book](../README.md) · **built** — the analysis, and what came of it

**Status: everything below is done except stored derivation.** The five blockers are
closed, the degradations are answered or recorded as decided-against, and
[`aperture-viewer`](../crates/aperture-viewer) is the site. The analysis is kept as
written, with what happened marked against it — a gap analysis edited to match the
outcome is one nobody can calibrate against.

The target is Glean's code-navigation demo: browse a repository, open a file, click a
symbol, land on its definition — plus the things the demo *implies* and Glass actually
serves, which are search, find-references and a symbol panel. This file decomposes that
into the queries it needs, runs each one against Aperture, and records what came back.

Every plan quoted below was produced by `:plan` against the built-in schema, and the
row-count claims come from [`bench/FINDINGS.md`](../bench/FINDINGS.md)'s 18.2M-fact
`dotnet/runtime` index. Nothing here is inferred from reading the source.

---

## 0. What the target is, precisely

There is **no hosted Glean demo**. It ships as a ~7 GB Docker image
(`ghcr.io/facebookincubator/glean/demo`) holding a pre-built index of the React
repository, plus two servers: `glean-server` and `glean-hyperlink`
(`glean/demo/Hyperlink.hs`, ~550 lines of Haskell). The hyperlink server does exactly
three things:

| route | what it does |
|---|---|
| `/` | `predicate @Src.File wild` → a sorted list of every path, as links |
| `/<path>` | read the file from disk, ask Glean for every xref in it, splice `<a>` tags over the byte spans, render `<pre>` |
| `/<path>?offset=N` | the same, plus a script that scrolls to a byte offset |

Two things are worth noticing about that. The source text comes **from the filesystem,
not from Glean** — the demo is given `--root /react-code`. And the whole navigation
surface rests on one query shape: *every cross-reference in this file, with its source
span and its target's location.*

What Glass — the production service the demo is a toy of — adds on top
(`glean/glass/if/glass.thrift`): `documentSymbolListX`, `documentSymbolIndex`,
`findReferenceRanges`, `describeSymbol`, `symbolLocation`, `resolveSymbols`,
`searchSymbol`, `searchRelated`, `searchRelatedNeighborhood`.

So this analysis reads against two bars, and they are very different sizes:

- **The demo bar** — file list, file view with working hyperlinks, jump to definition.
- **The product bar** — the above plus search, find-references, a symbol panel, and
  type-hierarchy navigation.

---

## 1. The screens, decomposed — with the plan each one actually compiles to

✅ seeks · ⚠️ works but scans or costs more than it should · ❌ not expressible

**As it stands now:** rows 4 and 6 seek and carry their spans (`src.FileXRef`,
`at.length`, `src.DeclSpan`); 9 seeks (`src.SearchByLowerName`); 14 and 16 seek
(`src.AttributeOf`, `src.DerivesFrom`); 18 is `QUERY_COUNT`; 19 is ranked over a
bounded window by the viewer, which is where ranking belongs; and 20 is expressible,
because `<=` is a token now. The table below is what it looked like before any of
that.

| # | Screen | Query | Plan | |
|---|---|---|---|---|
| 1 | Repo root: list files | `F where src.File F` | `src.File scan` | ✅ |
| 2 | Browse a directory | `F where src.File F; F = "store"..` | `src.File seek["store"..]` | ✅ |
| 3 | **File text** | `{l = L.line, t = L.value} where L = src.Line {file = src.File "p"}` | `src.File seek` → `src.Line seek[file = r0#, line = _]` | ✅ |
| 4 | **Xrefs in a file** | `R where R = src.Ref {file = src.File "p"}` | `src.File seek` → **`src.Ref scan where file == r0#`** | ❌ |
| 5 | Xref target location | `{t = R.to.name, tf = R.to.module.file, l = R.to.line}` | `fetch[r0.to]` → `fetch[r1.module]` | ✅ |
| 6 | Underline the reference | — | `src.Ref.at` is `{line, col}`; **there is no length or end** | ❌ |
| 7 | File outline | `src.Decl {module = src.Module {file = src.File "p"}}` | `File seek` → `Module seek[file=…]` → `Decl seek[module=…]` | ✅ |
| 8 | Search box (prefix) | `src.SearchByName {name = "enc".., to = D}` | `src.SearchByName seek[name = "enc".., to = _]` | ✅ |
| 9 | Search, case-insensitive | — | no `toLower`, no case-folded index | ❌ |
| 10 | **Find references** | `src.SearchByName {name = N, to = D}; R = src.Ref {to = D}` | `SearchByName seek` → `src.Ref seek[to = r0.to, file = _, at = _]` | ✅ |
| 11 | Symbol panel: kind | `D.value` | `head r0.value` | ✅ |
| 12 | Symbol panel: type / doc | `src.TypeOf {decl = D}` | `src.TypeOf seek[decl = r0.to]` | ✅ |
| 13 | Symbol panel: parameters | `src.Param {decl = D}` | `src.Param seek[decl = …, index = _, name = _]` | ✅ |
| 14 | Symbol panel: attributes | `src.Attribute {target = D}` | **`src.Attribute scan where target == r0#`** | ⚠️ |
| 15 | Who derives from this | `src.Extends {base = D, type = T}` | `src.Extends seek[base = r0.to, type = _]` | ✅ |
| 16 | What does this derive from | `src.Extends {type = D}` | scan | ⚠️ |
| 17 | Transitive hierarchy | — | no recursion in the language | ❌ |
| 18 | "1,234 results" | — | no aggregation | ❌ |
| 19 | Best match first | — | no ordering; rows arrive in key order | ❌ |
| 20 | What symbol is at offset N | — | needs `<=`, which is a **lexer error** | ❌ |

Rows 3 and 10 are the two that used to be recorded as broken and are not. Row 4 is the
one that decides whether the demo can be built at all.

---

## 2. The blockers

### B1 — "every xref in this file" is a full scan of the largest relational predicate

This is *the* hyperlink demo query, and it is the one Aperture is keyed against:

```
r0 <- src.File seek["store/keys.py"]
r1 <- src.Ref scan
     where file == r0#
```

The profiler agrees — it labels the step `full scan`. `src.Ref` holds **4,879,151** rows
on `dotnet/runtime`, so every file view reads five million rows to find the few hundred
in one file.

**It is not a mistake, and that is what makes it interesting.** `src.Ref` is keyed
`{to, file, at}` deliberately: leading with the target is what made *find-references*
seek ([findings §2 and §11](../bench/FINDINGS.md)), and before that fix it was
find-references that could not be answered. The file view wants the same facts keyed the
other way. One predicate cannot lead with two different fields.

Glean's answer is a **derived stored predicate** —
`codemarkup.FileEntityXRefLocations : { file : src.File, xref : XRefLocation }` — the
same data, stored a second time in the order the reader wants. That is
[Phase 8b](../PLAN.md), which is not built.

Aperture's answer *today* is to write the second copy from the indexer. That works, and
it is worth naming what it means: **it would be the third time.** `src.SearchByName` is
already "declaration names, stored keyed the way search wants to read them, written by
hand until Phase 8b can declare one" (`schemas/code.aps`), and `src.Implements` is
already the *transitive closure* of the interface graph, written down because "there is
no recursion in focus to close a transitive relation with afterwards"
(`Aperture.Indexer/Indexer.cs`). Three hand-written derivations, each with the same
comment attached, is the strongest available argument that 8b is the next real phase
rather than a nice-to-have.

### B2 — a reference has a position but no extent

`src.Ref.at` is `{ line : int, col : int }`. To draw a hyperlink you need to know where
the identifier *ends*, and nothing in the index says. `src.Decl` is worse: it carries a
`line` and no column at all, so "highlight the definition" has nothing to highlight.

Glean carries `src.ByteSpan { start, length }` everywhere and its `codemarkup.Location`
is a `RangeSpan`. The data exists on Aperture's side too — Roslyn hands the indexer a
`TextSpan` and a `FileLinePositionSpan` and both are discarded.

Cost: two fields in `schemas/code.aps`, a few lines in the indexer, and a re-index.
Cheap now, and considerably less cheap once somebody's index is in production — which is
the same sentence [findings §11](../bench/FINDINGS.md) wrote about the key order, one
phase too late.

### B3 — deep paging requires a sticky server-side stream

The resume `Cursor` is real, it is bytes-only, and [I4](invariants.md#i4) says a resumed
run equals an uninterrupted one. **It never crosses the wire.** The server holds it in
the session (`aperture-server/src/session.rs`, `next: Option<Cursor>`), keyed by stream
id, and the client's `Rows` is a bookmark naming that stream — not a token that carries
the position.

For a web tier that matters more than it sounds, because the obvious workaround is also
unavailable: "everything after key K" cannot be expressed. `"a"..` is a **prefix**
constraint, there is no upper bound form, and `<`/`>` are not tokens. So a stateless
`?page=7` has no implementation — not a slow one, none. A site either holds a
connection and a live stream per in-flight result (and pays B4), or caps every result at
one page.

### B4 — a connection retains ~3.5 kB per query, for its whole life

[Findings §7](../bench/FINDINGS.md) measured it and named the exposure precisely: *"A
connection pool is exactly the shape that hits the bottom row, and it is the one to size
RAM for."* One connection issuing 200,000 point lookups grew RSS by ~500 MB;
`read_loop`'s stream map has no removal path and the client never reuses a stream id.

A web tier is a connection pool by construction. At the measured search throughput
(~6,100 q/s) a pooled connection passes 200,000 queries in **33 seconds**. So either the
pool recycles connections on a query count — which is a workaround with a number in it
that nobody will remember to keep true — or the costed fix lands first: remove the
stream from the map when its task completes, and let the client reuse ids.

This is the one item on the list that is a correctness fix rather than a feature, and it
is small.

### B5 — there is no HTTP surface, and the client is blocking

`aperture-client` is synchronous: `Connection::query(&mut self, …)`. One connection
serves one caller at a time, though it can hold several open result bookmarks. Nothing
in the workspace depends on an HTTP library.

That is ordinary work rather than a gap in the database, and it is listed because it is
not *nothing*: a pooled, recycled, blocking-client-behind-an-async-web-server tier is a
real component with real failure modes, and B3 and B4 are both its problems to hold.

---

## 3. The degradations — screens that work, but worse than Glean's

**Search is a case-sensitive prefix and nothing else.** No `toLower` (Angle has exactly
two string primitives and that is one of them), no substring, no fuzzy, no scope search.
A two-branch disjunction (`"Enc"..` | `"enc"..`) compiles and seeks twice, but it only
fixes the first letter. The real answer is a case-folded index predicate written by the
indexer — B1's pattern again.

**No ranking, no counts.** Results arrive in key order, which for a name prefix is
alphabetical — acceptable for type-ahead, wrong for "best match first". `prim.size (all
…)` is how Angle counts; focus has no aggregation, so "1,234 results" is unavailable and
so is "showing 50 of many".

**Two key orders point the wrong way for the symbol panel.** `src.Attribute` is
`{attribute, target}` — right for *"everything marked `[Obsolete]`"*, a scan for
*"what is this declaration marked with"*. `src.Extends` is `{base, type}` — right for
*"who derives from this"*, a scan for *"what does this derive from"*, which is the
direction a symbol panel actually wants. Both are the B1 tension at smaller scale.

**The symbol panel is N round trips.** Kind, type, doc, parameters, attributes are five
predicates and there is no way to ask five unrelated questions in one query, and no
batching in the protocol. Glean's Haxl coalesces exactly this into one request.

**Statement order decides the plan, because there is no cost model.** These two queries
differ only in the order of their two statements:

```
{…} where P = src.Param {decl = D}; src.SearchByName {name = "encode_key", to = D}
  r0 <- src.Param scan                              ← 3M rows
  r1 <- src.SearchByName seek[name = …, to = r0.decl]

{…} where src.SearchByName {name = "encode_key", to = D}; P = src.Param {decl = D}
  r0 <- src.SearchByName seek[name = "encode_key", to = _]
  r1 <- src.Param seek[decl = r0.to, index = _, name = _]
```

`reorder` runs whatever is runnable and takes source order among the candidates. Both
statements are runnable at the start — one as a seek, one as a scan — and nothing
prefers the seek. For hand-written queries in a web tier that is a hazard with no
diagnostic attached: the wrong ordering is not an error, it is a 3M-row scan.
[The comparison](glean-comparison.md#missing-compiler-stages) records the absent cost
model; this is what it looks like from the product side.

**One language.** The Glean demo indexes React. Aperture's only real indexer is C#
(Roslyn + Buildalyzer), plus a toy Python walker for `example/`. Building *the same*
demo means writing a JS/TS indexer; building an equivalent one means pointing at a C#
repository, and `dotnet/runtime` is already indexed at 18.2M facts. The second is
obviously right, and it is worth saying out loud that it makes the demo not a
reproduction.

---

## 4. What the language is missing, and which of it this site needs

Angle features the `codemarkup` layer uses, against focus:

| Angle | focus | does the site need it? |
|---|---|---|
| derived predicates (`P : … where …`) | **none** — `derive` parses, draws `nyi/derivation` | **yes** — B1, search index, closures |
| `nat` arithmetic (`+`) | not lexed | **yes** — span ends |
| comparisons `<`, `<=` | **lexer error** | **yes** — containment, "what is at offset N" |
| `if … then … else` | none | no — avoidable |
| sum types / unions | `nyi/union`, Phase 8.6 | only for multi-language `code.Entity` |
| `enum` | `nyi/enum` | no — a string kind works |
| `maybe` | `nyi/maybe` | no — a missing fact is the same answer |
| `bool` | not in the type model | no |
| `[T]` arrays, `set T` | `nyi/array`, `nyi/set` | only for scope search (`scope : [string]`) |
| recursion / fixpoint | none | for transitive hierarchy — or write the closure |
| `all q` + `prim.size` | none | for result counts |
| `toLower` | none | for case-insensitive search — or index it |
| `evolves` | `nyi/evolves` | not for a demo |

The type model is **three constructors away from Glean's runtime**, not eight away from
its surface ([the comparison](glean-comparison.md#the-type-model-is-narrower-than-gleans)) —
`bool`, `maybe`, `enum` and tuples are all sugar Glean lowers before storage. What is
genuinely absent and genuinely needed here is the short list at the top of that table:
derived predicates, comparisons, and arithmetic.

Note the shape of the "no" column. Almost everything the site needs and cannot say can
be **pushed into the indexer** — a second key order, a lowercase copy, a closure, a
precomputed span. That is not an accident and it is not free: each one is a decision to
make the writer hold knowledge the reader should have been able to ask for, and three of
them already exist.

---

## 5. What is already good — and one correction to the record

Stated because a gap analysis that only lists gaps is not an analysis.

- **Find-references seeks.** `src.Ref seek[to = r0.to, file = _, at = _]`. This was the
  headline blocker in [findings §11](../bench/FINDINGS.md) — a 4.9M-row scan *per
  matching declaration*, which "did not return three rows in five minutes" — and the key
  order fix landed. The plan is verified; the timing has not been re-measured, because
  that needs a re-index.
- **Reading through references costs no extra query.** `R.to.module.file` compiles to
  `fetch[r0.to]` then `fetch[r1.module]` — one point read per level, inside the same
  plan. Building an `<a href>` for every xref in a file is one query, not one query per
  link.
- **Prefix search is fast and measured.** ~6,100 q/s saturated, 0.75 ms of server CPU
  per query, p50 2.3 ms at 8 in flight; ~2,000 concurrent users at a 3-second think time
  costs 16% of one box. The search box is the one part of this site that is already a
  solved performance problem.
- **Paging is clean.** Cancel mid-result returns a tidy end and leaves the connection
  usable; ~900,000 paged queries moved RSS 756 → 797 MB and stopped. The
  abandoned-client leak that *would* have mattered for a browser tier is fixed.
- **Disjunction, negation, denial and subqueries all compile**, and `\more` in the wire
  shell holds a real cursor across a round trip — so [I4](invariants.md#i4) has an
  interactive exerciser, which [the comparison](glean-comparison.md) still records as
  missing.

### The correction: a fact's value *can* be read

[`bench/FINDINGS.md` §4](../bench/FINDINGS.md) says:

> `src.Line` is `{ file, line } -> string` and holds 8,583,810 line texts — 133 MB, the
> largest predicate in the index. **No focus query can read one.**

That is wrong. `.value` is the value side and it projects, in process and over the wire:

```
$ aperture query code '{n = D.name, k = D.value} where D = src.Decl _' --limit 5
K      N
def    key_of
class  CodecError
…

focus> :plan L.value where L = src.Line _
  r0 <- src.Line scan
  head r0.value
```

The corpus has said so since before that finding was written
(`corpus.rs`: `"X.value where X = test.Foo _"` → `Supported`, *"`.value` is the fact's
value side — Project::Value"*). What is deferred is **matching** on a value
([I6](invariants.md#i6)), not reading one. The finding appears to have tried `->`
spellings, which are indeed parse errors, and generalised.

It matters here more than as an erratum: **it is what makes screen 3 work.** Serving a
file's source text out of `src.Line` is a seek plus one value read per row, which means
the site does not need the filesystem the way Glean's demo does — and it also means F5
(a chunk has no byte budget), recorded as blocked on this, is exercisable after all.

---

## 6. A sequence, if this is wanted

**Phase A — the demo bar, with no engine work.** Everything here is schema, indexer and
a new binary.

1. **Spans.** `src.Ref.at` gains an extent; `src.Decl` gains a column and an end. (B2)
2. **`src.FileXRef { file, at, to }`** — the file-leading second copy, written by the
   indexer beside `src.Ref`. (B1)
3. **`src.SearchByLowerName`** — the case-folded search index, same shape. (§3)
4. Re-index a C# repository. `dotnet/runtime` is ~hours.
5. An HTTP tier over `aperture-client`: a recycled connection pool, three routes, and
   the hyperlink splice. (B5)

That produces file browsing, file viewing with working hyperlinks, jump-to-definition,
find-references and prefix search — i.e. the demo, and most of the product bar.

**Phase B — the parts that are the database's job.**

6. **Stream-map removal** (findings §7's costed fix). This should arguably lead rather
   than follow: a pool is unsafe without it. (B4)
7. **A cursor the client can hold**, or a key lower bound in the language. Either makes
   stateless paging possible. (B3)
8. **Comparisons and arithmetic** — the
   [primitives decision](open-decisions.md#primitives-in-the-query-language), which is
   smaller than it sounds and would be the first thing in the language to lower a
   `Step::Derive` at all. Unlocks containment, span ends and line ranges.
9. **[Phase 8b](../PLAN.md) stored derivation** — retires steps 2 and 3 and the
   `src.Implements` closure, and turns "written by hand" into a declaration. Gated on
   the [re-derivation vs I11](open-decisions.md) decision.
10. **Counts and ordering**, so the search UI can be honest about how many results there
    are and which one is best.

**Not proposed:** unions, enums, `maybe`, arrays, recursion. Each is real, none is on
the path to this site, and three of them are Phase 8.6's 29-file blast radius.

---

## 6a. What was built

Phase A, and none of it needed the engine:

| | | |
|---|---|---|
| A1 | **Spans** | `src.Ref.at` gained `length`, in the *key* so it is in the register the scan holds rather than a point read per row; `src.DeclSpan` carries where a declaration's name starts and where it ends, as a sibling in `src.TypeOf`'s shape rather than folded into an identity |
| A2 | **`src.FileXRef`** | `src.Ref` keyed `{file, at, to}`, so a file's references seek and arrive in the order a renderer splices them |
| A3 | **`src.SearchByLowerName`** | the search index case-folded, since focus has no `toLower` |
| — | **`src.DerivesFrom`, `src.AttributeOf`** | the two key orders a symbol panel wanted and `src.Extends`/`src.Attribute` answer backwards |
| A4 | **Re-index** | `dotnet/runtime`, 32,710 files, ~25M facts — up from 18.2M, which is what five more predicates cost |
| A5 | **The site** | [`aperture-viewer`](../crates/aperture-viewer), over `aperture-client` and nothing below it |

Phase B, all but the one that is gated on a decision:

| | | |
|---|---|---|
| B1 | **Stream-map removal** | a stream's task ends when its work does, the reader sweeps dead handles, and the client recycles ids. `streams_live` was already the gauge and nothing was allowed to decrement it |
| B2 | **A cursor the client holds** | `Cursor::to_bytes`/`from_bytes` and a `QUERY_PAGE` frame, so paging stops needing the connection. Guarded by taking every page on a *new* connection and comparing the concatenation against the uninterrupted result |
| B3 | **Comparisons and arithmetic** | `<`, `<=`, `>`, `>=` as residuals — a **byte** compare, since the key encoding is order-preserving ([I1](invariants.md#i1)) — and `+`/`-` as the first thing in focus to lower a `Step::Derive` at all |
| B4 | **Stored derivation** | not built, and the only item here that is *asked* to wait |
| B5 | **Counts and ordering** | `QUERY_COUNT` is the same plan with a different accumulator and never encodes a row. Ranking is the viewer's, over a bounded window, and §6b says why |

### 6b. Two things deliberately not built

**A general `ORDER BY`.** Rows arrive in key order, and any other order means either
materialising the result — an anti-pattern here, and for reasons that have not changed
— or a reverse-scan direction through `Source`, `FactStore`, the stack frame and the
resume cursor. The second is a real change to the machine's hot path and to
[I4](invariants.md#i4), and nothing on this site wants it: search results are
alphabetical and nobody asked for them backwards. What "best match first" actually
means is ranking, which is a judgement rather than an order, and the viewer makes it
over a window whose size the page states.

**Batching several questions into one request.** A symbol panel is five predicates and
five round trips, and Glean's Haxl coalesces exactly that. It is a protocol feature
with its own design questions — what a batch is if one member fails, what it does to
cancellation — and it is not on the path to a working site. Recorded, not scheduled.

### 6c. Measured, against 25,046,499 facts

`dotnet/runtime` re-indexed and sealed — 32,710 files, 25.0M facts, 3.4 GB. The
viewer serving it, page by page:

| page | time | what it did |
|---|---|---|
| file list | 1.1 ms | in memory; the listing is loaded once at startup |
| search, `jsonserializer` | 3.9 ms | 64 matches, ranked, from one prefix seek |
| search, `parse` | 50.7 ms | the wide one — thousands of matches counted, 200 read, 50 shown |
| symbol, `JsonSerializer` | 10.0 ms | declaration, span, 3,003 uses counted, 50 linked |
| file view, 908 lines | 26.6 ms | 387 cross-file hyperlinks, each resolved to its target's line |

And B1, the query the whole analysis was about, measured both ways on the same
question — every cross-reference in one file:

```
src.FileXRef      387 examined       3.5 ms
src.Ref     4,879,151 examined   1,918.6 ms   ← labelled `full scan` by the profiler
```

**546×.** The `src.Ref` number is what every file view would have cost.

Counting is worth its own line: 39,209 matches counted in 32 ms against 74 ms to
receive them — which understates it, because the receiving side was `--format count`
in the same process rather than a browser.

### 6d. The cost model bit the viewer, and it was not statement order

§3 recorded "statement order decides the plan" as a hazard with no diagnostic. The
viewer hit a **sharper** version of it, and only against the real index: its own search
page took **58 seconds**.

The query was `src.SearchByLowerName {name = "…".., to = D}; D = src.Decl {module = M}`
— seek the folded index, then read the declaration. The plan:

```
src.Decl               888,177 examined   full scan
src.SearchByLowerName  56,843,328 examined
```

**A row bind claims its variable.** `D = src.Decl {…}` says what `D` *is*, so
`flatten`'s `Claims` makes every other mention of `D` a read, and the level binding it
has to run first. That is not an ordering question and no reordering can rescue it —
`reorder` was working exactly as designed.

The fix is to read *through* the reference the seek already bound —
`{file = D.module.file}` rather than binding `D` — which is the plan the query
obviously meant:

```
src.SearchByLowerName seek[name = "…".., to = _]   64 examined
fetch src.Decl                                     64
fetch src.Module                                   64
```

**2.1 ms against 30,222 ms**, on the identical answer. Both spellings are things a
person would write, one of them is fourteen thousand times slower, and nothing warns.
That is the strongest evidence this analysis produced for a cost model — stronger than
§3's version, because it was found by a consumer rather than by inspection, and because
the trap is *shape* rather than order.

**What is done about it.** The engine now does this transformation itself:
`flatten::chasable` marks a row bind whose variable another key holds at a reference to
the same predicate, and `emit` lowers it as a fetch. The query above, **unchanged**, is
2.772 ms on the 25M-fact index — from 30,222 ms.

It is gated on two *structural* conditions rather than on statistics, and the second one
is where §6d's reasoning was wrong. Chasing is **not** unconditionally better: a bind
whose pattern fixes its key is a point seek, and running that first and splicing its id
beats scanning the referrer. So the bind must give no constant *and* the splice must not
be able to seek at the reference site. Where a splice would seek, which of the two plans
wins depends on how big the two predicates are — and that is a cost question, which is
still unanswered.

It also marks the bind *chasable* rather than rewriting it, so a chasable bind can still
run first, as the scan it always was. The first version did rewrite it, and the property
battery caught what that costs: orders that used to compile stopped. A lowering that
removes orders trades a slow query for a broken one.

Beside it, the **guard on the consumer** stays —
`no_page_reads_a_predicate_whole` profiles every query in `query::census()` and fails if
any step reports `full_scan`. It is what caught nothing in the engine and would catch the
next query written in a shape neither condition covers.

That the guard is possible at all is the useful part: `ProfileStep::full_scan` is a
property of the *plan*, so a two-file test corpus detects it exactly as a 25M-fact one
does. The check costs milliseconds and needs no benchmark. It is the mechanical guard
[CLAUDE.md](../CLAUDE.md) asks for — *"non-functional criteria are part of done, and are
tested, not asserted"* — and it was missing while the slow query was live: **every test
here passed with it in place**, because at two files both spellings answer identically.

`Paths::load` is exempt by not being in the census, and a second test says so by name
rather than leaving the omission to look like an oversight.

What this does **not** do is help anybody else. A guard in the viewer protects the
viewer; the next consumer writes the same query and gets the same 58 seconds. The engine
answer is a cost model, or failing that a diagnostic — "this level scans a predicate
while a seek was available" is knowable at flatten time, since it is exactly what
`Claims` decided. Neither is built.

### 6e. What the work turned up

Four things found by building it rather than by reading:

- **A record-valued predicate panicked the compiler.** `X.value` projects; `X.value.a`
  typechecks, because a value's type has fields now, and then flatten declined
  *quietly* — which trips its own "no plan without a reason" assertion. It is
  `nyi/value-field` now. Reachable from a schema somebody wrote, which is input.
- **The handshake fingerprint hashed traversal order.** Lowering sorts a schema's
  predicates; a hand-written client lists them however reads well. The .NET demo was
  refused against a schema it agreed with predicate for predicate.
- **A register address was a level's position**, and a derived bind takes an address
  without being a level. `level_mut` counted levels; it finds one by what it *binds*
  now.
- **A query's head record is sorted by field name at lowering**, so `{line, col,
  length}` arrives as `col, length, line`. The viewer read rows positionally until an
  end-to-end test rendered an outline with the name and the kind swapped.

### 6d. What is still missing, and now known precisely

- **A reference to a scalar-keyed predicate has no field to read through.** `R.to.name`
  works because `src.Decl` has a field called `name`; `src.File`'s key is a bare string,
  so a reference's file comes back as an id and the viewer resolves it from a map it
  loads at startup. A fetch through a reference already exists — what is missing is a
  way to *name* the whole key of the fetched fact.
- **No cost model, and now the shape of the gap is precise.** Lookup-chasing closed the
  case where the answer needs no statistics; what is left is exactly the case where it
  does. `test.Ref {of = P}` holds its reference at field 0, so binding `P` first lets the
  id lead that key's seek — one seek per referenced row — and chasing instead costs a scan
  of the referrer. Which wins depends on their sizes, and the compiler has no per-predicate
  counts to ask. Glean maintains them incrementally *and spends them on planning*, which
  this comparison already listed as a hole; this is the first place the two holes turn out
  to be one.
- **Statement order still decides the plan** where both statements are runnable, and the
  wrong order is still a three-million-row scan with no diagnostic attached.
- **Stored derivation.** Five predicates in `schemas/code.aps` are now a second key
  order over data already there. That is five apologies in five comments where there
  used to be two.

---

## 7. The three things to remember

1. **One query shape blocks the demo, and the fix is a key order that already exists in
   the other direction.** `src.Ref` cannot lead with both its target and its file. The
   database's answer to that is a derived predicate; it does not have one yet, so the
   indexer writes it — for the third time.
2. **The engine is in better shape than the record says.** Find-references seeks, values
   read, references chase, paging is clean, and search is measured at six thousand
   queries a second. Two of the three things a file view needs already work.
3. **The gaps that remain are mostly at the edges, not in the machine.** Spans in the
   schema, a stream map in the server, a cursor in the protocol, an HTTP tier that does
   not exist, and a cost model that would stop a hand-written join scanning three
   million rows because its two statements were typed in the wrong order. None of those
   is a reshape of the executor.
