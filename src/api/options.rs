use crate::SqlLimits;

const DEFAULT_PAGE_SIZE: u32 = 4096;
const DEFAULT_CACHE_PAGES: usize = 1024;
const DEFAULT_MAX_DATABASE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const DEFAULT_CHECKPOINT_WAL_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_WAL_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_QUERY_MEMORY_BYTES: u64 = 64 * 1024 * 1024;

/// Access requested when opening a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessMode {
    /// Open the database for reads and writes; this is the default.
    #[default]
    ReadWrite,
    /// Open the database for reads only; attempts to start writes return
    /// [`crate::ErrorCode::Unsupported`].
    ReadOnly,
}

/// Database creation, opening, storage, and SQL preparation limits.
///
/// Defaults are read-write access, a 4,096-byte page, 1,024 cache pages, a 10-GiB database
/// maximum, a 64-MiB checkpoint threshold, a 1-GiB WAL maximum, a 64-MiB query-memory limit,
/// and [`SqlLimits::default`]. Builder methods only record values; validation described by each
/// builder occurs when the relevant database or query operation is attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    access_mode: AccessMode,
    page_size: u32,
    cache_pages: usize,
    max_database_bytes: u64,
    checkpoint_wal_bytes: u64,
    max_wal_bytes: u64,
    max_query_memory_bytes: u64,
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
            max_query_memory_bytes: DEFAULT_MAX_QUERY_MEMORY_BYTES,
            sql_limits: SqlLimits::default(),
        }
    }
}

impl Options {
    /// Returns the access mode used by [`crate::Database::open`].
    pub const fn access_mode(&self) -> AccessMode {
        self.access_mode
    }

    /// Returns the page size requested for newly created databases.
    ///
    /// Existing databases use their on-disk page size instead.
    pub const fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Returns the maximum number of cached pages used by create and open operations.
    pub const fn cache_pages(&self) -> usize {
        self.cache_pages
    }

    /// Returns the maximum main-database size in bytes used by create and open operations.
    pub const fn max_database_bytes(&self) -> u64 {
        self.max_database_bytes
    }

    /// Returns the WAL byte threshold that requests a checkpoint before a write transaction.
    pub const fn checkpoint_wal_bytes(&self) -> u64 {
        self.checkpoint_wal_bytes
    }

    /// Returns the maximum WAL size in bytes.
    pub const fn max_wal_bytes(&self) -> u64 {
        self.max_wal_bytes
    }

    /// Returns the deterministic per-query work-memory budget in bytes.
    pub const fn max_query_memory_bytes(&self) -> u64 {
        self.max_query_memory_bytes
    }

    /// Returns the SQL preparation limits copied into new connections.
    pub const fn sql_limits(&self) -> &SqlLimits {
        &self.sql_limits
    }

    /// Sets read-only access for [`crate::Database::open`].
    ///
    /// [`crate::Database::create`] always creates a writable database and ignores this setting.
    pub const fn read_only(self) -> Self {
        self.with_access_mode(AccessMode::ReadOnly)
    }

    /// Sets read-write access for [`crate::Database::open`].
    pub const fn read_write(self) -> Self {
        self.with_access_mode(AccessMode::ReadWrite)
    }

    /// Sets the access mode used by [`crate::Database::open`].
    ///
    /// [`crate::Database::create`] always creates a writable database and ignores this setting.
    pub const fn with_access_mode(mut self, access_mode: AccessMode) -> Self {
        self.access_mode = access_mode;
        self
    }

    /// Sets the page size used only by [`crate::Database::create`].
    ///
    /// Accepted create-time values are 4,096, 8,192, and 16,384 bytes; another value makes
    /// creation return [`crate::ErrorCode::Type`]. Open ignores this setting and uses the on-disk
    /// page size.
    pub const fn with_page_size(mut self, page_size: u32) -> Self {
        self.page_size = page_size;
        self
    }

    /// Sets the cache capacity used by both create and open operations.
    ///
    /// The accepted range is `1..=usize::MAX`; zero makes create or open return
    /// [`crate::ErrorCode::Type`].
    pub const fn with_cache_pages(mut self, cache_pages: usize) -> Self {
        self.cache_pages = cache_pages;
        self
    }

    /// Sets the maximum main-database size used by both create and open operations.
    ///
    /// The value must accommodate the 8,192-byte superblock area and all existing pages. A limit
    /// that is already too small returns [`crate::ErrorCode::Corruption`]; later growth beyond the
    /// limit returns the same category.
    pub const fn with_max_database_bytes(mut self, max_database_bytes: u64) -> Self {
        self.max_database_bytes = max_database_bytes;
        self
    }

    /// Sets the positive WAL size that triggers a checkpoint before a write transaction.
    ///
    /// This setting applies to create and open. Zero makes either operation return
    /// [`crate::ErrorCode::Type`].
    pub const fn with_checkpoint_wal_bytes(mut self, checkpoint_wal_bytes: u64) -> Self {
        self.checkpoint_wal_bytes = checkpoint_wal_bytes;
        self
    }

    /// Sets the positive maximum WAL size used by create and open operations.
    ///
    /// Zero makes create or open return [`crate::ErrorCode::Type`]. Opening requires enough space
    /// for the existing WAL header and content and returns [`crate::ErrorCode::Corruption`] when
    /// the limit is already too small. A transaction that cannot fit in an empty WAL within the
    /// limit returns [`crate::ErrorCode::Transaction`].
    pub const fn with_max_wal_bytes(mut self, max_wal_bytes: u64) -> Self {
        self.max_wal_bytes = max_wal_bytes;
        self
    }

    /// Sets the deterministic work-memory budget copied to each connection.
    ///
    /// The accepted query-time range is `1..=u64::MAX`; zero is retained by create and open but
    /// makes query construction return [`crate::ErrorCode::Type`]. Exceeding a positive budget
    /// returns [`crate::ErrorCode::ResourceLimit`].
    pub const fn with_max_query_memory_bytes(mut self, max_query_memory_bytes: u64) -> Self {
        self.max_query_memory_bytes = max_query_memory_bytes;
        self
    }

    /// Sets the SQL preparation limits copied to connections created from this database handle.
    ///
    /// This setting applies to both create and open operations.
    pub const fn with_sql_limits(mut self, sql_limits: SqlLimits) -> Self {
        self.sql_limits = sql_limits;
        self
    }
}

pub(super) const fn default_create_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}
