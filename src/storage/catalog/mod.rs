pub(crate) mod key;
pub(crate) mod record;
pub(crate) mod types;

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

#[cfg(test)]
use std::cell::{Cell, RefCell};

use super::{
    PageRef,
    btree::{BTree, TreeKind},
    features::{CATALOG_V1, INDEX_CONSTRAINT_V1, validate_feature_bits},
    index::key::IndexKeyCodec,
    pager::{PageSource, ReadSnapshot, WriteBatch},
    validate::{
        ValidationReport, validate_freelist, validate_relation_roots, validate_relation_trees,
        validate_tree,
    },
};
use crate::storage::constraint::program::CheckProgram;
use crate::{DbError, ErrorCode, Result};
use key::DecodedCatalogKey;
use types::{
    CatalogMeta, CatalogName, ColumnDef, ColumnId, ConstraintBundle, ConstraintDef, ConstraintId,
    ConstraintKind, ForeignKeyTarget, IndexDef, IndexId, IndexOwnership, ObjectId, ObjectKind,
    RowId, SchemaVersion, TableDef, TableId,
};

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConstraintBundleFaultPoint {
    RecordsInserted,
    FeatureEnabled,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConstraintRemovalFaultPoint {
    ReferencedBy,
    Columns,
    TableMapping,
    NameMapping,
    Definition,
}

#[cfg(test)]
thread_local! {
    static CONSTRAINT_BUNDLE_FAILURE: Cell<Option<ConstraintBundleFaultPoint>> = const { Cell::new(None) };
    static CONSTRAINT_BUNDLE_FAULT_OBSERVATIONS: RefCell<Vec<(ConstraintBundleFaultPoint, bool)>> = const { RefCell::new(Vec::new()) };
    static CONSTRAINT_REMOVAL_FAILURE: Cell<Option<ConstraintRemovalFaultPoint>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct ConstraintRemovalFaultGuard;

#[cfg(test)]
impl Drop for ConstraintRemovalFaultGuard {
    fn drop(&mut self) {
        CONSTRAINT_REMOVAL_FAILURE.set(None);
    }
}

#[cfg(test)]
pub(crate) fn fail_constraint_removal_at_for_test(
    point: ConstraintRemovalFaultPoint,
) -> ConstraintRemovalFaultGuard {
    CONSTRAINT_REMOVAL_FAILURE.set(Some(point));
    ConstraintRemovalFaultGuard
}

#[cfg(test)]
pub(crate) fn fail_constraint_bundle_at_for_test(point: ConstraintBundleFaultPoint) {
    CONSTRAINT_BUNDLE_FAULT_OBSERVATIONS.with(|observations| observations.borrow_mut().clear());
    CONSTRAINT_BUNDLE_FAILURE.set(Some(point));
}

#[cfg(test)]
pub(crate) fn take_constraint_bundle_fault_observations_for_test()
-> Vec<(ConstraintBundleFaultPoint, bool)> {
    CONSTRAINT_BUNDLE_FAULT_OBSERVATIONS
        .with(|observations| std::mem::take(&mut *observations.borrow_mut()))
}

#[cfg(test)]
fn constraint_bundle_fault_for_test(
    batch: &WriteBatch,
    point: ConstraintBundleFaultPoint,
) -> Result<()> {
    CONSTRAINT_BUNDLE_FAILURE.with(|failure| {
        if failure.get() == Some(point) {
            failure.set(None);
            CONSTRAINT_BUNDLE_FAULT_OBSERVATIONS.with(|observations| {
                observations
                    .borrow_mut()
                    .push((point, batch.feature_bits() & INDEX_CONSTRAINT_V1 != 0));
            });
            Err(DbError::new(
                ErrorCode::Io,
                "injected constraint bundle insertion failure",
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn constraint_removal_fault_for_test(point: ConstraintRemovalFaultPoint) -> Result<()> {
    CONSTRAINT_REMOVAL_FAILURE.with(|failure| {
        if failure.get() == Some(point) {
            failure.set(None);
            Err(DbError::new(
                ErrorCode::Io,
                "injected constraint record-family removal failure",
            ))
        } else {
            Ok(())
        }
    })
}

/// A fully decoded and cross-checked image. No page source is retained.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CatalogImage {
    meta: CatalogMeta,
    tables: BTreeMap<TableId, TableDef>,
    table_names: BTreeMap<String, TableId>,
    columns: BTreeMap<ColumnId, ColumnDef>,
    column_names: BTreeMap<TableId, BTreeMap<String, ColumnId>>,
    column_ordinals: BTreeMap<TableId, BTreeMap<u16, ColumnId>>,
    indexes: BTreeMap<IndexId, IndexDef>,
    index_names: BTreeMap<String, IndexId>,
    table_indexes: BTreeMap<TableId, BTreeSet<IndexId>>,
    index_columns: BTreeMap<IndexId, BTreeMap<u16, ColumnId>>,
    constraints: BTreeMap<ConstraintId, ConstraintDef>,
    constraint_names: BTreeMap<String, ConstraintId>,
    table_constraints: BTreeMap<TableId, BTreeSet<ConstraintId>>,
    constraint_columns: BTreeMap<ConstraintId, BTreeMap<u16, ColumnId>>,
    referenced_by: BTreeMap<ConstraintId, BTreeSet<ConstraintId>>,
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
            indexes: BTreeMap::new(),
            index_names: BTreeMap::new(),
            table_indexes: BTreeMap::new(),
            index_columns: BTreeMap::new(),
            constraints: BTreeMap::new(),
            constraint_names: BTreeMap::new(),
            table_constraints: BTreeMap::new(),
            constraint_columns: BTreeMap::new(),
            referenced_by: BTreeMap::new(),
        }
    }

    fn decode(source: &mut impl PageSource, tree: &BTree, feature_bits: u64) -> Result<Self> {
        let mut image = Self::empty();
        let mut meta = None;
        let mut cursor = tree.scan(source, Bound::Unbounded, Bound::Unbounded)?;
        while let Some((key, value)) = cursor.next()? {
            if key
                .first()
                .is_some_and(|family| (0x20..=0x34).contains(family))
            {
                require_index_feature(feature_bits)?;
            }
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
                DecodedCatalogKey::IndexName(name) => {
                    require_index_feature(feature_bits)?;
                    insert_unique(
                        &mut image.index_names,
                        name.to_owned(),
                        record::decode_index_name_mapping(&value)?,
                    )?;
                }
                DecodedCatalogKey::IndexDefinition(id) => {
                    require_index_feature(feature_bits)?;
                    let index = record::decode_index_definition(id, &value)?;
                    insert_unique(&mut image.indexes, id, index)?;
                }
                DecodedCatalogKey::TableIndex { table_id, index_id } => {
                    require_index_feature(feature_bits)?;
                    if record::decode_table_index_mapping(&value)? != index_id {
                        return Err(corruption("table index mapping does not match its key"));
                    }
                    if !image
                        .table_indexes
                        .entry(table_id)
                        .or_default()
                        .insert(index_id)
                    {
                        return Err(corruption(
                            "catalog contains a duplicate table index mapping",
                        ));
                    }
                }
                DecodedCatalogKey::IndexColumn { index_id, ordinal } => {
                    require_index_feature(feature_bits)?;
                    insert_unique(
                        image.index_columns.entry(index_id).or_default(),
                        ordinal,
                        record::decode_index_column_mapping(&value)?,
                    )?;
                }
                DecodedCatalogKey::ConstraintName(name) => {
                    require_index_feature(feature_bits)?;
                    insert_unique(
                        &mut image.constraint_names,
                        name.to_owned(),
                        record::decode_constraint_name_mapping(&value)?,
                    )?;
                }
                DecodedCatalogKey::ConstraintDefinition(id) => {
                    require_index_feature(feature_bits)?;
                    insert_unique(
                        &mut image.constraints,
                        id,
                        record::decode_constraint_definition(id, &value)?,
                    )?;
                }
                DecodedCatalogKey::TableConstraint {
                    table_id,
                    constraint_id,
                } => {
                    require_index_feature(feature_bits)?;
                    if record::decode_table_constraint_mapping(&value)? != constraint_id {
                        return Err(corruption(
                            "table constraint mapping does not match its key",
                        ));
                    }
                    if !image
                        .table_constraints
                        .entry(table_id)
                        .or_default()
                        .insert(constraint_id)
                    {
                        return Err(corruption(
                            "catalog contains a duplicate table constraint mapping",
                        ));
                    }
                }
                DecodedCatalogKey::ConstraintColumn {
                    constraint_id,
                    ordinal,
                } => {
                    require_index_feature(feature_bits)?;
                    insert_unique(
                        image.constraint_columns.entry(constraint_id).or_default(),
                        ordinal,
                        record::decode_constraint_column_mapping(&value)?,
                    )?;
                }
                DecodedCatalogKey::ReferencedBy {
                    parent_id: parent,
                    child_id: child,
                } => {
                    require_index_feature(feature_bits)?;
                    if record::decode_referenced_by_mapping(&value)? != child
                        || !image.referenced_by.entry(parent).or_default().insert(child)
                    {
                        return Err(corruption(
                            "catalog contains an invalid referenced-by mapping",
                        ));
                    }
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
        for (name, id) in &self.index_names {
            if self
                .indexes
                .get(id)
                .is_none_or(|index| index.name().as_str() != name)
            {
                return Err(corruption("index name mapping has no matching definition"));
            }
        }
        for index in self.indexes.values() {
            if !object_ids.insert(index.id().into()) {
                return Err(corruption("catalog definitions share a global object ID"));
            }
            if !self.tables.contains_key(&index.table_id()) {
                return Err(corruption("index definition references a missing table"));
            }
            if self.index_names.get(index.name().as_str()) != Some(&index.id()) {
                return Err(corruption("index definition has no matching name mapping"));
            }
            if !self
                .table_indexes
                .get(&index.table_id())
                .is_some_and(|indexes| indexes.contains(&index.id()))
            {
                return Err(corruption(
                    "index definition has no matching table index mapping",
                ));
            }
            let columns = self.index_columns.get(&index.id());
            if columns.map_or(0, BTreeMap::len) != usize::from(index.column_count()) {
                return Err(corruption(
                    "index column count does not match ordinal mappings",
                ));
            }
            let mut seen_columns = BTreeSet::new();
            for (expected, (ordinal, column_id)) in columns
                .into_iter()
                .flat_map(|columns| columns.iter())
                .enumerate()
            {
                let expected = u16::try_from(expected)
                    .map_err(|_| corruption("index column ordinal exceeds u16"))?;
                if *ordinal != expected {
                    return Err(corruption(
                        "index column ordinals are not contiguous from zero",
                    ));
                }
                let column = self.columns.get(column_id).ok_or_else(|| {
                    corruption("index column mapping references a missing column")
                })?;
                if column.table_id() != index.table_id() {
                    return Err(corruption("index column is not owned by the index table"));
                }
                if !seen_columns.insert(*column_id) {
                    return Err(corruption("index contains a repeated column"));
                }
            }
            if let IndexOwnership::Constraint(constraint_id) = index.ownership() {
                if self
                    .constraints
                    .get(&constraint_id)
                    .is_none_or(|constraint| constraint.index_id() != Some(index.id()))
                {
                    return Err(corruption(
                        "constraint-owned index has no matching owner constraint",
                    ));
                }
            }
        }
        for (table_id, indexes) in &self.table_indexes {
            for index_id in indexes {
                if self
                    .indexes
                    .get(index_id)
                    .is_none_or(|index| index.table_id() != *table_id)
                {
                    return Err(corruption(
                        "table index mapping has no matching owned definition",
                    ));
                }
            }
        }
        for (index_id, columns) in &self.index_columns {
            let index = self
                .indexes
                .get(index_id)
                .ok_or_else(|| corruption("index column mapping has no matching definition"))?;
            for column_id in columns.values() {
                if self
                    .columns
                    .get(column_id)
                    .is_none_or(|column| column.table_id() != index.table_id())
                {
                    return Err(corruption(
                        "index column mapping has no matching owned column",
                    ));
                }
            }
        }
        for (name, id) in &self.constraint_names {
            if self
                .constraints
                .get(id)
                .is_none_or(|constraint| constraint.name().as_str() != name)
            {
                return Err(corruption(
                    "constraint name mapping has no matching definition",
                ));
            }
        }
        let mut primary_key_tables = BTreeSet::new();
        for constraint in self.constraints.values() {
            if !object_ids.insert(constraint.id().into()) {
                return Err(corruption("catalog definitions share a global object ID"));
            }
            if constraint.id().get() >= self.meta.next_object_id().get() {
                return Err(corruption("constraint ID is not below the next object ID"));
            }
            if !self.tables.contains_key(&constraint.table_id()) {
                return Err(corruption(
                    "constraint definition references a missing table",
                ));
            }
            if self.constraint_names.get(constraint.name().as_str()) != Some(&constraint.id()) {
                return Err(corruption(
                    "constraint definition has no matching name mapping",
                ));
            }
            if !self
                .table_constraints
                .get(&constraint.table_id())
                .is_some_and(|constraints| constraints.contains(&constraint.id()))
            {
                return Err(corruption(
                    "constraint definition has no matching table constraint mapping",
                ));
            }
            let columns = self.constraint_columns.get(&constraint.id());
            if columns.map_or(0, BTreeMap::len) != usize::from(constraint.column_count()) {
                return Err(corruption(
                    "constraint column count does not match ordinal mappings",
                ));
            }
            let mut seen_columns = BTreeSet::new();
            for (expected, (ordinal, column_id)) in columns
                .into_iter()
                .flat_map(|columns| columns.iter())
                .enumerate()
            {
                let expected = u16::try_from(expected)
                    .map_err(|_| corruption("constraint column ordinal exceeds u16"))?;
                if *ordinal != expected {
                    return Err(corruption(
                        "constraint column ordinals are not contiguous from zero",
                    ));
                }
                let column = self.columns.get(column_id).ok_or_else(|| {
                    corruption("constraint column mapping references a missing column")
                })?;
                if column.table_id() != constraint.table_id() {
                    return Err(corruption(
                        "constraint column is not owned by the constraint table",
                    ));
                }
                if !seen_columns.insert(*column_id) {
                    return Err(corruption("constraint contains a repeated column"));
                }
                if matches!(
                    constraint.kind(),
                    ConstraintKind::PrimaryKey | ConstraintKind::NotNull
                ) && column.nullable()
                {
                    return Err(corruption(
                        "primary-key and not-null columns must be non-nullable",
                    ));
                }
            }
            match constraint.kind() {
                ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
                    if constraint.kind() == ConstraintKind::PrimaryKey
                        && !primary_key_tables.insert(constraint.table_id())
                    {
                        return Err(corruption("table has more than one primary key"));
                    }
                    let index_id = constraint
                        .index_id()
                        .ok_or_else(|| corruption("key constraint has no matching system index"))?;
                    let index = self.indexes.get(&index_id).ok_or_else(|| {
                        corruption("key constraint references a missing system index")
                    })?;
                    if index.ownership() != IndexOwnership::Constraint(constraint.id()) {
                        return Err(corruption(
                            "key constraint system index has the wrong owner",
                        ));
                    }
                    if index.table_id() != constraint.table_id() {
                        return Err(corruption(
                            "key constraint system index belongs to the wrong table",
                        ));
                    }
                    if !index.unique() {
                        return Err(corruption("key constraint system index is not unique"));
                    }
                    let index_columns = self.index_columns.get(&index.id());
                    if columns
                        .into_iter()
                        .flat_map(|columns| columns.values())
                        .ne(index_columns
                            .into_iter()
                            .flat_map(|columns| columns.values()))
                    {
                        return Err(corruption(
                            "key constraint and system index column order differs",
                        ));
                    }
                }
                ConstraintKind::NotNull => {}
                ConstraintKind::ForeignKey => self.validate_foreign_key(
                    constraint,
                    constraint
                        .foreign_key_target()
                        .ok_or_else(|| corruption("foreign-key constraint has no parent target"))?,
                    columns
                        .ok_or_else(|| corruption("foreign-key constraint has no local columns"))?,
                )?,
                ConstraintKind::Check => {
                    let program = CheckProgram::from_bytes(
                        constraint.check_program().to_vec().into_boxed_slice(),
                    )
                    .map_err(normalize_check_program_error)?;
                    let columns = self
                        .column_ordinals
                        .get(&constraint.table_id())
                        .into_iter()
                        .flat_map(|ordinals| ordinals.values())
                        .map(|id| {
                            self.columns
                                .get(id)
                                .cloned()
                                .ok_or_else(|| corruption("check table column is missing"))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    program
                        .validate(&columns)
                        .map_err(normalize_check_program_error)?;
                }
            }
        }
        for (table_id, constraints) in &self.table_constraints {
            for constraint_id in constraints {
                if self
                    .constraints
                    .get(constraint_id)
                    .is_none_or(|constraint| constraint.table_id() != *table_id)
                {
                    return Err(corruption(
                        "table constraint mapping has no matching owned definition",
                    ));
                }
            }
        }
        for (constraint_id, columns) in &self.constraint_columns {
            let constraint = self.constraints.get(constraint_id).ok_or_else(|| {
                corruption("constraint column mapping has no matching definition")
            })?;
            for column_id in columns.values() {
                if self
                    .columns
                    .get(column_id)
                    .is_none_or(|column| column.table_id() != constraint.table_id())
                {
                    return Err(corruption(
                        "constraint column mapping has no matching owned column",
                    ));
                }
            }
        }
        for (parent_id, children) in &self.referenced_by {
            let parent = self.constraints.get(parent_id).ok_or_else(|| {
                corruption("referenced-by mapping references a missing parent constraint")
            })?;
            for child_id in children {
                let child = self.constraints.get(child_id).ok_or_else(|| {
                    corruption("referenced-by mapping references a missing child constraint")
                })?;
                if child.kind() != ConstraintKind::ForeignKey
                    || child.foreign_key_target()
                        != Some(ForeignKeyTarget::new(parent.table_id(), *parent_id))
                {
                    return Err(corruption(
                        "referenced-by mapping does not match the child foreign key",
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_foreign_key(
        &self,
        child: &ConstraintDef,
        target: ForeignKeyTarget,
        local_columns: &BTreeMap<u16, ColumnId>,
    ) -> Result<()> {
        if child.table_id() == target.table_id() {
            return Err(corruption("foreign key cannot reference its own table"));
        }
        let parent = self
            .constraints
            .get(&target.constraint_id())
            .ok_or_else(|| corruption("foreign key references a missing parent constraint"))?;
        if parent.table_id() != target.table_id() {
            return Err(corruption(
                "foreign-key parent constraint belongs to the wrong table",
            ));
        }
        if !matches!(
            parent.kind(),
            ConstraintKind::PrimaryKey | ConstraintKind::Unique
        ) {
            return Err(corruption(
                "foreign-key parent is not a primary-key or unique constraint",
            ));
        }
        let index = parent
            .index_id()
            .and_then(|id| self.indexes.get(&id))
            .ok_or_else(|| corruption("foreign-key parent has no system index"))?;
        if index.table_id() != parent.table_id()
            || index.ownership() != IndexOwnership::Constraint(parent.id())
            || !index.unique()
        {
            return Err(corruption(
                "foreign-key parent has no constraint-owned unique system index",
            ));
        }
        let parent_columns = self
            .constraint_columns
            .get(&parent.id())
            .ok_or_else(|| corruption("foreign-key parent has no constraint columns"))?;
        if local_columns.len() != parent_columns.len()
            || local_columns.len() != usize::from(child.column_count())
            || parent_columns.len() != usize::from(parent.column_count())
        {
            return Err(corruption(
                "foreign-key child and parent column counts differ",
            ));
        }
        for (expected, (ordinal, _)) in local_columns.iter().enumerate() {
            if *ordinal
                != u16::try_from(expected)
                    .map_err(|_| corruption("foreign-key child ordinal exceeds u16"))?
            {
                return Err(corruption(
                    "foreign-key child columns are not contiguous from zero",
                ));
            }
        }
        for (expected, (ordinal, _)) in parent_columns.iter().enumerate() {
            if *ordinal
                != u16::try_from(expected)
                    .map_err(|_| corruption("foreign-key parent ordinal exceeds u16"))?
            {
                return Err(corruption(
                    "foreign-key parent columns are not contiguous from zero",
                ));
            }
        }
        for (child_column_id, parent_column_id) in
            local_columns.values().zip(parent_columns.values())
        {
            let child_column = self.columns.get(child_column_id).ok_or_else(|| {
                corruption("foreign-key child column mapping references a missing column")
            })?;
            let parent_column = self.columns.get(parent_column_id).ok_or_else(|| {
                corruption("foreign-key parent column mapping references a missing column")
            })?;
            let child_type = child_column.column_type();
            let parent_type = parent_column.column_type();
            if child_type != parent_type
                && !(child_type == types::ColumnType::Int64
                    && parent_type == types::ColumnType::Decimal)
            {
                return Err(corruption(
                    "foreign-key child and parent column types are incompatible",
                ));
            }
        }
        if !self
            .referenced_by
            .get(&parent.id())
            .is_some_and(|children| children.contains(&child.id()))
        {
            return Err(corruption(
                "foreign-key constraint has no matching referenced-by mapping",
            ));
        }
        Ok(())
    }
}

fn normalize_check_program_error(error: DbError) -> DbError {
    if error.code() == ErrorCode::Corruption {
        corruption("check constraint program is invalid")
    } else {
        error
    }
}

#[derive(Debug)]
pub(crate) struct CatalogReader {
    image: CatalogImage,
}

impl CatalogReader {
    pub(super) fn open_empty_or_v1(snapshot: &mut ReadSnapshot) -> Result<Self> {
        let feature_bits = snapshot.feature_bits();
        validate_feature_bits(feature_bits)?;
        if feature_bits & CATALOG_V1 == 0 {
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
        Self::open_validated(snapshot, &tree, report, feature_bits)
    }

    pub(super) fn open_empty_or_v1_batch(batch: &mut WriteBatch) -> Result<Self> {
        let feature_bits = batch.feature_bits();
        validate_feature_bits(feature_bits)?;
        if feature_bits & CATALOG_V1 == 0 {
            if batch.catalog_root().0 != 0 {
                return Err(unsupported_legacy());
            }
            return Ok(Self {
                image: CatalogImage::empty(),
            });
        }
        let (tree, report) = batch
            .catalog_tree_with_report(TreeKind::Table)?
            .ok_or_else(|| corruption("catalog V1 feature requires a catalog root"))?;
        batch.require_current_catalog_root(tree.root())?;
        let reader = Self::open_validated(batch, &tree, report, feature_bits)?;
        for table in reader.image.tables.values() {
            batch.require_current_catalog_root(table.root())?;
        }
        for index in reader.image.indexes.values() {
            batch.require_current_catalog_root(index.root())?;
        }
        Ok(reader)
    }

    /// Validate an unpublished candidate; its current root can differ from the header.
    pub(super) fn open_candidate(
        batch: &mut WriteBatch,
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
        let reader = Self::open_validated(batch, tree, report, feature_bits)?;
        for table in reader.image.tables.values() {
            batch.require_current_catalog_root(table.root())?;
        }
        for index in reader.image.indexes.values() {
            batch.require_current_catalog_root(index.root())?;
        }
        Ok(reader)
    }

    fn open_validated(
        source: &mut impl PageSource,
        tree: &BTree,
        report: ValidationReport,
        feature_bits: u64,
    ) -> Result<Self> {
        let image = CatalogImage::decode(source, tree, feature_bits)?;
        let mut live = report.all_live_pages();
        validate_relation_roots(
            source,
            image.tables.values().map(TableDef::root),
            image.indexes.values().map(IndexDef::root),
            &mut live,
        )?;
        validate_freelist(source, &live)?;
        Ok(Self { image })
    }

    /// Explicit, potentially expensive validation. Ordinary open never walks the
    /// user descendants; here every live tree is checked against every other tree.
    pub(super) fn validate_deep(&self, snapshot: &mut ReadSnapshot) -> Result<()> {
        let feature_bits = snapshot.feature_bits();
        validate_feature_bits(feature_bits)?;
        if feature_bits & CATALOG_V1 == 0 {
            if snapshot.catalog_root().0 != 0 {
                return Err(unsupported_legacy());
            }
            self.require_matching_image(&CatalogImage::empty())?;
            return validate_freelist(snapshot, &BTreeSet::new()).map(|_| ());
        }
        let (tree, report) = snapshot
            .catalog_tree_with_report(TreeKind::Table)?
            .ok_or_else(|| corruption("catalog V1 feature requires a catalog root"))?;
        self.validate_deep_from(snapshot, &tree, report, feature_bits)
    }

    /// Validate every table tree in a write batch before a destructive catalog
    /// operation can release pages still reachable through another root.
    pub(super) fn validate_deep_batch(&self, batch: &mut WriteBatch) -> Result<()> {
        let feature_bits = batch.feature_bits();
        validate_feature_bits(feature_bits)?;
        let (tree, report) = batch
            .catalog_tree_with_report(TreeKind::Table)?
            .ok_or_else(|| corruption("catalog V1 feature requires a catalog root"))?;
        self.validate_deep_from(batch, &tree, report, feature_bits)
    }

    fn validate_deep_from(
        &self,
        source: &mut impl PageSource,
        tree: &BTree,
        report: ValidationReport,
        feature_bits: u64,
    ) -> Result<()> {
        // A reader retains no snapshot lease. Do not combine its immutable
        // definitions with a different snapshot's catalog, roots, or counters.
        self.require_matching_image(&CatalogImage::decode(source, tree, feature_bits)?)?;
        let mut live = report.all_live_pages();
        validate_relation_trees(
            source,
            self.image.tables.values().map(TableDef::root),
            self.image.indexes.values().map(IndexDef::root),
            &mut live,
        )?;
        self.validate_index_entries(source)?;
        validate_freelist(source, &live).map(|_| ())
    }

    fn validate_index_entries(&self, source: &mut impl PageSource) -> Result<()> {
        for index in self.image.indexes.values() {
            let columns = self
                .columns_for_index(index.id())
                .cloned()
                .collect::<Vec<_>>();
            let tree = BTree::from_root(TreeKind::Index, index.root());
            let mut cursor = tree.scan(source, Bound::Unbounded, Bound::Unbounded)?;
            while let Some((key, value)) = cursor.next()? {
                if !value.is_empty() {
                    return Err(corruption("index leaf value must be empty"));
                }
                IndexKeyCodec::decode(&columns, &key)?;
            }
        }
        Ok(())
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

    pub(crate) fn find_column_by_id(&self, id: ColumnId) -> Option<&ColumnDef> {
        self.image.columns.get(&id)
    }

    pub(crate) fn find_index_by_name(&self, name: &CatalogName) -> Option<&IndexDef> {
        self.image
            .index_names
            .get(name.as_str())
            .and_then(|id| self.image.indexes.get(id))
    }

    pub(crate) fn find_index_by_id(&self, id: IndexId) -> Option<&IndexDef> {
        self.image.indexes.get(&id)
    }

    pub(crate) fn indexes_for_table(&self, table: TableId) -> impl Iterator<Item = &IndexDef> {
        self.image
            .table_indexes
            .get(&table)
            .into_iter()
            .flat_map(|indexes| indexes.iter())
            .map(|id| {
                self.image
                    .indexes
                    .get(id)
                    .expect("validated table index references an existing index")
            })
    }

    pub(crate) fn columns_for_index(&self, index: IndexId) -> impl Iterator<Item = &ColumnDef> {
        self.image
            .index_columns
            .get(&index)
            .into_iter()
            .flat_map(|columns| columns.values())
            .map(|id| {
                self.image
                    .columns
                    .get(id)
                    .expect("validated index column references an existing column")
            })
    }

    pub(crate) fn find_constraint_by_id(&self, id: ConstraintId) -> Option<&ConstraintDef> {
        self.image.constraints.get(&id)
    }

    pub(crate) fn find_constraint_by_name(&self, name: &CatalogName) -> Option<&ConstraintDef> {
        self.image
            .constraint_names
            .get(name.as_str())
            .and_then(|id| self.image.constraints.get(id))
    }

    pub(crate) fn constraints_for_table(
        &self,
        table: TableId,
    ) -> impl Iterator<Item = &ConstraintDef> {
        self.image
            .table_constraints
            .get(&table)
            .into_iter()
            .flat_map(|constraints| constraints.iter())
            .map(|id| {
                self.image
                    .constraints
                    .get(id)
                    .expect("validated table constraint references an existing constraint")
            })
    }

    pub(crate) fn columns_for_constraint(
        &self,
        constraint: ConstraintId,
    ) -> impl Iterator<Item = &ColumnDef> {
        self.image
            .constraint_columns
            .get(&constraint)
            .into_iter()
            .flat_map(|columns| columns.values())
            .map(|id| {
                self.image
                    .columns
                    .get(id)
                    .expect("validated constraint column references an existing column")
            })
    }

    pub(crate) fn find_parent_key_constraint(
        &self,
        table_id: TableId,
        ordered_columns: &[ColumnId],
    ) -> Option<&ConstraintDef> {
        self.constraints_for_table(table_id).find(|constraint| {
            matches!(
                constraint.kind(),
                ConstraintKind::PrimaryKey | ConstraintKind::Unique
            ) && self
                .image
                .constraint_columns
                .get(&constraint.id())
                .is_some_and(|columns| {
                    columns
                        .values()
                        .copied()
                        .eq(ordered_columns.iter().copied())
                })
        })
    }

    pub(crate) fn foreign_keys_referencing(
        &self,
        parent_constraint: ConstraintId,
    ) -> impl Iterator<Item = &ConstraintDef> {
        self.image
            .referenced_by
            .get(&parent_constraint)
            .into_iter()
            .flat_map(|children| children.iter())
            .map(|child| {
                self.image
                    .constraints
                    .get(child)
                    .expect("validated referenced-by mapping references a foreign key")
            })
    }

    pub(crate) fn referenced_by_exists(&self, parent_constraint: ConstraintId) -> bool {
        self.image.referenced_by.contains_key(&parent_constraint)
    }

    fn validate_foreign_key_candidate(
        &self,
        child: &ConstraintDef,
        local_columns: &[ColumnId],
    ) -> Result<ForeignKeyTarget> {
        let target = child.foreign_key_target().ok_or_else(|| {
            DbError::new(ErrorCode::Constraint, "foreign key has no parent target")
        })?;
        if child.table_id() == target.table_id() {
            return Err(DbError::new(
                ErrorCode::Constraint,
                "foreign key cannot reference its own table",
            ));
        }
        let parent = self
            .image
            .constraints
            .get(&target.constraint_id())
            .ok_or_else(|| {
                DbError::new(
                    ErrorCode::Constraint,
                    "foreign key references a missing parent constraint",
                )
            })?;
        if parent.table_id() != target.table_id()
            || !matches!(
                parent.kind(),
                ConstraintKind::PrimaryKey | ConstraintKind::Unique
            )
        {
            return Err(DbError::new(
                ErrorCode::Constraint,
                "foreign-key parent is not an eligible key constraint",
            ));
        }
        let index = parent
            .index_id()
            .and_then(|id| self.image.indexes.get(&id))
            .filter(|index| {
                index.table_id() == parent.table_id()
                    && index.ownership() == IndexOwnership::Constraint(parent.id())
                    && index.unique()
            })
            .ok_or_else(|| {
                DbError::new(
                    ErrorCode::Constraint,
                    "foreign-key parent has no constraint-owned unique system index",
                )
            })?;
        let parent_columns = self
            .image
            .constraint_columns
            .get(&parent.id())
            .ok_or_else(|| corruption("validated parent constraint columns are missing"))?;
        let index_columns = self
            .image
            .index_columns
            .get(&index.id())
            .ok_or_else(|| corruption("validated parent index columns are missing"))?;
        if parent_columns.values().ne(index_columns.values()) {
            return Err(corruption(
                "validated parent constraint and index column order differs",
            ));
        }
        if local_columns.len() != parent_columns.len() {
            return Err(DbError::new(
                ErrorCode::Constraint,
                "foreign-key child and parent column counts differ",
            ));
        }
        for (child_column_id, parent_column_id) in local_columns.iter().zip(parent_columns.values())
        {
            let child_column = self.image.columns.get(child_column_id).ok_or_else(|| {
                DbError::new(
                    ErrorCode::Constraint,
                    "foreign-key child column does not exist",
                )
            })?;
            let parent_column =
                self.image.columns.get(parent_column_id).ok_or_else(|| {
                    corruption("validated foreign-key parent column does not exist")
                })?;
            if child_column.table_id() != child.table_id() {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    "foreign-key child column belongs to another table",
                ));
            }
            let child_type = child_column.column_type();
            let parent_type = parent_column.column_type();
            if child_type != parent_type
                && !(child_type == types::ColumnType::Int64
                    && parent_type == types::ColumnType::Decimal)
            {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    "foreign-key child and parent column types are incompatible",
                ));
            }
        }
        Ok(target)
    }

    /// Returns whether a persisted V1 object has this exact typed identity.
    ///
    pub(crate) fn object_id_exists(&self, kind: ObjectKind, object_id: ObjectId) -> bool {
        match kind {
            ObjectKind::Table => TableId::from_object(object_id, kind)
                .ok()
                .is_some_and(|id| self.image.tables.contains_key(&id)),
            ObjectKind::Column => ColumnId::from_object(object_id, kind)
                .ok()
                .is_some_and(|id| self.image.columns.contains_key(&id)),
            ObjectKind::Index => IndexId::from_object(object_id, kind)
                .ok()
                .is_some_and(|id| self.image.indexes.contains_key(&id)),
            ObjectKind::Constraint => ConstraintId::from_object(object_id, kind)
                .ok()
                .is_some_and(|id| self.image.constraints.contains_key(&id)),
        }
    }

    /// Returns whether this object kind's persistent name namespace is occupied.
    ///
    pub(crate) fn object_name_exists(&self, kind: ObjectKind, name: &CatalogName) -> bool {
        match kind {
            ObjectKind::Table => self.image.table_names.contains_key(name.as_str()),
            ObjectKind::Index => self.image.index_names.contains_key(name.as_str()),
            ObjectKind::Constraint => self.image.constraint_names.contains_key(name.as_str()),
            ObjectKind::Column => false,
        }
    }

    pub(crate) const fn next_object_id(&self) -> ObjectId {
        self.image.meta.next_object_id()
    }
}

/// Batch-local directory identity. Mutations never retain a borrow of the batch.
pub(super) struct CatalogWriter {
    tree: BTree,
    dirty: bool,
}

pub(super) struct CatalogWriterCheckpoint {
    root: PageRef,
    dirty: bool,
}

impl CatalogWriter {
    pub(super) fn bootstrap(batch: &mut WriteBatch) -> Result<Self> {
        let checkpoint = batch.begin_mutation_scope()?;
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
                CatalogReader::open_validated(batch, &tree, report, features)?;
                Ok(Self { tree, dirty: false })
            }
        })();
        match result {
            Ok(writer) => {
                batch.finish_mutation_scope(checkpoint)?;
                Ok(writer)
            }
            Err(error) => {
                batch.rollback_mutation_scope(checkpoint)?;
                Err(error)
            }
        }
    }

    pub(super) const fn checkpoint(&self) -> CatalogWriterCheckpoint {
        CatalogWriterCheckpoint {
            root: self.tree.root(),
            dirty: self.dirty,
        }
    }

    /// Restore the batch image and the writer's separately owned state together.
    pub(super) fn restore(&mut self, writer_checkpoint: CatalogWriterCheckpoint) {
        self.tree = BTree::from_root(TreeKind::Table, writer_checkpoint.root);
        self.dirty = writer_checkpoint.dirty;
    }

    pub(super) const fn root(&self) -> PageRef {
        self.tree.root()
    }

    fn mutate<T>(
        &mut self,
        batch: &mut WriteBatch,
        mutation: impl FnOnce(&mut BTree, &mut WriteBatch) -> Result<T>,
    ) -> Result<T> {
        let checkpoint = batch.begin_mutation_scope()?;
        let writer_checkpoint = self.checkpoint();
        match mutation(&mut self.tree, batch) {
            Ok(value) => {
                // Bootstrap owns the pending gate until finish, even on clean opens.
                self.dirty = true;
                batch.finish_mutation_scope(checkpoint)?;
                Ok(value)
            }
            Err(error) => {
                batch.rollback_mutation_scope(checkpoint)?;
                self.tree = BTree::from_root(TreeKind::Table, writer_checkpoint.root);
                self.dirty = writer_checkpoint.dirty;
                Err(error)
            }
        }
    }

    pub(crate) fn allocate_object_id(&mut self, batch: &mut WriteBatch) -> Result<ObjectId> {
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

    /// Check the writer's candidate catalog directly so generated names can be
    /// selected after their stable constraint ID is allocated but before any
    /// definition record is published.
    pub(crate) fn constraint_name_exists(
        &self,
        batch: &mut WriteBatch,
        name: &CatalogName,
    ) -> Result<bool> {
        Ok(self
            .tree
            .get(batch, &key::constraint_name_key(name)?)?
            .is_some())
    }

    pub(crate) fn insert_table_definition(
        &mut self,
        batch: &mut WriteBatch,
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
        batch: &mut WriteBatch,
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

    pub(crate) fn insert_index_definition(
        &mut self,
        batch: &mut WriteBatch,
        index: &IndexDef,
        columns: &[ColumnId],
    ) -> Result<()> {
        self.mutate(batch, |tree, batch| {
            if index.ownership() != IndexOwnership::Explicit {
                return Err(DbError::new(
                    ErrorCode::Unsupported,
                    "constraint-owned indexes cannot be mutated before constraint catalog support",
                ));
            }
            if columns.len() != usize::from(index.column_count()) {
                return Err(DbError::new(
                    ErrorCode::Type,
                    "index column list does not match the index column count",
                ));
            }
            read_table_definition(tree, batch, index.table_id())?;
            require_available_object(tree, batch, index.id().into())?;
            let name_key = key::index_name_key(index.name())?;
            require_available_key(tree, batch, &name_key, "duplicate index name")?;
            require_available_key(
                tree,
                batch,
                &key::table_index_key(index.table_id(), index.id()),
                "duplicate table index mapping",
            )?;
            let mut seen = BTreeSet::new();
            for (ordinal, column_id) in columns.iter().copied().enumerate() {
                if !seen.insert(column_id) {
                    return Err(DbError::new(
                        ErrorCode::Constraint,
                        "index column list contains a repeated column",
                    ));
                }
                let ordinal = u16::try_from(ordinal).map_err(|_| {
                    DbError::new(ErrorCode::Type, "index column ordinal exceeds u16")
                })?;
                let column = read_column_definition(tree, batch, column_id)?;
                if column.table_id() != index.table_id() {
                    return Err(DbError::new(
                        ErrorCode::Constraint,
                        "index column is not owned by the index table",
                    ));
                }
                require_available_key(
                    tree,
                    batch,
                    &key::index_column_key(index.id(), ordinal),
                    "duplicate index column ordinal",
                )?;
            }
            if TreeKind::from_root_page_type(batch.read(index.root())?.header().page_type())
                != Some(TreeKind::Index)
            {
                return Err(corruption(
                    "catalog index root must reference an Index B+Tree",
                ));
            }
            tree.insert(
                batch,
                &key::index_definition_key(index.id()),
                &record::encode_index_definition(index),
            )?;
            tree.insert(
                batch,
                &name_key,
                &record::encode_index_name_mapping(index.id()),
            )?;
            tree.insert(
                batch,
                &key::table_index_key(index.table_id(), index.id()),
                &record::encode_table_index_mapping(index.id()),
            )?;
            for (ordinal, column_id) in columns.iter().copied().enumerate() {
                let ordinal = u16::try_from(ordinal).map_err(|_| {
                    DbError::new(ErrorCode::Type, "index column ordinal exceeds u16")
                })?;
                tree.insert(
                    batch,
                    &key::index_column_key(index.id(), ordinal),
                    &record::encode_index_column_mapping(column_id),
                )?;
            }
            batch.track_tree(TreeKind::Index, index.root())?;
            batch.enable_feature_bits(INDEX_CONSTRAINT_V1)
        })
    }

    pub(crate) fn insert_constraint_bundle(
        &mut self,
        batch: &mut WriteBatch,
        bundle: &ConstraintBundle,
    ) -> Result<()> {
        self.insert_constraint_bundle_with_record_hook(batch, bundle, |_| Ok(()))
    }

    fn insert_constraint_bundle_with_record_hook(
        &mut self,
        batch: &mut WriteBatch,
        bundle: &ConstraintBundle,
        mut record_hook: impl FnMut(usize) -> Result<()>,
    ) -> Result<()> {
        let features = batch.feature_bits();
        let current = CatalogReader::open_candidate(batch, &self.tree, features)?;
        let has_primary_key = current
            .constraints_for_table(bundle.definition.table_id())
            .any(|constraint| constraint.kind() == ConstraintKind::PrimaryKey);
        let foreign_key_target = if bundle.definition.kind() == ConstraintKind::ForeignKey {
            Some(
                current
                    .validate_foreign_key_candidate(&bundle.definition, bundle.columns.as_ref())?,
            )
        } else {
            None
        };
        self.mutate(batch, |tree, batch| {
            let mut record_number = 0_usize;
            let mut record_written = || {
                let completed = record_number;
                record_number = record_number.checked_add(1).ok_or_else(|| {
                    DbError::new(
                        ErrorCode::ResourceLimit,
                        "constraint bundle record count is exhausted",
                    )
                })?;
                record_hook(completed)
            };
            let definition = &bundle.definition;
            read_table_definition(tree, batch, definition.table_id())?;
            require_allocated_object_id(tree, batch, definition.id().into())?;
            require_available_object(tree, batch, definition.id().into())?;
            let constraint_name_key = key::constraint_name_key(definition.name())?;
            let table_constraint_key =
                key::table_constraint_key(definition.table_id(), definition.id());
            require_available_key(
                tree,
                batch,
                &constraint_name_key,
                "duplicate constraint name",
            )?;
            require_available_key(
                tree,
                batch,
                &table_constraint_key,
                "duplicate table constraint mapping",
            )?;
            if bundle.columns.len() != usize::from(definition.column_count()) {
                return Err(DbError::new(
                    ErrorCode::Type,
                    "constraint column list does not match the constraint column count",
                ));
            }
            if definition.kind() == ConstraintKind::PrimaryKey && has_primary_key {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    "table already has a primary key",
                ));
            }
            let mut seen_columns = BTreeSet::new();
            for (ordinal, column_id) in bundle.columns.iter().copied().enumerate() {
                if !seen_columns.insert(column_id) {
                    return Err(DbError::new(
                        ErrorCode::Constraint,
                        "constraint column list contains a repeated column",
                    ));
                }
                let ordinal = u16::try_from(ordinal).map_err(|_| {
                    DbError::new(ErrorCode::Type, "constraint column ordinal exceeds u16")
                })?;
                let column = read_column_definition(tree, batch, column_id)?;
                if column.table_id() != definition.table_id() {
                    return Err(DbError::new(
                        ErrorCode::Constraint,
                        "constraint column is not owned by the constraint table",
                    ));
                }
                if matches!(
                    definition.kind(),
                    ConstraintKind::PrimaryKey | ConstraintKind::NotNull
                ) && column.nullable()
                {
                    return Err(DbError::new(
                        ErrorCode::Constraint,
                        "primary-key and not-null columns must be non-nullable",
                    ));
                }
                require_available_key(
                    tree,
                    batch,
                    &key::constraint_column_key(definition.id(), ordinal),
                    "duplicate constraint column ordinal",
                )?;
            }

            let prepared_index = match (&bundle.system_index, definition.index_id()) {
                (Some((index, index_columns)), Some(index_id)) => {
                    if index.id() != index_id
                        || index.table_id() != definition.table_id()
                        || index.ownership() != IndexOwnership::Constraint(definition.id())
                    {
                        return Err(DbError::new(
                            ErrorCode::Constraint,
                            "system index identity does not match its owner constraint",
                        ));
                    }
                    if !index.unique() {
                        return Err(DbError::new(
                            ErrorCode::Constraint,
                            "key constraint system index must be unique",
                        ));
                    }
                    if index_columns.as_ref() != bundle.columns.as_ref()
                        || index_columns.len() != usize::from(index.column_count())
                    {
                        return Err(DbError::new(
                            ErrorCode::Constraint,
                            "system index columns do not match the owner constraint",
                        ));
                    }
                    if index.id().get() == definition.id().get() {
                        return Err(DbError::new(
                            ErrorCode::Constraint,
                            "catalog definitions share a global object ID",
                        ));
                    }
                    require_allocated_object_id(tree, batch, index.id().into())?;
                    require_available_object(tree, batch, index.id().into())?;
                    let index_name_key = key::index_name_key(index.name())?;
                    let table_index_key = key::table_index_key(index.table_id(), index.id());
                    require_available_key(tree, batch, &index_name_key, "duplicate index name")?;
                    require_available_key(
                        tree,
                        batch,
                        &table_index_key,
                        "duplicate table index mapping",
                    )?;
                    for (ordinal, _) in index_columns.iter().enumerate() {
                        let ordinal = u16::try_from(ordinal).map_err(|_| {
                            DbError::new(ErrorCode::Type, "index column ordinal exceeds u16")
                        })?;
                        require_available_key(
                            tree,
                            batch,
                            &key::index_column_key(index.id(), ordinal),
                            "duplicate index column ordinal",
                        )?;
                    }
                    batch.require_current_catalog_root(index.root())?;
                    if TreeKind::from_root_page_type(batch.read(index.root())?.header().page_type())
                        != Some(TreeKind::Index)
                    {
                        return Err(corruption(
                            "catalog index root must reference an Index B+Tree",
                        ));
                    }
                    validate_tree(batch, &BTree::from_root(TreeKind::Index, index.root()))?;
                    let mut live = validate_tree(batch, tree)?.all_live_pages();
                    validate_relation_roots(
                        batch,
                        current.image.tables.values().map(TableDef::root),
                        current
                            .image
                            .indexes
                            .values()
                            .map(IndexDef::root)
                            .chain([index.root()]),
                        &mut live,
                    )?;
                    Some((index, index_columns, index_name_key, table_index_key))
                }
                (None, None) => None,
                _ => {
                    return Err(DbError::new(
                        ErrorCode::Constraint,
                        "constraint and system index presence do not match",
                    ));
                }
            };

            tree.insert(
                batch,
                &key::constraint_definition_key(definition.id()),
                &record::encode_constraint_definition(definition),
            )?;
            record_written()?;
            tree.insert(
                batch,
                &constraint_name_key,
                &record::encode_constraint_name_mapping(definition.id()),
            )?;
            record_written()?;
            tree.insert(
                batch,
                &table_constraint_key,
                &record::encode_table_constraint_mapping(definition.id()),
            )?;
            record_written()?;
            for (ordinal, column_id) in bundle.columns.iter().copied().enumerate() {
                let ordinal = u16::try_from(ordinal).map_err(|_| {
                    DbError::new(ErrorCode::Type, "constraint column ordinal exceeds u16")
                })?;
                tree.insert(
                    batch,
                    &key::constraint_column_key(definition.id(), ordinal),
                    &record::encode_constraint_column_mapping(column_id),
                )?;
                record_written()?;
            }
            if let Some(target) = foreign_key_target {
                insert_referenced_by(tree, batch, target.constraint_id(), definition.id())?;
                record_written()?;
            }
            if let Some((index, index_columns, index_name_key, table_index_key)) = prepared_index {
                tree.insert(
                    batch,
                    &key::index_definition_key(index.id()),
                    &record::encode_index_definition(index),
                )?;
                record_written()?;
                tree.insert(
                    batch,
                    &index_name_key,
                    &record::encode_index_name_mapping(index.id()),
                )?;
                record_written()?;
                tree.insert(
                    batch,
                    &table_index_key,
                    &record::encode_table_index_mapping(index.id()),
                )?;
                record_written()?;
                for (ordinal, column_id) in index_columns.iter().copied().enumerate() {
                    let ordinal = u16::try_from(ordinal).map_err(|_| {
                        DbError::new(ErrorCode::Type, "index column ordinal exceeds u16")
                    })?;
                    tree.insert(
                        batch,
                        &key::index_column_key(index.id(), ordinal),
                        &record::encode_index_column_mapping(column_id),
                    )?;
                    record_written()?;
                }
                batch.track_tree(TreeKind::Index, index.root())?;
            }
            #[cfg(test)]
            constraint_bundle_fault_for_test(batch, ConstraintBundleFaultPoint::RecordsInserted)?;
            batch.enable_feature_bits(INDEX_CONSTRAINT_V1)?;
            #[cfg(test)]
            constraint_bundle_fault_for_test(batch, ConstraintBundleFaultPoint::FeatureEnabled)?;
            Ok(())
        })
    }

    pub(crate) fn update_table_root(
        &mut self,
        batch: &mut WriteBatch,
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

    pub(crate) fn update_index_root(
        &mut self,
        batch: &mut WriteBatch,
        index_id: IndexId,
        root: PageRef,
    ) -> Result<()> {
        self.mutate(batch, |tree, batch| {
            let index = read_index_definition(tree, batch, index_id)?;
            batch.require_tracked_index_root(root)?;
            replace_record(
                tree,
                batch,
                &key::index_definition_key(index_id),
                &record::encode_index_definition(&IndexDef::new(
                    index.id(),
                    index.table_id(),
                    root,
                    index.column_count(),
                    index.unique(),
                    index.ownership(),
                    index.name().clone(),
                )?),
            )
        })
    }

    pub(crate) fn remove_constraint(
        &mut self,
        batch: &mut WriteBatch,
        constraint: &ConstraintDef,
        columns: &[ColumnId],
    ) -> Result<()> {
        let features = batch.feature_bits();
        let reader = CatalogReader::open_candidate(batch, &self.tree, features)?;
        let report = validate_tree(batch, &self.tree)?;
        reader.validate_deep_from(batch, &self.tree, report, features)?;
        self.mutate(batch, |tree, batch| {
            require_matching_constraint_record(tree, batch, constraint, columns)?;
            if constraint.foreign_key_target().is_none()
                && reader.referenced_by_exists(constraint.id())
            {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    "referenced parent constraint cannot be removed",
                ));
            }
            let system_index = if let Some(index_id) = constraint.index_id() {
                let index = read_index_definition(tree, batch, index_id)?;
                if index.ownership() != IndexOwnership::Constraint(constraint.id()) {
                    return Err(corruption(
                        "bound constraint system index has the wrong owner",
                    ));
                }
                require_matching_index_record(tree, batch, &index, columns)?;
                Some(index)
            } else {
                None
            };
            if let Some(target) = constraint.foreign_key_target() {
                remove_referenced_by(tree, batch, target.constraint_id(), constraint.id())?;
                #[cfg(test)]
                constraint_removal_fault_for_test(ConstraintRemovalFaultPoint::ReferencedBy)?;
            }
            for (ordinal, _) in columns.iter().enumerate() {
                let ordinal = u16::try_from(ordinal).map_err(|_| {
                    DbError::new(ErrorCode::Type, "constraint column ordinal exceeds u16")
                })?;
                delete_required_record(
                    tree,
                    batch,
                    &key::constraint_column_key(constraint.id(), ordinal),
                )?;
            }
            #[cfg(test)]
            constraint_removal_fault_for_test(ConstraintRemovalFaultPoint::Columns)?;
            delete_required_record(
                tree,
                batch,
                &key::table_constraint_key(constraint.table_id(), constraint.id()),
            )?;
            #[cfg(test)]
            constraint_removal_fault_for_test(ConstraintRemovalFaultPoint::TableMapping)?;
            delete_required_record(tree, batch, &key::constraint_name_key(constraint.name())?)?;
            #[cfg(test)]
            constraint_removal_fault_for_test(ConstraintRemovalFaultPoint::NameMapping)?;
            delete_required_record(
                tree,
                batch,
                &key::constraint_definition_key(constraint.id()),
            )?;
            #[cfg(test)]
            constraint_removal_fault_for_test(ConstraintRemovalFaultPoint::Definition)?;
            if let Some(index) = system_index {
                for (ordinal, _) in columns.iter().enumerate() {
                    let ordinal = u16::try_from(ordinal).map_err(|_| {
                        DbError::new(ErrorCode::Type, "index column ordinal exceeds u16")
                    })?;
                    delete_required_record(
                        tree,
                        batch,
                        &key::index_column_key(index.id(), ordinal),
                    )?;
                }
                delete_required_record(
                    tree,
                    batch,
                    &key::table_index_key(index.table_id(), index.id()),
                )?;
                delete_required_record(tree, batch, &key::index_name_key(index.name())?)?;
                delete_required_record(tree, batch, &key::index_definition_key(index.id()))?;
                batch.release_tree_later(TreeKind::Index, index.root())?;
            }
            Ok(())
        })
    }

    pub(crate) fn allocate_rowid(
        &mut self,
        batch: &mut WriteBatch,
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

    pub(crate) fn bump_table_schema_version(
        &mut self,
        batch: &mut WriteBatch,
        table_id: TableId,
    ) -> Result<SchemaVersion> {
        self.mutate(batch, |tree, batch| {
            let table = read_table_definition(tree, batch, table_id)?;
            let next = table.schema_version().get().checked_add(1).ok_or_else(|| {
                DbError::new(ErrorCode::Type, "table schema version counter is exhausted")
            })?;
            let schema_version = SchemaVersion::new(next)?;
            replace_record(
                tree,
                batch,
                &key::table_definition_key(table_id),
                &record::encode_table_definition(&table.with_schema_version(schema_version)),
            )?;
            Ok(schema_version)
        })
    }

    pub(crate) fn remove_index(
        &mut self,
        batch: &mut WriteBatch,
        index: &IndexDef,
        columns: &[ColumnId],
    ) -> Result<()> {
        let features = batch.feature_bits();
        let reader = CatalogReader::open_candidate(batch, &self.tree, features)?;
        // The writer can own an unpublished bootstrap or split root. Validate
        // that exact candidate, not the previous root still in the header.
        let report = validate_tree(batch, &self.tree)?;
        reader.validate_deep_from(batch, &self.tree, report, features)?;
        self.mutate(batch, |tree, batch| {
            if index.ownership() != IndexOwnership::Explicit {
                return Err(DbError::new(
                    ErrorCode::Unsupported,
                    "constraint-owned indexes cannot be mutated before constraint catalog support",
                ));
            }
            require_matching_index_record(tree, batch, index, columns)?;
            for (ordinal, column_id) in columns.iter().copied().enumerate() {
                let ordinal = u16::try_from(ordinal).map_err(|_| {
                    DbError::new(ErrorCode::Type, "index column ordinal exceeds u16")
                })?;
                let key = key::index_column_key(index.id(), ordinal);
                let value = tree.get(batch, &key)?.ok_or_else(|| {
                    corruption("bound index column mapping is missing before mutation")
                })?;
                if record::decode_index_column_mapping(&value)? != column_id {
                    return Err(corruption(
                        "bound index column mapping changed before mutation",
                    ));
                }
                delete_required_record(tree, batch, &key)?;
            }
            delete_required_record(
                tree,
                batch,
                &key::table_index_key(index.table_id(), index.id()),
            )?;
            delete_required_record(tree, batch, &key::index_name_key(index.name())?)?;
            delete_required_record(tree, batch, &key::index_definition_key(index.id()))?;
            batch.release_tree_later(TreeKind::Index, index.root())
        })
    }

    pub(crate) fn remove_relation(
        &mut self,
        batch: &mut WriteBatch,
        table: &TableDef,
        columns: &[ColumnDef],
    ) -> Result<()> {
        self.mutate(batch, |tree, batch| {
            if columns.len() != usize::from(table.column_count()) {
                return Err(corruption(
                    "bound relation columns do not match the table column count",
                ));
            }
            require_matching_table_record(tree, batch, table)?;
            for (ordinal, column) in columns.iter().enumerate() {
                if column.table_id() != table.id() || usize::from(column.ordinal()) != ordinal {
                    return Err(corruption(
                        "bound relation columns do not match the table definition",
                    ));
                }
                require_matching_column_record(tree, batch, column)?;
            }

            for column in columns {
                delete_required_record(
                    tree,
                    batch,
                    &key::column_name_key(column.table_id(), column.name())?,
                )?;
                delete_required_record(
                    tree,
                    batch,
                    &key::column_ordinal_key(column.table_id(), column.ordinal()),
                )?;
                delete_required_record(tree, batch, &key::column_definition_key(column.id()))?;
            }
            delete_required_record(tree, batch, &key::table_name_key(table.name())?)?;
            delete_required_record(tree, batch, &key::table_definition_key(table.id()))?;
            batch.release_tree_later(TreeKind::Table, table.root())
        })
    }

    /// Publish only the fully validated current root; failure leaves commit gated.
    pub(super) fn finish(self, batch: &mut WriteBatch) -> Result<()> {
        if self.dirty {
            CatalogReader::open_candidate(batch, &self.tree, batch.feature_bits())?;
            batch.track_tree(TreeKind::Table, self.tree.root())?;
            batch.set_catalog_root(self.tree.root())?;
        }
        batch.finish_catalog_update()
    }
}

fn read_table_definition(tree: &BTree, batch: &mut WriteBatch, id: TableId) -> Result<TableDef> {
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

fn read_column_definition(tree: &BTree, batch: &mut WriteBatch, id: ColumnId) -> Result<ColumnDef> {
    let bytes = tree
        .get(batch, &key::column_definition_key(id))?
        .ok_or_else(|| {
            DbError::new(
                ErrorCode::Corruption,
                format!("catalog column definition {} is missing", id.get()),
            )
        })?;
    record::decode_column_definition(id, &bytes)
}

fn read_index_definition(tree: &BTree, batch: &mut WriteBatch, id: IndexId) -> Result<IndexDef> {
    let bytes = tree
        .get(batch, &key::index_definition_key(id))?
        .ok_or_else(|| {
            DbError::new(
                ErrorCode::Corruption,
                format!("catalog index definition {} is missing", id.get()),
            )
        })?;
    record::decode_index_definition(id, &bytes)
}

fn read_constraint_definition(
    tree: &BTree,
    batch: &mut WriteBatch,
    id: ConstraintId,
) -> Result<ConstraintDef> {
    let bytes = tree
        .get(batch, &key::constraint_definition_key(id))?
        .ok_or_else(|| {
            DbError::new(
                ErrorCode::Corruption,
                format!("catalog constraint definition {} is missing", id.get()),
            )
        })?;
    record::decode_constraint_definition(id, &bytes)
}

fn require_allocated_object_id(tree: &BTree, batch: &mut WriteBatch, id: ObjectId) -> Result<()> {
    let bytes = tree
        .get(batch, &key::catalog_meta_key())?
        .ok_or_else(|| corruption("catalog meta record is missing"))?;
    if id.get() >= record::decode_catalog_meta(&bytes)?.next_object_id().get() {
        return Err(DbError::new(
            ErrorCode::Constraint,
            format!("catalog object ID {} has not been allocated", id.get()),
        ));
    }
    Ok(())
}

fn require_available_object(tree: &BTree, batch: &mut WriteBatch, id: ObjectId) -> Result<()> {
    for key in [
        key::table_definition_key(TableId::new(id.get())?),
        key::column_definition_key(ColumnId::new(id.get())?),
        key::index_definition_key(IndexId::new(id.get())?),
        key::constraint_definition_key(ConstraintId::new(id.get())?),
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
    batch: &mut WriteBatch,
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
    batch: &mut WriteBatch,
    key: &[u8],
    value: &[u8],
) -> Result<()> {
    if tree.delete(batch, key)?.is_none() {
        return Err(corruption("catalog replacement record is missing"));
    }
    tree.insert(batch, key, value)
}

fn insert_referenced_by(
    tree: &mut BTree,
    batch: &mut WriteBatch,
    parent: ConstraintId,
    child: ConstraintId,
) -> Result<()> {
    let key = key::referenced_by_key(parent, child);
    require_available_key(tree, batch, &key, "duplicate referenced-by mapping")?;
    tree.insert(batch, &key, &record::encode_referenced_by_mapping(child))
}

fn remove_referenced_by(
    tree: &mut BTree,
    batch: &mut WriteBatch,
    parent: ConstraintId,
    child: ConstraintId,
) -> Result<()> {
    let key = key::referenced_by_key(parent, child);
    let value = tree
        .get(batch, &key)?
        .ok_or_else(|| corruption("bound referenced-by mapping is missing before mutation"))?;
    if record::decode_referenced_by_mapping(&value)? != child {
        return Err(corruption(
            "bound referenced-by mapping changed before mutation",
        ));
    }
    delete_required_record(tree, batch, &key)
}

fn require_matching_table_record(
    tree: &BTree,
    batch: &mut WriteBatch,
    expected: &TableDef,
) -> Result<()> {
    let bytes = tree
        .get(batch, &key::table_definition_key(expected.id()))?
        .ok_or_else(|| corruption("bound table definition is missing before mutation"))?;
    if record::decode_table_definition(expected.id(), &bytes)? != *expected {
        return Err(corruption("bound table definition changed before mutation"));
    }
    let name_bytes = tree
        .get(batch, &key::table_name_key(expected.name())?)?
        .ok_or_else(|| corruption("bound table name mapping is missing before mutation"))?;
    if record::decode_table_name_mapping(&name_bytes)? != expected.id() {
        return Err(corruption(
            "bound table name mapping changed before mutation",
        ));
    }
    Ok(())
}

fn require_matching_column_record(
    tree: &BTree,
    batch: &mut WriteBatch,
    expected: &ColumnDef,
) -> Result<()> {
    let bytes = tree
        .get(batch, &key::column_definition_key(expected.id()))?
        .ok_or_else(|| corruption("bound column definition is missing before mutation"))?;
    if record::decode_column_definition(expected.id(), &bytes)? != *expected {
        return Err(corruption(
            "bound column definition changed before mutation",
        ));
    }
    let name_bytes = tree
        .get(
            batch,
            &key::column_name_key(expected.table_id(), expected.name())?,
        )?
        .ok_or_else(|| corruption("bound column name mapping is missing before mutation"))?;
    if record::decode_column_name_mapping(&name_bytes)? != expected.id() {
        return Err(corruption(
            "bound column name mapping changed before mutation",
        ));
    }
    let ordinal_bytes = tree
        .get(
            batch,
            &key::column_ordinal_key(expected.table_id(), expected.ordinal()),
        )?
        .ok_or_else(|| corruption("bound column ordinal mapping is missing before mutation"))?;
    if record::decode_column_ordinal_mapping(&ordinal_bytes)? != expected.id() {
        return Err(corruption(
            "bound column ordinal mapping changed before mutation",
        ));
    }
    Ok(())
}

fn require_matching_index_record(
    tree: &BTree,
    batch: &mut WriteBatch,
    expected: &IndexDef,
    columns: &[ColumnId],
) -> Result<()> {
    if columns.len() != usize::from(expected.column_count()) {
        return Err(corruption(
            "bound index columns do not match the index column count",
        ));
    }
    let bytes = tree
        .get(batch, &key::index_definition_key(expected.id()))?
        .ok_or_else(|| corruption("bound index definition is missing before mutation"))?;
    if record::decode_index_definition(expected.id(), &bytes)? != *expected {
        return Err(corruption("bound index definition changed before mutation"));
    }
    let name_bytes = tree
        .get(batch, &key::index_name_key(expected.name())?)?
        .ok_or_else(|| corruption("bound index name mapping is missing before mutation"))?;
    if record::decode_index_name_mapping(&name_bytes)? != expected.id() {
        return Err(corruption(
            "bound index name mapping changed before mutation",
        ));
    }
    let table_bytes = tree
        .get(
            batch,
            &key::table_index_key(expected.table_id(), expected.id()),
        )?
        .ok_or_else(|| corruption("bound table index mapping is missing before mutation"))?;
    if record::decode_table_index_mapping(&table_bytes)? != expected.id() {
        return Err(corruption(
            "bound table index mapping changed before mutation",
        ));
    }
    Ok(())
}

fn require_matching_constraint_record(
    tree: &BTree,
    batch: &mut WriteBatch,
    expected: &ConstraintDef,
    columns: &[ColumnId],
) -> Result<()> {
    if columns.len() != usize::from(expected.column_count()) {
        return Err(corruption(
            "bound constraint columns do not match the constraint column count",
        ));
    }
    if read_constraint_definition(tree, batch, expected.id())? != *expected {
        return Err(corruption(
            "bound constraint definition changed before mutation",
        ));
    }
    let name_bytes = tree
        .get(batch, &key::constraint_name_key(expected.name())?)?
        .ok_or_else(|| corruption("bound constraint name mapping is missing before mutation"))?;
    if record::decode_constraint_name_mapping(&name_bytes)? != expected.id() {
        return Err(corruption(
            "bound constraint name mapping changed before mutation",
        ));
    }
    let table_bytes = tree
        .get(
            batch,
            &key::table_constraint_key(expected.table_id(), expected.id()),
        )?
        .ok_or_else(|| corruption("bound table constraint mapping is missing before mutation"))?;
    if record::decode_table_constraint_mapping(&table_bytes)? != expected.id() {
        return Err(corruption(
            "bound table constraint mapping changed before mutation",
        ));
    }
    for (ordinal, column_id) in columns.iter().copied().enumerate() {
        let ordinal = u16::try_from(ordinal)
            .map_err(|_| DbError::new(ErrorCode::Type, "constraint column ordinal exceeds u16"))?;
        let bytes = tree
            .get(batch, &key::constraint_column_key(expected.id(), ordinal))?
            .ok_or_else(|| {
                corruption("bound constraint column mapping is missing before mutation")
            })?;
        if record::decode_constraint_column_mapping(&bytes)? != column_id {
            return Err(corruption(
                "bound constraint column mapping changed before mutation",
            ));
        }
    }
    Ok(())
}

fn delete_required_record(tree: &mut BTree, batch: &mut WriteBatch, key: &[u8]) -> Result<()> {
    if tree.delete(batch, key)?.is_none() {
        return Err(corruption("catalog record is missing during mutation"));
    }
    Ok(())
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

fn require_index_feature(feature_bits: u64) -> Result<()> {
    if feature_bits & INDEX_CONSTRAINT_V1 == 0 {
        return Err(corruption(
            "index or constraint catalog record requires INDEX_CONSTRAINT_V1",
        ));
    }
    Ok(())
}

fn unsupported_legacy() -> DbError {
    DbError::new(
        ErrorCode::Unsupported,
        "nonempty catalog without CATALOG_V1 is unsupported",
    )
}

#[cfg(test)]
pub(crate) fn drop_and_recreate_users(pager: &super::Pager) -> Result<CatalogReader> {
    let (old_table, old_columns) = users_for_test_mutation(pager)?;
    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;

    writer.mutate(&mut batch, |tree, batch| {
        require_matching_table_record(tree, batch, &old_table)?;
        for column in &old_columns {
            require_matching_column_record(tree, batch, column)?;
        }

        for column in &old_columns {
            delete_required_record(
                tree,
                batch,
                &key::column_name_key(column.table_id(), column.name())?,
            )?;
            delete_required_record(
                tree,
                batch,
                &key::column_ordinal_key(column.table_id(), column.ordinal()),
            )?;
            delete_required_record(tree, batch, &key::column_definition_key(column.id()))?;
        }
        delete_required_record(tree, batch, &key::table_name_key(old_table.name())?)?;
        delete_required_record(tree, batch, &key::table_definition_key(old_table.id()))?;
        batch.release_btree_later(old_table.root())?;
        Ok(())
    })?;

    let new_table_id = TableId::new(writer.allocate_object_id(&mut batch)?.get())?;
    if new_table_id == old_table.id() {
        return Err(corruption("test replacement table must use a new identity"));
    }
    let new_root = BTree::create(&mut batch, TreeKind::Table)?.root();
    let new_table = TableDef::new(
        new_table_id,
        old_table.schema_version(),
        new_root,
        old_table.next_rowid(),
        old_table.column_count(),
        old_table.name().clone(),
    )?;
    writer.insert_table_definition(&mut batch, &new_table)?;
    for old_column in &old_columns {
        let new_column = ColumnDef::new(
            ColumnId::new(writer.allocate_object_id(&mut batch)?.get())?,
            new_table_id,
            old_column.ordinal(),
            old_column.column_type(),
            old_column.nullable(),
            old_column.name().clone(),
        )?;
        writer.insert_column_definition(&mut batch, &new_column)?;
    }
    writer.finish(&mut batch)?;
    batch.commit()?;

    let replacement = open_catalog_for_test(pager)?;
    let users_name = CatalogName::new("USERS")?;
    let replacement_users = replacement
        .find_table_by_name(&users_name)
        .ok_or_else(|| corruption("test replacement USERS definition is missing"))?;
    if replacement_users.id() == old_table.id()
        || replacement_users.schema_version() != old_table.schema_version()
        || replacement_users.column_count() != old_table.column_count()
    {
        return Err(corruption(
            "test replacement USERS definition is not cross-checked",
        ));
    }
    for (replacement_column, old_column) in replacement
        .columns_for_table(replacement_users.id())
        .zip(old_columns.iter())
    {
        if replacement_column.ordinal() != old_column.ordinal()
            || replacement_column.name() != old_column.name()
            || replacement_column.column_type() != old_column.column_type()
            || replacement_column.nullable() != old_column.nullable()
            || replacement_column.id() == old_column.id()
        {
            return Err(corruption(
                "test replacement USERS columns are not cross-checked",
            ));
        }
    }
    Ok(replacement)
}

#[cfg(test)]
pub(crate) fn replace_users_id_column_id(pager: &super::Pager) -> Result<CatalogReader> {
    let (table, columns) = users_for_test_mutation(pager)?;
    let id_name = CatalogName::new("ID")?;
    let old_column = columns
        .iter()
        .find(|column| column.name() == &id_name)
        .cloned()
        .ok_or_else(|| corruption("test catalog is missing USERS.ID"))?;
    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;

    writer.mutate(&mut batch, |tree, batch| {
        require_matching_table_record(tree, batch, &table)?;
        require_matching_column_record(tree, batch, &old_column)?;
        delete_required_record(
            tree,
            batch,
            &key::column_name_key(old_column.table_id(), old_column.name())?,
        )?;
        delete_required_record(
            tree,
            batch,
            &key::column_ordinal_key(old_column.table_id(), old_column.ordinal()),
        )?;
        delete_required_record(tree, batch, &key::column_definition_key(old_column.id()))
    })?;

    let replacement = ColumnDef::new(
        ColumnId::new(writer.allocate_object_id(&mut batch)?.get())?,
        old_column.table_id(),
        old_column.ordinal(),
        old_column.column_type(),
        old_column.nullable(),
        old_column.name().clone(),
    )?;
    writer.insert_column_definition(&mut batch, &replacement)?;
    writer.finish(&mut batch)?;
    batch.commit()?;

    let reader = open_catalog_for_test(pager)?;
    let replaced_column = reader
        .find_column_by_name(table.id(), &id_name)
        .ok_or_else(|| corruption("test replacement USERS.ID is missing"))?;
    if replaced_column.id() == old_column.id()
        || replaced_column.id() != replacement.id()
        || replaced_column.table_id() != old_column.table_id()
        || replaced_column.ordinal() != old_column.ordinal()
        || replaced_column.column_type() != old_column.column_type()
        || replaced_column.nullable() != old_column.nullable()
        || table.schema_version()
            != reader
                .find_table_by_id(table.id())
                .ok_or_else(|| {
                    corruption("test USERS table disappeared during column replacement")
                })?
                .schema_version()
    {
        return Err(corruption(
            "test replacement USERS.ID was not cross-checked",
        ));
    }
    Ok(reader)
}

#[cfg(test)]
pub(crate) fn bump_users_schema_version(pager: &super::Pager) -> Result<CatalogReader> {
    let (old_table, _) = users_for_test_mutation(pager)?;
    let next_version = old_table
        .schema_version()
        .get()
        .checked_add(1)
        .ok_or_else(|| DbError::new(ErrorCode::Type, "test schema version is exhausted"))?;
    let updated = TableDef::new(
        old_table.id(),
        types::SchemaVersion::new(next_version)?,
        old_table.root(),
        old_table.next_rowid(),
        old_table.column_count(),
        old_table.name().clone(),
    )?;
    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;
    writer.mutate(&mut batch, |tree, batch| {
        require_matching_table_record(tree, batch, &old_table)?;
        replace_record(
            tree,
            batch,
            &key::table_definition_key(old_table.id()),
            &record::encode_table_definition(&updated),
        )
    })?;
    writer.finish(&mut batch)?;
    batch.commit()?;

    let reader = open_catalog_for_test(pager)?;
    if reader
        .find_table_by_id(old_table.id())
        .is_none_or(|table| table.schema_version() != updated.schema_version())
    {
        return Err(corruption("test USERS schema version was not committed"));
    }
    Ok(reader)
}

#[cfg(test)]
pub(crate) fn add_unrelated_table(pager: &super::Pager) -> Result<CatalogReader> {
    let mut batch = pager.begin_write()?;
    let mut writer = CatalogWriter::bootstrap(&mut batch)?;
    let table_id = TableId::new(writer.allocate_object_id(&mut batch)?.get())?;
    let root = BTree::create(&mut batch, TreeKind::Table)?.root();
    let table = TableDef::new(
        table_id,
        types::SchemaVersion::new(1)?,
        root,
        RowId::new(1)?,
        1,
        CatalogName::new("OTHER")?,
    )?;
    writer.insert_table_definition(&mut batch, &table)?;
    let column = ColumnDef::new(
        ColumnId::new(writer.allocate_object_id(&mut batch)?.get())?,
        table_id,
        0,
        types::ColumnType::Int64,
        false,
        CatalogName::new("ID")?,
    )?;
    writer.insert_column_definition(&mut batch, &column)?;
    writer.finish(&mut batch)?;
    batch.commit()?;

    let reader = open_catalog_for_test(pager)?;
    if reader.find_table_by_name(table.name()) != Some(&table) {
        return Err(corruption("test unrelated table was not committed"));
    }
    Ok(reader)
}

#[cfg(test)]
fn users_for_test_mutation(pager: &super::Pager) -> Result<(TableDef, Vec<ColumnDef>)> {
    let reader = open_catalog_for_test(pager)?;
    let users_name = CatalogName::new("USERS")?;
    let table = reader
        .find_table_by_name(&users_name)
        .cloned()
        .ok_or_else(|| corruption("test catalog is missing USERS"))?;
    let columns = reader
        .columns_for_table(table.id())
        .cloned()
        .collect::<Vec<_>>();
    if columns.len() != usize::from(table.column_count()) {
        return Err(corruption("test USERS catalog columns are incomplete"));
    }
    Ok((table, columns))
}

#[cfg(test)]
fn open_catalog_for_test(pager: &super::Pager) -> Result<CatalogReader> {
    let mut snapshot = pager.begin_read()?;
    CatalogReader::open_empty_or_v1(&mut snapshot)
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
            constraint::program::{CheckProgramTemplate, CheckTemplateInstruction, CheckType},
            features::CATALOG_V1,
            file::OpenMode,
            page::PageType,
            pager::{Pager, PagerConfig, WriteBatch},
            relation::{ColumnSpec, TableSpec, TableWrite, create_relation, drop_relation},
            row::{RowCodec, encode_rowid_key},
            test_support::{TempDatabase, test_pager},
        },
    };

    type ConstraintCandidateMutation = Box<dyn Fn(&mut WriteBatch, &mut Records, PageRef, PageRef)>;
    type ConstraintCandidateCase = (&'static str, ErrorCode, u64, ConstraintCandidateMutation);
    type CheckProgramMutation = Box<dyn Fn(&mut Records)>;
    type CheckProgramCase = (&'static str, ErrorCode, CheckProgramMutation);

    #[derive(Clone, Copy)]
    enum ForeignKeyCandidate {
        MissingReferencedBy,
        OrphanReferencedBy,
        WrongParentKey,
        WrongChildValue,
        ExplicitUniqueParentIndex,
        ParentColumnOrderMismatch,
        ChildInt64ParentDecimal,
        ChildDecimalParentInt64,
        SelfReference,
        Valid,
    }

    #[test]
    fn check_program_error_normalization_preserves_non_corruption_codes() {
        assert_eq!(
            normalize_check_program_error(DbError::new(ErrorCode::Unsupported, "future")).code(),
            ErrorCode::Unsupported
        );
        assert_eq!(
            normalize_check_program_error(DbError::new(ErrorCode::ResourceLimit, "limit")).code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            normalize_check_program_error(DbError::new(ErrorCode::Corruption, "bad")).code(),
            ErrorCode::Corruption
        );
    }

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

    #[test]
    fn remove_relation_deletes_exact_records_and_releases_the_complete_bound_tree() {
        // Production break caught: deleting by name or releasing only the root
        // leaves column mappings, descendants, or overflow pages live after DROP.
        let (_temp, pager) = test_pager(4096).unwrap();
        let spec = TableSpec::new(
            CatalogName::new("T").unwrap(),
            vec![ColumnSpec::new(
                CatalogName::new("PAYLOAD").unwrap(),
                ColumnType::Blob,
                false,
            )],
        )
        .unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let table_id = create_relation(&mut batch, &spec).unwrap();
        let mut table_write = TableWrite::open(&mut batch, table_id).unwrap();
        for value in 0_u8..96 {
            table_write
                .insert(&mut batch, &[crate::Value::Blob(vec![value; 128])])
                .unwrap();
        }
        table_write.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let (table, columns, user_pages) = {
            let mut snapshot = pager.begin_read().unwrap();
            let reader = snapshot.catalog_reader().unwrap();
            let table = reader.find_table_by_id(table_id).unwrap().clone();
            let columns = reader
                .columns_for_table(table_id)
                .cloned()
                .collect::<Vec<_>>();
            let pages = validate_tree(
                &mut snapshot,
                &BTree::from_root(TreeKind::Table, table.root()),
            )
            .unwrap()
            .all_live_pages();
            assert!(
                pages.len() > 1,
                "fixture must own descendants or overflow pages"
            );
            (table, columns, pages)
        };
        let committed_page_count = pager.page_count();

        let mut batch = pager.try_begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        assert_eq!(
            writer
                .remove_relation(&mut batch, &table, &[])
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert!(
            writer
                .tree
                .get(&mut batch, &table_definition_key(table_id))
                .unwrap()
                .is_some(),
            "failed exact-match validation must restore the table record"
        );

        writer
            .remove_relation(&mut batch, &table, &columns)
            .unwrap();
        assert!(
            writer
                .tree
                .get(&mut batch, &table_definition_key(table_id))
                .unwrap()
                .is_none()
        );
        assert!(
            writer
                .tree
                .get(&mut batch, &table_name_key(table.name()).unwrap())
                .unwrap()
                .is_none()
        );
        for column in &columns {
            for key in [
                column_definition_key(column.id()),
                column_name_key(table_id, column.name()).unwrap(),
                column_ordinal_key(table_id, column.ordinal()),
            ] {
                assert!(writer.tree.get(&mut batch, &key).unwrap().is_none());
            }
        }
        assert!(
            writer
                .tree
                .get(&mut batch, &catalog_meta_key())
                .unwrap()
                .is_some(),
            "DROP must retain unrelated catalog records"
        );
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let mut snapshot = pager.begin_read().unwrap();
        assert!(
            snapshot
                .catalog_reader()
                .unwrap()
                .find_table_by_id(table_id)
                .is_none()
        );
        drop(snapshot);

        let mut allocation = pager.try_begin_write().unwrap();
        let mut reused = BTreeSet::new();
        loop {
            let page = allocation.allocate(PageType::TableLeaf).unwrap();
            if page.page_id().0 >= committed_page_count {
                break;
            }
            reused.insert(page.page_id());
        }
        assert!(
            user_pages.is_subset(&reused),
            "every page owned by the dropped table must return to the freelist"
        );
        allocation.rollback().unwrap();
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

    fn explicit_index(root: PageRef) -> IndexDef {
        IndexDef::new(
            IndexId::new(30).unwrap(),
            TableId::new(10).unwrap(),
            root,
            2,
            false,
            IndexOwnership::Explicit,
            CatalogName::new("IX_T_AB").unwrap(),
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

    #[test]
    fn catalog_requires_bidirectional_index_records_and_contiguous_columns() {
        // Production break caught: accepting any incomplete or inconsistent
        // direction makes the decoded image unsafe for later DDL consumers.
        let (_temp, pager) = raw_index_catalog(|_, _, _, _| {});
        let mut read = pager.begin_read().unwrap();
        let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        let index_name = CatalogName::new("IX_T_AB").unwrap();
        let index = catalog.find_index_by_name(&index_name).unwrap();
        assert!(std::ptr::eq(
            index,
            catalog.find_index_by_id(index.id()).unwrap()
        ));
        assert_eq!(
            catalog
                .indexes_for_table(index.table_id())
                .map(IndexDef::id)
                .collect::<Vec<_>>(),
            [index.id()]
        );
        assert_eq!(
            catalog
                .columns_for_index(index.id())
                .map(ColumnDef::id)
                .collect::<Vec<_>>(),
            [ColumnId::new(20).unwrap(), ColumnId::new(21).unwrap()]
        );
        assert!(catalog.object_id_exists(ObjectKind::Index, index.id().into()));
        assert!(catalog.object_name_exists(ObjectKind::Index, &index_name));
        drop(read);

        assert_bad_index_records("missing index name", |rows, _, _, _| {
            remove_record(
                rows,
                &index_name_key(&CatalogName::new("IX_T_AB").unwrap()).unwrap(),
            );
        });
        assert_bad_index_records("missing table index", |rows, _, _, _| {
            remove_record(
                rows,
                &table_index_key(TableId::new(10).unwrap(), IndexId::new(30).unwrap()),
            );
        });
        assert_bad_index_records("missing index column", |rows, _, _, _| {
            remove_record(rows, &index_column_key(IndexId::new(30).unwrap(), 0));
        });
        assert_bad_index_records("repeated index column", |rows, _, _, _| {
            *record_value_mut(rows, &index_column_key(IndexId::new(30).unwrap(), 1)) =
                encode_index_column_mapping(ColumnId::new(20).unwrap());
        });
        assert_bad_index_records("noncontiguous index columns", |rows, _, _, _| {
            let value = remove_record(rows, &index_column_key(IndexId::new(30).unwrap(), 1));
            rows.push((index_column_key(IndexId::new(30).unwrap(), 2), value));
        });
        assert_bad_index_records("wrong table index", |rows, _, _, _| {
            let value = remove_record(
                rows,
                &table_index_key(TableId::new(10).unwrap(), IndexId::new(30).unwrap()),
            );
            rows.push((
                table_index_key(TableId::new(40).unwrap(), IndexId::new(30).unwrap()),
                value,
            ));
        });
        assert_bad_index_records("foreign index column", |rows, _, _, _| {
            *record_value_mut(rows, &index_column_key(IndexId::new(30).unwrap(), 1)) =
                encode_index_column_mapping(ColumnId::new(41).unwrap());
        });
        assert_bad_index_records(
            "index reuses a column object ID",
            |rows, _, index_root, _| {
                let old_id = IndexId::new(30).unwrap();
                let colliding_id = IndexId::new(20).unwrap();
                *record_value_mut(
                    rows,
                    &index_name_key(&CatalogName::new("IX_T_AB").unwrap()).unwrap(),
                ) = encode_index_name_mapping(colliding_id);
                remove_record(rows, &index_definition_key(old_id));
                let colliding = IndexDef::new(
                    colliding_id,
                    TableId::new(10).unwrap(),
                    index_root,
                    2,
                    false,
                    IndexOwnership::Explicit,
                    CatalogName::new("IX_T_AB").unwrap(),
                )
                .unwrap();
                rows.push((
                    index_definition_key(colliding_id),
                    encode_index_definition(&colliding),
                ));
                remove_record(rows, &table_index_key(TableId::new(10).unwrap(), old_id));
                rows.push((
                    table_index_key(TableId::new(10).unwrap(), colliding_id),
                    encode_table_index_mapping(colliding_id),
                ));
                for ordinal in 0..2 {
                    let value = remove_record(rows, &index_column_key(old_id, ordinal));
                    rows.push((index_column_key(colliding_id, ordinal), value));
                }
            },
        );
        assert_bad_index_records("index root has table kind", |rows, table_root, _, _| {
            let index = explicit_index(table_root);
            *record_value_mut(rows, &index_definition_key(index.id())) =
                encode_index_definition(&index);
        });
    }

    #[test]
    fn catalog_rejects_4c2_records_without_the_feature_bit() {
        let (_temp, pager) = raw_index_catalog_with_features(CATALOG_V1, |_, _, _, _| {});
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn catalog_requires_feature_bit_for_the_entire_4c2_key_range() {
        // A future family inside the reserved 4C2 range is unsupported only
        // after the header has declared that catalog extension.
        for family in [0x24, 0x2f] {
            assert_bad_records(
                "undeclared 4C2 key family",
                ErrorCode::Corruption,
                |rows, _| {
                    rows.push((vec![family], vec![]));
                },
            );
            assert_bad_records_with_features(
                "declared future 4C2 key family",
                ErrorCode::Unsupported,
                CATALOG_V1 | INDEX_CONSTRAINT_V1,
                |rows, _| rows.push((vec![family], vec![])),
            );
        }
    }

    #[test]
    fn phase_4c2_index_corruption_rejects_malformed_physical_entries_only_during_deep_validation() {
        // Production break caught: generic B+Tree validation accepts opaque
        // bytes, so deep catalog validation must decode every index key with
        // its declared columns and require empty leaf values.
        use crate::storage::index::key::IndexKeyCodec;

        let columns = [column(20, 0, "A"), column(21, 1, "B")];
        let valid = IndexKeyCodec::encode(
            &columns,
            &[crate::Value::Int64(7), crate::Value::Int64(9)],
            RowId::new(1).unwrap(),
        )
        .unwrap()
        .into_physical();
        let mut zero_rowid = valid.clone();
        let rowid_start = zero_rowid.len() - 8;
        zero_rowid[rowid_start..].fill(0);

        for (case, key, value) in [
            ("malformed key", vec![0], Vec::new()),
            ("nonempty leaf value", valid, vec![1]),
            ("missing ROWID", zero_rowid, Vec::new()),
        ] {
            let (_temp, pager) = raw_index_catalog_with_entries(&[(key, value)]);
            let mut read = pager.begin_read().unwrap();
            let catalog = CatalogReader::open_empty_or_v1(&mut read)
                .expect("ordinary open must remain root/catalog-only");
            assert_eq!(
                catalog.validate_deep(&mut read).unwrap_err().code(),
                ErrorCode::Corruption,
                "{case}"
            );
        }
    }

    #[test]
    fn phase_4c2_format2_real_files_open_lazily_and_read_only_preserves_every_byte() {
        // Production break caught: opening an older format-2 file must not
        // manufacture catalog/index state, and malformed feature declarations
        // must be classified without rewriting either persisted file.
        let (temp, pager) = test_pager(4096).unwrap();
        drop(pager);
        assert_format2_read_only_open_preserves_bytes(&temp, 0, false, None);

        let (temp, pager, _) = committed_single_column_catalog();
        drop(pager);
        assert_format2_read_only_open_preserves_bytes(&temp, CATALOG_V1, false, None);

        let (temp, pager, _) = committed_single_column_catalog();
        let mut batch = pager.begin_write().unwrap();
        batch.enable_feature_bits(INDEX_CONSTRAINT_V1).unwrap();
        batch.commit().unwrap();
        drop(pager);
        assert_format2_read_only_open_preserves_bytes(
            &temp,
            CATALOG_V1 | INDEX_CONSTRAINT_V1,
            false,
            None,
        );

        let (temp, pager) = raw_index_catalog(|_, _, _, _| {});
        drop(pager);
        assert_format2_read_only_open_preserves_bytes(
            &temp,
            CATALOG_V1 | INDEX_CONSTRAINT_V1,
            true,
            None,
        );

        let (temp, pager) = raw_index_catalog_with_features(CATALOG_V1, |_, _, _, _| {});
        drop(pager);
        assert_format2_read_only_open_preserves_bytes(
            &temp,
            CATALOG_V1,
            false,
            Some(ErrorCode::Corruption),
        );

        let (temp, pager) = test_pager(4096).unwrap();
        drop(pager);
        set_real_file_feature_bits(&temp, INDEX_CONSTRAINT_V1);
        assert_format2_read_only_open_preserves_bytes(
            &temp,
            INDEX_CONSTRAINT_V1,
            false,
            Some(ErrorCode::Corruption),
        );

        let (temp, pager) = test_pager(4096).unwrap();
        drop(pager);
        set_real_file_feature_bits(&temp, 1 << 63);
        assert_format2_read_only_open_preserves_bytes(
            &temp,
            1 << 63,
            false,
            Some(ErrorCode::Unsupported),
        );
    }

    #[test]
    fn remove_index_validates_the_unpublished_writer_catalog() {
        // Destructive validation must use the writer's candidate root, not
        // the still-empty header, when CREATE and DROP share a fresh batch.
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let table = new_table(1, root, 1, "T");
        let column = new_column(2, table.id(), 0, "C");
        writer.insert_table_definition(&mut batch, &table).unwrap();
        writer
            .insert_column_definition(&mut batch, &column)
            .unwrap();
        let index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let index = IndexDef::new(
            IndexId::new(3).unwrap(),
            table.id(),
            index_root,
            1,
            false,
            IndexOwnership::Explicit,
            CatalogName::new("IX_T_C").unwrap(),
        )
        .unwrap();
        writer
            .insert_index_definition(&mut batch, &index, &[column.id()])
            .unwrap();
        writer
            .remove_index(&mut batch, &index, &[column.id()])
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        let catalog = read.catalog_reader().unwrap();
        assert!(catalog.find_index_by_id(index.id()).is_none());
        assert_eq!(catalog.find_table_by_id(table.id()), Some(&table));
        assert_eq!(read.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
        catalog.validate_deep(&mut read).unwrap();
    }

    #[test]
    fn index_catalog_mutations_restore_records_roots_and_schema_version() {
        // Production break caught: statement rollback must restore catalog bytes,
        // feature bits, schema version, and every tracked index-root transition.
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let column_id = {
            let mut read = pager.begin_read().unwrap();
            read.catalog_reader()
                .unwrap()
                .columns_for_table(table_id)
                .next()
                .unwrap()
                .id()
        };
        let mut batch = pager.begin_write().unwrap();
        let checkpoint = batch.statement_checkpoint().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let index_id = IndexId::new(writer.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
        let old_root = tree.root();
        let index = IndexDef::new(
            index_id,
            table_id,
            old_root,
            1,
            false,
            IndexOwnership::Explicit,
            CatalogName::new("IX_T_C").unwrap(),
        )
        .unwrap();
        writer
            .insert_index_definition(&mut batch, &index, &[column_id])
            .unwrap();
        split_index_tree(&mut tree, &mut batch);
        assert_ne!(tree.root(), old_root);
        writer
            .update_index_root(&mut batch, index.id(), tree.root())
            .unwrap();
        assert_eq!(
            writer
                .bump_table_schema_version(&mut batch, table_id)
                .unwrap(),
            SchemaVersion::new(2).unwrap()
        );
        writer.finish(&mut batch).unwrap();

        let catalog = batch.catalog_reader().unwrap();
        let staged = catalog.find_index_by_id(index.id()).unwrap();
        assert_eq!(staged.root(), tree.root());
        assert_eq!(
            catalog.find_table_by_id(table_id).unwrap().schema_version(),
            SchemaVersion::new(2).unwrap()
        );
        assert_eq!(
            batch.feature_bits(),
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1
        );

        batch.restore_statement(checkpoint);
        let restored = batch.catalog_reader().unwrap();
        assert!(restored.find_index_by_id(index.id()).is_none());
        assert_eq!(
            restored
                .find_table_by_id(table_id)
                .unwrap()
                .schema_version(),
            SchemaVersion::new(1).unwrap()
        );
        assert_eq!(batch.feature_bits(), CATALOG_V1);
        batch.commit().unwrap();
    }

    #[test]
    fn remove_index_requires_exact_records_and_releases_the_complete_tree() {
        // Production break caught: a partial delete or root-only release leaves
        // catalog links, descendants, or overflow pages reachable after DROP.
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let column_id = {
            let mut read = pager.begin_read().unwrap();
            read.catalog_reader()
                .unwrap()
                .columns_for_table(table_id)
                .next()
                .unwrap()
                .id()
        };
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let index_id = IndexId::new(writer.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let mut tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
        let indexed_column = column(column_id.get(), 0, "C");
        for key in 0_u64..1_000 {
            let physical = crate::storage::index::key::IndexKeyCodec::encode(
                std::slice::from_ref(&indexed_column),
                &[crate::Value::Int64(i64::try_from(key).unwrap())],
                RowId::new(key + 1).unwrap(),
            )
            .unwrap()
            .into_physical();
            tree.insert(&mut batch, &physical, &[]).unwrap();
        }
        let index = IndexDef::new(
            index_id,
            table_id,
            tree.root(),
            1,
            true,
            IndexOwnership::Explicit,
            CatalogName::new("IX_T_C").unwrap(),
        )
        .unwrap();
        writer
            .insert_index_definition(&mut batch, &index, &[column_id])
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let (index, index_pages) = {
            let mut read = pager.begin_read().unwrap();
            let catalog = read.catalog_reader().unwrap();
            let index = catalog.find_index_by_id(index_id).unwrap().clone();
            let pages = validate_tree(&mut read, &BTree::from_root(TreeKind::Index, index.root()))
                .unwrap()
                .all_live_pages();
            assert!(pages.len() > 2);
            (index, pages)
        };
        let committed_page_count = pager.page_count();

        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let before = current_records(&writer, &mut batch);
        assert_eq!(
            writer
                .remove_index(
                    &mut batch,
                    &index,
                    &[ColumnId::new(column_id.get() + 100).unwrap()],
                )
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(current_records(&writer, &mut batch), before);
        writer
            .remove_index(&mut batch, &index, &[column_id])
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let catalog = read.catalog_reader().unwrap();
        assert!(catalog.find_index_by_id(index.id()).is_none());
        assert!(catalog.indexes_for_table(table_id).next().is_none());
        assert_eq!(
            read.feature_bits(),
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1
        );
        drop(read);

        let mut allocation = pager.begin_write().unwrap();
        let mut reused = BTreeSet::new();
        loop {
            let page = allocation.allocate(PageType::IndexLeaf).unwrap();
            if page.page_id().0 >= committed_page_count {
                break;
            }
            reused.insert(page.page_id());
        }
        assert!(index_pages.is_subset(&reused));
        allocation.rollback().unwrap();
    }

    #[test]
    fn rejected_index_insertions_leave_records_tracking_and_feature_bits_unchanged() {
        // Every rejected logical insertion must restore the complete batch,
        // including a failure encountered after catalog records were written.
        let (_temp, pager) = raw_index_catalog(|_, _, _, _| {});
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let released = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        batch.release_btree_later(released).unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let columns = [ColumnId::new(20).unwrap(), ColumnId::new(21).unwrap()];
        let foreign = ColumnId::new(41).unwrap();
        let cases = [
            (
                30,
                10,
                root,
                "NEW",
                columns.to_vec(),
                IndexOwnership::Explicit,
                ErrorCode::Constraint,
            ),
            (
                20,
                10,
                root,
                "NEW",
                columns.to_vec(),
                IndexOwnership::Explicit,
                ErrorCode::Constraint,
            ),
            (
                100,
                10,
                root,
                "IX_T_AB",
                columns.to_vec(),
                IndexOwnership::Explicit,
                ErrorCode::Constraint,
            ),
            (
                100,
                10,
                root,
                "NEW",
                vec![columns[0]],
                IndexOwnership::Explicit,
                ErrorCode::Type,
            ),
            (
                100,
                10,
                root,
                "NEW",
                vec![columns[0]; 2],
                IndexOwnership::Explicit,
                ErrorCode::Constraint,
            ),
            (
                100,
                10,
                root,
                "NEW",
                vec![columns[0], foreign],
                IndexOwnership::Explicit,
                ErrorCode::Constraint,
            ),
            (
                100,
                99,
                root,
                "NEW",
                columns.to_vec(),
                IndexOwnership::Explicit,
                ErrorCode::Corruption,
            ),
            (
                100,
                10,
                table_root,
                "NEW",
                columns.to_vec(),
                IndexOwnership::Explicit,
                ErrorCode::Corruption,
            ),
            (
                100,
                10,
                released,
                "NEW",
                columns.to_vec(),
                IndexOwnership::Explicit,
                ErrorCode::Corruption,
            ),
            (
                100,
                10,
                root,
                "NEW",
                columns.to_vec(),
                IndexOwnership::Constraint(ConstraintId::new(101).unwrap()),
                ErrorCode::Unsupported,
            ),
        ];
        for (id, table, root, name, supplied, ownership, expected) in cases {
            let index = IndexDef::new(
                IndexId::new(id).unwrap(),
                TableId::new(table).unwrap(),
                root,
                2,
                false,
                ownership,
                CatalogName::new(name).unwrap(),
            )
            .unwrap();
            let records_before = current_records(&writer, &mut batch);
            let before = batch.checkpoint_mutation().unwrap();
            let writer_root = writer.root();
            assert_eq!(
                writer
                    .insert_index_definition(&mut batch, &index, &supplied)
                    .unwrap_err()
                    .code(),
                expected
            );
            before.assert_matches_batch_for_test(&batch);
            assert_eq!(writer.root(), writer_root);
            assert!(!writer.dirty);
            assert_eq!(current_records(&writer, &mut batch), records_before);
        }
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn index_object_id_allocation_and_exhausted_schema_counter_are_atomic() {
        // Allocation must include the index namespace; schema increment must
        // reject u64::MAX without wrapping or altering any other record.
        let (_temp, pager) = raw_index_catalog(|rows, table_root, _, _| {
            *record_value_mut(rows, &catalog_meta_key()) =
                encode_catalog_meta(&CatalogMeta::new(ObjectId::new(30).unwrap()));
            let table =
                table(table_root).with_schema_version(SchemaVersion::new(u64::MAX).unwrap());
            *record_value_mut(rows, &table_definition_key(table.id())) =
                encode_table_definition(&table);
        });
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let before = batch.checkpoint_mutation().unwrap();
        assert_eq!(
            writer.allocate_object_id(&mut batch).unwrap_err().code(),
            ErrorCode::Constraint
        );
        before.assert_matches_batch_for_test(&batch);
        assert_eq!(
            writer
                .bump_table_schema_version(&mut batch, TableId::new(10).unwrap())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        before.assert_matches_batch_for_test(&batch);
        assert!(!writer.dirty);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn index_root_and_schema_changes_commit_and_reopen_without_changing_other_records() {
        // Root replacement must reject stale/untracked/wrong-kind references;
        // successful replacement and schema bump affect only their definitions.
        let (temp, pager) = raw_index_catalog(|_, _, _, _| {});
        let mut batch = pager.begin_write().unwrap();
        let old = batch.catalog_reader().unwrap();
        let original = old
            .find_index_by_id(IndexId::new(30).unwrap())
            .unwrap()
            .clone();
        let original_table = old.find_table_by_id(original.table_id()).unwrap().clone();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let mut index_tree = BTree::from_root(TreeKind::Index, original.root());
        split_two_column_index_tree(&mut index_tree, &mut batch);
        let untracked = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let wrong_kind = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        batch.track_tree(TreeKind::Index, wrong_kind).unwrap();
        for root in [
            original.root(),
            untracked,
            wrong_kind,
            PageRef::new(
                index_tree.root().page_id(),
                index_tree.root().generation() + 1,
            )
            .unwrap(),
        ] {
            let before = batch.checkpoint_mutation().unwrap();
            assert_eq!(
                writer
                    .update_index_root(&mut batch, original.id(), root)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
            before.assert_matches_batch_for_test(&batch);
        }
        // Remove the intentionally wrong test registration before publication.
        batch
            .refresh_tree(TreeKind::Index, wrong_kind, index_tree.root())
            .unwrap();
        let before_records = current_records(&writer, &mut batch);
        writer
            .update_index_root(&mut batch, original.id(), index_tree.root())
            .unwrap();
        let next = writer
            .bump_table_schema_version(&mut batch, original.table_id())
            .unwrap();
        assert_eq!(next.get(), original_table.schema_version().get() + 1);
        let after_records = current_records(&writer, &mut batch);
        let changed_keys = before_records
            .iter()
            .zip(&after_records)
            .filter_map(|(before, after)| (before != after).then_some(before.0.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            changed_keys,
            [
                table_definition_key(original.table_id()),
                index_definition_key(original.id())
            ]
        );
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        drop(pager);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let pager = Pager::open(temp.path(), mode, PagerConfig::test_default(4096)).unwrap();
            let mut read = pager.begin_read().unwrap();
            let catalog = read.catalog_reader().unwrap();
            let expected = IndexDef::new(
                original.id(),
                original.table_id(),
                index_tree.root(),
                original.column_count(),
                original.unique(),
                original.ownership(),
                original.name().clone(),
            )
            .unwrap();
            assert_eq!(catalog.find_index_by_id(original.id()), Some(&expected));
            assert_eq!(
                catalog.find_table_by_id(original.table_id()),
                Some(&original_table.clone().with_schema_version(next))
            );
            catalog.validate_deep(&mut read).unwrap();
        }
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

    fn current_records(writer: &CatalogWriter, batch: &mut WriteBatch) -> Records {
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

    fn split_user_tree(tree: &mut BTree, batch: &mut WriteBatch) -> u64 {
        let old_root = tree.root();
        for key in 0u64..200 {
            tree.insert(batch, &key.to_be_bytes(), &[7; 128]).unwrap();
            if tree.root() != old_root {
                return key + 1;
            }
        }
        panic!("fixture must split its user tree root");
    }

    fn split_index_tree(tree: &mut BTree, batch: &mut WriteBatch) -> u64 {
        let old_root = tree.root();
        for key in 0_u64..200 {
            tree.insert(batch, &key.to_be_bytes(), &[7; 128]).unwrap();
            if tree.root() != old_root {
                return key + 1;
            }
        }
        panic!("fixture must split its index tree root");
    }

    fn split_two_column_index_tree(tree: &mut BTree, batch: &mut WriteBatch) -> u64 {
        let old_root = tree.root();
        let columns = [column(20, 0, "A"), column(21, 1, "B")];
        for key in 0_u64..200 {
            let physical = crate::storage::index::key::IndexKeyCodec::encode(
                &columns,
                &[
                    crate::Value::Int64(i64::try_from(key).unwrap()),
                    crate::Value::Int64(i64::try_from(key).unwrap()),
                ],
                RowId::new(key + 1).unwrap(),
            )
            .unwrap()
            .into_physical();
            tree.insert(batch, &physical, &[]).unwrap();
            if tree.root() != old_root {
                return key + 1;
            }
        }
        panic!("fixture must split its two-column index tree root");
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
        let mutate = |tree: &mut BTree, batch: &mut WriteBatch| {
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

    fn grow_catalog_for_split(writer: &mut CatalogWriter, batch: &mut WriteBatch) {
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

    fn index_records(table_root: PageRef, index_root: PageRef, foreign_root: PageRef) -> Records {
        let mut rows = records(table_root);
        let foreign = new_table(40, foreign_root, 1, "F");
        let foreign_column = new_column(41, foreign.id(), 0, "Z");
        let index = explicit_index(index_root);
        rows.extend([
            (
                table_name_key(foreign.name()).unwrap(),
                encode_table_name_mapping(foreign.id()),
            ),
            (
                table_definition_key(foreign.id()),
                encode_table_definition(&foreign),
            ),
            (
                column_name_key(foreign.id(), foreign_column.name()).unwrap(),
                encode_column_name_mapping(foreign_column.id()),
            ),
            (
                column_ordinal_key(foreign.id(), foreign_column.ordinal()),
                encode_column_ordinal_mapping(foreign_column.id()),
            ),
            (
                column_definition_key(foreign_column.id()),
                encode_column_definition(&foreign_column),
            ),
            (
                index_name_key(index.name()).unwrap(),
                encode_index_name_mapping(index.id()),
            ),
            (
                index_definition_key(index.id()),
                encode_index_definition(&index),
            ),
            (
                table_index_key(index.table_id(), index.id()),
                encode_table_index_mapping(index.id()),
            ),
            (
                index_column_key(index.id(), 0),
                encode_index_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                index_column_key(index.id(), 1),
                encode_index_column_mapping(ColumnId::new(21).unwrap()),
            ),
        ]);
        rows
    }

    #[test]
    fn constraint_bundle_insert_remove_and_root_update_restore_exact_candidate() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let table_id = TableId::new(writer.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let table = new_table(table_id.get(), table_root, 1, "T");
        writer.insert_table_definition(&mut batch, &table).unwrap();
        let column_id =
            ColumnId::new(writer.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let column = ColumnDef::new(
            column_id,
            table_id,
            0,
            ColumnType::Int64,
            false,
            CatalogName::new("C").unwrap(),
        )
        .unwrap();
        writer
            .insert_column_definition(&mut batch, &column)
            .unwrap();
        let constraint_id =
            ConstraintId::new(writer.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let index_id = IndexId::new(writer.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let definition = ConstraintDef::new(
            constraint_id,
            table_id,
            ConstraintKind::PrimaryKey,
            1,
            Some(index_id),
            None,
            Box::new([]),
            CatalogName::new("PK_T").unwrap(),
        )
        .unwrap();
        let index = IndexDef::new(
            index_id,
            table_id,
            index_root,
            1,
            true,
            IndexOwnership::Constraint(constraint_id),
            CatalogName::new("SYS_PK_T").unwrap(),
        )
        .unwrap();
        let bundle = ConstraintBundle {
            definition: definition.clone(),
            columns: vec![column_id].into_boxed_slice(),
            system_index: Some((index.clone(), vec![column_id].into_boxed_slice())),
        };
        let before_insert = current_records(&writer, &mut batch);
        for fail_after in 0..8 {
            let checkpoint = batch.checkpoint_mutation().unwrap();
            let writer_root = writer.root();
            let error = writer
                .insert_constraint_bundle_with_record_hook(&mut batch, &bundle, |record_number| {
                    if record_number == fail_after {
                        Err(DbError::new(
                            ErrorCode::Io,
                            "injected after constraint bundle record",
                        ))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Io, "record {fail_after}");
            checkpoint.assert_matches_batch_for_test(&batch);
            assert_eq!(writer.root(), writer_root);
            assert_eq!(current_records(&writer, &mut batch), before_insert);
        }
        writer
            .insert_constraint_bundle(&mut batch, &bundle)
            .unwrap();
        let records_after_insert = current_records(&writer, &mut batch);
        let checkpoint = batch.checkpoint_mutation().unwrap();
        let writer_root = writer.root();
        assert_eq!(
            writer
                .insert_constraint_bundle(&mut batch, &bundle)
                .unwrap_err()
                .code(),
            ErrorCode::Constraint
        );
        checkpoint.assert_matches_batch_for_test(&batch);
        assert_eq!(writer.root(), writer_root);
        assert_eq!(current_records(&writer, &mut batch), records_after_insert);
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let catalog = read.catalog_reader().unwrap();
        assert_eq!(
            catalog.find_constraint_by_id(constraint_id),
            Some(&definition)
        );
        assert_eq!(
            catalog.find_constraint_by_name(&CatalogName::new("PK_T").unwrap()),
            Some(&definition)
        );
        assert_eq!(
            catalog
                .constraints_for_table(table_id)
                .map(ConstraintDef::id)
                .collect::<Vec<_>>(),
            [constraint_id]
        );
        assert_eq!(
            catalog
                .columns_for_constraint(constraint_id)
                .map(ColumnDef::id)
                .collect::<Vec<_>>(),
            [column_id]
        );
        assert!(catalog.object_id_exists(ObjectKind::Constraint, constraint_id.into()));
        assert!(
            catalog.object_name_exists(ObjectKind::Constraint, &CatalogName::new("PK_T").unwrap())
        );
        assert!(!catalog.object_name_exists(ObjectKind::Index, &CatalogName::new("PK_T").unwrap()));
        drop(read);

        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        batch.track_tree(TreeKind::Index, index_root).unwrap();
        writer
            .update_index_root(&mut batch, index_id, index_root)
            .unwrap();
        writer
            .remove_constraint(&mut batch, &definition, &[column_id])
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        let mut read = pager.begin_read().unwrap();
        let catalog = read.catalog_reader().unwrap();
        assert!(catalog.find_constraint_by_id(constraint_id).is_none());
        assert!(catalog.find_index_by_id(index_id).is_none());
        assert_eq!(catalog.image.meta.next_object_id().get(), 5);
    }

    fn local_constraint_records(table_root: PageRef, index_root: PageRef) -> Records {
        let mut rows = records(table_root);
        let constraint_id = ConstraintId::new(50).unwrap();
        let index = IndexDef::new(
            IndexId::new(30).unwrap(),
            TableId::new(10).unwrap(),
            index_root,
            2,
            true,
            IndexOwnership::Constraint(constraint_id),
            CatalogName::new("PK_T").unwrap(),
        )
        .unwrap();
        let constraint = ConstraintDef::new(
            constraint_id,
            TableId::new(10).unwrap(),
            ConstraintKind::PrimaryKey,
            2,
            Some(index.id()),
            None,
            Box::new([]),
            CatalogName::new("PK_T").unwrap(),
        )
        .unwrap();
        rows.extend([
            (
                index_name_key(index.name()).unwrap(),
                encode_index_name_mapping(index.id()),
            ),
            (
                index_definition_key(index.id()),
                encode_index_definition(&index),
            ),
            (
                table_index_key(index.table_id(), index.id()),
                encode_table_index_mapping(index.id()),
            ),
            (
                index_column_key(index.id(), 0),
                encode_index_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                index_column_key(index.id(), 1),
                encode_index_column_mapping(ColumnId::new(21).unwrap()),
            ),
            (
                constraint_name_key(constraint.name()).unwrap(),
                encode_constraint_name_mapping(constraint.id()),
            ),
            (
                constraint_definition_key(constraint.id()),
                encode_constraint_definition(&constraint),
            ),
            (
                table_constraint_key(constraint.table_id(), constraint.id()),
                encode_table_constraint_mapping(constraint.id()),
            ),
            (
                constraint_column_key(constraint.id(), 0),
                encode_constraint_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                constraint_column_key(constraint.id(), 1),
                encode_constraint_column_mapping(ColumnId::new(21).unwrap()),
            ),
        ]);
        rows
    }

    fn parent_constraint_id() -> ConstraintId {
        ConstraintId::new(50).unwrap()
    }

    fn child_constraint_id(child: u64) -> ConstraintId {
        match child {
            1 => ConstraintId::new(60).unwrap(),
            2 => ConstraintId::new(80).unwrap(),
            _ => panic!("foreign-key fixture has only two children"),
        }
    }

    fn candidate_without_referenced_by() -> ForeignKeyCandidate {
        ForeignKeyCandidate::MissingReferencedBy
    }

    fn candidate_with_orphan_referenced_by() -> ForeignKeyCandidate {
        ForeignKeyCandidate::OrphanReferencedBy
    }

    fn candidate_with_wrong_parent_key() -> ForeignKeyCandidate {
        ForeignKeyCandidate::WrongParentKey
    }

    fn candidate_with_wrong_child_value() -> ForeignKeyCandidate {
        ForeignKeyCandidate::WrongChildValue
    }

    fn candidate_with_explicit_unique_parent_index() -> ForeignKeyCandidate {
        ForeignKeyCandidate::ExplicitUniqueParentIndex
    }

    fn candidate_with_parent_column_order_mismatch() -> ForeignKeyCandidate {
        ForeignKeyCandidate::ParentColumnOrderMismatch
    }

    fn graph_column(
        id: u64,
        table_id: TableId,
        ordinal: u16,
        column_type: ColumnType,
        nullable: bool,
        name: &str,
    ) -> ColumnDef {
        ColumnDef::new(
            ColumnId::new(id).unwrap(),
            table_id,
            ordinal,
            column_type,
            nullable,
            CatalogName::new(name).unwrap(),
        )
        .unwrap()
    }

    fn append_graph_table(rows: &mut Records, table: &TableDef, columns: &[ColumnDef]) {
        rows.extend([
            (
                table_name_key(table.name()).unwrap(),
                encode_table_name_mapping(table.id()),
            ),
            (
                table_definition_key(table.id()),
                encode_table_definition(table),
            ),
        ]);
        for column in columns {
            rows.extend([
                (
                    column_name_key(column.table_id(), column.name()).unwrap(),
                    encode_column_name_mapping(column.id()),
                ),
                (
                    column_ordinal_key(column.table_id(), column.ordinal()),
                    encode_column_ordinal_mapping(column.id()),
                ),
                (
                    column_definition_key(column.id()),
                    encode_column_definition(column),
                ),
            ]);
        }
    }

    fn append_graph_constraint(
        rows: &mut Records,
        constraint: &ConstraintDef,
        columns: &[ColumnId],
    ) {
        rows.extend([
            (
                constraint_name_key(constraint.name()).unwrap(),
                encode_constraint_name_mapping(constraint.id()),
            ),
            (
                constraint_definition_key(constraint.id()),
                encode_constraint_definition(constraint),
            ),
            (
                table_constraint_key(constraint.table_id(), constraint.id()),
                encode_table_constraint_mapping(constraint.id()),
            ),
        ]);
        for (ordinal, column) in columns.iter().copied().enumerate() {
            rows.push((
                constraint_column_key(constraint.id(), u16::try_from(ordinal).unwrap()),
                encode_constraint_column_mapping(column),
            ));
        }
    }

    fn foreign_key_graph_records(
        parent_root: PageRef,
        parent_index_root: PageRef,
        first_child_root: PageRef,
        second_child_root: PageRef,
    ) -> Records {
        let parent_table = new_table(10, parent_root, 2, "PARENT");
        let parent_columns = [
            graph_column(20, parent_table.id(), 0, ColumnType::Int64, false, "A"),
            graph_column(21, parent_table.id(), 1, ColumnType::Text, false, "B"),
        ];
        let parent_constraint = ConstraintDef::new(
            parent_constraint_id(),
            parent_table.id(),
            ConstraintKind::PrimaryKey,
            2,
            Some(IndexId::new(30).unwrap()),
            None,
            Box::new([]),
            CatalogName::new("PK_PARENT").unwrap(),
        )
        .unwrap();
        let parent_index = IndexDef::new(
            IndexId::new(30).unwrap(),
            parent_table.id(),
            parent_index_root,
            2,
            true,
            IndexOwnership::Constraint(parent_constraint.id()),
            CatalogName::new("SYS_PK_PARENT").unwrap(),
        )
        .unwrap();

        let mut rows = vec![(
            catalog_meta_key(),
            encode_catalog_meta(&CatalogMeta::new(ObjectId::new(100).unwrap())),
        )];
        append_graph_table(&mut rows, &parent_table, &parent_columns);
        rows.extend([
            (
                index_name_key(parent_index.name()).unwrap(),
                encode_index_name_mapping(parent_index.id()),
            ),
            (
                index_definition_key(parent_index.id()),
                encode_index_definition(&parent_index),
            ),
            (
                table_index_key(parent_index.table_id(), parent_index.id()),
                encode_table_index_mapping(parent_index.id()),
            ),
            (
                index_column_key(parent_index.id(), 0),
                encode_index_column_mapping(parent_columns[0].id()),
            ),
            (
                index_column_key(parent_index.id(), 1),
                encode_index_column_mapping(parent_columns[1].id()),
            ),
        ]);
        append_graph_constraint(
            &mut rows,
            &parent_constraint,
            &[parent_columns[0].id(), parent_columns[1].id()],
        );

        for (table, columns, constraint) in [
            (
                new_table(40, first_child_root, 2, "CHILD_1"),
                [
                    graph_column(
                        41,
                        TableId::new(40).unwrap(),
                        0,
                        ColumnType::Int64,
                        true,
                        "A",
                    ),
                    graph_column(
                        42,
                        TableId::new(40).unwrap(),
                        1,
                        ColumnType::Text,
                        true,
                        "B",
                    ),
                ],
                ConstraintDef::new(
                    child_constraint_id(1),
                    TableId::new(40).unwrap(),
                    ConstraintKind::ForeignKey,
                    2,
                    None,
                    Some(ForeignKeyTarget::new(
                        parent_table.id(),
                        parent_constraint.id(),
                    )),
                    Box::new([]),
                    CatalogName::new("FK_CHILD_1_PARENT").unwrap(),
                )
                .unwrap(),
            ),
            (
                new_table(70, second_child_root, 2, "CHILD_2"),
                [
                    graph_column(
                        71,
                        TableId::new(70).unwrap(),
                        0,
                        ColumnType::Int64,
                        true,
                        "A",
                    ),
                    graph_column(
                        72,
                        TableId::new(70).unwrap(),
                        1,
                        ColumnType::Text,
                        true,
                        "B",
                    ),
                ],
                ConstraintDef::new(
                    child_constraint_id(2),
                    TableId::new(70).unwrap(),
                    ConstraintKind::ForeignKey,
                    2,
                    None,
                    Some(ForeignKeyTarget::new(
                        parent_table.id(),
                        parent_constraint.id(),
                    )),
                    Box::new([]),
                    CatalogName::new("FK_CHILD_2_PARENT").unwrap(),
                )
                .unwrap(),
            ),
        ] {
            append_graph_table(&mut rows, &table, &columns);
            append_graph_constraint(&mut rows, &constraint, &[columns[0].id(), columns[1].id()]);
            rows.push((
                referenced_by_key(parent_constraint.id(), constraint.id()),
                encode_referenced_by_mapping(constraint.id()),
            ));
        }
        rows
    }

    fn open_candidate(candidate: ForeignKeyCandidate) -> Result<CatalogReader> {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let parent_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let parent_index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let first_child_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let second_child_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut rows = foreign_key_graph_records(
            parent_root,
            parent_index_root,
            first_child_root,
            second_child_root,
        );
        match candidate {
            ForeignKeyCandidate::MissingReferencedBy => {
                remove_record(
                    &mut rows,
                    &referenced_by_key(parent_constraint_id(), child_constraint_id(1)),
                );
            }
            ForeignKeyCandidate::OrphanReferencedBy => {
                let orphan = ConstraintId::new(90).unwrap();
                rows.push((
                    referenced_by_key(parent_constraint_id(), orphan),
                    encode_referenced_by_mapping(orphan),
                ));
            }
            ForeignKeyCandidate::WrongParentKey => {
                let child = ConstraintDef::new(
                    child_constraint_id(1),
                    TableId::new(40).unwrap(),
                    ConstraintKind::ForeignKey,
                    2,
                    None,
                    Some(ForeignKeyTarget::new(
                        TableId::new(70).unwrap(),
                        child_constraint_id(2),
                    )),
                    Box::new([]),
                    CatalogName::new("FK_CHILD_1_PARENT").unwrap(),
                )
                .unwrap();
                *record_value_mut(
                    &mut rows,
                    &constraint_definition_key(child_constraint_id(1)),
                ) = encode_constraint_definition(&child);
                remove_record(
                    &mut rows,
                    &referenced_by_key(parent_constraint_id(), child_constraint_id(1)),
                );
                rows.push((
                    referenced_by_key(child_constraint_id(2), child_constraint_id(1)),
                    encode_referenced_by_mapping(child_constraint_id(1)),
                ));
            }
            ForeignKeyCandidate::WrongChildValue => {
                *record_value_mut(
                    &mut rows,
                    &referenced_by_key(parent_constraint_id(), child_constraint_id(1)),
                ) = encode_referenced_by_mapping(child_constraint_id(2));
            }
            ForeignKeyCandidate::ExplicitUniqueParentIndex => {
                remove_record(
                    &mut rows,
                    &constraint_name_key(&CatalogName::new("PK_PARENT").unwrap()).unwrap(),
                );
                remove_record(
                    &mut rows,
                    &constraint_definition_key(parent_constraint_id()),
                );
                remove_record(
                    &mut rows,
                    &table_constraint_key(TableId::new(10).unwrap(), parent_constraint_id()),
                );
                remove_record(&mut rows, &constraint_column_key(parent_constraint_id(), 0));
                remove_record(&mut rows, &constraint_column_key(parent_constraint_id(), 1));
                let explicit = IndexDef::new(
                    IndexId::new(30).unwrap(),
                    TableId::new(10).unwrap(),
                    parent_index_root,
                    2,
                    true,
                    IndexOwnership::Explicit,
                    CatalogName::new("SYS_PK_PARENT").unwrap(),
                )
                .unwrap();
                *record_value_mut(&mut rows, &index_definition_key(explicit.id())) =
                    encode_index_definition(&explicit);
            }
            ForeignKeyCandidate::ParentColumnOrderMismatch => {
                *record_value_mut(&mut rows, &constraint_column_key(parent_constraint_id(), 0)) =
                    encode_constraint_column_mapping(ColumnId::new(21).unwrap());
                *record_value_mut(&mut rows, &constraint_column_key(parent_constraint_id(), 1)) =
                    encode_constraint_column_mapping(ColumnId::new(20).unwrap());
                *record_value_mut(&mut rows, &index_column_key(IndexId::new(30).unwrap(), 0)) =
                    encode_index_column_mapping(ColumnId::new(21).unwrap());
                *record_value_mut(&mut rows, &index_column_key(IndexId::new(30).unwrap(), 1)) =
                    encode_index_column_mapping(ColumnId::new(20).unwrap());
            }
            ForeignKeyCandidate::ChildInt64ParentDecimal => {
                let parent_column = graph_column(
                    20,
                    TableId::new(10).unwrap(),
                    0,
                    ColumnType::Decimal,
                    false,
                    "A",
                );
                *record_value_mut(&mut rows, &column_definition_key(parent_column.id())) =
                    encode_column_definition(&parent_column);
            }
            ForeignKeyCandidate::ChildDecimalParentInt64 => {
                let child_column = graph_column(
                    41,
                    TableId::new(40).unwrap(),
                    0,
                    ColumnType::Decimal,
                    true,
                    "A",
                );
                *record_value_mut(&mut rows, &column_definition_key(child_column.id())) =
                    encode_column_definition(&child_column);
            }
            ForeignKeyCandidate::SelfReference => {
                let child = ConstraintDef::new(
                    child_constraint_id(1),
                    TableId::new(40).unwrap(),
                    ConstraintKind::ForeignKey,
                    2,
                    None,
                    Some(ForeignKeyTarget::new(
                        TableId::new(40).unwrap(),
                        child_constraint_id(1),
                    )),
                    Box::new([]),
                    CatalogName::new("FK_CHILD_1_PARENT").unwrap(),
                )
                .unwrap();
                *record_value_mut(
                    &mut rows,
                    &constraint_definition_key(child_constraint_id(1)),
                ) = encode_constraint_definition(&child);
                remove_record(
                    &mut rows,
                    &referenced_by_key(parent_constraint_id(), child_constraint_id(1)),
                );
                rows.push((
                    referenced_by_key(child.id(), child.id()),
                    encode_referenced_by_mapping(child.id()),
                ));
            }
            ForeignKeyCandidate::Valid => {}
        }
        let tree = raw_tree(&mut batch, rows);
        CatalogReader::open_candidate(
            &mut batch,
            &tree,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        )
    }

    fn open_valid_parent_and_two_children() -> CatalogReader {
        open_candidate(ForeignKeyCandidate::Valid).unwrap()
    }

    fn remove_graph_constraint(rows: &mut Records, constraint: &ConstraintDef) {
        remove_record(rows, &constraint_name_key(constraint.name()).unwrap());
        remove_record(rows, &constraint_definition_key(constraint.id()));
        remove_record(
            rows,
            &table_constraint_key(constraint.table_id(), constraint.id()),
        );
        for ordinal in 0..constraint.column_count() {
            remove_record(rows, &constraint_column_key(constraint.id(), ordinal));
        }
        if let Some(parent) = constraint.parent_constraint_id() {
            remove_record(rows, &referenced_by_key(parent, constraint.id()));
        }
    }

    #[test]
    fn foreign_key_graph_rejects_every_one_sided_or_misdirected_reference() {
        for candidate in [
            candidate_without_referenced_by(),
            candidate_with_orphan_referenced_by(),
            candidate_with_wrong_parent_key(),
            candidate_with_wrong_child_value(),
            candidate_with_explicit_unique_parent_index(),
            candidate_with_parent_column_order_mismatch(),
        ] {
            assert_eq!(
                open_candidate(candidate).unwrap_err().code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn valid_foreign_key_graph_exposes_both_directions_in_constraint_id_order() {
        let catalog = open_valid_parent_and_two_children();
        assert_eq!(
            catalog
                .foreign_keys_referencing(parent_constraint_id())
                .map(ConstraintDef::id)
                .collect::<Vec<_>>(),
            vec![child_constraint_id(1), child_constraint_id(2)]
        );
    }

    #[test]
    fn parent_key_lookup_requires_an_exact_declared_key_and_reports_reverse_edges() {
        let catalog = open_valid_parent_and_two_children();
        let parent_columns = [ColumnId::new(20).unwrap(), ColumnId::new(21).unwrap()];
        assert_eq!(
            catalog
                .find_parent_key_constraint(TableId::new(10).unwrap(), &parent_columns)
                .map(ConstraintDef::id),
            Some(parent_constraint_id())
        );
        assert!(
            catalog
                .find_parent_key_constraint(
                    TableId::new(10).unwrap(),
                    &[ColumnId::new(20).unwrap()],
                )
                .is_none()
        );
        assert!(
            catalog
                .find_parent_key_constraint(
                    TableId::new(10).unwrap(),
                    &[parent_columns[1], parent_columns[0]],
                )
                .is_none()
        );
        assert!(catalog.referenced_by_exists(parent_constraint_id()));
        assert!(!catalog.referenced_by_exists(child_constraint_id(1)));
    }

    #[test]
    fn foreign_key_types_are_directional_and_self_references_are_corruption() {
        assert!(open_candidate(ForeignKeyCandidate::ChildInt64ParentDecimal).is_ok());
        for candidate in [
            ForeignKeyCandidate::ChildDecimalParentInt64,
            ForeignKeyCandidate::SelfReference,
        ] {
            assert_eq!(
                open_candidate(candidate).unwrap_err().code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn foreign_key_writer_inserts_and_removes_the_exact_reverse_record() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let parent_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let parent_index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let first_child_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let second_child_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut rows = foreign_key_graph_records(
            parent_root,
            parent_index_root,
            first_child_root,
            second_child_root,
        );
        let child = ConstraintDef::new(
            child_constraint_id(1),
            TableId::new(40).unwrap(),
            ConstraintKind::ForeignKey,
            2,
            None,
            Some(ForeignKeyTarget::new(
                TableId::new(10).unwrap(),
                parent_constraint_id(),
            )),
            Box::new([]),
            CatalogName::new("FK_CHILD_1_PARENT").unwrap(),
        )
        .unwrap();
        let second_child = ConstraintDef::new(
            child_constraint_id(2),
            TableId::new(70).unwrap(),
            ConstraintKind::ForeignKey,
            2,
            None,
            Some(ForeignKeyTarget::new(
                TableId::new(10).unwrap(),
                parent_constraint_id(),
            )),
            Box::new([]),
            CatalogName::new("FK_CHILD_2_PARENT").unwrap(),
        )
        .unwrap();
        remove_graph_constraint(&mut rows, &child);
        remove_graph_constraint(&mut rows, &second_child);
        let tree = raw_tree(&mut batch, rows);
        publish_with_features(
            &mut batch,
            &tree,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        );
        batch.commit().unwrap();

        let child_columns = [ColumnId::new(41).unwrap(), ColumnId::new(42).unwrap()];
        let bundle = ConstraintBundle {
            definition: child.clone(),
            columns: child_columns.into(),
            system_index: None,
        };
        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let before_insert = current_records(&writer, &mut batch);
        for fail_after in 0..6 {
            let checkpoint = batch.checkpoint_mutation().unwrap();
            let writer_root = writer.root();
            let error = writer
                .insert_constraint_bundle_with_record_hook(&mut batch, &bundle, |record_number| {
                    if record_number == fail_after {
                        Err(DbError::new(
                            ErrorCode::Io,
                            "injected after foreign-key bundle record",
                        ))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Io, "record {fail_after}");
            checkpoint.assert_matches_batch_for_test(&batch);
            assert_eq!(writer.root(), writer_root);
            assert_eq!(current_records(&writer, &mut batch), before_insert);
        }
        writer
            .insert_constraint_bundle(&mut batch, &bundle)
            .unwrap();
        let reverse_key = referenced_by_key(parent_constraint_id(), child.id());
        assert_eq!(
            current_records(&writer, &mut batch)
                .into_iter()
                .find(|(key, _)| key == &reverse_key)
                .map(|(_, value)| decode_referenced_by_mapping(&value).unwrap()),
            Some(child.id())
        );
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let catalog = read.catalog_reader().unwrap();
        assert_eq!(
            catalog
                .foreign_keys_referencing(parent_constraint_id())
                .map(ConstraintDef::id)
                .collect::<Vec<_>>(),
            vec![child.id()]
        );
        drop(read);

        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let before_remove = current_records(&writer, &mut batch);
        let checkpoint = batch.checkpoint_mutation().unwrap();
        let writer_root = writer.root();
        {
            let _fault =
                fail_constraint_removal_at_for_test(ConstraintRemovalFaultPoint::ReferencedBy);
            assert_eq!(
                writer
                    .remove_constraint(&mut batch, &child, &child_columns)
                    .unwrap_err()
                    .code(),
                ErrorCode::Io
            );
        }
        checkpoint.assert_matches_batch_for_test(&batch);
        assert_eq!(writer.root(), writer_root);
        assert_eq!(current_records(&writer, &mut batch), before_remove);
        writer
            .remove_constraint(&mut batch, &child, &child_columns)
            .unwrap();
        assert!(
            current_records(&writer, &mut batch)
                .into_iter()
                .all(|(key, _)| key != reverse_key)
        );
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let catalog = read.catalog_reader().unwrap();
        assert!(!catalog.referenced_by_exists(parent_constraint_id()));
        assert!(catalog.find_constraint_by_id(child.id()).is_none());
    }

    fn local_check_constraint_records(table_root: PageRef) -> Records {
        let program = CheckProgramTemplate::new(vec![CheckTemplateInstruction::Literal {
            value: crate::Value::Bool(true),
            result: CheckType::Bool,
        }])
        .unwrap()
        .resolve(&[])
        .unwrap();
        let constraint = ConstraintDef::new(
            ConstraintId::new(50).unwrap(),
            TableId::new(10).unwrap(),
            ConstraintKind::Check,
            0,
            None,
            None,
            program.as_bytes().to_vec().into_boxed_slice(),
            CatalogName::new("CK_T").unwrap(),
        )
        .unwrap();
        let mut rows = records(table_root);
        rows.extend([
            (
                constraint_name_key(constraint.name()).unwrap(),
                encode_constraint_name_mapping(constraint.id()),
            ),
            (
                constraint_definition_key(constraint.id()),
                encode_constraint_definition(&constraint),
            ),
            (
                table_constraint_key(constraint.table_id(), constraint.id()),
                encode_table_constraint_mapping(constraint.id()),
            ),
        ]);
        rows
    }

    fn local_not_null_constraint_records(table_root: PageRef) -> Records {
        let constraint = ConstraintDef::new(
            ConstraintId::new(50).unwrap(),
            TableId::new(10).unwrap(),
            ConstraintKind::NotNull,
            1,
            None,
            None,
            Box::new([]),
            CatalogName::new("NN_T").unwrap(),
        )
        .unwrap();
        let mut rows = records(table_root);
        rows.extend([
            (
                constraint_name_key(constraint.name()).unwrap(),
                encode_constraint_name_mapping(constraint.id()),
            ),
            (
                constraint_definition_key(constraint.id()),
                encode_constraint_definition(&constraint),
            ),
            (
                table_constraint_key(constraint.table_id(), constraint.id()),
                encode_table_constraint_mapping(constraint.id()),
            ),
            (
                constraint_column_key(constraint.id(), 0),
                encode_constraint_column_mapping(ColumnId::new(20).unwrap()),
            ),
        ]);
        rows
    }

    fn append_foreign_table(rows: &mut Records, root: PageRef) {
        let table = new_table(40, root, 1, "F");
        let column = new_column(41, table.id(), 0, "Z");
        rows.extend([
            (
                table_name_key(table.name()).unwrap(),
                encode_table_name_mapping(table.id()),
            ),
            (
                table_definition_key(table.id()),
                encode_table_definition(&table),
            ),
            (
                column_name_key(column.table_id(), column.name()).unwrap(),
                encode_column_name_mapping(column.id()),
            ),
            (
                column_ordinal_key(column.table_id(), column.ordinal()),
                encode_column_ordinal_mapping(column.id()),
            ),
            (
                column_definition_key(column.id()),
                encode_column_definition(&column),
            ),
        ]);
    }

    fn append_second_primary_key(rows: &mut Records, root: PageRef) {
        let constraint_id = ConstraintId::new(51).unwrap();
        let index = IndexDef::new(
            IndexId::new(31).unwrap(),
            TableId::new(10).unwrap(),
            root,
            2,
            true,
            IndexOwnership::Constraint(constraint_id),
            CatalogName::new("SYS_PK_T_2").unwrap(),
        )
        .unwrap();
        let constraint = ConstraintDef::new(
            constraint_id,
            TableId::new(10).unwrap(),
            ConstraintKind::PrimaryKey,
            2,
            Some(index.id()),
            None,
            Box::new([]),
            CatalogName::new("PK_T_2").unwrap(),
        )
        .unwrap();
        rows.extend([
            (
                index_name_key(index.name()).unwrap(),
                encode_index_name_mapping(index.id()),
            ),
            (
                index_definition_key(index.id()),
                encode_index_definition(&index),
            ),
            (
                table_index_key(index.table_id(), index.id()),
                encode_table_index_mapping(index.id()),
            ),
            (
                index_column_key(index.id(), 0),
                encode_index_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                index_column_key(index.id(), 1),
                encode_index_column_mapping(ColumnId::new(21).unwrap()),
            ),
            (
                constraint_name_key(constraint.name()).unwrap(),
                encode_constraint_name_mapping(constraint.id()),
            ),
            (
                constraint_definition_key(constraint.id()),
                encode_constraint_definition(&constraint),
            ),
            (
                table_constraint_key(constraint.table_id(), constraint.id()),
                encode_table_constraint_mapping(constraint.id()),
            ),
            (
                constraint_column_key(constraint.id(), 0),
                encode_constraint_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                constraint_column_key(constraint.id(), 1),
                encode_constraint_column_mapping(ColumnId::new(21).unwrap()),
            ),
        ]);
    }

    fn append_constraint_owned_index_without_reverse(rows: &mut Records, root: PageRef) {
        let index = IndexDef::new(
            IndexId::new(31).unwrap(),
            TableId::new(10).unwrap(),
            root,
            2,
            true,
            IndexOwnership::Constraint(ConstraintId::new(50).unwrap()),
            CatalogName::new("SYS_ORPHAN").unwrap(),
        )
        .unwrap();
        rows.extend([
            (
                index_name_key(index.name()).unwrap(),
                encode_index_name_mapping(index.id()),
            ),
            (
                index_definition_key(index.id()),
                encode_index_definition(&index),
            ),
            (
                table_index_key(index.table_id(), index.id()),
                encode_table_index_mapping(index.id()),
            ),
            (
                index_column_key(index.id(), 0),
                encode_index_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                index_column_key(index.id(), 1),
                encode_index_column_mapping(ColumnId::new(21).unwrap()),
            ),
        ]);
    }

    #[test]
    fn catalog_requires_complete_bidirectional_local_constraint_graph() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let complete = local_constraint_records(table_root, index_root);
        let tree = raw_tree(&mut batch, complete.clone());
        let features = CATALOG_V1 | INDEX_CONSTRAINT_V1;
        assert!(CatalogReader::open_candidate(&mut batch, &tree, features).is_ok());

        let required_keys = [
            constraint_name_key(&CatalogName::new("PK_T").unwrap()).unwrap(),
            constraint_definition_key(ConstraintId::new(50).unwrap()),
            table_constraint_key(TableId::new(10).unwrap(), ConstraintId::new(50).unwrap()),
            constraint_column_key(ConstraintId::new(50).unwrap(), 0),
            index_definition_key(IndexId::new(30).unwrap()),
        ];
        for missing in required_keys {
            let mut incomplete = complete.clone();
            remove_record(&mut incomplete, &missing);
            let tree = raw_tree(&mut batch, incomplete);
            assert_eq!(
                CatalogReader::open_candidate(&mut batch, &tree, features)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption,
                "missing family key {missing:?}"
            );
        }
    }

    #[test]
    fn catalog_rejects_two_primary_keys_and_foreign_keys_in_phase_4c2b() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let first_index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let second_index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let mut two_primary_keys = local_constraint_records(table_root, first_index_root);
        let second_constraint_id = ConstraintId::new(51).unwrap();
        let second_index = IndexDef::new(
            IndexId::new(31).unwrap(),
            TableId::new(10).unwrap(),
            second_index_root,
            2,
            true,
            IndexOwnership::Constraint(second_constraint_id),
            CatalogName::new("PK_T_2").unwrap(),
        )
        .unwrap();
        let second_constraint = ConstraintDef::new(
            second_constraint_id,
            TableId::new(10).unwrap(),
            ConstraintKind::PrimaryKey,
            2,
            Some(second_index.id()),
            None,
            Box::new([]),
            CatalogName::new("PK_T_2").unwrap(),
        )
        .unwrap();
        two_primary_keys.extend([
            (
                index_name_key(second_index.name()).unwrap(),
                encode_index_name_mapping(second_index.id()),
            ),
            (
                index_definition_key(second_index.id()),
                encode_index_definition(&second_index),
            ),
            (
                table_index_key(second_index.table_id(), second_index.id()),
                encode_table_index_mapping(second_index.id()),
            ),
            (
                index_column_key(second_index.id(), 0),
                encode_index_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                index_column_key(second_index.id(), 1),
                encode_index_column_mapping(ColumnId::new(21).unwrap()),
            ),
            (
                constraint_name_key(second_constraint.name()).unwrap(),
                encode_constraint_name_mapping(second_constraint.id()),
            ),
            (
                constraint_definition_key(second_constraint.id()),
                encode_constraint_definition(&second_constraint),
            ),
            (
                table_constraint_key(second_constraint.table_id(), second_constraint.id()),
                encode_table_constraint_mapping(second_constraint.id()),
            ),
            (
                constraint_column_key(second_constraint.id(), 0),
                encode_constraint_column_mapping(ColumnId::new(20).unwrap()),
            ),
            (
                constraint_column_key(second_constraint.id(), 1),
                encode_constraint_column_mapping(ColumnId::new(21).unwrap()),
            ),
        ]);
        let tree = raw_tree(&mut batch, two_primary_keys);
        let features = CATALOG_V1 | INDEX_CONSTRAINT_V1;
        assert_eq!(
            CatalogReader::open_candidate(&mut batch, &tree, features)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut foreign_key = local_constraint_records(table_root, first_index_root);
        let bytes = record_value_mut(
            &mut foreign_key,
            &constraint_definition_key(ConstraintId::new(50).unwrap()),
        );
        bytes[24] = 5;
        let tree = raw_tree(&mut batch, foreign_key);
        let error = CatalogReader::open_candidate(&mut batch, &tree, features).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Corruption);
    }

    fn remove_record(rows: &mut Records, key: &[u8]) -> Vec<u8> {
        let position = rows
            .iter()
            .position(|(candidate, _)| candidate == key)
            .unwrap_or_else(|| panic!("fixture record is missing: {key:?}"));
        rows.remove(position).1
    }

    fn record_value_mut<'a>(rows: &'a mut Records, key: &[u8]) -> &'a mut Vec<u8> {
        &mut rows
            .iter_mut()
            .find(|(candidate, _)| candidate == key)
            .unwrap_or_else(|| panic!("fixture record is missing: {key:?}"))
            .1
    }

    fn publish_with_features(batch: &mut WriteBatch, tree: &BTree, feature_bits: u64) {
        batch.enable_feature_bits(feature_bits).unwrap();
        batch.set_catalog_root(tree.root()).unwrap();
    }

    fn raw_index_catalog(
        mutate: impl FnOnce(&mut Records, PageRef, PageRef, PageRef),
    ) -> (TempDatabase, Pager) {
        raw_index_catalog_with_features(
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
            mutate,
        )
    }

    #[test]
    fn phase_4c2b_format2_committed_local_constraint_catalog_reopens_without_migration() {
        // Production break caught: a persisted 4C2-B catalog must be accepted
        // as an ordinary format-2 image; open must neither migrate it nor lose
        // the system-index/constraint graph after recovery.
        let (temp, pager) = raw_local_constraint_catalog(|_, _, _| {});
        let mut read = pager.begin_read().unwrap();
        let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        assert_eq!(
            catalog
                .constraints_for_table(TableId::new(10).unwrap())
                .count(),
            1
        );
        assert_eq!(
            catalog.indexes_for_table(TableId::new(10).unwrap()).count(),
            1
        );
        catalog.validate_deep(&mut read).unwrap();
        drop(read);
        drop(pager);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let pager = Pager::open(temp.path(), mode, PagerConfig::test_default(4096)).unwrap();
            let mut read = pager.begin_read().unwrap();
            let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(
                catalog
                    .constraints_for_table(TableId::new(10).unwrap())
                    .count(),
                1
            );
        }
    }

    #[test]
    fn phase_4c2b_constraint_corruption_rejects_committed_candidate_without_fallback() {
        // Production break caught: recovery must reject a complete, malformed
        // local-constraint commit instead of silently choosing the older base
        // catalog.  Each mutation below starts from a valid durable base image.
        let complete_features = CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1;
        let cases: Vec<ConstraintCandidateCase> = vec![
            (
                "missing constraint name",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    remove_record(
                        rows,
                        &constraint_name_key(&CatalogName::new("PK_T").unwrap()).unwrap(),
                    );
                }),
            ),
            (
                "aliased constraint name",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    *record_value_mut(
                        rows,
                        &constraint_name_key(&CatalogName::new("PK_T").unwrap()).unwrap(),
                    ) = encode_constraint_name_mapping(ConstraintId::new(51).unwrap());
                }),
            ),
            (
                "missing table constraint mapping",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    remove_record(
                        rows,
                        &table_constraint_key(
                            TableId::new(10).unwrap(),
                            ConstraintId::new(50).unwrap(),
                        ),
                    );
                }),
            ),
            (
                "aliased table constraint mapping",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    *record_value_mut(
                        rows,
                        &table_constraint_key(
                            TableId::new(10).unwrap(),
                            ConstraintId::new(50).unwrap(),
                        ),
                    ) = encode_table_constraint_mapping(ConstraintId::new(51).unwrap());
                }),
            ),
            (
                "missing constraint column mapping",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    remove_record(
                        rows,
                        &constraint_column_key(ConstraintId::new(50).unwrap(), 0),
                    );
                }),
            ),
            (
                "duplicate constraint columns",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    *record_value_mut(
                        rows,
                        &constraint_column_key(ConstraintId::new(50).unwrap(), 1),
                    ) = encode_constraint_column_mapping(ColumnId::new(20).unwrap());
                }),
            ),
            (
                "noncontiguous constraint columns",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    let key = constraint_column_key(ConstraintId::new(50).unwrap(), 1);
                    let value = remove_record(rows, &key);
                    rows.push((
                        constraint_column_key(ConstraintId::new(50).unwrap(), 2),
                        value,
                    ));
                }),
            ),
            (
                "wrong constraint column owner",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|batch, rows, _, _| {
                    let foreign_root = BTree::create(batch, TreeKind::Table).unwrap().root();
                    append_foreign_table(rows, foreign_root);
                    *record_value_mut(
                        rows,
                        &constraint_column_key(ConstraintId::new(50).unwrap(), 0),
                    ) = encode_constraint_column_mapping(ColumnId::new(41).unwrap());
                }),
            ),
            (
                "wrong constraint table owner",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|batch, rows, _, _| {
                    let foreign_root = BTree::create(batch, TreeKind::Table).unwrap().root();
                    append_foreign_table(rows, foreign_root);
                    let mapping = remove_record(
                        rows,
                        &table_constraint_key(
                            TableId::new(10).unwrap(),
                            ConstraintId::new(50).unwrap(),
                        ),
                    );
                    rows.push((
                        table_constraint_key(
                            TableId::new(40).unwrap(),
                            ConstraintId::new(50).unwrap(),
                        ),
                        mapping,
                    ));
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[16..24]
                        .copy_from_slice(&40_u64.to_le_bytes());
                }),
            ),
            (
                "two primary keys",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|batch, rows, _, _| {
                    let second_root = BTree::create(batch, TreeKind::Index).unwrap().root();
                    append_second_primary_key(rows, second_root);
                }),
            ),
            (
                "primary key without system index",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[28..36]
                        .fill(0);
                }),
            ),
            (
                "unique constraint without system index",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    let bytes = record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    );
                    bytes[24] = 2;
                    bytes[28..36].fill(0);
                }),
            ),
            (
                "nonunique primary key index",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    record_value_mut(rows, &index_definition_key(IndexId::new(30).unwrap()))[42] =
                        0;
                }),
            ),
            (
                "key constraint index belongs to another table",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|batch, rows, _, _| {
                    let foreign_root = BTree::create(batch, TreeKind::Table).unwrap().root();
                    append_foreign_table(rows, foreign_root);
                    record_value_mut(rows, &index_definition_key(IndexId::new(30).unwrap()))
                        [16..24]
                        .copy_from_slice(&40_u64.to_le_bytes());
                }),
            ),
            (
                "key constraint index columns differ",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    *record_value_mut(rows, &index_column_key(IndexId::new(30).unwrap(), 1)) =
                        encode_index_column_mapping(ColumnId::new(20).unwrap());
                }),
            ),
            (
                "constraint-owned index has mismatched owner ID",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    record_value_mut(rows, &index_definition_key(IndexId::new(30).unwrap()))
                        [46..54]
                        .copy_from_slice(&51_u64.to_le_bytes());
                }),
            ),
            (
                "constraint-owned index has no reverse constraint link",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|batch, rows, _, _| {
                    let extra_root = BTree::create(batch, TreeKind::Index).unwrap().root();
                    append_constraint_owned_index_without_reverse(rows, extra_root);
                }),
            ),
            (
                "nullable primary-key column",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    record_value_mut(rows, &column_definition_key(ColumnId::new(20).unwrap()))
                        [27] = 1;
                }),
            ),
            (
                "constraint records without the index feature",
                ErrorCode::Corruption,
                CATALOG_V1,
                Box::new(|_, _, _, _| {}),
            ),
            (
                "future constraint record version",
                ErrorCode::Unsupported,
                complete_features,
                Box::new(|_, rows, _, _| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[1] = 2;
                }),
            ),
            (
                "foreign-key constraint definition",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[24] = 5;
                }),
            ),
            (
                "foreign-key reverse edge",
                ErrorCode::Corruption,
                complete_features,
                Box::new(|_, rows, _, _| {
                    rows.push((
                        referenced_by_key(
                            ConstraintId::new(50).unwrap(),
                            ConstraintId::new(51).unwrap(),
                        ),
                        vec![],
                    ));
                }),
            ),
        ];
        for (label, expected, feature_bits, mutate) in cases {
            let (temp, pager) = raw_local_constraint_candidate_after_base(
                feature_bits,
                |batch, rows, table_root, index_root| mutate(batch, rows, table_root, index_root),
            );
            let mut read = pager.begin_read().unwrap();
            assert_eq!(
                match CatalogReader::open_empty_or_v1(&mut read) {
                    Ok(_) => panic!("current snapshot accepted {label}"),
                    Err(error) => error.code(),
                },
                expected,
                "current snapshot: {label}"
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
    }

    #[test]
    fn phase_4c2b_constraint_corruption_classifies_committed_check_program_bytes() {
        // Production break caught: a catalog reader that validates only the
        // outer ConstraintDef can accept a corrupt persisted program until a
        // DML statement happens to execute it.
        let cases: Vec<CheckProgramCase> = vec![
            (
                "empty program",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[56..60]
                        .fill(0);
                }),
            ),
            (
                "oversized program",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[56..60]
                        .copy_from_slice(&1_048_577_u32.to_le_bytes());
                }),
            ),
            (
                "future program version",
                ErrorCode::Unsupported,
                Box::new(|rows| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[64] = 2;
                }),
            ),
            (
                "unknown program opcode",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[72] = 0xff;
                }),
            ),
            (
                "unknown program result type",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[73] = 0xff;
                }),
            ),
            (
                "stack underflow",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    let bytes = record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    );
                    bytes[72] = 0x12;
                    bytes[73] = 9;
                    bytes[74..76].copy_from_slice(&1_u16.to_le_bytes());
                    bytes[76..80].fill(0);
                }),
            ),
            (
                "non-Bool final value",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    rewrite_check_program_as_column(
                        record_value_mut(
                            rows,
                            &constraint_definition_key(ConstraintId::new(50).unwrap()),
                        ),
                        20,
                        1,
                    );
                }),
            ),
            (
                "invalid column ID",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    rewrite_check_program_as_column(
                        record_value_mut(
                            rows,
                            &constraint_definition_key(ConstraintId::new(50).unwrap()),
                        ),
                        99,
                        1,
                    );
                }),
            ),
            (
                "truncated instruction stream",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[66..70]
                        .copy_from_slice(&2_u32.to_le_bytes());
                }),
            ),
            (
                "trailing program bytes",
                ErrorCode::Corruption,
                Box::new(|rows| {
                    let bytes = record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    );
                    let program_len = u32::from_le_bytes(bytes[56..60].try_into().unwrap());
                    let program_end = 64 + usize::try_from(program_len).unwrap();
                    let old_payload_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                    bytes.insert(program_end, 0);
                    bytes[56..60].copy_from_slice(&(program_len + 1).to_le_bytes());
                    bytes[4..8].copy_from_slice(&(old_payload_len + 1).to_le_bytes());
                }),
            ),
        ];
        for (label, expected, mutate) in cases {
            let (temp, pager) = raw_check_constraint_candidate_after_base(|rows, _| mutate(rows));
            let mut read = pager.begin_read().unwrap();
            assert_eq!(
                match CatalogReader::open_empty_or_v1(&mut read) {
                    Ok(_) => panic!("current snapshot accepted {label}"),
                    Err(error) => error.code(),
                },
                expected,
                "current snapshot: {label}"
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
    }

    #[test]
    fn phase_4c2b_constraint_corruption_rejects_invalid_not_null_and_check_shapes() {
        // A local constraint's type-specific shape is a catalog-open contract,
        // not a deferred DML error.  Every case below commits after a valid base
        // image so recovery cannot select that base as a fallback.
        let not_null = raw_not_null_constraint_candidate_after_base(|rows, _| {
            record_value_mut(rows, &column_definition_key(ColumnId::new(20).unwrap()))[27] = 1;
        });
        assert_committed_constraint_candidate_rejected(
            "nullable not-null column",
            ErrorCode::Corruption,
            not_null,
        );

        for (label, mutate) in [
            (
                "check with constrained columns",
                Box::new(|rows: &mut Records| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[26..28]
                        .copy_from_slice(&1_u16.to_le_bytes());
                }) as Box<dyn Fn(&mut Records)>,
            ),
            (
                "check with a system index",
                Box::new(|rows: &mut Records| {
                    record_value_mut(
                        rows,
                        &constraint_definition_key(ConstraintId::new(50).unwrap()),
                    )[28..36]
                        .copy_from_slice(&30_u64.to_le_bytes());
                }),
            ),
        ] {
            let candidate = raw_check_constraint_candidate_after_base(|rows, _| mutate(rows));
            assert_committed_constraint_candidate_rejected(label, ErrorCode::Corruption, candidate);
        }
    }

    #[test]
    fn phase_4c2b_index_feature_without_catalog_rejects_committed_candidate_without_fallback() {
        // `enable_feature_bits` correctly refuses this combination before it
        // can commit. A checksummed, complete WAL candidate can nevertheless
        // be durably corrupted. Recovery must reject that candidate instead
        // of selecting the earlier complete checkpointed catalog.
        let (temp, pager, _) = committed_single_column_catalog();
        let prior_lsn = pager.latest_lsn().unwrap();
        let mut prior = pager.begin_read().unwrap();
        assert!(
            CatalogReader::open_empty_or_v1(&mut prior)
                .unwrap()
                .find_table_by_name(&CatalogName::new("T").unwrap())
                .is_some(),
            "the prior complete catalog must be valid before the candidate"
        );
        drop(prior);
        assert_eq!(pager.checkpoint().unwrap().to_lsn, prior_lsn);

        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let table_id = writer.allocate_object_id(&mut batch).unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let candidate_table = new_table(table_id.get(), table_root, 0, "LATER");
        writer
            .insert_table_definition(&mut batch, &candidate_table)
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
        assert_ne!(pager.latest_lsn().unwrap(), prior_lsn);
        let mut latest = pager.begin_read().unwrap();
        assert!(
            CatalogReader::open_empty_or_v1(&mut latest)
                .unwrap()
                .find_table_by_name(&CatalogName::new("LATER").unwrap())
                .is_some(),
            "the later catalog candidate must be complete before patching"
        );
        drop(latest);

        // This is the immediate catalog validation of the later committed
        // candidate with precisely the feature declaration that is patched
        // into its durable WAL commit below.
        let mut current = pager.begin_write().unwrap();
        let (candidate_tree, _) = current
            .catalog_tree_with_report(TreeKind::Table)
            .unwrap()
            .unwrap();
        assert_eq!(
            CatalogReader::open_candidate(
                &mut current,
                &candidate_tree,
                super::super::features::INDEX_CONSTRAINT_V1,
            )
            .unwrap_err()
            .code(),
            ErrorCode::Corruption,
            "current committed candidate"
        );
        current.rollback().unwrap();
        drop(current);
        drop(pager);

        set_latest_wal_commit_feature_bits(&temp, super::super::features::INDEX_CONSTRAINT_V1);
        let main = std::fs::read(temp.path()).unwrap();
        let wal_path = crate::storage::wal::wal_path(temp.path());
        let wal = std::fs::read(&wal_path).unwrap();
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let error = match Pager::open(temp.path(), mode, PagerConfig::test_default(4096)) {
                Ok(_) => panic!("index feature without catalog V1 was accepted"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Corruption, "mode: {mode:?}");
            assert_eq!(std::fs::read(temp.path()).unwrap(), main, "mode: {mode:?}");
            assert_eq!(std::fs::read(&wal_path).unwrap(), wal, "mode: {mode:?}");
        }
    }

    fn rewrite_check_program_as_column(bytes: &mut Vec<u8>, column_id: u64, result: u8) {
        let current_len =
            usize::try_from(u32::from_le_bytes(bytes[56..60].try_into().unwrap())).unwrap();
        let program_start = 64;
        let program_end = program_start + current_len;
        let target_len = 24;
        assert!(
            current_len <= target_len,
            "fixture check program remains small"
        );
        for _ in current_len..target_len {
            bytes.insert(program_end, 0);
        }
        let payload_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        bytes[4..8].copy_from_slice(
            &(payload_len + u32::try_from(target_len - current_len).unwrap()).to_le_bytes(),
        );
        bytes[56..60].copy_from_slice(&u32::try_from(target_len).unwrap().to_le_bytes());
        bytes[program_start..program_start + 8].copy_from_slice(&[1, 0, 1, 0, 0, 0, 0, 0]);
        bytes[program_start + 8..program_start + 16]
            .copy_from_slice(&[1, result, 0, 0, 8, 0, 0, 0]);
        bytes[program_start + 16..program_start + 24].copy_from_slice(&column_id.to_be_bytes());
    }

    fn raw_local_constraint_catalog(
        mutate: impl FnOnce(&mut Records, PageRef, PageRef),
    ) -> (TempDatabase, Pager) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let mut rows = local_constraint_records(table_root, index_root);
        mutate(&mut rows, table_root, index_root);
        let catalog = raw_tree(&mut batch, rows);
        publish_with_features(
            &mut batch,
            &catalog,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        );
        batch.commit().unwrap();
        (temp, pager)
    }

    fn raw_local_constraint_candidate_after_base(
        feature_bits: u64,
        mutate: impl FnOnce(&mut WriteBatch, &mut Records, PageRef, PageRef),
    ) -> (TempDatabase, Pager) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let base_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let base_catalog = raw_tree(&mut batch, records(base_root));
        publish(&mut batch, &base_catalog);
        batch.commit().unwrap();

        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let mut rows = local_constraint_records(table_root, index_root);
        mutate(&mut batch, &mut rows, table_root, index_root);
        let catalog = raw_tree(&mut batch, rows);
        publish_with_features(&mut batch, &catalog, feature_bits);
        batch.commit().unwrap();
        (temp, pager)
    }

    fn raw_check_constraint_candidate_after_base(
        mutate: impl FnOnce(&mut Records, PageRef),
    ) -> (TempDatabase, Pager) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let base_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let base_catalog = raw_tree(&mut batch, records(base_root));
        publish(&mut batch, &base_catalog);
        batch.commit().unwrap();

        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut rows = local_check_constraint_records(table_root);
        mutate(&mut rows, table_root);
        let catalog = raw_tree(&mut batch, rows);
        publish_with_features(
            &mut batch,
            &catalog,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        );
        batch.commit().unwrap();
        (temp, pager)
    }

    fn raw_not_null_constraint_candidate_after_base(
        mutate: impl FnOnce(&mut Records, PageRef),
    ) -> (TempDatabase, Pager) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let base_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let base_catalog = raw_tree(&mut batch, records(base_root));
        publish(&mut batch, &base_catalog);
        batch.commit().unwrap();

        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut rows = local_not_null_constraint_records(table_root);
        mutate(&mut rows, table_root);
        let catalog = raw_tree(&mut batch, rows);
        publish_with_features(
            &mut batch,
            &catalog,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        );
        batch.commit().unwrap();
        (temp, pager)
    }

    fn assert_committed_constraint_candidate_rejected(
        label: &str,
        expected: ErrorCode,
        (temp, pager): (TempDatabase, Pager),
    ) {
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap_err()
                .code(),
            expected,
            "current snapshot: {label}"
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

    fn raw_index_catalog_with_features(
        feature_bits: u64,
        mutate: impl FnOnce(&mut Records, PageRef, PageRef, PageRef),
    ) -> (TempDatabase, Pager) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let index_root = BTree::create(&mut batch, TreeKind::Index).unwrap().root();
        let foreign_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut rows = index_records(table_root, index_root, foreign_root);
        mutate(&mut rows, table_root, index_root, foreign_root);
        let catalog = raw_tree(&mut batch, rows);
        publish_with_features(&mut batch, &catalog, feature_bits);
        batch.commit().unwrap();
        (temp, pager)
    }

    fn raw_index_catalog_with_entries(entries: &[(Vec<u8>, Vec<u8>)]) -> (TempDatabase, Pager) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut index_tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
        for (key, value) in entries {
            index_tree.insert(&mut batch, key, value).unwrap();
        }
        let foreign_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let catalog = raw_tree(
            &mut batch,
            index_records(table_root, index_tree.root(), foreign_root),
        );
        publish_with_features(
            &mut batch,
            &catalog,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        );
        batch.commit().unwrap();
        (temp, pager)
    }

    fn set_real_file_feature_bits(temp: &TempDatabase, bits: u64) {
        let mut bytes = std::fs::read(temp.path()).unwrap();
        for slot_start in [0, super::super::header::SUPERBLOCK_SLOT_LEN] {
            bytes[slot_start + 88..slot_start + 96].copy_from_slice(&bits.to_le_bytes());
            let checksum = crate::storage::checksum::crc32c(
                &bytes[slot_start..slot_start + super::super::header::SUPERBLOCK_SLOT_LEN - 4],
            )
            .to_le_bytes();
            bytes[slot_start + super::super::header::SUPERBLOCK_SLOT_LEN - 4
                ..slot_start + super::super::header::SUPERBLOCK_SLOT_LEN]
                .copy_from_slice(&checksum);
        }
        std::fs::write(temp.path(), bytes).unwrap();
    }

    fn set_latest_wal_commit_feature_bits(temp: &TempDatabase, bits: u64) {
        const WAL_HEADER_AREA_LEN: usize = 4096 * 2;
        const RECORD_HEADER_LEN: usize = 64;
        const COMMIT_PAYLOAD_LEN: usize = 64;
        const COMMIT_RECORD_TYPE: u8 = 2;
        const RECORD_HEADER_CRC_OFFSET: usize = 52;
        const RECORD_PAYLOAD_CRC_OFFSET: usize = 48;
        const COMMIT_FEATURE_BITS_OFFSET: usize = 40;

        let wal_path = crate::storage::wal::wal_path(temp.path());
        let mut bytes = std::fs::read(&wal_path).unwrap();
        let mut offset = WAL_HEADER_AREA_LEN;
        let mut latest_commit = None;
        while offset < bytes.len() {
            assert!(
                offset + RECORD_HEADER_LEN <= bytes.len(),
                "fixture WAL record has a complete header"
            );
            assert_eq!(
                &bytes[offset..offset + 8],
                b"ORAWREC\0",
                "fixture WAL record uses the expected magic"
            );
            let payload_len = usize::try_from(u32::from_le_bytes(
                bytes[offset + 12..offset + 16].try_into().unwrap(),
            ))
            .unwrap();
            let payload_end = offset + RECORD_HEADER_LEN + payload_len;
            let record_end = (payload_end + 7) & !7;
            assert!(
                record_end <= bytes.len(),
                "fixture WAL record lies within the durable WAL"
            );
            if bytes[offset + 8] == COMMIT_RECORD_TYPE {
                assert_eq!(payload_len, COMMIT_PAYLOAD_LEN);
                latest_commit = Some(offset);
            }
            offset = record_end;
        }
        assert_eq!(offset, bytes.len(), "fixture WAL has no trailing bytes");

        let commit = latest_commit.expect("fixture WAL has a committed candidate");
        let payload = commit + RECORD_HEADER_LEN;
        bytes[payload + COMMIT_FEATURE_BITS_OFFSET..payload + COMMIT_FEATURE_BITS_OFFSET + 8]
            .copy_from_slice(&bits.to_le_bytes());
        let payload_crc =
            crate::storage::checksum::crc32c(&bytes[payload..payload + COMMIT_PAYLOAD_LEN]);
        bytes[commit + RECORD_PAYLOAD_CRC_OFFSET..commit + RECORD_PAYLOAD_CRC_OFFSET + 4]
            .copy_from_slice(&payload_crc.to_le_bytes());
        let mut header = [0; RECORD_HEADER_LEN];
        header.copy_from_slice(&bytes[commit..commit + RECORD_HEADER_LEN]);
        header[RECORD_HEADER_CRC_OFFSET..RECORD_HEADER_CRC_OFFSET + 4].fill(0);
        let header_crc = crate::storage::checksum::crc32c(&header);
        bytes[commit + RECORD_HEADER_CRC_OFFSET..commit + RECORD_HEADER_CRC_OFFSET + 4]
            .copy_from_slice(&header_crc.to_le_bytes());
        std::fs::write(wal_path, bytes).unwrap();
    }

    fn assert_format2_read_only_open_preserves_bytes(
        temp: &TempDatabase,
        expected_features: u64,
        has_index: bool,
        expected_error: Option<ErrorCode>,
    ) {
        let main_before = std::fs::read(temp.path()).unwrap();
        let wal_path = crate::storage::wal::wal_path(temp.path());
        let wal_before = std::fs::read(&wal_path).unwrap();
        match (
            Pager::open(
                temp.path(),
                OpenMode::ReadOnly,
                PagerConfig::test_default(4096),
            ),
            expected_error,
        ) {
            (Ok(pager), None) => {
                let mut read = pager.begin_read().unwrap();
                assert_eq!(read.feature_bits(), expected_features);
                let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
                assert_eq!(
                    catalog
                        .find_index_by_name(&CatalogName::new("IX_T_AB").unwrap())
                        .is_some(),
                    has_index
                );
            }
            (Err(error), Some(expected)) => assert_eq!(error.code(), expected),
            (Ok(_), Some(expected)) => panic!("format-2 file was accepted, wanted {expected:?}"),
            (Err(error), None) => panic!("format-2 file was rejected: {error}"),
        }
        assert_eq!(std::fs::read(temp.path()).unwrap(), main_before);
        assert_eq!(std::fs::read(wal_path).unwrap(), wal_before);
    }

    fn assert_bad_index_records(
        label: &str,
        mutate: impl FnOnce(&mut Records, PageRef, PageRef, PageRef),
    ) {
        let (temp, pager) = raw_index_catalog(mutate);
        let mut read = pager.begin_read().unwrap();
        assert_eq!(
            CatalogReader::open_empty_or_v1(&mut read)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption,
            "snapshot: {label}"
        );
        drop(read);
        drop(pager);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let error = match Pager::open(temp.path(), mode, PagerConfig::test_default(4096)) {
                Ok(_) => panic!("recovery accepted {label}"),
                Err(error) => error,
            };
            assert_eq!(error.code(), ErrorCode::Corruption, "recovery: {label}");
        }
    }

    fn raw_tree(batch: &mut WriteBatch, records: Records) -> BTree {
        let mut tree = BTree::create(batch, TreeKind::Table).unwrap();
        for (key, value) in records {
            tree.insert(batch, &key, &value).unwrap();
        }
        batch.track_tree(TreeKind::Table, tree.root()).unwrap();
        tree
    }

    fn publish(batch: &mut WriteBatch, tree: &BTree) {
        publish_with_features(batch, tree, CATALOG_V1);
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
        let reader = CatalogReader::open_candidate(
            &mut batch,
            &tree,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        )
        .unwrap();
        assert!(reader.find_index_by_id(IndexId::new(1).unwrap()).is_none());
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
        assert_bad_records_with_features(label, expected, CATALOG_V1, mutate);
    }

    fn assert_bad_records_with_features(
        label: &str,
        expected: ErrorCode,
        feature_bits: u64,
        mutate: impl FnOnce(&mut Records, PageRef),
    ) {
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let mut rows = records(root);
        mutate(&mut rows, root);
        let tree = raw_tree(&mut batch, rows);
        assert_eq!(
            CatalogReader::open_candidate(&mut batch, &tree, feature_bits)
                .unwrap_err()
                .code(),
            expected,
            "candidate: {label}"
        );
        publish_with_features(&mut batch, &tree, feature_bits);
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
        for family in [0x15, 0xff] {
            assert_bad_records("reserved family", ErrorCode::Unsupported, |rows, _| {
                rows.push((vec![family], vec![]));
            });
        }
        for family in [0x20, 0x21, 0x30] {
            assert_bad_records(
                "malformed known family",
                ErrorCode::Corruption,
                |rows, _| {
                    rows.push((vec![family], vec![]));
                },
            );
        }
        let local_constraint_keys = [
            constraint_name_key(&CatalogName::new("C").unwrap()).unwrap(),
            constraint_definition_key(ConstraintId::new(50).unwrap()),
            table_constraint_key(TableId::new(10).unwrap(), ConstraintId::new(50).unwrap()),
            constraint_column_key(ConstraintId::new(50).unwrap(), 0),
        ];
        let referenced_by = referenced_by_key(
            ConstraintId::new(50).unwrap(),
            ConstraintId::new(51).unwrap(),
        );
        for key in local_constraint_keys
            .clone()
            .into_iter()
            .chain([referenced_by.clone()])
        {
            assert_bad_records(
                "4C2 record without feature bit",
                ErrorCode::Corruption,
                |rows, _| rows.push((key, vec![])),
            );
        }
        for key in local_constraint_keys {
            assert_bad_records_with_features(
                "malformed or incomplete local constraint family",
                ErrorCode::Corruption,
                CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
                |rows, _| rows.push((key, vec![])),
            );
        }
        assert_bad_records_with_features(
            "foreign-key reverse edge",
            ErrorCode::Corruption,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
            |rows, _| rows.push((referenced_by, vec![])),
        );
        assert_bad_records_with_features(
            "orphaned constraint-owned index",
            ErrorCode::Corruption,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
            |rows, root| {
                let index = IndexDef::new(
                    IndexId::new(30).unwrap(),
                    TableId::new(10).unwrap(),
                    root,
                    1,
                    true,
                    IndexOwnership::Constraint(ConstraintId::new(50).unwrap()),
                    CatalogName::new("IX_C").unwrap(),
                )
                .unwrap();
                rows.push((
                    index_definition_key(index.id()),
                    encode_index_definition(&index),
                ));
            },
        );
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
    fn old_snapshot_keeps_old_catalog_and_roots_across_index_ddl_and_dml() {
        // Production break caught: an old snapshot must not observe the
        // feature/catalog/table-root publication from a later indexed commit.
        let (_temp, pager, table_id) = committed_single_column_catalog();
        let (table, columns) = {
            let mut read = pager.begin_read().unwrap();
            let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            (
                catalog.find_table_by_id(table_id).unwrap().clone(),
                catalog
                    .columns_for_table(table_id)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let first_row =
            RowCodec::encode(&table, &columns, &[crate::Value::Int64(1)], 1024).unwrap();
        let first_rowid = {
            let mut batch = pager.begin_write().unwrap();
            let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
            let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
            let mut rows = BTree::from_root(TreeKind::Table, table.root());
            rows.insert(&mut batch, &encode_rowid_key(rowid), &first_row)
                .unwrap();
            writer
                .update_table_root(&mut batch, table_id, rows.root())
                .unwrap();
            writer.finish(&mut batch).unwrap();
            batch.commit().unwrap();
            rowid
        };

        let mut old_snapshot = pager.begin_read().unwrap();
        let old = CatalogReader::open_empty_or_v1(&mut old_snapshot).unwrap();
        let old_feature_bits = old_snapshot.feature_bits();
        let old_catalog_root = old_snapshot.catalog_root();
        let old_table = old.find_table_by_id(table_id).unwrap().clone();
        let index_name = CatalogName::new("IX_T_C").unwrap();
        assert!(old.find_index_by_name(&index_name).is_none());

        let mut batch = pager.begin_write().unwrap();
        let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
        let mut rows = BTree::from_root(TreeKind::Table, old_table.root());
        let mut index_tree = BTree::create(&mut batch, TreeKind::Index).unwrap();
        for value in 2_i64..=200 {
            let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
            let row = RowCodec::encode(&old_table, &columns, &[crate::Value::Int64(value)], 1024)
                .unwrap();
            rows.insert(&mut batch, &encode_rowid_key(rowid), &row)
                .unwrap();
            let key = IndexKeyCodec::encode(&columns, &[crate::Value::Int64(value)], rowid)
                .unwrap()
                .into_physical();
            index_tree.insert(&mut batch, &key, &[]).unwrap();
        }
        let original_key = IndexKeyCodec::encode(&columns, &[crate::Value::Int64(1)], first_rowid)
            .unwrap()
            .into_physical();
        index_tree.insert(&mut batch, &original_key, &[]).unwrap();
        let index_id = IndexId::new(writer.allocate_object_id(&mut batch).unwrap().get()).unwrap();
        let index = IndexDef::new(
            index_id,
            table_id,
            index_tree.root(),
            1,
            false,
            IndexOwnership::Explicit,
            index_name.clone(),
        )
        .unwrap();
        writer
            .update_table_root(&mut batch, table_id, rows.root())
            .unwrap();
        writer
            .insert_index_definition(&mut batch, &index, &[columns[0].id()])
            .unwrap();
        writer
            .bump_table_schema_version(&mut batch, table_id)
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();

        assert_eq!(old_snapshot.feature_bits(), old_feature_bits);
        assert_eq!(old_snapshot.catalog_root(), old_catalog_root);
        assert!(old.find_index_by_name(&index_name).is_none());
        assert_eq!(
            BTree::from_root(TreeKind::Table, old_table.root())
                .get(&mut old_snapshot, &encode_rowid_key(first_rowid))
                .unwrap(),
            Some(first_row)
        );
        assert!(
            BTree::from_root(TreeKind::Table, old_table.root())
                .get(&mut old_snapshot, &encode_rowid_key(RowId::new(2).unwrap()))
                .unwrap()
                .is_none()
        );

        let mut fresh_snapshot = pager.begin_read().unwrap();
        let fresh = CatalogReader::open_empty_or_v1(&mut fresh_snapshot).unwrap();
        let fresh_table = fresh.find_table_by_id(table_id).unwrap();
        let fresh_index = fresh.find_index_by_name(&index_name).unwrap();
        assert_eq!(
            fresh_snapshot.feature_bits(),
            CATALOG_V1 | INDEX_CONSTRAINT_V1
        );
        assert_ne!(fresh_table.root(), old_table.root());
        assert_eq!(fresh_index.root(), index_tree.root());
        assert_eq!(
            BTree::from_root(TreeKind::Table, fresh_table.root())
                .get(
                    &mut fresh_snapshot,
                    &encode_rowid_key(RowId::new(2).unwrap())
                )
                .unwrap(),
            Some(RowCodec::encode(&old_table, &columns, &[crate::Value::Int64(2)], 1024).unwrap())
        );
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

    fn large_user_tree(batch: &mut WriteBatch) -> BTree {
        let mut tree = BTree::create(batch, TreeKind::Table).unwrap();
        for key in 0_u64..80 {
            tree.insert(batch, &key.to_be_bytes(), &[42; 700]).unwrap();
        }
        tree.insert(batch, b"overflow", &[43; 5000]).unwrap();
        tree
    }

    fn large_index_tree(batch: &mut WriteBatch) -> BTree {
        let mut tree = BTree::create(batch, TreeKind::Index).unwrap();
        for key in 0_u64..80 {
            tree.insert(batch, &key.to_be_bytes(), &[42; 700]).unwrap();
        }
        tree.insert(batch, b"overflow", &[43; 5000]).unwrap();
        tree
    }

    fn copy_internal_root(batch: &mut WriteBatch, from: PageRef, to: PageRef) {
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
    fn drop_rejects_distinct_table_roots_that_share_descendants_without_releasing_pages() {
        // Production break caught: DROP must not release descendants still
        // reachable through another individually valid table root.
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let first_tree = large_user_tree(&mut batch);
        let second_root = batch.allocate_append(PageType::TableInternal).unwrap();
        copy_internal_root(&mut batch, first_tree.root(), second_root);

        let second = new_table(30, second_root, 0, "U");
        let mut rows = records(first_tree.root());
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
        let catalog_tree = raw_tree(&mut batch, rows);
        validate_tree(&mut batch, &first_tree).unwrap();
        validate_tree(&mut batch, &BTree::from_root(TreeKind::Table, second_root)).unwrap();
        CatalogReader::open_candidate(&mut batch, &catalog_tree, CATALOG_V1).unwrap();
        publish(&mut batch, &catalog_tree);
        batch.commit().unwrap();

        let committed_page_count = pager.page_count();
        let (first, second, first_pages, second_pages, shared_pages) = {
            let mut snapshot = pager.begin_read().unwrap();
            let catalog = snapshot.catalog_reader().unwrap();
            let first = catalog
                .find_table_by_id(TableId::new(10).unwrap())
                .unwrap()
                .clone();
            let second = catalog.find_table_by_id(second.id()).unwrap().clone();
            assert_ne!(first.root(), second.root());
            let first_pages = validate_tree(
                &mut snapshot,
                &BTree::from_root(TreeKind::Table, first.root()),
            )
            .unwrap()
            .all_live_pages();
            let second_pages = validate_tree(
                &mut snapshot,
                &BTree::from_root(TreeKind::Table, second.root()),
            )
            .unwrap()
            .all_live_pages();
            let shared_pages = first_pages
                .intersection(&second_pages)
                .copied()
                .collect::<BTreeSet<_>>();
            assert!(!shared_pages.is_empty());
            (first, second, first_pages, second_pages, shared_pages)
        };

        let mut batch = pager.try_begin_write().unwrap();
        let error = drop_relation(&mut batch, first.id()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Corruption);
        assert!(error.to_string().contains("overlap"));
        let catalog = batch.catalog_reader().unwrap();
        assert_eq!(catalog.find_table_by_id(first.id()), Some(&first));
        assert_eq!(catalog.find_table_by_id(second.id()), Some(&second));
        assert_eq!(
            validate_tree(&mut batch, &BTree::from_root(TreeKind::Table, first.root()))
                .unwrap()
                .all_live_pages(),
            first_pages
        );
        assert_eq!(
            validate_tree(
                &mut batch,
                &BTree::from_root(TreeKind::Table, second.root())
            )
            .unwrap()
            .all_live_pages(),
            second_pages
        );
        batch.commit().unwrap();

        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        assert_eq!(catalog.find_table_by_id(first.id()), Some(&first));
        assert_eq!(catalog.find_table_by_id(second.id()), Some(&second));
        drop(snapshot);

        let mut allocation = pager.try_begin_write().unwrap();
        let mut reused = BTreeSet::new();
        loop {
            let page = allocation.allocate(PageType::TableLeaf).unwrap();
            if page.page_id().0 >= committed_page_count {
                break;
            }
            reused.insert(page.page_id());
        }
        assert!(shared_pages.is_disjoint(&reused));
        allocation.rollback().unwrap();
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
        let catalog =
            CatalogReader::open_validated(&mut source, &tree, report, CATALOG_V1).unwrap();
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
            .validate_deep_from(&mut source, &tree, report, CATALOG_V1)
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
    fn ordinary_open_is_shallow_but_deep_validation_walks_index_descendants() {
        // Production break caught: omitting index trees from the deep live-set
        // accepts corrupt index descendants that table-only validation never sees.
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let foreign_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let index = large_index_tree(&mut batch);
        let report = validate_tree(&mut batch, &index).unwrap();
        assert!(report.tree_pages.len() > 2);
        assert!(!report.overflow_pages.is_empty());
        let catalog = raw_tree(
            &mut batch,
            index_records(table_root, index.root(), foreign_root),
        );
        publish_with_features(
            &mut batch,
            &catalog,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        );
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let leaf_id = *report
            .tree_pages
            .iter()
            .find(|id| {
                **id != index.root().page_id()
                    && read.read_page_by_id(**id).unwrap().header().page_type()
                        == PageType::IndexLeaf
            })
            .unwrap();
        let leaf = read.read_page_by_id(leaf_id).unwrap();
        let leaf_ref = PageRef::new(leaf_id, leaf.header().generation()).unwrap();
        drop(read);

        let mut batch = pager.begin_write().unwrap();
        batch.modify(leaf_ref).unwrap().bytes_mut()[48..56].fill(0xff);
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        assert_eq!(
            catalog.validate_deep(&mut read).unwrap_err().code(),
            ErrorCode::Corruption
        );
        drop(read);
        drop(pager);
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

    #[test]
    fn ordinary_open_is_shallow_but_deep_validation_walks_constraint_system_index_descendants() {
        // The local-constraint graph must use the same shallow-open boundary as
        // explicit indexes: a checksummed malformed system-index descendant is
        // accepted by ordinary open, then rejected only by explicit validation.
        let (temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.begin_write().unwrap();
        let table_root = BTree::create(&mut batch, TreeKind::Table).unwrap().root();
        let index = large_index_tree(&mut batch);
        let report = validate_tree(&mut batch, &index).unwrap();
        assert!(report.tree_pages.len() > 2);
        let catalog = raw_tree(
            &mut batch,
            local_constraint_records(table_root, index.root()),
        );
        publish_with_features(
            &mut batch,
            &catalog,
            CATALOG_V1 | super::super::features::INDEX_CONSTRAINT_V1,
        );
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let leaf_id = *report
            .tree_pages
            .iter()
            .find(|id| {
                **id != index.root().page_id()
                    && read.read_page_by_id(**id).unwrap().header().page_type()
                        == PageType::IndexLeaf
            })
            .unwrap();
        let leaf = read.read_page_by_id(leaf_id).unwrap();
        let leaf_ref = PageRef::new(leaf_id, leaf.header().generation()).unwrap();
        drop(read);

        let mut batch = pager.begin_write().unwrap();
        batch.modify(leaf_ref).unwrap().bytes_mut()[48..56].fill(0xff);
        batch.commit().unwrap();

        let mut read = pager.begin_read().unwrap();
        let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
        let constraint = catalog
            .find_constraint_by_id(ConstraintId::new(50).unwrap())
            .unwrap();
        assert_eq!(constraint.index_id(), Some(IndexId::new(30).unwrap()));
        assert_eq!(
            catalog
                .find_index_by_id(IndexId::new(30).unwrap())
                .unwrap()
                .ownership(),
            IndexOwnership::Constraint(constraint.id())
        );
        assert_eq!(
            catalog.validate_deep(&mut read).unwrap_err().code(),
            ErrorCode::Corruption
        );
        drop(read);
        drop(pager);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            let pager = Pager::open(temp.path(), mode, PagerConfig::test_default(4096)).unwrap();
            let mut read = pager.begin_read().unwrap();
            let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
            assert_eq!(
                catalog.validate_deep(&mut read).unwrap_err().code(),
                ErrorCode::Corruption,
                "mode: {mode:?}"
            );
        }
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
        CatalogReader::open_validated(&mut source, &tree, report, CATALOG_V1).unwrap();
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
