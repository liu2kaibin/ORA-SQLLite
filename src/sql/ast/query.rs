use super::super::{Identifier, SqlSpan};
use super::AstExpr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AstQuery {
    pub(crate) distinct: bool,
    pub(crate) projection: Vec<SelectItem>,
    pub(crate) from: TableSource,
    pub(crate) joins: Vec<Join>,
    pub(crate) selection: Option<AstExpr>,
    pub(crate) group_by: Vec<AstExpr>,
    pub(crate) having: Option<AstExpr>,
    pub(crate) order_by: Vec<OrderItem>,
    pub(crate) span: SqlSpan,
}

impl AstQuery {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectItem {
    Expr {
        expr: AstExpr,
        alias: Option<Identifier>,
        span: SqlSpan,
    },
    Wildcard {
        qualifier: Option<Identifier>,
        span: SqlSpan,
    },
}

impl SelectItem {
    pub(crate) const fn span(&self) -> SqlSpan {
        match self {
            Self::Expr { span, .. } | Self::Wildcard { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableSource {
    pub(crate) table: Identifier,
    pub(crate) alias: Option<Identifier>,
    pub(crate) span: SqlSpan,
}

impl TableSource {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Join {
    pub(crate) kind: JoinKind,
    pub(crate) source: TableSource,
    pub(crate) condition: AstExpr,
    pub(crate) span: SqlSpan,
}

impl Join {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrderItem {
    pub(crate) expr: AstExpr,
    pub(crate) direction: OrderDirection,
    pub(crate) nulls: NullPlacement,
    pub(crate) span: SqlSpan,
}

impl OrderItem {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderDirection {
    Default,
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullPlacement {
    Default,
    First,
    Last,
}
