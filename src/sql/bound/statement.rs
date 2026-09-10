use crate::{
    api::{ColumnMetadata, ParameterMetadata},
    storage::catalog::types::{CatalogName, ObjectKind, SchemaVersion, TableId},
};

use super::super::SqlSpan;
use super::BoundExpr;

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
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BoundStatementKind {
    Query { projection: Vec<BoundExpr> },
    Insert,
    Update,
    Delete,
    CreateTable,
    DropTable,
    CreateIndex,
    DropIndex,
    Begin,
    Commit,
    Rollback,
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
