pub(crate) mod key;

use crate::storage::{
    PageRef, WriteBatch,
    btree::{BTree, TreeKind},
    catalog::{
        CatalogReader, CatalogWriter,
        types::{
            CatalogName, ColumnDef, ColumnId, IndexDef, IndexId, IndexOwnership, MAX_COLUMNS,
            ObjectKind, RowId, TableId,
        },
    },
    relation::Relation,
};
use crate::{DbError, ErrorCode, Result, Value};
use key::IndexKeyCodec;

pub(crate) struct IndexSpec {
    name: CatalogName,
    table_id: TableId,
    columns: Box<[ColumnId]>,
    unique: bool,
}

impl IndexSpec {
    pub(crate) fn new(
        name: CatalogName,
        table_id: TableId,
        columns: Vec<ColumnId>,
        unique: bool,
    ) -> Result<Self> {
        if columns.is_empty() || columns.len() > usize::from(MAX_COLUMNS) {
            return Err(DbError::new(
                ErrorCode::Type,
                "index column count must be between 1 and 1000",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        if columns.iter().any(|column| !seen.insert(*column)) {
            return Err(DbError::new(
                ErrorCode::Type,
                "index column list contains a repeated column",
            ));
        }
        Ok(Self {
            name,
            table_id,
            columns: columns.into_boxed_slice(),
            unique,
        })
    }
}

pub(crate) fn create_index(batch: &mut WriteBatch, spec: &IndexSpec) -> Result<IndexId> {
    // Clone the current relation and ordered column definitions before opening
    // the write gate. No retained reader or plan contains a physical root.
    let (relation, columns) = {
        let catalog = batch.catalog_reader()?;
        if catalog.object_name_exists(ObjectKind::Index, &spec.name) {
            return Err(DbError::new(
                ErrorCode::SchemaChanged,
                "prepared CREATE INDEX name is no longer available",
            ));
        }
        let relation = Relation::open(&catalog, spec.table_id)?;
        let columns = spec
            .columns
            .iter()
            .map(|id| {
                catalog
                    .find_column_by_id(*id)
                    .filter(|column| column.table_id() == spec.table_id)
                    .cloned()
                    .ok_or_else(|| {
                        DbError::new(ErrorCode::SchemaChanged, "indexed column ownership changed")
                    })
            })
            .collect::<Result<Box<[_]>>>()?;
        (relation, columns)
    };
    let checkpoint = batch.statement_checkpoint()?;
    let result = (|| {
        let mut catalog = CatalogWriter::bootstrap(batch)?;
        let id = IndexId::new(catalog.allocate_object_id(batch)?.get())?;
        let tree = BTree::create(batch, TreeKind::Index)?;
        let definition = IndexDef::new(
            id,
            spec.table_id,
            tree.root(),
            spec.columns.len() as u16,
            spec.unique,
            IndexOwnership::Explicit,
            spec.name.clone(),
        )?;
        let mut storage = IndexStorage::from_new(
            IndexSchema {
                definition,
                columns,
            },
            tree,
        );
        // ROWID order, one decoded row and one physical key at a time. There is
        // no retained key collection, sort, or spill file; NULLs are inserted too.
        let mut scan = relation.scan(usize::MAX);
        while let Some(row) = scan.next_batch(batch)? {
            let entry = storage.prepare(row.values(), row.rowid())?;
            storage.check_unique(batch, &entry, None)?;
            storage.insert(batch, &entry)?;
        }
        let definition = IndexDef::new(
            id,
            spec.table_id,
            storage.root(),
            spec.columns.len() as u16,
            spec.unique,
            IndexOwnership::Explicit,
            spec.name.clone(),
        )?;
        // This writes all four record kinds and enables the feature in the
        // gated candidate. Neither is published until the version bump and
        // writer finish succeed; the outer scope restores all on any failure.
        catalog.insert_index_definition(batch, &definition, &spec.columns)?;
        catalog.bump_table_schema_version(batch, spec.table_id)?;
        catalog.finish(batch)?;
        Ok(id)
    })();
    match result {
        Ok(id) => {
            batch.finish_statement(checkpoint)?;
            Ok(id)
        }
        Err(error) => {
            batch.restore_statement(checkpoint);
            Err(error)
        }
    }
}

#[derive(Clone)]
pub(crate) struct IndexSchema {
    definition: IndexDef,
    // Index-key order; each definition retains its stable ID and table ordinal.
    columns: Box<[ColumnDef]>,
}

pub(crate) struct PreparedIndexEntry {
    physical: Vec<u8>,
    logical_len: usize,
    contains_null: bool,
    rowid: RowId,
}

/// Batch-local facade with no retained page-source borrow. After restoring a
/// statement checkpoint, discard this facade and reopen it from the catalog.
/// Callers open their catalog writer before mutating the tree (collapse can
/// release the old root), then publish via CatalogWriter::update_index_root.
pub(crate) struct IndexStorage {
    schema: IndexSchema,
    tree: BTree,
    original_root: PageRef,
}

impl IndexStorage {
    pub(crate) fn open(catalog: &CatalogReader, index: IndexId) -> Result<Self> {
        let definition = catalog.find_index_by_id(index).cloned().ok_or_else(|| {
            DbError::new(
                ErrorCode::SchemaChanged,
                format!("index ID {} does not exist", index.get()),
            )
        })?;
        let tree = BTree::from_root(TreeKind::Index, definition.root());
        let columns = catalog
            .columns_for_index(index)
            .cloned()
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self::from_new(
            IndexSchema {
                definition,
                columns,
            },
            tree,
        ))
    }

    pub(in crate::storage) fn from_new(schema: IndexSchema, tree: BTree) -> Self {
        let original_root = tree.root();
        Self {
            schema,
            tree,
            original_root,
        }
    }

    /// Values are the complete table row, not a preselected index-key tuple.
    /// Preparation (including the physical key size limit) never mutates pages.
    pub(crate) fn prepare(&self, values: &[Value], rowid: RowId) -> Result<PreparedIndexEntry> {
        let selected = self
            .schema
            .columns
            .iter()
            .map(|column| {
                values
                    .get(usize::from(column.ordinal()))
                    .cloned()
                    .ok_or_else(|| {
                        DbError::new(ErrorCode::Type, "row is missing an indexed column")
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let encoded = IndexKeyCodec::encode(&self.schema.columns, &selected, rowid)?;
        Ok(PreparedIndexEntry {
            physical: encoded.physical().to_vec(),
            logical_len: encoded.logical().len(),
            contains_null: encoded.contains_null(),
            rowid: encoded.rowid(),
        })
    }

    pub(crate) fn check_unique(
        &self,
        batch: &mut WriteBatch,
        entry: &PreparedIndexEntry,
        exclude: Option<RowId>,
    ) -> Result<()> {
        if !self.schema.definition.unique() || entry.contains_null {
            return Ok(());
        }
        let logical = &entry.physical[..entry.logical_len];
        let (start, end) = IndexKeyCodec::prefix_bounds(logical);
        let mut scan = self.tree.scan_state(
            start.as_ref().map(Vec::as_slice),
            end.as_ref().map(Vec::as_slice),
        )?;
        while let Some((physical, value)) = scan.next(batch)? {
            if !value.is_empty() {
                return Err(corruption("index leaf value must be empty"));
            }
            let found = IndexKeyCodec::decode(&self.schema.columns, &physical)?.rowid();
            if Some(found) != exclude {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    format!(
                        "unique index {} is violated",
                        self.schema.definition.name().as_str()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Logical uniqueness is a separate probe; every row, including NULLs,
    /// receives a physical key whose ROWID makes it unique.
    pub(crate) fn insert(
        &mut self,
        batch: &mut WriteBatch,
        entry: &PreparedIndexEntry,
    ) -> Result<()> {
        self.tree
            .insert(batch, &entry.physical, &[])
            .map_err(|error| {
                if error.code() == ErrorCode::Constraint {
                    corruption("duplicate physical index key")
                } else {
                    error
                }
            })
    }

    pub(crate) fn delete(
        &mut self,
        batch: &mut WriteBatch,
        entry: &PreparedIndexEntry,
    ) -> Result<()> {
        // Inspect first: corruption must not delete the offending value. The
        // batch is exclusively borrowed throughout the exact lookup/deletion.
        match self.tree.get(batch, &entry.physical)? {
            Some(value) if value.is_empty() => {}
            Some(_) => return Err(corruption("index leaf value must be empty")),
            None => return Err(corruption("expected physical index key is missing")),
        }
        match self.tree.delete(batch, &entry.physical)? {
            Some(value) if value.is_empty() => Ok(()),
            Some(_) => Err(corruption("index leaf value must be empty")),
            None => Err(corruption("expected physical index key is missing")),
        }
    }

    pub(crate) const fn root(&self) -> PageRef {
        self.tree.root()
    }
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::{IndexSpec, IndexStorage, create_index, key::IndexKeyCodec};
    use crate::storage::{
        OpenMode, Pager, PagerConfig, WriteBatch,
        btree::{BTree, TreeKind},
        catalog::{
            CatalogWriter,
            types::{
                CatalogName, ColumnDef, ColumnId, ColumnType, IndexDef, IndexId, IndexOwnership,
                RowId, SchemaVersion, TableDef, TableId,
            },
        },
        features::{CATALOG_V1, INDEX_CONSTRAINT_V1},
        relation::{ColumnSpec, TableSpec, TableWrite, create_relation},
        test_support::{TempDatabase, test_pager},
    };
    use crate::{ErrorCode, Result, Value};

    fn create_fixture(
        types: &[ColumnType],
        version: u64,
        rows: &[Vec<Value>],
    ) -> (TempDatabase, Pager, WriteBatch, TableId, Vec<ColumnId>) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut catalog = CatalogWriter::bootstrap(&mut batch).unwrap();
        let table_id = TableId::new(catalog.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let table = TableDef::new(
            table_id,
            SchemaVersion::new(version).unwrap(),
            BTree::create(&mut batch, TreeKind::Table).unwrap().root(),
            RowId::new(1).unwrap(),
            types.len() as u16,
            CatalogName::new("T").unwrap(),
        )
        .unwrap();
        catalog.insert_table_definition(&mut batch, &table).unwrap();
        let mut ids = Vec::new();
        for (ordinal, ty) in types.iter().enumerate() {
            let id = ColumnId::new(catalog.allocate_object_id(&mut batch).unwrap().get()).unwrap();
            let column = ColumnDef::new(
                id,
                table_id,
                ordinal as u16,
                *ty,
                true,
                CatalogName::new(format!("C{ordinal}")).unwrap(),
            )
            .unwrap();
            catalog
                .insert_column_definition(&mut batch, &column)
                .unwrap();
            ids.push(id);
        }
        catalog.finish(&mut batch).unwrap();
        let mut writer = TableWrite::open(&mut batch, table_id).unwrap();
        for row in rows {
            writer.insert(&mut batch, row).unwrap();
        }
        writer.finish(&mut batch).unwrap();
        (temp, pager, batch, table_id, ids)
    }

    #[test]
    fn create_index_persists_column_order_and_every_null_row_after_reopen() {
        // Excluding NULL rows or using table rather than declared key order
        // changes these independently specified physical leaf ROWIDs.
        let rows = vec![
            vec![Value::Int64(1), Value::Int64(2)],
            vec![Value::Int64(2), Value::Int64(1)],
            vec![Value::Null, Value::Null],
            vec![Value::Null, Value::Null],
            vec![Value::Int64(1), Value::Null],
            vec![Value::Int64(1), Value::Null],
            vec![Value::Null, Value::Int64(2)],
            vec![Value::Null, Value::Int64(2)],
        ];
        let (temp, pager, mut batch, table, ids) =
            create_fixture(&[ColumnType::Int64, ColumnType::Int64], 1, &rows);
        assert_eq!(batch.feature_bits(), CATALOG_V1);
        let columns = vec![ids[1], ids[0]];
        let spec = IndexSpec::new(
            CatalogName::new("UX_T").unwrap(),
            table,
            columns.clone(),
            true,
        )
        .unwrap();
        let index = create_index(&mut batch, &spec).unwrap();
        assert_eq!(batch.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
        batch.commit().unwrap();
        drop(pager);
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        catalog.validate_deep(&mut snapshot).unwrap();
        assert_eq!(snapshot.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
        assert_eq!(
            catalog
                .find_table_by_id(table)
                .unwrap()
                .schema_version()
                .get(),
            2
        );
        assert_eq!(
            catalog
                .columns_for_index(index)
                .map(ColumnDef::id)
                .collect::<Vec<_>>(),
            columns
        );
        let storage = IndexStorage::open(&catalog, index).unwrap();
        let mut scan = storage
            .tree
            .scan_state(Bound::Unbounded, Bound::Unbounded)
            .unwrap();
        for rowid in [2, 1, 7, 8, 5, 6, 3, 4] {
            let (key, value) = scan.next(&mut snapshot).unwrap().unwrap();
            assert!(value.is_empty());
            let decoded = IndexKeyCodec::decode(&storage.schema.columns, &key).unwrap();
            assert_eq!(decoded.rowid().get(), rowid);
        }
        assert!(scan.next(&mut snapshot).unwrap().is_none());
    }

    #[test]
    fn create_index_late_duplicate_restores_all_batch_state_and_reuses_id() {
        // The duplicate comes after root splits; restoration must include dirty
        // bytes, allocations/releases, roots, catalog gate, counter and bits.
        let mut rows = (0..240).map(|n| vec![Value::Int64(n)]).collect::<Vec<_>>();
        rows.push(vec![Value::Int64(0)]);
        let (_temp, _pager, mut batch, table, ids) = create_fixture(&[ColumnType::Int64], 1, &rows);
        let before = batch.checkpoint_mutation().unwrap();
        let spec =
            IndexSpec::new(CatalogName::new("UX_T").unwrap(), table, ids.clone(), true).unwrap();
        let error = create_index(&mut batch, &spec).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Constraint);
        assert!(error.to_string().contains("UX_T"));
        before.assert_matches_batch_for_test(&batch);
        let catalog = batch.catalog_reader().unwrap();
        assert!(catalog.find_index_by_name(&spec.name).is_none());
        assert_eq!(
            catalog
                .find_table_by_id(table)
                .unwrap()
                .schema_version()
                .get(),
            1
        );
        catalog.validate_deep_batch(&mut batch).unwrap();
        let spec = IndexSpec::new(spec.name, table, ids, false).unwrap();
        let index = create_index(&mut batch, &spec).unwrap();
        assert_eq!(index.get(), 3, "failed build must not consume object ID 3");
        let catalog = batch.catalog_reader().unwrap();
        let storage = IndexStorage::open(&catalog, index).unwrap();
        let mut scan = storage
            .tree
            .scan_state(Bound::Unbounded, Bound::Unbounded)
            .unwrap();
        let mut count = 0;
        while scan.next(&mut batch).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 241);
        catalog.validate_deep_batch(&mut batch).unwrap();
        let before_second = batch.checkpoint_mutation().unwrap();
        let second = IndexSpec::new(
            CatalogName::new("UX_SECOND").unwrap(),
            table,
            spec.columns.to_vec(),
            true,
        )
        .unwrap();
        assert_eq!(
            create_index(&mut batch, &second).unwrap_err().code(),
            ErrorCode::Constraint
        );
        before_second.assert_matches_batch_for_test(&batch);
        assert_eq!(batch.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
        batch.commit().unwrap();
    }

    #[test]
    fn create_index_nonfinite_type_failure_restores_all_batch_state() {
        // Row storage preserves all Float64 bits; the index codec must reject
        // nonfinite values after earlier entries without publishing a partial tree.
        let rows = vec![vec![Value::Float64(1.0)], vec![Value::Float64(f64::NAN)]];
        let (_temp, _pager, mut batch, table, ids) =
            create_fixture(&[ColumnType::Float64], 1, &rows);
        let before = batch.checkpoint_mutation().unwrap();
        let spec = IndexSpec::new(CatalogName::new("IX_T").unwrap(), table, ids, false).unwrap();
        assert_eq!(
            create_index(&mut batch, &spec).unwrap_err().code(),
            ErrorCode::Type
        );
        before.assert_matches_batch_for_test(&batch);
        assert_eq!(batch.feature_bits(), CATALOG_V1);
        batch
            .catalog_reader()
            .unwrap()
            .validate_deep_batch(&mut batch)
            .unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn create_index_oversized_key_restores_all_batch_state() {
        // Failure after one successful insert must be just as atomic as an
        // oversized first key, including the candidate feature flag.
        let rows = vec![
            vec![Value::Text("small".into())],
            vec![Value::Text("x".repeat(1014).into())],
        ];
        let (_temp, _pager, mut batch, table, ids) = create_fixture(&[ColumnType::Text], 1, &rows);
        let before = batch.checkpoint_mutation().unwrap();
        let spec = IndexSpec::new(CatalogName::new("IX_T").unwrap(), table, ids, false).unwrap();
        assert_eq!(
            create_index(&mut batch, &spec).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
        before.assert_matches_batch_for_test(&batch);
        assert_eq!(batch.feature_bits(), CATALOG_V1);
        batch
            .catalog_reader()
            .unwrap()
            .validate_deep_batch(&mut batch)
            .unwrap();
        let mut writer = TableWrite::open(&mut batch, table).unwrap();
        writer
            .insert(&mut batch, &[Value::Text("after".into())])
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn create_index_schema_bump_failure_restores_published_candidate_records_and_bit() {
        // Unlike duplicate/key failures, schema overflow occurs after all four
        // record kinds and INDEX_CONSTRAINT_V1 were staged by CatalogWriter.
        let rows = (0..240).map(|n| vec![Value::Int64(n)]).collect::<Vec<_>>();
        let (_temp, _pager, mut batch, table, ids) =
            create_fixture(&[ColumnType::Int64], u64::MAX, &rows);
        let before = batch.checkpoint_mutation().unwrap();
        let spec = IndexSpec::new(CatalogName::new("IX_T").unwrap(), table, ids, false).unwrap();
        assert_eq!(
            create_index(&mut batch, &spec).unwrap_err().code(),
            ErrorCode::Type
        );
        before.assert_matches_batch_for_test(&batch);
        assert_eq!(batch.feature_bits(), CATALOG_V1);
        let catalog = batch.catalog_reader().unwrap();
        assert!(catalog.find_index_by_name(&spec.name).is_none());
        assert_eq!(
            catalog
                .find_table_by_id(table)
                .unwrap()
                .schema_version()
                .get(),
            u64::MAX
        );
        catalog.validate_deep_batch(&mut batch).unwrap();
        let mut writer = TableWrite::open(&mut batch, table).unwrap();
        writer.insert(&mut batch, &[Value::Int64(241)]).unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn create_index_rejects_invalid_specs_and_foreign_columns_without_mutation() {
        // Invalid shape/ownership is rejected before taking the catalog gate.
        let (_temp, _pager, mut batch, table, ids) = create_fixture(&[ColumnType::Int64], 1, &[]);
        let name = CatalogName::new("IX_T").unwrap();
        for columns in [vec![], vec![ids[0], ids[0]], vec![ids[0]; 1001]] {
            assert!(
                matches!(IndexSpec::new(name.clone(), table, columns, false), Err(error) if error.code() == ErrorCode::Type)
            );
        }
        let before = batch.checkpoint_mutation().unwrap();
        let spec = IndexSpec::new(name, table, vec![ColumnId::new(999).unwrap()], false).unwrap();
        assert_eq!(
            create_index(&mut batch, &spec).unwrap_err().code(),
            ErrorCode::SchemaChanged
        );
        before.assert_matches_batch_for_test(&batch);
        batch.catalog_reader().unwrap();
    }

    struct IndexFixture {
        storage: IndexStorage,
        batch: WriteBatch,
        pager: Pager,
        index: IndexId,
        _temp: TempDatabase,
    }

    fn writable_index_fixture(unique: bool, types: &[ColumnType]) -> IndexFixture {
        indexed_fixture(unique, types, &(0..types.len()).collect::<Vec<_>>())
    }

    fn indexed_fixture(unique: bool, types: &[ColumnType], ordinals: &[usize]) -> IndexFixture {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table = create_relation(
            &mut batch,
            &TableSpec::new(
                CatalogName::new("T").unwrap(),
                types
                    .iter()
                    .enumerate()
                    .map(|(ordinal, ty)| {
                        ColumnSpec::new(CatalogName::new(format!("C{ordinal}")).unwrap(), *ty, true)
                    })
                    .collect(),
            )
            .unwrap(),
        )
        .unwrap();
        let columns = batch
            .catalog_reader()
            .unwrap()
            .columns_for_table(table)
            .cloned()
            .collect::<Vec<_>>();
        let mut catalog = CatalogWriter::bootstrap(&mut batch).unwrap();
        let index = IndexId::new(catalog.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
        let definition = IndexDef::new(
            index,
            table,
            tree.root(),
            ordinals.len() as u16,
            unique,
            IndexOwnership::Explicit,
            CatalogName::new("IX_T").unwrap(),
        )
        .unwrap();
        catalog
            .insert_index_definition(
                &mut batch,
                &definition,
                &ordinals
                    .iter()
                    .map(|ordinal| columns[*ordinal].id())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        catalog.finish(&mut batch).unwrap();
        let storage = IndexStorage::open(&batch.catalog_reader().unwrap(), index).unwrap();
        IndexFixture {
            storage,
            batch,
            pager,
            index,
            _temp: temp,
        }
    }

    impl IndexFixture {
        fn insert(&mut self, values: &[Value], rowid: u64) -> Result<()> {
            let entry = self.storage.prepare(values, RowId::new(rowid)?)?;
            self.storage.check_unique(&mut self.batch, &entry, None)?;
            self.storage.insert(&mut self.batch, &entry)
        }

        fn probe(&mut self, values: &[Value], exclude: Option<RowId>) -> Result<()> {
            let entry = self.storage.prepare(values, RowId::new(999).unwrap())?;
            self.storage.check_unique(&mut self.batch, &entry, exclude)
        }

        fn rowids(&mut self) -> Vec<u64> {
            // Inspect physical leaves, not a logical/NULL-filtered facade view.
            let mut scan = self
                .storage
                .tree
                .scan_state(Bound::Unbounded, Bound::Unbounded)
                .unwrap();
            let mut rowids = Vec::new();
            while let Some((physical, value)) = scan.next(&mut self.batch).unwrap() {
                assert!(value.is_empty());
                rowids.push(
                    IndexKeyCodec::decode(&self.storage.schema.columns, &physical)
                        .unwrap()
                        .rowid()
                        .get(),
                );
            }
            rowids
        }

        fn publish_root(&mut self) {
            let mut catalog = CatalogWriter::bootstrap(&mut self.batch).unwrap();
            catalog
                .update_index_root(&mut self.batch, self.index, self.storage.root())
                .unwrap();
            catalog.finish(&mut self.batch).unwrap();
        }

        fn reopen_storage(&mut self) -> IndexStorage {
            IndexStorage::open(&self.batch.catalog_reader().unwrap(), self.index).unwrap()
        }

        fn deep_validate(&mut self) {
            self.batch
                .catalog_reader()
                .unwrap()
                .validate_deep_batch(&mut self.batch)
                .unwrap();
        }
    }

    #[test]
    fn index_storage_records_every_row_and_enforces_only_complete_unique_keys() {
        let mut fixture = writable_index_fixture(true, &[ColumnType::Int64, ColumnType::Text]);
        fixture
            .insert(&[Value::Int64(1), Value::Text("A".into())], 1)
            .unwrap();
        assert_eq!(
            fixture
                .insert(&[Value::Int64(1), Value::Text("A".into())], 2)
                .unwrap_err()
                .code(),
            ErrorCode::Constraint
        );
        fixture.insert(&[Value::Int64(1), Value::Null], 3).unwrap();
        fixture.insert(&[Value::Int64(1), Value::Null], 4).unwrap();
        assert_eq!(fixture.rowids(), vec![1, 3, 4]);
    }

    #[test]
    fn update_unique_probe_excludes_only_its_own_rowid() {
        let mut fixture = writable_index_fixture(true, &[ColumnType::Int64]);
        fixture.insert(&[Value::Int64(7)], 1).unwrap();
        assert!(
            fixture
                .probe(&[Value::Int64(7)], Some(RowId::new(1).unwrap()))
                .is_ok()
        );
        assert_eq!(
            fixture
                .probe(&[Value::Int64(7)], Some(RowId::new(2).unwrap()))
                .unwrap_err()
                .code(),
            ErrorCode::Constraint
        );
        // Excluding one row must not hide a second row in the same prefix.
        let second = fixture
            .storage
            .prepare(&[Value::Int64(7)], RowId::new(2).unwrap())
            .unwrap();
        fixture.storage.insert(&mut fixture.batch, &second).unwrap();
        assert_eq!(
            fixture
                .probe(&[Value::Int64(7)], Some(RowId::new(1).unwrap()))
                .unwrap_err()
                .code(),
            ErrorCode::Constraint
        );
        assert!(fixture.probe(&[Value::Int64(6)], None).is_ok());
        assert!(fixture.probe(&[Value::Int64(8)], None).is_ok());
    }

    #[test]
    fn ordinary_index_retains_duplicates_and_all_null_rows() {
        let mut fixture = writable_index_fixture(false, &[ColumnType::Int64]);
        for (value, rowid) in [
            (Value::Int64(7), 1),
            (Value::Int64(7), 2),
            (Value::Null, 3),
            (Value::Null, 4),
        ] {
            fixture.insert(&[value], rowid).unwrap();
        }
        assert_eq!(fixture.rowids(), vec![1, 2, 3, 4]);
        fixture.deep_validate();
    }

    #[test]
    fn unique_index_retains_all_null_keys_and_rejects_malformed_stored_rowid() {
        let mut fixture = writable_index_fixture(true, &[ColumnType::Int64]);
        fixture.insert(&[Value::Null], 1).unwrap();
        fixture.insert(&[Value::Null], 2).unwrap();
        assert_eq!(fixture.rowids(), vec![1, 2]);
        let entry = fixture
            .storage
            .prepare(&[Value::Int64(7)], RowId::new(3).unwrap())
            .unwrap();
        let mut malformed = entry.physical;
        malformed[entry.logical_len..].fill(0);
        fixture
            .storage
            .tree
            .insert(&mut fixture.batch, &malformed, &[])
            .unwrap();
        assert_eq!(
            fixture.probe(&[Value::Int64(7)], None).unwrap_err().code(),
            ErrorCode::Corruption
        );
        // The probe is bounded: unrelated malformed prefixes do not interfere.
        assert!(fixture.probe(&[Value::Int64(8)], None).is_ok());
    }

    #[test]
    fn open_missing_index_is_schema_changed_and_new_tree_can_be_built_before_publication() {
        let mut fixture = writable_index_fixture(false, &[ColumnType::Int64]);
        let catalog = fixture.batch.catalog_reader().unwrap();
        assert_eq!(
            IndexStorage::open(&catalog, IndexId::new(999).unwrap())
                .err()
                .unwrap()
                .code(),
            ErrorCode::SchemaChanged
        );
        let tree = BTree::create(&mut fixture.batch, TreeKind::Index).unwrap();
        let root = tree.root();
        let schema = fixture.storage.schema.clone();
        let mut storage = IndexStorage::from_new(schema, tree);
        assert_eq!(storage.original_root, root);
        let entry = storage
            .prepare(&[Value::Int64(7)], RowId::new(3).unwrap())
            .unwrap();
        storage.insert(&mut fixture.batch, &entry).unwrap();
        fixture.batch.require_tracked_index_root(root).unwrap();
        assert_eq!(
            storage
                .tree
                .get(&mut fixture.batch, &entry.physical)
                .unwrap(),
            Some(vec![])
        );
        assert!(fixture.rowids().is_empty());
    }

    #[test]
    fn prepare_selects_index_order_by_table_ordinal_and_limits_before_mutation() {
        let mut fixture = indexed_fixture(
            true,
            &[ColumnType::Text, ColumnType::Bool, ColumnType::Int64],
            &[2, 0],
        );
        let entry = fixture
            .storage
            .prepare(
                &[Value::Text("A".into()), Value::Bool(true), Value::Int64(7)],
                RowId::new(9).unwrap(),
            )
            .unwrap();
        assert_eq!(entry.rowid, RowId::new(9).unwrap());
        assert_eq!(entry.logical_len + 8, entry.physical.len());
        assert!(!entry.contains_null);
        let decoded =
            IndexKeyCodec::decode(&fixture.storage.schema.columns, &entry.physical).unwrap();
        assert_eq!(
            decoded.values(),
            &[Value::Int64(7), Value::Text("A".into())]
        );
        assert_eq!(
            fixture
                .storage
                .prepare(&[Value::Text("A".into())], RowId::new(1).unwrap())
                .err()
                .unwrap()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            fixture
                .insert(
                    &[
                        Value::Text("A".repeat(1100)),
                        Value::Bool(false),
                        Value::Int64(7)
                    ],
                    1
                )
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        assert!(fixture.rowids().is_empty());
        fixture.deep_validate();
    }

    #[test]
    fn physical_duplicate_missing_delete_and_nonempty_values_are_corruption() {
        let mut fixture = writable_index_fixture(false, &[ColumnType::Int64]);
        fixture.insert(&[Value::Int64(7)], 1).unwrap();
        fixture.insert(&[Value::Int64(7)], 2).unwrap();
        let entry = fixture
            .storage
            .prepare(&[Value::Int64(7)], RowId::new(1).unwrap())
            .unwrap();
        assert_eq!(
            fixture
                .storage
                .insert(&mut fixture.batch, &entry)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        fixture.storage.delete(&mut fixture.batch, &entry).unwrap();
        assert_eq!(fixture.rowids(), vec![2]);
        assert_eq!(
            fixture
                .storage
                .delete(&mut fixture.batch, &entry)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        fixture
            .storage
            .tree
            .insert(&mut fixture.batch, &entry.physical, b"bad")
            .unwrap();
        assert_eq!(
            fixture
                .storage
                .delete(&mut fixture.batch, &entry)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            fixture
                .storage
                .tree
                .get(&mut fixture.batch, &entry.physical)
                .unwrap(),
            Some(b"bad".to_vec())
        );

        let mut unique = writable_index_fixture(true, &[ColumnType::Int64]);
        let entry = unique
            .storage
            .prepare(&[Value::Int64(7)], RowId::new(1).unwrap())
            .unwrap();
        unique
            .storage
            .tree
            .insert(&mut unique.batch, &entry.physical, b"bad")
            .unwrap();
        assert_eq!(
            unique
                .probe(&[Value::Int64(7)], Some(RowId::new(1).unwrap()))
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn index_storage_root_changes_are_restorable_and_track_index_kind() {
        let mut fixture = writable_index_fixture(false, &[ColumnType::Int64]);
        let before = fixture.storage.root();
        let checkpoint = fixture.batch.statement_checkpoint().unwrap();
        for rowid in 1..=200 {
            fixture
                .insert(&[Value::Int64(rowid as i64)], rowid)
                .unwrap();
        }
        let split = fixture.storage.root();
        assert_ne!(split, before);
        fixture.batch.require_tracked_index_root(split).unwrap();
        assert_eq!(
            fixture
                .batch
                .require_tracked_table_root(split)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            fixture
                .batch
                .require_tracked_index_root(before)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        fixture.publish_root();
        fixture.deep_validate();
        fixture.batch.restore_statement(checkpoint);
        fixture.storage = fixture.reopen_storage();
        assert_eq!(fixture.storage.root(), before);
        fixture.batch.require_tracked_index_root(before).unwrap();
        assert_eq!(
            fixture
                .batch
                .require_tracked_index_root(split)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert!(fixture.rowids().is_empty());
        fixture.deep_validate();
    }

    #[test]
    fn collapsed_root_and_entries_restore_then_survive_commit() {
        let mut fixture = writable_index_fixture(true, &[ColumnType::Int64]);
        let leaf = fixture.storage.root();
        for rowid in 1..=200 {
            fixture
                .insert(&[Value::Int64(rowid as i64)], rowid)
                .unwrap();
        }
        let split = fixture.storage.root();
        fixture.publish_root();
        let checkpoint = fixture.batch.statement_checkpoint().unwrap();
        let mut catalog = CatalogWriter::bootstrap(&mut fixture.batch).unwrap();
        for rowid in (1..=200).rev() {
            let entry = fixture
                .storage
                .prepare(&[Value::Int64(rowid as i64)], RowId::new(rowid).unwrap())
                .unwrap();
            fixture.storage.delete(&mut fixture.batch, &entry).unwrap();
        }
        assert_eq!(fixture.storage.root(), leaf);
        fixture.batch.require_tracked_index_root(leaf).unwrap();
        catalog
            .update_index_root(&mut fixture.batch, fixture.index, leaf)
            .unwrap();
        catalog.finish(&mut fixture.batch).unwrap();
        fixture.deep_validate();
        assert!(fixture.rowids().is_empty());
        fixture.batch.restore_statement(checkpoint);
        fixture.storage = fixture.reopen_storage();
        assert_eq!(fixture.storage.root(), split);
        assert_eq!(fixture.rowids(), (1..=200).collect::<Vec<_>>());
        fixture.deep_validate();
        fixture.batch.commit().unwrap();
        drop(fixture.pager);
        let reopened = Pager::open(
            fixture._temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut snapshot = reopened.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        catalog.validate_deep(&mut snapshot).unwrap();
        let storage = IndexStorage::open(&catalog, fixture.index).unwrap();
        assert_eq!(storage.root(), split);
        let mut scan = storage
            .tree
            .scan_state(Bound::Unbounded, Bound::Unbounded)
            .unwrap();
        for expected in 1..=200 {
            let (physical, value) = scan.next(&mut snapshot).unwrap().unwrap();
            assert!(value.is_empty());
            assert_eq!(
                IndexKeyCodec::decode(&storage.schema.columns, &physical)
                    .unwrap()
                    .rowid()
                    .get(),
                expected
            );
        }
        assert!(scan.next(&mut snapshot).unwrap().is_none());
    }
}
