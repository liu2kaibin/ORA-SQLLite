use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, RwLock, TryLockError},
};

use super::{
    Lsn, PageId, PageRef,
    btree::{
        BTree, INTERNAL_METADATA_END, InternalCell, LEAF_METADATA_END, LeafCell, TreeKind,
        decode_internal_leftmost, decode_leaf_right_sibling,
    },
    cache::{CacheKey, PageCache, PageHandle},
    features::validate_feature_bits,
    file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode, checked_page_offset},
    freelist::{Freelist, FreelistWalk},
    header::DatabaseHeader,
    overflow,
    page::{PageFrame, PageType},
    slotted::SlottedPage,
    snapshot::{SnapshotLease, SnapshotRegistry},
    validate::{ValidationReport, validate_freelist, validate_tree},
    wal::{Wal, format::CommitState, index::WalIndex},
};
use crate::{DbError, ErrorCode, Result};

use crate::fault::FaultInjector;

pub(crate) struct PagerConfig {
    pub(super) page_size: u32,
    pub(super) uuid: [u8; 16],
    pub(super) max_database_bytes: u64,
    pub(super) cache_pages: usize,
    pub(super) checkpoint_wal_bytes: u64,
    pub(super) max_wal_bytes: u64,
}

impl PagerConfig {
    pub(super) fn new(page_size: u32, uuid: [u8; 16]) -> Result<Self> {
        DatabaseHeader::new(page_size, uuid)?;
        Ok(Self {
            page_size,
            uuid,
            max_database_bytes: DEFAULT_MAX_DATABASE_BYTES,
            cache_pages: 1024,
            checkpoint_wal_bytes: 64 * 1024 * 1024,
            max_wal_bytes: super::wal::DEFAULT_MAX_WAL_BYTES,
        })
    }

    fn validate_wal_limits(&self) -> Result<()> {
        if self.checkpoint_wal_bytes == 0 || self.max_wal_bytes == 0 {
            return Err(DbError::new(ErrorCode::Type, "WAL limits must be positive"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Pager {
    inner: Arc<PagerInner>,
}

pub(super) struct PagerInner {
    pub(super) main: Mutex<DatabaseFile>,
    pub(super) wal: Mutex<Wal>,
    pub(super) published: RwLock<PublishedState>,
    pub(super) snapshots: SnapshotRegistry,
    pub(super) cache: Mutex<PageCache>,
    pub(super) cache_pages: usize,
    writer: Mutex<()>,
    mode: OpenMode,
    max_database_bytes: u64,
    max_wal_bytes: u64,
    checkpoint_wal_bytes: u64,
    #[cfg(test)]
    reused_page_count: Mutex<u64>,
}

pub(super) struct PublishedState {
    pub(super) checkpoint: DatabaseHeader,
    pub(super) latest_lsn: Lsn,
    latest: CommitState,
    pub(super) wal_index: WalIndex,
    pub(super) write_state: WriteState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteState {
    Ready,
    NeedsRecovery,
}

pub(crate) struct ReadSnapshot {
    pager: Pager,
    lsn: Lsn,
    state: CommitState,
    checkpoint: DatabaseHeader,
    _lease: SnapshotLease,
}

impl Pager {
    pub(super) fn checkpoint(&self) -> Result<super::checkpoint::CheckpointResult> {
        if self.inner.mode == OpenMode::ReadOnly {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "read-only pager cannot checkpoint",
            ));
        }
        let _writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| internal("pager writer mutex is poisoned"))?;
        super::checkpoint::run(&self.inner)
    }

    pub(super) fn create(path: &Path, config: PagerConfig) -> Result<Self> {
        Self::create_with_faults(path, config, Arc::new(crate::fault::NoFaults))
    }

    fn create_with_faults(
        path: &Path,
        config: PagerConfig,
        faults: Arc<dyn FaultInjector>,
    ) -> Result<Self> {
        config.validate_wal_limits()?;
        let cache = PageCache::new(config.cache_pages)?;
        let mut file = DatabaseFile::create_with_faults(
            path,
            config.page_size,
            config.uuid,
            config.max_database_bytes,
            faults.clone(),
        )?;
        let header = file.header();
        let wal = Wal::create_with_faults(path, &header, faults)?;
        file.finish_wal_initialization()?;
        let header = file.header();
        Self::from_recovered(
            file,
            wal,
            cache,
            OpenMode::ReadWrite,
            &config,
            header,
            header.checkpoint_lsn(),
            checkpoint_state(&header),
            WalIndex::default(),
        )
    }

    pub(super) fn open(path: &Path, mode: OpenMode, config: PagerConfig) -> Result<Self> {
        config.validate_wal_limits()?;
        let file = DatabaseFile::open_for_recovery(path, mode, config.max_database_bytes)?;
        let header = file.header();
        let cache = PageCache::new(config.cache_pages)?;
        let mut wal = Wal::open_with_limit(
            path,
            &header,
            mode,
            config.max_wal_bytes,
            file.can_recreate_wal(),
        )?;
        let recovered = wal.recover(&header, mode)?;
        file.validate_recovered_tail(recovered.latest_state.page_count)?;
        let pager = Self::from_recovered(
            file,
            wal,
            cache,
            mode,
            &config,
            header,
            recovered.latest_lsn,
            recovered.latest_state,
            recovered.index,
        )?;
        pager.validate_recovered()?;
        if mode == OpenMode::ReadWrite {
            let mut file = pager.main_lock()?;
            if file.header().publish_sequence() <= 2 {
                pager.wal_lock()?.sync_all()?;
                pager.wal_lock()?.sync_directory()?;
                super::file::sync_parent_directory(
                    path,
                    &crate::fault::NoFaults,
                    crate::fault::FaultPoint::BeforeMainDirectorySync,
                    crate::fault::FaultPoint::AfterMainDirectorySync,
                )?;
            }
            file.finish_wal_initialization()?;
            let header = file.header();
            drop(file);
            pager.published_write()?.checkpoint = header;
        }
        Ok(pager)
    }

    #[allow(clippy::too_many_arguments)]
    fn from_recovered(
        file: DatabaseFile,
        wal: Wal,
        cache: PageCache,
        mode: OpenMode,
        config: &PagerConfig,
        checkpoint: DatabaseHeader,
        latest_lsn: Lsn,
        latest: CommitState,
        wal_index: WalIndex,
    ) -> Result<Self> {
        if latest_lsn < checkpoint.checkpoint_lsn() {
            return Err(corruption(
                "recovered WAL LSN predates the database checkpoint",
            ));
        }
        validate_recovered_page_count(
            latest.page_count,
            checkpoint.page_size(),
            config.max_database_bytes,
        )?;
        Ok(Self {
            inner: Arc::new(PagerInner {
                main: Mutex::new(file),
                wal: Mutex::new(wal),
                published: RwLock::new(PublishedState {
                    checkpoint,
                    latest_lsn,
                    latest,
                    wal_index,
                    write_state: WriteState::Ready,
                }),
                snapshots: SnapshotRegistry::default(),
                cache: Mutex::new(cache),
                cache_pages: config.cache_pages,
                writer: Mutex::new(()),
                mode,
                max_database_bytes: config.max_database_bytes,
                max_wal_bytes: config.max_wal_bytes,
                checkpoint_wal_bytes: config.checkpoint_wal_bytes,
                #[cfg(test)]
                reused_page_count: Mutex::new(0),
            }),
        })
    }

    pub(crate) fn database_uuid(&self) -> Result<[u8; 16]> {
        Ok(self.published_read()?.checkpoint.uuid())
    }

    pub(crate) fn begin_read(&self) -> Result<ReadSnapshot> {
        let published = self.published_read()?;
        let lsn = published.latest_lsn;
        let state = published.wal_index.state_at(&published.checkpoint, lsn)?;
        let checkpoint = published.checkpoint;
        let lease = self.inner.snapshots.register(lsn)?;
        drop(published);
        Ok(ReadSnapshot {
            pager: self.clone(),
            lsn,
            state,
            checkpoint,
            _lease: lease,
        })
    }

    pub(crate) fn begin_write(&self) -> Result<WriteBatch<'_>> {
        if self.inner.mode == OpenMode::ReadOnly {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "read-only pager cannot begin a write batch",
            ));
        }
        let writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| internal("pager writer mutex is poisoned"))?;
        self.begin_write_with_guard(writer)
    }

    pub(super) fn try_begin_write(&self) -> Result<WriteBatch<'_>> {
        if self.inner.mode == OpenMode::ReadOnly {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "read-only pager cannot begin a write batch",
            ));
        }
        let writer = match self.inner.writer.try_lock() {
            Ok(writer) => writer,
            Err(TryLockError::WouldBlock) => {
                return Err(DbError::new(
                    ErrorCode::Busy,
                    "another write batch is active",
                ));
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err(internal("pager writer mutex is poisoned"));
            }
        };
        self.begin_write_with_guard(writer)
    }

    fn begin_write_with_guard<'a>(&'a self, writer: MutexGuard<'a, ()>) -> Result<WriteBatch<'a>> {
        let wal_len = self.wal_lock()?.len();
        if wal_len >= self.inner.checkpoint_wal_bytes {
            super::checkpoint::run(&self.inner)?;
        }
        let (base_lsn, committed_state, checkpoint) = {
            let published = self.published_read()?;
            if published.write_state == WriteState::NeedsRecovery {
                return Err(DbError::new(
                    ErrorCode::Transaction,
                    "WAL recovery is required before another write batch can begin",
                ));
            }
            (published.latest_lsn, published.latest, published.checkpoint)
        };
        let mut base_header = checkpoint;
        base_header.set_catalog_root(committed_state.catalog_root);
        base_header.set_freelist_root(committed_state.freelist_root);
        base_header.set_features(committed_state.feature_bits)?;
        let mut read = self.begin_read()?;
        let committed_freelist = walk_freelist(
            &mut read,
            committed_state.freelist_root,
            committed_state.page_count,
        )?;
        let committed_catalog = snapshot_catalog_reachability(
            &mut read,
            committed_state.catalog_root,
            committed_state.page_count,
        )?;
        let committed_live_pages = committed_catalog
            .as_ref()
            .map_or_else(BTreeSet::new, |(_, pages)| pages.clone());
        validate_freelist(&mut read, &committed_live_pages)?;
        drop(read);

        Ok(WriteBatch {
            pager: self,
            _writer: writer,
            base_lsn,
            committed_state,
            base_header,
            dirty: BTreeMap::new(),
            new_pages: BTreeSet::new(),
            pending_generations: BTreeMap::new(),
            pending_releases: BTreeMap::new(),
            committed_freelist,
            committed_catalog,
            candidate_header: None,
            tracked_trees: Vec::new(),
            superseded_roots: HashSet::new(),
            catalog_update_pending: false,
            catalog_validation_required: false,
            finished: false,
        })
    }

    pub(super) fn latest_lsn(&self) -> Result<Lsn> {
        Ok(self.published_read()?.latest_lsn)
    }

    pub(super) fn read(&self, reference: PageRef) -> Result<PageHandle> {
        let mut snapshot = self.begin_read()?;
        snapshot.read(reference)
    }

    pub(super) fn page_count(&self) -> u64 {
        self.published_read()
            .expect("pager published state mutex is poisoned")
            .latest
            .page_count
    }

    pub(super) fn page_size(&self) -> u32 {
        self.published_read()
            .expect("pager published state mutex is poisoned")
            .checkpoint
            .page_size()
    }

    pub(super) fn is_poisoned(&self) -> bool {
        self.published_read()
            .is_ok_and(|published| published.write_state == WriteState::NeedsRecovery)
    }

    #[cfg(test)]
    pub(super) fn reused_page_count(&self) -> u64 {
        *self
            .inner
            .reused_page_count
            .lock()
            .expect("test-only reused page count mutex is poisoned")
    }

    #[cfg(test)]
    pub(super) fn with_faults(self, faults: Arc<dyn FaultInjector>) -> Self {
        let inner = match Arc::try_unwrap(self.inner) {
            Ok(inner) => inner,
            Err(_) => panic!("test fault injection requires an unshared pager"),
        };
        let main = inner
            .main
            .into_inner()
            .expect("test main mutex must not be poisoned")
            .with_faults(faults.clone());
        let wal = inner
            .wal
            .into_inner()
            .expect("test WAL mutex must not be poisoned")
            .with_faults(faults);
        Self {
            inner: Arc::new(PagerInner {
                main: Mutex::new(main),
                wal: Mutex::new(wal),
                published: inner.published,
                snapshots: inner.snapshots,
                cache: inner.cache,
                cache_pages: inner.cache_pages,
                writer: inner.writer,
                mode: inner.mode,
                max_database_bytes: inner.max_database_bytes,
                max_wal_bytes: inner.max_wal_bytes,
                checkpoint_wal_bytes: inner.checkpoint_wal_bytes,
                reused_page_count: inner.reused_page_count,
            }),
        }
    }

    fn validate_recovered(&self) -> Result<()> {
        let mut snapshot = self.begin_read()?;
        validate_feature_bits(snapshot.feature_bits())?;
        if snapshot.feature_bits() & super::features::CATALOG_V1 != 0 {
            // Recovery has selected a complete commit. Semantic corruption in that
            // candidate is fatal; do not reinterpret it as an incomplete WAL tail.
            return super::catalog::CatalogReader::open_empty_or_v1(&mut snapshot).map(|_| ());
        }
        // Featureless Phase 3 catalogs have no V1 record contract. Retain their
        // existing structural and freelist validation without decoding V1 records.
        let catalog_root = snapshot.state.catalog_root;
        let page_count = snapshot.state.page_count;
        let catalog = snapshot_catalog_reachability(&mut snapshot, catalog_root, page_count)?;
        let live_pages = catalog.map_or_else(BTreeSet::new, |(_, pages)| pages);
        validate_freelist(&mut snapshot, &live_pages).map(|_| ())
    }

    fn read_at(
        &self,
        snapshot: Lsn,
        state: CommitState,
        checkpoint: DatabaseHeader,
        reference: PageRef,
    ) -> Result<PageHandle> {
        let handle = self.read_by_id_at(snapshot, state, checkpoint, reference.page_id())?;
        if handle.header().generation() != reference.generation() {
            return Err(corruption(
                "page frame generation does not match the requested page",
            ));
        }
        Ok(handle)
    }

    fn read_by_id_at(
        &self,
        snapshot: Lsn,
        state: CommitState,
        _checkpoint: DatabaseHeader,
        page_id: PageId,
    ) -> Result<PageHandle> {
        if page_id.0 == 0 || page_id.0 >= state.page_count {
            return Err(corruption("page id is outside the visible database"));
        }
        let (version, checkpoint) = {
            let published = self.published_read()?;
            if snapshot < published.checkpoint.checkpoint_lsn() {
                return Err(corruption("snapshot predates the database checkpoint"));
            }
            (
                published.wal_index.visible_version(page_id, snapshot),
                published.checkpoint,
            )
        };
        let key = CacheKey {
            page_id,
            source_lsn: version.map_or(checkpoint.checkpoint_lsn(), |version| version.lsn),
        };
        if let Some(handle) = self.cache_lock()?.get(key) {
            return Ok(handle);
        }

        let frame = match version {
            Some(version) => {
                self.wal_lock()?
                    .read_page_frame(page_id, version, checkpoint.page_size())?
            }
            None => {
                if page_id.0 >= checkpoint.page_count() {
                    return Err(corruption("page is absent from both WAL and checkpoint"));
                }
                PageFrame::decode(self.main_lock()?.read_page(page_id)?, page_id, None, None)?
            }
        };
        let handle = Arc::new(frame);
        self.cache_lock()?.insert_clean(key, handle.clone())?;
        Ok(handle)
    }

    fn main_lock(&self) -> Result<MutexGuard<'_, DatabaseFile>> {
        self.inner
            .main
            .lock()
            .map_err(|_| internal("pager main-file mutex is poisoned"))
    }

    fn wal_lock(&self) -> Result<MutexGuard<'_, Wal>> {
        self.inner
            .wal
            .lock()
            .map_err(|_| internal("pager WAL mutex is poisoned"))
    }

    fn cache_lock(&self) -> Result<MutexGuard<'_, PageCache>> {
        self.inner
            .cache
            .lock()
            .map_err(|_| internal("pager cache mutex is poisoned"))
    }

    fn published_read(&self) -> Result<std::sync::RwLockReadGuard<'_, PublishedState>> {
        self.inner
            .published
            .read()
            .map_err(|_| internal("pager published-state lock is poisoned"))
    }

    fn published_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, PublishedState>> {
        self.inner
            .published
            .write()
            .map_err(|_| internal("pager published-state lock is poisoned"))
    }
}

#[cfg(test)]
pub(super) fn checkpoint_after_writer_contention(
    pager: &Pager,
    on_contention: impl FnOnce(),
) -> Result<super::checkpoint::CheckpointResult> {
    if pager.inner.mode == OpenMode::ReadOnly {
        return Err(DbError::new(
            ErrorCode::Unsupported,
            "read-only pager cannot checkpoint",
        ));
    }
    match pager.inner.writer.try_lock() {
        Ok(_writer) => {
            return Err(internal(
                "checkpoint contention test requires an active writer",
            ));
        }
        Err(TryLockError::WouldBlock) => on_contention(),
        Err(TryLockError::Poisoned(_)) => {
            return Err(internal("pager writer mutex is poisoned"));
        }
    }
    let _writer = pager
        .inner
        .writer
        .lock()
        .map_err(|_| internal("pager writer mutex is poisoned"))?;
    super::checkpoint::run(&pager.inner)
}

pub(super) trait PageSource {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle>;

    fn read_page_by_id(&mut self, _page_id: PageId) -> Result<PageHandle> {
        Err(corruption(
            "page source cannot read a page without a generation",
        ))
    }

    fn page_count(&self) -> Result<u64>;

    fn freelist_root(&self) -> Result<PageId> {
        Ok(PageId(0))
    }
}

impl ReadSnapshot {
    pub(crate) fn catalog_reader(&mut self) -> Result<super::catalog::CatalogReader> {
        super::catalog::CatalogReader::open_empty_or_v1(self)
    }

    pub(super) const fn lsn(&self) -> Lsn {
        self.lsn
    }

    pub(super) const fn feature_bits(&self) -> u64 {
        self.state.feature_bits
    }

    pub(super) fn read(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.pager
            .read_at(self.lsn, self.state, self.checkpoint, reference)
    }

    pub(super) fn catalog_tree(&mut self, kind: TreeKind) -> Result<Option<BTree>> {
        let Some((tree, report)) = self.catalog_tree_with_report(kind)? else {
            return Ok(None);
        };
        validate_freelist(self, &report.all_live_pages())?;
        Ok(Some(tree))
    }

    pub(super) const fn catalog_root(&self) -> PageId {
        self.state.catalog_root
    }

    pub(super) fn catalog_tree_with_report(
        &mut self,
        kind: TreeKind,
    ) -> Result<Option<(BTree, ValidationReport)>> {
        catalog_tree_with_report(self, self.state.catalog_root, kind)
    }
}

fn catalog_tree_with_report(
    source: &mut impl PageSource,
    page_id: PageId,
    kind: TreeKind,
) -> Result<Option<(BTree, ValidationReport)>> {
    if page_id.0 == 0 {
        return Ok(None);
    }
    if page_id.0 >= source.page_count()? {
        return Err(corruption("catalog root is outside the database"));
    }
    let frame = source.read_page_by_id(page_id)?;
    let root = PageRef::new(page_id, frame.header().generation())?;
    if TreeKind::from_root_page_type(frame.header().page_type()) != Some(kind) {
        return Err(corruption(
            "catalog root page type does not match the requested tree kind",
        ));
    }
    let tree = BTree::from_root(kind, root);
    let report = validate_tree(source, &tree)?;
    Ok(Some((tree, report)))
}

impl PageSource for ReadSnapshot {
    fn read_page(&mut self, reference: PageRef) -> Result<PageHandle> {
        self.read(reference)
    }

    fn read_page_by_id(&mut self, page_id: PageId) -> Result<PageHandle> {
        self.pager
            .read_by_id_at(self.lsn, self.state, self.checkpoint, page_id)
    }

    fn page_count(&self) -> Result<u64> {
        Ok(self.state.page_count)
    }

    fn freelist_root(&self) -> Result<PageId> {
        Ok(self.state.freelist_root)
    }
}

fn checkpoint_state(header: &DatabaseHeader) -> CommitState {
    CommitState {
        page_count: header.page_count(),
        catalog_root: header.catalog_root(),
        freelist_root: header.freelist_root(),
        feature_bits: header.features(),
    }
}

fn validate_recovered_page_count(
    page_count: u64,
    page_size: u32,
    max_database_bytes: u64,
) -> Result<()> {
    let last_page_id = page_count
        .checked_sub(1)
        .ok_or_else(|| corruption("recovered WAL page count must be nonzero"))?;
    if last_page_id == 0 {
        if super::header::SUPERBLOCK_AREA_LEN > max_database_bytes {
            return Err(corruption(
                "recovered WAL database exceeds the configured maximum size",
            ));
        }
        return Ok(());
    }
    checked_page_offset(PageId(last_page_id), page_size, max_database_bytes).map(|_| ())
}

fn walk_freelist(
    source: &mut impl PageSource,
    root: PageId,
    page_count: u64,
) -> Result<FreelistWalk> {
    Freelist::walk(root, page_count, |page_id| {
        let frame = source.read_page_by_id(page_id)?;
        copy_clean_frame(frame.as_ref())
    })
}

fn snapshot_catalog_reachability(
    source: &mut impl PageSource,
    page_id: PageId,
    page_count: u64,
) -> Result<Option<(PageRef, BTreeSet<PageId>)>> {
    if page_id.0 == 0 {
        return Ok(None);
    }
    if page_id.0 >= page_count {
        return Err(corruption("catalog root is outside the database"));
    }
    let frame = source.read_page_by_id(page_id)?;
    let root = PageRef::new(page_id, frame.header().generation())?;
    let kind = TreeKind::from_root_page_type(frame.header().page_type())
        .ok_or_else(|| corruption("catalog root does not reference a B+Tree page"))?;
    let report = validate_tree(source, &BTree::from_root(kind, root))?;
    Ok(Some((root, report.all_live_pages())))
}

pub(crate) struct WriteBatch<'a> {
    pager: &'a Pager,
    _writer: MutexGuard<'a, ()>,
    base_lsn: Lsn,
    committed_state: CommitState,
    base_header: DatabaseHeader,
    dirty: BTreeMap<PageId, PageFrame>,
    new_pages: BTreeSet<PageId>,
    pending_generations: BTreeMap<PageId, u64>,
    pending_releases: BTreeMap<PageId, PageRef>,
    committed_freelist: FreelistWalk,
    committed_catalog: Option<(PageRef, BTreeSet<PageId>)>,
    candidate_header: Option<DatabaseHeader>,
    tracked_trees: Vec<(TreeKind, PageRef)>,
    // Retain transitions even before a catalog writer opens. An old root may
    // still be a valid descendant, so page validation alone cannot reject it.
    superseded_roots: HashSet<PageRef>,
    catalog_update_pending: bool,
    // A successful finish binds this batch to catalog validation through commit.
    // Raw low-level batches remain available for deliberate corruption fixtures.
    catalog_validation_required: bool,
    finished: bool,
}

pub(super) struct WriteBatchCheckpoint {
    dirty: BTreeMap<PageId, PageFrame>,
    new_pages: BTreeSet<PageId>,
    pending_generations: BTreeMap<PageId, u64>,
    pending_releases: BTreeMap<PageId, PageRef>,
    candidate_header: Option<DatabaseHeader>,
    tracked_trees: Vec<(TreeKind, PageRef)>,
    superseded_roots: HashSet<PageRef>,
    catalog_update_pending: bool,
    catalog_validation_required: bool,
}

impl WriteBatch<'_> {
    pub(crate) fn catalog_reader(&mut self) -> Result<super::catalog::CatalogReader> {
        self.require_active()?;
        self.require_catalog_finished()?;
        super::catalog::CatalogReader::open_empty_or_v1_batch(self)
    }

    pub(super) const fn catalog_root(&self) -> PageId {
        self.current_header().catalog_root()
    }

    pub(super) fn begin_catalog_update(&mut self) -> Result<()> {
        self.require_active()?;
        self.require_catalog_finished()?;
        self.catalog_update_pending = true;
        Ok(())
    }

    pub(super) fn finish_catalog_update(&mut self) -> Result<()> {
        self.require_active()?;
        if !self.catalog_update_pending {
            return Err(DbError::new(
                ErrorCode::Transaction,
                "no catalog update is pending",
            ));
        }
        self.validate_catalog_candidate()?;
        self.catalog_update_pending = false;
        self.catalog_validation_required = true;
        Ok(())
    }

    fn validate_catalog_candidate(&mut self) -> Result<()> {
        let features = self.feature_bits();
        validate_feature_bits(features)?;
        if features & super::features::CATALOG_V1 == 0 || self.catalog_root().0 == 0 {
            return Err(corruption(
                "catalog update requires a complete V1 root state",
            ));
        }
        let frame = self.read_frame_by_id(self.catalog_root())?;
        let root = PageRef::new(self.catalog_root(), frame.header().generation())?;
        let tree = BTree::from_root(TreeKind::Table, root);
        super::catalog::CatalogReader::open_candidate(self, &tree, features)?;
        Ok(())
    }

    pub(super) fn require_current_catalog_root(&self, root: PageRef) -> Result<()> {
        if self.pending_releases.contains_key(&root.page_id()) {
            return Err(corruption("catalog references a root pending release"));
        }
        if self.superseded_roots.contains(&root) {
            return Err(corruption("catalog references a superseded B+Tree root"));
        }
        Ok(())
    }

    pub(super) fn catalog_tree_with_report(
        &mut self,
        kind: TreeKind,
    ) -> Result<Option<(BTree, ValidationReport)>> {
        self.require_active()?;
        catalog_tree_with_report(self, self.current_header().catalog_root(), kind)
    }

    fn read_base(&self, reference: PageRef) -> Result<PageHandle> {
        self.pager.read_at(
            self.base_lsn,
            self.committed_state,
            self.base_header,
            reference,
        )
    }

    pub(super) const fn feature_bits(&self) -> u64 {
        self.current_header().features()
    }

    pub(super) fn enable_feature_bits(&mut self, bits: u64) -> Result<()> {
        self.require_active()?;
        let candidate = self.feature_bits() | bits;
        validate_feature_bits(candidate)?;
        let mut header = self.current_header();
        header.set_features(candidate)?;
        self.candidate_header = Some(header);
        Ok(())
    }

    pub(super) fn checkpoint_mutation(&mut self) -> Result<WriteBatchCheckpoint> {
        self.require_active()?;
        let dirty = self
            .dirty
            .iter_mut()
            .map(|(page_id, frame)| Ok((*page_id, private_frame(frame)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(WriteBatchCheckpoint {
            dirty,
            new_pages: self.new_pages.clone(),
            pending_generations: self.pending_generations.clone(),
            pending_releases: self.pending_releases.clone(),
            candidate_header: self.candidate_header,
            tracked_trees: self.tracked_trees.clone(),
            superseded_roots: self.superseded_roots.clone(),
            catalog_update_pending: self.catalog_update_pending,
            catalog_validation_required: self.catalog_validation_required,
        })
    }

    pub(super) fn restore_mutation(&mut self, checkpoint: WriteBatchCheckpoint) {
        self.dirty = checkpoint.dirty;
        self.new_pages = checkpoint.new_pages;
        self.pending_generations = checkpoint.pending_generations;
        self.pending_releases = checkpoint.pending_releases;
        self.candidate_header = checkpoint.candidate_header;
        self.tracked_trees = checkpoint.tracked_trees;
        self.superseded_roots = checkpoint.superseded_roots;
        self.catalog_update_pending = checkpoint.catalog_update_pending;
        self.catalog_validation_required = checkpoint.catalog_validation_required;
    }

    pub(super) fn track_tree(&mut self, kind: TreeKind, root: PageRef) -> Result<()> {
        self.require_active()?;
        if self.pending_releases.contains_key(&root.page_id()) {
            return Err(corruption(
                "released page cannot remain a tracked tree root in the same batch",
            ));
        }
        self.read(root)?;
        if !self.tracked_trees.contains(&(kind, root)) {
            self.tracked_trees.push((kind, root));
        }
        Ok(())
    }

    /// A catalog root replacement must name an exact current registered Table root.
    pub(super) fn require_tracked_table_root(&mut self, root: PageRef) -> Result<()> {
        self.require_active()?;
        self.require_current_catalog_root(root)?;
        if !self.tracked_trees.contains(&(TreeKind::Table, root)) {
            return Err(corruption(
                "table root must be a registered current Table B+Tree root",
            ));
        }
        if TreeKind::from_root_page_type(self.read(root)?.header().page_type())
            != Some(TreeKind::Table)
        {
            return Err(corruption("table root must reference a Table B+Tree page"));
        }
        Ok(())
    }

    pub(super) fn refresh_tree(
        &mut self,
        kind: TreeKind,
        previous_root: PageRef,
        root: PageRef,
    ) -> Result<()> {
        if previous_root != root {
            self.tracked_trees
                .retain(|tracked| *tracked != (kind, previous_root));
        }
        self.track_tree(kind, root)?;
        if previous_root != root {
            self.superseded_roots.insert(previous_root);
            // A root can become current again after a split followed by collapse.
            self.superseded_roots.remove(&root);
        }
        Ok(())
    }

    pub(super) fn replace_tree_root(
        &mut self,
        kind: TreeKind,
        previous_root: PageRef,
        root: PageRef,
    ) -> Result<()> {
        self.refresh_tree(kind, previous_root, root)?;
        if self.current_header().catalog_root() == previous_root.page_id() {
            validate_tree(self, &BTree::from_root(kind, root))?;
            let mut header = self.current_header();
            header.set_catalog_root(root.page_id());
            self.candidate_header = Some(header);
        }
        Ok(())
    }

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

        self.read_base(reference)
    }

    pub(super) fn modify(&mut self, reference: PageRef) -> Result<&mut PageFrame> {
        self.require_active()?;
        if self.pending_releases.contains_key(&reference.page_id()) {
            return Err(corruption(
                "released page cannot become dirty in the same batch",
            ));
        }
        if !self.dirty.contains_key(&reference.page_id()) {
            let handle = self.read_base(reference)?;
            self.dirty
                .insert(reference.page_id(), copy_clean_frame(handle.as_ref())?);
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
            .committed_state
            .page_count
            .checked_add(allocated)
            .ok_or_else(|| corruption("page id overflow while allocating"))?;
        let reference = PageRef::new(PageId(page_id), 1)?;
        page_id
            .checked_add(1)
            .ok_or_else(|| corruption("page count overflow while allocating"))?;
        checked_page_offset(
            reference.page_id(),
            self.base_header.page_size(),
            self.pager.inner.max_database_bytes,
        )?;
        let frame = PageFrame::new_zeroed(page_type, reference, self.base_header.page_size())?;
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

    pub(crate) fn rollback(&mut self) -> Result<()> {
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
        let page_type = self.read(root)?.header().page_type();
        let kind = TreeKind::from_root_page_type(page_type)
            .ok_or_else(|| corruption("catalog root must reference a B+Tree page"))?;
        if !self.tracked_trees.contains(&(kind, root)) {
            return Err(corruption(
                "catalog root must be a registered current B+Tree root",
            ));
        }
        let tree = BTree::from_root(kind, root);
        validate_tree(self, &tree)?;

        if self.current_header().catalog_root() == root.page_id() {
            return Ok(());
        }

        if let Some((committed_root, committed_pages)) = &self.committed_catalog {
            if *committed_root != root && committed_pages.contains(&root.page_id()) {
                return Err(corruption(
                    "reachable non-root catalog tree page cannot become the catalog root",
                ));
            }
        }

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

    pub(super) fn release_btree_later(&mut self, reference: PageRef) -> Result<()> {
        self.require_active()?;
        let batch_allocation = self
            .pending_generations
            .get(&reference.page_id())
            .is_some_and(|generation| *generation == reference.generation());
        if self.pending_releases.contains_key(&reference.page_id()) {
            return Err(corruption("page is already pending release in this batch"));
        }
        if reference.page_id().0 == 0 {
            return Err(corruption("database header cannot enter the freelist"));
        }
        if reference.page_id() == self.current_header().catalog_root() {
            return Err(corruption("active catalog root cannot enter the freelist"));
        }
        let page_type = if let Some(frame) = self.dirty.get(&reference.page_id()) {
            if frame.header().generation() != reference.generation() {
                return Err(corruption(
                    "page frame generation does not match the requested page",
                ));
            }
            frame.header().page_type()
        } else {
            self.read(reference)?.header().page_type()
        };
        if !matches!(
            page_type,
            PageType::TableLeaf
                | PageType::TableInternal
                | PageType::IndexLeaf
                | PageType::IndexInternal
                | PageType::Overflow
        ) {
            return Err(corruption(
                "B+Tree release requires a B+Tree or owned overflow page",
            ));
        }

        if self.committed_freelist.contains_trunk(reference.page_id()) && !batch_allocation {
            return Err(corruption(
                "freelist trunk cannot enter the freelist as a leaf",
            ));
        }
        if self.committed_freelist.contains_leaf(reference.page_id()) && !batch_allocation {
            return Err(corruption(
                "page is already recorded in the committed freelist",
            ));
        }
        if self.pending_generations.contains_key(&reference.page_id()) && !batch_allocation {
            return Err(corruption(
                "new page generation does not match the requested B+Tree page",
            ));
        }

        if batch_allocation {
            let mut tombstone = PageFrame::new_zeroed(
                PageType::TableLeaf,
                reference,
                self.base_header.page_size(),
            )?;
            SlottedPage::open(&mut tombstone, LEAF_METADATA_END)?;
            self.dirty.insert(reference.page_id(), tombstone);
        } else {
            self.read_base(reference)?;
            self.dirty.remove(&reference.page_id());
        }
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
        let staged_batch_allocation = self
            .pending_releases
            .get(&reference.page_id())
            .is_some_and(|pending| *pending == reference)
            && self
                .pending_generations
                .get(&reference.page_id())
                .is_some_and(|generation| *generation == reference.generation())
            && self
                .dirty
                .get(&reference.page_id())
                .is_some_and(|frame| frame.header().generation() == reference.generation());
        if (self.dirty.contains_key(&reference.page_id())
            || self.new_pages.contains(&reference.page_id()))
            && !staged_batch_allocation
        {
            return Err(corruption("dirty live page cannot enter the freelist"));
        }
        if self.committed_freelist.contains_trunk(reference.page_id()) && !staged_batch_allocation {
            return Err(corruption(
                "freelist trunk cannot enter the freelist as a leaf",
            ));
        }
        if self.committed_freelist.contains_leaf(reference.page_id()) && !staged_batch_allocation {
            return Err(corruption(
                "page is already recorded in the committed freelist",
            ));
        }
        self.read(reference)?;
        Ok(())
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.require_active()?;
        // Reject before preparing releases or materializing a header-only change,
        // so unfinished writers always produce Transaction and never reach WAL I/O.
        self.require_catalog_finished()?;
        self.prepare_freelist_releases()?;
        if self.dirty.is_empty() && self.candidate_header.is_some() {
            let catalog_root = self.current_header().catalog_root();
            if catalog_root.0 == 0 {
                return Err(corruption(
                    "catalog root state change requires a nonzero root page",
                ));
            }
            let frame = self.read_frame_by_id(catalog_root)?;
            self.dirty.insert(catalog_root, frame);
        }
        self.validate_for_commit()?;

        if self.dirty.is_empty() && self.candidate_header.is_none() {
            self.finish();
            return Ok(());
        }

        let transaction_lsn = self.base_lsn.0.checked_add(1).map(Lsn).ok_or_else(|| {
            DbError::new(ErrorCode::Transaction, "WAL LSN overflow requires recovery")
        })?;
        let target_page_count = self.target_page_count()?;
        let roots = self.candidate_header.unwrap_or(self.base_header);
        let candidate_state = CommitState {
            page_count: target_page_count,
            catalog_root: roots.catalog_root(),
            freelist_root: roots.freelist_root(),
            feature_bits: roots.features(),
        };
        let frames = self
            .dirty
            .iter()
            .map(|(page_id, frame)| {
                (
                    *page_id,
                    frame.header().generation(),
                    frame.bytes().to_vec(),
                )
            })
            .collect::<Vec<_>>();
        self.preflight_wal_limit(&frames)?;
        let publication_observer = self.pager.wal_lock()?.publication_observer();
        let versions = self.pager.wal_lock()?.projected_versions(&frames)?;
        // The writer guard excludes both another commit and checkpoint. Prepare
        // all validation, allocation and map changes before touching the WAL.
        let mut candidate_index = self.pager.published_read()?.wal_index.clone();
        candidate_index.publish(transaction_lsn, Some(candidate_state), &versions)?;
        // Verify write-lock health before durable I/O. Only this writer can
        // acquire a write guard until publication (read-guard panic cannot poison).
        drop(self.pager.published_write()?);
        let mut wal = self.pager.wal_lock()?;
        match wal.append_transaction(transaction_lsn, self.base_lsn, candidate_state, &frames) {
            Ok(_) => (),
            Err(error) => return self.fail_wal_commit(error),
        };
        if let Err(error) = wal.sync_all() {
            return self.fail_wal_commit(error);
        }
        drop(wal);
        publication_observer.observe_publication(crate::fault::FaultPoint::BeforeWalPublish);
        let retired_index = {
            let mut published = self
                .pager
                .inner
                .published
                .write()
                .expect("exclusive writer prechecked published lock before WAL I/O");
            let retired = std::mem::replace(&mut published.wal_index, candidate_index);
            published.latest_lsn = transaction_lsn;
            published.latest = candidate_state;
            retired
        };
        publication_observer.observe_publication(crate::fault::FaultPoint::AfterWalPublish);
        drop(retired_index); // recursive map destruction is outside the publication lock
        self.replace_cache_images(transaction_lsn);
        self.finish();
        Ok(())
    }

    fn fail_wal_commit<T>(&mut self, error: DbError) -> Result<T> {
        if let Ok(mut published) = self.pager.published_write() {
            published.write_state = WriteState::NeedsRecovery;
        }
        self.finish();
        Err(error)
    }

    fn preflight_wal_limit(&self, frames: &[(PageId, u64, Vec<u8>)]) -> Result<()> {
        let page_bytes = u64::from(self.base_header.page_size())
            .checked_add(64)
            .ok_or_else(|| corruption("WAL frame length overflow"))?;
        let frame_count =
            u64::try_from(frames.len()).map_err(|_| corruption("WAL frame count exceeds u64"))?;
        let transaction_bytes = page_bytes
            .checked_mul(frame_count)
            .and_then(|bytes| bytes.checked_add(128))
            .ok_or_else(|| corruption("WAL transaction length overflow"))?;
        let limit = self.pager.inner.max_wal_bytes;
        if super::wal::WAL_HEADER_AREA_LEN
            .checked_add(transaction_bytes)
            .is_none_or(|total| total > limit)
        {
            return Err(DbError::new(
                ErrorCode::Transaction,
                "transaction cannot fit in an empty WAL within the hard limit",
            ));
        }
        let exceeds = |wal_len: u64| {
            wal_len
                .checked_add(transaction_bytes)
                .is_none_or(|total| total > limit)
        };
        let wal_len = self.pager.wal_lock()?.len();
        if exceeds(wal_len) {
            // The batch already owns the writer mutex; checkpoint only committed pages.
            super::checkpoint::run(&self.pager.inner)?;
            let wal_len = self.pager.wal_lock()?.len();
            if exceeds(wal_len) {
                let message = match self.pager.inner.snapshots.oldest()? {
                    Some(oldest) => {
                        format!("WAL hard limit blocked by oldest snapshot LSN {}", oldest.0)
                    }
                    None => "WAL hard limit requires a checkpoint retry after snapshot release"
                        .to_owned(),
                };
                return Err(DbError::new(ErrorCode::Busy, message));
            }
        }
        Ok(())
    }

    fn validate_for_commit(&mut self) -> Result<()> {
        self.require_catalog_finished()?;
        if self.catalog_validation_required {
            self.validate_catalog_candidate()?;
        }
        let target_page_count = self.target_page_count()?;
        if target_page_count == 1 {
            if super::header::SUPERBLOCK_AREA_LEN > self.pager.inner.max_database_bytes {
                return Err(corruption(
                    "commit exceeds the configured maximum database size",
                ));
            }
        } else {
            checked_page_offset(
                PageId(target_page_count - 1),
                self.base_header.page_size(),
                self.pager.inner.max_database_bytes,
            )?;
        }

        for (page_id, frame) in &mut self.dirty {
            if frame.header().page_id() != *page_id {
                return Err(corruption(
                    "dirty page image key does not match its page header",
                ));
            }
            if let Some(generation) = self.pending_generations.get(page_id) {
                if frame.header().generation() != *generation {
                    return Err(corruption(
                        "new page image generation does not match its allocation",
                    ));
                }
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
        let (candidate_catalog_root, mut live_pages) = self.candidate_catalog_reachability()?;
        let tracked_trees = self.tracked_trees.clone();
        for (kind, root) in tracked_trees {
            if candidate_catalog_root == Some(root) {
                continue;
            }
            let tree = BTree::from_root(kind, root);
            let report = validate_tree(self, &tree)?;
            if !live_pages.is_disjoint(&report.all_live_pages()) {
                return Err(corruption("tracked B+Trees overlap in live pages"));
            }
            live_pages.extend(report.all_live_pages());
        }
        validate_freelist(self, &live_pages)?;
        Ok(())
    }

    fn candidate_catalog_reachability(&mut self) -> Result<(Option<PageRef>, BTreeSet<PageId>)> {
        let page_id = self.current_header().catalog_root();
        if page_id.0 == 0 {
            return Ok((None, BTreeSet::new()));
        }
        if page_id.0 >= self.target_page_count()? {
            return Err(corruption("catalog root is outside the candidate database"));
        }
        let frame = self.read_frame_by_id(page_id)?;
        let root = PageRef::new(page_id, frame.header().generation())?;
        let kind = TreeKind::from_root_page_type(frame.header().page_type())
            .ok_or_else(|| corruption("catalog root does not reference a B+Tree page"))?;
        let report = validate_tree(self, &BTree::from_root(kind, root))?;
        Ok((Some(root), report.all_live_pages()))
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
                self.base_header.page_size(),
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
        if Freelist::is_full(&trunk, self.base_header.page_size())? {
            let mut frame = PageFrame::new_zeroed(
                PageType::Freelist,
                released_reference,
                self.base_header.page_size(),
            )?;
            Freelist::write(&mut frame, &Freelist::empty_trunk(Some(root_reference)))?;
            self.dirty.insert(page_id, frame);
            self.stage_freelist_root(page_id);
        } else {
            Freelist::release(&mut trunk, page_id, self.base_header.page_size())?;
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
        let frame = PageFrame::new_zeroed(page_type, reference, self.base_header.page_size())?;
        #[cfg(test)]
        {
            let mut reused = self
                .pager
                .inner
                .reused_page_count
                .lock()
                .map_err(|_| internal("test-only reused page count mutex is poisoned"))?;
            *reused = reused
                .checked_add(1)
                .ok_or_else(|| corruption("test-only reused page counter overflow"))?;
        }
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
        let page_count = self.target_page_count()?;
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
        let handle = self.pager.read_by_id_at(
            self.base_lsn,
            self.committed_state,
            self.base_header,
            page_id,
        )?;
        copy_clean_frame(handle.as_ref())
    }

    const fn current_header(&self) -> DatabaseHeader {
        match self.candidate_header {
            Some(header) => header,
            None => self.base_header,
        }
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
            None => Ok(self.committed_state.page_count),
        }
    }

    fn replace_cache_images(&mut self, source_lsn: Lsn) {
        for (page_id, frame) in std::mem::take(&mut self.dirty) {
            let handle = Arc::new(frame);
            if let Ok(mut cache) = self.pager.inner.cache.try_lock() {
                let _ = cache.insert_clean(
                    CacheKey {
                        page_id,
                        source_lsn,
                    },
                    handle,
                );
            }
        }
    }

    fn require_active(&self) -> Result<()> {
        if self.finished {
            return Err(DbError::new(
                ErrorCode::Transaction,
                "write batch has already finished",
            ));
        }
        if self.pager.published_read()?.write_state == WriteState::NeedsRecovery {
            return Err(DbError::new(
                ErrorCode::Transaction,
                "WAL recovery is required before another write batch can begin",
            ));
        }
        Ok(())
    }

    fn require_catalog_finished(&self) -> Result<()> {
        if self.catalog_update_pending {
            return Err(DbError::new(
                ErrorCode::Transaction,
                "catalog writer must finish before the batch can commit",
            ));
        }
        Ok(())
    }

    fn finish(&mut self) {
        self.dirty.clear();
        self.new_pages.clear();
        self.pending_generations.clear();
        self.pending_releases.clear();
        self.committed_freelist = FreelistWalk::default();
        self.committed_catalog = None;
        self.candidate_header = None;
        self.tracked_trees.clear();
        self.superseded_roots.clear();
        self.catalog_update_pending = false;
        self.catalog_validation_required = false;
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

    fn read_page_by_id(&mut self, page_id: PageId) -> Result<PageHandle> {
        self.require_active()?;
        let frame = self.read_frame_by_id(page_id)?;
        Ok(Arc::new(frame))
    }

    fn page_count(&self) -> Result<u64> {
        let appended = u64::try_from(self.new_pages.len())
            .map_err(|_| corruption("new page count exceeds u64"))?;
        self.pager
            .page_count()
            .checked_add(appended)
            .ok_or_else(|| corruption("batch target page count overflow"))
    }

    fn freelist_root(&self) -> Result<PageId> {
        Ok(self.current_header().freelist_root())
    }
}

fn private_handle(frame: &mut PageFrame) -> Result<PageHandle> {
    Ok(Arc::new(private_frame(frame)?))
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

fn copy_clean_frame(frame: &PageFrame) -> Result<PageFrame> {
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

fn internal(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        ops::Bound,
        sync::{Arc, Mutex},
    };

    use super::{PageSource, Pager, PagerConfig, copy_clean_frame};
    use crate::{
        DbError, ErrorCode, Result,
        fault::{FaultInjector, FaultPoint},
    };

    use super::super::{
        Lsn, PageId, PageRef,
        btree::{
            BTree, INTERNAL_METADATA_END, InternalCell, LEAF_METADATA_END, TreeKind,
            decode_internal_leftmost, encode_internal_leftmost,
        },
        catalog::CatalogWriter,
        features::CATALOG_V1,
        file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode, read_all_for_test},
        freelist::Freelist,
        page::{PageFrame, PageType},
        slotted::SlottedPage,
        test_support::{TempDatabase, test_pager},
        validate::validate_tree,
        wal::{Wal, format::CommitState, wal_path},
    };

    #[test]
    fn catalog_checkpoint_publishes_the_matching_snapshot_commit_state() {
        use super::super::catalog::{CatalogReader, types::*};
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        batch.allocate_append(PageType::TableLeaf).unwrap();
        batch.commit().unwrap();
        let featureless = pager.begin_read().unwrap();
        let mut batch = pager.begin_write().unwrap();
        CatalogWriter::bootstrap(&mut batch)
            .unwrap()
            .finish(&mut batch)
            .unwrap();
        batch.commit().unwrap();
        let mut old_v1 = pager.begin_read().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        for index in 0..48 {
            let id = writer.allocate_object_id(&mut batch).unwrap();
            let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
            let table = TableDef::new(
                TableId::new(id.get()).unwrap(),
                SchemaVersion::new(1).unwrap(),
                root,
                RowId::new(1).unwrap(),
                0,
                CatalogName::new(format!("T{index}")).unwrap(),
            )
            .unwrap();
            writer.insert_table_definition(&mut batch, &table).unwrap();
        }
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let latest = pager.begin_read().unwrap();
        assert_ne!(latest.catalog_root(), old_v1.catalog_root());
        assert_eq!(pager.checkpoint().unwrap().to_lsn, featureless.lsn());
        assert_eq!(
            super::checkpoint_state(&pager.published_read().unwrap().checkpoint),
            featureless.state
        );
        drop(featureless);
        assert_eq!(pager.checkpoint().unwrap().to_lsn, old_v1.lsn());
        assert_eq!(
            super::checkpoint_state(&pager.published_read().unwrap().checkpoint),
            old_v1.state
        );
        let old = CatalogReader::open_empty_or_v1(&mut old_v1).unwrap();
        assert!(old.find_table_by_id(TableId::new(1).unwrap()).is_none());
        old.validate_deep(&mut old_v1).unwrap();
        drop(old_v1);
        let latest_lsn = latest.lsn();
        drop(latest);
        drop(pager);
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut read = pager.begin_read().unwrap();
        assert_eq!(read.lsn(), latest_lsn);
        let recovered = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        assert!(
            recovered
                .find_table_by_id(TableId::new(48).unwrap())
                .is_some()
        );
        recovered.validate_deep(&mut read).unwrap();
    }

    #[test]
    fn recovery_preserves_featureless_legacy_catalog_trees() {
        for kind in [TreeKind::Table, TreeKind::Index] {
            let (temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let mut tree = BTree::create(&mut batch, kind).unwrap();
            tree.insert(&mut batch, b"legacy", b"not a V1 record")
                .unwrap();
            batch.set_catalog_root(tree.root()).unwrap();
            batch.commit().unwrap();
            drop(pager);
            for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
                let pager =
                    Pager::open(temp.path(), mode, PagerConfig::test_default(4096)).unwrap();
                let mut snapshot = pager.begin_read().unwrap();
                assert_eq!(snapshot.feature_bits(), 0);
                assert_eq!(
                    tree.get(&mut snapshot, b"legacy").unwrap(),
                    Some(b"not a V1 record".to_vec())
                );
            }
        }
    }

    #[test]
    fn catalog_update_gate_requires_an_active_batch_and_valid_v1_state() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        assert_eq!(
            batch.finish_catalog_update().unwrap_err().code(),
            ErrorCode::Transaction
        );
        batch.begin_catalog_update().unwrap();
        assert_eq!(
            batch.begin_catalog_update().unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            batch.finish_catalog_update().unwrap_err().code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            batch.validate_for_commit().unwrap_err().code(),
            ErrorCode::Transaction
        );
        batch.enable_feature_bits(CATALOG_V1).unwrap();
        assert_eq!(
            batch.finish_catalog_update().unwrap_err().code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            batch.validate_for_commit().unwrap_err().code(),
            ErrorCode::Transaction
        );
        batch.rollback().unwrap();
        assert_eq!(
            batch.begin_catalog_update().unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            batch.finish_catalog_update().unwrap_err().code(),
            ErrorCode::Transaction
        );
    }

    #[test]
    fn catalog_update_checkpoints_restore_both_pending_and_nonpending_states() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let empty = batch.checkpoint_mutation().unwrap();
        let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let pending = batch.checkpoint_mutation().unwrap();
        writer.finish(&mut batch).unwrap();
        batch.validate_for_commit().unwrap();
        batch.restore_mutation(pending);
        assert_eq!(
            batch.validate_for_commit().unwrap_err().code(),
            ErrorCode::Transaction
        );
        batch.restore_mutation(empty);
        assert_eq!(batch.feature_bits(), 0);
        assert_eq!(batch.catalog_root(), PageId(0));
        assert_eq!(batch.page_count().unwrap(), 1);
        batch.commit().unwrap();
        assert_eq!(pager.latest_lsn().unwrap(), Lsn(0));
    }

    #[test]
    fn finished_catalog_validation_survives_checkpoint_restore_and_clears_on_rollback() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let empty = batch.checkpoint_mutation().unwrap();
        CatalogWriter::bootstrap(&mut batch)
            .unwrap()
            .finish(&mut batch)
            .unwrap();
        let finished = batch.checkpoint_mutation().unwrap();
        batch.restore_mutation(empty);
        batch.validate_for_commit().unwrap();
        batch.restore_mutation(finished);
        let (mut tree, _) = batch
            .catalog_tree_with_report(TreeKind::Table)
            .unwrap()
            .unwrap();
        tree.delete(&mut batch, &[0x00]).unwrap();
        assert_eq!(
            batch.validate_for_commit().unwrap_err().code(),
            ErrorCode::Corruption
        );
        batch.rollback().unwrap();
        assert!(!batch.catalog_validation_required);
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
    }

    #[test]
    fn failed_bootstrap_allocation_restores_existing_batch_mutations_and_gate() {
        let temp = TempDatabase::new("catalog-bootstrap-allocation-limit");
        let mut config = PagerConfig::test_default(4096);
        config.max_database_bytes = super::super::header::SUPERBLOCK_AREA_LEN + 4096;
        let pager = Pager::create(temp.path(), config).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let unrelated = batch.allocate(PageType::Catalog).unwrap();
        batch.modify(unrelated).unwrap().bytes_mut()[100] = 0x5a;
        let before = batch.checkpoint_mutation().unwrap();
        let error = match CatalogWriter::bootstrap(&mut batch) {
            Ok(_) => panic!("catalog allocation exceeded the configured database limit"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_eq!(batch.candidate_header, before.candidate_header);
        assert_eq!(batch.new_pages, before.new_pages);
        assert_eq!(batch.pending_generations, before.pending_generations);
        assert_eq!(batch.pending_releases, before.pending_releases);
        assert_eq!(batch.tracked_trees, before.tracked_trees);
        assert_eq!(batch.read(unrelated).unwrap().bytes()[100], 0x5a);
        assert_eq!(batch.page_count().unwrap(), 2);
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        assert_eq!(read.feature_bits(), 0);
        assert_eq!(read.catalog_root(), PageId(0));
        assert_eq!(read.read(unrelated).unwrap().bytes()[100], 0x5a);
    }

    #[test]
    fn final_c1_initialized_database_never_recreates_a_missing_wal() {
        for committed in [true, false] {
            for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
                let temp = TempDatabase::new("final-c1-initialized");
                let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
                if committed {
                    let mut batch = pager.begin_write().unwrap();
                    batch.allocate_append(PageType::TableLeaf).unwrap();
                    batch.commit().unwrap();
                }
                drop(pager);
                let before = fs::read(temp.path()).unwrap();
                fs::remove_file(wal_path(temp.path())).unwrap();
                let error = match Pager::open(temp.path(), mode, PagerConfig::test_default(4096)) {
                    Ok(_) => panic!(
                        "initialized database recreated missing WAL: committed={committed}, mode={mode:?}"
                    ),
                    Err(error) => error,
                };
                assert_eq!(error.code(), ErrorCode::Io);
                assert_eq!(fs::read(temp.path()).unwrap(), before);
                assert!(!wal_path(temp.path()).exists());
            }
        }
    }

    #[test]
    fn final_c2_create_faults_cover_main_wal_names_and_marker_in_order() {
        let temp = TempDatabase::new("final-c2-create-trace");
        let trace = Arc::new(RecordingFaults::never());
        let pager =
            Pager::create_with_faults(temp.path(), PagerConfig::test_default(4096), trace.clone())
                .unwrap();
        drop(pager);
        let points = trace.recorded();
        assert_eq!(
            points,
            vec![
                FaultPoint::BeforeSuperblockWrite,
                FaultPoint::AfterSuperblockWrite,
                FaultPoint::BeforeMainSync,
                FaultPoint::AfterMainSync,
                FaultPoint::BeforeSuperblockWrite,
                FaultPoint::AfterSuperblockWrite,
                FaultPoint::BeforeMainSync,
                FaultPoint::AfterMainSync,
                FaultPoint::BeforeMainDirectorySync,
                FaultPoint::AfterMainDirectorySync,
                FaultPoint::BeforeWalHeaderWrite,
                FaultPoint::AfterWalHeaderWrite,
                FaultPoint::BeforeWalSync,
                FaultPoint::AfterWalSync,
                FaultPoint::BeforeWalHeaderWrite,
                FaultPoint::AfterWalHeaderWrite,
                FaultPoint::BeforeWalSync,
                FaultPoint::AfterWalSync,
                FaultPoint::BeforeWalDirectorySync,
                FaultPoint::AfterWalDirectorySync,
                FaultPoint::BeforeSuperblockWrite,
                FaultPoint::AfterSuperblockWrite,
                FaultPoint::BeforeMainSync,
                FaultPoint::AfterMainSync,
            ]
        );
        for occurrence in 1..=points.len() {
            let temp = TempDatabase::new("final-c2-create-failure");
            let faults = Arc::new(RecordingFaults::at(occurrence));
            let error = match Pager::create_with_faults(
                temp.path(),
                PagerConfig::test_default(4096),
                faults.clone(),
            ) {
                Ok(_) => panic!("create acknowledged failed durability boundary {occurrence}"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Io);
            assert_eq!(faults.recorded(), points[..occurrence]);
            // Error paths may leave an incomplete pair; they never report success.
            // If the main initial pair exists and WAL was rolled back, RW open
            // can safely finish creation and durably mark it before returning.
            if occurrence >= 9 {
                let recovered = Pager::open(
                    temp.path(),
                    OpenMode::ReadWrite,
                    PagerConfig::test_default(4096),
                )
                .unwrap();
                assert_eq!(recovered.latest_lsn().unwrap(), Lsn(0));
                drop(recovered);
            }
        }
    }

    #[test]
    fn final_c1_read_write_open_finishes_initialization_marker() {
        let temp = TempDatabase::new("final-c1-unmarked");
        let file = DatabaseFile::create(temp.path(), 4096, [0x5a; 16], DEFAULT_MAX_DATABASE_BYTES)
            .unwrap();
        let wal = Wal::create(temp.path(), &file.header()).unwrap();
        drop(wal);
        drop(file);
        let initial = fs::read(temp.path()).unwrap();
        let reader = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        drop(reader);
        assert_eq!(fs::read(temp.path()).unwrap(), initial);
        let writer = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        drop(writer);
        fs::remove_file(wal_path(temp.path())).unwrap();
        assert!(
            Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096)
            )
            .is_err(),
            "successful open must finish initialization before returning"
        );
    }

    #[test]
    fn commit_changes_only_the_wal_until_checkpoint() {
        let temp = TempDatabase::new("pager-wal-only-commit");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        // The database's exclusive cross-process lock prevents a second Windows file handle
        // from reading it. Read through its owning handle while preserving the same bytes
        // assertion required for a WAL-only commit.
        let before = database_bytes(&pager);

        {
            let mut batch = pager.begin_write().expect("begin write");
            batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate a page");
            batch.commit().expect("commit page through WAL");
        }

        assert_eq!(database_bytes(&pager), before);
        assert!(
            fs::metadata(wal_path(temp.path()))
                .expect("read WAL metadata")
                .len()
                > 8192
        );
    }

    #[test]
    fn final_i3_commit_trace_observes_each_real_record_and_partial_prefix() {
        let temp = TempDatabase::new("final-i3-trace");
        let faults = Arc::new(RecordingFaults::never());
        let pager = pager_with_faults(&temp, faults.clone());
        let mut batch = pager.begin_write().unwrap();
        for _ in 0..3 {
            batch.allocate_append(PageType::TableLeaf).unwrap();
        }
        batch.commit().unwrap();
        let trace = faults
            .recorded()
            .iter()
            .map(|point| format!("{point:?}"))
            .collect::<Vec<_>>();
        for point in [
            "BeforeWalRecordWrite",
            "AfterWalRecordHeaderPrefixWrite",
            "AfterWalRecordPayloadPrefixWrite",
            "AfterWalRecordWrite",
        ] {
            assert_eq!(
                trace.iter().filter(|name| *name == point).count(),
                4,
                "three PageFrames and one Commit must each reach {point}"
            );
        }
        assert_eq!(
            &trace[trace.len() - 4..],
            &[
                "BeforeWalSync",
                "AfterWalSync",
                "BeforeWalPublish",
                "AfterWalPublish"
            ]
        );
    }

    #[test]
    fn final_i4_invalid_index_candidate_fails_before_any_wal_io() {
        let temp = TempDatabase::new("final-i4-preflight");
        let faults = Arc::new(RecordingFaults::never());
        let pager = pager_with_faults(&temp, faults.clone());
        let state = CommitState {
            page_count: 2,
            catalog_root: PageId(0),
            freelist_root: PageId(0),
            feature_bits: 0,
        };
        pager
            .published_write()
            .unwrap()
            .wal_index
            .publish(Lsn(1), Some(state), &[(PageId(1), 8192, 1)])
            .unwrap();
        let before = fs::read(wal_path(temp.path())).unwrap();
        let mut batch = pager.begin_write().unwrap();
        batch.allocate_append(PageType::TableLeaf).unwrap();
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Corruption);
        assert!(
            faults.recorded().is_empty(),
            "invalid index candidate reached WAL I/O: {:?}",
            faults.recorded()
        );
        assert_eq!(fs::read(wal_path(temp.path())).unwrap(), before);
        assert_eq!(pager.latest_lsn().unwrap(), Lsn(0));
    }

    #[test]
    fn final_i4_durable_commit_does_not_wait_for_cache_lock() {
        use std::{sync::mpsc, time::Duration};
        struct SyncGate {
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }
        impl FaultInjector for SyncGate {
            fn check(&self, point: FaultPoint) -> Result<()> {
                if point == FaultPoint::AfterWalSync {
                    self.entered.send(()).unwrap();
                    self.resume.lock().unwrap().recv().unwrap();
                }
                Ok(())
            }
        }
        let temp = TempDatabase::new("final-i4-cache-gate");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
            .unwrap()
            .with_faults(Arc::new(SyncGate {
                entered: entered_tx,
                resume: Mutex::new(resume_rx),
            }));
        let worker = pager.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut batch = worker.begin_write().unwrap();
            batch.allocate_append(PageType::TableLeaf).unwrap();
            done_tx.send(batch.commit()).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let cache = pager.cache_lock().unwrap();
        resume_tx.send(()).unwrap();
        let committed = done_rx.recv_timeout(Duration::from_secs(2));
        drop(cache); // always release before joining, including the regression failure
        writer.join().unwrap();
        committed
            .expect("durable commit blocked on best-effort cache publication")
            .unwrap();
        assert_eq!(pager.latest_lsn().unwrap(), Lsn(1));
    }

    #[test]
    fn checkpoint_registration_races_obey_published_then_registry_lock_order() {
        use std::{
            sync::{
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            time::Duration,
        };

        struct PauseAt {
            point: FaultPoint,
            armed: AtomicBool,
            entered: mpsc::Sender<()>,
            resume: Mutex<mpsc::Receiver<()>>,
        }
        impl FaultInjector for PauseAt {
            fn check(&self, point: FaultPoint) -> Result<()> {
                if point == self.point && self.armed.swap(false, Ordering::SeqCst) {
                    self.entered.send(()).unwrap();
                    self.resume
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| {
                            DbError::new(ErrorCode::Internal, "checkpoint pause timed out")
                        })?;
                }
                Ok(())
            }
        }

        for pause_point in [FaultPoint::BeforeMainSync, FaultPoint::BeforeWalHeaderWrite] {
            let temp = TempDatabase::new("checkpoint-registration-race");
            let (entered_tx, entered_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let faults = Arc::new(PauseAt {
                point: pause_point,
                armed: AtomicBool::new(false),
                entered: entered_tx,
                resume: Mutex::new(resume_rx),
            });
            let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
                .unwrap()
                .with_faults(faults.clone());
            let page = {
                let mut batch = pager.begin_write().unwrap();
                let page = batch.allocate_append(PageType::Catalog).unwrap();
                batch.modify(page).unwrap().bytes_mut()[100] = 77;
                batch.commit().unwrap();
                page
            };
            faults.armed.store(true, Ordering::SeqCst);
            let worker = pager.clone();
            let (checkpoint_tx, checkpoint_rx) = mpsc::channel();
            let checkpoint_thread =
                std::thread::spawn(move || checkpoint_tx.send(worker.checkpoint()).unwrap());
            entered_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("checkpoint reached exact persistence boundary");
            if pause_point == FaultPoint::BeforeMainSync {
                // This registration wins the race while materialization holds no state lock.
                let mut snapshot = pager.begin_read().unwrap();
                resume_tx.send(()).unwrap();
                let result = checkpoint_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("no lock inversion")
                    .unwrap();
                assert!(!result.wal_reset);
                assert_eq!(snapshot.read(page).unwrap().bytes()[100], 77);
                assert!(std::fs::metadata(wal_path(temp.path())).unwrap().len() > 8192);
                drop(snapshot);
            } else {
                // The reset has the published write lock before taking EmptySnapshotGuard.
                assert!(matches!(
                    pager.inner.published.try_read(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                let reader = pager.clone();
                let (attempt_tx, attempt_rx) = mpsc::channel();
                let (read_tx, read_rx) = mpsc::channel();
                let reader_thread = std::thread::spawn(move || {
                    attempt_tx.send(()).unwrap();
                    read_tx.send(reader.begin_read()).unwrap();
                });
                attempt_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(matches!(read_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
                resume_tx.send(()).unwrap();
                let result = checkpoint_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("reset cannot deadlock with registration")
                    .unwrap();
                assert!(result.wal_reset);
                let mut snapshot = read_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("registration resumes after reset")
                    .unwrap();
                assert_eq!(snapshot.lsn(), Lsn(1));
                assert_eq!(snapshot.read(page).unwrap().bytes()[100], 77);
                reader_thread.join().unwrap();
            }
            checkpoint_thread.join().unwrap();
            assert_eq!(pager.read(page).unwrap().bytes()[100], 77);
        }
    }

    #[test]
    fn synced_commit_reopens_from_wal() {
        let temp = TempDatabase::new("pager-wal-reopen");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create catalog tree");
            tree.insert(&mut batch, b"wal-key", b"wal-value")
                .expect("insert catalog value");
            batch
                .track_tree(TreeKind::Table, tree.root())
                .expect("track catalog tree");
            batch
                .set_catalog_root(tree.root())
                .expect("set catalog root");
            batch.commit().expect("commit catalog tree");
            tree.root()
        };
        drop(pager);

        let reopened = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .expect("reopen pager from WAL");
        assert_eq!(reopened.latest_lsn().expect("read latest LSN"), Lsn(1));
        let mut snapshot = reopened.begin_read().expect("begin read snapshot");
        let tree = snapshot
            .catalog_tree(TreeKind::Table)
            .expect("recover catalog tree")
            .expect("catalog root is present");
        assert_eq!(tree.root(), root);
        assert_eq!(
            tree.get(&mut snapshot, b"wal-key")
                .expect("read recovered catalog value"),
            Some(b"wal-value".to_vec())
        );
    }

    #[test]
    fn snapshot_catalog_tree_keeps_its_root_across_a_later_split() {
        let temp = TempDatabase::new("pager-snapshot-catalog-root");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let mut tree = {
            let mut batch = pager.begin_write().expect("begin initial batch");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            for index in 0..4u8 {
                let key = format!("k{index:03}").into_bytes();
                tree.insert(&mut batch, &key, &[index; 700])
                    .expect("insert initial value");
            }
            batch
                .set_catalog_root(tree.root())
                .expect("publish initial root");
            batch.commit().expect("commit initial root");
            tree
        };

        let mut old_snapshot = pager.begin_read().expect("begin LSN-1 snapshot");
        let old_catalog = old_snapshot
            .catalog_tree(TreeKind::Table)
            .expect("resolve catalog through old snapshot")
            .expect("catalog root exists");

        {
            let mut batch = pager.begin_write().expect("begin split batch");
            for index in 4..30u8 {
                let key = format!("k{index:03}").into_bytes();
                tree.insert(&mut batch, &key, &[index; 700])
                    .expect("grow catalog tree");
            }
            assert_ne!(
                tree.root(),
                old_catalog.root(),
                "fixture must split the root"
            );
            batch
                .set_catalog_root(tree.root())
                .expect("publish split root");
            batch.commit().expect("commit split root");
        }

        assert_eq!(
            old_catalog
                .get(&mut old_snapshot, b"k003")
                .expect("read old tree through old snapshot"),
            Some(vec![3; 700])
        );
    }

    #[test]
    fn old_snapshot_keeps_old_page_after_new_commit() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin first write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate page");
            batch.modify(page).expect("modify first image").bytes_mut()[100] = 0x11;
            batch.commit().expect("commit first image");
            page
        };
        let mut old_snapshot = pager.begin_read().expect("begin LSN-1 snapshot");
        assert_eq!(old_snapshot.lsn(), Lsn(1));

        {
            let mut batch = pager.begin_write().expect("begin second write");
            batch
                .modify(page)
                .expect("modify replacement image")
                .bytes_mut()[100] = 0x22;
            batch.commit().expect("commit replacement image");
        }

        let mut new_snapshot = pager.begin_read().expect("begin LSN-2 snapshot");
        assert_eq!(new_snapshot.lsn(), Lsn(2));
        assert_eq!(
            old_snapshot
                .read(page)
                .expect("read page through old snapshot")
                .bytes()[100],
            0x11
        );
        assert_eq!(
            new_snapshot
                .read(page)
                .expect("read page through new snapshot")
                .bytes()[100],
            0x22
        );
    }

    #[test]
    fn committed_feature_bits_are_visible_only_to_new_snapshots() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
        let old_snapshot = pager.begin_read().expect("begin old snapshot");
        assert_eq!(old_snapshot.feature_bits(), 0);

        let mut batch = pager.begin_write().expect("begin feature write");
        assert_eq!(batch.feature_bits(), 0);
        batch
            .enable_feature_bits(CATALOG_V1)
            .expect("enable catalog feature");
        assert_eq!(batch.feature_bits(), CATALOG_V1);
        batch
            .allocate_append(PageType::Catalog)
            .expect("allocate catalog page");
        batch.commit().expect("commit catalog feature");

        let new_snapshot = pager.begin_read().expect("begin new snapshot");
        assert_eq!(old_snapshot.feature_bits(), 0);
        assert_eq!(new_snapshot.feature_bits(), CATALOG_V1);
    }

    #[test]
    fn wal_sync_failure_is_not_published_and_blocks_later_writes() {
        let temp = TempDatabase::new("pager-wal-sync-failure");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin initial write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate initial page");
            batch.modify(page).expect("modify initial page").bytes_mut()[100] = 0x33;
            batch.commit().expect("commit initial page");
            page
        };
        let faults = Arc::new(RecordingFaults::at(9));
        let pager = pager.with_faults(faults.clone());
        let mut old_snapshot = pager.begin_read().expect("begin old snapshot");

        let error = {
            let mut batch = pager.begin_write().expect("begin failing write");
            batch
                .modify(page)
                .expect("modify replacement page")
                .bytes_mut()[100] = 0x44;
            batch.commit().expect_err("WAL sync fault must fail commit")
        };
        assert_eq!(error.code(), ErrorCode::Io);
        assert_eq!(faults.recorded(), commit_fault_points()[..9]);
        assert_eq!(pager.latest_lsn().expect("read latest LSN"), Lsn(1));
        assert_eq!(
            old_snapshot
                .read(page)
                .expect("old snapshot remains readable")
                .bytes()[100],
            0x33
        );
        let write_error = match pager.try_begin_write() {
            Ok(_) => panic!("failed WAL sync requires recovery before a later write"),
            Err(error) => error,
        };
        assert_eq!(write_error.code(), ErrorCode::Transaction);
        assert!(write_error.to_string().contains("recovery"));
    }

    #[test]
    fn cold_snapshot_reads_published_wal_page_after_os_append_failure() {
        let temp = TempDatabase::new("pager-wal-os-append-failure");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin initial write");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate initial page");
            batch.modify(page).expect("modify initial page").bytes_mut()[100] = 0x33;
            batch.commit().expect("commit initial page");
            page
        };
        let mut old_snapshot = pager.begin_read().expect("begin old snapshot");

        let checkpoint = pager
            .published_read()
            .expect("read checkpoint header")
            .checkpoint;
        let read_only_wal =
            Wal::open(temp.path(), &checkpoint, OpenMode::ReadOnly).expect("open read-only WAL");
        *pager.wal_lock().expect("lock WAL") = read_only_wal;

        let error = {
            let mut batch = pager.begin_write().expect("begin failing write");
            batch
                .modify(page)
                .expect("modify replacement page")
                .bytes_mut()[100] = 0x44;
            batch
                .commit()
                .expect_err("read-only WAL append must fail through write_all")
        };
        assert_eq!(error.code(), ErrorCode::Io);
        pager
            .cache_lock()
            .expect("lock page cache")
            .remove_page_versions(page.page_id());

        assert_eq!(
            old_snapshot
                .read(page)
                .expect("published WAL prefix remains readable after failed append")
                .bytes()[100],
            0x33
        );
    }

    #[test]
    fn lsn_overflow_rejects_before_wal_io() {
        let temp = TempDatabase::new("pager-lsn-overflow");
        let mut database =
            DatabaseFile::create(temp.path(), 4096, [0x6d; 16], DEFAULT_MAX_DATABASE_BYTES)
                .expect("create database file");
        let checkpoint = database
            .header()
            .with_checkpoint(
                Lsn(u64::MAX - 1),
                1,
                PageId(0),
                PageId(0),
                database.header().wal_generation(),
            )
            .expect("build pre-overflow checkpoint");
        database
            .publish_superblock(checkpoint)
            .expect("publish pre-overflow checkpoint");
        let mut wal = Wal::create(temp.path(), &checkpoint).expect("create WAL");
        let page = PageRef::new(PageId(1), 1).expect("page reference");
        let mut frame =
            PageFrame::new_zeroed(PageType::TableLeaf, page, 4096).expect("create page frame");
        SlottedPage::open(&mut frame, LEAF_METADATA_END).expect("initialize page frame");
        frame.prepare_for_write().expect("prepare page frame");
        wal.append_transaction(
            Lsn(u64::MAX),
            Lsn(u64::MAX - 1),
            CommitState {
                page_count: 2,
                catalog_root: PageId(0),
                freelist_root: PageId(0),
                feature_bits: checkpoint.features(),
            },
            &[(page.page_id(), page.generation(), frame.bytes().to_vec())],
        )
        .expect("seed recovered max-LSN transaction");
        wal.sync_all().expect("sync max-LSN transaction");
        drop(wal);
        drop(database);

        let before = fs::read(wal_path(temp.path())).expect("read WAL before overflow attempt");
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .expect("recover max-LSN pager");
        assert_eq!(pager.latest_lsn().expect("read latest LSN"), Lsn(u64::MAX));
        let error = {
            let mut batch = pager.begin_write().expect("begin overflowing write");
            batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate page before commit");
            batch.commit().expect_err("LSN overflow must reject commit")
        };
        assert_eq!(error.code(), ErrorCode::Transaction);
        assert_eq!(
            fs::read(wal_path(temp.path())).expect("read WAL after overflow attempt"),
            before
        );
    }

    #[test]
    fn reopen_rejects_wal_recovered_pages_above_the_configured_database_limit() {
        let temp = TempDatabase::new("pager-recovered-size-limit");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        {
            let mut batch = pager.begin_write().expect("begin write");
            batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate WAL-only page");
            batch.commit().expect("commit WAL-only page");
        }
        drop(pager);

        let mut limited = PagerConfig::test_default(4096);
        limited.max_database_bytes = 8192;
        let error = match Pager::open(temp.path(), OpenMode::ReadOnly, limited) {
            Ok(_) => panic!("recovery must reject a logical page beyond the configured limit"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Corruption);
    }

    #[test]
    fn final_i2_open_checks_wal_limit_before_scanning_or_truncating() {
        for corrupt_record in [false, true] {
            for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
                let temp = TempDatabase::new("final-i2-wal-limit");
                let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
                let mut batch = pager.begin_write().unwrap();
                batch.allocate_append(PageType::TableLeaf).unwrap();
                batch.commit().unwrap();
                drop(pager);
                let path = wal_path(temp.path());
                let mut bytes = fs::read(&path).unwrap();
                if corrupt_record {
                    bytes[8192] ^= 1;
                }
                bytes.push(0); // a scanner would truncate this tail in RW mode
                fs::write(&path, &bytes).unwrap();
                let main = fs::read(temp.path()).unwrap();
                let mut config = PagerConfig::test_default(4096);
                config.max_wal_bytes = 8192;
                let error = match Pager::open(temp.path(), mode, config) {
                    Ok(_) => panic!("WAL above the configured limit was scanned"),
                    Err(error) => error,
                };
                assert!(
                    error
                        .to_string()
                        .contains("WAL file exceeds configured maximum size"),
                    "must reject size before parsing the corrupt record: {error}"
                );
                assert_eq!(fs::read(&path).unwrap(), bytes);
                assert_eq!(fs::read(temp.path()).unwrap(), main);
            }
        }
    }

    #[test]
    fn refreshing_one_tracked_tree_preserves_another_same_kind_tree() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
        let mut batch = pager.begin_write().expect("begin write");
        let mut growing = BTree::create(&mut batch, TreeKind::Table).expect("create growing tree");
        let mut other = BTree::create(&mut batch, TreeKind::Table).expect("create other tree");
        other
            .insert(&mut batch, b"other", b"value")
            .expect("track other tree");
        let other_root = other.root();
        let old_growing_root = growing.root();

        for value in 0..8u32 {
            let mut key = vec![0x61; 1024];
            key[..4].copy_from_slice(&value.to_be_bytes());
            growing
                .insert(&mut batch, &key, b"v")
                .expect("grow first tree root");
        }

        assert_ne!(growing.root(), old_growing_root);
        assert!(batch.tracked_trees.contains(&(TreeKind::Table, other_root)));
        assert!(
            batch
                .tracked_trees
                .contains(&(TreeKind::Table, growing.root()))
        );
        assert!(
            !batch
                .tracked_trees
                .contains(&(TreeKind::Table, old_growing_root))
        );
        assert_eq!(batch.tracked_trees.len(), 2);
    }

    #[test]
    fn uncommitted_dirty_pages_are_invisible_after_rollback() {
        let temp = TempDatabase::new("pager-private");
        let pager = Pager::create(temp.path(), PagerConfig::new(4096, [1; 16]).unwrap())
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
        let pager =
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
            batch.read(page).expect("batch overlay read").bytes()[100],
            0x7b
        );
        batch.rollback().expect("rollback");
    }

    #[test]
    fn commit_publishes_dirty_page_and_replaces_clean_cache() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
        assert_eq!(pager.cache_lock().expect("lock cache").len(), 1);
        let first = pager.read(page).expect("read committed page");
        assert_eq!(first.bytes()[100], 0x2a);
        let second = pager.read(page).expect("read cached page");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn modifying_a_committed_page_keeps_existing_clean_handles_unchanged() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
                batch.read(page).expect("read dirty copy").bytes()[100],
                0x22
            );
            batch.commit().expect("commit dirty copy");
        }

        assert_eq!(clean_handle.bytes()[100], 0x11);
        let current_handle = pager.read(page).expect("read replacement image");
        assert_eq!(current_handle.bytes()[100], 0x22);
        assert!(!Arc::ptr_eq(&clean_handle, &current_handle));
    }

    #[test]
    fn held_clean_handle_allows_batch_modify_without_a_refcell_panic() {
        let (_temp, pager) = test_pager(4096).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin seed batch");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate seed page");
            batch.commit().expect("commit seed page");
            page
        };
        let handle = pager.read(page).expect("hold clean cache handle");
        let _held_frame = handle.as_ref();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut batch = pager.begin_write()?;
            batch.modify(page)?;
            batch.rollback()
        }));

        result
            .expect("batch modify must not panic while a clean frame is immutably borrowed")
            .expect("batch modify must retain normal supported behavior");
    }

    #[test]
    fn held_immutable_handle_does_not_block_reads_or_writes() {
        let (_temp, pager) = test_pager(4096).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin seed batch");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate seed page");
            batch.modify(page).expect("modify seed page").bytes_mut()[100] = 0x11;
            batch.commit().expect("commit seed page");
            page
        };
        let held_handle = pager.read(page).expect("hold clean cache handle");

        let mut batch = pager.begin_write().expect("begin replacement write");
        batch
            .modify(page)
            .expect("modify replacement image")
            .bytes_mut()[100] = 0x22;
        batch.commit().expect("commit replacement image");

        assert_eq!(held_handle.bytes()[100], 0x11);
        let current_handle = pager.read(page).expect("read replacement image");
        assert_eq!(current_handle.bytes()[100], 0x22);
    }

    #[test]
    fn read_only_open_allows_reads_and_rejects_write_batches() {
        let temp = TempDatabase::new("pager-read-only");
        let writable = Pager::create(temp.path(), PagerConfig::test_default(4096))
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

        let read_only = Pager::open(
            temp.path(),
            super::super::file::OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .expect("open read only");

        assert_eq!(
            read_only.read(page).expect("read committed page").bytes()[100],
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
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin write");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("create catalog tree");
            let root = tree.root();
            batch
                .track_tree(TreeKind::Table, root)
                .expect("register catalog tree");
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
        let mut snapshot = reopened.begin_read().expect("begin read snapshot");
        assert_eq!(
            snapshot
                .catalog_tree(TreeKind::Table)
                .expect("recover catalog tree")
                .expect("catalog root exists")
                .root(),
            root
        );
    }

    #[test]
    fn catalog_tree_persists_mixed_values_and_read_only_reopen_never_changes_bytes() {
        for page_size in [4096, 8192, 16384] {
            let temp = TempDatabase::new("pager-catalog-reopen");
            let pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))
                .expect("create pager");
            let mut expected = Vec::new();
            let root = {
                let mut batch = pager.begin_write().expect("begin write");
                let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
                let mut inserted = 0u16;
                while !root_has_internal_child(&mut batch, tree.root())
                    .expect("inspect batch tree height")
                {
                    assert!(
                        inserted < 512,
                        "tree must reach three levels within the fixture bound"
                    );
                    let key = wide_key(inserted);
                    let value = if inserted % 31 == 0 {
                        vec![u8::try_from(inserted % 251).expect("byte"); 5000]
                    } else {
                        vec![u8::try_from(inserted % 251).expect("byte")]
                    };
                    tree.insert(&mut batch, &key, &value)
                        .expect("insert tree value");
                    if inserted % 9 != 0 {
                        expected.push((key, value));
                    }
                    inserted += 1;
                }
                for index in (0..inserted).filter(|index| index % 9 == 0) {
                    let key = wide_key(index);
                    assert!(
                        tree.delete(&mut batch, &key)
                            .expect("delete tree value")
                            .is_some()
                    );
                }
                batch
                    .set_catalog_root(tree.root())
                    .expect("publish catalog root");
                let root = tree.root();
                batch.commit().expect("commit catalog tree");
                root
            };
            drop(pager);

            let before_second_open = std::fs::read(temp.path()).expect("read committed bytes");
            let reopened = Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(page_size),
            )
            .expect("open persisted pager");
            let mut snapshot = reopened.begin_read().expect("begin read snapshot");
            let tree = snapshot
                .catalog_tree(TreeKind::Table)
                .expect("recover catalog tree")
                .expect("catalog tree exists");
            assert_eq!(tree.root(), root);
            assert!(
                root_has_internal_child(&mut snapshot, tree.root())
                    .expect("recovered tree has three levels"),
                "page size {page_size} must retain an internal child below the root",
            );
            let mut actual = Vec::new();
            let mut cursor = tree
                .scan(&mut snapshot, Bound::Unbounded, Bound::Unbounded)
                .expect("scan recovered tree");
            while let Some(pair) = cursor.next().expect("read recovered pair") {
                actual.push(pair);
            }
            assert_eq!(actual, expected, "page size {page_size}");
            drop(cursor);
            drop(snapshot);
            drop(reopened);

            let second_open = Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(page_size),
            )
            .expect("second read-only open");
            assert_eq!(
                database_bytes(&second_open),
                before_second_open,
                "read-only reopen must not write at page size {page_size}",
            );
        }
    }

    #[test]
    fn commit_preflight_rejects_a_tracked_tree_with_a_bad_separator_before_file_io() {
        let temp = TempDatabase::new("pager-tree-preflight");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = database_bytes(&pager);
        let error = {
            let mut batch = pager.begin_write().expect("begin write");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            for index in 0..30u8 {
                let key = format!("k{index:03}").into_bytes();
                tree.insert(&mut batch, &key, &[index; 700])
                    .expect("insert value");
            }
            let root = tree.root();
            let frame = batch.modify(root).expect("modify internal root");
            {
                let mut page =
                    SlottedPage::open(frame, INTERNAL_METADATA_END).expect("open root slots");
                let original = InternalCell::decode(&page.cell(0).expect("read separator"))
                    .expect("decode separator");
                page.remove(0).expect("remove separator");
                let replacement = InternalCell::new(b"z".to_vec(), original.right())
                    .expect("bad replacement separator");
                page.insert(0, &replacement.encode().expect("encode replacement"))
                    .expect("insert replacement separator");
            }
            batch.commit().expect_err("bad tracked tree must reject")
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_file_is_unchanged(&pager, &before);
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn validation_preserves_busy_when_every_cache_slot_is_pinned() {
        let temp = TempDatabase::new("pager-validation-busy");
        let mut config = PagerConfig::test_default(4096);
        config.cache_pages = 1;
        let pager = Pager::create(temp.path(), config).expect("create one-slot pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin tree batch");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            for index in 0..30u8 {
                tree.insert(&mut batch, &wide_key(u16::from(index)), &[index; 700])
                    .expect("insert enough rows to make an internal root");
            }
            batch.commit().expect("commit tree");
            tree
        };
        drop(pager);

        let mut config = PagerConfig::test_default(4096);
        config.cache_pages = 1;
        let pager =
            Pager::open(temp.path(), OpenMode::ReadWrite, config).expect("reopen one-slot pager");
        let held = pager
            .read(tree.root())
            .expect("pin tree root in the only cache slot");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut snapshot = pager.begin_read()?;
            validate_tree(&mut snapshot, &tree)
        }));

        let error = result
            .expect("validation must not panic on a fully pinned cache")
            .expect_err("validation must report the full cache");
        assert_eq!(error.code(), ErrorCode::Busy);
        drop(held);
    }

    #[test]
    fn held_clean_handle_allows_validation_without_a_refcell_panic() {
        let (_temp, pager) = test_pager(4096).expect("create pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin tree batch");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            tree.insert(&mut batch, b"held", b"value")
                .expect("insert tree value");
            batch.commit().expect("commit tree");
            tree
        };
        let handle = pager.read(tree.root()).expect("hold clean root handle");
        let _held_frame = handle.as_ref();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut snapshot = pager.begin_read()?;
            validate_tree(&mut snapshot, &tree)
        }));

        result
            .expect("validation must not panic while a clean frame is immutably borrowed")
            .expect("validation must retain normal supported behavior");
    }

    #[test]
    fn set_catalog_root_rejects_an_existing_tree_rightmost_leaf_before_file_io() {
        let temp = TempDatabase::new("pager-catalog-non-root");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin tree batch");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            for index in 0..30u8 {
                let key = format!("k{index:03}").into_bytes();
                tree.insert(&mut batch, &key, &[index; 700])
                    .expect("insert tree value");
            }
            batch.set_catalog_root(tree.root()).expect("publish root");
            batch.commit().expect("commit tree");
            tree
        };
        let before = database_bytes(&pager);

        let error = {
            let mut batch = pager.begin_write().expect("begin replacement batch");
            batch
                .track_tree(TreeKind::Table, tree.root())
                .expect("register current root");
            let rightmost = {
                let root = batch.modify(tree.root()).expect("read root");
                let cell_count = usize::from(root.header().cell_count());
                let slots = SlottedPage::open(root, INTERNAL_METADATA_END).expect("open root");
                InternalCell::decode(
                    &slots
                        .cell(cell_count - 1)
                        .expect("read rightmost separator"),
                )
                .expect("decode rightmost separator")
                .right()
            };
            batch
                .set_catalog_root(rightmost)
                .expect_err("non-root leaf cannot become catalog root")
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_file_is_unchanged(&pager, &before);
        assert_eq!(
            pager
                .published_read()
                .expect("read published state")
                .latest
                .catalog_root,
            tree.root().page_id()
        );
    }

    #[test]
    fn set_catalog_root_rejects_a_registered_committed_descendant() {
        let temp = TempDatabase::new("pager-registered-catalog-descendant");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let tree = {
            let mut batch = pager.begin_write().expect("begin tree batch");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            for index in 0..30u8 {
                tree.insert(&mut batch, &wide_key(u16::from(index)), &[index; 700])
                    .expect("insert tree value");
            }
            batch.set_catalog_root(tree.root()).expect("publish root");
            batch.commit().expect("commit tree");
            tree
        };
        let before = database_bytes(&pager);
        let error = {
            let mut batch = pager.begin_write().expect("begin replacement batch");
            let descendant = rightmost_child(&mut batch, tree.root());
            batch
                .track_tree(TreeKind::Table, descendant)
                .expect("register the committed descendant");
            batch
                .set_catalog_root(descendant)
                .expect_err("registered committed descendants must still be rejected")
        };

        assert_eq!(error.code(), ErrorCode::Corruption);
        assert!(
            error
                .to_string()
                .contains("reachable non-root catalog tree page cannot become the catalog root")
        );
        assert_file_is_unchanged(&pager, &before);
    }

    #[test]
    fn catalog_root_split_can_publish_and_reopen_the_new_tracked_root() {
        let temp = TempDatabase::new("pager-catalog-root-split");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let mut tree = {
            let mut batch = pager.begin_write().expect("begin initial batch");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            for index in 0..4u8 {
                let key = format!("k{index:03}").into_bytes();
                tree.insert(&mut batch, &key, &[index; 700])
                    .expect("insert initial value");
            }
            batch
                .set_catalog_root(tree.root())
                .expect("publish initial root");
            batch.commit().expect("commit initial root");
            tree
        };
        let original_root = tree.root();

        {
            let mut batch = pager.begin_write().expect("begin split batch");
            for index in 4..30u8 {
                let key = format!("k{index:03}").into_bytes();
                tree.insert(&mut batch, &key, &[index; 700])
                    .expect("insert split value");
            }
            assert_ne!(
                tree.root(),
                original_root,
                "fixture must split the catalog root"
            );
            batch
                .set_catalog_root(tree.root())
                .expect("publish the split root");
            batch.commit().expect("commit split root");
        }
        drop(pager);

        let reopened = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .expect("reopen split tree");
        let mut snapshot = reopened.begin_read().expect("begin read snapshot");
        let recovered = snapshot
            .catalog_tree(TreeKind::Table)
            .expect("recover split tree")
            .expect("catalog root exists");
        let mut cursor = recovered
            .scan(&mut snapshot, Bound::Unbounded, Bound::Unbounded)
            .expect("scan recovered split tree");
        let mut count = 0u8;
        while cursor.next().expect("read recovered value").is_some() {
            count += 1;
        }
        assert_eq!(count, 30);
    }

    #[test]
    fn unchanged_registered_catalog_root_can_be_published_again() {
        let temp = TempDatabase::new("pager-catalog-unchanged-root");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin initial batch");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
            batch
                .track_tree(TreeKind::Table, tree.root())
                .expect("register root");
            batch.set_catalog_root(tree.root()).expect("publish root");
            batch.commit().expect("commit root");
            tree.root()
        };

        {
            let mut batch = pager.begin_write().expect("begin unchanged-root batch");
            batch
                .track_tree(TreeKind::Table, root)
                .expect("register old root");
            batch.set_catalog_root(root).expect("republish old root");
            batch.commit().expect("commit unchanged root");
        }
        drop(pager);

        let reopened = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .expect("reopen unchanged root");
        let mut snapshot = reopened.begin_read().expect("begin read snapshot");
        assert_eq!(
            snapshot
                .catalog_tree(TreeKind::Table)
                .expect("recover unchanged root")
                .expect("catalog root exists")
                .root(),
            root
        );
    }

    #[test]
    fn catalog_recovery_rejects_a_tampered_checkpoint_that_regresses_wal_page_count() {
        let temp = TempDatabase::new("pager-catalog-freelist-overlap");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        {
            let mut batch = pager.begin_write().expect("begin catalog batch");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("create catalog tree");
            batch
                .track_tree(TreeKind::Table, tree.root())
                .expect("register catalog tree");
            batch
                .set_catalog_root(tree.root())
                .expect("publish catalog tree");
            batch.commit().expect("commit catalog tree");
        }
        let (first_free, second_free) = {
            let mut batch = pager.begin_write().expect("begin free-tree batch");
            let first = BTree::create(&mut batch, TreeKind::Table).expect("create first free tree");
            let second =
                BTree::create(&mut batch, TreeKind::Table).expect("create second free tree");
            batch.commit().expect("commit free trees");
            (first.root(), second.root())
        };
        {
            let mut batch = pager.begin_write().expect("begin release batch");
            batch
                .release_btree_later(first_free)
                .expect("release first tree root");
            batch
                .release_btree_later(second_free)
                .expect("release second tree root");
            batch.commit().expect("commit freelist leaves");
        }
        assert_ne!(
            pager
                .published_read()
                .expect("published state")
                .latest
                .catalog_root,
            second_free.page_id()
        );
        // Tampering the checkpoint-0 page count to a future count would require
        // shrinking at WAL commit 1. Recovery must reject that inconsistent state.
        let recovered_page_count = pager.page_count();
        let mut main = pager.main_lock().expect("lock main database");
        main.extend_to_pages(recovered_page_count)
            .expect("extend stale main database");
        let header = main
            .header()
            .with_checkpoint(
                main.header().checkpoint_lsn(),
                recovered_page_count,
                second_free.page_id(),
                main.header().freelist_root(),
                main.header().wal_generation(),
            )
            .expect("build stale main superblock");
        main.publish_superblock(header)
            .expect("publish stale main superblock");
        drop(main);
        drop(pager);

        let main = fs::read(temp.path()).unwrap();
        let wal = fs::read(wal_path(temp.path())).unwrap();
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let error = match Pager::open(temp.path(), mode, PagerConfig::test_default(4096)) {
                Ok(_) => panic!("tampered checkpoint accepted"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Corruption);
            assert!(
                error
                    .to_string()
                    .contains("shrinks the checkpoint page count")
            );
            assert_eq!(fs::read(temp.path()).unwrap(), main);
            assert_eq!(fs::read(wal_path(temp.path())).unwrap(), wal);
        }
    }

    #[test]
    fn corrupt_wal_freelist_overlap_is_rejected_during_recovery_without_changing_main_database() {
        for live_target in [
            FreelistOverlapTarget::CatalogRoot,
            FreelistOverlapTarget::Overflow,
        ] {
            let (temp, freelist_root, target) = catalog_with_freelist_and_live_target(live_target);
            let before =
                std::fs::read(temp.path()).expect("read main database before WAL corruption");
            append_freelist_overlap_to_wal(&temp, freelist_root, target);

            let error = match Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            ) {
                Ok(_) => panic!("recovery must reject the checksummed WAL freelist overlap"),
                Err(error) => error,
            };

            assert_eq!(error.code(), ErrorCode::Corruption, "{live_target:?}");
            assert_eq!(
                std::fs::read(temp.path()).expect("read main database after WAL corruption"),
                before,
                "{live_target:?}"
            );
        }
    }

    #[test]
    fn candidate_freelist_overlap_rejects_commit_before_file_io() {
        for live_target in [
            FreelistOverlapTarget::CatalogRoot,
            FreelistOverlapTarget::Overflow,
        ] {
            let (temp, freelist_root, target) = catalog_with_freelist_and_live_target(live_target);
            let before = std::fs::read(temp.path()).expect("read valid fixture bytes");
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            )
            .expect("open valid fixture");
            let error = {
                let mut batch = pager.begin_write().expect("begin valid write batch");
                let mut trunk = Freelist::decode(
                    batch
                        .modify(freelist_root)
                        .expect("copy freelist into the batch"),
                )
                .expect("decode valid freelist");
                Freelist::release(&mut trunk, target.page_id(), 4096)
                    .expect("stage live page in candidate freelist");
                Freelist::write(
                    batch
                        .modify(freelist_root)
                        .expect("modify candidate freelist"),
                    &trunk,
                )
                .expect("encode checksummed candidate freelist");
                batch
                    .commit()
                    .expect_err("candidate freelist cannot overlap a live catalog page")
            };

            assert_eq!(error.code(), ErrorCode::Corruption, "{live_target:?}");
            assert_file_is_unchanged(&pager, &before);
        }
    }

    #[test]
    fn release_later_requires_a_visible_page_and_rejects_duplicates() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
        drop(batch);

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
        drop(dirty_batch);

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
        let (_temp, pager) = test_pager(4096).expect("create test pager");
        let root = {
            let mut batch = pager.begin_write().expect("begin catalog batch");
            let tree = BTree::create(&mut batch, TreeKind::Table).expect("create catalog tree");
            let root = tree.root();
            batch
                .track_tree(TreeKind::Table, root)
                .expect("register catalog tree");
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
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin allocation batch");
            let page = batch.allocate(PageType::TableLeaf).expect("allocate page");
            batch.commit().expect("commit page");
            page
        };
        let before = database_bytes(&pager);

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

        assert_file_is_unchanged(&pager, &before);
        pager
            .read(page)
            .expect("original live page remains readable");
    }

    #[test]
    fn release_then_set_catalog_root_rejects_a_live_and_free_page_before_file_io() {
        let temp = TempDatabase::new("pager-release-then-catalog-root");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin allocation batch");
            let page = batch
                .allocate(PageType::Catalog)
                .expect("allocate catalog page");
            batch.commit().expect("commit page");
            page
        };
        let before = database_bytes(&pager);

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

        assert_file_is_unchanged(&pager, &before);
        assert_eq!(
            pager
                .published_read()
                .expect("published state")
                .latest
                .catalog_root,
            PageId(0)
        );
    }

    #[test]
    fn leaf_generation_overflow_preserves_the_batch_freelist_state() {
        let temp = TempDatabase::new("pager-freelist-leaf-generation-overflow");
        let pager = pager_with_freelist(&temp, 1, &[u64::MAX]);
        let before = database_bytes(&pager);
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

        assert_file_is_unchanged(&pager, &before);
        assert_eq!(
            pager
                .published_read()
                .expect("published state")
                .latest
                .freelist_root,
            PageId(1)
        );
    }

    #[test]
    fn empty_root_generation_overflow_preserves_the_staged_root() {
        let temp = TempDatabase::new("pager-freelist-root-generation-overflow");
        let pager = pager_with_freelist(&temp, u64::MAX, &[]);
        let before = database_bytes(&pager);
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

        assert_file_is_unchanged(&pager, &before);
        assert_eq!(
            pager
                .published_read()
                .expect("published state")
                .latest
                .freelist_root,
            PageId(1)
        );
    }

    #[test]
    fn full_root_promotes_released_page_and_empty_root_is_allocated() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
        assert_eq!(
            pager
                .published_read()
                .expect("published state")
                .latest
                .freelist_root,
            promoted_root.page_id()
        );
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
        assert_eq!(
            pager
                .published_read()
                .expect("published state")
                .latest
                .freelist_root,
            refs[0].page_id()
        );
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
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
        assert_eq!(
            pager.reused_page_count(),
            100,
            "the test-only allocator counter must record every freelist reuse"
        );
    }

    #[test]
    fn pending_release_cannot_be_reused_by_its_own_batch() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
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
    fn btree_release_rejects_header_duplicate_and_non_owned_page_types() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager) = test_pager(page_size).expect("create test pager");
            let mut batch = pager.begin_write().expect("begin validation batch");
            let header = PageRef {
                page_id: PageId(0),
                generation: 1,
            };
            assert_eq!(
                batch
                    .release_btree_later(header)
                    .expect_err("header page must be rejected")
                    .code(),
                ErrorCode::Corruption
            );

            let catalog = batch.allocate(PageType::Catalog).expect("allocate catalog");
            assert_eq!(
                batch
                    .release_btree_later(catalog)
                    .expect_err("catalog page must be rejected")
                    .code(),
                ErrorCode::Corruption
            );
            assert_eq!(
                batch
                    .read(catalog)
                    .expect("catalog remains readable")
                    .header()
                    .page_type(),
                PageType::Catalog
            );
            batch
                .modify(catalog)
                .expect("catalog remains live after rejection");

            let freelist = batch
                .allocate(PageType::Freelist)
                .expect("allocate freelist page");
            Freelist::write(
                batch.modify(freelist).expect("initialize freelist page"),
                &Freelist::empty_trunk(None),
            )
            .expect("write freelist page");
            assert_eq!(
                batch
                    .release_btree_later(freelist)
                    .expect_err("freelist page must be rejected")
                    .code(),
                ErrorCode::Corruption
            );
            assert_eq!(
                batch
                    .read(freelist)
                    .expect("freelist remains readable")
                    .header()
                    .page_type(),
                PageType::Freelist
            );
            batch
                .modify(freelist)
                .expect("freelist remains live after rejection");

            let leaf = batch.allocate(PageType::TableLeaf).expect("allocate leaf");
            SlottedPage::open(batch.modify(leaf).expect("initialize leaf"), 56).expect("open leaf");
            batch
                .release_btree_later(leaf)
                .expect("first B+Tree release");
            assert_eq!(
                batch
                    .release_btree_later(leaf)
                    .expect_err("duplicate B+Tree release must fail")
                    .code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn btree_release_discards_a_dirty_committed_image_until_later_reuse() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager) = test_pager(page_size).expect("create test pager");
            let page = {
                let mut batch = pager.begin_write().expect("begin allocation batch");
                let page = batch.allocate(PageType::TableLeaf).expect("allocate leaf");
                SlottedPage::open(batch.modify(page).expect("initialize leaf"), 56)
                    .expect("open leaf");
                batch.commit().expect("commit leaf");
                page
            };

            {
                let mut batch = pager.begin_write().expect("begin release batch");
                batch.modify(page).expect("dirty committed leaf");
                batch
                    .release_btree_later(page)
                    .expect("release dirty B+Tree page");
                let appended = batch
                    .allocate(PageType::TableLeaf)
                    .expect("same batch must append");
                assert_ne!(appended.page_id(), page.page_id());
                SlottedPage::open(
                    batch.modify(appended).expect("initialize appended leaf"),
                    56,
                )
                .expect("open appended leaf");
                batch.commit().expect("commit release");
            }

            let replacement = {
                let mut batch = pager.begin_write().expect("begin reuse batch");
                let replacement = batch
                    .allocate(PageType::TableLeaf)
                    .expect("reuse released page");
                SlottedPage::open(
                    batch.modify(replacement).expect("initialize replacement"),
                    56,
                )
                .expect("open replacement leaf");
                batch.commit().expect("commit replacement");
                replacement
            };
            assert_eq!(replacement.page_id(), page.page_id());
            assert_eq!(replacement.generation(), page.generation() + 1);
            assert_eq!(
                pager
                    .read(page)
                    .expect_err("old generation is stale")
                    .code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn btree_release_of_same_batch_page_is_checkpointed_and_reused_later() {
        for page_size in [4096, 8192, 16384] {
            let (_temp, pager) = test_pager(page_size).expect("create test pager");
            let seeds = {
                let mut batch = pager.begin_write().expect("begin seed allocation");
                let seeds = (0..2)
                    .map(|_| batch.allocate(PageType::TableLeaf).expect("allocate seed"))
                    .collect::<Vec<_>>();
                for seed in &seeds {
                    SlottedPage::open(batch.modify(*seed).expect("initialize seed"), 56)
                        .expect("open seed leaf");
                }
                batch.commit().expect("commit seeds");
                seeds
            };
            {
                let mut batch = pager.begin_write().expect("begin seed release");
                for seed in seeds {
                    batch.release_later(seed).expect("release seed");
                }
                batch.commit().expect("commit seed freelist");
            }
            let released = {
                let mut batch = pager.begin_write().expect("begin allocation batch");
                let page = batch
                    .allocate_append(PageType::TableLeaf)
                    .expect("append leaf");
                SlottedPage::open(batch.modify(page).expect("initialize leaf"), 56)
                    .expect("open leaf");
                let checkpoint = batch.checkpoint_mutation().expect("checkpoint");
                batch
                    .release_btree_later(page)
                    .expect("release new B+Tree page");
                batch.restore_mutation(checkpoint);
                batch.read(page).expect("checkpoint restored live page");
                batch
                    .release_btree_later(page)
                    .expect("release restored new B+Tree page");
                let appended = batch
                    .allocate(PageType::TableLeaf)
                    .expect("allocate another page in the same batch");
                assert_ne!(appended.page_id(), page.page_id());
                SlottedPage::open(
                    batch.modify(appended).expect("initialize appended leaf"),
                    56,
                )
                .expect("open appended leaf");
                batch.commit().expect("commit new-page release");
                page
            };

            let persisted = pager.read(released).expect("read persisted tombstone");
            assert_eq!(persisted.header().page_type(), PageType::TableLeaf);
            assert_eq!(persisted.header().cell_count(), 0);

            let replacement = {
                let mut batch = pager.begin_write().expect("begin reuse batch");
                let replacement = batch
                    .allocate(PageType::TableLeaf)
                    .expect("reuse released page");
                SlottedPage::open(
                    batch.modify(replacement).expect("initialize replacement"),
                    56,
                )
                .expect("open replacement leaf");
                batch.commit().expect("commit replacement");
                replacement
            };
            assert_eq!(replacement.page_id(), released.page_id());
            assert_eq!(replacement.generation(), released.generation() + 1);
            assert_eq!(
                pager
                    .read(released)
                    .expect_err("old generation is stale")
                    .code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn malformed_freelist_body_rejects_commit_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-freelist");
        let pager =
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
        let before = database_bytes(&pager);
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
        assert_file_is_unchanged(&pager, &before);
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn page_source_reads_committed_pages_and_batch_overlays() {
        let (_temp, pager) = test_pager(4096).expect("create test pager");
        let page = {
            let mut batch = pager.begin_write().expect("begin initial batch");
            let page = batch
                .allocate_append(PageType::TableLeaf)
                .expect("allocate page");
            batch.modify(page).expect("modify initial page").bytes_mut()[100] = 0x44;
            batch.commit().expect("commit initial page");
            page
        };

        let mut snapshot = pager.begin_read().expect("begin read snapshot");
        assert_eq!(read_source_byte(&mut snapshot, page), 0x44);
        let mut batch = pager.begin_write().expect("begin overlay batch");
        batch.modify(page).expect("modify overlay page").bytes_mut()[100] = 0x55;
        assert_eq!(read_source_byte(&mut batch, page), 0x55);
        batch.rollback().expect("rollback overlay batch");
    }

    fn read_source_byte(source: &mut impl PageSource, reference: PageRef) -> u8 {
        source
            .read_page(reference)
            .expect("read page source")
            .bytes()[100]
    }

    #[test]
    fn append_rejects_pages_beyond_the_configured_limit_before_file_changes() {
        let temp = TempDatabase::new("pager-limit");
        let mut config = PagerConfig::test_default(4096);
        config.max_database_bytes = 8192;
        let pager = Pager::create(temp.path(), config).expect("create limited pager");
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
        let header = file
            .header()
            .with_checkpoint(
                file.header().checkpoint_lsn(),
                2,
                PageId(0),
                PageId(0),
                file.header().wal_generation(),
            )
            .expect("build page-count superblock");
        file.publish_superblock(header)
            .expect("publish page-count superblock");
        let wal = Wal::create(temp.path(), &header).expect("create matching empty WAL");
        drop(wal);
        drop(file);

        let pager = Pager::open(
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
        assert_eq!(pager.cache_lock().expect("cache").len(), 0);
    }

    #[test]
    fn commit_checks_fault_boundaries_in_deterministic_order() {
        let temp = TempDatabase::new("pager-fault-order");
        let faults = Arc::new(RecordingFaults::never());
        let pager = pager_with_faults(&temp, faults.clone());
        let mut batch = pager.begin_write().expect("begin write");
        batch
            .allocate_append(PageType::TableLeaf)
            .expect("allocate append");

        batch.commit().expect("commit");

        let mut expected = commit_fault_points();
        expected.extend([FaultPoint::BeforeWalPublish, FaultPoint::AfterWalPublish]);
        assert_eq!(faults.recorded(), expected);
    }

    #[test]
    fn every_commit_wal_fault_requires_recovery_before_later_writes() {
        let fault_points = commit_fault_points();

        for occurrence in 1..=fault_points.len() {
            let temp = TempDatabase::new(&format!("pager-fault-{occurrence}"));
            let faults = Arc::new(RecordingFaults::at(occurrence));
            let pager = pager_with_faults(&temp, faults.clone());
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
                pager
                    .read(page)
                    .expect_err("unpublished page remains invisible")
                    .code(),
                ErrorCode::Corruption
            );
            let write_error = match pager.begin_write() {
                Ok(_) => panic!("poisoned pager must reject write batches"),
                Err(error) => error,
            };
            assert_eq!(write_error.code(), ErrorCode::Transaction);
            assert!(write_error.to_string().contains("recovery"));
        }
    }

    #[test]
    fn invalid_leaf_sibling_metadata_fails_before_file_io() {
        for page_type in [PageType::TableLeaf, PageType::IndexLeaf] {
            let temp = TempDatabase::new("pager-invalid-leaf-metadata");
            let pager =
                Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
            let before = database_bytes(&pager);
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
            assert_file_is_unchanged(&pager, &before);
            assert!(!pager.is_poisoned());
        }
    }

    #[test]
    fn malformed_leaf_layout_fails_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-leaf-layout");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = database_bytes(&pager);
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
        assert_file_is_unchanged(&pager, &before);
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn malformed_leaf_cell_fails_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-leaf-cell");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = database_bytes(&pager);
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
        assert_file_is_unchanged(&pager, &before);
        assert!(!pager.is_poisoned());
    }

    #[test]
    fn invalid_internal_leftmost_metadata_fails_before_file_io() {
        for page_type in [PageType::TableInternal, PageType::IndexInternal] {
            let temp = TempDatabase::new("pager-invalid-internal-metadata");
            let pager =
                Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
            let before = database_bytes(&pager);
            let error = {
                let mut batch = pager.begin_write().expect("begin write");
                batch.allocate_append(page_type).expect("allocate page");
                match batch.commit() {
                    Ok(()) => panic!("invalid internal leftmost metadata must reject commit"),
                    Err(error) => error,
                }
            };

            assert_eq!(error.code(), ErrorCode::Corruption);
            assert_file_is_unchanged(&pager, &before);
            assert!(!pager.is_poisoned());
        }
    }

    #[test]
    fn malformed_internal_cell_fails_before_file_io() {
        let temp = TempDatabase::new("pager-invalid-internal-cell");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = database_bytes(&pager);
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
        assert_file_is_unchanged(&pager, &before);
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
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let before = database_bytes(&pager);
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

        assert_file_is_unchanged(&pager, &before);
        assert_eq!(
            pager
                .read(page)
                .expect_err("dropped page is invisible")
                .code(),
            ErrorCode::Corruption
        );
        assert!(!pager.is_poisoned());
    }

    fn database_bytes(pager: &Pager) -> Vec<u8> {
        read_all_for_test(&mut pager.main_lock().expect("lock main database"))
            .expect("read database through owning handle")
    }

    fn assert_file_is_unchanged(pager: &Pager, before: &[u8]) {
        let after = database_bytes(pager);
        assert_eq!(after, before);
        assert_eq!(after.len(), before.len());
    }

    fn root_has_internal_child(source: &mut impl PageSource, root: PageRef) -> Result<bool> {
        let root = source.read_page(root)?;
        let root = root.as_ref();
        if root.header().page_type() != PageType::TableInternal {
            return Ok(false);
        }
        let leftmost = decode_internal_leftmost(&root.bytes()[48..INTERNAL_METADATA_END])?;
        let child = source.read_page(leftmost)?;
        Ok(child.header().page_type() == PageType::TableInternal)
    }

    fn rightmost_child(source: &mut impl PageSource, root: PageRef) -> PageRef {
        let root = source.read_page(root).expect("read internal root");
        let mut root = copy_clean_frame(root.as_ref()).expect("copy internal root");
        let cell_count = usize::from(root.header().cell_count());
        let slots = SlottedPage::open(&mut root, INTERNAL_METADATA_END).expect("open root slots");
        InternalCell::decode(
            &slots
                .cell(cell_count - 1)
                .expect("read rightmost separator"),
        )
        .expect("decode rightmost separator")
        .right()
    }

    fn wide_key(index: u16) -> Vec<u8> {
        let mut key = format!("k{index:06}").into_bytes();
        key.resize(1024, b'x');
        key
    }

    fn pager_with_faults(temp: &TempDatabase, faults: Arc<dyn FaultInjector>) -> Pager {
        Pager::create(temp.path(), PagerConfig::test_default(4096))
            .expect("create pager")
            .with_faults(faults)
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
        let header = header
            .with_checkpoint(
                header.checkpoint_lsn(),
                page_count,
                header.catalog_root(),
                header.freelist_root(),
                header.wal_generation(),
            )
            .expect("build freelist superblock");
        file.publish_superblock(header)
            .expect("publish freelist root");
        let wal = Wal::create(temp.path(), &header).expect("create matching empty WAL");
        drop(wal);
        drop(file);

        Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .expect("open pager with freelist")
    }

    #[derive(Debug, Clone, Copy)]
    enum FreelistOverlapTarget {
        CatalogRoot,
        Overflow,
    }

    fn catalog_with_freelist_and_live_target(
        target_kind: FreelistOverlapTarget,
    ) -> (TempDatabase, PageRef, PageRef) {
        let temp = TempDatabase::new("pager-freelist-live-overlap");
        let pager =
            Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager");
        let catalog_root = {
            let mut batch = pager.begin_write().expect("begin catalog batch");
            let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create catalog tree");
            tree.insert(&mut batch, b"catalog-overflow", &vec![0x5a; 5000])
                .expect("insert catalog overflow value");
            let root = tree.root();
            batch
                .track_tree(TreeKind::Table, root)
                .expect("track catalog tree");
            batch.set_catalog_root(root).expect("publish catalog root");
            batch.commit().expect("commit catalog tree");
            root
        };
        let first_free = {
            let mut batch = pager.begin_write().expect("begin first free allocation");
            let page = batch
                .allocate_append(PageType::Catalog)
                .expect("append first free page");
            batch.commit().expect("commit first free page");
            page
        };
        {
            let mut batch = pager.begin_write().expect("begin first free release");
            batch
                .release_later(first_free)
                .expect("release first free page");
            batch.commit().expect("commit freelist root");
        }
        let second_free = {
            let mut batch = pager.begin_write().expect("begin second free allocation");
            let page = batch
                .allocate_append(PageType::Catalog)
                .expect("append second free page");
            batch.commit().expect("commit second free page");
            page
        };
        {
            let mut batch = pager.begin_write().expect("begin second free release");
            batch
                .release_later(second_free)
                .expect("release second free page");
            batch.commit().expect("commit freelist leaf");
        }

        let (freelist_root, target) = {
            let mut snapshot = pager.begin_read().expect("begin read snapshot");
            let freelist_root_id = snapshot.freelist_root().expect("read freelist root");
            let freelist_root_frame = snapshot
                .read_page_by_id(freelist_root_id)
                .expect("read persisted freelist root");
            let freelist_root =
                PageRef::new(freelist_root_id, freelist_root_frame.header().generation())
                    .expect("freelist root reference");
            let target = match target_kind {
                FreelistOverlapTarget::CatalogRoot => catalog_root,
                FreelistOverlapTarget::Overflow => (1..snapshot.page_count().expect("page count"))
                    .find_map(|page_id| {
                        let page_id = PageId(page_id);
                        let frame = snapshot
                            .read_page_by_id(page_id)
                            .expect("read catalog page");
                        (frame.header().page_type() == PageType::Overflow).then(|| {
                            PageRef::new(page_id, frame.header().generation())
                                .expect("overflow page reference")
                        })
                    })
                    .expect("catalog tree retains an overflow page"),
            };
            (freelist_root, target)
        };
        drop(pager);
        (temp, freelist_root, target)
    }

    fn append_freelist_overlap_to_wal(temp: &TempDatabase, root: PageRef, target: PageRef) {
        let main = DatabaseFile::open(temp.path(), OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
            .expect("open main database");
        let checkpoint = main.header();
        let mut wal = Wal::open(temp.path(), &checkpoint, OpenMode::ReadWrite)
            .expect("open WAL for corruption fixture");
        let recovered = wal
            .recover(&checkpoint, OpenMode::ReadOnly)
            .expect("recover valid WAL fixture");
        let version = recovered
            .index
            .visible_version(root.page_id(), recovered.latest_lsn)
            .expect("freelist root must have a WAL version");
        let mut frame = wal
            .read_page_frame(root.page_id(), version, checkpoint.page_size())
            .expect("read persisted freelist frame from WAL");
        let mut trunk = Freelist::decode(&frame).expect("decode persisted freelist frame");
        Freelist::release(&mut trunk, target.page_id(), checkpoint.page_size())
            .expect("add overlapping freelist leaf");
        Freelist::write(&mut frame, &trunk).expect("encode overlapping freelist frame");
        frame
            .prepare_for_write()
            .expect("prepare overlapping freelist frame");
        let transaction_lsn = recovered
            .latest_lsn
            .0
            .checked_add(1)
            .map(Lsn)
            .expect("fixture WAL LSN does not overflow");
        wal.append_transaction(
            transaction_lsn,
            recovered.latest_lsn,
            recovered.latest_state,
            &[(root.page_id(), root.generation(), frame.bytes().to_vec())],
        )
        .expect("append checksummed corrupt-state WAL transaction");
        wal.sync_all().expect("sync corrupt-state WAL transaction");
    }

    fn commit_fault_points() -> Vec<FaultPoint> {
        vec![
            FaultPoint::BeforeWalRecordWrite,
            FaultPoint::AfterWalRecordHeaderPrefixWrite,
            FaultPoint::AfterWalRecordPayloadPrefixWrite,
            FaultPoint::AfterWalRecordWrite,
            FaultPoint::BeforeWalRecordWrite,
            FaultPoint::AfterWalRecordHeaderPrefixWrite,
            FaultPoint::AfterWalRecordPayloadPrefixWrite,
            FaultPoint::AfterWalRecordWrite,
            FaultPoint::BeforeWalSync,
            FaultPoint::AfterWalSync,
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
