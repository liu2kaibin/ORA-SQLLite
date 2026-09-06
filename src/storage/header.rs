use super::{
    Lsn, OpenMode, PageId,
    checksum::crc32c,
    codec::{Decoder, Encoder},
};
use crate::{DbError, ErrorCode, Result};

pub(super) const SUPERBLOCK_SLOT_LEN: usize = 4096;
pub(super) const SUPERBLOCK_AREA_LEN: u64 = 8192;

const SUPERBLOCK_PREFIX_LEN: usize = SUPERBLOCK_SLOT_LEN - 4;
const DATABASE_MAGIC: &[u8; 16] = b"ORA-SQLLITE-SB\0\0";
const FORMAT_MAJOR: u16 = 2;
const FORMAT_MINOR: u16 = 0;
const KNOWN_FEATURES: u64 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SuperblockSlot {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DatabaseHeader {
    major: u16,
    minor: u16,
    page_size: u32,
    uuid: [u8; 16],
    publish_sequence: u64,
    checkpoint_lsn: Lsn,
    page_count: u64,
    catalog_root: PageId,
    freelist_root: PageId,
    wal_generation: u64,
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
            publish_sequence: 1,
            checkpoint_lsn: Lsn(0),
            page_count: 1,
            catalog_root: PageId(0),
            freelist_root: PageId(0),
            wal_generation: 1,
            features: 0,
        })
    }

    pub(super) fn encode_slot(&self) -> [u8; SUPERBLOCK_SLOT_LEN] {
        let mut encoder = Encoder::new();
        encoder.put_bytes(DATABASE_MAGIC);
        encoder.put_u16(self.major);
        encoder.put_u16(self.minor);
        encoder.put_u32(self.page_size);
        encoder.put_bytes(&self.uuid);
        encoder.put_u64(self.publish_sequence);
        encoder.put_u64(self.checkpoint_lsn.0);
        encoder.put_u64(self.page_count);
        encoder.put_u64(self.catalog_root.0);
        encoder.put_u64(self.freelist_root.0);
        encoder.put_u64(self.wal_generation);
        encoder.put_u64(self.features);
        encoder.put_bytes(&[0; SUPERBLOCK_PREFIX_LEN - 96]);

        let mut bytes = encoder.into_bytes();
        debug_assert_eq!(bytes.len(), SUPERBLOCK_PREFIX_LEN);
        let checksum = crc32c(&bytes);
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes
            .try_into()
            .expect("superblock slot encoding has a fixed length")
    }

    pub(super) const fn page_size(&self) -> u32 {
        self.page_size
    }

    pub(super) const fn uuid(&self) -> [u8; 16] {
        self.uuid
    }

    pub(super) const fn publish_sequence(&self) -> u64 {
        self.publish_sequence
    }

    pub(super) const fn checkpoint_lsn(&self) -> Lsn {
        self.checkpoint_lsn
    }

    pub(super) const fn page_count(&self) -> u64 {
        self.page_count
    }

    pub(super) const fn wal_generation(&self) -> u64 {
        self.wal_generation
    }

    pub(super) const fn minor(&self) -> u16 {
        self.minor
    }

    pub(super) const fn features(&self) -> u64 {
        self.features
    }

    pub(super) const fn catalog_root(&self) -> PageId {
        self.catalog_root
    }

    pub(super) const fn freelist_root(&self) -> PageId {
        self.freelist_root
    }

    pub(super) fn set_catalog_root(&mut self, root: PageId) {
        self.catalog_root = root;
    }

    pub(super) fn set_freelist_root(&mut self, root: PageId) {
        self.freelist_root = root;
    }

    pub(super) fn with_checkpoint(
        self,
        checkpoint_lsn: Lsn,
        page_count: u64,
        catalog_root: PageId,
        freelist_root: PageId,
        wal_generation: u64,
    ) -> Result<Self> {
        if checkpoint_lsn.0 < self.checkpoint_lsn.0 {
            return Err(corruption("database checkpoint LSN cannot move backwards"));
        }
        let publish_sequence = self
            .publish_sequence
            .checked_add(1)
            .ok_or_else(|| corruption("database publish sequence overflow"))?;
        let candidate = Self {
            publish_sequence,
            checkpoint_lsn,
            page_count,
            catalog_root,
            freelist_root,
            wal_generation,
            ..self
        };
        candidate.validate_fields()?;
        Ok(candidate)
    }

    pub(super) fn validate_open(&self, mode: OpenMode) -> Result<()> {
        if self.minor > FORMAT_MINOR && mode == OpenMode::ReadWrite {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "database format minor version requires read-only access",
            ));
        }
        Ok(())
    }

    pub(super) fn decode_slot(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != SUPERBLOCK_SLOT_LEN {
            return Err(corruption(
                "database superblock slot must be exactly 4096 bytes",
            ));
        }

        let mut decoder = Decoder::new(bytes);
        let magic = decoder.get_bytes(16)?;
        let major = decoder.get_u16()?;
        let minor = decoder.get_u16()?;
        let page_size = decoder.get_u32()?;
        let mut uuid = [0; 16];
        uuid.copy_from_slice(decoder.get_bytes(16)?);
        let publish_sequence = decoder.get_u64()?;
        let checkpoint_lsn = Lsn(decoder.get_u64()?);
        let page_count = decoder.get_u64()?;
        let catalog_root = PageId(decoder.get_u64()?);
        let freelist_root = PageId(decoder.get_u64()?);
        let wal_generation = decoder.get_u64()?;
        let features = decoder.get_u64()?;
        let reserved = decoder.get_bytes(SUPERBLOCK_PREFIX_LEN - 96)?;
        let stored_checksum = decoder.get_u32()?;

        if magic != DATABASE_MAGIC {
            return Err(corruption("invalid database superblock magic"));
        }
        if stored_checksum != crc32c(&bytes[..SUPERBLOCK_PREFIX_LEN]) {
            return Err(corruption("invalid database superblock checksum"));
        }
        if major != FORMAT_MAJOR {
            return Err(corruption("unsupported database format major version"));
        }
        if reserved.iter().any(|&byte| byte != 0) {
            return Err(corruption("database superblock reserved bytes are nonzero"));
        }

        let header = Self {
            major,
            minor,
            page_size,
            uuid,
            publish_sequence,
            checkpoint_lsn,
            page_count,
            catalog_root,
            freelist_root,
            wal_generation,
            features,
        };
        header.validate_fields()?;
        Ok(header)
    }

    pub(super) fn select(slot_a: &[u8], slot_b: &[u8]) -> Result<(Self, SuperblockSlot)> {
        let decoded_a = Self::decode_slot(slot_a).ok();
        let decoded_b = Self::decode_slot(slot_b).ok();
        match (decoded_a, decoded_b) {
            (None, None) => Err(corruption("both database superblock slots are invalid")),
            (Some(header), None) => Ok((header, SuperblockSlot::A)),
            (None, Some(header)) => Ok((header, SuperblockSlot::B)),
            (Some(a), Some(b)) => {
                if a.uuid != b.uuid || a.page_size != b.page_size {
                    return Err(corruption(
                        "database superblock slots disagree on database identity",
                    ));
                }
                if a.publish_sequence == b.publish_sequence {
                    if a != b {
                        return Err(corruption(
                            "database superblock slots reuse a sequence for different state",
                        ));
                    }
                    return Ok((a, SuperblockSlot::A));
                }
                if a.publish_sequence > b.publish_sequence {
                    Ok((a, SuperblockSlot::A))
                } else {
                    Ok((b, SuperblockSlot::B))
                }
            }
        }
    }

    fn validate_fields(&self) -> Result<()> {
        if !is_valid_page_size(self.page_size) {
            return Err(corruption("invalid database page size"));
        }
        if self.publish_sequence == 0 {
            return Err(corruption("database publish sequence must be nonzero"));
        }
        if self.page_count == 0 {
            return Err(corruption("database page count must be nonzero"));
        }
        validate_root_page("catalog root", self.catalog_root, self.page_count)?;
        validate_root_page("freelist root", self.freelist_root, self.page_count)?;
        if self.wal_generation == 0 {
            return Err(corruption("database WAL generation must be nonzero"));
        }
        if self.features & !KNOWN_FEATURES != 0 {
            return Err(corruption("database header has unknown feature bits"));
        }
        Ok(())
    }
}

const fn is_valid_page_size(page_size: u32) -> bool {
    matches!(page_size, 4096 | 8192 | 16384)
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

fn validate_root_page(name: &'static str, root: PageId, page_count: u64) -> Result<()> {
    if root.0 != 0 && root.0 >= page_count {
        return Err(DbError::new(
            ErrorCode::Corruption,
            format!("database {name} is outside the database"),
        ));
    }
    Ok(())
}

#[test]
fn database_header_round_trips() {
    let header = DatabaseHeader::new(4096, [7; 16]).expect("valid header");
    let bytes = header.encode_slot();
    assert_eq!(&bytes[..16], b"ORA-SQLLITE-SB\0\0");
    assert_eq!(DatabaseHeader::decode_slot(&bytes).expect("decode"), header);
}

#[test]
fn database_header_rejects_corruption_and_page_size() {
    assert!(DatabaseHeader::new(2048, [1; 16]).is_err());
    let mut bytes = DatabaseHeader::new(8192, [1; 16])
        .expect("header")
        .encode_slot();
    bytes[32] ^= 1;
    assert_eq!(
        DatabaseHeader::decode_slot(&bytes)
            .expect_err("checksum")
            .code(),
        crate::ErrorCode::Corruption
    );
}

#[test]
fn superblock_slot_uses_fixed_format_2_offsets_and_crc() {
    let header = DatabaseHeader::new(4096, [0x5a; 16]).unwrap();
    let bytes = header.encode_slot();
    assert_eq!(bytes.len(), 4096);
    assert_eq!(&bytes[..16], b"ORA-SQLLITE-SB\0\0");
    assert_eq!(&bytes[16..20], &[2, 0, 0, 0]);
    assert_eq!(&bytes[20..24], &4096u32.to_le_bytes());
    assert_eq!(&bytes[24..40], &[0x5a; 16]);
    assert!(bytes[96..4092].iter().all(|byte| *byte == 0));
    assert_eq!(
        u32::from_le_bytes(bytes[4092..].try_into().unwrap()),
        crc32c(&bytes[..4092])
    );
}

#[test]
fn select_uses_the_highest_valid_publish_sequence() {
    let mut a = DatabaseHeader::new(4096, [1; 16]).unwrap();
    let mut b = a;
    a.publish_sequence = 7;
    b.publish_sequence = 8;
    let (selected, slot) = DatabaseHeader::select(&a.encode_slot(), &b.encode_slot()).unwrap();
    assert_eq!(selected.publish_sequence(), 8);
    assert_eq!(slot, SuperblockSlot::B);
}

#[test]
fn equal_sequences_with_different_state_are_corruption() {
    let mut a = DatabaseHeader::new(4096, [1; 16]).unwrap();
    let mut b = a;
    a.publish_sequence = 9;
    b.publish_sequence = 9;
    b.page_count = 2;
    b.catalog_root = PageId(1);
    assert_eq!(
        DatabaseHeader::select(&a.encode_slot(), &b.encode_slot())
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn select_accepts_one_valid_slot_and_rejects_two_invalid_slots() {
    let header = DatabaseHeader::new(4096, [2; 16]).unwrap();
    let valid = header.encode_slot();
    let mut corrupt = valid;
    corrupt[95] ^= 1;

    assert_eq!(
        DatabaseHeader::select(&corrupt, &valid).unwrap(),
        (header, SuperblockSlot::B)
    );
    assert_eq!(
        DatabaseHeader::select(&corrupt, &corrupt)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn select_rejects_mismatched_database_identity() {
    let a = DatabaseHeader::new(4096, [1; 16]).unwrap();
    let b = DatabaseHeader::new(4096, [2; 16]).unwrap();
    assert_eq!(
        DatabaseHeader::select(&a.encode_slot(), &b.encode_slot())
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
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
    let valid = DatabaseHeader::new(4096, [3; 16])
        .expect("header")
        .encode_slot();

    for mutate in [
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| bytes[0] ^= 1,
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| bytes[16..18].copy_from_slice(&1u16.to_le_bytes()),
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| {
            bytes[20..24].copy_from_slice(&2048u32.to_le_bytes())
        },
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| bytes[40..48].copy_from_slice(&0u64.to_le_bytes()),
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| bytes[56..64].copy_from_slice(&0u64.to_le_bytes()),
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| bytes[80..88].copy_from_slice(&0u64.to_le_bytes()),
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| bytes[88] = 1,
        |bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]| bytes[96] = 1,
    ] {
        let mut bytes = valid;
        mutate(&mut bytes);
        refresh_database_crc(&mut bytes);
        let error = DatabaseHeader::decode_slot(&bytes).expect_err("invalid disk field");
        assert_eq!(error.code(), crate::ErrorCode::Corruption);
    }
}

#[test]
fn database_header_decode_requires_exact_layout_size() {
    let bytes = DatabaseHeader::new(4096, [4; 16])
        .expect("header")
        .encode_slot();

    assert_eq!(
        DatabaseHeader::decode_slot(&bytes[..SUPERBLOCK_SLOT_LEN - 1])
            .expect_err("truncated header")
            .code(),
        crate::ErrorCode::Corruption
    );

    let mut extended = bytes.to_vec();
    extended.push(0);
    assert_eq!(
        DatabaseHeader::decode_slot(&extended)
            .expect_err("extended header")
            .code(),
        crate::ErrorCode::Corruption
    );
}

#[test]
fn future_minor_is_read_only_when_features_are_known() {
    let mut bytes = DatabaseHeader::new(4096, [9; 16]).unwrap().encode_slot();
    bytes[18..20].copy_from_slice(&1u16.to_le_bytes());
    refresh_database_crc(&mut bytes);
    let header = DatabaseHeader::decode_slot(&bytes).expect("structurally valid");
    header
        .validate_open(super::OpenMode::ReadOnly)
        .expect("safe read-only open");
    assert_eq!(
        header
            .validate_open(super::OpenMode::ReadWrite)
            .unwrap_err()
            .code(),
        crate::ErrorCode::Unsupported
    );
}

#[test]
fn with_checkpoint_builds_a_validated_new_publication() {
    let mut header = DatabaseHeader::new(8192, [0xa5; 16]).expect("header");
    header.set_catalog_root(PageId(7));
    header.set_freelist_root(PageId(8));
    header.page_count = 9;
    let candidate = header
        .with_checkpoint(Lsn(12), 10, PageId(7), PageId(8), 3)
        .expect("valid checkpoint state");

    assert_eq!(header.page_size(), 8192);
    assert_eq!(header.uuid(), [0xa5; 16]);
    assert_eq!(header.minor(), 0);
    assert_eq!(header.features(), 0);
    assert_eq!(header.catalog_root(), PageId(7));
    assert_eq!(header.freelist_root(), PageId(8));
    assert_eq!(header.publish_sequence(), 1);
    assert_eq!(header.checkpoint_lsn(), Lsn(0));
    assert_eq!(header.page_count(), 9);
    assert_eq!(header.wal_generation(), 1);

    assert_eq!(candidate.publish_sequence(), 2);
    assert_eq!(candidate.checkpoint_lsn(), Lsn(12));
    assert_eq!(candidate.page_count(), 10);
    assert_eq!(candidate.catalog_root(), PageId(7));
    assert_eq!(candidate.freelist_root(), PageId(8));
    assert_eq!(candidate.wal_generation(), 3);

    assert_eq!(
        header
            .with_checkpoint(Lsn(1), 1, PageId(1), PageId(0), 1)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
    header.publish_sequence = u64::MAX;
    assert_eq!(
        header
            .with_checkpoint(Lsn(1), 9, PageId(7), PageId(8), 1)
            .unwrap_err()
            .code(),
        ErrorCode::Corruption
    );
}

fn refresh_database_crc(bytes: &mut [u8; SUPERBLOCK_SLOT_LEN]) {
    let checksum = super::checksum::crc32c(&bytes[..SUPERBLOCK_SLOT_LEN - 4]).to_le_bytes();
    bytes[SUPERBLOCK_SLOT_LEN - 4..].copy_from_slice(&checksum);
}
