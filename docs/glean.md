# Fjord and Glean

> Reference doc. **Read this before proposing a feature Glean has** — every difference
> between the two systems should be one of three things, and it should be clear which:
> adopted, a deliberate divergence, or an honest gap. An omission reads exactly like a
> decision until someone writes it down.

This file has two parts on two axes. **Part I is the lineage ledger** — for each idea, where
it came from. **Part II asks what each system can be *asked* to do, what it *spends* doing
it, and what it *charges*.** Both are **verified against Glean's source**, not its published
design — Part I against commit `95c0fb6`, Part II against `d2c8bdae` (the newer read wins
where they disagree, flagged inline). That matters: the first version of the ledger was
written from the public docs and most of its rows were in the wrong bucket, with every
correction running one way — over-claiming lineage, under-claiming our own mechanisms.
Glean's own in-tree prose contradicts its code in at least one place (`OWNERSHIP.md`
describes 64K-element blocks where `setu32.h` implements 256), so nothing here rests on its
documentation alone. Where a claim could not be read from source it is marked `[inferred]`.

---

# Part I — lineage

## 1. Adopted

The shape of the system is Glean's, and most of it is not restated here because the chapters
already are that:

- **Facts, predicates, and the key/value split** — a predicate fixes a fact's type; the key
  identifies and is indexed, the value is carried and fetched on demand
  ([chapter 1](../website/content/concepts.md)).
- **Two maps, not one** — an index from `predicate ++ key → id` and an identity map from
  `id → fact`. This is the layout the design was taken from, and the correspondence is
  literal: Glean's RocksDB column families are *named* `keys` and `entities`, with the same
  directions (`glean/rocksdb/container-impl.h:52-64`,
  `glean/rocksdb/database-impl.cpp:498-517`). Both systems therefore **store the key twice**,
  which Glean names as its main space defect (`glean/website/docs/implementation/db.md:87`) —
  see [chapter 3](../website/content/storage.md).
- **A predicate query is a prefix scan**, and seeking within one extends the prefix by whole
  leading key fields. Note what this does *not* require: an exact-prefix seek needs only a
  self-delimiting, canonical encoding — **not** an order-preserving one. That is a separate
  bet, and it is ours (§2).
- **Write once, then read forever.** Glean finishes a DB; we seal it (`Writable → Complete`,
  `ops-I2/I3`). Every cheap thing downstream — free snapshots,
  bytes-only resume, fearless parallel ingest — is bought with this.
- **Execution is a nested loop with backtracking**, one loop level per generator, and the
  generator order *is* the loop nesting (`glean/db/Glean/Query/Codegen.hs:994-1042`;
  [chapter 4](../website/content/executor.md)). Backtracking is a backward jump there and a frame pop here.
- **The store behind a seam.** Glean reaches `seek`/`next` through syscall function-pointer
  registers rather than opcodes — the exact analogue of our `FactStore` trait, and the same
  place it inserts `Stacked` and `Sliced` for incrementality.
- **Queries paginate through a continuation** rather than a held cursor
  ([chapter 5](../website/content/executor.md)). Glean's is opaque bytes handed back to the client, and so is
  ours; the divergence is what is *in* it (§2).
- **Conflict handling is a deterministic reject.** Same key, different value is an error, not
  last-writer-wins: `Define::define` returns `Id::INVALID` (`glean/rts/define.h:20-30`) and
  `defineBatch` raises "invalid fact redefinition" (`glean/rts/define.cpp:91-102`). Identical
  facts dedup in both. This was previously filed as a divergence; it is Glean's default rule,
  and `ops-I5` adopts it. What Glean does that we refuse is *disable*
  it on three paths — see §2.
- **Both maps written atomically** — Glean uses one `WriteBatch` over entities, keys, the id
  counter and stats (`glean/rocksdb/database-impl.cpp:480-537`);
  [I12](../website/content/invariants.md#i12) is the same rule.
- **String encoding**, in detail: NUL-escaped, terminator-delimited, byte-lexicographic, and a
  string prefix seek built by *dropping* the terminator
  (`glean/rts/string.h:16-26`, `glean/db/Glean/Query/Codegen.hs:1278-1284`). This is the one
  place the two codecs genuinely agree ([chapter 2](../website/content/storage.md)).
- **Cost tiers as the ranking for reordering** — point match before prefix match before scan,
  with omitted key fields treated as wildcards that close the prefix. This much of Glean's
  `Reorder` we do share; the algorithm around it we do not, and the earlier claim that we use
  its topological sort and antichains was false in both directions (§2).
- **Cancellation counted in rows *examined*** — Glean checks every 100th call to `next`
  (`glean/rts/query.cpp:26-28,304-319`), which is the conclusion
  [the settled record](../PLAN.md#settled-decisions--recorded-so-they-are-not-reopened) records for our executor, reached independently.
- **Derived predicates**, on-demand and stored. Two of the mechanisms we cite are real:
  `DerivedFactGenerator` (`glean/db/Glean/Query/Codegen/Types.hs:158`) and `DerivedAndStored`
  (`glean/angle/Glean/Angle/Types.hs:619`, spelled `stored`). Three corrections:
  `Derive when` is a Haskell constructor pattern, not Angle syntax; there are **three** modes,
  not two (`DeriveOnDemand | DerivedAndStored | DeriveIfEmpty`); and **`captureKey` is not a
  derivation mechanism at all** — it is the trick that rewrites `X = pred pat` so the *client*
  needs no second fetch (`glean/db/Glean/Query/Flatten.hs:549-586`), which we do not need
  because [I5](../website/content/invariants.md#i5) already puts the whole row in the register. The seam split —
  dynamic (built) versus stored ([the roadmap](../PLAN.md#stored-derivation)) — *is* Glean's seam, and
  `ops-I8` matches its documented derive-before-finish ordering
  almost verbatim.
- **A self-describing DB** — the schema travels with the data, and identity is a fingerprint
  over a canonical form. Glean's `SchemaId` is a hash over a sorted name environment, which is
  structurally our qualified-name → predicate-fingerprint map. One difference matters and is a
  constraint on our canonical form: **Glean's per-predicate hash is a Merkle hash**, so a
  change propagates transitively through every referring predicate
  ([chapter 6](../website/content/schema-language.md)).

---

---

## 2. Deliberate divergences

| Dimension | Glean | Fjord | Why, and where it is recorded |
|---|---|---|---|
| **Deployment** | A Thrift service holding many DBs; clients are remote | **Embedded library first**, with a wire protocol added for a server that reuses the same executor | The executor consumes a `(handle, snapshot)` and assumes no connection ([chapter 3](../website/content/storage.md), [operations](../website/content/operations.md)) |
| **Query execution** | Angle compiles to **bytecode for a query VM** (C++) — a flat 60-instruction register machine | A **defunctionalised abstract machine** — an explicit frame stack, no bytecode | [I7](../website/content/invariants.md#i7). The reason is *not* that an interpreter's stack would have to be saved: Glean's VM has **no call stack** ("we don't have a stack (yet)", `glean/db/Glean/Query/Codegen.hs:573-576`). It is that Glean's continuation carries the **entire bytecode program**, PC, literals, every register and every output buffer, plus a second `traverse` subroutine (`glean/rts/bytecode/subroutine.cpp:370-381`) — and is **version-locked** to the bytecode ABI, so any bytecode change invalidates every in-flight continuation ([chapter 4](../website/content/executor.md)) |
| **Resume state** | A continuation is opaque client-held bytes — self-contained down to the program, resumable in another process | A **bytes-only `Cursor`**: one detached row per open level, ~two orders of magnitude smaller | [I4](../website/content/invariants.md#i4). Both are client-held; the divergences are *size*, the absence of an ABI lock, and the **verification direction** — we re-seek the saved key and check the fact id, where Glean maps id → key and carries no `Repo`, so a continuation replayed against the wrong DB silently resumes at the wrong row ([chapter 5](../website/content/executor.md)) |
| **Query isolation** | **No snapshot at all** — fresh iterators at whatever LSM version is current, even *within* a page (`GetSnapshot` appears nowhere in `glean/rocksdb/`) | One immutable snapshot per query, **released at every suspend** | [I8](../website/content/invariants.md#i8). Stronger than Glean in both directions: a real per-query view that Glean lacks, which nonetheless pins no LSM generation while a portal idles ([chapter 5](../website/content/executor.md)) |
| **Key encoding** | **Not order-preserving.** Fact keys use LEB128 (`255` = `FF 01` sorts before `256` = `80 02`, `glean/rts/bytecode/subroutine.cpp:33-37`). Glean *has* an order-preserving varint (`glean/rts/nat.h:20-64`) and uses it only for storage-level keys | An **FDB-inspired order-preserving tuple codec** with a frozen marker table | [I1](../website/content/invariants.md#i1)–[I3](../website/content/invariants.md#i3). A real divergence, not shared ground — and one whose payoff is still **deferred**: order-preservation buys value-range scans, and no ordering operator exists yet ([chapter 2](../website/content/storage.md)) |
| **Self-delimiting bytes** | **Untagged and positional** — records are bare concatenation; skipping a field is schema-driven codegen (`glean/hs/Glean/RTS/Traverse.hs:27-119`) | Marker-tagged; `skip` needs no schema | [I2](../website/content/invariants.md#i2). Glean shows the hot-loop argument is the weak one — with per-query codegen, tags are pure overhead. The real value is schema-free tooling, golden-byte tests, and the byte-level `Int`/`Fact` distinction ([chapter 2](../website/content/storage.md)) |
| **Values in the scan loop** | **Fetched during a scan** — a non-wild value chunk sets `needs_value` and the scan does a second store lookup per row (`glean/db/Glean/Query/Codegen.hs:1009,1088`) | Values never enter the scan hot loop | [I6](../website/content/invariants.md#i6). Ours, not adopted. Affordable partly because value patterns are deferred (`nyi/value-match`) ([chapter 3](../website/content/storage.md)) |
| **Union discriminants** | **List position** — no discriminant syntax exists (`glean/angle/Glean/Angle/Parser.y:306-309`); stability comes from remapping alternatives **by name** at query time, with a synthetic `unknown` (`glean/db/Glean/Query/Transform.hs:551-579`) | Explicit, assigned-once, append-only tags | [I10](../website/content/invariants.md#i10). Append-only tags are not "the only safe scheme" — they are the only safe scheme **without** a query-time transform layer, which [I13](../website/content/invariants.md#i13) declines ([chapter 6](../website/content/schema-language.md)) |
| **Fact ids** | A **dense** monotonic id space per DB — `glean/rts/lookup.h:92-99` says ids "are supposed to be dense", and five subsystems depend on it: substitution vectors, `FactSet` indexing, Elias-Fano ownership sets, the `factOwners` interval map, and the `id < mid` stacking test | A **snowflake**: 24-bit predicate tag + 40-bit per-predicate sequence | [I11](../website/content/invariants.md#i11). Density is load-bearing in Glean far beyond stacking, so this costs more than the first version of this file said — but *within* a predicate our ids are dense, so each predicate is the same dense-map shape, and only a fact set **spanning** predicates degrades. In exchange: Glean has **no concurrent writer** at the storage layer and buys parallelism back with its whole rebase/substitution subsystem, where two of our ingest workers on different predicates share no counter ([chapter 3](../website/content/storage.md)) |
| **Scan order** | **No guarantee** — "in no specified order" (`glean/rts/lookup.h:125-127`); Glean *removed* its reliance on ordered iteration to support limited key sizes (`db.md:143`) | Lexicographic order is depended on absolutely | [I1](../website/content/invariants.md#i1). Ours is the stricter commitment, and it has a price: it forecloses the key truncation Glean adopted, and we carry no key-size budget or degradation path ([chapter 3](../website/content/storage.md)) |
| **Physical layout** | One store per DB; the predicate is an 8-byte key prefix with a fixed-prefix transform | **One keyspace pair per predicate** (`keys.<id>`, `entities.<id>`) | Physical isolation makes bulk ingest embarrassingly parallel, and fjall's `ingest()` wants strictly ascending keys. Two consequences we get free: no predicate id in every `entities` row, and no stats column family in the write batch to answer `count(pid)` ([chapter 3](../website/content/storage.md)) |
| **Schema versioning** | **Edit in place with an automatic per-predicate transform** triggered by a hash mismatch (`glean/db/Glean/Database/Schema.hs:640-657`). `schema all.N` is the *name-resolution scope*, not the version axis; `evolves` is the manual path and only takes effect when **no facts** of the old schema exist | **One schema per DB, embedded and frozen at create**; compatibility is subset containment | [I13](../website/content/invariants.md#i13). Glean's own escape hatch for a breaking change is "bump the version and treat it as separate" or "produce two DBs" — which *is* our default workflow, so the freeze promotes Glean's fallback to a rule ([chapter 6](../website/content/schema-language.md), [the roadmap](../PLAN.md)) |
| **Reorder algorithm** | Transitive **lookup-chasing**, then greedy tier selection over a ranked cost lattice, then a **separate feasibility pass that can give up** (`glean/db/Glean/Query/Reorder.hs:420-430,539-544,575-639`). The heuristic is key-prefix boundness, **not** cardinality — `PredicateStats` is never imported by `Reorder.hs` | A greedy **runnable frontier**, **provably complete**, with no cost model yet | [chapter 7](../website/content/query-language.md). Neither side does topological sort or antichains; the earlier claim was false in both directions. Completeness is ours to claim — Glean's pass can fail to order a satisfiable query |
| **Incrementality — stacking** | **Stacked DBs**: compose two DBs by fact-id range (`glean/rts/stacked.h:20-144`). Needs no ownership | **None** | `ops-I9`. The cheaper half, and the one the snowflake could carve for; the seam that would carry it is `FactStore::{scan, point}` — exactly where Glean puts `Stacked` |
| **Incrementality — ownership** | **Ownership sets**: per-fact set expressions letting a delta *hide* base facts, ~7% of DB size, checked as a **per-row filter** on every iterator (`glean/rts/ownership/slice.h:167-233`) | **None** | `ops-I9`. The expensive half, and the one to keep declining: the filter is literally our [I6](../website/content/invariants.md#i6)/[I9](../website/content/invariants.md#i9) anti-patterns, propagation is O(facts) in time *and* space, and it **bans negation in stored derived predicates** purely for invalidation cost. It is also Glean's **authorization** substrate, which ties this row to `ops-I10` |
| **Ingest** | JSON/binary batches over Thrift; a binary `Batch` is one opaque sequential blob and is **not splittable** — parallelism is *across* batches | **Fact files** with sync markers, chunk-split and merged in parallel, and a **single write funnel** every writer passes | `ops-I4/I5`. The argument is Glean's own: it must set `ignoreRedef` on three paths, with the source comment *"we are ignoring actual errors and silently picking one of the two facts… That's bad, but I don't see an alternative"* (`glean/db/Glean/Write/SendAndRebaseQueue.hs:408-426`) — first-writer-wins, order-dependent, and exactly what reproducibility forbids |
| **Negation** | Angle's `!` is *written* as pattern syntax but is unit-typed and **desugars to a negated statement group** (`glean/db/Glean/Query/Typecheck.hs:626-654`); placement is forced by an `Ordered`/`Floating` tag plus a reorder rule | sigla negates a **statement**, compiled to a `Step::Test`; placement is forced by the **reads edge** the frontier already had | Both negate statements, so the level is not the divergence — what differs is that Angle can write `(!A) | B` (only bare `!A | B` fails, on precedence), and that Glean needs a tag where `reads` = "the variables it names, captures nothing" says the same thing to a frontier that only ever runs what is runnable. Glean's rule is *adopted*, verbatim in effect: negations run after their parent-scope binds "to ensure consistent semantics regardless of order" (`glean/db/Glean/Query/Reorder.hs:547-573`). Still ours to build: a negated **group**, which needs a level inside a test (`nyi/negation`) |
| **Cancellation** | **Global only** (`interruptRunningQueries`) | A per-query cancellation token | [chapter 5](../website/content/executor.md). Small, and ours |
| **Diagnostics** | One `Doc ()`, fail-fast on the first error, no codes, a text location prefix — and `Reorder.hs` can show a user flattened IR they never wrote | A closed `Code` enum, an accumulating sink, rendered source spans, and corpus entries asserting exact code sets | [testing.md](../website/content/testing.md). Not a Glean idea, and the clearest place we are simply better |
| **Design method** | — | An **invariant registry** with a numbered guard test each, and non-functional claims held by *mechanical* guards (allocation counters, decode probes, drop probes) | [testing.md](../website/content/testing.md), [invariants.md](../website/content/invariants.md). Glean's equivalent for the same properties is a header comment and code review — its resume safety rests on a hand-maintained "don't keep pointers in registers across `Suspend`" rule with a live workaround |

---

---

## 3. Not decided — the honest gaps

### The type model is narrower than Glean's

Glean's **runtime** has eight type constructors — byte, nat, string, array, tuple, sum, set,
predicate reference (`glean/hs/Glean/RTS/Types.hs:185-194`). `bool`, `maybe`, `enum`, tuples
and named types are **sugar lowered before storage** (`glean/.../Schema/Util.hs:35-62`), so the
surface list overstates the distance: `PredicateTy`'s five constructors — `Int`, `Str`,
`Fact`, `Record`, and (since unions landed) `Union` — are **three** away from Glean's runtime
(byte, array, set), not eight away from its surface. Two things the earlier version of this
row missed: **`set T` is a real Glean type** (only 7 uses in all of Glean, so separately
deferrable), and Glean has **no signed integer** — a place we are *wider*.

The codec has reserved marker bands in the right sort positions
([chapter 2](../website/content/storage.md)), so most of these are not a one-way door *in
the encoding*. **Arrays are the exception**, and it is a seekability exception rather than a
band-allocation one: a length-prefixed array cannot be prefix-matched, which Glean states
outright — *"MatchArrayPrefix doesn't actually look at a prefix because arrays encode their
length at the front"* (`glean/db/Glean/Query/Reorder.hs:794-796`).

**Multiplicity** is the decision with a deadline, and the framing it was opened with is a false
binary: **Glean does both, deliberately, for the same data** — a compact array-bearing fact,
then a `stored` derived predicate that explodes it with `[..]` to get the seekable index. See
[the settled record](../PLAN.md#settled-decisions--recorded-so-they-are-not-reopened) for the
evidence.

**Status: decided — one fact per element, `nyi/array` naming the decision — and the
constraint stands: arrays before stored derivation ship the storage win and none of the
query mitigation.**

### Recursion — Glean has it, we do not

Previously recorded as a shared decision ("both decline it"). **False.** Glean has an opt-in
**semi-naive fixpoint** behind `--experimental-recursion`: `calling` returns a plain fact seek
instead of expanding (`glean/db/Glean/Query/Flatten.hs:296-308`) and the body is wrapped in a
loop that runs while `firstFreeId` grows (`glean/db/Glean/Query/Codegen.hs:1412-1465`), tested
on transitive closure. Its SCC rejection comments that "this is a constraint we will remove in
the future".

Declining it here is still right, but it must be recorded as *our* choice and as the one item
on this page that is a genuine **machine reshape**: the loop is driven by facts being *written*
mid-query, and `enumerate` has neither an arm that re-runs the body nor a write path, and
holding state across iterations conflicts with [I8](../website/content/invariants.md#i8).
**Status: undecided, and expensive.**

### Primitives, expressions and aggregation — mostly closed

Order comparisons and arithmetic are in the language. A comparison is a residual and a
**byte** compare, since the key encoding is order-preserving
([I1](../website/content/invariants.md#i1)) — which is wider than Angle, whose comparisons are nat-only;
strings compare here for the same reason integers do. Arithmetic is `+` and `-` on
integers, wrapping, and it is the first thing in sigla to lower a `Step::Derive` at all.

**Aggregation is still a Fjord-only absence, and the shape of the absence changed.**
There is no `count` in the language and no `all q` to build a set from. What there is
instead is `QUERY_COUNT`: the same plan with a different accumulator, counted at the
consumer, which is where this design has always put aggregation — the difference is
that it no longer costs a full result over the wire. `prim.size (all …)` as a *term*
still needs set construction.

What remains absent: **if-then-else**, and a **sargeable** comparison. The second is
worth knowing about — an order comparison on a leading key field denotes one contiguous
run of the key order, unlike a denial, so unlike `NotPrefix` there *is* a seek form to
look for later. **Status: built, except aggregation-as-a-term.**


### Missing compiler stages

Glean has an **`Opt` stage** with no counterpart here — and the reason it needs one is
instructive. Its flattener emits a statement for *every* read (a field select becomes a record
destructure, a deref becomes a fact lookup), so unification and substitution exist to remove
the redundancy that uniformity creates; where the pass fails to fire, `Codegen` rebuilds the
term into a buffer and matches it byte-wise, per row. We substitute a **location** instead of a
term — `flatten::Slot` is `optSubst` with the runtime cost removed — so the redundancy is never
generated and there is nothing for a pass to undo. Two of `Opt`'s jobs *were* worth taking, and
have been: **statement decomposition** (`expandStmt`'s `{A,B} = {C,D}` → `A=C; B=D`, here as a
record pattern destructuring any slot, with the trivial leaves never built rather than built
and dropped) and the reach of substitution *through a record*, which a constant-only fold could
not do. **Lookup-chasing is now built**, and it was the item on this list that cost
the most: a row bind whose variable another key holds at a reference to the same predicate
is lowered as a `Source::Fetch` rather than a level, which took a real query from 30,222 ms
to 2.772 ms on a 25M-fact index. Two *structural* conditions gate it — the bind must give
no constant anywhere, and splicing the id at the reference site must not extend that key's
seek — so it needs no statistics and makes no judgement about sizes. And it marks the bind
*chasable* rather than rewriting it, so the bind can still run first as the scan it was and
no order that compiled before stops compiling.

What remains genuinely absent: a **cost model** — which is the other half of the same
story, since chasing declines exactly the cases where the answer depends on how big two
predicates are — and `Prune`'s empty-predicate short-circuit, which a sealed DB could
answer *exactly* rather than approximately. `Ordered`/`Floating` statement tags
are present (`reorder::Placement`) and carry **no ordering rule at all** — the rule they were
kept for — negation placement — turned out not to need them: a negation captures
nothing, so every variable it names is a `read`, and a frontier that only runs what is runnable
cannot place it early. Glean's own use of the tags — floating statements first — would break
the claim that a nested pattern and its two-statement spelling compile to the same plan, so it
stays declined. What the tag is for here is `preserves_written_order`, a property rather than a
rule. **Generator synthesis for an unbound variable does not port at all**: it fires in Glean
because prim args, `all`, `if`/`then`/`else` and or-branches can mention a predicate-typed
variable no generator binds, and in sigla a `Fact` type can only come from a fact pattern
(which binds it) or a fact-typed key field (which captures it), so the precondition never
holds. [Chapter 7](../website/content/query-language.md).


### Operational capabilities

Separating what is inherent to being a *service* from what is a real hole. Inherent, and
therefore not ours to want: the janitor, sharding, ACLs, async write handles, remote backup
scheduling. **Genuine holes:** no provenance on a DB (Glean's identity records where it came
from); no database properties; **no at-rest validation** — Glean's `Validate` runs six checks,
two of which are literally [I1](../website/content/invariants.md#i1) (enumeration order) and
[I12](../website/content/invariants.md#i12) (`idByKey` agreement); no per-predicate stats, which Glean maintains
incrementally for an O(1) read *and spends on planning*, and which per-predicate keyspaces make
nearly free here; no retention policy. **`:more` is closed** — the wire shell holds a real
cursor across a round trip, and the cursor crosses the wire as bytes, so
paging survives the connection that started it, which is more than Glean's continuations do for a
stateless caller.
[Operations](../website/content/operations.md).

### The idiomatic spelling of a join — closed

In Angle, nested fact patterns — `Knows { from = Person { id = 1 }}` — are *the* way one writes
a traversal. It was parsed, typechecked and deferred in three pieces of very different
size; **all three are built**, so the
nested spelling now compiles — to the *same plan* as the two-statement form, which is the sense
in which it is a spelling rather than a second way to run a query.

Fjord and Angle agree on the mechanism, too: a reference is followed by its **id**, so a
join through one reads no second fact. Reading *through* a reference — a field or value of the
fact it names — is the other half, and a lookup rather than a compare; Angle does it freely, and
Fjord now does it as a
[`Source::Fetch`](../website/content/executor.md) level, one point read per row of
the level above it. The two halves stay distinct in the IR on purpose: the cheap one is a
compare against an id already in a register, and conflating them is how a key gets spliced where
an id belongs. What is left deferred is narrow — a reference held in a fact's *value*
(`nyi/fact-field`), where the id is not in a register's key bytes to read.

---

---

## 4. Where we are ahead, and had not said so

Collected because the first version of this file claimed almost none of it, and because three
of these are places Glean's source shows the cost of *not* having done it.

- **Non-functional invariants held mechanically** — allocation counters, decode probes, drop
  probes, with positive controls. Glean binds eagerly (word fields decoded into registers,
  non-word fields memcpy'd into a buffer *before* the inner loop) and reallocates its output
  buffer per row past a 23-byte small-string threshold. One caveat we should own: our
  allocation guard is **single-level**, and a join allocates per outer row.
- **Values out of the scan loop** ([I6](../website/content/invariants.md#i6)) and **an immutable per-query
  snapshot** ([I8](../website/content/invariants.md#i8)) — Glean has neither.
- **Snapshot release is structural**, because `enumerate` consumes `self`; Glean's equivalent
  is a comment.
- **Resume verification catches a mismatched DB**; Glean's direction does not, and its
  result-dedup set is per-page and absent from the continuation, so a paged Glean query can
  return **cross-page duplicates** — a live violation of the property
  [I4](../website/content/invariants.md#i4) exists to pin.
- **A provably complete reorder**, a **per-query cancellation token**, and **signed integers**.
- **Diagnostics and literal hygiene** — Angle silently wraps nat overflow and silently takes
  the first of duplicate fields; both are rejected here with codes. Angle's own guide shows an
  inference failure that sigla handles with no annotation.
- **Concurrent per-predicate writers**, and a high-water mark recovered from data rather than
  a persisted counter that can go missing.
- **"Deferred" as a first-class executable category** — no unimplemented feature may surface as
  a syntax error, with corpus entries pinning exact rows against a real DB. Nothing in Glean
  ties surface, diagnostics and answers together in one table.

---

---

# Part II — capabilities, efficiency, and what a query costs

Three questions with different answers. On capability we are narrower and know it. On
efficiency the picture is mixed. On the cost model neither system estimates anything, and the
two have made *opposite* choices about where to put the safety.

## 0. The question that opened this file: fact ids across databases

*How does Glass handle fact ids when results come from different DBs?*

**It doesn't — because it never lets a fact id out.** The answer has four parts, and each one
is a design decision we can copy or decline independently.

### A Glean fact id is less self-describing than ours, not more

`Id` is a bare `uint64_t` with no structure at all — no predicate tag, no database tag
(`glean/rts/id.h:19-22,140-146`). `0` is invalid and `1024` is the first assignable id;
everything else is a dense per-database sequence. The predicate is not *in* the id, it is
*looked up* from it (`typeById`, `glean/rts/lookup.h:83`). So an id is meaningless without
knowing which database issued it, and there is nothing in the bits to tell you that you got
it wrong. Our snowflake — 24-bit predicate tag + 40-bit per-predicate sequence
([I11](../website/content/invariants.md#i11)) — at least catches a predicate mismatch.

### The public identity is a string, derived from content, with the repo as its first token

Glass's client-facing name for a thing is a `SymbolId`: URI-encoded tokens joined by `/`,
built as `repo / language-short-code / entity-encoding…`
(`glean/glass/Glean/Glass/SymbolId.hs:125-140`). `www/php/Glean/getLatestRepo` is the
worked example in the source comment. It is reversible by *searching*, not by dereferencing:
`symbolTokens` splits it back into `(RepoName, Language, [Text], [SymbolFilter])`
(`SymbolId.hs:185-198`) and `searchEntityLocation` dispatches on the language to a
per-language Angle query that finds the entity again by name
(`glean/glass/Glean/Glass/Search.hs:47-95`).

The comment on that function is worth reading in full, because it is an admission we would
have to make too: *"this is different to e.g. approximate string search, as we should _always_
be able to decode valid symbol ids back to their (unique\*) entity"* — and then lists three
ways the uniqueness fails (weird code, Hack namespaces, bugs in the encoder), with duplicates
logged to an error stream. **A content-derived name is stable across databases and
occasionally ambiguous; a fact id is exact and database-local.** Glean picks stability and
pays for the ambiguity with a log line.

### Where an id *must* survive a database boundary, origin travels beside it — explicitly

This is the part that answers the open question — whether a cross-database result can hide
which database each row came from ([open decisions](../PLAN.md#settled-decisions--recorded-so-they-are-not-reopened)).

```haskell
data SearchEntity t =
  SearchEntity {
    entityRepo :: !Glean.Repo, -- vital to know which repo this came from
    decl :: !t
  }
```
`glean/glass/Glean/Glass/Search/Class.hs:63-67` — **Glean's own comment**, not a gloss.

And every follow-up query is re-scoped to that repo before it runs:
`withRepo (entityRepo this) $ …` appears at
`glean/glass/Glean/Glass/Handler/Symbols.hs:150,177,182,215,644` and
`glean/glass/Glean/Glass/Neighborhood.hs:63`. Where raw ids have to be carried across a
boundary at all, Glass builds an explicit side map from id to origin —
`Map (Glean.IdOf Src.File) (RepoName, Path)`, `glean/glass/Glean/Glass/Digest.hs:48-60`.

The scoping is enforced by the **type system**, not by discipline. `RepoHaxl` carries a
`HasRepo` constraint (one repo); `ReposHaxl` carries `QueryRepos` (many). The `HasRepo`
instance for the multi-repo environment exists but is deliberately **not exported**, with the
reason in the source: *"its not exported so the user doesn't accidently run a query over only
the first repo"* (`glean/client/hs/Glean/Haxl/Repos.hs:45-52`). A query that forgets to say
which database it means does not compile.

### The union is three different policies, chosen per call site

There is no single cross-database query. There is a fan-out combinator and three ways to
merge:

| Policy | Definition | Semantics |
|---|---|---|
| `queryAllRepos` | `concat <$> queryEachRepo act` (`Repos.hs:87-90`) | Plain concatenation, database order |
| `takeFairN` | `take n (concat (List.transpose xs))` (`glean/glass/Glean/Glass/Utils.hs:145-155`) | **Fair round-robin interleave** — the doctest is `takeFairN 3 [[1,2],[3],[4,5]] == [1,3,4]` |
| `firstOrErrors` | `glean/glass/Glean/Glass/Handler/Utils.hs:83-96` | First success wins; all failures collected; returns `FoundSome (repo :| rest)` so the answer records **which** databases answered |

`searchReposWithLimit` applies the limit **twice** — once per database inside the fan-out and
again after the merge (`Utils.hs:128-140`) — and carries the comment *"we would like this to
be concurrent"*, so the fan-out is sequential Haxl rounds today.

Note that `takeFairN` is the same instinct as our `outbound` writer: when several producers
feed one consumer, interleave rather than concatenate, or the first producer starves the rest.
Glean reached it for search relevance, we reached it for latency
([§2.9](#29-fairness-and-scheduling)); it is the same fix.

### Stacking is not an answer to this, and cannot be made into one

`Stacked` composes two databases at the `Lookup` seam by fact-id range: `mid` is the upper
database's `startingId()`, and every operation is `id < mid ? base : stacked`
(`glean/rts/stacked.h:20-144`). That only works because the delta's ids were allocated
*above* the base's — and that is decided at **create** time, by reading the base's
`firstFreeId` (`glean/db/Glean/Database/Create.hs:220`, reached only via
`Dependencies_stacked`/`Dependencies_pruned`, `Create.hs:73-90`).

**Two independently-built databases can never be stacked.** For a database-per-CI-run — the
case that motivates wanting this at all — stacking is structurally unavailable in Glean too, no
matter how much of its machinery one ported.

### What this settles for us

- **Origin cannot be hidden, and Glean's source is the argument for it.** Not from a consumer
  that will *use a result again* — expand a reference, compare two rows, deduplicate. Glean
  carries it per result, in the type, and annotates the field "vital".
- **What Glean does hide is fact ids** — by never exporting them across a boundary at all. Its
  cross-repo union is safe under plain `concat` *because* the things being concatenated are
  content-derived strings, not database-local numbers. That is the trade: if we want a union
  whose rows are freely comparable, the thing in the row cannot be a `FactId`.
- **A fan-out needs a merge policy, not a merge.** Glean has three, picked per call site. Ours
  would need at least the fair interleave, since concatenation across forty CI databases means
  the first one supplies every row of the first page.
- **`ops-I9` stays intact.** Nothing above needs a layer dimension in `Access` or a layer tag
  in `Cursor`. Every mechanism in this section sits *above* the executor, in the service that
  owns several database handles — which is the one design constraint to carry forward
  ([open decisions](../PLAN.md#settled-decisions--recorded-so-they-are-not-reopened)).

---

---

## 1. Capabilities

Marked from source on both sides. "—" means absent, not merely unused.

### 1.1 Query language

| Capability | Glean | Fjord | Notes |
|---|---|---|---|
| Conjunction, disjunction | ✅ | ✅ | Both compile `\|` without DNF expansion |
| Negation of a statement | ✅ | ✅ | Glean desugars pattern-`!` to a negated group (`Typecheck.hs:626-654`); ours is a `Step::Test` |
| Negation of a *group* | ✅ | — | `nyi/negation` |
| Union types, alternative match and select | ✅ positional tags, remapped by name at query time | ✅ explicit append-only tags | The divergence is the discriminant scheme — Part I |
| Denial of a value shape | generic `!=` (`PrimOpNeExpr`) | ✅ `!=` incl. prefix | Ours is a fifth statement kind with its own residuals |
| Nested fact patterns (join spelling) | ✅ | ✅ | Same plan as the two-statement form |
| Read *through* a reference | ✅ | ✅ `Source::Fetch` | One point read per row of the level above |
| Order comparisons | nat only | **all types** | Ours is a byte compare, sound because [I1](../website/content/invariants.md#i1) |
| Arithmetic | `+` on nat (`PrimOpAddNat`) | `+`, `-` on int, wrapping | Ours is the first producer of `Step::Derive` |
| Sets / `all q` | ✅ — 9 runtime syscalls (`query.cpp:612-620`) | — | See [§1.4](#14-aggregation) |
| Arrays, array element generator | ✅ | — | Not prefix-matchable in Glean either (`Reorder.hs:794-796`) |
| if-then-else | ✅ | — | |
| String prefix match | ✅ | ✅ | The one place the codecs genuinely agree |
| `toLower`, `reverse`, `concat`, `zip`, `length`, `size` | ✅ (15 prims total) | — | Full list in the footnote[^prims] |
| Recursion / fixpoint | ✅ behind `--experimental-recursion` | — | A genuine machine reshape for us |
| Order within a prefix scan | **none** — `seek` returns each key *"in no specified order"* (`lookup.h:126`) | lexicographic, depended on absolutely | Glean orders `enumerate` by id but not `seek` by key; ours is the stricter promise, and it forecloses the key truncation Glean adopted |
| Reverse iteration | ✅ `enumerateBack` (`lookup.h:113-124`) | — | We have no descending scan at the seam |

[^prims]: `PrimOpAddNat`, `Concat`, `GeNat`, `GtNat`, `LeNat`, `LtNat`, `NeNat`, `NeExpr`,
`Length`, `Size`, `Reverse`, `ToLower`, `Zip`, `RelToAbsByteSpans`, `UnpackByteSpans`
(`glean/angle/Glean/Angle/Types.hs`). Two of the fifteen are code-search-specific byte-span
helpers, which is worth noticing: Glean's primitive set is not a general expression language
either.

### 1.2 Types

Glean's runtime has eight constructors — byte, nat, string, array, tuple, sum, set, predicate
reference (`glean/angle/Glean/Angle/Types.hs:399-421`); `maybe`, `enum` and `bool` are sugar
lowered before storage. Ours has five (`Int`, `Str`, `Fact`, `Record`, `Union`). Glean has
**no signed integer**, so we are wider there. Its typechecker has **row polymorphism** — `HasTy` constrains
a variable to any record or sum containing at least the named fields
(`Types.hs:424-429`) — which is a real inference capability we have no analogue for.

### 1.3 Answering, paging, and inspection

This row is much closer than the language rows, and in two places we are ahead.

| Capability | Glean | Fjord |
|---|---|---|
| Paging by opaque client-held token | ✅ `UserQueryCont` | ✅ `Cursor` |
| Token survives the connection | ✅ | ✅ |
| Token verified before use | version + **self-checksum only** — see [§2.8](#28-what-a-resume-costs) | layout version + **plan fingerprint** |
| Count without shipping rows | ✅ `omit_results` (`glean.thrift:471`) | ✅ `QUERY_COUNT` |
| Compile without running | ✅ `just_check` (`glean.thrift:478`) | ✅ `:plan` / `:type` |
| Show the IR / compiled form | ✅ `debug.ir`, `debug.bytecode` (`glean.thrift:484,487`) | ✅ `:plan` |
| Per-predicate rows examined | ✅ `collect_facts_searched` (`glean.thrift:463`) | ✅ `iter::Profile` — per *step*, which is finer |
| Recursive expansion of references | ✅ **server-side**, 3 modes | ✅ **client-side**, depth-bounded |
| Expansion scoped by predicate | ✅ `expand_predicates` (`glean.thrift:475`) | — (we scope by depth) |
| Result de-duplication | ✅ by fact id — but **per page only** | — see [§1.3a](#13a-why-we-cannot-deduplicate-yet-and-what-would-let-us) |

Glean can dedup because a query's result is always facts *of one predicate*, so there is an id
to key on (`results_added`, `glean/rts/query.cpp:182,240`). Its version is also a live bug — the
set is per-execution and absent from the continuation, so a paged query can return the same fact
on two pages.

### 1.3a Why we cannot deduplicate yet, and what would let us

**This is a real gap, not a difference in kind.** An earlier draft of this file recorded it as
"not applicable, there is no id to dedup by"; that was wrong, and the reasoning matters because
the obvious fix does not work.

**Deduplicating on the witness tuple is provably a no-op.** An emitted row's witness is the set
of `FactId`s bound by the plan's [`Step::Level`](../website/content/executor.md)s — the only steps that bind a
row, since a `Derive` is pure and recomputed and a `Test` binds nothing. `enumerate` advances
every level monotonically over a sorted scan, so **no witness tuple is ever visited twice in a
run**. A set keyed on the id product would allocate per row and reject nothing.

**Duplicates come from the projection collapsing distinct witnesses.** Two witnesses that differ
only in a field the head does not project give the same row. That is every semi-join question a
code-search page asks — *which files reference this symbol* answers one row per reference, not
per file.

**The obstacle to deduplicating on the projected row is resume, not identity.** A set of emitted
rows must survive a suspend or paging returns cross-page duplicates — precisely Glean's bug
above. Putting the set in the `Cursor` makes the cursor O(distinct rows so far), which destroys
the property [chapter 5](../website/content/executor.md) is built on: one detached row per open level, bytes only.

**The way out is adjacency, and [I1](../website/content/invariants.md#i1) already paid for it.** The output stream
is lexicographically ordered by the concatenation of the levels' key orders in nesting order. If
the projected fields are a **prefix of that order, every duplicate is adjacent**, and
adjacent-duplicate elimination needs exactly one row of state — which fits the cursor with no
change to its shape.

The condition is decidable at plan time, and it covers the case that matters: project the outer
level's leading key fields and nothing from the levels below, and the inner level's
multiplication collapses. What it does not cover is projecting a *non-leading* key field —
`name` where the key order is `(file, name)` — whose values are not monotone along the stream
and which genuinely needs the unbounded set.

So the shape to build is a `distinct` that **compiles under the prefix condition and is refused
with a diagnostic otherwise** — permissive grammar, narrowed at flatten, with a code and a
corpus entry ([chapter 7](../website/content/query-language.md)) — rather than one that silently falls back to an
unbounded set. `QUERY_COUNT` with distinct is the same mechanism and free under the same
condition. One refinement to leave for later: a field functionally determined by a join key is
monotone along the stream even when it is not a syntactic prefix, so the condition can be
relaxed without touching the cursor.

### 1.4 Aggregation

Glean: `all q` builds a set, `prim.size` counts it, and sets are a first-class runtime type with
nine dedicated syscalls and a `maxSetSize` budget (`query.cpp:612-620,563`).

Fjord: no set construction, no `count` in the language. `QUERY_COUNT` runs the same plan with
a counting accumulator, so a count costs no rows over the wire — but `prim.size (all …)` as a
*term inside a query* has no spelling. This remains the clearest single capability gap.

### 1.5 Lifecycle and operations

| Capability | Glean | Fjord |
|---|---|---|
| Multi-database server | ✅ | ✅ `registry` |
| Cross-database query | ✅ *in the service layer only* (§0) | — |
| Incremental: stack a delta on a base | ✅ but **create-time only** (§0) | — |
| Incremental: per-fact visibility (ownership) | ✅ ~7% of DB size | — (declined) |
| At-rest validation | ✅ six checks, two of which are our [I1](../website/content/invariants.md#i1)/[I12](../website/content/invariants.md#i12) | — |
| Per-predicate stats | ✅ maintained incrementally, O(1) read, returned as an `Interval` (bounds, not exact) | — but **cheaper than Glean's when built** — see below |
| Compaction on seal | ✅ `optimize(compact)` (`container-impl.cpp:288-312`) | ✅ sealing merges |
| Write backpressure | ✅ reject + computed retry delay | flow control on the socket |
| Derived-and-stored predicates | ✅ | [the roadmap](../PLAN.md#stored-derivation) |
| Schema evolution in place | ✅ query-time transform | — (frozen, [I13](../website/content/invariants.md#i13)) |

On per-predicate stats: Glean maintains a `stats` column family incrementally and reads an
`Interval` — **bounds, not a count** — and its own source says why an exact number would not help
anyway (*"Because of ownership slicing it may be that even though `Backend.predicateStats` gives
a positive number all facts could be filtered out"*, `UserQuery.hs:600-602`).

Two properties we already have make ours strictly better, and neither cost anything to arrange:

- **One keyspace pair per predicate**, so a predicate's fact count is a property of a tree rather
  than a tally to maintain.
- **Insert-only, by construction.** Nothing deletes a row from `keys` or `entities` — the only
  `remove` in the store is `FORMAT_KEY` in the `meta` keyspace
  (`crates/fjord-store/src/store.rs:1429`). fjall's `approximate_len()` is O(1) and its doc
  says it is *reliable* for exactly this workload (`fjall-3.1.8/src/keyspace/mod.rs:480-506`);
  `len()` is an O(n) scan and is not needed.

So an **exact O(1) count per predicate** is available today and unused. Two bonuses fall out:
`keys.approximate_len() == entities.approximate_len()` is an O(1) [I12](../website/content/invariants.md#i12) check,
and the numbers are what a cost model would need if one is ever built
([§3.1](#31-what-is-known-before-running--almost-nothing-on-both-sides)).

Where it should surface is already decided and written down: `fjord-server/src/stats.rs`'s own
module doc names *"a virtual predicate over the socket that exists"* as the durable home for
counters, and [`catalogue`](../crates/fjord-server/src/catalogue.rs) is the built precedent —
`fjord.db.List` is answered at the `FactStore` seam, so it gains the plan IR no variant, the
cursor no case, and filtering, joining and paging all work the first time. A predicate-stats
virtual predicate plus a `:stat` alias is the same shape as `:list`, and needs no new frame kind.

---

---

## 2. Efficiency — where each system spends

### 2.1 Where a row lives while it is being matched

Glean compiles each query to bytecode for a **43-instruction** register machine
(`glean/bytecode/def/Glean/Bytecode/Generate/Instruction.hs:99-420`), against which the
executor registers **19 syscalls** for a user query (`query.cpp:601-620`).
Part I calls it a "60-instruction" machine; 43 + 19 = 62
is the likely source of that figure, and the distinction matters — the syscalls are the store
seam, the analogue of our `FactStore`, not part of the instruction set. What is striking is the
*shape* of the instruction set: over a third of it is byte-stream cursor manipulation —
`InputNat`, `InputSkipNat`, `InputSkipTrustedString`, `InputShiftBytes`, `OutputNat`,
`GetOutputSize`. Glean does not decode a fact into a structure and then match it. It generates
code that walks the encoded bytes in place.

That is the same insight as [I5](../website/content/invariants.md#i5) — the register holds the whole row, fields
decode lazily — reached by a different route. Glean gets it by *generating* the walk per query;
we get it by *interpreting* a plan whose steps name field offsets. The generated version has no
per-field dispatch; ours has no compile step and no ABI to version. Neither is obviously
better, and the comparison is already in Part I; what is
worth adding here is that Glean's own instruction set is the strongest available evidence that
the decode-lazily rule is the load-bearing one, since Glean paid for a code generator to get it.

### 2.2 The value read

`FactIterator::get(Demand demand)` where `Demand` is `{KeyOnly, KeyValue}`, with the comment
*"Demand says whether to include its value which might be more expensive (rocksdb will do an
additional lookup)"* (`glean/rts/lookup.h:27,33-35`). The executor passes the demand through
from a bytecode operand (`query.cpp:326-328`).

So Glean has the same knowledge [I6](../website/content/invariants.md#i6) encodes, and spends it as a **runtime
parameter** rather than a structural guarantee — the scan *can* fetch a value, and does when a
value pattern is non-wild. Ours cannot, which is why our version is an invariant with a decode
probe behind it rather than a flag. The cost of our version is that value patterns stay deferred
(`nyi/value-match`).

### 2.3 Point reads on the write path — the biggest thing to take

**Glean has an LRU cache in front of `idByKey`/`typeById`/`factById`, and it exists specifically
for writes.** `LookupCache` (`glean/rts/cache.h:22-113`) is described as *"An LRU fact cache for
speeding up point lookups (and only those) during writes"*, and it is engineered well past a
naive LRU:

- It caches **partial** facts — type only, type+key, or everything — depending on which lookup
  populated it (`cache.h:24-28`).
- Hash maps behind a write-priority read-write lock; the LRU list behind a separate mutex.
- **The LRU list is not touched on access.** Accesses go into append-only, *lossy*, per-thread
  buffers (`Touched`), drained only on eviction or when full — explicitly *"quite similar to
  what, e.g., Java's Caffeine library does"* (`cache.h:36-43`).
- Eleven counters split hit / miss / **failure** (fact does not exist) / **delete** (cached but
  with too little information for this call), `cache.h:60-90`.

Now ours. Interning a fact does `resolve_or_create` → `FjallDb::intern` → `fact_at`, and
`fact_at` performs **two live LSM point reads per fact**: one on `keys` to find the id, then one
on `entities` to compare the stored value (`crates/fjord-store/src/store.rs:459-520`). There
is no cache anywhere on that path. For a nested code index every reference resolution is a
`fact_at`, and the vast majority *hit* — a single `src.File` is referenced by thousands of facts,
and each reference re-reads it from the LSM.

Two separable wins, in order of size:

1. **A `LookupCache` equivalent in front of `fact_at`.** The workload is ideal for it: a
   syntax walk emits references to the same parents in bursts, and a sealed-once database means
   no invalidation problem at all — an entry can never become stale, because a written fact
   never changes. That is a *simpler* cache than Glean's, since ours needs no coherence story.
2. **Skip the `entities` read when the declared value type is empty.** Glean does exactly this,
   with the reasoning spelled out: *"If it is empty then it must be the only inhabitant of its
   type… there is no need to check against the value stored in the database as that must be
   empty, too"* (`glean/rts/stacked.h:176-186`). Most predicates in `code_index` are key-only,
   so on those the second point read is provably redundant. Ours currently does it
   unconditionally, and the doc comment gives a second reason — it doubles as an
   [I12](../website/content/invariants.md#i12) check — so this one is a **deliberate** trade rather than an
   oversight. Worth reconsidering with a number attached: I12 is guarded by a test, and paying
   for it on every interned reference at ingest is a strange place to buy insurance.

Neither touches an invariant, and **both are now built**, with the measurements that argued
for them and the ones that judged them: [findings §12](../bench/FINDINGS.md) found 73.6% of
the write path's work was re-reading something already present; §13 priced the cache at 23%
of a resolve pass; §16 watched it run at 73.05% hits of an available 73.12%, with the
key-only fast path removing the second read entirely.

**The third item, which this section did not see because it was looking at cost.** Glean's write
path is 48 threads over per-repo queues excluded by a *try*-mutex whose loser "deduplicates and
then writes anyway" (`glean/db/Glean/Database/Write/Batch.hs:221-234`, with an open `TODO` asking
whether it should) — which is a per-*database* exclusion standing in for a per-*key* one, and the
double-create hazard is what `ignoreRedef` then has to absorb downstream. Ours had the same shape
with the count set to one: a single writer task per database, doing the excluding by existing.
Going parallel was therefore not "adopting Glean's concurrency"; it was taking the primitive
Glean's own arrangement shows the absence of — the striped merge frontier, and the cache is
sharded by the same stripe rather than sitting behind one mutex.

**One property of ours worth recording while it is cheap.** Glean lists a bounded `LookupCache`
whose eviction depends on timing among the six reasons its databases are not reproducible
(`ops-I4`). Ours cannot inherit that: a miss falls through to an authoritative LSM read, so
eviction changes *speed* and never *outcome*. It stays true only while that remains the case — a
cache that ever becomes the sole record of an uncommitted write (which is what batching commits per
block would make it) is load-bearing for correctness, and its eviction policy becomes an
[I12](../website/content/invariants.md#i12) concern. The parallel-ingestion work states that as a constraint
rather than discovering it.

### 2.4 Walking a fact's references

Glean generates a **per-predicate traversal subroutine**, and the generator prunes statically:
`hasRefs ty` decides whether a subtree can contain a reference at all, and subtrees that cannot
are skipped without being walked (`glean/hs/Glean/RTS/Traverse.hs:27-60`). A ref-free field
costs a pointer bump; a ref-free predicate costs nothing.

Our `references()` walks the whole decoded value structurally every time, with no such test
(`crates/fjord-client/src/expand.rs:364-375`). Since `hasRefs` is a pure function of a
predicate's declared type, it can be precomputed once per schema and consulted before the walk —
so `:expand` over a result whose predicates hold no references would do no work instead of
walking every field of every row. Small, self-contained, and exactly the kind of thing the
freshly-built expansion path should have before it is measured.

### 2.5 Physical storage tuning

Glean tunes RocksDB per column family, and the reasoning is recorded:

| Family | Setting | Why |
|---|---|---|
| `keys` | `NewFixedPrefixTransform(8)` (`container-impl.cpp:39-42`) | The predicate is an 8-byte key prefix, so prefix seeks get a prefix bloom |
| `entities` | `inplace_update_support = false` | Otherwise RocksDB asserts when iterating backwards |
| `admin`, `stats`, `ownershipUnits`, `ownershipUnitIds`, `factOwnerPages`, `batchDescriptors` | `OptimizeForPointLookup(10)` (`container-impl.cpp:32,44,51,57,94,103`) | Small metadata families, never scanned |
| all | `NewBloomFilterPolicy(10, false)`, `whole_key_filtering = true` (`:152-153`) | |
| all | `block_size_deviation = 100` (`:161-172`) | **See below** |
| database | `allow_concurrent_memtable_write = false` (`:145`) | One writer, by construction |

The `block_size_deviation` comment is a production incident written down:

> The default setting of `block_size_deviation = 10` means that RocksDB will always add another
> KV to the current block if it is <90% full, even if the key is huge. We had an issue where
> there was a large entry in the entities column family adjacent to smaller entries in the same
> block, and because the block was massive it didn't get cached, so we had very poor performance
> for fetching all keys in that block.

**We create both keyspaces with `KeyspaceCreateOptions::default`** — every call site in
`crates/fjord-store/src/store.rs` (`:175,298,304,1399,1414,1426`). The honest reading is not
"untuned": fjall 3.1.8's defaults already do most of what Glean sets by hand — bloom filters at
1e-4 FPR on L0 and 10 bits/key above it, L0 index and filter blocks pinned, 4 KiB data blocks
(`fjall-3.1.8/src/keyspace/options.rs:84-113`). What is left on the table is the part that is a
*statement about our workload*, which only we can make:

- **`expect_point_read_hits(true)` on `entities`.** It drops last-level filters for ~90% less
  filter data. Our `entities` reads are point reads by an id that a `keys` row or a register
  already produced, so they hit — the exception is a client-supplied id via `F`/`f`, where a
  miss is possible and would get slower. That is the right side of the trade: a bogus id is the
  exceptional path, and `Found::Missing` already treats it as a fault.
- **A per-tree block-size or KV-separation decision for `entities`.** This is Glean's incident,
  and the shape matches: `entities` holds key+value bytes and its rows vary enormously in size,
  while `keys` rows are small and uniform. fjall exposes `data_block_size_policy` and
  `with_kv_separation` (`options.rs:526,688`), and KV separation is the modern answer to exactly
  the problem Glean patched with `block_size_deviation`.
- **We should not copy `OptimizeForPointLookup` onto `entities`.** Glean does not either — that
  setting goes only to its small metadata families. `entities` is iterated at open (high-water
  mark recovery reads its last key, sealing merges it), and a hash-index layout would trade
  that away.

Everything here is a keyspace *creation* option, so it is a one-way door per database — which
makes it a thing to decide before the next large corpus is built, not after.

### 2.6 How results are accumulated

Glean **materialises the whole page**, into nine parallel vectors plus two hash sets
(`query.cpp:239-257`) — four for results, five for expanded nested facts — and counts bytes as
it goes. We stream rows into a chunk of at most
`CHUNK_ROWS = 256` (`crates/fjord-server/src/session.rs:1235`) and hand it to the outbound
writer.

Ours is the better shape and it is already an anti-pattern in
[`conventions.md`](../AGENTS.md), so the only thing to add is what Glean buys with it: a
byte budget it can enforce exactly, and result dedup. Both need the whole page in hand.

### 2.7 Expansion: server-side vs client-side

Glean expands references **in the server, inside the result loop**. `Depth` is
`{ResultsOnly, ExpandRecursive, ExpandPartial}` (`glean/rts/query.h:42`); `recordResult` runs the
predicate's traverse subroutine over the fact's bytes to collect referenced ids, then drains a
worklist, fetching each and traversing it in turn (`query.cpp:450-505`). `ExpandPartial` filters
by a predicate allowlist (`nestedFact`, `query.cpp:438-448`). Expanded facts ride back in a
separate set of vectors, so the client gets one round trip with everything in it.

Ours is the opposite decision — client-side, breadth-first, one round trip per level, depth
bounded, cached across the rows of a page (`crates/fjord-client/src/expand.rs`). The
reasoning is recorded: how deep to expand is a display decision, and expansion is orthogonal to
paging, profiling and counting.

Reading Glean's version, two observations hold up:

- **The predicate allowlist is the better dial for a code-search page**, and we do not have it.
  "Expand references to `src.File` but not to `src.Decl`" is what a file view actually wants;
  "expand two hops" is a proxy for it. It is additive — a set of predicate ids alongside the
  depth — and it needs no server change, since our expansion already groups ids by predicate
  per level for refusal attribution.
- **Glean's round-trip count is 1, ours is the depth.** On a Unix socket that is cheap and the
  cache absorbs repeats within a page (measured: 8 point reads then 7 on the second page). Over
  TCP with latency it is not. Worth knowing before anyone points `--expand` at a remote server.

### 2.8 What a resume costs

This corrects a detail in Part I, which says our
verification direction catches a mismatched database and Glean's does not. That is right, but
the mechanism is more specific than "no check", and the cost asymmetry runs the other way from
what the size comparison suggests.

Glean's continuation carries, per live iterator, the current fact's **id**, its predicate, the
prefix length, and the iterator's id bounds (`query.cpp:411-424`). To resume, it must
**re-read that fact by id** to recover the key bytes, then re-seek with the recovered prefix,
then assert the re-seek landed on the same key — and it raises `"restart iter fact not found"`
if the fact is gone (`query.cpp:690-737`). So a Glean resume costs **one point read per open
iterator**, plus a re-seek.

Ours stores the key bytes themselves, so a resume is a seek and no point read
([chapter 5](../website/content/executor.md)). Our token is bigger per level and cheaper to use.

On verification: `UserQueryCont` does carry `version` and `hash` (`glean.thrift:398-405`), and
both are checked (`UserQuery.hs:1258-1268`) — but the `hash` is a **self-checksum**, over the
continuation bytes, `nextId`, version and return type (`UserQuery.hs:1270-1283`), with the
comment *"really just a checksum to detect accidental corruption"*. It cannot detect a
continuation replayed against a different query or a different database. The `version` is the
global bytecode ABI number, currently 15 with `lowestSupportedVersion = 15`
(`Instruction.hs:87-96`) — so any bytecode change invalidates every in-flight continuation
everywhere, which is the coarsest possible version of what our per-plan fingerprint does
precisely.

### 2.9 Fairness and scheduling

**Glean has round-robin fairness on writes, across databases, and it is deliberate.** The type
comment says so — *"So that we can round-robin writes to repos, have a queue of write queues"*
(`glean/db/Glean/Database/Types.hs:198-202`) — and `dequeueLoop`
(`glean/db/Glean/Database/Writes.hs:154-190`) implements it with three documented rules: a queue
with work left goes to the back; a not-ready checkpoint keeps the search going and its queue
goes to the back; if only unready checkpoints remain it blocks rather than spinning. The source
notes it is an O(n) STM transaction and argues that n stays small.

Backpressure is by **rejection with a computed delay**: writes are accounted against a global
memory budget, and over budget the write is refused with `Retry` carrying a backoff derived from
that queue's recent latency, clamped to 1–1000 s (`Write/Queue.hs:126-155`).

Ours is round-robin too, on a different axis: `outbound` interleaves **per-stream queues within
one connection**, because *"a query returning a million rows"* would otherwise starve every
other stream on the socket (`crates/fjord-server/src/outbound.rs:1-11,180`). Backpressure is
the bounded per-stream queue plus the socket.

The two are complementary, and neither system has the other's:

| Axis | Glean | Fjord |
|---|---|---|
| Writes across databases | ✅ round-robin, checkpoint-aware | single funnel, `ops-I5` |
| Frames across streams on one connection | — (Thrift request/response) | ✅ round-robin |
| Queries against each other | — (a thread per Thrift request) | — |
| Backpressure on writes | reject + computed retry | socket flow control |

So the fairness worth protecting here is real and is ours: **nothing in Glean makes two
concurrent readers fair to each other, and nothing in Glean interleaves one connection's
streams.** What Glean has that we do not is fairness across *databases* on the write side — which
becomes interesting exactly when many CI runs ingest into one server, i.e. the scenario in §0.

### 2.10 The write path in the large

Glean's producers do not hold ids. A batch is written with **locally-allocated** ids and then
renumbered: `Substitution` is a dense `Id -> Id` map over a contiguous block
(`glean/rts/substitution.h:26-80`), and `FactSet::rebase` splits a set into the part covered by
the substitution (which becomes global) and the part beyond it (which gets fresh ids)
(`glean/rts/factset.h:315-325`). Ids being *dense* is what makes the substitution a vector
rather than a map, which is one of the five places `glean-comparison.md` already lists density
as load-bearing.

Two smaller notes from the same file that read as warnings:

- `FactSet`'s prefix seek carries *"WARNING: This is currently not intended for production use
  as it is very slow. The first call for each predicate will be especially slow as it will need
  to create an index"* (`factset.h:282-288`). Glean's write buffer cannot be queried efficiently
  — the index is built lazily on first seek. Our `MemStore` is tests-only, which is the same
  admission made as a rule.
- A memory post-mortem: the maps from id and from key to `Fact*` *"used quite a bit of memory
  (almost exactly as much as the facts themselves)"* and were replaced with sets using
  heterogeneous key comparison (`factset.h:30-33`). Anything of ours that keeps a
  `HashMap<Key, …>` beside the facts inherits that arithmetic — worth remembering for the
  `LookupCache` in [§2.3](#23-point-reads-on-the-write-path--the-biggest-thing-to-take), which
  should key on bytes it already owns rather than copies.

---

---

## 3. The cost model

Two different questions get called "cost model", and the two systems answer them oppositely.

### 3.1 What is known before running — almost nothing, on both sides

Glean's `Reorder` ranks statements on a **seven-class symbolic ladder** —
`StmtFilter < StmtLookup < StmtPointMatch < StmtPrefixFactMatch < StmtPrefixMatch < StmtScan <
StmtUnresolved` (`glean/db/Glean/Query/Reorder.hs:283-291`) — chosen greedily with re-binding
after each pick (`Reorder.hs:262-280`). The classification is a function of **which key fields
are bound**, never of how many facts exist: `Reorder.hs` does not import `PredicateStats` at
all.

Where Glean *does* consult statistics, it is not for ordering:

- **Section pruning for incremental derivation.** `makeIncremental` rewrites a query's
  generators into base-only and stacked-seeking variants, driven by a
  `SeekSection -> Pid -> Bool` predicate built from per-predicate counts
  (`glean/db/Glean/Query/Incremental.hs:24-80`, `UserQuery.hs:575,603-616`). And the source
  flags its own limitation: *"Because of ownership slicing it may be that even though
  `Backend.predicateStats` gives a positive number all facts could be filtered out"*
  (`UserQuery.hs:600-602`).
- **A diagnostic.** A warning that a predicate has no facts in this database
  (`UserQuery.hs:705-736`) — a message, not a plan change.

Ours has no cost model at all, and the reorder is a runnable frontier that is provably complete.
The comparison to draw is narrow but real: **Glean's ladder is a cheap static approximation of
sargeability, and ours is the same information expressed as a frontier constraint.** Neither
system will pick the smaller of two scans. On a sealed database we could answer exactly what
Glean approximates — per-predicate counts are nearly free with per-predicate keyspaces — and
Part I already lists that as a hole. What this file adds
is that Glean's own experience says the payoff is *not* in join ordering, where it never wired
the statistics in, but in **pruning** — the empty-predicate short-circuit and the section skip.

### 3.2 What a query is charged — four budgets against one

Glean charges a query four ways, all at runtime, all optional, all with server-imposed defaults
if the client omits them (`glean.thrift:424-451`):

| Budget | Enforcement |
|---|---|
| `max_results` | Passed into the bytecode as a **register** |
| `max_bytes` | Also a register, **decremented by the generated code**; the server reads back what is left to learn the bytes produced (`query.cpp:634-644`) |
| `max_time_ms` | Coarse steady clock, checked on 1 call in 100 to `next`, because *"Clock::now() is not cheap"* (`query.cpp:26-28,211-221`) |
| `maxSetSize` | Bound on intermediate set size (`query.cpp:563,580`) |

Plus a **global** kill switch: `interruptRunningQueries` stamps an atomic timestamp and every
query started before it aborts (`query.cpp:31-34,651-653`). There is no per-query cancellation
*handle*: a client cannot stop its own in-flight query, only outlive its `max_time_ms` or be
caught by a global interrupt.

Above that, the *service* adds its own ceilings — Glass caps every query at
`MAXIMUM_SYMBOLS_QUERY_LIMIT = 10000` results and `MAXIMUM_QUERY_TIME_LIMIT = 15000` ms
(`glean/glass/if/glass.thrift:24,27`), applied via combinators on every search
(`glean/glass/Glean/Glass/Utils.hs:96-127`).

Ours charges **one** way — rows per page (`page.limit`, chunked at `CHUNK_ROWS = 256`) — and
cancels through a per-query `CancellationToken` polled every `CANCELLATION_STRIDE = 4096` rows
examined (`crates/fjord-engine/src/iter.rs:389-480`).

The asymmetry is worth naming precisely, because it is a real gap and not merely a different
taste:

- **Fjord has no time budget and no byte budget.** A single pathological query cannot be
  stopped by the server on its own initiative; it can only be cancelled by whoever holds the
  token. On a shared server that is an availability hole, and it is the one place the cost model
  is genuinely thinner rather than differently shaped.
- **Our cancellation check is cheaper and coarser.** An atomic load every 4096 rows against
  Glean's clock read every 100 `next` calls. The 40× stride is affordable *because* the check is
  not a clock read — but it also means we could afford a deadline on the same stride nearly for
  free, since a coarse monotonic read every 4096 rows is noise. That is the cheapest available
  fix for the gap above.
- **Glean's byte budget lives inside the generated loop.** That is elegant and we cannot copy it
  directly — we have no codegen — but the equivalent is a byte counter in the chunk accumulator,
  which already knows each encoded row's length.
- **We put the safety at measurement time, Glean puts it at runtime.**
  [`docs/performance.md`](../website/content/performance.md) states a capacity target and
  [`bench/FINDINGS.md`](../bench/FINDINGS.md) reads every number against it; Glean has no
  equivalent document and no capacity benchmark — its `bench/` directory is seven
  micro-benchmarks (define, redefine, rename, makefact, compile, decode, query)
  (`glean/bench/`). Neither approach substitutes for the other: a target does not stop a
  runaway query, and a budget does not tell you how many users the box holds.

### 3.3 What the two cost models agree on

Both count **rows examined, not rows produced**, and both expose it: Glean's
`collect_facts_searched` returns per-predicate counts, ours returns per-*step* counts through
`iter::Profile`. Per-step is finer, and it is the right granularity for the same reason the
ladder is not enough — the interesting number is which level of a join blew up.

---

---

## 4. What to take — reviewed and agreed

Each of these is additive, respects every invariant, and wants a number before and after.
The open ones live in [`PLAN.md`](../PLAN.md)'s operational-gaps and language-backlog tables;
this section holds the reasoning.

| # | Item | Status |
|---|---|---|
| 1 | A lookup cache in front of `fact_at` | **built** — findings §13, §16 |
| 2 | Skip the `entities` read when the declared value type is empty | **built** — the I12 check it doubled as was handed to the tests that own it |
| 3 | `distinct`, under the adjacency condition | agreed, unbuilt — the gap is real |
| 4 | Per-predicate stats, and a `:stat` command | agreed, unbuilt |
| 5 | `hasRefs` precomputed per predicate, before the expansion walk | agreed, unbuilt |
| 6 | Server-side reference expansion, predicate-scoped | agreed, unbuilt |
| 7 | fjall keyspace tuning | agreed, gated on being able to measure it |
| 8 | A deadline and a byte budget | proposed |
| 9 | Row polymorphism in the typechecker | backlog |
| 10 | Fair write scheduling across databases; a fair-interleave merge | when a fan-out exists |

1. **A lookup cache in front of `fact_at`** ([§2.3](#23-point-reads-on-the-write-path--the-biggest-thing-to-take)).
   The largest single item, and *simpler* for us than for Glean because a sealed-once database
   makes an entry permanently valid — no coherence, no invalidation. Key on bytes already owned
   (`factset.h:30-33`'s post-mortem). Guard: an ingest allocation/read-count probe, not a timing
   test.
2. **Skip the `entities` read when the declared value type is empty**
   ([§2.3](#23-point-reads-on-the-write-path--the-biggest-thing-to-take)). Provably redundant on
   key-only predicates, which is most of `code_index`. The incidental
   [I12](../website/content/invariants.md#i12) check it performs has to go somewhere else or be given up
   deliberately — state the answer either way.
3. **`distinct`, under the adjacency condition**
   ([§1.3a](#13a-why-we-cannot-deduplicate-yet-and-what-would-let-us)). One row of cursor state,
   decided at plan time, refused with a diagnostic when the projection is not a prefix of the
   output order. Do **not** build the id-product version: it is provably a no-op. Do **not**
   build the unbounded set: it puts O(distinct rows) in a cursor whose whole property is being
   small.
4. **Per-predicate stats, and a `:stat` command**
   ([§1.5](#15-lifecycle-and-operations)). An exact O(1) count is available today via
   `approximate_len()` and unused, because per-predicate keyspaces plus insert-only make it
   exact rather than approximate. Surface it as a **virtual predicate** with a `:stat` alias —
   the shape `fjord.db.List` already established, and the home `stats.rs`'s module doc already
   names. Spend it on **pruning**, not join ordering: Glean never wired its statistics into
   `Reorder` and its section-pruning use is where the payoff showed up
   ([§3.1](#31-what-is-known-before-running--almost-nothing-on-both-sides)).
5. **`hasRefs` precomputed per predicate, consulted before the expansion walk**
   ([§2.4](#24-walking-a-facts-references)). Small, self-contained, and a prerequisite for item 6
   rather than an alternative to it — a server that expands references needs the same static
   reference map, so build it once and use it on both sides.
6. **Server-side reference expansion, predicate-scoped**
   ([§2.7](#27-expansion-server-side-vs-client-side)). A **flag on the query message, not a
   fourth query kind** — expansion stays orthogonal to paging, profiling and counting, which is
   the reason it was not one. Collapses the depth-many round trips into one, which is what makes
   `--expand` usable over TCP rather than only over a Unix socket. The predicate allowlist is the
   better dial than depth for a code-search page, and our expansion already groups ids by
   predicate per level. Keep the client-side path: it is what makes `:expand` retroactive on a
   page already fetched, and it is the only one that works against a server that does not have
   the flag.
7. **fjall keyspace tuning** ([§2.5](#25-physical-storage-tuning)) — `expect_point_read_hits(true)`
   on `entities`, and a block-size or KV-separation decision. **Measure, do not assume.** Two
   constraints on the harness this needs: keyspace options are fixed at *creation*, so a
   comparison has to **build a database per setting** rather than toggle a knob on an existing
   one; and the effect being looked for is on point-read latency at a size where the filters
   matter, which means a corpus at the `fj-runtime` scale rather than a fixture. Until that
   exists, fjall's defaults are the right answer, because they already do most of what Glean sets
   by hand.
8. **A deadline on the cancellation stride, and a byte budget in the chunk accumulator**
   ([§3.2](#32-what-a-query-is-charged--four-budgets-against-one)). Together these close the only
   genuine hole in the cost model. A coarse monotonic read every 4096 rows is free at our row
   costs; the accumulator already knows each row's encoded length.
9. **Row polymorphism in the typechecker** ([§1.2](#12-types)). Glean's `HasTy` constrains a
   variable to any record or sum containing at least the named fields. Backlog: it is an
   inference capability with no invariant attached and no phase waiting on it.
10. **Fair round-robin across databases on the write side** ([§2.9](#29-fairness-and-scheduling)),
    and **a fair-interleave merge policy** for any future fan-out (`takeFairN`, not `concat`, §0).
    Both arrive with the multi-database work rather than before it.

---

---

## 5. What not to take

- **Ownership.** Already declined in Part I; the
  efficiency reading only strengthens it. `FactIterator::filter(base, visible)`
  (`glean/rts/lookup.h:47-53`) is a per-row predicate on every iterator, which is our
  [I6](../website/content/invariants.md#i6)/[I9](../website/content/invariants.md#i9) anti-patterns with a different name, and
  `UserQuery.hs:600-602` shows it also makes Glean's *own statistics* unreliable.
- **A global-only interrupt.** `interruptRunningQueries` kills every query older than the stamp.
  Our per-query token is strictly better and costs nothing.
- **Materialising a page to enable dedup and an exact byte count.** The byte count is worth
  having (item 4) and can be had by counting as we stream; the dedup does not apply to an
  arbitrary projection ([§1.3](#13-answering-paging-and-inspection)).
- **A globally-versioned resume ABI.** Glean's `lowestSupportedVersion == version` means every
  bytecode change invalidates every in-flight continuation. Our per-plan fingerprint is the
  finer instrument and already built.
- **Dense global fact ids.** The price is visible across five subsystems in Glean and buys
  stacking, which §0 shows cannot compose two independently-built databases anyway.
- **And the thing to actively protect: the fair outbound writer.** Nothing in Glean interleaves
  one connection's streams, and nothing in Glean makes two readers fair to each other. It is
  ours, it is measured ([`bench/FINDINGS.md`](../bench/FINDINGS.md) §6, §10), and none of the
  items in §4 touch it.

---

---

## The things to remember

1. **The storage layout and execution shape are Glean's; the mechanisms are ours** — and the
   line is further over than this file once drew it. Order-preserving keys, self-delimiting
   bytes, values-out-of-the-loop, stable discriminants, snowflake ids, per-predicate keyspaces
   and an abstract machine instead of a bytecode VM are all divergences, each because a
   specific invariant asked for it.
2. **Immutability is the divergence everything else follows from.** No stacking, no ownership,
   no in-place evolution — and in exchange, free snapshots, byte-resumable queries, parallel
   ingest, and a stored-derivation story with none of the invalidation rules Glean's ownership
   forces on it. If incrementality ever becomes a requirement, that is the trade being
   reopened, not a feature being added.
3. **Glean's answer to cross-database identity is to not have one.** Fact ids never leave a
   database; the public name is a content-derived string whose first token is the repo; origin
   travels beside every result that will be used again, in the type. Everything in Part II §0
   lives above the executor, so `ops-I9` stays intact.
4. **On efficiency the remaining gap is capability, not spend.** The write-path gap this file
   found — two uncached point reads per interned reference — is closed and measured. What
   Glean still has that we do not is on the query surface: recursion, aggregation-as-a-term,
   arrays. The read-path benchmark ([`bench/glean-read-path.md`](../bench/glean-read-path.md))
   is what prices them.
5. **Neither system estimates a query's cost, and the two put the safety in different
   places.** Glean charges four runtime budgets and has no capacity document; we have a
   capacity target, a measurement ladder, and one budget. The honest gap is a time and byte
   budget, and our cancellation stride is already the cheap place to put both.
