pub(crate) mod key;
pub(crate) mod record;
pub(crate) mod types;

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

use super::{
    btree::{BTree, TreeKind},
    features::{CATALOG_V1, validate_feature_bits},
    pager::{PageSource, ReadSnapshot, WriteBatch},
    validate::{ValidationReport, validate_freelist, validate_table_roots, validate_tree},
};
use crate::{DbError, ErrorCode, Result};
use key::DecodedCatalogKey;
use types::{CatalogMeta, CatalogName, ColumnDef, ColumnId, ObjectId, TableDef, TableId};

/// A fully decoded and cross-checked image. No page source is retained.
#[derive(Debug)]
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
        let report = validate_tree(batch, tree)?;
        Self::open_validated(batch, tree, report)
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
            test_support::test_pager,
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
        let (_temp, pager) = test_pager(4096).unwrap();
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
        let (_temp, pager) = test_pager(4096).unwrap();
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
        let (_temp, pager) = test_pager(4096).unwrap();
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
    }
}
