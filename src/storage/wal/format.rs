use super::super::{
    Lsn, PageId,
    checksum::{Crc32c, crc32c},
};
use crate::{DbError, ErrorCode, Result};

pub(super) const WAL_HEADER_SLOT_LEN: usize = 4096;
pub(super) const RECORD_HEADER_LEN: usize = 64;
pub(super) const COMMIT_PAYLOAD_LEN: usize = 64;

const WAL_MAGIC: &[u8; 16] = b"ORA-SQLLITE-WAL\0";
const RECORD_MAGIC: &[u8; 8] = b"ORAWREC\0";
const WAL_FORMAT_MAJOR: u16 = 1;
const WAL_FORMAT_MINOR: u16 = 0;
const WAL_HEADER_CRC_OFFSET: usize = WAL_HEADER_SLOT_LEN - 4;
const RECORD_HEADER_CRC_OFFSET: usize = 52;
const KNOWN_FEATURE_BITS: u64 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WalHeader {
    pub(super) page_size: u32,
    pub(super) database_uuid: [u8; 16],
    pub(super) wal_generation: u64,
    pub(super) base_checkpoint_lsn: Lsn,
    pub(super) salt: [u8; 16],
    pub(super) header_sequence: u64,
}

impl WalHeader {
    pub(super) fn encode(&self) -> Result<[u8; WAL_HEADER_SLOT_LEN]> {
        self.validate()?;
        let mut bytes = [0; WAL_HEADER_SLOT_LEN];
        bytes[0..16].copy_from_slice(WAL_MAGIC);
        bytes[16..18].copy_from_slice(&WAL_FORMAT_MAJOR.to_le_bytes());
        bytes[18..20].copy_from_slice(&WAL_FORMAT_MINOR.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.page_size.to_le_bytes());
        bytes[24..40].copy_from_slice(&self.database_uuid);
        bytes[40..48].copy_from_slice(&self.wal_generation.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.base_checkpoint_lsn.0.to_le_bytes());
        bytes[56..72].copy_from_slice(&self.salt);
        bytes[72..80].copy_from_slice(&self.header_sequence.to_le_bytes());
        let checksum = crc32c(&bytes[..WAL_HEADER_CRC_OFFSET]);
        bytes[WAL_HEADER_CRC_OFFSET..].copy_from_slice(&checksum.to_le_bytes());
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8], expected_uuid: [u8; 16]) -> Result<Self> {
        let bytes: &[u8; WAL_HEADER_SLOT_LEN] = bytes
            .try_into()
            .map_err(|_| corruption("invalid WAL header slot length"))?;
        if &bytes[0..16] != WAL_MAGIC {
            return Err(corruption("invalid WAL header magic"));
        }
        if read_u16(bytes, 16) != WAL_FORMAT_MAJOR || read_u16(bytes, 18) != WAL_FORMAT_MINOR {
            return Err(corruption("unsupported WAL header format"));
        }
        if bytes[80..WAL_HEADER_CRC_OFFSET]
            .iter()
            .any(|&byte| byte != 0)
        {
            return Err(corruption("nonzero WAL header reserved bytes"));
        }
        if read_u32(bytes, WAL_HEADER_CRC_OFFSET) != crc32c(&bytes[..WAL_HEADER_CRC_OFFSET]) {
            return Err(corruption("invalid WAL header checksum"));
        }

        let database_uuid = read_array(bytes, 24);
        if database_uuid != expected_uuid {
            return Err(corruption("WAL database UUID mismatch"));
        }
        let header = Self {
            page_size: read_u32(bytes, 20),
            database_uuid,
            wal_generation: read_u64(bytes, 40),
            base_checkpoint_lsn: Lsn(read_u64(bytes, 48)),
            salt: read_array(bytes, 56),
            header_sequence: read_u64(bytes, 72),
        };
        header.validate()?;
        Ok(header)
    }

    fn validate(&self) -> Result<()> {
        validate_page_size(self.page_size)?;
        if self.wal_generation == 0 {
            return Err(corruption("WAL generation must be nonzero"));
        }
        if self.salt.iter().all(|&byte| byte == 0) {
            return Err(corruption("WAL salt must be nonzero"));
        }
        if self.header_sequence == 0 {
            return Err(corruption("WAL header sequence must be nonzero"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordType {
    PageFrame,
    Commit,
}

impl RecordType {
    const fn byte(self) -> u8 {
        match self {
            Self::PageFrame => 1,
            Self::Commit => 2,
        }
    }

    fn decode(byte: u8) -> Result<Self> {
        match byte {
            1 => Ok(Self::PageFrame),
            2 => Ok(Self::Commit),
            _ => Err(corruption("unknown WAL record type")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecordHeader {
    pub(super) record_type: RecordType,
    pub(super) payload_len: u32,
    pub(super) transaction_lsn: Lsn,
    pub(super) ordinal: u32,
    pub(super) page_id: PageId,
    pub(super) generation: u64,
    pub(super) payload_crc32c: u32,
    pub(super) wal_generation: u64,
}

impl RecordHeader {
    pub(super) fn page_frame(
        transaction_lsn: Lsn,
        ordinal: u32,
        page_id: PageId,
        generation: u64,
        wal_generation: u64,
        payload: &[u8],
    ) -> Result<Self> {
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| corruption("WAL record payload is too large"))?;
        let mut header = Self {
            record_type: RecordType::PageFrame,
            payload_len,
            transaction_lsn,
            ordinal,
            page_id,
            generation,
            payload_crc32c: 0,
            wal_generation,
        };
        header.validate(payload_len)?;
        header.payload_crc32c = crc32c(payload);
        Ok(header)
    }

    pub(super) fn commit(
        transaction_lsn: Lsn,
        ordinal: u32,
        wal_generation: u64,
        payload: &[u8],
    ) -> Result<Self> {
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| corruption("WAL record payload is too large"))?;
        let mut header = Self {
            record_type: RecordType::Commit,
            payload_len,
            transaction_lsn,
            ordinal,
            page_id: PageId(0),
            generation: 0,
            payload_crc32c: 0,
            wal_generation,
        };
        header.validate(4096)?;
        header.payload_crc32c = crc32c(payload);
        Ok(header)
    }

    pub(super) fn encode(&self, page_size: u32) -> Result<[u8; RECORD_HEADER_LEN]> {
        self.validate(page_size)?;
        let mut bytes = [0; RECORD_HEADER_LEN];
        bytes[0..8].copy_from_slice(RECORD_MAGIC);
        bytes[8] = self.record_type.byte();
        bytes[10..12].copy_from_slice(&(RECORD_HEADER_LEN as u16).to_le_bytes());
        bytes[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.transaction_lsn.0.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.ordinal.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.page_id.0.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.generation.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.payload_crc32c.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.wal_generation.to_le_bytes());
        let checksum = record_header_checksum(&bytes);
        bytes[RECORD_HEADER_CRC_OFFSET..RECORD_HEADER_CRC_OFFSET + 4]
            .copy_from_slice(&checksum.to_le_bytes());
        Ok(bytes)
    }

    pub(super) fn decode(
        bytes: &[u8],
        page_size: u32,
        expected_wal_generation: u64,
    ) -> Result<Self> {
        let bytes: &[u8; RECORD_HEADER_LEN] = bytes
            .try_into()
            .map_err(|_| corruption("invalid WAL record header length"))?;
        if &bytes[0..8] != RECORD_MAGIC {
            return Err(corruption("invalid WAL record magic"));
        }
        if bytes[9] != 0 {
            return Err(corruption("nonzero WAL record flags"));
        }
        if read_u16(bytes, 10) != RECORD_HEADER_LEN as u16 {
            return Err(corruption("invalid WAL record header length field"));
        }
        if read_u32(bytes, 28) != 0 {
            return Err(corruption("nonzero WAL record reserved field"));
        }
        if read_u32(bytes, RECORD_HEADER_CRC_OFFSET) != record_header_checksum(bytes) {
            return Err(corruption("invalid WAL record header checksum"));
        }
        let header = Self {
            record_type: RecordType::decode(bytes[8])?,
            payload_len: read_u32(bytes, 12),
            transaction_lsn: Lsn(read_u64(bytes, 16)),
            ordinal: read_u32(bytes, 24),
            page_id: PageId(read_u64(bytes, 32)),
            generation: read_u64(bytes, 40),
            payload_crc32c: read_u32(bytes, 48),
            wal_generation: read_u64(bytes, 56),
        };
        header.validate(page_size)?;
        if header.wal_generation != expected_wal_generation {
            return Err(corruption("WAL record generation mismatch"));
        }
        Ok(header)
    }

    pub(super) fn validate_payload(&self, payload: &[u8], padding: &[u8]) -> Result<()> {
        let payload_len = usize::try_from(self.payload_len)
            .map_err(|_| corruption("WAL record payload length does not fit memory"))?;
        if payload.len() != payload_len {
            return Err(corruption("WAL record payload length mismatch"));
        }
        if padding.len() != zero_padding_len(self.payload_len)
            || padding.iter().any(|&byte| byte != 0)
        {
            return Err(corruption("invalid WAL record padding"));
        }
        if crc32c(payload) != self.payload_crc32c {
            return Err(corruption("invalid WAL record payload checksum"));
        }
        Ok(())
    }

    fn validate(&self, page_size: u32) -> Result<()> {
        validate_page_size(page_size)?;
        if self.transaction_lsn.0 == 0 {
            return Err(corruption("WAL record LSN must be nonzero"));
        }
        if self.wal_generation == 0 {
            return Err(corruption("WAL record generation must be nonzero"));
        }
        match self.record_type {
            RecordType::PageFrame => {
                if self.payload_len != page_size {
                    return Err(corruption("page-frame payload length mismatch"));
                }
                if self.page_id.0 == 0 {
                    return Err(corruption("page-frame record cannot target page zero"));
                }
                if self.generation == 0 {
                    return Err(corruption("page-frame generation must be nonzero"));
                }
            }
            RecordType::Commit => {
                if self.payload_len != COMMIT_PAYLOAD_LEN as u32 {
                    return Err(corruption("commit payload length mismatch"));
                }
                if self.page_id.0 != 0 || self.generation != 0 {
                    return Err(corruption("commit record has page identity"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommitState {
    pub(crate) page_count: u64,
    pub(crate) catalog_root: PageId,
    pub(crate) freelist_root: PageId,
    pub(crate) feature_bits: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CommitPayload {
    pub(super) previous_lsn: Lsn,
    pub(super) state: CommitState,
    pub(super) frame_count: u32,
    pub(super) transaction_crc32c: u32,
}

impl CommitPayload {
    pub(super) fn encode(&self) -> Result<[u8; COMMIT_PAYLOAD_LEN]> {
        self.validate()?;
        let mut bytes = [0; COMMIT_PAYLOAD_LEN];
        bytes[0..8].copy_from_slice(&self.previous_lsn.0.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.state.page_count.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.state.catalog_root.0.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.state.freelist_root.0.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.frame_count.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.transaction_crc32c.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.state.feature_bits.to_le_bytes());
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        let bytes: &[u8; COMMIT_PAYLOAD_LEN] = bytes
            .try_into()
            .map_err(|_| corruption("invalid WAL commit payload length"))?;
        if bytes[48..64].iter().any(|&byte| byte != 0) {
            return Err(corruption("nonzero WAL commit reserved bytes"));
        }
        let payload = Self {
            previous_lsn: Lsn(read_u64(bytes, 0)),
            state: CommitState {
                page_count: read_u64(bytes, 8),
                catalog_root: PageId(read_u64(bytes, 16)),
                freelist_root: PageId(read_u64(bytes, 24)),
                feature_bits: read_u64(bytes, 40),
            },
            frame_count: read_u32(bytes, 32),
            transaction_crc32c: read_u32(bytes, 36),
        };
        payload.validate()?;
        Ok(payload)
    }

    fn validate(&self) -> Result<()> {
        if self.state.page_count == 0 {
            return Err(corruption("commit page count must be nonzero"));
        }
        validate_root("catalog", self.state.catalog_root, self.state.page_count)?;
        validate_root("freelist", self.state.freelist_root, self.state.page_count)?;
        if self.state.feature_bits & !KNOWN_FEATURE_BITS != 0 {
            return Err(corruption("commit payload has unknown feature bits"));
        }
        Ok(())
    }
}

pub(super) const fn zero_padding_len(payload_len: u32) -> usize {
    ((8 - (payload_len & 7)) & 7) as usize
}

pub(super) fn record_len(payload_len: u32) -> Result<u64> {
    let without_padding = (RECORD_HEADER_LEN as u64)
        .checked_add(u64::from(payload_len))
        .ok_or_else(|| corruption("WAL record length overflow"))?;
    without_padding
        .checked_add(zero_padding_len(payload_len) as u64)
        .ok_or_else(|| corruption("WAL record length overflow"))
}

pub(super) fn record_end(record_start: u64, payload_len: u32) -> Result<u64> {
    record_start
        .checked_add(record_len(payload_len)?)
        .ok_or_else(|| corruption("WAL record end overflow"))
}

fn record_header_checksum(bytes: &[u8; RECORD_HEADER_LEN]) -> u32 {
    let mut checksum = Crc32c::new();
    checksum.update(&bytes[..RECORD_HEADER_CRC_OFFSET]);
    checksum.update(&[0; 4]);
    checksum.update(&bytes[RECORD_HEADER_CRC_OFFSET + 4..]);
    checksum.finish()
}

fn validate_page_size(page_size: u32) -> Result<()> {
    if matches!(page_size, 4096 | 8192 | 16384) {
        Ok(())
    } else {
        Err(corruption("invalid WAL page size"))
    }
}

fn validate_root(name: &'static str, root: PageId, page_count: u64) -> Result<()> {
    if root.0 != 0 && root.0 >= page_count {
        Err(DbError::new(
            ErrorCode::Corruption,
            format!("commit {name} root is outside the database"),
        ))
    } else {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(read_array(bytes, offset))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(read_array(bytes, offset))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(read_array(bytes, offset))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    bytes[offset..offset + N]
        .try_into()
        .expect("fixed WAL field is in a validated fixed-size buffer")
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ErrorCode,
        storage::{Lsn, PageId},
    };

    const UUID: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    const SALT: [u8; 16] = [
        0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e,
        0x2f,
    ];

    #[test]
    fn wal_header_has_the_exact_little_endian_slot_layout() {
        let header = WalHeader {
            page_size: 4096,
            database_uuid: UUID,
            wal_generation: 0x0102_0304_0506_0708,
            base_checkpoint_lsn: Lsn(0x1112_1314_1516_1718),
            salt: SALT,
            header_sequence: 0x2122_2324_2526_2728,
        };

        let bytes = header.encode().expect("encode WAL header");

        assert_eq!(bytes.len(), 4096);
        assert_eq!(&bytes[0..16], b"ORA-SQLLITE-WAL\0");
        assert_eq!(&bytes[16..18], &[0x01, 0x00]);
        assert_eq!(&bytes[18..20], &[0x00, 0x00]);
        assert_eq!(&bytes[20..24], &[0x00, 0x10, 0x00, 0x00]);
        assert_eq!(&bytes[24..40], &UUID);
        assert_eq!(
            &bytes[40..48],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        assert_eq!(
            &bytes[48..56],
            &[0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]
        );
        assert_eq!(&bytes[56..72], &SALT);
        assert_eq!(
            &bytes[72..80],
            &[0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21]
        );
        assert!(bytes[80..4092].iter().all(|&byte| byte == 0));
        assert_eq!(&bytes[4092..4096], &[0xd6, 0x4c, 0xa4, 0x66]);

        assert_eq!(
            WalHeader::decode(&bytes, UUID).expect("decode WAL header"),
            header
        );
    }

    #[test]
    fn wal_header_decode_rejects_wrong_length_reserved_bytes_crc_and_uuid() {
        let header = valid_wal_header();
        let bytes = header.encode().expect("encode WAL header");
        for malformed in [&bytes[..4095], &[0_u8; 4097][..]] {
            assert_corruption(WalHeader::decode(malformed, UUID));
        }

        let mut reserved = bytes;
        reserved[80] = 1;
        assert_corruption(WalHeader::decode(&reserved, UUID));

        let mut bad_crc = header.encode().expect("encode WAL header");
        bad_crc[40] ^= 1;
        assert_corruption(WalHeader::decode(&bad_crc, UUID));

        assert_corruption(WalHeader::decode(
            &header.encode().expect("encode WAL header"),
            [0xff; 16],
        ));
    }

    #[test]
    fn record_headers_have_exact_page_frame_and_commit_layouts() {
        let page_payload = vec![0xa5; 4096];
        let page = RecordHeader::page_frame(
            Lsn(0x0102_0304_0506_0708),
            0x1112_1314,
            PageId(0x2122_2324_2526_2728),
            0x3132_3334_3536_3738,
            0x4142_4344_4546_4748,
            &page_payload,
        )
        .expect("page-frame header");
        let bytes = page.encode(4096).expect("encode page-frame header");

        assert_eq!(&bytes[0..8], b"ORAWREC\0");
        assert_eq!(bytes[8], 1);
        assert_eq!(bytes[9], 0);
        assert_eq!(&bytes[10..12], &[0x40, 0x00]);
        assert_eq!(&bytes[12..16], &[0x00, 0x10, 0x00, 0x00]);
        assert_eq!(
            &bytes[16..24],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        assert_eq!(&bytes[24..28], &[0x14, 0x13, 0x12, 0x11]);
        assert_eq!(&bytes[28..32], &[0; 4]);
        assert_eq!(
            &bytes[32..40],
            &[0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21]
        );
        assert_eq!(
            &bytes[40..48],
            &[0x38, 0x37, 0x36, 0x35, 0x34, 0x33, 0x32, 0x31]
        );
        assert_eq!(&bytes[48..52], &[0xd3, 0x33, 0xc9, 0x8a]);
        assert_eq!(&bytes[52..56], &[0x88, 0xfc, 0xc8, 0x3d]);
        let mut header_checksum_input = bytes;
        header_checksum_input[52..56].fill(0);
        assert_eq!(
            crate::storage::checksum::crc32c(&header_checksum_input),
            0x3dc8_fc88
        );
        assert_eq!(
            &bytes[56..64],
            &[0x48, 0x47, 0x46, 0x45, 0x44, 0x43, 0x42, 0x41]
        );
        let decoded = RecordHeader::decode(&bytes, 4096, page.wal_generation)
            .expect("decode page-frame header");
        assert_eq!(decoded, page);
        decoded
            .validate_payload(&page_payload, &[])
            .expect("validate page payload and padding");

        let commit_payload = valid_commit_payload().encode().expect("commit payload");
        let commit = RecordHeader::commit(Lsn(9), 3, 7, &commit_payload).expect("commit header");
        let commit_bytes = commit.encode(4096).expect("encode commit header");
        assert_eq!(commit_bytes[8], 2);
        assert_eq!(&commit_bytes[12..16], &[64, 0, 0, 0]);
        assert_eq!(&commit_bytes[32..48], &[0; 16]);
        RecordHeader::decode(&commit_bytes, 4096, 7)
            .expect("decode commit header")
            .validate_payload(&commit_payload, &[])
            .expect("validate commit payload and padding");
    }

    #[test]
    fn record_decode_rejects_unknown_type_fixed_fields_and_type_invariants() {
        let payload = vec![0x5a; 4096];
        let header = RecordHeader::page_frame(Lsn(1), 0, PageId(4), 2, 3, &payload)
            .expect("page-frame header");
        let bytes = header.encode(4096).expect("encode record header");

        for malformed in [&bytes[..63], &[0_u8; 65][..]] {
            assert_corruption(RecordHeader::decode(malformed, 4096, 3));
        }

        for (offset, value) in [(8, 3), (9, 1), (10, 63), (28, 1)] {
            let mut malformed = bytes;
            malformed[offset] = value;
            restamp_record_header(&mut malformed);
            assert_corruption(RecordHeader::decode(&malformed, 4096, 3));
        }

        for range in [12..16, 32..40, 40..48] {
            let mut malformed = bytes;
            malformed[range].fill(0);
            restamp_record_header(&mut malformed);
            assert_corruption(RecordHeader::decode(&malformed, 4096, 3));
        }

        let commit_payload = valid_commit_payload().encode().expect("commit payload");
        let commit = RecordHeader::commit(Lsn(1), 0, 3, &commit_payload)
            .expect("commit header")
            .encode(4096)
            .expect("encode commit header");
        for range in [32..40, 40..48] {
            let mut malformed = commit;
            malformed[range].copy_from_slice(&1_u64.to_le_bytes());
            restamp_record_header(&mut malformed);
            assert_corruption(RecordHeader::decode(&malformed, 4096, 3));
        }
    }

    #[test]
    fn record_payload_validation_rejects_length_crc_and_padding_corruption() {
        let payload = vec![0x5a; 4096];
        let header = RecordHeader::page_frame(Lsn(1), 0, PageId(4), 2, 3, &payload)
            .expect("page-frame header");

        assert_corruption(header.validate_payload(&payload[..4095], &[]));
        let mut corrupted = payload.clone();
        corrupted[200] ^= 1;
        assert_corruption(header.validate_payload(&corrupted, &[]));
        assert_corruption(header.validate_payload(&payload, &[0]));
        assert_corruption(header.validate_payload(&payload, &[1]));
    }

    #[test]
    fn commit_payload_has_exact_layout_and_rejects_fixed_field_corruption() {
        let payload = valid_commit_payload();
        let bytes = payload.encode().expect("encode commit payload");

        assert_eq!(&bytes[0..8], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&bytes[8..16], &[0x40, 0x30, 0x20, 0x10, 0, 0, 0, 0]);
        assert_eq!(&bytes[16..24], &[7, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[24..32], &[9, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[32..36], &[3, 0, 0, 0]);
        assert_eq!(&bytes[36..40], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(&bytes[40..48], &[0; 8]);
        assert_eq!(&bytes[48..64], &[0; 16]);
        assert_eq!(
            CommitPayload::decode(&bytes).expect("decode commit payload"),
            payload
        );

        assert_corruption(CommitPayload::decode(&bytes[..63]));
        let mut reserved = bytes;
        reserved[48] = 1;
        assert_corruption(CommitPayload::decode(&reserved));
    }

    #[test]
    fn record_length_alignment_and_end_arithmetic_are_checked() {
        assert_eq!(zero_padding_len(0), 0);
        assert_eq!(zero_padding_len(1), 7);
        assert_eq!(zero_padding_len(7), 1);
        assert_eq!(zero_padding_len(8), 0);
        assert_eq!(record_len(0).expect("empty payload record"), 64);
        assert_eq!(record_len(4097).expect("unaligned payload record"), 4168);
        assert_eq!(record_end(8192, 4097).expect("record end"), 12_360);
        assert_corruption(record_end(u64::MAX - 63, 1));
    }

    fn valid_wal_header() -> WalHeader {
        WalHeader {
            page_size: 4096,
            database_uuid: UUID,
            wal_generation: 3,
            base_checkpoint_lsn: Lsn(2),
            salt: SALT,
            header_sequence: 4,
        }
    }

    fn valid_commit_payload() -> CommitPayload {
        CommitPayload {
            previous_lsn: Lsn(0x0102_0304_0506_0708),
            state: CommitState {
                page_count: 0x1020_3040,
                catalog_root: PageId(7),
                freelist_root: PageId(9),
                feature_bits: 0,
            },
            frame_count: 3,
            transaction_crc32c: 0xa1b2_c3d4,
        }
    }

    fn restamp_record_header(bytes: &mut [u8; 64]) {
        bytes[52..56].fill(0);
        let checksum = crate::storage::checksum::crc32c(bytes);
        bytes[52..56].copy_from_slice(&checksum.to_le_bytes());
    }

    fn assert_corruption<T: std::fmt::Debug>(result: crate::Result<T>) {
        assert_eq!(
            result.expect_err("malformed WAL value must fail").code(),
            ErrorCode::Corruption
        );
    }
}
