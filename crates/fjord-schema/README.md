# fjord-schema

The type model, the schema language and schema identity for **Fjord DB** — an embedded,
immutable fact database.

Most programs want [`fjord-db`](https://crates.io/crates/fjord-db), which re-exports what a
client needs from here. This crate is the bottom of the workspace: it depends on nothing else
of Fjord's, and everything else depends on it.

- **`schema`** — what a predicate *is*. A `Predicate` has a key and an optional value; a
  record's fields are an ordered slice, never a map, because **declaration order is the
  physical key order** and therefore decides what a query can seek on rather than merely
  filter.
- **`id`** — what a stored row is *called*. A `FactId` is stable, unique, and never reused
  within a database.
- **`syntax`** — the schema language: lexer, grammar, parse, lower, print, and import
  resolution. `syntax::read(name, source)` is the ordinary way to get a `Schema`.
- **`fingerprint`** — schema *identity*: a canonical form, a number per predicate and one
  for the whole schema, and subset containment. This is what a client carries and what a
  handshake compares, so two ends that disagree about the data model find out before a byte
  of data flows.

A schema is **frozen into a database when it is created** and travels with it: the database
is self-describing, and a server serves each one from its own embedded copy rather than from
anything the server holds.

```rust
let schema = fjord_schema::syntax::read(
    "notes.sigla",
    "schema note { predicate Line : string }",
)?;
assert_eq!(schema.len(), 1);
# Ok::<(), String>(())
```

## Licence

MIT.
