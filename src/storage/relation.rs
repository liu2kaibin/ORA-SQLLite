use std::{collections::BTreeSet, ops::Bound};

#[cfg(test)]
use std::cell::{Cell, RefCell};

use crate::{DbError, ErrorCode, Result, Value};

#[cfg(test)]
use super::catalog::{
    ConstraintBundleFaultPoint, ConstraintRemovalFaultGuard, ConstraintRemovalFaultPoint,
    fail_constraint_bundle_at_for_test, fail_constraint_removal_at_for_test,
};

use super::{
    PageRef, ReadSnapshot, WriteBatch,
    btree::{BTree, BTreeScanState, TreeKind},
    catalog::{
        CatalogReader, CatalogWriter,
        types::{
            CatalogName, ColumnDef, ColumnId, ColumnType, ConstraintBundle, ConstraintDef,
            ConstraintId, ConstraintKind, ForeignKeyTarget, IndexDef, IndexId, IndexOwnership,
            MAX_COLUMNS, ObjectKind, RowId, SchemaVersion, TableDef, TableId,
        },
    },
    constraint::{
        ConstraintCatalog, ForeignKeyCatalog,
        program::{CheckMemory, CheckProgramTemplate},
    },
    index::{IndexStorage, PreparedIndexEntry},
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

pub(crate) struct ConstrainedTableSpec<'a> {
    table: TableSpec,
    constraints: Box<[TableConstraintSpec<'a>]>,
}

pub(crate) enum TableConstraintSpec<'a> {
    PrimaryKey {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
    },
    Unique {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
    },
    NotNull {
        name: Option<CatalogName>,
        column: u16,
    },
    Check {
        name: Option<CatalogName>,
        template: &'a CheckProgramTemplate,
    },
    ForeignKey {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
        parent_table_id: TableId,
        parent_constraint_id: ConstraintId,
    },
}

struct AllocatedConstraint {
    id: ConstraintId,
    index_id: Option<IndexId>,
    name: CatalogName,
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

/// The only SQL DML writer. It keeps one table tree, every owned index tree,
/// and one catalog writer together so a failed row mutation never publishes an
/// intermediate root set.
pub(crate) struct ConstrainedTableWrite {
    schema: Relation,
    constraints: ConstraintCatalog,
    foreign_keys: ForeignKeyCatalog,
    rows: BTree,
    indexes: Box<[IndexStorage]>,
    catalog: CatalogWriter,
    original_table_root: PageRef,
    next_rowid: RowId,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConstrainedCreateFaultPoint {
    SystemIndexRootAllocated,
    ConstraintRecordFamilyInserted,
    FeatureEnabled,
    ParentVersionBumped,
    CatalogFinished,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConstrainedDropFaultPoint {
    ExplicitIndexRelease,
    SystemIndexRelease,
    ReferencedByRemoval,
    ConstraintRemoval,
    ParentVersionBump,
    TableRelease,
    CatalogFinished,
}

#[cfg(test)]
thread_local! {
    static NEXT_AFTER_FAILURE: Cell<Option<(usize, ErrorCode)>> = const { Cell::new(None) };
    static VALUE_VALIDATION_LIMITS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    static CONSTRAINED_CREATE_FAILURE: Cell<Option<ConstrainedCreateFaultPoint>> = const { Cell::new(None) };
    static CONSTRAINED_DROP_FAILURE: Cell<Option<(ConstrainedDropFaultPoint, usize)>> = const { Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct ConstrainedDropFaultGuard;

#[cfg(test)]
impl Drop for ConstrainedDropFaultGuard {
    fn drop(&mut self) {
        CONSTRAINED_DROP_FAILURE.set(None);
    }
}

#[cfg(test)]
pub(crate) fn fail_constrained_drop_at_for_test(
    point: ConstrainedDropFaultPoint,
    successful_occurrences: usize,
) -> ConstrainedDropFaultGuard {
    CONSTRAINED_DROP_FAILURE.set(Some((point, successful_occurrences)));
    ConstrainedDropFaultGuard
}

#[cfg(test)]
pub(crate) fn fail_table_write_next_after_for_test(successful_calls: usize, code: ErrorCode) {
    NEXT_AFTER_FAILURE.set(Some((successful_calls, code)));
}

#[cfg(test)]
pub(crate) fn corrupt_relation_row_for_test(
    batch: &mut WriteBatch,
    table_id: TableId,
    rowid: RowId,
    bytes: &[u8],
) -> Result<()> {
    let table = batch
        .catalog_reader()?
        .find_table_by_id(table_id)
        .cloned()
        .ok_or_else(|| DbError::new(ErrorCode::SchemaChanged, "test table disappeared"))?;
    let mut catalog = CatalogWriter::bootstrap(batch)?;
    let mut rows = BTree::from_root(TreeKind::Table, table.root());
    let key = encode_rowid_key(rowid);
    if rows.delete(batch, &key)?.is_none() {
        return Err(DbError::new(
            ErrorCode::SchemaChanged,
            "test row disappeared",
        ));
    }
    rows.insert(batch, &key, bytes)?;
    catalog.update_table_root(batch, table_id, rows.root())?;
    catalog.finish(batch)
}

#[cfg(test)]
pub(crate) fn take_value_validation_limits_for_test() -> Vec<usize> {
    VALUE_VALIDATION_LIMITS.with(|limits| std::mem::take(&mut *limits.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn fail_constrained_create_at_for_test(point: ConstrainedCreateFaultPoint) {
    match point {
        ConstrainedCreateFaultPoint::ConstraintRecordFamilyInserted => {
            fail_constraint_bundle_at_for_test(ConstraintBundleFaultPoint::RecordsInserted);
        }
        ConstrainedCreateFaultPoint::FeatureEnabled => {
            fail_constraint_bundle_at_for_test(ConstraintBundleFaultPoint::FeatureEnabled);
        }
        ConstrainedCreateFaultPoint::SystemIndexRootAllocated
        | ConstrainedCreateFaultPoint::ParentVersionBumped
        | ConstrainedCreateFaultPoint::CatalogFinished => {
            CONSTRAINED_CREATE_FAILURE.set(Some(point));
        }
    }
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

#[cfg(test)]
fn constrained_create_fault_for_test(point: ConstrainedCreateFaultPoint) -> Result<()> {
    CONSTRAINED_CREATE_FAILURE.with(|failure| {
        if failure.get() == Some(point) {
            failure.set(None);
            Err(DbError::new(
                ErrorCode::Io,
                "injected constrained CREATE late failure",
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn constrained_drop_fault_for_test(point: ConstrainedDropFaultPoint) -> Result<()> {
    CONSTRAINED_DROP_FAILURE.with(|failure| match failure.get() {
        Some((armed, 0)) if armed == point => {
            failure.set(None);
            Err(DbError::new(
                ErrorCode::Io,
                "injected constrained DROP late failure",
            ))
        }
        Some((armed, remaining)) if armed == point => {
            failure.set(Some((armed, remaining - 1)));
            Ok(())
        }
        _ => Ok(()),
    })
}

#[cfg(test)]
fn constrained_drop_referenced_by_fault_for_test() -> Option<ConstraintRemovalFaultGuard> {
    CONSTRAINED_DROP_FAILURE.with(|failure| match failure.get() {
        Some((ConstrainedDropFaultPoint::ReferencedByRemoval, 0)) => {
            failure.set(None);
            Some(fail_constraint_removal_at_for_test(
                ConstraintRemovalFaultPoint::ReferencedBy,
            ))
        }
        Some((ConstrainedDropFaultPoint::ReferencedByRemoval, remaining)) => {
            failure.set(Some((
                ConstrainedDropFaultPoint::ReferencedByRemoval,
                remaining - 1,
            )));
            None
        }
        _ => None,
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

impl<'a> ConstrainedTableSpec<'a> {
    pub(crate) fn new(table: TableSpec, constraints: Vec<TableConstraintSpec<'a>>) -> Result<Self> {
        let mut explicit_names = BTreeSet::new();
        let mut primary_key_seen = false;
        let mut required_non_nullable = vec![false; table.columns().len()];
        for constraint in &constraints {
            if let Some(name) = constraint.explicit_name() {
                validate_constraint_name(name)?;
                if !explicit_names.insert(name.as_str()) {
                    return Err(DbError::new(
                        ErrorCode::Constraint,
                        "CREATE TABLE contains a duplicate constraint name",
                    ));
                }
            }
            match constraint {
                TableConstraintSpec::PrimaryKey { columns, .. } => {
                    if primary_key_seen {
                        return Err(DbError::new(
                            ErrorCode::Constraint,
                            "CREATE TABLE contains multiple primary keys",
                        ));
                    }
                    primary_key_seen = true;
                    validate_key_columns(columns, table.columns().len())?;
                    for ordinal in columns {
                        required_non_nullable[usize::from(*ordinal)] = true;
                    }
                }
                TableConstraintSpec::Unique { columns, .. } => {
                    validate_key_columns(columns, table.columns().len())?;
                }
                TableConstraintSpec::NotNull { column, .. } => {
                    let column = usize::from(*column);
                    if column >= table.columns().len() {
                        return Err(DbError::new(
                            ErrorCode::Type,
                            "not-null constraint column ordinal is absent",
                        ));
                    }
                    required_non_nullable[column] = true;
                }
                // CheckProgramTemplate is structurally validated by its
                // checked constructor. Its only identity conversion happens
                // below, after stable ColumnIds have been allocated under the
                // writer and before any catalog definition is published.
                TableConstraintSpec::Check { .. } => {}
                TableConstraintSpec::ForeignKey { columns, .. } => {
                    validate_key_columns(columns, table.columns().len())?;
                }
            }
        }
        for (column, required_non_nullable) in table.columns().iter().zip(required_non_nullable) {
            if required_non_nullable && column.nullable() {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    "primary-key and not-null columns must be non-nullable",
                ));
            }
        }
        Ok(Self {
            table,
            constraints: constraints.into_boxed_slice(),
        })
    }

    pub(crate) const fn table(&self) -> &TableSpec {
        &self.table
    }

    pub(crate) const fn constraints(&self) -> &[TableConstraintSpec<'a>] {
        &self.constraints
    }
}

impl TableConstraintSpec<'_> {
    fn explicit_name(&self) -> Option<&CatalogName> {
        match self {
            Self::PrimaryKey { name, .. }
            | Self::Unique { name, .. }
            | Self::NotNull { name, .. }
            | Self::Check { name, .. }
            | Self::ForeignKey { name, .. } => name.as_ref(),
        }
    }

    fn key_columns(&self) -> Option<&[u16]> {
        match self {
            Self::PrimaryKey { columns, .. } | Self::Unique { columns, .. } => Some(columns),
            Self::NotNull { .. } | Self::Check { .. } | Self::ForeignKey { .. } => None,
        }
    }

    fn kind(&self) -> ConstraintKind {
        match self {
            Self::PrimaryKey { .. } => ConstraintKind::PrimaryKey,
            Self::Unique { .. } => ConstraintKind::Unique,
            Self::NotNull { .. } => ConstraintKind::NotNull,
            Self::Check { .. } => ConstraintKind::Check,
            Self::ForeignKey { .. } => ConstraintKind::ForeignKey,
        }
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

    pub(crate) fn columns(&self) -> &[ColumnDef] {
        &self.columns
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

impl ConstrainedTableWrite {
    pub(crate) fn open(batch: &mut WriteBatch, table_id: TableId) -> Result<Self> {
        let (schema, constraints, foreign_keys, indexes) = {
            let catalog = batch.catalog_reader()?;
            let schema = Relation::open(&catalog, table_id)?;
            let constraints = ConstraintCatalog::open(&catalog, table_id)?;
            let foreign_keys = ForeignKeyCatalog::open(&catalog, table_id)?;
            let mut indexes = catalog
                .indexes_for_table(table_id)
                .map(|index| IndexStorage::open(&catalog, index.id()))
                .collect::<Result<Vec<_>>>()?;
            indexes.sort_by_key(IndexStorage::id);
            (
                schema,
                constraints,
                foreign_keys,
                indexes.into_boxed_slice(),
            )
        };
        let original_table_root = schema.table.root();
        let next_rowid = schema.table.next_rowid();
        let rows = BTree::from_root(TreeKind::Table, original_table_root);
        let catalog = CatalogWriter::bootstrap(batch)?;
        Ok(Self {
            schema,
            constraints,
            foreign_keys,
            rows,
            indexes,
            catalog,
            original_table_root,
            next_rowid,
        })
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

    pub(crate) fn validate_row(
        &self,
        values: &[Value],
        memory: &mut impl CheckMemory,
    ) -> Result<()> {
        self.constraints
            .validate_row(&self.schema.columns, values, memory)
    }

    pub(crate) fn insert(
        &mut self,
        batch: &mut WriteBatch,
        values: &[Value],
        max_row_bytes: usize,
        memory: &mut impl CheckMemory,
    ) -> Result<RowId> {
        self.validate_row(values, memory)?;
        self.foreign_keys.validate_child_row(batch, values)?;
        let encoded = RowCodec::encode(
            &self.schema.table,
            &self.schema.columns,
            values,
            max_row_bytes,
        )?;
        let rowid = self.next_rowid;
        let entries = self.prepare_entries(values, rowid)?;
        self.check_unique(batch, &entries, None)?;
        let scope = batch.begin_mutation_scope()?;
        let catalog_checkpoint = self.catalog.checkpoint();
        let table_root = self.rows.root();
        let index_roots = self.index_roots();
        let result = (|| {
            let allocated = self.catalog.allocate_rowid(batch, self.schema.table.id())?;
            if allocated != rowid {
                return Err(DbError::new(
                    ErrorCode::Corruption,
                    "catalog ROWID changed while a table writer was open",
                ));
            }
            self.rows
                .insert(batch, &encode_rowid_key(rowid), &encoded)?;
            for (index, entry) in self.indexes.iter_mut().zip(&entries) {
                index.insert(batch, entry)?;
            }
            Ok(rowid)
        })();
        match result {
            Ok(rowid) => {
                batch.finish_mutation_scope(scope)?;
                self.next_rowid = RowId::new(rowid.get().checked_add(1).ok_or_else(|| {
                    DbError::new(ErrorCode::Type, "ROWID counter exhausted for table")
                })?)?;
                Ok(rowid)
            }
            Err(error) => {
                batch.rollback_mutation_scope(scope)?;
                self.restore(catalog_checkpoint, table_root, &index_roots);
                Err(error)
            }
        }
    }

    pub(crate) fn replace(
        &mut self,
        batch: &mut WriteBatch,
        rowid: RowId,
        old: &[Value],
        new: &[Value],
        max_row_bytes: usize,
        memory: &mut impl CheckMemory,
        foreign_key_work_bytes: u64,
    ) -> Result<()> {
        self.validate_row(new, memory)?;
        self.foreign_keys.validate_child_row(batch, new)?;
        let encoded =
            RowCodec::encode(&self.schema.table, &self.schema.columns, new, max_row_bytes)?;
        let old_entries = self.prepare_entries(old, rowid)?;
        let new_entries = self.prepare_entries(new, rowid)?;
        self.check_unique(batch, &new_entries, Some(rowid))?;
        self.foreign_keys
            .validate_parent_update(batch, old, new, foreign_key_work_bytes)?;
        let scope = batch.begin_mutation_scope()?;
        let catalog_checkpoint = self.catalog.checkpoint();
        let table_root = self.rows.root();
        let index_roots = self.index_roots();
        let key = encode_rowid_key(rowid);
        let result = (|| {
            for (index, entry) in self.indexes.iter_mut().zip(&old_entries) {
                index.delete(batch, entry)?;
            }
            if self.rows.delete(batch, &key)?.is_none() {
                return Err(DbError::new(
                    ErrorCode::Corruption,
                    format!("row ID {} disappeared during replacement", rowid.get()),
                ));
            }
            self.rows.insert(batch, &key, &encoded)?;
            for (index, entry) in self.indexes.iter_mut().zip(&new_entries) {
                index.insert(batch, entry)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => batch.finish_mutation_scope(scope),
            Err(error) => {
                batch.rollback_mutation_scope(scope)?;
                self.restore(catalog_checkpoint, table_root, &index_roots);
                Err(error)
            }
        }
    }

    pub(crate) fn delete(
        &mut self,
        batch: &mut WriteBatch,
        rowid: RowId,
        old: &[Value],
        foreign_key_work_bytes: u64,
    ) -> Result<bool> {
        let entries = self.prepare_entries(old, rowid)?;
        let key = encode_rowid_key(rowid);
        if self.rows.get(batch, &key)?.is_none() {
            return Ok(false);
        }
        self.foreign_keys
            .validate_parent_delete(batch, old, foreign_key_work_bytes)?;
        let scope = batch.begin_mutation_scope()?;
        let catalog_checkpoint = self.catalog.checkpoint();
        let table_root = self.rows.root();
        let index_roots = self.index_roots();
        let result = (|| {
            for (index, entry) in self.indexes.iter_mut().zip(&entries) {
                index.delete(batch, entry)?;
            }
            match self.rows.delete(batch, &key)? {
                Some(_) => Ok(true),
                None => Err(DbError::new(
                    ErrorCode::Corruption,
                    format!("row ID {} disappeared during deletion", rowid.get()),
                )),
            }
        })();
        match result {
            Ok(deleted) => {
                batch.finish_mutation_scope(scope)?;
                Ok(deleted)
            }
            Err(error) => {
                batch.rollback_mutation_scope(scope)?;
                self.restore(catalog_checkpoint, table_root, &index_roots);
                Err(error)
            }
        }
    }

    pub(crate) fn finish(mut self, batch: &mut WriteBatch) -> Result<()> {
        let scope = batch.begin_mutation_scope()?;
        let catalog_checkpoint = self.catalog.checkpoint();
        let result = (|| {
            if self.rows.root() != self.original_table_root {
                self.catalog
                    .update_table_root(batch, self.schema.table.id(), self.rows.root())?;
            }
            for index in &self.indexes {
                if index.root_changed() {
                    self.catalog
                        .update_index_root(batch, index.id(), index.root())?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            batch.rollback_mutation_scope(scope)?;
            self.catalog.restore(catalog_checkpoint);
            return Err(error);
        }
        let result = self.catalog.finish(batch);
        match result {
            Ok(()) => batch.finish_mutation_scope(scope),
            Err(error) => {
                batch.rollback_mutation_scope(scope)?;
                Err(error)
            }
        }
    }

    fn prepare_entries(&self, values: &[Value], rowid: RowId) -> Result<Vec<PreparedIndexEntry>> {
        self.indexes
            .iter()
            .map(|index| index.prepare(values, rowid))
            .collect()
    }

    fn check_unique(
        &self,
        batch: &mut WriteBatch,
        entries: &[PreparedIndexEntry],
        exclude: Option<RowId>,
    ) -> Result<()> {
        for (index, entry) in self.indexes.iter().zip(entries) {
            index.check_unique(batch, entry, exclude)?;
        }
        Ok(())
    }

    fn index_roots(&self) -> Vec<PageRef> {
        self.indexes.iter().map(IndexStorage::root).collect()
    }

    fn restore(
        &mut self,
        catalog_checkpoint: super::catalog::CatalogWriterCheckpoint,
        table_root: PageRef,
        index_roots: &[PageRef],
    ) {
        self.catalog.restore(catalog_checkpoint);
        self.rows = BTree::from_root(TreeKind::Table, table_root);
        for (index, root) in self.indexes.iter_mut().zip(index_roots) {
            index.restore_root(*root);
        }
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

/// Atomically materialize a new relation with its local constraints and any
/// constraint-owned unique indexes. The entire candidate remains behind one
/// statement checkpoint until the catalog writer validates and publishes it.
pub(crate) fn create_constrained_relation(
    batch: &mut WriteBatch,
    spec: &ConstrainedTableSpec<'_>,
) -> Result<TableId> {
    prevalidate_constrained_create(batch, spec)?;
    let checkpoint = batch.statement_checkpoint()?;
    let result = (|| {
        let mut catalog = CatalogWriter::bootstrap(batch)?;
        let table_id = TableId::new(catalog.allocate_object_id(batch)?.get())?;
        let columns = spec
            .table()
            .columns()
            .iter()
            .enumerate()
            .map(|(ordinal, column)| {
                ColumnDef::new(
                    ColumnId::new(catalog.allocate_object_id(batch)?.get())?,
                    table_id,
                    u16::try_from(ordinal).map_err(|_| {
                        DbError::new(ErrorCode::Type, "column ordinal does not fit the catalog")
                    })?,
                    column.column_type(),
                    column.nullable(),
                    column.name().clone(),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let mut statement_names = spec
            .constraints()
            .iter()
            .filter_map(TableConstraintSpec::explicit_name)
            .map(|name| name.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let mut allocated = Vec::with_capacity(spec.constraints().len());
        for constraint in spec.constraints() {
            let id = ConstraintId::new(catalog.allocate_object_id(batch)?.get())?;
            let index_id = match constraint.kind() {
                ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
                    Some(IndexId::new(catalog.allocate_object_id(batch)?.get())?)
                }
                ConstraintKind::NotNull | ConstraintKind::Check | ConstraintKind::ForeignKey => {
                    None
                }
            };
            let name = match constraint.explicit_name() {
                Some(name) => name.clone(),
                None => generate_constraint_name(id, |candidate| {
                    Ok(statement_names.contains(candidate.as_str())
                        || catalog.constraint_name_exists(batch, candidate)?)
                })?,
            };
            statement_names.insert(name.as_str().to_owned());
            allocated.push(AllocatedConstraint { id, index_id, name });
        }

        let table_tree = BTree::create(batch, TreeKind::Table)?;
        let table = TableDef::new(
            table_id,
            SchemaVersion::new(1)?,
            table_tree.root(),
            RowId::new(1)?,
            u16::try_from(columns.len()).map_err(|_| {
                DbError::new(
                    ErrorCode::Type,
                    "table column count does not fit the catalog",
                )
            })?,
            spec.table().name().clone(),
        )?;
        let column_ids = columns.iter().map(ColumnDef::id).collect::<Vec<_>>();
        let bundles = spec
            .constraints()
            .iter()
            .zip(allocated)
            .map(|(constraint, allocated)| {
                build_constraint_bundle(batch, table_id, &column_ids, constraint, allocated)
            })
            .collect::<Result<Vec<_>>>()?;

        // Do not publish a definition until the complete graph (including all
        // roots and resolved CHECK programs) is available in memory.
        catalog.insert_table_definition(batch, &table)?;
        for column in &columns {
            catalog.insert_column_definition(batch, column)?;
        }
        for bundle in &bundles {
            catalog.insert_constraint_bundle(batch, bundle)?;
        }
        let parent_tables = bundles
            .iter()
            .filter_map(|bundle| bundle.definition.parent_table_id())
            .collect::<BTreeSet<_>>();
        for parent_table in parent_tables {
            catalog.bump_table_schema_version(batch, parent_table)?;
            #[cfg(test)]
            constrained_create_fault_for_test(ConstrainedCreateFaultPoint::ParentVersionBumped)?;
        }
        catalog.finish(batch)?;
        #[cfg(test)]
        constrained_create_fault_for_test(ConstrainedCreateFaultPoint::CatalogFinished)?;
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

fn prevalidate_constrained_create(
    batch: &mut WriteBatch,
    spec: &ConstrainedTableSpec<'_>,
) -> Result<()> {
    let catalog = batch.catalog_reader()?;
    if catalog.object_name_exists(ObjectKind::Table, spec.table().name()) {
        return Err(DbError::new(
            ErrorCode::SchemaChanged,
            "prepared CREATE name is no longer available",
        ));
    }
    for constraint in spec.constraints() {
        if let Some(name) = constraint.explicit_name() {
            if catalog.object_name_exists(ObjectKind::Constraint, name) {
                return Err(DbError::new(
                    ErrorCode::SchemaChanged,
                    "prepared constraint name is no longer available",
                ));
            }
        }
        if let TableConstraintSpec::ForeignKey {
            columns,
            parent_table_id,
            parent_constraint_id,
            ..
        } = constraint
        {
            prevalidate_foreign_key_target(
                &catalog,
                spec.table(),
                columns,
                *parent_table_id,
                *parent_constraint_id,
            )?;
        }
    }
    let allocated = required_object_count(spec)?;
    catalog
        .next_object_id()
        .get()
        .checked_add(allocated)
        .ok_or_else(|| DbError::new(ErrorCode::Type, "catalog object ID counter is exhausted"))?;
    Ok(())
}

fn prevalidate_foreign_key_target(
    catalog: &CatalogReader,
    child_table: &TableSpec,
    local_ordinals: &[u16],
    parent_table_id: TableId,
    parent_constraint_id: ConstraintId,
) -> Result<()> {
    let parent_table = catalog.find_table_by_id(parent_table_id).ok_or_else(|| {
        DbError::new(
            ErrorCode::SchemaChanged,
            "prepared foreign key parent table is no longer present",
        )
    })?;
    parent_table
        .schema_version()
        .get()
        .checked_add(1)
        .ok_or_else(|| {
            DbError::new(ErrorCode::Type, "table schema version counter is exhausted")
        })?;
    let parent_constraint = catalog
        .find_constraint_by_id(parent_constraint_id)
        .filter(|constraint| {
            constraint.table_id() == parent_table_id
                && matches!(
                    constraint.kind(),
                    ConstraintKind::PrimaryKey | ConstraintKind::Unique
                )
        })
        .ok_or_else(|| {
            DbError::new(
                ErrorCode::SchemaChanged,
                "prepared foreign key parent constraint identity has changed",
            )
        })?;
    let parent_columns = catalog
        .columns_for_constraint(parent_constraint.id())
        .collect::<Vec<_>>();
    if local_ordinals.len() != parent_columns.len() {
        return Err(DbError::new(
            ErrorCode::SchemaChanged,
            "prepared foreign key parent constraint column count has changed",
        ));
    }
    for (local_ordinal, parent_column) in local_ordinals.iter().zip(parent_columns) {
        let child_type = child_table
            .columns()
            .get(usize::from(*local_ordinal))
            .ok_or_else(|| {
                DbError::new(
                    ErrorCode::Type,
                    "foreign-key child column ordinal is absent",
                )
            })?
            .column_type();
        let parent_type = parent_column.column_type();
        if child_type != parent_type
            && !(child_type == ColumnType::Int64 && parent_type == ColumnType::Decimal)
        {
            return Err(DbError::new(
                ErrorCode::SchemaChanged,
                "prepared foreign key child and parent column types are incompatible",
            ));
        }
    }
    Ok(())
}

fn required_object_count(spec: &ConstrainedTableSpec<'_>) -> Result<u64> {
    let columns = u64::try_from(spec.table().columns().len()).map_err(|_| {
        DbError::new(
            ErrorCode::Type,
            "table column count exceeds stable ID range",
        )
    })?;
    let constraints = u64::try_from(spec.constraints().len())
        .map_err(|_| DbError::new(ErrorCode::Type, "constraint count exceeds stable ID range"))?;
    let system_indexes = u64::try_from(
        spec.constraints()
            .iter()
            .filter(|constraint| constraint.key_columns().is_some())
            .count(),
    )
    .map_err(|_| DbError::new(ErrorCode::Type, "index count exceeds stable ID range"))?;
    1_u64
        .checked_add(columns)
        .and_then(|count| count.checked_add(constraints))
        .and_then(|count| count.checked_add(system_indexes))
        .ok_or_else(|| DbError::new(ErrorCode::Type, "catalog object ID counter is exhausted"))
}

fn validate_constraint_name(name: &CatalogName) -> Result<()> {
    if name.as_bytes().len() > 128 {
        return Err(DbError::new(
            ErrorCode::Type,
            "constraint name must not exceed 128 UTF-8 bytes",
        ));
    }
    Ok(())
}

fn validate_key_columns(columns: &[u16], column_count: usize) -> Result<()> {
    if columns.is_empty() || columns.len() > usize::from(MAX_COLUMNS) {
        return Err(DbError::new(
            ErrorCode::Type,
            "key constraint column count must be between 1 and 1000",
        ));
    }
    let mut seen = BTreeSet::new();
    for ordinal in columns {
        if usize::from(*ordinal) >= column_count {
            return Err(DbError::new(
                ErrorCode::Type,
                "key constraint column ordinal is absent",
            ));
        }
        if !seen.insert(*ordinal) {
            return Err(DbError::new(
                ErrorCode::Constraint,
                "key constraint contains a repeated column",
            ));
        }
    }
    Ok(())
}

fn generate_constraint_name(
    id: ConstraintId,
    mut occupied: impl FnMut(&CatalogName) -> Result<bool>,
) -> Result<CatalogName> {
    let base = format!("SYS_C{}", id.get());
    let mut suffix: Option<u64> = None;
    loop {
        let candidate = match suffix {
            None => base.clone(),
            Some(suffix) => {
                let suffix = suffix.to_string();
                let bytes = base
                    .len()
                    .checked_add(1)
                    .and_then(|length| length.checked_add(suffix.len()))
                    .ok_or_else(|| {
                        DbError::new(ErrorCode::Type, "generated constraint name is too long")
                    })?;
                if bytes > 128 {
                    return Err(DbError::new(
                        ErrorCode::Type,
                        "generated constraint name must not exceed 128 UTF-8 bytes",
                    ));
                }
                format!("{base}_{suffix}")
            }
        };
        if candidate.len() > 128 {
            return Err(DbError::new(
                ErrorCode::Type,
                "generated constraint name must not exceed 128 UTF-8 bytes",
            ));
        }
        let candidate = CatalogName::new(candidate)?;
        if !occupied(&candidate)? {
            return Ok(candidate);
        }
        suffix = Some(suffix.unwrap_or(0).checked_add(1).ok_or_else(|| {
            DbError::new(
                ErrorCode::Type,
                "generated constraint name suffix is exhausted",
            )
        })?);
    }
}

fn build_constraint_bundle(
    batch: &mut WriteBatch,
    table_id: TableId,
    column_ids: &[ColumnId],
    constraint: &TableConstraintSpec<'_>,
    allocated: AllocatedConstraint,
) -> Result<ConstraintBundle> {
    let columns = match constraint {
        TableConstraintSpec::PrimaryKey { columns, .. }
        | TableConstraintSpec::Unique { columns, .. }
        | TableConstraintSpec::ForeignKey { columns, .. } => columns
            .iter()
            .map(|ordinal| {
                column_ids
                    .get(usize::from(*ordinal))
                    .copied()
                    .ok_or_else(|| {
                        DbError::new(ErrorCode::Type, "key constraint column ordinal is absent")
                    })
            })
            .collect::<Result<Vec<_>>>(),
        TableConstraintSpec::NotNull { column, .. } => column_ids
            .get(usize::from(*column))
            .copied()
            .map(|column| vec![column])
            .ok_or_else(|| {
                DbError::new(
                    ErrorCode::Type,
                    "not-null constraint column ordinal is absent",
                )
            }),
        TableConstraintSpec::Check { .. } => Ok(Vec::new()),
    }?;
    let program = match constraint {
        TableConstraintSpec::Check { template, .. } => Some(
            template
                .resolve(column_ids)?
                .as_bytes()
                .to_vec()
                .into_boxed_slice(),
        ),
        _ => None,
    };
    let foreign_key_target = match constraint {
        TableConstraintSpec::ForeignKey {
            parent_table_id,
            parent_constraint_id,
            ..
        } => Some(ForeignKeyTarget::new(
            *parent_table_id,
            *parent_constraint_id,
        )),
        _ => None,
    };
    let definition = ConstraintDef::new(
        allocated.id,
        table_id,
        constraint.kind(),
        u16::try_from(columns.len()).map_err(|_| {
            DbError::new(
                ErrorCode::Type,
                "constraint column count does not fit the catalog",
            )
        })?,
        allocated.index_id,
        foreign_key_target,
        program.unwrap_or_default(),
        allocated.name.clone(),
    )?;
    let system_index = match allocated.index_id {
        Some(index_id) => {
            let tree = BTree::create(batch, TreeKind::Index)?;
            #[cfg(test)]
            constrained_create_fault_for_test(
                ConstrainedCreateFaultPoint::SystemIndexRootAllocated,
            )?;
            let index = IndexDef::new(
                index_id,
                table_id,
                tree.root(),
                u16::try_from(columns.len()).map_err(|_| {
                    DbError::new(
                        ErrorCode::Type,
                        "index column count does not fit the catalog",
                    )
                })?,
                true,
                IndexOwnership::Constraint(allocated.id),
                allocated.name,
            )?;
            Some((index, columns.clone().into_boxed_slice()))
        }
        None => None,
    };
    Ok(ConstraintBundle {
        definition,
        columns: columns.into_boxed_slice(),
        system_index,
    })
}

fn first_incoming_foreign_key(
    catalog: &CatalogReader,
    table_id: TableId,
) -> Option<&ConstraintDef> {
    catalog
        .constraints_for_table(table_id)
        .filter(|constraint| {
            matches!(
                constraint.kind(),
                ConstraintKind::PrimaryKey | ConstraintKind::Unique
            )
        })
        .flat_map(|constraint| catalog.foreign_keys_referencing(constraint.id()))
        .min_by_key(|constraint| constraint.id())
}

pub(crate) fn drop_relation(batch: &mut WriteBatch, table_id: TableId) -> Result<()> {
    let checkpoint = batch.statement_checkpoint()?;
    let result = (|| {
        let (relation, explicit_indexes, constraints, parent_tables) = {
            let catalog = batch.catalog_reader()?;
            let relation = Relation::open(&catalog, table_id)?;
            if let Some(foreign_key) = first_incoming_foreign_key(&catalog, table_id) {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    format!(
                        "table is referenced by foreign key {}",
                        foreign_key.name().as_str()
                    ),
                ));
            }
            let indexes = catalog
                .indexes_for_table(table_id)
                .map(|index| {
                    let columns = catalog
                        .columns_for_index(index.id())
                        .map(ColumnDef::id)
                        .collect::<Vec<_>>();
                    (index.clone(), columns)
                })
                .collect::<Vec<_>>();
            let constraints = catalog
                .constraints_for_table(table_id)
                .map(|constraint| {
                    let columns = catalog
                        .columns_for_constraint(constraint.id())
                        .map(ColumnDef::id)
                        .collect::<Vec<_>>();
                    (constraint.clone(), columns)
                })
                .collect::<Vec<_>>();
            let parent_tables = constraints
                .iter()
                .filter_map(|(constraint, _)| constraint.parent_table_id())
                .collect::<BTreeSet<_>>();
            // Prove every catalog relationship, owned tree, and live/free
            // disjointness before deleting a record or staging a release.
            catalog.validate_deep_batch(batch)?;
            let mut system_owners = BTreeSet::new();
            let mut explicit_indexes = Vec::new();
            for (index, columns) in indexes {
                match index.ownership() {
                    IndexOwnership::Explicit => explicit_indexes.push((index, columns)),
                    IndexOwnership::Constraint(owner_id) => {
                        if !system_owners.insert(owner_id) {
                            return Err(DbError::new(
                                ErrorCode::Corruption,
                                "constraint owns more than one system index",
                            ));
                        }
                        let owner = constraints
                            .iter()
                            .find(|(constraint, _)| constraint.id() == owner_id)
                            .ok_or_else(|| {
                                DbError::new(
                                    ErrorCode::Corruption,
                                    "system index owner is not a local table constraint",
                                )
                            })?;
                        if !matches!(
                            owner.0.kind(),
                            ConstraintKind::PrimaryKey | ConstraintKind::Unique
                        ) || owner.0.index_id() != Some(index.id())
                        {
                            return Err(DbError::new(
                                ErrorCode::Corruption,
                                "system index is not paired with its key constraint",
                            ));
                        }
                    }
                }
            }
            for (constraint, _) in &constraints {
                if matches!(
                    constraint.kind(),
                    ConstraintKind::PrimaryKey | ConstraintKind::Unique
                ) && !system_owners.contains(&constraint.id())
                {
                    return Err(DbError::new(
                        ErrorCode::Corruption,
                        "key constraint is not paired with exactly one system index",
                    ));
                }
            }
            (relation, explicit_indexes, constraints, parent_tables)
        };
        let mut catalog = CatalogWriter::bootstrap(batch)?;
        for (index, columns) in explicit_indexes {
            catalog.remove_index(batch, &index, &columns)?;
            #[cfg(test)]
            {
                super::index::after_index_release_for_test()?;
                constrained_drop_fault_for_test(ConstrainedDropFaultPoint::ExplicitIndexRelease)?;
            }
        }
        for (constraint, columns) in constraints {
            #[cfg(test)]
            let releases_system_index = constraint.index_id().is_some();
            #[cfg(test)]
            let _referenced_by_fault = constraint
                .foreign_key_target()
                .and_then(|_| constrained_drop_referenced_by_fault_for_test());
            catalog.remove_constraint(batch, &constraint, &columns)?;
            #[cfg(test)]
            if releases_system_index {
                super::index::after_index_release_for_test()?;
                constrained_drop_fault_for_test(ConstrainedDropFaultPoint::SystemIndexRelease)?;
            }
            #[cfg(test)]
            constrained_drop_fault_for_test(ConstrainedDropFaultPoint::ConstraintRemoval)?;
        }
        for parent_table in parent_tables {
            catalog.bump_table_schema_version(batch, parent_table)?;
            #[cfg(test)]
            constrained_drop_fault_for_test(ConstrainedDropFaultPoint::ParentVersionBump)?;
        }
        catalog.remove_relation(batch, &relation.table, &relation.columns)?;
        #[cfg(test)]
        constrained_drop_fault_for_test(ConstrainedDropFaultPoint::TableRelease)?;
        catalog.finish(batch)?;
        #[cfg(test)]
        constrained_drop_fault_for_test(ConstrainedDropFaultPoint::CatalogFinished)?;
        Ok(())
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
    use super::{
        ColumnSpec, ConstrainedCreateFaultPoint, ConstrainedDropFaultPoint, ConstrainedTableSpec,
        ConstrainedTableWrite, Relation, TableConstraintSpec, TableSpec, TableWrite,
        create_constrained_relation, create_relation, drop_relation,
        fail_constrained_create_at_for_test, fail_constrained_drop_at_for_test,
    };
    use crate::storage::index::{IndexSpec, create_index};
    use crate::storage::{
        Pager,
        btree::{BTree, TreeKind},
        catalog::{
            CatalogReader, CatalogWriter, ConstraintBundleFaultPoint, ConstraintRemovalFaultPoint,
            fail_constraint_removal_at_for_test,
            take_constraint_bundle_fault_observations_for_test,
            types::{
                CatalogName, ColumnDef, ColumnId, ColumnType, ConstraintId, ConstraintKind,
                ForeignKeyTarget, IndexOwnership, RowId, SchemaVersion, TableDef, TableId,
            },
        },
        constraint::program::{
            CheckBinaryOp, CheckProgramTemplate, CheckTemplateInstruction, CheckType,
        },
        features::INDEX_CONSTRAINT_V1,
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
    fn constrained_create_allocates_complete_graph_in_one_deterministic_order() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();

        let table = create_constrained_relation(&mut batch, &constrained_spec()).unwrap();
        let catalog = batch.catalog_reader().unwrap();
        let constraints = catalog.constraints_for_table(table).collect::<Vec<_>>();
        assert_eq!(constraints.len(), 4);
        assert_eq!(
            constraints[0].name().as_str(),
            format!("SYS_C{}", constraints[0].id().get())
        );
        for constraint in constraints {
            if matches!(
                constraint.kind(),
                ConstraintKind::PrimaryKey | ConstraintKind::Unique
            ) {
                let index = catalog
                    .find_index_by_id(constraint.index_id().unwrap())
                    .unwrap();
                assert_eq!(
                    index.ownership(),
                    IndexOwnership::Constraint(constraint.id())
                );
                assert!(index.unique());
                assert_eq!(index.name(), constraint.name());
                assert_eq!(
                    catalog
                        .columns_for_index(index.id())
                        .map(ColumnDef::id)
                        .collect::<Vec<_>>(),
                    catalog
                        .columns_for_constraint(constraint.id())
                        .map(ColumnDef::id)
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn constrained_create_publishes_foreign_key_and_reverse_edge_atomically() {
        // Production breaks caught: publishing only one side of the FK graph,
        // leaking any allocator/page/catalog/version state after a late error,
        // or giving a foreign key an unnecessary system index.
        let (_temp, pager) = test_pager(4096).unwrap();
        let (parent_table_id, parent_constraint_id, parent_version) = {
            let mut batch = pager.try_begin_write().unwrap();
            let parent = create_constrained_relation(
                &mut batch,
                &ConstrainedTableSpec::new(
                    table_spec("PARENT", &[("ID", ColumnType::Int64, false)]),
                    vec![TableConstraintSpec::PrimaryKey {
                        name: Some(CatalogName::new("PK_PARENT").unwrap()),
                        columns: vec![0].into_boxed_slice(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
            let catalog = batch.catalog_reader().unwrap();
            let parent_constraint = catalog
                .find_constraint_by_name(&CatalogName::new("PK_PARENT").unwrap())
                .unwrap()
                .id();
            let version = catalog.find_table_by_id(parent).unwrap().schema_version();
            batch.commit().unwrap();
            (parent, parent_constraint, version)
        };
        let child_spec = ConstrainedTableSpec::new(
            table_spec("CHILD", &[("PARENT_ID", ColumnType::Int64, true)]),
            vec![TableConstraintSpec::ForeignKey {
                name: Some(CatalogName::new("FK_CHILD_PARENT").unwrap()),
                columns: vec![0].into_boxed_slice(),
                parent_table_id,
                parent_constraint_id,
            }],
        )
        .unwrap();
        let mut batch = pager.try_begin_write().unwrap();

        for point in [
            ConstrainedCreateFaultPoint::ConstraintRecordFamilyInserted,
            ConstrainedCreateFaultPoint::FeatureEnabled,
            ConstrainedCreateFaultPoint::ParentVersionBumped,
            ConstrainedCreateFaultPoint::CatalogFinished,
        ] {
            let before = batch.checkpoint_mutation().unwrap();
            fail_constrained_create_at_for_test(point);
            let error = create_constrained_relation(&mut batch, &child_spec).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Io, "{point:?}");
            before.assert_matches_batch_for_test(&batch);
            let catalog = batch.catalog_reader().unwrap();
            assert_eq!(
                catalog
                    .find_table_by_id(parent_table_id)
                    .unwrap()
                    .schema_version(),
                parent_version,
                "{point:?}"
            );
            assert!(
                catalog
                    .find_table_by_name(&CatalogName::new("CHILD").unwrap())
                    .is_none(),
                "{point:?}"
            );
            assert_eq!(
                catalog
                    .foreign_keys_referencing(parent_constraint_id)
                    .count(),
                0,
                "{point:?}"
            );
        }

        let child_table_id = create_constrained_relation(&mut batch, &child_spec).unwrap();
        let catalog = batch.catalog_reader().unwrap();
        let child = catalog
            .find_constraint_by_name(&CatalogName::new("FK_CHILD_PARENT").unwrap())
            .unwrap();
        assert_eq!(child.table_id(), child_table_id);
        assert_eq!(child.kind(), ConstraintKind::ForeignKey);
        assert_eq!(child.index_id(), None);
        assert_eq!(
            child.foreign_key_target(),
            Some(ForeignKeyTarget::new(parent_table_id, parent_constraint_id))
        );
        assert_eq!(
            catalog
                .columns_for_constraint(child.id())
                .map(ColumnDef::table_id)
                .collect::<Vec<_>>(),
            vec![child_table_id]
        );
        assert_eq!(
            catalog
                .foreign_keys_referencing(parent_constraint_id)
                .map(|constraint| constraint.id())
                .collect::<Vec<ConstraintId>>(),
            vec![child.id()]
        );
        assert_eq!(
            catalog
                .find_table_by_id(parent_table_id)
                .unwrap()
                .schema_version()
                .get(),
            parent_version.get() + 1
        );
    }

    #[test]
    fn constrained_create_bumps_each_distinct_parent_once() {
        // Production break caught: deriving bumps directly from FK bundles can
        // invalidate one parent twice while missing a different parent.
        let (_temp, pager) = test_pager(4096).unwrap();
        let (first_parent, first_key, second_parent, second_key) = {
            let mut batch = pager.try_begin_write().unwrap();
            let first_parent = create_constrained_relation(
                &mut batch,
                &ConstrainedTableSpec::new(
                    table_spec("FIRST_PARENT", &[("ID", ColumnType::Int64, false)]),
                    vec![TableConstraintSpec::PrimaryKey {
                        name: Some(CatalogName::new("PK_FIRST_PARENT").unwrap()),
                        columns: vec![0].into_boxed_slice(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
            let second_parent = create_constrained_relation(
                &mut batch,
                &ConstrainedTableSpec::new(
                    table_spec("SECOND_PARENT", &[("ID", ColumnType::Int64, false)]),
                    vec![TableConstraintSpec::PrimaryKey {
                        name: Some(CatalogName::new("PK_SECOND_PARENT").unwrap()),
                        columns: vec![0].into_boxed_slice(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
            let catalog = batch.catalog_reader().unwrap();
            let first_key = catalog
                .find_constraint_by_name(&CatalogName::new("PK_FIRST_PARENT").unwrap())
                .unwrap()
                .id();
            let second_key = catalog
                .find_constraint_by_name(&CatalogName::new("PK_SECOND_PARENT").unwrap())
                .unwrap()
                .id();
            batch.commit().unwrap();
            (first_parent, first_key, second_parent, second_key)
        };
        let mut batch = pager.try_begin_write().unwrap();
        create_constrained_relation(
            &mut batch,
            &ConstrainedTableSpec::new(
                table_spec(
                    "MULTI_CHILD",
                    &[
                        ("FIRST_A", ColumnType::Int64, true),
                        ("FIRST_B", ColumnType::Int64, true),
                        ("SECOND", ColumnType::Int64, true),
                    ],
                ),
                vec![
                    TableConstraintSpec::ForeignKey {
                        name: Some(CatalogName::new("FK_FIRST_A").unwrap()),
                        columns: vec![0].into_boxed_slice(),
                        parent_table_id: first_parent,
                        parent_constraint_id: first_key,
                    },
                    TableConstraintSpec::ForeignKey {
                        name: Some(CatalogName::new("FK_FIRST_B").unwrap()),
                        columns: vec![1].into_boxed_slice(),
                        parent_table_id: first_parent,
                        parent_constraint_id: first_key,
                    },
                    TableConstraintSpec::ForeignKey {
                        name: Some(CatalogName::new("FK_SECOND").unwrap()),
                        columns: vec![2].into_boxed_slice(),
                        parent_table_id: second_parent,
                        parent_constraint_id: second_key,
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
        let catalog = batch.catalog_reader().unwrap();
        assert_eq!(
            catalog
                .find_table_by_id(first_parent)
                .unwrap()
                .schema_version()
                .get(),
            2
        );
        assert_eq!(
            catalog
                .find_table_by_id(second_parent)
                .unwrap()
                .schema_version()
                .get(),
            2
        );
    }

    #[test]
    fn drop_parent_is_rejected_before_any_page_or_catalog_change() {
        // Production breaks caught: discovering incoming dependencies only
        // after teardown starts, or reporting a later FK instead of the lowest
        // child ConstraintId across every key on the parent.
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let parent = create_constrained_relation(
            &mut batch,
            &ConstrainedTableSpec::new(
                table_spec(
                    "PARENT",
                    &[
                        ("ID", ColumnType::Int64, false),
                        ("ALT_ID", ColumnType::Int64, true),
                    ],
                ),
                vec![
                    TableConstraintSpec::PrimaryKey {
                        name: Some(CatalogName::new("PK_PARENT").unwrap()),
                        columns: vec![0].into_boxed_slice(),
                    },
                    TableConstraintSpec::Unique {
                        name: Some(CatalogName::new("UQ_PARENT_ALT").unwrap()),
                        columns: vec![1].into_boxed_slice(),
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
        let (primary_key, alternate_key) = {
            let catalog = batch.catalog_reader().unwrap();
            (
                catalog
                    .find_constraint_by_name(&CatalogName::new("PK_PARENT").unwrap())
                    .unwrap()
                    .id(),
                catalog
                    .find_constraint_by_name(&CatalogName::new("UQ_PARENT_ALT").unwrap())
                    .unwrap()
                    .id(),
            )
        };
        create_constrained_relation(
            &mut batch,
            &ConstrainedTableSpec::new(
                table_spec("LOW_CHILD", &[("PARENT_ID", ColumnType::Int64, true)]),
                vec![TableConstraintSpec::ForeignKey {
                    name: Some(CatalogName::new("FK_LOWEST_CHILD_PARENT").unwrap()),
                    columns: vec![0].into_boxed_slice(),
                    parent_table_id: parent,
                    parent_constraint_id: primary_key,
                }],
            )
            .unwrap(),
        )
        .unwrap();
        create_constrained_relation(
            &mut batch,
            &ConstrainedTableSpec::new(
                table_spec("HIGH_CHILD", &[("PARENT_ID", ColumnType::Int64, true)]),
                vec![TableConstraintSpec::ForeignKey {
                    name: Some(CatalogName::new("FK_HIGHER_CHILD_PARENT").unwrap()),
                    columns: vec![0].into_boxed_slice(),
                    parent_table_id: parent,
                    parent_constraint_id: alternate_key,
                }],
            )
            .unwrap(),
        )
        .unwrap();

        let before = batch.checkpoint_mutation().unwrap();
        let error = drop_relation(&mut batch, parent).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Constraint);
        assert!(error.to_string().contains("FK_LOWEST_CHILD_PARENT"));
        before.assert_matches_batch_for_test(&batch);
        batch
            .catalog_reader()
            .unwrap()
            .validate_deep_batch(&mut batch)
            .unwrap();
    }

    #[test]
    fn drop_child_removes_outgoing_edges_bumps_parents_and_rolls_back_every_fault() {
        // Production breaks caught: leaving an outgoing edge behind, bumping a
        // parent once per FK instead of once per table, or leaking any staged
        // catalog/page/version/release state after a late failure.
        let (_temp, pager) = test_pager(4096).unwrap();
        let (first_parent, first_key, second_parent, second_key, child) =
            foreign_key_drop_fixture(&pager);
        let mut batch = pager.try_begin_write().unwrap();
        let original_versions = {
            let catalog = batch.catalog_reader().unwrap();
            (
                catalog
                    .find_table_by_id(first_parent)
                    .unwrap()
                    .schema_version(),
                catalog
                    .find_table_by_id(second_parent)
                    .unwrap()
                    .schema_version(),
            )
        };
        let points = [
            (ConstrainedDropFaultPoint::ReferencedByRemoval, 0),
            (ConstrainedDropFaultPoint::ReferencedByRemoval, 1),
            (ConstrainedDropFaultPoint::ReferencedByRemoval, 2),
            (ConstrainedDropFaultPoint::ConstraintRemoval, 0),
            (ConstrainedDropFaultPoint::ConstraintRemoval, 1),
            (ConstrainedDropFaultPoint::ConstraintRemoval, 2),
            (ConstrainedDropFaultPoint::ParentVersionBump, 0),
            (ConstrainedDropFaultPoint::ParentVersionBump, 1),
            (ConstrainedDropFaultPoint::TableRelease, 0),
            (ConstrainedDropFaultPoint::CatalogFinished, 0),
        ];
        for (point, occurrence) in points {
            let before = batch.checkpoint_mutation().unwrap();
            let _fault = fail_constrained_drop_at_for_test(point, occurrence);
            let error = drop_relation(&mut batch, child).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Io, "{point:?}[{occurrence}]");
            before.assert_matches_batch_for_test(&batch);
            let catalog = batch.catalog_reader().unwrap();
            assert_eq!(catalog.foreign_keys_referencing(first_key).count(), 2);
            assert_eq!(catalog.foreign_keys_referencing(second_key).count(), 1);
            assert_eq!(
                catalog
                    .find_table_by_id(first_parent)
                    .unwrap()
                    .schema_version(),
                original_versions.0
            );
            assert_eq!(
                catalog
                    .find_table_by_id(second_parent)
                    .unwrap()
                    .schema_version(),
                original_versions.1
            );
            catalog.validate_deep_batch(&mut batch).unwrap();
        }

        drop_relation(&mut batch, child).unwrap();
        let catalog = batch.catalog_reader().unwrap();
        assert_eq!(catalog.foreign_keys_referencing(first_key).count(), 0);
        assert_eq!(catalog.foreign_keys_referencing(second_key).count(), 0);
        assert_eq!(
            catalog
                .find_table_by_id(first_parent)
                .unwrap()
                .schema_version()
                .get(),
            original_versions.0.get() + 1
        );
        assert_eq!(
            catalog
                .find_table_by_id(second_parent)
                .unwrap()
                .schema_version()
                .get(),
            original_versions.1.get() + 1
        );
        drop(catalog);
        drop_relation(&mut batch, first_parent).unwrap();
        drop_relation(&mut batch, second_parent).unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn drop_child_invalidates_prepared_parent_dml() {
        // Production break caught: removing an FK changes the parent's write
        // contract, so an already prepared parent mutation must not execute.
        let (temp, pager) = test_pager(4096).unwrap();
        let (_parent, _key, _second_parent, _second_key, _child) = foreign_key_drop_fixture(&pager);
        drop(pager);

        let database = Database::open(temp.path(), Options::default()).unwrap();
        let mut parent_connection = database.connect().unwrap();
        let update = parent_connection
            .prepare("UPDATE FIRST_PARENT SET ID = ID + 1 WHERE ID = 1")
            .unwrap();
        let mut drop_connection = database.connect().unwrap();
        let drop_child = drop_connection.prepare("DROP TABLE MULTI_CHILD").unwrap();
        drop_connection
            .execute(&drop_child, Parameters::none())
            .unwrap();
        assert_eq!(
            parent_connection
                .execute(&update, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );

        let mut snapshot = database.connect().unwrap();
        assert_eq!(
            snapshot
                .prepare("DROP TABLE MULTI_CHILD")
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn drop_constrained_table_releases_all_local_roots_and_records() {
        // Production break caught: treating every constraint-owned index like
        // an independently dropped index rejects the table drop and leaves the
        // complete local constraint graph live.
        use crate::storage::validate::{validate_freelist, validate_tree};

        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let table = create_constrained_relation(&mut batch, &constrained_spec()).unwrap();
        let explicit_columns = batch
            .catalog_reader()
            .unwrap()
            .columns_for_table(table)
            .take(1)
            .cloned()
            .collect::<Vec<_>>();
        let explicit_index = create_index(
            &mut batch,
            &IndexSpec::new(
                CatalogName::new("IX_CONSTRAINED_ID").unwrap(),
                table,
                explicit_columns,
                false,
            )
            .unwrap(),
            u64::MAX,
        )
        .unwrap();
        let (table_root, index_records, constraint_records, next_object_id) = {
            let catalog = batch.catalog_reader().unwrap();
            let table_definition = catalog.find_table_by_id(table).unwrap();
            (
                table_definition.root(),
                catalog
                    .indexes_for_table(table)
                    .map(|index| (index.id(), index.root(), index.name().clone()))
                    .collect::<Vec<_>>(),
                catalog
                    .constraints_for_table(table)
                    .map(|constraint| (constraint.id(), constraint.name().clone()))
                    .collect::<Vec<_>>(),
                catalog.next_object_id(),
            )
        };
        assert!(index_records.iter().any(|(id, _, _)| *id == explicit_index));
        let mut released_pages =
            validate_tree(&mut batch, &BTree::from_root(TreeKind::Table, table_root))
                .unwrap()
                .all_live_pages();
        for (_, root, _) in &index_records {
            released_pages.extend(
                validate_tree(&mut batch, &BTree::from_root(TreeKind::Index, *root))
                    .unwrap()
                    .all_live_pages(),
            );
        }
        batch.commit().unwrap();

        let mut batch = pager.try_begin_write().unwrap();
        drop_relation(&mut batch, table).unwrap();
        batch.commit().unwrap();

        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        catalog.validate_deep(&mut snapshot).unwrap();
        assert!(catalog.find_table_by_id(table).is_none());
        assert!(catalog.columns_for_table(table).next().is_none());
        assert!(catalog.indexes_for_table(table).next().is_none());
        assert!(catalog.constraints_for_table(table).next().is_none());
        for (id, _, name) in index_records {
            assert!(catalog.find_index_by_id(id).is_none());
            assert!(catalog.find_index_by_name(&name).is_none());
            assert!(catalog.columns_for_index(id).next().is_none());
        }
        for (id, name) in constraint_records {
            assert!(catalog.find_constraint_by_id(id).is_none());
            assert!(catalog.find_constraint_by_name(&name).is_none());
            assert!(catalog.columns_for_constraint(id).next().is_none());
        }
        assert_eq!(catalog.next_object_id(), next_object_id);
        assert_ne!(snapshot.feature_bits() & INDEX_CONSTRAINT_V1, 0);
        let free = validate_freelist(&mut snapshot, &Default::default()).unwrap();
        assert!(free.is_superset(&released_pages));
    }

    #[test]
    fn every_constrained_drop_failure_restores_records_roots_and_release_queue() {
        // Production break caught: a failure after any dependency teardown
        // stage must restore the exact statement candidate, including staged
        // releases and the catalog root selected by a completed writer.
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut setup = pager.try_begin_write().unwrap();
        let table = create_constrained_relation(&mut setup, &constrained_spec()).unwrap();
        let explicit_columns = setup
            .catalog_reader()
            .unwrap()
            .columns_for_table(table)
            .take(1)
            .cloned()
            .collect::<Vec<_>>();
        create_index(
            &mut setup,
            &IndexSpec::new(
                CatalogName::new("IX_DROP_FAULT").unwrap(),
                table,
                explicit_columns,
                false,
            )
            .unwrap(),
            u64::MAX,
        )
        .unwrap();
        setup.commit().unwrap();

        let mut batch = pager.try_begin_write().unwrap();
        let points = [
            (ConstrainedDropFaultPoint::ExplicitIndexRelease, 0),
            (ConstrainedDropFaultPoint::SystemIndexRelease, 0),
            (ConstrainedDropFaultPoint::SystemIndexRelease, 1),
            (ConstrainedDropFaultPoint::ConstraintRemoval, 0),
            (ConstrainedDropFaultPoint::ConstraintRemoval, 1),
            (ConstrainedDropFaultPoint::ConstraintRemoval, 2),
            (ConstrainedDropFaultPoint::ConstraintRemoval, 3),
            (ConstrainedDropFaultPoint::TableRelease, 0),
            (ConstrainedDropFaultPoint::CatalogFinished, 0),
        ];
        for (point, occurrence) in points {
            let before = batch.checkpoint_mutation().unwrap();
            let _fault = fail_constrained_drop_at_for_test(point, occurrence);
            let error = drop_relation(&mut batch, table).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Io, "{point:?}[{occurrence}]");
            before.assert_matches_batch_for_test(&batch);
            let catalog = batch.catalog_reader().unwrap();
            assert_eq!(catalog.indexes_for_table(table).count(), 3);
            assert_eq!(catalog.constraints_for_table(table).count(), 4);
            catalog.validate_deep_batch(&mut batch).unwrap();
        }
        for point in [
            ConstraintRemovalFaultPoint::Columns,
            ConstraintRemovalFaultPoint::TableMapping,
            ConstraintRemovalFaultPoint::NameMapping,
            ConstraintRemovalFaultPoint::Definition,
        ] {
            let before = batch.checkpoint_mutation().unwrap();
            let _fault = fail_constraint_removal_at_for_test(point);
            let error = drop_relation(&mut batch, table).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Io, "{point:?}");
            before.assert_matches_batch_for_test(&batch);
            let catalog = batch.catalog_reader().unwrap();
            assert_eq!(catalog.indexes_for_table(table).count(), 3);
            assert_eq!(catalog.constraints_for_table(table).count(), 4);
            catalog.validate_deep_batch(&mut batch).unwrap();
        }
        batch.rollback().unwrap();
    }

    #[test]
    fn generated_constraint_names_use_checked_collision_suffixes() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();

        create_constrained_relation(&mut batch, &explicit_name_spec("SYS_C9")).unwrap();
        create_constrained_relation(&mut batch, &explicit_name_spec("SYS_C9_1")).unwrap();
        let table =
            create_constrained_relation(&mut batch, &single_generated_constraint_spec()).unwrap();
        let catalog = batch.catalog_reader().unwrap();
        let constraint = catalog.constraints_for_table(table).next().unwrap();
        assert_eq!(constraint.id().get(), 9);
        assert_eq!(constraint.name().as_str(), "SYS_C9_2");
    }

    #[test]
    fn generated_constraint_name_skips_committed_base_and_persists_after_reopen() {
        let (temp, pager) = test_pager(4096).unwrap();
        {
            let mut batch = pager.try_begin_write().unwrap();
            create_constrained_relation(
                &mut batch,
                &named_not_null_spec("COMMITTED_OCCUPIER", "SYS_C6"),
            )
            .unwrap();
            batch.commit().unwrap();
        }
        drop(pager);

        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let (generated_id, index_id) = {
            let mut batch = pager.try_begin_write().unwrap();
            assert_eq!(batch.catalog_reader().unwrap().next_object_id().get(), 4);
            let table = create_constrained_relation(
                &mut batch,
                &generated_primary_key_spec("AFTER_COMMITTED_COLLISION"),
            )
            .unwrap();
            let (constraint_id, index_id) = {
                let catalog = batch.catalog_reader().unwrap();
                assert_eq!(
                    catalog
                        .find_constraint_by_name(&CatalogName::new("SYS_C6").unwrap())
                        .unwrap()
                        .id()
                        .get(),
                    3
                );
                let constraint = catalog.constraints_for_table(table).next().unwrap();
                assert_eq!(constraint.id().get(), 6);
                assert_eq!(constraint.name().as_str(), "SYS_C6_1");
                let index = catalog
                    .find_index_by_id(constraint.index_id().unwrap())
                    .unwrap();
                assert_eq!(index.name(), constraint.name());
                assert_eq!(
                    catalog
                        .find_index_by_name(&CatalogName::new("SYS_C6_1").unwrap())
                        .unwrap()
                        .id(),
                    index.id()
                );
                (constraint.id().get(), index.id().get())
            };
            batch.commit().unwrap();
            (constraint_id, index_id)
        };
        drop(pager);

        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let catalog = batch.catalog_reader().unwrap();
        assert_eq!(
            catalog
                .find_constraint_by_name(&CatalogName::new("SYS_C6_1").unwrap())
                .unwrap()
                .id()
                .get(),
            generated_id
        );
        assert_eq!(
            catalog
                .find_index_by_name(&CatalogName::new("SYS_C6_1").unwrap())
                .unwrap()
                .id()
                .get(),
            index_id
        );
        assert_ne!(generated_id, index_id);
    }

    #[test]
    fn generated_constraint_name_skips_same_create_explicit_name_and_reopens() {
        let (temp, pager) = test_pager(4096).unwrap();
        let (generated_id, index_id) = {
            let mut batch = pager.try_begin_write().unwrap();
            let table =
                create_constrained_relation(&mut batch, &same_create_collision_spec()).unwrap();
            let (constraint_id, index_id) = {
                let catalog = batch.catalog_reader().unwrap();
                assert_eq!(
                    catalog
                        .find_constraint_by_name(&CatalogName::new("SYS_C4").unwrap())
                        .unwrap()
                        .id()
                        .get(),
                    3
                );
                let constraint = catalog
                    .constraints_for_table(table)
                    .find(|constraint| constraint.kind() == ConstraintKind::PrimaryKey)
                    .unwrap();
                assert_eq!(constraint.id().get(), 4);
                assert_eq!(constraint.name().as_str(), "SYS_C4_1");
                let index = catalog
                    .find_index_by_id(constraint.index_id().unwrap())
                    .unwrap();
                assert_eq!(index.name(), constraint.name());
                assert_eq!(
                    catalog
                        .find_constraint_by_name(&CatalogName::new("SYS_C4_1").unwrap())
                        .unwrap()
                        .id(),
                    constraint.id()
                );
                assert_eq!(
                    catalog
                        .find_index_by_name(&CatalogName::new("SYS_C4_1").unwrap())
                        .unwrap()
                        .id(),
                    index.id()
                );
                (constraint.id().get(), index.id().get())
            };
            batch.commit().unwrap();
            (constraint_id, index_id)
        };
        drop(pager);

        let pager = Pager::open(
            temp.path(),
            OpenMode::ReadWrite,
            PagerConfig::test_default(4096),
        )
        .unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let catalog = batch.catalog_reader().unwrap();
        assert_eq!(
            catalog
                .find_constraint_by_name(&CatalogName::new("SYS_C4_1").unwrap())
                .unwrap()
                .id()
                .get(),
            generated_id
        );
        assert_eq!(
            catalog
                .find_index_by_name(&CatalogName::new("SYS_C4_1").unwrap())
                .unwrap()
                .id()
                .get(),
            index_id
        );
        assert_ne!(generated_id, index_id);
    }

    #[test]
    fn constrained_create_late_faults_restore_the_complete_candidate() {
        for point in [
            ConstrainedCreateFaultPoint::SystemIndexRootAllocated,
            ConstrainedCreateFaultPoint::ConstraintRecordFamilyInserted,
            ConstrainedCreateFaultPoint::FeatureEnabled,
            ConstrainedCreateFaultPoint::CatalogFinished,
        ] {
            let (_temp, pager) = test_pager(4096).unwrap();
            let mut batch = pager.try_begin_write().unwrap();
            let before = batch.checkpoint_mutation().unwrap();
            let before_features = batch.feature_bits();
            let before_catalog_root = batch.catalog_root();
            let before_next_object_id = batch.catalog_reader().unwrap().next_object_id().get();
            let table_name = CatalogName::new("FAULTED_CREATE").unwrap();
            let candidate_name = CatalogName::new("SYS_C3").unwrap();

            fail_constrained_create_at_for_test(point);
            let error = create_constrained_relation(
                &mut batch,
                &generated_primary_key_spec("FAULTED_CREATE"),
            )
            .unwrap_err();

            assert_eq!(error.code(), ErrorCode::Io, "{point:?}");
            let catalog_observations = take_constraint_bundle_fault_observations_for_test();
            match point {
                ConstrainedCreateFaultPoint::ConstraintRecordFamilyInserted => assert_eq!(
                    catalog_observations,
                    vec![(ConstraintBundleFaultPoint::RecordsInserted, false)]
                ),
                ConstrainedCreateFaultPoint::FeatureEnabled => assert_eq!(
                    catalog_observations,
                    vec![(ConstraintBundleFaultPoint::FeatureEnabled, true)]
                ),
                ConstrainedCreateFaultPoint::SystemIndexRootAllocated
                | ConstrainedCreateFaultPoint::ParentVersionBumped
                | ConstrainedCreateFaultPoint::CatalogFinished => {
                    assert!(catalog_observations.is_empty(), "{point:?}")
                }
            }
            before.assert_matches_batch_for_test(&batch);
            assert_eq!(batch.feature_bits(), before_features, "{point:?}");
            assert_eq!(batch.catalog_root(), before_catalog_root, "{point:?}");
            let catalog = batch.catalog_reader().unwrap();
            assert_eq!(
                catalog.next_object_id().get(),
                before_next_object_id,
                "{point:?}"
            );
            assert!(
                catalog.find_table_by_name(&table_name).is_none(),
                "{point:?}"
            );
            assert!(
                catalog.find_constraint_by_name(&candidate_name).is_none(),
                "{point:?}"
            );
            assert!(
                catalog.find_index_by_name(&candidate_name).is_none(),
                "{point:?}"
            );
        }
    }

    #[test]
    fn constrained_create_late_check_failure_restores_ids_pages_feature_and_catalog() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let template = CheckProgramTemplate::new(vec![
            CheckTemplateInstruction::Column {
                ordinal: 1,
                result: CheckType::Int64,
            },
            CheckTemplateInstruction::Literal {
                value: Value::Int64(0),
                result: CheckType::Int64,
            },
            CheckTemplateInstruction::Binary {
                op: CheckBinaryOp::GreaterThan,
                result: CheckType::Bool,
            },
        ])
        .unwrap();
        let spec = ConstrainedTableSpec::new(
            table_spec("LATE_CHECK", &[("ID", ColumnType::Int64, true)]),
            vec![TableConstraintSpec::Check {
                name: None,
                template: &template,
            }],
        )
        .unwrap();

        let before = batch.checkpoint_mutation().unwrap();
        assert_eq!(
            create_constrained_relation(&mut batch, &spec)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        before.assert_matches_batch_for_test(&batch);
        assert_eq!(batch.feature_bits() & INDEX_CONSTRAINT_V1, 0);
        assert!(
            batch
                .catalog_reader()
                .unwrap()
                .find_table_by_name(&CatalogName::new("LATE_CHECK").unwrap())
                .is_none()
        );
    }

    #[test]
    fn plain_create_does_not_enable_index_constraint_feature() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();

        create_relation(
            &mut batch,
            &table_spec("PLAIN", &[("ID", ColumnType::Int64, false)]),
        )
        .unwrap();

        assert_eq!(batch.feature_bits() & INDEX_CONSTRAINT_V1, 0);
    }

    #[test]
    fn constrained_writer_reuses_deleted_unique_key_without_schema_bump_and_deep_validates() {
        // Production breaks caught: leaving an old physical key behind after a
        // replacement/deletion blocks its reuse; publishing table/index roots
        // separately can also make a deep catalog validation fail.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("T", &[("A", ColumnType::Int64, false)]),
            &[vec![Value::Int64(1)]],
        );
        let mut batch = pager.try_begin_write().unwrap();
        let columns = batch
            .catalog_reader()
            .unwrap()
            .columns_for_table(table_id)
            .cloned()
            .collect();
        create_index(
            &mut batch,
            &IndexSpec::new(CatalogName::new("UX_T_A").unwrap(), table_id, columns, true).unwrap(),
            64 * 1024 * 1024,
        )
        .unwrap();
        batch.commit().unwrap();

        let mut batch = pager.try_begin_write().unwrap();
        let before_version = batch
            .catalog_reader()
            .unwrap()
            .find_table_by_id(table_id)
            .unwrap()
            .schema_version();
        let mut writer = ConstrainedTableWrite::open(&mut batch, table_id).unwrap();
        let mut memory = crate::sql::exec::WorkMemory::new(u64::MAX).unwrap();
        let row = writer
            .next_after(&mut batch, None, usize::MAX)
            .unwrap()
            .unwrap();
        writer
            .replace(
                &mut batch,
                row.rowid(),
                row.values(),
                &[Value::Int64(2)],
                usize::MAX,
                &mut memory,
                u64::MAX,
            )
            .unwrap();
        let row = writer
            .next_after(&mut batch, None, usize::MAX)
            .unwrap()
            .unwrap();
        assert!(
            writer
                .delete(&mut batch, row.rowid(), row.values(), u64::MAX)
                .unwrap()
        );
        writer
            .insert(&mut batch, &[Value::Int64(1)], usize::MAX, &mut memory)
            .unwrap();
        writer.finish(&mut batch).unwrap();
        let catalog = batch.catalog_reader().unwrap();
        assert_eq!(
            catalog.find_table_by_id(table_id).unwrap().schema_version(),
            before_version
        );
        catalog.validate_deep_batch(&mut batch).unwrap();
        batch.commit().unwrap();
        assert_eq!(
            read_relation(&pager, "T").unwrap().1,
            vec![(2, vec![Value::Int64(1)])]
        );
    }

    #[test]
    fn constrained_writer_reports_the_persisted_unique_constraint_name() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let spec = ConstrainedTableSpec::new(
            table_spec("T", &[("ID", ColumnType::Int64, true)]),
            vec![TableConstraintSpec::Unique {
                name: Some(CatalogName::new("UQ_T").unwrap()),
                columns: vec![0].into_boxed_slice(),
            }],
        )
        .unwrap();
        let table_id = create_constrained_relation(&mut batch, &spec).unwrap();
        let mut writer = ConstrainedTableWrite::open(&mut batch, table_id).unwrap();
        let mut memory = crate::sql::exec::WorkMemory::new(u64::MAX).unwrap();

        writer
            .insert(&mut batch, &[Value::Int64(1)], usize::MAX, &mut memory)
            .unwrap();
        let error = writer
            .insert(&mut batch, &[Value::Int64(1)], usize::MAX, &mut memory)
            .unwrap_err();

        assert_eq!(error.code(), ErrorCode::Constraint);
        assert!(error.to_string().contains("constraint UQ_T is violated"));
    }

    #[test]
    fn constrained_writer_reports_persisted_primary_not_null_and_check_names() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let positive = CheckProgramTemplate::new(vec![
            CheckTemplateInstruction::Column {
                ordinal: 2,
                result: CheckType::Int64,
            },
            CheckTemplateInstruction::Literal {
                value: Value::Int64(0),
                result: CheckType::Int64,
            },
            CheckTemplateInstruction::Binary {
                op: CheckBinaryOp::GreaterThan,
                result: CheckType::Bool,
            },
        ])
        .unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let spec = ConstrainedTableSpec::new(
            table_spec(
                "T",
                &[
                    ("ID", ColumnType::Int64, false),
                    ("U", ColumnType::Int64, true),
                    ("N", ColumnType::Int64, false),
                ],
            ),
            vec![
                TableConstraintSpec::PrimaryKey {
                    name: Some(CatalogName::new("PK_T").unwrap()),
                    columns: vec![0].into_boxed_slice(),
                },
                TableConstraintSpec::Unique {
                    name: Some(CatalogName::new("UQ_T").unwrap()),
                    columns: vec![1].into_boxed_slice(),
                },
                TableConstraintSpec::NotNull {
                    name: Some(CatalogName::new("NN_T").unwrap()),
                    column: 2,
                },
                TableConstraintSpec::Check {
                    name: Some(CatalogName::new("CK_T").unwrap()),
                    template: &positive,
                },
            ],
        )
        .unwrap();
        let table_id = create_constrained_relation(&mut batch, &spec).unwrap();
        let mut writer = ConstrainedTableWrite::open(&mut batch, table_id).unwrap();
        let mut memory = crate::sql::exec::WorkMemory::new(u64::MAX).unwrap();

        writer
            .insert(
                &mut batch,
                &[Value::Int64(1), Value::Int64(1), Value::Int64(1)],
                usize::MAX,
                &mut memory,
            )
            .unwrap();
        for (values, name) in [
            (vec![Value::Null, Value::Int64(2), Value::Int64(1)], "PK_T"),
            (
                vec![Value::Int64(2), Value::Int64(1), Value::Int64(1)],
                "UQ_T",
            ),
            (vec![Value::Int64(2), Value::Int64(2), Value::Null], "NN_T"),
            (
                vec![Value::Int64(2), Value::Int64(2), Value::Int64(0)],
                "CK_T",
            ),
        ] {
            let error = writer
                .insert(&mut batch, &values, usize::MAX, &mut memory)
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Constraint);
            assert!(
                error
                    .to_string()
                    .contains(&format!("constraint {name} is violated")),
                "{name} returned {error}"
            );
        }
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn constrained_update_excludes_only_its_rowid_and_keeps_the_system_index_consistent() {
        let (_temp, pager) = test_pager(4096).unwrap();
        let mut batch = pager.try_begin_write().unwrap();
        let spec = ConstrainedTableSpec::new(
            table_spec("T", &[("ID", ColumnType::Int64, false)]),
            vec![TableConstraintSpec::PrimaryKey {
                name: Some(CatalogName::new("PK_T").unwrap()),
                columns: vec![0].into_boxed_slice(),
            }],
        )
        .unwrap();
        let table_id = create_constrained_relation(&mut batch, &spec).unwrap();
        let mut writer = ConstrainedTableWrite::open(&mut batch, table_id).unwrap();
        let mut memory = crate::sql::exec::WorkMemory::new(u64::MAX).unwrap();
        let first = writer
            .insert(&mut batch, &[Value::Int64(1)], usize::MAX, &mut memory)
            .unwrap();
        writer
            .insert(&mut batch, &[Value::Int64(2)], usize::MAX, &mut memory)
            .unwrap();

        let error = writer
            .replace(
                &mut batch,
                first,
                &[Value::Int64(1)],
                &[Value::Int64(2)],
                usize::MAX,
                &mut memory,
                u64::MAX,
            )
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Constraint);
        assert!(error.to_string().contains("constraint PK_T is violated"));
        writer
            .replace(
                &mut batch,
                first,
                &[Value::Int64(1)],
                &[Value::Int64(1)],
                usize::MAX,
                &mut memory,
                u64::MAX,
            )
            .unwrap();
        writer.finish(&mut batch).unwrap();
        batch
            .catalog_reader()
            .unwrap()
            .validate_deep_batch(&mut batch)
            .unwrap();
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn constrained_insert_key_limit_preserves_catalog_table_index_roots_and_rowid() {
        // Acceptance enhancement: the existing preflight implementation already
        // rejects this key before mutation. These direct snapshots guard each
        // individual root/counter against a future early allocation or split.
        let (_temp, pager) = test_pager(4096).unwrap();
        let table_id = committed_relation(
            &pager,
            table_spec("T", &[("A", ColumnType::Text, false)]),
            &[],
        );
        let mut batch = pager.try_begin_write().unwrap();
        let columns = batch
            .catalog_reader()
            .unwrap()
            .columns_for_table(table_id)
            .cloned()
            .collect();
        create_index(
            &mut batch,
            &IndexSpec::new(
                CatalogName::new("IX_T_A").unwrap(),
                table_id,
                columns,
                false,
            )
            .unwrap(),
            64 * 1024 * 1024,
        )
        .unwrap();
        batch.commit().unwrap();

        let mut batch = pager.try_begin_write().unwrap();
        let before = {
            let catalog = batch.catalog_reader().unwrap();
            let table = catalog.find_table_by_id(table_id).unwrap();
            (
                batch.catalog_root(),
                table.next_rowid(),
                table.root(),
                catalog
                    .indexes_for_table(table_id)
                    .map(|index| (index.id(), index.root()))
                    .collect::<Vec<_>>(),
            )
        };
        let mut writer = ConstrainedTableWrite::open(&mut batch, table_id).unwrap();
        let mut memory = crate::sql::exec::WorkMemory::new(u64::MAX).unwrap();
        assert_eq!(
            writer
                .insert(
                    &mut batch,
                    &[Value::Text("x".repeat(1014))],
                    usize::MAX,
                    &mut memory,
                )
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        let after = {
            let features = batch.feature_bits();
            let tree = BTree::from_root(TreeKind::Table, writer.catalog.root());
            let catalog = CatalogReader::open_candidate(&mut batch, &tree, features).unwrap();
            let table = catalog.find_table_by_id(table_id).unwrap();
            (
                batch.catalog_root(),
                table.next_rowid(),
                table.root(),
                catalog
                    .indexes_for_table(table_id)
                    .map(|index| (index.id(), index.root()))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(after, before);

        assert_eq!(
            writer
                .insert(
                    &mut batch,
                    &[Value::Text("after".into())],
                    usize::MAX,
                    &mut memory,
                )
                .unwrap()
                .get(),
            1
        );
        writer.finish(&mut batch).unwrap();
        batch.commit().unwrap();
    }

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

    fn foreign_key_drop_fixture(
        pager: &Pager,
    ) -> (TableId, ConstraintId, TableId, ConstraintId, TableId) {
        let mut batch = pager.try_begin_write().unwrap();
        let first_parent = create_constrained_relation(
            &mut batch,
            &ConstrainedTableSpec::new(
                table_spec("FIRST_PARENT", &[("ID", ColumnType::Int64, false)]),
                vec![TableConstraintSpec::PrimaryKey {
                    name: Some(CatalogName::new("PK_FIRST_PARENT").unwrap()),
                    columns: vec![0].into_boxed_slice(),
                }],
            )
            .unwrap(),
        )
        .unwrap();
        let second_parent = create_constrained_relation(
            &mut batch,
            &ConstrainedTableSpec::new(
                table_spec("SECOND_PARENT", &[("ID", ColumnType::Int64, false)]),
                vec![TableConstraintSpec::PrimaryKey {
                    name: Some(CatalogName::new("PK_SECOND_PARENT").unwrap()),
                    columns: vec![0].into_boxed_slice(),
                }],
            )
            .unwrap(),
        )
        .unwrap();
        let (first_key, second_key) = {
            let catalog = batch.catalog_reader().unwrap();
            (
                catalog
                    .find_constraint_by_name(&CatalogName::new("PK_FIRST_PARENT").unwrap())
                    .unwrap()
                    .id(),
                catalog
                    .find_constraint_by_name(&CatalogName::new("PK_SECOND_PARENT").unwrap())
                    .unwrap()
                    .id(),
            )
        };
        let child = create_constrained_relation(
            &mut batch,
            &ConstrainedTableSpec::new(
                table_spec(
                    "MULTI_CHILD",
                    &[
                        ("FIRST_A", ColumnType::Int64, true),
                        ("FIRST_B", ColumnType::Int64, true),
                        ("SECOND", ColumnType::Int64, true),
                    ],
                ),
                vec![
                    TableConstraintSpec::ForeignKey {
                        name: Some(CatalogName::new("FK_FIRST_A").unwrap()),
                        columns: vec![0].into_boxed_slice(),
                        parent_table_id: first_parent,
                        parent_constraint_id: first_key,
                    },
                    TableConstraintSpec::ForeignKey {
                        name: Some(CatalogName::new("FK_FIRST_B").unwrap()),
                        columns: vec![1].into_boxed_slice(),
                        parent_table_id: first_parent,
                        parent_constraint_id: first_key,
                    },
                    TableConstraintSpec::ForeignKey {
                        name: Some(CatalogName::new("FK_SECOND").unwrap()),
                        columns: vec![2].into_boxed_slice(),
                        parent_table_id: second_parent,
                        parent_constraint_id: second_key,
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
        batch.commit().unwrap();
        (first_parent, first_key, second_parent, second_key, child)
    }

    fn constrained_spec() -> ConstrainedTableSpec<'static> {
        ConstrainedTableSpec::new(
            table_spec(
                "CONSTRAINED",
                &[
                    ("ID", ColumnType::Int64, false),
                    ("EMAIL", ColumnType::Text, true),
                    ("REQUIRED", ColumnType::Text, false),
                ],
            ),
            vec![
                TableConstraintSpec::PrimaryKey {
                    name: None,
                    columns: vec![0].into_boxed_slice(),
                },
                TableConstraintSpec::Unique {
                    name: None,
                    columns: vec![1].into_boxed_slice(),
                },
                TableConstraintSpec::NotNull {
                    name: None,
                    column: 2,
                },
                TableConstraintSpec::NotNull {
                    name: None,
                    column: 0,
                },
            ],
        )
        .unwrap()
    }

    fn explicit_name_spec(name: &str) -> ConstrainedTableSpec<'static> {
        named_not_null_spec(name, name)
    }

    fn named_not_null_spec(
        table_name: &str,
        constraint_name: &str,
    ) -> ConstrainedTableSpec<'static> {
        ConstrainedTableSpec::new(
            table_spec(table_name, &[("ID", ColumnType::Int64, false)]),
            vec![TableConstraintSpec::NotNull {
                name: Some(CatalogName::new(constraint_name).unwrap()),
                column: 0,
            }],
        )
        .unwrap()
    }

    fn generated_primary_key_spec(table_name: &str) -> ConstrainedTableSpec<'static> {
        ConstrainedTableSpec::new(
            table_spec(table_name, &[("ID", ColumnType::Int64, false)]),
            vec![TableConstraintSpec::PrimaryKey {
                name: None,
                columns: vec![0].into_boxed_slice(),
            }],
        )
        .unwrap()
    }

    fn same_create_collision_spec() -> ConstrainedTableSpec<'static> {
        ConstrainedTableSpec::new(
            table_spec("SAME_CREATE_COLLISION", &[("ID", ColumnType::Int64, false)]),
            vec![
                TableConstraintSpec::NotNull {
                    name: Some(CatalogName::new("SYS_C4").unwrap()),
                    column: 0,
                },
                TableConstraintSpec::PrimaryKey {
                    name: None,
                    columns: vec![0].into_boxed_slice(),
                },
            ],
        )
        .unwrap()
    }

    fn single_generated_constraint_spec() -> ConstrainedTableSpec<'static> {
        ConstrainedTableSpec::new(
            table_spec("GENERATED", &[("ID", ColumnType::Int64, false)]),
            vec![TableConstraintSpec::NotNull {
                name: None,
                column: 0,
            }],
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
