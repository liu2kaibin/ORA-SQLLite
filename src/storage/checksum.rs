use crate::{DbError, ErrorCode, Result};

const CHECKSUM_OFFSET: usize = 36;
const CHECKSUM_LEN: usize = 4;

pub(super) struct Crc32c(u32);

impl Crc32c {
    pub(super) const fn new() -> Self {
        Self(!0)
    }

    pub(super) fn update(&mut self, bytes: &[u8]) {
        crc32c_update(&mut self.0, bytes);
    }

    pub(super) const fn finish(self) -> u32 {
        !self.0
    }
}

pub(super) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(bytes);
    crc.finish()
}

fn crc32c_update(crc: &mut u32, bytes: &[u8]) {
    for &byte in bytes {
        *crc ^= u32::from(byte);
        for _ in 0..8 {
            if *crc & 1 == 1 {
                *crc = (*crc >> 1) ^ 0x82f6_3b78;
            } else {
                *crc >>= 1;
            }
        }
    }
}

pub(super) fn stamp_page_checksum(bytes: &mut [u8]) -> Result<u32> {
    validate_page_size(bytes.len())?;
    let checksum = checksum_with_zeroed_field(bytes);
    bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN].copy_from_slice(&checksum.to_le_bytes());
    Ok(checksum)
}

pub(super) fn verify_page_checksum(bytes: &[u8]) -> Result<()> {
    validate_page_size(bytes.len())?;
    let stored = u32::from_le_bytes(
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN]
            .try_into()
            .expect("checksum field has a fixed length"),
    );
    if stored != checksum_with_zeroed_field(bytes) {
        return Err(corruption("invalid page checksum"));
    }
    Ok(())
}

fn checksum_with_zeroed_field(bytes: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(&bytes[..CHECKSUM_OFFSET]);
    crc.update(&[0; CHECKSUM_LEN]);
    crc.update(&bytes[CHECKSUM_OFFSET + CHECKSUM_LEN..]);
    crc.finish()
}

fn validate_page_size(page_len: usize) -> Result<()> {
    if matches!(page_len, 4096 | 8192 | 16384) {
        Ok(())
    } else {
        Err(corruption("invalid page size for checksum"))
    }
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[test]
fn crc32c_matches_standard_vector() {
    assert_eq!(crc32c(b"123456789"), 0xe306_9283);
}

#[test]
fn streaming_crc32c_matches_the_same_bytes_in_one_slice() {
    let mut checksum = Crc32c::new();
    checksum.update(b"123");
    checksum.update(b"");
    checksum.update(b"456789");
    assert_eq!(checksum.finish(), 0xe306_9283);
}

#[test]
fn whole_page_checksum_ignores_only_its_own_field() {
    let mut page = vec![0x5a; 4096];
    page[36..40].fill(0);
    let stored = stamp_page_checksum(&mut page).expect("stamp");
    assert_eq!(stored, u32::from_le_bytes(page[36..40].try_into().unwrap()));
    verify_page_checksum(&page).expect("valid page");
    page[100] ^= 1;
    assert_eq!(
        verify_page_checksum(&page).unwrap_err().code(),
        crate::ErrorCode::Corruption
    );
}

#[test]
fn segmented_page_checksum_matches_the_byte_identical_zeroed_field_contract() {
    for page_size in [4096usize, 8192, 16384] {
        let mut original = (0..page_size)
            .map(|index| u8::try_from(index % 251).expect("test byte"))
            .collect::<Vec<_>>();
        original[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN]
            .copy_from_slice(&0xa5a5_5a5au32.to_le_bytes());

        let mut expected = original.clone();
        expected[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN].fill(0);
        let checksum = crc32c(&expected);
        expected[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN]
            .copy_from_slice(&checksum.to_le_bytes());

        let mut actual = original;
        assert_eq!(
            stamp_page_checksum(&mut actual).expect("stamp whole page"),
            checksum
        );
        assert_eq!(actual, expected, "page size {page_size}");
        verify_page_checksum(&actual).expect("verify byte-identical page");
    }
}
