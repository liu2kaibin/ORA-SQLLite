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

    pub(crate) fn take_kind(&mut self) -> AstExprKind {
        std::mem::replace(&mut self.kind, AstExprKind::Null)
    }

    fn detach_children(&mut self, work: &mut Vec<Self>) {
        match self.take_kind() {
            AstExprKind::Unary { expr, .. } | AstExprKind::IsNull { expr, .. } => work.push(*expr),
            AstExprKind::Binary { left, right, .. } => {
                work.push(*left);
                work.push(*right);
            }
            AstExprKind::InList { expr, values, .. } => {
                work.push(*expr);
                work.extend(values);
            }
            AstExprKind::Between {
                expr, low, high, ..
            } => {
                work.push(*expr);
                work.push(*low);
                work.push(*high);
            }
            AstExprKind::Like { expr, pattern, .. } => {
                work.push(*expr);
                work.push(*pattern);
            }
            AstExprKind::Aggregate {
                argument: AggregateArgument::Expr(expr),
                ..
            } => work.push(*expr),
            _ => {}
        }
    }
}

impl Drop for AstExpr {
    fn drop(&mut self) {
        let mut work = Vec::new();
        self.detach_children(&mut work);
        while let Some(mut child) = work.pop() {
            child.detach_children(&mut work);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize) -> SqlSpan {
        SqlSpan::new(start, start + 1).unwrap()
    }

    fn leaf(start: usize) -> AstExpr {
        AstExpr {
            kind: AstExprKind::Null,
            span: span(start),
        }
    }

    fn expression_kind_tag(kind: &AstExprKind) -> u8 {
        match kind {
            AstExprKind::Null => 0,
            AstExprKind::Bool(_) => 1,
            AstExprKind::Integer(_) => 2,
            AstExprKind::Decimal(_) => 3,
            AstExprKind::Float(_) => 4,
            AstExprKind::Text(_) => 5,
            AstExprKind::Blob(_) => 6,
            AstExprKind::Column(_) => 7,
            AstExprKind::Parameter(_) => 8,
            AstExprKind::Unary { .. } => 9,
            AstExprKind::Binary { .. } => 10,
            AstExprKind::IsNull { .. } => 11,
            AstExprKind::InList { .. } => 12,
            AstExprKind::Between { .. } => 13,
            AstExprKind::Like { .. } => 14,
            AstExprKind::Aggregate { .. } => 15,
        }
    }

    fn unary_tag(op: UnaryOp) -> u8 {
        match op {
            UnaryOp::Positive => 0,
            UnaryOp::Negative => 1,
            UnaryOp::Not => 2,
        }
    }

    fn binary_tag(op: BinaryOp) -> u8 {
        match op {
            BinaryOp::Add => 0,
            BinaryOp::Subtract => 1,
            BinaryOp::Multiply => 2,
            BinaryOp::Divide => 3,
            BinaryOp::Modulo => 4,
            BinaryOp::Equal => 5,
            BinaryOp::NotEqual => 6,
            BinaryOp::LessThan => 7,
            BinaryOp::LessThanOrEqual => 8,
            BinaryOp::GreaterThan => 9,
            BinaryOp::GreaterThanOrEqual => 10,
            BinaryOp::And => 11,
            BinaryOp::Or => 12,
        }
    }

    fn aggregate_function_tag(function: AggregateFunction) -> u8 {
        match function {
            AggregateFunction::Count => 0,
            AggregateFunction::Sum => 1,
            AggregateFunction::Avg => 2,
            AggregateFunction::Min => 3,
            AggregateFunction::Max => 4,
        }
    }

    fn aggregate_argument_tag(argument: &AggregateArgument) -> u8 {
        match argument {
            AggregateArgument::Wildcard(_) => 0,
            AggregateArgument::Expr(_) => 1,
        }
    }

    #[test]
    fn expression_variants_are_closed_owned_and_span_preserving() {
        let mut integer_source = String::from("42");
        let mut text_source = String::from("owned text");
        let column = Identifier::unquoted("amount", span(80)).unwrap();
        let parameter = ParameterKey::parse(":amount", span(81)).unwrap();
        let kinds = vec![
            AstExprKind::Null,
            AstExprKind::Bool(true),
            AstExprKind::Integer(integer_source.clone()),
            AstExprKind::Decimal(String::from("12.50")),
            AstExprKind::Float(String::from("1e2")),
            AstExprKind::Text(text_source.clone()),
            AstExprKind::Blob(vec![0x00, 0xff]),
            AstExprKind::Column(vec![column]),
            AstExprKind::Parameter(parameter),
            AstExprKind::Unary {
                op: UnaryOp::Positive,
                expr: Box::new(leaf(82)),
            },
            AstExprKind::Binary {
                left: Box::new(leaf(83)),
                op: BinaryOp::Equal,
                right: Box::new(leaf(84)),
            },
            AstExprKind::IsNull {
                expr: Box::new(leaf(85)),
                negated: true,
            },
            AstExprKind::InList {
                expr: Box::new(leaf(86)),
                values: vec![leaf(87)],
                negated: false,
            },
            AstExprKind::Between {
                expr: Box::new(leaf(88)),
                low: Box::new(leaf(89)),
                high: Box::new(leaf(90)),
                negated: true,
            },
            AstExprKind::Like {
                expr: Box::new(leaf(91)),
                pattern: Box::new(leaf(92)),
                negated: false,
            },
            AstExprKind::Aggregate {
                function: AggregateFunction::Count,
                argument: AggregateArgument::Wildcard(span(93)),
            },
        ];
        integer_source.clear();
        text_source.clear();

        let expressions = kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| AstExpr {
                kind,
                span: span(index),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            expressions
                .iter()
                .map(|expr| expression_kind_tag(&expr.kind))
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(
            expressions.iter().map(AstExpr::span).collect::<Vec<_>>(),
            [
                span(0),
                span(1),
                span(2),
                span(3),
                span(4),
                span(5),
                span(6),
                span(7),
                span(8),
                span(9),
                span(10),
                span(11),
                span(12),
                span(13),
                span(14),
                span(15),
            ]
        );
        assert!(matches!(
            &expressions[2].kind,
            AstExprKind::Integer(value) if value == "42"
        ));
        assert!(matches!(
            &expressions[5].kind,
            AstExprKind::Text(value) if value == "owned text"
        ));
        assert!(matches!(
            &expressions[6].kind,
            AstExprKind::Blob(value) if value == &[0x00, 0xff]
        ));
        assert!(matches!(
            &expressions[7].kind,
            AstExprKind::Column(parts) if parts[0].as_str() == "AMOUNT"
        ));
        assert!(matches!(
            &expressions[8].kind,
            AstExprKind::Parameter(ParameterKey::Name(name)) if name == "AMOUNT"
        ));

        assert_eq!(
            [UnaryOp::Positive, UnaryOp::Negative, UnaryOp::Not].map(unary_tag),
            [0, 1, 2]
        );
        assert_eq!(
            [
                BinaryOp::Add,
                BinaryOp::Subtract,
                BinaryOp::Multiply,
                BinaryOp::Divide,
                BinaryOp::Modulo,
                BinaryOp::Equal,
                BinaryOp::NotEqual,
                BinaryOp::LessThan,
                BinaryOp::LessThanOrEqual,
                BinaryOp::GreaterThan,
                BinaryOp::GreaterThanOrEqual,
                BinaryOp::And,
                BinaryOp::Or,
            ]
            .map(binary_tag),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        assert_eq!(
            [
                AggregateFunction::Count,
                AggregateFunction::Sum,
                AggregateFunction::Avg,
                AggregateFunction::Min,
                AggregateFunction::Max,
            ]
            .map(aggregate_function_tag),
            [0, 1, 2, 3, 4]
        );

        let arguments = [
            AggregateArgument::Wildcard(span(94)),
            AggregateArgument::Expr(Box::new(leaf(95))),
        ];
        assert_eq!(arguments.each_ref().map(aggregate_argument_tag), [0, 1]);
        assert_eq!(
            arguments.each_ref().map(|argument| argument.span()),
            [span(94), span(95)]
        );
    }
}
