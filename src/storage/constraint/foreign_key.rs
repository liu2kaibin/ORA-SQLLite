use crate::{DbError, ErrorCode, Result, Value};

use super::super::{
    WriteBatch,
    catalog::{
        CatalogReader,
        types::{
            CatalogName, ColumnDef, ColumnType, ConstraintDef, ConstraintId, ConstraintKind,
            IndexId, IndexOwnership, TableId,
        },
    },
    index::IndexStorage,
};

pub(crate) struct ForeignKeySpec {
    name: CatalogName,
    child_table_id: TableId,
    child_columns: Box<[ColumnDef]>,
    parent_table_id: TableId,
    parent_constraint_id: ConstraintId,
    parent_columns: Box<[ColumnDef]>,
    parent_index_id: IndexId,
    #[allow(dead_code)]
    child_probe_index_id: Option<IndexId>,
}

pub(crate) struct ForeignKeyCatalog {
    outgoing: Box<[ForeignKeySpec]>,
    #[allow(dead_code)]
    incoming: Box<[ForeignKeySpec]>,
    parent_indexes: Box<[IndexStorage]>,
}

impl ForeignKeyCatalog {
    pub(crate) fn open(catalog: &CatalogReader, table_id: TableId) -> Result<Self> {
        let mut outgoing = catalog
            .constraints_for_table(table_id)
            .filter(|constraint| constraint.kind() == ConstraintKind::ForeignKey)
            .map(|constraint| Ok((constraint.id(), build_spec(catalog, constraint)?)))
            .collect::<Result<Vec<_>>>()?;
        outgoing.sort_by_key(|(constraint_id, _)| *constraint_id);

        let mut incoming = Vec::new();
        for parent in catalog
            .constraints_for_table(table_id)
            .filter(|constraint| {
                matches!(
                    constraint.kind(),
                    ConstraintKind::PrimaryKey | ConstraintKind::Unique
                )
            })
        {
            for child in catalog.foreign_keys_referencing(parent.id()) {
                incoming.push((child.id(), build_spec(catalog, child)?));
            }
        }
        incoming.sort_by_key(|(constraint_id, _)| *constraint_id);

        let outgoing = outgoing
            .into_iter()
            .map(|(_, spec)| spec)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let parent_indexes = outgoing
            .iter()
            .map(|spec| open_parent_index(catalog, spec))
            .collect::<Result<Vec<_>>>()?
            .into_boxed_slice();
        Ok(Self {
            outgoing,
            incoming: incoming
                .into_iter()
                .map(|(_, spec)| spec)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            parent_indexes,
        })
    }

    pub(crate) fn validate_child_row(
        &self,
        batch: &mut WriteBatch,
        child_values: &[Value],
    ) -> Result<()> {
        for (spec, parent_index) in self.outgoing.iter().zip(&self.parent_indexes) {
            let Some(parent_values) = parent_key_values(spec, child_values)? else {
                continue;
            };
            if !parent_index.contains_key(batch, &parent_values)? {
                return Err(DbError::new(
                    ErrorCode::Constraint,
                    format!("constraint {} is violated", spec.name.as_str()),
                ));
            }
        }
        Ok(())
    }
}

fn open_parent_index(catalog: &CatalogReader, spec: &ForeignKeySpec) -> Result<IndexStorage> {
    let parent = catalog
        .find_constraint_by_id(spec.parent_constraint_id)
        .ok_or_else(|| corruption("foreign-key parent constraint disappeared at runtime"))?;
    let index = catalog
        .find_index_by_id(spec.parent_index_id)
        .ok_or_else(|| corruption("foreign-key parent system index disappeared at runtime"))?;
    if parent.table_id() != spec.parent_table_id
        || !matches!(
            parent.kind(),
            ConstraintKind::PrimaryKey | ConstraintKind::Unique
        )
        || parent.index_id() != Some(spec.parent_index_id)
        || index.table_id() != spec.parent_table_id
        || index.ownership() != IndexOwnership::Constraint(spec.parent_constraint_id)
        || !index.unique()
        || catalog
            .columns_for_constraint(spec.parent_constraint_id)
            .ne(spec.parent_columns.iter())
        || catalog
            .columns_for_index(spec.parent_index_id)
            .ne(spec.parent_columns.iter())
    {
        return Err(corruption(
            "validated foreign-key parent descriptor diverged at runtime",
        ));
    }
    IndexStorage::open(catalog, spec.parent_index_id)
        .map_err(|_| corruption("foreign-key parent index could not be reopened"))
}

fn build_spec(catalog: &CatalogReader, child: &ConstraintDef) -> Result<ForeignKeySpec> {
    if child.kind() != ConstraintKind::ForeignKey {
        return Err(corruption(
            "foreign-key runtime descriptor has a non-foreign-key child",
        ));
    }
    let target = child
        .foreign_key_target()
        .ok_or_else(|| corruption("foreign-key runtime descriptor has no parent target"))?;
    if child.table_id() == target.table_id() {
        return Err(corruption("foreign key cannot reference its own table"));
    }
    let parent = catalog
        .find_constraint_by_id(target.constraint_id())
        .ok_or_else(|| corruption("foreign key references a missing parent constraint"))?;
    if parent.table_id() != target.table_id()
        || !matches!(
            parent.kind(),
            ConstraintKind::PrimaryKey | ConstraintKind::Unique
        )
    {
        return Err(corruption(
            "foreign-key parent is not an owned primary-key or unique constraint",
        ));
    }
    let parent_index_id = parent
        .index_id()
        .ok_or_else(|| corruption("foreign-key parent has no system index"))?;
    let parent_index = catalog
        .find_index_by_id(parent_index_id)
        .ok_or_else(|| corruption("foreign-key parent system index is missing"))?;
    if parent_index.table_id() != parent.table_id()
        || parent_index.ownership() != IndexOwnership::Constraint(parent.id())
        || !parent_index.unique()
    {
        return Err(corruption(
            "foreign-key parent has no constraint-owned unique system index",
        ));
    }

    let child_columns = catalog
        .columns_for_constraint(child.id())
        .cloned()
        .collect::<Vec<_>>();
    let parent_columns = catalog
        .columns_for_constraint(parent.id())
        .cloned()
        .collect::<Vec<_>>();
    if child_columns.is_empty()
        || child_columns.len() != usize::from(child.column_count())
        || parent_columns.len() != usize::from(parent.column_count())
        || child_columns.len() != parent_columns.len()
        || child_columns
            .iter()
            .any(|column| column.table_id() != child.table_id())
        || parent_columns
            .iter()
            .any(|column| column.table_id() != parent.table_id())
    {
        return Err(corruption(
            "foreign-key child and parent column ownership or counts differ",
        ));
    }
    if catalog
        .columns_for_index(parent_index_id)
        .map(ColumnDef::id)
        .ne(parent_columns.iter().map(ColumnDef::id))
    {
        return Err(corruption(
            "foreign-key parent constraint and system index column order differs",
        ));
    }
    for (child_column, parent_column) in child_columns.iter().zip(&parent_columns) {
        if child_column.column_type() != parent_column.column_type()
            && !matches!(
                (child_column.column_type(), parent_column.column_type()),
                (ColumnType::Int64, ColumnType::Decimal)
            )
        {
            return Err(corruption("validated foreign-key column types diverged"));
        }
    }
    if !catalog
        .foreign_keys_referencing(parent.id())
        .any(|candidate| candidate.id() == child.id())
    {
        return Err(corruption(
            "foreign-key runtime descriptor has no reverse catalog edge",
        ));
    }

    let child_probe_index_id = catalog
        .indexes_for_table(child.table_id())
        .find_map(|index| {
            catalog
                .columns_for_index(index.id())
                .map(ColumnDef::id)
                .eq(child_columns.iter().map(ColumnDef::id))
                .then_some(index.id())
        });
    Ok(ForeignKeySpec {
        name: child.name().clone(),
        child_table_id: child.table_id(),
        child_columns: child_columns.into_boxed_slice(),
        parent_table_id: parent.table_id(),
        parent_constraint_id: parent.id(),
        parent_columns: parent_columns.into_boxed_slice(),
        parent_index_id,
        child_probe_index_id,
    })
}

fn parent_key_values(spec: &ForeignKeySpec, child_row: &[Value]) -> Result<Option<Box<[Value]>>> {
    let mut values = Vec::with_capacity(spec.child_columns.len());
    for (child, parent) in spec.child_columns.iter().zip(&spec.parent_columns) {
        if child.table_id() != spec.child_table_id {
            return Err(corruption(
                "validated foreign-key child descriptor diverged at runtime",
            ));
        }
        let value = child_row.get(usize::from(child.ordinal())).ok_or_else(|| {
            DbError::new(ErrorCode::Type, "foreign-key row is missing a local column")
        })?;
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        values.push(match (child.column_type(), parent.column_type(), value) {
            (ColumnType::Int64, ColumnType::Decimal, Value::Int64(value)) => {
                Value::Decimal(crate::scalar::int_decimal(*value)?)
            }
            (left, right, value) if left == right => value.clone(),
            _ => {
                return Err(corruption("validated foreign-key column types diverged"));
            }
        });
    }
    Ok(Some(values.into_boxed_slice()))
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}
