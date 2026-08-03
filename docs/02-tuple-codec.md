# 2. The tuple codec

> [Aperture design book](../README.md) · [← 1. Concepts](01-concepts.md) · **Chapter 2** · [3. The storage model →](03-storage-model.md)

The **tuple codec** turns typed values into bytes. It is the foundation the entire storage
model stands on, and it has three properties that must all hold at once: encodings are
**order-preserving**, **self-delimiting**, and (once data exists) **frozen**. This chapter
explains each, why it matters, and the marker table that implements them.

Code: `src/focus/tuple.rs`. Tests there are the project's densest — the codec is the most
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

Byte order *is* semantic order. This is the property that makes the whole database work:
because a predicate's facts are stored as `predicate_id ++ encoded_key`, sorted
lexicographically, a **prefix scan over a byte range is exactly a predicate query over a
value range**. Range queries, point lookups, and joins all reduce to "scan a sorted byte
range." No secondary structure, no per-query sort.

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

The executor must skip over fields it doesn't care about (to reach field 3 of a key, it
walks past fields 0–2) and it must do so **without type information**, deep in the scan hot
loop. So every encoding carries its own shape. There are exactly **three skip-shape
families**:

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
