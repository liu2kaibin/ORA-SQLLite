use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::Path,
    rc::Rc,
};

use super::{
    PageId, PageRef,
    btree::{
        INTERNAL_METADATA_END, InternalCell, LEAF_METADATA_END, LeafCell, decode_internal_leftmost,
        decode_leaf_right_sibling,
    },
    cache::{PageCache, PageHandle},
    file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode},
    freelist::{Freelist, FreelistWalk},
    header::DatabaseHeader,
    overflow,
    page::{PageFrame, PageType},
    slotted::SlottedPage,
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
        let cache = PageCache::new(config.cache_pages)?;
        let file = DatabaseFile::create(
            path,
            config.page_size,
            config.uuid,
            config.max_database_bytes,
        )?;
        let header = file.header();

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
        let committed_freelist = self.walk_freelist()?;

        Ok(WriteBatch {
            pager: self,
            dirty: BTreeMap::new(),
            new_pages: BTreeSet::new(),
            pending_generations: BTreeMap::new(),
            pending_releases: BTreeMap::new(),
            committed_freelist,
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

    fn read_frame(&mut self, page_id: PageId) -> Result<PageFrame> {
        self.require_ready()?;
        PageFrame::decode(self.file.read_page(page_id)?, page_id, None, None)
    }

    fn walk_freelist(&mut self) -> Result<FreelistWalk> {
        let root = self.header.freelist_root();
        let page_count = self.file.page_count();
        Freelist::walk(root, page_count, |page_id| self.read_frame(page_id))
    }
}

pub(super) trait PageSource {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle>;

    fn page_count(&self) -> u64;
}

impl PageSource for Pager {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.read(reference)
    }

    fn page_count(&self) -> u64 {
        self.page_count()
    }
}

pub(super) struct WriteBatch<'a> {
    pager: &'a mut Pager,
    dirty: BTreeMap<PageId, PageFrame>,
    new_pages: BTreeSet<PageId>,
    pending_generations: BTreeMap<PageId, u64>,
    pending_releases: BTreeMap<PageId, PageRef>,
    committed_freelist: FreelistWalk,
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
        if self.pending_releases.contains_key(&reference.page_id()) {
            return Err(corruption(
                "released page cannot become dirty in the same batch",
            ));
        }
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

    pub(super) fn allocate(&mut self, page_type: PageType) -> Result<PageRef> {
        self.require_active()?;
        let root = self.current_header().freelist_root();
        if root.0 == 0 {
            return self.allocate_append(page_type);
        }

        let (root_reference, mut trunk) = {
            let frame = self.read_frame_by_id(root)?;
            (
                PageRef::new(root, frame.header().generation())?,
                Freelist::decode(&frame)?,
            )
        };

        if let Some(page_id) = trunk.leaves().last().copied() {
            let generation = self.next_generation(page_id)?;
            let allocated = Freelist::allocate(&mut trunk)
                .ok_or_else(|| corruption("freelist leaf disappeared before allocation"))?;
            debug_assert_eq!(allocated, page_id);
            Freelist::write(self.freelist_frame(root)?, &trunk)?;
            return self.allocate_reused(page_id, generation, page_type);
        }

        let generation = self.next_generation(root_reference.page_id())?;
        self.stage_freelist_root(trunk.next().map_or(PageId(0), PageRef::page_id));
        self.allocate_reused(root_reference.page_id(), generation, page_type)
    }

    pub(super) fn rollback(&mut self) -> Result<()> {
        self.require_active()?;
        self.finish();
        Ok(())
    }

    pub(super) fn set_catalog_root(&mut self, root: PageRef) -> Result<()> {
        self.require_active()?;
        if self.pending_releases.contains_key(&root.page_id()) {
            return Err(corruption(
                "released page cannot become the catalog root in the same batch",
            ));
        }
        self.read(root)?;

        let mut header = self.current_header();
        header.set_catalog_root(root.page_id());
        self.candidate_header = Some(header);
        Ok(())
    }

    pub(super) fn release_later(&mut self, reference: PageRef) -> Result<()> {
        self.require_active()?;
        if self.pending_releases.contains_key(&reference.page_id()) {
            return Err(corruption("page is already pending release in this batch"));
        }
        self.validate_releasable(reference)?;
        self.pending_releases.insert(reference.page_id(), reference);
        Ok(())
    }

    fn validate_releasable(&mut self, reference: PageRef) -> Result<()> {
        if reference.page_id().0 == 0 {
            return Err(corruption("database header cannot enter the freelist"));
        }
        if reference.page_id() == self.current_header().catalog_root() {
            return Err(corruption("active catalog root cannot enter the freelist"));
        }
        if self.dirty.contains_key(&reference.page_id())
            || self.new_pages.contains(&reference.page_id())
        {
            return Err(corruption("dirty live page cannot enter the freelist"));
        }
        if self.committed_freelist.contains_trunk(reference.page_id()) {
            return Err(corruption(
                "freelist trunk cannot enter the freelist as a leaf",
            ));
        }
        if self.committed_freelist.contains_leaf(reference.page_id()) {
            return Err(corruption(
                "page is already recorded in the committed freelist",
            ));
        }
        self.read(reference)?;
        Ok(())
    }

    pub(super) fn commit(mut self) -> Result<()> {
        self.require_active()?;
        self.prepare_freelist_releases()?;
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
            validate_page_body(frame)?;
            frame.prepare_for_write()?;
            PageFrame::decode(
                frame.bytes().to_vec(),
                *page_id,
                Some(frame.header().page_type()),
                Some(frame.header().generation()),
            )?;
        }
        self.validate_freelist()?;
        Ok(())
    }

    fn prepare_freelist_releases(&mut self) -> Result<()> {
        let releases = self.pending_releases.values().copied().collect::<Vec<_>>();
        for reference in &releases {
            self.validate_releasable(*reference)?;
        }
        for reference in releases {
            let page_id = reference.page_id();
            self.release_to_freelist(page_id)?;
        }
        Ok(())
    }

    fn release_to_freelist(&mut self, page_id: PageId) -> Result<()> {
        let released = self.read_frame_by_id(page_id)?;
        let released_reference = PageRef::new(page_id, released.header().generation())?;
        let root = self.current_header().freelist_root();

        if root.0 == 0 {
            let mut frame = PageFrame::new_zeroed(
                PageType::Freelist,
                released_reference,
                self.pager.header.page_size(),
            )?;
            Freelist::write(&mut frame, &Freelist::empty_trunk(None))?;
            self.dirty.insert(page_id, frame);
            self.stage_freelist_root(page_id);
            return Ok(());
        }

        let (root_reference, mut trunk) = {
            let frame = self.freelist_frame(root)?;
            (
                PageRef::new(root, frame.header().generation())?,
                Freelist::decode(frame)?,
            )
        };
        if Freelist::is_full(&trunk, self.pager.header.page_size())? {
            let mut frame = PageFrame::new_zeroed(
                PageType::Freelist,
                released_reference,
                self.pager.header.page_size(),
            )?;
            Freelist::write(&mut frame, &Freelist::empty_trunk(Some(root_reference)))?;
            self.dirty.insert(page_id, frame);
            self.stage_freelist_root(page_id);
        } else {
            Freelist::release(&mut trunk, page_id, self.pager.header.page_size())?;
            Freelist::write(self.freelist_frame(root)?, &trunk)?;
        }
        Ok(())
    }

    fn allocate_reused(
        &mut self,
        page_id: PageId,
        generation: u64,
        page_type: PageType,
    ) -> Result<PageRef> {
        let reference = PageRef::new(page_id, generation)?;
        let frame = PageFrame::new_zeroed(page_type, reference, self.pager.header.page_size())?;
        self.dirty.insert(page_id, frame);
        self.pending_generations.insert(page_id, generation);
        Ok(reference)
    }

    fn next_generation(&mut self, page_id: PageId) -> Result<u64> {
        self.read_frame_by_id(page_id)?
            .header()
            .generation()
            .checked_add(1)
            .ok_or_else(|| corruption("page generation overflow while reusing freelist page"))
    }

    fn validate_freelist(&mut self) -> Result<()> {
        let root = self.current_header().freelist_root();
        let page_count = self.pager.file.page_count();
        Freelist::walk(root, page_count, |page_id| self.read_frame_by_id(page_id)).map(|_| ())
    }

    fn freelist_frame(&mut self, page_id: PageId) -> Result<&mut PageFrame> {
        if !self.dirty.contains_key(&page_id) {
            let frame = self.read_frame_by_id(page_id)?;
            self.dirty.insert(page_id, frame);
        }
        let frame = self
            .dirty
            .get_mut(&page_id)
            .ok_or_else(|| corruption("freelist page is missing from the batch overlay"))?;
        if frame.header().page_type() != PageType::Freelist {
            return Err(corruption(
                "freelist root does not point to a freelist page",
            ));
        }
        Ok(frame)
    }

    fn read_frame_by_id(&mut self, page_id: PageId) -> Result<PageFrame> {
        if let Some(frame) = self.dirty.get_mut(&page_id) {
            return private_frame(frame);
        }
        self.pager.read_frame(page_id)
    }

    fn current_header(&self) -> DatabaseHeader {
        self.candidate_header.unwrap_or(self.pager.header)
    }

    fn stage_freelist_root(&mut self, root: PageId) {
        let mut header = self.current_header();
        header.set_freelist_root(root);
        self.candidate_header = Some(header);
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
        self.committed_freelist = FreelistWalk::default();
        self.candidate_header = None;
        self.finished = true;
    }
}

fn validate_page_body(frame: &mut PageFrame) -> Result<()> {
    match frame.header().page_type() {
        PageType::TableLeaf | PageType::IndexLeaf => validate_leaf_page(frame),
        PageType::TableInternal | PageType::IndexInternal => validate_internal_page(frame),
        PageType::Freelist => Freelist::decode(frame).map(|_| ()),
        PageType::Overflow => overflow::validate_local(frame),
        PageType::Catalog => Ok(()),
    }
}

fn validate_leaf_page(frame: &mut PageFrame) -> Result<()> {
    let cells = {
        let page = SlottedPage::open(frame, LEAF_METADATA_END)?;
        page.cells()?
    };
    decode_leaf_right_sibling(
        &frame.bytes()[LEAF_METADATA_END - 8..LEAF_METADATA_END],
        frame.header().right_sibling(),
    )?;
    for cell in cells {
        LeafCell::decode(&cell)?;
    }
    Ok(())
}

fn validate_internal_page(frame: &mut PageFrame) -> Result<()> {
    let cells = {
        let page = SlottedPage::open(frame, INTERNAL_METADATA_END)?;
        page.cells()?
    };
    decode_internal_leftmost(&frame.bytes()[INTERNAL_METADATA_END - 16..INTERNAL_METADATA_END])?;
    for cell in cells {
        InternalCell::decode(&cell)?;
    }
    Ok(())
}

impl PageSource for WriteBatch<'_> {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.read(reference)
    }

    fn page_count(&self) -> u64 {
        self.pager.page_count()
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
        btree::encode_internal_leftmost,
        file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode},
        freelist::Freelist,
        page::{PageFrame, PageType},
        slotted::SlottedPage,
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

        let mut dirty_batch = pager.begin_write().expect("begin dirty allocation batch");
        let dirty = dirty_batch
            .allocate(PageType::TableLeaf)
            .expect("allocate dirty page");
        assert_eq!(
            dirty_batch
                .release_later(dirty)
                .expect_err("dirty page cannot be released")
                .code(),
            ErrorCode::Corruption
        );
        dirty_batch
            .rollback()
            .expect("rollback dirty allocation batch");

        {
            let mut release_batch = pager.begin_write().expect("begin committed release batch");
            release_batch
                .release_later(page)
                .expect("release committed page");
            release_batch.commit().expect("commit freelist trunk");
        }
        let mut trunk_batch = pager.begin_write().expect("begin trunk release batch");
        assert_eq!(
            trunk_batch
                .release_later(page)
                .expect_err("freelist trunk cannot be released")
                .code(),
            ErrorCode::Corruption
        );
        trunk_batch
            .rollback()
            .expect("rollback trunk release batch");
    }

    #[test]
    fn release_later_rejects_the_active_catalog_root() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin catalog batch");
            let root = batch
                .allocate(PageType::Catalog)
                .expect("allocate catalog root");
            batch.set_catalog_root(root).expect("set catalog root");
            batch.commit().expect("commit catalog root");
            root
        };

        let mut batch = pager.begin_write().expect("begin catalog release batch");
        assert_eq!(
            batch
                .release_later(root)
                .expect_err("active catalog root cannot be released")
                .code(),
            ErrorCode::Corruption
        );
        batch.rollback().expect("rollback catalog release batch");
    }

    #[test]
    fn release_then_modify_rejects_a_live_and_free_page_before_file_io() {
        let temp = TempDatabase::new("pager-release-then-modify");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin allocation batch");
            let page = batch.allocate(PageType::TableLeaf).expect("allocate page");
            batch.commit().expect("commit page");
            page
        };
        let before = std::fs::read(temp.path()).expect("read database before rejected overlap");

        {
            let mut batch = pager.begin_write().expect("begin overlap batch");
            batch.release_later(page).expect("defer page release");
            assert_eq!(
                batch
                    .modify(page)
                    .expect_err("released page cannot become dirty")
                    .code(),
                ErrorCode::Corruption
            );
            batch.rollback().expect("rollback rejected overlap batch");
        }

        assert_file_is_unchanged(&temp, &before);
        pager
            .read(page)
            .expect("original live page remains readable");
    }

    #[test]
    fn release_then_set_catalog_root_rejects_a_live_and_free_page_before_file_io() {
        let temp = TempDatabase::new("pager-release-then-catalog-root");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin allocation batch");
            let page = batch
                .allocate(PageType::Catalog)
                .expect("allocate catalog page");
            batch.commit().expect("commit page");
            page
        };
        let before = std::fs::read(temp.path()).expect("read database before rejected overlap");

        {
            let mut batch = pager.begin_write().expect("begin overlap batch");
            batch.release_later(page).expect("defer page release");
            assert_eq!(
                batch
                    .set_catalog_root(page)
                    .expect_err("released page cannot become catalog root")
                    .code(),
                ErrorCode::Corruption
            );
            batch.rollback().expect("rollback rejected overlap batch");
        }

        assert_file_is_unchanged(&temp, &before);
        assert_eq!(pager.header.catalog_root(), PageId(0));
    }

    #[test]
    fn leaf_generation_overflow_preserves_the_batch_freelist_state() {
        let temp = TempDatabase::new("pager-freelist-leaf-generation-overflow");
        let mut pager = pager_with_freelist(&temp, 1, &[u64::MAX]);
        let before = std::fs::read(temp.path()).expect("read database before rejected reuse");
        let mut batch = pager.begin_write().expect("begin overflow batch");

        for _ in 0..2 {
            assert_eq!(
                batch
                    .allocate(PageType::TableLeaf)
                    .expect_err("generation overflow must reject allocation")
                    .code(),
                ErrorCode::Corruption
            );
        }
        batch.rollback().expect("rollback overflow batch");

        assert_file_is_unchanged(&temp, &before);
        assert_eq!(pager.header.freelist_root(), PageId(1));
    }

    #[test]
    fn empty_root_generation_overflow_preserves_the_staged_root() {
        let temp = TempDatabase::new("pager-freelist-root-generation-overflow");
        let mut pager = pager_with_freelist(&temp, u64::MAX, &[]);
        let before = std::fs::read(temp.path()).expect("read database before rejected reuse");
        let mut batch = pager.begin_write().expect("begin overflow batch");

        for _ in 0..2 {
            assert_eq!(
                batch
                    .allocate(PageType::TableLeaf)
                    .expect_err("generation overflow must reject allocation")
                    .code(),
                ErrorCode::Corruption
            );
        }
        batch.rollback().expect("rollback overflow batch");

        assert_file_is_unchanged(&temp, &before);
        assert_eq!(pager.header.freelist_root(), PageId(1));
    }

    #[test]
    fn full_root_promotes_released_page_and_empty_root_is_allocated() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let capacity = Freelist::leaf_capacity(4096).expect("freelist leaf capacity");
        let refs = {
            let mut batch = pager.begin_write().expect("begin allocation batch");
            let refs = (0..=capacity)
                .map(|_| batch.allocate(PageType::TableLeaf).expect("allocate page"))
                .collect::<Vec<_>>();
            batch.commit().expect("commit allocations");
            refs
        };
        {
            let mut batch = pager.begin_write().expect("begin release batch");
            for reference in &refs {
                batch.release_later(*reference).expect("release page");
            }
            batch.commit().expect("commit releases");
        }

        let mut next = pager.begin_write().expect("begin reuse batch");
        let reused = next.allocate(PageType::TableLeaf).expect("reuse page");

        assert!(refs.iter().any(|old| old.page_id() == reused.page_id()));
        assert_eq!(reused.generation(), 2);
    }

    #[test]
    fn release_after_a_full_root_promotes_an_empty_trunk() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let capacity = Freelist::leaf_capacity(4096).expect("freelist leaf capacity");
        let refs = {
            let mut batch = pager.begin_write().expect("begin allocation batch");
            let refs = (0..capacity + 2)
                .map(|_| batch.allocate(PageType::TableLeaf).expect("allocate page"))
                .collect::<Vec<_>>();
            batch.commit().expect("commit allocations");
            refs
        };
        {
            let mut batch = pager.begin_write().expect("begin release batch");
            for reference in &refs {
                batch.release_later(*reference).expect("release page");
            }
            batch.commit().expect("commit releases");
        }

        let promoted_root = *refs.last().expect("allocated pages");
        assert_eq!(pager.header.freelist_root(), promoted_root.page_id());
        let first = {
            let mut batch = pager.begin_write().expect("begin root reuse batch");
            let page = batch
                .allocate(PageType::TableLeaf)
                .expect("allocate empty root");
            batch.commit().expect("commit root reuse");
            page
        };

        assert_eq!(first.page_id(), promoted_root.page_id());
        assert_eq!(first.generation(), 2);
        assert_eq!(pager.header.freelist_root(), refs[0].page_id());
        let second = {
            let mut batch = pager.begin_write().expect("begin leaf reuse batch");
            let page = batch
                .allocate(PageType::TableLeaf)
                .expect("allocate root leaf");
            batch.rollback().expect("rollback leaf reuse");
            page
        };
        assert_eq!(second.page_id(), refs[capacity].page_id());
        assert_eq!(second.generation(), 2);
    }

    #[test]
    fn releases_are_reused_in_a_later_batch_without_growing_the_file() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let original = {
            let mut batch = pager.begin_write().expect("begin first allocation batch");
            let pages = (0..100)
                .map(|_| batch.allocate(PageType::TableLeaf).expect("allocate page"))
                .collect::<Vec<_>>();
            batch.commit().expect("commit first allocation batch");
            pages
        };
        let initial_page_count = pager.page_count();

        {
            let mut batch = pager.begin_write().expect("begin release batch");
            for reference in original.iter().rev() {
                batch.release_later(*reference).expect("defer release");
            }
            batch.commit().expect("commit releases");
        }

        let replacement = {
            let mut batch = pager.begin_write().expect("begin reuse batch");
            let pages = (0..100)
                .map(|_| batch.allocate(PageType::TableLeaf).expect("reuse page"))
                .collect::<Vec<_>>();
            batch.commit().expect("commit reused pages");
            pages
        };

        assert_eq!(pager.page_count(), initial_page_count);
        for old in &original {
            assert_eq!(
                pager
                    .read(*old)
                    .expect_err("stale reference must fail")
                    .code(),
                ErrorCode::Corruption
            );
        }
        for current in &replacement {
            assert_eq!(current.generation(), 2);
            pager.read(*current).expect("replacement must be readable");
        }
    }

    #[test]
    fn pending_release_cannot_be_reused_by_its_own_batch() {
        let (_temp, mut pager) = test_pager(4096).expect("create test pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin first allocation batch");
            let page = batch.allocate(PageType::TableLeaf).expect("allocate page");
            batch.commit().expect("commit page");
            page
        };

        let mut batch = pager
            .begin_write()
            .expect("begin release and allocate batch");
        batch.release_later(page).expect("defer release");
        let allocated = batch
            .allocate(PageType::TableLeaf)
            .expect("append allocation");

        assert_ne!(allocated.page_id(), page.page_id());
    }

    #[test]
    fn malformed_freelist_body_rejects_commit_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-freelist");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin allocation batch");
            let page = batch.allocate(PageType::TableLeaf).expect("allocate page");
            batch.commit().expect("commit page");
            page
        };
        {
            let mut batch = pager.begin_write().expect("begin release batch");
            batch.release_later(page).expect("release page");
            batch.commit().expect("commit release");
        }
        let before = std::fs::read(temp.path()).expect("read database before malformed commit");
        let error = {
            let mut batch = pager.begin_write().expect("begin invalid freelist batch");
            batch
                .modify(page)
                .expect("modify freelist trunk")
                .bytes_mut()[68..72]
                .copy_from_slice(&1u32.to_le_bytes());
            match batch.commit() {
                Ok(()) => panic!("nonzero freelist reserved field must reject commit"),
                Err(error) => error,
            }
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_file_is_unchanged(&temp, &before);
        assert!(!pager.is_poisoned());
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

    #[test]
    fn invalid_leaf_sibling_metadata_fails_before_file_io() {
        for page_type in [PageType::TableLeaf, PageType::IndexLeaf] {
            let temp = TempDatabase::new("pager-invalid-leaf-metadata");
            let mut pager =
                Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
            let before = std::fs::read(temp.path()).expect("read database before commit");
            let error = {
                let mut batch = pager.begin_write().expect("begin write");
                let page = batch.allocate_append(page_type).expect("allocate page");
                batch.modify(page).expect("modify page").bytes_mut()[48..56]
                    .copy_from_slice(&1u64.to_le_bytes());
                match batch.commit() {
                    Ok(()) => panic!("invalid leaf sibling metadata must reject commit"),
                    Err(error) => error,
                }
            };

            assert_eq!(error.code(), ErrorCode::Corruption);
            assert_file_is_unchanged(&temp, &before);
            assert!(!pager.is_poisoned());
        }
    }

    #[test]
    fn malformed_leaf_layout_fails_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-leaf-layout");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = std::fs::read(temp.path()).expect("read database before commit");
        let error = {
            let mut batch = pager.begin_write().expect("begin write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate leaf");
            let frame = batch.modify(page).expect("modify leaf");
            frame.set_layout(1, 60, 4095).expect("set bounded layout");
            frame.bytes_mut()[56..60].copy_from_slice(&[50, 0, 1, 0]);
            match batch.commit() {
                Ok(()) => panic!("malformed leaf layout must reject commit"),
                Err(error) => error,
            }
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_file_is_unchanged(&temp, &before);
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn malformed_leaf_cell_fails_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-leaf-cell");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = std::fs::read(temp.path()).expect("read database before commit");
        let error = {
            let mut batch = pager.begin_write().expect("begin write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate leaf");
            {
                let frame = batch.modify(page).expect("modify leaf");
                let mut slotted = SlottedPage::open(frame, 56).expect("open empty leaf");
                slotted.insert(0, &[0]).expect("insert raw malformed cell");
            }
            match batch.commit() {
                Ok(()) => panic!("malformed leaf cell must reject commit"),
                Err(error) => error,
            }
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_file_is_unchanged(&temp, &before);
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn invalid_internal_leftmost_metadata_fails_before_file_io() {
        for page_type in [PageType::TableInternal, PageType::IndexInternal] {
            let temp = TempDatabase::new("pager-invalid-internal-metadata");
            let mut pager =
                Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
            let before = std::fs::read(temp.path()).expect("read database before commit");
            let error = {
                let mut batch = pager.begin_write().expect("begin write");
                batch.allocate_append(page_type).expect("allocate page");
                match batch.commit() {
                    Ok(()) => panic!("invalid internal leftmost metadata must reject commit"),
                    Err(error) => error,
                }
            };

            assert_eq!(error.code(), ErrorCode::Corruption);
            assert_file_is_unchanged(&temp, &before);
            assert!(!pager.is_poisoned());
        }
    }

    #[test]
    fn malformed_internal_cell_fails_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-internal-cell");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = std::fs::read(temp.path()).expect("read database before commit");
        let error = {
            let mut batch = pager.begin_write().expect("begin write");
            let page = batch
                .allocate_append(PageType::TableInternal)
                .expect("allocate internal page");
            {
                let frame = batch.modify(page).expect("modify internal page");
                frame.bytes_mut()[48..64].copy_from_slice(&encode_internal_leftmost(
                    PageRef::new(PageId(1), 1).expect("leftmost reference"),
                ));
                let mut slotted = SlottedPage::open(frame, 64).expect("open internal page");
                slotted.insert(0, &[0]).expect("insert raw malformed cell");
            }
            match batch.commit() {
                Ok(()) => panic!("malformed internal cell must reject commit"),
                Err(error) => error,
            }
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_file_is_unchanged(&temp, &before);
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn cache_configuration_failure_does_not_create_a_database_file() {
        let temp = TempDatabase::new("pager-cache-config");
        let mut invalid_config = PagerConfig::test_default(4096);
        invalid_config.cache_pages = 0;

        let error = match Pager::create(temp.path(), invalid_config) {
            Ok(_) => panic!("zero cache capacity must reject pager creation"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Type);
        assert!(!temp.path().exists());

        let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
            .expect("retry after rejected pager creation");
        assert_eq!(pager.page_count(), 1);
    }

    #[test]
    fn dropping_an_unfinished_batch_leaves_the_file_unchanged() {
        let temp = TempDatabase::new("pager-drop-batch");
        let mut pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = std::fs::read(temp.path()).expect("read database before batch");
        let page;
        {
            let mut batch = pager.begin_write().expect("begin write");
            page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate append");
            batch
                .modify(page)
                .expect("modify private image")
                .bytes_mut()[100] = 0x5c;
        }

        assert_file_is_unchanged(&temp, &before);
        assert_eq!(
            pager
                .read(page)
                .expect_err("dropped page is invisible")
                .code(),
            ErrorCode::Corruption
        );
        assert!(!pager.is_poisoned());
    }

    fn assert_file_is_unchanged(temp: &TempDatabase, before: &[u8]) {
        let after = std::fs::read(temp.path()).expect("read database after rejected commit");
        assert_eq!(after, before);
        assert_eq!(
            std::fs::metadata(temp.path())
                .expect("database metadata after rejected commit")
                .len(),
            u64::try_from(before.len()).expect("test database length fits u64")
        );
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

    fn pager_with_freelist(
        temp: &TempDatabase,
        root_generation: u64,
        leaf_generations: &[u64],
    ) -> Pager {
        let page_count = u64::try_from(leaf_generations.len())
            .expect("leaf count fits u64")
            .checked_add(2)
            .expect("page count fits u64");
        let mut file =
            DatabaseFile::create(temp.path(), 4096, [0x8a; 16], DEFAULT_MAX_DATABASE_BYTES)
                .expect("create database file");
        file.extend_to_pages(page_count)
            .expect("extend database file");

        let mut trunk = Freelist::empty_trunk(None);
        for (index, generation) in leaf_generations.iter().copied().enumerate() {
            let page_id = PageId(u64::try_from(index).expect("leaf index fits u64") + 2);
            let reference = PageRef::new(page_id, generation).expect("leaf reference");
            let mut leaf =
                PageFrame::new_zeroed(PageType::TableLeaf, reference, 4096).expect("leaf frame");
            leaf.prepare_for_write().expect("prepare leaf frame");
            file.write_page(page_id, leaf.bytes())
                .expect("write leaf page");
            Freelist::release(&mut trunk, page_id, 4096).expect("append freelist leaf");
        }

        let root = PageRef::new(PageId(1), root_generation).expect("freelist root reference");
        let mut root_frame =
            PageFrame::new_zeroed(PageType::Freelist, root, 4096).expect("freelist root frame");
        Freelist::write(&mut root_frame, &trunk).expect("encode freelist root");
        root_frame
            .prepare_for_write()
            .expect("prepare freelist root");
        file.write_page(PageId(1), root_frame.bytes())
            .expect("write freelist root");
        file.sync_all().expect("sync freelist pages");

        let mut header = file.header();
        header.set_freelist_root(PageId(1));
        file.write_header(header).expect("publish freelist root");
        file.sync_all().expect("sync database header");
        drop(file);

        Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .expect("open pager with freelist")
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
