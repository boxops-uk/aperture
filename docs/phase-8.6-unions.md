# Phase 8.6 — Unions

> The last step of [Phase 8](phase-8-schemas.md), and the one that reaches the machine.
> `schema::discriminants_append_only` is the only `#[ignore]`d guard left in the ledger
> ([PLAN](../PLAN.md)), and [I10](invariants.md#i10) is the only invariant with no live guard.
>
> This file was the plan *before* the build, written because the expensive parts of unions are
> decided by the encoding rather than by the executor, and because two of the decisions are
> frozen the moment one union fact is written.
>
> **Built.** Every recommendation below was taken as written. What the plan got wrong is recorded
> in §10, which is the reason to write one: four things surfaced only in the doing, and one of
> them was that [I10](invariants.md#i10)'s guard could not be built as specified.

Already settled or already built, and not re-argued here:

- **The declaration syntax parses.** `{ a : int = 0 | b : string = 1 }` is `Rule::SumFields` in
  [`grammar.llw`](../crates/fjord-schema/src/syntax/grammar.llw), refused by name in
  [`lower::braced`](../crates/fjord-schema/src/syntax/lower.rs) with `nyi/union`, and pinned by a
  corpus entry. An alternative's discriminant is explicit because [I10](invariants.md#i10) needs
  somewhere to write the number down — the deliberate divergence from Angle, which numbers by
  position.
- **The select syntax parses.** `X.alt?` is `ExprKind::Select`, distinct from `Access`, drawing
  `nyi/union-select` at typecheck with a corpus entry that says what it will lower to.
- **D5's three recommendations** ([phase 8](phase-8-schemas.md#d5--unions-three-sub-decisions-all-frozen-the-moment-one-fact-is-written)):
  explicit append-only discriminants, an unknown tag is an error rather than a synthetic
  alternative, and marker `0x52` appended after `MARK_FACT_REF`. This file turns them into
  encodings and checks.

---

## 0. What 8.6 ships

`PredicateTy::Union`, the codec's `0x52`, a union as a value and as a pattern, `X.alt?`, the four
checks I10 actually decomposes into (§6), and a non-Rust client that can write one.

**Not** in 8.6: `maybe`, `enum` (both sugar over a union, both with a naming decision that enters
the fingerprint), arrays, a union in `schemas/code.sigla`, binding a discriminant as a *value*,
and matching "any alternative" (§9).

---

## 1. The encoding decides how much of this touches the machine

Three sub-decisions. The first one is the whole plan: it decides whether `skip` grows a state
machine and whether `FieldPath` changes shape.

### D-a — A union value is a **terminated group**: `0x52 <disc> <payload> 0x00`

Recommended: **terminated**, escaping nulls inside exactly as a record does.

A union is then a record with a tag and one element, and every reader already knows what to do
with it:

- [`skip`](../crates/fjord-encoding/src/tuple.rs) gains an arm that is `MARK_RECORD`'s plus
  "consume the discriminant" — the existing depth counter and terminator logic close it. **None
  of the seven `if record_depth == 0 { return Ok(i) }` sites move.**
- `nested_field_span` ([`iter.rs`](../crates/fjord-engine/src/iter.rs)) walks it as it walks a
  record: assert the marker, step past it, take element 0.
- "Inside a group" stays one concept: a group is terminated **and** escapes nulls. The encoder's
  `record_depth` becomes `group_depth` and nothing else about escaping changes.

The alternative is **unterminated** — `0x52 <disc> <payload>`, self-delimiting already, one byte
cheaper in every stored key forever. It is rejected because of where the cost lands: a value with
a payload owed but no terminator means `skip` must carry a per-depth count of *values still owed*,
and every completion site in it becomes "complete a value, decrement, return only if nothing is
owed and the depth is zero". That is a rewrite of the codec's most safety-critical function
([I2](invariants.md#i2)), whose failure mode is a wrong field offset — a silently wrong answer,
not an error. It also splits "escapes nulls" from "has a terminator", because a null payload must
still be escaped when the union sits inside a record. One byte per union value is the right price
for neither of those.

### D-b — The discriminant is `put_u64` / `get_u64`

Recommended: **the codec's existing unsigned integer encoding**
([`tuple.rs`](../crates/fjord-encoding/src/tuple.rs)), not a fixed-width field of its own.

It is order-preserving and self-delimiting already, has its own batteries, cannot represent a
negative tag (so "a negative discriminant" is unrepresentable rather than a validation), costs one
byte for `= 0` and two up to 255, and imposes **no cap** on a discriminant — which matters because
explicit tags invite protobuf habits (`= 10`, `= 20`, reserved ranges). A fixed `u16` would make
the payload's offset a constant instead of one `get_u64`; that is not worth a cap on a number a
schema author chooses and cannot change later.

Consequence for [I1](invariants.md#i1), as D5 asked: a union sorts after every other type (its
marker is the highest), and within a union by discriminant then payload — so a key's alternatives
**cluster**, and a select on a leading key field is a prefix of the key order (§3).

### D-c — An unknown discriminant is an error

`StoreCodecError::UnknownDiscriminant { tag }`, surfaced as an `FjordError` — never a synthetic
`unknown` alternative. Glean needs one because it projects between schemas at query time;
[I13](invariants.md#i13) means Fjord never has two schemas to project between. Two sites raise it:
`decode_typed_at`, and the payload walk of §3's D-d.

---

## 2. The query surface needs no new syntax

Both spellings already parse, and that is not luck — it is the same shape Angle uses.

| written | is | lowers to |
|---|---|---|
| `{ alt = p }` | the **injection** — a union value, in a key pattern or on the way in | seek bytes if `p` is constant, else a residual |
| `{ alt = _ }` | the same, payload unconstrained | the discriminant as a **seek prefix** |
| `X.alt?` | the **select** — match this alternative and bind its payload | a `DiscriminantEq` residual plus a payload read |

An injection is a one-field `anon_record_primary`, which means it is only a union **against an
expected type**: checked, never inferred. Every position that can hold one has a declared type —
a fact's key field, its value side — and `ty::Checker` is already bidirectional (`check` beside
`infer`), so this is a rule rather than a coercion. Inferred with no expectation it stays a
record and unification reports the mismatch; the diagnostic for a one-field record meeting a union
is worth writing by hand.

**Flatten needs no annotation to tell the two apart**: it already walks `(PredicateTy, ExprKind)`
in pairs (`constant`, the key-field walk, `resolve`, `dereference`), so a union-typed position
holding a one-field record *is* the injection at the point where the decision is needed.

---

## 3. The executor

This is the part the phase was sequenced for, and with D-a taken it is small.

| site | change | why not more |
|---|---|---|
| `nested_field_span` | accept `MARK_UNION`: consume marker + discriminant, then element 0 | a union is a group of one; the record walk already bounds itself to the field |
| `check_residuals` | one arm: `ResidualOp::DiscriminantEq(u32)` | a byte-prefix compare against `[0x52] ++ put_u64(disc)` — borrowed span, no decode, no allocation |
| `project` / `decode_typed_at` | a `Union` arm | projection is already type-driven |
| `build_prefix` | **nothing** | the discriminant is constant bytes: `SeekKeyPart::Bytes` |
| `Source`, `Step`, `Level`, `StackFrame`, `Cursor` | **nothing** | a select is a filter and a payload read; neither iterates |

So: no new frame kind, no new step kind, no cursor entry kind, no change to what a level counts.
[I4](invariants.md#i4)/[I7](invariants.md#i7)/[I8](invariants.md#i8) are untouched **by
construction** — the battery is re-run, not re-derived. [I5](invariants.md#i5) and
[I6](invariants.md#i6) hold because a payload is a span of the register's own key bytes and no
alternative needs a point read; [I9](invariants.md#i9) holds because the discriminant compare is
against a stack buffer, like `EqRegisterFactId`'s.

### D-d — A payload step is a `FieldPath` step whose index **is** the expected discriminant

`FieldPath` is `field` plus `nested: Box<[usize]>`, it is in the fixed contract, and the resume
fingerprint hashes it. Three ways to name a payload in it:

- **A** — step `0`, the payload being element 0. Zero change, and no check: a path stepping into
  the wrong alternative reads another type's bytes at that offset and answers with whatever was
  there. That is the silent fault the `FactRef` marker split and `Source::Fetch`'s declared
  `predicate_id` both exist to prevent.
- **B** — `nested` becomes a step *enum* (`Field(usize) | Payload(u32)`). Says it in the
  representation, at the cost of changing the fixed contract, every construction site, `Display`,
  and the fingerprint (where a `Payload` and a `Field` must not hash alike).
- **E — recommended.** Keep `Box<[usize]>` and let a step at a *union* position mean **the
  expected discriminant**, checked against the stored one. A step's meaning already depends on the
  constructor it lands on (at a record it is a field index; at a scalar it is an error); at a
  union it is a tag. A mismatch is `FjordError::DiscriminantMismatch` — loud, and unreachable from
  a compiled plan (below). No representation change, and the fingerprint distinguishes two selects
  on one field for free.

Give it a named constructor — `FieldPath::payload(&self, disc)`, one line over `then` — so a call
site says which meaning it intends. The wart is `Display`: `0.3` prints a tag where a reader
expects a field index. `print` holds the schema and can render `0.sym`; the bare `Display` is a
test-and-diagnostic rendering and can carry the ambiguity.

**Why the error is unreachable from a compiled plan.** Residuals within a source are ordered and
short-circuit, so flatten emits the `DiscriminantEq` **before** any residual reading through that
field's payload; a seek on an injection puts the discriminant in the prefix, so a non-matching row
is never scanned; and a path rooted at *another* register reads a row whose own level already
filtered it. Every payload read is therefore preceded by its check on the same row. That is an
obligation on flatten, not on the machine, so it gets a unit test on the emitted order rather than
a comment.

### A select is a filter wherever it is written

`X.alt? where X = test.Foo _` — the corpus entry — puts the select in the **head**. A head cannot
filter, so the `DiscriminantEq` is hoisted into the residuals of the level that binds `X`, and the
head projects the payload path. This is the first head expression that implies a filter, and it
has one edge worth a test: when that level has several sources (a disjunction), the residual goes
into **each** source, with the path computed per source — two sources are two key layouts, which
is exactly why residuals hang off a `Source` and not a `Level`.

### Sargeability, and what is not built

`test.Foo { x = { sym = "f" } }` is one seek: `0x52` ++ tag ++ the payload's bytes. `{ sym = _ }`
is the same seek one part shorter. A select written as a *statement* over a variable a later level
captures (`test.Foo X; X.alt?`) is a **prefix constraint keyed by a variable**, which is the shape
`X = "a"..` already has — so it could narrow that level's seek through machinery that exists.
Noted, not built: 8.6 filters it.

---

## 4. The plan IR and the fingerprint

- `Fingerprint::ty` gains arm `4` for `Union`, hashing each alternative's **discriminant** and
  payload shape, with names skipped for the reason a record's are.
- `ResidualOp::DiscriminantEq` gets a **distinct tag**, the rule `NotEqConst`/`NotPrefix` are
  already under: two plans differing only in a residual's kind must not accept each other's
  cursors.
- `Fingerprint::path` needs no change under D-d. A discriminant `0` and a field index `0` hash
  alike, and that collision is unreachable: which one a step means is decided by the declared type
  at that position, and a predicate's field is a record or a union but never both.
- The cursor's **layout version does not change**. Nothing about an entry, or about how many
  entries a plan has, moves.

---

## 5. Everything else that must answer "what is a union"

An exhaustive match is the good case — the compiler finds it. These are the ones to look for
instead:

**Twelve `let PredicateTy::Record(..) = .. else` sites take the not-a-record path silently.** Each
must decide whether a union is refused there or handled:
`fjord-schema/src/schema.rs:44` (`find_field`), `syntax/lower.rs:715,738,756`,
`fjord-engine/src/flatten.rs:1546,2145,2896,4245,4317`, `print.rs:540,641`,
`fjord-cli/src/sample_schema.rs:299`.

| where | what it must say | failure mode if missed |
|---|---|---|
| `tuple.rs` | `skip`, `encode_typed_at`, `decode_typed_at`, an encoder/decoder `union` combinator beside `record`, `Value::Union`, `rank()` in `Ord` | order or skip wrong ⇒ silently wrong rows |
| `tuple.rs` golden | `MARK_UNION = 0x52` appended to the marker table test | [I3](invariants.md#i3): the edit is deliberate, never a renumber |
| `tuple.rs` fixtures | `TySpec`/`arb_*` gain a union arm | union bytes get none of the generative coverage the rest has |
| `fjord-wire/value.rs` | `WireValue::Union { disc, payload }`, encode and decode | a producer cannot write one |
| `fjord-wire/desc.rs` | `Desc::Union` and `TAG_UNION = 4`, carrying alternative **names** (a peer has no interner) | a query head returning a union cannot be described |
| `fjord-ingest/intern.rs` | walk into a payload | a nested reference inside a payload is never interned |
| `fjord-store/fact.rs` | resolve an alternative *name* to its discriminant | a hand-written fact cannot hold a union |
| `fjord-store/identity.rs` | `TAG_UNION` appended; hash the **discriminant**, not the name | `ops-I4` disagrees across two builds of the same facts |
| `fjord-client/expand.rs` | `references` and `substitute` walk payloads | `:expand` silently under-expands |
| `fjord-server/rows.rs`, `engine/print.rs`, `cli/output.rs` | render `{ "alt": payload }`, and a union type in `:type` | unreadable output |
| `fjord-schema/syntax/print.rs` | print a union back | `create --schema` refuses a schema it cannot reprint (8.4's round trip) |
| `fjord-schema/fingerprint.rs` | discriminants in the canonical form | a renumber does not move the number, and I10 has no teeth |
| `clients/dotnet` | `WireValue`/`Desc` arms in the C# codec, and a union in the golden corpus | the protocol is not implementable from outside — the one thing the Rust tests cannot answer |

The wire's two tag tables (`desc`, and the value encoding) are **appended** to, so an older peer
meets an unknown tag and errors rather than mis-decoding. Say that in the protocol chapter rather
than leaving it to be discovered.

---

## 6. I10, restated so it can be guarded

The invariant reads *"a schema edit that renumbers or reuses a discriminant is rejected at
load"*. That cannot be implemented as written: under [I13](invariants.md#i13) a database's schema
is frozen at create and there is no second schema at load to compare it against. The four checks
that **are** implementable, and together mean what I10 means:

1. **Within one schema** — two alternatives of one union may not share a discriminant. A schema
   error at `lower`, with a span. (This is the only one that is a "load".)
2. **Identity** — a discriminant is part of the canonical form, so renumbering moves the
   per-predicate and whole-schema fingerprint. With a negative control: a schema that differs only
   in a tag must **not** fingerprint alike.
3. **`schema diff`** — a renumbered or reused discriminant is `Breaking`, with the reason naming
   the alternative. An *appended* alternative is also Breaking under subset containment, and must
   say so distinctly: it is I10-safe and still moves the predicate's identity.
4. **Decode** — a stored discriminant no alternative declares is `UnknownDiscriminant`, not a
   mis-decode (D-c).

Worth writing into the invariant while it is being made live: **what I10 buys under I13.** Not
cross-schema compatibility — the fingerprint handshake already refuses that. What it buys is that
a schema's *edit history* keeps every fact any earlier version of it wrote meaning the same thing,
which is what makes an appended alternative a rebuild rather than a reindex, and what a future
export or migration path would have to stand on.

---

## 7. Sequence

Codec first, because everything downstream is frozen by it, and the generative batteries are the
only thing that will catch a mis-skip. Each step ends green.

- **8.6.1 — Decisions.** ✅ D-a, D-b, D-c, D-d recorded in
  [`open-decisions.md`](open-decisions.md) and this file; D5's recommendations discharged.
- **8.6.2 — The codec.** ✅ `MARK_UNION`, encode/decode/skip/order, `Value::Union`, the golden
  marker table edited deliberately, and the `arb_*` union arm. *Done when* I1 order-preservation
  and I2 skip-without-a-schema are green over generated union values, including the single-
  alternative and empty-payload cases [testing](testing.md) already names.
- **8.6.3 — The type model and the DSL.** ✅ `PredicateTy::Union`, `lower::braced`'s sum path (a
  missing `: ty` is the empty record `{}`, which is what `enum` will desugar to), the twelve
  let-else sites, `print`'s round trip, the canonical form, `diff`, and §6's four checks. *Done
  when* `discriminants_append_only` is green and un-ignored — **the last `#[ignore]` in the
  ledger.**
- **8.6.4 — Typecheck and flatten.** ✅ `Ty::Union`, the select, the checked injection, the
  `DiscriminantEq` residual and its emitted order, the seek, the head hoist.
- **8.6.5 — The executor.** ✅ §3's three sites. *Done when* the `nyi/union-select` corpus entry is
  `Supported` with its rows and `Code::NyiUnionSelect` is retired from `Code::ALL`.
- **8.6.6 — The batteries.** ✅ (the *plan* generator; the query generator's model stays
  scalar-only — see below) The schema-first `(plan, store)` generator gains unions, and the
  resume battery is re-run on fjall and `MemStore`. This is where I4–I9 get their union coverage,
  and it is not optional: nothing else exercises a payload path through the whole machine.

  **What was built, and the one thing that was not.** The `(plan, store)` generator draws
  `FieldTy::Union` and a `DiscriminantEq` residual, and a census asserts both are *reached*
  (`iter::the_battery_reaches_a_union_key_and_a_tag_check`) — so the resume battery, the fjall
  differential arm and the NFR guards all run over union keys. The **query** generator does not:
  it compares the executor against a model written in plain Rust, and that model reasons about
  prefixes, constants and variable types field by field. Teaching it unions at the same time as
  teaching the compiler them would leave the oracle and the code under test learning one
  constructor together, which is an oracle worth less. The compiled paths are covered instead by
  the corpus — nine entries, resumed at every cut point — and by the union laws.
- **8.6.7 — Out through the wire.** ✅ `WireValue`, `Desc`, `intern`, `expand`, rendering, then the
  .NET client and a union in the golden corpus — stated independently on each side, as it is now.

---

## 8. The one-way doors

| | frozen the moment | recovered by |
|---|---|---|
| `MARK_UNION = 0x52` | the first union fact | nothing — [I3](invariants.md#i3) forbids renumbering |
| The group form and terminator (D-a) | the same moment | a codec version, i.e. a different `codec` stamp ([I15](invariants.md#i15)) |
| The discriminant encoding (D-b) | the same moment | the same |
| A schema's discriminants (D5/I10) | the first union fact **that schema** writes | an on-disk migration |
| `identity.rs`'s `TAG_UNION` | the first `finish` of a database holding one | recomputing every artifact's identity |
| Alternative names in the canonical form | the first artifact ships | a fingerprint version |

---

## 9. Not in 8.6, deliberately

- **`maybe` and `enum`.** Both are sugar over a union, and both need a *naming* decision that
  enters the fingerprint (what are `maybe`'s two alternatives called, and which tags do they
  take?). Cheap after 8.6, wrong to guess during it.
- **A union in `schemas/code.sigla`.** [Phase 11](phase-11-code-search.md) wants one for a
  multi-language `code.Entity`. It is a flag day — the sample schema's fingerprint moves, and the
  two .NET constants and the golden move with it — so it is its own commit, after 8.6 is green.
- **Binding a discriminant as a value**, and matching *any* alternative. Both are real; neither is
  needed to write or read a union, and each is a new question about what a query can name.
- **A sargeable select in statement position** (§3). The existing prefix-constraint path would
  take it; 8.6 filters instead.

---

## 10. What the plan got wrong

Four things, none of them a decision — the decisions held. Recorded because a plan whose misses
are not written down reads, next time, as a plan that had none.

1. **The claims walk had to learn about injections.** `scan_field` decides which variables a
   generator *captures*, and it knew records and not unions — so
   `test.Tagged {what = {num = X}, id = _}` came back "nothing binds `X`", for a query that
   plainly does. Nothing in §3's account of the executor would have led anywhere near it: the
   safety check runs before a plan exists.
2. **`print` needed a trailing `|` for a union of one.** Braces are shared with a record and the
   separator after the first field is what tells them apart, so `{ only: string = 0 }` reads back
   as a *record*. `create --schema` proves the round trip before anything is written, so this
   surfaced as a schema that would not survive being embedded — which is exactly where it should
   surface, and only because the round trip is a test over the whole corpus.
3. **Typecheck now defers nothing at all.** Union select was its last `nyi/`, which turned a test
   named `deferred_constructs_report_themselves` into a claim worth making the other way round:
   what still carries an `nyi/` code is flatten's, and the corpus is where those live.
4. **I10's guard could not be built as specified**, which §6 predicted and the build confirmed in
   the sharpest way: writing the four checks out was the only way to see that the fifth — the one
   the invariant actually named — has no subject. The registry carries the corrected wording.

One thing the plan predicted and undersold: the **twelve silent let-else sites** were the whole
of the risk. Every other site the compiler pointed at. The estimate of "29 files" came to 36, and
the extra seven were tests and renderings — the cheap kind.

---

> [← 8. Schemas](phase-8-schemas.md) · [Index](../README.md) · [Invariants](invariants.md)
