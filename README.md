# Aperture

**Aperture** (the product: *Aperture DB*) is an embedded, immutable **fact database**.
**focus** is its typed, Datalog-flavoured query and schema language (Angle-inspired,
Glean-influenced). Facts are typed records identified by a `FactId`, grouped by predicate,
stored in an LSM (fjall) and queried by compiling focus queries to a nested-loop plan run
by a suspendable, pull-based virtual machine.

The database is **immutable**: a DB is built once (schema → base facts → derivations),
sealed, and thereafter only read. That single decision is what makes the rest of the
design tractable — snapshots are trivial, resume tokens can be plain bytes, and parallel
ingestion is "fearless."

> **Status.** Being taken from prototype to production. In `src/focus/`: the engine spine
> (codec, executor, resume, projection) and the fjall store are built and guarded, and the
> **front end reaches the `Plan`** — focus text parses, lowers, typechecks and flattens, with
> every construct deferred to a later phase drawing a diagnostic that names it. A query is now
> **answerable end to end**: `aperture` compiles what you type and runs it against a real
> store, joins *through fact references* included, and every supported construct in the corpus
> is checked against the rows it returns rather than only against the plan it produced.
> **Not yet built:** derived facts, ingestion, schema parsing and the operational layer. See
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
   keyspace per predicate, `FactId` allocation, and the atomic two-CF write. *(I11–I12.)*
4. [**The executor (the VM)**](docs/04-executor.md) — the plan IR, the register file, and
   the `enumerate` nested-loop driver. Why it's a defunctionalised state machine. *(I5–I7,
   I9.)*
5. [**Suspend & resume**](docs/05-resume.md) — the byte-only `Cursor`, how resume
   reproduces an uninterrupted run exactly, snapshot release, and cancellation. *(I4, I8.)*
6. [**Types & schema**](docs/06-types-and-schema.md) — `PredicateTy`, records, unions and
   their stable discriminants, and schema identity (canonical form + fingerprint). *(I10,
   I13.)*
7. [**Compilation**](docs/07-compilation.md) — lex → parse → typecheck → flatten → reorder,
   the tree layers, sargeability (seek · splice · residual), why identity reordering is
   *correct*, what flatten defers, and derived facts (the one deliberate machine change).
8. [**Operations**](docs/aperture-cli-design.md) — the CLI, the `Writable → Complete`
   lifecycle, the parallel ingestion pipeline, the wire protocol, and the operational
   invariants. *(ops-I1–ops-I10.)* The operational design of record.

**Reference docs (look up, don't read cover-to-cover):**

- [**Invariant registry**](docs/invariants.md) — every invariant (`I1`–`I13`,
  `ops-I1`–`ops-I10`) in one table: one-line statement, its guard test, and a link to the
  chapter that explains it. **The fastest way to check "what must I not break here."**
- [**Testing methodology**](docs/testing.md) — property-first, generator-first testing;
  the three property tiers; the invariant coverage ledger.
- [**Conventions & anti-patterns**](docs/conventions.md) — house style, and the things
  that look reasonable but are wrong here.
- [**Open decisions**](docs/open-decisions.md) — what's not yet settled (and where the
  settled ones landed).
- [**Aperture vs Glean**](docs/glean-comparison.md) — what we take from Glean, what we
  deliberately changed and why, and the capabilities we have **neither built nor ruled out**.
  Read it before proposing a feature Glean has.
- [**Glossary**](docs/glossary.md) — every term of art in one place.

---

## Two invariant namespaces (don't conflate them)

- **Engine invariants `I1`–`I13`** — codec, executor/resume, storage, identity. Explained
  in chapters 2–6, indexed in the [registry](docs/invariants.md).
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

Module map: `src/focus/` is the live engine and language — all new work lands there.
`src/main.rs` is the `aperture` focus shell, which compiles and runs what you type against a
real store. `src/focus.rs` is a commented-out graveyard. See
[Concepts](docs/01-concepts.md) for detail.
