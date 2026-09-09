use super::super::{Identifier, ParameterKey, SqlSpan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AstExpr {
    pub(crate) kind: AstExprKind,
    pub(crate) span: SqlSpan,
}

impl AstExpr {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AstExprKind {
    Null,
    Bool(bool),
    Integer(String),
    Decimal(String),
    Float(String),
    Text(String),
    Blob(Vec<u8>),
    Column(Vec<Identifier>),
    Parameter(ParameterKey),
    Unary {
        op: UnaryOp,
        expr: Box<AstExpr>,
    },
    Binary {
        left: Box<AstExpr>,
        op: BinaryOp,
        right: Box<AstExpr>,
    },
    IsNull {
        expr: Box<AstExpr>,
        negated: bool,
    },
    InList {
        expr: Box<AstExpr>,
        values: Vec<AstExpr>,
        negated: bool,
    },
    Between {
        expr: Box<AstExpr>,
        low: Box<AstExpr>,
        high: Box<AstExpr>,
        negated: bool,
    },
    Like {
        expr: Box<AstExpr>,
        pattern: Box<AstExpr>,
        negated: bool,
    },
    Aggregate {
        function: AggregateFunction,
        argument: AggregateArgument,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Positive,
    Negative,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
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
pub(crate) enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AggregateArgument {
    Wildcard(SqlSpan),
    Expr(Box<AstExpr>),
}

impl AggregateArgument {
    pub(crate) const fn span(&self) -> SqlSpan {
        match self {
            Self::Wildcard(span) => *span,
            Self::Expr(expr) => expr.span(),
        }
    }
}
