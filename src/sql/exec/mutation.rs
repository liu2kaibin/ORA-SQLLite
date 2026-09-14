use crate::{
    DbError, ErrorCode, ExecutionResult, Parameters, Result, Statement,
    sql::plan::{CreateTablePlan, Plan, PlanKind},
    storage::{
        WriteBatch,
        catalog::types::ObjectKind,
        relation::{ColumnSpec, TableSpec, create_relation, drop_relation},
    },
};

use super::{ParameterFrame, WorkMemory};

/// A write plan whose API kind, parameters, and retained schema have all been checked.
pub(crate) struct ValidatedWrite<'a> {
    plan: &'a Plan,
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
        plan: statement.plan(),
        params,
        memory,
    })
}

pub(crate) fn execute_validated_write_plan(
    batch: &mut WriteBatch,
    validated: ValidatedWrite<'_>,
) -> Result<ExecutionResult> {
    let ValidatedWrite {
        plan,
        params: _params,
        memory: _memory,
    } = validated;
    match plan.kind() {
        PlanKind::CreateTable(plan) => execute_create_table(batch, plan),
        PlanKind::DropTable { table_id } => {
            drop_relation(batch, *table_id)?;
            Ok(ExecutionResult::new(0))
        }
        kind => Err(unsupported_write_kind(kind)),
    }
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
    use super::{execute_validated_write_plan, validate_write_execution};
    use crate::sql::bind_sql;
    use crate::storage::test_support::phase_4b_catalog_fixture;
    use crate::{ErrorCode, Parameters, SqlLimits, Statement};

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
}
