use super::{
    AggregateDefinition, ExecExpr, IndexAccessPlan, expr::ExprCompiler, index::choose_index,
};
use crate::{
    Result,
    api::ColumnMetadata,
    sql::{
        SqlType,
        ast::{JoinKind, NullPlacement, OrderDirection},
        bound::{
            BoundAggregateFunction, BoundBinaryOp, BoundExprKind, BoundOrderExpression, BoundQuery,
            BoundSource, BoundUnaryOp, SchemaDependency, SourceId,
        },
    },
};

pub(crate) struct QueryPlan {
    sources: Box<[BoundSource]>,
    joins: Box<[JoinPlan]>,
    selection: Option<ExecExpr>,
    index_access: Option<IndexAccessPlan>,
    group_by: Box<[ExecExpr]>,
    aggregates: Box<[AggregateDefinition]>,
    having: Option<ExecExpr>,
    projection: Box<[ExecExpr]>,
    distinct: bool,
    order_by: Box<[OrderItem]>,
    columns: Box<[ColumnMetadata]>,
    dependencies: Box<[SchemaDependency]>,
}

impl QueryPlan {
    pub(super) fn from_bound(
        bound: &BoundQuery,
        expressions: &mut ExprCompiler<'_, '_, '_>,
    ) -> Result<Self> {
        // Stable aggregate slot order: projection left-to-right, then remaining
        // clauses in logical order. ORDER BY projection references keep indices.
        let projection = bound
            .projection
            .iter()
            .map(|expr| expressions.compile(expr))
            .collect::<Result<_>>()?;
        let joins = bound
            .joins
            .iter()
            .map(|join| {
                Ok(JoinPlan {
                    kind: join.kind,
                    source_id: join.source_id,
                    condition: expressions.compile(&join.condition)?,
                })
            })
            .collect::<Result<_>>()?;
        let selection = bound
            .selection
            .as_ref()
            .map(|expr| expressions.compile(expr))
            .transpose()?;
        let index_access = if bound.sources.len() == 1
            && bound.joins.is_empty()
            && index_scan_preserves_expression_evaluation(bound)
        {
            match bound.selection.as_ref() {
                Some(selection) => choose_index(&bound.sources[0], selection)
                    .map(|choice| IndexAccessPlan::from_choice(choice, expressions))
                    .transpose()?,
                None => None,
            }
        } else {
            None
        };
        let group_by = bound
            .group_by
            .iter()
            .map(|expr| expressions.compile(expr))
            .collect::<Result<_>>()?;
        let having = bound
            .having
            .as_ref()
            .map(|expr| expressions.compile(expr))
            .transpose()?;
        let order_by = bound
            .order_by
            .iter()
            .map(|item| {
                let expression = match &item.expression {
                    BoundOrderExpression::Projection(index) => OrderExpression::Projection(*index),
                    BoundOrderExpression::Expression(expr) => {
                        OrderExpression::Expression(expressions.compile(expr)?)
                    }
                };
                Ok(OrderItem {
                    expression,
                    direction: item.direction,
                    nulls: item.nulls,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            sources: bound.sources.as_slice().into(),
            joins,
            selection,
            index_access,
            group_by,
            aggregates: expressions.take_aggregates(),
            having,
            projection,
            distinct: bound.distinct,
            order_by,
            columns: bound.columns.as_slice().into(),
            dependencies: bound.dependencies.as_slice().into(),
        })
    }

    pub(crate) fn sources(&self) -> &[BoundSource] {
        &self.sources
    }
    pub(crate) fn joins(&self) -> &[JoinPlan] {
        &self.joins
    }
    pub(crate) fn selection(&self) -> Option<&ExecExpr> {
        self.selection.as_ref()
    }
    pub(crate) fn index_access(&self) -> Option<&IndexAccessPlan> {
        self.index_access.as_ref()
    }
    pub(crate) fn group_by(&self) -> &[ExecExpr] {
        &self.group_by
    }
    pub(crate) fn aggregates(&self) -> &[AggregateDefinition] {
        &self.aggregates
    }
    pub(crate) fn having(&self) -> Option<&ExecExpr> {
        self.having.as_ref()
    }
    pub(crate) fn projection(&self) -> &[ExecExpr] {
        &self.projection
    }
    pub(crate) const fn distinct(&self) -> bool {
        self.distinct
    }
    pub(crate) fn order_by(&self) -> &[OrderItem] {
        &self.order_by
    }
    pub(crate) fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }
    pub(crate) fn dependencies(&self) -> &[SchemaDependency] {
        &self.dependencies
    }
}

fn index_scan_preserves_expression_evaluation(bound: &BoundQuery) -> bool {
    // WHERE is eager: skipped rows can hide arithmetic errors. Reordering
    // candidates can also change projection rows emitted before an error, or
    // SUM/AVG overflow and rounding. Keep ROWID order for these computations,
    // including expressions nested inside otherwise safe residual predicates.
    let mut work: Vec<_> = bound
        .projection
        .iter()
        .chain(bound.selection.iter())
        .chain(bound.group_by.iter())
        .chain(bound.having.iter())
        .collect();
    work.extend(
        bound
            .order_by
            .iter()
            .map(|item| item.expression.expression(&bound.projection)),
    );
    while let Some(expression) = work.pop() {
        match &expression.kind {
            BoundExprKind::Unary { op, expr } => match op {
                BoundUnaryOp::Negative => return false,
                BoundUnaryOp::Positive if expr.sql_type == SqlType::Unknown => return false,
                BoundUnaryOp::Positive | BoundUnaryOp::Not => work.push(expr),
            },
            BoundExprKind::Binary { left, op, right } => match op {
                BoundBinaryOp::Add
                | BoundBinaryOp::Subtract
                | BoundBinaryOp::Multiply
                | BoundBinaryOp::Divide
                | BoundBinaryOp::Modulo => return false,
                BoundBinaryOp::Equal
                | BoundBinaryOp::NotEqual
                | BoundBinaryOp::LessThan
                | BoundBinaryOp::LessThanOrEqual
                | BoundBinaryOp::GreaterThan
                | BoundBinaryOp::GreaterThanOrEqual => {
                    // Unknown parameters are validated independently; binding
                    // has not proved their runtime comparison types compatible.
                    if left.sql_type == SqlType::Unknown || right.sql_type == SqlType::Unknown {
                        return false;
                    }
                    work.push(left);
                    work.push(right);
                }
                BoundBinaryOp::And | BoundBinaryOp::Or => {
                    work.push(left);
                    work.push(right);
                }
            },
            BoundExprKind::IsNull { expr, .. } => work.push(expr),
            BoundExprKind::InList { expr, values, .. } => {
                if expr.sql_type == SqlType::Unknown
                    || values
                        .iter()
                        .any(|value| value.sql_type == SqlType::Unknown)
                {
                    return false;
                }
                work.push(expr);
                work.extend(values);
            }
            BoundExprKind::Between {
                expr, low, high, ..
            } => {
                if [expr, low, high]
                    .iter()
                    .any(|operand| operand.sql_type == SqlType::Unknown)
                {
                    return false;
                }
                work.push(expr);
                work.push(low);
                work.push(high);
            }
            BoundExprKind::Like { expr, pattern, .. } => {
                work.push(expr);
                work.push(pattern);
            }
            BoundExprKind::Aggregate { function, argument } => match function {
                BoundAggregateFunction::Sum | BoundAggregateFunction::Avg => return false,
                BoundAggregateFunction::Count
                | BoundAggregateFunction::Min
                | BoundAggregateFunction::Max => {
                    if let Some(argument) = argument {
                        work.push(argument);
                    }
                }
            },
            BoundExprKind::Literal(_)
            | BoundExprKind::Column { .. }
            | BoundExprKind::Parameter(_) => {}
            BoundExprKind::NewTableColumn { .. } => return false,
        }
    }
    true
}

pub(crate) struct JoinPlan {
    kind: JoinKind,
    source_id: SourceId,
    condition: ExecExpr,
}

impl JoinPlan {
    pub(crate) const fn kind(&self) -> JoinKind {
        self.kind
    }
    pub(crate) const fn source_id(&self) -> SourceId {
        self.source_id
    }
    pub(crate) fn condition(&self) -> &ExecExpr {
        &self.condition
    }
}

pub(crate) struct OrderItem {
    expression: OrderExpression,
    direction: OrderDirection,
    nulls: NullPlacement,
}

impl OrderItem {
    pub(crate) fn expression(&self) -> &OrderExpression {
        &self.expression
    }
    pub(crate) const fn direction(&self) -> OrderDirection {
        self.direction
    }
    pub(crate) const fn nulls(&self) -> NullPlacement {
        self.nulls
    }
}

pub(crate) enum OrderExpression {
    Projection(usize),
    Expression(ExecExpr),
}
