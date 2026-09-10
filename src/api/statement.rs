use std::sync::Arc;

use crate::sql::bound::BoundStatement;

use super::{ColumnMetadata, ParameterMetadata};

/// An immutable prepared statement and its stable public metadata.
#[derive(Debug, Clone)]
pub struct Statement {
    bound: Arc<BoundStatement>,
    parameters: Box<[ParameterMetadata]>,
    columns: Box<[ColumnMetadata]>,
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
}

#[cfg(test)]
mod tests {
    use crate::{
        api::{ColumnMetadata, DataType, ParameterIdentifier, ParameterMetadata},
        sql::{
            SqlSpan, SqlType,
            bound::{
                BoundExpr, BoundExprKind, BoundStatement, BoundStatementKind, SchemaDependency,
            },
        },
        storage::catalog::types::{ColumnId, SchemaVersion, TableId},
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
}
