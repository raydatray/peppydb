//! The record and segment-header format.

use crate::record::{
    HEADER_BYTES, MAX_RECORD_BYTES, batch_bytes, decode, decode_header, encode_batch,
    encode_header, encode_op, op_bytes,
};
use crate::{Op, Record};

fn put<'a>(key: &'a [u8], value: &'a [u8]) -> Op<'a> {
    Op::Put { key, value }
}

fn encoded_op(seq: u64, op: Op<'_>) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_op(&mut buf, seq, op);
    assert_eq!(buf.len(), op_bytes(op));
    buf
}

fn encoded_batch(seq: u64, ops: &[Op<'_>]) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_batch(&mut buf, seq, ops);
    assert_eq!(buf.len(), batch_bytes(ops));
    buf
}

#[test]
fn crc32c_matches_the_rfc_3720_check_value() {
    assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn every_kind_round_trips() {
    let long = vec![7u8; 5000];
    let ops = [
        put(b"key", b"value"),
        put(b"", b""),
        Op::Delete { key: b"gone" },
        Op::Delete { key: b"" },
        put(&long, &long),
    ];
    for (seq, op) in (1..).zip(ops) {
        let buf = encoded_op(seq, op);
        assert_eq!(
            decode(&buf),
            Some((Record { seq, ops: vec![op] }, buf.len()))
        );
    }
    for batch in [&ops[..], &[], &ops[2..3]] {
        let buf = encoded_batch(99, batch);
        let (record, used) = decode(&buf).unwrap();
        assert_eq!(
            (record.seq, record.ops.as_slice(), used),
            (99, batch, buf.len())
        );
    }
    // Records decode one after another.
    let mut buf = encoded_op(1, ops[0]);
    buf.extend(encoded_batch(2, &ops));
    let (first, used) = decode(&buf).unwrap();
    assert_eq!(first.seq, 1);
    assert_eq!(decode(&buf[used..]).unwrap().0.seq, 2);
}

#[test]
fn every_cut_and_every_bit_flip_fails_to_decode() {
    let ops = [put(b"abc", b"defgh"), Op::Delete { key: b"x" }];
    for buf in [encoded_op(5, ops[0]), encoded_batch(6, &ops)] {
        for cut in 0..buf.len() {
            assert_eq!(decode(&buf[..cut]), None, "cut at {cut}");
        }
        for byte in 0..buf.len() {
            for bit in 0..8 {
                let mut flipped = buf.clone();
                flipped[byte] ^= 1 << bit;
                assert_eq!(decode(&flipped), None, "flip {byte}.{bit}");
            }
        }
    }
}

#[test]
fn bad_lengths_zeros_nesting_and_trailing_bytes_fail() {
    let mut buf = encoded_op(1, put(b"k", b"v"));
    for len in [0u32, 8, u32::MAX, MAX_RECORD_BYTES as u32 + 1] {
        buf[..4].copy_from_slice(&len.to_le_bytes());
        assert_eq!(decode(&buf), None, "len {len}");
    }
    assert_eq!(decode(&[0; 64]), None);

    // A batch nesting a batch, with a correct checksum.
    let mut nested = encoded_batch(1, &[]);
    nested.truncate(nested.len() - 4);
    let body_rest = [
        1u32.to_le_bytes().to_vec(),
        vec![2],
        0u32.to_le_bytes().to_vec(),
    ]
    .concat();
    nested.extend(&body_rest);
    reseal(&mut nested);
    assert_eq!(decode(&nested), None);

    // Trailing payload bytes inside the record.
    let mut trailing = encoded_op(1, Op::Delete { key: b"k" });
    trailing.push(0);
    reseal(&mut trailing);
    assert_eq!(decode(&trailing), None);

    // An unknown kind.
    let mut unknown = encoded_op(1, Op::Delete { key: b"k" });
    unknown[16] = 9;
    reseal(&mut unknown);
    assert_eq!(decode(&unknown), None);
}

/// Recompute `len` and `crc` over the body.
fn reseal(buf: &mut [u8]) {
    let len = (buf.len() - 8) as u32;
    buf[..4].copy_from_slice(&len.to_le_bytes());
    let crc = crc32c::crc32c_append(crc32c::crc32c(&len.to_le_bytes()), &buf[8..]);
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn segment_headers_round_trip_and_reject_damage() {
    let header = encode_header(12_034);
    assert_eq!(header.len(), HEADER_BYTES);
    assert_eq!(decode_header(&header), Ok(12_034));
    let mut bad = header;
    bad[0] = b'X';
    assert_eq!(decode_header(&bad), Err("bad magic"));
    let mut bad = header;
    bad[17] ^= 1;
    assert_eq!(decode_header(&bad), Err("bad header checksum"));
    let mut bad = header;
    bad[8] = 2;
    let crc = crc32c::crc32c(&bad[..24]);
    bad[24..28].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(decode_header(&bad), Err("unsupported version"));
    assert_eq!(decode_header(&header[..31]), Err("short header"));
}
