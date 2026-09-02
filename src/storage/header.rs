use super::{
    Lsn, PageId,
    codec::{Decoder, Encoder},
    crc32c,
};
use crate::{DbError, ErrorCode, Result};

const DATABASE_HEADER_LEN: usize = 128;
const DATABASE_HEADER_PREFIX_LEN: usize = 124;
const DATABASE_MAGIC: &[u8; 16] = b"ORA-SQLLITE\0\0\0\0\0";
const FORMAT_MAJOR: u16 = 1;
const FORMAT_MINOR: u16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DatabaseHeader {
    major: u16,
    minor: u16,
    page_size: u32,
    uuid: [u8; 16],
    catalog_root: PageId,
    freelist_root: PageId,
    checkpoint_lsn: Lsn,
    features: u64,
}

impl DatabaseHeader {
    pub(super) fn new(page_size: u32, uuid: [u8; 16]) -> Result<Self> {
        if !is_valid_page_size(page_size) {
            return Err(DbError::new(
                ErrorCode::Type,
                "database page size must be 4096, 8192, or 16384 bytes",
            ));
        }

        Ok(Self {
            major: FORMAT_MAJOR,
            minor: FORMAT_MINOR,
            page_size,
            uuid,
            catalog_root: PageId(0),
            freelist_root: PageId(0),
            checkpoint_lsn: Lsn(0),
            features: 0,
        })
    }

    pub(super) fn encode(&self) -> [u8; DATABASE_HEADER_LEN] {
        let mut encoder = Encoder::new();
        encoder.put_bytes(DATABASE_MAGIC);
        encoder.put_u16(self.major);
        encoder.put_u16(self.minor);
        encoder.put_u32(self.page_size);
        encoder.put_bytes(&self.uuid);
        encoder.put_u64(self.catalog_root.0);
        encoder.put_u64(self.freelist_root.0);
        encoder.put_u64(self.checkpoint_lsn.0);
        encoder.put_u64(self.features);
        encoder.put_bytes(&[0; 52]);

        let mut bytes = encoder.into_bytes();
        debug_assert_eq!(bytes.len(), DATABASE_HEADER_PREFIX_LEN);
        let checksum = crc32c(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes
            .try_into()
            .expect("database header encoding has a fixed length")
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != DATABASE_HEADER_LEN {
            return Err(corruption("database header must be exactly 128 bytes"));
        }

        let mut decoder = Decoder::new(bytes);
        let magic = decoder.get_bytes(16)?;
        let major = decoder.get_u16()?;
        let minor = decoder.get_u16()?;
        let page_size = decoder.get_u32()?;
        let mut uuid = [0; 16];
        uuid.copy_from_slice(decoder.get_bytes(16)?);
        let catalog_root = PageId(decoder.get_u64()?);
        let freelist_root = PageId(decoder.get_u64()?);
        let checkpoint_lsn = Lsn(decoder.get_u64()?);
        let features = decoder.get_u64()?;
        let reserved = decoder.get_bytes(52)?;
        let stored_checksum = decoder.get_u32()?;

        if magic != DATABASE_MAGIC {
            return Err(corruption("invalid database header magic"));
        }
        if stored_checksum != crc32c(&bytes[..DATABASE_HEADER_PREFIX_LEN]) {
            return Err(corruption("invalid database header checksum"));
        }
        if major != FORMAT_MAJOR {
            return Err(corruption("unsupported database format major version"));
        }
        if !is_valid_page_size(page_size) {
            return Err(corruption("invalid database page size"));
        }
        if features != 0 {
            return Err(corruption("database header has unknown feature bits"));
        }
        if reserved.iter().any(|&byte| byte != 0) {
            return Err(corruption("database header reserved bytes are nonzero"));
        }

        Ok(Self {
            major,
            minor,
            page_size,
            uuid,
            catalog_root,
            freelist_root,
            checkpoint_lsn,
            features,
        })
    }
}

const fn is_valid_page_size(page_size: u32) -> bool {
    matches!(page_size, 4096 | 8192 | 16384)
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[test]
fn database_header_round_trips() {
    let header = DatabaseHeader::new(4096, [7; 16]).expect("valid header");
    let bytes = header.encode();
    assert_eq!(&bytes[..16], b"ORA-SQLLITE\0\0\0\0\0");
    assert_eq!(DatabaseHeader::decode(&bytes).expect("decode"), header);
}

#[test]
fn database_header_rejects_corruption_and_page_size() {
    assert!(DatabaseHeader::new(2048, [1; 16]).is_err());
    let mut bytes = DatabaseHeader::new(8192, [1; 16]).expect("header").encode();
    bytes[32] ^= 1;
    assert_eq!(
        DatabaseHeader::decode(&bytes).expect_err("checksum").code(),
        crate::ErrorCode::Corruption
    );
}

#[test]
fn database_header_uses_fixed_little_endian_offsets() {
    let mut header = DatabaseHeader::new(4096, [0x5a; 16]).expect("header");
    header.catalog_root = PageId(0x0102_0304_0506_0708);
    header.freelist_root = PageId(0x1112_1314_1516_1718);
    header.checkpoint_lsn = Lsn(0x2122_2324_2526_2728);

    let bytes = header.encode();

    assert_eq!(bytes.len(), 128);
    assert_eq!(&bytes[16..20], &[1, 0, 0, 0]);
    assert_eq!(&bytes[20..24], &[0, 0x10, 0, 0]);
    assert_eq!(&bytes[24..40], &[0x5a; 16]);
    assert_eq!(&bytes[40..48], &[8, 7, 6, 5, 4, 3, 2, 1]);
    assert_eq!(
        &bytes[48..56],
        &[0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]
    );
    assert_eq!(
        &bytes[56..64],
        &[0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21]
    );
    assert_eq!(&bytes[64..72], &[0; 8]);
    assert_eq!(&bytes[72..124], &[0; 52]);
    assert_eq!(
        u32::from_le_bytes(bytes[124..128].try_into().expect("checksum bytes")),
        super::crc32c(&bytes[..124])
    );
}

#[test]
fn database_header_uses_type_for_constructor_page_size_errors() {
    for page_size in [0, 2048, 4097, 32_768] {
        let error = DatabaseHeader::new(page_size, [0; 16]).expect_err("invalid page size");
        assert_eq!(error.code(), crate::ErrorCode::Type);
    }
}

#[test]
fn database_header_decode_rejects_invalid_disk_fields_as_corruption() {
    let valid = DatabaseHeader::new(4096, [3; 16]).expect("header").encode();

    for mutate in [
        |bytes: &mut [u8; 128]| bytes[0] ^= 1,
        |bytes: &mut [u8; 128]| bytes[16..18].copy_from_slice(&2u16.to_le_bytes()),
        |bytes: &mut [u8; 128]| bytes[20..24].copy_from_slice(&2048u32.to_le_bytes()),
        |bytes: &mut [u8; 128]| bytes[64] = 1,
        |bytes: &mut [u8; 128]| bytes[72] = 1,
    ] {
        let mut bytes = valid;
        mutate(&mut bytes);
        refresh_database_crc(&mut bytes);
        let error = DatabaseHeader::decode(&bytes).expect_err("invalid disk field");
        assert_eq!(error.code(), crate::ErrorCode::Corruption);
    }
}

#[test]
fn database_header_decode_requires_exact_layout_size() {
    let bytes = DatabaseHeader::new(4096, [4; 16]).expect("header").encode();

    assert_eq!(
        DatabaseHeader::decode(&bytes[..127])
            .expect_err("truncated header")
            .code(),
        crate::ErrorCode::Corruption
    );

    let mut extended = bytes.to_vec();
    extended.push(0);
    assert_eq!(
        DatabaseHeader::decode(&extended)
            .expect_err("extended header")
            .code(),
        crate::ErrorCode::Corruption
    );
}

fn refresh_database_crc(bytes: &mut [u8; 128]) {
    let checksum = super::crc32c(&bytes[..124]).to_le_bytes();
    bytes[124..128].copy_from_slice(&checksum);
}
