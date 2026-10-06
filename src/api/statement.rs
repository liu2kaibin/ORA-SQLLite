use std::{fmt, sync::Arc};

use crate::{
    Result, SqlLimits,
    sql::{bind_sql, bound::BoundStatement, plan::Plan},
    storage::catalog::CatalogReader,
};

use super::{ColumnMetadata, ParameterMetadata};

/// An owned, immutable prepared plan with stable public metadata.
///
/// The parameter and result-column metadata is fixed at preparation. Execution revalidates the
/// statement's database identity and retained schema dependencies; a mismatch returns
/// [`crate::ErrorCode::SchemaChanged`]. Clones share the immutable bound statement and plan.
#[derive(Clone)]
pub struct Statement {
    bound: Arc<BoundStatement>,
    plan: Arc<Plan>,
    parameters: Box<[ParameterMetadata]>,
    columns: Box<[ColumnMetadata]>,
}

impl fmt::Debug for Statement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Statement")
            .field("parameters", &PublicParameters(&self.parameters))
            .field("columns", &PublicColumns(&self.columns))
            .finish()
    }
}

struct PublicParameters<'a>(&'a [ParameterMetadata]);

impl fmt::Debug for PublicParameters<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.0.iter().map(PublicParameter))
            .finish()
    }
}

struct PublicParameter<'a>(&'a ParameterMetadata);

impl fmt::Debug for PublicParameter<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParameterMetadata")
            .field("identifier", self.0.identifier())
            .field("data_type", &self.0.data_type())
            .finish()
    }
}

struct PublicColumns<'a>(&'a [ColumnMetadata]);

impl fmt::Debug for PublicColumns<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.0.iter().map(PublicColumn))
            .finish()
    }
}

struct PublicColumn<'a>(&'a ColumnMetadata);

impl fmt::Debug for PublicColumn<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ColumnMetadata")
            .field("name", &self.0.name())
            .field("data_type", &self.0.data_type())
            .field("nullable", &self.0.nullable())
            .finish()
    }
}

impl Statement {
    pub(crate) fn from_bound(bound: BoundStatement, sql: &str) -> Result<Self> {
        let plan = Arc::new(Plan::from_bound(&bound, sql)?);
        let parameters = bound.parameters().to_vec().into_boxed_slice();
        let columns = bound.columns().to_vec().into_boxed_slice();
        Ok(Self {
            bound: Arc::new(bound),
            plan,
            parameters,
            columns,
        })
    }

    /// Returns stable bind-parameter metadata in logical parameter order.
    pub fn parameters(&self) -> &[ParameterMetadata] {
        &self.parameters
    }

    /// Returns stable result-column metadata in output order.
    pub fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }

    #[allow(dead_code, reason = "execution is added in Phase 4C")]
    pub(crate) fn bound(&self) -> &BoundStatement {
        &self.bound
    }

    pub(crate) fn plan(&self) -> &Plan {
        &self.plan
    }

    pub(crate) fn validate_schema(
        &self,
        database_uuid: [u8; 16],
        catalog: &CatalogReader,
    ) -> Result<()> {
        self.bound.validate_schema(database_uuid, catalog)
    }
}

/// Runs the single prepare pipeline shared by connection and transaction APIs.
pub(crate) fn prepare_with_catalog(
    sql: &str,
    limits: &SqlLimits,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
) -> Result<Statement> {
    let bound = bind_sql(sql, limits, catalog, database_uuid)?;
    Statement::from_bound(bound, sql)
}

#[cfg(test)]
mod tests {
    use crate::{
        ErrorCode, Value,
        api::{ColumnMetadata, DataType, ParameterIdentifier, ParameterMetadata},
        sql::{
            SqlSpan, SqlType,
            bound::{
                BoundExpr, BoundExprKind, BoundStatement, BoundStatementKind, SchemaDependency,
            },
        },
        storage::{
            catalog::types::{ColumnId, SchemaVersion, TableId},
            test_support::phase_4b_catalog_fixture,
        },
    };

    use super::Statement;

    fn fixture_bound_select() -> BoundStatement {
        let table_id = TableId::new(1).unwrap();
        let column_id = ColumnId::new(2).unwrap();
        let schema_version = SchemaVersion::new(1).unwrap();

        BoundStatement {
            kind: BoundStatementKind::Query {
                query: crate::sql::bound::BoundQuery {
                    projection: vec![BoundExpr {
                        kind: BoundExprKind::Column {
                            source_id: crate::sql::bound::SourceId(0),
                            table_id,
                            column_id,
                            ordinal: 0,
                        },
                        sql_type: SqlType::Int64,
                        nullable: false,
                        span: SqlSpan::new(7, 9).unwrap(),
                    }],
                    distinct: false,
                    sources: Vec::new(),
                    joins: Vec::new(),
                    selection: None,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    columns: Vec::new(),
                    dependencies: Vec::new(),
                },
            },
            database_uuid: [7; 16],
            dependencies: vec![SchemaDependency {
                database_uuid: [7; 16],
                table_id,
                schema_version,
            }],
            created_names: Vec::new(),
            parameters: vec![ParameterMetadata {
                identifier: ParameterIdentifier::Position(1),
                data_type: Some(DataType::Int64),
                allow_oracle_text_source: false,
            }],
            columns: vec![ColumnMetadata {
                name: "ID".to_owned(),
                data_type: DataType::Int64,
                nullable: false,
                source_table_id: Some(table_id),
                source_column_id: Some(column_id),
            }],
            span: SqlSpan::new(0, 24).unwrap(),
        }
    }

    #[test]
    fn statement_exposes_owned_stable_metadata() {
        let bound = fixture_bound_select();
        let statement = Statement::from_bound(bound, &" ".repeat(32)).unwrap();
        assert_eq!(
            statement.parameters()[0].identifier(),
            &ParameterIdentifier::Position(1)
        );
        assert_eq!(statement.parameters()[0].data_type(), Some(DataType::Int64));
        assert_eq!(statement.columns()[0].name(), "ID");
        assert_eq!(statement.columns()[0].data_type(), DataType::Int64);
    }

    #[test]
    fn statement_clone_shares_the_compiled_plan_without_borrowing_source() {
        // Recompiling/cloning recursive IR or retaining the caller's source
        // breaks the immutable, cheaply shared prepared-statement boundary.
        let source = " ".repeat(32);
        let statement = Statement::from_bound(fixture_bound_select(), &source).unwrap();
        drop(source);
        let cloned = statement.clone();
        assert!(std::sync::Arc::ptr_eq(&statement.bound, &cloned.bound));
        assert!(std::sync::Arc::ptr_eq(&statement.plan, &cloned.plan));
        assert!(matches!(
            cloned.plan().kind(),
            crate::sql::plan::PlanKind::Query(_)
        ));
        assert_eq!(cloned.plan().parameters(), cloned.parameters());
        assert_eq!(cloned.plan().columns(), cloned.columns());
        assert_eq!(cloned.plan().dependencies(), cloned.bound().dependencies());
    }

    #[test]
    fn statement_debug_exposes_metadata_but_not_bound_ir() {
        let mut bound = fixture_bound_select();
        let BoundStatementKind::Query { query } = &mut bound.kind else {
            panic!("query");
        };
        query.projection = vec![BoundExpr {
            kind: BoundExprKind::Literal(Value::Int64(987_654_321)),
            sql_type: SqlType::Int64,
            nullable: false,
            span: SqlSpan::new(17, 26).unwrap(),
        }];

        let rendered = format!(
            "{:?}",
            Statement::from_bound(bound, &" ".repeat(32)).unwrap()
        );
        assert!(rendered.contains("parameters"), "{rendered}");
        assert!(rendered.contains("columns"), "{rendered}");
        for private in [
            "BoundStatement",
            "Plan",
            "database_uuid",
            "table_id",
            "column_id",
            "span",
            "987654321",
        ] {
            assert!(!rendered.contains(private), "leaked {private}: {rendered}");
        }
    }

    #[test]
    fn statement_keeps_the_bound_schema_validation_contract() {
        // Production break caught: an API statement that bypasses its retained
        // bound identity could execute against a different database snapshot.
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let database_uuid = [7; 16];
        let statement = Statement::from_bound(fixture_bound_select(), &" ".repeat(32)).unwrap();

        assert!(statement.validate_schema(database_uuid, &catalog).is_ok());
        let mut wrong_uuid = database_uuid;
        wrong_uuid[0] ^= 0xff;
        assert_eq!(
            statement
                .validate_schema(wrong_uuid, &catalog)
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
    }
}
