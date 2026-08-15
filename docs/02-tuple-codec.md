# 2. The tuple codec

> [Aperture design book](../README.md) · [← 1. Concepts](01-concepts.md) · **Chapter 2** · [3. The storage model →](03-storage-model.md)

The **tuple codec** turns typed values into bytes. It is the foundation the entire storage
model stands on, and it has three properties that must all hold at once: encodings are
**order-preserving**, **self-delimiting**, and (once data exists) **frozen**. This chapter
explains each, why it matters, and the marker table that implements them.

Code: `crates/aperture-encoding/src/tuple.rs`. Tests there are the project's densest — the codec is the most
property-tested subsystem, for the reasons below.

> **Naming.** This is the **tuple codec** (FoundationDB-*inspired*, not FDB-compatible —
> don't call it "FDB"). It encodes both keys and values (see
> [chapter 3](03-storage-model.md)). A separate **transport/wire codec** applies to rows
> *after* they leave the executor and carries none of these constraints — see
> [Operations](aperture-cli-design.md) and [open decisions](open-decisions.md).

---

## Property 1 — Order-preserving ([I1](invariants.md#i1))

**The invariant:** for all values `a`, `b` of the same type,

```
memcmp(encode(a), encode(b)) == semantic_compare(a, b)
```

Byte order *is* semantic order. Two claims run together here and have to be kept apart. An
**exact-prefix** scan — every fact of predicate `P`, or every fact whose leading key fields
equal given values — needs only that each encoding be self-delimiting and **canonical**: one
value, one byte string, so the matching rows are exactly the rows carrying that byte prefix.
Order-preservation buys the *other* thing: a **value-range** scan (`X > 3` as a bounded seek
rather than a filter) and rows that arrive in semantic order with no sort. Point lookups and
joins reduce to the first, ranges and ordered output to the second; either way there is no
secondary structure and no per-query sort.

Glean is the proof that the two are separable. Its fact keys carry LEB128 varints, which
mis-order — 255 encodes `FF 01`, 256 encodes `80 02`, and `FF > 80` — and it serves a
production fact database on exact-prefix seeks all the same. It *owns* an order-preserving
prefix-varint, whose header states memcmp-comparability as requirement #1
(`glean/rts/nat.h:20-64`), and spends it only on storage-level keys, never inside a fact key;
then it dropped its reliance on ordered iteration deliberately, to support backends with
limited key sizes, and now documents a prefix iterator as returning facts "in no specified
order". *Evidence:* [the Glean comparison](glean-comparison.md).

So I1 is **a divergence of this design's own, and the divergent half of it is currently
unspent**: `ResidualOp` (`crates/aperture-engine/src/plan.rs`) has no ordering arm, and `<` and `>` are not
lexer tokens, so an order comparison does not even lex. The bet is kept because it costs
almost nothing to hold and cannot be retrofitted — the marker table freezes the moment data
exists ([I3](invariants.md#i3)), and the [format stamp](03-storage-model.md#the-format-stamp-i15)
buys a *future* codec a number, not a migration for the one already written. What *is*
spent today is the weaker, store-level half of the same property: the scan is lexicographic,
and resume re-seeks against that order
([chapter 3](03-storage-model.md#the-order-a-scan-is-promised-in)).

Get this wrong by one byte and queries silently return wrong rows — no crash, no error.
That is why order-preservation is property-tested against an **independent comparator**
(not the code under test) over generated value pairs, and why it is **the gate for any
codec change whatsoever.**

### How integers preserve order

Integers are the hard case (strings sort naturally as bytes; integers do not — two's
complement puts negatives above positives, and fixed width wastes space). The scheme:

- **Variable-width, minimal-magnitude.** A small number uses few bytes; the width is
  carried *in the marker byte*, so the marker sorts first and orders values across widths.
- **Negatives use the ones'-complement of the magnitude**, so that "more negative" sorts
  "smaller."
- **Wider negatives get *smaller* markers**, so a very negative number (needing more bytes)
  sorts below a less negative one.
- **`i64` and `u64` share the positive band** — a positive value has one encoding whether
  its static type was signed or unsigned.

The **decoder is a canonicalising validator**: it recomputes the width from the decoded
magnitude and *rejects any non-minimal encoding*, and rejects out-of-range values. This is
essential — it means **one value ⇒ exactly one legal byte string** (a bijection). Without
canonicalisation, two byte strings could decode to the same integer, breaking the
order-preservation guarantee (which is stated over encodings) and the round-trip property.

---

## Property 2 — Self-delimiting ([I2](invariants.md#i2))

**The invariant:** the marker byte alone tells you how to advance past a value — no schema
required.

The executor skips over fields it doesn't care about — to reach field 3 of a key it walks past
fields 0–2 — with **no type information** in hand, deep in the scan hot loop. That is a
consequence of [I7](invariants.md#i7), not a law of encodings: the alternative is a compiler in
the loop. Glean's fact encoding is **untagged and positional** — a record is the bare
concatenation of its fields, no wrapper and no terminator — and its skip is **schema-driven
codegen**, a per-predicate bytecode traversal emitted from the type
(`glean/hs/Glean/RTS/Traverse.hs:27-119`), which goes one better than any tag-driven skip can:
it proves a whole suffix holds no fact references and never emits the walk at all. Against a
compiler, tags are pure overhead — a byte per field on disk and a branch per field in the loop
([the comparison](glean-comparison.md) has the byte arithmetic).

What a tag buys instead, and the reason to keep it, is that **the bytes can be walked without
the schema**: a golden-bytes test, a dump of a row whose predicate is unknown, the diagnosis of
a corrupt key are each a `skip` loop over untyped bytes. The tag is also where the byte-level
`Int`/`Fact` distinction lives — `MARK_FACT_REF` has a marker of its own, so an id is
distinguishable from an integer *in the bytes* rather than only in the schema that was consulted
when they were written, which is the class of silent mismatch
[chapter 3](03-storage-model.md#writing-a-fact-by-hand) is built to prevent.

So every encoding carries its own shape. There are exactly **three skip-shape families**:

1. **Fixed-width** — the marker implies a fixed number of payload bytes (e.g. zero has
   none; a fact reference has 8).
2. **Terminator-walk** — read until a terminator byte (strings).
3. **Width-in-marker** — the marker encodes the payload width (integers).

`skip(bytes, pos)` reads the marker at `pos`, applies the right family, and lands
**exactly** at the start of the next value. "Exactly" is a testable property: `skip` must
land on the next value's first byte, and a *full decode must consume exactly to
end-of-input* (trailing bytes are an error). Both directions are property-tested over
nested values (this is [tier-2 metamorphic testing](testing.md)).

### Records and the null/terminator subtlety

A **record** is `MARK_RECORD <element> <element> … MARK_TERM`. Skipping a record means
walking its interior in **nested mode** until the matching terminator.

There is one sharp edge: the terminator byte is `0x00`, and a bare **null value** is also
`0x00`. To tell "a null *element* inside a record" from "the record *terminator*," a null
element is **escaped as `0x00 0xFF`**. In nested mode, `skip` treats a `0x00` *not*
followed by `0xFF` as the terminator, and `0x00 0xFF` as a null element. This is why the
codec has both `MARK_NULL` and `MARK_ESCAPE`.

The one-byte terminator has a second consequence, and it is a **requirement on the marker
table** rather than a property of strings. An embedded NUL inside a string escapes the same way,
so within a record `encode("a") < encode("a\0")` compares the shorter string's terminator
against the longer one's escape byte — and holds only because the byte that follows a
terminator, the *next field's marker*, is below `0xFF`. Every marker a value can begin with is
at most `0x51` and the reserved bands stop at `0xFE`, so it holds by construction; it is stated
on [the table](#the-marker-table) because nothing else keeps it true. Glean spends two bytes on
a terminator (`00 00`, with embedded NUL escaping to `00 01`, `glean/rts/string.h:16-26`) and
gets the same ordering argument *locally*, with no dependence on what follows.

The rest of the string encoding is genuine convergence, arrived at twice: escape NUL,
terminate, sort by `memcmp` — and build a prefix seek by encoding the prefix and **dropping the
terminator**, which is what makes `"al"..` a byte range rather than a filter
(`crates/aperture-engine/src/flatten.rs`; Glean emits the same trick).

### Bounded nesting

Record nesting is capped at `MAX_RECORD_DEPTH` (256). A hostile or corrupt byte string
with runaway nesting surfaces as a `BadRecord` **error, never a stack overflow** — see
[conventions](conventions.md) on errors-not-panics on data paths.

---

## Property 3 — Frozen on disk ([I3](invariants.md#i3))

**The invariant:** marker *values and their relative order* are semantic — a marker is the
most-significant part of a value's sort key — so once any data is written, they **cannot
change** without an on-disk migration.

New types don't get to renumber the table. They go into a **reserved band** in the correct
skip-family, chosen so their marker byte places the type where it should sort. Renumbering
an existing marker after data exists silently corrupts every stored key. There is a
golden-bytes test pinning every marker precisely so a renumber breaks loudly.

**I3 still holds forever for every database already written, and a format version is what makes
that a bound rather than a dead end.** A migration presupposes detection, and for a while nothing
written here said which encoding wrote it. Now it does: every DB carries a
[format stamp](03-storage-model.md#the-format-stamp-i15) whose `codec` half is exactly this
table's version ([I15](invariants.md#i15)), checked at open. Read the gain narrowly — a database
stamped `codec 1` is bound by this section as strictly as before, and renumbering a marker under
that stamp corrupts it just the same. What the stamp buys is that a *future* codec is a different
number rather than an impossibility. Glean has the same two halves, one of them finer-grained
than ours: a DB binary-representation version with negotiated readable/writable sets, and a
separately versioned bytecode ABI carrying its own lowest-supported floor
(`glean/bytecode/def/Glean/Bytecode/Generate/Instruction.hs:86-96`). The negotiated *set* is the
refinement we deliberately have not taken — our rule is equality, because "readable up to N" is a
promise about every past encoding and there is not yet a past encoding to make it about.

**A reserved band is not the whole decision for a container type.** For a scalar it genuinely is:
pick a marker in the right skip-family, the type slots in where it sorts, nothing existing moves.
An **array** is different — length-prefixed or terminator-delimited is a choice *inside* the
encoding, and a length-prefixed array cannot be prefix-matched at all, because the length sorts
ahead of the elements. Glean, which length-prefixes, says it outright: "MatchArrayPrefix doesn't
actually look at a prefix because arrays encode their length at the front"
(`glean/db/Glean/Query/Reorder.hs:794-796`). So arrays *are* a one-way door in a way the reserved
scalar bands are not, and the [multiplicity decision](open-decisions.md) has an encoding half.

---

## The marker table

Markers in ascending byte order — which is ascending **sort** order. Reserved gaps are
where future types slot in without renumbering.

| Byte(s)     | Marker            | Meaning                              | Skip family      |
|-------------|-------------------|--------------------------------------|------------------|
| `0x00`      | `MARK_NULL` / `MARK_TERM` | null value · record terminator | (see note)       |
| `0x01–0x20` | *(reserved)*      | future types sorting below `String`  |                  |
| `0x21`      | `MARK_STRING`     | UTF-8 string                         | terminator-walk  |
| `0x22`      | `MARK_RECORD`     | record start (paired with `MARK_TERM`) | nested         |
| `0x23–0x3F` | *(reserved)*      |                                      |                  |
| `0x40–0x47` | `MARK_INT_NEG`    | negative integers, width 8 … 1       | width-in-marker  |
| `0x48`      | `MARK_INT_ZERO`   | zero                                 | fixed (0 bytes)  |
| `0x49–0x50` | `MARK_INT_POS`    | positive integers / `u64`, width 1 … 8 | width-in-marker |
| `0x51`      | `MARK_FACT_REF`   | reference to a fact (`FactId`)       | fixed (8 bytes)  |
| `0x52–0xFE` | *(reserved)*      |                                      |                  |
| `0xFF`      | `MARK_ESCAPE`     | escapes a null *element* (`0x00 0xFF`) | —              |

Reading the integer band:

- **Zero** is `0x48`, the centre.
- **Positives** climb `0x49`→`0x50` as width grows: `0x49` is a 1-byte magnitude, `0x50` an
  8-byte one — so larger positives sort higher.
- **Negatives** fall `0x47`→`0x40` as width grows: `0x47` is a 1-byte magnitude (closest to
  zero), `0x40` an 8-byte one (most negative) — so more-negative sorts lower. Width is
  `MARK_INT_ZERO - marker` for negatives, `marker - MARK_INT_ZERO` for positives.

The **type ordering** falls out of the table: `null < string < record < integers <
fact-ref`. Within a typed field all values share a type, so cross-type ordering never
affects a real query; but the ordering is still frozen because the marker is part of the
sort key.

**Every marker a value can begin with stays below `MARK_ESCAPE`** — which is why the reserved
band stops at `0xFE`. A new type numbered `0xFF` would silently invert
[string ordering across a record boundary](#records-and-the-nullterminator-subtlety), so this
is part of what [I3](invariants.md#i3) freezes, not a spare byte.

> **On `MARK_FACT_REF`.** Fact references have their **own fixed-width marker** (`0x51`), so
> a value's bytes are self-describing without the schema and the byte-level `Int`/`Fact`
> distinction is enforced. (This resolves the "does `FactRef` share the integer encoding?"
> question — it does not. See [open decisions](open-decisions.md).)

---

## Invariants owned by this chapter

| # | Statement | Guard test |
|---|-----------|------------|
| [I1](invariants.md#i1) | Key encoding is order-preserving. | `codec::order_preservation` (tier-2 vs independent comparator) + round-trip |
| [I2](invariants.md#i2) | Encoding is self-delimiting; `skip` needs no schema. | `codec::skip_exactness` + trailing-bytes-rejected + max-depth-errors |
| [I3](invariants.md#i3) | The marker table is frozen on disk. | golden marker-bytes test |

These are the codec's whole reason for existing. Any change here re-runs the
order-preservation battery — no exceptions.

---

> **Reading path:** [← 1. Concepts](01-concepts.md) · **2. The tuple codec** · [3. The storage model →](03-storage-model.md)
