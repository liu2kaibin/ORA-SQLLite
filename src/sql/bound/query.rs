use super::{BoundExpr, SchemaDependency};
use crate::{
    api::ColumnMetadata,
    sql::ast::{JoinKind, NullPlacement, OrderDirection},
    storage::catalog::types::{SchemaVersion, TableId},
};

/// Query-local occurrence, not a catalog identity. Ordered from FROM through JOINs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SourceId(pub(crate) usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundSource {
    pub(crate) source_id: SourceId,
    pub(crate) table_id: TableId,
    pub(crate) schema_version: SchemaVersion,
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
