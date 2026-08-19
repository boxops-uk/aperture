# fjord-db

A client for **Fjord DB** — an embedded, immutable **fact database**.

```toml
[dependencies]
fjord-db = "0.0.1"
```

A database is built once — schema, then facts — sealed, and thereafter only read. Facts are
typed records identified by a `FactId`, grouped by predicate, and queried in **sigla**, a
typed, Datalog-flavoured language.

This crate is a façade over the three that do the work, so getting started is one
dependency rather than three chosen correctly:

| Crate | What it is |
|---|---|
| [`fjord-client`](https://docs.rs/fjord-client) | Connections, queries, paging, writing, expansion, addresses |
| [`fjord-schema`](https://docs.rs/fjord-schema) | The type model, the schema language, and schema identity |
| [`fjord-wire`](https://docs.rs/fjord-wire) | The transport codec and the protocol's message vocabulary |

The storage codec, the store, the query engine, the write funnel and the server are
internal and are not published: a package is what it takes to talk to a database and read
rows back, not the shape of what is answering.

## Reading

The client must have the schema, because the protocol does not describe one — the value
codec sends no field names and no type markers, since both ends already have them. A reader
can ask for the database's own, which is the only way to be right about it:

```rust,no_run
use std::{path::Path, sync::Arc};
use fjord_db::{Connection, Mode, Schema};

let mut connection = Connection::connect(
    Path::new("/tmp/fjord.sock"),
    "code",
    Arc::new(Schema::empty()),   // a reader has no claim to make
    Mode::ReadOnly,
    false,
)?;

let schema = Arc::new(connection.served_schema()?);

let mut rows = connection.query("F where src.File F")?;
for row in connection.take(&mut rows, 20)? {
    println!("{row:?}");
}
# Ok::<(), fjord_db::ClientError>(())
```

`take` reads *n* rows and stops, leaving the stream open: nothing is buffered at either
end, because the server suspends holding a bytes-only cursor and has already released its
snapshot. A pause of a millisecond and a pause of an hour cost it the same thing.

## Writing

A producer states the schema itself and asserts it at the handshake, so a disagreement is a
refused connection rather than a database full of rows nobody can read back.

```rust,no_run
let source = std::fs::read_to_string("schemas/code.sigla")?;
let schema = fjord_db::read_schema("code.sigla", &source)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

**A reference is the whole target fact, nested inline** — not an id. So a producer keeps no
book of what it has written: it emits what it holds where it stands, and the write path
interns each nested fact, creating it or finding what that key already names. Sending the
same facts twice writes nothing, which is what makes retrying a dropped connection safe.

## Status

`0.0.1`, and honest about it: no authentication (the transport is the trust boundary), no
union types, no stored derivation, no ingestion from files. See
[the status page](https://github.com/boxops-uk/fjord/blob/main/website/content/status.md)
for the full inventory.

## Licence

MIT.
