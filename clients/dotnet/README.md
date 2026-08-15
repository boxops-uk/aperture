# The .NET client

A C# implementation of Aperture's wire protocol, and a console program that uses it to
write facts into a real database and query them back.

It exists to answer a question the Rust tests cannot: **is the protocol implementable
from outside?** A client written in the same repository, against the same types, can
agree with the server by accident — sharing a constant, sharing an enum, sharing an
assumption nobody wrote down. This one shares nothing but the specification, and it
found two things the Rust side had not: a block header length stated in one place and
miscounted in another, and a fingerprint that would have depended on the client's byte
order.

| Project | What it is |
|---|---|
| `Aperture.Client` | the library: varints, CRC-32, the value codec, blocks, frames, the handshake, a connection |
| `Aperture.Demo` | a console program that writes a small code index and queries it |

## Running it

From the repository root:

```sh
cargo build --bin aperture-serve

rm -rf /tmp/ap-demo && mkdir -p /tmp/ap-demo
./target/debug/aperture-serve \
    --socket /tmp/ap-demo/aperture.sock \
    --data-dir /tmp/ap-demo/db \
    --ready-file /tmp/ap-demo/ready &

# `--ready-file` appears only once the listener is accepting, so waiting on it is a
# signal rather than a race.
while [ ! -e /tmp/ap-demo/ready ]; do sleep 0.1; done

dotnet run --project clients/dotnet/Aperture.Demo -- --socket /tmp/ap-demo/aperture.sock
```

Or `./clients/dotnet/run-demo.sh`, which is the above.

## What the demo shows

**A producer that holds no fact ids.** Every reference it writes is the target fact
itself, nested inline, two levels deep — a declaration names its module, which names
its file. It keeps no map from entities to identities and emits in whatever order it
likes. The server interns each target and substitutes the id.

That is the whole reason a reference on the way in is a fact rather than an id. An
indexer walking a syntax tree knows the file when it reaches the declaration; every
id-based scheme would make it remember what the server called things.

The counts show it working:

```
writing 6 declarations, every reference nested
  created 12, deduped 6 (of 18 facts touched)
  6 declarations + 3 modules + 3 files = 12 distinct facts

the same block again: created 0, deduped 18
```

Six declarations name three modules and three files, so eighteen facts are *touched*
and twelve are *written*. Sending the same block again writes nothing at all —
interning is idempotent, which is what makes retrying after a dropped connection safe.

## What a client has to know, and what it does not

**It has to have the schema.** The value codec sends no field names, no type markers
and no record arities: the server has them and so does the client, and sending what the
reader already has is what a transmission-shaped format declines to do. `Schema.cs` is
that, written down.

Until schemas are parsed (PLAN Phase 8) a client writes the schema out by hand and
asserts it at the handshake with a fingerprint, which is what turns "we disagree about
the schema" from a corrupted database into a refused connection. Passing
`assertSchema: false` sends `0`, meaning *do not check* — right for a reader, wrong for
a producer.

**It does not have to know how facts are stored.** The storage codec is
order-preserving, self-delimiting and frozen on disk; none of that is on the wire, and
a client never sees it.

## What is not implemented

The client mirrors the server, so it stops where the server does. Streams are issued
sequentially — the ids are real and the server tags every reply with one, but this
client sends a stream's frames and reads its replies before starting the next. There is
no cancellation and no flow control. All three are named as deferred in
[operations §5](../../docs/aperture-cli-design.md).

There is no test project: the console program *is* the test, and it is a better one
than a unit suite would be, because it runs against the real server over a real socket.
A unit test of this codec against constants copied from the Rust would prove the
constants were copied.
