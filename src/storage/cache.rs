use std::{collections::BTreeMap, sync::Arc};

use super::{
    PageId, PageRef,
    page::{PageFrame, PageType},
};

use crate::{DbError, ErrorCode, Result};

pub(super) type PageHandle = Arc<PageFrame>;

#[test]
fn page_handles_are_shareable_immutable_arcs() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PageHandle>();

    let page = handle(1);
    let clone = page.clone();
    assert!(Arc::ptr_eq(&page, &clone));
    assert_eq!(page.header().page_id(), PageId(1));
}

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
        let page_id = handle.header().page_id();
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
                .filter(|(_, entry)| Arc::strong_count(&entry.handle) == 1)
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

#[test]
fn zero_cache_capacity_returns_type_error() {
    assert_eq!(
        PageCache::new(0).expect_err("zero cache capacity").code(),
        ErrorCode::Type
    );
}

#[test]
fn cache_length_never_exceeds_capacity() {
    let mut cache = PageCache::new(2).expect("cache");

    for page_id in 1..=4 {
        cache
            .insert_clean(handle(page_id))
            .expect("insert unpinned page");
        assert!(cache.len() <= 2);
    }

    assert_eq!(cache.len(), 2);
    assert!(cache.get(PageId(1)).is_none());
    assert!(cache.get(PageId(2)).is_none());
    assert!(cache.get(PageId(3)).is_some());
    assert!(cache.get(PageId(4)).is_some());
}

#[test]
fn equal_last_used_entries_evict_the_smaller_page_id() {
    let mut cache = PageCache::new(2).expect("cache");
    cache.insert_clean(handle(2)).expect("insert page two");
    cache.insert_clean(handle(1)).expect("insert page one");
    cache
        .entries
        .get_mut(&PageId(1))
        .expect("page one")
        .last_used = 7;
    cache
        .entries
        .get_mut(&PageId(2))
        .expect("page two")
        .last_used = 7;

    cache.insert_clean(handle(3)).expect("evict tied page");

    assert!(cache.get(PageId(1)).is_none());
    assert!(cache.get(PageId(2)).is_some());
    assert!(cache.get(PageId(3)).is_some());
}

#[test]
fn remove_returns_the_entry_and_updates_length() {
    let mut cache = PageCache::new(2).expect("cache");
    cache.insert_clean(handle(1)).expect("insert page one");
    assert_eq!(cache.len(), 1);

    let removed = cache.remove(PageId(1)).expect("removed page");
    assert_eq!(removed.header().page_id(), PageId(1));
    assert_eq!(cache.len(), 0);
    assert!(cache.remove(PageId(1)).is_none());
}

#[test]
fn access_clock_overflow_preserves_lru_eviction_order() {
    let mut cache = PageCache::new(2).expect("cache");
    cache.insert_clean(handle(1)).expect("insert page one");
    cache.insert_clean(handle(2)).expect("insert page two");
    cache.get(PageId(1)).expect("refresh page one");
    cache.access_clock = u64::MAX;

    cache
        .insert_clean(handle(3))
        .expect("insert after clock overflow");

    cache
        .insert_clean(handle(4))
        .expect("evict oldest page after clock overflow");

    assert!(cache.get(PageId(1)).is_none());
    assert!(cache.get(PageId(2)).is_none());
    assert!(cache.get(PageId(3)).is_some());
    assert!(cache.get(PageId(4)).is_some());
}

fn handle(id: u64) -> PageHandle {
    Arc::new(
        PageFrame::new_zeroed(
            PageType::TableLeaf,
            PageRef::new(PageId(id), 1).expect("page reference"),
            4096,
        )
        .expect("page frame"),
    )
}
