//! **One fact encoding, not two** — the claim
//! [operations §8](../../../docs/aperture-cli-design.md) makes about the wire and the
//! fact file, checked rather than asserted.
//!
//! An integration test rather than a unit one for two reasons. It spans `value`,
//! `block` and `frame`, so it belongs to none of them; and it exercises the crate
//! from outside, which is the position a client is in — the one place a `pub` that
//! should not be, or a type a caller cannot construct, shows up.

use aperture_schema::{
    id::FactId,
    schema::{Predicate, PredicateId, PredicateTy, Schema},
};
use aperture_wire::{
    FrameKind, StreamId, WireFact, WireRef, WireValue, block, decode_block, decode_frame,
    encode_block, encode_frame, find_sync,
};
use lasso::Rodeo;
use std::sync::Arc;

/// A two-predicate code index: files, and declarations that reference one.
fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let (file, decl) = (
        rodeo.get_or_intern("src.File"),
        rodeo.get_or_intern("src.Decl"),
    );
    let (f_file, f_line, f_name) = (
        rodeo.get_or_intern("file"),
        rodeo.get_or_intern("line"),
        rodeo.get_or_intern("name"),
    );

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![
            Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: decl,
                // Sorted by name, as everywhere: file, line, name.
                key: PredicateTy::Record(
                    vec![
                        (f_file, PredicateTy::Fact(PredicateId(0))),
                        (f_line, PredicateTy::Int),
                        (f_name, PredicateTy::Str),
                    ]
                    .into(),
                ),
                value: None,
            },
        ]),
    )
}

fn decl(file: WireRef, line: i64, name: &str) -> WireFact {
    WireFact {
        predicate: PredicateId(1),
        key: WireValue::Record(
            vec![
                WireValue::Ref(file),
                WireValue::Int(line),
                WireValue::Str(name.to_owned()),
            ]
            .into(),
        ),
        value: None,
    }
}

fn file(path: &str) -> WireFact {
    WireFact {
        predicate: PredicateId(0),
        key: WireValue::Str(path.to_owned()),
        value: None,
    }
}

/// **The same bytes ride a frame and sit in a file.**
///
/// One block is encoded once. It is then read two ways: as the payload of a
/// `CopyData` frame, the way a write stream carries it, and as a run of a fact file,
/// the way the bulk path will. Both yield the same facts, because there is one
/// encoding — which is what stops the file format and the wire format drifting into
/// two things that have to be kept in step by hand.
#[test]
fn a_block_is_the_same_bytes_on_a_socket_and_in_a_file() {
    let schema = schema();

    // A declaration whose reference is *nested* — what an indexer sends, holding no
    // ids at all — and one whose reference is an id, from a producer that has one.
    let nested = decl(
        WireRef::Nested(Box::new(file("store/keys.py"))),
        12,
        "key_of",
    );
    let by_id = decl(
        WireRef::Id(FactId::new(PredicateId(0), 3).expect("an id")),
        48,
        "Store.put",
    );

    let facts = vec![nested, by_id];

    let mut block_bytes = vec![];
    encode_block(&mut block_bytes, &schema, PredicateId(1), &facts).expect("a block");

    // On the wire: the block is a CopyData frame's payload, and nothing else.
    let mut wire = vec![];
    encode_frame(&mut wire, FrameKind::COPY_DATA, StreamId(1), &block_bytes).expect("a frame");

    let (header, payload, used) = decode_frame(&wire).expect("the frame decodes");
    assert_eq!(header.kind, FrameKind::COPY_DATA);
    assert_eq!(used, wire.len());
    assert_eq!(
        payload,
        &block_bytes[..],
        "a frame carries the block verbatim"
    );

    let (from_wire, _) = decode_block(payload, &schema).expect("the block decodes");

    // In a file: the same block bytes, found by scanning rather than by being handed
    // a frame boundary.
    let file_bytes = block_bytes.clone();
    let at = find_sync(&file_bytes, 0).expect("a block boundary");
    let (from_file, _) = decode_block(&file_bytes[at..], &schema).expect("the block decodes");

    assert_eq!(from_wire, facts);
    assert_eq!(from_file, facts);
}

/// A file of several blocks splits at an arbitrary offset — seek anywhere, scan to
/// the next boundary, read whole blocks from there. This is the property a Glean
/// `Batch` cannot offer, and the reason the format carries a marker at all.
#[test]
fn a_file_splits_at_an_arbitrary_offset() {
    let schema = schema();

    let batches = vec![
        (
            PredicateId(0),
            vec![file("store/keys.py"), file("store/codec.py")],
        ),
        (
            PredicateId(1),
            vec![decl(
                WireRef::Id(FactId::new(PredicateId(0), 1).expect("an id")),
                7,
                "encode_key",
            )],
        ),
        (PredicateId(0), vec![file("query/plan.py")]),
    ];

    let mut file_bytes = vec![];
    let mut boundaries = vec![];

    for (predicate, facts) in &batches {
        boundaries.push(file_bytes.len());
        encode_block(&mut file_bytes, &schema, *predicate, facts).expect("a block");
    }

    // From every possible offset, the scan lands on the next real boundary — and the
    // block there reads whole. A worker handed a byte range does exactly this.
    for from in 0..file_bytes.len() {
        let found = find_sync(&file_bytes, from);
        let expected = boundaries.iter().copied().find(|b| *b >= from);
        assert_eq!(found, expected, "scanning from offset {from}");

        if let Some(at) = found {
            let which = boundaries
                .iter()
                .position(|b| *b == at)
                .expect("a boundary");
            let (facts, _) = decode_block(&file_bytes[at..], &schema).expect("a block decodes");
            assert_eq!(facts, batches[which].1);
        }
    }
}

/// A file cut mid-block — a truncated upload, a killed writer — is reported at the
/// damaged block rather than silently yielding fewer facts.
#[test]
fn a_file_cut_mid_block_is_reported_not_shortened() {
    let schema = schema();

    let mut file_bytes = vec![];
    encode_block(&mut file_bytes, &schema, PredicateId(0), &[file("a.py")]).expect("a block");
    let first = file_bytes.len();
    encode_block(&mut file_bytes, &schema, PredicateId(0), &[file("b.py")]).expect("a block");

    // Cut anywhere inside the second block.
    for cut in first + 1..file_bytes.len() {
        let truncated = &file_bytes[..cut];

        let (facts, used) = decode_block(truncated, &schema).expect("the first block is intact");
        assert_eq!(facts.len(), 1);
        assert_eq!(used, first);

        assert!(
            decode_block(&truncated[used..], &schema).is_err(),
            "a block cut at {cut} decoded instead of reporting damage"
        );
    }
}

/// The overhead is what the format says it is, and small against a real block —
/// worth pinning, because a marker and a header are the price paid for splittability
/// and the trade is only good while it stays this size.
#[test]
fn framing_overhead_is_thirty_bytes_a_block_and_nine_a_frame() {
    let schema = schema();

    assert_eq!(block::OVERHEAD, 30);
    assert_eq!(aperture_wire::frame::HEADER_LEN, 9);

    let facts: Vec<WireFact> = (0..100)
        .map(|n| {
            decl(
                WireRef::Id(FactId::new(PredicateId(0), 1).expect("an id")),
                n,
                "some_declaration_name",
            )
        })
        .collect();

    let mut block_bytes = vec![];
    encode_block(&mut block_bytes, &schema, PredicateId(1), &facts).expect("a block");

    let payload = block_bytes.len() - block::OVERHEAD;
    assert!(
        block::OVERHEAD * 50 < payload,
        "framing is {} B against {payload} B of facts, which is more than 2% overhead",
        block::OVERHEAD
    );
}
