pub(crate) mod key;
pub(crate) mod scan;

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
    columns: Box<[ColumnDef]>,
    unique: bool,
}

impl IndexSpec {
    pub(crate) fn new(
        name: CatalogName,
        table_id: TableId,
        columns: Vec<ColumnDef>,
        unique: bool,
    ) -> Result<Self> {
        if columns.is_empty() || columns.len() > usize::from(MAX_COLUMNS) {
            return Err(DbError::new(
                ErrorCode::Type,
                "index column count must be between 1 and 1000",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        if columns.iter().any(|column| !seen.insert(column.id())) {
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

pub(crate) fn create_index(
    batch: &mut WriteBatch,
    spec: &IndexSpec,
    work_bytes: u64,
) -> Result<IndexId> {
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
            .map(|retained| {
                catalog
                    .find_column_by_id(retained.id())
                    .filter(|column| {
                        column.table_id() == spec.table_id
                            && column.table_id() == retained.table_id()
                            && column.ordinal() == retained.ordinal()
                            && column.column_type() == retained.column_type()
                            && column.nullable() == retained.nullable()
                    })
                    .cloned()
                    .ok_or_else(|| {
                        DbError::new(
                            ErrorCode::SchemaChanged,
                            "indexed column definition changed",
                        )
                    })
            })
            .collect::<Result<Box<[_]>>>()?;
        (relation, columns)
    };
    // Eight bounded key buffers cover encoding, prefix bounds/cursor copies,
    // a probed physical key and decoded key payload; Value bodies are separate.
    let key_scratch = 8 * key::INDEX_KEY_BUFFER_BYTES as u64
        + 256
        + columns.len() as u64 * std::mem::size_of::<Value>() as u64;
    let row_budget = work_bytes.checked_sub(key_scratch).ok_or_else(|| {
        DbError::new(
            ErrorCode::ResourceLimit,
            "index build scratch exceeds work memory",
        )
    })?;
    let mut scan = relation.scan_with_work_budget(row_budget)?;
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
                unique_violation: UniqueViolationName::ExplicitIndex(spec.name.clone()),
            },
            tree,
        );
        // ROWID order, one decoded row and one physical key at a time. There is
        // no retained key collection, sort, or spill file; NULLs are inserted too.
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
        let column_ids = spec
            .columns
            .iter()
            .map(ColumnDef::id)
            .collect::<Vec<ColumnId>>();
        catalog.insert_index_definition(batch, &definition, &column_ids)?;
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

/// Resolve only the retained identity. Names are never rebound during DROP.
pub(crate) fn drop_index(batch: &mut WriteBatch, index_id: IndexId) -> Result<()> {
    let checkpoint = batch.statement_checkpoint()?;
    let result = (|| {
        let (index, columns) = {
            let catalog = batch.catalog_reader()?;
            let index = catalog.find_index_by_id(index_id).cloned().ok_or_else(|| {
                DbError::new(ErrorCode::SchemaChanged, "prepared index no longer exists")
            })?;
            let owner_name = if let IndexOwnership::Constraint(owner_id) = index.ownership() {
                let owner = catalog.find_constraint_by_id(owner_id).ok_or_else(|| {
                    DbError::new(
                        ErrorCode::Corruption,
                        "constraint-owned index references a missing owner constraint",
                    )
                })?;
                Some(owner.name())
            } else {
                None
            };
            require_explicit_owner(&index, owner_name)?;
            Relation::open(&catalog, index.table_id())?;
            let columns = catalog
                .columns_for_index(index_id)
                .map(ColumnDef::id)
                .collect::<Vec<_>>();
            // Include relation/index descendants and all live/free overlaps
            // before deleting even the first catalog record or releasing a page.
            catalog.validate_deep_batch(batch)?;
            (index, columns)
        };
        let mut catalog = CatalogWriter::bootstrap(batch)?;
        catalog.remove_index(batch, &index, &columns)?;
        #[cfg(test)]
        after_index_release_for_test()?;
        catalog.bump_table_schema_version(batch, index.table_id())?;
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

pub(super) fn require_explicit_owner(
    index: &IndexDef,
    owner_name: Option<&CatalogName>,
) -> Result<()> {
    if index.ownership() != IndexOwnership::Explicit {
        let owner_name = owner_name.ok_or_else(|| {
            DbError::new(
                ErrorCode::Corruption,
                "constraint-owned index is missing its owner name",
            )
        })?;
        return Err(DbError::new(
            ErrorCode::Constraint,
            format!(
                "index {} is owned by constraint {} and cannot be dropped independently",
                index.name().as_str(),
                owner_name.as_str()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static INDEX_RELEASE_FAILURE: std::cell::Cell<Option<(usize, ErrorCode)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct IndexReleaseFaultGuard;

#[cfg(test)]
impl Drop for IndexReleaseFaultGuard {
    fn drop(&mut self) {
        INDEX_RELEASE_FAILURE.set(None);
    }
}

/// Inject after real catalog deletion and whole-tree staging, so tests exercise
/// restoration of the lifecycle checkpoint rather than a no-op error path.
#[cfg(test)]
pub(crate) fn fail_index_release_for_test(
    successful_releases: usize,
    code: ErrorCode,
) -> IndexReleaseFaultGuard {
    INDEX_RELEASE_FAILURE.set(Some((successful_releases, code)));
    IndexReleaseFaultGuard
}

#[cfg(test)]
pub(crate) fn corrupt_index_entry_for_test(
    batch: &mut WriteBatch,
    index_id: IndexId,
    table_values: &[Value],
    rowid: RowId,
    leaf_value: &[u8],
) -> Result<()> {
    let mut storage = IndexStorage::open(&batch.catalog_reader()?, index_id)?;
    let original_root = storage.root();
    let entry = storage.prepare(table_values, rowid)?;
    if storage.tree.delete(batch, &entry.physical)?.is_none() {
        return Err(DbError::new(
            ErrorCode::SchemaChanged,
            "test index entry disappeared",
        ));
    }
    storage.tree.insert(batch, &entry.physical, leaf_value)?;
    if storage.root() != original_root {
        return Err(DbError::new(
            ErrorCode::Internal,
            "test index corruption unexpectedly changed its root",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn after_index_release_for_test() -> Result<()> {
    INDEX_RELEASE_FAILURE.with(|failure| match failure.get() {
        Some((0, code)) => {
            failure.set(None);
            Err(DbError::new(code, "injected after index tree release"))
        }
        Some((remaining, code)) => {
            failure.set(Some((remaining - 1, code)));
            Ok(())
        }
        None => Ok(()),
    })
}

#[derive(Clone)]
pub(crate) struct IndexSchema {
    definition: IndexDef,
    // Index-key order; each definition retains its stable ID and table ordinal.
    columns: Box<[ColumnDef]>,
    unique_violation: UniqueViolationName,
}

#[derive(Clone)]
enum UniqueViolationName {
    ExplicitIndex(CatalogName),
    Constraint(CatalogName),
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
        let unique_violation = match definition.ownership() {
            IndexOwnership::Explicit => {
                UniqueViolationName::ExplicitIndex(definition.name().clone())
            }
            IndexOwnership::Constraint(owner_id) => {
                let owner = catalog
                    .find_constraint_by_id(owner_id)
                    .ok_or_else(|| corruption("constraint-owned index has no owner constraint"))?;
                if owner.index_id() != Some(index) {
                    return Err(corruption(
                        "constraint-owned index does not match its owner constraint",
                    ));
                }
                UniqueViolationName::Constraint(owner.name().clone())
            }
        };
        Ok(Self::from_new(
            IndexSchema {
                definition,
                columns,
                unique_violation,
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
        let selected = self.schema.columns.iter().map(|column| {
            values
                .get(usize::from(column.ordinal()))
                .ok_or_else(|| DbError::new(ErrorCode::Type, "row is missing an indexed column"))
        });
        let encoded = IndexKeyCodec::encode_borrowed(&self.schema.columns, selected, rowid)?;
        let logical_len = encoded.logical().len();
        let contains_null = encoded.contains_null();
        let rowid = encoded.rowid();
        Ok(PreparedIndexEntry {
            physical: encoded.into_physical(),
            logical_len,
            contains_null,
            rowid,
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
                return Err(self.unique_violation());
            }
        }
        Ok(())
    }

    pub(in crate::storage) fn contains_key(
        &self,
        source: &mut impl super::pager::PageSource,
        key_values: &[Value],
    ) -> Result<bool> {
        self.contains_prefix(source, self.schema.columns.len(), key_values)
    }

    pub(in crate::storage) fn contains_prefix(
        &self,
        source: &mut impl super::pager::PageSource,
        prefix_columns: usize,
        values: &[Value],
    ) -> Result<bool> {
        if prefix_columns == 0
            || prefix_columns > self.schema.columns.len()
            || values.len() != prefix_columns
        {
            return Err(DbError::new(
                ErrorCode::Type,
                "index prefix requires one value per leading column",
            ));
        }
        let logical = IndexKeyCodec::encode_prefix(&self.schema.columns[..prefix_columns], values)?;
        let (start, end) = IndexKeyCodec::checked_prefix_bounds(&logical)?;
        let mut scan = self.tree.scan_state(
            start.as_ref().map(Vec::as_slice),
            end.as_ref().map(Vec::as_slice),
        )?;
        let mut found = false;
        while let Some((physical, value)) = scan.next(source)? {
            if !value.is_empty() {
                return Err(corruption("index leaf value must be empty"));
            }
            IndexKeyCodec::decode(&self.schema.columns, &physical)?;
            found = true;
        }
        Ok(found)
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

    pub(crate) const fn id(&self) -> IndexId {
        self.schema.definition.id()
    }

    pub(crate) fn root_changed(&self) -> bool {
        self.tree.root() != self.original_root
    }

    pub(in crate::storage) fn restore_root(&mut self, root: PageRef) {
        self.tree = BTree::from_root(TreeKind::Index, root);
    }

    fn unique_violation(&self) -> DbError {
        match &self.schema.unique_violation {
            UniqueViolationName::ExplicitIndex(name) => DbError::new(
                ErrorCode::Constraint,
                format!("unique index {} is violated", name.as_str()),
            ),
            UniqueViolationName::Constraint(name) => DbError::new(
                ErrorCode::Constraint,
                format!("constraint {} is violated", name.as_str()),
            ),
        }
    }
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::{IndexSpec, IndexStorage, key::IndexKeyCodec};
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

    fn create_index(batch: &mut WriteBatch, spec: &IndexSpec) -> Result<IndexId> {
        super::create_index(batch, spec, 64 * 1024 * 1024)
    }

    fn create_fixture(
        types: &[ColumnType],
        version: u64,
        rows: &[Vec<Value>],
    ) -> (TempDatabase, Pager, WriteBatch, TableId, Vec<ColumnDef>) {
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
            ids.push(column);
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
    fn drop_index_reclaims_multilevel_tree_and_retains_empty_catalog_after_reopen() {
        use crate::storage::validate::{validate_freelist, validate_tree};
        // Long real index keys force internal/leaf pages. Root-only release,
        // clearing the feature bit, or failing to bump the schema breaks this.
        let rows = (0..240)
            .map(|n| vec![Value::Text(format!("{n:04}{}", "x".repeat(800)))])
            .collect::<Vec<_>>();
        let (temp, pager, mut batch, table, columns) =
            create_fixture(&[ColumnType::Text], 1, &rows);
        let spec =
            IndexSpec::new(CatalogName::new("IX_T").unwrap(), table, columns, false).unwrap();
        let index = create_index(&mut batch, &spec).unwrap();
        let definition = batch
            .catalog_reader()
            .unwrap()
            .find_index_by_id(index)
            .unwrap()
            .clone();
        let pages = validate_tree(
            &mut batch,
            &BTree::from_root(TreeKind::Index, definition.root()),
        )
        .unwrap()
        .all_live_pages();
        assert!(pages.len() > 20, "fixture must have multiple index levels");
        let root = batch.read(definition.root()).unwrap();
        let child = crate::storage::btree::decode_internal_leftmost(&root.bytes()[48..64]).unwrap();
        assert_eq!(
            batch.read(child).unwrap().header().page_type(),
            crate::storage::page::PageType::IndexInternal,
            "fixture must exercise internal descendants, not only leaf children"
        );
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        super::drop_index(&mut batch, index).unwrap();
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
        assert!(catalog.find_index_by_id(index).is_none());
        assert!(catalog.find_index_by_name(&spec.name).is_none());
        assert_eq!(catalog.columns_for_index(index).count(), 0);
        assert_eq!(catalog.indexes_for_table(table).count(), 0);
        assert_eq!(
            catalog
                .find_table_by_id(table)
                .unwrap()
                .schema_version()
                .get(),
            3
        );
        assert_eq!(snapshot.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
        let free = validate_freelist(&mut snapshot, &Default::default()).unwrap();
        assert!(
            free.is_superset(&pages),
            "all old index pages must be reusable"
        );
    }

    #[test]
    fn drop_index_missing_id_and_constraint_owner_do_not_mutate() {
        use crate::storage::catalog::types::ConstraintId;
        let (_temp, _pager, mut batch, table, columns) =
            create_fixture(&[ColumnType::Int64], 1, &[]);
        let spec =
            IndexSpec::new(CatalogName::new("IX_T").unwrap(), table, columns, false).unwrap();
        let index = create_index(&mut batch, &spec).unwrap();
        let definition = batch
            .catalog_reader()
            .unwrap()
            .find_index_by_id(index)
            .unwrap()
            .clone();
        let before = batch.checkpoint_mutation().unwrap();
        assert_eq!(
            super::drop_index(&mut batch, IndexId::new(9999).unwrap())
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
        before.assert_matches_batch_for_test(&batch);
        // Phase 4C2-A intentionally cannot persist constraint catalog records.
        // Exercise the future-owner guard with an actual typed definition.
        let owned = IndexDef::new(
            index,
            table,
            definition.root(),
            1,
            false,
            IndexOwnership::Constraint(ConstraintId::new(99).unwrap()),
            definition.name().clone(),
        )
        .unwrap();
        assert_eq!(
            super::require_explicit_owner(&owned, Some(owned.name()))
                .unwrap_err()
                .code(),
            ErrorCode::Constraint
        );
        before.assert_matches_batch_for_test(&batch);
        assert_eq!(
            batch.catalog_reader().unwrap().find_index_by_id(index),
            Some(&definition)
        );
    }

    #[test]
    fn drop_index_schema_bump_failure_restores_removed_records_and_released_pages() {
        // Real late failure after removal (no fault injection): restoring only
        // CatalogWriter's local checkpoint would lose the whole index tree.
        let (_temp, _pager, mut batch, table, columns) =
            create_fixture(&[ColumnType::Int64], u64::MAX - 1, &[vec![Value::Int64(7)]]);
        let index = create_index(
            &mut batch,
            &IndexSpec::new(CatalogName::new("IX_T").unwrap(), table, columns, false).unwrap(),
        )
        .unwrap();
        let before = batch.checkpoint_mutation().unwrap();
        assert_eq!(
            super::drop_index(&mut batch, index).unwrap_err().code(),
            ErrorCode::Type
        );
        before.assert_matches_batch_for_test(&batch);
        let catalog = batch.catalog_reader().unwrap();
        assert!(catalog.find_index_by_id(index).is_some());
        assert_eq!(
            catalog
                .find_table_by_id(table)
                .unwrap()
                .schema_version()
                .get(),
            u64::MAX
        );
        catalog.validate_deep_batch(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn drop_index_release_fault_restores_every_batch_field_and_reopen() {
        let (temp, pager, mut batch, table, columns) =
            create_fixture(&[ColumnType::Int64], 1, &[vec![Value::Int64(7)]]);
        let index = create_index(
            &mut batch,
            &IndexSpec::new(CatalogName::new("IX_T").unwrap(), table, columns, false).unwrap(),
        )
        .unwrap();
        let definition = batch
            .catalog_reader()
            .unwrap()
            .find_index_by_id(index)
            .unwrap()
            .clone();
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        // Include earlier private transaction work in the exact checkpoint.
        create_relation(
            &mut batch,
            &TableSpec::new(
                CatalogName::new("PRIOR").unwrap(),
                vec![ColumnSpec::new(
                    CatalogName::new("C").unwrap(),
                    ColumnType::Int64,
                    true,
                )],
            )
            .unwrap(),
        )
        .unwrap();
        let before = batch.checkpoint_mutation().unwrap();
        let _fault = super::fail_index_release_for_test(0, ErrorCode::Io);
        assert_eq!(
            super::drop_index(&mut batch, index).unwrap_err().code(),
            ErrorCode::Io
        );
        before.assert_matches_batch_for_test(&batch);
        batch
            .catalog_reader()
            .unwrap()
            .validate_deep_batch(&mut batch)
            .unwrap();
        batch.rollback().unwrap();
        drop(batch);
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
        assert_eq!(catalog.find_index_by_id(index), Some(&definition));
        assert_eq!(
            catalog
                .find_table_by_id(table)
                .unwrap()
                .schema_version()
                .get(),
            2
        );
        assert!(
            catalog
                .find_table_by_name(&CatalogName::new("PRIOR").unwrap())
                .is_none()
        );
    }

    #[test]
    fn drop_relation_reclaims_two_indexes_and_relation_without_live_free_overlap() {
        use crate::storage::validate::{validate_freelist, validate_tree};
        let rows = (0..80)
            .map(|n| vec![Value::Text(format!("{n:04}{}", "x".repeat(800)))])
            .collect::<Vec<_>>();
        let (temp, pager, mut batch, table, columns) =
            create_fixture(&[ColumnType::Text], 1, &rows);
        let mut indexes = Vec::new();
        for name in ["IX_A", "IX_B"] {
            indexes.push(
                create_index(
                    &mut batch,
                    &IndexSpec::new(
                        CatalogName::new(name).unwrap(),
                        table,
                        columns.clone(),
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
            );
        }
        let catalog = batch.catalog_reader().unwrap();
        let table_root = catalog.find_table_by_id(table).unwrap().root();
        let mut pages = validate_tree(&mut batch, &BTree::from_root(TreeKind::Table, table_root))
            .unwrap()
            .all_live_pages();
        for index in &indexes {
            let root = catalog.find_index_by_id(*index).unwrap().root();
            pages.extend(
                validate_tree(&mut batch, &BTree::from_root(TreeKind::Index, root))
                    .unwrap()
                    .all_live_pages(),
            );
        }
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        crate::storage::relation::drop_relation(&mut batch, table).unwrap();
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
        assert!(catalog.find_table_by_id(table).is_none());
        assert_eq!(catalog.columns_for_table(table).count(), 0);
        assert_eq!(catalog.indexes_for_table(table).count(), 0);
        for index in indexes {
            assert!(catalog.find_index_by_id(index).is_none());
            assert_eq!(catalog.columns_for_index(index).count(), 0);
        }
        assert_eq!(snapshot.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
        let free = validate_freelist(&mut snapshot, &Default::default()).unwrap();
        assert!(free.is_superset(&pages));
    }

    #[test]
    fn drop_relation_second_index_release_failure_restores_first_index_and_table() {
        let (_temp, pager, mut batch, table, columns) =
            create_fixture(&[ColumnType::Int64], 1, &[vec![Value::Int64(1)]]);
        for name in ["IX_A", "IX_B"] {
            create_index(
                &mut batch,
                &IndexSpec::new(
                    CatalogName::new(name).unwrap(),
                    table,
                    columns.clone(),
                    false,
                )
                .unwrap(),
            )
            .unwrap();
        }
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let before = batch.checkpoint_mutation().unwrap();
        let _fault = super::fail_index_release_for_test(1, ErrorCode::Io);
        assert_eq!(
            crate::storage::relation::drop_relation(&mut batch, table)
                .unwrap_err()
                .code(),
            ErrorCode::Io
        );
        before.assert_matches_batch_for_test(&batch);
        let catalog = batch.catalog_reader().unwrap();
        assert_eq!(catalog.indexes_for_table(table).count(), 2);
        catalog.validate_deep_batch(&mut batch).unwrap();
        batch.rollback().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        snapshot
            .catalog_reader()
            .unwrap()
            .validate_deep(&mut snapshot)
            .unwrap();
    }

    #[test]
    fn drop_index_and_drop_relation_release_errors_preserve_transaction_classification() {
        use crate::{Database, Options, Parameters};
        // The hook fires only after actual catalog deletion/page release. An
        // ordinary error must restore the statement but keep prior writes;
        // each fatal class must poison the explicit transaction, not commit it.
        for sql in ["DROP INDEX IX_T", "DROP TABLE T"] {
            for code in [
                ErrorCode::Constraint,
                ErrorCode::Io,
                ErrorCode::Corruption,
                ErrorCode::Internal,
            ] {
                let temp = TempDatabase::new("index-drop-transaction-errors");
                let database = Database::create(temp.path(), Options::default()).unwrap();
                let mut connection = database.connect().unwrap();
                for setup in [
                    "CREATE TABLE T (A INT64)",
                    "INSERT INTO T VALUES (7)",
                    "CREATE INDEX IX_T ON T (A)",
                ] {
                    let statement = connection.prepare(setup).unwrap();
                    connection.execute(&statement, Parameters::none()).unwrap();
                }
                let retained = connection.prepare("SELECT A FROM T").unwrap();
                let mut transaction = connection.begin_write().unwrap();
                let prior = transaction.prepare("CREATE TABLE PRIOR (A INT64)").unwrap();
                transaction.execute(&prior, Parameters::none()).unwrap();
                let drop_statement = transaction.prepare(sql).unwrap();
                let _fault = super::fail_index_release_for_test(0, code);
                assert_eq!(
                    transaction
                        .execute(&drop_statement, Parameters::none())
                        .unwrap_err()
                        .code(),
                    code
                );
                if code == ErrorCode::Constraint {
                    let mut rows = transaction.query(&retained, Parameters::none()).unwrap();
                    assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(7)]);
                    drop(rows);
                    let insert = transaction.prepare("INSERT INTO PRIOR VALUES (9)").unwrap();
                    transaction.execute(&insert, Parameters::none()).unwrap();
                    assert!(transaction.prepare("DROP INDEX IX_T").is_ok());
                    transaction.commit().unwrap();
                } else {
                    assert_eq!(
                        transaction.prepare("SELECT A FROM T").unwrap_err().code(),
                        ErrorCode::Transaction
                    );
                    assert_eq!(
                        transaction
                            .execute(&drop_statement, Parameters::none())
                            .unwrap_err()
                            .code(),
                        ErrorCode::Transaction
                    );
                    assert_eq!(
                        transaction.commit().unwrap_err().code(),
                        ErrorCode::Transaction
                    );
                }
                drop(connection);
                drop(database);
                let database = Database::open(temp.path(), Options::default()).unwrap();
                let mut connection = database.connect().unwrap();
                assert!(connection.prepare("DROP INDEX IX_T").is_ok());
                assert_eq!(
                    connection.prepare("SELECT A FROM PRIOR").is_ok(),
                    code == ErrorCode::Constraint
                );
                let mut rows = connection.query(&retained, Parameters::none()).unwrap();
                assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(7)]);
            }
        }
    }

    #[test]
    fn drop_index_and_drop_relation_validate_all_descendants_before_first_release() {
        use crate::storage::{
            PageRef, btree::LEAF_METADATA_END, pager::PageSource, validate::validate_tree,
        };
        let rows = (0..40)
            .map(|n| vec![Value::Text(format!("{n:04}{}", "x".repeat(800)))])
            .collect::<Vec<_>>();
        let (_temp, _pager, mut batch, table, columns) =
            create_fixture(&[ColumnType::Text], 1, &rows);
        let mut last = None;
        for name in ["IX_A", "IX_B"] {
            last = Some(
                create_index(
                    &mut batch,
                    &IndexSpec::new(
                        CatalogName::new(name).unwrap(),
                        table,
                        columns.clone(),
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
            );
        }
        let index = batch
            .catalog_reader()
            .unwrap()
            .find_index_by_id(last.unwrap())
            .unwrap()
            .clone();
        let report =
            validate_tree(&mut batch, &BTree::from_root(TreeKind::Index, index.root())).unwrap();
        let mut child = None;
        for page in report.tree_pages {
            let frame = batch.read_page_by_id(page).unwrap();
            if frame.header().page_type() == crate::storage::page::PageType::IndexLeaf {
                child = Some(PageRef::new(page, frame.header().generation()).unwrap());
                break;
            }
        }
        let child = child.expect("multi-page fixture needs a leaf child");
        assert_ne!(child, index.root());
        // Destroy a descendant slot offset; normal catalog open still succeeds.
        batch.modify(child).unwrap().bytes_mut()[LEAF_METADATA_END..LEAF_METADATA_END + 2].fill(0);
        batch.catalog_reader().unwrap();
        let before = batch.checkpoint_mutation().unwrap();
        let _fault = super::fail_index_release_for_test(0, ErrorCode::Io);
        assert_eq!(
            super::drop_index(&mut batch, index.id())
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        before.assert_matches_batch_for_test(&batch);
        assert_eq!(
            crate::storage::relation::drop_relation(&mut batch, table)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        before.assert_matches_batch_for_test(&batch);
        assert!(
            super::INDEX_RELEASE_FAILURE.get().is_some(),
            "preflight must not release even the healthy first index"
        );
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
        let columns = vec![ids[1].clone(), ids[0].clone()];
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
            columns.iter().map(ColumnDef::id).collect::<Vec<_>>()
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
            vec![Value::Text("x".repeat(1014))],
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
        for columns in [
            vec![],
            vec![ids[0].clone(), ids[0].clone()],
            vec![ids[0].clone(); 1001],
        ] {
            assert!(
                matches!(IndexSpec::new(name.clone(), table, columns, false), Err(error) if error.code() == ErrorCode::Type)
            );
        }
        let before = batch.checkpoint_mutation().unwrap();
        let absent = ColumnDef::new(
            ColumnId::new(999).unwrap(),
            table,
            0,
            ColumnType::Int64,
            true,
            CatalogName::new("C0").unwrap(),
        )
        .unwrap();
        let spec = IndexSpec::new(name, table, vec![absent], false).unwrap();
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
    fn exact_parent_key_probe_decodes_every_entry_and_rejects_corruption() {
        // Production breaks caught: treating a bounded prefix hit as existence
        // without decoding it, returning after the first valid duplicate, or
        // accepting nonempty index leaf payloads.
        let mut fixture = writable_index_fixture(true, &[ColumnType::Int64]);
        fixture.insert(&[Value::Int64(7)], 1).unwrap();
        assert!(
            fixture
                .storage
                .contains_key(&mut fixture.batch, &[Value::Int64(7)])
                .unwrap()
        );
        assert!(
            !fixture
                .storage
                .contains_key(&mut fixture.batch, &[Value::Int64(8)])
                .unwrap()
        );

        let corrupt = fixture
            .storage
            .prepare(&[Value::Int64(7)], RowId::new(2).unwrap())
            .unwrap();
        fixture
            .storage
            .tree
            .insert(&mut fixture.batch, &corrupt.physical, b"bad")
            .unwrap();
        assert_eq!(
            fixture
                .storage
                .contains_key(&mut fixture.batch, &[Value::Int64(7)])
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        fixture
            .storage
            .tree
            .delete(&mut fixture.batch, &corrupt.physical)
            .unwrap();
        let mut malformed = corrupt.physical;
        malformed[corrupt.logical_len..].fill(0);
        fixture
            .storage
            .tree
            .insert(&mut fixture.batch, &malformed, &[])
            .unwrap();
        assert_eq!(
            fixture
                .storage
                .contains_key(&mut fixture.batch, &[Value::Int64(7)])
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn prefix_probe_accepts_trailing_columns_and_decodes_every_matching_entry() {
        // Production breaks caught: requiring a complete index key rejects a
        // usable FK-leading index, while returning at the first matching entry
        // masks corruption later in the same logical prefix.
        let mut fixture = writable_index_fixture(false, &[ColumnType::Int64, ColumnType::Text]);
        fixture
            .insert(&[Value::Int64(7), Value::Text("A".into())], 1)
            .unwrap();
        fixture
            .insert(&[Value::Int64(7), Value::Text("B".into())], 2)
            .unwrap();
        fixture
            .insert(&[Value::Int64(8), Value::Text("C".into())], 3)
            .unwrap();
        assert!(
            fixture
                .storage
                .contains_prefix(&mut fixture.batch, 1, &[Value::Int64(7)])
                .unwrap()
        );
        assert!(
            !fixture
                .storage
                .contains_prefix(&mut fixture.batch, 1, &[Value::Int64(9)])
                .unwrap()
        );
        for (count, values) in [(0, vec![]), (3, vec![Value::Int64(7)])] {
            assert_eq!(
                fixture
                    .storage
                    .contains_prefix(&mut fixture.batch, count, &values)
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
        }
        assert_eq!(
            fixture
                .storage
                .contains_prefix(
                    &mut fixture.batch,
                    1,
                    &[Value::Int64(7), Value::Text("A".into())],
                )
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );

        let corrupt = fixture
            .storage
            .prepare(
                &[Value::Int64(7), Value::Text("Z".into())],
                RowId::new(4).unwrap(),
            )
            .unwrap();
        fixture
            .storage
            .tree
            .insert(&mut fixture.batch, &corrupt.physical, b"bad")
            .unwrap();
        assert_eq!(
            fixture
                .storage
                .contains_prefix(&mut fixture.batch, 1, &[Value::Int64(7)])
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn exact_parent_key_probe_reads_the_current_split_root() {
        // Production break caught: retaining the index's original leaf root
        // makes a probe miss keys after the parent system index splits.
        let mut fixture = writable_index_fixture(true, &[ColumnType::Int64]);
        let original_root = fixture.storage.root();
        for rowid in 1..=200 {
            fixture
                .insert(&[Value::Int64(rowid as i64)], rowid)
                .unwrap();
        }
        assert_ne!(fixture.storage.root(), original_root);
        for value in [1, 100, 200] {
            assert!(
                fixture
                    .storage
                    .contains_key(&mut fixture.batch, &[Value::Int64(value)])
                    .unwrap()
            );
        }
        assert!(
            !fixture
                .storage
                .contains_key(&mut fixture.batch, &[Value::Int64(201)])
                .unwrap()
        );
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
            decoded.values().expect("V1 components"),
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
