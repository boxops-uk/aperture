//! A query result, and the place a client keeps in it.
//!
//! # A bookmark, not an iterator
//!
//! [`Rows`] holds no borrow of the connection, and that is the design rather than an
//! inconvenience. A `Rows` that borrowed the socket mutably would make it impossible to
//! have two open — and two open is the point of the stream id, of the server's
//! per-stream tasks, and of a shell that can hold one result at `\more` while running
//! another query. So the connection does the I/O and this is what remembers where the
//! last one stopped.
//!
//! Nothing here buffers rows. The place is kept by the *stream* staying open, which
//! costs the server a suspended query loop and a bytes-only cursor — never a snapshot
//! ([I8](../../../docs/invariants.md#i8)).

use aperture_schema::schema::{LocalInterner, PredicateTy, Schema};
use aperture_wire::{Desc, QueryProfile, StreamId, WireValue, value::decode_value};

use crate::error::ClientError;

/// A result in progress, or one that has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Streaming,
    /// Ended, with the count the server reported — which after a cancel is what it
    /// *sent* rather than what the query would have matched.
    Ended(u64),
}

/// One query's rows, and where the reader has got to.
pub struct Rows {
    stream: StreamId,
    desc: Desc,
    ty: PredicateTy,
    /// Kept for the life of the result because [`ty`](Rows::ty) is made of symbols
    /// minted in it. Decoding never resolves one — rows are positional — but a type
    /// whose namespace had been dropped would be a trap laid for the first caller who
    /// wanted a field name.
    _interner: LocalInterner,
    seen: u64,
    state: State,
    /// `Some` once the server has reported what the query examined — which it does
    /// only when the query was issued with [`Connection::query_profiled`], and only
    /// once, just before the result ends.
    profile: Option<QueryProfile>,
}

impl Rows {
    pub(crate) fn new(
        stream: StreamId,
        desc: Desc,
        ty: PredicateTy,
        interner: LocalInterner,
    ) -> Rows {
        Rows {
            stream,
            desc,
            ty,
            _interner: interner,
            seen: 0,
            state: State::Streaming,
            profile: None,
        }
    }

    /// What the query examined, once it has ended.
    ///
    /// `None` for a query that did not ask, and for one still running — the frame
    /// arrives just before the result ends, because the count is not final until the
    /// last chunk has run.
    #[must_use]
    pub fn profile(&self) -> Option<&QueryProfile> {
        self.profile.as_ref()
    }

    pub(crate) fn set_profile(&mut self, profile: QueryProfile) {
        self.profile = Some(profile);
    }

    /// The shape every row has: the query's **head** type, named.
    ///
    /// The one place the format carries type tags, and it carries them once per stream
    /// rather than once per field per row — which is exactly the trade that makes
    /// tagging affordable here and not in a fact.
    #[must_use]
    pub fn desc(&self) -> &Desc {
        &self.desc
    }

    #[must_use]
    pub fn stream(&self) -> StreamId {
        self.stream
    }

    /// Rows this reader has taken so far.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// Whether the result has ended — exhausted or cancelled.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(self.state, State::Ended(_))
    }

    /// What the server said it sent, once the result has ended; `0` before that.
    #[must_use]
    pub fn sent(&self) -> u64 {
        match self.state {
            State::Ended(sent) => sent,
            State::Streaming => 0,
        }
    }

    /// Decode one row's payload against the descriptor's type.
    ///
    /// **Positionally**, because that is the only correct way: the descriptor and the
    /// row come from the same head type walked in the same order, and matching by name
    /// cannot work when a head names fields no predicate declares.
    pub(crate) fn decode(
        &mut self,
        payload: &[u8],
        schema: &Schema,
    ) -> Result<WireValue, ClientError> {
        let (value, used) = decode_value(payload, schema, &self.ty)?;

        // Trailing bytes are a fault, not slack — the same rule the handshake messages
        // follow. A row longer than its own type means the two ends disagree about the
        // shape, and reading the prefix would let both think they agreed.
        if used != payload.len() {
            return Err(ClientError::Protocol(format!(
                "a row on stream {} carries {} bytes past its type",
                self.stream.0,
                payload.len() - used
            )));
        }

        self.seen += 1;
        Ok(value)
    }

    /// Count a row that arrived and was thrown away — what a cancel does with the rows
    /// already in flight. They still *arrived*, so the tally below still means
    /// "everything the server sent reached here".
    pub(crate) fn skip(&mut self) {
        self.seen += 1;
    }

    /// Record the end, and check the server's count against what was actually read.
    ///
    /// The count is not decoration: a resume that dropped or repeated a row would
    /// disagree here, and this is the cheapest place any client will ever notice.
    pub(crate) fn finish(&mut self, sent: u64) -> Result<(), ClientError> {
        if sent != self.seen {
            return Err(ClientError::Protocol(format!(
                "the server says it sent {sent} rows on stream {}, and {} arrived",
                self.stream.0, self.seen
            )));
        }

        self.state = State::Ended(sent);
        Ok(())
    }
}

impl std::fmt::Debug for Rows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rows")
            .field("stream", &self.stream.0)
            .field("desc", &self.desc)
            .field("seen", &self.seen)
            .field("state", &self.state)
            .finish()
    }
}
