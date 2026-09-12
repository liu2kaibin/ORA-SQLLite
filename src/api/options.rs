use crate::SqlLimits;

const DEFAULT_PAGE_SIZE: u32 = 4096;
const DEFAULT_CACHE_PAGES: usize = 1024;
const DEFAULT_MAX_DATABASE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_CHECKPOINT_WAL_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_WAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Access requested when opening a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessMode {
    #[default]
    ReadWrite,
    ReadOnly,
}

/// Database creation, opening, storage, and SQL preparation limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    access_mode: AccessMode,
    page_size: u32,
    cache_pages: usize,
    max_database_bytes: u64,
    checkpoint_wal_bytes: u64,
    max_wal_bytes: u64,
    sql_limits: SqlLimits,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            access_mode: AccessMode::ReadWrite,
            page_size: DEFAULT_PAGE_SIZE,
            cache_pages: DEFAULT_CACHE_PAGES,
            max_database_bytes: DEFAULT_MAX_DATABASE_BYTES,
            checkpoint_wal_bytes: DEFAULT_CHECKPOINT_WAL_BYTES,
            max_wal_bytes: DEFAULT_MAX_WAL_BYTES,
            sql_limits: SqlLimits::default(),
        }
    }
}

impl Options {
    pub const fn access_mode(&self) -> AccessMode {
        self.access_mode
    }

    /// Page size for newly created databases. Existing databases use their on-disk page size.
    pub const fn page_size(&self) -> u32 {
        self.page_size
    }

    pub const fn cache_pages(&self) -> usize {
        self.cache_pages
    }

    pub const fn max_database_bytes(&self) -> u64 {
        self.max_database_bytes
    }

    pub const fn checkpoint_wal_bytes(&self) -> u64 {
        self.checkpoint_wal_bytes
    }

    pub const fn max_wal_bytes(&self) -> u64 {
        self.max_wal_bytes
    }

    pub const fn sql_limits(&self) -> &SqlLimits {
        &self.sql_limits
    }

    pub const fn read_only(self) -> Self {
        self.with_access_mode(AccessMode::ReadOnly)
    }

    pub const fn read_write(self) -> Self {
        self.with_access_mode(AccessMode::ReadWrite)
    }

    pub const fn with_access_mode(mut self, access_mode: AccessMode) -> Self {
        self.access_mode = access_mode;
        self
    }

    pub const fn with_page_size(mut self, page_size: u32) -> Self {
        self.page_size = page_size;
        self
    }

    pub const fn with_cache_pages(mut self, cache_pages: usize) -> Self {
        self.cache_pages = cache_pages;
        self
    }

    pub const fn with_max_database_bytes(mut self, max_database_bytes: u64) -> Self {
        self.max_database_bytes = max_database_bytes;
        self
    }

    pub const fn with_checkpoint_wal_bytes(mut self, checkpoint_wal_bytes: u64) -> Self {
        self.checkpoint_wal_bytes = checkpoint_wal_bytes;
        self
    }

    pub const fn with_max_wal_bytes(mut self, max_wal_bytes: u64) -> Self {
        self.max_wal_bytes = max_wal_bytes;
        self
    }

    pub const fn with_sql_limits(mut self, sql_limits: SqlLimits) -> Self {
        self.sql_limits = sql_limits;
        self
    }
}

pub(super) const fn default_create_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}
