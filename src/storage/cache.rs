use std::{collections::BTreeMap, sync::Arc};

use super::{
    Lsn, PageId, PageRef,
    page::{PageFrame, PageType},
};

use crate::{DbError, ErrorCode, Result};

pub(super) type PageHandle = Arc<PageFrame>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct CacheKey {
    pub(super) page_id: PageId,
    pub(super) source_lsn: Lsn,
}

#[test]
fn page_handles_are_shareable_immutable_arcs() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PageHandle>();

    let page = handle(1);
    let clone = page.clone();
    assert!(Arc::ptr_eq(&page, &clone));
    assert_eq!(page.header().page_id(), PageId(1));
}

#[test]
fn page_cache_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PageCache>();
}

#[derive(Debug)]
struct CacheEntry {
    handle: PageHandle,
    last_used: u64,
}

#[derive(Debug)]
pub(super) struct PageCache {
    capacity: usize,
    entries: BTreeMap<CacheKey, CacheEntry>,
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

    pub(super) fn get(&mut self, key: CacheKey) -> Option<PageHandle> {
        let last_used = self.next_access();
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = last_used;
        Some(entry.handle.clone())
    }

    pub(super) fn insert_clean(&mut self, key: CacheKey, handle: PageHandle) -> Result<()> {
        let last_used = self.next_access();

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.handle = handle;
            entry.last_used = last_used;
            return Ok(());
        }

        if self.entries.len() == self.capacity {
            let evicted_key = self
                .entries
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.handle) == 1)
                .min_by_key(|(key, entry)| (entry.last_used, **key))
                .map(|(key, _)| *key)
                .ok_or_else(|| DbError::new(ErrorCode::Busy, "all cached pages are pinned"))?;
            self.entries.remove(&evicted_key);
        }

        self.entries.insert(key, CacheEntry { handle, last_used });
        Ok(())
    }

    pub(super) fn remove(&mut self, key: CacheKey) -> Option<PageHandle> {
        self.entries.remove(&key).map(|entry| entry.handle)
    }

    pub(super) fn remove_page_versions(&mut self, page_id: PageId) {
        self.entries.retain(|key, _| key.page_id != page_id);
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
        let mut keys: Vec<_> = self.entries.keys().copied().collect();
        keys.sort_unstable_by_key(|key| {
            let entry = self
                .entries
                .get(key)
                .expect("cache entries retain their collected keys");
            (entry.last_used, *key)
        });

        for (index, key) in keys.into_iter().enumerate() {
            self.entries
                .get_mut(&key)
                .expect("cache entries retain their collected keys")
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
    cache
        .insert_clean(key(1, 0), one.clone())
        .expect("insert page one");
    cache
        .insert_clean(key(2, 0), two.clone())
        .expect("insert page two");
    drop(two);

    cache.get(key(1, 0)).expect("cached page one");
    cache
        .insert_clean(key(3, 0), handle(3))
        .expect("evict unpinned page two");
    assert!(cache.get(key(1, 0)).is_some());
    let pin_three = cache.get(key(3, 0)).expect("cached page three");
    assert_eq!(
        cache
            .insert_clean(key(4, 0), handle(4))
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
            .insert_clean(key(page_id, 0), handle(page_id))
            .expect("insert unpinned page");
        assert!(cache.len() <= 2);
    }

    assert_eq!(cache.len(), 2);
    assert!(cache.get(key(1, 0)).is_none());
    assert!(cache.get(key(2, 0)).is_none());
    assert!(cache.get(key(3, 0)).is_some());
    assert!(cache.get(key(4, 0)).is_some());
}

#[test]
fn equal_last_used_entries_evict_the_smaller_page_id() {
    let mut cache = PageCache::new(2).expect("cache");
    cache
        .insert_clean(key(2, 0), handle(2))
        .expect("insert page two");
    cache
        .insert_clean(key(1, 0), handle(1))
        .expect("insert page one");
    cache
        .entries
        .get_mut(&key(1, 0))
        .expect("page one")
        .last_used = 7;
    cache
        .entries
        .get_mut(&key(2, 0))
        .expect("page two")
        .last_used = 7;

    cache
        .insert_clean(key(3, 0), handle(3))
        .expect("evict tied page");

    assert!(cache.get(key(1, 0)).is_none());
    assert!(cache.get(key(2, 0)).is_some());
    assert!(cache.get(key(3, 0)).is_some());
}

#[test]
fn remove_returns_the_entry_and_updates_length() {
    let mut cache = PageCache::new(2).expect("cache");
    cache
        .insert_clean(key(1, 0), handle(1))
        .expect("insert page one");
    assert_eq!(cache.len(), 1);

    let removed = cache.remove(key(1, 0)).expect("removed page");
    assert_eq!(removed.header().page_id(), PageId(1));
    assert_eq!(cache.len(), 0);
    assert!(cache.remove(key(1, 0)).is_none());
}

#[test]
fn access_clock_overflow_preserves_lru_eviction_order() {
    let mut cache = PageCache::new(2).expect("cache");
    cache
        .insert_clean(key(1, 0), handle(1))
        .expect("insert page one");
    cache
        .insert_clean(key(2, 0), handle(2))
        .expect("insert page two");
    cache.get(key(1, 0)).expect("refresh page one");
    cache.access_clock = u64::MAX;

    cache
        .insert_clean(key(3, 0), handle(3))
        .expect("insert after clock overflow");

    cache
        .insert_clean(key(4, 0), handle(4))
        .expect("evict oldest page after clock overflow");

    assert!(cache.get(key(1, 0)).is_none());
    assert!(cache.get(key(2, 0)).is_none());
    assert!(cache.get(key(3, 0)).is_some());
    assert!(cache.get(key(4, 0)).is_some());
}

#[test]
fn page_versions_with_different_source_lsns_coexist() {
    let mut cache = PageCache::new(2).expect("cache");
    cache
        .insert_clean(key(5, 2), marked_handle(5, 0x22))
        .expect("insert old version");
    cache
        .insert_clean(key(5, 7), marked_handle(5, 0x77))
        .expect("insert new version");

    assert_eq!(cache.len(), 2);
    assert_eq!(cache.get(key(5, 2)).expect("old version").bytes()[48], 0x22);
    assert_eq!(cache.get(key(5, 7)).expect("new version").bytes()[48], 0x77);
}

#[test]
fn capacity_counts_versions_and_evicts_the_oldest_version() {
    let mut cache = PageCache::new(2).expect("cache");
    cache
        .insert_clean(key(5, 2), marked_handle(5, 0x22))
        .expect("insert old version");
    cache
        .insert_clean(key(5, 7), marked_handle(5, 0x77))
        .expect("insert new version");
    cache
        .insert_clean(key(6, 8), marked_handle(6, 0x88))
        .expect("evict oldest version");

    assert_eq!(cache.len(), 2);
    assert!(cache.get(key(5, 2)).is_none());
    assert!(cache.get(key(5, 7)).is_some());
    assert!(cache.get(key(6, 8)).is_some());
}

#[test]
fn pinned_old_version_does_not_block_evicting_an_unpinned_new_version() {
    let mut cache = PageCache::new(2).expect("cache");
    let pinned_old = marked_handle(5, 0x22);
    cache
        .insert_clean(key(5, 2), pinned_old.clone())
        .expect("insert pinned old version");
    cache
        .insert_clean(key(5, 7), marked_handle(5, 0x77))
        .expect("insert unpinned new version");
    cache
        .insert_clean(key(6, 8), marked_handle(6, 0x88))
        .expect("evict unpinned new version");

    assert!(cache.get(key(5, 2)).is_some());
    assert!(cache.get(key(5, 7)).is_none());
    assert!(cache.get(key(6, 8)).is_some());
}

#[test]
fn remove_targets_one_cache_key_and_remove_page_versions_targets_all_versions() {
    let mut cache = PageCache::new(3).expect("cache");
    cache
        .insert_clean(key(5, 2), marked_handle(5, 0x22))
        .expect("insert old version");
    cache
        .insert_clean(key(5, 7), marked_handle(5, 0x77))
        .expect("insert new version");
    cache
        .insert_clean(key(6, 8), marked_handle(6, 0x88))
        .expect("insert other page");

    assert!(cache.remove(key(5, 2)).is_some());
    assert!(cache.get(key(5, 7)).is_some());
    cache.remove_page_versions(PageId(5));
    assert!(cache.get(key(5, 7)).is_none());
    assert!(cache.get(key(6, 8)).is_some());
}

const fn key(page_id: u64, source_lsn: u64) -> CacheKey {
    CacheKey {
        page_id: PageId(page_id),
        source_lsn: Lsn(source_lsn),
    }
}

fn marked_handle(id: u64, marker: u8) -> PageHandle {
    let mut frame = PageFrame::new_zeroed(
        PageType::TableLeaf,
        PageRef::new(PageId(id), 1).expect("page reference"),
        4096,
    )
    .expect("page frame");
    frame.bytes_mut()[48] = marker;
    Arc::new(frame)
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
