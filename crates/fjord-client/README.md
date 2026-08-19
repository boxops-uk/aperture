# fjord-client

The Rust client for **Fjord DB** — an embedded, immutable fact database.

Most programs want [`fjord-db`](https://crates.io/crates/fjord-db), which is this crate plus
the two it needs, behind one dependency. Use this one directly if you would rather name what
you depend on.

```toml
[dependencies]
fjord-client = "0.0.1"
fjord-schema = "0.0.1"
```

What it is made of is `fjord-wire` and a socket. It depends on no storage engine, no query
engine and no async runtime — the calls are blocking, and the I/O policy is the program's:
reconnection, retry and timeouts belong to a caller that knows whether it is a shell telling
a person or a deriver that should try again.

- **Connect** over a Unix socket or TCP. One protocol, one handshake; only the pipe differs.
- **Query** and page. A result is a *bookmark* rather than an iterator holding the socket, so
  several can be open at once and `take(n)` leaves the stream where it was.
- **Write** facts, with references nested inline to any depth. Interning is the server's, and
  it is idempotent.
- **Fetch and expand** — a stored reference is an id, and `Expander` replaces one with the
  fact it names, recursively, under a depth bound.

The **schema is the client's**: the transport codec sends no field names and no type markers,
because both ends already have them. A reader can ask the server for the database's own with
`served_schema`; a producer states it and asserts it at the handshake, which turns "we
disagree about the data model" from a corrupt read months later into a refusal at connect
time.

There is a second implementation of the same protocol in C# under
[`clients/dotnet`](https://github.com/boxops-uk/fjord/tree/main/clients/dotnet), sharing no
constants and no enums with this one, plus a byte-for-byte golden test between the two
encoders. That is what says the protocol is implementable from outside rather than merely
implemented here.

## Licence

MIT.
