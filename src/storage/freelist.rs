use std::collections::BTreeSet;

use super::{
    PageId, PageRef,
    page::{PageFrame, PageType},
};
use crate::{DbError, ErrorCode, Result};

const PAGE_HEADER_LEN: usize = 48;
const TRUNK_PREFIX_LEN: usize = 24;
const LEAF_OFFSET: usize = PAGE_HEADER_LEN + TRUNK_PREFIX_LEN;

pub(super) struct Freelist;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FreelistTrunk {
    next: Option<PageRef>,
    leaves: Vec<PageId>,
}

impl FreelistTrunk {
    pub(super) const fn next(&self) -> Option<PageRef> {
        self.next
    }

    pub(super) fn leaves(&self) -> &[PageId] {
        &self.leaves
    }
}

#[derive(Debug, Default)]
pub(super) struct FreelistWalk {
    trunk_pages: BTreeSet<PageId>,
    leaf_pages: BTreeSet<PageId>,
}

impl FreelistWalk {
    pub(super) fn contains_trunk(&self, page_id: PageId) -> bool {
        self.trunk_pages.contains(&page_id)
    }

    pub(super) fn contains_leaf(&self, page_id: PageId) -> bool {
        self.leaf_pages.contains(&page_id)
    }
}

impl Freelist {
    pub(super) fn leaf_capacity(page_size: u32) -> Result<usize> {
        let page_size = usize::try_from(page_size)
            .map_err(|_| corruption("freelist page size exceeds usize"))?;
        let leaf_bytes = page_size
            .checked_sub(LEAF_OFFSET)
            .ok_or_else(|| corruption("freelist page is smaller than its trunk prefix"))?;
        Ok(leaf_bytes / std::mem::size_of::<u64>())
    }

    pub(super) fn empty_trunk(next: Option<PageRef>) -> FreelistTrunk {
        FreelistTrunk {
            next,
            leaves: Vec::new(),
        }
    }

    pub(super) fn decode(frame: &PageFrame) -> Result<FreelistTrunk> {
        if frame.header().page_type() != PageType::Freelist {
            return Err(corruption("freelist body belongs to a non-freelist page"));
        }
        if frame.header().page_id().0 == 0 || frame.header().generation() == 0 {
            return Err(corruption("freelist trunk has an invalid page reference"));
        }

        let bytes = frame.bytes();
        let capacity = Self::leaf_capacity(
            u32::try_from(bytes.len()).map_err(|_| corruption("freelist page exceeds u32"))?,
        )?;
        let next_page_id = decode_u64(bytes, PAGE_HEADER_LEN)?;
        let next_generation = decode_u64(bytes, PAGE_HEADER_LEN + 8)?;
        let leaf_count = usize::try_from(decode_u32(bytes, PAGE_HEADER_LEN + 16)?)
            .map_err(|_| corruption("freelist leaf count exceeds usize"))?;
        let reserved = decode_u32(bytes, PAGE_HEADER_LEN + 20)?;

        if reserved != 0 {
            return Err(corruption("freelist trunk reserved field is nonzero"));
        }
        if leaf_count > capacity {
            return Err(corruption("freelist leaf count exceeds trunk capacity"));
        }

        let next = match (next_page_id, next_generation) {
            (0, 0) => None,
            (0, _) | (_, 0) => {
                return Err(corruption(
                    "freelist next trunk page id and generation must both be zero or nonzero",
                ));
            }
            (page_id, generation) => Some(PageRef::new(PageId(page_id), generation)?),
        };

        let mut leaves = Vec::with_capacity(leaf_count);
        let mut seen = BTreeSet::new();
        for index in 0..leaf_count {
            let offset = LEAF_OFFSET
                .checked_add(
                    index
                        .checked_mul(std::mem::size_of::<u64>())
                        .ok_or_else(|| corruption("freelist leaf offset overflow"))?,
                )
                .ok_or_else(|| corruption("freelist leaf offset overflow"))?;
            let page_id = PageId(decode_u64(bytes, offset)?);
            if page_id.0 == 0 {
                return Err(corruption("freelist leaf page id must be nonzero"));
            }
            if !seen.insert(page_id) {
                return Err(corruption("freelist trunk has duplicate leaf page ids"));
            }
            leaves.push(page_id);
        }

        let used_end = LEAF_OFFSET
            .checked_add(
                leaf_count
                    .checked_mul(std::mem::size_of::<u64>())
                    .ok_or_else(|| corruption("freelist body length overflow"))?,
            )
            .ok_or_else(|| corruption("freelist body length overflow"))?;
        if bytes[used_end..].iter().any(|&byte| byte != 0) {
            return Err(corruption("freelist unused body bytes are nonzero"));
        }

        Ok(FreelistTrunk { next, leaves })
    }

    pub(super) fn write(frame: &mut PageFrame, trunk: &FreelistTrunk) -> Result<()> {
        if frame.header().page_type() != PageType::Freelist {
            return Err(corruption("freelist body belongs to a non-freelist page"));
        }
        if frame.header().page_id().0 == 0 || frame.header().generation() == 0 {
            return Err(corruption("freelist trunk has an invalid page reference"));
        }

        let capacity = Self::leaf_capacity(
            u32::try_from(frame.bytes().len())
                .map_err(|_| corruption("freelist page exceeds u32"))?,
        )?;
        if trunk.leaves.len() > capacity {
            return Err(corruption("freelist leaf count exceeds trunk capacity"));
        }

        let mut seen = BTreeSet::new();
        for &page_id in &trunk.leaves {
            if page_id.0 == 0 {
                return Err(corruption("freelist leaf page id must be nonzero"));
            }
            if !seen.insert(page_id) {
                return Err(corruption("freelist trunk has duplicate leaf page ids"));
            }
        }

        let leaf_count = u32::try_from(trunk.leaves.len())
            .map_err(|_| corruption("freelist leaf count exceeds u32"))?;
        let bytes = frame.bytes_mut();
        bytes[PAGE_HEADER_LEN..].fill(0);
        if let Some(next) = trunk.next {
            bytes[PAGE_HEADER_LEN..PAGE_HEADER_LEN + 8]
                .copy_from_slice(&next.page_id().0.to_le_bytes());
            bytes[PAGE_HEADER_LEN + 8..PAGE_HEADER_LEN + 16]
                .copy_from_slice(&next.generation().to_le_bytes());
        }
        bytes[PAGE_HEADER_LEN + 16..PAGE_HEADER_LEN + 20]
            .copy_from_slice(&leaf_count.to_le_bytes());
        for (index, page_id) in trunk.leaves.iter().enumerate() {
            let offset = LEAF_OFFSET
                .checked_add(
                    index
                        .checked_mul(std::mem::size_of::<u64>())
                        .ok_or_else(|| corruption("freelist leaf offset overflow"))?,
                )
                .ok_or_else(|| corruption("freelist leaf offset overflow"))?;
            bytes[offset..offset + std::mem::size_of::<u64>()]
                .copy_from_slice(&page_id.0.to_le_bytes());
        }
        Ok(())
    }

    pub(super) fn allocate(trunk: &mut FreelistTrunk) -> Option<PageId> {
        trunk.leaves.pop()
    }

    pub(super) fn is_full(trunk: &FreelistTrunk, page_size: u32) -> Result<bool> {
        Ok(trunk.leaves.len() >= Self::leaf_capacity(page_size)?)
    }

    pub(super) fn release(
        trunk: &mut FreelistTrunk,
        page_id: PageId,
        page_size: u32,
    ) -> Result<()> {
        if page_id.0 == 0 {
            return Err(corruption("freelist leaf page id must be nonzero"));
        }
        if trunk.leaves.contains(&page_id) {
            return Err(corruption("freelist trunk has duplicate leaf page ids"));
        }
        if trunk.leaves.len() >= Self::leaf_capacity(page_size)? {
            return Err(corruption("freelist trunk is full"));
        }
        trunk.leaves.push(page_id);
        Ok(())
    }

    pub(super) fn walk(
        root: PageId,
        page_count: u64,
        mut read_page: impl FnMut(PageId) -> Result<PageFrame>,
    ) -> Result<FreelistWalk> {
        if root.0 == 0 {
            return Ok(FreelistWalk::default());
        }

        let mut result = FreelistWalk::default();
        let mut current = Some((root, None));
        while let Some((page_id, expected_generation)) = current {
            validate_page_id(page_id, page_count)?;
            if !result.trunk_pages.insert(page_id) {
                return Err(corruption("freelist trunk chain has a cycle"));
            }

            let frame = read_page(page_id)?;
            if frame.header().page_id() != page_id
                || frame.header().page_type() != PageType::Freelist
                || frame.header().generation() == 0
            {
                return Err(corruption(
                    "freelist trunk reference does not match its page",
                ));
            }
            if expected_generation
                .is_some_and(|generation| frame.header().generation() != generation)
            {
                return Err(corruption(
                    "freelist trunk generation does not match its reference",
                ));
            }

            let trunk = Self::decode(&frame)?;
            for leaf in trunk.leaves() {
                validate_page_id(*leaf, page_count)?;
                if !result.leaf_pages.insert(*leaf) {
                    return Err(corruption("freelist has duplicate leaf page ids"));
                }
                let leaf_frame = read_page(*leaf)?;
                if leaf_frame.header().page_id() != *leaf
                    || leaf_frame.header().generation() == 0
                    || leaf_frame.header().page_type() == PageType::Freelist
                {
                    return Err(corruption(
                        "freelist leaf reference does not match its page",
                    ));
                }
            }

            current = trunk
                .next()
                .map(|next| (next.page_id(), Some(next.generation())));
        }

        if !result.trunk_pages.is_disjoint(&result.leaf_pages) {
            return Err(corruption("freelist trunk and leaf page sets overlap"));
        }
        Ok(result)
    }
}

fn validate_page_id(page_id: PageId, page_count: u64) -> Result<()> {
    if page_id.0 == 0 || page_id.0 >= page_count {
        return Err(corruption("freelist page id is outside the database"));
    }
    Ok(())
}

fn decode_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(std::mem::size_of::<u64>())
        .ok_or_else(|| corruption("freelist field offset overflow"))?;
    let field: [u8; 8] = bytes
        .get(offset..end)
        .ok_or_else(|| corruption("freelist field exceeds page boundary"))?
        .try_into()
        .map_err(|_| corruption("freelist u64 field has invalid length"))?;
    Ok(u64::from_le_bytes(field))
}

fn decode_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(std::mem::size_of::<u32>())
        .ok_or_else(|| corruption("freelist field offset overflow"))?;
    let field: [u8; 4] = bytes
        .get(offset..end)
        .ok_or_else(|| corruption("freelist field exceeds page boundary"))?
        .try_into()
        .map_err(|_| corruption("freelist u32 field has invalid length"))?;
    Ok(u32::from_le_bytes(field))
}

fn corruption(message: impl Into<String>) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::Freelist;
    use crate::{ErrorCode, Result};

    use super::super::{
        PageId, PageRef,
        page::{PageFrame, PageType},
    };

    #[test]
    fn leaf_capacity_reserves_the_fixed_trunk_prefix() {
        assert_eq!(Freelist::leaf_capacity(4096).expect("leaf capacity"), 503);
    }

    #[test]
    fn trunk_codec_uses_the_fixed_little_endian_prefix() {
        let next = PageRef::new(PageId(9), 4).expect("next trunk reference");
        let mut frame = PageFrame::new_zeroed(
            PageType::Freelist,
            PageRef::new(PageId(3), 2).expect("trunk page reference"),
            4096,
        )
        .expect("trunk frame");
        let mut trunk = Freelist::empty_trunk(Some(next));
        Freelist::release(&mut trunk, PageId(7), 4096).expect("add first leaf");
        Freelist::release(&mut trunk, PageId(11), 4096).expect("add second leaf");

        Freelist::write(&mut frame, &trunk).expect("write trunk body");

        assert_eq!(
            &frame.bytes()[48..64],
            &[9, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(&frame.bytes()[64..72], &[2, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            &frame.bytes()[72..88],
            &[7, 0, 0, 0, 0, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0]
        );

        let decoded = Freelist::decode(&frame).expect("decode trunk body");
        assert_eq!(decoded.next(), Some(next));
        assert_eq!(decoded.leaves(), &[PageId(7), PageId(11)]);
    }

    #[test]
    fn walk_rejects_trunk_cycles_and_duplicate_leaf_ids() {
        let cycle = Freelist::walk(PageId(1), 2, |page_id| {
            assert_eq!(page_id, PageId(1));
            trunk_frame(
                PageId(1),
                Some(PageRef::new(PageId(1), 1).expect("self reference")),
                &[],
            )
        });
        assert_eq!(
            cycle.expect_err("trunk cycle must reject").code(),
            ErrorCode::Corruption
        );

        let duplicate = Freelist::walk(PageId(1), 4, |page_id| match page_id {
            PageId(1) => trunk_frame(
                PageId(1),
                Some(PageRef::new(PageId(2), 1).expect("second trunk reference")),
                &[PageId(3)],
            ),
            PageId(2) => trunk_frame(PageId(2), None, &[PageId(3)]),
            PageId(3) => PageFrame::new_zeroed(
                PageType::TableLeaf,
                PageRef::new(PageId(3), 1).expect("leaf reference"),
                4096,
            ),
            _ => unreachable!("walk must not read an unexpected page"),
        });
        assert_eq!(
            duplicate
                .expect_err("duplicate freelist leaf must reject")
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn walk_rejects_malformed_references_and_freelist_leaves() {
        let mut partial_next = trunk_frame(PageId(1), None, &[]).expect("trunk frame");
        partial_next.bytes_mut()[48..56].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(
            Freelist::decode(&partial_next)
                .expect_err("partial next trunk reference must reject")
                .code(),
            ErrorCode::Corruption
        );

        let generation_mismatch = Freelist::walk(PageId(1), 3, |page_id| match page_id {
            PageId(1) => trunk_frame(
                PageId(1),
                Some(PageRef::new(PageId(2), 2).expect("next trunk reference")),
                &[],
            ),
            PageId(2) => trunk_frame(PageId(2), None, &[]),
            _ => unreachable!("walk must not read an unexpected page"),
        });
        assert_eq!(
            generation_mismatch
                .expect_err("next trunk generation mismatch must reject")
                .code(),
            ErrorCode::Corruption
        );

        let out_of_range = Freelist::walk(PageId(1), 2, |page_id| match page_id {
            PageId(1) => trunk_frame(PageId(1), None, &[PageId(2)]),
            _ => unreachable!("out-of-range leaf must reject before a read"),
        });
        assert_eq!(
            out_of_range
                .expect_err("out-of-range leaf must reject")
                .code(),
            ErrorCode::Corruption
        );

        let overlap = Freelist::walk(PageId(1), 2, |page_id| {
            assert_eq!(page_id, PageId(1));
            trunk_frame(PageId(1), None, &[PageId(1)])
        });
        assert_eq!(
            overlap
                .expect_err("trunk page cannot also be a leaf")
                .code(),
            ErrorCode::Corruption
        );

        let invalid_leaf_type = Freelist::walk(PageId(1), 3, |page_id| match page_id {
            PageId(1) => trunk_frame(PageId(1), None, &[PageId(2)]),
            PageId(2) => trunk_frame(PageId(2), None, &[]),
            _ => unreachable!("walk must not read an unexpected page"),
        });
        assert_eq!(
            invalid_leaf_type
                .expect_err("freelist page cannot masquerade as a leaf")
                .code(),
            ErrorCode::Corruption
        );
    }

    fn trunk_frame(page_id: PageId, next: Option<PageRef>, leaves: &[PageId]) -> Result<PageFrame> {
        let mut frame = PageFrame::new_zeroed(
            PageType::Freelist,
            PageRef::new(page_id, 1).expect("trunk reference"),
            4096,
        )?;
        let mut trunk = Freelist::empty_trunk(next);
        for &leaf in leaves {
            Freelist::release(&mut trunk, leaf, 4096)?;
        }
        Freelist::write(&mut frame, &trunk)?;
        Ok(frame)
    }
}
