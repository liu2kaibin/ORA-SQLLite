use std::{fmt, sync::Arc};

use crate::{Result, sql::bound::BoundStatement, storage::catalog::CatalogReader};

use super::{ColumnMetadata, ParameterMetadata};

/// An immutable prepared statement and its stable public metadata.
#[derive(Clone)]
pub struct Statement {
    bound: Arc<BoundStatement>,
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
    #[allow(
        dead_code,
        reason = "the Phase 4B prepare pipeline is added in a later task"
    )]
    pub(crate) fn from_bound(bound: BoundStatement) -> Self {
        let parameters = bound.parameters().to_vec().into_boxed_slice();
        let columns = bound.columns().to_vec().into_boxed_slice();
        Self {
            bound: Arc::new(bound),
            parameters,
            columns,
        }
    }

    pub fn parameters(&self) -> &[ParameterMetadata] {
        &self.parameters
    }

    pub fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }

    #[allow(dead_code, reason = "execution is added in Phase 4C")]
    pub(crate) fn bound(&self) -> &BoundStatement {
        &self.bound
    }

    #[allow(dead_code, reason = "execution is added in Phase 4C")]
    pub(crate) fn validate_schema(
        &self,
        database_uuid: [u8; 16],
        catalog: &CatalogReader,
    ) -> Result<()> {
        self.bound.validate_schema(database_uuid, catalog)
    }
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
                projection: vec![BoundExpr {
                    kind: BoundExprKind::Column {
                        table_id,
                        column_id,
                        ordinal: 0,
                    },
                    sql_type: SqlType::Int64,
                    nullable: false,
                    span: SqlSpan::new(7, 9).unwrap(),
                }],
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
        let statement = Statement::from_bound(bound);
        assert_eq!(
            statement.parameters()[0].identifier(),
            &ParameterIdentifier::Position(1)
        );
        assert_eq!(statement.parameters()[0].data_type(), Some(DataType::Int64));
        assert_eq!(statement.columns()[0].name(), "ID");
        assert_eq!(statement.columns()[0].data_type(), DataType::Int64);
    }

    #[test]
    fn statement_debug_exposes_metadata_but_not_bound_ir() {
        let mut bound = fixture_bound_select();
        bound.kind = BoundStatementKind::Query {
            projection: vec![BoundExpr {
                kind: BoundExprKind::Literal(Value::Int64(987_654_321)),
                sql_type: SqlType::Int64,
                nullable: false,
                span: SqlSpan::new(17, 26).unwrap(),
            }],
        };

        let rendered = format!("{:?}", Statement::from_bound(bound));
        assert!(rendered.contains("parameters"), "{rendered}");
        assert!(rendered.contains("columns"), "{rendered}");
        for private in [
            "BoundStatement",
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
        let statement = Statement::from_bound(fixture_bound_select());

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
