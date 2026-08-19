# fjord-wire

The **transport** codec and message vocabulary for **Fjord DB** — an embedded, immutable
fact database.

Most programs want [`fjord-db`](https://crates.io/crates/fjord-db) or
[`fjord-client`](https://crates.io/crates/fjord-client); this crate is what they are built
out of, and is the specification a client in another language implements against.

There are **two codecs in Fjord and they share no bytes**, which is the distinction this
crate exists to keep structural:

| | storage (internal) | transport (here) |
|---|---|---|
| read by | the executor, off disk, in the scan hot loop | a peer, off a socket |
| ordered? | **yes** — `memcmp` *is* semantic order | no; nothing memcmps a frame |
| self-delimiting? | **yes** — a field can be skipped with no schema | no; the reader has the schema |
| frozen? | **yes**, the moment data exists | no — versioned by the handshake |

What is here: `varint`, `crc`, the schema-driven `value` and fact encoding, `block` (a run of
one predicate's facts behind a sync marker), `frame` (`[kind][stream][length]`), and
`protocol` — what a startup frame carries and what a stream's life looks like, shared by
server and client, with **no I/O policy** in it.

It is a sibling of the storage codec rather than a layer on it: it depends on `fjord-schema`
alone.

## Licence

MIT.
