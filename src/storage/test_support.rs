use std::{
    collections::BTreeMap,
    fs,
    io::{ErrorKind, Write},
    ops::Bound,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    DbError, ErrorCode, Result, Value,
    fault::{FaultInjector, FaultPoint},
};

use super::{
    Lsn, PageId, PageRef, Pager, PagerConfig, ReadSnapshot, WriteBatch,
    btree::{BTree, TreeKind},
    catalog::{
        CatalogReader, CatalogWriter,
        types::{
            CatalogName, ColumnDef, ColumnId, ColumnType, IndexDef, IndexId, IndexOwnership, RowId,
            SchemaVersion, TableDef, TableId,
        },
    },
    checksum::stamp_page_checksum,
    file::{DEFAULT_MAX_DATABASE_BYTES, DatabaseFile, OpenMode, checked_page_offset},
    header::{SUPERBLOCK_AREA_LEN, SUPERBLOCK_SLOT_LEN, SuperblockSlot},
    page::{PageFrame, PageType},
    pager::PageSource,
    relation::Relation,
    row::{RowCodec, decode_rowid_key, encode_rowid_key},
    validate::validate_tree,
    wal::{Wal, wal_path},
};

static NEXT_TEMP_DATABASE: AtomicU64 = AtomicU64::new(0);
static NEXT_QUERY_DATABASE_UUID: AtomicU8 = AtomicU8::new(1);

pub(crate) struct TempDatabase {
    path: PathBuf,
}

impl TempDatabase {
    pub(crate) fn new(label: &str) -> Self {
        Self::new_with_sequence(label, || NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed))
    }

    fn new_with_sequence(label: &str, mut next_sequence: impl FnMut() -> u64) -> Self {
        let process_id = std::process::id();
        let label = sanitized_label(label);

        loop {
            let sequence = next_sequence();
            let path = std::env::temp_dir()
                .join(format!("ora-sqllite-{process_id}-{label}-{sequence}.db"));
            if !path.exists() {
                return Self { path };
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl PagerConfig {
    pub(super) fn test_default(page_size: u32) -> Self {
        Self::new(page_size, [0x5a; 16]).expect("test pager config uses a supported page size")
    }
}

pub(super) fn test_pager(page_size: u32) -> Result<(TempDatabase, Pager)> {
    let temp = TempDatabase::new("pager");
    let pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    Ok((temp, pager))
}

pub(super) fn create_pager(temp: &TempDatabase) -> Pager {
    Pager::create(temp.path(), PagerConfig::test_default(4096)).expect("create pager")
}

pub(super) fn durable_lengths(temp: &TempDatabase) -> (u64, u64) {
    (
        fs::metadata(temp.path())
            .expect("read database metadata")
            .len(),
        fs::metadata(wal_path(temp.path()))
            .expect("read WAL metadata")
            .len(),
    )
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        for path in [&self.path, &wal_path(&self.path)] {
            if let Err(error) = fs::remove_file(path) {
                if error.kind() != ErrorKind::NotFound {
                    // Test cleanup is best effort; a later test must never remove a different path.
                }
            }
        }
    }
}

pub(super) fn write_wal_fixture(database_path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(wal_path(database_path), bytes)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("write WAL fixture: {error}")))
}

pub(crate) struct Phase4bCatalogFixture {
    temp: TempDatabase,
    wal_path: PathBuf,
    page_size: u32,
    users_id: TableId,
    orders_id: TableId,
}

impl Phase4bCatalogFixture {
    pub(crate) fn pager(&self) -> Pager {
        Pager::open(
            self.main_path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(self.page_size),
        )
        .expect("open the Phase 4B catalog fixture")
    }

    pub(crate) fn main_path(&self) -> &Path {
        self.temp.path()
    }

    pub(crate) fn wal_path(&self) -> &Path {
        &self.wal_path
    }

    pub(crate) const fn users_id(&self) -> TableId {
        self.users_id
    }

    pub(crate) const fn orders_id(&self) -> TableId {
        self.orders_id
    }
}

pub(crate) fn phase_4b_catalog_fixture(page_size: u32) -> Phase4bCatalogFixture {
    phase_4b_catalog_fixture_with_uuid(page_size, [0x5a; 16])
}

/// A Phase 4B catalog with one explicit USERS(ID) index for SQL front-end tests.
///
/// This deliberately remains test support: production index DDL is planned by
/// Phase 4C2-A and its storage lifecycle belongs to a later task.
pub(crate) struct Phase4bExplicitIndexFixture {
    fixture: Phase4bCatalogFixture,
    index_id: IndexId,
}

impl Phase4bExplicitIndexFixture {
    pub(crate) fn pager(&self) -> Pager {
        self.fixture.pager()
    }

    pub(crate) const fn users_id(&self) -> TableId {
        self.fixture.users_id()
    }

    pub(crate) const fn index_id(&self) -> IndexId {
        self.index_id
    }
}

pub(crate) fn phase_4b_catalog_fixture_with_explicit_index(
    page_size: u32,
    name: &str,
) -> Phase4bExplicitIndexFixture {
    let fixture = phase_4b_catalog_fixture(page_size);
    let pager = fixture.pager();
    let mut snapshot = pager
        .begin_read()
        .expect("read explicit-index fixture catalog");
    let catalog = snapshot
        .catalog_reader()
        .expect("open explicit-index fixture catalog");
    let id_column = catalog
        .find_column_by_name(fixture.users_id(), &CatalogName::new("ID").unwrap())
        .expect("USERS(ID) in explicit-index fixture")
        .id();
    drop(catalog);
    drop(snapshot);

    let mut batch = pager
        .begin_write()
        .expect("begin explicit-index fixture write");
    let mut writer =
        CatalogWriter::bootstrap(&mut batch).expect("bootstrap explicit-index fixture catalog");
    let index_id = IndexId::new(
        writer
            .allocate_object_id(&mut batch)
            .expect("allocate explicit index id")
            .get(),
    )
    .expect("explicit index id is nonzero");
    let root = BTree::create(&mut batch, TreeKind::Index)
        .expect("create explicit index tree")
        .root();
    let index = IndexDef::new(
        index_id,
        fixture.users_id(),
        root,
        1,
        true,
        IndexOwnership::Explicit,
        CatalogName::new(name).expect("explicit index name"),
    )
    .expect("build explicit index definition");
    writer
        .insert_index_definition(&mut batch, &index, &[id_column])
        .expect("insert explicit index definition");
    writer
        .finish(&mut batch)
        .expect("finish explicit-index fixture catalog");
    batch
        .commit()
        .expect("commit explicit-index fixture catalog");

    Phase4bExplicitIndexFixture { fixture, index_id }
}

/// Removes the explicit fixture index without changing the owner table's
/// schema version, for prepared-statement identity tests.
pub(crate) fn phase_4b_remove_explicit_index(pager: &Pager, index_id: IndexId) -> Result<()> {
    let (index, columns) = explicit_index_parts(pager, index_id)?;
    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;
    writer.remove_index(&mut batch, &index, &columns)?;
    writer.finish(&mut batch)?;
    batch.commit()
}

/// Rehomes the same explicit fixture IndexId to ORDERS without changing USERS'
/// schema version, for prepared-statement owner-identity tests.
pub(crate) fn phase_4b_rehome_explicit_index_to_orders(
    pager: &Pager,
    fixture: &Phase4bExplicitIndexFixture,
) -> Result<()> {
    let (index, columns) = explicit_index_parts(pager, fixture.index_id())?;
    let mut snapshot = pager.begin_read()?;
    let catalog = snapshot.catalog_reader()?;
    let orders_id_column = catalog
        .find_column_by_name(fixture.fixture.orders_id(), &CatalogName::new("ID")?)
        .ok_or_else(|| DbError::new(ErrorCode::Internal, "fixture ORDERS(ID) is missing"))?
        .id();
    drop(catalog);
    drop(snapshot);

    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;
    writer.remove_index(&mut batch, &index, &columns)?;
    let root = BTree::create(&mut batch, TreeKind::Index)?.root();
    let rehomed = IndexDef::new(
        index.id(),
        fixture.fixture.orders_id(),
        root,
        1,
        index.unique(),
        IndexOwnership::Explicit,
        index.name().clone(),
    )?;
    writer.insert_index_definition(&mut batch, &rehomed, &[orders_id_column])?;
    writer.finish(&mut batch)?;
    batch.commit()
}

fn explicit_index_parts(pager: &Pager, index_id: IndexId) -> Result<(IndexDef, Vec<ColumnId>)> {
    let mut snapshot = pager.begin_read()?;
    let catalog = snapshot.catalog_reader()?;
    let index = catalog
        .find_index_by_id(index_id)
        .cloned()
        .ok_or_else(|| DbError::new(ErrorCode::Internal, "fixture index is missing"))?;
    let columns = catalog
        .columns_for_index(index.id())
        .map(|column| column.id())
        .collect();
    Ok((index, columns))
}

fn phase_4b_catalog_fixture_with_uuid(
    page_size: u32,
    database_uuid: [u8; 16],
) -> Phase4bCatalogFixture {
    let temp = TempDatabase::new("phase4b-catalog");
    let pager = Pager::create(
        temp.path(),
        PagerConfig::new(page_size, database_uuid).expect("query fixture pager config"),
    )
    .expect("create the Phase 4B catalog fixture");
    let mut batch = pager
        .begin_write()
        .expect("begin the Phase 4B catalog fixture write");
    let mut writer =
        CatalogWriter::bootstrap(&mut batch).expect("bootstrap the Phase 4B catalog fixture");
    let users_id = insert_phase_4b_table(
        &mut writer,
        &mut batch,
        "USERS",
        &[
            ("ID", ColumnType::Int64, false),
            ("NAME", ColumnType::Text, true),
            ("AMOUNT", ColumnType::Decimal, true),
            ("FLOAT_COL", ColumnType::Float64, true),
        ],
    )
    .expect("insert USERS into the Phase 4B catalog fixture");
    let orders_id = insert_phase_4b_table(
        &mut writer,
        &mut batch,
        "ORDERS",
        &[
            ("ID", ColumnType::Int64, false),
            ("USER_ID", ColumnType::Int64, false),
        ],
    )
    .expect("insert ORDERS into the Phase 4B catalog fixture");
    writer
        .finish(&mut batch)
        .expect("finish the Phase 4B catalog fixture");
    batch.commit().expect("commit the Phase 4B catalog fixture");
    drop(pager);

    let fixture_wal_path = wal_path(temp.path());
    Phase4bCatalogFixture {
        temp,
        wal_path: fixture_wal_path,
        page_size,
        users_id,
        orders_id,
    }
}

pub(crate) fn phase_4c1_query_relation_fixture(rows: &[(i64, &str)]) -> Phase4bCatalogFixture {
    let uuid_byte = NEXT_QUERY_DATABASE_UUID.fetch_add(1, Ordering::Relaxed);
    let fixture = phase_4b_catalog_fixture_with_uuid(4096, [uuid_byte; 16]);
    let pager = fixture.pager();
    let mut snapshot = pager.begin_read().expect("open query fixture catalog");
    let catalog = snapshot
        .catalog_reader()
        .expect("read query fixture catalog");
    let table = catalog
        .find_table_by_id(fixture.users_id())
        .expect("query fixture USERS table")
        .clone();
    let columns = catalog
        .columns_for_table(fixture.users_id())
        .cloned()
        .collect::<Vec<_>>();
    drop(snapshot);

    let mut batch = pager.begin_write().expect("begin query fixture write");
    let mut writer = CatalogWriter::bootstrap(&mut batch).expect("open query fixture catalog");
    let mut tree = BTree::from_root(TreeKind::Table, table.root());
    for (id, name) in rows {
        let rowid = writer
            .allocate_rowid(&mut batch, fixture.users_id())
            .expect("allocate query fixture ROWID");
        let values = [
            crate::Value::Int64(*id),
            crate::Value::Text((*name).to_owned()),
            crate::Value::Null,
            crate::Value::Null,
        ];
        let bytes = RowCodec::encode(&table, &columns, &values, 64 * 1024)
            .expect("encode query fixture row");
        tree.insert(&mut batch, &encode_rowid_key(rowid), &bytes)
            .expect("insert query fixture row");
    }
    if !rows.is_empty() {
        writer
            .update_table_root(&mut batch, fixture.users_id(), tree.root())
            .expect("publish query fixture table root");
    }
    writer
        .finish(&mut batch)
        .expect("finish query fixture catalog");
    batch.commit().expect("commit query fixture rows");
    fixture
}

pub(crate) fn phase_4c1_corrupt_query_relation_fixture() -> Phase4bCatalogFixture {
    let uuid_byte = NEXT_QUERY_DATABASE_UUID.fetch_add(1, Ordering::Relaxed);
    let fixture = phase_4b_catalog_fixture_with_uuid(4096, [uuid_byte; 16]);
    let pager = fixture.pager();
    let mut snapshot = pager.begin_read().expect("open corrupt query catalog");
    let catalog = snapshot
        .catalog_reader()
        .expect("read corrupt query catalog");
    let table = catalog
        .find_table_by_id(fixture.users_id())
        .expect("corrupt query USERS table")
        .clone();
    drop(snapshot);

    let mut batch = pager.begin_write().expect("begin corrupt query write");
    let mut writer = CatalogWriter::bootstrap(&mut batch).expect("open corrupt query catalog");
    let mut tree = BTree::from_root(TreeKind::Table, table.root());
    let rowid = writer
        .allocate_rowid(&mut batch, fixture.users_id())
        .expect("allocate corrupt query ROWID");
    tree.insert(&mut batch, &encode_rowid_key(rowid), b"not-a-valid-row")
        .expect("insert corrupt query row");
    writer
        .update_table_root(&mut batch, fixture.users_id(), tree.root())
        .expect("publish corrupt query table root");
    writer
        .finish(&mut batch)
        .expect("finish corrupt query catalog");
    batch.commit().expect("commit corrupt query row");
    fixture
}

pub(crate) fn phase_4c1_corrupt_batch_catalog(batch: &mut WriteBatch) {
    let (mut catalog, _) = batch
        .catalog_tree_with_report(TreeKind::Table)
        .expect("read construction-failure catalog")
        .expect("construction-failure catalog root");
    catalog
        .insert(batch, &[0x11], b"invalid-catalog-record")
        .expect("insert invalid catalog record");
    batch
        .set_catalog_root(catalog.root())
        .expect("retain invalid catalog root");
}

fn insert_phase_4b_table(
    writer: &mut CatalogWriter,
    batch: &mut WriteBatch,
    name: &str,
    columns: &[(&str, ColumnType, bool)],
) -> Result<TableId> {
    let table_id = TableId::new(writer.allocate_object_id(batch)?.get())?;
    let table_root = BTree::create(batch, TreeKind::Table)?.root();
    let table = TableDef::new(
        table_id,
        SchemaVersion::new(1)?,
        table_root,
        RowId::new(1)?,
        u16::try_from(columns.len())
            .map_err(|_| DbError::new(ErrorCode::Type, "fixture column count exceeds u16"))?,
        CatalogName::new(name)?,
    )?;
    writer.insert_table_definition(batch, &table)?;
    for (ordinal, (name, column_type, nullable)) in columns.iter().enumerate() {
        let column_id = ColumnId::new(writer.allocate_object_id(batch)?.get())?;
        let column = ColumnDef::new(
            column_id,
            table_id,
            u16::try_from(ordinal)
                .map_err(|_| DbError::new(ErrorCode::Type, "fixture ordinal exceeds u16"))?,
            *column_type,
            *nullable,
            CatalogName::new(*name)?,
        )?;
        writer.insert_column_definition(batch, &column)?;
    }
    Ok(table_id)
}

pub(crate) fn phase_4b_add_table(
    pager: &Pager,
    name: &str,
    columns: &[(&str, ColumnType, bool)],
) -> Result<TableId> {
    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;
    let table_id = insert_phase_4b_table(&mut writer, &mut batch, name, columns)?;
    writer.finish(&mut batch)?;
    batch.commit()?;
    Ok(table_id)
}

#[test]
fn phase_4b_bridge_reads_uuid_and_real_catalog_from_both_transaction_kinds() {
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    assert_ne!(pager.database_uuid().unwrap(), [0; 16]);

    let mut read = pager.begin_read().unwrap();
    assert!(
        read.catalog_reader()
            .unwrap()
            .find_table_by_id(fixture.users_id())
            .is_some()
    );
    drop(read);

    let mut write = pager.begin_write().unwrap();
    assert!(
        write
            .catalog_reader()
            .unwrap()
            .find_table_by_id(fixture.users_id())
            .is_some()
    );
    write.rollback().unwrap();
}

#[test]
fn phase_4b_catalog_fixture_has_exact_checked_schema_and_paths() {
    let fixture = phase_4b_catalog_fixture(4096);
    assert!(fixture.main_path().is_file());
    assert!(fixture.wal_path().is_file());

    let mut read = fixture.pager().begin_read().unwrap();
    let catalog = read.catalog_reader().unwrap();
    let users = catalog.find_table_by_id(fixture.users_id()).unwrap();
    let orders = catalog.find_table_by_id(fixture.orders_id()).unwrap();
    assert_eq!(users.name().as_str(), "USERS");
    assert_eq!(orders.name().as_str(), "ORDERS");

    let users_columns = catalog
        .columns_for_table(fixture.users_id())
        .map(|column| {
            (
                column.name().as_str(),
                column.column_type(),
                column.nullable(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        users_columns,
        vec![
            ("ID", ColumnType::Int64, false),
            ("NAME", ColumnType::Text, true),
            ("AMOUNT", ColumnType::Decimal, true),
            ("FLOAT_COL", ColumnType::Float64, true),
        ]
    );
    let orders_columns = catalog
        .columns_for_table(fixture.orders_id())
        .map(|column| {
            (
                column.name().as_str(),
                column.column_type(),
                column.nullable(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        orders_columns,
        vec![
            ("ID", ColumnType::Int64, false),
            ("USER_ID", ColumnType::Int64, false),
        ]
    );
}

#[test]
fn phase_4b_write_catalog_reader_rejects_a_pending_catalog_writer() {
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    let mut write = pager.begin_write().unwrap();
    let _writer = CatalogWriter::bootstrap(&mut write).unwrap();
    assert_eq!(
        write.catalog_reader().unwrap_err().code(),
        ErrorCode::Transaction
    );
    write.rollback().unwrap();
}

#[test]
fn phase_4b_catalog_bridge_preserves_an_uninitialized_empty_image() {
    let (_temp, pager) = test_pager(4096).unwrap();
    let absent = TableId::new(1).unwrap();

    let mut read = pager.begin_read().unwrap();
    assert!(
        read.catalog_reader()
            .unwrap()
            .find_table_by_id(absent)
            .is_none()
    );
    drop(read);

    let mut write = pager.begin_write().unwrap();
    assert!(
        write
            .catalog_reader()
            .unwrap()
            .find_table_by_id(absent)
            .is_none()
    );
    write.rollback().unwrap();
}

fn sanitized_label(label: &str) -> String {
    let label: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if label.is_empty() {
        "database".to_owned()
    } else {
        label
    }
}

#[test]
fn temporary_database_skips_a_direct_name_collision_and_removes_only_its_exact_file() {
    let label = "direct-collision";
    let first_sequence = NEXT_TEMP_DATABASE.fetch_add(2, Ordering::Relaxed);
    let retry_sequence = first_sequence
        .checked_add(1)
        .expect("temporary database retry sequence must not overflow");
    let collision = std::env::temp_dir().join(format!(
        "ora-sqllite-{}-{label}-{first_sequence}.db",
        std::process::id(),
    ));
    fs::write(&collision, b"pre-existing collision").expect("create collision file");

    let mut sequences = [first_sequence, retry_sequence].into_iter();
    let database = TempDatabase::new_with_sequence(label, || {
        sequences
            .next()
            .expect("collision retry must consume exactly two candidates")
    });
    let database_path = database.path().to_path_buf();
    assert_eq!(
        database_path,
        std::env::temp_dir().join(format!(
            "ora-sqllite-{}-{label}-{retry_sequence}.db",
            std::process::id()
        )),
        "TempDatabase::new must retry immediately after the occupied first candidate"
    );
    let sentinel = database_path.with_extension("sentinel");
    let wal = wal_path(&database_path);
    assert_ne!(database_path, collision);
    fs::write(&database_path, b"owned database file").expect("create owned file");
    fs::write(&wal, b"owned WAL file").expect("create owned WAL file");
    fs::write(&sentinel, b"unrelated sentinel").expect("create sentinel file");
    drop(database);

    assert!(
        collision.exists(),
        "Drop must not remove a collided filename"
    );
    assert!(
        !database_path.exists(),
        "Drop must remove its exact generated filename"
    );
    assert!(
        !wal.exists(),
        "Drop must remove the WAL paired with its exact generated filename"
    );
    assert!(
        sentinel.exists(),
        "Drop must not remove an adjacent unrelated filename"
    );
    fs::remove_file(collision).expect("remove collision fixture");
    fs::remove_file(sentinel).expect("remove sentinel fixture");
}

#[test]
fn multi_reader_single_writer_matches_reference_model() {
    use std::{
        sync::{Barrier, mpsc},
        thread,
    };

    const FIRST_COMMIT: u64 = 1;
    const FINAL_COMMIT: u64 = 20;

    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Pager>();
    assert_send::<super::pager::ReadSnapshot>();
    assert_send_sync::<super::cache::PageHandle>();

    let temp = TempDatabase::new("multi-reader-single-writer");
    let pager = Arc::new(
        Pager::create(temp.path(), PagerConfig::test_default(4096))
            .expect("create concurrent pager"),
    );
    let root = create_concurrency_table(&pager).expect("commit LSN-1 fixture");
    assert_eq!(
        pager.latest_lsn().expect("read fixture LSN"),
        Lsn(FIRST_COMMIT)
    );

    let mut old_snapshot = pager.begin_read().expect("open LSN-1 snapshot");
    assert_eq!(old_snapshot.lsn(), Lsn(FIRST_COMMIT));
    let initial_model = BTreeMap::from([
        (b"counter".to_vec(), vec![1]),
        (b"stable".to_vec(), b"unchanged".to_vec()),
    ]);
    let start = Arc::new(Barrier::new(3));
    let (old_commit_tx, old_commit_rx) = mpsc::sync_channel(0);
    let (old_done_tx, old_done_rx) = mpsc::sync_channel(0);
    let (fresh_commit_tx, fresh_commit_rx) = mpsc::sync_channel(0);
    let (fresh_done_tx, fresh_done_rx) = mpsc::sync_channel(0);
    let writer_initial_model = initial_model.clone();

    let (final_model, old_observations, fresh_observations) = thread::scope(|scope| {
        let writer_pager = Arc::clone(&pager);
        let writer_start = Arc::clone(&start);
        let writer = scope.spawn(move || -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
            let mut tree = BTree::from_root(TreeKind::Table, root);
            let mut model = writer_initial_model;
            writer_start.wait();
            for commit in (FIRST_COMMIT + 1)..=FINAL_COMMIT {
                let mut batch = writer_pager.begin_write()?;
                let removed = tree.delete(&mut batch, b"counter")?;
                assert_eq!(removed, model.remove(b"counter".as_slice()));
                let value = vec![u8::try_from(commit).expect("test commit fits u8")];
                tree.insert(&mut batch, b"counter", &value)?;
                batch.track_tree(TreeKind::Table, tree.root())?;
                batch.set_catalog_root(tree.root())?;
                batch.commit()?;
                model.insert(b"counter".to_vec(), value);

                old_commit_tx
                    .send(())
                    .expect("old reader remains available");
                fresh_commit_tx
                    .send((Lsn(commit), model.clone()))
                    .expect("fresh reader remains available");
                old_done_rx.recv().expect("old reader completed commit");
                fresh_done_rx.recv().expect("fresh reader completed commit");
            }
            Ok(model)
        });

        let old_start = Arc::clone(&start);
        let old_reader = scope.spawn(move || -> Result<Vec<BTreeMap<Vec<u8>, Vec<u8>>>> {
            let tree = BTree::from_root(TreeKind::Table, root);
            let mut observations = Vec::new();
            old_start.wait();
            while old_commit_rx.recv().is_ok() {
                let observed = collect_all(&tree, &mut old_snapshot)?.into_iter().collect();
                observations.push(observed);
                old_done_tx.send(()).expect("writer remains available");
            }
            Ok(observations)
        });

        let fresh_pager = Arc::clone(&pager);
        let fresh_start = Arc::clone(&start);
        let fresh_reader = scope.spawn(move || -> Result<Vec<_>> {
            let tree = BTree::from_root(TreeKind::Table, root);
            let mut observations = Vec::new();
            fresh_start.wait();
            while let Ok((expected_lsn, expected_model)) = fresh_commit_rx.recv() {
                let mut snapshot = fresh_pager.begin_read()?;
                let actual_lsn = snapshot.lsn();
                let actual_model: BTreeMap<_, _> =
                    collect_all(&tree, &mut snapshot)?.into_iter().collect();
                observations.push((expected_lsn, actual_lsn, expected_model, actual_model));
                fresh_done_tx.send(()).expect("writer remains available");
            }
            Ok(observations)
        });

        (
            writer
                .join()
                .expect("writer thread must not panic")
                .expect("writer commits"),
            old_reader
                .join()
                .expect("old-reader thread must not panic")
                .expect("old-reader observations"),
            fresh_reader
                .join()
                .expect("fresh-reader thread must not panic")
                .expect("fresh-reader observations"),
        )
    });

    assert_eq!(old_observations.len(), 19);
    for observed in old_observations {
        assert_eq!(observed, initial_model, "LSN-1 snapshot must stay fixed");
    }
    assert_eq!(fresh_observations.len(), 19);
    let mut previous_lsn = Lsn(FIRST_COMMIT);
    for (expected_lsn, actual_lsn, expected_model, actual_model) in fresh_observations {
        assert!(actual_lsn >= previous_lsn, "fresh snapshot LSN regressed");
        assert_eq!(
            actual_lsn, expected_lsn,
            "reader must see the released commit"
        );
        assert_eq!(
            actual_model, expected_model,
            "fresh snapshot diverged from model"
        );
        previous_lsn = actual_lsn;
    }

    let tree = BTree::from_root(TreeKind::Table, root);
    let mut final_snapshot = pager.begin_read().expect("open final snapshot");
    assert_eq!(final_snapshot.lsn(), Lsn(FINAL_COMMIT));
    let final_actual: BTreeMap<_, _> = collect_all(&tree, &mut final_snapshot)
        .expect("scan final snapshot")
        .into_iter()
        .collect();
    assert_eq!(final_actual, final_model);
}

#[test]
fn unfinished_writer_is_busy_readers_progress_and_checkpoint_waits_for_rollback() {
    use std::{sync::mpsc, thread};

    let temp = TempDatabase::new("writer-checkpoint-contention");
    let pager = Arc::new(
        Pager::create(temp.path(), PagerConfig::test_default(4096))
            .expect("create contention pager"),
    );
    let root = create_concurrency_table(&pager).expect("commit contention fixture");
    let (writer_ready_tx, writer_ready_rx) = mpsc::sync_channel(0);
    let (release_writer_tx, release_writer_rx) = mpsc::sync_channel(0);
    let (checkpoint_go_tx, checkpoint_go_rx) = mpsc::sync_channel(0);
    let (checkpoint_contended_tx, checkpoint_contended_rx) = mpsc::sync_channel(0);
    let (checkpoint_done_tx, checkpoint_done_rx) = mpsc::sync_channel(0);

    thread::scope(|scope| {
        let writer_pager = Arc::clone(&pager);
        let writer = scope.spawn(move || -> Result<()> {
            let mut tree = BTree::from_root(TreeKind::Table, root);
            let mut batch = writer_pager.begin_write()?;
            assert_eq!(tree.delete(&mut batch, b"counter")?, Some(vec![1]));
            tree.insert(&mut batch, b"counter", b"uncommitted")?;
            writer_ready_tx
                .send(())
                .expect("contention driver remains available");
            release_writer_rx
                .recv()
                .expect("contention driver releases writer");
            drop(batch);
            Ok(())
        });

        let checkpoint_pager = Arc::clone(&pager);
        let checkpoint = scope.spawn(move || {
            checkpoint_go_rx.recv().expect("checkpoint is started");
            let result =
                super::pager::checkpoint_after_writer_contention(&checkpoint_pager, || {
                    checkpoint_contended_tx
                        .send(())
                        .expect("contention driver remains available");
                });
            checkpoint_done_tx
                .send(result)
                .expect("contention driver receives checkpoint result");
        });

        writer_ready_rx
            .recv()
            .expect("unfinished writer acquired guard");
        let busy = match pager.try_begin_write() {
            Ok(_) => panic!("second writer must not acquire the active writer guard"),
            Err(error) => error,
        };
        assert_eq!(busy.code(), ErrorCode::Busy);

        let tree = BTree::from_root(TreeKind::Table, root);
        let mut reader = pager
            .begin_read()
            .expect("reader opens beside unfinished writer");
        assert_eq!(reader.lsn(), Lsn(1));
        assert_eq!(
            tree.get(&mut reader, b"counter").expect("reader lookup"),
            Some(vec![1])
        );
        drop(reader);

        checkpoint_go_tx.send(()).expect("start checkpoint attempt");
        checkpoint_contended_rx
            .recv()
            .expect("checkpoint observed the active writer before blocking");
        release_writer_tx
            .send(())
            .expect("roll back unfinished writer");
        writer
            .join()
            .expect("writer thread must not panic")
            .expect("writer rollback");
        let result = checkpoint_done_rx
            .recv()
            .expect("checkpoint completes after writer rollback")
            .expect("checkpoint succeeds");
        assert_eq!(result.to_lsn, Lsn(1));
        checkpoint.join().expect("checkpoint thread must not panic");
    });
}

#[test]
#[ignore = "spawned by database_lock_is_process_wide_and_releases_windows_paths"]
fn database_lock_child() {
    let path = std::env::var_os("ORA_SQLLITE_LOCK_CHILD_PATH")
        .expect("lock child database path is provided");
    let expected =
        std::env::var("ORA_SQLLITE_LOCK_CHILD_EXPECT").expect("lock child expectation is provided");
    let marker_path = std::env::var_os("ORA_SQLLITE_LOCK_CHILD_MARKER_PATH")
        .expect("lock child marker path is provided");
    let nonce =
        std::env::var("ORA_SQLLITE_LOCK_CHILD_NONCE").expect("lock child nonce is provided");
    let opened = Pager::open(
        Path::new(&path),
        OpenMode::ReadWrite,
        PagerConfig::test_default(4096),
    );
    match expected.as_str() {
        "busy" => match opened {
            Ok(_) => panic!("child unexpectedly acquired its parent's database lock"),
            Err(error) => assert_eq!(error.code(), ErrorCode::Busy),
        },
        "open" => drop(opened.expect("child opens database after parent Pager drop")),
        other => panic!("unknown lock child expectation: {other}"),
    }
    let mut marker = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker_path)
        .expect("create unique lock child marker");
    marker
        .write_all(format!("{expected}:{nonce}").as_bytes())
        .expect("write lock child marker");
    marker.sync_all().expect("sync lock child marker");
}

#[test]
fn database_lock_is_process_wide_and_releases_windows_paths() {
    use std::process::Command;

    let temp = TempDatabase::new("database-lock-process");
    let database_path = temp.path().to_path_buf();
    let wal = wal_path(&database_path);
    let pager =
        Pager::create(&database_path, PagerConfig::test_default(4096)).expect("create lock owner");
    let executable = std::env::current_exe().expect("locate current test executable");
    let busy_nonce = format!(
        "{}-{}-busy",
        std::process::id(),
        NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let busy_marker = database_path.with_extension(format!("{busy_nonce}.marker"));

    let busy = Command::new(&executable)
        .args([
            "--exact",
            "storage::test_support::database_lock_child",
            "--ignored",
        ])
        .env("ORA_SQLLITE_LOCK_CHILD_PATH", &database_path)
        .env("ORA_SQLLITE_LOCK_CHILD_EXPECT", "busy")
        .env("ORA_SQLLITE_LOCK_CHILD_MARKER_PATH", &busy_marker)
        .env("ORA_SQLLITE_LOCK_CHILD_NONCE", &busy_nonce)
        .status()
        .expect("run lock-contention child");
    assert_eq!(
        busy.code(),
        Some(0),
        "active owner must map child open to Busy"
    );
    verify_and_remove_lock_child_marker(&busy_marker, "busy", &busy_nonce);

    drop(pager);
    let open_nonce = format!(
        "{}-{}-open",
        std::process::id(),
        NEXT_TEMP_DATABASE.fetch_add(1, Ordering::Relaxed)
    );
    let open_marker = database_path.with_extension(format!("{open_nonce}.marker"));
    let opened = Command::new(executable)
        .args([
            "--exact",
            "storage::test_support::database_lock_child",
            "--ignored",
        ])
        .env("ORA_SQLLITE_LOCK_CHILD_PATH", &database_path)
        .env("ORA_SQLLITE_LOCK_CHILD_EXPECT", "open")
        .env("ORA_SQLLITE_LOCK_CHILD_MARKER_PATH", &open_marker)
        .env("ORA_SQLLITE_LOCK_CHILD_NONCE", &open_nonce)
        .status()
        .expect("run post-drop child");
    assert_eq!(opened.code(), Some(0), "drop must release the process lock");
    verify_and_remove_lock_child_marker(&open_marker, "open", &open_nonce);

    fs::remove_file(&wal).expect("remove exact WAL path after child exit");
    fs::remove_file(&database_path).expect("remove exact database path after child exit");
    assert!(!database_path.exists());
    assert!(!wal.exists());
    assert!(!busy_marker.exists());
    assert!(!open_marker.exists());
}

fn verify_and_remove_lock_child_marker(marker: &Path, mode: &str, nonce: &str) {
    let contents = fs::read_to_string(marker)
        .expect("lock child marker is missing; the exact helper may not have run");
    fs::remove_file(marker).expect("remove exact lock child marker");
    assert_eq!(contents, format!("{mode}:{nonce}"));
}

fn create_concurrency_table(pager: &Pager) -> Result<PageRef> {
    let mut batch = pager.begin_write()?;
    let mut tree = BTree::create(&mut batch, TreeKind::Table)?;
    tree.insert(&mut batch, b"counter", &[1])?;
    tree.insert(&mut batch, b"stable", b"unchanged")?;
    batch.track_tree(TreeKind::Table, tree.root())?;
    batch.set_catalog_root(tree.root())?;
    let root = tree.root();
    batch.commit()?;
    Ok(root)
}

pub(crate) struct RecordingFaults {
    fail_on: Option<(FaultPoint, usize)>,
    observed: Mutex<Vec<FaultPoint>>,
}

impl RecordingFaults {
    pub(crate) fn never() -> Self {
        Self {
            fail_on: None,
            observed: Mutex::new(Vec::new()),
        }
    }

    fn at(point: FaultPoint, occurrence: usize) -> Self {
        Self {
            fail_on: Some((point, occurrence)),
            observed: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn observed(&self) -> Vec<FaultPoint> {
        self.observed.lock().expect("fault record lock").clone()
    }
}

impl FaultInjector for RecordingFaults {
    fn check(&self, point: FaultPoint) -> Result<()> {
        let mut observed = self.observed.lock().expect("fault record lock");
        observed.push(point);
        let occurrence = observed.iter().filter(|&&seen| seen == point).count();
        if self.fail_on == Some((point, occurrence)) {
            return Err(DbError::new(ErrorCode::Io, "injected pager I/O fault"));
        }
        Ok(())
    }
}

pub(crate) fn phase_4c1_recording_crash_pager(path: &Path) -> (Pager, Arc<RecordingFaults>) {
    let faults = Arc::new(RecordingFaults::never());
    let pager = crash_open(path).with_faults(faults.clone());
    (pager, faults)
}

pub(crate) fn phase_4c1_aborting_crash_pager(
    path: &Path,
    trace: Vec<FaultPoint>,
    point: FaultPoint,
    occurrence: usize,
    marker: String,
) -> Pager {
    crash_open(path).with_faults(Arc::new(Phase4c1AbortAt {
        trace,
        point,
        occurrence,
        marker,
        observed: Mutex::new(Vec::new()),
    }))
}

pub(crate) fn phase_4c1_exact_crash_child_command(child_test: &str) -> std::process::Command {
    let executable = std::env::current_exe().expect("find test executable");
    let mut command = phase_4c1_crash_command_wrapper(&executable);
    command.args(["--exact", child_test, "--ignored", "--nocapture"]);
    command
}

#[cfg(unix)]
fn phase_4c1_crash_command_wrapper(executable: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("sh");
    command.args(["-c", "ulimit -c 0 && exec \"$@\"", "ora-sqllite-crash"]);
    command.arg(executable);
    command
}

#[cfg(not(unix))]
fn phase_4c1_crash_command_wrapper(executable: &Path) -> std::process::Command {
    std::process::Command::new(executable)
}

#[test]
fn phase_4c1_exact_child_command_retains_the_libtest_argument_contract() {
    let child_test = "storage::test_support::phase_4c1_argument_contract_child";
    let command = phase_4c1_exact_crash_child_command(child_test);
    let arguments = command.get_args().collect::<Vec<_>>();
    let expected = ["--exact", child_test, "--ignored", "--nocapture"];
    assert_eq!(
        &arguments[arguments.len() - expected.len()..],
        expected.map(std::ffi::OsStr::new),
        "platform wrapper must forward the exact ignored child test"
    );
}

pub(crate) fn phase_4c1_copy_crash_pair(source: &Path) -> TempDatabase {
    let temp = TempDatabase::new("phase4c1-sql-crash-case");
    assert!(!temp.path().exists() && !wal_path(temp.path()).exists());
    fs::copy(source, temp.path()).expect("copy SQL crash main file");
    fs::copy(wal_path(source), wal_path(temp.path())).expect("copy SQL crash WAL");
    assert_eq!(crash_pair_bytes(temp.path()), crash_pair_bytes(source));
    temp
}

pub(crate) fn phase_4c1_main_only_crash_fixture() -> TempDatabase {
    let temp = TempDatabase::new("phase4c1-main-only-fixture");
    let file = DatabaseFile::create(temp.path(), 4096, [0x4c; 16], DEFAULT_MAX_DATABASE_BYTES)
        .expect("create initialized main file without its WAL");
    drop(file);
    assert!(temp.path().is_file());
    assert!(!wal_path(temp.path()).exists());
    temp
}

pub(crate) fn phase_4c1_copy_main_only(source: &Path) -> TempDatabase {
    let temp = TempDatabase::new("phase4c1-main-only-case");
    assert!(!temp.path().exists() && !wal_path(temp.path()).exists());
    fs::copy(source, temp.path()).expect("copy main-only SQL crash fixture");
    temp
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Phase4c1CrashPathKind {
    Initialization,
    Sql,
}

pub(crate) fn phase_4c1_validate_crash_path(path: PathBuf, kind: Phase4c1CrashPathKind) -> PathBuf {
    assert!(
        path.is_absolute(),
        "invalid SQL crash configuration: relative path"
    );
    assert!(
        !path.components().any(|component| matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )),
        "invalid SQL crash configuration: noncanonical path components"
    );
    let canonical = fs::canonicalize(&path).expect("invalid SQL crash configuration: path");
    assert!(
        canonical.as_os_str() == path.as_os_str(),
        "invalid SQL crash configuration: noncanonical path"
    );
    let canonical_temp =
        fs::canonicalize(std::env::temp_dir()).expect("canonicalize system temp directory");
    assert_eq!(
        path.parent(),
        Some(canonical_temp.as_path()),
        "invalid SQL crash configuration: path outside temp directory"
    );
    assert!(
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(is_phase4c1_case_file_name),
        "invalid SQL crash configuration: non-case path"
    );
    let wal = wal_path(&path);
    match kind {
        Phase4c1CrashPathKind::Initialization => {
            assert!(
                path.is_file() && !wal.exists(),
                "invalid SQL crash configuration: initialization requires main only"
            );
            drop(
                DatabaseFile::open_for_recovery(
                    &path,
                    OpenMode::ReadOnly,
                    DEFAULT_MAX_DATABASE_BYTES,
                )
                .expect("invalid SQL crash configuration: unreadable main file"),
            );
        }
        Phase4c1CrashPathKind::Sql => {
            assert!(
                path.is_file() && wal.is_file(),
                "invalid SQL crash configuration: incomplete database/WAL pair"
            );
            let pager = Pager::open(&path, OpenMode::ReadOnly, PagerConfig::test_default(4096))
                .expect("invalid SQL crash configuration: unreadable database/WAL pair");
            let mut snapshot = pager
                .begin_read()
                .expect("invalid SQL crash configuration: unreadable snapshot");
            let catalog = snapshot
                .catalog_reader()
                .expect("invalid SQL crash configuration: unreadable catalog");
            catalog
                .validate_deep(&mut snapshot)
                .expect("invalid SQL crash configuration: invalid catalog or user tree");
        }
    }
    path
}

fn is_phase4c1_case_file_name(name: &str) -> bool {
    let Some(name) = name
        .strip_prefix("ora-sqllite-")
        .and_then(|name| name.strip_suffix(".db"))
    else {
        return false;
    };
    let Some((process_id, name)) = name.split_once("-phase4c1-") else {
        return false;
    };
    let Some((label, sequence)) = name.rsplit_once("-case-") else {
        return false;
    };
    !process_id.is_empty()
        && process_id.bytes().all(|byte| byte.is_ascii_digit())
        && !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

#[test]
fn phase_4c1_crash_path_validation_accepts_only_owned_case_files_without_writing() {
    let sql_fixture = phase4a_fixture("bootstrap");
    let sql_case = phase_4c1_copy_crash_pair(sql_fixture.path());
    let sql_path = fs::canonicalize(sql_case.path()).unwrap();
    let sql_before = crash_pair_bytes(&sql_path);
    assert_eq!(
        phase_4c1_validate_crash_path(sql_path.clone(), Phase4c1CrashPathKind::Sql),
        sql_path
    );
    assert_eq!(crash_pair_bytes(sql_case.path()), sql_before);

    let init_fixture = phase_4c1_main_only_crash_fixture();
    let init_case = phase_4c1_copy_main_only(init_fixture.path());
    let init_path = fs::canonicalize(init_case.path()).unwrap();
    let init_before = fs::read(&init_path).unwrap();
    assert_eq!(
        phase_4c1_validate_crash_path(init_path.clone(), Phase4c1CrashPathKind::Initialization,),
        init_path
    );
    assert_eq!(fs::read(init_case.path()).unwrap(), init_before);
    assert!(!wal_path(init_case.path()).exists());
}

#[test]
fn phase_4c1_crash_path_validation_rejects_unsafe_paths_without_writing() {
    let fixture = phase4a_fixture("bootstrap");
    let case = phase_4c1_copy_crash_pair(fixture.path());
    let canonical = fs::canonicalize(case.path()).unwrap();
    let before = crash_pair_bytes(&canonical);

    let relative = PathBuf::from(canonical.file_name().unwrap());
    let guard_directory = canonical.with_extension("path-guard-directory");
    fs::create_dir(&guard_directory).expect("create exact path-guard directory");
    #[cfg(windows)]
    let noncanonical = std::env::temp_dir().join(canonical.file_name().unwrap());
    #[cfg(not(windows))]
    let noncanonical = guard_directory
        .join("..")
        .join(canonical.file_name().unwrap());
    for (case_name, path) in [
        ("relative", relative),
        ("noncanonical", noncanonical),
        ("wrong-name", fs::canonicalize(fixture.path()).unwrap()),
    ] {
        let rejected = std::panic::catch_unwind(|| {
            phase_4c1_validate_crash_path(path, Phase4c1CrashPathKind::Sql)
        });
        assert!(
            rejected.is_err(),
            "unsafe {case_name} crash path was accepted"
        );
        assert_eq!(crash_pair_bytes(&canonical), before);
    }

    let nested = guard_directory.join(canonical.file_name().unwrap());
    fs::copy(&canonical, &nested).expect("copy controlled nested main file");
    fs::copy(wal_path(&canonical), wal_path(&nested)).expect("copy controlled nested WAL");
    let nested = fs::canonicalize(nested).unwrap();
    let nested_before = crash_pair_bytes(&nested);
    let rejected = std::panic::catch_unwind(|| {
        phase_4c1_validate_crash_path(nested.clone(), Phase4c1CrashPathKind::Sql)
    });
    assert!(rejected.is_err(), "nested temp path was accepted");
    assert_eq!(crash_pair_bytes(&nested), nested_before);
    fs::remove_file(wal_path(&nested)).expect("remove exact nested WAL");
    fs::remove_file(&nested).expect("remove exact nested main file");
    fs::remove_dir(&guard_directory).expect("remove exact path-guard directory");

    let main_only_fixture = phase_4c1_main_only_crash_fixture();
    let main_only_case = phase_4c1_copy_main_only(main_only_fixture.path());
    let main_only_path = fs::canonicalize(main_only_case.path()).unwrap();
    let main_before = fs::read(&main_only_path).unwrap();
    let rejected = std::panic::catch_unwind(|| {
        phase_4c1_validate_crash_path(main_only_path, Phase4c1CrashPathKind::Sql)
    });
    assert!(rejected.is_err(), "SQL mode accepted a missing WAL");
    assert_eq!(fs::read(main_only_case.path()).unwrap(), main_before);
    assert!(!wal_path(main_only_case.path()).exists());

    let init_case = phase_4c1_copy_crash_pair(fixture.path());
    let init_path = fs::canonicalize(init_case.path()).unwrap();
    let init_before = crash_pair_bytes(&init_path);
    let rejected = std::panic::catch_unwind(|| {
        phase_4c1_validate_crash_path(init_path, Phase4c1CrashPathKind::Initialization)
    });
    assert!(
        rejected.is_err(),
        "initialization mode accepted an existing WAL"
    );
    assert_eq!(crash_pair_bytes(init_case.path()), init_before);
}

pub(crate) fn phase_4c1_wal_initialization_trace(path: &Path) -> Vec<FaultPoint> {
    let file =
        DatabaseFile::open_for_recovery(path, OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)
            .expect("open main-only fixture");
    let faults = Arc::new(RecordingFaults::never());
    let wal = Wal::create_with_faults(path, &file.header(), faults.clone())
        .expect("trace WAL initialization");
    drop(wal);
    drop(file);
    faults.observed()
}

pub(crate) fn phase_4c1_abort_wal_initialization(
    path: &Path,
    trace: Vec<FaultPoint>,
    point: FaultPoint,
    occurrence: usize,
    marker: String,
) {
    let file =
        DatabaseFile::open_for_recovery(path, OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)
            .expect("open main-only crash fixture");
    let faults = Arc::new(Phase4c1AbortAt {
        trace,
        point,
        occurrence,
        marker,
        observed: Mutex::new(Vec::new()),
    });
    let _wal = Wal::create_with_faults(path, &file.header(), faults)
        .expect("initialize WAL through configured crash point");
}

pub(crate) fn phase_4c1_recovered_lsn(path: &Path) -> Lsn {
    let file =
        DatabaseFile::open_for_recovery(path, OpenMode::ReadOnly, DEFAULT_MAX_DATABASE_BYTES)
            .expect("open SQL crash main file without mutation");
    let header = file.header();
    let mut wal =
        Wal::open(path, &header, OpenMode::ReadOnly).expect("open SQL crash WAL without mutation");
    wal.recover(&header, OpenMode::ReadOnly)
        .expect("scan SQL crash WAL with production recovery acceptance")
        .latest_lsn
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Phase4c1SqlStorageState {
    pub(crate) lsn: Lsn,
    pub(crate) catalog_records: BTreeMap<Vec<u8>, Vec<u8>>,
    pub(crate) next_rowid: Option<u64>,
    pub(crate) rows: Vec<(u64, Vec<Value>)>,
}

pub(crate) fn phase_4c1_sql_storage_state(path: &Path) -> Phase4c1SqlStorageState {
    let pager = crash_open(path);
    let mut snapshot = pager.begin_read().expect("open SQL recovery snapshot");
    let catalog = CatalogReader::open_empty_or_v1(&mut snapshot).expect("open recovered catalog");
    catalog
        .validate_deep(&mut snapshot)
        .expect("deep-validate recovered catalog and user trees");
    let catalog_records = snapshot
        .catalog_tree(TreeKind::Table)
        .expect("read recovered catalog tree")
        .map_or_else(BTreeMap::new, |tree| {
            collect_all(&tree, &mut snapshot)
                .expect("scan recovered catalog records")
                .into_iter()
                .collect()
        });
    let table = catalog
        .find_table_by_name(&CatalogName::new("T").expect("valid recovery table name"))
        .cloned();
    let next_rowid = table.as_ref().map(|table| table.next_rowid().get());
    let mut rows = Vec::new();
    if let Some(table) = table {
        let relation = Relation::open(&catalog, table.id()).expect("open recovered T relation");
        let mut scan = relation.scan(PHASE_4A_MAX_ROW);
        while let Some(row) = scan
            .next_snapshot(&mut snapshot)
            .expect("scan recovered T row")
        {
            rows.push((row.rowid().get(), row.values().to_vec()));
        }
    }
    Phase4c1SqlStorageState {
        lsn: snapshot.lsn(),
        catalog_records,
        next_rowid,
        rows,
    }
}

struct Phase4c1AbortAt {
    trace: Vec<FaultPoint>,
    point: FaultPoint,
    occurrence: usize,
    marker: String,
    observed: Mutex<Vec<FaultPoint>>,
}

impl FaultInjector for Phase4c1AbortAt {
    fn check(&self, point: FaultPoint) -> Result<()> {
        let mut observed = self.observed.lock().expect("SQL crash trace lock");
        assert_eq!(
            self.trace.get(observed.len()),
            Some(&point),
            "SQL crash child diverged from successful trace"
        );
        observed.push(point);
        let occurrence = observed.iter().filter(|&&seen| seen == point).count();
        if (point, occurrence) == (self.point, self.occurrence) {
            let mut stderr = std::io::stderr().lock();
            writeln!(stderr, "{} {point:?} {occurrence}", self.marker).unwrap();
            stderr.flush().unwrap();
            std::process::abort();
        }
        Ok(())
    }
}

pub(crate) fn phase_4c1_commit_io_fault_pager(temp: &TempDatabase) -> Pager {
    Pager::create(temp.path(), PagerConfig::test_default(4096))
        .expect("create SQL commit fault pager")
        .with_faults(Arc::new(RecordingFaults::at(
            FaultPoint::BeforeWalRecordWrite,
            1,
        )))
}

#[test]
fn test_fault_injector_is_used_after_pager_creation() {
    let temp = TempDatabase::new("test-fault-injector");
    let faults = Arc::new(RecordingFaults::at(FaultPoint::BeforeWalRecordWrite, 1));
    let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
        .expect("create pager")
        .with_faults(faults.clone());
    let page = {
        let mut batch = pager.begin_write().expect("begin write");
        let page = batch
            .allocate_append(super::page::PageType::TableLeaf)
            .expect("allocate page");
        assert_eq!(
            batch
                .commit()
                .expect_err("configured fault must fail commit")
                .code(),
            ErrorCode::Io
        );
        page
    };

    assert_eq!(faults.observed(), vec![FaultPoint::BeforeWalRecordWrite]);
    assert!(pager.is_poisoned());
    assert_eq!(
        pager
            .read(page)
            .expect_err("unpublished page remains invisible")
            .code(),
        ErrorCode::Corruption
    );
}

#[test]
fn every_wal_commit_fault_requires_recovery_after_a_multi_page_transaction() {
    let baseline_faults = Arc::new(RecordingFaults::never());
    let baseline = commit_multi_page_tree(baseline_faults.clone()).expect("baseline commit");
    assert_commit_trace(&baseline);

    for point in [
        FaultPoint::BeforeWalRecordWrite,
        FaultPoint::AfterWalRecordHeaderPrefixWrite,
        FaultPoint::AfterWalRecordPayloadPrefixWrite,
        FaultPoint::AfterWalRecordWrite,
        FaultPoint::BeforeWalSync,
        FaultPoint::AfterWalSync,
    ] {
        let occurrences = baseline.iter().filter(|&&seen| seen == point).count();
        for occurrence in 1..=occurrences {
            let faults = Arc::new(RecordingFaults::at(point, occurrence));
            let temp = TempDatabase::new(&format!("multi-page-fault-{point:?}-{occurrence}"));
            let pager = Pager::create(temp.path(), PagerConfig::test_default(4096))
                .expect("create fault pager")
                .with_faults(faults.clone());
            let root = {
                let mut batch = pager.begin_write().expect("begin multi-page batch");
                let mut tree = BTree::create(&mut batch, TreeKind::Table).expect("create tree");
                insert_multi_page_fixture(&mut tree, &mut batch).expect("stage tree pages");
                batch
                    .track_tree(TreeKind::Table, tree.root())
                    .expect("track tree");
                batch.set_catalog_root(tree.root()).expect("publish root");
                assert!(batch.page_count().expect("page count") > 3);
                let root = tree.root();
                assert_eq!(
                    batch
                        .commit()
                        .expect_err("injected multi-page fault must fail")
                        .code(),
                    ErrorCode::Io
                );
                root
            };

            let stop = nth_fault_index(&baseline, point, occurrence);
            assert_eq!(faults.observed(), baseline[..=stop]);
            assert!(pager.is_poisoned());
            assert_eq!(
                pager
                    .read(root)
                    .expect_err("unpublished root remains invisible")
                    .code(),
                ErrorCode::Corruption
            );
            let write_error = match pager.begin_write() {
                Ok(_) => panic!("poisoned pager must reject write batches"),
                Err(error) => error,
            };
            assert_eq!(write_error.code(), ErrorCode::Transaction);

            drop(pager);
            if let Ok(reopened) = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            ) {
                assert!(!reopened.is_poisoned());
            }
        }
    }
}

fn commit_multi_page_tree(faults: Arc<RecordingFaults>) -> Result<Vec<FaultPoint>> {
    let temp = TempDatabase::new("multi-page-fault-baseline");
    let pager =
        Pager::create(temp.path(), PagerConfig::test_default(4096))?.with_faults(faults.clone());
    let mut batch = pager.begin_write()?;
    let mut tree = BTree::create(&mut batch, TreeKind::Table)?;
    insert_multi_page_fixture(&mut tree, &mut batch)?;
    batch.track_tree(TreeKind::Table, tree.root())?;
    batch.set_catalog_root(tree.root())?;
    batch.commit()?;
    Ok(faults.observed())
}

fn insert_multi_page_fixture(tree: &mut BTree, batch: &mut super::pager::WriteBatch) -> Result<()> {
    for index in 0..30u8 {
        tree.insert(batch, &index.to_be_bytes(), &[index; 400])?;
    }
    Ok(())
}

fn nth_fault_index(events: &[FaultPoint], point: FaultPoint, occurrence: usize) -> usize {
    events
        .iter()
        .enumerate()
        .filter(|(_, seen)| **seen == point)
        .nth(occurrence - 1)
        .map(|(index, _)| index)
        .expect("recorded configured fault occurrence")
}

const MODEL_OPERATIONS: usize = 10_000;
const MODEL_VALIDATION_INTERVAL: usize = 100;
const MODEL_PERSISTENCE_INTERVAL: usize = 1_000;
const MODEL_SEED: u64 = 0x8d26_1a75_40ce_9b31;
const MODEL_DUPLICATE_INSERTS_PER_WINDOW: usize = 9;
const MODEL_MIN_DISTINCT_KEYS: usize = 3_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelProof {
    operations: usize,
    validations: usize,
    commits: usize,
    reopens: usize,
    distinct_keys: usize,
    tree_height: u64,
    duplicate_insert_attempts: usize,
}

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
#[ignore = "exhaustive six-configuration storage model; run in dedicated CI"]
fn fixed_seed_reference_model_covers_every_page_size_and_tree_kind() {
    let mut proofs = Vec::new();
    for page_size in [4096, 8192, 16384] {
        for kind in [TreeKind::Table, TreeKind::Index] {
            let proof = run_reference_model(page_size, kind).expect("fixed-seed reference model");
            proofs.push((page_size, kind, proof));
        }
    }

    for (page_size, kind, proof) in proofs {
        assert_eq!(proof.operations, MODEL_OPERATIONS);
        assert_eq!(proof.validations, MODEL_OPERATIONS / 100);
        assert_eq!(proof.commits, MODEL_OPERATIONS / MODEL_PERSISTENCE_INTERVAL);
        assert_eq!(proof.reopens, MODEL_OPERATIONS / MODEL_PERSISTENCE_INTERVAL);
        assert!(
            proof.distinct_keys >= MODEL_MIN_DISTINCT_KEYS,
            "page_size={page_size}, kind={kind:?} must exercise high-cardinality keys"
        );
        assert!(
            proof.tree_height >= 2,
            "page_size={page_size}, kind={kind:?} must form a nontrivial B+Tree shape"
        );
        assert_eq!(
            proof.duplicate_insert_attempts,
            (MODEL_OPERATIONS / MODEL_VALIDATION_INTERVAL) * MODEL_DUPLICATE_INSERTS_PER_WINDOW,
            "page_size={page_size}, kind={kind:?} must deliberately exercise duplicate inserts"
        );
    }
}

#[test]
fn reference_model_commits_once_per_persistence_window() {
    let proof = run_reference_model_operations(4096, TreeKind::Table, 1_000)
        .expect("reference-model persistence window");

    assert_eq!(proof.validations, 10);
    assert_eq!(proof.commits, 1, "1,000 operations must use one commit");
    assert_eq!(proof.reopens, 1);
}

fn run_reference_model(page_size: u32, kind: TreeKind) -> Result<ModelProof> {
    run_reference_model_operations(page_size, kind, MODEL_OPERATIONS)
}

fn run_reference_model_operations(
    page_size: u32,
    kind: TreeKind,
    operations: usize,
) -> Result<ModelProof> {
    assert_eq!(
        operations % MODEL_VALIDATION_INTERVAL,
        0,
        "model runs whole validation windows"
    );
    assert_eq!(
        operations % MODEL_PERSISTENCE_INTERVAL,
        0,
        "model commits whole persistence windows"
    );
    let temp = TempDatabase::new(&format!("model-{page_size}-{kind:?}"));
    let mut pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    let mut tree = create_catalog_tree(&mut pager, kind)?;
    let mut model = BTreeMap::new();
    let mut random = XorShift64(MODEL_SEED);
    let mut fresh_key_sequence = 0u64;
    let mut query_key_sequence = 0u64;
    let mut seen_key_lengths = [false; 65];
    let mut validations = 0;
    let mut commits = 0;
    let mut reopens = 0;
    let mut duplicate_insert_attempts = 0;

    for persistence_window in 0..(operations / MODEL_PERSISTENCE_INTERVAL) {
        let mut batch = pager.begin_write()?;
        for checkpoint in 0..(MODEL_PERSISTENCE_INTERVAL / MODEL_VALIDATION_INTERVAL) {
            let window = persistence_window
                * (MODEL_PERSISTENCE_INTERVAL / MODEL_VALIDATION_INTERVAL)
                + checkpoint;
            for insert_index in 0..45 {
                let deliberate_duplicate = insert_index % 5 == 4;
                let key = if deliberate_duplicate {
                    known_model_key(&mut random, &model)
                } else {
                    fresh_workload_key(&mut random, &mut fresh_key_sequence, &model)
                };
                seen_key_lengths[key.len()] = true;
                let value = workload_value(page_size, &mut random);
                let expected = if deliberate_duplicate {
                    Err(ErrorCode::Constraint)
                } else {
                    Ok(())
                };
                let actual = tree
                    .insert(&mut batch, &key, &value)
                    .map_err(|error| error.code());
                assert_eq!(
                    actual, expected,
                    "insert result must mirror the reference model"
                );
                if deliberate_duplicate {
                    duplicate_insert_attempts += 1;
                } else {
                    model.insert(key, value);
                }
            }
            for delete_index in 0..25 {
                let key = query_workload_key(&mut random, &mut query_key_sequence, &model, 8);
                seen_key_lengths[key.len()] = true;
                let expected = model.remove(&key);
                let actual = tree.delete(&mut batch, &key).map_err(|error| {
                    DbError::new(
                        error.code(),
                        format!(
                            "reference-model delete failed at page_size={page_size}, kind={kind:?}, window={window}, delete={delete_index}: {error}"
                        ),
                    )
                })?;
                assert_eq!(
                    actual, expected,
                    "delete result must mirror the reference model"
                );
            }

            for _ in 0..20 {
                let key = query_workload_key(&mut random, &mut query_key_sequence, &model, 2);
                seen_key_lengths[key.len()] = true;
                assert_eq!(
                    tree.get(&mut batch, &key)?,
                    model.get(&key).cloned(),
                    "point lookup must mirror the reference model"
                );
            }
            for _ in 0..10 {
                let first = query_workload_key(&mut random, &mut query_key_sequence, &model, 2);
                let second = query_workload_key(&mut random, &mut query_key_sequence, &model, 2);
                seen_key_lengths[first.len()] = true;
                seen_key_lengths[second.len()] = true;
                let (start, end) = if first <= second {
                    (first, second)
                } else {
                    (second, first)
                };
                assert_eq!(
                    collect_range(&tree, &mut batch, &start, &end)?,
                    model_range(&model, &start, &end),
                    "range scan must mirror the reference model"
                );
            }

            validate_tree(&mut batch, &tree)?;
            validations += 1;
        }

        batch.track_tree(kind, tree.root())?;
        batch.set_catalog_root(tree.root())?;
        batch.commit()?;
        commits += 1;

        drop(pager);
        pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(page_size),
        )?;
        let mut snapshot = pager.begin_read()?;
        tree = snapshot
            .catalog_tree(kind)?
            .ok_or_else(|| DbError::new(ErrorCode::Corruption, "model catalog root is missing"))?;
        assert_eq!(
            collect_all(&tree, &mut snapshot)?,
            model
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Vec<_>>(),
            "full scan after reopen must mirror the reference model"
        );
        reopens += 1;
    }

    assert!(
        seen_key_lengths.into_iter().all(|seen| seen),
        "fixed-seed model must cover key lengths 0 through 64"
    );
    let mut snapshot = pager.begin_read()?;
    let tree_height = tree_height(&mut snapshot, tree.root())?;

    Ok(ModelProof {
        operations,
        validations,
        commits,
        reopens,
        distinct_keys: model.len(),
        tree_height,
        duplicate_insert_attempts,
    })
}

fn create_catalog_tree(pager: &mut Pager, kind: TreeKind) -> Result<BTree> {
    let mut batch = pager.begin_write()?;
    let tree = BTree::create(&mut batch, kind)?;
    batch.track_tree(kind, tree.root())?;
    batch.set_catalog_root(tree.root())?;
    batch.commit()?;
    Ok(tree)
}

fn fresh_workload_key(
    random: &mut XorShift64,
    next_sequence: &mut u64,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
    loop {
        let sequence = next_workload_key_sequence(next_sequence);
        let key = sequenced_workload_key(random, sequence);
        if !model.contains_key(&key) {
            return key;
        }
    }
}

fn query_workload_key(
    random: &mut XorShift64,
    next_sequence: &mut u64,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    existing_divisor: u64,
) -> Vec<u8> {
    assert_ne!(
        existing_divisor, 0,
        "query key selection divisor must be nonzero"
    );
    if !model.is_empty() && random.next() % existing_divisor == 0 {
        known_model_key(random, model)
    } else {
        sequenced_workload_key(random, next_workload_key_sequence(next_sequence))
    }
}

fn known_model_key(random: &mut XorShift64, model: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let model_len = u64::try_from(model.len()).expect("model length fits u64");
    let index = usize::try_from(random.next() % model_len).expect("model index fits usize");
    model
        .keys()
        .nth(index)
        .expect("nonempty model must contain the selected key")
        .clone()
}

fn next_workload_key_sequence(next_sequence: &mut u64) -> u64 {
    let sequence = *next_sequence;
    *next_sequence = next_sequence
        .checked_add(1)
        .expect("fixed-seed key sequence must not overflow");
    sequence
}

fn sequenced_workload_key(random: &mut XorShift64, sequence: u64) -> Vec<u8> {
    let key_len = usize::try_from(sequence % 65).expect("model key length fits usize");
    let mut key = vec![0; key_len];
    let entropy = random.next().to_le_bytes();
    let sequence_bytes = sequence.to_le_bytes();
    for (index, byte) in key.iter_mut().enumerate() {
        let index_byte = u8::try_from(index).expect("key index fits u8");
        *byte = entropy[index % entropy.len()]
            ^ sequence_bytes[(index * 3) % sequence_bytes.len()]
                .wrapping_add(index_byte.wrapping_mul(17));
    }
    key
}

fn workload_value(page_size: u32, random: &mut XorShift64) -> Vec<u8> {
    let page_size = usize::try_from(page_size).expect("supported page size fits usize");
    let len = match random.next() % 10 {
        0 => 0,
        1 => 31,
        2..=4 => 127,
        5 | 6 => page_size / 2,
        7 | 8 => page_size.saturating_sub(128),
        _ => page_size + 111,
    };
    vec![random.next() as u8; len]
}

fn collect_all(tree: &BTree, source: &mut impl PageSource) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut cursor = tree.scan(source, Bound::Unbounded, Bound::Unbounded)?;
    let mut entries = Vec::new();
    while let Some(entry) = cursor.next()? {
        entries.push(entry);
    }
    Ok(entries)
}

fn collect_range(
    tree: &BTree,
    source: &mut impl PageSource,
    start: &[u8],
    end: &[u8],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut cursor = tree.scan(source, Bound::Included(start), Bound::Excluded(end))?;
    let mut entries = Vec::new();
    while let Some(entry) = cursor.next()? {
        entries.push(entry);
    }
    Ok(entries)
}

fn model_range(
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    start: &[u8],
    end: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    model
        .iter()
        .filter(|(key, _)| key.as_slice() >= start && key.as_slice() < end)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

struct CorruptionFixture {
    temp: TempDatabase,
    page_size: u32,
    table: BTree,
    index: BTree,
    pages: Vec<(PageRef, PageType)>,
}

#[test]
fn checksum_corruption_is_rejected_for_every_page_type_at_every_page_size() {
    for page_size in [4096, 8192, 16384] {
        let fixture = corruption_fixture(page_size).expect("create corruption fixture");
        for (reference, page_type) in &fixture.pages {
            flip_page_byte(
                fixture.temp.path(),
                fixture.page_size,
                *reference,
                page_size as usize - 1,
            )
            .expect("corrupt whole-page checksum");
            let error = match Pager::open(
                fixture.temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(page_size),
            ) {
                Ok(pager) => pager
                    .read(*reference)
                    .expect_err("checksum-corrupted page must reject reads"),
                Err(error) => error,
            };
            assert_eq!(
                error.code(),
                ErrorCode::Corruption,
                "checksum corruption must reject {page_type:?} at page size {page_size}"
            );
        }
    }
}

#[test]
fn truncation_and_restamped_field_corruption_return_controlled_errors() {
    for truncate_to in [127u64, 4097, 4096] {
        let fixture = corruption_fixture(4096).expect("create truncation fixture");
        truncate_file(fixture.temp.path(), truncate_to).expect("truncate fixture");
        assert_controlled_corruption(|| {
            Pager::open(
                fixture.temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(4096),
            )
            .map(|_| ())
        });
    }

    let fixture = corruption_fixture(4096).expect("create header corruption fixture");
    flip_header_byte(fixture.temp.path(), SuperblockSlot::A, 0).expect("corrupt first superblock");
    Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(4096),
    )
    .expect("one valid superblock remains usable");
    flip_header_byte(fixture.temp.path(), SuperblockSlot::B, 0).expect("corrupt second superblock");
    assert_controlled_corruption(|| {
        Pager::open(
            fixture.temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .map(|_| ())
    });

    let fixture = corruption_fixture(4096).expect("create page id fixture");
    let leaf = fixture.page(PageType::TableLeaf);
    patch_page(&fixture, leaf, 4, &u64::MAX.to_le_bytes()).expect("restamp page id");
    assert_controlled_corruption(|| read_page_from_fixture(&fixture, leaf));

    let fixture = corruption_fixture(4096).expect("create generation fixture");
    let leaf = fixture.page(PageType::TableLeaf);
    patch_page(&fixture, leaf, 12, &0u64.to_le_bytes()).expect("restamp generation");
    assert_controlled_corruption(|| read_page_from_fixture(&fixture, leaf));

    let fixture = corruption_fixture(4096).expect("create slot fixture");
    let leaf = fixture.page(PageType::TableLeaf);
    patch_page(&fixture, leaf, 56, &[0xfe, 0x0f, 0x04, 0x00]).expect("restamp slot");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Table));

    let fixture = corruption_fixture(4096).expect("create sibling fixture");
    let sibling_leaf = fixture
        .pages
        .iter()
        .find_map(|(reference, page_type)| {
            (*page_type == PageType::TableLeaf
                && page_bytes(&fixture, *reference)[28..36] != [0; 8])
                .then_some(*reference)
        })
        .expect("fixture has a linked table leaf");
    patch_page(&fixture, sibling_leaf, 28, &[0; 8]).expect("restamp sibling");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Table));

    let fixture = corruption_fixture(4096).expect("create child fixture");
    let root = fixture.table.root();
    patch_page(&fixture, root, 48, &[0; 8]).expect("restamp child reference");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Table));

    let fixture = corruption_fixture(4096).expect("create overflow fixture");
    let overflow = fixture
        .pages
        .iter()
        .find_map(|(reference, page_type)| {
            (*page_type == PageType::Overflow && page_bytes(&fixture, *reference)[48..56] != [0; 8])
                .then_some(*reference)
        })
        .expect("fixture has a linked overflow page");
    patch_page(&fixture, overflow, 48, &[0; 8]).expect("restamp overflow link");
    assert_controlled_corruption(|| validate_fixture_tree(&fixture, TreeKind::Index));

    let fixture = corruption_fixture(4096).expect("create freelist fixture");
    let freelist = fixture.page(PageType::Freelist);
    patch_page(&fixture, freelist, 68, &1u32.to_le_bytes())
        .expect("restamp freelist reserved field");
    assert_controlled_corruption(|| {
        let pager = Pager::open(
            fixture.temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )?;
        pager.begin_write().map(|_| ())
    });
}

impl CorruptionFixture {
    fn page(&self, page_type: PageType) -> PageRef {
        self.pages
            .iter()
            .find_map(|(reference, seen_type)| (*seen_type == page_type).then_some(*reference))
            .unwrap_or_else(|| panic!("fixture is missing {page_type:?}"))
    }
}

fn corruption_fixture(page_size: u32) -> Result<CorruptionFixture> {
    let temp = TempDatabase::new(&format!("corruption-{page_size}"));
    let pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    let (mut table, index) = {
        let mut batch = pager.begin_write()?;
        let mut table = BTree::create(&mut batch, TreeKind::Table)?;
        let mut index = BTree::create(&mut batch, TreeKind::Index)?;
        for number in 0..24u8 {
            table.insert(&mut batch, &fixture_key(0x11, number), b"table")?;
            index.insert(&mut batch, &fixture_key(0x22, number), b"index")?;
        }
        table.insert(
            &mut batch,
            b"released-overflow",
            &vec![0x31; usize::try_from(page_size).expect("page size fits usize") + 111],
        )?;
        index.insert(
            &mut batch,
            b"live-overflow",
            &vec![0x32; usize::try_from(page_size).expect("page size fits usize") + 111],
        )?;
        batch.allocate_append(PageType::Catalog)?;
        batch.track_tree(TreeKind::Table, table.root())?;
        batch.track_tree(TreeKind::Index, index.root())?;
        batch.set_catalog_root(table.root())?;
        batch.commit()?;
        (table, index)
    };
    {
        let mut batch = pager.begin_write()?;
        assert!(table.delete(&mut batch, b"released-overflow")?.is_some());
        batch.track_tree(TreeKind::Table, table.root())?;
        batch.track_tree(TreeKind::Index, index.root())?;
        batch.set_catalog_root(table.root())?;
        batch.commit()?;
    }
    let pages = materialize_wal_fixture_to_checkpoint(&temp, pager, table.root().page_id())?;
    for page_type in [
        PageType::Catalog,
        PageType::TableInternal,
        PageType::TableLeaf,
        PageType::IndexInternal,
        PageType::IndexLeaf,
        PageType::Overflow,
        PageType::Freelist,
    ] {
        assert!(
            pages.iter().any(|(_, seen_type)| *seen_type == page_type),
            "fixture must contain {page_type:?}"
        );
    }
    Ok(CorruptionFixture {
        temp,
        page_size,
        table,
        index,
        pages,
    })
}

fn materialize_wal_fixture_to_checkpoint(
    temp: &TempDatabase,
    pager: Pager,
    catalog_root: PageId,
) -> Result<Vec<(PageRef, PageType)>> {
    let latest_lsn = pager.latest_lsn()?;
    let mut snapshot = pager.begin_read()?;
    let page_count = snapshot.page_count()?;
    let freelist_root = snapshot.freelist_root()?;
    let mut images = Vec::new();
    let mut pages = Vec::new();
    for page_id in 1..page_count {
        let page_id = PageId(page_id);
        let frame = snapshot.read_page_by_id(page_id)?;
        let reference = PageRef::new(frame.header().page_id(), frame.header().generation())?;
        images.push((page_id, frame.bytes().to_vec()));
        pages.push((reference, frame.header().page_type()));
    }
    drop(snapshot);
    drop(pager);

    let mut main =
        DatabaseFile::open(temp.path(), OpenMode::ReadWrite, DEFAULT_MAX_DATABASE_BYTES)?;
    main.extend_to_pages(page_count)?;
    for (page_id, bytes) in images {
        main.write_page(page_id, &bytes)?;
    }
    main.sync_all()?;
    let base = main.header();
    let checkpoint = base.with_checkpoint(
        latest_lsn,
        page_count,
        catalog_root,
        freelist_root,
        base.wal_generation(),
    )?;
    main.publish_superblock(checkpoint)?;
    let fallback_checkpoint = main.header().with_checkpoint(
        latest_lsn,
        page_count,
        catalog_root,
        freelist_root,
        main.header().wal_generation(),
    )?;
    main.publish_superblock(fallback_checkpoint)?;
    drop(main);

    fs::remove_file(wal_path(temp.path())).map_err(|error| {
        DbError::new(
            ErrorCode::Io,
            format!("remove fixture WAL before rebasing: {error}"),
        )
    })?;
    let wal = Wal::create(temp.path(), &fallback_checkpoint)?;
    drop(wal);
    Ok(pages)
}

fn fixture_key(prefix: u8, number: u8) -> Vec<u8> {
    let mut key = vec![prefix; 1024];
    key[..2].copy_from_slice(&[prefix, number]);
    key
}

fn fixture_pages(path: &Path, page_size: u32) -> Result<Vec<(PageRef, PageType)>> {
    let bytes = fs::read(path).map_err(|error| {
        DbError::new(ErrorCode::Io, format!("read corruption fixture: {error}"))
    })?;
    let page_len = usize::try_from(page_size).expect("page size fits usize");
    let ordinary_bytes = bytes
        .len()
        .checked_sub(usize::try_from(SUPERBLOCK_AREA_LEN).expect("prefix length fits usize"))
        .ok_or_else(|| DbError::new(ErrorCode::Corruption, "fixture omits superblocks"))?;
    let mut pages = Vec::new();
    for page_id in 1..=ordinary_bytes / page_len {
        let page_id = PageId(u64::try_from(page_id).expect("page id fits u64"));
        let offset = fixture_page_offset(page_id, page_size)?;
        let frame = PageFrame::decode(
            bytes[offset..offset + page_len].to_vec(),
            page_id,
            None,
            None,
        )?;
        let reference = PageRef::new(frame.header().page_id(), frame.header().generation())?;
        pages.push((reference, frame.header().page_type()));
    }
    Ok(pages)
}

fn page_bytes(fixture: &CorruptionFixture, reference: PageRef) -> Vec<u8> {
    let bytes = fs::read(fixture.temp.path()).expect("read fixture bytes");
    let page_len = usize::try_from(fixture.page_size).expect("page size fits usize");
    let offset =
        fixture_page_offset(reference.page_id(), fixture.page_size).expect("fixture page offset");
    bytes[offset..offset + page_len].to_vec()
}

fn patch_page(
    fixture: &CorruptionFixture,
    reference: PageRef,
    offset: usize,
    replacement: &[u8],
) -> Result<()> {
    let mut bytes = fs::read(fixture.temp.path())
        .map_err(|error| DbError::new(ErrorCode::Io, format!("read fixture for patch: {error}")))?;
    let page_len = usize::try_from(fixture.page_size).expect("page size fits usize");
    let page_offset = fixture_page_offset(reference.page_id(), fixture.page_size)?;
    let page = &mut bytes[page_offset..page_offset + page_len];
    page[offset..offset + replacement.len()].copy_from_slice(replacement);
    stamp_page_checksum(page)?;
    fs::write(fixture.temp.path(), bytes)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("write patched fixture: {error}")))
}

fn flip_page_byte(path: &Path, page_size: u32, reference: PageRef, offset: usize) -> Result<()> {
    let mut bytes = fs::read(path).map_err(|error| {
        DbError::new(
            ErrorCode::Io,
            format!("read fixture for checksum flip: {error}"),
        )
    })?;
    let page_offset = fixture_page_offset(reference.page_id(), page_size)?;
    bytes[page_offset + offset] ^= 1;
    fs::write(path, bytes).map_err(|error| {
        DbError::new(
            ErrorCode::Io,
            format!("write checksum-flipped fixture: {error}"),
        )
    })
}

fn flip_header_byte(path: &Path, slot: SuperblockSlot, offset: usize) -> Result<()> {
    if offset >= SUPERBLOCK_SLOT_LEN {
        return Err(DbError::new(
            ErrorCode::Corruption,
            "superblock corruption offset is outside the selected slot",
        ));
    }
    let mut bytes = fs::read(path)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("read header fixture: {error}")))?;
    let slot_start = match slot {
        SuperblockSlot::A => 0,
        SuperblockSlot::B => SUPERBLOCK_SLOT_LEN,
    };
    bytes[slot_start + offset] ^= 1;
    fs::write(path, bytes)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("write header fixture: {error}")))
}

fn fixture_page_offset(page_id: PageId, page_size: u32) -> Result<usize> {
    usize::try_from(checked_page_offset(page_id, page_size, u64::MAX)?)
        .map_err(|_| DbError::new(ErrorCode::Corruption, "fixture page offset exceeds usize"))
}

fn truncate_file(path: &Path, len: u64) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("open truncation fixture: {error}")))?
        .set_len(len)
        .map_err(|error| DbError::new(ErrorCode::Io, format!("truncate fixture: {error}")))
}

fn read_page_from_fixture(fixture: &CorruptionFixture, reference: PageRef) -> Result<()> {
    let pager = Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(fixture.page_size),
    )?;
    pager.read(reference).map(|_| ())
}

fn validate_fixture_tree(fixture: &CorruptionFixture, kind: TreeKind) -> Result<()> {
    let pager = Pager::open(
        fixture.temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(fixture.page_size),
    )?;
    let tree = match kind {
        TreeKind::Table => &fixture.table,
        TreeKind::Index => &fixture.index,
    };
    let mut snapshot = pager.begin_read()?;
    validate_tree(&mut snapshot, tree).map(|_| ())
}

fn assert_controlled_corruption(action: impl FnOnce() -> Result<()>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
        .expect("corruption path must not panic");
    assert_eq!(
        result
            .expect_err("corruption path must return an error")
            .code(),
        ErrorCode::Corruption
    );
}

// Phase 4A expectations are deliberately separate from the production codecs.
#[derive(Debug, Clone, PartialEq)]
struct CatalogModel {
    initialized: bool,
    next_object_id: u64,
    tables: BTreeMap<u64, ModelTable>,
}

#[derive(Debug, Clone, PartialEq)]
struct ModelTable {
    name: String,
    next_rowid: u64,
    columns: Vec<ModelColumn>,
    rows: BTreeMap<u64, (Vec<Value>, Vec<u8>)>,
}

#[derive(Debug, Clone, PartialEq)]
struct ModelColumn {
    id: u64,
    name: String,
    kind: ColumnType,
    nullable: bool,
}

impl CatalogModel {
    fn empty() -> Self {
        Self {
            initialized: false,
            next_object_id: 1,
            tables: BTreeMap::new(),
        }
    }

    fn add_table(&mut self, name: String, kinds: &[ColumnType]) -> u64 {
        self.initialized = true;
        let id = self.next_object_id;
        self.next_object_id += 1;
        let columns = kinds
            .iter()
            .enumerate()
            .map(|(ordinal, &kind)| {
                let column = ModelColumn {
                    id: self.next_object_id,
                    name: format!("C{ordinal}"),
                    kind,
                    nullable: ordinal != 0,
                };
                self.next_object_id += 1;
                column
            })
            .collect();
        assert!(
            self.tables
                .insert(
                    id,
                    ModelTable {
                        name,
                        next_rowid: 1,
                        columns,
                        rows: BTreeMap::new(),
                    }
                )
                .is_none()
        );
        id
    }

    fn add_row(&mut self, table: u64, values: Vec<Value>) -> u64 {
        let table = self.tables.get_mut(&table).unwrap();
        let id = table.next_rowid;
        table.next_rowid += 1;
        let bytes = model_row_bytes(&values);
        assert!(table.rows.insert(id, (values, bytes)).is_none());
        id
    }
}

struct Phase4aLcg(u64);

impl Phase4aLcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 16
    }
}

const PHASE_4A_KINDS: [ColumnType; 4] = [
    ColumnType::Int64,
    ColumnType::Text,
    ColumnType::Blob,
    ColumnType::Bool,
];
const PHASE_4A_MAX_ROW: usize = 16_384;

// A small, independent specification encoder for the model's four scalar types.
// It uses literal V1 framing, not RowCodec or the storage Encoder.
fn model_row_bytes(values: &[Value]) -> Vec<u8> {
    let mut bytes = b"OROW\x01\x00\x00\x00".to_vec();
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&(values.len() as u16).to_le_bytes());
    let bitmap_len = values.len().div_ceil(8);
    bytes.extend_from_slice(&(bitmap_len as u16).to_le_bytes());
    bytes.extend_from_slice(&(values.len() as u32 * 8).to_le_bytes());
    let mut bitmap = vec![0u8; bitmap_len];
    let mut directory = Vec::new();
    let mut payload = Vec::new();
    for (ordinal, value) in values.iter().enumerate() {
        let encoded = match value {
            Value::Null => {
                bitmap[ordinal / 8] |= 1 << (ordinal % 8);
                directory.extend_from_slice(&[0; 8]);
                continue;
            }
            Value::Int64(value) => value.to_le_bytes().to_vec(),
            Value::Text(value) => value.as_bytes().to_vec(),
            Value::Blob(value) => value.clone(),
            Value::Bool(value) => vec![u8::from(*value)],
            _ => panic!("scalar is outside the reference model"),
        };
        directory.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        directory.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        payload.extend(encoded);
    }
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend(bitmap);
    bytes.extend(directory);
    bytes.extend(payload);
    bytes
}

fn model_definitions(id: u64, table: &ModelTable, root: PageRef) -> (TableDef, Vec<ColumnDef>) {
    let definition = TableDef::new(
        TableId::new(id).unwrap(),
        SchemaVersion::new(1).unwrap(),
        root,
        RowId::new(1).unwrap(),
        table.columns.len() as u16,
        CatalogName::new(table.name.clone()).unwrap(),
    )
    .unwrap();
    let columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(ordinal, column)| {
            ColumnDef::new(
                ColumnId::new(column.id).unwrap(),
                definition.id(),
                ordinal as u16,
                column.kind,
                column.nullable,
                CatalogName::new(column.name.clone()).unwrap(),
            )
            .unwrap()
        })
        .collect();
    (definition, columns)
}

fn phase4a_create_table(
    writer: &mut CatalogWriter,
    batch: &mut WriteBatch,
    model: &CatalogModel,
    id: u64,
) -> Result<TableDef> {
    assert_eq!(writer.allocate_object_id(batch)?.get(), id);
    let root = BTree::create(batch, TreeKind::Table)?.root();
    let (table, columns) = model_definitions(id, &model.tables[&id], root);
    writer.insert_table_definition(batch, &table)?;
    // Allocate in model order, insert in reverse order to test ordinal sorting.
    for column in &columns {
        assert_eq!(writer.allocate_object_id(batch)?.get(), column.id().get());
    }
    for column in columns.iter().rev() {
        writer.insert_column_definition(batch, column)?;
    }
    Ok(table)
}

fn phase4a_insert_row(
    writer: &mut CatalogWriter,
    batch: &mut WriteBatch,
    table: &TableDef,
    model: &CatalogModel,
    rowid: u64,
    tree: &mut BTree,
) -> Result<()> {
    let expected = &model.tables[&table.id().get()];
    let (_, columns) = model_definitions(table.id().get(), expected, tree.root());
    let (values, expected_bytes) = &expected.rows[&rowid];
    let allocated = writer.allocate_rowid(batch, table.id())?;
    assert_eq!(allocated.get(), rowid);
    let bytes = RowCodec::encode(table, &columns, values, PHASE_4A_MAX_ROW)?;
    assert_eq!(
        &bytes, expected_bytes,
        "encoder must match independent V1 bytes"
    );
    tree.insert(batch, &encode_rowid_key(allocated), &bytes)?;
    writer.update_table_root(batch, table.id(), tree.root())
}

fn model_envelope(kind: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = vec![kind, 1, 0, 0];
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend(payload);
    bytes
}

fn model_id_key(kind: u8, id: u64) -> Vec<u8> {
    let mut bytes = vec![kind];
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes
}

fn model_name_key(kind: u8, owner: Option<u64>, name: &str) -> Vec<u8> {
    let mut bytes = vec![kind];
    if let Some(id) = owner {
        bytes.extend_from_slice(&id.to_be_bytes());
    }
    bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn assert_phase4a_model(snapshot: &mut ReadSnapshot, model: &CatalogModel) -> Result<()> {
    assert_eq!(
        snapshot.feature_bits(),
        if model.initialized {
            super::features::CATALOG_V1
        } else {
            0
        },
        "catalog initialization feature must match the independent model"
    );
    assert_eq!(
        snapshot.catalog_root().0 != 0,
        model.initialized,
        "catalog initialization root must match the independent model"
    );
    let reader = CatalogReader::open_empty_or_v1(snapshot)?;
    reader.validate_deep(snapshot)?;
    let tree = snapshot.catalog_tree(TreeKind::Table)?;
    let mut expected_records = BTreeMap::new();
    assert_eq!(tree.is_some(), model.initialized);
    if model.initialized {
        let mut meta = vec![1, 0, 0, 0];
        meta.extend_from_slice(&model.next_object_id.to_le_bytes());
        expected_records.insert(vec![0], model_envelope(0, meta));
    }
    for (&id, table) in &model.tables {
        let actual = reader
            .find_table_by_id(TableId::new(id)?)
            .expect("model table exists");
        assert_eq!(
            reader.find_table_by_name(&CatalogName::new(table.name.clone())?),
            Some(actual)
        );
        assert_eq!(actual.name().as_str(), table.name);
        assert_eq!(actual.schema_version().get(), 1);
        assert_eq!(actual.next_rowid().get(), table.next_rowid);
        assert_eq!(usize::from(actual.column_count()), table.columns.len());
        let (_, columns) = model_definitions(id, table, actual.root());
        assert_eq!(
            reader
                .columns_for_table(actual.id())
                .cloned()
                .collect::<Vec<_>>(),
            columns
        );
        assert!(
            reader
                .find_column_by_name(actual.id(), &CatalogName::new("MISSING")?)
                .is_none()
        );

        expected_records.insert(
            model_name_key(0x10, None, &table.name),
            model_envelope(0x10, id.to_le_bytes().to_vec()),
        );
        let mut definition = Vec::new();
        for value in [
            id,
            1,
            actual.root().page_id().0,
            actual.root().generation(),
            table.next_rowid,
        ] {
            definition.extend_from_slice(&value.to_le_bytes());
        }
        definition.extend_from_slice(&(columns.len() as u16).to_le_bytes());
        definition.extend_from_slice(&(table.name.len() as u16).to_le_bytes());
        definition.extend_from_slice(table.name.as_bytes());
        expected_records.insert(model_id_key(0x11, id), model_envelope(0x11, definition));

        for column in &columns {
            assert_eq!(
                reader.find_column_by_name(actual.id(), column.name()),
                Some(column)
            );
            let mapping = column.id().get().to_le_bytes().to_vec();
            expected_records.insert(
                model_name_key(0x12, Some(id), column.name().as_str()),
                model_envelope(0x12, mapping.clone()),
            );
            let mut ordinal_key = model_id_key(0x13, id);
            ordinal_key.extend_from_slice(&column.ordinal().to_be_bytes());
            expected_records.insert(ordinal_key, model_envelope(0x13, mapping));
            let mut definition = column.id().get().to_le_bytes().to_vec();
            definition.extend_from_slice(&id.to_le_bytes());
            definition.extend_from_slice(&column.ordinal().to_le_bytes());
            definition
                .extend_from_slice(&[column.column_type() as u8, u8::from(column.nullable())]);
            definition.extend_from_slice(&(column.name().as_bytes().len() as u16).to_le_bytes());
            definition.extend_from_slice(&[0, 0]);
            definition.extend_from_slice(column.name().as_bytes());
            expected_records.insert(
                model_id_key(0x14, column.id().get()),
                model_envelope(0x14, definition),
            );
        }
        let rows = BTree::from_root(TreeKind::Table, actual.root());
        let scanned = collect_all(&rows, snapshot)?;
        assert_eq!(
            scanned.len(),
            table.rows.len(),
            "all rows, including rollback absence"
        );
        for ((key, bytes), (&rowid, (values, expected_bytes))) in scanned.iter().zip(&table.rows) {
            assert_eq!(key, &rowid.to_be_bytes());
            assert_eq!(decode_rowid_key(key)?.get(), rowid);
            assert_eq!(bytes, expected_bytes);
            assert_eq!(
                &RowCodec::decode(actual, &columns, bytes, PHASE_4A_MAX_ROW)?,
                values
            );
            assert_eq!(rows.get(snapshot, key)?.as_ref(), Some(bytes));
        }
        assert!(
            rows.get(snapshot, &table.next_rowid.to_be_bytes())?
                .is_none()
        );
    }
    assert!(
        reader
            .find_table_by_name(&CatalogName::new("MISSING")?)
            .is_none()
    );
    assert!(
        reader
            .find_table_by_id(TableId::new(model.next_object_id)?)
            .is_none()
    );
    let actual_records: BTreeMap<_, _> = match tree {
        Some(tree) => collect_all(&tree, snapshot)?.into_iter().collect(),
        None => BTreeMap::new(),
    };
    assert_eq!(
        actual_records, expected_records,
        "every mapping, definition, and global counter"
    );
    Ok(())
}

#[test]
fn phase_4a_reference_model() {
    // A missing row insert, counter update, mapping, or rollback must fail this model.
    for seed in [0x04a0_cafe_0123_4567, 0x04a0_dead_7654_3210] {
        let temp = TempDatabase::new("phase4a-reference");
        let mut pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
        let mut model = CatalogModel::empty();
        let mut random = Phase4aLcg(seed);
        let (mut operations, mut commits, mut rollbacks, mut reopens) = (0, 0, 0, 0);
        for window in 0..8 {
            let mut old = pager.begin_read().unwrap();
            assert_phase4a_model(&mut old, &model).unwrap();
            let mut candidate = model.clone();
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            let id = candidate.add_table(format!("T{window}_表"), &PHASE_4A_KINDS);
            let table = phase4a_create_table(&mut writer, &mut batch, &candidate, id).unwrap();
            operations += 5;
            let reader = CatalogReader::open_empty_or_v1(&mut old).unwrap();
            let mut roots = BTreeMap::new();
            roots.insert(id, BTree::from_root(TreeKind::Table, table.root()));
            for sequence in 0..58 {
                let ids: Vec<_> = candidate.tables.keys().copied().collect();
                let target = ids[random.next() as usize % ids.len()];
                let definition = if target == id {
                    table.clone()
                } else {
                    reader
                        .find_table_by_id(TableId::new(target).unwrap())
                        .unwrap()
                        .clone()
                };
                let tree = roots
                    .entry(target)
                    .or_insert_with(|| BTree::from_root(TreeKind::Table, definition.root()));
                let number = random.next();
                let values = vec![
                    Value::Int64(number as i64),
                    if number % 5 == 0 {
                        Value::Null
                    } else {
                        Value::Text(format!("行-{number}"))
                    },
                    Value::Blob(vec![
                        number as u8;
                        if window == 0 && sequence == 0 {
                            5000
                        } else {
                            7
                        }
                    ]),
                    Value::Bool(number & 1 != 0),
                ];
                let rowid = candidate.add_row(target, values);
                phase4a_insert_row(
                    &mut writer,
                    &mut batch,
                    &definition,
                    &candidate,
                    rowid,
                    tree,
                )
                .unwrap();
                operations += 1;
            }
            if window % 4 == 3 {
                // Exercise rollback both before and after the catalog publication gate.
                if window == 3 {
                    writer.finish(&mut batch).unwrap();
                }
                batch.rollback().unwrap();
                drop(batch);
                rollbacks += 1;
            } else {
                writer.finish(&mut batch).unwrap();
                batch.commit().unwrap();
                commits += 1;
            }
            assert_phase4a_model(&mut old, &model).unwrap();
            drop(old);
            if window % 4 != 3 {
                model = candidate;
            }
            assert_phase4a_model(&mut pager.begin_read().unwrap(), &model).unwrap();
            if window % 2 == 1 {
                if window % 4 == 1 {
                    pager.checkpoint().unwrap();
                }
                drop(pager);
                pager = crash_open(temp.path());
                assert_phase4a_model(&mut pager.begin_read().unwrap(), &model).unwrap();
                reopens += 1;
            }
        }
        assert_eq!((operations, commits, rollbacks, reopens), (504, 6, 2, 4));
        eprintln!(
            "phase4a seed={seed:#x}: {operations} mutations, {commits} commits, {rollbacks} rollbacks, {reopens} reopens, 8 retained snapshots"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CatalogMutationScalars {
    next_object_id: u64,
    schema_version: u64,
    next_rowid: u64,
}

fn phase4a_catalog_flip_expectation(
    family: u8,
    offset: usize,
    next_object_id: u64,
) -> std::result::Result<CatalogMutationScalars, ErrorCode> {
    let mut expected = CatalogMutationScalars {
        next_object_id,
        schema_version: 1,
        next_rowid: 1,
    };
    // These are the only valid single-byte flips in this independently specified
    // one-table fixture. All other fields violate framing, mappings, or root identity.
    match (family, offset) {
        (_, 0 | 1) => return Err(ErrorCode::Unsupported),
        (0, 12..=19) => expected.next_object_id ^= 0x80u64 << ((offset - 12) * 8),
        (0x11, 16..=23) => expected.schema_version ^= 0x80u64 << ((offset - 16) * 8),
        (0x11, 40..=47) => expected.next_rowid ^= 0x80u64 << ((offset - 40) * 8),
        _ => return Err(ErrorCode::Corruption),
    }
    Ok(expected)
}

#[test]
fn phase_4a_catalog_reserved_field_expectations() {
    for offset in [27, 30, 31, 24, 25, 26, 28, 29] {
        assert_eq!(
            phase4a_catalog_flip_expectation(0x14, offset, 6),
            Err(ErrorCode::Corruption),
            "ColumnDef offset {offset} is a known malformed field, not an optional error"
        );
    }
}

fn assert_phase4a_catalog_mutation(
    reader: &CatalogReader,
    batch: &mut WriteBatch,
    tree: &BTree,
    original: &TableDef,
    model: &CatalogModel,
    records: &[(Vec<u8>, Vec<u8>)],
    scalars: CatalogMutationScalars,
) {
    let expected = TableDef::new(
        original.id(),
        SchemaVersion::new(scalars.schema_version).unwrap(),
        original.root(),
        RowId::new(scalars.next_rowid).unwrap(),
        original.column_count(),
        original.name().clone(),
    )
    .unwrap();
    assert_eq!(reader.find_table_by_id(expected.id()), Some(&expected));
    assert_eq!(reader.find_table_by_name(expected.name()), Some(&expected));
    let (_, columns) = model_definitions(
        original.id().get(),
        &model.tables[&original.id().get()],
        original.root(),
    );
    assert_eq!(
        reader
            .columns_for_table(expected.id())
            .cloned()
            .collect::<Vec<_>>(),
        columns
    );
    for column in &columns {
        assert_eq!(
            reader.find_column_by_name(expected.id(), column.name()),
            Some(column)
        );
    }
    // The unmodified records were already checked against the independent model.
    // Rebuild both possibly changed payloads from predicted scalars, never from
    // decoded fields or by copying the mutated payload as its own expectation.
    let mut expected_records: BTreeMap<_, _> = records.iter().cloned().collect();
    let mut meta = vec![1, 0, 0, 0];
    meta.extend_from_slice(&scalars.next_object_id.to_le_bytes());
    expected_records.insert(vec![0], model_envelope(0, meta));
    let mut definition = Vec::new();
    for value in [
        original.id().get(),
        scalars.schema_version,
        original.root().page_id().0,
        original.root().generation(),
        scalars.next_rowid,
    ] {
        definition.extend_from_slice(&value.to_le_bytes());
    }
    definition.extend_from_slice(&original.column_count().to_le_bytes());
    definition.extend_from_slice(&(original.name().as_bytes().len() as u16).to_le_bytes());
    definition.extend_from_slice(original.name().as_bytes());
    expected_records.insert(
        model_id_key(0x11, original.id().get()),
        model_envelope(0x11, definition),
    );
    assert_eq!(
        collect_all(tree, batch)
            .unwrap()
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
        expected_records
    );
    // CatalogReader's meta is private. Exercise the real decoded counter via
    // allocation as well as asserting the independently reconstructed meta bytes.
    let mut writer = CatalogWriter::bootstrap(batch).unwrap();
    assert_eq!(
        writer.allocate_object_id(batch).unwrap().get(),
        scalars.next_object_id
    );
}

#[test]
fn phase_4a_bootstrap_oracle_rejects_allocation_without_catalog() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let (_temp, pager) = test_pager(4096).unwrap();
    let before = phase4a_state(&pager);
    let mut batch = pager.begin_write().unwrap();
    BTree::create(&mut batch, TreeKind::Table).unwrap();
    batch.commit().unwrap();
    let unpublished = phase4a_state(&pager);
    assert_eq!(unpublished.lsn.0, before.lsn.0 + 1);
    assert!(unpublished.page_count > before.page_count);
    assert_eq!(unpublished.features, 0);
    assert_eq!(unpublished.root, None);
    let rejected = catch_unwind(AssertUnwindSafe(|| {
        assert_phase4a_model(
            &mut pager.begin_read().unwrap(),
            &phase4a_scenario_model("bootstrap", true),
        )
        .unwrap();
    }));
    assert!(
        rejected.is_err(),
        "bootstrap oracle accepted allocation/LSN progress without catalog publication"
    );
}

#[test]
fn phase_4a_bootstrap_model_requires_initialized_meta() {
    let (_temp, pager) = test_pager(4096).unwrap();
    let pre = phase4a_scenario_model("bootstrap", false);
    let post = phase4a_scenario_model("bootstrap", true);
    let mut retained = pager.begin_read().unwrap();
    phase4a_apply_model(&pager, &pre, &post).unwrap();
    assert_phase4a_model(&mut retained, &pre).unwrap();
    assert_phase4a_model(&mut pager.begin_read().unwrap(), &post).unwrap();
}

#[test]
fn catalog_record_corruption_matrix_never_panics() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let (_temp, pager) = test_pager(4096).unwrap();
    let mut model = CatalogModel::empty();
    let id = model.add_table("T表".to_owned(), &PHASE_4A_KINDS);
    let mut batch = pager.begin_write().unwrap();
    let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
    let original = phase4a_create_table(&mut writer, &mut batch, &model, id).unwrap();
    writer.finish(&mut batch).unwrap();
    batch.commit().unwrap();
    let mut snapshot = pager.begin_read().unwrap();
    assert_phase4a_model(&mut snapshot, &model).unwrap();
    let tree = snapshot.catalog_tree(TreeKind::Table).unwrap().unwrap();
    let records = collect_all(&tree, &mut snapshot).unwrap();
    drop(snapshot);
    let mut cases = 0;
    let mut valid_cases = 0;
    let mut families = std::collections::BTreeSet::new();
    for (key, value) in &records {
        families.insert(key[0]);
        let mut variants = Vec::new();
        for end in 0..value.len() {
            variants.push((
                key.clone(),
                value[..end].to_vec(),
                Err(ErrorCode::Corruption),
            ));
        }
        for offset in 0..value.len() {
            let mut changed = value.clone();
            changed[offset] ^= 0x80;
            let expected = phase4a_catalog_flip_expectation(key[0], offset, model.next_object_id);
            variants.push((key.clone(), changed, expected));
        }
        let mut version_zero = value.clone();
        version_zero[1] = 0;
        variants.push((key.clone(), version_zero, Err(ErrorCode::Corruption)));
        // Include keys, not just values: malformed names/IDs must never escape the reader.
        for end in 0..key.len() {
            variants.push((
                key[..end].to_vec(),
                value.clone(),
                Err(ErrorCode::Corruption),
            ));
        }
        for offset in 0..key.len() {
            let mut changed = key.clone();
            changed[offset] ^= 0x80;
            variants.push((
                changed,
                value.clone(),
                Err(if offset == 0 {
                    ErrorCode::Unsupported
                } else {
                    ErrorCode::Corruption
                }),
            ));
        }
        for (changed_key, changed_value, expected) in variants {
            let mut batch = pager.begin_write().unwrap();
            let mut candidate = BTree::from_root(TreeKind::Table, tree.root());
            let result = catch_unwind(AssertUnwindSafe(|| -> Result<CatalogReader> {
                assert_eq!(candidate.delete(&mut batch, key)?, Some(value.clone()));
                candidate.insert(&mut batch, &changed_key, &changed_value)?;
                CatalogReader::open_candidate(&mut batch, &candidate, super::features::CATALOG_V1)
            }))
            .expect("catalog mutation/open must never panic");
            match (result, expected) {
                (Err(error), Err(code)) => assert_eq!(
                    error.code(),
                    code,
                    "key={key:?} mutation={changed_key:?}/{changed_value:?}"
                ),
                (Err(error), Ok(wanted)) => {
                    panic!("valid catalog mutation {wanted:?} rejected: {error}")
                }
                (Ok(reader), Ok(wanted)) => {
                    assert_phase4a_catalog_mutation(
                        &reader, &mut batch, &candidate, &original, &model, &records, wanted,
                    );
                    valid_cases += 1;
                }
                (Ok(_), Err(code)) => panic!(
                    "invalid catalog record accepted, wanted {code:?}; key={changed_key:?} bytes={changed_value:?}"
                ),
            }
            batch.rollback().unwrap();
            cases += 1;
        }
    }
    assert_eq!(
        families.into_iter().collect::<Vec<_>>(),
        [0, 0x10, 0x11, 0x12, 0x13, 0x14]
    );
    assert_phase4a_model(&mut pager.begin_read().unwrap(), &model).unwrap();
    assert!(cases > 500);
    assert_eq!(
        valid_cases, 24,
        "exactly three independently predicted eight-byte scalar ranges"
    );
    eprintln!(
        "catalog corruption: {cases} cases across six record families; {valid_cases} independently checked valid mutations"
    );
}

#[test]
fn row_corruption_matrix_never_panics() {
    use std::panic::catch_unwind;
    let kinds = [
        ColumnType::Int64,
        ColumnType::Decimal,
        ColumnType::Float64,
        ColumnType::Text,
        ColumnType::Blob,
        ColumnType::Date,
        ColumnType::Timestamp,
        ColumnType::TimestampWithTimeZone,
        ColumnType::Bool,
        ColumnType::Text,
    ];
    let mut model = CatalogModel::empty();
    let id = model.add_table("MIXED".to_owned(), &kinds);
    let (table, columns) =
        model_definitions(id, &model.tables[&id], PageRef::new(PageId(2), 1).unwrap());
    let date = crate::OracleDate::new(2024, 2, 29, 23, 59, 58).unwrap();
    let values = vec![
        Value::Int64(-123),
        Value::Decimal(crate::Decimal::new(true, 2, 12345).unwrap()),
        Value::Float64(1.25),
        Value::Text("中文".to_owned()),
        Value::Blob(vec![0, 1, 255]),
        Value::Date(date),
        Value::Timestamp(crate::OracleTimestamp::new(date, 123456, None).unwrap()),
        Value::Timestamp(crate::OracleTimestamp::new(date, 654321, Some(-480)).unwrap()),
        Value::Bool(true),
        Value::Null,
    ];
    let mut cases = 0;
    for values in [values.clone(), {
        let mut sparse = vec![Value::Null; values.len()];
        sparse[0] = Value::Int64(i64::MIN);
        sparse[3] = Value::Text(String::new());
        sparse[4] = Value::Blob(Vec::new());
        sparse
    }] {
        let bytes = RowCodec::encode(&table, &columns, &values, PHASE_4A_MAX_ROW).unwrap();
        assert_eq!(
            RowCodec::decode(&table, &columns, &bytes, PHASE_4A_MAX_ROW).unwrap(),
            values
        );
        let mut variants = Vec::new();
        for end in 0..bytes.len() {
            variants.push((bytes[..end].to_vec(), Some(ErrorCode::Corruption)));
        }
        // Every byte includes all headers, bitmaps, directory offsets/lengths,
        // scalar reserved fields, and variable payloads.
        for offset in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[offset] ^= 0x80;
            variants.push((
                changed,
                if [4, 5].contains(&offset) {
                    Some(ErrorCode::Unsupported)
                } else if offset < 32 {
                    Some(ErrorCode::Corruption)
                } else {
                    None
                },
            ));
        }
        let mut zero_version = bytes.clone();
        zero_version[4..6].fill(0);
        variants.push((zero_version, Some(ErrorCode::Corruption)));
        for (changed, expected) in variants {
            let decoded =
                catch_unwind(|| RowCodec::decode(&table, &columns, &changed, PHASE_4A_MAX_ROW))
                    .expect("row decoding must never panic");
            match (decoded, expected) {
                (Err(error), Some(code)) => assert_eq!(error.code(), code, "{changed:?}"),
                (Err(error), None) => assert_eq!(error.code(), ErrorCode::Corruption),
                (Ok(decoded), None) => {
                    assert_eq!(decoded.len(), columns.len());
                    assert_eq!(
                        RowCodec::encode(&table, &columns, &decoded, PHASE_4A_MAX_ROW).unwrap(),
                        changed
                    );
                }
                (Ok(_), Some(code)) => panic!("invalid row accepted, wanted {code:?}"),
            }
            cases += 1;
        }
    }
    assert!(cases > 500);
    eprintln!(
        "row corruption: {cases} truncation/byte-flip cases, all nine types and NULL/empty payloads"
    );
}

const PHASE_4A_CRASH_CHILD: &str = "storage::test_support::phase_4a_catalog_crash_child";
const PHASE_4A_SCENARIO_ENV: &str = "ORA_SQLLITE_PHASE4A_SCENARIO";
const PHASE_4A_FAULT_ENV: &str = "ORA_SQLLITE_PHASE4A_FAULT";
const PHASE_4A_OCCURRENCE_ENV: &str = "ORA_SQLLITE_PHASE4A_OCCURRENCE";
const PHASE_4A_PATH_ENV: &str = "ORA_SQLLITE_PHASE4A_PATH";
const PHASE_4A_TRACE_ENV: &str = "ORA_SQLLITE_PHASE4A_TRACE";
const PHASE_4A_SCENARIOS: [&str; 4] = [
    "bootstrap",
    "catalog-root-split",
    "bulk-metadata",
    "row-insert",
];
const PHASE_4A_WAL_POINTS: [FaultPoint; 8] = [
    FaultPoint::BeforeWalRecordWrite,
    FaultPoint::AfterWalRecordHeaderPrefixWrite,
    FaultPoint::AfterWalRecordPayloadPrefixWrite,
    FaultPoint::AfterWalRecordWrite,
    FaultPoint::BeforeWalSync,
    FaultPoint::AfterWalSync,
    FaultPoint::BeforeWalPublish,
    FaultPoint::AfterWalPublish,
];

fn phase4a_scenario_model(scenario: &str, post: bool) -> CatalogModel {
    let mut model = CatalogModel::empty();
    model.initialized = post || scenario != "bootstrap";
    match scenario {
        "bootstrap" => {}
        "catalog-root-split" => {
            for n in 0..if post { 3 } else { 1 } {
                model.add_table(format!("S{n}_{}", "x".repeat(900)), &[]);
            }
        }
        "bulk-metadata" => {
            if post {
                for n in 0..8 {
                    model.add_table(format!("B{n}_{}", "y".repeat(80)), &PHASE_4A_KINDS);
                }
            }
        }
        "row-insert" => {
            let id = model.add_table("ROWS".to_owned(), &PHASE_4A_KINDS);
            for n in 0..if post { 32 } else { 8 } {
                model.add_row(
                    id,
                    vec![
                        Value::Int64(n),
                        Value::Text(format!("row-{n}")),
                        Value::Blob(vec![n as u8; if n == 31 { 5000 } else { 128 }]),
                        Value::Bool(n % 2 == 0),
                    ],
                );
            }
        }
        _ => panic!("invalid crash configuration: scenario"),
    }
    model
}

fn phase4a_apply_model(pager: &Pager, before: &CatalogModel, after: &CatalogModel) -> Result<()> {
    let mut snapshot = pager.begin_read()?;
    assert_phase4a_model(&mut snapshot, before)?;
    let reader = CatalogReader::open_empty_or_v1(&mut snapshot)?;
    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;
    for (&id, table) in &after.tables {
        let definition = if before.tables.contains_key(&id) {
            reader.find_table_by_id(TableId::new(id)?).unwrap().clone()
        } else {
            phase4a_create_table(&mut writer, &mut batch, after, id)?
        };
        let mut rows = BTree::from_root(TreeKind::Table, definition.root());
        let first = before.tables.get(&id).map_or(1, |table| table.next_rowid);
        for rowid in first..table.next_rowid {
            phase4a_insert_row(
                &mut writer,
                &mut batch,
                &definition,
                after,
                rowid,
                &mut rows,
            )?;
        }
    }
    writer.finish(&mut batch)?;
    batch.commit()
}

fn phase4a_fixture(scenario: &str) -> TempDatabase {
    let temp = TempDatabase::new("phase4a-crash-fixture");
    let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
    let model = phase4a_scenario_model(scenario, false);
    if scenario != "bootstrap" {
        phase4a_apply_model(&pager, &CatalogModel::empty(), &model).unwrap();
        pager.checkpoint().unwrap();
    }
    assert_phase4a_model(&mut pager.begin_read().unwrap(), &model).unwrap();
    drop(pager);
    temp
}

fn phase4a_copy_fixture(fixture: &TempDatabase) -> TempDatabase {
    let temp = TempDatabase::new("phase4a-crash-case");
    assert!(!temp.path().exists() && !wal_path(temp.path()).exists());
    fs::copy(fixture.path(), temp.path()).unwrap();
    fs::copy(wal_path(fixture.path()), wal_path(temp.path())).unwrap();
    assert_eq!(
        crash_pair_bytes(temp.path()),
        crash_pair_bytes(fixture.path())
    );
    temp
}

#[derive(Debug, PartialEq, Eq)]
struct Phase4aState {
    lsn: Lsn,
    features: u64,
    root: Option<PageRef>,
    page_count: u64,
    freelist: PageId,
    records: BTreeMap<Vec<u8>, Vec<u8>>,
    rows: BTreeMap<u64, BTreeMap<Vec<u8>, Vec<u8>>>,
}

fn phase4a_state(pager: &Pager) -> Phase4aState {
    phase4a_snapshot_state(&mut pager.begin_read().unwrap())
}

fn phase4a_snapshot_state(snapshot: &mut ReadSnapshot) -> Phase4aState {
    let reader = CatalogReader::open_empty_or_v1(snapshot).unwrap();
    reader.validate_deep(snapshot).unwrap();
    let tree = snapshot.catalog_tree(TreeKind::Table).unwrap();
    let records: BTreeMap<_, _> = tree.as_ref().map_or_else(BTreeMap::new, |tree| {
        collect_all(tree, snapshot).unwrap().into_iter().collect()
    });
    let mut rows = BTreeMap::new();
    for key in records.keys().filter(|key| key[0] == 0x11) {
        let id = u64::from_be_bytes(key[1..9].try_into().unwrap());
        let table = reader.find_table_by_id(TableId::new(id).unwrap()).unwrap();
        let columns: Vec<_> = reader.columns_for_table(table.id()).cloned().collect();
        let tree = BTree::from_root(TreeKind::Table, table.root());
        let entries = collect_all(&tree, snapshot).unwrap();
        for (key, value) in &entries {
            decode_rowid_key(key).unwrap();
            RowCodec::decode(table, &columns, value, PHASE_4A_MAX_ROW).unwrap();
        }
        rows.insert(id, entries.into_iter().collect());
    }
    Phase4aState {
        lsn: snapshot.lsn(),
        features: snapshot.feature_bits(),
        root: tree.map(|tree| tree.root()),
        page_count: snapshot.page_count().unwrap(),
        freelist: snapshot.freelist_root().unwrap(),
        records,
        rows,
    }
}

fn phase4a_trace(
    fixture: &TempDatabase,
    scenario: &str,
) -> (Vec<FaultPoint>, Phase4aState, Phase4aState) {
    let temp = phase4a_copy_fixture(fixture);
    let faults = Arc::new(RecordingFaults::never());
    let pager = crash_open(temp.path()).with_faults(faults.clone());
    let pre = phase4a_state(&pager);
    let wal_start = fs::metadata(wal_path(temp.path())).unwrap().len() as usize;
    phase4a_apply_model(
        &pager,
        &phase4a_scenario_model(scenario, false),
        &phase4a_scenario_model(scenario, true),
    )
    .unwrap();
    let post = phase4a_state(&pager);
    assert_phase4a_model(
        &mut pager.begin_read().unwrap(),
        &phase4a_scenario_model(scenario, true),
    )
    .unwrap();
    assert_ne!(pre, post);
    assert_eq!(post.lsn.0, pre.lsn.0 + 1, "one transaction per scenario");
    if scenario == "catalog-root-split" {
        assert_ne!(pre.root, post.root, "catalog root really split");
    }
    if scenario == "bulk-metadata" {
        assert_ne!(
            pre.root, post.root,
            "bulk definitions really split the catalog"
        );
        assert_eq!(post.records.len(), 1 + 8 * (2 + 4 * 3));
    }
    if scenario == "row-insert" {
        let old = &pre.records[&model_id_key(0x11, 1)];
        let new = &post.records[&model_id_key(0x11, 1)];
        assert_ne!(&old[24..40], &new[24..40], "user row root really split");
        assert_eq!(post.rows[&1].len(), 32);
    }
    let trace = faults.observed();
    assert_commit_trace(&trace);
    let bytes = fs::read(wal_path(temp.path())).unwrap();
    let (mut offset, mut records) = (wal_start, 0);
    while offset < bytes.len() {
        let payload =
            u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap()) as usize;
        offset += 64 + payload;
        records += 1;
    }
    assert_eq!(offset, bytes.len());
    assert_eq!(
        trace.len(),
        records * 4 + 4,
        "all physical records expose every boundary"
    );
    drop(pager);
    (trace, pre, post)
}

fn phase4a_assert_recovery(
    path: &Path,
    scenario: &str,
    pre: &Phase4aState,
    post: &Phase4aState,
    durable: bool,
) {
    let pager = crash_open(path);
    let recovered = phase4a_state(&pager);
    if durable {
        assert_eq!(
            &recovered, post,
            "durable WAL sync must preserve the whole post-state"
        );
    } else {
        assert!(
            &recovered == pre || &recovered == post,
            "partial catalog/counter/root/row state"
        );
    }
    let is_post = &recovered == post;
    let model = phase4a_scenario_model(scenario, is_post);
    assert_phase4a_model(&mut pager.begin_read().unwrap(), &model).unwrap();
    drop(pager);
    // Reopen before checkpoint too: recovery itself must be repeatable.
    let repaired = crash_pair_bytes(path);
    let pager = crash_open(path);
    assert_eq!(phase4a_state(&pager), recovered);
    drop(pager);
    assert_eq!(
        crash_pair_bytes(path),
        repaired,
        "repeat recovery changed files"
    );
    let pager = crash_open(path);
    pager.checkpoint().unwrap();
    assert_eq!(phase4a_state(&pager), recovered);
    drop(pager);
    let checkpointed = crash_pair_bytes(path);
    let pager = crash_open(path);
    assert_eq!(phase4a_state(&pager), recovered);
    assert_phase4a_model(&mut pager.begin_read().unwrap(), &model).unwrap();
    let result = pager.checkpoint().unwrap();
    assert_eq!(result.pages_written, 0);
    assert!(!result.wal_reset);
    drop(pager);
    assert_eq!(crash_pair_bytes(path), checkpointed);
    // Synchronous child wait plus exact removals prove no leaked handles/artifacts.
    fs::remove_file(wal_path(path)).unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn phase_4a_catalog_io_fault_boundaries() {
    let mut cases = 0;
    for scenario in PHASE_4A_SCENARIOS {
        let fixture = phase4a_fixture(scenario);
        let unchanged = crash_pair_bytes(fixture.path());
        let (trace, pre, post) = phase4a_trace(&fixture, scenario);
        let durable = trace
            .iter()
            .position(|&point| point == FaultPoint::AfterWalSync)
            .unwrap();
        // Fast ordinary suite covers all eight kinds at their first occurrence.
        // The dedicated subprocess matrix below covers *every* occurrence.
        for point in PHASE_4A_WAL_POINTS {
            let index = trace.iter().position(|&seen| seen == point).unwrap();
            let temp = phase4a_copy_fixture(&fixture);
            let faults = Arc::new(RecordingFaults::at(point, 1));
            let pager = crash_open(temp.path()).with_faults(faults.clone());
            let mut retained = pager.begin_read().unwrap();
            let result = phase4a_apply_model(
                &pager,
                &phase4a_scenario_model(scenario, false),
                &phase4a_scenario_model(scenario, true),
            );
            if matches!(
                point,
                FaultPoint::BeforeWalPublish | FaultPoint::AfterWalPublish
            ) {
                result.unwrap(); // Publication observers cannot report recoverable I/O errors.
                assert_eq!(faults.observed(), trace);
            } else {
                assert_eq!(result.unwrap_err().code(), ErrorCode::Io);
                assert_eq!(faults.observed(), trace[..=index]);
            }
            // A synced transaction with an injected error is durable but unpublished.
            // Publication observers cannot convert successful publication into I/O failure.
            let live_post = matches!(
                point,
                FaultPoint::BeforeWalPublish | FaultPoint::AfterWalPublish
            );
            assert_eq!(
                &phase4a_state(&pager),
                if live_post { &post } else { &pre },
                "fresh live snapshot at {scenario} {point:?}"
            );
            assert_phase4a_model(
                &mut pager.begin_read().unwrap(),
                &phase4a_scenario_model(scenario, live_post),
            )
            .unwrap();
            assert_eq!(
                phase4a_snapshot_state(&mut retained),
                pre,
                "retained snapshot at {scenario} {point:?}"
            );
            assert_phase4a_model(&mut retained, &phase4a_scenario_model(scenario, false)).unwrap();
            drop(retained);
            drop(pager);
            phase4a_assert_recovery(temp.path(), scenario, &pre, &post, index >= durable);
            cases += 1;
        }
        assert_eq!(crash_pair_bytes(fixture.path()), unchanged);
    }
    assert_eq!(cases, 32);
    eprintln!("phase4a I/O matrix: {cases} exact injected boundaries and repeat-open checks");
}

fn phase4a_child_command(
    path: &Path,
    scenario: &str,
    point: &str,
    occurrence: &str,
    trace: &[FaultPoint],
) -> std::process::Command {
    let executable = std::env::current_exe().unwrap();
    #[cfg(unix)]
    let mut command = {
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "ulimit -c 0 && exec \"$@\"",
            "ora-sqllite-phase4a-crash",
        ]);
        command.arg(executable);
        command
    };
    #[cfg(not(unix))]
    let mut command = std::process::Command::new(executable);
    command
        .args(["--exact", PHASE_4A_CRASH_CHILD, "--ignored", "--nocapture"])
        .env(PHASE_4A_PATH_ENV, path)
        .env(PHASE_4A_SCENARIO_ENV, scenario)
        .env(PHASE_4A_FAULT_ENV, point)
        .env(PHASE_4A_OCCURRENCE_ENV, occurrence)
        .env(
            PHASE_4A_TRACE_ENV,
            trace
                .iter()
                .map(|point| format!("{point:?}"))
                .collect::<Vec<_>>()
                .join(","),
        );
    command
}

#[test]
#[ignore = "only launched with validated parameters by phase_4a_catalog_crash_recovery_matrix"]
fn phase_4a_catalog_crash_child() {
    let scenario =
        std::env::var(PHASE_4A_SCENARIO_ENV).expect("invalid crash configuration: scenario");
    assert!(
        PHASE_4A_SCENARIOS.contains(&scenario.as_str()),
        "invalid crash configuration: scenario"
    );
    let parse = |name: &str| {
        PHASE_4A_WAL_POINTS
            .into_iter()
            .find(|point| format!("{point:?}") == name)
    };
    let point =
        parse(&std::env::var(PHASE_4A_FAULT_ENV).expect("invalid crash configuration: point"))
            .expect("invalid crash configuration: point");
    let occurrence_text =
        std::env::var(PHASE_4A_OCCURRENCE_ENV).expect("invalid crash configuration: occurrence");
    let occurrence: usize = occurrence_text
        .parse()
        .expect("invalid crash configuration: occurrence");
    assert!(
        occurrence > 0 && occurrence.to_string() == occurrence_text,
        "invalid crash configuration: occurrence"
    );
    let trace_text = std::env::var(PHASE_4A_TRACE_ENV).expect("invalid crash configuration: trace");
    let trace: Vec<_> = trace_text
        .split(',')
        .map(|name| parse(name).expect("invalid crash configuration: trace"))
        .collect();
    assert_commit_trace(&trace);
    assert!(
        trace.iter().filter(|&&seen| seen == point).count() >= occurrence,
        "invalid crash configuration: occurrence outside successful trace"
    );
    let path = PathBuf::from(
        std::env::var_os(PHASE_4A_PATH_ENV).expect("invalid crash configuration: path"),
    );
    assert_eq!(
        fs::canonicalize(&path).expect("invalid crash configuration: path"),
        path,
        "invalid crash configuration: noncanonical path"
    );
    assert_eq!(
        path.parent(),
        Some(fs::canonicalize(std::env::temp_dir()).unwrap().as_path()),
        "invalid crash configuration: path outside temp directory"
    );
    assert!(
        path.file_name()
            .unwrap()
            .to_str()
            .is_some_and(|name| name.starts_with("ora-sqllite-")
                && name.contains("-phase4a-crash-case-")
                && name.ends_with(".db")),
        "invalid crash configuration: non-scenario path"
    );
    assert!(
        path.is_file() && wal_path(&path).is_file(),
        "invalid crash configuration: incomplete pair"
    );
    // Validate the fixture read-only before any potentially mutating recovery.
    let read_only =
        Pager::open(&path, OpenMode::ReadOnly, PagerConfig::test_default(4096)).unwrap();
    assert_phase4a_model(
        &mut read_only.begin_read().unwrap(),
        &phase4a_scenario_model(&scenario, false),
    )
    .unwrap();
    assert_eq!(
        read_only.latest_lsn().unwrap().0,
        u64::from(scenario != "bootstrap")
    );
    drop(read_only);
    let pager = crash_open(&path).with_faults(Arc::new(Phase4aAbortAt {
        scenario: scenario.clone(),
        point,
        occurrence,
        trace,
        observed: Mutex::new(Vec::new()),
    }));
    phase4a_apply_model(
        &pager,
        &phase4a_scenario_model(&scenario, false),
        &phase4a_scenario_model(&scenario, true),
    )
    .unwrap();
    panic!("configured crash boundary was not reached");
}

struct Phase4aAbortAt {
    scenario: String,
    point: FaultPoint,
    occurrence: usize,
    trace: Vec<FaultPoint>,
    observed: Mutex<Vec<FaultPoint>>,
}

impl FaultInjector for Phase4aAbortAt {
    fn check(&self, point: FaultPoint) -> Result<()> {
        let mut observed = self.observed.lock().unwrap();
        assert_eq!(
            self.trace.get(observed.len()),
            Some(&point),
            "child diverged from successful trace"
        );
        observed.push(point);
        let occurrence = observed.iter().filter(|&&seen| seen == point).count();
        if (point, occurrence) == (self.point, self.occurrence) {
            let mut stderr = std::io::stderr().lock();
            writeln!(
                stderr,
                "PHASE4A_ABORT {} {point:?} {occurrence}",
                self.scenario
            )
            .unwrap();
            stderr.flush().unwrap();
            std::process::abort();
        }
        Ok(())
    }
}

#[test]
fn phase_4a_crash_child_rejects_invalid_configuration_without_writing() {
    let fixture = phase4a_fixture("bootstrap");
    let (trace, _, _) = phase4a_trace(&fixture, "bootstrap");
    for (scenario, point, occurrence, path_mode) in [
        ("invalid", "BeforeWalRecordWrite", "1", "valid"),
        ("bootstrap", "unknown", "1", "valid"),
        ("bootstrap", "BeforeWalRecordWrite", "0", "valid"),
        ("bootstrap", "BeforeWalRecordWrite", "01", "valid"),
        ("bootstrap", "BeforeWalRecordWrite", "999999", "valid"),
        ("bootstrap", "BeforeWalRecordWrite", "1", "missing"),
        ("bootstrap", "BeforeWalRecordWrite", "1", "fixture"),
    ] {
        let temp = phase4a_copy_fixture(&fixture);
        let path = match path_mode {
            "missing" => temp.path().with_extension("missing"),
            "fixture" => fs::canonicalize(fixture.path()).unwrap(),
            _ => fs::canonicalize(temp.path()).unwrap(),
        };
        let protected_path = if path_mode == "fixture" {
            fixture.path()
        } else {
            temp.path()
        };
        if path.is_file() {
            assert_eq!(
                fs::canonicalize(protected_path).unwrap(),
                path,
                "snapshot must protect the actual child DB/WAL pair"
            );
        }
        let before = crash_pair_bytes(protected_path);
        let output = phase4a_child_command(&path, scenario, point, occurrence, &trace)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(101),
            "invalid child parameters must fail, not abort"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("invalid crash configuration:"), "{stderr}");
        assert!(!stderr.contains("PHASE4A_ABORT"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
        assert_eq!(crash_pair_bytes(protected_path), before);
    }
}

#[test]
#[ignore = "dedicated Phase 4A subprocess matrix; every WAL occurrence aborts one child"]
fn phase_4a_catalog_crash_recovery_matrix() {
    let mut crashes = 0;
    for scenario in PHASE_4A_SCENARIOS {
        let fixture = phase4a_fixture(scenario);
        let unchanged = crash_pair_bytes(fixture.path());
        let (trace, pre, post) = phase4a_trace(&fixture, scenario);
        let durable = trace
            .iter()
            .position(|&point| point == FaultPoint::AfterWalSync)
            .unwrap();
        for point in PHASE_4A_WAL_POINTS {
            let count = trace.iter().filter(|&&seen| seen == point).count();
            assert!(count > 0, "uncovered {scenario} {point:?}");
            eprintln!("phase4a {scenario} {point:?}: occurrences={count}");
        }
        for (index, &point) in trace.iter().enumerate() {
            let occurrence = trace[..=index]
                .iter()
                .filter(|&&seen| seen == point)
                .count();
            let temp = phase4a_copy_fixture(&fixture);
            let output = phase4a_child_command(
                &fs::canonicalize(temp.path()).unwrap(),
                scenario,
                &format!("{point:?}"),
                &occurrence.to_string(),
                &trace,
            )
            .output()
            .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("running 1 test"),
                "exact child did not run: {stdout}"
            );
            assert!(
                stderr
                    .lines()
                    .any(|line| line == format!("PHASE4A_ABORT {scenario} {point:?} {occurrence}")),
                "child missed fault: status={} stderr={stderr}",
                output.status
            );
            assert!(!output.status.success());
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(
                    output.status.signal(),
                    Some(6),
                    "child must die from SIGABRT"
                );
            }
            #[cfg(windows)]
            assert_eq!(
                output.status.code(),
                Some(-1_073_740_791),
                "Windows abort fast-fail 0xC0000409"
            );
            phase4a_assert_recovery(temp.path(), scenario, &pre, &post, index >= durable);
            crashes += 1;
            eprintln!(
                "phase4a {scenario} {point:?}/{occurrence}: atomic, deep-valid, repeat-open stable, handles released"
            );
        }
        assert_eq!(crash_pair_bytes(fixture.path()), unchanged);
    }
    eprintln!("phase4a crash matrix: {crashes} inspected child aborts across all four scenarios");
}

const CRASH_CHILD_TEST: &str = "storage::test_support::phase_3_crash_child";
const CRASH_POINTS: [FaultPoint; 20] = [
    FaultPoint::BeforeWalRecordWrite,
    FaultPoint::AfterWalRecordWrite,
    FaultPoint::BeforeWalSync,
    FaultPoint::AfterWalSync,
    FaultPoint::BeforeMainExtend,
    FaultPoint::AfterMainExtend,
    FaultPoint::BeforeMainPageWrite,
    FaultPoint::AfterMainPageWrite,
    FaultPoint::BeforeMainSync,
    FaultPoint::AfterMainSync,
    FaultPoint::BeforeSuperblockWrite,
    FaultPoint::AfterSuperblockWrite,
    FaultPoint::BeforeWalHeaderWrite,
    FaultPoint::AfterWalHeaderWrite,
    FaultPoint::BeforeWalTruncate,
    FaultPoint::AfterWalTruncate,
    FaultPoint::AfterWalRecordHeaderPrefixWrite,
    FaultPoint::AfterWalRecordPayloadPrefixWrite,
    FaultPoint::BeforeWalPublish,
    FaultPoint::AfterWalPublish,
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct CrashState {
    lsn: Lsn,
    root: PageRef,
    page_count: u64,
    freelist_root: PageId,
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
}

fn crash_model(post: bool) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut rows = BTreeMap::new();
    for number in 0..8u8 {
        rows.insert(fixture_key(0x33, number), vec![number; 128]);
    }
    rows.insert(b"stable-overflow".to_vec(), vec![0x71; 5000]);
    if post {
        rows.remove(&fixture_key(0x33, 0));
        rows.remove(&fixture_key(0x33, 3));
        rows.insert(fixture_key(0x33, 2), vec![0x92; 6000]);
        for number in 8..12u8 {
            rows.insert(fixture_key(0x33, number), vec![number; 128]);
        }
    }
    rows
}

fn crash_state(pager: &Pager) -> CrashState {
    let mut snapshot = pager.begin_read().expect("open crash snapshot");
    let tree = snapshot
        .catalog_tree(TreeKind::Table)
        .expect("validate crash tree and freelist")
        .expect("crash fixture has a catalog tree");
    CrashState {
        lsn: snapshot.lsn(),
        root: tree.root(),
        page_count: snapshot.page_count().expect("read page count"),
        freelist_root: snapshot.freelist_root().expect("read freelist root"),
        rows: collect_all(&tree, &mut snapshot)
            .expect("scan complete crash tree")
            .into_iter()
            .collect(),
    }
}

fn crash_open(path: &Path) -> Pager {
    Pager::open(path, OpenMode::ReadWrite, PagerConfig::test_default(4096))
        .expect("open crash fixture with fresh parent/child handles")
}

fn crash_commit(pager: &Pager) {
    let mut tree = {
        let mut snapshot = pager.begin_read().expect("read commit root");
        snapshot.catalog_tree(TreeKind::Table).unwrap().unwrap()
    };
    let mut batch = pager.begin_write().expect("begin exactly one crash commit");
    for number in [0, 2, 3] {
        assert!(
            tree.delete(&mut batch, &fixture_key(0x33, number))
                .unwrap()
                .is_some()
        );
    }
    tree.insert(&mut batch, &fixture_key(0x33, 2), &vec![0x92; 6000])
        .unwrap();
    for number in 8..12u8 {
        tree.insert(&mut batch, &fixture_key(0x33, number), &[number; 128])
            .unwrap();
    }
    batch.track_tree(TreeKind::Table, tree.root()).unwrap();
    batch.set_catalog_root(tree.root()).unwrap();
    batch.commit().expect("commit crash transaction");
}

fn crash_fixture(mode: &str) -> TempDatabase {
    let temp = TempDatabase::new("phase3-crash-fixture");
    let pager = Pager::create(temp.path(), PagerConfig::test_default(4096)).unwrap();
    let mut batch = pager.begin_write().unwrap();
    let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
    for (key, value) in crash_model(false) {
        tree.insert(&mut batch, &key, &value).unwrap();
    }
    batch.track_tree(TreeKind::Table, tree.root()).unwrap();
    batch.set_catalog_root(tree.root()).unwrap();
    batch.commit().unwrap();
    pager.checkpoint().unwrap();
    match mode {
        "commit" => {}
        "checkpoint" => crash_commit(&pager),
        _ => panic!("invalid crash fixture mode"),
    }
    let state = crash_state(&pager);
    assert_eq!(state.rows, crash_model(mode == "checkpoint"));
    assert_eq!(state.lsn, Lsn(if mode == "checkpoint" { 2 } else { 1 }));
    drop(pager);
    temp
}

fn crash_pair_bytes(path: &Path) -> (Vec<u8>, Vec<u8>) {
    (fs::read(path).unwrap(), fs::read(wal_path(path)).unwrap())
}

fn copy_crash_fixture(fixture: &TempDatabase) -> TempDatabase {
    let temp = TempDatabase::new("phase3-crash-case");
    assert!(!temp.path().exists() && !wal_path(temp.path()).exists());
    fs::copy(fixture.path(), temp.path()).unwrap();
    fs::copy(wal_path(fixture.path()), wal_path(temp.path())).unwrap();
    assert_eq!(
        crash_pair_bytes(temp.path()),
        crash_pair_bytes(fixture.path())
    );
    temp
}

fn crash_operation(pager: &Pager, mode: &str) {
    match mode {
        "commit" => crash_commit(pager),
        "checkpoint" => {
            pager.checkpoint().expect("exactly one crash checkpoint");
        }
        _ => panic!("invalid crash operation mode"),
    }
}

fn crash_trace(fixture: &TempDatabase, mode: &str) -> (Vec<FaultPoint>, CrashState, CrashState) {
    let temp = copy_crash_fixture(fixture);
    let faults = Arc::new(RecordingFaults::never());
    let pager = crash_open(temp.path()).with_faults(faults.clone());
    let pre = crash_state(&pager);
    let record_start = fs::metadata(wal_path(temp.path())).unwrap().len() as usize;
    crash_operation(&pager, mode);
    let post = crash_state(&pager);
    assert_eq!(post.rows, crash_model(true));
    assert_eq!(post.lsn, Lsn(2));
    if mode == "commit" {
        assert_commit_trace(&faults.observed());
        // Count independently from actual persisted record framing, not from hooks.
        let bytes = fs::read(wal_path(temp.path())).unwrap();
        let mut offset = record_start;
        let mut records = 0;
        while offset < bytes.len() {
            let payload =
                u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap()) as usize;
            offset += 64 + payload;
            records += 1;
        }
        assert_eq!(offset, bytes.len());
        assert_eq!(
            faults.observed().len(),
            records * 4 + 4,
            "each physical record must expose both prefix boundaries"
        );
        assert_ne!(pre, post);
    } else {
        assert_eq!(pre, post, "checkpoint cannot change the logical state");
    }
    drop(pager);
    (faults.observed(), pre, post)
}

pub(crate) fn assert_commit_trace(trace: &[FaultPoint]) {
    let record_points = [
        FaultPoint::BeforeWalRecordWrite,
        FaultPoint::AfterWalRecordHeaderPrefixWrite,
        FaultPoint::AfterWalRecordPayloadPrefixWrite,
        FaultPoint::AfterWalRecordWrite,
    ];
    assert!(
        trace.len() >= 12 && (trace.len() - 4) % 4 == 0,
        "invalid crash configuration: commit trace length"
    );
    let records_end = trace.len() - 4;
    for record in trace[..records_end].chunks_exact(4) {
        assert_eq!(
            record, record_points,
            "invalid crash configuration: record trace"
        );
    }
    assert_eq!(
        &trace[records_end..],
        &[
            FaultPoint::BeforeWalSync,
            FaultPoint::AfterWalSync,
            FaultPoint::BeforeWalPublish,
            FaultPoint::AfterWalPublish
        ],
        "invalid crash configuration: publish trace"
    );
}

fn crash_child_command(
    path: &Path,
    mode: &str,
    point: &str,
    occurrence: &str,
    trace: &[FaultPoint],
) -> std::process::Command {
    let executable = std::env::current_exe().expect("find test executable");
    // Suppress core files without unsafe pre_exec or changing the parent's process limits.
    #[cfg(unix)]
    let mut command = {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "ulimit -c 0 && exec \"$@\"", "ora-sqllite-crash"]);
        command.arg(executable);
        command
    };
    #[cfg(not(unix))]
    let mut command = std::process::Command::new(executable);
    command
        .args(["--exact", CRASH_CHILD_TEST, "--ignored", "--nocapture"])
        .env("ORA_SQLLITE_CRASH_PATH", path)
        .env("ORA_SQLLITE_CRASH_MODE", mode)
        .env("ORA_SQLLITE_CRASH_POINT", point)
        .env("ORA_SQLLITE_CRASH_OCCURRENCE", occurrence)
        .env(
            "ORA_SQLLITE_CRASH_TRACE",
            trace
                .iter()
                .map(|point| format!("{point:?}"))
                .collect::<Vec<_>>()
                .join(","),
        );
    command
}

#[test]
#[ignore = "only launched with validated parameters by phase_3_crash_recovery_matrix"]
fn phase_3_crash_child() {
    let mode = std::env::var("ORA_SQLLITE_CRASH_MODE").expect("crash mode is required");
    assert!(
        matches!(mode.as_str(), "commit" | "checkpoint"),
        "invalid crash configuration: mode"
    );
    let point_text = std::env::var("ORA_SQLLITE_CRASH_POINT").expect("crash point is required");
    let parse_point = |name: &str| {
        CRASH_POINTS
            .into_iter()
            .find(|point| format!("{point:?}") == name)
    };
    let point = parse_point(&point_text).expect("invalid crash configuration: point");
    let occurrence_text =
        std::env::var("ORA_SQLLITE_CRASH_OCCURRENCE").expect("crash occurrence is required");
    let occurrence: usize = occurrence_text
        .parse()
        .expect("invalid crash configuration: occurrence");
    assert!(
        occurrence > 0 && occurrence_text == occurrence.to_string(),
        "invalid crash configuration: occurrence"
    );
    let trace_text =
        std::env::var("ORA_SQLLITE_CRASH_TRACE").expect("successful trace is required");
    let trace: Vec<_> = trace_text
        .split(',')
        .map(|name| parse_point(name).expect("invalid crash configuration: trace point"))
        .collect();
    assert!(
        occurrence <= trace.iter().filter(|&&seen| seen == point).count(),
        "invalid crash configuration: occurrence outside successful trace"
    );
    if mode == "commit" {
        assert_commit_trace(&trace);
    } else {
        assert!(
            !trace.iter().any(|point| CRASH_POINTS[..2].contains(point)),
            "invalid crash configuration: checkpoint trace"
        );
        for required in &CRASH_POINTS[2..16] {
            assert!(
                trace.contains(required),
                "invalid crash configuration: incomplete checkpoint trace"
            );
        }
    }
    let path = PathBuf::from(
        std::env::var_os("ORA_SQLLITE_CRASH_PATH").expect("scenario database path is required"),
    );
    let canonical = fs::canonicalize(&path).expect("invalid crash configuration: database path");
    assert_eq!(
        canonical, path,
        "invalid crash configuration: noncanonical path"
    );
    assert_eq!(
        path.parent(),
        Some(fs::canonicalize(std::env::temp_dir()).unwrap().as_path()),
        "invalid crash configuration: path outside temp directory"
    );
    assert!(
        path.file_name()
            .unwrap()
            .to_str()
            .is_some_and(|name| name.starts_with("ora-sqllite-")
                && name.contains("-phase3-crash-case-")
                && name.ends_with(".db")),
        "invalid crash configuration: non-scenario path"
    );
    assert!(
        path.is_file() && wal_path(&path).is_file(),
        "invalid crash configuration: incomplete database/WAL pair"
    );
    let pager = crash_open(&path);
    let initial = crash_state(&pager);
    assert_eq!(
        initial.rows,
        crash_model(mode == "checkpoint"),
        "invalid crash configuration: fixture model"
    );
    assert_eq!(
        initial.lsn,
        Lsn(if mode == "checkpoint" { 2 } else { 1 }),
        "invalid crash configuration: fixture LSN"
    );
    let pager = pager.with_faults(Arc::new(AbortAt {
        mode: mode.clone(),
        point,
        occurrence,
        trace,
        observed: Mutex::new(Vec::new()),
    }));
    crash_operation(&pager, &mode);
    panic!("configured crash boundary was not reached");
}

struct AbortAt {
    mode: String,
    point: FaultPoint,
    occurrence: usize,
    trace: Vec<FaultPoint>,
    observed: Mutex<Vec<FaultPoint>>,
}

impl FaultInjector for AbortAt {
    fn check(&self, point: FaultPoint) -> Result<()> {
        let mut observed = self.observed.lock().unwrap();
        assert_eq!(
            self.trace.get(observed.len()),
            Some(&point),
            "child diverged from successful trace"
        );
        observed.push(point);
        let occurrence = observed.iter().filter(|&&seen| seen == point).count();
        if (point, occurrence) == (self.point, self.occurrence) {
            let mut stderr = std::io::stderr().lock();
            writeln!(stderr, "PHASE3_ABORT {} {point:?} {occurrence}", self.mode).unwrap();
            stderr.flush().unwrap();
            std::process::abort();
        }
        Ok(())
    }
}

#[test]
fn crash_child_rejects_invalid_configuration_without_writing() {
    let fixture = crash_fixture("commit");
    let (trace, _, _) = crash_trace(&fixture, "commit");
    for (mode, point, occurrence, missing_path) in [
        ("invalid", "BeforeWalRecordWrite", "1", false),
        ("commit", "unknown", "1", false),
        ("commit", "BeforeWalRecordWrite", "0", false),
        ("commit", "BeforeWalRecordWrite", "999999", false),
        ("commit", "BeforeWalRecordWrite", "1", true),
    ] {
        let temp = copy_crash_fixture(&fixture);
        let before = crash_pair_bytes(temp.path());
        let path = if missing_path {
            temp.path().with_extension("missing")
        } else {
            fs::canonicalize(temp.path()).unwrap()
        };
        let output = crash_child_command(&path, mode, point, occurrence, &trace)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(101),
            "bad child parameters must fail the test, not abort"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("invalid crash configuration:"), "{stderr}");
        assert!(!stderr.contains("PHASE3_ABORT"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
        assert_eq!(crash_pair_bytes(temp.path()), before);
    }
}

#[test]
#[ignore = "dedicated subprocess crash matrix; each boundary aborts one child"]
fn phase_3_crash_recovery_matrix() {
    let mut covered = Vec::new();
    let mut crashes = 0usize;
    for mode in ["commit", "checkpoint"] {
        let fixture = crash_fixture(mode);
        let fixture_bytes = crash_pair_bytes(fixture.path());
        let (trace, pre, post) = crash_trace(&fixture, mode);
        assert!(
            !trace.is_empty(),
            "empty trace cannot establish crash coverage"
        );
        let durable_index = trace
            .iter()
            .position(|point| *point == FaultPoint::AfterWalSync)
            .unwrap();
        for point in CRASH_POINTS {
            let count = trace.iter().filter(|&&seen| seen == point).count();
            if count != 0 {
                eprintln!("phase3 {mode} {point:?}: occurrences={count}");
            }
        }
        for (index, &point) in trace.iter().enumerate() {
            let occurrence = trace[..=index]
                .iter()
                .filter(|&&seen| seen == point)
                .count();
            let temp = copy_crash_fixture(&fixture);
            let path = fs::canonicalize(temp.path()).unwrap();
            let output = crash_child_command(
                &path,
                mode,
                &format!("{point:?}"),
                &occurrence.to_string(),
                &trace,
            )
            .output()
            .expect("synchronously wait and reap crash child");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("running 1 test"),
                "exact child test did not run: {stdout}"
            );
            assert!(
                stderr
                    .lines()
                    .any(|line| line == format!("PHASE3_ABORT {mode} {point:?} {occurrence}")),
                "child did not hit its exact fault: status={} stderr={stderr}",
                output.status
            );
            assert!(!output.status.success(), "crash child returned success");
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(
                    output.status.signal(),
                    Some(6),
                    "child must die from SIGABRT"
                );
            }
            #[cfg(windows)]
            assert_eq!(
                output.status.code(),
                Some(-1_073_740_791),
                "child must die from Windows abort fast-fail (0xC0000409)"
            );

            let pager = crash_open(temp.path());
            let recovered = crash_state(&pager);
            if mode == "commit" && index >= durable_index {
                assert_eq!(
                    recovered, post,
                    "durable commit must survive {point:?}/{occurrence}"
                );
            } else {
                assert!(
                    recovered == pre || recovered == post,
                    "partial state at {mode} {point:?}/{occurrence}"
                );
            }
            let verdict = if recovered == post { "post" } else { "pre" };
            pager.checkpoint().expect("checkpoint recovered state");
            assert_eq!(crash_state(&pager), recovered);
            drop(pager);
            let repaired_bytes = crash_pair_bytes(temp.path());
            let reopened = crash_open(temp.path());
            assert_eq!(
                crash_state(&reopened),
                recovered,
                "second reopen changed logical state"
            );
            let checkpoint = reopened.checkpoint().expect("idempotent second checkpoint");
            assert_eq!(checkpoint.pages_written, 0);
            assert!(!checkpoint.wal_reset);
            drop(reopened);
            assert_eq!(
                crash_pair_bytes(temp.path()),
                repaired_bytes,
                "second reopen/checkpoint changed bytes"
            );
            // Unlike best-effort Drop, prove all child/parent handles released both exact files.
            fs::remove_file(wal_path(temp.path())).unwrap();
            fs::remove_file(temp.path()).unwrap();
            covered.push(point);
            crashes = crashes.checked_add(1).unwrap();
            eprintln!(
                "phase3 {mode} {point:?}/{occurrence}: exit={} model={verdict} second-reopen+checkpoint=byte/logical-idempotent handles=released",
                output.status
            );
        }
        assert_eq!(
            crash_pair_bytes(fixture.path()),
            fixture_bytes,
            "source fixture must remain unchanged"
        );
    }
    for point in CRASH_POINTS {
        assert!(covered.contains(&point), "uncovered boundary: {point:?}");
    }
    eprintln!(
        "phase3 crash matrix: {crashes} inspected child aborts; all 20 named boundaries covered"
    );
    full_length_corruption_cases();
}

#[test]
fn phase_3_full_length_corruption_matrix() {
    full_length_corruption_cases();
}

fn full_length_corruption_cases() {
    const RECORD_START: usize = 8192;
    let fixture = crash_fixture("checkpoint");
    for (case, field, value, restamp, reason) in [
        ("bad header CRC", 52, None, false, "header checksum"),
        ("bad payload CRC", 64, None, false, "payload checksum"),
        (
            "bad LSN",
            16,
            Some(1u64),
            true,
            "LSN does not follow the base checkpoint",
        ),
        (
            "bad WAL generation",
            56,
            Some(3),
            true,
            "generation mismatch",
        ),
        (
            "bad page generation",
            40,
            Some(0),
            true,
            "generation must be nonzero",
        ),
    ] {
        let temp = copy_crash_fixture(&fixture);
        let mut bytes = fs::read(wal_path(temp.path())).unwrap();
        let original_len = bytes.len();
        let offset = RECORD_START.checked_add(field).unwrap();
        if let Some(value) = value {
            bytes[offset..offset.checked_add(8).unwrap()].copy_from_slice(&value.to_le_bytes());
        } else {
            bytes[offset] ^= 1;
        }
        if restamp {
            let header = &mut bytes[RECORD_START..RECORD_START + 64];
            header[52..56].fill(0);
            let crc = super::checksum::crc32c(header);
            header[52..56].copy_from_slice(&crc.to_le_bytes());
        }
        assert_eq!(
            bytes.len(),
            original_len,
            "{case} must not be a torn suffix"
        );
        fs::write(wal_path(temp.path()), bytes).unwrap();
        assert_crash_corruption_retained(temp.path(), case, reason);
    }

    let temp = copy_crash_fixture(&fixture);
    let old_wal = fs::read(wal_path(temp.path())).unwrap();
    let pager = crash_open(temp.path());
    pager.checkpoint().unwrap();
    let expected = crash_state(&pager);
    let mut batch = pager.begin_write().unwrap();
    batch.allocate_append(PageType::TableLeaf).unwrap();
    batch.commit().unwrap();
    drop(pager);
    let current_wal = fs::read(wal_path(temp.path())).unwrap();
    assert!(old_wal.len() > RECORD_START && current_wal.len() > RECORD_START);
    let mut mixed = current_wal[..RECORD_START].to_vec();
    mixed.extend_from_slice(&old_wal[RECORD_START..]);
    fs::write(wal_path(temp.path()), &mixed).unwrap();
    // Control: these are valid, complete previous-generation records, not arbitrary garbage.
    let old_only = Pager::open(
        temp.path(),
        OpenMode::ReadOnly,
        PagerConfig::test_default(4096),
    )
    .unwrap();
    assert_eq!(crash_state(&old_only), expected);
    drop(old_only);
    assert_eq!(fs::read(wal_path(temp.path())).unwrap(), mixed);
    mixed.extend_from_slice(&current_wal[RECORD_START..]);
    fs::write(wal_path(temp.path()), &mixed).unwrap();
    assert_crash_corruption_retained(
        temp.path(),
        "valid old-generation suffix followed by current-generation records",
        "contains another generation",
    );

    let temp = copy_crash_fixture(&fixture);
    let original_len = fs::metadata(temp.path()).unwrap().len();
    flip_header_byte(temp.path(), SuperblockSlot::A, 0).unwrap();
    flip_header_byte(temp.path(), SuperblockSlot::B, 0).unwrap();
    assert_eq!(fs::metadata(temp.path()).unwrap().len(), original_len);
    assert_crash_corruption_retained(temp.path(), "both main superblocks invalid", "superblock");
    eprintln!(
        "phase3 corruption matrix: 7 full-length cases rejected; readonly/readwrite bytes retained"
    );
}

fn assert_crash_corruption_retained(path: &Path, case: &str, reason: &str) {
    let before = crash_pair_bytes(path);
    for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
        let result = Pager::open(path, mode, PagerConfig::test_default(4096));
        let error = match result {
            Ok(_) => panic!("{case} was silently accepted"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Corruption, "{case}: {error}");
        assert!(
            error.to_string().contains(reason),
            "{case} failed for an unrelated reason: {error}"
        );
        assert_eq!(
            crash_pair_bytes(path),
            before,
            "{case} must never silently truncate or rewrite either file"
        );
    }
    eprintln!("phase3 corruption {case}: Corruption; full database/WAL bytes retained");
}

struct ScaleMetrics {
    rows: u64,
    file_bytes: u64,
    tree_height: u64,
    reused_pages: u64,
    elapsed: Duration,
}

#[test]
#[ignore = "manual million-record Phase 2 scale probe"]
fn million_small_records_report_storage_shape() {
    let metrics = run_scale_probe(1_000_000, 8192).expect("scale probe");
    eprintln!(
        "rows={} bytes={} height={} reused_pages={} elapsed_ms={}",
        metrics.rows,
        metrics.file_bytes,
        metrics.tree_height,
        metrics.reused_pages,
        metrics.elapsed.as_millis()
    );
    assert_eq!(metrics.rows, 1_000_000);
}

fn run_scale_probe(rows: u64, page_size: u32) -> Result<ScaleMetrics> {
    const BATCH_ROWS: u64 = 10_000;

    let started = Instant::now();
    let temp = TempDatabase::new("million-small-records");
    let mut pager = Pager::create(temp.path(), PagerConfig::test_default(page_size))?;
    let mut tree = create_catalog_tree(&mut pager, TreeKind::Table)?;
    let mut next_row = 0u64;

    while next_row < rows {
        let mut batch = pager.begin_write()?;
        let end = next_row.saturating_add(BATCH_ROWS).min(rows);
        while next_row < end {
            tree.insert(&mut batch, &next_row.to_be_bytes(), b"phase-2-scale")?;
            next_row += 1;
        }
        batch.track_tree(TreeKind::Table, tree.root())?;
        batch.set_catalog_root(tree.root())?;
        batch.commit()?;
    }
    let file_bytes = SUPERBLOCK_AREA_LEN
        .checked_add(
            pager
                .page_count()
                .checked_sub(1)
                .and_then(|ordinary_pages| ordinary_pages.checked_mul(u64::from(page_size)))
                .ok_or_else(|| DbError::new(ErrorCode::Corruption, "scale file length overflow"))?,
        )
        .ok_or_else(|| DbError::new(ErrorCode::Corruption, "scale file length overflow"))?;
    let mut snapshot = pager.begin_read()?;
    let tree_height = tree_height(&mut snapshot, tree.root())?;

    Ok(ScaleMetrics {
        rows,
        file_bytes,
        tree_height,
        reused_pages: pager.reused_page_count(),
        elapsed: started.elapsed(),
    })
}

fn tree_height(source: &mut impl PageSource, mut current: PageRef) -> Result<u64> {
    let mut height = 0u64;
    loop {
        let frame = source.read_page(current)?;
        height = height
            .checked_add(1)
            .ok_or_else(|| DbError::new(ErrorCode::Corruption, "scale tree height overflow"))?;
        current = match frame.header().page_type() {
            PageType::TableLeaf | PageType::IndexLeaf => return Ok(height),
            PageType::TableInternal | PageType::IndexInternal => {
                super::btree::decode_internal_leftmost(&frame.bytes()[48..64])?
            }
            _ => {
                return Err(DbError::new(
                    ErrorCode::Corruption,
                    "scale tree contains a non-B+Tree page",
                ));
            }
        };
    }
}
