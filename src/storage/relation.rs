use std::ops::Bound;

use crate::{DbError, ErrorCode, Result, Value};

use super::{
    ReadSnapshot, WriteBatch,
    btree::{BTree, BTreeScanState, TreeKind},
    catalog::{
        CatalogReader,
        types::{ColumnDef, RowId, TableDef, TableId},
    },
    pager::PageSource,
    row::{RowCodec, decode_rowid_key},
};

#[derive(Clone)]
pub(crate) struct Relation {
    table: TableDef,
    columns: Box<[ColumnDef]>,
}

pub(crate) struct RelationScan {
    relation: Relation,
    state: BTreeScanState,
    max_row_bytes: usize,
}

pub(crate) struct StoredRow {
    rowid: RowId,
    values: Box<[Value]>,
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

    pub(crate) fn scan(&self, max_row_bytes: usize) -> RelationScan {
        let rows = BTree::from_root(TreeKind::Table, self.table.root());
        let state = rows
            .scan_state(Bound::Unbounded, Bound::Unbounded)
            .expect("unbounded relation scan bounds are always valid");
        RelationScan {
            relation: self.clone(),
            state,
            max_row_bytes,
        }
    }
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
        let Some((key, bytes)) = self.state.next(source)? else {
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
}

#[cfg(test)]
mod tests {
    use super::Relation;
    use crate::storage::{
        Pager,
        btree::{BTree, TreeKind},
        catalog::{
            CatalogWriter,
            types::{
                CatalogName, ColumnDef, ColumnId, ColumnType, RowId, SchemaVersion, TableDef,
                TableId,
            },
        },
        row::{RowCodec, encode_rowid_key},
        test_support::{TempDatabase, test_pager},
    };
    use crate::{ErrorCode, Result, Value};

    const LARGE_BLOB_BYTES: usize = 20_000;
    const LARGE_ROW_BYTES: usize = 20_041;

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
