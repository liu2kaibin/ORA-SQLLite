use crate::{DbError, ErrorCode, Result};

const CHECKSUM_OFFSET: usize = 36;
const CHECKSUM_LEN: usize = 4;

pub(super) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;

    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0x82f6_3b78;
            } else {
                crc >>= 1;
            }
        }
    }

    !crc
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
    let mut checksummed = bytes.to_vec();
    let checksum_field = &mut checksummed[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN];
    checksum_field.fill(0);
    crc32c(&checksummed)
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
