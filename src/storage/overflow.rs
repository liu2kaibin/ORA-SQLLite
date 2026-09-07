use std::collections::BTreeSet;

use super::{
    PageId, PageRef,
    page::{PageFrame, PageType},
    pager::{PageSource, WriteBatch},
};
use crate::{DbError, ErrorCode, Result};

const PAGE_HEADER_LEN: usize = 48;
const OVERFLOW_PREFIX_LEN: usize = 24;
const NEXT_PAGE_ID_OFFSET: usize = PAGE_HEADER_LEN;
const NEXT_GENERATION_OFFSET: usize = NEXT_PAGE_ID_OFFSET + 8;
const CHUNK_LEN_OFFSET: usize = NEXT_GENERATION_OFFSET + 8;
const RESERVED_OFFSET: usize = CHUNK_LEN_OFFSET + 4;
const CHUNK_OFFSET: usize = PAGE_HEADER_LEN + OVERFLOW_PREFIX_LEN;

pub(super) fn write_value(batch: &mut WriteBatch<'_>, bytes: &[u8]) -> Result<Option<PageRef>> {
    if bytes.is_empty() {
        return Ok(None);
    }

    let first = batch.allocate(PageType::Overflow)?;
    let chunk_capacity = {
        let frame = batch.modify(first)?;
        OverflowBody::chunk_capacity(frame.bytes().len())?
    };
    let value_len =
        u64::try_from(bytes.len()).map_err(|_| corruption("overflow value length exceeds u64"))?;
    let capacity = u64::try_from(chunk_capacity)
        .map_err(|_| corruption("overflow page chunk capacity exceeds u64"))?;
    let page_count = value_len
        .checked_add(
            capacity
                .checked_sub(1)
                .ok_or_else(|| corruption("overflow page chunk capacity is zero"))?,
        )
        .ok_or_else(|| corruption("overflow page count overflow"))?
        / capacity;
    let page_count =
        usize::try_from(page_count).map_err(|_| corruption("overflow page count exceeds usize"))?;

    let mut references = Vec::new();
    references
        .try_reserve_exact(page_count)
        .map_err(|_| corruption("overflow reference allocation failed"))?;
    references.push(first);
    for _ in 1..page_count {
        references.push(batch.allocate(PageType::Overflow)?);
    }

    let mut start = 0usize;
    for (index, reference) in references.iter().copied().enumerate() {
        let end = start
            .checked_add(chunk_capacity)
            .map(|end| end.min(bytes.len()))
            .ok_or_else(|| corruption("overflow chunk offset overflow"))?;
        let next = references.get(index + 1).copied();
        OverflowBody::write(batch.modify(reference)?, next, &bytes[start..end])?;
        start = end;
    }
    if start != bytes.len() {
        return Err(corruption(
            "overflow writer did not consume the whole value",
        ));
    }

    Ok(Some(first))
}

pub(super) fn read_value(
    source: &mut impl PageSource,
    first: PageRef,
    total_len: u64,
) -> Result<Vec<u8>> {
    let mut value = Vec::new();
    walk_chain(source, first, total_len, |_, chunk| {
        value
            .try_reserve(chunk.len())
            .map_err(|_| corruption("overflow value allocation failed"))?;
        value.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(value)
}

pub(super) fn release_chain(
    batch: &mut WriteBatch<'_>,
    first: PageRef,
    total_len: u64,
) -> Result<()> {
    let chain = collect_chain(batch, first, total_len)?;
    for reference in chain {
        batch.release_later(reference)?;
    }
    Ok(())
}

pub(super) fn collect_chain(
    source: &mut impl PageSource,
    first: PageRef,
    total_len: u64,
) -> Result<Vec<PageRef>> {
    let mut chain = Vec::new();
    walk_chain(source, first, total_len, |reference, _| {
        chain
            .try_reserve(1)
            .map_err(|_| corruption("overflow chain allocation failed"))?;
        chain.push(reference);
        Ok(())
    })?;
    Ok(chain)
}

pub(super) fn validate_local(frame: &PageFrame) -> Result<()> {
    OverflowBody::decode(frame).map(|_| ())
}

fn walk_chain(
    source: &mut impl PageSource,
    first: PageRef,
    total_len: u64,
    mut visit: impl FnMut(PageRef, &[u8]) -> Result<()>,
) -> Result<()> {
    if total_len == 0 {
        return Err(corruption(
            "overflow chains require a positive declared value length",
        ));
    }

    let max_hops = source
        .page_count()?
        .checked_sub(1)
        .ok_or_else(|| corruption("database has no header page"))?;
    let mut current = first;
    let mut visited = BTreeSet::new();
    let mut hops = 0u64;
    let mut value_len = 0u64;

    loop {
        if hops >= max_hops {
            return Err(corruption("overflow chain exceeds the ordinary page count"));
        }
        hops = hops
            .checked_add(1)
            .ok_or_else(|| corruption("overflow chain hop count overflow"))?;
        if !visited.insert(current.page_id()) {
            return Err(corruption("overflow chain has a cycle"));
        }

        let handle = source.read_page(current)?;
        let frame = handle.as_ref();
        if frame.header().page_id() != current.page_id()
            || frame.header().generation() != current.generation()
        {
            return Err(corruption(
                "overflow page reference does not match the loaded page",
            ));
        }
        let body = OverflowBody::decode(frame)?;
        let chunk = body.chunk(frame)?;
        value_len = value_len
            .checked_add(
                u64::try_from(chunk.len())
                    .map_err(|_| corruption("overflow chunk length exceeds u64"))?,
            )
            .ok_or_else(|| corruption("overflow value length overflow"))?;
        if value_len > total_len {
            return Err(corruption(
                "overflow chain contains more bytes than its declared length",
            ));
        }
        visit(current, chunk)?;

        match body.next() {
            Some(next) => current = next,
            None if value_len == total_len => return Ok(()),
            None => {
                return Err(corruption(
                    "overflow chain ends before its declared value length",
                ));
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OverflowBody {
    next: Option<PageRef>,
    chunk_len: usize,
}

impl OverflowBody {
    fn chunk_capacity(page_len: usize) -> Result<usize> {
        page_len
            .checked_sub(CHUNK_OFFSET)
            .ok_or_else(|| corruption("overflow page is smaller than its fixed prefix"))
    }

    fn decode(frame: &PageFrame) -> Result<Self> {
        if frame.header().page_type() != PageType::Overflow {
            return Err(corruption("overflow body belongs to a non-overflow page"));
        }
        if frame.header().page_id().0 == 0 || frame.header().generation() == 0 {
            return Err(corruption("overflow page has an invalid page reference"));
        }

        let bytes = frame.bytes();
        let capacity = Self::chunk_capacity(bytes.len())?;
        let next_page_id = decode_u64(bytes, NEXT_PAGE_ID_OFFSET)?;
        let next_generation = decode_u64(bytes, NEXT_GENERATION_OFFSET)?;
        let chunk_len = usize::try_from(decode_u32(bytes, CHUNK_LEN_OFFSET)?)
            .map_err(|_| corruption("overflow chunk length exceeds usize"))?;
        let reserved = decode_u32(bytes, RESERVED_OFFSET)?;

        if reserved != 0 {
            return Err(corruption("overflow page reserved bytes are nonzero"));
        }
        if chunk_len > capacity {
            return Err(corruption("overflow chunk length exceeds page capacity"));
        }

        let next = match (next_page_id, next_generation) {
            (0, 0) => None,
            (0, _) | (_, 0) => {
                return Err(corruption(
                    "overflow next page id and generation must both be zero or nonzero",
                ));
            }
            (page_id, generation) => Some(PageRef::new(PageId(page_id), generation)?),
        };

        Ok(Self { next, chunk_len })
    }

    fn write(frame: &mut PageFrame, next: Option<PageRef>, chunk: &[u8]) -> Result<()> {
        if frame.header().page_type() != PageType::Overflow {
            return Err(corruption("overflow body belongs to a non-overflow page"));
        }
        let capacity = Self::chunk_capacity(frame.bytes().len())?;
        if chunk.len() > capacity {
            return Err(corruption("overflow chunk length exceeds page capacity"));
        }
        let chunk_len = u32::try_from(chunk.len())
            .map_err(|_| corruption("overflow chunk length exceeds u32"))?;

        let bytes = frame.bytes_mut();
        bytes[NEXT_PAGE_ID_OFFSET..].fill(0);
        if let Some(next) = next {
            bytes[NEXT_PAGE_ID_OFFSET..NEXT_GENERATION_OFFSET]
                .copy_from_slice(&next.page_id().0.to_le_bytes());
            bytes[NEXT_GENERATION_OFFSET..CHUNK_LEN_OFFSET]
                .copy_from_slice(&next.generation().to_le_bytes());
        }
        bytes[CHUNK_LEN_OFFSET..RESERVED_OFFSET].copy_from_slice(&chunk_len.to_le_bytes());
        let chunk_end = CHUNK_OFFSET
            .checked_add(chunk.len())
            .ok_or_else(|| corruption("overflow chunk end offset overflow"))?;
        bytes[CHUNK_OFFSET..chunk_end].copy_from_slice(chunk);
        Ok(())
    }

    fn next(self) -> Option<PageRef> {
        self.next
    }

    fn chunk(self, frame: &PageFrame) -> Result<&[u8]> {
        let end = CHUNK_OFFSET
            .checked_add(self.chunk_len)
            .ok_or_else(|| corruption("overflow chunk end offset overflow"))?;
        frame
            .bytes()
            .get(CHUNK_OFFSET..end)
            .ok_or_else(|| corruption("overflow chunk exceeds page boundary"))
    }
}

fn decode_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(std::mem::size_of::<u64>())
        .ok_or_else(|| corruption("overflow field offset overflow"))?;
    let field: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| corruption("overflow field exceeds page boundary"))?
        .try_into()
        .map_err(|_| corruption("overflow u64 field has invalid length"))?;
    Ok(u64::from_le_bytes(field))
}

fn decode_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(std::mem::size_of::<u32>())
        .ok_or_else(|| corruption("overflow field offset overflow"))?;
    let field: [u8; 4] = bytes
        .get(offset..end)
        .ok_or_else(|| corruption("overflow field exceeds page boundary"))?
        .try_into()
        .map_err(|_| corruption("overflow u32 field has invalid length"))?;
    Ok(u32::from_le_bytes(field))
}

fn corruption(message: impl Into<String>) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use super::{collect_chain, read_value, release_chain, write_value};
    use crate::{ErrorCode, Result};

    use super::super::{
        PageId, PageRef,
        cache::PageHandle,
        file::OpenMode,
        page::{PageFrame, PageType},
        pager::{PageSource, Pager, PagerConfig},
        test_support::{TempDatabase, test_pager},
    };

    const CHUNK_OFFSET: usize = 72;

    #[test]
    fn overflow_round_trips_and_releases_multiple_pages() {
        let value = vec![0x6d; 9000];
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let first = {
            let mut batch = pager.begin_write().expect("begin overflow write");
            let first = write_value(&mut batch, &value)
                .expect("write overflow value")
                .expect("nonempty value allocates an overflow page");
            batch.commit().expect("commit overflow value");
            first
        };

        assert_eq!(
            read_value(&mut pager, first, value.len() as u64).expect("read overflow value"),
            value
        );
        let chain = collect_chain(&mut pager, first, 9000).expect("collect overflow chain");
        assert_eq!(chain.len(), 3);

        {
            let mut batch = pager.begin_write().expect("begin overflow release");
            release_chain(&mut batch, first, 9000).expect("release overflow chain");
            batch.commit().expect("commit overflow release");
        }

        let reused = {
            let mut batch = pager.begin_write().expect("begin reuse write");
            let reused = write_value(&mut batch, b"x")
                .expect("write replacement value")
                .expect("replacement value allocates a page");
            batch.commit().expect("commit replacement value");
            reused
        };
        assert!(
            chain
                .iter()
                .any(|reference| reference.page_id() == reused.page_id())
        );
    }

    #[test]
    fn overflow_round_trips_one_page_boundaries() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, mut pager) = test_pager(page_size).expect("create test pager");
            let before = pager.page_count();
            let empty = {
                let mut batch = pager.begin_write().expect("begin empty value write");
                let result = write_value(&mut batch, b"").expect("empty value write");
                batch.commit().expect("commit empty value write");
                result
            };
            assert_eq!(empty, None);
            assert_eq!(pager.page_count(), before);

            let capacity =
                usize::try_from(page_size).expect("test page size fits usize") - CHUNK_OFFSET;
            for len in [1, capacity, capacity + 1] {
                let value = vec![0x3a; len];
                let (_temp, mut pager) = test_pager(page_size).expect("create test pager");
                let first = {
                    let mut batch = pager.begin_write().expect("begin overflow write");
                    let first = write_value(&mut batch, &value)
                        .expect("write overflow value")
                        .expect("nonempty value allocates a page");
                    batch.commit().expect("commit overflow value");
                    first
                };

                assert_eq!(
                    read_value(&mut pager, first, len as u64).expect("read overflow value"),
                    value
                );
                assert_eq!(
                    collect_chain(&mut pager, first, len as u64)
                        .expect("collect overflow chain")
                        .len(),
                    if len <= capacity { 1 } else { 2 }
                );
            }
        }
    }

    #[test]
    fn batch_overlay_reads_a_multi_page_overflow_chain_before_commit() {
        let value = vec![0x27; 9000];
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let mut batch = pager.begin_write().expect("begin overflow write");
        let first = write_value(&mut batch, &value)
            .expect("write overflow value")
            .expect("nonempty value allocates a page");

        assert_eq!(
            collect_chain(&mut batch, first, value.len() as u64)
                .expect("collect batch-local overflow chain")
                .len(),
            3
        );
        assert_eq!(
            read_value(&mut batch, first, value.len() as u64)
                .expect("read batch-local overflow value"),
            value
        );
        batch
            .rollback()
            .expect("rollback batch-local overflow value");
    }

    #[test]
    fn overflow_chain_rejects_corrupt_bodies_and_references() {
        let capacity = 4096 - CHUNK_OFFSET;
        let cases = [
            (
                "cycle",
                source(
                    3,
                    [
                        (
                            reference(1, 1),
                            overflow_frame(reference(1, 1), Some(reference(2, 1)), 1, &[0xa1]),
                        ),
                        (
                            reference(2, 1),
                            overflow_frame(reference(2, 1), Some(reference(1, 1)), 1, &[0xa2]),
                        ),
                    ],
                ),
                reference(1, 1),
                2,
            ),
            (
                "wrong generation",
                source(
                    2,
                    [(
                        reference(1, 1),
                        overflow_frame(reference(1, 1), None, 1, &[0xa3]),
                    )],
                ),
                reference(1, 2),
                1,
            ),
            (
                "wrong page type",
                source(
                    2,
                    [(reference(1, 1), frame(PageType::TableLeaf, reference(1, 1)))],
                ),
                reference(1, 1),
                1,
            ),
            (
                "chunk above capacity",
                source(
                    2,
                    [(
                        reference(1, 1),
                        overflow_frame(
                            reference(1, 1),
                            None,
                            u32::try_from(capacity + 1).unwrap(),
                            &[],
                        ),
                    )],
                ),
                reference(1, 1),
                u64::try_from(capacity + 1).unwrap(),
            ),
            (
                "early end",
                source(
                    2,
                    [(
                        reference(1, 1),
                        overflow_frame(reference(1, 1), None, 1, &[0xa4]),
                    )],
                ),
                reference(1, 1),
                2,
            ),
            (
                "zero declared length",
                source(
                    2,
                    [(
                        reference(1, 1),
                        overflow_frame(reference(1, 1), None, 0, &[]),
                    )],
                ),
                reference(1, 1),
                0,
            ),
            (
                "excess bytes",
                source(
                    2,
                    [(
                        reference(1, 1),
                        overflow_frame(reference(1, 1), None, 2, &[0xa5, 0xa6]),
                    )],
                ),
                reference(1, 1),
                1,
            ),
            (
                "more pages than the database",
                source(
                    3,
                    [
                        (
                            reference(1, 1),
                            overflow_frame(reference(1, 1), Some(reference(2, 1)), 1, &[0xa7]),
                        ),
                        (
                            reference(2, 1),
                            overflow_frame(reference(2, 1), Some(reference(3, 1)), 1, &[0xa8]),
                        ),
                        (
                            reference(3, 1),
                            overflow_frame(reference(3, 1), None, 1, &[0xa9]),
                        ),
                    ],
                ),
                reference(1, 1),
                3,
            ),
        ];

        for (name, mut source, first, total_len) in cases {
            assert_eq!(
                read_from_source(&mut source, first, total_len)
                    .expect_err(name)
                    .code(),
                ErrorCode::Corruption,
                "{name} must be corruption"
            );
        }
    }

    #[test]
    fn overflow_chain_rejects_nonzero_reserved_bytes_and_bad_checksum() {
        let reference = reference(1, 1);
        let mut reserved = overflow_frame(reference, None, 1, &[0xbe]);
        reserved.bytes_mut()[68] = 1;
        reserved
            .prepare_for_write()
            .expect("restamp reserved field");

        assert_eq!(
            read_from_source(&mut source(2, [(reference, reserved)]), reference, 1)
                .expect_err("reserved bytes")
                .code(),
            ErrorCode::Corruption
        );

        let mut checksum = overflow_frame(reference, None, 1, &[0xef]);
        checksum.bytes_mut()[CHUNK_OFFSET] ^= 1;
        assert_eq!(
            read_from_source(&mut source(2, [(reference, checksum)]), reference, 1)
                .expect_err("bad checksum")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn malformed_dirty_overflow_body_rejects_commit_before_file_io() {
        let temp = TempDatabase::new("overflow-invalid-body");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create test pager");
        drop(pager);
        let before = std::fs::read(temp.path()).expect("read database before invalid commit");
        let mut pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .expect("reopen test pager");
        let error = {
            let mut batch = pager.begin_write().expect("begin overflow write");
            let first = write_value(&mut batch, b"x")
                .expect("write overflow value")
                .expect("nonempty value allocates a page");
            batch
                .modify(first)
                .expect("modify overflow page")
                .bytes_mut()[68] = 1;
            match batch.commit() {
                Ok(()) => panic!("nonzero overflow reserved bytes must reject commit"),
                Err(error) => error,
            }
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert!(!pager.is_poisoned());
        drop(pager);
        assert_eq!(
            std::fs::read(temp.path()).expect("read database after invalid commit"),
            before
        );
    }

    fn read_from_source(
        source: &mut impl PageSource,
        first: PageRef,
        total_len: u64,
    ) -> Result<Vec<u8>> {
        read_value(source, first, total_len)
    }

    struct TestSource {
        page_count: u64,
        pages: BTreeMap<PageId, Vec<u8>>,
    }

    impl PageSource for TestSource {
        fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
            let bytes = self
                .pages
                .get(&reference.page_id())
                .ok_or_else(|| crate::DbError::new(ErrorCode::Corruption, "test page not found"))?
                .clone();
            let frame = PageFrame::decode(
                bytes,
                reference.page_id(),
                None,
                Some(reference.generation()),
            )?;
            Ok(Arc::new(frame))
        }

        fn page_count(&self) -> Result<u64> {
            Ok(self.page_count)
        }
    }

    fn source<const N: usize>(page_count: u64, pages: [(PageRef, PageFrame); N]) -> TestSource {
        TestSource {
            page_count,
            pages: pages
                .into_iter()
                .map(|(reference, frame)| (reference.page_id(), frame.bytes().to_vec()))
                .collect(),
        }
    }

    fn reference(page_id: u64, generation: u64) -> PageRef {
        PageRef::new(PageId(page_id), generation).expect("test reference")
    }

    fn frame(page_type: PageType, reference: PageRef) -> PageFrame {
        let mut frame = PageFrame::new_zeroed(page_type, reference, 4096).expect("test frame");
        frame.prepare_for_write().expect("stamp test frame");
        frame
    }

    fn overflow_frame(
        reference: PageRef,
        next: Option<PageRef>,
        chunk_len: u32,
        chunk: &[u8],
    ) -> PageFrame {
        let mut frame = frame(PageType::Overflow, reference);
        let bytes = frame.bytes_mut();
        if let Some(next) = next {
            bytes[48..56].copy_from_slice(&next.page_id().0.to_le_bytes());
            bytes[56..64].copy_from_slice(&next.generation().to_le_bytes());
        }
        bytes[64..68].copy_from_slice(&chunk_len.to_le_bytes());
        bytes[CHUNK_OFFSET..CHUNK_OFFSET + chunk.len()].copy_from_slice(chunk);
        frame.prepare_for_write().expect("restamp overflow frame");
        frame
    }
}
