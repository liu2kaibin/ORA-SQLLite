use std::cmp::Ordering;

use crate::{DbError, Decimal, ErrorCode, Result, Value};

use super::super::{
    WriteBatch,
    catalog::{
        CatalogReader,
        types::{
            CatalogName, ColumnDef, ColumnType, ConstraintDef, ConstraintId, ConstraintKind,
            IndexId, IndexOwnership, TableId,
        },
    },
    index::{IndexStorage, key::IndexKeyCodec},
    relation::Relation,
};

pub(crate) struct ForeignKeySpec {
    name: CatalogName,
    child_table_id: TableId,
    child_columns: Box<[ColumnDef]>,
    parent_table_id: TableId,
    parent_constraint_id: ConstraintId,
    parent_columns: Box<[ColumnDef]>,
    parent_index_id: IndexId,
    child_probe_index_id: Option<IndexId>,
}

pub(crate) struct ForeignKeyCatalog {
    outgoing: Box<[ForeignKeySpec]>,
    incoming: Box<[IncomingForeignKey]>,
    parent_indexes: Box<[IndexStorage]>,
}

struct IncomingForeignKey {
    spec: ForeignKeySpec,
    child_relation: Relation,
    child_probe_index: Option<IndexStorage>,
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
        let incoming = incoming
            .into_iter()
            .map(|(_, spec)| {
                let child_relation = Relation::open(catalog, spec.child_table_id)?;
                let child_probe_index = spec
                    .child_probe_index_id
                    .map(|index_id| IndexStorage::open(catalog, index_id))
                    .transpose()?;
                Ok(IncomingForeignKey {
                    spec,
                    child_relation,
                    child_probe_index,
                })
            })
            .collect::<Result<Vec<_>>>()?
            .into_boxed_slice();
        Ok(Self {
            outgoing,
            incoming,
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

    pub(crate) fn validate_parent_update(
        &self,
        batch: &mut WriteBatch,
        old_parent_row: &[Value],
        new_parent_row: &[Value],
        work_bytes: u64,
    ) -> Result<()> {
        for incoming in &self.incoming {
            let old_key = row_key_values(&incoming.spec.parent_columns, old_parent_row)?;
            let new_key = row_key_values(&incoming.spec.parent_columns, new_parent_row)?;
            let old_logical =
                IndexKeyCodec::encode_prefix(&incoming.spec.parent_columns, &old_key)?;
            let new_logical =
                IndexKeyCodec::encode_prefix(&incoming.spec.parent_columns, &new_key)?;
            if old_logical == new_logical {
                continue;
            }
            validate_parent_key(incoming, batch, &old_key, work_bytes)?;
        }
        Ok(())
    }

    pub(crate) fn validate_parent_delete(
        &self,
        batch: &mut WriteBatch,
        old_parent_row: &[Value],
        work_bytes: u64,
    ) -> Result<()> {
        for incoming in &self.incoming {
            let old_key = row_key_values(&incoming.spec.parent_columns, old_parent_row)?;
            validate_parent_key(incoming, batch, &old_key, work_bytes)?;
        }
        Ok(())
    }
}

fn validate_parent_key(
    incoming: &IncomingForeignKey,
    batch: &mut WriteBatch,
    parent_key: &[Value],
    work_bytes: u64,
) -> Result<()> {
    if child_table_contains_key(incoming, batch, parent_key, work_bytes)? {
        return Err(DbError::new(
            ErrorCode::Constraint,
            format!("constraint {} is violated", incoming.spec.name.as_str()),
        ));
    }
    Ok(())
}

fn child_table_contains_key(
    incoming: &IncomingForeignKey,
    batch: &mut WriteBatch,
    parent_key: &[Value],
    work_bytes: u64,
) -> Result<bool> {
    if parent_key.iter().any(|value| matches!(value, Value::Null)) {
        return Ok(false);
    }
    if let Some(index) = &incoming.child_probe_index {
        let Some(child_key) = child_probe_values(&incoming.spec, parent_key)? else {
            return Ok(false);
        };
        return index.contains_prefix(batch, incoming.spec.child_columns.len(), &child_key);
    }
    let mut scan = incoming.child_relation.scan_with_work_budget(work_bytes)?;
    while let Some(row) = scan.next_batch(batch)? {
        if child_row_references_parent(&incoming.spec, row.values(), parent_key)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn child_probe_values(spec: &ForeignKeySpec, parent_key: &[Value]) -> Result<Option<Box<[Value]>>> {
    if parent_key.len() != spec.parent_columns.len() {
        return Err(corruption("foreign-key parent probe arity diverged"));
    }
    let mut values = Vec::with_capacity(parent_key.len());
    for ((child, parent), value) in spec
        .child_columns
        .iter()
        .zip(&spec.parent_columns)
        .zip(parent_key)
    {
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        let value = match (child.column_type(), parent.column_type(), value) {
            (ColumnType::Int64, ColumnType::Decimal, Value::Decimal(value)) => {
                let Some(value) = decimal_to_i64_exact(*value) else {
                    return Ok(None);
                };
                Value::Int64(value)
            }
            (child_type, parent_type, value) if child_type == parent_type => value.clone(),
            _ => return Err(corruption("validated foreign-key probe types diverged")),
        };
        values.push(value);
    }
    Ok(Some(values.into_boxed_slice()))
}

fn child_row_references_parent(
    spec: &ForeignKeySpec,
    child_row: &[Value],
    parent_key: &[Value],
) -> Result<bool> {
    if parent_key.len() != spec.parent_columns.len() {
        return Err(corruption("foreign-key parent fallback arity diverged"));
    }
    for ((child, parent), parent_value) in spec
        .child_columns
        .iter()
        .zip(&spec.parent_columns)
        .zip(parent_key)
    {
        let child_value = child_row.get(usize::from(child.ordinal())).ok_or_else(|| {
            DbError::new(ErrorCode::Type, "foreign-key row is missing a local column")
        })?;
        if matches!(child_value, Value::Null) || matches!(parent_value, Value::Null) {
            return Ok(false);
        }
        let equal = match (child.column_type(), parent.column_type(), child_value) {
            (ColumnType::Int64, ColumnType::Decimal, Value::Int64(value)) => {
                let normalized = Value::Decimal(crate::scalar::int_decimal(*value)?);
                crate::scalar::sql_total_cmp(&normalized, parent_value)? == Ordering::Equal
            }
            (child_type, parent_type, child_value) if child_type == parent_type => {
                crate::scalar::sql_total_cmp(child_value, parent_value)? == Ordering::Equal
            }
            _ => return Err(corruption("validated foreign-key fallback types diverged")),
        };
        if !equal {
            return Ok(false);
        }
    }
    Ok(true)
}

fn row_key_values(columns: &[ColumnDef], row: &[Value]) -> Result<Box<[Value]>> {
    columns
        .iter()
        .map(|column| {
            row.get(usize::from(column.ordinal()))
                .cloned()
                .ok_or_else(|| DbError::new(ErrorCode::Type, "row is missing a key column"))
        })
        .collect::<Result<Vec<_>>>()
        .map(Vec::into_boxed_slice)
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
        .filter(|index| {
            usize::from(index.column_count()) >= child_columns.len()
                && catalog
                    .columns_for_index(index.id())
                    .take(child_columns.len())
                    .map(ColumnDef::id)
                    .eq(child_columns.iter().map(ColumnDef::id))
        })
        .min_by_key(|index| index.id())
        .map(|index| index.id());
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

fn decimal_to_i64_exact(value: Decimal) -> Option<i64> {
    let scale = i16::from(value.scale());
    let magnitude = if scale >= 0 {
        let divisor = 10_u128.checked_pow(u32::try_from(scale).ok()?)?;
        if value.magnitude().checked_rem(divisor)? != 0 {
            return None;
        }
        value.magnitude().checked_div(divisor)?
    } else {
        let multiplier = 10_u128.checked_pow(u32::try_from(-scale).ok()?)?;
        value.magnitude().checked_mul(multiplier)?
    };
    let negative_limit = u128::from(i64::MAX as u64) + 1;
    let limit = if value.negative() {
        negative_limit
    } else {
        u128::from(i64::MAX as u64)
    };
    if magnitude > limit {
        return None;
    }
    if value.negative() {
        if magnitude == negative_limit {
            Some(i64::MIN)
        } else {
            i64::try_from(magnitude).ok()?.checked_neg()
        }
    } else {
        i64::try_from(magnitude).ok()
    }
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::decimal_to_i64_exact;
    use crate::Decimal;

    fn decimal(negative: bool, scale: i8, magnitude: u128) -> Decimal {
        Decimal::new(negative, scale, magnitude).unwrap()
    }

    #[test]
    fn decimal_parent_to_int64_probe_is_exact_checked_and_handles_min_boundary() {
        // Production breaks caught: truncating a fraction, overflowing scale
        // multiplication, or negating the i64::MIN magnitude can produce a
        // false child-index match or panic instead of an impossible probe.
        for (value, expected) in [
            (decimal(false, 1, 1_230), Some(123)),
            (decimal(false, -2, 123), Some(12_300)),
            (decimal(true, 0, 9_223_372_036_854_775_808), Some(i64::MIN)),
            (decimal(false, 0, 9_223_372_036_854_775_807), Some(i64::MAX)),
            (decimal(false, 1, 1_231), None),
            (decimal(false, 0, 9_223_372_036_854_775_808), None),
            (decimal(true, 0, 9_223_372_036_854_775_809), None),
            (decimal(false, -38, 1), None),
        ] {
            assert_eq!(decimal_to_i64_exact(value), expected, "{value:?}");
        }
    }
}
