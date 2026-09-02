mod btree;
mod cache;
mod checksum;
mod codec;
mod file;
mod header;
mod page;
mod pager;
mod slotted;

#[cfg(test)]
mod test_support;

use file::OpenMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct PageId(pub(super) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct PageRef {
    page_id: PageId,
    generation: u64,
}

impl PageRef {
    pub(super) fn new(page_id: PageId, generation: u64) -> crate::Result<Self> {
        if page_id.0 == 0 {
            return Err(crate::DbError::new(
                crate::ErrorCode::Corruption,
                "page references cannot point to the database header",
            ));
        }
        if generation == 0 {
            return Err(crate::DbError::new(
                crate::ErrorCode::Corruption,
                "page reference generation must be nonzero",
            ));
        }

        Ok(Self {
            page_id,
            generation,
        })
    }

    pub(super) const fn page_id(self) -> PageId {
        self.page_id
    }

    pub(super) const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Lsn(pub(super) u64);

#[test]
fn page_ref_rejects_header_page_and_zero_generation() {
    assert_eq!(
        PageRef::new(PageId(0), 1).unwrap_err().code(),
        crate::ErrorCode::Corruption
    );
    assert_eq!(
        PageRef::new(PageId(1), 0).unwrap_err().code(),
        crate::ErrorCode::Corruption
    );
    assert_eq!(PageRef::new(PageId(7), 3).unwrap().page_id(), PageId(7));
}
