use super::page::PageFrame;
use crate::{DbError, ErrorCode, Result};

const PAGE_HEADER_LEN: usize = 48;
const SLOT_LEN: usize = 4;

#[derive(Debug, Clone, Copy)]
struct Slot {
    offset: usize,
    len: usize,
}

pub(super) struct SlottedPage<'a> {
    frame: &'a mut PageFrame,
    metadata_end: usize,
}

impl<'a> SlottedPage<'a> {
    pub(super) fn open(frame: &'a mut PageFrame, metadata_end: usize) -> Result<Self> {
        validate_metadata_end(frame, metadata_end)?;

        let initial_slot_end = usize::from(frame.header().slot_end());
        let page_len = frame.bytes().len();
        if frame.header().cell_count() == 0 && initial_slot_end == PAGE_HEADER_LEN {
            if usize::from(frame.header().payload_start()) != page_len {
                return Err(corruption(
                    "empty page layout has an unexpected payload start",
                ));
            }
            frame.set_layout(
                0,
                u16::try_from(metadata_end)
                    .map_err(|_| corruption("node metadata end exceeds disk layout"))?,
                u16::try_from(page_len)
                    .map_err(|_| corruption("page length exceeds disk layout"))?,
            )?;
        }

        let page = Self {
            frame,
            metadata_end,
        };
        page.validate()?;
        Ok(page)
    }

    pub(super) fn cell(&self, index: usize) -> Result<Vec<u8>> {
        let slots = self.slots()?;
        let slot = slots
            .get(index)
            .ok_or_else(|| type_error("cell index is outside the slot directory"))?;
        self.frame
            .bytes()
            .get(slot.offset..slot.end()?)
            .ok_or_else(|| corruption("cell payload lies beyond the page boundary"))
            .map(ToOwned::to_owned)
    }

    pub(super) fn cells(&self) -> Result<Vec<Vec<u8>>> {
        let slots = self.slots()?;
        slots
            .iter()
            .map(|slot| {
                self.frame
                    .bytes()
                    .get(slot.offset..slot.end()?)
                    .ok_or_else(|| corruption("cell payload lies beyond the page boundary"))
                    .map(ToOwned::to_owned)
            })
            .collect()
    }

    pub(super) fn insert(&mut self, index: usize, cell: &[u8]) -> Result<()> {
        let slots = self.slots()?;
        if index > slots.len() {
            return Err(type_error("insert index is outside the slot directory"));
        }

        let cell_len = cell.len();
        let cell_len_u16 = u16::try_from(cell_len)
            .map_err(|_| type_error("cell length exceeds the slotted-page limit"))?;
        let cell_count = u16::try_from(slots.len())
            .map_err(|_| corruption("slot count exceeds the disk layout"))?;
        let new_count = cell_count
            .checked_add(1)
            .ok_or_else(|| type_error("slot count exceeds the slotted-page limit"))?;
        let new_slot_end = self.slot_end_for_count(new_count)?;
        let page_len = self.frame.bytes().len();
        let existing_payload_len = slots.iter().try_fold(0usize, |total, slot| {
            total
                .checked_add(slot.len)
                .ok_or_else(|| corruption("total cell payload length overflows"))
        })?;
        let payload_limit = page_len
            .checked_sub(new_slot_end)
            .ok_or_else(|| corruption("slot directory exceeds the page boundary"))?;
        let compacted_free = payload_limit
            .checked_sub(existing_payload_len)
            .ok_or_else(|| corruption("cell payloads overlap the slot directory"))?;
        if cell_len > compacted_free {
            return Err(type_error("cell cannot fit on the configured page"));
        }

        let required_contiguous = cell_len
            .checked_add(SLOT_LEN)
            .ok_or_else(|| type_error("cell length exceeds the slotted-page limit"))?;
        if self.contiguous_free()? < required_contiguous {
            self.compact()?;
        }

        let old_slot_end = usize::from(self.frame.header().slot_end());
        let old_payload_start = usize::from(self.frame.header().payload_start());
        let new_payload_start = old_payload_start
            .checked_sub(cell_len)
            .ok_or_else(|| corruption("cell payload underflows the page"))?;
        if new_payload_start < new_slot_end {
            return Err(corruption(
                "cell payload overlaps the expanded slot directory",
            ));
        }
        let slot_offset = self.slot_offset(index)?;
        let shifted_slot_offset = slot_offset
            .checked_add(SLOT_LEN)
            .ok_or_else(|| corruption("slot directory shift overflows"))?;
        let slot_end = slot_offset
            .checked_add(SLOT_LEN)
            .ok_or_else(|| corruption("slot record end overflows"))?;
        let new_payload_end = new_payload_start
            .checked_add(cell_len)
            .ok_or_else(|| corruption("cell payload end overflows"))?;

        {
            let bytes = self.frame.bytes_mut();
            bytes.copy_within(slot_offset..old_slot_end, shifted_slot_offset);
            bytes[new_payload_start..new_payload_end].copy_from_slice(cell);
            write_slot(
                &mut bytes[slot_offset..slot_end],
                new_payload_start,
                cell_len_u16,
            )?;
        }
        self.frame.set_layout(
            new_count,
            u16::try_from(new_slot_end)
                .map_err(|_| corruption("slot directory end exceeds disk layout"))?,
            u16::try_from(new_payload_start)
                .map_err(|_| corruption("payload start exceeds disk layout"))?,
        )?;
        self.validate()
    }

    pub(super) fn remove(&mut self, index: usize) -> Result<Vec<u8>> {
        let slots = self.slots()?;
        let slot = *slots
            .get(index)
            .ok_or_else(|| type_error("remove index is outside the slot directory"))?;
        let cell = self
            .frame
            .bytes()
            .get(slot.offset..slot.end()?)
            .ok_or_else(|| corruption("cell payload lies beyond the page boundary"))?
            .to_vec();
        let old_slot_end = usize::from(self.frame.header().slot_end());
        let new_count = u16::try_from(slots.len())
            .map_err(|_| corruption("slot count exceeds the disk layout"))?
            .checked_sub(1)
            .ok_or_else(|| corruption("slot directory has no cell to remove"))?;
        let new_slot_end = self.slot_end_for_count(new_count)?;
        let slot_offset = self.slot_offset(index)?;
        let after_slot_offset = slot_offset
            .checked_add(SLOT_LEN)
            .ok_or_else(|| corruption("slot directory shift overflows"))?;
        let new_payload_start = slots
            .iter()
            .enumerate()
            .filter(|(slot_index, _)| *slot_index != index)
            .map(|(_, slot)| slot.offset)
            .min()
            .unwrap_or(self.frame.bytes().len());

        {
            let bytes = self.frame.bytes_mut();
            bytes.copy_within(after_slot_offset..old_slot_end, slot_offset);
            bytes[new_slot_end..old_slot_end].fill(0);
        }
        self.frame.set_layout(
            new_count,
            u16::try_from(new_slot_end)
                .map_err(|_| corruption("slot directory end exceeds disk layout"))?,
            u16::try_from(new_payload_start)
                .map_err(|_| corruption("payload start exceeds disk layout"))?,
        )?;
        self.compact()?;
        Ok(cell)
    }

    pub(super) fn compact(&mut self) -> Result<()> {
        let cells = self.cells()?;
        let cell_count = u16::try_from(cells.len())
            .map_err(|_| corruption("slot count exceeds the disk layout"))?;
        let slot_end = self.slot_end_for_count(cell_count)?;
        let page_len = self.frame.bytes().len();
        let slot_offsets: Vec<usize> = (0..cells.len())
            .map(|index| self.slot_offset(index))
            .collect::<Result<_>>()?;
        let mut payload_start = page_len;

        {
            let bytes = self.frame.bytes_mut();
            bytes[slot_end..].fill(0);
            for (slot_offset, cell) in slot_offsets.iter().copied().zip(&cells) {
                payload_start = payload_start
                    .checked_sub(cell.len())
                    .ok_or_else(|| corruption("cell payload underflows the page"))?;
                if payload_start < slot_end {
                    return Err(corruption("cell payload overlaps the slot directory"));
                }
                let payload_end = payload_start
                    .checked_add(cell.len())
                    .ok_or_else(|| corruption("cell payload end overflows"))?;
                let slot_record_end = slot_offset
                    .checked_add(SLOT_LEN)
                    .ok_or_else(|| corruption("slot record end overflows"))?;
                bytes[payload_start..payload_end].copy_from_slice(cell);
                write_slot(
                    &mut bytes[slot_offset..slot_record_end],
                    payload_start,
                    u16::try_from(cell.len())
                        .map_err(|_| corruption("existing cell length exceeds disk layout"))?,
                )?;
            }
        }
        self.frame.set_layout(
            cell_count,
            u16::try_from(slot_end)
                .map_err(|_| corruption("slot directory end exceeds disk layout"))?,
            u16::try_from(payload_start)
                .map_err(|_| corruption("payload start exceeds disk layout"))?,
        )?;
        self.validate()
    }

    pub(super) fn contiguous_free(&self) -> Result<usize> {
        self.validate()?;
        usize::from(self.frame.header().payload_start())
            .checked_sub(usize::from(self.frame.header().slot_end()))
            .ok_or_else(|| corruption("page free-space boundaries cross"))
    }

    pub(super) fn compacted_free(&self) -> Result<usize> {
        let slots = self.slots()?;
        let payload_len = slots.iter().try_fold(0usize, |total, slot| {
            total
                .checked_add(slot.len)
                .ok_or_else(|| corruption("total cell payload length overflows"))
        })?;
        self.frame
            .bytes()
            .len()
            .checked_sub(usize::from(self.frame.header().slot_end()))
            .and_then(|available| available.checked_sub(payload_len))
            .ok_or_else(|| corruption("cell payloads overlap the slot directory"))
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_metadata_end(self.frame, self.metadata_end)?;
        let cell_count = self.frame.header().cell_count();
        let expected_slot_end = self.slot_end_for_count(cell_count)?;
        let actual_slot_end = usize::from(self.frame.header().slot_end());
        let payload_start = usize::from(self.frame.header().payload_start());
        let page_len = self.frame.bytes().len();
        if actual_slot_end != expected_slot_end {
            return Err(corruption(
                "slot directory end does not match the cell count",
            ));
        }
        if actual_slot_end > payload_start || payload_start > page_len {
            return Err(corruption("page free-space boundaries cross"));
        }

        let slots = self.read_slots_unchecked(cell_count)?;
        let mut lowest_payload_offset = page_len;
        for (index, slot) in slots.iter().enumerate() {
            let payload_end = slot.end()?;
            if slot.offset < actual_slot_end || payload_end > page_len {
                return Err(corruption(
                    "cell payload overlaps page metadata or exceeds the page",
                ));
            }
            if slot.offset < payload_start {
                return Err(corruption(
                    "cell payload lies below the declared payload start",
                ));
            }
            lowest_payload_offset = lowest_payload_offset.min(slot.offset);
            for earlier in &slots[..index] {
                if slot.offset < earlier.end()? && earlier.offset < payload_end {
                    return Err(corruption("cell payload slots overlap"));
                }
            }
        }
        if payload_start != lowest_payload_offset {
            return Err(corruption(
                "payload start does not match the lowest live cell",
            ));
        }
        Ok(())
    }

    fn slots(&self) -> Result<Vec<Slot>> {
        self.validate()?;
        self.read_slots_unchecked(self.frame.header().cell_count())
    }

    fn read_slots_unchecked(&self, cell_count: u16) -> Result<Vec<Slot>> {
        (0..usize::from(cell_count))
            .map(|index| {
                let slot_offset = self.slot_offset(index)?;
                let slot_end = slot_offset
                    .checked_add(SLOT_LEN)
                    .ok_or_else(|| corruption("slot record end overflows"))?;
                let slot_bytes = self
                    .frame
                    .bytes()
                    .get(slot_offset..slot_end)
                    .ok_or_else(|| corruption("slot directory lies beyond the page boundary"))?;
                Ok(Slot {
                    offset: usize::from(u16::from_le_bytes(
                        slot_bytes[..2]
                            .try_into()
                            .map_err(|_| corruption("slot offset is truncated"))?,
                    )),
                    len: usize::from(u16::from_le_bytes(
                        slot_bytes[2..]
                            .try_into()
                            .map_err(|_| corruption("slot length is truncated"))?,
                    )),
                })
            })
            .collect()
    }

    fn slot_offset(&self, index: usize) -> Result<usize> {
        let slot_bytes = index
            .checked_mul(SLOT_LEN)
            .ok_or_else(|| corruption("slot directory offset overflows"))?;
        self.metadata_end
            .checked_add(slot_bytes)
            .ok_or_else(|| corruption("slot directory offset overflows"))
    }

    fn slot_end_for_count(&self, count: u16) -> Result<usize> {
        let slots_len = usize::from(count)
            .checked_mul(SLOT_LEN)
            .ok_or_else(|| corruption("slot directory length overflows"))?;
        let slot_end = self
            .metadata_end
            .checked_add(slots_len)
            .ok_or_else(|| corruption("slot directory end overflows"))?;
        if slot_end > self.frame.bytes().len() {
            return Err(corruption("slot directory lies beyond the page boundary"));
        }
        Ok(slot_end)
    }
}

impl Slot {
    fn end(self) -> Result<usize> {
        self.offset
            .checked_add(self.len)
            .ok_or_else(|| corruption("cell payload end overflows"))
    }
}

fn validate_metadata_end(frame: &PageFrame, metadata_end: usize) -> Result<()> {
    if metadata_end < PAGE_HEADER_LEN {
        return Err(corruption("node metadata overlaps the page header"));
    }
    if metadata_end > frame.bytes().len() {
        return Err(corruption("node metadata exceeds the page boundary"));
    }
    if u16::try_from(metadata_end).is_err() {
        return Err(corruption("node metadata end exceeds disk layout"));
    }
    Ok(())
}

fn write_slot(slot_bytes: &mut [u8], offset: usize, len: u16) -> Result<()> {
    if slot_bytes.len() != SLOT_LEN {
        return Err(corruption("slot encoding has an invalid length"));
    }
    let offset =
        u16::try_from(offset).map_err(|_| corruption("slot offset exceeds disk layout"))?;
    slot_bytes[..2].copy_from_slice(&offset.to_le_bytes());
    slot_bytes[2..].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

fn type_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Type, message)
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::SlottedPage;
    use crate::ErrorCode;

    use super::super::{
        PageId, PageRef,
        checksum::stamp_page_checksum,
        page::{PageFrame, PageType},
    };

    #[test]
    fn insert_remove_and_compact_preserve_cell_order_at_every_page_size() {
        for page_size in [4096, 8192, 16_384] {
            let mut frame = leaf_frame(page_size);
            let mut page = SlottedPage::open(&mut frame, 56).expect("open slotted page");
            page.insert(0, b"bbb").expect("first insert");
            page.insert(0, b"a").expect("front insert");
            page.insert(1, b"cc").expect("middle insert");

            assert_eq!(
                page.cells().expect("cells"),
                vec![b"a".to_vec(), b"cc".to_vec(), b"bbb".to_vec()]
            );
            assert_eq!(page.remove(1).expect("remove"), b"cc");
            page.compact().expect("compact");
            page.validate().expect("valid compacted page");
            assert_eq!(
                page.cells().expect("cells after compaction"),
                vec![b"a".to_vec(), b"bbb".to_vec()]
            );
        }
    }

    #[test]
    fn overlapping_slots_are_corruption() {
        let mut frame = encoded_frame_with_slots(&[(4000, 20), (4010, 20)]);
        assert_eq!(open_error(&mut frame).code(), ErrorCode::Corruption);
    }

    #[test]
    fn insert_compacts_a_valid_fragmented_page_before_consuming_compacted_space() {
        let mut frame = encoded_frame_with_cells(&[(80, b"aa"), (4094, b"zz")]);
        let mut page = SlottedPage::open(&mut frame, 56).expect("open fragmented page");
        assert_eq!(page.contiguous_free().expect("contiguous free"), 16);
        assert_eq!(page.compacted_free().expect("compacted free"), 4028);

        page.insert(1, &[0x6d; 100])
            .expect("insert after compaction");

        assert_eq!(
            page.cells().expect("cells after insertion"),
            vec![b"aa".to_vec(), vec![0x6d; 100], b"zz".to_vec()]
        );
        page.validate().expect("valid compacted layout");
    }

    #[test]
    fn inserts_reject_out_of_range_slots_and_cells_that_cannot_fit() {
        let mut frame = leaf_frame(4096);
        let mut page = SlottedPage::open(&mut frame, 56).expect("open slotted page");

        assert_eq!(
            page.insert(1, b"a")
                .expect_err("out of range insert")
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            page.insert(0, &[0; 4037])
                .expect_err("cell plus slot must fit")
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            page.cells().expect("unchanged cells"),
            Vec::<Vec<u8>>::new()
        );
        page.validate()
            .expect("layout unchanged after rejected inserts");
    }

    #[test]
    fn slot_directory_layout_must_match_metadata_and_cell_count() {
        let mut frame = encoded_frame_with_slots(&[(4095, 1)]);
        frame
            .set_layout(1, 56, 4095)
            .expect("header accepts locally bounded layout");
        frame.prepare_for_write().expect("restamped test frame");

        assert_eq!(open_error(&mut frame).code(), ErrorCode::Corruption);
    }

    #[test]
    fn payload_start_must_bound_the_lowest_live_cell() {
        let mut frame = encoded_frame_with_cells(&[(4000, b"x")]);
        frame
            .set_layout(1, 60, 3999)
            .expect("header accepts locally bounded layout");
        frame.prepare_for_write().expect("restamped test frame");

        assert_eq!(open_error(&mut frame).code(), ErrorCode::Corruption);
    }

    fn leaf_frame(page_size: u32) -> PageFrame {
        PageFrame::new_zeroed(
            PageType::TableLeaf,
            PageRef::new(PageId(1), 1).expect("page reference"),
            page_size,
        )
        .expect("zeroed leaf frame")
    }

    fn open_error(frame: &mut PageFrame) -> crate::DbError {
        match SlottedPage::open(frame, 56) {
            Ok(_) => panic!("malformed slotted page must be rejected"),
            Err(error) => error,
        }
    }

    fn encoded_frame_with_slots(slots: &[(u16, u16)]) -> PageFrame {
        let frame = leaf_frame(4096);
        let mut header = *frame.header();
        let slot_bytes = slots
            .len()
            .checked_mul(4)
            .expect("test slot bytes fit usize");
        let slot_end = 56usize
            .checked_add(slot_bytes)
            .expect("test slot end fits usize");
        let payload_start = slots
            .iter()
            .map(|(offset, _)| usize::from(*offset))
            .min()
            .unwrap_or(4096);
        header
            .set_layout(
                u16::try_from(slots.len()).expect("test cell count fits u16"),
                u16::try_from(slot_end).expect("test slot end fits u16"),
                u16::try_from(payload_start).expect("test payload start fits u16"),
            )
            .expect("test layout is locally bounded");

        let mut bytes = frame.bytes().to_vec();
        bytes[..48].copy_from_slice(&header.encode());
        for (index, (offset, len)) in slots.iter().enumerate() {
            let start = 56usize
                .checked_add(index.checked_mul(4).expect("test slot offset fits usize"))
                .expect("test slot offset fits usize");
            bytes[start..start + 2].copy_from_slice(&offset.to_le_bytes());
            bytes[start + 2..start + 4].copy_from_slice(&len.to_le_bytes());
        }
        stamp_page_checksum(&mut bytes).expect("stamp test page");

        PageFrame::decode(bytes, PageId(1), Some(PageType::TableLeaf), Some(1))
            .expect("decode test page")
    }

    fn encoded_frame_with_cells(cells: &[(u16, &[u8])]) -> PageFrame {
        let slots: Vec<(u16, u16)> = cells
            .iter()
            .map(|(offset, cell)| {
                (
                    *offset,
                    u16::try_from(cell.len()).expect("test cell length fits u16"),
                )
            })
            .collect();
        let mut frame = encoded_frame_with_slots(&slots);
        for (offset, cell) in cells {
            let start = usize::from(*offset);
            let end = start
                .checked_add(cell.len())
                .expect("test cell end fits usize");
            frame.bytes_mut()[start..end].copy_from_slice(cell);
        }
        frame.prepare_for_write().expect("restamp test page");
        frame
    }
}
