//! The CRC32C that superblocks and pages use.

/// RFC 3720 §B.4 gives the CRC32C (Castagnoli) check value.
#[test]
fn crc32c_matches_the_rfc_3720_check_value() {
    assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
    assert_eq!(crc32c::crc32c(&[]), 0);
    assert_eq!(crc32c::crc32c(&[0; 32]), 0x8A91_36AA);
}
