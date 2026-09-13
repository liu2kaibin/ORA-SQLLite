use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    fmt,
    hash::{Hash, Hasher},
};

use super::{Locations, invalid_plan};
use crate::{
    Result, Value,
    api::{ParameterIdentifier, ParameterMetadata},
    error::SqlLocation,
    sql::{
        SqlType,
        bound::{
            BoundAggregateFunction, BoundBinaryOp, BoundExpr, BoundExprKind, BoundUnaryOp, SourceId,
        },
    },
};

pub(crate) struct ExecExpr {
    kind: ExecExprKind,
    data_type: SqlType,
    nullable: bool,
    location: SqlLocation,
}

impl ExecExpr {
    pub(crate) fn kind(&self) -> &ExecExprKind {
        &self.kind
    }
    pub(crate) const fn data_type(&self) -> SqlType {
        self.data_type
    }
    pub(crate) const fn nullable(&self) -> bool {
        self.nullable
    }
    pub(crate) const fn location(&self) -> SqlLocation {
        self.location
    }

    fn detach_children(&mut self, work: &mut Vec<Self>) {
        match std::mem::replace(&mut self.kind, ExecExprKind::Literal(Value::Null)) {
            ExecExprKind::Unary { expr, .. } | ExecExprKind::IsNull { expr, .. } => {
                work.push(*expr)
            }
            ExecExprKind::Binary { left, right, .. } => {
                work.push(*left);
                work.push(*right);
            }
            ExecExprKind::InList { expr, values, .. } => {
                work.push(*expr);
                work.extend(values);
            }
            ExecExprKind::Between {
                expr, low, high, ..
            } => {
                work.push(*expr);
                work.push(*low);
                work.push(*high);
            }
            ExecExprKind::Like { expr, pattern, .. } => {
                work.push(*expr);
                work.push(*pattern);
            }
            ExecExprKind::Literal(_)
            | ExecExprKind::Column { .. }
            | ExecExprKind::Parameter { .. }
            | ExecExprKind::Aggregate { .. } => {}
        }
    }
}

impl Drop for ExecExpr {
    fn drop(&mut self) {
        let mut work = Vec::new();
        self.detach_children(&mut work);
        while let Some(mut child) = work.pop() {
            child.detach_children(&mut work);
        }
    }
}

impl fmt::Debug for ExecExpr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ExecExpr").finish_non_exhaustive()
    }
}

pub(crate) enum ExecExprKind {
    Literal(Value),
    Column {
        source: SourceId,
        ordinal: u16,
    },
    Parameter {
        slot: u16,
    },
    Unary {
        op: BoundUnaryOp,
        expr: Box<ExecExpr>,
    },
    Binary {
        left: Box<ExecExpr>,
        op: BoundBinaryOp,
        right: Box<ExecExpr>,
    },
    IsNull {
        expr: Box<ExecExpr>,
        negated: bool,
    },
    InList {
        expr: Box<ExecExpr>,
        values: Box<[ExecExpr]>,
        negated: bool,
    },
    Between {
        expr: Box<ExecExpr>,
        low: Box<ExecExpr>,
        high: Box<ExecExpr>,
        negated: bool,
    },
    Like {
        expr: Box<ExecExpr>,
        pattern: Box<ExecExpr>,
        negated: bool,
    },
    Aggregate {
        slot: u32,
    },
}

pub(crate) struct AggregateDefinition {
    function: BoundAggregateFunction,
    argument: Option<ExecExpr>,
    data_type: SqlType,
    nullable: bool,
    location: SqlLocation,
}

impl AggregateDefinition {
    pub(crate) const fn function(&self) -> BoundAggregateFunction {
        self.function
    }
    pub(crate) fn argument(&self) -> Option<&ExecExpr> {
        self.argument.as_ref()
    }
    pub(crate) const fn data_type(&self) -> SqlType {
        self.data_type
    }
    pub(crate) const fn nullable(&self) -> bool {
        self.nullable
    }
    pub(crate) const fn location(&self) -> SqlLocation {
        self.location
    }
}

pub(super) struct ExprCompiler<'bound, 'map, 'sql> {
    parameters: HashMap<&'bound ParameterIdentifier, u16>,
    locations: &'map Locations<'sql>,
    aggregates: Vec<AggregateDefinition>,
    aggregate_buckets: HashMap<u64, Vec<usize>>,
}

impl<'bound, 'map, 'sql> ExprCompiler<'bound, 'map, 'sql> {
    pub(super) fn new(
        parameters: &'bound [ParameterMetadata],
        locations: &'map Locations<'sql>,
    ) -> Result<Self> {
        let mut slots = HashMap::new();
        for (slot, parameter) in parameters.iter().enumerate() {
            let slot = u16::try_from(slot)
                .map_err(|_| invalid_plan("parameter slot exceeds its bound"))?;
            if slots.insert(parameter.identifier(), slot).is_some() {
                return Err(invalid_plan("duplicate bound parameter metadata"));
            }
        }
        Ok(Self {
            parameters: slots,
            locations,
            aggregates: Vec::new(),
            aggregate_buckets: HashMap::new(),
        })
    }

    pub(super) fn location(&self, span: crate::sql::SqlSpan) -> Result<SqlLocation> {
        self.locations.resolve(span)
    }

    pub(super) fn take_aggregates(&mut self) -> Box<[AggregateDefinition]> {
        self.aggregate_buckets.clear();
        std::mem::take(&mut self.aggregates).into_boxed_slice()
    }

    /// Postorder on an explicit stack preserves operand order without recursion.
    pub(super) fn compile(&mut self, root: &BoundExpr) -> Result<ExecExpr> {
        let mut work = vec![(root, false)];
        let mut values: Vec<ExecExpr> = Vec::new();
        while let Some((bound, ready)) = work.pop() {
            if !ready {
                work.push((bound, true));
                match &bound.kind {
                    BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                        work.push((expr, false))
                    }
                    BoundExprKind::Binary { left, right, .. } => {
                        work.push((right, false));
                        work.push((left, false));
                    }
                    BoundExprKind::InList { expr, values, .. } => {
                        work.extend(values.iter().rev().map(|value| (value, false)));
                        work.push((expr, false));
                    }
                    BoundExprKind::Between {
                        expr, low, high, ..
                    } => {
                        work.push((high, false));
                        work.push((low, false));
                        work.push((expr, false));
                    }
                    BoundExprKind::Like { expr, pattern, .. } => {
                        work.push((pattern, false));
                        work.push((expr, false));
                    }
                    BoundExprKind::Aggregate { argument, .. } => {
                        if let Some(argument) = argument {
                            work.push((argument, false));
                        }
                    }
                    BoundExprKind::Literal(_)
                    | BoundExprKind::Column { .. }
                    | BoundExprKind::NewTableColumn { .. }
                    | BoundExprKind::Parameter(_) => {}
                }
                continue;
            }
            let location = self.location(bound.span)?;
            let kind = match &bound.kind {
                BoundExprKind::Literal(value) => ExecExprKind::Literal(value.clone()),
                BoundExprKind::Column {
                    source_id, ordinal, ..
                } => ExecExprKind::Column {
                    source: *source_id,
                    ordinal: *ordinal,
                },
                BoundExprKind::NewTableColumn { .. } => {
                    return Err(invalid_plan(
                        "new-table columns require deferred constraint planning",
                    ));
                }
                BoundExprKind::Parameter(identifier) => ExecExprKind::Parameter {
                    slot: *self
                        .parameters
                        .get(identifier)
                        .ok_or_else(|| invalid_plan("bound parameter metadata is missing"))?,
                },
                BoundExprKind::Unary { op, .. } => ExecExprKind::Unary {
                    op: *op,
                    expr: pop(&mut values)?,
                },
                BoundExprKind::Binary { op, .. } => {
                    let right = pop(&mut values)?;
                    let left = pop(&mut values)?;
                    ExecExprKind::Binary {
                        left,
                        op: *op,
                        right,
                    }
                }
                BoundExprKind::IsNull { negated, .. } => ExecExprKind::IsNull {
                    expr: pop(&mut values)?,
                    negated: *negated,
                },
                BoundExprKind::InList {
                    values: list,
                    negated,
                    ..
                } => {
                    let start = values
                        .len()
                        .checked_sub(list.len())
                        .ok_or_else(|| invalid_plan("expression operand stack is incomplete"))?;
                    let list = values.split_off(start).into_boxed_slice();
                    ExecExprKind::InList {
                        expr: pop(&mut values)?,
                        values: list,
                        negated: *negated,
                    }
                }
                BoundExprKind::Between { negated, .. } => {
                    let high = pop(&mut values)?;
                    let low = pop(&mut values)?;
                    ExecExprKind::Between {
                        expr: pop(&mut values)?,
                        low,
                        high,
                        negated: *negated,
                    }
                }
                BoundExprKind::Like { negated, .. } => {
                    let pattern = pop(&mut values)?;
                    ExecExprKind::Like {
                        expr: pop(&mut values)?,
                        pattern,
                        negated: *negated,
                    }
                }
                BoundExprKind::Aggregate { function, argument } => {
                    let argument = if argument.is_some() {
                        Some(*pop(&mut values)?)
                    } else {
                        None
                    };
                    let definition = AggregateDefinition {
                        function: *function,
                        argument,
                        data_type: bound.sql_type,
                        nullable: bound.nullable,
                        location,
                    };
                    ExecExprKind::Aggregate {
                        slot: self.aggregate_slot(definition)?,
                    }
                }
            };
            values.push(ExecExpr {
                kind,
                data_type: bound.sql_type,
                nullable: bound.nullable,
                location,
            });
        }
        if values.len() != 1 {
            return Err(invalid_plan("expression operand stack is incomplete"));
        }
        Ok(*pop(&mut values)?)
    }

    fn aggregate_slot(&mut self, definition: AggregateDefinition) -> Result<u32> {
        let key = aggregate_key(&definition);
        if let Some(bucket) = self.aggregate_buckets.get(&key) {
            for index in bucket {
                let existing = &self.aggregates[*index];
                if existing.function == definition.function
                    && existing.data_type == definition.data_type
                    && existing.nullable == definition.nullable
                    && match (&existing.argument, &definition.argument) {
                        (None, None) => true,
                        (Some(left), Some(right)) => same_expression(left, right),
                        _ => false,
                    }
                {
                    return u32::try_from(*index)
                        .map_err(|_| invalid_plan("aggregate slot exceeds its bound"));
                }
            }
        }
        let index = self.aggregates.len();
        let slot =
            u32::try_from(index).map_err(|_| invalid_plan("aggregate slot exceeds its bound"))?;
        self.aggregates.push(definition);
        self.aggregate_buckets.entry(key).or_default().push(index);
        Ok(slot)
    }
}

fn pop(values: &mut Vec<ExecExpr>) -> Result<Box<ExecExpr>> {
    values
        .pop()
        .map(Box::new)
        .ok_or_else(|| invalid_plan("expression operand stack is incomplete"))
}

/// Locations are deliberately excluded; occurrences retain their own locations.
fn aggregate_key(definition: &AggregateDefinition) -> u64 {
    let mut hash = DefaultHasher::new();
    std::mem::discriminant(&definition.function).hash(&mut hash);
    definition.data_type.hash(&mut hash);
    definition.nullable.hash(&mut hash);
    definition.argument.is_some().hash(&mut hash);
    let mut work: Vec<&ExecExpr> = definition.argument.iter().collect();
    while let Some(expr) = work.pop() {
        expr.data_type.hash(&mut hash);
        expr.nullable.hash(&mut hash);
        std::mem::discriminant(&expr.kind).hash(&mut hash);
        match &expr.kind {
            ExecExprKind::Literal(value) => hash_value(value, &mut hash),
            ExecExprKind::Column { source, ordinal } => {
                source.hash(&mut hash);
                ordinal.hash(&mut hash);
            }
            ExecExprKind::Parameter { slot } => slot.hash(&mut hash),
            ExecExprKind::Unary { op, expr } => {
                std::mem::discriminant(op).hash(&mut hash);
                work.push(expr);
            }
            ExecExprKind::Binary { left, op, right } => {
                std::mem::discriminant(op).hash(&mut hash);
                work.push(right);
                work.push(left);
            }
            ExecExprKind::IsNull { expr, negated } => {
                negated.hash(&mut hash);
                work.push(expr);
            }
            ExecExprKind::InList {
                expr,
                values,
                negated,
            } => {
                negated.hash(&mut hash);
                values.len().hash(&mut hash);
                work.extend(values.iter().rev());
                work.push(expr);
            }
            ExecExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                negated.hash(&mut hash);
                work.push(high);
                work.push(low);
                work.push(expr);
            }
            ExecExprKind::Like {
                expr,
                pattern,
                negated,
            } => {
                negated.hash(&mut hash);
                work.push(pattern);
                work.push(expr);
            }
            ExecExprKind::Aggregate { slot } => slot.hash(&mut hash),
        }
    }
    hash.finish()
}

fn hash_value(value: &Value, hash: &mut impl Hasher) {
    std::mem::discriminant(value).hash(hash);
    match value {
        Value::Null => {}
        Value::Int64(value) => value.hash(hash),
        Value::Decimal(value) => {
            value.negative().hash(hash);
            value.scale().hash(hash);
            value.magnitude().hash(hash);
        }
        Value::Float64(value) => {
            let bits = if *value == 0.0 { 0 } else { value.to_bits() };
            bits.hash(hash);
        }
        Value::Text(value) => value.hash(hash),
        Value::Blob(value) => value.hash(hash),
        Value::Date(value) => hash_date(*value, hash),
        Value::Timestamp(value) => {
            hash_date(value.date(), hash);
            value.micros().hash(hash);
            value.offset_minutes().hash(hash);
        }
        Value::Bool(value) => value.hash(hash),
    }
}

fn hash_date(value: crate::OracleDate, hash: &mut impl Hasher) {
    (
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
    )
        .hash(hash);
}

fn same_expression(left: &ExecExpr, right: &ExecExpr) -> bool {
    let mut work = vec![(left, right)];
    while let Some((left, right)) = work.pop() {
        if left.data_type != right.data_type || left.nullable != right.nullable {
            return false;
        }
        match &left.kind {
            ExecExprKind::Literal(left) => {
                let ExecExprKind::Literal(right) = &right.kind else {
                    return false;
                };
                if left != right {
                    return false;
                }
            }
            ExecExprKind::Column { source, ordinal } => {
                let ExecExprKind::Column {
                    source: other_source,
                    ordinal: other_ordinal,
                } = &right.kind
                else {
                    return false;
                };
                if source != other_source || ordinal != other_ordinal {
                    return false;
                }
            }
            ExecExprKind::Parameter { slot } => {
                let ExecExprKind::Parameter { slot: other } = &right.kind else {
                    return false;
                };
                if slot != other {
                    return false;
                }
            }
            ExecExprKind::Unary { op, expr } => {
                let ExecExprKind::Unary {
                    op: other_op,
                    expr: other_expr,
                } = &right.kind
                else {
                    return false;
                };
                if op != other_op {
                    return false;
                }
                work.push((expr, other_expr));
            }
            ExecExprKind::Binary {
                left,
                op,
                right: child,
            } => {
                let ExecExprKind::Binary {
                    left: other_left,
                    op: other_op,
                    right: other_right,
                } = &right.kind
                else {
                    return false;
                };
                if op != other_op {
                    return false;
                }
                work.push((left, other_left));
                work.push((child, other_right));
            }
            ExecExprKind::IsNull { expr, negated } => {
                let ExecExprKind::IsNull {
                    expr: other_expr,
                    negated: other_negated,
                } = &right.kind
                else {
                    return false;
                };
                if negated != other_negated {
                    return false;
                }
                work.push((expr, other_expr));
            }
            ExecExprKind::InList {
                expr,
                values,
                negated,
            } => {
                let ExecExprKind::InList {
                    expr: other_expr,
                    values: other_values,
                    negated: other_negated,
                } = &right.kind
                else {
                    return false;
                };
                if negated != other_negated || values.len() != other_values.len() {
                    return false;
                }
                work.push((expr, other_expr));
                work.extend(values.iter().zip(other_values));
            }
            ExecExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let ExecExprKind::Between {
                    expr: other_expr,
                    low: other_low,
                    high: other_high,
                    negated: other_negated,
                } = &right.kind
                else {
                    return false;
                };
                if negated != other_negated {
                    return false;
                }
                work.push((expr, other_expr));
                work.push((low, other_low));
                work.push((high, other_high));
            }
            ExecExprKind::Like {
                expr,
                pattern,
                negated,
            } => {
                let ExecExprKind::Like {
                    expr: other_expr,
                    pattern: other_pattern,
                    negated: other_negated,
                } = &right.kind
                else {
                    return false;
                };
                if negated != other_negated {
                    return false;
                }
                work.push((expr, other_expr));
                work.push((pattern, other_pattern));
            }
            ExecExprKind::Aggregate { slot } => {
                let ExecExprKind::Aggregate { slot: other } = &right.kind else {
                    return false;
                };
                if slot != other {
                    return false;
                }
            }
        }
    }
    true
}
