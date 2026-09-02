use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use super::{
    PageId, PageRef,
    page::{PageFrame, PageType},
};

use crate::{DbError, ErrorCode, Result};

pub(super) type PageHandle = Rc<RefCell<PageFrame>>;

#[derive(Debug)]
struct CacheEntry {
    handle: PageHandle,
    last_used: u64,
}

#[derive(Debug)]
pub(super) struct PageCache {
    capacity: usize,
    entries: BTreeMap<PageId, CacheEntry>,
    access_clock: u64,
}

impl PageCache {
    pub(super) fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(DbError::new(
                ErrorCode::Type,
                "page cache capacity must be nonzero",
            ));
        }

        Ok(Self {
            capacity,
            entries: BTreeMap::new(),
            access_clock: 0,
        })
    }

    pub(super) fn get(&mut self, page_id: PageId) -> Option<PageHandle> {
        let last_used = self.next_access();
        let entry = self.entries.get_mut(&page_id)?;
        entry.last_used = last_used;
        Some(entry.handle.clone())
    }

    pub(super) fn insert_clean(&mut self, handle: PageHandle) -> Result<()> {
        let page_id = handle.borrow().header().page_id();
        let last_used = self.next_access();

        if let Some(entry) = self.entries.get_mut(&page_id) {
            entry.handle = handle;
            entry.last_used = last_used;
            return Ok(());
        }

        if self.entries.len() == self.capacity {
            let evicted_page_id = self
                .entries
                .iter()
                .filter(|(_, entry)| Rc::strong_count(&entry.handle) == 1)
                .min_by_key(|(page_id, entry)| (entry.last_used, **page_id))
                .map(|(page_id, _)| *page_id)
                .ok_or_else(|| DbError::new(ErrorCode::Busy, "all cached pages are pinned"))?;
            self.entries.remove(&evicted_page_id);
        }

        self.entries
            .insert(page_id, CacheEntry { handle, last_used });
        Ok(())
    }

    pub(super) fn remove(&mut self, page_id: PageId) -> Option<PageHandle> {
        self.entries.remove(&page_id).map(|entry| entry.handle)
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    fn next_access(&mut self) -> u64 {
        if self.access_clock == u64::MAX {
            self.renormalize_access_clock();
        }
        self.access_clock += 1;
        self.access_clock
    }

    fn renormalize_access_clock(&mut self) {
        let mut page_ids: Vec<_> = self.entries.keys().copied().collect();
        page_ids.sort_unstable_by_key(|page_id| {
            let entry = self
                .entries
                .get(page_id)
                .expect("cache entries retain their collected page ids");
            (entry.last_used, *page_id)
        });

        for (index, page_id) in page_ids.into_iter().enumerate() {
            self.entries
                .get_mut(&page_id)
                .expect("cache entries retain their collected page ids")
                .last_used = index as u64;
        }
        self.access_clock = self.entries.len() as u64;
    }
}

#[test]
fn lru_skips_pinned_pages_and_reports_busy_when_all_are_pinned() {
    let mut cache = PageCache::new(2).expect("cache");
    let one = handle(1);
    let two = handle(2);
    cache.insert_clean(one.clone()).expect("insert page one");
    cache.insert_clean(two.clone()).expect("insert page two");
    drop(two);

    cache.get(PageId(1)).expect("cached page one");
    cache
        .insert_clean(handle(3))
        .expect("evict unpinned page two");
    assert!(cache.get(PageId(1)).is_some());
    let pin_three = cache.get(PageId(3)).expect("cached page three");
    assert_eq!(
        cache
            .insert_clean(handle(4))
            .expect_err("all cached pages are pinned")
            .code(),
        crate::ErrorCode::Busy
    );
    drop(pin_three);
}

fn handle(id: u64) -> PageHandle {
    Rc::new(RefCell::new(
        PageFrame::new_zeroed(
            PageType::TableLeaf,
            PageRef::new(PageId(id), 1).expect("page reference"),
            4096,
        )
        .expect("page frame"),
    ))
}
