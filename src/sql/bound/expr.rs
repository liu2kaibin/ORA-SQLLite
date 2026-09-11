use crate::{
    Value,
    api::ParameterIdentifier,
    storage::catalog::types::{ColumnId, TableId},
};

use super::super::{SqlSpan, SqlType};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundExpr {
    pub kind: BoundExprKind,
    pub sql_type: SqlType,
    pub nullable: bool,
    pub span: SqlSpan,
}

impl BoundExpr {
    pub(crate) const fn new(
        kind: BoundExprKind,
        sql_type: SqlType,
        nullable: bool,
        span: SqlSpan,
    ) -> Self {
        Self {
            kind,
            sql_type,
            nullable,
            span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BoundExprKind {
    Literal(Value),
    Column {
        table_id: TableId,
        column_id: ColumnId,
        ordinal: u16,
    },
    /// A column of the table being defined by a CREATE TABLE constraint.
    ///
    /// It has no catalog identity before Phase 4C persists the new definition;
    /// the ordinal is scoped by its enclosing `BoundStatementKind::CreateTable`.
    NewTableColumn {
        ordinal: u16,
    },
    Parameter(ParameterIdentifier),
    Unary {
        op: BoundUnaryOp,
        expr: Box<BoundExpr>,
    },
    Binary {
        left: Box<BoundExpr>,
        op: BoundBinaryOp,
        right: Box<BoundExpr>,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    InList {
        expr: Box<BoundExpr>,
        values: Vec<BoundExpr>,
        negated: bool,
    },
    Between {
        expr: Box<BoundExpr>,
        low: Box<BoundExpr>,
        high: Box<BoundExpr>,
        negated: bool,
    },
    Like {
        expr: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        negated: bool,
    },
    Aggregate {
        function: BoundAggregateFunction,
        argument: Option<Box<BoundExpr>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundUnaryOp {
    Positive,
    Negative,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundAggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}
