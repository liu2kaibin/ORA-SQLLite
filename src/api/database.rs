use std::{fmt, path::Path};

use crate::{
    DbError, ErrorCode, Result, SqlLimits,
    storage::{OpenMode, Pager, PagerConfig},
};

use super::{AccessMode, Connection, Options, options::default_create_page_size};

pub(crate) trait UuidSource {
    fn fill(&self, uuid: &mut [u8; 16]) -> std::result::Result<(), ()>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OsUuidSource;

impl UuidSource for OsUuidSource {
    fn fill(&self, uuid: &mut [u8; 16]) -> std::result::Result<(), ()> {
        getrandom::fill(uuid).map_err(|_| ())
    }
}

pub(crate) fn generate_database_uuid(source: &dyn UuidSource) -> Result<[u8; 16]> {
    let mut uuid = [0; 16];
    source.fill(&mut uuid).map_err(|_| {
        DbError::new(
            ErrorCode::Io,
            "operating system random source could not create a database identity",
        )
    })?;
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Ok(uuid)
}

/// A cloneable handle to an embedded database.
#[derive(Clone)]
pub struct Database {
    pub(super) pager: Pager,
    pub(super) sql_limits: SqlLimits,
    pub(super) max_query_memory_bytes: u64,
}

impl fmt::Debug for Database {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Database")
            .field("sql_limits", &self.sql_limits)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Creates a new database without replacing an existing path.
    pub fn create(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        Self::create_with_uuid_source(path, options, &OsUuidSource)
    }

    /// Opens an existing database without creating a missing path.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let mode = match options.access_mode() {
            AccessMode::ReadWrite => OpenMode::ReadWrite,
            AccessMode::ReadOnly => OpenMode::ReadOnly,
        };
        let config = pager_config(default_create_page_size(), [0; 16], &options)?;
        let pager = Pager::open(path.as_ref(), mode, config)?;
        Ok(Self {
            pager,
            sql_limits: *options.sql_limits(),
            max_query_memory_bytes: options.max_query_memory_bytes(),
        })
    }

    /// Creates an independent session for this database.
    pub fn connect(&self) -> Result<Connection> {
        Ok(Connection::new(
            self.pager.clone(),
            self.sql_limits,
            self.max_query_memory_bytes,
        ))
    }

    pub(crate) fn create_with_uuid_source(
        path: impl AsRef<Path>,
        options: Options,
        source: &dyn UuidSource,
    ) -> Result<Self> {
        let uuid = generate_database_uuid(source)?;
        let config = pager_config(options.page_size(), uuid, &options)?;
        let pager = Pager::create(path.as_ref(), config)?;
        Ok(Self {
            pager,
            sql_limits: *options.sql_limits(),
            max_query_memory_bytes: options.max_query_memory_bytes(),
        })
    }

    #[allow(
        dead_code,
        reason = "database identity is consumed by later Phase 4B binding tasks"
    )]
    pub(crate) fn database_uuid(&self) -> Result<[u8; 16]> {
        self.pager.database_uuid()
    }
}

fn pager_config(page_size: u32, uuid: [u8; 16], options: &Options) -> Result<PagerConfig> {
    Ok(PagerConfig::new(page_size, uuid)?.with_runtime_limits(
        options.max_database_bytes(),
        options.cache_pages(),
        options.checkpoint_wal_bytes(),
        options.max_wal_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use crate::{
        ErrorCode,
        api::{Database, Options, database::generate_database_uuid},
        storage::test_support::TempDatabase,
    };

    use super::UuidSource;

    struct FailUuid;

    impl UuidSource for FailUuid {
        fn fill(&self, _uuid: &mut [u8; 16]) -> std::result::Result<(), ()> {
            Err(())
        }
    }

    struct FixedUuid([u8; 16]);

    impl UuidSource for FixedUuid {
        fn fill(&self, uuid: &mut [u8; 16]) -> std::result::Result<(), ()> {
            *uuid = self.0;
            Ok(())
        }
    }

    #[test]
    fn uuid_source_failure_happens_before_any_file_creation() {
        let paths = TempDatabase::new("uuid-failure");
        let error = Database::create_with_uuid_source(paths.path(), Options::default(), &FailUuid)
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io);
        assert!(!paths.path().exists());
        assert!(!wal_path(paths.path()).exists());
    }

    #[test]
    fn generated_uuid_has_v4_and_rfc4122_bits() {
        let uuid = generate_database_uuid(&FixedUuid([0xff; 16])).unwrap();
        assert_eq!(uuid[6] >> 4, 4);
        assert_eq!(uuid[8] >> 6, 2);
        assert_eq!(uuid[0], 0xff);
        assert_eq!(uuid[15], 0xff);
    }

    #[test]
    fn created_database_retains_its_normalized_identity() {
        let paths = TempDatabase::new("database-identity");
        let database = Database::create_with_uuid_source(
            paths.path(),
            Options::default(),
            &FixedUuid([0x2a; 16]),
        )
        .unwrap();
        let mut expected = [0x2a; 16];
        expected[6] = 0x4a;
        expected[8] = 0xaa;
        assert_eq!(database.database_uuid().unwrap(), expected);
    }

    #[test]
    fn open_maps_read_only_and_read_write_modes_to_the_pager() {
        let paths = TempDatabase::new("database-access-mode");
        let created = Database::create_with_uuid_source(
            paths.path(),
            Options::default(),
            &FixedUuid([1; 16]),
        )
        .unwrap();
        drop(created);

        let read_only = Database::open(paths.path(), Options::default().read_only()).unwrap();
        let read_only_error = match read_only.pager.begin_write() {
            Ok(_) => panic!("read-only database granted a write batch"),
            Err(error) => error,
        };
        assert_eq!(read_only_error.code(), ErrorCode::Unsupported);
        drop(read_only);

        let read_write = Database::open(paths.path(), Options::default().read_write()).unwrap();
        let mut batch = read_write.pager.begin_write().unwrap();
        batch.rollback().unwrap();
    }

    fn wal_path(main: &std::path::Path) -> PathBuf {
        let mut wal = OsString::from(main.as_os_str());
        wal.push("-wal");
        PathBuf::from(wal)
    }
}
