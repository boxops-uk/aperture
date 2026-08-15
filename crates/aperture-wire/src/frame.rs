//! **Frames** — the connection's multiplexing unit.
//!
//! ```text
//!   [kind u8][stream u32][length u32][payload]
//!    └────────── header, 9 B ───────┘ └ length B
//! ```
//!
//! PostgreSQL-inspired and deliberately not PostgreSQL-compatible: PG's header is a
//! type byte and a length, and every message belongs to the one conversation the
//! connection *is*. The `stream` field is what departs from that, and it is the
//! reason for departing — PG's model is strictly serial, so a long query blocks a
//! short one behind it. Here a query is a stream and a write is a stream, several
//! run at once on one connection, and a frame says which
//! ([operations §6](../../../docs/aperture-cli-design.md#6-wire-protocol--the-write-stream)).
//!
//! # The frame layer does not know the protocol
//!
//! [`FrameKind`] is a `u8` newtype with constants, not an enum, and that is a
//! decision rather than a placeholder. A frame layer's job is to say where a message
//! starts and stops; deciding what the message *means* is the layer above. An enum
//! would make an unrecognised kind a decode failure, when the correct behaviour for
//! a framing layer is to hand it up intact — which is also what lets a peer at a
//! newer protocol version be answered with "I do not know that message" rather than
//! "your bytes are malformed".
//!
//! The vocabulary itself is not settled here, and the constants below are only the
//! ones operations §6 already names.
//!
//! # Length is checked before it is trusted
//!
//! A `length` read off a socket sizes a read and, in a naive implementation, an
//! allocation. It is bounded by [`MAX_PAYLOAD`] and rejected past it — the sort of
//! thing the storage codec never has to think about, because its bytes come from our
//! own disk and a wire codec's come from whoever connected.

use crate::error::WireError;

/// What kind of message a frame carries.
///
/// A newtype rather than an enum — see the module docs. `Display` renders the byte,
/// since an unknown kind is a thing that must be reportable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameKind(pub u8);

impl FrameKind {
    /// Backend → frontend: ready to receive a write stream's blocks.
    pub const COPY_IN_RESPONSE: FrameKind = FrameKind(b'G');
    /// Either direction: one [block](crate::block) of facts.
    pub const COPY_DATA: FrameKind = FrameKind(b'd');
    /// Frontend → backend: the write stream's blocks are finished.
    pub const COPY_DONE: FrameKind = FrameKind(b'c');
    /// Backend → frontend: what the following rows look like, once per query
    /// stream. A query's row shape comes from its *head*, not from a predicate, so
    /// unlike a fact it cannot be read off the schema alone.
    pub const ROW_DESCRIPTION: FrameKind = FrameKind(b'T');
    /// Backend → frontend: one row of a result.
    pub const DATA_ROW: FrameKind = FrameKind(b'D');
    /// Either direction: this stream failed, and why.
    pub const ERROR: FrameKind = FrameKind(b'E');
}

impl std::fmt::Display for FrameKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Printable kinds read as themselves, which is most of the value of having
        // borrowed PG's letters.
        if self.0.is_ascii_graphic() {
            write!(f, "{}", self.0 as char)
        } else {
            write!(f, "0x{:02x}", self.0)
        }
    }
}

/// Which stream on the connection a frame belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(pub u32);

/// Bytes of header before a frame's payload.
pub const HEADER_LEN: usize = 1 + 4 + 4;

/// The most payload one frame may carry (64 MiB), matching a block's cap so that a
/// `CopyData` frame can hold exactly one maximal block.
pub const MAX_PAYLOAD: u32 = crate::block::MAX_PAYLOAD;

/// A frame's header, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: FrameKind,
    pub stream: StreamId,
    pub length: u32,
}

/// Append a frame carrying `payload`.
pub fn encode_frame(
    out: &mut Vec<u8>,
    kind: FrameKind,
    stream: StreamId,
    payload: &[u8],
) -> Result<(), WireError> {
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|n| *n <= MAX_PAYLOAD);

    let Some(length) = length else {
        return Err(WireError::BlockTooLarge {
            what: "frame payload bytes",
            declared: payload.len() as u64,
            max: u64::from(MAX_PAYLOAD),
        });
    };

    out.push(kind.0);
    out.extend_from_slice(&stream.0.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(payload);

    Ok(())
}

/// Read a frame's header without waiting for its payload.
///
/// What a reader on a socket calls first: nine bytes tell it how many more to await,
/// which is the whole reason the length is fixed-width and up front.
pub fn decode_header(bytes: &[u8]) -> Result<FrameHeader, WireError> {
    if bytes.len() < HEADER_LEN {
        return Err(WireError::UnexpectedEof);
    }

    let length = u32::from_le_bytes(bytes[5..9].try_into().expect("four bytes"));

    if length > MAX_PAYLOAD {
        return Err(WireError::BlockTooLarge {
            what: "frame payload bytes",
            declared: u64::from(length),
            max: u64::from(MAX_PAYLOAD),
        });
    }

    Ok(FrameHeader {
        kind: FrameKind(bytes[0]),
        stream: StreamId(u32::from_le_bytes(
            bytes[1..5].try_into().expect("four bytes"),
        )),
        length,
    })
}

/// Read one whole frame, returning its header, its payload, and the bytes consumed.
pub fn decode_frame(bytes: &[u8]) -> Result<(FrameHeader, &[u8], usize), WireError> {
    let header = decode_header(bytes)?;
    let end = HEADER_LEN + header.length as usize;

    if end > bytes.len() {
        return Err(WireError::LengthOutOfRange {
            declared: u64::from(header.length),
            available: bytes.len() - HEADER_LEN,
        });
    }

    Ok((header, &bytes[HEADER_LEN..end], end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::proptest::prelude::*;

    #[test]
    fn a_frame_round_trips_and_reports_what_it_consumed() {
        let mut out = vec![];
        encode_frame(
            &mut out,
            FrameKind::COPY_DATA,
            StreamId(7),
            b"a block would go here",
        )
        .expect("a frame");

        let (header, payload, used) = decode_frame(&out).expect("it decodes");

        assert_eq!(header.kind, FrameKind::COPY_DATA);
        assert_eq!(header.stream, StreamId(7));
        assert_eq!(payload, b"a block would go here");
        assert_eq!(used, out.len());
        assert_eq!(used, HEADER_LEN + payload.len());
    }

    /// **Streams interleave**, which is the whole reason for the `stream` field: PG's
    /// strictly-serial model is what leaves a short query stuck behind a long one.
    /// Frames of different streams alternate in one buffer and each is read back
    /// whole, in order, with its own stream intact.
    #[test]
    fn frames_of_different_streams_interleave_on_one_connection() {
        let sent = [
            (FrameKind::ROW_DESCRIPTION, StreamId(1), &b"query A"[..]),
            (FrameKind::COPY_DATA, StreamId(2), &b"write block"[..]),
            (FrameKind::DATA_ROW, StreamId(1), &b"row 1"[..]),
            (FrameKind::COPY_DATA, StreamId(2), &b"another block"[..]),
            (FrameKind::DATA_ROW, StreamId(1), &b"row 2"[..]),
            (FrameKind::COPY_DONE, StreamId(2), &b""[..]),
        ];

        let mut wire = vec![];
        for (kind, stream, payload) in sent {
            encode_frame(&mut wire, kind, stream, payload).expect("a frame");
        }

        let mut at = 0;
        let mut read = vec![];
        while at < wire.len() {
            let (header, payload, used) = decode_frame(&wire[at..]).expect("a frame decodes");
            read.push((header.kind, header.stream, payload.to_vec()));
            at += used;
        }

        assert_eq!(
            read,
            sent.iter()
                .map(|(k, s, p)| (*k, *s, p.to_vec()))
                .collect::<Vec<_>>()
        );
    }

    /// **An unknown kind decodes.** A framing layer delimits; it does not interpret.
    /// Refusing here would turn "a message I do not handle" into "your bytes are
    /// malformed", and would leave a reader unable to skip the frame it just failed
    /// to understand — which is the one thing the length is for.
    #[test]
    fn an_unknown_kind_is_delimited_not_rejected() {
        let mut out = vec![];
        encode_frame(&mut out, FrameKind(0xAB), StreamId(3), b"from the future").expect("a frame");

        let (header, payload, used) = decode_frame(&out).expect("it still decodes");
        assert_eq!(header.kind, FrameKind(0xAB));
        assert_eq!(payload, b"from the future");
        assert_eq!(used, out.len());

        // And it renders as something a diagnostic can print.
        assert_eq!(FrameKind(0xAB).to_string(), "0xab");
        assert_eq!(FrameKind::COPY_DATA.to_string(), "d");
    }

    /// A header arrives before its payload does, so the header alone has to be
    /// readable — that is what tells a socket reader how many more bytes to await.
    #[test]
    fn a_header_reads_before_its_payload_arrives() {
        let mut out = vec![];
        encode_frame(&mut out, FrameKind::COPY_DATA, StreamId(1), &[0u8; 500]).expect("a frame");

        let header = decode_header(&out[..HEADER_LEN]).expect("nine bytes are enough");
        assert_eq!(header.length, 500);

        // The whole frame is not there yet, and asking for it says so rather than
        // returning a short payload.
        assert!(matches!(
            decode_frame(&out[..HEADER_LEN + 100]),
            Err(WireError::LengthOutOfRange { .. })
        ));
    }

    /// **A length is bounded before it is trusted.** It sizes a read and, in a naive
    /// reader, an allocation — from a number a peer chose. The storage codec never
    /// faces this; its bytes come from our own disk.
    #[test]
    fn an_oversized_length_is_refused_without_reading_it() {
        let mut header = vec![FrameKind::COPY_DATA.0];
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&u32::MAX.to_le_bytes());

        assert!(matches!(
            decode_header(&header),
            Err(WireError::BlockTooLarge { .. })
        ));

        // And the encoder will not produce one either, so the bound is a property of
        // the format rather than of one side's caution.
        let mut out = vec![];
        assert!(matches!(
            encode_frame(
                &mut out,
                FrameKind::COPY_DATA,
                StreamId(1),
                &vec![0u8; MAX_PAYLOAD as usize + 1]
            ),
            Err(WireError::BlockTooLarge { .. })
        ));
    }

    #[test]
    fn a_truncated_header_is_refused() {
        let mut out = vec![];
        encode_frame(&mut out, FrameKind::COPY_DONE, StreamId(1), b"").expect("a frame");

        for cut in 0..HEADER_LEN {
            assert_eq!(decode_header(&out[..cut]), Err(WireError::UnexpectedEof));
        }
    }

    proptest! {
        #[test]
        fn any_frame_round_trips(
            kind in any::<u8>(),
            stream in any::<u32>(),
            payload in ::proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let mut out = vec![];
            encode_frame(&mut out, FrameKind(kind), StreamId(stream), &payload)
                .expect("within bounds");

            let (header, read, used) = decode_frame(&out).expect("it decodes");

            prop_assert_eq!(header.kind, FrameKind(kind));
            prop_assert_eq!(header.stream, StreamId(stream));
            prop_assert_eq!(read, &payload[..]);
            prop_assert_eq!(used, out.len());
        }

        /// A stream of frames is read back exactly, whatever the payloads — the
        /// property the interleaving test above is one case of.
        #[test]
        fn a_run_of_frames_reads_back_in_order(
            frames in ::proptest::collection::vec(
                (any::<u8>(), any::<u32>(), ::proptest::collection::vec(any::<u8>(), 0..64)),
                0..16,
            ),
        ) {
            let mut wire = vec![];
            for (kind, stream, payload) in &frames {
                encode_frame(&mut wire, FrameKind(*kind), StreamId(*stream), payload)
                    .expect("within bounds");
            }

            let mut at = 0;
            let mut read = vec![];
            while at < wire.len() {
                let (header, payload, used) = decode_frame(&wire[at..]).expect("a frame decodes");
                read.push((header.kind.0, header.stream.0, payload.to_vec()));
                at += used;
            }

            prop_assert_eq!(read, frames);
        }
    }
}
