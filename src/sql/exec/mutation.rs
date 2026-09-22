use crate::{
    DbError, ErrorCode, ExecutionResult, Parameters, Result, Statement,
    sql::{
        SqlType,
        bound::SourceId,
        plan::{CreateTablePlan, DeletePlan, InsertPlan, PlanKind, UpdatePlan},
    },
    storage::{
        WriteBatch,
        catalog::types::ObjectKind,
        index::{IndexSpec, create_index, drop_index},
        relation::{
            ColumnSpec, ConstrainedTableWrite, Relation, RelationValueValidation, TableSpec,
            create_relation, drop_relation,
        },
    },
};

use super::{
    EvalRow, ParameterFrame, WorkMemory, eval,
    expr::int_decimal,
    memory::{
        MATERIALIZED_CONTAINER_BYTES, checked_add_bytes, checked_array_bytes,
        owned_value_payload_bytes, owned_values_bytes, resource_limit,
    },
};

/// A write plan whose API kind, parameters, and retained schema have all been checked.
pub(crate) struct ValidatedWrite<'a> {
    statement: &'a Statement,
    params: ParameterFrame,
    memory: WorkMemory,
}

pub(crate) fn validate_write_execution<'a>(
    batch: &mut WriteBatch,
    statement: &'a Statement,
    parameters: Parameters<'_>,
    memory_limit: u64,
) -> Result<ValidatedWrite<'a>> {
    reject_wrong_execute_api_kind(statement.plan().kind())?;
    let mut memory = WorkMemory::new(memory_limit)?;
    let params = ParameterFrame::bind(statement.parameters(), parameters, &mut memory)?;
    let catalog = batch.catalog_reader()?;
    statement.validate_schema(batch.database_uuid(), &catalog)?;
    if matches!(
        statement.plan().kind(),
        PlanKind::DeferredUnsupported { .. }
    ) {
        return Err(unsupported_4c2());
    }
    Ok(ValidatedWrite {
        statement,
        params,
        memory,
    })
}

pub(crate) fn execute_validated_write_plan(
    batch: &mut WriteBatch,
    validated: ValidatedWrite<'_>,
) -> Result<ExecutionResult> {
    let ValidatedWrite {
        statement,
        params,
        mut memory,
    } = validated;
    match statement.plan().kind() {
        PlanKind::Insert(plan) => execute_insert(batch, plan, &params, &memory),
        PlanKind::Update(plan) => execute_update(batch, plan, &params, &mut memory),
        PlanKind::Delete(plan) => execute_delete(batch, plan, &params, &mut memory),
        PlanKind::CreateTable(plan) => execute_create_table(batch, plan),
        PlanKind::CreateIndex(plan) => {
            // Recheck database identity, owner-table version, and the created
            // name next to mutation, even for an already validated write plan.
            let catalog = batch.catalog_reader()?;
            statement.validate_schema(batch.database_uuid(), &catalog)?;
            let spec = IndexSpec::new(
                plan.name().clone(),
                plan.table().table_id(),
                plan.columns()
                    .iter()
                    .map(|target| {
                        let column =
                            catalog
                                .find_column_by_id(target.column_id())
                                .ok_or_else(|| {
                                    DbError::new(
                                        ErrorCode::SchemaChanged,
                                        "prepared indexed column no longer exists",
                                    )
                                    .with_sql_location(target.location())
                                })?;
                        target.validate_catalog_column(column)?;
                        Ok(column.clone())
                    })
                    .collect::<Result<Vec<_>>>()?,
                plan.unique(),
            )?;
            // Lend the remaining query budget to the storage lifecycle. It
            // bounds simultaneous raw/decoded rows and key/probe scratch.
            let available = memory.remaining();
            memory.reserve(available)?;
            let result = create_index(batch, &spec, available);
            memory.release(available)?;
            result?;
            Ok(ExecutionResult::new(0))
        }
        PlanKind::DropIndex(plan) => {
            // Recheck retained database, table version and index owner identity
            // immediately before mutation, even after earlier validation.
            let catalog = batch.catalog_reader()?;
            statement.validate_schema(batch.database_uuid(), &catalog)?;
            drop_index(batch, plan.index_id())?;
            Ok(ExecutionResult::new(0))
        }
        PlanKind::DropTable { table_id } => {
            let catalog = batch.catalog_reader()?;
            statement.validate_schema(batch.database_uuid(), &catalog)?;
            drop_relation(batch, *table_id)?;
            Ok(ExecutionResult::new(0))
        }
        kind => Err(unsupported_write_kind(kind)),
    }
}

fn execute_update(
    batch: &mut WriteBatch,
    plan: &UpdatePlan,
    params: &ParameterFrame,
    memory: &mut WorkMemory,
) -> Result<ExecutionResult> {
    let table_id = plan.table().table_id();
    let relation = Relation::open_from_batch(batch, table_id)?;
    for assignment in plan.assignments() {
        if usize::from(assignment.target().ordinal()) >= relation.column_count() {
            return Err(DbError::new(
                ErrorCode::Internal,
                "UPDATE target ordinal is outside the table schema",
            ));
        }
    }
    let mut writer = ConstrainedTableWrite::open(batch, table_id)?;
    let mut last = None;
    let mut affected = 0_u64;
    while let Some(stored) = writer
        .next_after(
            batch,
            last,
            usize::try_from(memory.remaining()).unwrap_or(usize::MAX),
        )
        .map_err(map_row_memory_limit)?
    {
        let rowid = stored.rowid();
        last = Some(rowid);
        let old_values = stored.into_values();
        let old_bytes = owned_values_bytes(old_values.iter())?;
        memory.reserve(old_bytes)?;
        let old = EvalRow::with_source(SourceId(0), old_values, old_bytes)?;
        if !predicate_matches(plan.selection(), &old, params)? {
            memory.release(old_bytes)?;
            continue;
        }
        let (values, assignment_bytes) = evaluate_update_assignments(plan, &old, params, memory)?;
        memory.reserve(old_bytes)?;
        let mut replacement = Vec::with_capacity(relation.column_count());
        for ordinal in 0..relation.column_count() {
            let ordinal = u16::try_from(ordinal).map_err(|_| {
                DbError::new(
                    ErrorCode::Internal,
                    "UPDATE relation column ordinal exceeds its bound",
                )
            })?;
            replacement.push(
                old.source_value(SourceId(0), ordinal)
                    .cloned()
                    .ok_or_else(|| {
                        DbError::new(
                            ErrorCode::Internal,
                            "UPDATE old row is missing a relation column",
                        )
                    })?,
            );
        }
        for (assignment, value) in plan.assignments().iter().zip(values) {
            let target = replacement
                .get_mut(usize::from(assignment.target().ordinal()))
                .ok_or_else(|| {
                    DbError::new(
                        ErrorCode::Internal,
                        "UPDATE target ordinal is outside the replacement row",
                    )
                })?;
            *target = value;
        }
        validate_update_replacement(&relation, plan, &replacement, old_bytes, memory)?;
        // Validation must happen before allocating the third owned row so an
        // invalid assignment keeps its Type diagnostic. On the valid path,
        // retain the original row for index deletion and account for it before
        // cloning, while no B+Tree mutation has happened yet.
        memory.reserve(old_bytes)?;
        let old_for_index = (0..relation.column_count())
            .map(|ordinal| {
                let ordinal = u16::try_from(ordinal).map_err(|_| {
                    DbError::new(
                        ErrorCode::Internal,
                        "UPDATE relation column ordinal exceeds its bound",
                    )
                })?;
                old.source_value(SourceId(0), ordinal)
                    .cloned()
                    .ok_or_else(|| {
                        DbError::new(
                            ErrorCode::Internal,
                            "UPDATE old row is missing a relation column",
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        writer.replace(batch, rowid, &old_for_index, &replacement)?;
        affected = checked_affected_count(affected)?;
        memory.release(checked_add_bytes(
            checked_add_bytes(checked_add_bytes(old_bytes, assignment_bytes)?, old_bytes)?,
            old_bytes,
        )?)?;
    }
    writer.finish(batch)?;
    Ok(ExecutionResult::new(affected))
}

fn evaluate_update_assignments(
    plan: &UpdatePlan,
    old: &EvalRow,
    params: &ParameterFrame,
    memory: &mut WorkMemory,
) -> Result<(Vec<crate::Value>, u64)> {
    let count = u64::try_from(plan.assignments().len()).map_err(|_| resource_limit())?;
    let mut bytes = checked_add_bytes(
        MATERIALIZED_CONTAINER_BYTES,
        checked_array_bytes(count, std::mem::size_of::<crate::Value>() as u64)?,
    )?;
    memory.reserve(bytes)?;
    let mut values = Vec::with_capacity(plan.assignments().len());
    for assignment in plan.assignments() {
        let value = coerce_assignment(
            eval(assignment.value(), old, params)?,
            assignment.target().data_type(),
        )?;
        let payload = owned_value_payload_bytes(&value)?;
        memory.reserve(payload)?;
        bytes = checked_add_bytes(bytes, payload)?;
        values.push(value);
    }
    Ok((values, bytes))
}

fn execute_delete(
    batch: &mut WriteBatch,
    plan: &DeletePlan,
    params: &ParameterFrame,
    memory: &mut WorkMemory,
) -> Result<ExecutionResult> {
    let relation = Relation::open_from_batch(batch, plan.table().table_id())?;
    let mut writer = ConstrainedTableWrite::open(batch, plan.table().table_id())?;
    let mut last = None;
    let mut affected = 0_u64;
    while let Some(stored) = writer
        .next_after(
            batch,
            last,
            usize::try_from(memory.remaining()).unwrap_or(usize::MAX),
        )
        .map_err(map_row_memory_limit)?
    {
        let rowid = stored.rowid();
        last = Some(rowid);
        let old_values = stored.into_values();
        let old_bytes = owned_values_bytes(old_values.iter())?;
        memory.reserve(old_bytes)?;
        let old = EvalRow::with_source(SourceId(0), old_values, old_bytes)?;
        if predicate_matches(plan.selection(), &old, params)? {
            memory.reserve(old_bytes)?;
            let old_for_index = (0..relation.column_count())
                .map(|ordinal| {
                    let ordinal = u16::try_from(ordinal).map_err(|_| {
                        DbError::new(
                            ErrorCode::Internal,
                            "DELETE relation column ordinal exceeds its bound",
                        )
                    })?;
                    old.source_value(SourceId(0), ordinal)
                        .cloned()
                        .ok_or_else(|| {
                            DbError::new(
                                ErrorCode::Internal,
                                "DELETE old row is missing a relation column",
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            let deleted = writer.delete(batch, rowid, &old_for_index);
            memory.release(old_bytes)?;
            if deleted? {
                affected = checked_affected_count(affected)?;
            }
        }
        memory.release(old_bytes)?;
    }
    writer.finish(batch)?;
    Ok(ExecutionResult::new(affected))
}

fn map_row_memory_limit(error: DbError) -> DbError {
    if error.code() == ErrorCode::Type {
        resource_limit()
    } else {
        error
    }
}

fn validate_update_replacement(
    relation: &Relation,
    plan: &UpdatePlan,
    replacement: &[crate::Value],
    planned_old_clone_bytes: u64,
    memory: &mut WorkMemory,
) -> Result<()> {
    // Keep the old row clone in the statement budget before allocating it.
    // RowCodec checks value shapes before it classifies the bounded length, so
    // an invalid assignment preserves its Type diagnostic even when the valid
    // replacement could not coexist with that clone.
    let row_budget = memory.remaining().saturating_sub(planned_old_clone_bytes);
    let max_row_bytes = usize::try_from(row_budget).unwrap_or(usize::MAX);
    let validation = relation.validate_update_values(replacement, max_row_bytes);
    match validation {
        Ok(RelationValueValidation::Valid) => Ok(()),
        Ok(RelationValueValidation::ExceedsLimit) => Err(resource_limit()),
        Ok(RelationValueValidation::InvalidValue { ordinal, error }) => {
            let assignment = plan
                .assignments()
                .iter()
                .find(|assignment| assignment.target().ordinal() == ordinal)
                .ok_or_else(|| {
                    DbError::new(
                        ErrorCode::Internal,
                        "UPDATE replacement contains an invalid unassigned value",
                    )
                });
            match assignment {
                Ok(assignment) => Err(error.with_sql_location(assignment.value().location())),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn predicate_matches(
    predicate: Option<&crate::sql::plan::ExecExpr>,
    row: &EvalRow,
    params: &ParameterFrame,
) -> Result<bool> {
    let Some(predicate) = predicate else {
        return Ok(true);
    };
    predicate_value_matches(eval(predicate, row, params)?, predicate.location())
}

fn predicate_value_matches(
    value: crate::Value,
    location: crate::error::SqlLocation,
) -> Result<bool> {
    match value {
        crate::Value::Bool(value) => Ok(value),
        crate::Value::Null => Ok(false),
        _ => Err(
            DbError::new(ErrorCode::Type, "write predicate requires a boolean value")
                .with_sql_location(location),
        ),
    }
}

fn checked_affected_count(affected: u64) -> Result<u64> {
    affected.checked_add(1).ok_or_else(resource_limit)
}

fn coerce_assignment(value: crate::Value, target: SqlType) -> Result<crate::Value> {
    if target == SqlType::Decimal {
        if let crate::Value::Int64(integer) = value {
            return Ok(crate::Value::Decimal(int_decimal(integer)?));
        }
    }
    Ok(value)
}

fn execute_insert(
    batch: &mut WriteBatch,
    plan: &InsertPlan,
    params: &ParameterFrame,
    memory: &WorkMemory,
) -> Result<ExecutionResult> {
    if plan.targets().len() != plan.values().len() {
        return Err(DbError::new(
            ErrorCode::Internal,
            "INSERT target and value counts differ in the execution plan",
        ));
    }
    let table_id = plan.table().table_id();
    let relation = Relation::open_from_batch(batch, table_id)?;
    let mut values = vec![crate::Value::Null; relation.column_count()];
    let empty_row = EvalRow::default();
    for (target, expression) in plan.targets().iter().zip(plan.values()) {
        let value = coerce_assignment(eval(expression, &empty_row, params)?, target.data_type())?;
        let slot = values
            .get_mut(usize::from(target.ordinal()))
            .ok_or_else(|| {
                DbError::new(
                    ErrorCode::Internal,
                    "INSERT target ordinal is outside the table schema",
                )
            })?;
        *slot = value;
    }
    let max_row_bytes = usize::try_from(memory.remaining()).unwrap_or(usize::MAX);
    relation.validate_values(&values, max_row_bytes)?;
    let mut writer = ConstrainedTableWrite::open(batch, table_id)?;
    writer.insert(batch, &values)?;
    writer.finish(batch)?;
    Ok(ExecutionResult::new(1))
}

fn execute_create_table(batch: &mut WriteBatch, plan: &CreateTablePlan) -> Result<ExecutionResult> {
    // Keep this check adjacent to the mutation facade. Validation already made
    // the same assertion before the outer statement checkpoint, but execution
    // must never turn a retained CREATE into a different catalog operation.
    if batch
        .catalog_reader()?
        .object_name_exists(ObjectKind::Table, plan.name())
    {
        return Err(DbError::new(
            ErrorCode::SchemaChanged,
            "prepared CREATE name is no longer available",
        ));
    }
    let columns = plan
        .columns()
        .iter()
        .map(|column| {
            ColumnSpec::new(
                column.name().clone(),
                column.column_type(),
                column.nullable(),
            )
        })
        .collect();
    let spec = TableSpec::new(plan.name().clone(), columns)?;
    create_relation(batch, &spec)?;
    Ok(ExecutionResult::new(0))
}

fn reject_wrong_execute_api_kind(kind: &PlanKind) -> Result<()> {
    match kind {
        PlanKind::Query(_) => Err(DbError::new(
            ErrorCode::Type,
            "execute requires a non-row-producing statement",
        )),
        PlanKind::Begin | PlanKind::Commit | PlanKind::Rollback => Err(DbError::new(
            ErrorCode::Transaction,
            "SQL transaction control cannot execute inside a Rust transaction",
        )),
        _ => Ok(()),
    }
}

fn unsupported_4c2() -> DbError {
    DbError::new(
        ErrorCode::Unsupported,
        "statement execution requires Phase 4C2",
    )
}

fn unsupported_write_kind(_kind: &PlanKind) -> DbError {
    DbError::new(
        ErrorCode::Unsupported,
        "write statement execution is not implemented",
    )
}

#[cfg(test)]
mod tests {
    use super::{
        checked_affected_count, execute_validated_write_plan, predicate_value_matches,
        validate_write_execution,
    };
    use crate::error::SqlLocation;
    use crate::sql::bind_sql;
    use crate::storage::relation::Relation;
    use crate::storage::test_support::phase_4b_catalog_fixture;
    use crate::{ErrorCode, Parameters, SqlLimits, Statement, Value};

    fn prepare(pager: &crate::storage::Pager, sql: &str) -> Statement {
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let bound = bind_sql(
            sql,
            &SqlLimits::default(),
            &catalog,
            pager.database_uuid().unwrap(),
        )
        .unwrap();
        Statement::from_bound(bound, sql).unwrap()
    }

    #[test]
    fn write_predicates_match_only_true_and_locate_non_boolean_values() {
        // Production breaks caught: treating NULL as true mutates filtered
        // rows, while returning an unlocated Type loses the retained SQL span.
        let location = SqlLocation::new(7, 2, 3).unwrap();
        assert!(predicate_value_matches(Value::Bool(true), location).unwrap());
        assert!(!predicate_value_matches(Value::Bool(false), location).unwrap());
        assert!(!predicate_value_matches(Value::Null, location).unwrap());
        let error = predicate_value_matches(Value::Int64(1), location).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        assert_eq!(error.sql_location(), Some(location));
    }

    #[test]
    fn affected_count_overflow_is_a_resource_limit() {
        // Production break caught: wrapping u64::MAX would acknowledge an
        // unrepresentable result instead of failing the atomic statement.
        assert_eq!(checked_affected_count(0).unwrap(), 1);
        assert_eq!(
            checked_affected_count(u64::MAX).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn create_rechecks_the_name_immediately_before_relation_mutation() {
        // Production break caught: relying only on earlier validation would let
        // two already-validated CREATE plans claim the same catalog name.
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let first = prepare(&pager, "CREATE TABLE NEW_TABLE (ID INT64)");
        let stale = prepare(&pager, "CREATE TABLE NEW_TABLE (ID INT64)");
        let mut batch = pager.begin_write().unwrap();
        let first =
            validate_write_execution(&mut batch, &first, Parameters::none(), 64 * 1024 * 1024)
                .unwrap();
        let stale =
            validate_write_execution(&mut batch, &stale, Parameters::none(), 64 * 1024 * 1024)
                .unwrap();
        execute_validated_write_plan(&mut batch, first).unwrap();
        assert_eq!(
            execute_validated_write_plan(&mut batch, stale)
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
        assert!(
            batch
                .catalog_reader()
                .unwrap()
                .find_table_by_name(
                    &crate::storage::catalog::types::CatalogName::new("NEW_TABLE").unwrap()
                )
                .is_some()
        );
        batch.rollback().unwrap();
    }

    #[test]
    fn create_index_rejects_changed_column_identity() {
        assert_create_index_rejects_changed_column(
            crate::storage::test_support::IndexColumnChange::Identity,
        );
    }

    #[test]
    fn create_index_rejects_changed_column_owner() {
        assert_create_index_rejects_changed_column(
            crate::storage::test_support::IndexColumnChange::Owner,
        );
    }

    #[test]
    fn create_index_rejects_changed_column_ordinal() {
        assert_create_index_rejects_changed_column(
            crate::storage::test_support::IndexColumnChange::Ordinal,
        );
    }

    #[test]
    fn create_index_rejects_changed_column_logical_type() {
        assert_create_index_rejects_changed_column(
            crate::storage::test_support::IndexColumnChange::LogicalType,
        );
    }

    #[test]
    fn create_index_rejects_changed_column_nullability() {
        assert_create_index_rejects_changed_column(
            crate::storage::test_support::IndexColumnChange::Nullability,
        );
    }

    fn assert_create_index_rejects_changed_column(
        change: crate::storage::test_support::IndexColumnChange,
    ) {
        // These fixtures remain valid catalogs with unchanged table versions,
        // so schema-version checks cannot hide omitted column-shape checks.
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let statement = prepare(&pager, "CREATE INDEX IX_STALE ON USERS (ID)");
        let mut batch = pager.begin_write().unwrap();
        let validated =
            validate_write_execution(&mut batch, &statement, Parameters::none(), 64 * 1024 * 1024)
                .unwrap();
        crate::storage::test_support::change_empty_index_fixture_column(&mut batch, change)
            .unwrap();
        let result =
            crate::storage::test_support::assert_batch_unchanged_after(&mut batch, |batch| {
                execute_validated_write_plan(batch, validated)
            });
        assert_eq!(result.unwrap_err().code(), ErrorCode::SchemaChanged);
        batch.catalog_reader().unwrap();
        batch.commit().unwrap();
    }

    #[test]
    fn create_index_low_memory_restores_every_batch_field() {
        // A per-key limit alone cannot account for an unindexed overflow value,
        // or the simultaneous raw/decoded/key allocations of an indexable row.
        for (target, length, limit) in [("NAME", 900, 10_000), ("ID", 20_000, 16_384)] {
            let payload = "x".repeat(length);
            let fixture = crate::storage::test_support::phase_4c1_query_relation_fixture(&[
                (1, "small"),
                (2, &payload),
            ]);
            let pager = fixture.pager();
            let statement = prepare(
                &pager,
                &format!("CREATE INDEX IX_MEMORY ON USERS ({target})"),
            );
            let mut batch = pager.begin_write().unwrap();
            let validated =
                validate_write_execution(&mut batch, &statement, Parameters::none(), limit)
                    .unwrap();
            let result =
                crate::storage::test_support::assert_batch_unchanged_after(&mut batch, |batch| {
                    execute_validated_write_plan(batch, validated)
                });
            assert_eq!(result.unwrap_err().code(), ErrorCode::ResourceLimit);
            batch.catalog_reader().unwrap();
            let good = prepare(&pager, "INSERT INTO USERS (ID) VALUES (9)");
            let validated =
                validate_write_execution(&mut batch, &good, Parameters::none(), limit).unwrap();
            execute_validated_write_plan(&mut batch, validated).unwrap();
            batch.commit().unwrap();
        }
    }

    #[test]
    fn create_index_rechecks_name_version_and_database_after_validation() {
        // Validation can be separated from execution internally: neither a
        // name taken on a different table, a version bump, nor a different
        // database with identical object IDs may reuse the validated request.
        let fixture = crate::storage::test_support::phase_4c1_query_relation_fixture(&[]);
        let pager = fixture.pager();
        let first = prepare(&pager, "CREATE INDEX IX_SHARED ON USERS (ID)");
        let same_name = prepare(&pager, "CREATE INDEX IX_SHARED ON ORDERS (ID)");
        let same_table = prepare(&pager, "CREATE INDEX IX_NEW ON USERS (ID)");
        let mut batch = pager.begin_write().unwrap();
        let validated = [&first, &same_name, &same_table].map(|statement| {
            validate_write_execution(&mut batch, statement, Parameters::none(), 64 * 1024 * 1024)
                .unwrap()
        });
        let [first, same_name, same_table] = validated;
        execute_validated_write_plan(&mut batch, first).unwrap();
        for stale in [same_name, same_table] {
            assert_eq!(
                execute_validated_write_plan(&mut batch, stale)
                    .unwrap_err()
                    .code(),
                ErrorCode::SchemaChanged
            );
        }
        let catalog = batch.catalog_reader().unwrap();
        assert!(
            catalog
                .find_index_by_name(
                    &crate::storage::catalog::types::CatalogName::new("IX_NEW").unwrap()
                )
                .is_none()
        );
        batch.rollback().unwrap();

        let other_fixture = crate::storage::test_support::phase_4c1_query_relation_fixture(&[]);
        let other_pager = other_fixture.pager();
        let mut batch = pager.begin_write().unwrap();
        let statement = prepare(&pager, "CREATE INDEX IX_DATABASE ON USERS (ID)");
        let validated =
            validate_write_execution(&mut batch, &statement, Parameters::none(), 64 * 1024 * 1024)
                .unwrap();
        let mut other = other_pager.begin_write().unwrap();
        assert_eq!(
            execute_validated_write_plan(&mut other, validated)
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
        batch.rollback().unwrap();
        other.rollback().unwrap();
    }

    #[test]
    fn drop_index_rechecks_retained_identity_owner_version_and_database_before_mutation() {
        use crate::storage::test_support::{
            phase_4b_catalog_fixture_with_explicit_index, phase_4b_rehome_explicit_index_to_orders,
            phase_4b_remove_explicit_index,
        };
        // ValidatedWrite intentionally does not borrow the batch. Swapping to a
        // later/current batch must still reject stale identities before writes.
        for change in ["missing", "owner", "version", "database", "same-name"] {
            let fixture = phase_4b_catalog_fixture_with_explicit_index(4096, "IX_T");
            let pager = fixture.pager();
            let statement = prepare(&pager, "DROP INDEX IX_T");
            let mut batch = pager.begin_write().unwrap();
            let validated = validate_write_execution(
                &mut batch,
                &statement,
                Parameters::none(),
                64 * 1024 * 1024,
            )
            .unwrap();
            batch.rollback().unwrap();
            let other_fixture = crate::storage::test_support::phase_4c1_query_relation_fixture(&[]);
            match change {
                "missing" | "same-name" => {
                    phase_4b_remove_explicit_index(&pager, fixture.index_id()).unwrap()
                }
                "owner" => phase_4b_rehome_explicit_index_to_orders(&pager, &fixture).unwrap(),
                _ => {}
            }
            if matches!(change, "version" | "same-name") {
                let create = prepare(
                    &pager,
                    if change == "version" {
                        "CREATE INDEX IX_NEW ON USERS (ID)"
                    } else {
                        "CREATE INDEX IX_T ON USERS (ID)"
                    },
                );
                let mut batch = pager.begin_write().unwrap();
                let create = validate_write_execution(
                    &mut batch,
                    &create,
                    Parameters::none(),
                    64 * 1024 * 1024,
                )
                .unwrap();
                execute_validated_write_plan(&mut batch, create).unwrap();
                batch.commit().unwrap();
            }
            let target = if change == "database" {
                other_fixture.pager()
            } else {
                pager
            };
            let mut batch = target.begin_write().unwrap();
            crate::storage::test_support::assert_batch_unchanged_after(&mut batch, |batch| {
                assert_eq!(
                    execute_validated_write_plan(batch, validated)
                        .unwrap_err()
                        .code(),
                    ErrorCode::SchemaChanged,
                    "{change}"
                );
            });
            batch.rollback().unwrap();
        }
    }

    #[test]
    fn insert_prevalidates_nullability_and_row_size_before_any_write() {
        // Production break caught: opening TableWrite before complete-row
        // validation can alter the catalog gate/root/ROWID on an ordinary error.
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let null_id = prepare(&pager, "INSERT INTO USERS (ID) VALUES (NULL)");
        let oversized_sql = format!(
            "INSERT INTO USERS (ID, NAME) VALUES (1, '{}')",
            "x".repeat(512)
        );
        let oversized = prepare(&pager, &oversized_sql);
        let good = prepare(&pager, "INSERT INTO USERS (ID) VALUES (1)");
        let mut batch = pager.begin_write().unwrap();
        let before = batch
            .catalog_reader()
            .unwrap()
            .find_table_by_id(fixture.users_id())
            .unwrap()
            .clone();

        for (statement, memory_limit) in [(&null_id, 64 * 1024 * 1024), (&oversized, 128)] {
            let validated =
                validate_write_execution(&mut batch, statement, Parameters::none(), memory_limit)
                    .unwrap();
            assert_eq!(
                execute_validated_write_plan(&mut batch, validated)
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
            let after = batch
                .catalog_reader()
                .expect("prevalidation must not open the catalog write gate")
                .find_table_by_id(fixture.users_id())
                .unwrap()
                .clone();
            assert_eq!(after.root(), before.root());
            assert_eq!(after.next_rowid(), before.next_rowid());
        }

        let validated =
            validate_write_execution(&mut batch, &good, Parameters::none(), 64 * 1024 * 1024)
                .unwrap();
        assert_eq!(
            execute_validated_write_plan(&mut batch, validated)
                .unwrap()
                .rows_affected(),
            1
        );
        let relation = Relation::open_from_batch(&mut batch, fixture.users_id()).unwrap();
        let mut scan = relation.scan(usize::MAX);
        let row = scan.next_batch(&mut batch).unwrap().unwrap();
        assert_eq!(row.rowid().get(), 1);
        assert_eq!(
            row.values(),
            &[Value::Int64(1), Value::Null, Value::Null, Value::Null]
        );
        batch.rollback().unwrap();
    }
}
