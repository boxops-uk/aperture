# Conventions & anti-patterns

> [Aperture design book](../README.md) · reference doc

House style and the traps. The [invariants](invariants.md) say what must be true; this says
how to write code that keeps them true, and lists the things that look reasonable here and
are wrong.

---

## Working practices

- **Test-driven, property-first.** Write the property (and the strategy signature) first,
  watch it fail, then fill the impl. The property statement is the spec — name the invariant
  before writing the code. Every change ends in a passing test that proves the *specific*
  thing works; "it compiles" is not done. Full method: [testing](testing.md).
- **Every invariant owns a guard test, written up front** — even red / `#[ignore]`-pending
  ones. The [ledger](testing.md#the-invariant-coverage-ledger) is the coverage record.
- **Keep diffs reviewable in one sitting.** The dominant failure mode of work here is a
  large, mostly-correct diff whose 10%-wrong part is expensive to find. Small,
  self-contained, test-backed changes keep human review a real gate.
- **Non-functional criteria are part of *done*, and are *tested*** — not asserted. "No
  per-row heap allocation", "values never fetched in the scan loop", "resume pins no
  snapshot" each have a mechanical guard ([testing](testing.md#nfr-guards-are-mechanical-not-eyeballed)).
- **Respect the invariants absolutely.** Several look like implementation detail but are
  load-bearing or frozen on disk. If a change seems to require violating one, stop and flag
  it — don't "simplify" past it.

---

## Code conventions

- **Errors, not panics, on data paths.** Corrupt stored bytes must surface as a
  `StoreError` / `StoreCodecError` / `ApertureError` variant, never `unwrap`/`panic` — a bad
  byte shouldn't take down a connection. `unwrap` is acceptable only where an invariant makes
  it truly impossible, with a comment saying why. (`BadResumeKey`, `BadRecord`, and the
  codec's canonicalisation rejections are examples of this done right.)
- **A front-end phase reports by pushing, never by returning.** Diagnostics go into the
  compilation's `Diagnostics` sink (`aperture_engine::diag`); a phase returns its artifact and nothing
  else. A returned `Vec<Diagnostic>` is one a caller can quietly drop, which turns "every
  diagnostic reaches the user" into a convention each call site has to keep
  ([chapter 7](07-compilation.md#the-compilation-driver)). Report with a `Code`, not a
  string: the enum is the taxonomy, and a code that names nothing is a test passing for the
  wrong reason.
- **Ownership types signal sharing.**
  - `Box<[T]>` for owned-once inner structure (a `Plan`'s generators, a residual list).
  - `Arc<T>` **only** at genuine sharing boundaries (a `Plan` shared across a
    portal/executor; a frozen schema shared across queries). Don't reach for `Arc` by reflex.
  - `Arc<str>` / `Arc<[u8]>` for content deduplicated across many owners (interned names,
    cached encoded constants).
  - `ByteView` clones are refcount bumps, not copies — that's why a whole-row register is
    cheap to share ([I5](invariants.md#i5)/[I9](invariants.md#i9)).
- **Record fields are sorted `[(Symbol, T)]` slices, everywhere** — `Box<[…]>` when owned,
  `Arc<[…]>` when shared in the schema. **Never `HashMap`.** Deterministic order is required
  by order-preservation ([I1](invariants.md#i1)) and schema fingerprinting; one allocation
  and a linear scan beat hashing at record arities. ([chapter 6](06-types-and-schema.md).)
- **Permissive grammar, narrow later.** The grammar/parser stay uniform and permissive;
  meaningless constructs (wildcard in head, non-variable bind LHS, `.value` shadowing) are
  rejected at **typecheck/flatten** with clear diagnostics — not contorted into the grammar.
  ([chapter 7](07-compilation.md).)
- **Symbols are interned; runtime code is interner-free.** Two-tier interning (frozen
  `SchemaInterner` + per-query `Rodeo`), schema-first resolution; resolve to `&str`/`Arc<str>`
  at plan-build time. ([chapter 6](06-types-and-schema.md).)
- **Follow neighbouring code.** Match the surrounding comment density, naming, and idiom.

---

## Anti-patterns (look reasonable, are wrong here)

Each breaks a specific invariant — the reference is the point.

- **Materialising a full result set.** Defeats streaming/backpressure. Pull one row at a
  time; suspend on backpressure. ([chapter 5](05-resume.md).)
- **Decoding fields eagerly at bind time.** Breaks [I5](invariants.md#i5)/[I9](invariants.md#i9).
  Decode lazily at read sites.
- **Fetching a value inside the scan loop.** Breaks [I6](invariants.md#i6). Values come from
  `entities` at projection only.
- **Holding an iterator / `Slice` across a suspended portal.** Breaks [I8](invariants.md#i8)
  — pins a snapshot and a whole superseded generation.
- **Rewriting the `enumerate` driver as native recursion.** Breaks [I7](invariants.md#i7) —
  no more byte-resume.
- **Writing one column family without the other, or outside a batch.** Breaks
  [I12](invariants.md#i12) — half-present facts are silent corruption.
- **Hand-encoding a fact's key to reach `put_fact`.** Three of its preconditions fail
  *silently* — a record key is flat, the field order is the schema's, and only the schema
  says whether there is a value side — so the fact is written and then never found. Write a
  `aperture_store::fact::Fact` and use `FjallDb::put`, which resolves the fields by name
  ([chapter 3](03-storage-model.md#writing-a-fact-by-hand)). In particular `encode_typed` is
  *not* the key encoder: it keeps a record's wrapper, which is right for a value and wrong
  for a key.
- **Renumbering markers or union discriminants after data exists.** Breaks
  [I3](invariants.md#i3)/[I10](invariants.md#i10) — an on-disk migration. Likewise ingesting
  fact-typed fields before the [`FactRef` marker](open-decisions.md) was decided.
- **DNF-expanding disjunction across sibling conjuncts.** Exponential blow-up; use the
  `FlatDisjunction` node. ([chapter 7](07-compilation.md).)
- **Reshaping the core machine to add an "additive" feature.** If a feature seems to need a
  machine change, that's a signal to stop and reconsider — the *only* sanctioned machine
  changes are the two named ones (derived facts, the `FactRef` marker), each with its own
  phase and invariant. ([chapter 7](07-compilation.md), [PLAN.md](../PLAN.md).)
- **`HashMap` for record fields.** Non-deterministic order. Use a sorted slice.
- **`unwrap`/`panic` on decoded data.** Return an error variant.
- **Adding an invariant-critical feature without its guard test written first.**

---

> [← Testing](testing.md) · [Index](../README.md) · [Open decisions →](open-decisions.md)
