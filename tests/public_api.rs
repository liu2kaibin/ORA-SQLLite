mod common;

#[test]
fn persistent_check_parameters_are_rejected_without_prepare_mutation() {
    use ora_sqllite::{DataType, ParameterIdentifier};
    let paths = TempPaths::new("public-final-constraints");
    let db = Database::create(paths.main(), Options::default()).unwrap();
    drop(db);
    let before_main = std::fs::read(paths.main()).unwrap();
    let before_wal = std::fs::read(paths.wal()).unwrap();
    let db = Database::open(paths.main(), Options::default()).unwrap();
    let mut conn = db.connect().unwrap();
    let error = conn
        .prepare("CREATE TABLE FLAGS (B BOOL CHECK (:first = :second AND :second = TRUE))")
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Bind);
    assert_eq!(error.to_string(), "[1100] CHECK cannot contain parameters");
    assert_eq!(error.sql_location(), None);

    // The same parameter expression remains valid outside a persisted CHECK,
    // and its public metadata contract is unchanged.
    let statement = conn
        .prepare("SELECT :first = :second AND :second = TRUE")
        .unwrap();
    assert_eq!(
        statement
            .parameters()
            .iter()
            .map(|p| p.identifier().clone())
            .collect::<Vec<_>>(),
        [
            ParameterIdentifier::Name("FIRST".to_owned()),
            ParameterIdentifier::Name("SECOND".to_owned())
        ]
    );
    assert!(
        statement
            .parameters()
            .iter()
            .all(|p| p.data_type() == Some(DataType::Bool))
    );
    // Rejected and successful prepare calls are read-only, and the rejected
    // CREATE did not reserve its table name.
    conn.prepare("CREATE TABLE FLAGS (B BOOL)").unwrap();
    drop(conn);
    drop(db);
    assert_eq!(std::fs::read(paths.main()).unwrap(), before_main);
    assert_eq!(std::fs::read(paths.wal()).unwrap(), before_wal);
    let cloned = statement.clone();
    drop(statement);
    assert_eq!(cloned.parameters().len(), 2);
}

use common::TempPaths;
use ora_sqllite::{
    AccessMode, Database, ErrorCode, Options, ReadTransaction, SqlLimits, Statement,
    WriteTransaction,
};

fn prepare_read_transaction<'connection>(
    transaction: &mut ReadTransaction<'connection>,
    sql: &str,
) -> ora_sqllite::Result<Statement> {
    transaction.prepare(sql)
}

fn prepare_write_transaction<'connection>(
    transaction: &mut WriteTransaction<'connection>,
    sql: &str,
) -> ora_sqllite::Result<Statement> {
    transaction.prepare(sql)
}

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

#[test]
fn public_prepare_accepts_create_table_on_a_new_empty_database() {
    // Production break caught: a public prepare path that bypasses the empty
    // catalog bridge or attempts execution cannot prepare this DDL read-only.
    let paths = TempPaths::new("public-prepare-create-table");
    let db = Database::create(paths.main(), Options::default()).unwrap();
    drop(db);
    let before_main = std::fs::read(paths.main()).unwrap();
    let before_wal = std::fs::read(paths.wal()).unwrap();

    let statement = {
        let db = Database::open(paths.main(), Options::default()).unwrap();
        let mut conn = db.connect().unwrap();
        conn.prepare("CREATE TABLE T (ID INT64)").unwrap()
    };

    assert!(statement.parameters().is_empty());
    assert!(statement.columns().is_empty());
    assert_eq!(std::fs::read(paths.main()).unwrap(), before_main);
    assert_eq!(std::fs::read(paths.wal()).unwrap(), before_wal);
}

#[test]
fn public_transaction_prepare_methods_have_the_required_mutable_receivers() {
    // Production break caught: making either transaction prepare method
    // crate-private, changing its mutable receiver, or returning a non-public
    // statement type makes this external-crate integration contract fail.
    let paths = TempPaths::new("public-transaction-prepare");
    let db = Database::create(paths.main(), Options::default()).unwrap();

    let mut connection = db.connect().unwrap();
    let mut read = connection.begin_read().unwrap();
    let read_statement = prepare_read_transaction(&mut read, "CREATE TABLE T (ID INT64)").unwrap();
    assert!(read_statement.parameters().is_empty());
    assert!(read_statement.columns().is_empty());
    drop(read);

    let mut write = connection.begin_write().unwrap();
    let write_statement =
        prepare_write_transaction(&mut write, "CREATE TABLE T (ID INT64)").unwrap();
    assert!(write_statement.parameters().is_empty());
    assert!(write_statement.columns().is_empty());
    write.rollback().unwrap();
}
