use std::ops::Bound;

#[cfg(test)]
use std::cell::{Cell, RefCell};

use crate::{DbError, ErrorCode, Result, Value};

use super::{
    PageRef, ReadSnapshot, WriteBatch,
    btree::{BTree, BTreeScanState, TreeKind},
    catalog::{
        CatalogReader, CatalogWriter,
        types::{
            CatalogName, ColumnDef, ColumnId, ColumnType, MAX_COLUMNS, RowId, SchemaVersion,
            TableDef, TableId,
        },
    },
    pager::PageSource,
    row::{RowCodec, RowCodecValidation, decode_rowid_key, encode_rowid_key},
};

#[derive(Clone)]
pub(crate) struct ColumnSpec {
    name: CatalogName,
    column_type: ColumnType,
    nullable: bool,
}

#[derive(Clone)]
pub(crate) struct TableSpec {
    name: CatalogName,
    columns: Box<[ColumnSpec]>,
}

#[derive(Clone)]
pub(crate) struct Relation {
    table: TableDef,
    columns: Box<[ColumnDef]>,
}

pub(crate) struct RelationScan {
    relation: Relation,
    state: BTreeScanState,
    max_row_bytes: usize,
    bounded_materialization: bool,
}

pub(crate) struct StoredRow {
    rowid: RowId,
    values: Box<[Value]>,
}

pub(crate) enum RelationValueValidation {
    Valid,
    ExceedsLimit,
    InvalidValue { ordinal: u16, error: DbError },
}

pub(crate) struct TableWrite {
    schema: Relation,
    rows: BTree,
    catalog: CatalogWriter,
    original_root: PageRef,
}

#[cfg(test)]
thread_local! {
    static NEXT_AFTER_FAILURE: Cell<Option<(usize, ErrorCode)>> = const { Cell::new(None) };
    static VALUE_VALIDATION_LIMITS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn fail_table_write_next_after_for_test(successful_calls: usize, code: ErrorCode) {
    NEXT_AFTER_FAILURE.set(Some((successful_calls, code)));
}

#[cfg(test)]
pub(crate) fn take_value_validation_limits_for_test() -> Vec<usize> {
    VALUE_VALIDATION_LIMITS.with(|limits| std::mem::take(&mut *limits.borrow_mut()))
}

#[cfg(test)]
fn injected_next_after_error() -> Option<DbError> {
    NEXT_AFTER_FAILURE.with(|failure| match failure.get() {
        Some((0, code)) => {
            failure.set(None);
            Some(DbError::new(code, "injected TableWrite next_after failure"))
        }
        Some((remaining, code)) => {
            failure.set(Some((remaining - 1, code)));
            None
        }
        None => None,
    })
}

impl ColumnSpec {
    pub(crate) const fn new(name: CatalogName, column_type: ColumnType, nullable: bool) -> Self {
        Self {
            name,
            column_type,
            nullable,
        }
    }

    pub(crate) const fn name(&self) -> &CatalogName {
        &self.name
    }

    pub(crate) const fn column_type(&self) -> ColumnType {
        self.column_type
    }

    pub(crate) const fn nullable(&self) -> bool {
        self.nullable
    }
}

impl TableSpec {
    pub(crate) fn new(name: CatalogName, columns: Vec<ColumnSpec>) -> Result<Self> {
        if columns.len() > usize::from(MAX_COLUMNS) {
            return Err(DbError::new(
                ErrorCode::Type,
                "table column count must not exceed 1000",
            ));
        }
        Ok(Self {
            name,
            columns: columns.into_boxed_slice(),
        })
    }

    pub(crate) const fn name(&self) -> &CatalogName {
        &self.name
    }

    pub(crate) const fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }
}

impl Relation {
    pub(crate) fn open(catalog: &CatalogReader, table_id: TableId) -> Result<Self> {
        let table = catalog.find_table_by_id(table_id).cloned().ok_or_else(|| {
            DbError::new(
                ErrorCode::SchemaChanged,
                format!("table ID {} does not exist", table_id.get()),
            )
        })?;
        let columns = catalog
            .columns_for_table(table_id)
            .cloned()
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self { table, columns })
    }

    pub(crate) fn open_from_batch(batch: &mut WriteBatch, table_id: TableId) -> Result<Self> {
        let catalog = batch.catalog_reader()?;
        Self::open(&catalog, table_id)
    }

    pub(crate) fn column_count(&self) -> usize {
        self.columns.len()
    }

    pub(crate) fn validate_values(&self, values: &[Value], max_row_bytes: usize) -> Result<()> {
        RowCodec::encode(&self.table, &self.columns, values, max_row_bytes).map(|_| ())
    }

    pub(crate) fn validate_update_values(
        &self,
        values: &[Value],
        max_row_bytes: usize,
    ) -> Result<RelationValueValidation> {
        #[cfg(test)]
        VALUE_VALIDATION_LIMITS.with(|limits| limits.borrow_mut().push(max_row_bytes));
        match RowCodec::validate(&self.table, &self.columns, values, max_row_bytes) {
            Ok(RowCodecValidation::Valid) => Ok(RelationValueValidation::Valid),
            Ok(RowCodecValidation::ExceedsCallerLimit) => Ok(RelationValueValidation::ExceedsLimit),
            Err(error) if error.code() == ErrorCode::Type => {
                if values.len() != self.columns.len() {
                    return Err(error);
                }
                for (ordinal, (column, value)) in self.columns.iter().zip(values).enumerate() {
                    if let Some(error) = invalid_value_shape(column, value) {
                        let ordinal = u16::try_from(ordinal).map_err(|_| {
                            DbError::new(
                                ErrorCode::Internal,
                                "relation column ordinal exceeds its bound",
                            )
                        })?;
                        return Ok(RelationValueValidation::InvalidValue { ordinal, error });
                    }
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn scan(&self, max_row_bytes: usize) -> RelationScan {
        let rows = BTree::from_root(TreeKind::Table, self.table.root());
        let state = rows
            .scan_state(Bound::Unbounded, Bound::Unbounded)
            .expect("unbounded relation scan bounds are always valid");
        RelationScan {
            relation: self.clone(),
            state,
            max_row_bytes,
            bounded_materialization: false,
        }
    }

    /// Conservative peak for raw materialization + decoded payload + directory
    /// and Value bodies. Unlike ordinary scans, enforce this before value I/O.
    pub(crate) fn scan_with_work_budget(&self, work_bytes: u64) -> Result<RelationScan> {
        let per_column = std::mem::size_of::<Value>() as u64
            + std::mem::size_of::<Option<(usize, usize)>>() as u64;
        let containers = 128_u64 + self.columns.len() as u64 * per_column;
        let raw_limit = work_bytes.checked_sub(containers).ok_or_else(|| {
            DbError::new(
                ErrorCode::ResourceLimit,
                "row containers exceed work memory",
            )
        })? / 3;
        let raw_limit = usize::try_from(raw_limit).map_err(|_| {
            DbError::new(
                ErrorCode::ResourceLimit,
                "row memory budget exceeds platform size",
            )
        })?;
        let mut scan = self.scan(raw_limit);
        scan.bounded_materialization = true;
        Ok(scan)
    }
}

fn invalid_value_shape(column: &ColumnDef, value: &Value) -> Option<DbError> {
    if matches!(value, Value::Null) {
        return (!column.nullable()).then(|| {
            DbError::new(
                ErrorCode::Type,
                "NULL is not valid for a non-nullable column",
            )
        });
    }
    let valid = match (column.column_type(), value) {
        (ColumnType::Int64, Value::Int64(_))
        | (ColumnType::Decimal, Value::Decimal(_))
        | (ColumnType::Float64, Value::Float64(_))
        | (ColumnType::Text, Value::Text(_))
        | (ColumnType::Blob, Value::Blob(_))
        | (ColumnType::Date, Value::Date(_))
        | (ColumnType::Bool, Value::Bool(_)) => true,
        (ColumnType::Timestamp, Value::Timestamp(value)) => value.offset_minutes().is_none(),
        (ColumnType::TimestampWithTimeZone, Value::Timestamp(value)) => {
            value.offset_minutes().is_some()
        }
        _ => false,
    };
    (!valid).then(|| {
        DbError::new(
            ErrorCode::Type,
            "value does not match its bound column type",
        )
    })
}

impl RelationScan {
    pub(crate) fn next_snapshot(
        &mut self,
        snapshot: &mut ReadSnapshot,
    ) -> Result<Option<StoredRow>> {
        self.next_from(snapshot)
    }

    pub(crate) fn next_batch(&mut self, batch: &mut WriteBatch) -> Result<Option<StoredRow>> {
        self.next_from(batch)
    }

    fn next_from(&mut self, source: &mut impl PageSource) -> Result<Option<StoredRow>> {
        let next = if self.bounded_materialization {
            self.state
                .next_value_limited(source, self.max_row_bytes as u64)?
        } else {
            self.state.next(source)?
        };
        let Some((key, bytes)) = next else {
            return Ok(None);
        };
        let rowid = decode_rowid_key(&key)?;
        let values = RowCodec::decode(
            &self.relation.table,
            &self.relation.columns,
            &bytes,
            self.max_row_bytes,
        )?
        .into_boxed_slice();
        Ok(Some(StoredRow { rowid, values }))
    }
}

impl StoredRow {
    pub(crate) const fn rowid(&self) -> RowId {
        self.rowid
    }

    pub(crate) fn values(&self) -> &[Value] {
        &self.values
    }

    pub(crate) fn into_values(self) -> Box<[Value]> {
        self.values
    }
}

impl TableWrite {
    pub(crate) fn open(batch: &mut WriteBatch, table_id: TableId) -> Result<Self> {
        let schema = {
            let catalog = batch.catalog_reader()?;
            Relation::open(&catalog, table_id)?
        };
        let original_root = schema.table.root();
        let rows = BTree::from_root(TreeKind::Table, original_root);
        let catalog = CatalogWriter::bootstrap(batch)?;
        Ok(Self {
            schema,
            rows,
            catalog,
            original_root,
        })
    }

    pub(crate) fn insert(&mut self, batch: &mut WriteBatch, values: &[Value]) -> Result<RowId> {
        let bytes = RowCodec::encode(&self.schema.table, &self.schema.columns, values, usize::MAX)?;
        let checkpoint = batch.begin_mutation_scope()?;
        let catalog_checkpoint = self.catalog.checkpoint();
        let rows_root = self.rows.root();
        let result = (|| {
            let rowid = self.catalog.allocate_rowid(batch, self.schema.table.id())?;
            self.rows.insert(batch, &encode_rowid_key(rowid), &bytes)?;
            Ok(rowid)
        })();
        match result {
            Ok(rowid) => {
                batch.finish_mutation_scope(checkpoint)?;
                Ok(rowid)
            }
            Err(error) => {
                batch.rollback_mutation_scope(checkpoint)?;
                self.catalog.restore(catalog_checkpoint);
                self.rows = BTree::from_root(TreeKind::Table, rows_root);
                Err(error)
            }
        }
    }

    pub(crate) fn replace(
        &mut self,
        batch: &mut WriteBatch,
        rowid: RowId,
        values: &[Value],
    ) -> Result<()> {
        let bytes = RowCodec::encode(&self.schema.table, &self.schema.columns, values, usize::MAX)?;
        let checkpoint = batch.begin_mutation_scope()?;
        let catalog_checkpoint = self.catalog.checkpoint();
        let rows_root = self.rows.root();
        let key = encode_rowid_key(rowid);
        let result = (|| {
            if self.rows.delete(batch, &key)?.is_none() {
                return Err(DbError::new(
                    ErrorCode::Corruption,
                    format!("row ID {} disappeared during replacement", rowid.get()),
                ));
            }
            self.rows.insert(batch, &key, &bytes)
        })();
        match result {
            Ok(()) => batch.finish_mutation_scope(checkpoint),
            Err(error) => {
                batch.rollback_mutation_scope(checkpoint)?;
                self.catalog.restore(catalog_checkpoint);
                self.rows = BTree::from_root(TreeKind::Table, rows_root);
                Err(error)
            }
        }
    }

    pub(crate) fn delete(&mut self, batch: &mut WriteBatch, rowid: RowId) -> Result<bool> {
        self.rows
            .delete(batch, &encode_rowid_key(rowid))
            .map(|removed| removed.is_some())
    }

    pub(crate) fn next_after(
        &mut self,
        batch: &mut WriteBatch,
        rowid: Option<RowId>,
        max_row_bytes: usize,
    ) -> Result<Option<StoredRow>> {
        #[cfg(test)]
        if let Some(error) = injected_next_after_error() {
            return Err(error);
        }
        let mut state = if let Some(rowid) = rowid {
            let key = encode_rowid_key(rowid);
            self.rows
                .scan_state(Bound::Excluded(key.as_slice()), Bound::Unbounded)?
        } else {
            self.rows.scan_state(Bound::Unbounded, Bound::Unbounded)?
        };
        let Some((key, bytes)) = state.next(batch)? else {
            return Ok(None);
        };
        let rowid = decode_rowid_key(&key)?;
        let values = RowCodec::decode(
            &self.schema.table,
            &self.schema.columns,
            &bytes,
            max_row_bytes,
        )?
        .into_boxed_slice();
        Ok(Some(StoredRow { rowid, values }))
    }

    pub(crate) fn finish(mut self, batch: &mut WriteBatch) -> Result<()> {
        if self.rows.root() != self.original_root {
            self.catalog
                .update_table_root(batch, self.schema.table.id(), self.rows.root())?;
        }
        self.catalog.finish(batch)
    }
}

pub(crate) fn create_relation(batch: &mut WriteBatch, spec: &TableSpec) -> Result<TableId> {
    let checkpoint = batch.statement_checkpoint()?;
    let result = (|| {
        let mut catalog = CatalogWriter::bootstrap(batch)?;
        let table_id = TableId::new(catalog.allocate_object_id(batch)?.get())?;
        let root = BTree::create(batch, TreeKind::Table)?.root();
        let column_count = u16::try_from(spec.columns().len()).map_err(|_| {
            DbError::new(
                ErrorCode::Type,
                "table column count does not fit the catalog",
            )
        })?;
        let table = TableDef::new(
            table_id,
            SchemaVersion::new(1)?,
            root,
            RowId::new(1)?,
            column_count,
            spec.name().clone(),
        )?;
        catalog.insert_table_definition(batch, &table)?;
        for (ordinal, spec) in spec.columns().iter().enumerate() {
            let column = ColumnDef::new(
                ColumnId::new(catalog.allocate_object_id(batch)?.get())?,
                table_id,
                u16::try_from(ordinal).map_err(|_| {
                    DbError::new(ErrorCode::Type, "column ordinal does not fit the catalog")
                })?,
                spec.column_type(),
                spec.nullable(),
                spec.name().clone(),
            )?;
            catalog.insert_column_definition(batch, &column)?;
        }
        catalog.finish(batch)?;
        Ok(table_id)
    })();
    match result {
        Ok(table_id) => {
            batch.finish_statement(checkpoint)?;
            Ok(table_id)
        }
        Err(error) => {
            batch.restore_statement(checkpoint);
            Err(error)
        }
    }
}

pub(crate) fn drop_relation(batch: &mut WriteBatch, table_id: TableId) -> Result<()> {
    let checkpoint = batch.statement_checkpoint()?;
    let result = (|| {
        let relation = {
            let catalog = batch.catalog_reader()?;
            let relation = Relation::open(&catalog, table_id)?;
            catalog.validate_deep_batch(batch)?;
            relation
        };
        let mut catalog = CatalogWriter::bootstrap(batch)?;
        catalog.remove_relation(batch, &relation.table, &relation.columns)?;
        catalog.finish(batch)
    })();
    match result {
        Ok(()) => batch.finish_statement(checkpoint),
        Err(error) => {
            batch.restore_statement(checkpoint);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ColumnSpec, Relation, TableSpec, TableWrite, create_relation, drop_relation};
    use crate::storage::{
        Pager,
        btree::{BTree, TreeKind},
        catalog::{
            CatalogReader, CatalogWriter,
            types::{
                CatalogName, ColumnDef, ColumnId, ColumnType, RowId, SchemaVersion, TableDef,
                TableId,
            },
        },
        file::OpenMode,
        pager::PagerConfig,
        row::{RowCodec, encode_rowid_key},
        test_support::{TempDatabase, phase_4b_catalog_fixture, test_pager},
    };
    use crate::{Database, ErrorCode, Options, Parameters, Result, Value};

    const LARGE_BLOB_BYTES: usize = 20_000;
    const LARGE_ROW_BYTES: usize = 20_041;
    type RelationContents = (TableDef, Vec<(u64, Vec<Value>)>);

    #[test]
    fn relation_prevalidates_complete_rows_without_opening_the_catalog_gate() {
        // Production break caught: deferring NULL/type/size checks until
        // TableWrite::open can leave the catalog gate open before INSERT fails.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec(
                "T",
                &[
                    ("ID", ColumnType::Int64, false),
                    ("PAYLOAD", ColumnType::Text, true),
                ],
            ),
            &[],
        );
        let mut batch = pager.try_begin_write().unwrap();
        let before = batch
            .catalog_reader()
            .unwrap()
            .find_table_by_id(table_id)
            .unwrap()
            .clone();
        let relation = Relation::open_from_batch(&mut batch, table_id).unwrap();

        for (values, limit) in [
            (vec![Value::Null, Value::Null], usize::MAX),
            (vec![Value::Text("wrong".into()), Value::Null], usize::MAX),
            (vec![Value::Int64(1), Value::Text("too large".into())], 1),
        ] {
            assert_eq!(
                relation.validate_values(&values, limit).unwrap_err().code(),
                ErrorCode::Type
            );
            let after = batch
                .catalog_reader()
                .expect("prevalidation must not leave a catalog writer open")
                .find_table_by_id(table_id)
                .unwrap()
                .clone();
            assert_eq!(after.root(), before.root());
            assert_eq!(after.next_rowid(), before.next_rowid());
        }
        batch.rollback().unwrap();
    }

    #[test]
    fn sql_insert_root_split_rolls_back_and_commits_as_one_relation() {
        // Production break caught: publishing row pages without the matching
        // catalog root/next ROWID, or failing to restore them on rollback,
        // tears a root-splitting sequence executed through the SQL API.
        let fixture = phase_4b_catalog_fixture(4096);
        let before = {
            let pager = fixture.pager();
            read_relation(&pager, "USERS").unwrap()
        };

        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let insert = transaction
            .prepare("INSERT INTO USERS (ID, NAME) VALUES (:1, :2)")
            .unwrap();
        for id in 0_i64..96 {
            let values = [Value::Int64(id).into(), Value::Text("x".repeat(128)).into()];
            transaction
                .execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        let count = transaction.prepare("SELECT COUNT(*) FROM USERS").unwrap();
        let mut rows = transaction.query(&count, Parameters::none()).unwrap();
        assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(96)]);
        drop(rows);
        transaction.rollback().unwrap();
        drop(connection);
        drop(db);

        let rolled_back = {
            let pager = fixture.pager();
            read_relation(&pager, "USERS").unwrap()
        };
        assert_eq!(rolled_back, before);

        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let insert = transaction
            .prepare("INSERT INTO USERS (ID, NAME) VALUES (:1, :2)")
            .unwrap();
        for id in 0_i64..96 {
            let values = [Value::Int64(id).into(), Value::Text("x".repeat(128)).into()];
            transaction
                .execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(connection);
        drop(db);

        let committed = {
            let pager = fixture.pager();
            read_relation(&pager, "USERS").unwrap()
        };
        assert_ne!(committed.0.root(), before.0.root());
        assert_eq!(committed.0.next_rowid().get(), 97);
        assert_eq!(committed.1.len(), 96);
    }

    #[test]
    fn relation_write_updates_catalog_root_rowid_and_rows_together() {
        // Production break caught: publishing a row without the matching
        // catalog root and next ROWID makes the committed relation undecodable.
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let table_id = create_relation(
            &mut batch,
            &table_spec("T", &[("ID", ColumnType::Int64, true)]),
        )
        .unwrap();
        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        let rowid = writer.insert(&mut batch, &[Value::Int64(7)]).unwrap();
        assert_eq!(rowid.get(), 1);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let (table, rows) = read_relation(&pager, "T").unwrap();
        assert_eq!(table.next_rowid().get(), 2);
        assert_eq!(rows, vec![(1, vec![Value::Int64(7)])]);
    }

    #[test]
    fn table_write_inserts_replaces_deletes_and_resumes_after_rowid() {
        // Production break caught: mutation code that rewrites ROWID, treats a
        // missing delete as affected, or resumes inclusively repeats a row.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("T", &[("ID", ColumnType::Int64, true)]),
            &[],
        );
        let mut batch = pager.try_begin_write().unwrap();
        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        let first = writer.insert(&mut batch, &[Value::Int64(1)]).unwrap();
        let second = writer.insert(&mut batch, &[Value::Int64(2)]).unwrap();
        let third = writer.insert(&mut batch, &[Value::Int64(3)]).unwrap();
        assert_eq!([first.get(), second.get(), third.get()], [1, 2, 3]);

        writer
            .replace(&mut batch, second, &[Value::Int64(20)])
            .unwrap();
        assert!(writer.delete(&mut batch, first).unwrap());
        assert!(!writer.delete(&mut batch, first).unwrap());

        let row = writer
            .next_after(&mut batch, None, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(row.rowid(), second);
        assert_eq!(row.values(), &[Value::Int64(20)]);
        let row = writer
            .next_after(&mut batch, Some(second), usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(row.rowid(), third);
        assert_eq!(row.values(), &[Value::Int64(3)]);
        assert!(
            writer
                .next_after(&mut batch, Some(third), usize::MAX)
                .unwrap()
                .is_none()
        );

        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let (table, rows) = read_relation(&pager, "T").unwrap();
        assert_eq!(table.next_rowid().get(), 4);
        assert_eq!(
            rows,
            vec![(2, vec![Value::Int64(20)]), (3, vec![Value::Int64(3)])]
        );
    }

    #[test]
    fn table_write_next_after_honors_the_caller_row_decode_limit() {
        // Production break caught: decoding with usize::MAX lets UPDATE/DELETE
        // retain a row larger than the execution caller's resource boundary.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("T", &[("PAYLOAD", ColumnType::Text, false)]),
            &[vec![Value::Text("x".repeat(256))]],
        );
        let mut batch = pager.try_begin_write().unwrap();
        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        let error = match writer.next_after(&mut batch, None, 64) {
            Err(error) => error,
            Ok(_) => panic!("the caller row limit must reject the stored row"),
        };
        assert_eq!(error.code(), ErrorCode::Type);
        let row = writer
            .next_after(&mut batch, None, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(row.values(), &[Value::Text("x".repeat(256))]);
        writer.finish(&mut batch).unwrap();
        batch.rollback().unwrap();
    }

    #[test]
    fn table_write_finish_updates_only_a_changed_row_tree_root() {
        // Production break caught: always replacing the TableDef can publish a
        // stale root, while never replacing it loses a split row tree.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("SMALL", &[("ID", ColumnType::Int64, false)]),
            &[],
        );
        let original = read_relation(&pager, "SMALL").unwrap().0.root();
        let mut batch = pager.try_begin_write().unwrap();
        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        writer.insert(&mut batch, &[Value::Int64(1)]).unwrap();
        assert_eq!(writer.rows.root(), original);
        writer.finish(&mut batch).unwrap();
        assert_eq!(
            batch
                .catalog_reader()
                .unwrap()
                .find_table_by_id(table_id)
                .unwrap()
                .root(),
            original
        );
        batch.commit().unwrap();

        let table_id = committed_relation(
            &pager,
            table_spec("GROWING", &[("PAYLOAD", ColumnType::Blob, false)]),
            &[],
        );
        let original = read_relation(&pager, "GROWING").unwrap().0.root();
        let mut batch = pager.try_begin_write().unwrap();
        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        for value in 0_u8..96 {
            writer
                .insert(&mut batch, &[Value::Blob(vec![value; 128])])
                .unwrap();
        }
        let changed = writer.rows.root();
        assert_ne!(changed, original);
        writer.finish(&mut batch).unwrap();
        assert_eq!(
            batch
                .catalog_reader()
                .unwrap()
                .find_table_by_id(table_id)
                .unwrap()
                .root(),
            changed
        );
        batch.commit().unwrap();
        assert_eq!(read_relation(&pager, "GROWING").unwrap().1.len(), 96);
    }

    #[test]
    fn restoring_statement_checkpoint_removes_catalog_root_rowid_and_page_mutations() {
        // Production break caught: restoring only dirty row bytes can leave a
        // split root, allocated pages, next ROWID, or catalog gate behind.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("T", &[("PAYLOAD", ColumnType::Blob, false)]),
            &[vec![Value::Blob(vec![0x11; 128])]],
        );
        let before = read_relation(&pager, "T").unwrap();
        let page_count = pager.page_count();

        let mut batch = pager.try_begin_write().unwrap();
        let checkpoint = batch.statement_checkpoint().unwrap();
        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        for value in 0_u8..96 {
            writer
                .insert(&mut batch, &[Value::Blob(vec![value; 128])])
                .unwrap();
        }
        writer.finish(&mut batch).unwrap();
        assert_ne!(
            batch
                .catalog_reader()
                .unwrap()
                .find_table_by_id(table_id)
                .unwrap()
                .root(),
            before.0.root()
        );
        batch.restore_statement(checkpoint);
        batch.commit().unwrap();

        assert_eq!(pager.page_count(), page_count);
        assert_eq!(read_relation(&pager, "T").unwrap(), before);
    }

    #[test]
    fn restoring_statement_checkpoint_preserves_independent_fatal_state() {
        // Production break caught: copying a pre-error rollback flag out of the
        // checkpoint would let a fatal transaction commit after restoration.
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let checkpoint = batch.statement_checkpoint().unwrap();
        create_relation(
            &mut batch,
            &table_spec("T", &[("ID", ColumnType::Int64, false)]),
        )
        .unwrap();
        batch.mark_rollback_only();
        batch.restore_statement(checkpoint);
        assert_eq!(
            batch.catalog_reader().unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
    }

    #[test]
    fn failed_insert_after_rowid_allocation_restores_batch_writer_and_clean_finish_state() {
        // Production break caught: restoring only the batch after a duplicate
        // row key leaves CatalogWriter dirty even though ROWID allocation was
        // rolled back, so finish publishes state a clean writer would not.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("T", &[("ID", ColumnType::Int64, false)]),
            &[],
        );

        let mut batch = pager.try_begin_write().unwrap();
        let relation = {
            let catalog = batch.catalog_reader().unwrap();
            Relation::open(&catalog, table_id).unwrap()
        };
        assert_eq!(relation.table.next_rowid().get(), 1);
        let bytes = RowCodec::encode(
            &relation.table,
            &relation.columns,
            &[Value::Int64(7)],
            usize::MAX,
        )
        .unwrap();
        let mut rows = BTree::from_root(TreeKind::Table, relation.table.root());
        rows.insert(
            &mut batch,
            &encode_rowid_key(RowId::new(1).unwrap()),
            &bytes,
        )
        .unwrap();
        assert_eq!(rows.root(), relation.table.root());

        let before_clean_finish = batch.checkpoint_mutation().unwrap();
        let clean_writer = TableWrite::open(&mut batch, table_id).unwrap();
        clean_writer.finish(&mut batch).unwrap();
        let clean_finish = batch.checkpoint_mutation().unwrap();
        batch.restore_mutation(before_clean_finish);

        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        let catalog_root = writer.catalog.root();
        let rows_root = writer.rows.root();
        let expected_row = writer
            .next_after(&mut batch, None, usize::MAX)
            .unwrap()
            .unwrap();
        let expected_row = (expected_row.rowid(), expected_row.values().to_vec());
        assert!(
            writer
                .next_after(&mut batch, Some(expected_row.0), usize::MAX)
                .unwrap()
                .is_none()
        );
        let before_failed_insert = batch.checkpoint_mutation().unwrap();

        let error = writer.insert(&mut batch, &[Value::Int64(8)]).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Constraint);
        assert!(error.to_string().contains("duplicate physical B+Tree key"));
        before_failed_insert.assert_matches_batch_for_test(&batch);
        assert_eq!(writer.catalog.root(), catalog_root);
        assert_eq!(writer.rows.root(), rows_root);

        let features = batch.feature_bits();
        let catalog_tree = BTree::from_root(TreeKind::Table, writer.catalog.root());
        let catalog = CatalogReader::open_candidate(&mut batch, &catalog_tree, features).unwrap();
        assert_eq!(
            catalog
                .find_table_by_id(table_id)
                .unwrap()
                .next_rowid()
                .get(),
            1
        );
        let row = writer
            .next_after(&mut batch, None, usize::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.rowid(), row.values()),
            (expected_row.0, expected_row.1.as_slice())
        );
        assert!(
            writer
                .next_after(&mut batch, Some(expected_row.0), usize::MAX)
                .unwrap()
                .is_none()
        );

        writer.finish(&mut batch).unwrap();
        clean_finish.assert_matches_batch_for_test(&batch);
        batch.rollback().unwrap();
    }

    #[test]
    fn failed_duplicate_create_restores_object_ids_pages_and_catalog_gate() {
        // Production break caught: a CREATE failure after allocating IDs and a
        // row-tree root must not consume them or leave the batch uncommittable.
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let first = create_relation(
            &mut batch,
            &table_spec("T", &[("ID", ColumnType::Int64, false)]),
        )
        .unwrap();
        assert_eq!(first.get(), 1);
        assert_eq!(
            create_relation(
                &mut batch,
                &table_spec("T", &[("OTHER", ColumnType::Int64, false)]),
            )
            .unwrap_err()
            .code(),
            ErrorCode::Constraint
        );
        let second = create_relation(
            &mut batch,
            &table_spec("U", &[("ID", ColumnType::Int64, false)]),
        )
        .unwrap();
        assert_eq!(second.get(), 3);
        batch.commit().unwrap();
        assert!(read_relation(&pager, "T").is_some());
        assert!(read_relation(&pager, "U").is_some());
    }

    #[test]
    fn create_rows_and_drop_publish_atomically_across_reopen() {
        // Production break caught: WAL publication that separates catalog,
        // next ROWID, tree root, and rows produces a torn reopened relation.
        let (temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("T", &[("ID", ColumnType::Int64, false)]),
            &[vec![Value::Int64(10)], vec![Value::Int64(20)]],
        );
        drop(pager);

        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let (table, rows) = read_relation(&pager, "T").unwrap();
        assert_eq!(table.next_rowid().get(), 3);
        assert_eq!(
            rows,
            vec![(1, vec![Value::Int64(10)]), (2, vec![Value::Int64(20)])]
        );
        let mut batch = pager.try_begin_write().unwrap();
        drop_relation(&mut batch, table_id).unwrap();
        batch.commit().unwrap();
        drop(pager);

        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        assert!(read_relation(&pager, "T").is_none());
    }

    #[test]
    fn relation_scan_decodes_rowid_and_values_without_borrowing_snapshot() {
        let fixture = phase_4a_relation_fixture();
        let mut snapshot = fixture.pager().begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let relation = Relation::open(&catalog, fixture.table_id()).unwrap();
        let mut scan = relation.scan(16_384);
        let first = scan.next_snapshot(&mut snapshot).unwrap().unwrap();
        assert_eq!(first.rowid().get(), 1);
        assert_eq!(first.values(), &[Value::Int64(10)]);
    }

    #[test]
    fn relation_scan_honors_caller_row_limit_for_snapshot_and_batch() {
        let fixture = phase_4a_large_relation_fixture();
        let mut snapshot = fixture.pager().begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let relation = Relation::open(&catalog, fixture.table_id()).unwrap();

        let mut sufficient = relation.scan(LARGE_ROW_BYTES);
        assert_eq!(
            sufficient
                .next_snapshot(&mut snapshot)
                .unwrap()
                .unwrap()
                .values(),
            &[large_blob_value()]
        );
        let mut too_small = relation.scan(LARGE_ROW_BYTES - 1);
        assert_eq!(
            too_small
                .next_snapshot(&mut snapshot)
                .err()
                .expect("smaller snapshot limit must reject the row")
                .code(),
            ErrorCode::Type
        );
        drop(snapshot);

        let mut batch = fixture.pager().begin_write().unwrap();
        let catalog = batch.catalog_reader().unwrap();
        let relation = Relation::open(&catalog, fixture.table_id()).unwrap();
        let mut sufficient = relation.scan(LARGE_ROW_BYTES);
        assert_eq!(
            sufficient.next_batch(&mut batch).unwrap().unwrap().values(),
            &[large_blob_value()]
        );
        let mut too_small = relation.scan(LARGE_ROW_BYTES - 1);
        assert_eq!(
            too_small
                .next_batch(&mut batch)
                .err()
                .expect("smaller batch limit must reject the row")
                .code(),
            ErrorCode::Type
        );
        batch.rollback().unwrap();
    }

    struct Phase4aRelationFixture {
        _temp: TempDatabase,
        pager: Pager,
        table_id: TableId,
    }

    impl Phase4aRelationFixture {
        fn pager(&self) -> &Pager {
            &self.pager
        }

        const fn table_id(&self) -> TableId {
            self.table_id
        }
    }

    fn phase_4a_relation_fixture() -> Phase4aRelationFixture {
        let (temp, pager) = test_pager(4096).expect("test pager");
        let table_id = write_relation_fixture(&pager, ColumnType::Int64, Value::Int64(10), 16_384)
            .expect("write relation fixture");
        Phase4aRelationFixture {
            _temp: temp,
            pager,
            table_id,
        }
    }

    fn phase_4a_large_relation_fixture() -> Phase4aRelationFixture {
        let (temp, pager) = test_pager(4096).expect("test pager");
        let table_id = write_relation_fixture(
            &pager,
            ColumnType::Blob,
            large_blob_value(),
            LARGE_ROW_BYTES,
        )
        .expect("write large relation fixture");
        Phase4aRelationFixture {
            _temp: temp,
            pager,
            table_id,
        }
    }

    fn large_blob_value() -> Value {
        Value::Blob(vec![0x5a; LARGE_BLOB_BYTES])
    }

    fn table_spec(name: &str, columns: &[(&str, ColumnType, bool)]) -> TableSpec {
        TableSpec::new(
            CatalogName::new(name).unwrap(),
            columns
                .iter()
                .map(|(name, column_type, nullable)| {
                    ColumnSpec::new(CatalogName::new(*name).unwrap(), *column_type, *nullable)
                })
                .collect(),
        )
        .unwrap()
    }

    fn committed_relation(pager: &Pager, spec: TableSpec, rows: &[Vec<Value>]) -> TableId {
        let mut batch = pager.try_begin_write().unwrap();
        let table_id = create_relation(&mut batch, &spec).unwrap();
        if !rows.is_empty() {
            let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
            for values in rows {
                writer.insert(&mut batch, values).unwrap();
            }
            writer.finish(&mut batch).unwrap();
        }
        batch.commit().unwrap();
        table_id
    }

    fn read_relation(pager: &Pager, name: &str) -> Option<RelationContents> {
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let table = catalog
            .find_table_by_name(&CatalogName::new(name).unwrap())?
            .clone();
        let relation = Relation::open(&catalog, table.id()).unwrap();
        let mut scan = relation.scan(usize::MAX);
        let mut rows = Vec::new();
        while let Some(row) = scan.next_snapshot(&mut snapshot).unwrap() {
            rows.push((row.rowid().get(), row.values().to_vec()));
        }
        Some((table, rows))
    }

    fn write_relation_fixture(
        pager: &Pager,
        column_type: ColumnType,
        value: Value,
        max_row_bytes: usize,
    ) -> Result<TableId> {
        let mut batch = pager.begin_write()?;
        let mut catalog = CatalogWriter::bootstrap(&mut batch)?;
        let table_id = TableId::new(catalog.allocate_object_id(&mut batch)?.get())?;
        let mut rows = BTree::create(&mut batch, TreeKind::Table)?;
        let table = TableDef::new(
            table_id,
            SchemaVersion::new(1)?,
            rows.root(),
            RowId::new(1)?,
            1,
            CatalogName::new("T")?,
        )?;
        catalog.insert_table_definition(&mut batch, &table)?;
        let column = ColumnDef::new(
            ColumnId::new(catalog.allocate_object_id(&mut batch)?.get())?,
            table_id,
            0,
            column_type,
            false,
            CatalogName::new("N")?,
        )?;
        catalog.insert_column_definition(&mut batch, &column)?;
        let rowid = catalog.allocate_rowid(&mut batch, table_id)?;
        let bytes = RowCodec::encode(&table, &[column], &[value], max_row_bytes)?;
        rows.insert(&mut batch, &encode_rowid_key(rowid), &bytes)?;
        catalog.update_table_root(&mut batch, table_id, rows.root())?;
        catalog.finish(&mut batch)?;
        batch.commit()?;
        Ok(table_id)
    }
}
