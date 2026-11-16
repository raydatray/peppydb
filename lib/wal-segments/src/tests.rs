use super::*;

const PLAIN: Format = Format {
    magic: *b"TESTPLAN",
    version: 1,
    extension: "seg",
    min_body: 1,
    max_body: 1 << 10,
    align: 1,
};

const PADDED: Format = Format {
    magic: *b"TESTPADS",
    align: 8,
    ..PLAIN
};

fn framed(format: &Format, bodies: &[&[u8]]) -> Vec<u8> {
    let mut buf = format.encode_header(1).to_vec();
    for body in bodies {
        format.frame(&mut buf, |buf| buf.extend(*body));
    }
    buf
}

fn decode_all<'a>(format: &Format, bytes: &'a [u8]) -> (Vec<&'a [u8]>, usize) {
    let mut offset = HEADER_BYTES;
    let mut bodies = Vec::new();
    while let Some((body, used)) = format.decode_frame(&bytes[offset..]) {
        bodies.push(body);
        offset += used;
    }
    (bodies, offset)
}

#[test]
fn crc32c_matches_the_rfc_3720_check_value() {
    assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn headers_round_trip_and_reject_damage() {
    for format in [PLAIN, PADDED] {
        let header = format.encode_header(12_034);
        assert_eq!(format.decode_header(&header), Ok(12_034));
        assert_eq!(format.decode_header(&header[..31]), Err("short header"));
        let mut bad = header;
        bad[0] ^= 1;
        assert_eq!(format.decode_header(&bad), Err("bad magic"));
        let mut bad = header;
        bad[17] ^= 1;
        assert_eq!(format.decode_header(&bad), Err("bad header checksum"));
        let other = Format {
            version: 2,
            ..format
        };
        assert_eq!(other.decode_header(&header), Err("unsupported version"));
    }
    // One log's header is not another's.
    assert_eq!(
        PADDED.decode_header(&PLAIN.encode_header(1)),
        Err("bad magic")
    );
}

#[test]
fn frames_round_trip_with_and_without_padding() {
    let bodies: Vec<Vec<u8>> = (1..=20).map(|n| vec![n as u8; n]).collect();
    let refs: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
    for format in [PLAIN, PADDED] {
        let bytes = framed(&format, &refs);
        let (decoded, end) = decode_all(&format, &bytes);
        assert_eq!(decoded, refs);
        assert_eq!(end, bytes.len());
        let sizes: usize = bodies.iter().map(|b| format.frame_bytes(b.len())).sum();
        assert_eq!(bytes.len(), HEADER_BYTES + sizes);
    }
}

#[test]
fn padded_frames_start_at_eight_aligned_offsets() {
    let bodies: Vec<Vec<u8>> = (1..=17).map(|n| vec![7; n]).collect();
    let refs: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
    let bytes = framed(&PADDED, &refs);
    let mut offset = HEADER_BYTES;
    while offset < bytes.len() {
        assert_eq!(offset % 8, 0);
        offset += PADDED.decode_frame(&bytes[offset..]).unwrap().1;
    }
    assert_eq!(PLAIN.frame_bytes(3), 11);
    assert_eq!(PADDED.frame_bytes(3), 16);
    assert_eq!(PADDED.frame_bytes(8), 16);
}

#[test]
fn damaged_frames_do_not_decode() {
    for format in [PLAIN, PADDED] {
        let bytes = framed(&format, &[b"hello"]);
        let frame = &bytes[HEADER_BYTES..];
        // Every cut is a torn frame.
        for cut in 0..frame.len() {
            assert_eq!(format.decode_frame(&frame[..cut]), None, "cut {cut}");
        }
        // Every flipped bit in the prefix or body is caught.
        for at in 0..PREFIX_BYTES + 5 {
            let mut bad = frame.to_vec();
            bad[at] ^= 0x10;
            assert_eq!(format.decode_frame(&bad), None, "flip {at}");
        }
    }
    // Non-zero padding is damage.
    let mut bad = framed(&PADDED, &[b"hello"])[HEADER_BYTES..].to_vec();
    *bad.last_mut().unwrap() = 1;
    assert_eq!(PADDED.decode_frame(&bad), None);
}

#[test]
fn bodies_outside_the_bounds_and_zeros_do_not_decode() {
    assert_eq!(PLAIN.decode_frame(&[0; 64]), None);
    let mut big = Vec::new();
    PLAIN.frame(&mut big, |buf| buf.resize(buf.len() + (1 << 10) + 1, 0));
    assert_eq!(PLAIN.decode_frame(&big), None);
    let strict = Format {
        min_body: 4,
        ..PLAIN
    };
    let mut small = Vec::new();
    strict.frame(&mut small, |buf| buf.extend([1, 2, 3]));
    assert_eq!(strict.decode_frame(&small), None);
}

#[test]
fn intact_frame_after_finds_later_frames_only() {
    for format in [PLAIN, PADDED] {
        let bytes = framed(&format, &[b"one", b"two", b"three"]);
        let first = format.frame_bytes(3);
        let second_at = HEADER_BYTES + first;
        let mut damaged = bytes.clone();
        damaged[HEADER_BYTES + 9] ^= 1; // inside the first body
        assert!(format.intact_frame_after(&damaged, HEADER_BYTES, |_| true));
        assert!(format.intact_frame_after(&damaged, HEADER_BYTES, |body| body == b"three"));
        assert!(!format.intact_frame_after(&damaged, HEADER_BYTES, |body| body == b"one"));
        // Nothing after the last frame.
        let last_at = second_at + format.frame_bytes(3);
        assert!(!format.intact_frame_after(&bytes, last_at, |_| true));
    }
}

#[test]
fn names_round_trip_and_other_files_are_ignored() {
    assert_eq!(PLAIN.file_name(42), "00000000000000000042.seg");
    assert_eq!(PLAIN.parse_name("00000000000000000042.seg"), Some(42));
    for other in [
        "00000000000000000042.wal",
        "0000000000000000042.seg",
        "0000000000000000004x.seg",
        "00000000000000000042seg",
        "state",
    ] {
        assert_eq!(PLAIN.parse_name(other), None, "{other}");
    }

    let dir = tempfile::tempdir().unwrap();
    assert!(PLAIN.list(&dir.path().join("absent")).unwrap().is_empty());
    for first in [30, 1, 7] {
        std::fs::write(PLAIN.path(dir.path(), first), b"").unwrap();
    }
    std::fs::write(dir.path().join("state"), b"").unwrap();
    std::fs::write(PADDED.path(dir.path(), 2).with_extension("wal"), b"").unwrap();
    let firsts: Vec<_> = PLAIN
        .list(dir.path())
        .unwrap()
        .into_iter()
        .map(|(first, _)| first)
        .collect();
    assert_eq!(firsts, [1, 7, 30]);
    sync_dir(dir.path()).unwrap();
}
