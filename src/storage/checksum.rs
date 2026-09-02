#[test]
fn crc32c_matches_standard_vector() {
    assert_eq!(super::crc32c(b"123456789"), 0xe306_9283);
}
