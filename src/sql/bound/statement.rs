use crate::{
    DbError, ErrorCode, Result,
    api::{ColumnMetadata, ParameterMetadata},
    storage::catalog::{
        CatalogReader,
        types::{CatalogName, ColumnId, ColumnType, IndexId, ObjectKind, SchemaVersion, TableId},
    },
};

use super::super::SqlSpan;
use super::{BoundExpr, BoundExprKind};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundStatement {
    pub(crate) kind: BoundStatementKind,
    pub(crate) database_uuid: [u8; 16],
    pub(crate) dependencies: Vec<SchemaDependency>,
    pub(crate) created_names: Vec<CreatedNameDependency>,
    pub(crate) parameters: Vec<ParameterMetadata>,
    pub(crate) columns: Vec<ColumnMetadata>,
    pub(crate) span: SqlSpan,
}

impl BoundStatement {
    pub(crate) fn parameters(&self) -> &[ParameterMetadata] {
        &self.parameters
    }

    pub(crate) fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }

    pub(crate) fn dependencies(&self) -> &[SchemaDependency] {
        &self.dependencies
    }

    pub(crate) fn target_ordinals(&self) -> Vec<u16> {
        match &self.kind {
            BoundStatementKind::Insert { targets, .. } => {
                targets.iter().map(|target| target.ordinal).collect()
            }
            BoundStatementKind::Update { assignments, .. } => assignments
                .iter()
                .map(|assignment| assignment.target.ordinal)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Rechecks the immutable catalog facts on which this statement was prepared.
    ///
    /// The catalog reader has already verified its on-disk image. This routine only
    /// compares logical identities, so unrelated catalog changes do not invalidate
    /// an otherwise usable prepared statement.
    pub(crate) fn validate_schema(
        &self,
        database_uuid: [u8; 16],
        catalog: &CatalogReader,
    ) -> Result<()> {
        if self.database_uuid != database_uuid {
            return Err(schema_changed(
                "prepared statement belongs to another database",
            ));
        }

        for dependency in &self.dependencies {
            if dependency.database_uuid != self.database_uuid
                || !catalog.object_id_exists(ObjectKind::Table, dependency.table_id.into())
            {
                return Err(schema_changed(
                    "prepared table identity is no longer present",
                ));
            }
            let Some(table) = catalog.find_table_by_id(dependency.table_id) else {
                return Err(schema_changed(
                    "prepared table identity is no longer present",
                ));
            };
            if table.schema_version() != dependency.schema_version {
                return Err(schema_changed("prepared table schema version has changed"));
            }
        }

        self.validate_column_identities(catalog)?;

        for created_name in &self.created_names {
            if catalog.object_name_exists(created_name.object_kind, &created_name.name) {
                return Err(schema_changed(
                    "prepared CREATE name is no longer available",
                ));
            }
        }
        Ok(())
    }

    fn validate_column_identities(&self, catalog: &CatalogReader) -> Result<()> {
        let mut columns = Vec::new();
        let mut expressions = Vec::new();
        match &self.kind {
            BoundStatementKind::Query { projection } => expressions.extend(projection.iter()),
            BoundStatementKind::Insert {
                targets, values, ..
            } => {
                columns.extend(targets.iter());
                expressions.extend(values.iter());
            }
            BoundStatementKind::Update {
                assignments,
                selection,
                ..
            } => {
                for assignment in assignments {
                    columns.push(&assignment.target);
                    expressions.push(&assignment.value);
                }
                if let Some(selection) = selection {
                    expressions.push(selection);
                }
            }
            BoundStatementKind::Delete { selection, .. } => {
                if let Some(selection) = selection {
                    expressions.push(selection);
                }
            }
            BoundStatementKind::CreateTable { constraints, .. } => {
                for constraint in constraints {
                    match constraint {
                        BoundConstraint::Check { expression, .. } => expressions.push(expression),
                        BoundConstraint::ForeignKey {
                            referenced_columns, ..
                        } => columns.extend(referenced_columns.iter()),
                        BoundConstraint::PrimaryKey { .. }
                        | BoundConstraint::Unique { .. }
                        | BoundConstraint::NotNull { .. } => {}
                    }
                }
            }
            BoundStatementKind::CreateIndex { columns: index, .. } => {
                columns.extend(index.iter());
            }
            BoundStatementKind::DropTable { .. }
            | BoundStatementKind::DropIndex { .. }
            | BoundStatementKind::Begin
            | BoundStatementKind::Commit
            | BoundStatementKind::Rollback => {}
        }

        for column in columns {
            validate_column_identity(catalog, column.table_id, column.column_id)?;
        }
        validate_expression_columns(catalog, expressions)
    }
}

fn validate_expression_columns(catalog: &CatalogReader, roots: Vec<&BoundExpr>) -> Result<()> {
    let mut work = roots;
    while let Some(expression) = work.pop() {
        match &expression.kind {
            BoundExprKind::Column {
                table_id,
                column_id,
                ..
            } => validate_column_identity(catalog, *table_id, *column_id)?,
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                work.push(expr);
            }
            BoundExprKind::Binary { left, right, .. } => {
                work.push(left);
                work.push(right);
            }
            BoundExprKind::InList { expr, values, .. } => {
                work.push(expr);
                work.extend(values.iter());
            }
            BoundExprKind::Between {
                expr, low, high, ..
            } => {
                work.push(expr);
                work.push(low);
                work.push(high);
            }
            BoundExprKind::Like { expr, pattern, .. } => {
                work.push(expr);
                work.push(pattern);
            }
            BoundExprKind::Aggregate {
                argument: Some(argument),
                ..
            } => work.push(argument),
            BoundExprKind::Literal(_)
            | BoundExprKind::NewTableColumn { .. }
            | BoundExprKind::Parameter(_)
            | BoundExprKind::Aggregate { argument: None, .. } => {}
        }
    }
    Ok(())
}

fn validate_column_identity(
    catalog: &CatalogReader,
    table_id: TableId,
    column_id: ColumnId,
) -> Result<()> {
    if !catalog.object_id_exists(ObjectKind::Column, column_id.into()) {
        return Err(schema_changed(
            "prepared column identity is no longer present",
        ));
    }
    let Some(column) = catalog.find_column_by_id(column_id) else {
        return Err(schema_changed(
            "prepared column identity is no longer present",
        ));
    };
    if column.table_id() != table_id {
        return Err(schema_changed(
            "prepared column identity belongs to another table",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BoundStatementKind {
    Query {
        projection: Vec<BoundExpr>,
    },
    Insert {
        table: BoundTable,
        targets: Vec<BoundColumn>,
        values: Vec<BoundExpr>,
    },
    Update {
        table: BoundTable,
        assignments: Vec<BoundAssignment>,
        selection: Option<BoundExpr>,
    },
    Delete {
        table: BoundTable,
        selection: Option<BoundExpr>,
    },
    CreateTable {
        name: CatalogName,
        columns: Vec<BoundColumnDefinition>,
        constraints: Vec<BoundConstraint>,
    },
    DropTable {
        table_id: TableId,
    },
    CreateIndex {
        name: CatalogName,
        table: BoundTable,
        columns: Vec<BoundColumn>,
        unique: bool,
    },
    /// V1 does not persist index records, so the binder cannot construct this
    /// variant yet. Keeping an `IndexId` rather than a name prevents a Phase 4C
    /// executor from accidentally late-binding a replacement index.
    DropIndex {
        index_id: IndexId,
    },
    Begin,
    Commit,
    Rollback,
}

/// A stable catalog table identity carried by non-query statement kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundTable {
    pub(crate) table_id: TableId,
    pub(crate) schema_version: SchemaVersion,
    pub(crate) span: SqlSpan,
}

/// A stable catalog column identity and its logical type information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundColumn {
    pub(crate) table_id: TableId,
    pub(crate) column_id: ColumnId,
    pub(crate) ordinal: u16,
    pub(crate) sql_type: crate::sql::SqlType,
    pub(crate) nullable: bool,
    pub(crate) span: SqlSpan,
}

/// One target column and source expression in an UPDATE statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundAssignment {
    pub(crate) target: BoundColumn,
    pub(crate) value: BoundExpr,
    pub(crate) span: SqlSpan,
}

/// A new table's not-yet-persisted column definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundColumnDefinition {
    pub(crate) name: CatalogName,
    pub(crate) ordinal: u16,
    pub(crate) column_type: ColumnType,
    pub(crate) nullable: bool,
    pub(crate) span: SqlSpan,
}

/// Constraint semantics needed by the Phase 4C catalog mutation planner.
///
/// CHECK expressions are bound only against the new table's own columns. Their
/// `BoundExprKind::NewTableColumn` ordinals therefore refer to `columns` above.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BoundConstraint {
    PrimaryKey {
        name: Option<CatalogName>,
        columns: Vec<u16>,
        span: SqlSpan,
    },
    Unique {
        name: Option<CatalogName>,
        columns: Vec<u16>,
        span: SqlSpan,
    },
    NotNull {
        name: Option<CatalogName>,
        column: u16,
        span: SqlSpan,
    },
    Check {
        name: Option<CatalogName>,
        expression: BoundExpr,
        span: SqlSpan,
    },
    ForeignKey {
        name: Option<CatalogName>,
        columns: Vec<u16>,
        referenced_table: BoundTable,
        referenced_columns: Vec<BoundColumn>,
        span: SqlSpan,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaDependency {
    pub database_uuid: [u8; 16],
    pub table_id: TableId,
    pub schema_version: SchemaVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreatedNameDependency {
    pub object_kind: ObjectKind,
    pub name: CatalogName,
}

fn schema_changed(message: &'static str) -> DbError {
    DbError::new(ErrorCode::SchemaChanged, message)
}
