pub(crate) mod key;
pub(crate) mod record;
pub(crate) mod types;

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

use super::{
    PageRef,
    btree::{BTree, TreeKind},
    features::{CATALOG_V1, validate_feature_bits},
    pager::{PageSource, ReadSnapshot, WriteBatch, WriteBatchCheckpoint},
    validate::{
        ValidationReport, validate_freelist, validate_table_roots, validate_table_trees,
        validate_tree,
    },
};
use crate::{DbError, ErrorCode, Result};
use key::DecodedCatalogKey;
use types::{CatalogMeta, CatalogName, ColumnDef, ColumnId, ObjectId, RowId, TableDef, TableId};

/// A fully decoded and cross-checked image. No page source is retained.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CatalogImage {
    meta: CatalogMeta,
    tables: BTreeMap<TableId, TableDef>,
    table_names: BTreeMap<String, TableId>,
    columns: BTreeMap<ColumnId, ColumnDef>,
    column_names: BTreeMap<TableId, BTreeMap<String, ColumnId>>,
    column_ordinals: BTreeMap<TableId, BTreeMap<u16, ColumnId>>,
}

impl CatalogImage {
    fn empty() -> Self {
        Self {
            meta: CatalogMeta::new(ObjectId::new(1).expect("initial object ID is nonzero")),
            tables: BTreeMap::new(),
            table_names: BTreeMap::new(),
            columns: BTreeMap::new(),
            column_names: BTreeMap::new(),
            column_ordinals: BTreeMap::new(),
        }
    }

    fn decode(source: &mut impl PageSource, tree: &BTree) -> Result<Self> {
        let mut image = Self::empty();
        let mut meta = None;
        let mut cursor = tree.scan(source, Bound::Unbounded, Bound::Unbounded)?;
        while let Some((key, value)) = cursor.next()? {
            match key::decode_key(&key)? {
                DecodedCatalogKey::CatalogMeta => {
                    if meta.replace(record::decode_catalog_meta(&value)?).is_some() {
                        return Err(corruption("catalog contains duplicate meta records"));
                    }
                }
                DecodedCatalogKey::TableName(name) => {
                    insert_unique(
                        &mut image.table_names,
                        name.to_owned(),
                        record::decode_table_name_mapping(&value)?,
                    )?;
                }
                DecodedCatalogKey::TableDefinition(id) => {
                    insert_unique(
                        &mut image.tables,
                        id,
                        record::decode_table_definition(id, &value)?,
                    )?;
                }
                DecodedCatalogKey::ColumnName { table_id, name } => {
                    insert_unique(
                        image.column_names.entry(table_id).or_default(),
                        name.to_owned(),
                        record::decode_column_name_mapping(&value)?,
                    )?;
                }
                DecodedCatalogKey::ColumnOrdinal { table_id, ordinal } => {
                    insert_unique(
                        image.column_ordinals.entry(table_id).or_default(),
                        ordinal,
                        record::decode_column_ordinal_mapping(&value)?,
                    )?;
                }
                DecodedCatalogKey::ColumnDefinition(id) => {
                    insert_unique(
                        &mut image.columns,
                        id,
                        record::decode_column_definition(id, &value)?,
                    )?;
                }
            }
        }
        image.meta = meta.ok_or_else(|| corruption("catalog meta record is missing"))?;
        image.validate_mappings()?;
        Ok(image)
    }

    fn validate_mappings(&self) -> Result<()> {
        let mut object_ids: BTreeSet<ObjectId> =
            self.tables.keys().copied().map(Into::into).collect();
        // Check both directions: matching cardinalities alone do not rule out aliases.
        for (name, id) in &self.table_names {
            if self
                .tables
                .get(id)
                .is_none_or(|table| table.name().as_str() != name)
            {
                return Err(corruption("table name mapping has no matching definition"));
            }
        }
        for table in self.tables.values() {
            if self.table_names.get(table.name().as_str()) != Some(&table.id()) {
                return Err(corruption("table definition has no matching name mapping"));
            }
            let ordinals = self.column_ordinals.get(&table.id());
            if ordinals.map_or(0, BTreeMap::len) != usize::from(table.column_count()) {
                return Err(corruption(
                    "table column count does not match ordinal mappings",
                ));
            }
            for (expected, ordinal) in ordinals.into_iter().flat_map(|map| map.keys()).enumerate() {
                let expected = u16::try_from(expected)
                    .map_err(|_| corruption("column ordinal exceeds u16"))?;
                if *ordinal != expected {
                    return Err(corruption(
                        "table column ordinals are not contiguous from zero",
                    ));
                }
            }
        }
        for column in self.columns.values() {
            if !object_ids.insert(column.id().into()) {
                return Err(corruption("catalog definitions share a global object ID"));
            }
            if !self.tables.contains_key(&column.table_id()) {
                return Err(corruption("column definition references a missing table"));
            }
            if self
                .column_names
                .get(&column.table_id())
                .and_then(|names| names.get(column.name().as_str()))
                != Some(&column.id())
            {
                return Err(corruption("column definition has no matching name mapping"));
            }
            if self
                .column_ordinals
                .get(&column.table_id())
                .and_then(|ordinals| ordinals.get(&column.ordinal()))
                != Some(&column.id())
            {
                return Err(corruption(
                    "column definition has no matching ordinal mapping",
                ));
            }
        }
        for (table_id, names) in &self.column_names {
            for (name, id) in names {
                if self.columns.get(id).is_none_or(|column| {
                    column.table_id() != *table_id || column.name().as_str() != name
                }) {
                    return Err(corruption(
                        "column name mapping has no matching owned definition",
                    ));
                }
            }
        }
        for (table_id, ordinals) in &self.column_ordinals {
            for (ordinal, id) in ordinals {
                if self.columns.get(id).is_none_or(|column| {
                    column.table_id() != *table_id || column.ordinal() != *ordinal
                }) {
                    return Err(corruption(
                        "column ordinal mapping has no matching owned definition",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CatalogReader {
    image: CatalogImage,
}

impl CatalogReader {
    pub(super) fn open_empty_or_v1(snapshot: &mut ReadSnapshot) -> Result<Self> {
        if snapshot.feature_bits() & CATALOG_V1 == 0 {
            if snapshot.catalog_root().0 != 0 {
                return Err(unsupported_legacy());
            }
            return Ok(Self {
                image: CatalogImage::empty(),
            });
        }
        let (tree, report) = snapshot
            .catalog_tree_with_report(TreeKind::Table)?
            .ok_or_else(|| corruption("catalog V1 feature requires a catalog root"))?;
        Self::open_validated(snapshot, &tree, report)
    }

    /// Validate an unpublished candidate; its current root can differ from the header.
    pub(super) fn open_candidate(
        batch: &mut WriteBatch<'_>,
        tree: &BTree,
        feature_bits: u64,
    ) -> Result<Self> {
        validate_feature_bits(feature_bits)?;
        if feature_bits & CATALOG_V1 == 0 {
            return Err(unsupported_legacy());
        }
        if tree.kind() != TreeKind::Table {
            return Err(corruption("catalog must use a Table B+Tree"));
        }
        batch.require_current_catalog_root(tree.root())?;
        let report = validate_tree(batch, tree)?;
        let reader = Self::open_validated(batch, tree, report)?;
        for table in reader.image.tables.values() {
            batch.require_current_catalog_root(table.root())?;
        }
        Ok(reader)
    }

    fn open_validated(
        source: &mut impl PageSource,
        tree: &BTree,
        report: ValidationReport,
    ) -> Result<Self> {
        let image = CatalogImage::decode(source, tree)?;
        let mut live = report.all_live_pages();
        validate_table_roots(source, image.tables.values().map(TableDef::root), &mut live)?;
        validate_freelist(source, &live)?;
        Ok(Self { image })
    }

    /// Explicit, potentially expensive validation. Ordinary open never walks the
    /// user descendants; here every live tree is checked against every other tree.
    pub(super) fn validate_deep(&self, snapshot: &mut ReadSnapshot) -> Result<()> {
        validate_feature_bits(snapshot.feature_bits())?;
        if snapshot.feature_bits() & CATALOG_V1 == 0 {
            if snapshot.catalog_root().0 != 0 {
                return Err(unsupported_legacy());
            }
            self.require_matching_image(&CatalogImage::empty())?;
            return validate_freelist(snapshot, &BTreeSet::new()).map(|_| ());
        }
        let (tree, report) = snapshot
            .catalog_tree_with_report(TreeKind::Table)?
            .ok_or_else(|| corruption("catalog V1 feature requires a catalog root"))?;
        self.validate_deep_from(snapshot, &tree, report)
    }

    fn validate_deep_from(
        &self,
        source: &mut impl PageSource,
        tree: &BTree,
        report: ValidationReport,
    ) -> Result<()> {
        // A reader retains no snapshot lease. Do not combine its immutable
        // definitions with a different snapshot's catalog, roots, or counters.
        self.require_matching_image(&CatalogImage::decode(source, tree)?)?;
        let mut live = report.all_live_pages();
        validate_table_trees(
            source,
            self.image.tables.values().map(TableDef::root),
            &mut live,
        )?;
        validate_freelist(source, &live).map(|_| ())
    }

    fn require_matching_image(&self, image: &CatalogImage) -> Result<()> {
        if &self.image != image {
            return Err(DbError::new(
                ErrorCode::Transaction,
                "deep validation snapshot does not match the catalog reader",
            ));
        }
        Ok(())
    }

    pub(crate) fn find_table_by_name(&self, name: &CatalogName) -> Option<&TableDef> {
        self.image
            .table_names
            .get(name.as_str())
            .and_then(|id| self.image.tables.get(id))
    }

    pub(crate) fn find_table_by_id(&self, id: TableId) -> Option<&TableDef> {
        self.image.tables.get(&id)
    }

    pub(crate) fn columns_for_table(&self, table_id: TableId) -> impl Iterator<Item = &ColumnDef> {
        self.image
            .column_ordinals
            .get(&table_id)
            .into_iter()
            .flat_map(|ordinals| ordinals.values())
            .map(|id| {
                self.image
                    .columns
                    .get(id)
                    .expect("validated ordinal references an existing column")
            })
    }

    pub(crate) fn find_column_by_name(
        &self,
        table_id: TableId,
        name: &CatalogName,
    ) -> Option<&ColumnDef> {
        self.image
            .column_names
            .get(&table_id)
            .and_then(|names| names.get(name.as_str()))
            .and_then(|id| self.image.columns.get(id))
    }
}

/// Batch-local directory identity. Mutations never retain a borrow of the batch.
pub(super) struct CatalogWriter {
    tree: BTree,
    dirty: bool,
}

impl CatalogWriter {
    pub(super) fn bootstrap(batch: &mut WriteBatch<'_>) -> Result<Self> {
        let checkpoint = batch.checkpoint_mutation()?;
        let result = (|| {
            // One outstanding writer per batch prevents a later open from finishing
            // an older root. The checkpoint also restores this gate on every error.
            batch.begin_catalog_update()?;
            let features = batch.feature_bits();
            validate_feature_bits(features)?;
            if features & CATALOG_V1 == 0 {
                if batch.catalog_root().0 != 0 {
                    return Err(unsupported_legacy());
                }
                let mut tree = BTree::create(batch, TreeKind::Table)?;
                let meta = CatalogMeta::new(ObjectId::new(1)?);
                tree.insert(
                    batch,
                    &key::catalog_meta_key(),
                    &record::encode_catalog_meta(&meta),
                )?;
                batch.enable_feature_bits(CATALOG_V1)?;
                Ok(Self { tree, dirty: true })
            } else {
                let (tree, report) = batch
                    .catalog_tree_with_report(TreeKind::Table)?
                    .ok_or_else(|| corruption("catalog V1 feature requires a catalog root"))?;
                CatalogReader::open_validated(batch, &tree, report)?;
                Ok(Self { tree, dirty: false })
            }
        })();
        if result.is_err() {
            batch.restore_mutation(checkpoint);
        }
        result
    }

    /// Restore the batch image and the writer's separately owned root together.
    fn restore(
        &mut self,
        batch: &mut WriteBatch<'_>,
        checkpoint: WriteBatchCheckpoint,
        root: PageRef,
    ) {
        batch.restore_mutation(checkpoint);
        self.tree = BTree::from_root(TreeKind::Table, root);
    }

    fn mutate<T>(
        &mut self,
        batch: &mut WriteBatch<'_>,
        mutation: impl FnOnce(&mut BTree, &mut WriteBatch<'_>) -> Result<T>,
    ) -> Result<T> {
        let checkpoint = batch.checkpoint_mutation()?;
        let root = self.tree.root();
        match mutation(&mut self.tree, batch) {
            Ok(value) => {
                // Bootstrap owns the pending gate until finish, even on clean opens.
                self.dirty = true;
                Ok(value)
            }
            Err(error) => {
                self.restore(batch, checkpoint, root);
                Err(error)
            }
        }
    }

    pub(crate) fn allocate_object_id(&mut self, batch: &mut WriteBatch<'_>) -> Result<ObjectId> {
        self.mutate(batch, |tree, batch| {
            let key = key::catalog_meta_key();
            let bytes = tree
                .get(batch, &key)?
                .ok_or_else(|| corruption("catalog meta record is missing"))?;
            let candidate = record::decode_catalog_meta(&bytes)?.next_object_id();
            let next = candidate.get().checked_add(1).ok_or_else(|| {
                DbError::new(ErrorCode::Type, "catalog object ID counter is exhausted")
            })?;
            require_available_object(tree, batch, candidate)?;
            let meta = CatalogMeta::new(ObjectId::new(next)?);
            replace_record(tree, batch, &key, &record::encode_catalog_meta(&meta))?;
            Ok(candidate)
        })
    }

    pub(crate) fn insert_table_definition(
        &mut self,
        batch: &mut WriteBatch<'_>,
        table: &TableDef,
    ) -> Result<()> {
        self.mutate(batch, |tree, batch| {
            require_available_object(tree, batch, table.id().into())?;
            let name_key = key::table_name_key(table.name())?;
            require_available_key(tree, batch, &name_key, "duplicate table name")?;
            tree.insert(
                batch,
                &key::table_definition_key(table.id()),
                &record::encode_table_definition(table),
            )?;
            tree.insert(
                batch,
                &name_key,
                &record::encode_table_name_mapping(table.id()),
            )?;
            if TreeKind::from_root_page_type(batch.read(table.root())?.header().page_type())
                != Some(TreeKind::Table)
            {
                return Err(corruption(
                    "catalog table root must reference a Table B+Tree",
                ));
            }
            batch.track_tree(TreeKind::Table, table.root())
        })
    }

    pub(crate) fn insert_column_definition(
        &mut self,
        batch: &mut WriteBatch<'_>,
        column: &ColumnDef,
    ) -> Result<()> {
        self.mutate(batch, |tree, batch| {
            read_table_definition(tree, batch, column.table_id())?;
            // ColumnDef's checked constructor enforces ordinal < MAX_COLUMNS.
            require_available_object(tree, batch, column.id().into())?;
            let name_key = key::column_name_key(column.table_id(), column.name())?;
            let ordinal_key = key::column_ordinal_key(column.table_id(), column.ordinal());
            require_available_key(tree, batch, &name_key, "duplicate table-scoped column name")?;
            require_available_key(
                tree,
                batch,
                &ordinal_key,
                "duplicate table-scoped column ordinal",
            )?;
            tree.insert(
                batch,
                &key::column_definition_key(column.id()),
                &record::encode_column_definition(column),
            )?;
            tree.insert(
                batch,
                &name_key,
                &record::encode_column_name_mapping(column.id()),
            )?;
            tree.insert(
                batch,
                &ordinal_key,
                &record::encode_column_ordinal_mapping(column.id()),
            )
        })
    }

    pub(crate) fn update_table_root(
        &mut self,
        batch: &mut WriteBatch<'_>,
        table_id: TableId,
        root: PageRef,
    ) -> Result<()> {
        self.mutate(batch, |tree, batch| {
            let table = read_table_definition(tree, batch, table_id)?;
            batch.require_tracked_table_root(root)?;
            replace_record(
                tree,
                batch,
                &key::table_definition_key(table_id),
                &record::encode_table_definition(&table.with_root(root)),
            )
        })
    }

    pub(crate) fn allocate_rowid(
        &mut self,
        batch: &mut WriteBatch<'_>,
        table_id: TableId,
    ) -> Result<RowId> {
        self.mutate(batch, |tree, batch| {
            let table = read_table_definition(tree, batch, table_id)?;
            let candidate = table.next_rowid();
            let next = candidate.get().checked_add(1).ok_or_else(|| {
                DbError::new(
                    ErrorCode::Type,
                    format!("ROWID counter exhausted for table {}", table_id.get()),
                )
            })?;
            let table = table.with_next_rowid(RowId::new(next)?);
            replace_record(
                tree,
                batch,
                &key::table_definition_key(table_id),
                &record::encode_table_definition(&table),
            )?;
            Ok(candidate)
        })
    }

    /// Publish only the fully validated current root; failure leaves commit gated.
    pub(super) fn finish(self, batch: &mut WriteBatch<'_>) -> Result<()> {
        if self.dirty {
            CatalogReader::open_candidate(batch, &self.tree, batch.feature_bits())?;
            batch.track_tree(TreeKind::Table, self.tree.root())?;
            batch.set_catalog_root(self.tree.root())?;
        }
        batch.finish_catalog_update()
    }
}

fn read_table_definition(
    tree: &BTree,
    batch: &mut WriteBatch<'_>,
    id: TableId,
) -> Result<TableDef> {
    let bytes = tree
        .get(batch, &key::table_definition_key(id))?
        .ok_or_else(|| {
            DbError::new(
                ErrorCode::Corruption,
                format!("catalog table definition {} is missing", id.get()),
            )
        })?;
    record::decode_table_definition(id, &bytes)
}

fn require_available_object(tree: &BTree, batch: &mut WriteBatch<'_>, id: ObjectId) -> Result<()> {
    // V1 defines only table and column objects; their identities share one namespace.
    for key in [
        key::table_definition_key(TableId::new(id.get())?),
        key::column_definition_key(ColumnId::new(id.get())?),
    ] {
        if tree.get(batch, &key)?.is_some() {
            return Err(DbError::new(
                ErrorCode::Constraint,
                format!("catalog object ID {} already exists", id.get()),
            ));
        }
    }
    Ok(())
}

fn require_available_key(
    tree: &BTree,
    batch: &mut WriteBatch<'_>,
    key: &[u8],
    message: &'static str,
) -> Result<()> {
    if tree.get(batch, key)?.is_some() {
        return Err(DbError::new(ErrorCode::Constraint, message));
    }
    Ok(())
}

// Called only inside a logical mutation checkpoint, not as an independent upsert.
fn replace_record(
    tree: &mut BTree,
    batch: &mut WriteBatch<'_>,
    key: &[u8],
    value: &[u8],
) -> Result<()> {
    if tree.delete(batch, key)?.is_none() {
        return Err(corruption("catalog replacement record is missing"));
    }
    tree.insert(batch, key, value)
}

fn insert_unique<K: Ord, V>(map: &mut BTreeMap<K, V>, key: K, value: V) -> Result<()> {
    if map.insert(key, value).is_some() {
        return Err(corruption("catalog contains a duplicate decoded key"));
    }
    Ok(())
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

fn unsupported_legacy() -> DbError {
    DbError::new(
        ErrorCode::Unsupported,
        "nonempty catalog without CATALOG_V1 is unsupported",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{key::*, record::*, types::*};
    use crate::{
        ErrorCode,
        storage::{
            PageId, PageRef,
            btree::{BTree, TreeKind},
            features::CATALOG_V1,
            file::OpenMode,
            page::PageType,
            pager::{Pager, PagerConfig, WriteBatch},
            row::{RowCodec, encode_rowid_key},
            test_support::{TempDatabase, test_pager},
        },
    };

    #[test]
    fn uninitialized_snapshot_is_an_immutable_empty_catalog() {
        let (temp, pager) = test_pager(4096).unwrap();
        // Windows file locks require releasing the pager before byte comparisons.
        drop(pager);
        let before = std::fs::read(temp.path()).unwrap();
        let wal_path = crate::storage::wal::wal_path(temp.path());
        let wal_before = std::fs::read(&wal_path).unwrap();
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut read = pager.begin_read().unwrap();
        let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        let name = CatalogName::new("T").unwrap();
        assert!(catalog.find_table_by_name(&name).is_none());
        drop(read);
        drop(pager);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);
        assert_eq!(std::fs::read(wal_path).unwrap(), wal_before);
    }

    type Records = Vec<(Vec<u8>, Vec<u8>)>;

    fn new_table(id: u64, root: PageRef, count: u16, name: &str) -> TableDef {
        TableDef::new(
            TableId::new(id).unwrap(),
            SchemaVersion::new(1).unwrap(),
            root,
            RowId::new(1).unwrap(),
            count,
            CatalogName::new(name).unwrap(),
        )
        .unwrap()
    }

    fn new_column(id: u64, table_id: TableId, ordinal: u16, name: &str) -> ColumnDef {
        ColumnDef::new(
            ColumnId::new(id).unwrap(),
            table_id,
            ordinal,
            ColumnType::Int64,
            true,
            CatalogName::new(name).unwrap(),
        )
        .unwrap()
    }

    fn committed_single_column_catalog() -> (TempDatabase, Pager, TableId) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let id = writer.allocate_object_id(&mut batch).unwrap();
        let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let table = new_table(id.get(), root, 1, "T");
        writer.insert_table_definition(&mut batch, &table).unwrap();
        let column_id = writer.allocate_object_id(&mut batch).unwrap();
        writer
            .insert_column_definition(&mut batch, &new_column(column_id.get(), table.id(), 0, "C"))
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        (temp, pager, table.id())
    }

    fn allocate_then_rollback(pager: &Pager, table_id: TableId) -> RowId {
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
        batch.rollback().unwrap();
        rowid
    }

    fn allocate_then_commit(pager: &Pager, table_id: TableId) -> RowId {
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        rowid
    }

    #[test]
    fn rolled_back_rowid_is_reused_but_committed_rowid_is_not() {
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let rolled_back = allocate_then_rollback(&pager, table_id);
        let committed = allocate_then_commit(&pager, table_id);
        assert_eq!(rolled_back, RowId::new(1).unwrap());
        assert_eq!(committed, RowId::new(1).unwrap());
        assert_eq!(
            allocate_then_commit(&pager, table_id),
            RowId::new(2).unwrap()
        );
    }

    #[test]
    fn object_ids_are_sequential_and_only_commits_consume_them() {
        let (_temp, pager) = test_pager(4096).unwrap();
        for commit in [false, true] {
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            for expected in 1..=16 {
                assert_eq!(
                    writer.allocate_object_id(&mut batch).unwrap().get(),
                    expected
                );
            }
            if commit {
                writer.finish(&mut batch).unwrap();
                batch.commit().unwrap();
            } else {
                batch.rollback().unwrap();
            }
        }
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(writer.allocate_object_id(&mut batch).unwrap().get(), 17);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    fn current_records(writer: &CatalogWriter, batch: &mut WriteBatch<'_>) -> Records {
        let mut cursor = writer
            .tree
            .scan(batch, Bound::Unbounded, Bound::Unbounded)
            .unwrap();
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().unwrap() {
            rows.push(row);
        }
        rows
    }

    // Detect partial definition/name/ordinal inserts and accidental dirty marking.
    #[test]
    fn duplicate_definitions_names_ordinals_and_missing_owners_are_atomic() {
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let before = current_records(&writer, &mut batch);
        let pages = batch.page_count().unwrap();
        let catalog_root = writer.tree.root();
        for (id, name) in [(1, "Other"), (2, "Other"), (3, "T")] {
            let error = writer
                .insert_table_definition(&mut batch, &new_table(id, root, 0, name))
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Constraint);
            assert_eq!(current_records(&writer, &mut batch), before);
            assert_eq!(writer.tree.root(), catalog_root);
            assert_eq!(batch.page_count().unwrap(), pages);
            assert!(!writer.dirty);
        }
        for (id, ordinal, name) in [(1, 1, "D"), (2, 1, "D"), (3, 1, "C"), (3, 0, "D")] {
            let error = writer
                .insert_column_definition(&mut batch, &new_column(id, table_id, ordinal, name))
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Constraint);
            assert_eq!(current_records(&writer, &mut batch), before);
            assert_eq!(writer.tree.root(), catalog_root);
            assert_eq!(batch.page_count().unwrap(), pages);
            assert!(!writer.dirty);
        }
        let missing = TableId::new(999).unwrap();
        assert_eq!(
            writer
                .insert_column_definition(&mut batch, &new_column(3, missing, 0, "C"))
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            writer
                .allocate_rowid(&mut batch, missing)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            writer
                .update_table_root(&mut batch, missing, root)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(current_records(&writer, &mut batch), before);
        assert!(!writer.dirty);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn object_allocation_checks_both_definition_namespaces_before_advancing() {
        for candidate in [1, 2] {
            let (_temp, pager, _) = committed_single_column_catalog();
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            writer.tree.delete(&mut batch, &catalog_meta_key()).unwrap();
            writer
                .tree
                .insert(
                    &mut batch,
                    &catalog_meta_key(),
                    &encode_catalog_meta(&CatalogMeta::new(ObjectId::new(candidate).unwrap())),
                )
                .unwrap();
            let before = current_records(&writer, &mut batch);
            assert_eq!(
                writer.allocate_object_id(&mut batch).unwrap_err().code(),
                ErrorCode::Constraint
            );
            assert_eq!(current_records(&writer, &mut batch), before);
            assert!(!writer.dirty);
        }
    }

    #[test]
    fn failed_logical_mutation_restores_split_root_pages_tracking_and_gate() {
        let (_temp, pager, _) = committed_single_column_catalog();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let before = current_records(&writer, &mut batch);
        let root = writer.tree.root();
        let pages = batch.page_count().unwrap();
        let header_root = batch.catalog_root();
        let error = writer
            .mutate(&mut batch, |tree, batch| {
                tree.delete(batch, &catalog_meta_key())?;
                for n in 10_u64..50 {
                    tree.insert(
                        batch,
                        &table_definition_key(TableId::new(n)?),
                        &vec![0; 400],
                    )?;
                }
                assert_ne!(tree.root(), root);
                Err::<(), _>(DbError::new(
                    ErrorCode::Io,
                    "injected after split before logical completion",
                ))
            })
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io);
        assert_eq!(writer.tree.root(), root);
        assert_eq!(batch.catalog_root(), header_root);
        assert_eq!(batch.page_count().unwrap(), pages);
        assert_eq!(current_records(&writer, &mut batch), before);
        assert!(!writer.dirty);
        assert!(CatalogWriter::bootstrap(&mut batch).is_err());
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn exhausted_and_malformed_object_counters_never_mutate() {
        let (_temp, pager, _table_id) = committed_single_column_catalog();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        for candidate in [u64::MAX - 1, u64::MAX, 0] {
            let mut value = encode_catalog_meta(&CatalogMeta::new(ObjectId::new(1).unwrap()));
            value[12..20].copy_from_slice(&candidate.to_le_bytes());
            writer.tree.delete(&mut batch, &catalog_meta_key()).unwrap();
            writer
                .tree
                .insert(&mut batch, &catalog_meta_key(), &value)
                .unwrap();
            if candidate == u64::MAX - 1 {
                assert_eq!(
                    writer.allocate_object_id(&mut batch).unwrap().get(),
                    u64::MAX - 1
                );
            }
            let before = current_records(&writer, &mut batch);
            assert_eq!(
                writer.allocate_object_id(&mut batch).unwrap_err().code(),
                if candidate == 0 {
                    ErrorCode::Corruption
                } else {
                    ErrorCode::Type
                }
            );
            assert_eq!(current_records(&writer, &mut batch), before);
        }
    }

    #[test]
    fn exhausted_and_malformed_rowid_counters_never_mutate() {
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let key = table_definition_key(table_id);
        for candidate in [u64::MAX - 1, u64::MAX, 0] {
            let mut value = writer.tree.get(&mut batch, &key).unwrap().unwrap();
            value[40..48].copy_from_slice(&candidate.to_le_bytes());
            writer.tree.delete(&mut batch, &key).unwrap();
            writer.tree.insert(&mut batch, &key, &value).unwrap();
            if candidate == u64::MAX - 1 {
                assert_eq!(
                    writer.allocate_rowid(&mut batch, table_id).unwrap().get(),
                    u64::MAX - 1
                );
            }
            let before = current_records(&writer, &mut batch);
            assert_eq!(
                writer
                    .allocate_rowid(&mut batch, table_id)
                    .unwrap_err()
                    .code(),
                if candidate == 0 {
                    ErrorCode::Corruption
                } else {
                    ErrorCode::Type
                }
            );
            assert_eq!(current_records(&writer, &mut batch), before);
        }
    }

    fn split_user_tree(tree: &mut BTree, batch: &mut WriteBatch<'_>) -> u64 {
        let old_root = tree.root();
        for key in 0u64..200 {
            tree.insert(batch, &key.to_be_bytes(), &[7; 128]).unwrap();
            if tree.root() != old_root {
                return key + 1;
            }
        }
        panic!("fixture must split its user tree root");
    }

    // A finished writer must not authorize later releases or stale root pointers.
    // Every rejected batch must leave both files and the published LSN unchanged.
    fn reject_invalid_final_catalog_root(split: bool, timing: &str, dirty: bool) {
        struct NoWrites;
        impl crate::fault::FaultInjector for NoWrites {
            fn check(&self, point: crate::fault::FaultPoint) -> Result<()> {
                panic!("invalid final catalog reached I/O/publication: {point:?}");
            }
        }
        let (temp, pager, table_id) = committed_single_column_catalog();
        let reader = CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
        let root = reader.find_table_by_id(table_id).unwrap().root();
        let lsn = pager.latest_lsn().unwrap();
        drop(pager);
        let wal_path = crate::storage::wal::wal_path(temp.path());
        let before = (
            std::fs::read(temp.path()).unwrap(),
            std::fs::read(&wal_path).unwrap(),
        );
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap()
        .with_faults(std::sync::Arc::new(NoWrites));
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::from_root(TreeKind::Table, root);
        let mutate = |tree: &mut BTree, batch: &mut WriteBatch<'_>| {
            if split {
                split_user_tree(tree, batch);
            } else {
                batch.release_later(root).unwrap();
            }
        };
        if timing == "before" {
            mutate(&mut tree, &mut batch);
        }
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        if dirty {
            writer.allocate_object_id(&mut batch).unwrap();
        }
        if timing == "during" {
            mutate(&mut tree, &mut batch);
        }
        let finish = writer.finish(&mut batch);
        if timing == "after" {
            finish.unwrap();
            mutate(&mut tree, &mut batch);
            assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Corruption);
        } else {
            assert_eq!(finish.unwrap_err().code(), ErrorCode::Corruption);
            assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
        }
        assert_eq!(pager.latest_lsn().unwrap(), lsn);
        drop(pager);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before.0);
        assert_eq!(std::fs::read(wal_path).unwrap(), before.1);
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut read = pager.begin_read().unwrap();
        let reader = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        assert_eq!(reader.find_table_by_id(table_id).unwrap().root(), root);
        reader.validate_deep(&mut read).unwrap();
    }

    #[test]
    fn catalog_pending_root_releases_are_rejected_before_during_and_after_finish() {
        for timing in ["after", "before", "during"] {
            for dirty in [false, true] {
                reject_invalid_final_catalog_root(false, timing, dirty);
            }
        }
    }

    #[test]
    fn catalog_unrecorded_root_splits_are_rejected_before_during_and_after_finish() {
        for timing in ["after", "before", "during"] {
            for dirty in [false, true] {
                reject_invalid_final_catalog_root(true, timing, dirty);
            }
        }
    }

    #[test]
    fn catalog_recorded_root_splits_commit_before_during_and_after_writer_lifetime() {
        for timing in ["before", "during", "after"] {
            let (temp, pager, table_id) = committed_single_column_catalog();
            let reader = CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
            let old_root = reader.find_table_by_id(table_id).unwrap().root();
            let mut batch = pager.begin_write().unwrap();
            let mut tree = BTree::from_root(TreeKind::Table, old_root);
            let (mut writer, count) = if timing == "before" {
                let count = split_user_tree(&mut tree, &mut batch);
                (CatalogWriter::bootstrap(&mut batch).unwrap(), count)
            } else {
                let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
                if timing == "after" {
                    writer.finish(&mut batch).unwrap();
                    let count = split_user_tree(&mut tree, &mut batch);
                    (CatalogWriter::bootstrap(&mut batch).unwrap(), count)
                } else {
                    let count = split_user_tree(&mut tree, &mut batch);
                    (writer, count)
                }
            };
            writer
                .update_table_root(&mut batch, table_id, tree.root())
                .unwrap();
            writer.finish(&mut batch).unwrap();
            batch.commit().unwrap();
            drop(pager);
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            let mut read = pager.begin_read().unwrap();
            let reader = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(
                reader.find_table_by_id(table_id).unwrap().root(),
                tree.root()
            );
            for key in 0..count {
                assert_eq!(
                    tree.get(&mut read, &key.to_be_bytes()).unwrap(),
                    Some(vec![7; 128])
                );
            }
            reader.validate_deep(&mut read).unwrap();
        }
    }

    #[test]
    fn catalog_root_checkpoints_restore_valid_and_invalid_final_states() {
        for split in [false, true] {
            for restore_invalid in [false, true] {
                let (_temp, pager, table_id) = committed_single_column_catalog();
                let reader =
                    CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
                let root = reader.find_table_by_id(table_id).unwrap().root();
                let mut batch = pager.begin_write().unwrap();
                CatalogWriter::bootstrap(&mut batch)
                    .unwrap()
                    .finish(&mut batch)
                    .unwrap();
                let valid = batch.checkpoint_mutation().unwrap();
                if split {
                    split_user_tree(&mut BTree::from_root(TreeKind::Table, root), &mut batch);
                } else {
                    batch.release_later(root).unwrap();
                }
                let invalid = batch.checkpoint_mutation().unwrap();
                batch.restore_mutation(valid);
                // A rolled-back split must not leave the old root superseded.
                CatalogWriter::bootstrap(&mut batch)
                    .unwrap()
                    .finish(&mut batch)
                    .unwrap();
                if restore_invalid {
                    batch.restore_mutation(invalid);
                    assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Corruption);
                } else {
                    batch.commit().unwrap();
                }
            }
        }
    }

    #[test]
    fn catalog_root_can_become_current_again_after_split_then_collapse() {
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let reader = CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
        let root = reader.find_table_by_id(table_id).unwrap().root();
        let mut batch = pager.begin_write().unwrap();
        let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let mut tree = BTree::from_root(TreeKind::Table, root);
        let count = split_user_tree(&mut tree, &mut batch);
        for key in (1..count).rev() {
            assert_eq!(
                tree.delete(&mut batch, &key.to_be_bytes()).unwrap(),
                Some(vec![7; 128])
            );
        }
        assert_eq!(tree.root(), root);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        let reader = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        reader.validate_deep(&mut read).unwrap();
        assert_eq!(
            tree.get(&mut read, &0u64.to_be_bytes()).unwrap(),
            Some(vec![7; 128])
        );
    }

    #[test]
    fn finish_rejects_noncontiguous_or_missing_columns() {
        for ordinal in [None, Some(1)] {
            let (_temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
            let table = new_table(1, root, 1, "T");
            writer.insert_table_definition(&mut batch, &table).unwrap();
            if let Some(ordinal) = ordinal {
                writer
                    .insert_column_definition(&mut batch, &new_column(2, table.id(), ordinal, "C"))
                    .unwrap();
            }
            assert_eq!(
                writer.finish(&mut batch).unwrap_err().code(),
                ErrorCode::Corruption
            );
            assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
            assert_eq!(pager.begin_read().unwrap().catalog_root(), PageId(0));
        }
    }

    #[test]
    fn production_insertions_split_catalog_and_keep_table_scoped_column_names() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let first_root = writer.tree.root();
        for n in 0..24 {
            let id = writer.allocate_object_id(&mut batch).unwrap();
            let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
            let table = new_table(id.get(), root, 2, &format!("T{n}_{}", "x".repeat(80)));
            writer.insert_table_definition(&mut batch, &table).unwrap();
            for (ordinal, name) in [(1, "B"), (0, "A")] {
                let id = writer.allocate_object_id(&mut batch).unwrap();
                writer
                    .insert_column_definition(
                        &mut batch,
                        &new_column(id.get(), table.id(), ordinal, name),
                    )
                    .unwrap();
            }
        }
        assert_ne!(writer.tree.root(), first_root);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let reader = CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
        assert_eq!(reader.image.tables.len(), 24);
        assert_eq!(reader.image.meta.next_object_id().get(), 73);
        for table in reader.image.tables.values() {
            assert_eq!(
                reader
                    .columns_for_table(table.id())
                    .map(|c| c.ordinal())
                    .collect::<Vec<_>>(),
                [0, 1]
            );
            assert_eq!(
                reader
                    .find_column_by_name(table.id(), &CatalogName::new("A").unwrap())
                    .unwrap()
                    .ordinal(),
                0
            );
        }
    }

    #[test]
    fn table_root_replacement_requires_tracked_table_root_and_preserves_definition() {
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let mut read = pager.begin_read().unwrap();
        let reader = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        let old = reader.find_table_by_id(table_id).unwrap().clone();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let index = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        batch.track_tree(TreeKind::Index, index).unwrap();
        let untracked = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        for root in [
            index,
            untracked,
            PageRef::new(old.root().page_id(), old.root().generation() + 1).unwrap(),
        ] {
            let before = current_records(&writer, &mut batch);
            assert_eq!(
                writer
                    .update_table_root(&mut batch, table_id, root)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
            assert_eq!(current_records(&writer, &mut batch), before);
            assert!(!writer.dirty);
        }
        let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        batch.track_tree(TreeKind::Table, root).unwrap();
        writer
            .update_table_root(&mut batch, table_id, root)
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let reader = CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
        let updated = reader.find_table_by_id(table_id).unwrap();
        assert_eq!(updated.root(), root);
        assert_eq!(updated.id(), old.id());
        assert_eq!(updated.schema_version(), old.schema_version());
        assert_eq!(updated.next_rowid(), old.next_rowid());
        assert_eq!(updated.column_count(), old.column_count());
        assert_eq!(updated.name(), old.name());
    }

    #[test]
    fn row_counter_row_bytes_and_split_root_publish_or_rollback_together() {
        for commit in [false, true] {
            let (temp, pager, table_id) = committed_single_column_catalog();
            let mut old_read = pager.begin_read().unwrap();
            let old = CatalogReader::open_empty_or_v1(&mut old_read).unwrap();
            let table = old.find_table_by_id(table_id).unwrap();
            let columns = old.columns_for_table(table_id).cloned().collect::<Vec<_>>();
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            let mut rows = BTree::from_root(TreeKind::Table, table.root());
            for value in 1..=96 {
                let id = writer.allocate_rowid(&mut batch, table_id).unwrap();
                assert_eq!(id.get(), value);
                let encoded =
                    RowCodec::encode(table, &columns, &[crate::Value::Int64(value as i64)], 1024)
                        .unwrap();
                rows.insert(&mut batch, &encode_rowid_key(id), &encoded)
                    .unwrap();
                writer
                    .update_table_root(&mut batch, table_id, rows.root())
                    .unwrap();
            }
            assert_ne!(rows.root(), table.root());
            if commit {
                writer.finish(&mut batch).unwrap();
                batch.commit().unwrap();
            } else {
                let key = encode_rowid_key(RowId::new(96).unwrap());
                assert_eq!(
                    rows.insert(&mut batch, &key, b"injected duplicate before finish")
                        .unwrap_err()
                        .code(),
                    ErrorCode::Constraint
                );
                batch.rollback().unwrap();
                drop(batch);
            }
            assert!(
                BTree::from_root(TreeKind::Table, table.root())
                    .get(&mut old_read, &encode_rowid_key(RowId::new(1).unwrap()))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                old.find_table_by_id(table_id).unwrap().next_rowid().get(),
                1
            );
            drop(old_read);
            drop(pager);
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            let mut read = pager.begin_read().unwrap();
            let reader = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            let current = reader.find_table_by_id(table_id).unwrap();
            assert_eq!(current.next_rowid().get(), if commit { 97 } else { 1 });
            assert_eq!(
                current.root(),
                if commit { rows.root() } else { table.root() }
            );
            let reopened = BTree::from_root(TreeKind::Table, current.root());
            for value in 1..=96 {
                let encoded = reopened
                    .get(&mut read, &encode_rowid_key(RowId::new(value).unwrap()))
                    .unwrap();
                if commit {
                    assert_eq!(
                        RowCodec::decode(current, &columns, &encoded.unwrap(), 1024).unwrap(),
                        [crate::Value::Int64(value as i64)]
                    );
                } else {
                    assert!(encoded.is_none());
                }
            }
        }
    }

    #[test]
    fn rejected_user_root_undoes_both_new_table_records() {
        let (_temp, pager, _) = committed_single_column_catalog();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let user_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let stale = PageRef::new(
            user_root.page_id(),
            user_root.generation().checked_add(1).unwrap(),
        )
        .unwrap();
        let before = current_records(&writer, &mut batch);
        let root = writer.tree.root();
        let pages = batch.page_count().unwrap();
        assert_eq!(
            writer
                .insert_table_definition(&mut batch, &new_table(3, stale, 0, "NEW"))
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(current_records(&writer, &mut batch), before);
        assert_eq!(writer.tree.root(), root);
        assert_eq!(batch.page_count().unwrap(), pages);
        assert!(!writer.dirty);
        writer
            .insert_table_definition(&mut batch, &new_table(3, user_root, 0, "NEW"))
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let reader = CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
        assert_eq!(
            reader
                .find_table_by_name(&CatalogName::new("NEW").unwrap())
                .unwrap()
                .root(),
            user_root
        );
    }

    #[test]
    fn tracked_table_root_check_rejects_stale_split_roots_wrong_types_and_releases() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        let old_root = tree.root();
        assert_eq!(
            batch
                .require_tracked_table_root(old_root)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        for n in 0_u64..96 {
            tree.insert(&mut batch, &n.to_be_bytes(), &[1; 64]).unwrap();
        }
        assert_ne!(tree.root(), old_root);
        batch.require_tracked_table_root(tree.root()).unwrap();
        assert_eq!(
            batch
                .require_tracked_table_root(old_root)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        let index = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        batch.track_tree(TreeKind::Table, index).unwrap();
        assert_eq!(
            batch.require_tracked_table_root(index).unwrap_err().code(),
            ErrorCode::Corruption
        );
        batch.release_btree_later(tree.root()).unwrap();
        assert_eq!(
            batch
                .require_tracked_table_root(tree.root())
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        batch.rollback().unwrap();
        assert_eq!(
            batch
                .require_tracked_table_root(tree.root())
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
    }

    #[test]
    fn allocation_preserves_missing_and_future_record_errors_without_mutation() {
        for future in [false, true] {
            let (_temp, pager, table_id) = committed_single_column_catalog();
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            for key in [catalog_meta_key(), table_definition_key(table_id)] {
                let mut value = writer.tree.delete(&mut batch, &key).unwrap().unwrap();
                if future {
                    value[1] = 2;
                    writer.tree.insert(&mut batch, &key, &value).unwrap();
                }
                let before = current_records(&writer, &mut batch);
                let error = if key == catalog_meta_key() {
                    writer.allocate_object_id(&mut batch).unwrap_err()
                } else {
                    writer.allocate_rowid(&mut batch, table_id).unwrap_err()
                };
                assert_eq!(
                    error.code(),
                    if future {
                        ErrorCode::Unsupported
                    } else {
                        ErrorCode::Corruption
                    }
                );
                assert_eq!(current_records(&writer, &mut batch), before);
                assert!(!writer.dirty);
            }
        }
    }

    #[test]
    fn deleting_committed_row_does_not_recycle_its_rowid() {
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let reader = CatalogReader::open_empty_or_v1(&mut pager.begin_read().unwrap()).unwrap();
        let table = reader.find_table_by_id(table_id).unwrap();
        let columns = reader
            .columns_for_table(table_id)
            .cloned()
            .collect::<Vec<_>>();
        let encoded = RowCodec::encode(table, &columns, &[crate::Value::Int64(42)], 1024).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
        let mut rows = BTree::from_root(TreeKind::Table, table.root());
        rows.insert(&mut batch, &encode_rowid_key(rowid), &encoded)
            .unwrap();
        writer
            .update_table_root(&mut batch, table_id, rows.root())
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(
            rows.delete(&mut batch, &encode_rowid_key(rowid)).unwrap(),
            Some(encoded)
        );
        writer
            .update_table_root(&mut batch, table_id, rows.root())
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        assert_eq!(
            allocate_then_commit(&pager, table_id),
            RowId::new(2).unwrap()
        );
    }

    #[test]
    fn unfinished_catalog_writer_cannot_commit() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let _writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
        let mut read = pager.begin_read().unwrap();
        assert_eq!(read.feature_bits(), 0);
        assert_eq!(read.catalog_root(), PageId(0));
        assert_eq!(read.page_count().unwrap(), 1);
        CatalogReader::open_empty_or_v1(&mut read).unwrap();
    }

    #[test]
    fn bootstrap_publishes_only_one_meta_record_and_reopens_atomically() {
        for page_size in [4096, 16384] {
            let (temp, pager) = test_pager(page_size).unwrap();
            let mut old = pager.begin_read().unwrap();
            let mut batch = pager.begin_write().unwrap();
            let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            let root = writer.tree.root();
            assert_eq!(batch.feature_bits(), CATALOG_V1);
            assert_eq!(batch.catalog_root(), PageId(0));
            writer.finish(&mut batch).unwrap();
            assert_eq!(batch.catalog_root(), root.page_id());
            batch.commit().unwrap();
            assert_eq!(old.feature_bits(), 0);
            assert_eq!(old.catalog_root(), PageId(0));
            CatalogReader::open_empty_or_v1(&mut old).unwrap();
            drop(old);
            drop(pager);

            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(page_size),
            )
            .unwrap();
            let mut read = pager.begin_read().unwrap();
            assert_eq!(read.feature_bits(), CATALOG_V1);
            assert_eq!(read.page_count().unwrap(), 2);
            assert_eq!(read.catalog_root(), root.page_id());
            let reader = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(reader.image.meta.next_object_id().get(), 1);
            let (tree, _) = read
                .catalog_tree_with_report(TreeKind::Table)
                .unwrap()
                .unwrap();
            assert_eq!(tree.root(), root);
            let mut cursor = tree
                .scan(&mut read, Bound::Unbounded, Bound::Unbounded)
                .unwrap();
            let (key, value) = cursor.next().unwrap().unwrap();
            assert_eq!(key, [0]);
            assert_eq!(
                decode_catalog_meta(&value).unwrap().next_object_id().get(),
                1
            );
            assert!(cursor.next().unwrap().is_none());
        }
    }

    #[test]
    fn repeated_bootstrap_never_allocates_another_root_or_changes_persisted_bytes() {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let root = writer.tree.root();
        let pages = batch.page_count().unwrap();
        let error = match CatalogWriter::bootstrap(&mut batch) {
            Ok(_) => panic!("a second pending writer must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Transaction);
        assert_eq!(batch.page_count().unwrap(), pages);
        writer.finish(&mut batch).unwrap();
        let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(writer.tree.root(), root);
        assert_eq!(batch.page_count().unwrap(), pages);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        drop(pager);
        let before = std::fs::read(temp.path()).unwrap();
        let wal_path = crate::storage::wal::wal_path(temp.path());
        let wal_before = std::fs::read(&wal_path).unwrap();

        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let lsn = pager.latest_lsn().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(writer.tree.root(), root);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        assert_eq!(pager.latest_lsn().unwrap(), lsn);
        drop(pager);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);
        assert_eq!(std::fs::read(wal_path).unwrap(), wal_before);
    }

    #[test]
    fn bootstrap_rejects_legacy_root_before_decoding_its_records() {
        for valid_meta in [false, true] {
            let (_temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let rows = if valid_meta {
                vec![(
                    catalog_meta_key(),
                    encode_catalog_meta(&CatalogMeta::new(ObjectId::new(1).unwrap())),
                )]
            } else {
                vec![(vec![0xff], vec![0xff])]
            };
            let tree = raw_tree(&mut batch, rows);
            batch.set_catalog_root(tree.root()).unwrap();
            let pages = batch.page_count().unwrap();
            let error = match CatalogWriter::bootstrap(&mut batch) {
                Ok(_) => panic!("legacy catalog was interpreted as V1"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Unsupported);
            assert_eq!(batch.feature_bits(), 0);
            assert_eq!(batch.catalog_root(), tree.root().page_id());
            assert_eq!(batch.page_count().unwrap(), pages);
            // A refused bootstrap must not leave its gate or mutations behind.
            batch.commit().unwrap();
        }
    }

    #[test]
    fn bootstrap_rejects_v1_without_root_instead_of_rebuilding() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        batch.enable_feature_bits(CATALOG_V1).unwrap();
        batch.allocate(PageType::Catalog).unwrap();
        let pages = batch.page_count().unwrap();
        let error = match CatalogWriter::bootstrap(&mut batch) {
            Ok(_) => panic!("inconsistent catalog was silently rebuilt"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Corruption);
        assert_eq!(batch.catalog_root(), PageId(0));
        assert_eq!(batch.feature_bits(), CATALOG_V1);
        assert_eq!(batch.page_count().unwrap(), pages);
        // The low-level pager can still persist this intentionally inconsistent fixture.
        batch.commit().unwrap();
    }

    #[test]
    fn bootstrap_preserves_candidate_decoder_error_classifications() {
        let meta = encode_catalog_meta(&CatalogMeta::new(ObjectId::new(1).unwrap()));
        let mut newer_meta = meta.clone();
        newer_meta[1] = 2;
        for (rows, expected) in [
            (vec![], ErrorCode::Corruption),
            (
                vec![(catalog_meta_key(), vec![0; 16])],
                ErrorCode::Corruption,
            ),
            (
                vec![(catalog_meta_key(), newer_meta)],
                ErrorCode::Unsupported,
            ),
            (
                vec![(catalog_meta_key(), meta.clone()), (vec![0xff], vec![])],
                ErrorCode::Unsupported,
            ),
            (
                vec![(catalog_meta_key(), meta.clone()), (vec![0, 0], meta)],
                ErrorCode::Corruption,
            ),
        ] {
            let (_temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let tree = raw_tree(&mut batch, rows);
            publish(&mut batch, &tree);
            let pages = batch.page_count().unwrap();
            let error = match CatalogWriter::bootstrap(&mut batch) {
                Ok(_) => panic!("malformed catalog opened"),
                Err(error) => error,
            };
            assert_eq!(error.code(), expected);
            assert_eq!(batch.catalog_root(), tree.root().page_id());
            assert_eq!(batch.page_count().unwrap(), pages);
            batch.commit().unwrap();
        }
    }

    #[test]
    fn bootstrap_rejects_index_catalog_root() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
        batch.track_tree(TreeKind::Index, tree.root()).unwrap();
        publish(&mut batch, &tree);
        let error = match CatalogWriter::bootstrap(&mut batch) {
            Ok(_) => panic!("index catalog opened"),
            Err(error) => error,
        };
        assert_eq!(error.code(), ErrorCode::Corruption);
        batch.commit().unwrap();
    }

    #[test]
    fn rollback_before_or_after_catalog_finish_leaves_uninitialized_files_unchanged() {
        for finish in [false, true] {
            let (temp, pager) = test_pager(4096).unwrap();
            drop(pager);
            let before = std::fs::read(temp.path()).unwrap();
            let wal_path = crate::storage::wal::wal_path(temp.path());
            let wal_before = std::fs::read(&wal_path).unwrap();
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            let mut batch = pager.begin_write().unwrap();
            let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            if finish {
                writer.finish(&mut batch).unwrap();
            }
            batch.rollback().unwrap();
            drop(batch);
            drop(pager);
            assert_eq!(std::fs::read(temp.path()).unwrap(), before);
            assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            let mut read = pager.begin_read().unwrap();
            assert_eq!(read.feature_bits(), 0);
            assert_eq!(read.catalog_root(), PageId(0));
            assert_eq!(read.page_count().unwrap(), 1);
            CatalogReader::open_empty_or_v1(&mut read).unwrap();
        }
    }

    #[test]
    fn failed_catalog_finish_keeps_commit_gate_closed() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        writer.tree.delete(&mut batch, &catalog_meta_key()).unwrap();
        assert_eq!(
            writer.finish(&mut batch).unwrap_err().code(),
            ErrorCode::Corruption
        );
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
        let mut read = pager.begin_read().unwrap();
        assert_eq!(read.feature_bits(), 0);
        assert_eq!(read.catalog_root(), PageId(0));
        CatalogReader::open_empty_or_v1(&mut read).unwrap();
    }

    #[test]
    fn reopening_existing_catalog_still_requires_finish() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        CatalogWriter::bootstrap(&mut batch)
            .unwrap()
            .finish(&mut batch)
            .unwrap();
        batch.commit().unwrap();
        let lsn = pager.latest_lsn().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let _writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
        assert_eq!(pager.latest_lsn().unwrap(), lsn);
    }

    #[test]
    fn clean_writer_finish_revalidates_meta_and_keeps_failed_batch_uncommittable() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        CatalogWriter::bootstrap(&mut batch)
            .unwrap()
            .finish(&mut batch)
            .unwrap();
        batch.commit().unwrap();
        let lsn = pager.latest_lsn().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        // Simulate a lower-level mutation after a clean open: the gate must not
        // trust only feature/root presence or the earlier bootstrap validation.
        let mut tree = BTree::from_root(TreeKind::Table, writer.tree.root());
        tree.delete(&mut batch, &catalog_meta_key()).unwrap();
        assert_eq!(
            writer.finish(&mut batch).unwrap_err().code(),
            ErrorCode::Corruption
        );
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
        assert_eq!(pager.latest_lsn().unwrap(), lsn);
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap()
                .image
                .meta
                .next_object_id()
                .get(),
            1
        );
    }

    #[test]
    fn bootstrap_reuses_freelist_root_with_its_current_generation() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let spare = batch.allocate_append(PageType::TableLeaf).unwrap();
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        batch.release_later(spare).unwrap();
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(writer.tree.root().page_id(), spare.page_id());
        assert_eq!(writer.tree.root().generation(), 2);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        let (tree, _) = read
            .catalog_tree_with_report(TreeKind::Table)
            .unwrap()
            .unwrap();
        assert_eq!(tree.root().generation(), 2);
        assert_eq!(read.page_count().unwrap(), 2);
        CatalogReader::open_empty_or_v1(&mut read).unwrap();
    }

    #[test]
    fn catalog_finish_publishes_final_split_root_and_all_records() {
        for existing in [false, true] {
            let (temp, pager) = test_pager(4096).unwrap();
            if existing {
                let mut batch = pager.begin_write().unwrap();
                CatalogWriter::bootstrap(&mut batch)
                    .unwrap()
                    .finish(&mut batch)
                    .unwrap();
                batch.commit().unwrap();
            }
            let mut old = pager.begin_read().unwrap();
            let mut batch = pager.begin_write().unwrap();
            let header_root = batch.catalog_root();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            let initial_root = writer.tree.root();
            grow_catalog_for_split(&mut writer, &mut batch);
            let final_root = writer.tree.root();
            assert_ne!(
                final_root, initial_root,
                "fixture must split the directory root"
            );
            assert_eq!(
                batch.catalog_root(),
                header_root,
                "finish is the publication route"
            );
            writer.finish(&mut batch).unwrap();
            assert_eq!(batch.catalog_root(), final_root.page_id());
            batch.commit().unwrap();
            assert!(
                CatalogReader::open_empty_or_v1(&mut old)
                    .unwrap()
                    .image
                    .tables
                    .is_empty()
            );
            drop(old);
            drop(pager);
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            let mut read = pager.begin_read().unwrap();
            assert_eq!(read.catalog_root(), final_root.page_id());
            let reader = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(reader.image.tables.len(), 40);
            for id in 10..50 {
                assert!(reader.find_table_by_id(TableId::new(id).unwrap()).is_some());
            }
        }
    }

    #[test]
    fn rollback_or_omitted_finish_after_a_split_keeps_committed_catalog_unchanged() {
        for omit_finish in [false, true] {
            let (temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            let root = writer.tree.root();
            writer.finish(&mut batch).unwrap();
            batch.commit().unwrap();
            let lsn = pager.latest_lsn().unwrap();
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            grow_catalog_for_split(&mut writer, &mut batch);
            assert_ne!(writer.tree.root(), root);
            if omit_finish {
                assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Transaction);
            } else {
                writer.finish(&mut batch).unwrap();
                batch.rollback().unwrap();
                drop(batch);
            }
            assert_eq!(pager.latest_lsn().unwrap(), lsn);
            drop(pager);
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            let mut read = pager.begin_read().unwrap();
            assert_eq!(read.catalog_root(), root.page_id());
            assert_eq!(read.feature_bits(), CATALOG_V1);
            assert_eq!(read.page_count().unwrap(), 2);
            assert!(
                CatalogReader::open_empty_or_v1(&mut read)
                    .unwrap()
                    .image
                    .tables
                    .is_empty()
            );
        }
    }

    fn grow_catalog_for_split(writer: &mut CatalogWriter, batch: &mut WriteBatch<'_>) {
        for id in 10..50 {
            let root = BTree::create(batch, TreeKind::Table).unwrap().root();
            let table = TableDef::new(
                TableId::new(id).unwrap(),
                SchemaVersion::new(1).unwrap(),
                root,
                RowId::new(1).unwrap(),
                0,
                CatalogName::new(format!("T{id}_{}", "X".repeat(80))).unwrap(),
            )
            .unwrap();
            writer
                .tree
                .insert(
                    batch,
                    &table_name_key(table.name()).unwrap(),
                    &encode_table_name_mapping(table.id()),
                )
                .unwrap();
            writer
                .tree
                .insert(
                    batch,
                    &table_definition_key(table.id()),
                    &encode_table_definition(&table),
                )
                .unwrap();
        }
        writer.dirty = true;
    }

    fn table(root: PageRef) -> TableDef {
        TableDef::new(
            TableId::new(10).unwrap(),
            SchemaVersion::new(1).unwrap(),
            root,
            RowId::new(1).unwrap(),
            2,
            CatalogName::new("T").unwrap(),
        )
        .unwrap()
    }

    fn column(id: u64, ordinal: u16, name: &str) -> ColumnDef {
        ColumnDef::new(
            ColumnId::new(id).unwrap(),
            TableId::new(10).unwrap(),
            ordinal,
            ColumnType::Int64,
            false,
            CatalogName::new(name).unwrap(),
        )
        .unwrap()
    }

    fn records(root: PageRef) -> Records {
        let table = table(root);
        let mut records = vec![
            (
                catalog_meta_key(),
                encode_catalog_meta(&CatalogMeta::new(ObjectId::new(100).unwrap())),
            ),
            (
                table_name_key(table.name()).unwrap(),
                encode_table_name_mapping(table.id()),
            ),
            (
                table_definition_key(table.id()),
                encode_table_definition(&table),
            ),
        ];
        for col in [column(20, 0, "A"), column(21, 1, "B")] {
            records.extend([
                (
                    column_name_key(col.table_id(), col.name()).unwrap(),
                    encode_column_name_mapping(col.id()),
                ),
                (
                    column_ordinal_key(col.table_id(), col.ordinal()),
                    encode_column_ordinal_mapping(col.id()),
                ),
                (
                    column_definition_key(col.id()),
                    encode_column_definition(&col),
                ),
            ]);
        }
        records
    }

    fn raw_tree(batch: &mut WriteBatch<'_>, records: Records) -> BTree {
        let mut tree = BTree::create(batch, TreeKind::Table).unwrap();
        for (key, value) in records {
            tree.insert(batch, &key, &value).unwrap();
        }
        batch.track_tree(TreeKind::Table, tree.root()).unwrap();
        tree
    }

    fn publish(batch: &mut WriteBatch<'_>, tree: &BTree) {
        batch.enable_feature_bits(CATALOG_V1).unwrap();
        batch.set_catalog_root(tree.root()).unwrap();
    }

    #[test]
    fn valid_image_supports_immutable_ordered_lookups_and_snapshot_isolation() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut old = pager.begin_read().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let user = BTree::create(&mut batch, TreeKind::Table).unwrap();
        let tree = raw_tree(&mut batch, records(user.root()));
        // Candidate validation must work before the root is published in the header.
        let candidate = CatalogReader::open_candidate(&mut batch, &tree, CATALOG_V1).unwrap();
        publish(&mut batch, &tree);
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        let image = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        drop(read);
        for catalog in [candidate, image] {
            let name = CatalogName::new("T").unwrap();
            let table = catalog.find_table_by_name(&name).unwrap();
            assert_eq!(table.id().get(), 10);
            assert!(std::ptr::eq(
                table,
                catalog.find_table_by_id(table.id()).unwrap()
            ));
            let columns: Vec<_> = catalog.columns_for_table(table.id()).collect();
            assert_eq!(
                columns.iter().map(|col| col.id().get()).collect::<Vec<_>>(),
                [20, 21]
            );
            assert!(std::ptr::eq(
                columns[0],
                catalog
                    .find_column_by_name(table.id(), &CatalogName::new("A").unwrap())
                    .unwrap()
            ));
            assert!(
                catalog
                    .find_table_by_name(&CatalogName::new("t").unwrap())
                    .is_none()
            );
            assert!(
                catalog
                    .find_table_by_id(TableId::new(99).unwrap())
                    .is_none()
            );
            assert!(
                catalog
                    .columns_for_table(TableId::new(99).unwrap())
                    .next()
                    .is_none()
            );
            assert!(
                catalog
                    .find_column_by_name(table.id(), &CatalogName::new("absent").unwrap())
                    .is_none()
            );
        }
        assert!(
            CatalogReader::open_empty_or_v1(&mut old)
                .unwrap()
                .find_table_by_id(TableId::new(10).unwrap())
                .is_none()
        );
    }

    #[test]
    fn feature_root_combinations_have_exact_classifications() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        // Legacy roots must be rejected without even interpreting their contents.
        let tree = raw_tree(&mut batch, vec![(vec![0xff], vec![])]);
        assert_eq!(
            CatalogReader::open_candidate(&mut batch, &tree, 0)
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );
        batch.set_catalog_root(tree.root()).unwrap();
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );

        let (_temp2, pager2) = test_pager(4096).unwrap();
        let mut batch = pager2.begin_write().unwrap();
        batch.enable_feature_bits(CATALOG_V1).unwrap();
        batch.allocate_append(PageType::Catalog).unwrap();
        batch.commit().unwrap();
        let mut read = pager2.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn initialized_meta_only_image_is_valid() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let tree = raw_tree(
            &mut batch,
            vec![(
                catalog_meta_key(),
                encode_catalog_meta(&CatalogMeta::new(ObjectId::new(1).unwrap())),
            )],
        );
        let reader = CatalogReader::open_candidate(&mut batch, &tree, CATALOG_V1).unwrap();
        assert!(reader.find_table_by_id(TableId::new(1).unwrap()).is_none());
    }

    #[test]
    fn every_missing_record_is_corruption_in_candidate_and_snapshot() {
        for missing in 0..9 {
            assert_bad_records(
                &format!("missing record {missing}"),
                ErrorCode::Corruption,
                |rows, _| {
                    rows.remove(missing);
                },
            );
        }
    }

    fn assert_bad_records(
        label: &str,
        expected: ErrorCode,
        mutate: impl FnOnce(&mut Records, PageRef),
    ) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut rows = records(root);
        mutate(&mut rows, root);
        let tree = raw_tree(&mut batch, rows);
        assert_eq!(
            CatalogReader::open_candidate(&mut batch, &tree, CATALOG_V1)
                .unwrap_err()
                .code(),
            expected,
            "candidate: {label}"
        );
        publish(&mut batch, &tree);
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap_err()
                .code(),
            expected,
            "snapshot: {label}"
        );
        drop(read);
        drop(pager);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let error = match Pager::open(temp.path(), mode, PagerConfig::test_default(4096)) {
                Ok(_) => panic!("recovery accepted {label}"),
                Err(error) => error,
            };
            assert_eq!(error.code(), expected, "recovery: {label}");
        }
    }

    #[test]
    fn duplicate_reverse_mappings_and_dangling_or_wrong_kind_ids_are_corruption() {
        for mapping in [1, 3, 4] {
            assert_bad_records(
                "duplicate reverse mapping",
                ErrorCode::Corruption,
                |rows, _| {
                    let mut duplicate = rows[mapping].clone();
                    match mapping {
                        1 => {
                            duplicate.0 =
                                table_name_key(&CatalogName::new("ALIAS").unwrap()).unwrap()
                        }
                        3 => {
                            duplicate.0 = column_name_key(
                                TableId::new(10).unwrap(),
                                &CatalogName::new("ALIAS").unwrap(),
                            )
                            .unwrap()
                        }
                        _ => duplicate.0 = column_ordinal_key(TableId::new(10).unwrap(), 2),
                    }
                    rows.push(duplicate);
                },
            );
            for target in [99_u64, if mapping == 1 { 20 } else { 10 }] {
                assert_bad_records(
                    "dangling or wrong object kind",
                    ErrorCode::Corruption,
                    |rows, _| {
                        rows[mapping].1[8..16].copy_from_slice(&target.to_le_bytes());
                    },
                );
            }
        }
    }

    #[test]
    fn mismatched_definition_ids_names_ownership_ordinals_and_counts_are_corruption() {
        for definition in [2, 5] {
            assert_bad_records(
                "definition key/payload mismatch",
                ErrorCode::Corruption,
                |rows, _| {
                    rows[definition].1[8..16].copy_from_slice(&99_u64.to_le_bytes());
                },
            );
        }
        assert_bad_records(
            "table name mismatch",
            ErrorCode::Corruption,
            |rows, root| {
                let table = TableDef::new(
                    TableId::new(10).unwrap(),
                    SchemaVersion::new(1).unwrap(),
                    root,
                    RowId::new(1).unwrap(),
                    2,
                    CatalogName::new("OTHER").unwrap(),
                )
                .unwrap();
                rows[2].1 = encode_table_definition(&table);
            },
        );
        assert_bad_records("column name mismatch", ErrorCode::Corruption, |rows, _| {
            rows[5].1 = encode_column_definition(&column(20, 0, "OTHER"));
        });
        assert_bad_records("wrong table ownership", ErrorCode::Corruption, |rows, _| {
            rows[5].1[16..24].copy_from_slice(&99_u64.to_le_bytes());
        });
        assert_bad_records(
            "non-contiguous ordinals",
            ErrorCode::Corruption,
            |rows, _| {
                rows[7].0 = column_ordinal_key(TableId::new(10).unwrap(), 2);
                rows[8].1 = encode_column_definition(&column(21, 2, "B"));
            },
        );
        assert_bad_records("ordinal mismatch", ErrorCode::Corruption, |rows, _| {
            rows[8].1 = encode_column_definition(&column(21, 0, "B"));
        });
        assert_bad_records("count mismatch", ErrorCode::Corruption, |rows, _| {
            rows[2].1[48..50].copy_from_slice(&1_u16.to_le_bytes());
        });
        assert_bad_records(
            "duplicate global object ID",
            ErrorCode::Corruption,
            |rows, _| {
                let id = ColumnId::new(10).unwrap();
                rows[3].1 = encode_column_name_mapping(id);
                rows[4].1 = encode_column_ordinal_mapping(id);
                rows[5] = (
                    column_definition_key(id),
                    encode_column_definition(&column(10, 0, "A")),
                );
            },
        );
    }

    #[test]
    fn reserved_families_versions_and_wrong_record_kinds_keep_decoder_classifications() {
        for family in [0x15, 0x20, 0x21, 0x30, 0xff] {
            assert_bad_records("reserved family", ErrorCode::Unsupported, |rows, _| {
                rows.push((vec![family], vec![]));
            });
        }
        assert_bad_records("newer record", ErrorCode::Unsupported, |rows, _| {
            rows[0].1[1] = 2;
        });
        assert_bad_records("wrong record kind", ErrorCode::Corruption, |rows, _| {
            rows[1].1[0] = 0x12;
        });
        assert_bad_records(
            "duplicate malformed meta key",
            ErrorCode::Corruption,
            |rows, _| {
                rows.push((vec![0, 0], rows[0].1.clone()));
            },
        );
    }

    #[test]
    fn orphaned_column_owners_and_extra_reverse_mappings_are_corruption() {
        assert_bad_records(
            "fully mapped orphan column",
            ErrorCode::Corruption,
            |rows, _| {
                let orphan = TableId::new(99).unwrap();
                rows[3].0 = column_name_key(orphan, &CatalogName::new("A").unwrap()).unwrap();
                rows[4].0 = column_ordinal_key(orphan, 0);
                rows[5].1[16..24].copy_from_slice(&99_u64.to_le_bytes());
                // Keep T's remaining column sequence and count valid so the absent
                // owner, not an unrelated count or ordinal error, is what is tested.
                rows[2].1[48..50].copy_from_slice(&1_u16.to_le_bytes());
                rows[7].0 = column_ordinal_key(TableId::new(10).unwrap(), 0);
                rows[8].1 = encode_column_definition(&column(21, 0, "B"));
            },
        );
        for mapping in [3, 4] {
            assert_bad_records(
                "extra reverse mapping under absent owner",
                ErrorCode::Corruption,
                |rows, _| {
                    let mut duplicate = rows[mapping].clone();
                    duplicate.0[1..9].copy_from_slice(&99_u64.to_be_bytes());
                    rows.push(duplicate);
                },
            );
        }
    }

    #[test]
    fn shallow_user_roots_require_generation_type_bounds_and_disjointness() {
        for wrong_type in [
            PageType::IndexLeaf,
            PageType::IndexInternal,
            PageType::Catalog,
            PageType::Overflow,
            PageType::Freelist,
        ] {
            let (_temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let wrong = batch.allocate(wrong_type).unwrap();
            let tree = raw_tree(&mut batch, records(wrong));
            assert_eq!(
                CatalogReader::open_candidate(&mut batch, &tree, CATALOG_V1)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }
        assert_bad_records(
            "wrong root generation",
            ErrorCode::Corruption,
            |rows, root| {
                rows[2].1 = encode_table_definition(&table(
                    PageRef::new(root.page_id(), root.generation() + 1).unwrap(),
                ));
            },
        );
        assert_bad_records("out of bounds root", ErrorCode::Corruption, |rows, _| {
            rows[2].1 = encode_table_definition(&table(PageRef::new(PageId(u64::MAX), 1).unwrap()));
        });
        assert_bad_records(
            "duplicate user root",
            ErrorCode::Corruption,
            |rows, root| {
                let second = TableDef::new(
                    TableId::new(30).unwrap(),
                    SchemaVersion::new(1).unwrap(),
                    root,
                    RowId::new(1).unwrap(),
                    0,
                    CatalogName::new("U").unwrap(),
                )
                .unwrap();
                rows.push((
                    table_name_key(second.name()).unwrap(),
                    encode_table_name_mapping(second.id()),
                ));
                rows.push((
                    table_definition_key(second.id()),
                    encode_table_definition(&second),
                ));
            },
        );
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Table).unwrap();
        for (key, value) in records(tree.root()) {
            tree.insert(&mut batch, &key, &value).unwrap();
        }
        assert_eq!(
            CatalogReader::open_candidate(&mut batch, &tree, CATALOG_V1)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn candidate_catalog_requires_table_tree_kind() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let index = BTree::create(&mut batch, TreeKind::Index).unwrap();
        assert_eq!(
            CatalogReader::open_candidate(&mut batch, &index, CATALOG_V1)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn batch_and_snapshot_catalog_helpers_resolve_reused_root_generation() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let first = batch.allocate_append(PageType::TableLeaf).unwrap();
        let second = batch.allocate_append(PageType::TableLeaf).unwrap();
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        batch.release_later(first).unwrap();
        batch.release_later(second).unwrap();
        batch.commit().unwrap();
        let mut old = pager.begin_read().unwrap();
        let mut batch = pager.begin_write().unwrap();
        assert!(
            batch
                .catalog_tree_with_report(TreeKind::Table)
                .unwrap()
                .is_none()
        );
        let tree = raw_tree(
            &mut batch,
            vec![(
                catalog_meta_key(),
                encode_catalog_meta(&CatalogMeta::new(ObjectId::new(1).unwrap())),
            )],
        );
        assert_eq!(tree.root().page_id(), second.page_id());
        assert_eq!(tree.root().generation(), 2);
        publish(&mut batch, &tree);
        let (resolved, report) = batch
            .catalog_tree_with_report(TreeKind::Table)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.root(), tree.root());
        assert_eq!(
            report.all_live_pages(),
            BTreeSet::from([tree.root().page_id()])
        );
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        let (resolved, report) = read
            .catalog_tree_with_report(TreeKind::Table)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.root(), tree.root());
        assert_eq!(
            report.all_live_pages(),
            BTreeSet::from([tree.root().page_id()])
        );
        assert!(
            old.catalog_tree_with_report(TreeKind::Table)
                .unwrap()
                .is_none()
        );
        CatalogReader::open_empty_or_v1(&mut read).unwrap();
    }

    #[test]
    fn user_root_in_freelist_is_corruption_in_candidate_and_snapshot() {
        use crate::storage::freelist::Freelist;
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let user = BTree::create(&mut batch, TreeKind::Table).unwrap();
        let spare = batch.allocate_append(PageType::TableLeaf).unwrap();
        let tree = raw_tree(&mut batch, records(user.root()));
        publish(&mut batch, &tree);
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        batch.release_later(spare).unwrap();
        batch.commit().unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut trunk = Freelist::decode(batch.modify(spare).unwrap()).unwrap();
        Freelist::release(&mut trunk, user.root().page_id(), 4096).unwrap();
        Freelist::write(batch.modify(spare).unwrap(), &trunk).unwrap();
        assert_eq!(
            CatalogReader::open_candidate(&mut batch, &tree, CATALOG_V1)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        // Phase 3's pager knows only the catalog's physical live pages. This fixture
        // proves that the new reader also excludes roots named by catalog records.
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        drop(read);
        drop(pager);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let error = match Pager::open(temp.path(), mode, PagerConfig::test_default(4096)) {
                Ok(_) => panic!("recovery accepted a free user root"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Corruption);
        }
    }

    #[test]
    fn recovery_reopen_discards_an_incomplete_v1_tail_without_mixing_catalogs() {
        use crate::fault::{FaultInjector, FaultPoint};
        struct TornPage;
        impl FaultInjector for TornPage {
            fn check(&self, point: FaultPoint) -> Result<()> {
                if point == FaultPoint::AfterWalRecordPayloadPrefixWrite {
                    return Err(DbError::new(ErrorCode::Io, "injected incomplete V1 tail"));
                }
                Ok(())
            }
        }
        let (temp, pager, table_id) = committed_single_column_catalog();
        let committed_lsn = pager.latest_lsn().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let original = CatalogReader::open_empty_or_v1(&mut snapshot).unwrap();
        drop(snapshot);
        let pager = pager.with_faults(std::sync::Arc::new(TornPage));
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(
            writer.allocate_rowid(&mut batch, table_id).unwrap().get(),
            1
        );
        let id = writer.allocate_object_id(&mut batch).unwrap();
        let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        writer
            .insert_table_definition(&mut batch, &new_table(id.get(), root, 0, "INCOMPLETE"))
            .unwrap();
        writer.finish(&mut batch).unwrap();
        assert_eq!(batch.commit().unwrap_err().code(), ErrorCode::Io);
        drop(pager);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let pager = Pager::open(temp.path(), mode, PagerConfig::test_default(4096)).unwrap();
            let mut read = pager.begin_read().unwrap();
            assert_eq!(read.lsn(), committed_lsn);
            let recovered = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(recovered.image, original.image);
            recovered.validate_deep(&mut read).unwrap();
        }
    }

    #[test]
    fn deep_catalog_validation_handles_empty_catalog_and_rejects_mismatched_image() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut empty_snapshot = pager.begin_read().unwrap();
        let empty = CatalogReader::open_empty_or_v1(&mut empty_snapshot).unwrap();
        empty.validate_deep(&mut empty_snapshot).unwrap();
        let mut batch = pager.begin_write().unwrap();
        CatalogWriter::bootstrap(&mut batch)
            .unwrap()
            .finish(&mut batch)
            .unwrap();
        batch.commit().unwrap();
        let mut initialized = pager.begin_read().unwrap();
        let catalog = CatalogReader::open_empty_or_v1(&mut initialized).unwrap();
        catalog.validate_deep(&mut initialized).unwrap();
        drop(initialized);
        drop(empty_snapshot);

        let (_temp, pager, table_id) = committed_single_column_catalog();
        let mut old = pager.begin_read().unwrap();
        let catalog = CatalogReader::open_empty_or_v1(&mut old).unwrap();
        allocate_then_commit(&pager, table_id);
        let mut latest = pager.begin_read().unwrap();
        assert_eq!(
            catalog.validate_deep(&mut latest).unwrap_err().code(),
            ErrorCode::Transaction
        );
        catalog.validate_deep(&mut old).unwrap();
    }

    #[test]
    fn recovery_reopen_rejects_complete_v1_catalog_state_defects() {
        for checkpoint in [false, true] {
            for defect in [
                "missing root",
                "missing meta",
                "mapping",
                "schema",
                "generation",
                "type",
                "bounds",
                "record version",
                "meta version",
                "catalog type",
            ] {
                let (temp, pager) = test_pager(4096).unwrap();
                let mut batch = pager.begin_write().unwrap();
                if defect == "missing root" {
                    batch.allocate_append(PageType::TableLeaf).unwrap();
                } else {
                    CatalogWriter::bootstrap(&mut batch)
                        .unwrap()
                        .finish(&mut batch)
                        .unwrap();
                }
                batch.commit().unwrap();
                let previous_lsn = pager.latest_lsn().unwrap();
                let mut batch = pager.begin_write().unwrap();
                if defect == "missing root" {
                    batch.enable_feature_bits(CATALOG_V1).unwrap();
                    batch.allocate_append(PageType::TableLeaf).unwrap();
                } else {
                    let kind = if defect == "type" {
                        TreeKind::Index
                    } else {
                        TreeKind::Table
                    };
                    let user = BTree::create(&mut batch, kind).unwrap();
                    let mut rows = records(user.root());
                    match defect {
                        "missing meta" => {
                            rows.remove(0);
                        }
                        "mapping" => {
                            rows.remove(1);
                        }
                        "schema" => rows[2].1[16..24].fill(0),
                        "generation" => rows[2].1[32..40].copy_from_slice(&2_u64.to_le_bytes()),
                        "bounds" => rows[2].1[24..32].copy_from_slice(&u64::MAX.to_le_bytes()),
                        "record version" => rows[0].1[1] = 2,
                        "meta version" => rows[0].1[8..10].fill(0),
                        "type" | "catalog type" => {}
                        _ => unreachable!(),
                    }
                    let tree = if defect == "catalog type" {
                        let mut tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
                        for (key, value) in rows {
                            tree.insert(&mut batch, &key, &value).unwrap();
                        }
                        tree
                    } else {
                        raw_tree(&mut batch, rows)
                    };
                    publish(&mut batch, &tree);
                }
                // The low-level storage fixture commits complete checksummed bytes,
                // bypassing CatalogWriter::finish deliberately. Recovery may not hide
                // this durable corruption by selecting the older complete commit.
                batch.commit().unwrap();
                assert!(pager.latest_lsn().unwrap() > previous_lsn);
                if checkpoint {
                    pager.checkpoint().unwrap();
                }
                drop(pager);
                let before = std::fs::read(temp.path()).unwrap();
                let wal_path = crate::storage::wal::wal_path(temp.path());
                let wal_before = std::fs::read(&wal_path).unwrap();
                for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
                    let error =
                        match Pager::open(temp.path(), mode, PagerConfig::test_default(4096)) {
                            Ok(_) => panic!("recovery published {defect}, checkpoint={checkpoint}"),
                            Err(error) => error,
                        };
                    let expected = if defect == "record version" {
                        ErrorCode::Unsupported
                    } else {
                        ErrorCode::Corruption
                    };
                    assert_eq!(error.code(), expected, "{defect}, checkpoint={checkpoint}");
                    assert_eq!(std::fs::read(temp.path()).unwrap(), before);
                    assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
                }
            }
        }
    }

    #[test]
    fn old_catalog_snapshot_survives_new_rows_tables_checkpoint_and_reopen() {
        let (temp, pager, table_id) = committed_single_column_catalog();
        let mut old_snapshot = pager.begin_read().unwrap();
        let old = CatalogReader::open_empty_or_v1(&mut old_snapshot).unwrap();
        let old_table = old.find_table_by_id(table_id).unwrap().clone();
        let old_columns = old.columns_for_table(table_id).cloned().collect::<Vec<_>>();
        let old_lsn = old_snapshot.lsn();
        let old_root = old_snapshot.catalog_root();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let mut rows = BTree::from_root(TreeKind::Table, old_table.root());
        for value in 1..=96 {
            let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
            assert_eq!(rowid.get(), value);
            let encoded = RowCodec::encode(
                &old_table,
                &old_columns,
                &[crate::Value::Int64(i64::try_from(value).unwrap())],
                1024,
            )
            .unwrap();
            rows.insert(&mut batch, &encode_rowid_key(rowid), &encoded)
                .unwrap();
        }
        writer
            .update_table_root(&mut batch, table_id, rows.root())
            .unwrap();
        for index in 0..48 {
            let id = writer.allocate_object_id(&mut batch).unwrap();
            let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
            writer
                .insert_table_definition(
                    &mut batch,
                    &new_table(id.get(), root, 0, &format!("NEW{index}")),
                )
                .unwrap();
        }
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let latest_lsn = pager.latest_lsn().unwrap();
        let first_checkpoint = pager.checkpoint().unwrap();
        assert_eq!(first_checkpoint.to_lsn, old_lsn);
        assert!(!first_checkpoint.wal_reset);
        assert_eq!(old_snapshot.lsn(), old_lsn);
        assert_eq!(old_snapshot.catalog_root(), old_root);
        assert_eq!(old.find_table_by_id(table_id).unwrap(), &old_table);
        assert_eq!(
            old.columns_for_table(table_id).cloned().collect::<Vec<_>>(),
            old_columns
        );
        assert_eq!(old_table.next_rowid().get(), 1);
        assert_eq!(old.image.meta.next_object_id().get(), 3);
        assert_eq!(old.image.tables.len(), 1);
        let reread = CatalogReader::open_empty_or_v1(&mut old_snapshot).unwrap();
        assert_eq!(reread.image, old.image);
        old.validate_deep(&mut old_snapshot).unwrap();
        let mut latest_snapshot = pager.begin_read().unwrap();
        let latest = CatalogReader::open_empty_or_v1(&mut latest_snapshot).unwrap();
        assert_ne!(latest_snapshot.catalog_root(), old_root);
        assert_eq!(latest.image.tables.len(), 49);
        assert_eq!(latest.image.meta.next_object_id().get(), 51);
        let latest_table = latest.find_table_by_id(table_id).unwrap();
        assert_eq!(latest_table.next_rowid().get(), 97);
        assert_eq!(latest_table.root(), rows.root());
        assert_ne!(latest_table.root(), old_table.root());
        latest.validate_deep(&mut latest_snapshot).unwrap();
        drop(latest_snapshot);
        drop(old_snapshot);
        // Reopen first with the old checkpoint plus the newer complete WAL state.
        drop(pager);
        for reset in [false, true] {
            let pager = Pager::open(
                temp.path(),
                OpenMode::ReadWrite,
                PagerConfig::test_default(4096),
            )
            .unwrap();
            if reset {
                assert!(pager.checkpoint().unwrap().wal_reset);
            }
            let mut read = pager.begin_read().unwrap();
            assert_eq!(read.lsn(), latest_lsn);
            let recovered = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(recovered.image, latest.image);
            recovered.validate_deep(&mut read).unwrap();
        }
        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadOnly,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read).unwrap().image,
            latest.image
        );
    }

    fn large_user_tree(batch: &mut WriteBatch<'_>) -> BTree {
        let mut tree = BTree::create(batch, TreeKind::Table).unwrap();
        for key in 0_u64..80 {
            tree.insert(batch, &key.to_be_bytes(), &[42; 700]).unwrap();
        }
        tree.insert(batch, b"overflow", &[43; 5000]).unwrap();
        tree
    }

    fn copy_internal_root(batch: &mut WriteBatch<'_>, from: PageRef, to: PageRef) {
        let source = batch.read(from).unwrap();
        let header = source.header();
        assert_eq!(header.page_type(), PageType::TableInternal);
        let target = batch.modify(to).unwrap();
        target
            .set_layout(
                header.cell_count(),
                header.slot_end(),
                header.payload_start(),
            )
            .unwrap();
        target.bytes_mut()[48..].copy_from_slice(&source.bytes()[48..]);
    }

    #[test]
    fn deep_catalog_validation_rejects_cross_tree_overlap() {
        for overlap_catalog in [false, true] {
            let (_temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let user = large_user_tree(&mut batch);
            let alias = batch.allocate_append(PageType::TableInternal).unwrap();
            let mut rows = records(user.root());
            let second = new_table(30, alias, 0, "U");
            rows.extend([
                (
                    table_name_key(second.name()).unwrap(),
                    encode_table_name_mapping(second.id()),
                ),
                (
                    table_definition_key(second.id()),
                    encode_table_definition(&second),
                ),
            ]);
            // Force an internal catalog root so a distinct user root can share its
            // entire descendant set while both trees remain individually valid.
            for id in 31..80 {
                let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
                let table = new_table(id, root, 0, &format!("T{id}"));
                rows.extend([
                    (
                        table_name_key(table.name()).unwrap(),
                        encode_table_name_mapping(table.id()),
                    ),
                    (
                        table_definition_key(table.id()),
                        encode_table_definition(&table),
                    ),
                ]);
            }
            let catalog_tree = raw_tree(&mut batch, rows);
            let original = if overlap_catalog {
                catalog_tree.root()
            } else {
                user.root()
            };
            copy_internal_root(&mut batch, original, alias);
            validate_tree(&mut batch, &BTree::from_root(TreeKind::Table, alias)).unwrap();
            CatalogReader::open_candidate(&mut batch, &catalog_tree, CATALOG_V1).unwrap();
            publish(&mut batch, &catalog_tree);
            batch.commit().unwrap();
            let mut read = pager.begin_read().unwrap();
            let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            let error = catalog.validate_deep(&mut read).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Corruption);
            assert!(error.to_string().contains("overlap"));
        }
    }

    #[test]
    fn deep_catalog_validation_visits_internal_leaf_and_overflow_pages() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let user = large_user_tree(&mut batch);
        let user_report = validate_tree(&mut batch, &user).unwrap();
        assert!(user_report.tree_pages.len() > 2);
        assert!(!user_report.overflow_pages.is_empty());
        let tree = raw_tree(&mut batch, records(user.root()));
        publish(&mut batch, &tree);
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        let (tree, report) = read
            .catalog_tree_with_report(TreeKind::Table)
            .unwrap()
            .unwrap();
        let catalog_pages = report.all_live_pages();
        let mut source = CountingSource {
            snapshot: &mut read,
            reads: BTreeMap::new(),
        };
        let catalog = CatalogReader::open_validated(&mut source, &tree, report).unwrap();
        assert_eq!(source.reads.get(&user.root().page_id()), Some(&1));
        assert!(
            source
                .reads
                .keys()
                .all(|id| catalog_pages.contains(id) || *id == user.root().page_id())
        );
        source.reads.clear();
        let report = validate_tree(&mut source, &tree).unwrap();
        catalog
            .validate_deep_from(&mut source, &tree, report)
            .unwrap();
        assert!(
            user_report
                .all_live_pages()
                .iter()
                .all(|id| source.reads.contains_key(id))
        );
        assert!(catalog_pages.iter().all(|id| source.reads.contains_key(id)));
        catalog.validate_deep(&mut read).unwrap();
    }

    #[test]
    fn deep_catalog_validation_rejects_user_descendants_in_freelist() {
        use crate::storage::freelist::Freelist;
        for overflow in [false, true] {
            let (_temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let user = large_user_tree(&mut batch);
            let report = validate_tree(&mut batch, &user).unwrap();
            let target = if overflow {
                *report.overflow_pages.first().unwrap()
            } else {
                *report
                    .tree_pages
                    .iter()
                    .find(|id| **id != user.root().page_id())
                    .unwrap()
            };
            let spare = batch.allocate_append(PageType::TableLeaf).unwrap();
            let tree = raw_tree(&mut batch, records(user.root()));
            publish(&mut batch, &tree);
            batch.commit().unwrap();
            let mut batch = pager.begin_write().unwrap();
            batch.release_later(spare).unwrap();
            batch.commit().unwrap();
            let mut batch = pager.begin_write().unwrap();
            let mut trunk = Freelist::decode(batch.modify(spare).unwrap()).unwrap();
            Freelist::release(&mut trunk, target, 4096).unwrap();
            Freelist::write(batch.modify(spare).unwrap(), &trunk).unwrap();
            batch.commit().unwrap();
            let mut read = pager.begin_read().unwrap();
            let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            let error = catalog.validate_deep(&mut read).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Corruption);
            assert!(error.to_string().contains("overlap"));
        }
    }

    struct CountingSource<'a> {
        snapshot: &'a mut ReadSnapshot,
        reads: BTreeMap<PageId, usize>,
    }

    impl PageSource for CountingSource<'_> {
        fn read_page(&mut self, reference: PageRef) -> Result<crate::storage::cache::PageHandle> {
            *self.reads.entry(reference.page_id()).or_default() += 1;
            self.snapshot.read_page(reference)
        }

        fn read_page_by_id(
            &mut self,
            page_id: PageId,
        ) -> Result<crate::storage::cache::PageHandle> {
            *self.reads.entry(page_id).or_default() += 1;
            self.snapshot.read_page_by_id(page_id)
        }

        fn page_count(&self) -> Result<u64> {
            self.snapshot.page_count()
        }
        fn freelist_root(&self) -> Result<PageId> {
            self.snapshot.freelist_root()
        }
    }

    #[test]
    fn normal_open_scans_full_catalog_but_reads_each_user_root_once_and_no_descendants() {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut user = BTree::create(&mut batch, TreeKind::Table).unwrap();
        for key in 0_u64..120 {
            user.insert(&mut batch, &key.to_be_bytes(), &[42; 700])
                .unwrap();
        }
        let user_report = validate_tree(&mut batch, &user).unwrap();
        assert!(user_report.all_live_pages().len() > 20);
        assert_eq!(
            batch.read(user.root()).unwrap().header().page_type(),
            PageType::TableInternal
        );
        let mut rows = records(user.root());
        let mut user_roots = BTreeSet::from([user.root().page_id()]);
        for id in 30..80 {
            let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
            user_roots.insert(root.page_id());
            let table = TableDef::new(
                TableId::new(id).unwrap(),
                SchemaVersion::new(1).unwrap(),
                root,
                RowId::new(1).unwrap(),
                0,
                CatalogName::new(format!("T{id}")).unwrap(),
            )
            .unwrap();
            rows.extend([
                (
                    table_name_key(table.name()).unwrap(),
                    encode_table_name_mapping(table.id()),
                ),
                (
                    table_definition_key(table.id()),
                    encode_table_definition(&table),
                ),
            ]);
        }
        let tree = raw_tree(&mut batch, rows);
        publish(&mut batch, &tree);
        batch.commit().unwrap();

        // A checksummed but structurally malformed descendant is deliberately outside
        // normal-open validation; an explicit deep validator must still reject it.
        let mut read = pager.begin_read().unwrap();
        let leaf_id = *user_report
            .tree_pages
            .iter()
            .find(|id| {
                read.read_page_by_id(**id).unwrap().header().page_type() == PageType::TableLeaf
            })
            .unwrap();
        let leaf = read.read_page_by_id(leaf_id).unwrap();
        let leaf_ref = PageRef::new(leaf_id, leaf.header().generation()).unwrap();
        drop(read);
        let mut batch = pager.begin_write().unwrap();
        batch.modify(leaf_ref).unwrap().bytes_mut()[48..56].fill(0xff);
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            validate_tree(&mut read, &user).unwrap_err().code(),
            ErrorCode::Corruption
        );
        let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        assert!(
            catalog
                .find_table_by_id(TableId::new(79).unwrap())
                .is_some()
        );
        let (tree, report) = read
            .catalog_tree_with_report(TreeKind::Table)
            .unwrap()
            .unwrap();
        assert!(report.tree_pages.len() > 1);
        let catalog_pages = report.all_live_pages();
        let mut source = CountingSource {
            snapshot: &mut read,
            reads: BTreeMap::new(),
        };
        CatalogReader::open_validated(&mut source, &tree, report).unwrap();
        for id in &user_roots {
            assert_eq!(source.reads.get(id), Some(&1));
        }
        for id in user_report.all_live_pages().difference(&user_roots) {
            assert!(!source.reads.contains_key(id));
        }
        assert!(
            source
                .reads
                .keys()
                .all(|id| catalog_pages.contains(id) || user_roots.contains(id))
        );
        drop(read);
        drop(pager);
        // Pager recovery uses the same shallow contract, even when the descendant
        // bytes are durably checksummed but semantically invalid.
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let pager = Pager::open(temp.path(), mode, PagerConfig::test_default(4096)).unwrap();
            let mut read = pager.begin_read().unwrap();
            let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(
                catalog.validate_deep(&mut read).unwrap_err().code(),
                ErrorCode::Corruption
            );
        }
    }
}
