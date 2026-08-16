//! **Blocks** — the unit a fact travels in, on a socket and on disk alike.
//!
//! One block is a run of facts of *one predicate*, so the predicate id is paid once
//! rather than per fact. An indexer writing in visitation order emits small blocks
//! in bursts; a post-merge writer emits huge ones; blocks coalesce monotonically
//! through k-merges until fully ordered
//! ([operations §8](../../../docs/aperture-cli-design.md)). The same bytes are a
//! `CopyData` frame's payload on the wire and a run of a fact file on disk, which is
//! what makes "one fact encoding, not two" a thing that can be checked.
//!
//! ```text
//!   [sync: FF × 10][magic "APBK"][name_len u32][count u32][length u32][crc32 u32][name][payload]
//!    └ 10 B         └────────────────── header, 20 B ─────────────────┘  └ name_len  └ length B
//! ```
//!
//! # The predicate is named, not numbered
//!
//! The header carried a `predicate u32` — the *database's* id — until Phase 8. That made a
//! fact file meaningful only against the database whose numbering produced it, and it made
//! every client keep a table of ids in step with a server's. A name costs about six more
//! bytes **once per block**, against payloads of hundreds to thousands of facts, and buys
//! both back: a client never learns a database's numbering, and a file is portable to any
//! database whose schema declares those names.
//!
//! It is also a better failure. A wrong id decoded the payload as some other predicate's
//! shape, silently; a name that is not there is [`WireError::UnknownPredicateName`], before
//! a byte of payload is trusted.
//!
//! The name sits **after** the fixed-width fields on purpose — see below, a splitter must
//! reach `length` at a fixed offset — and cannot contribute to a sync marker for the reason
//! a string cannot: it is UTF-8, and UTF-8 never uses `0xF8`–`0xFF`.
//!
//! # The sync marker is structurally impossible in a block's own bytes
//!
//! A fact file must be splittable at an arbitrary offset — seek anywhere, scan
//! forward to the next block, hand the rest to a worker — which is the property
//! Glean's opaque sequential `Batch` cannot offer and the reason this format has a
//! marker at all.
//!
//! [Operations §8](../../../docs/aperture-cli-design.md) specifies the marker as "a
//! reserved, structurally-illegal byte sequence (unused type-tag run the encoder
//! never emits)", and describes every hit as *only a candidate* because "values
//! carry arbitrary bytes (blobs/source text), so a marker can occur inside one".
//! **Both halves need amending for this codec, in opposite directions**, and the
//! result is stronger than either:
//!
//! - There are no type tags to reserve a run of. The value encoding is
//!   [schema-driven](crate::value) and emits none.
//! - But a marker *cannot* occur inside a value here, and the reason is the encoding
//!   rather than luck. Ten consecutive `0xFF` bytes are unreachable:
//!
//!   1. **Strings contribute no `0xFF` at all.** A string is length-prefixed UTF-8,
//!      and UTF-8 never uses `0xF8`–`0xFF` in any position.
//!   2. **A varint contributes at most nine.** Continuation bytes are `0x80`–`0xFF`
//!      and the final byte is below `0x80`, so a run ends where the varint does; the
//!      longest possible is `u64::MAX`, which is `FF` nine times and then `01`.
//!   3. **Runs cannot join across values.** A varint's last byte is below `0x80`,
//!      so it is never `0xFF`, and a string's bytes never are.
//!
//! So the marker is genuinely illegal in a payload rather than merely improbable —
//! and the header is built so it cannot contribute one either: `count` and `length`
//! are capped below `0x0100_0000` and `0x0400_0000`, which leaves each a zero top
//! byte, and the only field free to be all-ones is the four-byte checksum.
//!
//! **A marker therefore appears exactly once per block, at its start.** For a
//! well-formed file a scan finds block boundaries and nothing else. Validation —
//! magic, then the checksum — is still load-bearing, but for the fault it is
//! actually for: a torn write, a flipped bit, a file cut mid-block. Not for
//! disambiguating data that happened to look like a header.
//!
//! # Fixed-width fields, and little-endian
//!
//! The header is fixed-width where the payload is varints, because a splitter has to
//! read `length` *before* it can trust anything else — a variable-width field would
//! have to be parsed to be skipped, and the whole point is to skip. Little-endian
//! because there is nothing to order: the storage codec's big-endian is an
//! [I1](../../../docs/invariants.md#i1) requirement, and this is the file where that
//! requirement is not inherited.

use aperture_schema::schema::{PredicateId, Schema};

use crate::{
    crc,
    error::WireError,
    value::{WireFact, decode_fact, encode_fact},
};

/// The resynchronisation marker: ten `0xFF` bytes, which this encoder cannot
/// otherwise produce. See the module docs for why ten and not fewer.
pub const SYNC: [u8; 10] = [0xFF; 10];

/// Identifies a block header, so a candidate is rejected in four bytes before its
/// checksum is computed.
pub const MAGIC: [u8; 4] = *b"APBK";

/// Bytes of header after the marker: magic, name length, count, length, checksum.
pub const HEADER_LEN: usize = MAGIC.len() + 4 + 4 + 4 + 4;

/// Total framing per block.
pub const OVERHEAD: usize = SYNC.len() + HEADER_LEN;

/// The most facts one block may declare.
///
/// Chosen to keep the field's top byte zero, so `count` can never contribute to a
/// run of `0xFF` — the marker's impossibility is by construction, not by hoping the
/// numbers stay small.
pub const MAX_FACTS: u32 = 0x00FF_FFFF;

/// The most payload bytes one block may carry (64 MiB), capped for the same reason
/// and one more: a `length` read off a socket sizes an allocation, and a peer is not
/// to be trusted with that.
pub const MAX_PAYLOAD: u32 = 0x0400_0000;

/// The longest predicate name a block may carry.
///
/// Capped for the same reason `count` and `length` are: the top two bytes stay zero, so the
/// field cannot contribute to a run of `0xFF`. A fully-qualified predicate name is a few
/// dozen bytes, so this is a bound on absurdity rather than a limit anyone meets.
pub const MAX_NAME: u32 = 0x0000_FFFF;

/// What a block says about itself, before its facts are decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader<'a> {
    /// The predicate's fully-qualified name, borrowed from the block's own bytes.
    pub predicate: &'a str,
    pub count: u32,
    /// Payload bytes, *not* counting the name — so a block occupies
    /// `OVERHEAD + name.len() + length`.
    pub length: u32,
}

/// Append a block carrying `facts`, all of predicate `predicate`.
///
/// # Errors
///
/// [`WireError::TypeMismatch`] if a fact is of another predicate — the block header
/// names the predicate once, so a stray fact would be decoded as the wrong shape at
/// the far end rather than rejected.
pub fn encode_block(
    out: &mut Vec<u8>,
    schema: &Schema,
    predicate: PredicateId,
    facts: &[WireFact],
) -> Result<(), WireError> {
    let count = u32::try_from(facts.len()).ok().filter(|n| *n <= MAX_FACTS);
    let Some(count) = count else {
        return Err(WireError::BlockTooLarge {
            what: "facts",
            declared: facts.len() as u64,
            max: u64::from(MAX_FACTS),
        });
    };

    // The name is resolved here, from the schema this call already takes, so a caller
    // still speaks its own `PredicateId` and only the *wire* carries a name.
    let name = schema
        .get(predicate)
        .and_then(|p| p.name())
        .ok_or(WireError::UnknownPredicate(predicate.0))?;

    let name_len = u32::try_from(name.len()).ok().filter(|n| *n <= MAX_NAME);
    let Some(name_len) = name_len else {
        return Err(WireError::BlockTooLarge {
            what: "predicate name bytes",
            declared: name.len() as u64,
            max: u64::from(MAX_NAME),
        });
    };

    let mut payload = vec![];
    for fact in facts {
        if fact.predicate != predicate {
            return Err(WireError::TypeMismatch(
                "a block carries one predicate, and this fact is of another",
            ));
        }
        encode_fact(&mut payload, schema, fact)?;
    }

    let length = u32::try_from(payload.len())
        .ok()
        .filter(|n| *n <= MAX_PAYLOAD);
    let Some(length) = length else {
        return Err(WireError::BlockTooLarge {
            what: "payload bytes",
            declared: payload.len() as u64,
            max: u64::from(MAX_PAYLOAD),
        });
    };

    out.extend_from_slice(&SYNC);

    // The checksum covers the header's own fields as well as the payload, so a
    // corrupted `count` or `length` is caught rather than trusted — which matters
    // because `length` is what a splitter uses to skip.
    let mut header = Vec::with_capacity(HEADER_LEN - 4);
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&name_len.to_le_bytes());
    header.extend_from_slice(&count.to_le_bytes());
    header.extend_from_slice(&length.to_le_bytes());

    // Name as well as payload: it is as load-bearing as either, and a corrupted one
    // would otherwise resolve to a different predicate or to none.
    let checksum = crc::finish(crc::update(
        crc::update(crc::update(crc::start(), &header), name.as_bytes()),
        &payload,
    ));

    out.extend_from_slice(&header);
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&payload);

    Ok(())
}

/// Read the header of the block starting at `bytes[0]`, without decoding its facts.
///
/// What a splitter calls: it validates enough to trust `length`, then skips.
pub fn decode_header(bytes: &[u8]) -> Result<BlockHeader<'_>, WireError> {
    if bytes.len() < OVERHEAD {
        return Err(WireError::UnexpectedEof);
    }

    if bytes[..SYNC.len()] != SYNC {
        return Err(WireError::NoSyncMarker);
    }

    let header = &bytes[SYNC.len()..OVERHEAD];
    if header[..MAGIC.len()] != MAGIC {
        return Err(WireError::BadMagic);
    }

    let field = |at: usize| -> u32 {
        let start = MAGIC.len() + at * 4;
        u32::from_le_bytes(header[start..start + 4].try_into().expect("four bytes"))
    };

    let (name_len, count, length, declared_crc) = (field(0), field(1), field(2), field(3));

    if name_len > MAX_NAME {
        return Err(WireError::BlockTooLarge {
            what: "predicate name bytes",
            declared: u64::from(name_len),
            max: u64::from(MAX_NAME),
        });
    }

    if count > MAX_FACTS {
        return Err(WireError::BlockTooLarge {
            what: "facts",
            declared: u64::from(count),
            max: u64::from(MAX_FACTS),
        });
    }

    if length > MAX_PAYLOAD {
        return Err(WireError::BlockTooLarge {
            what: "payload bytes",
            declared: u64::from(length),
            max: u64::from(MAX_PAYLOAD),
        });
    }

    let name_at = OVERHEAD;
    let payload_at = name_at + name_len as usize;
    let payload_end = payload_at + length as usize;

    if payload_end > bytes.len() {
        return Err(WireError::LengthOutOfRange {
            declared: u64::from(name_len) + u64::from(length),
            available: bytes.len() - name_at,
        });
    }

    let name =
        std::str::from_utf8(&bytes[name_at..payload_at]).map_err(|_| WireError::BadString)?;

    // Header, then name, then payload — the same order and the same bytes
    // `encode_block` folded, minus the checksum field itself.
    let checksum = crc::finish(crc::update(
        crc::update(
            crc::update(crc::start(), &header[..MAGIC.len() + 12]),
            name.as_bytes(),
        ),
        &bytes[payload_at..payload_end],
    ));

    if checksum != declared_crc {
        return Err(WireError::ChecksumMismatch {
            declared: declared_crc,
            computed: checksum,
        });
    }

    Ok(BlockHeader {
        predicate: name,
        count,
        length,
    })
}

/// Decode a whole block, returning its facts and the bytes it occupied.
pub fn decode_block(bytes: &[u8], schema: &Schema) -> Result<(Vec<WireFact>, usize), WireError> {
    let header = decode_header(bytes)?;

    // Name → *this* reader's id. Two databases may number a predicate differently and
    // neither has to care, which is the whole point of naming it on the wire.
    let predicate = schema
        .find_position(header.predicate)
        .map(|(id, _)| id)
        .ok_or_else(|| WireError::UnknownPredicateName(header.predicate.to_owned()))?;

    let name_len = header.predicate.len();
    let payload = &bytes[OVERHEAD + name_len..OVERHEAD + name_len + header.length as usize];
    let mut facts = Vec::with_capacity(header.count.min(4096) as usize);
    let mut at = 0;

    for _ in 0..header.count {
        let (fact, used) = decode_fact(&payload[at..], schema, predicate)?;
        facts.push(fact);
        at += used;
    }

    // The count and the length have to agree with each other and with the facts. A
    // block whose payload is longer than its facts is not a block with slack in it —
    // it is a block whose header and body disagree, which means one of them is a
    // different block's.
    if at != payload.len() {
        return Err(WireError::TrailingBytes(payload.len() - at));
    }

    Ok((facts, OVERHEAD + name_len + header.length as usize))
}

/// The offset of the next block at or after `from`, or `None`.
///
/// The splitter's primitive: seek anywhere in a file, call this, and start reading
/// whole blocks. `memchr` finds the marker's first byte at memory bandwidth and the
/// run is confirmed behind it — the "SIMD `memchr`-style scan" operations §8 calls
/// for.
///
/// A hit is a **marker**, not yet a block: this deliberately does not validate the
/// header, because the caller is about to and because the two failures want telling
/// apart. In a well-formed file the marker cannot occur inside a block's bytes (see
/// the module docs), so a hit here is a boundary unless the file is damaged.
#[must_use]
pub fn find_sync(haystack: &[u8], from: usize) -> Option<usize> {
    let mut at = from;

    while at < haystack.len() {
        let found = memchr::memchr(SYNC[0], &haystack[at..])? + at;

        if haystack.len() - found >= SYNC.len() && haystack[found..found + SYNC.len()] == SYNC {
            return Some(found);
        }

        at = found + 1;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        value::{
            WireValue,
            proptest::{SchemaAndFact, arb_schema_and_fact},
        },
        varint,
    };
    use ::proptest::prelude::*;
    use aperture_schema::schema::{Predicate, PredicateTy};
    use lasso::Rodeo;
    use std::sync::Arc;

    /// The longest run of `0xFF` anywhere in `bytes`.
    fn longest_ff_run(bytes: &[u8]) -> usize {
        let (mut best, mut run) = (0, 0);
        for &byte in bytes {
            run = if byte == 0xFF { run + 1 } else { 0 };
            best = best.max(run);
        }
        best
    }

    fn blocked(spec: &SchemaAndFact) -> (aperture_schema::schema::Schema, Vec<u8>, Vec<WireFact>) {
        let schema = spec.schema();
        let fact = spec.fact(&schema);
        let facts = vec![fact.clone(), fact];

        let mut out = vec![];
        encode_block(&mut out, &schema, facts[0].predicate, &facts).expect("a well-typed block");

        (schema, out, facts)
    }

    /// **The marker cannot occur inside a payload**, which is the claim the whole
    /// splitter rests on and the one that would be embarrassing to assert and not
    /// check.
    ///
    /// The argument is in the module docs — UTF-8 never emits `0xFF`, a varint's
    /// last byte is below `0x80` so runs cannot join, and the longest run one varint
    /// can make is nine. This is that argument as an experiment, over generated
    /// facts including the strings and integers most likely to break it.
    #[test]
    fn a_payload_can_never_contain_the_marker() {
        use ::proptest::{
            strategy::{Strategy, ValueTree},
            test_runner::TestRunner,
        };

        let mut runner = TestRunner::deterministic();
        let mut worst = 0;

        for _ in 0..400 {
            let spec = arb_schema_and_fact()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let schema = spec.schema();
            let fact = spec.fact(&schema);

            let payload = crate::value::to_bytes(&schema, &fact).expect("encodes");
            worst = worst.max(longest_ff_run(&payload));
        }

        assert!(
            worst < SYNC.len(),
            "a payload reached a run of {worst} 0xFF bytes; the marker is {} and must be \
             unreachable",
            SYNC.len()
        );
    }

    /// The bound is **nine**, not "small enough" — and `u64::MAX` is what reaches it,
    /// so the marker's length is one more than the worst case rather than a round
    /// number someone picked.
    #[test]
    fn the_longest_reachable_run_is_nine() {
        let mut out = vec![];
        varint::put_u64(&mut out, u64::MAX);
        assert_eq!(longest_ff_run(&out), 9);
        assert_eq!(SYNC.len(), 10);

        // Two maximal varints back to back still cannot join: the first one's last
        // byte is a terminator, and a terminator is below 0x80.
        let mut pair = vec![];
        varint::put_u64(&mut pair, u64::MAX);
        varint::put_u64(&mut pair, u64::MAX);
        assert_eq!(longest_ff_run(&pair), 9);
    }

    /// And the *header* cannot contribute one either — which is why `count` and
    /// `length` are capped where they are. Only the checksum is free to be all-ones,
    /// and four is not ten.
    #[test]
    fn a_whole_block_holds_exactly_one_marker() {
        use ::proptest::{
            strategy::{Strategy, ValueTree},
            test_runner::TestRunner,
        };

        let mut runner = TestRunner::deterministic();

        for _ in 0..200 {
            let spec = arb_schema_and_fact()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let (_, block, _) = blocked(&spec);

            assert_eq!(find_sync(&block, 0), Some(0));
            assert_eq!(
                find_sync(&block, 1),
                None,
                "a second marker in a block would make a split ambiguous"
            );
        }
    }

    /// A block round-trips, and the splitter finds every boundary in a run of them —
    /// from an arbitrary offset, which is the whole point of a marker.
    #[test]
    fn blocks_round_trip_and_split_from_any_offset() {
        let mut rodeo = Rodeo::new();
        let name = rodeo.get_or_intern("gen.P0");
        let schema = aperture_schema::schema::Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![Predicate {
                name,
                key: PredicateTy::Str,
                value: None,
            }]),
        );

        let block_of = |texts: &[&str]| {
            let facts: Vec<WireFact> = texts
                .iter()
                .map(|t| WireFact {
                    predicate: PredicateId(0),
                    key: WireValue::Str((*t).to_owned()),
                    value: None,
                })
                .collect();
            let mut out = vec![];
            encode_block(&mut out, &schema, PredicateId(0), &facts).expect("a block");
            (out, facts)
        };

        let (a, a_facts) = block_of(&["store/keys.py", "store/codec.py"]);
        let (b, b_facts) = block_of(&["query/plan.py"]);
        let (c, c_facts) = block_of(&[]);

        let mut file = vec![];
        for part in [&a, &b, &c] {
            file.extend_from_slice(part);
        }

        // Read it as a stream of blocks.
        let mut at = 0;
        let mut seen = vec![];
        while at < file.len() {
            let (facts, used) = decode_block(&file[at..], &schema).expect("a block decodes");
            seen.push(facts);
            at += used;
        }
        assert_eq!(seen, vec![a_facts, b_facts, c_facts.clone()]);

        // And find the boundaries from anywhere, which is what a splitter does after
        // seeking blindly into the middle of a file.
        let boundaries: Vec<usize> = vec![0, a.len(), a.len() + b.len()];
        for from in 0..file.len() {
            let expected = boundaries.iter().copied().find(|b| *b >= from);
            assert_eq!(find_sync(&file, from), expected, "scanning from {from}");
        }
    }

    /// An empty block is a block: zero facts, a real header, a real checksum. The
    /// merge emits them, and a reader that treated one as end-of-input would stop
    /// early.
    #[test]
    fn an_empty_block_is_still_a_block() {
        let mut rodeo = Rodeo::new();
        let name = rodeo.get_or_intern("gen.P0");
        let schema = aperture_schema::schema::Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![Predicate {
                name,
                key: PredicateTy::Int,
                value: None,
            }]),
        );

        let mut out = vec![];
        encode_block(&mut out, &schema, PredicateId(0), &[]).expect("an empty block");

        // Framing plus the name it carries — a block names its predicate now, so the
        // floor is `OVERHEAD` and the name rather than `OVERHEAD` alone.
        let framed = OVERHEAD + "gen.P0".len();
        assert_eq!(out.len(), framed);
        assert_eq!(decode_block(&out, &schema), Ok((vec![], framed)));
    }

    /// A fact of another predicate is refused at encode, because the header names
    /// the predicate once: sending it would have the far end decode the fact against
    /// the wrong shape rather than reject it.
    #[test]
    fn a_block_carries_one_predicate() {
        let mut rodeo = Rodeo::new();
        let (p0, p1) = (rodeo.get_or_intern("gen.P0"), rodeo.get_or_intern("gen.P1"));
        let schema = aperture_schema::schema::Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: p0,
                    key: PredicateTy::Int,
                    value: None,
                },
                Predicate {
                    name: p1,
                    key: PredicateTy::Int,
                    value: None,
                },
            ]),
        );

        let stray = WireFact {
            predicate: PredicateId(1),
            key: WireValue::Int(1),
            value: None,
        };

        let mut out = vec![];
        assert!(matches!(
            encode_block(&mut out, &schema, PredicateId(0), &[stray]),
            Err(WireError::TypeMismatch(_))
        ));
    }

    proptest! {
        /// **A block round-trips**, over generated schemas and facts.
        #[test]
        fn a_block_round_trips(spec in arb_schema_and_fact()) {
            let (schema, block, facts) = blocked(&spec);
            prop_assert_eq!(decode_block(&block, &schema), Ok((facts, block.len())));
        }

        /// **Corruption anywhere in a block is caught.** Every byte, every bit — the
        /// checksum covers the header's own fields as well as the payload, so a
        /// corrupted `length` is refused rather than used to skip to the wrong place.
        ///
        /// The marker itself is excluded: flipping a bit there does not corrupt a
        /// block, it *destroys* the boundary, and the splitter's answer is to find
        /// the next one rather than to report a bad block.
        #[test]
        fn a_corrupted_block_never_decodes_as_a_good_one(
            spec in arb_schema_and_fact(),
            bit in 0u8..8,
            offset in 0usize..64,
        ) {
            let (schema, block, facts) = blocked(&spec);
            let at = SYNC.len() + offset % (block.len() - SYNC.len());

            let mut corrupt = block.clone();
            corrupt[at] ^= 1 << bit;

            match decode_block(&corrupt, &schema) {
                Err(_) => {}
                Ok((decoded, _)) => prop_assert_eq!(
                    decoded, facts,
                    "a corrupted block decoded to different facts without an error"
                ),
            }
        }

        /// A block cut short is an error at every cut point, never a shorter block.
        #[test]
        fn a_truncated_block_never_decodes(spec in arb_schema_and_fact()) {
            let (schema, block, _) = blocked(&spec);

            for cut in 0..block.len() {
                prop_assert!(
                    decode_block(&block[..cut], &schema).is_err(),
                    "a block cut to {} of {} bytes decoded", cut, block.len()
                );
            }
        }
    }
}
