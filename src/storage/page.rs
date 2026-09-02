use super::{
    PageId, PageRef,
    checksum::{stamp_page_checksum, verify_page_checksum},
    codec::{Decoder, Encoder},
};
use crate::{DbError, ErrorCode, Result};

const PAGE_HEADER_LEN: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum PageType {
    Catalog = 1,
    TableInternal = 2,
    TableLeaf = 3,
    IndexInternal = 4,
    IndexLeaf = 5,
    Overflow = 6,
    Freelist = 7,
}

impl PageType {
    fn decode(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::Catalog),
            2 => Ok(Self::TableInternal),
            3 => Ok(Self::TableLeaf),
            4 => Ok(Self::IndexInternal),
            5 => Ok(Self::IndexLeaf),
            6 => Ok(Self::Overflow),
            7 => Ok(Self::Freelist),
            _ => Err(corruption("unknown page type")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PageHeader {
    page_type: PageType,
    flags: u8,
    page_id: PageId,
    generation: u64,
    cell_count: u16,
    slot_end: u16,
    payload_start: u16,
    right_sibling: Option<PageId>,
    checksum: u32,
    page_size: u32,
}

impl PageHeader {
    pub(super) fn new(
        page_type: PageType,
        page_id: PageId,
        generation: u64,
        page_size: u32,
    ) -> Result<Self> {
        if !is_valid_page_size(page_size) {
            return Err(corruption("invalid page size"));
        }

        Ok(Self {
            page_type,
            flags: 0,
            page_id,
            generation,
            cell_count: 0,
            slot_end: PAGE_HEADER_LEN as u16,
            payload_start: page_size as u16,
            right_sibling: None,
            checksum: 0,
            page_size,
        })
    }

    pub(super) const fn page_type(&self) -> PageType {
        self.page_type
    }

    pub(super) const fn flags(&self) -> u8 {
        self.flags
    }

    pub(super) const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) const fn cell_count(&self) -> u16 {
        self.cell_count
    }

    pub(super) const fn slot_end(&self) -> u16 {
        self.slot_end
    }

    pub(super) const fn payload_start(&self) -> u16 {
        self.payload_start
    }

    pub(super) const fn right_sibling(&self) -> Option<PageId> {
        self.right_sibling
    }

    pub(super) const fn checksum(&self) -> u32 {
        self.checksum
    }

    pub(super) fn set_checksum(&mut self, checksum: u32) {
        self.checksum = checksum;
    }

    pub(super) fn set_generation(&mut self, generation: u64) -> Result<()> {
        if generation == 0 {
            return Err(corruption("page generation must be nonzero"));
        }
        self.generation = generation;
        Ok(())
    }

    pub(super) fn set_right_sibling(&mut self, right_sibling: Option<PageId>) -> Result<()> {
        if right_sibling.is_some_and(|page_id| page_id.0 == 0) {
            return Err(corruption("right sibling page id must be nonzero"));
        }
        self.right_sibling = right_sibling;
        Ok(())
    }

    pub(super) fn set_layout(
        &mut self,
        cell_count: u16,
        slot_end: u16,
        payload_start: u16,
    ) -> Result<()> {
        validate_layout(slot_end, payload_start, self.page_size)?;
        self.cell_count = cell_count;
        self.slot_end = slot_end;
        self.payload_start = payload_start;
        Ok(())
    }

    pub(super) fn encode(&self) -> [u8; PAGE_HEADER_LEN] {
        let mut encoder = Encoder::new();
        encoder.put_u8(self.page_type as u8);
        encoder.put_u8(self.flags);
        encoder.put_u16(0);
        encoder.put_u64(self.page_id.0);
        encoder.put_u64(self.generation);
        encoder.put_u16(self.cell_count);
        encoder.put_u16(self.slot_end);
        encoder.put_u16(self.payload_start);
        encoder.put_u16(0);
        encoder.put_u64(self.right_sibling.map_or(0, |page_id| page_id.0));
        encoder.put_u32(self.checksum);
        encoder.put_bytes(&[0; 8]);
        encoder
            .into_bytes()
            .try_into()
            .expect("page header encoding has a fixed length")
    }

    pub(super) fn decode(bytes: &[u8], page_size: u32) -> Result<Self> {
        if bytes.len() != PAGE_HEADER_LEN {
            return Err(corruption("page header must be exactly 48 bytes"));
        }
        if !is_valid_page_size(page_size) {
            return Err(corruption("invalid page size"));
        }

        let mut decoder = Decoder::new(bytes);
        let page_type = PageType::decode(decoder.get_u8()?)?;
        let flags = decoder.get_u8()?;
        let reserved_1 = decoder.get_u16()?;
        let page_id = PageId(decoder.get_u64()?);
        let generation = decoder.get_u64()?;
        let cell_count = decoder.get_u16()?;
        let slot_end = decoder.get_u16()?;
        let payload_start = decoder.get_u16()?;
        let reserved_2 = decoder.get_u16()?;
        let right_sibling = match decoder.get_u64()? {
            0 => None,
            page_id => Some(PageId(page_id)),
        };
        let checksum = decoder.get_u32()?;
        let reserved_3 = decoder.get_bytes(8)?;

        if reserved_1 != 0 || reserved_2 != 0 || reserved_3.iter().any(|&byte| byte != 0) {
            return Err(corruption("page header reserved bytes are nonzero"));
        }
        validate_layout(slot_end, payload_start, page_size)?;

        Ok(Self {
            page_type,
            flags,
            page_id,
            generation,
            cell_count,
            slot_end,
            payload_start,
            right_sibling,
            checksum,
            page_size,
        })
    }
}

fn validate_layout(slot_end: u16, payload_start: u16, page_size: u32) -> Result<()> {
    if usize::from(slot_end) < PAGE_HEADER_LEN {
        return Err(corruption("page slot directory overlaps the header"));
    }
    if u32::from(payload_start) > page_size {
        return Err(corruption("page payload starts beyond the page boundary"));
    }
    if slot_end > payload_start {
        return Err(corruption("page free-space boundaries cross"));
    }
    Ok(())
}

const fn is_valid_page_size(page_size: u32) -> bool {
    matches!(page_size, 4096 | 8192 | 16384)
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[derive(Debug)]
pub(super) struct PageFrame {
    bytes: Vec<u8>,
    header: PageHeader,
}

impl PageFrame {
    pub(super) fn new_zeroed(
        page_type: PageType,
        page_ref: PageRef,
        page_size: u32,
    ) -> Result<Self> {
        let header = PageHeader::new(
            page_type,
            page_ref.page_id(),
            page_ref.generation(),
            page_size,
        )?;
        Ok(Self {
            bytes: vec![0; page_size as usize],
            header,
        })
    }

    pub(super) fn decode(
        bytes: Vec<u8>,
        expected_page_id: PageId,
        expected_page_type: Option<PageType>,
        expected_generation: Option<u64>,
    ) -> Result<Self> {
        verify_page_checksum(&bytes)?;
        let page_size = u32::try_from(bytes.len())
            .map_err(|_| corruption("page frame exceeds supported size"))?;
        let header = PageHeader::decode(&bytes[..PAGE_HEADER_LEN], page_size)?;

        if header.page_id() != expected_page_id {
            return Err(corruption(
                "page frame id does not match the requested page",
            ));
        }
        if expected_page_type.is_some_and(|page_type| header.page_type() != page_type) {
            return Err(corruption(
                "page frame type does not match the requested page",
            ));
        }
        if expected_generation.is_some_and(|generation| header.generation() != generation) {
            return Err(corruption(
                "page frame generation does not match the requested page",
            ));
        }

        Ok(Self { bytes, header })
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    pub(super) const fn header(&self) -> &PageHeader {
        &self.header
    }

    pub(super) fn prepare_for_write(&mut self) -> Result<()> {
        self.header.set_checksum(0);
        self.bytes[..PAGE_HEADER_LEN].copy_from_slice(&self.header.encode());
        let page_size = u32::try_from(self.bytes.len())
            .map_err(|_| corruption("page frame exceeds supported size"))?;
        self.header = PageHeader::decode(&self.bytes[..PAGE_HEADER_LEN], page_size)?;
        let checksum = stamp_page_checksum(&mut self.bytes)?;
        self.header.set_checksum(checksum);
        Ok(())
    }
}

#[test]
fn page_header_round_trips() {
    let header = PageHeader::new(PageType::TableLeaf, PageId(42), 3, 4096).expect("page header");
    let bytes = header.encode();
    assert_eq!(PageHeader::decode(&bytes, 4096).expect("decode"), header);
}

#[test]
fn page_header_rejects_invalid_free_space_bounds() {
    let error = PageHeader::new(PageType::TableLeaf, PageId(1), 1, 40).expect_err("small page");
    assert_eq!(error.code(), crate::ErrorCode::Corruption);
}

#[test]
fn page_header_uses_fixed_little_endian_offsets() {
    let mut header = PageHeader::new(
        PageType::TableLeaf,
        PageId(0x0102_0304_0506_0708),
        0x1112_1314_1516_1718,
        4096,
    )
    .expect("page header");
    header.flags = 0xa5;
    header.cell_count = 2;
    header.slot_end = 52;
    header.payload_start = 4000;
    header.right_sibling = Some(PageId(0x2122_2324_2526_2728));
    header.checksum = 0xa1b2_c3d4;

    let bytes = header.encode();

    assert_eq!(bytes.len(), 48);
    assert_eq!(&bytes[0..4], &[3, 0xa5, 0, 0]);
    assert_eq!(&bytes[4..12], &[8, 7, 6, 5, 4, 3, 2, 1]);
    assert_eq!(
        &bytes[12..20],
        &[0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]
    );
    assert_eq!(&bytes[20..28], &[2, 0, 52, 0, 0xa0, 0x0f, 0, 0]);
    assert_eq!(
        &bytes[28..36],
        &[0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21]
    );
    assert_eq!(&bytes[36..40], &[0xd4, 0xc3, 0xb2, 0xa1]);
    assert_eq!(&bytes[40..48], &[0; 8]);
    assert_eq!(PageHeader::decode(&bytes, 4096).expect("decode"), header);
}

#[test]
fn page_type_codes_are_stable_and_round_trip() {
    for (page_type, code) in [
        (PageType::Catalog, 1),
        (PageType::TableInternal, 2),
        (PageType::TableLeaf, 3),
        (PageType::IndexInternal, 4),
        (PageType::IndexLeaf, 5),
        (PageType::Overflow, 6),
        (PageType::Freelist, 7),
    ] {
        let header = PageHeader::new(page_type, PageId(1), 1, 4096).expect("page header");
        let bytes = header.encode();
        assert_eq!(bytes[0], code);
        assert_eq!(
            PageHeader::decode(&bytes, 4096).expect("decode").page_type,
            page_type
        );
    }
}

#[test]
fn page_header_decode_rejects_unknown_type_and_nonzero_reserved_bytes() {
    let valid = PageHeader::new(PageType::Catalog, PageId(1), 1, 4096)
        .expect("page header")
        .encode();

    let mut unknown_type = valid;
    unknown_type[0] = 0;
    assert_eq!(
        PageHeader::decode(&unknown_type, 4096)
            .expect_err("unknown page type")
            .code(),
        crate::ErrorCode::Corruption
    );

    for offset in [2, 3, 26, 27, 40, 41, 42, 43, 44, 45, 46, 47] {
        let mut bytes = valid;
        bytes[offset] = 1;
        assert_eq!(
            PageHeader::decode(&bytes, 4096)
                .expect_err("nonzero reserved byte")
                .code(),
            crate::ErrorCode::Corruption
        );
    }
}

#[test]
fn page_header_decode_rejects_invalid_free_space_boundaries() {
    let valid = PageHeader::new(PageType::TableLeaf, PageId(1), 1, 4096)
        .expect("page header")
        .encode();

    for mutate in [
        |bytes: &mut [u8; 48]| bytes[22..24].copy_from_slice(&47u16.to_le_bytes()),
        |bytes: &mut [u8; 48]| bytes[24..26].copy_from_slice(&4097u16.to_le_bytes()),
        |bytes: &mut [u8; 48]| {
            bytes[22..24].copy_from_slice(&100u16.to_le_bytes());
            bytes[24..26].copy_from_slice(&99u16.to_le_bytes());
        },
    ] {
        let mut bytes = valid;
        mutate(&mut bytes);
        assert_eq!(
            PageHeader::decode(&bytes, 4096)
                .expect_err("invalid free-space boundary")
                .code(),
            crate::ErrorCode::Corruption
        );
    }
}

#[test]
fn page_header_decode_requires_exact_layout_size() {
    let bytes = PageHeader::new(PageType::TableLeaf, PageId(1), 1, 4096)
        .expect("page header")
        .encode();

    assert_eq!(
        PageHeader::decode(&bytes[..47], 4096)
            .expect_err("truncated page header")
            .code(),
        crate::ErrorCode::Corruption
    );

    let mut extended = bytes.to_vec();
    extended.push(0);
    assert_eq!(
        PageHeader::decode(&extended, 4096)
            .expect_err("extended page header")
            .code(),
        crate::ErrorCode::Corruption
    );
}

#[test]
fn page_header_mutators_preserve_bounds() {
    let mut header = PageHeader::new(PageType::TableLeaf, PageId(1), 1, 4096).unwrap();
    header.set_layout(2, 56, 4000).expect("valid layout");
    assert_eq!(header.cell_count(), 2);
    assert_eq!(
        header.set_layout(2, 4001, 4000).unwrap_err().code(),
        crate::ErrorCode::Corruption
    );
}

#[test]
fn page_header_accessors_and_setters_round_trip() {
    let mut header = PageHeader::new(PageType::TableLeaf, PageId(7), 1, 8192).unwrap();
    header.set_generation(2).expect("generation");
    header
        .set_right_sibling(Some(PageId(8)))
        .expect("right sibling");
    header.set_checksum(0xa1b2_c3d4);

    assert_eq!(header.page_type(), PageType::TableLeaf);
    assert_eq!(header.flags(), 0);
    assert_eq!(header.page_id(), PageId(7));
    assert_eq!(header.generation(), 2);
    assert_eq!(header.cell_count(), 0);
    assert_eq!(header.slot_end(), 48);
    assert_eq!(header.payload_start(), 8192);
    assert_eq!(header.right_sibling(), Some(PageId(8)));
    assert_eq!(header.checksum(), 0xa1b2_c3d4);
}

#[test]
fn frame_decode_checks_crc_id_type_and_generation() {
    let expected = PageRef::new(PageId(5), 2).expect("page reference");
    let mut frame =
        PageFrame::new_zeroed(PageType::TableLeaf, expected, 4096).expect("zeroed page frame");
    frame.prepare_for_write().expect("prepare page frame");

    PageFrame::decode(
        frame.bytes().to_vec(),
        PageId(5),
        Some(PageType::TableLeaf),
        Some(2),
    )
    .expect("matching frame");
    assert_eq!(
        PageFrame::decode(frame.bytes().to_vec(), PageId(6), None, None)
            .expect_err("wrong page id")
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn frame_decode_rejects_checksum_corruption() {
    let mut bytes = encoded_frame(PageType::TableLeaf, 5, 2);
    bytes[100] ^= 1;

    assert_eq!(
        PageFrame::decode(bytes, PageId(5), Some(PageType::TableLeaf), Some(2))
            .expect_err("checksum corruption")
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn frame_decode_rejects_mismatched_page_type() {
    assert_eq!(
        PageFrame::decode(
            encoded_frame(PageType::TableLeaf, 5, 2),
            PageId(5),
            Some(PageType::Catalog),
            Some(2),
        )
        .expect_err("mismatched page type")
        .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn frame_decode_rejects_mismatched_generation() {
    assert_eq!(
        PageFrame::decode(
            encoded_frame(PageType::TableLeaf, 5, 2),
            PageId(5),
            Some(PageType::TableLeaf),
            Some(3),
        )
        .expect_err("mismatched generation")
        .code(),
        ErrorCode::Corruption
    );
}

fn encoded_frame(page_type: PageType, page_id: u64, generation: u64) -> Vec<u8> {
    let mut frame = PageFrame::new_zeroed(
        page_type,
        PageRef::new(PageId(page_id), generation).expect("page reference"),
        4096,
    )
    .expect("page frame");
    frame.prepare_for_write().expect("prepared page frame");
    frame.bytes().to_vec()
}
