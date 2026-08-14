# Aperture vs Glean — what we take, what we changed, what we have not decided

> [Aperture design book](../README.md) · reference doc

[Chapter 1](01-concepts.md) says Aperture is "inspired by [Glean](https://glean.software/),
not a clone." This is the ledger behind that sentence, because "not a clone" is only
meaningful if each difference is one of three things, and it is clear which:

1. **Adopted** — the same idea, sometimes under a different name.
2. **Deliberate divergence** — we know what Glean does and chose otherwise, with a reason
   recorded in the chapter that owns it.
3. **Not decided** — a capability Glean has that we have neither built nor ruled out. These
   are the dangerous ones, because an omission reads exactly like a decision until someone
   writes it down.

**This ledger is verified against Glean's source**, not against its published design — commit
`95c0fb6` (~105k lines of Haskell, ~44k of C++, 21k lines of `.angle` schemas, against
Aperture's 24k of Rust). That matters, because the first version of this file was written from
the public docs and **most of its rows were in the wrong bucket**. The corrections all ran one
way: we were over-claiming lineage from Glean and under-claiming our own mechanisms. Four
invariants presented as inherited — [I1](invariants.md#i1), [I2](invariants.md#i2),
[I6](invariants.md#i6), [I10](invariants.md#i10) — are divergences where Glean does the
opposite or nothing; and the conflict rule we called a divergence is Glean's own default.

Where a Glean detail is cited it is cited by file and line. Where it could not be read from
source it is marked `[inferred]`. **Glean's own in-tree prose contradicts its code in at least
one place** (`OWNERSHIP.md` describes 64K-element blocks where `setu32.h` implements 256), so
nothing here rests on Glean's documentation alone.

---

## 1. Adopted

The shape of the system is Glean's, and most of it is not restated here because the chapters
already are that:

- **Facts, predicates, and the key/value split** — a predicate fixes a fact's type; the key
  identifies and is indexed, the value is carried and fetched on demand
  ([chapter 1](01-concepts.md)).
- **Two maps, not one** — an index from `predicate ++ key → id` and an identity map from
  `id → fact`. This is the layout the design was taken from, and the correspondence is
  literal: Glean's RocksDB column families are *named* `keys` and `entities`, with the same
  directions (`glean/rocksdb/container-impl.h:52-64`,
  `glean/rocksdb/database-impl.cpp:498-517`). Both systems therefore **store the key twice**,
  which Glean names as its main space defect (`glean/website/docs/implementation/db.md:87`) —
  see [chapter 3](03-storage-model.md).
- **A predicate query is a prefix scan**, and seeking within one extends the prefix by whole
  leading key fields. Note what this does *not* require: an exact-prefix seek needs only a
  self-delimiting, canonical encoding — **not** an order-preserving one. That is a separate
  bet, and it is ours (§2).
- **Write once, then read forever.** Glean finishes a DB; we seal it (`Writable → Complete`,
  [ops-I2/I3](aperture-cli-design.md)). Every cheap thing downstream — free snapshots,
  bytes-only resume, fearless parallel ingest — is bought with this.
- **Execution is a nested loop with backtracking**, one loop level per generator, and the
  generator order *is* the loop nesting (`glean/db/Glean/Query/Codegen.hs:994-1042`;
  [chapter 4](04-executor.md)). Backtracking is a backward jump there and a frame pop here.
- **The store behind a seam.** Glean reaches `seek`/`next` through syscall function-pointer
  registers rather than opcodes — the exact analogue of our `FactStore` trait, and the same
  place it inserts `Stacked` and `Sliced` for incrementality.
- **Queries paginate through a continuation** rather than a held cursor
  ([chapter 5](05-resume.md)). Glean's is opaque bytes handed back to the client, and so is
  ours; the divergence is what is *in* it (§2).
- **Conflict handling is a deterministic reject.** Same key, different value is an error, not
  last-writer-wins: `Define::define` returns `Id::INVALID` (`glean/rts/define.h:20-30`) and
  `defineBatch` raises "invalid fact redefinition" (`glean/rts/define.cpp:91-102`). Identical
  facts dedup in both. This was previously filed as a divergence; it is Glean's default rule,
  and [ops-I5](aperture-cli-design.md) adopts it. What Glean does that we refuse is *disable*
  it on three paths — see §2.
- **Both maps written atomically** — Glean uses one `WriteBatch` over entities, keys, the id
  counter and stats (`glean/rocksdb/database-impl.cpp:480-537`);
  [I12](invariants.md#i12) is the same rule.
- **String encoding**, in detail: NUL-escaped, terminator-delimited, byte-lexicographic, and a
  string prefix seek built by *dropping* the terminator
  (`glean/rts/string.h:16-26`, `glean/db/Glean/Query/Codegen.hs:1278-1284`). This is the one
  place the two codecs genuinely agree ([chapter 2](02-tuple-codec.md)).
- **Cost tiers as the ranking for reordering** — point match before prefix match before scan,
  with omitted key fields treated as wildcards that close the prefix. This much of Glean's
  `Reorder` we do share; the algorithm around it we do not, and the earlier claim that we use
  its topological sort and antichains was false in both directions (§2).
- **Cancellation counted in rows *examined*** — Glean checks every 100th call to `next`
  (`glean/rts/query.cpp:26-28,304-319`), which is the conclusion
  [open-decisions](open-decisions.md) records for our executor, reached independently.
- **Derived predicates**, on-demand and stored. Two of the mechanisms we cite are real:
  `DerivedFactGenerator` (`glean/db/Glean/Query/Codegen/Types.hs:158`) and `DerivedAndStored`
  (`glean/angle/Glean/Angle/Types.hs:619`, spelled `stored`). Three corrections:
  `Derive when` is a Haskell constructor pattern, not Angle syntax; there are **three** modes,
  not two (`DeriveOnDemand | DerivedAndStored | DeriveIfEmpty`); and **`captureKey` is not a
  derivation mechanism at all** — it is the trick that rewrites `X = pred pat` so the *client*
  needs no second fetch (`glean/db/Glean/Query/Flatten.hs:549-586`), which we do not need
  because [I5](invariants.md#i5) already puts the whole row in the register. The seam split —
  dynamic ([Phase 6](../PLAN.md), built) versus stored (Phase 8b) — *is* Glean's seam, and
  [ops-I8](aperture-cli-design.md) matches its documented derive-before-finish ordering
  almost verbatim.
- **A self-describing DB** — the schema travels with the data, and identity is a fingerprint
  over a canonical form. Glean's `SchemaId` is a hash over a sorted name environment, which is
  structurally our qualified-name → predicate-fingerprint map. One difference matters and is a
  constraint on our canonical form: **Glean's per-predicate hash is a Merkle hash**, so a
  change propagates transitively through every referring predicate
  ([chapter 6](06-types-and-schema.md)).

---

## 2. Deliberate divergences

| Dimension | Glean | Aperture | Why, and where it is recorded |
|---|---|---|---|
| **Deployment** | A Thrift service holding many DBs; clients are remote | **Embedded library first**, with a wire protocol added for a server that reuses the same executor | The executor consumes a `(handle, snapshot)` and assumes no connection ([chapter 3](03-storage-model.md), [ops §5](aperture-cli-design.md)) |
| **Query execution** | Angle compiles to **bytecode for a query VM** (C++) — a flat 60-instruction register machine | A **defunctionalised abstract machine** — an explicit frame stack, no bytecode | [I7](invariants.md#i7). The reason is *not* that an interpreter's stack would have to be saved: Glean's VM has **no call stack** ("we don't have a stack (yet)", `glean/db/Glean/Query/Codegen.hs:573-576`). It is that Glean's continuation carries the **entire bytecode program**, PC, literals, every register and every output buffer, plus a second `traverse` subroutine (`glean/rts/bytecode/subroutine.cpp:370-381`) — and is **version-locked** to the bytecode ABI, so any bytecode change invalidates every in-flight continuation ([chapter 4](04-executor.md)) |
| **Resume state** | A continuation is opaque client-held bytes — self-contained down to the program, resumable in another process | A **bytes-only `Cursor`**: one detached row per open level, ~two orders of magnitude smaller | [I4](invariants.md#i4). Both are client-held; the divergences are *size*, the absence of an ABI lock, and the **verification direction** — we re-seek the saved key and check the fact id, where Glean maps id → key and carries no `Repo`, so a continuation replayed against the wrong DB silently resumes at the wrong row ([chapter 5](05-resume.md)) |
| **Query isolation** | **No snapshot at all** — fresh iterators at whatever LSM version is current, even *within* a page (`GetSnapshot` appears nowhere in `glean/rocksdb/`) | One immutable snapshot per query, **released at every suspend** | [I8](invariants.md#i8). Stronger than Glean in both directions: a real per-query view that Glean lacks, which nonetheless pins no LSM generation while a portal idles ([chapter 5](05-resume.md)) |
| **Key encoding** | **Not order-preserving.** Fact keys use LEB128 (`255` = `FF 01` sorts before `256` = `80 02`, `glean/rts/bytecode/subroutine.cpp:33-37`). Glean *has* an order-preserving varint (`glean/rts/nat.h:20-64`) and uses it only for storage-level keys | An **FDB-inspired order-preserving tuple codec** with a frozen marker table | [I1](invariants.md#i1)–[I3](invariants.md#i3). A real divergence, not shared ground — and one whose payoff is still **deferred**: order-preservation buys value-range scans, and no ordering operator exists yet ([chapter 2](02-tuple-codec.md)) |
| **Self-delimiting bytes** | **Untagged and positional** — records are bare concatenation; skipping a field is schema-driven codegen (`glean/hs/Glean/RTS/Traverse.hs:27-119`) | Marker-tagged; `skip` needs no schema | [I2](invariants.md#i2). Glean shows the hot-loop argument is the weak one — with per-query codegen, tags are pure overhead. The real value is schema-free tooling, golden-byte tests, and the byte-level `Int`/`Fact` distinction ([chapter 2](02-tuple-codec.md)) |
| **Values in the scan loop** | **Fetched during a scan** — a non-wild value chunk sets `needs_value` and the scan does a second store lookup per row (`glean/db/Glean/Query/Codegen.hs:1009,1088`) | Values never enter the scan hot loop | [I6](invariants.md#i6). Ours, not adopted. Affordable partly because value patterns are deferred (`nyi/value-match`) ([chapter 3](03-storage-model.md)) |
| **Union discriminants** | **List position** — no discriminant syntax exists (`glean/angle/Glean/Angle/Parser.y:306-309`); stability comes from remapping alternatives **by name** at query time, with a synthetic `unknown` (`glean/db/Glean/Query/Transform.hs:551-579`) | Explicit, assigned-once, append-only tags | [I10](invariants.md#i10). Append-only tags are not "the only safe scheme" — they are the only safe scheme **without** a query-time transform layer, which [I13](invariants.md#i13) declines ([chapter 6](06-types-and-schema.md)) |
| **Fact ids** | A **dense** monotonic id space per DB — `glean/rts/lookup.h:92-99` says ids "are supposed to be dense", and five subsystems depend on it: substitution vectors, `FactSet` indexing, Elias-Fano ownership sets, the `factOwners` interval map, and the `id < mid` stacking test | A **snowflake**: 24-bit predicate tag + 40-bit per-predicate sequence | [I11](invariants.md#i11). Density is load-bearing in Glean far beyond stacking, so this costs more than the first version of this file said — but *within* a predicate our ids are dense, so each predicate is the same dense-map shape, and only a fact set **spanning** predicates degrades. In exchange: Glean has **no concurrent writer** at the storage layer and buys parallelism back with its whole rebase/substitution subsystem, where two of our ingest workers on different predicates share no counter ([chapter 3](03-storage-model.md#factid-allocation-i11)) |
| **Scan order** | **No guarantee** — "in no specified order" (`glean/rts/lookup.h:125-127`); Glean *removed* its reliance on ordered iteration to support limited key sizes (`db.md:143`) | Lexicographic order is depended on absolutely | [I1](invariants.md#i1). Ours is the stricter commitment, and it has a price: it forecloses the key truncation Glean adopted, and we carry no key-size budget or degradation path ([chapter 3](03-storage-model.md)) |
| **Physical layout** | One store per DB; the predicate is an 8-byte key prefix with a fixed-prefix transform | **One keyspace pair per predicate** (`keys.<id>`, `entities.<id>`) | Physical isolation makes bulk ingest embarrassingly parallel, and fjall's `ingest()` wants strictly ascending keys. Two consequences we get free: no predicate id in every `entities` row, and no stats column family in the write batch to answer `count(pid)` ([chapter 3](03-storage-model.md)) |
| **Schema versioning** | **Edit in place with an automatic per-predicate transform** triggered by a hash mismatch (`glean/db/Glean/Database/Schema.hs:640-657`). `schema all.N` is the *name-resolution scope*, not the version axis; `evolves` is the manual path and only takes effect when **no facts** of the old schema exist | **One schema per DB, embedded and frozen at create**; compatibility is subset containment | [I13](invariants.md#i13). Glean's own escape hatch for a breaking change is "bump the version and treat it as separate" or "produce two DBs" — which *is* our default workflow, so the freeze promotes Glean's fallback to a rule ([chapter 6](06-types-and-schema.md), [ops §11](aperture-cli-design.md)) |
| **Reorder algorithm** | Transitive **lookup-chasing**, then greedy tier selection over a ranked cost lattice, then a **separate feasibility pass that can give up** (`glean/db/Glean/Query/Reorder.hs:420-430,539-544,575-639`). The heuristic is key-prefix boundness, **not** cardinality — `PredicateStats` is never imported by `Reorder.hs` | A greedy **runnable frontier**, **provably complete**, with no cost model yet | [chapter 7](07-compilation.md). Neither side does topological sort or antichains; the earlier claim was false in both directions. Completeness is ours to claim — Glean's pass can fail to order a satisfiable query |
| **Incrementality — stacking** | **Stacked DBs**: compose two DBs by fact-id range (`glean/rts/stacked.h:20-144`). Needs no ownership | **None** | [ops-I9](aperture-cli-design.md). The cheaper half, and the one the snowflake could carve for; the seam that would carry it is `FactStore::{scan, point}` — exactly where Glean puts `Stacked` |
| **Incrementality — ownership** | **Ownership sets**: per-fact set expressions letting a delta *hide* base facts, ~7% of DB size, checked as a **per-row filter** on every iterator (`glean/rts/ownership/slice.h:167-233`) | **None** | [ops-I9](aperture-cli-design.md). The expensive half, and the one to keep declining: the filter is literally our [I6](invariants.md#i6)/[I9](invariants.md#i9) anti-patterns, propagation is O(facts) in time *and* space, and it **bans negation in stored derived predicates** purely for invalidation cost. It is also Glean's **authorization** substrate, which ties this row to [ops-I10](aperture-cli-design.md) |
| **Ingest** | JSON/binary batches over Thrift; a binary `Batch` is one opaque sequential blob and is **not splittable** — parallelism is *across* batches | **Fact files** with sync markers, chunk-split and merged in parallel, and a **single write funnel** every writer passes | [ops-I4/I5](aperture-cli-design.md). The argument is Glean's own: it must set `ignoreRedef` on three paths, with the source comment *"we are ignoring actual errors and silently picking one of the two facts… That's bad, but I don't see an alternative"* (`glean/db/Glean/Write/SendAndRebaseQueue.hs:408-426`) — first-writer-wins, order-dependent, and exactly what reproducibility forbids ([Phase 7](../PLAN.md)) |
| **Negation** | Angle's `!` is *written* as pattern syntax but is unit-typed and **desugars to a negated statement group** (`glean/db/Glean/Query/Typecheck.hs:626-654`) | focus negates a **statement** | Both negate statements, so the level is not the divergence — what differs is that Angle can write `(!A) | B` (only bare `!A | B` fails, on precedence). The *rationale* survives intact and is vindicated by Glean: its reorder forces negations after their parent-scope binds "to ensure consistent semantics regardless of order" (`glean/db/Glean/Query/Reorder.hs:547-573`) ([`PLAN.md`](../PLAN.md) Phase 2) |
| **Cancellation** | **Global only** (`interruptRunningQueries`) | A per-query cancellation token | [chapter 5](05-resume.md). Small, and ours |
| **Diagnostics** | One `Doc ()`, fail-fast on the first error, no codes, a text location prefix — and `Reorder.hs` can show a user flattened IR they never wrote | A closed `Code` enum, an accumulating sink, rendered source spans, and corpus entries asserting exact code sets | [testing.md](testing.md). Not a Glean idea, and the clearest place we are simply better |
| **Design method** | — | An **invariant registry** with a numbered guard test each, and non-functional claims held by *mechanical* guards (allocation counters, decode probes, drop probes) | [testing.md](testing.md), [invariants.md](invariants.md). Glean's equivalent for the same properties is a header comment and code review — its resume safety rests on a hand-maintained "don't keep pointers in registers across `Suspend`" rule with a live workaround |

---

## 3. Not decided — the honest gaps

### The type model is narrower than Glean's

Glean's **runtime** has eight type constructors — byte, nat, string, array, tuple, sum, set,
predicate reference (`glean/hs/Glean/RTS/Types.hs:185-194`). `bool`, `maybe`, `enum`, tuples
and named types are **sugar lowered before storage** (`glean/.../Schema/Util.hs:35-62`), so the
ten-item surface list overstates the distance: `PredicateTy`'s four constructors — `Int`,
`Str`, `Fact`, `Record` — are **three** away from Glean's runtime, not eight away from its
surface. Two things the earlier version of this row missed: **`set T` is a real Glean type**
(only 7 uses in all of Glean, so separately deferrable), and Glean has **no signed integer** —
a place we are *wider*.

The codec has reserved marker bands in the right sort positions
([chapter 2](02-tuple-codec.md#the-marker-table)), so most of these are not a one-way door *in
the encoding*. **Arrays are the exception**, and it is a seekability exception rather than a
band-allocation one: a length-prefixed array cannot be prefix-matched, which Glean states
outright — *"MatchArrayPrefix doesn't actually look at a prefix because arrays encode their
length at the front"* (`glean/db/Glean/Query/Reorder.hs:794-796`).

**Multiplicity** is the decision with a deadline, and the framing it was opened with is a false
binary: **Glean does both, deliberately, for the same data** — a compact array-bearing fact,
then a `stored` derived predicate that explodes it with `[..]` to get the seekable index. See
[open-decisions](open-decisions.md#multiplicity--arrays-or-one-fact-per-element) for the
evidence and the three constraints that now attach to it.

**Status: undecided, and worth deciding before the schema DSL fixes what can be written.**

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
holding state across iterations conflicts with [I8](invariants.md#i8).
**Status: undecided, and expensive.**

### Primitives, expressions and aggregation

Angle has `prim.*` and if-then-else — but the surface is **much smaller than this file used to
imply**: exactly 15 primitives, arithmetic is `+` on nat only, string functions are `toLower`
and `reverse` only, and comparisons are nat-only plus a generic `!=`.

Two corrections on our side. Order comparisons are **not** "deferred with a seam" — there is no
`ResidualOp` arm and no lexer token for `<`/`>`, so a comparison is a **parse error**, which is
a deliberate exception to *permissive grammar, narrow later*. And **aggregation is not a shared
absence**: Angle has `all q` set construction plus `prim.size`/`prim.length`, so
`prim.size (all …)` is a count. Results are aggregated by the caller here, and that is now an
Aperture-only position rather than a shared one.

The seam for all of this exists — Glean's `PrimCall` is structurally a one-row generator, which
is exactly `Step::Derive` ([I14](invariants.md#i14)) — so it is additive when wanted.
**Status: undecided.** See [open-decisions](open-decisions.md#primitives-in-the-query-language).

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
not do. What remains genuinely absent: **lookup-chasing** (transitive propagation of boundness
before tier selection), a cost model, and `Prune`'s empty-predicate short-circuit — which a
sealed DB could answer *exactly* rather than approximately. `Ordered`/`Floating` statement tags
are now present (`reorder::Placement`) but carry no rule yet; the rule they exist for is Phase
6b's negation placement, and adopting Glean's own use of them — floating statements first —
would break the claim that a nested pattern and its two-statement spelling compile to the same
plan. **Generator synthesis for an unbound variable does not port at all**: it fires in Glean
because prim args, `all`, `if`/`then`/`else` and or-branches can mention a predicate-typed
variable no generator binds, and in focus a `Fact` type can only come from a fact pattern
(which binds it) or a fact-typed key field (which captures it), so the precondition never
holds. [Chapter 7](07-compilation.md).

### No on-disk format version

Glean versions both its DB format and its bytecode ABI. **Aperture versions nothing** — so
[I3](invariants.md#i3) must hold forever, because a reader has no way to detect which encoding
it is looking at. Not a decision, an omission. [Chapter 2](02-tuple-codec.md).

### Operational capabilities

Separating what is inherent to being a *service* from what is a real hole. Inherent, and
therefore not ours to want: the janitor, sharding, ACLs, async write handles, remote backup
scheduling. **Genuine holes:** no provenance on a DB (Glean's identity records where it came
from); no database properties; **no at-rest validation** — Glean's `Validate` runs six checks,
two of which are literally [I1](invariants.md#i1) (enumeration order) and
[I12](invariants.md#i12) (`idByKey` agreement); no per-predicate stats, which Glean maintains
incrementally for an O(1) read *and spends on planning*, and which per-predicate keyspaces make
nearly free here; no retention policy. And in the shell, **`:more`** — ours discards the cursor,
so [I4](invariants.md#i4) and [I8](invariants.md#i8) have no interactive exerciser.
[Operations](aperture-cli-design.md).

### The idiomatic spelling of a join — closed

In Angle, nested fact patterns — `Knows { from = Person { id = 1 }}` — are *the* way one writes
a traversal. Phase 4 parsed and typechecked that and then deferred it, in three pieces of very
different size; **Phase 5 landed all three**
([`PLAN.md`](../PLAN.md#reaching-a-fact-through-a-reference--three-sizes-listed-apart--phase-5)), so the
nested spelling now compiles — to the *same plan* as the two-statement form, which is the sense
in which it is a spelling rather than a second way to run a query.

Aperture and Angle agree on the mechanism, too: a reference is followed by its **id**, so a
join through one reads no second fact. Reading *through* a reference — a field or value of the
fact it names — is the other half, and a lookup rather than a compare; Angle does it freely, and
Aperture now does it as a
[`Source::Fetch`](04-executor.md#fetching-through-a-reference) level, one point read per row of
the level above it. The two halves stay distinct in the IR on purpose: the cheap one is a
compare against an id already in a register, and conflating them is how a key gets spliced where
an id belongs. What is left deferred is narrow — a reference held in a fact's *value*
(`nyi/fact-field`), where the id is not in a register's key bytes to read.

---

## 4. Where we are ahead, and had not said so

Collected because the first version of this file claimed almost none of it, and because three
of these are places Glean's source shows the cost of *not* having done it.

- **Non-functional invariants held mechanically** — allocation counters, decode probes, drop
  probes, with positive controls. Glean binds eagerly (word fields decoded into registers,
  non-word fields memcpy'd into a buffer *before* the inner loop) and reallocates its output
  buffer per row past a 23-byte small-string threshold. One caveat we should own: our
  allocation guard is **single-level**, and a join allocates per outer row.
- **Values out of the scan loop** ([I6](invariants.md#i6)) and **an immutable per-query
  snapshot** ([I8](invariants.md#i8)) — Glean has neither.
- **Snapshot release is structural**, because `enumerate` consumes `self`; Glean's equivalent
  is a comment.
- **Resume verification catches a mismatched DB**; Glean's direction does not, and its
  result-dedup set is per-page and absent from the continuation, so a paged Glean query can
  return **cross-page duplicates** — a live violation of the property
  [I4](invariants.md#i4) exists to pin.
- **A provably complete reorder**, a **per-query cancellation token**, and **signed integers**.
- **Diagnostics and literal hygiene** — Angle silently wraps nat overflow and silently takes
  the first of duplicate fields; both are rejected here with codes. Angle's own guide shows an
  inference failure that focus handles with no annotation.
- **Concurrent per-predicate writers**, and a high-water mark recovered from data rather than
  a persisted counter that can go missing.
- **"Deferred" as a first-class executable category** — no unimplemented feature may surface as
  a syntax error, with corpus entries pinning exact rows against a real DB. Nothing in Glean
  ties surface, diagnostics and answers together in one table.

---

## The three things to remember

1. **The storage layout and execution shape are Glean's; the mechanisms are ours** — and the
   line between those is further over than this file used to draw it. Order-preserving keys,
   self-delimiting bytes, values-out-of-the-loop, stable discriminants, snowflake ids,
   per-predicate keyspaces and an abstract machine instead of a bytecode VM are all
   divergences, each because a specific invariant asked for it.
2. **Immutability is the divergence everything else follows from.** No stacking, no ownership,
   no in-place evolution — and in exchange, free snapshots, byte-resumable queries, parallel
   ingest, and a stored-derivation story with none of the invalidation rules Glean's ownership
   forces on it. The causal chain runs: reproducibility → reject and funnel → no concurrent
   writers → no incrementality. If incrementality ever becomes a requirement, that is the trade
   being reopened, not a feature being added — and the honest counter-argument is that
   `ops-I9`'s "cheap because it is a tar-able file" is Glean's *own* motivation for
   incrementality with the sign flipped.
3. **The query surface is where we are still smaller**, and rather less of that is written down
   as a choice than it should be. Arrays are the decision with a deadline; recursion and
   aggregation are the two capabilities we now know Glean has and we had recorded as shared
   absences.

---

> [← Open decisions](open-decisions.md) · [Index](../README.md) · [Glossary →](glossary.md)
