use super::{BoundExpr, SchemaDependency};
use crate::{
    DbError, ErrorCode, Result,
    api::ColumnMetadata,
    oracle_type::OracleTypeDescriptor,
    sql::ast::{JoinKind, NullPlacement, OrderDirection},
    storage::catalog::types::{ColumnDef, ColumnId, ColumnType, IndexId, SchemaVersion, TableId},
};

/// Query-local occurrence, not a catalog identity. Ordered from FROM through JOINs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SourceId(pub(crate) usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundIndexColumn {
    column_id: ColumnId,
    ordinal: u16,
    column_type: ColumnType,
    oracle_type: Option<OracleTypeDescriptor>,
}

impl BoundIndexColumn {
    pub(crate) const fn new(column_id: ColumnId, ordinal: u16, column_type: ColumnType) -> Self {
        Self {
            column_id,
            ordinal,
            column_type,
            oracle_type: None,
        }
    }

    pub(crate) fn from_column(column: &ColumnDef) -> Self {
        Self {
            column_id: column.id(),
            ordinal: column.ordinal(),
            column_type: column.column_type(),
            oracle_type: column.oracle_type(),
        }
    }

    pub(crate) const fn column_id(&self) -> ColumnId {
        self.column_id
    }

    pub(crate) const fn ordinal(&self) -> u16 {
        self.ordinal
    }

    pub(crate) const fn column_type(&self) -> ColumnType {
        self.column_type
    }
    pub(crate) const fn oracle_type(&self) -> Option<OracleTypeDescriptor> {
        self.oracle_type
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundIndex {
    id: IndexId,
    unique: bool,
    columns: Box<[BoundIndexColumn]>,
}

impl BoundIndex {
    pub(crate) fn new(id: IndexId, unique: bool, columns: Box<[BoundIndexColumn]>) -> Result<Self> {
        if columns.is_empty() {
            return Err(DbError::new(
                ErrorCode::Internal,
                "bound index must contain at least one column",
            ));
        }
        Ok(Self {
            id,
            unique,
            columns,
        })
    }

    pub(crate) const fn id(&self) -> IndexId {
        self.id
    }

    pub(crate) const fn unique(&self) -> bool {
        self.unique
    }

    pub(crate) fn columns(&self) -> &[BoundIndexColumn] {
        &self.columns
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundSource {
    pub(crate) source_id: SourceId,
    pub(crate) table_id: TableId,
    pub(crate) schema_version: SchemaVersion,
    indexes: Box<[BoundIndex]>,
}

impl BoundSource {
    pub(crate) fn new(
        source_id: SourceId,
        table_id: TableId,
        schema_version: SchemaVersion,
        indexes: Box<[BoundIndex]>,
    ) -> Self {
        Self {
            source_id,
            table_id,
            schema_version,
            indexes,
        }
    }

    pub(crate) fn indexes(&self) -> &[BoundIndex] {
        &self.indexes
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundJoin {
    pub(crate) kind: JoinKind,
    pub(crate) source_id: SourceId,
    pub(crate) condition: BoundExpr,
}

/// Complete owned logical query, retained unchanged for the Phase 4C planner.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundQuery {
    pub(crate) distinct: bool,
    pub(crate) sources: Vec<BoundSource>,
    pub(crate) joins: Vec<BoundJoin>,
    pub(crate) selection: Option<BoundExpr>,
    pub(crate) group_by: Vec<BoundExpr>,
    pub(crate) having: Option<BoundExpr>,
    pub(crate) projection: Vec<BoundExpr>,
    pub(crate) order_by: Vec<BoundOrderItem>,
    pub(crate) columns: Vec<ColumnMetadata>,
    pub(crate) dependencies: Vec<SchemaDependency>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundOrderItem {
    pub(crate) expression: BoundOrderExpression,
    pub(crate) direction: OrderDirection,
    pub(crate) nulls: NullPlacement,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BoundOrderExpression {
    Projection(usize),
    Expression(BoundExpr),
}

impl BoundOrderExpression {
    pub(crate) fn expression<'a>(&'a self, projection: &'a [BoundExpr]) -> &'a BoundExpr {
        match self {
            Self::Projection(index) => &projection[*index],
            Self::Expression(expression) => expression,
        }
    }
}
