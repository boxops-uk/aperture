# Boxops.Fjord.Client

A .NET client for **Fjord DB** — an embedded, immutable **fact database**.

```bash
dotnet add package Boxops.Fjord.Client
```

No dependencies. `net8.0` and `net10.0`.

A database is built once — schema, then facts — sealed, and thereafter only read. Facts are
typed records identified by a `FactId`, grouped by predicate, and queried in **sigla**, a
typed, Datalog-flavoured language.

## What it is for

This is a **second implementation** of the wire protocol, sharing no constants, no enums and
no code with the Rust one. That is deliberate: a client written against the same types can
agree with the server by accident, and this one agrees only with the specification. It has
already earned that twice — it found a block header length stated in one place and miscounted
in another, and a fingerprint that would have depended on the client's byte order.

There is also a byte-for-byte golden test: this client's encoding of a fixed corpus is
checked against the Rust encoder's, with the schema and the corpus stated independently on
each side, because a shared statement would make the two agree by construction.

## Writing facts

```csharp
using Fjord.Client;

// A predicate id is its **position** in the schema, and a schema is sorted by name.
const uint File = 0;
const uint Module = 1;

// The fingerprint is carried, never computed: `fjord schema fingerprint` prints it, and
// the number only says which schema these shapes were written against.
var schema = new FjordSchema(
    [
        new FjordPredicate("src.File", FjordType.String, null),
        new FjordPredicate("src.Module", FjordType.Rec(
            ("file", FjordType.Reference(File)),
            ("name", FjordType.String)), null),
    ],
    fingerprint: 0x2c8d1f4b9a7e3506);

using var connection = FjordConnection.Connect(
    FjordAddress.Parse("/tmp/fjord.sock//code"),
    schema,
    SessionMode.ReadWrite);

// Record fields are **positional**, in the order the schema declares them — that order
// is the physical key order, so it is the schema's to decide and not the caller's.
FjordFact FileFact(string path) => new(File, FjordValue.Of(path));

// A module names its file by nesting the whole fact. No id, and no bookkeeping.
FjordFact ModuleFact(string path, string name) =>
    new(Module, FjordValue.Rec(
        FjordValue.Of(FjordRef.To(FileFact(path))),
        FjordValue.Of(name)));

var summary = connection.Write(Module, [ModuleFact("store/codec.py", "store.codec")]);
Console.WriteLine($"created {summary.Created}, deduped {summary.Deduped}");
```

**A reference is the whole target fact, nested inline** — not an id. So a producer keeps no
map from its own entities to assigned identities, and needs no emission order: it emits what
it holds where it stands, and the server *interns* each nested fact, creating it or finding
what that key already names. Sending the same facts twice writes nothing, which is what makes
retrying after a dropped connection safe.

## The schema is yours to state

Nothing in the protocol describes the data model: the value codec sends no field names, no
type markers and no record arities, because both ends already have them. So a client states
the schema it uses and asserts it at the handshake by fingerprint — which turns "we disagree
about the data model" from a corrupted database into a refused connection, before a byte of
data flows.

You do **not** have to know how facts are stored. The storage codec is order-preserving,
self-delimiting and frozen on disk; none of that is on the wire, and a client never sees it.

## Status

`0.0.1`. No authentication — the transport is the trust boundary. See
[the repository](https://github.com/boxops-uk/fjord) for the full inventory of what is and is
not built.

## Licence

MIT.
