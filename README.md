# Aperture

**Aperture** (the product: *Aperture DB*) is an embedded, immutable **fact database**.
**focus** is its typed, Datalog-flavoured query and schema language — a small, faithful subset of
Glean's Angle at the core, and its own thing past that ([what is inherited and what is
not](docs/glean-comparison.md)). Facts are typed records identified by a `FactId`, grouped by
predicate, stored in an LSM (fjall) and queried by compiling focus queries to a nested-loop plan
run by a suspendable, pull-based virtual machine.

The database is **immutable**: a DB is built once (schema → base facts → derivations),
sealed, and thereafter only read. That single decision is what makes the rest of the
design tractable — snapshots are trivial, resume tokens can be plain bytes, and parallel
ingestion is "fearless."

> **Status.** Being taken from prototype to production. In `crates/aperture-engine/`: the engine spine
> (codec, executor, resume, projection) and the fjall store are built and guarded, and the
> **front end reaches the `Plan`** — focus text parses, lowers, typechecks and flattens, with
> every construct deferred to a later phase drawing a diagnostic that names it. A query is now
> **answerable end to end**: `aperture` compiles what you type and runs it against a real
> store, joins *through fact references* included, and every supported construct in the corpus
> is checked against the rows it returns rather than only against the plan it produced. Facts
> are **written by hand as well-typed values** whose fields are resolved against the schema, so
> the shell's store is a code index built through the same API a deriver would use.
> A register now holds a **`Slot`** — a stored row or a computed value — and a plan's body is an
> ordered sequence of **steps**, so a value can be derived mid-query and *recomputed* rather than
> saved when a query suspends ([I14](docs/invariants.md#i14)); a bind to a constant is folded
> instead, at compile time.
> Since then the deferred query surface compiles (`|`, `never`, `!`, subqueries), the wire
> protocol and the operational layer are built — a database is created, written to, queried,
> sealed and removed against a running server — and **schemas are files**: a database is created
> against one, embeds it, and is served from that copy ([I13](docs/invariants.md#i13)).
> **Not yet built:** union types, bulk ingestion from files, and **stored** derivation. See
> [`PLAN.md`](PLAN.md) for the sequence and current state.

---

## The design book — read it in order

This documentation is a **book**: start here, follow the chapters, and each builds on the
last until you understand every aspect of the design, every invariant, and the reason
behind it. Each chapter is self-contained enough to load on its own when you only care
about one subsystem.

**Read these in order for the full picture:**

1. [**Concepts**](docs/01-concepts.md) — the fact model, predicates, `FactId`, the focus
   language at a glance, and the compilation pipeline. The mental model everything else
   refines. *Start here.*
2. [**The tuple codec**](docs/02-tuple-codec.md) — how values become order-preserving,
   self-delimiting bytes. The marker table and why it's frozen. *(Invariants I1–I3.)*
3. [**The storage model**](docs/03-storage-model.md) — the two column families, one
   keyspace per predicate, `FactId` allocation, the atomic two-CF write, the **format
   stamp** that says which encoding wrote a DB, and how a fact is **written by hand** (the
   three silent traps in `put_fact` that `aperture_store::fact` exists to close). *(I11–I12, I15.)*
4. [**The executor (the VM)**](docs/04-executor.md) — the plan IR, the register file, and
   the `enumerate` nested-loop driver. Why it's a defunctionalised state machine. *(I5–I7,
   I9.)*
5. [**Suspend & resume**](docs/05-resume.md) — the byte-only `Cursor`, how resume
   reproduces an uninterrupted run exactly, snapshot release, and cancellation. *(I4, I8.)*
6. [**Types & schema**](docs/06-types-and-schema.md) — `PredicateTy`, records, unions and
   their stable discriminants, and schema identity (canonical form + fingerprint). *(I10,
   I13.)*
7. [**Compilation**](docs/07-compilation.md) — lex → parse → typecheck → flatten → reorder,
   the tree layers, sargeability (seek · splice · residual), why the runnable frontier is
   *complete* — and load-bearing for acceptance, not just speed — what flatten defers, folding
   a constant bind, and derived facts — the two kinds, and which of them was the machine
   change. *(I14.)*
8. [**Operations**](docs/aperture-cli-design.md) — the CLI, the `Writable → Complete`
   lifecycle, the parallel ingestion pipeline, the wire protocol, and the operational
   invariants. *(ops-I1–ops-I10.)* The operational design of record.

**Reference docs (look up, don't read cover-to-cover):**

- [**Invariant registry**](docs/invariants.md) — every invariant (`I1`–`I14`,
  `ops-I1`–`ops-I10`) in one table: one-line statement, its guard test, and a link to the
  chapter that explains it. **The fastest way to check "what must I not break here."**
- [**Testing methodology**](docs/testing.md) — property-first, generator-first testing;
  the three property tiers; the invariant coverage ledger.
- [**Performance method & target**](docs/performance.md) — the measurement ladder, what
  makes a number reportable, and the **capacity target** every number is read against.
  Its companion is [`bench/FINDINGS.md`](bench/FINDINGS.md), which is what has actually
  been measured, at what size, and what acting on it would cost.
- [**Conventions & anti-patterns**](docs/conventions.md) — house style, and the things
  that look reasonable but are wrong here.
- [**Open decisions**](docs/open-decisions.md) — what's not yet settled (and where the
  settled ones landed).
- [**Aperture vs Glean**](docs/glean-comparison.md) — what we take from Glean, what we
  deliberately changed and why, which invariants are **ours** rather than inherited, and the
  capabilities we have **neither built nor ruled out**. Read it before proposing a feature Glean
  has, and before claiming a design here came from there.
- [**Capabilities, efficiency & cost vs Glean**](docs/glean-capabilities.md) — the other axis:
  what each system can be *asked* to do, what each *spends* doing it, and what each *charges*.
  Holds the cross-database identity answer (how Glass handles fact ids from different DBs), a
  ranked list of efficiency mechanisms worth taking, and the one place our cost model is
  genuinely thinner rather than differently shaped.
- [**Glossary**](docs/glossary.md) — every term of art in one place.

---

## Two invariant namespaces (don't conflate them)

- **Engine invariants `I1`–`I14`** — codec, executor/resume, storage, identity, and
  derived-bind purity. Explained in chapters 2–7, indexed in the [registry](docs/invariants.md).
- **Operational invariants `ops-I1`–`ops-I10`** — lifecycle, single-writer ownership,
  reproducibility, the one-write-funnel. Explained in [Operations](docs/aperture-cli-design.md).
  Always written `ops-Ix` so they're never mistaken for the engine `Ix`.

---

## Build & test

```
cargo build
cargo test                          # the green suite
cargo test -- --ignored --list      # the invariant coverage ledger (guards not yet live)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

## Working on Aperture

- [`CLAUDE.md`](CLAUDE.md) — the working contract loaded every session: how to work here,
  the invariants in brief, conventions. *(Will be slimmed to point into this book.)*
- [`PLAN.md`](PLAN.md) — the living phase tree: the build sequence and current state.

Module map: `crates/aperture-engine/` is the live engine and language — all new work lands there.
`src/main.rs` is the `aperture` focus shell, which compiles and runs what you type against a
real store — seeded with a real index of the Python corpus in [`example/`](example/README.md).
`crates/aperture-engine/src/lib.rs` is a commented-out graveyard. See [Concepts](docs/01-concepts.md) for detail.
