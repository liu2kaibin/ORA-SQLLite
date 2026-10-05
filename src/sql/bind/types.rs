use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    DbError, Decimal, ErrorCode, Result, Value,
    api::{ParameterIdentifier, ParameterMetadata},
    oracle_type::OracleTypeDescriptor,
    sql::{
        ParameterKey, SqlSpan, SqlType,
        ast::{AggregateArgument, AggregateFunction, AstExpr, AstExprKind, BinaryOp, UnaryOp},
        bound::{BoundAggregateFunction, BoundBinaryOp, BoundExpr, BoundExprKind, BoundUnaryOp},
    },
    storage::catalog::types::ColumnId,
};

use super::ColumnResolver;

#[derive(Debug)]
pub(crate) struct ParameterCollector {
    entries: Vec<ParameterEntry>,
    positions: HashMap<ParameterIdentifier, usize>,
    // Assignment paths are discovered before normal binding reaches each
    // parameter leaf. Keep their caps pending to preserve actual SQL
    // first-occurrence ordering in `entries`.
    pending_assignment_caps: HashMap<ParameterIdentifier, SqlType>,
    pending_oracle_text_sources: HashSet<ParameterIdentifier>,
    oracle_columns: HashMap<ColumnId, OracleTypeDescriptor>,
    parents: Vec<usize>,
    ranks: Vec<u8>,
    numeric_aggregate_links: HashSet<(usize, usize)>,
    finalized: bool,
    statement_constraints: bool,
    budget: BindingBudget,
}

#[derive(Debug)]
struct BindingBudget {
    limit: usize,
    used: usize,
}

impl Default for ParameterCollector {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            positions: HashMap::new(),
            pending_assignment_caps: HashMap::new(),
            pending_oracle_text_sources: HashSet::new(),
            oracle_columns: HashMap::new(),
            parents: Vec::new(),
            ranks: Vec::new(),
            numeric_aggregate_links: HashSet::new(),
            finalized: false,
            statement_constraints: false,
            budget: BindingBudget {
                limit: crate::SqlLimits::default().max_ast_nodes() as usize,
                used: 0,
            },
        }
    }
}

#[derive(Debug)]
struct ParameterEntry {
    identifier: ParameterIdentifier,
    sql_type: SqlType,
    // The narrowest source type that remains safe for every DML assignment.
    assignment_cap: Option<SqlType>,
    allow_oracle_text_source: bool,
    first_offset: usize,
}

impl ParameterCollector {
    pub(crate) fn defers_grouping(&self) -> bool {
        self.statement_constraints && !self.finalized && !self.entries.is_empty()
    }
    pub(crate) fn with_node_limit(limit: usize) -> Self {
        Self {
            statement_constraints: true,
            budget: BindingBudget { limit, used: 0 },
            ..Self::default()
        }
    }

    pub(crate) fn charge_nodes(&mut self, count: usize) -> Result<()> {
        let used = self
            .budget
            .used
            .checked_add(count)
            .filter(|used| *used <= self.budget.limit)
            .ok_or_else(|| {
                DbError::new(
                    ErrorCode::Bind,
                    "SQL binding exceeds the configured node/expansion budget",
                )
            })?;
        self.budget.used = used;
        Ok(())
    }
    pub(crate) fn constrain(&mut self, key: &ParameterKey, constraint: SqlType) -> Result<SqlType> {
        let identifier = parameter_identifier(key);
        self.constrain_identifier(identifier, constraint)
    }

    fn constrain_assignment(&mut self, key: &ParameterKey, target: SqlType) -> Result<()> {
        self.constrain_assignment_identifier(parameter_identifier(key), target)
    }

    fn constrain_identifier(
        &mut self,
        identifier: ParameterIdentifier,
        constraint: SqlType,
    ) -> Result<SqlType> {
        if self.finalized {
            let index = self.positions.get(&identifier).ok_or_else(|| {
                DbError::new(
                    ErrorCode::Internal,
                    "parameter was not collected before solving",
                )
            })?;
            let sql_type = self.entries[*index].sql_type;
            // A context may promote INT64 to DECIMAL, but must never rewrite
            // the solved parameter type retained by other occurrences.
            sql_type.unify(constraint)?;
            return Ok(sql_type);
        }
        if let Some(index) = self.positions.get(&identifier).copied() {
            let entry = &mut self.entries[index];
            let inferred = entry.sql_type.unify(constraint)?;
            entry.sql_type = match entry.assignment_cap {
                Some(assignment_cap) => {
                    parameter_type_with_assignment_cap(inferred, assignment_cap)?
                }
                None => inferred,
            };
            return Ok(entry.sql_type);
        }
        let assignment_cap = self.pending_assignment_caps.remove(&identifier);
        let allow_oracle_text_source = self.pending_oracle_text_sources.remove(&identifier);
        let sql_type = match assignment_cap {
            Some(assignment_cap) => parameter_type_with_assignment_cap(constraint, assignment_cap)?,
            None => constraint,
        };
        self.positions
            .insert(identifier.clone(), self.entries.len());
        self.parents.push(self.entries.len());
        self.ranks.push(0);
        self.entries.push(ParameterEntry {
            identifier,
            sql_type,
            assignment_cap,
            allow_oracle_text_source,
            first_offset: usize::MAX,
        });
        Ok(sql_type)
    }

    fn allow_oracle_text_source(&mut self, identifier: ParameterIdentifier) {
        if let Some(index) = self.positions.get(&identifier).copied() {
            self.entries[index].allow_oracle_text_source = true;
        } else {
            self.pending_oracle_text_sources.insert(identifier);
        }
    }

    fn register_oracle_column(
        &mut self,
        column_id: ColumnId,
        descriptor: Option<OracleTypeDescriptor>,
    ) {
        if let Some(descriptor) = descriptor {
            self.oracle_columns.insert(column_id, descriptor);
        }
    }

    fn allows_oracle_text_source_for_column(&self, column_id: ColumnId) -> bool {
        matches!(
            self.oracle_columns.get(&column_id),
            Some(
                OracleTypeDescriptor::Number { .. }
                    | OracleTypeDescriptor::Date
                    | OracleTypeDescriptor::Timestamp { .. }
                    | OracleTypeDescriptor::TimestampWithTimeZone { .. }
            )
        )
    }

    fn constrain_assignment_identifier(
        &mut self,
        identifier: ParameterIdentifier,
        target: SqlType,
    ) -> Result<()> {
        if self.finalized {
            let sql_type = self.constrain_identifier(identifier, SqlType::Unknown)?;
            return if assignment_type_is_compatible(sql_type, target) {
                Ok(())
            } else {
                Err(type_error(
                    "solved parameter is not compatible with the assignment target",
                ))
            };
        }
        if let Some(index) = self.positions.get(&identifier).copied() {
            let entry = &mut self.entries[index];
            let assignment_cap = match entry.assignment_cap {
                Some(current) => shared_assignment_parameter_type(current, target)?,
                None => target,
            };
            entry.assignment_cap = Some(assignment_cap);
            entry.sql_type = parameter_type_with_assignment_cap(entry.sql_type, assignment_cap)?;
            return Ok(());
        }
        if let Some(current) = self.pending_assignment_caps.get_mut(&identifier) {
            *current = shared_assignment_parameter_type(*current, target)?;
        } else {
            self.pending_assignment_caps.insert(identifier, target);
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Vec<ParameterMetadata> {
        if self
            .entries
            .iter()
            .all(|entry| matches!(entry.identifier, ParameterIdentifier::Position(_)))
        {
            self.entries.sort_by_key(|entry| match entry.identifier {
                ParameterIdentifier::Position(position) => position,
                ParameterIdentifier::Name(_) => unreachable!("all entries are positional"),
            });
        } else {
            self.entries.sort_by_key(|entry| entry.first_offset);
        }
        self.entries
            .into_iter()
            .map(|entry| {
                ParameterMetadata::inferred(
                    entry.identifier,
                    entry.sql_type,
                    entry.allow_oracle_text_source,
                )
            })
            .collect()
    }

    fn root(&mut self, index: usize) -> usize {
        let mut root = index;
        while self.parents[root] != root {
            root = self.parents[root];
        }
        let mut at = index;
        while self.parents[at] != at {
            let next = self.parents[at];
            self.parents[at] = root;
            at = next;
        }
        root
    }

    fn link_common<'a>(
        &mut self,
        expressions: impl IntoIterator<Item = &'a BoundExpr>,
    ) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        let mut first = None;
        let mut derived = Vec::new();
        let mut work: Vec<_> = expressions.into_iter().map(|expr| (expr, false)).collect();
        while let Some((expression, aggregate_result)) = work.pop() {
            match &expression.kind {
                BoundExprKind::Parameter(identifier) => {
                    let index = self.positions[identifier];
                    if aggregate_result {
                        derived.push(index);
                        continue;
                    }
                    let mut right = self.root(index);
                    if let Some(first) = first {
                        let mut left = self.root(first);
                        if left != right {
                            if self.ranks[left] < self.ranks[right] {
                                std::mem::swap(&mut left, &mut right);
                            }
                            self.parents[right] = left;
                            if self.ranks[left] == self.ranks[right] {
                                self.ranks[left] += 1;
                            }
                        }
                    } else {
                        first = Some(index);
                    }
                }
                BoundExprKind::Unary {
                    op: BoundUnaryOp::Positive | BoundUnaryOp::Negative,
                    expr,
                } => work.push((expr, aggregate_result)),
                BoundExprKind::Binary {
                    op:
                        BoundBinaryOp::Add
                        | BoundBinaryOp::Subtract
                        | BoundBinaryOp::Multiply
                        | BoundBinaryOp::Divide
                        | BoundBinaryOp::Modulo,
                    left,
                    right,
                } => {
                    work.push((left, aggregate_result));
                    work.push((right, aggregate_result));
                }
                BoundExprKind::Aggregate {
                    function: BoundAggregateFunction::Min | BoundAggregateFunction::Max,
                    argument: Some(argument),
                } => work.push((argument, aggregate_result)),
                BoundExprKind::Aggregate {
                    function: BoundAggregateFunction::Sum | BoundAggregateFunction::Avg,
                    argument: Some(argument),
                } => work.push((argument, true)),
                _ => {}
            }
        }
        if let Some(target) = first {
            for source in derived {
                if !self.numeric_aggregate_links.contains(&(source, target)) {
                    if self.numeric_aggregate_links.len() >= self.budget.limit {
                        return Err(DbError::new(
                            ErrorCode::Bind,
                            "SQL parameter constraints exceed the binding budget",
                        ));
                    }
                    self.numeric_aggregate_links.insert((source, target));
                }
            }
        }
        Ok(())
    }

    /// Solve linked parameter constraints once, then finalize by binding every
    /// expression again with the solution, rechecking operator domains and DML
    /// assignment direction and rebuilding all public output metadata.
    pub(crate) fn solve(&mut self) -> Result<bool> {
        if self.finalized {
            return Ok(!self.entries.is_empty());
        }
        if self.entries.is_empty() {
            self.finalized = true;
            return Ok(false);
        }
        let mut common = vec![SqlType::Unknown; self.entries.len()];
        for index in 0..self.entries.len() {
            let root = self.root(index);
            common[root] = common[root].unify(self.entries[index].sql_type)?;
        }
        // SUM/AVG output types are directed constraints, not equality links:
        // an INT64 input has a DECIMAL result. A worklist propagates these across
        // query clauses before final binding so earlier outputs cannot stay stale.
        let mut outgoing = vec![Vec::new(); self.entries.len()];
        let links: Vec<_> = self.numeric_aggregate_links.iter().copied().collect();
        for (source, target) in links {
            let source = self.root(source);
            let target = self.root(target);
            outgoing[source].push(target);
        }
        let mut queue: VecDeque<_> = (0..self.entries.len()).collect();
        let mut queued = vec![true; self.entries.len()];
        while let Some(source) = queue.pop_front() {
            queued[source] = false;
            if outgoing[source].is_empty() {
                continue;
            }
            let result = match common[source] {
                SqlType::Int64 | SqlType::Decimal => SqlType::Decimal,
                SqlType::Float64 => SqlType::Float64,
                SqlType::Null | SqlType::Unknown => continue,
                _ => return Err(type_error("SUM and AVG require a numeric input")),
            };
            for &target in &outgoing[source] {
                let inferred = common[target].unify(result)?;
                if inferred != common[target] {
                    common[target] = inferred;
                    if !queued[target] {
                        queue.push_back(target);
                        queued[target] = true;
                    }
                }
            }
        }
        for index in 0..self.entries.len() {
            let root = self.root(index);
            // Links share expression context, not directional write restrictions.
            // Apply only this identifier's own cap to the component solution.
            self.entries[index].sql_type = match self.entries[index].assignment_cap {
                Some(cap) => parameter_type_with_assignment_cap(common[root], cap)?,
                None => common[root],
            };
        }
        self.finalized = true;
        self.budget.used = 0;
        Ok(true)
    }
}

pub(crate) fn bind_expr(
    expr: &AstExpr,
    scope: &impl ColumnResolver,
    expected: Option<SqlType>,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    let mut work = vec![BindFrame::Visit(expr)];
    let mut values = Vec::new();
    while let Some(frame) = work.pop() {
        match frame {
            BindFrame::Visit(expr) => {
                context.charge_nodes(1)?;
                match &expr.kind {
                    AstExprKind::Null => values.push(BoundExpr::new(
                        BoundExprKind::Literal(Value::Null),
                        SqlType::Null,
                        true,
                        expr.span,
                    )),
                    AstExprKind::Bool(value) => values.push(BoundExpr::new(
                        BoundExprKind::Literal(Value::Bool(*value)),
                        SqlType::Bool,
                        false,
                        expr.span,
                    )),
                    AstExprKind::Integer(value) => {
                        let (value, sql_type) = parse_integer(value)?;
                        values.push(BoundExpr::new(
                            BoundExprKind::Literal(value),
                            sql_type,
                            false,
                            expr.span,
                        ));
                    }
                    AstExprKind::Decimal(value) => values.push(BoundExpr::new(
                        BoundExprKind::Literal(Value::Decimal(parse_decimal(value)?)),
                        SqlType::Decimal,
                        false,
                        expr.span,
                    )),
                    AstExprKind::Float(value) => values.push(BoundExpr::new(
                        BoundExprKind::Literal(Value::Float64(parse_float(value)?)),
                        SqlType::Float64,
                        false,
                        expr.span,
                    )),
                    AstExprKind::Text(value) => values.push(BoundExpr::new(
                        BoundExprKind::Literal(Value::Text(value.clone())),
                        SqlType::Text,
                        false,
                        expr.span,
                    )),
                    AstExprKind::Blob(value) => values.push(BoundExpr::new(
                        BoundExprKind::Literal(Value::Blob(value.clone())),
                        SqlType::Blob,
                        false,
                        expr.span,
                    )),
                    AstExprKind::Column(parts) => {
                        let column = scope.resolve_column(parts)?;
                        context.register_oracle_column(column.column_id, column.oracle_type);
                        values.push(BoundExpr::new(
                            BoundExprKind::Column {
                                source_id: column.source_id,
                                table_id: column.table_id,
                                column_id: column.column_id,
                                ordinal: column.ordinal,
                            },
                            column.sql_type,
                            column.nullable,
                            expr.span,
                        ));
                    }
                    AstExprKind::Parameter(key) => {
                        let sql_type = context.constrain(key, SqlType::Unknown)?;
                        if !context.finalized {
                            let index = context.positions[&parameter_identifier(key)];
                            context.entries[index].first_offset =
                                context.entries[index].first_offset.min(expr.span.start());
                        }
                        values.push(BoundExpr::new(
                            BoundExprKind::Parameter(parameter_identifier(key)),
                            sql_type,
                            true,
                            expr.span,
                        ));
                    }
                    AstExprKind::Unary { op, expr: operand } => {
                        work.push(BindFrame::FinishUnary {
                            span: expr.span,
                            op: *op,
                        });
                        work.push(BindFrame::Visit(operand));
                    }
                    AstExprKind::Binary { left, op, right } => {
                        work.push(BindFrame::FinishBinary {
                            span: expr.span,
                            op: *op,
                        });
                        work.push(BindFrame::Visit(right));
                        work.push(BindFrame::Visit(left));
                    }
                    AstExprKind::IsNull {
                        expr: operand,
                        negated,
                    } => {
                        work.push(BindFrame::FinishIsNull {
                            span: expr.span,
                            negated: *negated,
                        });
                        work.push(BindFrame::Visit(operand));
                    }
                    AstExprKind::InList {
                        expr: operand,
                        values: list,
                        negated,
                    } => {
                        work.push(BindFrame::FinishInList {
                            span: expr.span,
                            value_count: list.len(),
                            negated: *negated,
                        });
                        for value in list.iter().rev() {
                            work.push(BindFrame::Visit(value));
                        }
                        work.push(BindFrame::Visit(operand));
                    }
                    AstExprKind::Between {
                        expr: operand,
                        low,
                        high,
                        negated,
                    } => {
                        work.push(BindFrame::FinishBetween {
                            span: expr.span,
                            negated: *negated,
                        });
                        work.push(BindFrame::Visit(high));
                        work.push(BindFrame::Visit(low));
                        work.push(BindFrame::Visit(operand));
                    }
                    AstExprKind::Like {
                        expr: operand,
                        pattern,
                        negated,
                    } => {
                        work.push(BindFrame::FinishLike {
                            span: expr.span,
                            negated: *negated,
                        });
                        work.push(BindFrame::Visit(pattern));
                        work.push(BindFrame::Visit(operand));
                    }
                    AstExprKind::Aggregate { function, argument } => match argument {
                        AggregateArgument::Wildcard(_) => {
                            values.push(finish_aggregate(expr.span, *function, None, false)?)
                        }
                        AggregateArgument::Expr(argument) => {
                            work.push(BindFrame::FinishAggregate {
                                span: expr.span,
                                function: *function,
                            });
                            work.push(BindFrame::Visit(argument));
                        }
                    },
                }
            }
            BindFrame::FinishUnary { span, op } => {
                let mut operand = pop_bound(&mut values)?;
                if op == UnaryOp::Not {
                    apply_expected(&mut operand, SqlType::Bool, context)?;
                }
                let (kind, sql_type, nullable) = bind_unary(op, operand)?;
                values.push(BoundExpr::new(kind, sql_type, nullable, span));
            }
            BindFrame::FinishBinary { span, op } => {
                let right = pop_bound(&mut values)?;
                let left = pop_bound(&mut values)?;
                values.push(finish_binary(span, left, op, right, context)?);
            }
            BindFrame::FinishIsNull { span, negated } => {
                let operand = pop_bound(&mut values)?;
                values.push(BoundExpr::new(
                    BoundExprKind::IsNull {
                        expr: Box::new(operand),
                        negated,
                    },
                    SqlType::Bool,
                    false,
                    span,
                ));
            }
            BindFrame::FinishInList {
                span,
                value_count,
                negated,
            } => {
                let mut expressions = take_bound(&mut values, value_count.saturating_add(1))?;
                apply_common_type(&mut expressions, context)?;
                let operand = expressions.remove(0);
                let nullable = operand.nullable || expressions.iter().any(|value| value.nullable);
                values.push(BoundExpr::new(
                    BoundExprKind::InList {
                        expr: Box::new(operand),
                        values: expressions,
                        negated,
                    },
                    SqlType::Bool,
                    nullable,
                    span,
                ));
            }
            BindFrame::FinishBetween { span, negated } => {
                let mut expressions = take_bound(&mut values, 3)?;
                let common = apply_common_type(&mut expressions, context)?;
                if common == SqlType::Blob {
                    return Err(type_error("Blob supports only equality comparisons"));
                }
                let operand = expressions.remove(0);
                let low = expressions.remove(0);
                let high = expressions.remove(0);
                let nullable = operand.nullable || low.nullable || high.nullable;
                values.push(BoundExpr::new(
                    BoundExprKind::Between {
                        expr: Box::new(operand),
                        low: Box::new(low),
                        high: Box::new(high),
                        negated,
                    },
                    SqlType::Bool,
                    nullable,
                    span,
                ));
            }
            BindFrame::FinishLike { span, negated } => {
                let mut pattern = pop_bound(&mut values)?;
                let mut operand = pop_bound(&mut values)?;
                apply_expected(&mut operand, SqlType::Text, context)?;
                apply_expected(&mut pattern, SqlType::Text, context)?;
                let nullable = operand.nullable || pattern.nullable;
                values.push(BoundExpr::new(
                    BoundExprKind::Like {
                        expr: Box::new(operand),
                        pattern: Box::new(pattern),
                        negated,
                    },
                    SqlType::Bool,
                    nullable,
                    span,
                ));
            }
            BindFrame::FinishAggregate { span, function } => {
                let argument = pop_bound(&mut values)?;
                values.push(finish_aggregate(
                    span,
                    function,
                    Some(argument),
                    context.statement_constraints && !context.finalized,
                )?);
            }
        }
    }
    if values.len() != 1 {
        return Err(bind_internal_error());
    }
    let mut bound = pop_bound(&mut values)?;
    if let Some(expected) = expected {
        apply_expected(&mut bound, expected, context)?;
    }
    Ok(bound)
}

/// Binds an expression that will be stored in a known target column.
///
/// Common-type inference is symmetric for comparisons and arithmetic, but a
/// storage assignment is directional: it may widen INT64 into DECIMAL but it
/// must not narrow DECIMAL into INT64.
pub(crate) fn bind_assignment_expr(
    expr: &AstExpr,
    scope: &impl ColumnResolver,
    target: SqlType,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    bind_assignment_expr_for_column(expr, scope, target, false, context)
}

/// Binds a DML value and records the one Oracle-only exception to ordinary
/// parameter type compatibility. The raw Text remains in the frame so the
/// target column is the sole conversion boundary during mutation.
pub(crate) fn bind_assignment_expr_for_column(
    expr: &AstExpr,
    scope: &impl ColumnResolver,
    target: SqlType,
    allow_oracle_text_source: bool,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    register_assignment_parameter_caps(expr, target, context)?;
    if allow_oracle_text_source {
        register_oracle_text_sources(expr, context);
    }
    let mut expression = bind_expr(expr, scope, None, context)?;
    if !assignment_type_is_compatible(expression.sql_type, target) {
        return Err(type_error(
            "assignment expression is not compatible with the target column type",
        ));
    }
    apply_expected(&mut expression, target, context)?;
    Ok(expression)
}

fn register_oracle_text_sources(expression: &AstExpr, context: &mut ParameterCollector) {
    let mut work = vec![expression];
    while let Some(expression) = work.pop() {
        match &expression.kind {
            AstExprKind::Parameter(key) => {
                context.allow_oracle_text_source(parameter_identifier(key))
            }
            AstExprKind::Unary {
                op: UnaryOp::Positive | UnaryOp::Negative,
                expr,
            } => work.push(expr),
            AstExprKind::Binary {
                left,
                op:
                    BinaryOp::Add
                    | BinaryOp::Subtract
                    | BinaryOp::Multiply
                    | BinaryOp::Divide
                    | BinaryOp::Modulo,
                right,
            } => {
                work.push(right);
                work.push(left);
            }
            AstExprKind::Aggregate {
                function: AggregateFunction::Min | AggregateFunction::Max,
                argument: AggregateArgument::Expr(expr),
                ..
            } => work.push(expr),
            AstExprKind::Null
            | AstExprKind::Bool(_)
            | AstExprKind::Integer(_)
            | AstExprKind::Decimal(_)
            | AstExprKind::Float(_)
            | AstExprKind::Text(_)
            | AstExprKind::Blob(_)
            | AstExprKind::Column(_)
            | AstExprKind::Unary { .. }
            | AstExprKind::Binary { .. }
            | AstExprKind::IsNull { .. }
            | AstExprKind::InList { .. }
            | AstExprKind::Between { .. }
            | AstExprKind::Like { .. }
            | AstExprKind::Aggregate { .. } => {}
        }
    }
}

fn assignment_type_is_compatible(source: SqlType, target: SqlType) -> bool {
    matches!(source, SqlType::Null | SqlType::Unknown)
        || source == target
        || matches!((source, target), (SqlType::Int64, SqlType::Decimal))
}

fn shared_assignment_parameter_type(source: SqlType, target: SqlType) -> Result<SqlType> {
    if assignment_type_is_compatible(source, target) {
        return Ok(source);
    }
    if assignment_type_is_compatible(target, source) {
        return Ok(target);
    }
    Err(type_error(
        "parameter does not have one type safe for every assignment target",
    ))
}

fn parameter_type_with_assignment_cap(
    inferred: SqlType,
    assignment_cap: SqlType,
) -> Result<SqlType> {
    if matches!(inferred, SqlType::Null | SqlType::Unknown) {
        return Ok(assignment_cap);
    }
    if assignment_type_is_compatible(inferred, assignment_cap) {
        return Ok(inferred);
    }
    if assignment_type_is_compatible(assignment_cap, inferred) {
        return Ok(assignment_cap);
    }
    Err(type_error(
        "parameter does not have one type safe for every assignment target",
    ))
}

/// Records only parameter paths whose expression type receives the target
/// assignment context. Other expression forms infer their own input types.
fn register_assignment_parameter_caps(
    expression: &AstExpr,
    target: SqlType,
    context: &mut ParameterCollector,
) -> Result<()> {
    let mut work = vec![expression];
    while let Some(expression) = work.pop() {
        match &expression.kind {
            AstExprKind::Parameter(key) => context.constrain_assignment(key, target)?,
            AstExprKind::Unary {
                op: UnaryOp::Positive | UnaryOp::Negative,
                expr,
            } => work.push(expr),
            AstExprKind::Binary {
                left,
                op:
                    BinaryOp::Add
                    | BinaryOp::Subtract
                    | BinaryOp::Multiply
                    | BinaryOp::Divide
                    | BinaryOp::Modulo,
                right,
            } => {
                work.push(right);
                work.push(left);
            }
            AstExprKind::Aggregate {
                function: AggregateFunction::Min | AggregateFunction::Max,
                argument: AggregateArgument::Expr(argument),
            } => work.push(argument),
            AstExprKind::Null
            | AstExprKind::Bool(_)
            | AstExprKind::Integer(_)
            | AstExprKind::Decimal(_)
            | AstExprKind::Float(_)
            | AstExprKind::Text(_)
            | AstExprKind::Blob(_)
            | AstExprKind::Column(_)
            | AstExprKind::Unary { .. }
            | AstExprKind::Binary { .. }
            | AstExprKind::IsNull { .. }
            | AstExprKind::InList { .. }
            | AstExprKind::Between { .. }
            | AstExprKind::Like { .. }
            | AstExprKind::Aggregate { .. } => {}
        }
    }
    Ok(())
}

enum BindFrame<'a> {
    Visit(&'a AstExpr),
    FinishUnary {
        span: SqlSpan,
        op: UnaryOp,
    },
    FinishBinary {
        span: SqlSpan,
        op: BinaryOp,
    },
    FinishIsNull {
        span: SqlSpan,
        negated: bool,
    },
    FinishInList {
        span: SqlSpan,
        value_count: usize,
        negated: bool,
    },
    FinishBetween {
        span: SqlSpan,
        negated: bool,
    },
    FinishLike {
        span: SqlSpan,
        negated: bool,
    },
    FinishAggregate {
        span: SqlSpan,
        function: AggregateFunction,
    },
}

fn pop_bound(values: &mut Vec<BoundExpr>) -> Result<BoundExpr> {
    values.pop().ok_or_else(bind_internal_error)
}

fn take_bound(values: &mut Vec<BoundExpr>, count: usize) -> Result<Vec<BoundExpr>> {
    let start = values
        .len()
        .checked_sub(count)
        .ok_or_else(bind_internal_error)?;
    Ok(values.split_off(start))
}

fn bind_unary(op: UnaryOp, operand: BoundExpr) -> Result<(BoundExprKind, SqlType, bool)> {
    match op {
        UnaryOp::Not => {
            if operand.sql_type != SqlType::Bool {
                return Err(type_error("NOT requires a Bool operand"));
            }
            let nullable = operand.nullable;
            Ok((
                BoundExprKind::Unary {
                    op: BoundUnaryOp::Not,
                    expr: Box::new(operand),
                },
                SqlType::Bool,
                nullable,
            ))
        }
        UnaryOp::Positive | UnaryOp::Negative => {
            require_numeric_or_indeterminate(operand.sql_type)?;
            let sql_type = operand.sql_type;
            let nullable = operand.nullable;
            Ok((
                BoundExprKind::Unary {
                    op: match op {
                        UnaryOp::Positive => BoundUnaryOp::Positive,
                        UnaryOp::Negative => BoundUnaryOp::Negative,
                        UnaryOp::Not => unreachable!(),
                    },
                    expr: Box::new(operand),
                },
                sql_type,
                nullable,
            ))
        }
    }
}

fn finish_binary(
    span: SqlSpan,
    mut left: BoundExpr,
    op: BinaryOp,
    mut right: BoundExpr,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    if matches!(op, BinaryOp::And | BinaryOp::Or) {
        apply_expected(&mut left, SqlType::Bool, context)?;
        apply_expected(&mut right, SqlType::Bool, context)?;
        let nullable = left.nullable || right.nullable;
        return Ok(BoundExpr::new(
            BoundExprKind::Binary {
                left: Box::new(left),
                op: map_binary_op(op),
                right: Box::new(right),
            },
            SqlType::Bool,
            nullable,
            span,
        ));
    }

    if matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::LessThan
            | BinaryOp::LessThanOrEqual
            | BinaryOp::GreaterThan
            | BinaryOp::GreaterThanOrEqual
    ) {
        mark_oracle_comparison_text_source(&left, &right, context);
        mark_oracle_comparison_text_source(&right, &left, context);
    }
    context.link_common([&left, &right])?;
    let common = left.sql_type.unify(right.sql_type)?;
    apply_expected(&mut left, common, context)?;
    apply_expected(&mut right, common, context)?;

    let comparison = matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::LessThan
            | BinaryOp::LessThanOrEqual
            | BinaryOp::GreaterThan
            | BinaryOp::GreaterThanOrEqual
    );
    let (sql_type, nullable) = if comparison {
        if common == SqlType::Blob && !matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) {
            return Err(type_error("Blob supports only equality comparisons"));
        }
        (
            SqlType::Bool,
            left.nullable || right.nullable || matches!(common, SqlType::Null | SqlType::Unknown),
        )
    } else {
        require_numeric_or_indeterminate(common)?;
        (common, left.nullable || right.nullable)
    };

    Ok(BoundExpr::new(
        BoundExprKind::Binary {
            left: Box::new(left),
            op: map_binary_op(op),
            right: Box::new(right),
        },
        sql_type,
        nullable,
        span,
    ))
}

fn mark_oracle_comparison_text_source(
    column: &BoundExpr,
    parameter: &BoundExpr,
    context: &mut ParameterCollector,
) {
    let BoundExprKind::Column { column_id, .. } = column.kind else {
        return;
    };
    let BoundExprKind::Parameter(ref identifier) = parameter.kind else {
        return;
    };
    if context.allows_oracle_text_source_for_column(column_id) {
        context.allow_oracle_text_source(identifier.clone());
    }
}

fn finish_aggregate(
    span: SqlSpan,
    function: AggregateFunction,
    argument: Option<BoundExpr>,
    defer_unknown: bool,
) -> Result<BoundExpr> {
    let (sql_type, nullable) = match function {
        AggregateFunction::Count => (SqlType::Int64, false),
        AggregateFunction::Sum | AggregateFunction::Avg => {
            let input = argument
                .as_ref()
                .ok_or_else(|| type_error("only COUNT accepts a wildcard argument"))?;
            match input.sql_type {
                SqlType::Int64 | SqlType::Decimal => (SqlType::Decimal, true),
                SqlType::Float64 => (SqlType::Float64, true),
                SqlType::Unknown if defer_unknown => (SqlType::Unknown, true),
                _ => return Err(type_error("SUM and AVG require a numeric input")),
            }
        }
        AggregateFunction::Min | AggregateFunction::Max => {
            let input = argument
                .as_ref()
                .ok_or_else(|| type_error("only COUNT accepts a wildcard argument"))?;
            (input.sql_type, true)
        }
    };
    Ok(BoundExpr::new(
        BoundExprKind::Aggregate {
            function: match function {
                AggregateFunction::Count => BoundAggregateFunction::Count,
                AggregateFunction::Sum => BoundAggregateFunction::Sum,
                AggregateFunction::Avg => BoundAggregateFunction::Avg,
                AggregateFunction::Min => BoundAggregateFunction::Min,
                AggregateFunction::Max => BoundAggregateFunction::Max,
            },
            argument: argument.map(Box::new),
        },
        sql_type,
        nullable,
        span,
    ))
}

fn apply_common_type(
    expressions: &mut [BoundExpr],
    context: &mut ParameterCollector,
) -> Result<SqlType> {
    context.link_common(expressions.iter())?;
    let mut common = SqlType::Unknown;
    for expression in expressions.iter() {
        common = common.unify(expression.sql_type)?;
    }
    for expression in expressions {
        apply_expected(expression, common, context)?;
    }
    Ok(common)
}

fn apply_expected(
    expression: &mut BoundExpr,
    expected: SqlType,
    context: &mut ParameterCollector,
) -> Result<()> {
    let mut work = vec![(expression, expected)];
    while let Some((expression, expected)) = work.pop() {
        if matches!(
            &expression.kind,
            BoundExprKind::Unary {
                op: BoundUnaryOp::Positive | BoundUnaryOp::Negative,
                ..
            } | BoundExprKind::Binary {
                op: BoundBinaryOp::Add
                    | BoundBinaryOp::Subtract
                    | BoundBinaryOp::Multiply
                    | BoundBinaryOp::Divide
                    | BoundBinaryOp::Modulo,
                ..
            }
        ) {
            require_numeric_or_indeterminate(expected)?;
        }
        let common = expression.sql_type.unify(expected)?;
        if expression.sql_type == common {
            continue;
        }
        expression.sql_type = common;
        match &mut expression.kind {
            BoundExprKind::Parameter(identifier) => {
                expression.sql_type = context.constrain_identifier(identifier.clone(), common)?;
            }
            BoundExprKind::Unary {
                op: BoundUnaryOp::Positive | BoundUnaryOp::Negative,
                expr,
            } => {
                work.push((expr, common));
            }
            BoundExprKind::Binary {
                left,
                op:
                    BoundBinaryOp::Add
                    | BoundBinaryOp::Subtract
                    | BoundBinaryOp::Multiply
                    | BoundBinaryOp::Divide
                    | BoundBinaryOp::Modulo,
                right,
            } => {
                work.push((right, common));
                work.push((left, common));
            }
            BoundExprKind::Aggregate {
                function: BoundAggregateFunction::Min | BoundAggregateFunction::Max,
                argument: Some(argument),
            } => {
                work.push((argument, common));
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_integer(text: &str) -> Result<(Value, SqlType)> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(type_error("integer literal must contain only digits"));
    }
    if let Ok(value) = text.parse::<i64>() {
        return Ok((Value::Int64(value), SqlType::Int64));
    }
    let magnitude = text
        .parse::<u128>()
        .map_err(|_| type_error("integer literal is outside the supported Decimal range"))?;
    Ok((
        Value::Decimal(Decimal::new(false, 0, magnitude)?),
        SqlType::Decimal,
    ))
}

fn parse_decimal(text: &str) -> Result<Decimal> {
    if !valid_decimal_text(text) {
        return Err(type_error("decimal literal is not valid"));
    }
    let (whole, fractional) = text
        .split_once('.')
        .ok_or_else(|| type_error("decimal literal must contain a decimal point"))?;
    let scale = i8::try_from(fractional.len())
        .map_err(|_| type_error("decimal literal scale is outside the supported range"))?;
    let digits = format!("{whole}{fractional}");
    let normalized = digits.trim_start_matches('0');
    let magnitude = if normalized.is_empty() {
        0
    } else {
        normalized
            .parse::<u128>()
            .map_err(|_| type_error("decimal literal has more than 38 significant digits"))?
    };
    Decimal::new(false, scale, magnitude)
}

fn parse_float(text: &str) -> Result<f64> {
    if !text.bytes().any(|byte| matches!(byte, b'e' | b'E')) {
        return Err(type_error("float literal must use exponent notation"));
    }
    let value = text
        .parse::<f64>()
        .map_err(|_| type_error("float literal is not representable as Float64"))?;
    if !value.is_finite() {
        return Err(type_error("float literal must be finite"));
    }
    Ok(value)
}

fn valid_decimal_text(text: &str) -> bool {
    let Some((whole, fractional)) = text.split_once('.') else {
        return false;
    };
    (!whole.is_empty() || !fractional.is_empty())
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fractional.bytes().all(|byte| byte.is_ascii_digit())
}

fn parameter_identifier(key: &ParameterKey) -> ParameterIdentifier {
    match key {
        ParameterKey::Position(position) => ParameterIdentifier::Position(position.get()),
        ParameterKey::Name(name) => ParameterIdentifier::Name(name.clone()),
    }
}

fn require_numeric_or_indeterminate(sql_type: SqlType) -> Result<()> {
    if matches!(
        sql_type,
        SqlType::Int64 | SqlType::Decimal | SqlType::Float64 | SqlType::Null | SqlType::Unknown
    ) {
        Ok(())
    } else {
        Err(type_error("arithmetic requires numeric operands"))
    }
}

const fn map_binary_op(op: BinaryOp) -> BoundBinaryOp {
    match op {
        BinaryOp::Add => BoundBinaryOp::Add,
        BinaryOp::Subtract => BoundBinaryOp::Subtract,
        BinaryOp::Multiply => BoundBinaryOp::Multiply,
        BinaryOp::Divide => BoundBinaryOp::Divide,
        BinaryOp::Modulo => BoundBinaryOp::Modulo,
        BinaryOp::Equal => BoundBinaryOp::Equal,
        BinaryOp::NotEqual => BoundBinaryOp::NotEqual,
        BinaryOp::LessThan => BoundBinaryOp::LessThan,
        BinaryOp::LessThanOrEqual => BoundBinaryOp::LessThanOrEqual,
        BinaryOp::GreaterThan => BoundBinaryOp::GreaterThan,
        BinaryOp::GreaterThanOrEqual => BoundBinaryOp::GreaterThanOrEqual,
        BinaryOp::And => BoundBinaryOp::And,
        BinaryOp::Or => BoundBinaryOp::Or,
    }
}

fn type_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Type, message)
}

fn bind_internal_error() -> DbError {
    DbError::new(
        ErrorCode::Internal,
        "expression binder work stack is inconsistent",
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        ErrorCode, Result, Value,
        api::{DataType, ParameterIdentifier, ParameterMetadata},
        sql::{
            ParameterKey, SqlLimits, SqlSpan, SqlType,
            ast::{AstExpr, AstExprKind, AstStatement, SelectItem},
            bound::{BoundExpr, BoundExprKind},
            parser::parse_statement,
        },
        storage::{catalog::CatalogReader, test_support::phase_4b_catalog_fixture},
    };

    use super::{ParameterCollector, bind_expr};
    use crate::sql::bind::BindingScope;

    #[derive(Debug)]
    struct BoundWhere {
        expression: crate::sql::bound::BoundExpr,
        parameters: Vec<ParameterMetadata>,
    }

    impl BoundWhere {
        fn parameters(&self) -> &[ParameterMetadata] {
            &self.parameters
        }
    }

    fn numeric_fixture() -> CatalogReader {
        real_catalog()
    }

    fn mixed_fixture() -> CatalogReader {
        real_catalog()
    }

    fn real_catalog() -> CatalogReader {
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut read = pager.begin_read().unwrap();
        read.catalog_reader().unwrap()
    }

    fn bind_where(sql: &str, catalog: CatalogReader) -> Result<BoundWhere> {
        let statement = parse_statement(
            &format!("SELECT ID FROM USERS WHERE {sql}"),
            &SqlLimits::default(),
        )?;
        let AstStatement::Query(mut query) = statement else {
            unreachable!("test SQL is a query")
        };
        let scope = BindingScope::from_query_sources(&catalog, &query)?;
        let selection = query.selection.take().expect("test SQL has WHERE");
        let mut parameters = ParameterCollector::default();
        let expression = bind_expr(&selection, &scope, Some(SqlType::Bool), &mut parameters)?;
        Ok(BoundWhere {
            expression,
            parameters: parameters.finish(),
        })
    }

    fn assert_type(sql: &str, expected: DataType, nullable: bool) {
        let output = bind_projection(sql).unwrap();
        assert_eq!(
            DataType::from_sql_type(output.expression.sql_type),
            expected,
            "{sql}"
        );
        assert_eq!(output.expression.nullable, nullable, "{sql}");
    }

    fn bind_projection(sql: &str) -> Result<BoundWhere> {
        let statement =
            parse_statement(&format!("SELECT {sql} FROM USERS"), &SqlLimits::default())?;
        let AstStatement::Query(query) = statement else {
            unreachable!("test SQL is a query")
        };
        let scope = BindingScope::from_query_sources(&real_catalog(), &query)?;
        let SelectItem::Expr { expr, .. } = &query.projection[0] else {
            unreachable!("test projection is an expression")
        };
        let mut parameters = ParameterCollector::default();
        let expression = bind_expr(expr, &scope, None, &mut parameters)?;
        Ok(BoundWhere {
            expression,
            parameters: parameters.finish(),
        })
    }

    fn bind_ast(expr: AstExpr) -> Result<BoundExpr> {
        let statement = parse_statement("SELECT ID FROM USERS", &SqlLimits::default())?;
        let AstStatement::Query(query) = statement else {
            unreachable!("test SQL is a query")
        };
        let scope = BindingScope::from_query_sources(&real_catalog(), &query)?;
        bind_expr(&expr, &scope, None, &mut ParameterCollector::default())
    }

    fn literal_ast(kind: AstExprKind) -> AstExpr {
        AstExpr {
            kind,
            span: SqlSpan::new(0, 1).unwrap(),
        }
    }

    #[test]
    fn repeated_parameters_merge_compatible_constraints() {
        // Production break caught: retaining only the first constraint would
        // expose :1 as Int64 instead of the safe common Decimal type.
        let bound = bind_where("ID = :1 AND AMOUNT > :1", numeric_fixture()).unwrap();
        assert_eq!(bound.parameters()[0].data_type(), Some(DataType::Decimal));
        assert_eq!(bound.expression.sql_type, SqlType::Bool);
    }

    #[test]
    fn incompatible_parameter_contexts_fail_at_prepare_time() {
        // Production break caught: last-write-wins constraint collection would
        // let one logical parameter alternate between numeric and text values.
        let error = bind_where("ID = :x AND NAME = :x", mixed_fixture()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
    }

    #[test]
    fn aggregate_result_types_follow_the_contract() {
        // Production break caught: deriving aggregates directly from their
        // input would make COUNT nullable and leave SUM(Int64) as Int64.
        assert_type("COUNT(*)", DataType::Int64, false);
        assert_type("SUM(ID)", DataType::Decimal, true);
        assert_type("AVG(FLOAT_COL)", DataType::Float64, true);
    }

    #[test]
    fn parameter_constraint_matrix_rejects_every_non_contract_promotion() {
        // Production break caught: adding any implicit conversion beyond
        // Int64-to-Decimal would make prepare accept an ambiguous parameter.
        let types = [
            SqlType::Null,
            SqlType::Unknown,
            SqlType::Int64,
            SqlType::Decimal,
            SqlType::Float64,
            SqlType::Text,
            SqlType::Blob,
            SqlType::Date,
            SqlType::Timestamp,
            SqlType::Bool,
        ];
        let n = Some(None);
        let i = Some(Some(DataType::Int64));
        let d = Some(Some(DataType::Decimal));
        let f = Some(Some(DataType::Float64));
        let t = Some(Some(DataType::Text));
        let b = Some(Some(DataType::Blob));
        let da = Some(Some(DataType::Date));
        let ts = Some(Some(DataType::Timestamp));
        let bo = Some(Some(DataType::Bool));
        let x = None;
        let expected = [
            [n, n, i, d, f, t, b, da, ts, bo],
            [n, n, i, d, f, t, b, da, ts, bo],
            [i, i, i, d, x, x, x, x, x, x],
            [d, d, d, d, x, x, x, x, x, x],
            [f, f, x, x, f, x, x, x, x, x],
            [t, t, x, x, x, t, x, x, x, x],
            [b, b, x, x, x, x, b, x, x, x],
            [da, da, x, x, x, x, x, da, x, x],
            [ts, ts, x, x, x, x, x, x, ts, x],
            [bo, bo, x, x, x, x, x, x, x, bo],
        ];
        let key = ParameterKey::parse(":x", SqlSpan::new(0, 2).unwrap()).unwrap();

        for (left_index, left) in types.into_iter().enumerate() {
            for (right_index, right) in types.into_iter().enumerate() {
                let mut collector = ParameterCollector::default();
                collector.constrain(&key, left).unwrap();
                let actual = match collector.constrain(&key, right) {
                    Ok(_) => Some(collector.finish()[0].data_type()),
                    Err(error) => {
                        assert_eq!(error.code(), ErrorCode::Type);
                        None
                    }
                };
                assert_eq!(
                    actual, expected[left_index][right_index],
                    "{left:?} then {right:?}"
                );
            }
        }
    }

    #[test]
    fn numeric_literals_are_checked_before_entering_the_bound_ir() {
        // Production break caught: unchecked host parsing would wrap integers,
        // admit over-precision decimals, or retain infinite Float64 values.
        let int64 = bind_projection("9223372036854775807").unwrap().expression;
        assert!(matches!(
            int64.kind,
            BoundExprKind::Literal(Value::Int64(9_223_372_036_854_775_807))
        ));

        let decimal = bind_projection("9223372036854775808").unwrap().expression;
        assert_eq!(decimal.sql_type, SqlType::Decimal);
        assert!(matches!(
            decimal.kind,
            BoundExprKind::Literal(Value::Decimal(value))
                if !value.negative() && value.scale() == 0
                    && value.magnitude() == 9_223_372_036_854_775_808
        ));

        let fractional = bind_projection("12.50").unwrap().expression;
        assert!(matches!(
            fractional.kind,
            BoundExprKind::Literal(Value::Decimal(value))
                if value.scale() == 2 && value.magnitude() == 1_250
        ));
        let exponent = bind_projection("1e2").unwrap().expression;
        assert!(matches!(
            exponent.kind,
            BoundExprKind::Literal(Value::Float64(value)) if value == 100.0
        ));

        for invalid in [
            "100000000000000000000000000000000000000",
            "0.000000000000000000000000000000000000001",
            "1e999",
        ] {
            assert_eq!(
                bind_projection(invalid).unwrap_err().code(),
                ErrorCode::Type
            );
        }
        for kind in [
            AstExprKind::Integer("not-an-integer".to_owned()),
            AstExprKind::Integer("+1".to_owned()),
            AstExprKind::Decimal("1..2".to_owned()),
            AstExprKind::Decimal(".".to_owned()),
            AstExprKind::Float("NaN".to_owned()),
            AstExprKind::Float("1".to_owned()),
        ] {
            assert_eq!(
                bind_ast(literal_ast(kind)).unwrap_err().code(),
                ErrorCode::Type
            );
        }
    }

    #[test]
    fn aggregate_table_covers_every_input_and_nullable_rule() {
        // Production break caught: treating every aggregate alike would return
        // wrong widening or nullability for at least one supported function.
        for (sql, data_type, nullable) in [
            ("COUNT(ID)", DataType::Int64, false),
            ("COUNT(NAME)", DataType::Int64, false),
            ("SUM(ID)", DataType::Decimal, true),
            ("SUM(AMOUNT)", DataType::Decimal, true),
            ("SUM(FLOAT_COL)", DataType::Float64, true),
            ("AVG(ID)", DataType::Decimal, true),
            ("AVG(AMOUNT)", DataType::Decimal, true),
            ("AVG(FLOAT_COL)", DataType::Float64, true),
            ("MIN(ID)", DataType::Int64, true),
            ("MAX(NAME)", DataType::Text, true),
            ("MIN(FLOAT_COL)", DataType::Float64, true),
            ("MAX(NULL)", DataType::Null, true),
        ] {
            assert_type(sql, data_type, nullable);
        }

        for invalid in ["SUM(NAME)", "AVG(TRUE)", "SUM(NULL)", "SUM(:x)", "MAX(*)"] {
            assert_eq!(
                bind_projection(invalid).unwrap_err().code(),
                ErrorCode::Type,
                "{invalid}"
            );
        }

        let count_parameter = bind_projection("COUNT(:free)").unwrap();
        assert_eq!(count_parameter.parameters()[0].data_type(), None);
        assert_eq!(count_parameter.expression.sql_type, SqlType::Int64);
        let min_parameter = bind_projection("MIN(:free)").unwrap();
        assert_eq!(min_parameter.parameters()[0].data_type(), None);
        assert_eq!(min_parameter.expression.sql_type, SqlType::Unknown);
        assert!(min_parameter.expression.nullable);
    }

    #[test]
    fn conditions_require_bool_and_preserve_three_valued_nullability() {
        // Production break caught: accepting a scalar condition or marking a
        // nullable comparison non-null would violate SQL condition semantics.
        let definite = bind_where("ID = 1", numeric_fixture()).unwrap();
        assert_eq!(definite.expression.sql_type, SqlType::Bool);
        assert!(!definite.expression.nullable);
        assert!(
            bind_where("NAME = 'A'", mixed_fixture())
                .unwrap()
                .expression
                .nullable
        );
        assert!(
            bind_where("NOT NULL", mixed_fixture())
                .unwrap()
                .expression
                .nullable
        );
        assert_eq!(
            bind_where("ID", numeric_fixture()).unwrap_err().code(),
            ErrorCode::Type
        );
        assert_eq!(
            bind_where("NOT NAME", mixed_fixture()).unwrap_err().code(),
            ErrorCode::Type
        );

        let parameter = bind_where(":condition", mixed_fixture()).unwrap();
        assert_eq!(parameter.parameters()[0].data_type(), Some(DataType::Bool));
        assert!(parameter.expression.nullable);
    }

    #[test]
    fn like_in_and_between_enforce_operand_contracts() {
        // Production break caught: pairwise-only or unconstrained list binding
        // would admit incompatible members and leave parameters untyped.
        let like = bind_where("NAME LIKE :pattern", mixed_fixture()).unwrap();
        assert_eq!(like.parameters()[0].data_type(), Some(DataType::Text));
        assert!(like.expression.nullable);
        for invalid in ["ID LIKE '1'", "'A' LIKE 1"] {
            assert_eq!(
                bind_where(invalid, mixed_fixture()).unwrap_err().code(),
                ErrorCode::Type
            );
        }

        assert!(
            !bind_where("ID IN (1, 2)", numeric_fixture())
                .unwrap()
                .expression
                .nullable
        );
        assert!(
            bind_where("ID IN (1, NULL)", numeric_fixture())
                .unwrap()
                .expression
                .nullable
        );
        let in_parameters = bind_where("ID IN (:first, :second)", numeric_fixture()).unwrap();
        assert_eq!(
            in_parameters
                .parameters()
                .iter()
                .map(ParameterMetadata::data_type)
                .collect::<Vec<_>>(),
            [Some(DataType::Int64), Some(DataType::Int64)]
        );
        for invalid in ["ID IN (1e0)", "NAME IN ('A', 1)"] {
            assert_eq!(
                bind_where(invalid, mixed_fixture()).unwrap_err().code(),
                ErrorCode::Type
            );
        }

        assert!(
            !bind_where("ID BETWEEN 1 AND 2", numeric_fixture())
                .unwrap()
                .expression
                .nullable
        );
        let between = bind_where("AMOUNT BETWEEN :low AND :high", numeric_fixture()).unwrap();
        assert_eq!(
            between
                .parameters()
                .iter()
                .map(ParameterMetadata::data_type)
                .collect::<Vec<_>>(),
            [Some(DataType::Decimal), Some(DataType::Decimal)]
        );
        assert!(between.expression.nullable);
        assert_eq!(
            bind_where("AMOUNT BETWEEN ID AND FLOAT_COL", numeric_fixture())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            bind_where("X'00' BETWEEN X'00' AND X'01'", mixed_fixture())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn arithmetic_and_comparisons_use_only_the_conservative_common_type() {
        // Production break caught: generic numeric coercion would mix Float64
        // with exact numerics or allow arithmetic on nonnumeric operands.
        assert_type("ID + AMOUNT", DataType::Decimal, true);
        assert_type("FLOAT_COL + 1e0", DataType::Float64, true);
        assert_type(":x + :y", DataType::Unknown, true);
        for invalid in ["AMOUNT + FLOAT_COL", "NAME + 'A'", "AMOUNT = FLOAT_COL"] {
            assert_eq!(
                bind_projection(invalid).unwrap_err().code(),
                ErrorCode::Type,
                "{invalid}"
            );
        }
        assert!(
            !bind_where("X'00' = X'00'", mixed_fixture())
                .unwrap()
                .expression
                .nullable
        );
        assert_eq!(
            bind_where("X'00' < X'01'", mixed_fixture())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert!(
            !bind_where("'A' < 'B'", mixed_fixture())
                .unwrap()
                .expression
                .nullable
        );
    }

    #[test]
    fn nonnumeric_contexts_cannot_retype_arithmetic_or_unary_signs() {
        // Production break caught: applying an outer Bool or Text expectation
        // after numeric validation must not turn Unknown arithmetic leaves into
        // nonnumeric parameters and make an invalid expression prepare.
        for invalid in [
            ":x + :y",
            "(:x + :y) = NAME",
            "(:x + :y) LIKE 'x'",
            "+:x",
            "+:x LIKE 'x'",
        ] {
            assert_eq!(
                bind_where(invalid, mixed_fixture()).unwrap_err().code(),
                ErrorCode::Type,
                "{invalid}"
            );
        }
    }

    #[test]
    fn expected_types_reach_parameters_nested_inside_expressions() {
        // Production break caught: constraining only a composite result would
        // leave its parameter leaves Unknown despite a known comparison peer.
        let bound = bind_where("(:x + :y) = AMOUNT", numeric_fixture()).unwrap();
        assert_eq!(
            bound
                .parameters()
                .iter()
                .map(ParameterMetadata::data_type)
                .collect::<Vec<_>>(),
            [Some(DataType::Decimal), Some(DataType::Decimal)]
        );
    }

    #[test]
    fn parameter_metadata_order_is_numeric_or_first_named_occurrence() {
        // Production break caught: map iteration order would make metadata
        // unstable, while occurrence order is wrong for positional binds.
        let positional = bind_where("ID = :2 AND ID = :1", numeric_fixture()).unwrap();
        assert_eq!(
            positional
                .parameters()
                .iter()
                .map(ParameterMetadata::identifier)
                .collect::<Vec<_>>(),
            [
                &ParameterIdentifier::Position(1),
                &ParameterIdentifier::Position(2),
            ]
        );

        let named = bind_where(
            "ID = :second AND ID = :first AND AMOUNT > :SECOND",
            numeric_fixture(),
        )
        .unwrap();
        assert_eq!(
            named
                .parameters()
                .iter()
                .map(|parameter| (parameter.identifier(), parameter.data_type()))
                .collect::<Vec<_>>(),
            [
                (
                    &ParameterIdentifier::Name("SECOND".to_owned()),
                    Some(DataType::Decimal)
                ),
                (
                    &ParameterIdentifier::Name("FIRST".to_owned()),
                    Some(DataType::Int64)
                ),
            ]
        );
    }

    #[test]
    fn parser_parameter_style_and_continuity_errors_remain_prepare_time_errors() {
        // Production break caught: bypassing checked parser primitives in the
        // binder would admit mixed styles or gapped positional parameters.
        for invalid in ["ID = :1 AND ID = :name", "ID = :2"] {
            assert_eq!(
                bind_where(invalid, numeric_fixture()).unwrap_err().code(),
                ErrorCode::Bind
            );
        }
    }

    #[test]
    fn deeply_nested_owned_arithmetic_binds_without_process_stack_recursion() {
        // Production break caught: recursive AST binding or expected-type
        // propagation would overflow the test thread stack for an expression
        // supplied directly to the internal binder. Public parsing has a smaller
        // implementation bound and is exercised separately in subprocesses.
        const DEPTH: usize = 16_384;
        let limits = SqlLimits::default();
        let statement = parse_statement("SELECT ID FROM USERS", &limits).unwrap();
        let AstStatement::Query(query) = statement else {
            unreachable!("test SQL is a query")
        };
        let scope = BindingScope::from_query_sources(&real_catalog(), &query).unwrap();
        let span = SqlSpan::new(0, 1).unwrap();
        let key = ParameterKey::parse(":deep", span).unwrap();
        let mut expression = AstExpr {
            kind: AstExprKind::Parameter(key),
            span,
        };
        for _ in 0..DEPTH {
            expression = AstExpr {
                kind: AstExprKind::Binary {
                    left: Box::new(expression),
                    op: crate::sql::ast::BinaryOp::Add,
                    right: Box::new(literal_ast(AstExprKind::Integer("1".to_owned()))),
                },
                span,
            };
        }

        let mut parameters = ParameterCollector::default();
        let bound =
            bind_expr(&expression, &scope, Some(SqlType::Decimal), &mut parameters).unwrap();
        assert_eq!(bound.sql_type, SqlType::Decimal);
        assert_eq!(parameters.finish()[0].data_type(), Some(DataType::Decimal));

        drop(bound);
        drop(expression);
    }
}
