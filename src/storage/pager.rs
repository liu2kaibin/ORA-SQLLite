use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::Path,
    rc::Rc,
};

use super::{
    PageId, PageRef,
    cache::{PageCache, PageHandle},
    file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode},
    header::DatabaseHeader,
    page::{PageFrame, PageType},
};
use crate::{DbError, ErrorCode, Result};

pub(super) struct PagerConfig {
    pub(super) page_size: u32,
    pub(super) uuid: [u8; 16],
    pub(super) max_database_bytes: u64,
    pub(super) cache_pages: usize,
}

impl PagerConfig {
    pub(super) fn new(page_size: u32, uuid: [u8; 16]) -> Result<Self> {
        DatabaseHeader::new(page_size, uuid)?;
        Ok(Self {
            page_size,
            uuid,
            max_database_bytes: DEFAULT_MAX_DATABASE_BYTES,
            cache_pages: 1024,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PagerState {
    Ready,
    Poisoned,
}

pub(super) struct Pager {
    file: DatabaseFile,
    header: DatabaseHeader,
    cache: PageCache,
    state: PagerState,
    mode: OpenMode,
    max_database_bytes: u64,
}

impl Pager {
    pub(super) fn create(path: &Path, config: PagerConfig) -> Result<Self> {
        let file = DatabaseFile::create(
            path,
            config.page_size,
            config.uuid,
            config.max_database_bytes,
        )?;
        let header = file.header();
        let cache = PageCache::new(config.cache_pages)?;

        Ok(Self {
            file,
            header,
            cache,
            state: PagerState::Ready,
            mode: OpenMode::ReadWrite,
            max_database_bytes: config.max_database_bytes,
        })
    }

    pub(super) fn open(path: &Path, mode: OpenMode, config: PagerConfig) -> Result<Self> {
        let file = DatabaseFile::open(path, mode, config.max_database_bytes)?;
        let header = file.header();
        let cache = PageCache::new(config.cache_pages)?;

        Ok(Self {
            file,
            header,
            cache,
            state: PagerState::Ready,
            mode,
            max_database_bytes: config.max_database_bytes,
        })
    }

    pub(super) fn read(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.require_ready()?;

        if let Some(handle) = self.cache.get(reference.page_id()) {
            if handle.borrow().header().generation() == reference.generation() {
                return Ok(handle);
            }
            self.cache.remove(reference.page_id());
        }

        let frame = PageFrame::decode(
            self.file.read_page(reference.page_id())?,
            reference.page_id(),
            None,
            Some(reference.generation()),
        )?;
        let handle = Rc::new(RefCell::new(frame));
        self.cache.insert_clean(handle.clone())?;
        Ok(handle)
    }

    pub(super) fn begin_write(&mut self) -> Result<WriteBatch<'_>> {
        self.require_ready()?;
        if self.mode == OpenMode::ReadOnly {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "read-only pager cannot begin a write batch",
            ));
        }

        Ok(WriteBatch {
            pager: self,
            dirty: BTreeMap::new(),
            new_pages: BTreeSet::new(),
            pending_generations: BTreeMap::new(),
            pending_releases: BTreeSet::new(),
            candidate_header: None,
            finished: false,
        })
    }

    fn require_ready(&self) -> Result<()> {
        if self.state == PagerState::Poisoned {
            return Err(DbError::new(
                ErrorCode::Io,
                "pager is poisoned after failed file I/O; close and reopen the database",
            ));
        }
        Ok(())
    }

    pub(super) const fn page_count(&self) -> u64 {
        self.file.page_count()
    }

    pub(super) const fn page_size(&self) -> u32 {
        self.header.page_size()
    }

    pub(super) fn is_poisoned(&self) -> bool {
        self.state == PagerState::Poisoned
    }
}

pub(super) trait PageSource {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle>;
}

impl PageSource for Pager {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.read(reference)
    }
}

pub(super) struct WriteBatch<'a> {
    pager: &'a mut Pager,
    dirty: BTreeMap<PageId, PageFrame>,
    new_pages: BTreeSet<PageId>,
    pending_generations: BTreeMap<PageId, u64>,
    pending_releases: BTreeSet<PageId>,
    candidate_header: Option<DatabaseHeader>,
    finished: bool,
}

impl WriteBatch<'_> {
    pub(super) fn read(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.require_active()?;

        if let Some(frame) = self.dirty.get_mut(&reference.page_id()) {
            if frame.header().generation() != reference.generation() {
                return Err(corruption(
                    "page frame generation does not match the requested page",
                ));
            }
            return private_handle(frame);
        }

        self.pager.read(reference)
    }

    pub(super) fn modify(&mut self, reference: PageRef) -> Result<&mut PageFrame> {
        self.require_active()?;
        if !self.dirty.contains_key(&reference.page_id()) {
            let handle = self.pager.read(reference)?;
            let mut source = handle.borrow_mut();
            self.dirty
                .insert(reference.page_id(), private_frame(&mut source)?);
        }
        let frame = self
            .dirty
            .get_mut(&reference.page_id())
            .ok_or_else(|| corruption("page is not a batch-local allocation"))?;
        if frame.header().generation() != reference.generation() {
            return Err(corruption(
                "page frame generation does not match the requested page",
            ));
        }
        Ok(frame)
    }

    pub(super) fn allocate_append(&mut self, page_type: PageType) -> Result<PageRef> {
        self.require_active()?;

        let allocated = u64::try_from(self.new_pages.len())
            .map_err(|_| corruption("new page count exceeds u64"))?;
        let page_id = self
            .pager
            .file
            .page_count()
            .checked_add(allocated)
            .ok_or_else(|| corruption("page id overflow while allocating"))?;
        let reference = PageRef::new(PageId(page_id), 1)?;
        let next_page_count = page_id
            .checked_add(1)
            .ok_or_else(|| corruption("page count overflow while allocating"))?;
        let bytes = next_page_count
            .checked_mul(u64::from(self.pager.header.page_size()))
            .ok_or_else(|| corruption("database length overflow while allocating"))?;
        if bytes > self.pager.max_database_bytes {
            return Err(corruption(
                "allocation exceeds the configured maximum database size",
            ));
        }
        let frame = PageFrame::new_zeroed(page_type, reference, self.pager.header.page_size())?;
        self.dirty.insert(reference.page_id(), frame);
        self.new_pages.insert(reference.page_id());
        self.pending_generations
            .insert(reference.page_id(), reference.generation());
        Ok(reference)
    }

    pub(super) fn rollback(&mut self) -> Result<()> {
        self.require_active()?;
        self.finish();
        Ok(())
    }

    pub(super) fn set_catalog_root(&mut self, root: PageRef) -> Result<()> {
        self.require_active()?;
        self.read(root)?;

        let mut header = self.candidate_header.unwrap_or(self.pager.header);
        header.set_catalog_root(root.page_id());
        self.candidate_header = Some(header);
        Ok(())
    }

    pub(super) fn release_later(&mut self, reference: PageRef) -> Result<()> {
        self.require_active()?;
        self.read(reference)?;
        if !self.pending_releases.insert(reference.page_id()) {
            return Err(corruption("page is already pending release in this batch"));
        }
        Ok(())
    }

    pub(super) fn commit(&mut self) -> Result<()> {
        self.require_active()?;
        self.validate_for_commit()?;

        if self.dirty.is_empty() && self.candidate_header.is_none() {
            self.finish();
            return Ok(());
        }

        let target_page_count = self.target_page_count()?;
        self.pager.state = PagerState::Poisoned;
        self.pager.file.extend_to_pages(target_page_count)?;
        for (page_id, frame) in &self.dirty {
            self.pager.file.write_page(*page_id, frame.bytes())?;
        }
        self.pager.file.sync_all()?;
        let header = self.candidate_header.unwrap_or(self.pager.header);
        self.pager.file.write_header(header)?;
        self.pager.file.sync_all()?;

        self.pager.header = header;
        self.replace_cache_images();
        self.pager.state = PagerState::Ready;
        self.finish();
        Ok(())
    }

    fn validate_for_commit(&mut self) -> Result<()> {
        let target_page_count = self.target_page_count()?;
        let bytes = target_page_count
            .checked_mul(u64::from(self.pager.header.page_size()))
            .ok_or_else(|| corruption("database length overflow while committing"))?;
        if bytes > self.pager.max_database_bytes {
            return Err(corruption(
                "commit exceeds the configured maximum database size",
            ));
        }

        for (page_id, frame) in &mut self.dirty {
            if frame.header().page_id() != *page_id {
                return Err(corruption(
                    "dirty page image key does not match its page header",
                ));
            }
            if let Some(generation) = self.pending_generations.get(page_id)
                && frame.header().generation() != *generation
            {
                return Err(corruption(
                    "new page image generation does not match its allocation",
                ));
            }
            frame.prepare_for_write()?;
            PageFrame::decode(
                frame.bytes().to_vec(),
                *page_id,
                Some(frame.header().page_type()),
                Some(frame.header().generation()),
            )?;
        }
        Ok(())
    }

    fn target_page_count(&self) -> Result<u64> {
        match self.new_pages.last() {
            Some(page_id) => page_id
                .0
                .checked_add(1)
                .ok_or_else(|| corruption("page count overflow while committing")),
            None => Ok(self.pager.file.page_count()),
        }
    }

    fn replace_cache_images(&mut self) {
        for (page_id, frame) in std::mem::take(&mut self.dirty) {
            self.pager.cache.remove(page_id);
            let handle = Rc::new(RefCell::new(frame));
            let _ = self.pager.cache.insert_clean(handle);
        }
    }

    fn require_active(&self) -> Result<()> {
        if self.finished {
            return Err(DbError::new(
                ErrorCode::Transaction,
                "write batch has already finished",
            ));
        }
        self.pager.require_ready()
    }

    fn finish(&mut self) {
        self.dirty.clear();
        self.new_pages.clear();
        self.pending_generations.clear();
        self.pending_releases.clear();
        self.candidate_header = None;
        self.finished = true;
    }
}

impl PageSource for WriteBatch<'_> {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.read(reference)
    }
}

fn private_handle(frame: &mut PageFrame) -> Result<PageHandle> {
    Ok(Rc::new(RefCell::new(private_frame(frame)?)))
}

fn private_frame(frame: &mut PageFrame) -> Result<PageFrame> {
    frame.prepare_for_write()?;
    PageFrame::decode(
        frame.bytes().to_vec(),
        frame.header().page_id(),
        Some(frame.header().page_type()),
        Some(frame.header().generation()),
    )
}

fn corruption(message: impl Into<String>) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::{
        rc::Rc,
        sync::{Arc, Mutex},
    };

    use super::{PageSource, Pager, PagerConfig};
    use crate::{
        DbError, ErrorCode, Result,
        fault::{FaultInjector, FaultPoint},
    };

    use super::super::{
        PageId, PageRef,
        file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode},
        page::PageType,
        test_support::{TempDatabase, test_pager},
    };

    #[test]
    fn uncommitted_dirty_pages_are_invisible_after_rollback() {
        let temp = TempDatabase::new("pager-private");
        let mut pager = Pager::create(temp.path(), PagerConfig::new(4096, [1; 16]).unwrap())
            .expect("create pager");
        let root;

        {
            let mut batch = pager.begin_write().expect("begin write");
            root = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate append");
            batch
                .modify(root)
                .expect("modify private image")
                .bytes_mut()[100] = 7;
            batch.rollback().expect("rollback");
        }

        assert_eq!(pager.read(root).unwrap_err().code(), ErrorCode::Corruption);
    }

    #[test]
    fn batch_reads_its_own_private_image_before_commit() {
        let temp = TempDatabase::new("pager-overlay");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let mut batch = pager.begin_write().expect("begin write");
        let page = batch
            .allocate_append(PageType::TableLeaf)
            .expect("allocate append");
        batch
            .modify(page)
            .expect("modify private image")
            .bytes_mut()[100] = 0x7b;

        assert_eq!(
            batch
                .read(page)
                .expect("batch overlay read")
                .borrow()
                .bytes()[100],
            0x7b
        );
        batch.rollback().expect("rollback");
    }

    #[test]
    fn commit_publishes_dirty_page_and_replaces_clean_cache() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate append");
            batch
                .modify(page)
                .expect("modify private image")
                .bytes_mut()[100] = 0x2a;
            batch.commit().expect("commit");
            page
        };

        assert_eq!(pager.page_count(), 2);
        assert_eq!(pager.cache.len(), 1);
        let first = pager.read(page).expect("read committed page");
        assert_eq!(first.borrow().bytes()[100], 0x2a);
        let second = pager.read(page).expect("read cached page");
        assert!(Rc::ptr_eq(&first, &second));
    }

    #[test]
    fn modifying_a_committed_page_keeps_existing_clean_handles_unchanged() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin first write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate append");
            batch.modify(page).expect("modify first image").bytes_mut()[100] = 0x11;
            batch.commit().expect("commit first image");
            page
        };
        let clean_handle = pager.read(page).expect("pin clean page");

        {
            let mut batch = pager.begin_write().expect("begin second write");
            batch.modify(page).expect("copy committed page").bytes_mut()[100] = 0x22;
            assert_eq!(
                batch.read(page).expect("read dirty copy").borrow().bytes()[100],
                0x22
            );
            batch.commit().expect("commit dirty copy");
        }

        assert_eq!(clean_handle.borrow().bytes()[100], 0x11);
        let current_handle = pager.read(page).expect("read replacement image");
        assert_eq!(current_handle.borrow().bytes()[100], 0x22);
        assert!(!Rc::ptr_eq(&clean_handle, &current_handle));
    }

    #[test]
    fn read_only_open_allows_reads_and_rejects_write_batches() {
        let temp = TempDatabase::new("pager-read-only");
        let mut writable = Pager::create(temp.path(), PagerConfig::test_default(4096))
            .expect("create writable pager");
        let page = {
            let mut batch = writable.begin_write().expect("begin write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate append");
            batch.modify(page).expect("modify page").bytes_mut()[100] = 0x33;
            batch.commit().expect("commit page");
            page
        };
        drop(writable);

        let mut read_only = Pager::open(
            temp.path(),
            super::super::file::OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .expect("open read only");

        assert_eq!(
            read_only
                .read(page)
                .expect("read committed page")
                .borrow()
                .bytes()[100],
            0x33
        );
        let error = match read_only.begin_write() {
            Ok(_) => panic!("read-only pager must reject writes"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Unsupported);
    }

    #[test]
    fn invalid_catalog_root_rolls_back_without_poisoning_the_pager() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let mut batch = pager.begin_write().expect("begin write");

        assert_eq!(
            batch
                .set_catalog_root(PageRef::new(PageId(999), 1).expect("reference"))
                .expect_err("out-of-range catalog root")
                .code(),
            ErrorCode::Corruption
        );
        batch.rollback().expect("rollback logical failure");
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn commit_persists_a_catalog_root_after_its_page_is_durable() {
        let temp = TempDatabase::new("pager-catalog-root");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let root = batch
                .allocate_append(PageType::Catalog)
                .expect("allocate catalog page");
            batch.set_catalog_root(root).expect("set catalog root");
            batch.commit().expect("commit catalog root");
            root
        };
        drop(pager);

        let reopened = Pager::open(
            temp.path(),
            super::super::file::OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .expect("reopen pager");
        assert_eq!(reopened.header.catalog_root(), root.page_id());
    }

    #[test]
    fn release_later_requires_a_visible_page_and_rejects_duplicates() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin first write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate page");
            batch.commit().expect("commit page");
            page
        };
        let mut batch = pager.begin_write().expect("begin release batch");

        batch.release_later(page).expect("defer release");
        assert_eq!(
            batch
                .release_later(page)
                .expect_err("duplicate deferred release")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            batch
                .release_later(PageRef::new(PageId(999), 1).expect("reference"))
                .expect_err("out-of-range deferred release")
                .code(),
            ErrorCode::Corruption
        );
        batch.rollback().expect("rollback release batch");
    }

    #[test]
    fn page_source_reads_committed_pages_and_batch_overlays() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin initial batch");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate page");
            batch.modify(page).expect("modify initial page").bytes_mut()[100] = 0x44;
            batch.commit().expect("commit initial page");
            page
        };

        assert_eq!(read_source_byte(&mut pager, page), 0x44);
        let mut batch = pager.begin_write().expect("begin overlay batch");
        batch.modify(page).expect("modify overlay page").bytes_mut()[100] = 0x55;
        assert_eq!(read_source_byte(&mut batch, page), 0x55);
        batch.rollback().expect("rollback overlay batch");
    }

    fn read_source_byte(source: &mut impl PageSource, reference: PageRef) -> u8 {
        source
            .read_page(reference)
            .expect("read page source")
            .borrow()
            .bytes()[100]
    }

    #[test]
    fn append_rejects_pages_beyond_the_configured_limit_before_file_changes() {
        let temp = TempDatabase::new("pager-limit");
        let mut config = PagerConfig::test_default(4096);
        config.max_database_bytes = 4096;
        let mut pager = Pager::create(temp.path(), config).expect("create limited pager");
        let initial_len = std::fs::metadata(temp.path())
            .expect("database metadata")
            .len();
        let mut batch = pager.begin_write().expect("begin write");

        assert_eq!(
            batch
                .allocate_append(PageType::TableLeaf)
                .expect_err("append beyond configured maximum")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            std::fs::metadata(temp.path())
                .expect("database metadata after rejected append")
                .len(),
            initial_len
        );
        batch.rollback().expect("rollback rejected append");
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn pager_reports_the_opened_database_page_size() {
        let (_temp, pager) = test_pager(8192).expect("create 8192-byte pager");

        assert_eq!(pager.page_size(), 8192);
        assert_eq!(pager.page_count(), 1);
    }

    #[test]
    fn invalid_disk_pages_are_rejected_without_entering_the_cache() {
        let temp = TempDatabase::new("pager-invalid-page");
        let mut file =
            DatabaseFile::create(temp.path(), 4096, [0x6c; 16], DEFAULT_MAX_DATABASE_BYTES)
                .expect("create database file");
        file.extend_to_pages(2).expect("extend database file");
        file.write_page(PageId(1), &[0; 4096])
            .expect("write invalid page bytes");
        file.sync_all().expect("sync invalid page bytes");
        drop(file);

        let mut pager = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .expect("open pager");
        assert_eq!(
            pager
                .read(PageRef::new(PageId(1), 1).expect("page reference"))
                .expect_err("reject invalid disk page")
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(pager.cache.len(), 0);
    }

    #[test]
    fn commit_checks_fault_boundaries_in_deterministic_order() {
        let temp = TempDatabase::new("pager-fault-order");
        let faults = Arc::new(RecordingFaults::never());
        let mut pager = pager_with_faults(&temp, faults.clone());
        let mut batch = pager.begin_write().expect("begin write");
        batch
            .allocate_append(PageType::TableLeaf)
            .expect("allocate append");

        batch.commit().expect("commit");

        assert_eq!(faults.recorded(), commit_fault_points());
    }

    #[test]
    fn every_commit_fault_poisons_the_pager_after_the_file_operation_starts() {
        let fault_points = commit_fault_points();

        for occurrence in 1..=fault_points.len() {
            let temp = TempDatabase::new(&format!("pager-fault-{occurrence}"));
            let faults = Arc::new(RecordingFaults::at(occurrence));
            let mut pager = pager_with_faults(&temp, faults.clone());
            let page;
            {
                let mut batch = pager.begin_write().expect("begin write");
                page = batch
                    .allocate_append(PageType::TableLeaf)
                    .expect("allocate append");
                let error = match batch.commit() {
                    Ok(()) => panic!("fault occurrence {occurrence} must fail commit"),
                    Err(error) => error,
                };
                assert_eq!(error.code(), ErrorCode::Io);
            }

            assert_eq!(faults.recorded(), fault_points[..occurrence]);
            assert!(pager.is_poisoned());
            assert_eq!(
                pager.read(page).expect_err("poisoned read").code(),
                ErrorCode::Io
            );
            let write_error = match pager.begin_write() {
                Ok(_) => panic!("poisoned pager must reject write batches"),
                Err(error) => error,
            };
            assert_eq!(write_error.code(), ErrorCode::Io);
            assert!(write_error.to_string().contains("reopen"));
        }
    }

    fn pager_with_faults(temp: &TempDatabase, faults: Arc<dyn FaultInjector>) -> Pager {
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        pager.file =
            DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)
                .expect("reopen file for faults")
                .with_faults(faults);
        pager
    }

    fn commit_fault_points() -> Vec<FaultPoint> {
        vec![
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileSync,
            FaultPoint::AfterFileSync,
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileSync,
            FaultPoint::AfterFileSync,
        ]
    }

    struct RecordingFaults {
        fail_at: Option<usize>,
        points: Mutex<Vec<FaultPoint>>,
    }

    impl RecordingFaults {
        fn never() -> Self {
            Self {
                fail_at: None,
                points: Mutex::new(Vec::new()),
            }
        }

        fn at(occurrence: usize) -> Self {
            Self {
                fail_at: Some(occurrence),
                points: Mutex::new(Vec::new()),
            }
        }

        fn recorded(&self) -> Vec<FaultPoint> {
            self.points.lock().expect("fault record lock").clone()
        }
    }

    impl FaultInjector for RecordingFaults {
        fn check(&self, point: FaultPoint) -> Result<()> {
            let mut points = self.points.lock().expect("fault record lock");
            points.push(point);
            if self.fail_at == Some(points.len()) {
                return Err(DbError::new(ErrorCode::Io, "injected pager I/O fault"));
            }
            Ok(())
        }
    }
}
