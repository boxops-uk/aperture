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

The Glean column is "as we understand it from the public design and the parts of it this
project was modelled on"; where a detail of its internals is uncertain it is marked. Nothing
here is load-bearing on Glean being described perfectly — the value is the **Aperture** column
and which of the three buckets each row is in.

---

## 1. Adopted

The shape of the system is Glean's, and most of it is not restated here because the chapters
already are that:

- **Facts, predicates, and the key/value split** — a predicate fixes a fact's type; the key
  identifies and is indexed, the value is carried and fetched on demand
  ([chapter 1](01-concepts.md), [I6](invariants.md#i6)).
- **Two maps, not one** — an index from `predicate ++ key → id` and an identity map from
  `id → fact`, so scanning never touches values ([chapter 3](03-storage-model.md)). This is
  the layout the design was taken from.
- **A predicate query is a prefix scan**, and seeking within one extends the prefix by key
  fields — which is what makes an order-preserving encoding load-bearing rather than tidy.
- **Write once, then read forever.** Glean finishes a DB; we seal it (`Writable → Complete`,
  [ops-I2/I3](aperture-cli-design.md)). Every cheap thing downstream — free snapshots,
  bytes-only resume, fearless parallel ingest — is bought with this.
- **Execution is a nested loop with backtracking**, one loop level per generator, and the
  generator order *is* the loop nesting ([chapter 4](04-executor.md)).
- **Queries paginate through a continuation** rather than a held cursor
  ([chapter 5](05-resume.md)).
- **Derived predicates**, on-demand and stored, with the mechanism mirroring Glean's
  (`DerivedFactGenerator`, `Derive when`, the `captureKey` trick, `DerivedAndStored`) —
  [chapter 7](07-compilation.md#derived-facts), [Phase 6](../PLAN.md).
- **No general recursion.** Both decline it; a cycle among derived binds is a compile error.
- **Reordering as topological sort + antichains + a selectivity heuristic**, explicitly à la
  Glean's `Reorder`, and the bounded "PLAN-B" distribution of an alternation *into a single
  seek* ([chapter 7](07-compilation.md)).
- **A self-describing DB** — the schema travels with the data, and identity is a fingerprint
  over a canonical form ([chapter 6](06-types-and-schema.md)).

---

## 2. Deliberate divergences

| Dimension | Glean | Aperture | Why, and where it is recorded |
|---|---|---|---|
| **Deployment** | A Thrift service holding many DBs; clients are remote | **Embedded library first**, with a wire protocol added for a server that reuses the same executor | The executor consumes a `(handle, snapshot)` and assumes no connection ([chapter 3](03-storage-model.md), [ops §5](aperture-cli-design.md)) |
| **Query execution** | Angle compiles to **bytecode for a query VM** (C++) | A **defunctionalised abstract machine** — an explicit frame stack, no bytecode | [I7](invariants.md#i7). A VM was designed (the external ISA note) and *not* built: the machine has to be able to stop between any two rows and be rebuilt from bytes, and an interpreter's own stack is state a `Cursor` would have to carry ([chapter 4](04-executor.md#why-a-state-machine-and-not-recursion--i7)) |
| **Resume state** | A continuation is server-side query state | A **bytes-only `Cursor`**: one detached row per open level, nothing else — and the snapshot is *released* at every suspend | [I4](invariants.md#i4)/[I8](invariants.md#i8). An idle portal must pin no LSM generation, which is what makes many suspended queries cheap ([chapter 5](05-resume.md)) |
| **Key encoding** | Glean's own fact-key encoding | An **FDB-inspired order-preserving tuple codec** with a frozen marker table | [I1](invariants.md#i1)–[I3](invariants.md#i3). Order-preservation is what turns every range query into a byte scan, so the marker values are frozen the moment data exists ([chapter 2](02-tuple-codec.md)) |
| **Fact ids** | A monotonic id space per DB (id ranges are how stacked DBs work) | A **snowflake**: 24-bit predicate tag + 40-bit per-predicate sequence | [I11](invariants.md#i11). Two ingest workers on different predicates share no counter, and `point()` routes to one tree by slicing the tag ([chapter 3](03-storage-model.md#factid-allocation-i11)). **Consequence to keep in view:** Glean's flat id space is *also* how it addresses stacked DBs; if stacking ever lands here (ops-I9), the sequence space is what would have to be carved |
| **Physical layout** | One store per DB | **One keyspace pair per predicate** (`keys.<id>`, `entities.<id>`) | Physical isolation makes bulk ingest embarrassingly parallel, and fjall's `ingest()` wants strictly ascending keys ([chapter 3](03-storage-model.md)) |
| **Schema versioning** | Versioned schema *sets* (`schema all.N`) with `evolves` and query-time projection | **One schema per DB, embedded and frozen at create**; compatibility is subset containment | [I13](invariants.md#i13). Immutability makes the simple answer the right one for P0; `evolves` is deferred with the compatibility checker structured around a canonical-model diff rather than hashes ([ops §11](aperture-cli-design.md)) |
| **Incrementality** | **Stacked DBs + ownership sets**, so an index stays fresh | **None.** No cross-DB query, no stacking, no ownership | [ops-I9](aperture-cli-design.md). The seam kept is "don't hardcode *predicate + key is the whole address*" in planner layers. The intended workflow is a fresh sealed artifact per run — cheap because a Complete DB is a tar-able file |
| **Ingest** | JSON/binary batches over Thrift | **Fact files** with sync markers, chunk-split and merged in parallel, and a **single write funnel** every writer passes | [ops-I4/I5](aperture-cli-design.md). Conflict handling is a deterministic *reject*, never last-writer-wins, because reproducibility is an invariant here ([Phase 7](../PLAN.md)) |
| **Negation** | Angle negates a *pattern* | focus negates a **statement** | It is the level `reorder` moves things at, so negation belongs where the ordering decision is ([`PLAN.md`](../PLAN.md) Phase 2). Consequence: `(!A) | B` is not expressible |
| **Design method** | — | An **invariant registry** with a numbered guard test each, and non-functional claims held by *mechanical* guards (allocation counters, decode probes, drop probes) | [testing.md](testing.md), [invariants.md](invariants.md). Not a Glean idea; it is how this project keeps a small team honest about a system whose bugs are silent |

---

## 3. Not decided — the honest gaps

### The type model is narrower than Glean's

Glean's types include `nat`, `byte`, `string`, `bool`, arrays `[T]`, records, sums, enums,
`maybe T`, predicate references and type aliases. `PredicateTy` has **four** constructors —
`Int`, `Str`, `Fact`, `Record` — plus unions, which are designed-for and not yet present
([chapter 6](06-types-and-schema.md)).

The codec has reserved marker bands in the right sort positions
([chapter 2](02-tuple-codec.md#the-marker-table)), so none of these is a one-way door *in the
encoding*. The one that is more than a missing constructor is **multiplicity**:

> Without arrays, a one-to-many relationship is modelled as **one fact per element** rather
> than a fact with a list field. That is often the better answer for an index — every element
> becomes independently seekable, and nothing has to decode a list to filter it — but it
> changes how every schema is *written*. Deciding it after schemas exist means rewriting them.

`bool` and `maybe` are sugar over a union once unions land. `nat`/`byte` are a range question,
not a shape one. Type aliases are a schema-syntax concern for [Phase 8](../PLAN.md).

**Status: undecided, and worth deciding before the schema DSL fixes what can be written.**

### Primitives and expressions

Angle has `prim.*` — arithmetic, string operations, comparisons — and if-then-else. Aperture
has string prefix matching (a seek), and **order comparisons are listed as deferred**
(`ResidualOp` arms). Arithmetic, string functions and conditionals are listed nowhere: the
nearest thing is Phase 6's derived binds, which are the machinery a computed expression would
run on. **Status: undecided.** The seam exists (a derived bind is a pure function of the fact
bindings), so this is additive when wanted.

### The idiomatic spelling of a join — closed

In Angle, nested fact patterns — `Knows { from = Person { id = 1 }}` — are *the* way one writes
a traversal. Phase 4 parsed and typechecked that and then deferred it, in three pieces of very
different size; **Phase 5 landed all three**
([`PLAN.md`](../PLAN.md#reaching-a-fact-through-a-reference--three-sizes-listed-apart--phase-5)), so the
nested spelling now compiles — to the *same plan* as the two-statement form, which is the sense
in which it is a spelling rather than a second way to run a query.

Aperture and Angle agree on the mechanism, too: a reference is followed by its **id**, so a
join through one reads no second fact. What Aperture still defers is reaching *through* a
reference to a field or value of the fact it names (`nyi/fact-field`), which is a lookup rather
than a compare. Angle does that freely; here it needs the `Access::Fetch` kind the IR has not
grown yet.

### What no aggregation means

Neither system aggregates in the query language; results are aggregated by the caller. Listed
only so its absence is visibly *shared* rather than an omission here.

---

## The three things to remember

1. **The storage and execution model is Glean's; the mechanisms are ours** — order-preserving
   tuple codec, snowflake ids, per-predicate keyspaces, and an abstract machine instead of a
   bytecode VM, each because a specific invariant asked for it.
2. **Immutability is the divergence everything else follows from.** No stacking, no ownership,
   no in-place evolution — and in exchange, free snapshots, byte-resumable queries and
   parallel ingest. If incrementality ever becomes a requirement, that is the trade being
   reopened, not a feature being added.
3. **The type model and the query surface are where we are still smaller than Glean**, and
   only some of that is written down as a choice. Arrays are the decision with a deadline: the
   schema DSL ([Phase 8](../PLAN.md)) is what freezes how schemas are written.

---

> [← Open decisions](open-decisions.md) · [Index](../README.md) · [Glossary →](glossary.md)
