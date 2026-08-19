# Authentication, and what a principal may never be

> [Aperture design book](../README.md) · reference doc

**Nothing here is built.** This is the design of record for authentication, written before
the phase that would build it, so that the shape is argued once rather than discovered
during implementation. Where it contradicts a sentence elsewhere in the book — and it
contradicts three — the contradiction is named and the amendment is listed at the end.

[`ops-I10`](invariants.md#ops-i10) says *no in-DB auth; the transport is the trust boundary*,
and it is enforced by being default-closed: the server binds a Unix socket only, TCP is an
explicit `--listen-tcp` with no config-file entry and no environment variable, and the
operator who passes it takes on the gateway in front. That has held. What it does not answer
is the question now being asked of it — **who is at the other end, and what may they do** —
in a world where a credential is not typed into a connection string, where a workload's
identity is attested rather than asserted, and where the thing that proves it expires within
the hour.

---

## 1. Three questions, and the current design collapses them

| | Question | Today |
|---|---|---|
| **The pipe** | is it confidential and unmodified | nothing; a socket's file mode, or somebody else's gateway |
| **Authentication** | who is at the other end | not asked, and not answerable |
| **Authorization** | what may they do | everything, to every database under the root |

Separating them is most of the work, because the three have different costs and only the
third touches an invariant. The pipe is a dependency. Authentication is a value on the
session. Authorization is a decision made once, at the same instant `ops-I6` resolves a
session's mode and `ops-I2` refuses a write to a sealed database — which is the precedent
that keeps it out of the machine entirely.

---

## 2. Why the Vault/OpenBao-against-Postgres pattern is the wrong shape here

It is worth stating fairly first, because it is the pattern people mean when they say
credentials should not live in a connection string, and the instinct is right even though
the mechanism does not port.

OpenBao's database secrets engine holds a long-lived **root** credential for the target
database. On request it runs a creation statement against that database —
`CREATE ROLE "v-app-x7f2" WITH LOGIN PASSWORD '…' VALID UNTIL '…'` — hands the caller a
lease, and at expiry runs the revocation statement that drops the role. The application
never holds a static credential; the credential's lifetime is the lease's; a compromised one
is revocable by name.

Every part of that depends on the database owning a **mutable principal namespace**. Aperture
has none, and giving it one lands in one of two places, both bad:

- **Principals as facts.** They enter [`ops-I4`](invariants.md#ops-i4)'s identity — a database's
  identity is `hash(canonical schema, base facts)`, a multiset over each fact's logical form —
  so granting access would move the number that says two builds produced the same artifact.
  And it is impossible on the databases that most want access control:
  [`ops-I2`](invariants.md#ops-i2) makes a Complete database immutable, every write-mode open
  refused at establishment. You cannot `CREATE ROLE` in a sealed artifact.
- **Principals in a sidecar.** A mutable store beside an immutable artifact, per database.
  `finish` would need an opinion about it. `ops-I4`'s reproducibility would need a carve-out
  for it. And the deployment model this repo has actually planned for — horizontal read
  scaling as more processes each holding their own **copy** of a Complete database
  ([operations §5](aperture-cli-design.md)) — would replicate a grant table that goes stale
  the moment anything revokes, silently, per replica.

**So the pattern does not port, and the capability does.** What the lease buys — a credential
that is short-lived, obtained automatically, and never at rest — is delivered by an
attested identity instead, where the *issuer* is external and the database only **verifies**.
Verification is stateless: a trust bundle, a clock, and a policy. Nothing inside a database
changes, ever.

That is worth an invariant of its own, because it is the rule that rules the pattern out:

> <a id="ops-i11"></a>**Proposed `ops-I11` — a principal is never content.** No principal,
> credential, role or grant is stored as a fact, in a sidecar, or anywhere inside a database
> directory. Authorization is **configuration**, held by the server process; identity is
> **attested**, held by the peer. A database cannot be granted on, so its grants cannot go
> stale, cannot be replicated, and cannot enter its identity.

Stated as something a test can fail: *ingest a corpus under a policy, and the database's
content identity is byte-identical to the same ingest with no policy at all.*

**The issuer falls out as replaceable.** Because the server verifies against a trust bundle
rather than against SPIRE specifically, an OpenBao **PKI** engine issuing short-lived client
certificates fills the same socket. That is not a second mechanism to build; it is a
consequence of writing the verifier against a bundle, and the reason to write it that way.

---

## 3. Attested identity, end to end

The chain is worth stating in full because its links are usually conflated, and because
**IMDS attestation is one link that never touches Aperture**.

1. **Node attestation.** A SPIRE agent on an EC2 host presents the instance identity document
   from IMDSv2; the SPIRE server verifies its signature against the AWS public certificate and
   derives selectors from what the document and the API say about the instance — tags, security
   groups, the IAM role. That is the `aws_iid` node attestor; `azure_msi`, `gcp_iit` and
   `k8s_psat` are its peers. **This proves the machine, not the process.**
2. **Workload attestation.** A workload calls the agent's Workload API over a Unix socket. The
   agent inspects the *calling process* through that socket's peer credentials — uid, gid, pid
   — and `/proc`, or asks the kubelet, yielding `unix:uid:1000`, `k8s:sa:indexer`.
3. **A registration entry** maps (node selectors ∧ workload selectors) to a SPIFFE ID.
4. The agent returns an **X.509-SVID** — a leaf certificate whose only meaningful field is a URI
   SAN, `spiffe://corp/ns/index/sa/roslyn-indexer` — plus the trust bundle, and **re-issues at
   half TTL** (an hour by default). Nothing on disk, no file to reload, no restart.
5. Aperture's client presents it. Aperture's server verifies the chain against the bundle and
   reads the URI SAN as the principal.

**Step 2 is a mechanism this repo already has and throws away.** `server.rs` binds `_address`
on the Unix accept and `_peer` on the TCP one and drops both; `tokio::net::UnixStream::peer_cred()`
returns exactly what SPIRE's `unix` workload attestor asks the kernel for. So the local
principal is not a stopgap that mTLS replaces — it is the same trust root, one hop earlier,
and building it first makes the *shape* exist with a real value in it before any cryptography
is involved.

### One `Principal`, three attestors

```rust
enum Principal {
    /// SO_PEERCRED on a Unix socket. Kernel-attested, no crypto, no dependency.
    Peer   { uid: u32, gid: u32, pid: i32 },
    /// The URI SAN of a verified X.509-SVID.
    Spiffe { id: String, expires_at: SystemTime },
    /// A JWT-SVID or OIDC token, verified against a JWKS. See §5.
    Token  { subject: String },
    /// What an opted-in TCP port yields today.
    Anonymous,
}
```

`Anonymous` is in the enum deliberately. It names what exists now, so *"the port is reachable
by whoever can route to it"* becomes a value a policy can refuse rather than an absence
nothing can express.

---

## 4. mTLS costs zero protocol bytes, and that is the argument

**The handshake has no room, and this is a fact about the code rather than a preference.**
`decode_startup` ends by refusing leftovers:

```rust
if at != bytes.len() {
    return Err(WireError::TrailingBytes(bytes.len() - at));
}
```

So a credential field appended to the startup payload is rejected by every server already
built. Meanwhile `protocol::VERSION` has stayed at **2** through control frames, `FETCH`/
`FETCHED`, paging and profiling, because every extension this protocol has taken was a **new
frame kind** and never a new field — the module documentation says so twice, and
`clients/dotnet` is the check that it is true rather than hoped.

**mTLS needs neither.** The identity is settled by the TLS handshake before the first frame is
read, so the vocabulary does not change, `VERSION` does not move, and a client that has never
heard of any of this connects exactly as it does today. That is the same "additive" discipline
the protocol has kept since Phase 7, applied to the one feature that would otherwise break it.

Three things make it cheap on this side too:

- **`session::serve` is already generic over the pipe** — `R: AsyncRead + Unpin`,
  `W: AsyncWrite + Unpin + Send + 'static`. A `tokio_rustls::server::TlsStream` splits into
  those halves and the session is untouched.
- **The client's `Transport` is deliberately an enum, not a generic** — `Unix` and `Tcp` today,
  with the module documentation stating the trade. A third variant is the extension point it
  was shaped for. The constraint to respect is that `aperture-client` is **blocking** by design,
  so the TLS choice must have a blocking API (`rustls::StreamOwned` over a `TcpStream`);
  `tokio-rustls` server-side.
- **The listener is the only new machinery** — a third bind beside the socket and the TCP port,
  default-closed on the same terms.

### Why the terminator has to be the server

[`ops-I10`](invariants.md#ops-i10) currently delegates authentication to *"an authenticated
gateway / mTLS terminator / tunnel"*. **That does not survive contact with workload identity**:
a gateway that terminates TLS has consumed the peer certificate, and the identity in it exists
nowhere downstream. Preserving it means the gateway re-asserts it in a header — an
`X-Forwarded-Client-Cert` shape — which Aperture's protocol has no room for and should not
grow, because a forwarded identity is one the server must take on trust from a hop it cannot
verify.

So `ops-I10` is **not reversed by this design; it is made real.** "The transport is the trust
boundary" stays exactly true when the server *is* the transport's terminator. What moves is the
boundary: into the process that enforces it, out of a gateway trusted to have done it. And the
other half of `ops-I10` — no in-DB auth — survives untouched, strengthened into `ops-I11`.

The consequence for [§2 of operations](aperture-cli-design.md#2-addressing--connection-resolution)
is that its `user@` decision **stands, with a better reason**. It was dropped as syntax with
nothing behind it. With attested identity there is no user to name: the address says where the
database is, and the certificate says who is asking.

---

## 5. Bearer tokens, if they are ever wanted

A JWT-SVID or an OIDC token is the fallback for a client that cannot present a certificate —
a browser tier, a CI runner without an agent. It is the **only** shape here that needs bytes on
the wire, and the place for them is settled by precedent rather than taste:

**A new frame kind on stream 0, sent before `STARTUP`. Never a field appended to the startup
payload.** The precedent is `QUERY_PROFILE`/`PROFILE`, `QUERY_PAGE`, `FETCH`/`FETCHED` — every
one of them a kind rather than a flag, for the reason stated in `protocol.rs`: a payload whose
bytes gain a new meaning is a silent change for every client already sending the old ones. A
client that never authenticates neither sends the frame nor receives one, and `VERSION` stays
at 2.

Two sub-questions this design leaves open rather than answering:

- **Which error code a refusal carries.** `ErrorCode::from_byte` returns `None` for a code it
  does not know, so a new `Unauthorized` variant is a compatibility question, not a free
  addition. Reusing `ModeRefused`, `Refused` and `UnknownDatabase` may well be correct —
  see §6 for why `UnknownDatabase` is load-bearing.
- **Where the JWKS comes from**, and how its rotation is bounded. Unlike an SVID there is no
  agent to hand it over.

---

## 6. Authorization, at `(database, mode)` and no finer

The unit is the one the session already has. A policy is operator configuration — beside the
other server configuration, reloadable on `SIGHUP`, and by `ops-I11` never inside a database:

```toml
[[grant]]
principal = "spiffe://corp/ns/index/sa/roslyn-indexer"
databases = ["staging-*"]
mode      = "read-write"

[[grant]]
principal = "unix:uid:1000"
databases = ["*"]
mode      = "read-only"
```

**Evaluated once, at handshake**, against the startup frame's database and mode — the same
place and the same instant that `ops-I6` resolves the mode against the database's status and
`ops-I2` refuses a write to a Complete one. That is not an optimisation; it is the repo's own
answer to when a session-wide question gets asked, and following it means no per-frame check,
no per-row check, and nothing whatsoever in the executor.

Two consequences worth writing down before they are discovered:

- **A database a principal may not see answers `UnknownDatabase`, not a refusal.** A refusal
  that distinguishes *"no such database"* from *"not yours"* enumerates the catalogue for
  anybody who can open a connection.
- **`aperture.db.List` must filter by principal**, or the line above is a fiction. This is the
  point at which authorization enters the query language, and it is worth being precise about
  how little it costs: the catalogue is a **virtual predicate answered at the `FactStore`
  seam**, not a source in the executor — deliberately, so the plan IR gains no variant and
  [I4](invariants.md#i4) needs no re-proving. Filtering it is a filter over a `Vec` before the
  rows are encoded. It touches neither [I6](invariants.md#i6) nor [I9](invariants.md#i9),
  because it happens before anything the hot loop can see.

That second point is the whole justification for the ceiling. At `(database, mode)` the entire
enforcement surface sits *outside* the machine.

### The ceiling, and what it costs to raise

- **Per-predicate** — "may query `src.File`, not `build.Assembly`" — is enforceable at
  compile time in the server, still outside the hot loop, but it puts a principal into query
  compilation and needs an answer for derived and virtual predicates. Not taken.
- **Per-fact** is already priced by [operations §1](aperture-cli-design.md) and declined. Glean's
  per-fact visibility *is* ownership with different units: ACL groups are allocated as ownership
  units and ANDed into the same slices. So reopening `ops-I10` for per-fact authorization
  reopens [`ops-I9`](invariants.md#ops-i9) with it, at ownership's price — visibility moves
  inside the scan iterator, below the register, off the id in the `keys` row, which is the
  option Glean itself calls ugly and did not take. [I6](invariants.md#i6) and
  [I9](invariants.md#i9) would be amended. **Not taken, and not a gap.**

---

## 7. Revocation, honestly

An SVID lives an hour. A session may live longer — the viewer's `Pool` retires a connection
after ten thousand queries, and an indexing run holds a write session for hours. Authorization
is decided once, so **a live session outlives the credential that opened it.** This design does
not claim to revoke one.

What it does instead:

- a **maximum connection lifetime** on the server: past it, no new stream is accepted and the
  connection closes cleanly;
- `Pool` grows a time bound beside its count bound — its own documentation already anticipates
  this, saying a long-lived connection *"accumulates whatever the server attaches to a session,
  and this tier has no way to know when that changes"*, which is precisely what an authorization
  decision is;
- and the residual is stated rather than implied: **a bounded staleness window, not revocation.**

Anything stronger — re-verifying mid-session, a revocation channel, short-lived sessions forced
by the server — is a second design, and belongs in a later phase rather than in an implication
of this one.

---

## 8. What this contradicts, and what would have to change

Recorded here so the amendments are a list rather than a search. **None of them are made yet.**

| Where | What it says now | What this design needs |
|---|---|---|
| [operations §1](aperture-cli-design.md), `ops-I10` | authentication delegated to a gateway / mTLS terminator; *"a credential slot in the handshake is reserved in this document"* | the server terminates; the reserved slot is **retired**, because mTLS needs no bytes and a token needs a frame kind |
| [invariants](invariants.md#ops-i10) | the `ops-I1`–`ops-I10` range | `ops-I11`, and the range text in three files |
| `crates/aperture-server/src/lib.rs` | *"the handshake has a reserved credential slot and accepts anonymous"* | the first half deleted; the second half becomes `Principal::Anonymous` |
| `crates/aperture-server/src/server.rs` | *"a reserved credential slot nothing fills"* | same |
| `src/cli.rs`, `src/commands/serve.rs` | *"the handshake accepts anonymous"* | true, and now a value rather than an absence |

The retirement is the one to get right: leaving a reserved slot documented in three files while
the design has decided against filling it is how a book starts contradicting itself.

---

## 9. Build order, if it is built

Sized so each step lands with the suite green and the one with an invariant behind it lands in
the middle.

- **a — a principal exists.** `Principal`, a field on `Session`, `SO_PEERCRED` filling it on the
  socket path, `Anonymous` on TCP. Nothing refuses anything. This is the shape, with a real value
  in it.
- **b — a policy that can refuse.** Parse, load at `serve`, reload on `SIGHUP`, evaluate once in
  the handshake. `UnknownDatabase` for an invisible database; `aperture.db.List` filtered.
- **c — mTLS.** `--listen-tls` with certificate, key and bundle, default-closed on `--listen-tcp`'s
  terms; `tokio-rustls` server-side, a `Transport::Tls` variant client-side. URI SAN →
  `Principal::Spiffe`. **Acceptance: `protocol::VERSION` does not move, and the .NET client still
  connects.**
- **d — the Workload API.** Fetch the SVID and bundle from the agent socket rather than from
  files, and rotate in place at half TTL.
- **e — connection maximum lifetime**, and `Pool` grows its time bound.
- *deferred, named not built* — bearer tokens as a new frame kind (§5).

**Guards, to be written up front and `#[ignore]`d until the phase runs**, per
[testing](testing.md):

- `server::a_principal_is_never_written_to_a_database` — `ops-I11`, as the identity comparison
  in §2.
- `server::an_unauthorised_database_is_indistinguishable_from_a_missing_one`
- `server::the_catalogue_lists_only_what_the_principal_may_see`
- `client::the_dotnet_client_still_connects_at_protocol_version_2`

---

> [Index](../README.md) · [Operations](aperture-cli-design.md) · [Invariants](invariants.md) · [Open decisions](open-decisions.md)
