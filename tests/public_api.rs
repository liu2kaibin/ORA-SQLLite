mod common;

use common::TempPaths;
use ora_sqllite::{AccessMode, Database, ErrorCode, Options, SqlLimits};

#[test]
fn create_is_exclusive_and_open_never_creates() {
    let paths = TempPaths::new("public-create-open");
    let db = Database::create(paths.main(), Options::default()).unwrap();
    drop(db);
    assert_eq!(
        Database::create(paths.main(), Options::default())
            .unwrap_err()
            .code(),
        ErrorCode::Io
    );
    assert!(Database::open(paths.missing(), Options::default()).is_err());
    assert!(!paths.missing().exists());
}

#[test]
fn options_expose_defaults_and_builder_overrides() {
    let defaults = Options::default();
    assert_eq!(defaults.access_mode(), AccessMode::ReadWrite);
    assert_eq!(defaults.page_size(), 4096);
    assert_eq!(defaults.cache_pages(), 1024);
    assert_eq!(defaults.max_database_bytes(), 10 * 1024 * 1024 * 1024);
    assert_eq!(defaults.checkpoint_wal_bytes(), 64 * 1024 * 1024);
    assert_eq!(defaults.max_wal_bytes(), 1024 * 1024 * 1024);
    assert_eq!(defaults.sql_limits(), &SqlLimits::default());

    let sql_limits = SqlLimits::default().with_max_tokens(17).unwrap();
    let configured = Options::default()
        .read_only()
        .with_page_size(8192)
        .with_cache_pages(32)
        .with_max_database_bytes(128 * 1024 * 1024)
        .with_checkpoint_wal_bytes(2 * 1024 * 1024)
        .with_max_wal_bytes(8 * 1024 * 1024)
        .with_sql_limits(sql_limits);
    assert_eq!(configured.access_mode(), AccessMode::ReadOnly);
    assert_eq!(configured.page_size(), 8192);
    assert_eq!(configured.cache_pages(), 32);
    assert_eq!(configured.max_database_bytes(), 128 * 1024 * 1024);
    assert_eq!(configured.checkpoint_wal_bytes(), 2 * 1024 * 1024);
    assert_eq!(configured.max_wal_bytes(), 8 * 1024 * 1024);
    assert_eq!(configured.sql_limits(), &sql_limits);
    assert_eq!(configured.read_write().access_mode(), AccessMode::ReadWrite);
}

#[test]
fn opening_uses_disk_page_size_instead_of_create_option() {
    let paths = TempPaths::new("open-disk-page-size");
    let created = Database::create(paths.main(), Options::default().with_page_size(8192)).unwrap();
    drop(created);

    Database::open(paths.main(), Options::default().with_page_size(123)).unwrap();
}

#[test]
fn database_is_clone_send_and_sync() {
    fn assert_contract<T: Clone + Send + Sync>() {}
    assert_contract::<Database>();
}

#[test]
fn unfinished_write_transaction_rolls_back_and_read_only_rejects_writes() {
    let paths = TempPaths::new("transaction-raii");
    let db = Database::create(paths.main(), Options::default()).unwrap();
    drop(db);
    let before = std::fs::read(paths.main()).unwrap();
    let db = Database::open(paths.main(), Options::default()).unwrap();
    {
        let mut conn = db.connect().unwrap();
        let _write = conn.begin_write().unwrap();
    }
    drop(db);
    assert_eq!(std::fs::read(paths.main()).unwrap(), before);

    let read_only = Database::open(paths.main(), Options::default().read_only()).unwrap();
    let mut conn = read_only.connect().unwrap();
    assert_eq!(
        conn.begin_write().unwrap_err().code(),
        ErrorCode::Unsupported
    );
}
