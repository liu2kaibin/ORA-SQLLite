use std::collections::{HashMap, HashSet, hash_map::Entry};

use crate::{
    DbError, ErrorCode, Result, Value,
    api::{ColumnMetadata, DataType, ParameterIdentifier},
    sql::{
        Identifier, SqlType,
        ast::{AstExpr, AstExprKind, AstQuery, NullPlacement, OrderDirection, SelectItem},
        bound::{BoundExpr, BoundExprKind, SchemaDependency},
    },
    storage::catalog::CatalogReader,
};

use super::{BindingScope, ColumnResolver, ParameterCollector, ResolvedColumn, bind_expr};
use crate::sql::bound::{BoundJoin, BoundSource, SourceId};
pub(crate) use crate::sql::bound::{BoundOrderExpression, BoundOrderItem, BoundQuery};

#[derive(Clone, Copy)]
enum AliasIndex {
    Unique(usize),
    Ambiguous,
}

struct GroupIndex<'a> {
    buckets: HashMap<u64, Vec<&'a BoundExpr>>,
    key: fn(&BoundExpr) -> u64,
}

impl<'a> GroupIndex<'a> {
    fn new(group_by: &'a [BoundExpr]) -> Self {
        Self::with_key(group_by, structural_key)
    }

    fn with_key(group_by: &'a [BoundExpr], key: fn(&BoundExpr) -> u64) -> Self {
        let mut buckets = HashMap::with_capacity(group_by.len());
        for expression in group_by {
            buckets
                .entry(key(expression))
                .or_insert_with(Vec::new)
                .push(expression);
        }
        Self { buckets, key }
    }

    fn contains(&self, expression: &BoundExpr) -> bool {
        self.buckets
            .get(&(self.key)(expression))
            .is_some_and(|bucket| {
                bucket
                    .iter()
                    .any(|candidate| structurally_equal(expression, candidate))
            })
    }
}

enum GroupingFrame<'a> {
    Visit(&'a BoundExpr),
    Finish {
        expression: &'a BoundExpr,
        children: usize,
    },
}

/// Binds one parsed query against one immutable catalog image.
pub(crate) fn bind_query(
    query: &AstQuery,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    parameters: &mut ParameterCollector,
) -> Result<BoundQuery> {
    let join_scope = BindingScope::join_condition_scope(catalog, query)?;
    let scope = join_scope.clone().with_left_join_null_extension(query);
    let dependencies = dependencies_for_scope(&scope, database_uuid)?;

    let sources = scope
        .source_identities()
        .enumerate()
        .map(|(index, identity)| BoundSource {
            source_id: SourceId(index),
            table_id: identity.table_id,
            schema_version: identity.schema_version,
        })
        .collect();
    let mut joins = Vec::with_capacity(query.joins.len());
    for (index, join) in query.joins.iter().enumerate() {
        let condition_scope = join_scope.scope_for_join_condition(query, index)?;
        let condition = bind_condition(&join.condition, &condition_scope, parameters)?;
        reject_aggregate(&condition, "JOIN ON")?;
        joins.push(BoundJoin {
            kind: join.kind,
            source_id: SourceId(index + 1),
            condition,
        });
    }

    let selection = query
        .selection
        .as_ref()
        .map(|expression| bind_condition(expression, &scope, parameters))
        .transpose()?;
    if let Some(selection) = &selection {
        reject_aggregate(selection, "WHERE")?;
    }

    let mut group_by = Vec::with_capacity(query.group_by.len());
    for expression in &query.group_by {
        let bound = bind_expr(expression, &scope, None, parameters)?;
        reject_aggregate(&bound, "GROUP BY")?;
        group_by.push(bound);
    }

    let having = query
        .having
        .as_ref()
        .map(|expression| bind_condition(expression, &scope, parameters))
        .transpose()?;

    let mut projection = Vec::new();
    let mut columns = Vec::new();
    let mut aliases = HashMap::new();
    for item in &query.projection {
        match item {
            SelectItem::Expr { expr, alias, .. } => {
                let bound = bind_expr(expr, &scope, None, parameters)?;
                let name = projection_name(alias.as_ref(), expr, &scope, columns.len())?;
                if let Some(alias) = alias {
                    match aliases.entry(alias.as_str().to_owned()) {
                        Entry::Vacant(entry) => {
                            entry.insert(AliasIndex::Unique(projection.len()));
                        }
                        Entry::Occupied(mut entry) => {
                            entry.insert(AliasIndex::Ambiguous);
                        }
                    }
                }
                columns.push(column_metadata(name, &bound));
                projection.push(bound);
            }
            SelectItem::Wildcard { qualifier, span } => {
                parameters.charge_nodes(scope.star_count(qualifier.as_ref())?)?;
                for column in scope.star_columns(qualifier.as_ref())? {
                    projection.push(bound_column(&column, *span));
                    columns.push(column_metadata(
                        column.name.clone(),
                        projection.last().unwrap(),
                    ));
                }
            }
        }
    }

    let mut order_by = Vec::with_capacity(query.order_by.len());
    for item in &query.order_by {
        let expression = if let Some(position) = order_ordinal(&item.expr, projection.len())? {
            BoundOrderExpression::Projection(position)
        } else if let Some(position) = order_alias(&item.expr, &aliases)? {
            BoundOrderExpression::Projection(position)
        } else {
            BoundOrderExpression::Expression(bind_expr(&item.expr, &scope, None, parameters)?)
        };
        aggregate_profile(expression.expression(&projection))?;
        order_by.push(BoundOrderItem {
            expression,
            direction: item.direction,
            nulls: item.nulls,
        });
    }

    let has_aggregate = [having.as_ref()]
        .into_iter()
        .flatten()
        .chain(projection.iter())
        .chain(
            order_by
                .iter()
                .map(|item| item.expression.expression(&projection)),
        )
        .map(aggregate_profile)
        .try_fold(false, |found, aggregate| {
            aggregate.map(|aggregate| found || aggregate)
        })?;
    if (!group_by.is_empty() || has_aggregate || having.is_some()) && !parameters.defers_grouping()
    {
        let group_index = GroupIndex::new(&group_by);
        if let Some(having) = &having {
            validate_having_grouping(having, &group_index)?;
        }
        for expression in &projection {
            validate_grouping(expression, &group_index)?;
        }
        for item in &order_by {
            validate_grouping(item.expression.expression(&projection), &group_index)?;
        }
    }

    Ok(BoundQuery {
        distinct: query.distinct,
        sources,
        joins,
        selection,
        group_by,
        having,
        projection,
        order_by,
        columns,
        dependencies,
    })
}

fn bind_condition(
    expression: &AstExpr,
    scope: &impl ColumnResolver,
    parameters: &mut ParameterCollector,
) -> Result<BoundExpr> {
    bind_expr(expression, scope, Some(SqlType::Bool), parameters)
}

fn dependencies_for_scope(
    scope: &BindingScope,
    database_uuid: [u8; 16],
) -> Result<Vec<SchemaDependency>> {
    let mut dependencies = Vec::new();
    let mut seen = HashSet::new();
    for identity in scope.source_identities() {
        if !seen.insert((identity.table_id, identity.schema_version)) {
            continue;
        }
        let dependency = SchemaDependency {
            database_uuid,
            table_id: identity.table_id,
            schema_version: identity.schema_version,
        };
        dependencies.push(dependency);
    }
    Ok(dependencies)
}

fn bound_column(column: &ResolvedColumn, span: crate::sql::SqlSpan) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Column {
            source_id: column.source_id,
            table_id: column.table_id,
            column_id: column.column_id,
            ordinal: column.ordinal,
        },
        column.sql_type,
        column.nullable,
        span,
    )
}

fn projection_name(
    alias: Option<&Identifier>,
    expression: &AstExpr,
    scope: &BindingScope,
    position: usize,
) -> Result<String> {
    if let Some(alias) = alias {
        return Ok(alias.as_str().to_owned());
    }
    if let AstExprKind::Column(parts) = &expression.kind {
        return Ok(scope.resolve_column(parts)?.name);
    }
    if let AstExprKind::Aggregate { function, .. } = &expression.kind {
        return Ok(match function {
            crate::sql::ast::AggregateFunction::Count => "COUNT",
            crate::sql::ast::AggregateFunction::Sum => "SUM",
            crate::sql::ast::AggregateFunction::Avg => "AVG",
            crate::sql::ast::AggregateFunction::Min => "MIN",
            crate::sql::ast::AggregateFunction::Max => "MAX",
        }
        .to_owned());
    }
    Ok(format!("EXPR{}", position.saturating_add(1)))
}

fn column_metadata(name: String, expression: &BoundExpr) -> ColumnMetadata {
    let (source_table_id, source_column_id) = match &expression.kind {
        BoundExprKind::Column {
            table_id,
            column_id,
            ..
        } => (Some(*table_id), Some(*column_id)),
        _ => (None, None),
    };
    ColumnMetadata::new(
        name,
        DataType::from_sql_type(expression.sql_type),
        expression.nullable,
        source_table_id,
        source_column_id,
    )
}

fn order_ordinal(expression: &AstExpr, projection_len: usize) -> Result<Option<usize>> {
    let text = match &expression.kind {
        AstExprKind::Integer(text) => Some(text.as_str()),
        AstExprKind::Decimal(_) | AstExprKind::Float(_) => {
            return Err(bind_error("ORDER BY position must be a positive integer"));
        }
        AstExprKind::Unary { expr, .. }
            if matches!(
                &expr.kind,
                AstExprKind::Integer(_) | AstExprKind::Decimal(_) | AstExprKind::Float(_)
            ) =>
        {
            return Err(bind_error("ORDER BY position must be a positive integer"));
        }
        _ => None,
    };
    let Some(text) = text else {
        return Ok(None);
    };
    let position = text
        .parse::<usize>()
        .map_err(|_| bind_error("ORDER BY position is outside the supported range"))?;
    let index = position
        .checked_sub(1)
        .filter(|index| *index < projection_len)
        .ok_or_else(|| bind_error("ORDER BY position is outside the projection"))?;
    Ok(Some(index))
}

fn order_alias(
    expression: &AstExpr,
    aliases: &HashMap<String, AliasIndex>,
) -> Result<Option<usize>> {
    let AstExprKind::Column(parts) = &expression.kind else {
        return Ok(None);
    };
    let [identifier] = parts.as_slice() else {
        return Ok(None);
    };
    let Some(alias) = aliases.get(identifier.as_str()) else {
        return Ok(None);
    };
    match alias {
        AliasIndex::Unique(index) => Ok(Some(*index)),
        AliasIndex::Ambiguous => Err(bind_error("ORDER BY alias is ambiguous")),
    }
}

fn reject_aggregate(expression: &BoundExpr, clause: &'static str) -> Result<()> {
    if aggregate_profile(expression)? {
        return Err(bind_error(match clause {
            "JOIN ON" => "JOIN ON cannot contain aggregates",
            "WHERE" => "WHERE cannot contain aggregates",
            "GROUP BY" => "GROUP BY cannot contain aggregates",
            _ => "this clause cannot contain aggregates",
        }));
    }
    Ok(())
}

fn aggregate_profile(root: &BoundExpr) -> Result<bool> {
    let mut found = false;
    let mut work = vec![(root, false)];
    while let Some((expression, beneath_aggregate)) = work.pop() {
        let is_aggregate = matches!(&expression.kind, BoundExprKind::Aggregate { .. });
        if is_aggregate {
            if beneath_aggregate {
                return Err(bind_error("aggregate functions cannot be nested"));
            }
            found = true;
        }
        push_children(expression, is_aggregate || beneath_aggregate, &mut work);
    }
    Ok(found)
}

fn validate_having_grouping(expression: &BoundExpr, group_index: &GroupIndex<'_>) -> Result<()> {
    let mut work = vec![expression];
    while let Some(expression) = work.pop() {
        match &expression.kind {
            BoundExprKind::Aggregate { .. } => {}
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                work.push(expr);
            }
            BoundExprKind::Binary {
                left,
                op:
                    crate::sql::bound::BoundBinaryOp::Equal
                    | crate::sql::bound::BoundBinaryOp::NotEqual
                    | crate::sql::bound::BoundBinaryOp::LessThan
                    | crate::sql::bound::BoundBinaryOp::LessThanOrEqual
                    | crate::sql::bound::BoundBinaryOp::GreaterThan
                    | crate::sql::bound::BoundBinaryOp::GreaterThanOrEqual
                    | crate::sql::bound::BoundBinaryOp::And
                    | crate::sql::bound::BoundBinaryOp::Or,
                right,
            } => {
                work.push(right);
                work.push(left);
            }
            BoundExprKind::InList { expr, values, .. } => {
                work.extend(values.iter());
                work.push(expr);
            }
            BoundExprKind::Between {
                expr, low, high, ..
            } => {
                work.push(high);
                work.push(low);
                work.push(expr);
            }
            BoundExprKind::Like { expr, pattern, .. } => {
                work.push(pattern);
                work.push(expr);
            }
            _ => validate_grouping(expression, group_index)?,
        }
    }
    Ok(())
}

fn validate_grouping(expression: &BoundExpr, group_index: &GroupIndex<'_>) -> Result<()> {
    let mut work = vec![GroupingFrame::Visit(expression)];
    let mut aggregate_states: Vec<(&BoundExpr, bool)> = Vec::new();
    while let Some(frame) = work.pop() {
        match frame {
            GroupingFrame::Visit(expression)
                if matches!(&expression.kind, BoundExprKind::Aggregate { .. }) =>
            {
                aggregate_states.push((expression, true));
            }
            GroupingFrame::Visit(expression) => {
                let child_count = expression_child_count(expression);
                if child_count == 0 {
                    aggregate_states.push((expression, false));
                    continue;
                }
                work.push(GroupingFrame::Finish {
                    expression,
                    children: child_count,
                });
                push_children_plain(expression, &mut work);
            }
            GroupingFrame::Finish {
                expression,
                children,
            } => {
                let start = aggregate_states
                    .len()
                    .checked_sub(children)
                    .ok_or_else(|| bind_error("query grouping state is inconsistent"))?;
                let child_states = aggregate_states.split_off(start);
                let contains_aggregate = child_states.iter().any(|(_, aggregate)| *aggregate);
                if contains_aggregate {
                    for (child, aggregate) in child_states {
                        if !aggregate {
                            validate_ungrouped_subtree(child, group_index)?;
                        }
                    }
                }
                aggregate_states.push((expression, contains_aggregate));
            }
        }
    }
    let [(root, contains_aggregate)] = aggregate_states.as_slice() else {
        return Err(bind_error("query grouping state is inconsistent"));
    };
    if !contains_aggregate {
        validate_ungrouped_subtree(root, group_index)?;
    }
    Ok(())
}

fn validate_ungrouped_subtree(expression: &BoundExpr, group_index: &GroupIndex<'_>) -> Result<()> {
    if is_constant(expression) || group_index.contains(expression) {
        return Ok(());
    }
    Err(bind_error(
        "ungrouped expression must be structurally equivalent to GROUP BY",
    ))
}

fn is_constant(root: &BoundExpr) -> bool {
    let mut work = vec![(root, false)];
    while let Some((expression, _)) = work.pop() {
        if matches!(
            expression.kind,
            BoundExprKind::Column { .. } | BoundExprKind::NewTableColumn { .. }
        ) {
            return false;
        }
        push_children(expression, false, &mut work);
    }
    true
}

fn structural_key(root: &BoundExpr) -> u64 {
    let mut hasher = StructuralHasher::new();
    let mut work = vec![root];
    while let Some(expression) = work.pop() {
        hasher.write_byte(sql_type_tag(expression.sql_type));
        hasher.write_bool(expression.nullable);
        match &expression.kind {
            BoundExprKind::Literal(value) => {
                hasher.write_byte(0);
                hash_value(&mut hasher, value);
            }
            BoundExprKind::Column {
                source_id,
                table_id,
                column_id,
                ordinal,
            } => {
                hasher.write_byte(1);
                hasher.write_u64(source_id.0 as u64);
                hasher.write_u64(table_id.get());
                hasher.write_u64(column_id.get());
                hasher.write_u64(u64::from(*ordinal));
            }
            BoundExprKind::NewTableColumn { ordinal } => {
                hasher.write_byte(10);
                hasher.write_u64(u64::from(*ordinal));
            }
            BoundExprKind::Parameter(identifier) => {
                hasher.write_byte(2);
                match identifier {
                    ParameterIdentifier::Position(position) => {
                        hasher.write_byte(0);
                        hasher.write_u64(u64::from(*position));
                    }
                    ParameterIdentifier::Name(name) => {
                        hasher.write_byte(1);
                        hasher.write_bytes(name.as_bytes());
                    }
                }
            }
            BoundExprKind::Unary { op, expr } => {
                hasher.write_byte(3);
                hasher.write_byte(unary_tag(*op));
                work.push(expr);
            }
            BoundExprKind::Binary { left, op, right } => {
                hasher.write_byte(4);
                hasher.write_byte(binary_tag(*op));
                work.push(right);
                work.push(left);
            }
            BoundExprKind::IsNull { expr, negated } => {
                hasher.write_byte(5);
                hasher.write_bool(*negated);
                work.push(expr);
            }
            BoundExprKind::InList {
                expr,
                values,
                negated,
            } => {
                hasher.write_byte(6);
                hasher.write_bool(*negated);
                hasher.write_u64(values.len() as u64);
                for value in values.iter().rev() {
                    work.push(value);
                }
                work.push(expr);
            }
            BoundExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                hasher.write_byte(7);
                hasher.write_bool(*negated);
                work.push(high);
                work.push(low);
                work.push(expr);
            }
            BoundExprKind::Like {
                expr,
                pattern,
                negated,
            } => {
                hasher.write_byte(8);
                hasher.write_bool(*negated);
                work.push(pattern);
                work.push(expr);
            }
            BoundExprKind::Aggregate { function, argument } => {
                hasher.write_byte(9);
                hasher.write_byte(aggregate_tag(*function));
                hasher.write_bool(argument.is_some());
                if let Some(argument) = argument {
                    work.push(argument);
                }
            }
        }
    }
    hasher.finish()
}

struct StructuralHasher(u64);

impl StructuralHasher {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;

    const fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn write_byte(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }

    fn write_bool(&mut self, value: bool) {
        self.write_byte(u8::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.write_byte(byte);
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        for byte in bytes {
            self.write_byte(*byte);
        }
    }

    const fn finish(self) -> u64 {
        self.0
    }
}

fn hash_value(hasher: &mut StructuralHasher, value: &Value) {
    match value {
        Value::Null => hasher.write_byte(0),
        Value::Int64(value) => {
            hasher.write_byte(1);
            hasher.write_bytes(&value.to_le_bytes());
        }
        Value::Decimal(value) => {
            hasher.write_byte(2);
            hasher.write_bool(value.negative());
            hasher.write_byte(value.scale() as u8);
            hasher.write_bytes(&value.magnitude().to_le_bytes());
        }
        Value::Float64(value) => {
            hasher.write_byte(3);
            hasher.write_bytes(&value.to_bits().to_le_bytes());
        }
        Value::Text(value) => {
            hasher.write_byte(4);
            hasher.write_bytes(value.as_bytes());
        }
        Value::Blob(value) => {
            hasher.write_byte(5);
            hasher.write_bytes(value);
        }
        Value::Date(value) => {
            hasher.write_byte(6);
            hash_date(hasher, *value);
        }
        Value::Timestamp(value) => {
            hasher.write_byte(7);
            hash_date(hasher, value.date());
            hasher.write_bytes(&value.micros().to_le_bytes());
            match value.offset_minutes() {
                Some(offset) => {
                    hasher.write_bool(true);
                    hasher.write_bytes(&offset.to_le_bytes());
                }
                None => hasher.write_bool(false),
            }
        }
        Value::Bool(value) => {
            hasher.write_byte(8);
            hasher.write_bool(*value);
        }
    }
}

fn hash_date(hasher: &mut StructuralHasher, value: crate::OracleDate) {
    hasher.write_bytes(&value.year().to_le_bytes());
    hasher.write_byte(value.month());
    hasher.write_byte(value.day());
    hasher.write_byte(value.hour());
    hasher.write_byte(value.minute());
    hasher.write_byte(value.second());
}

const fn sql_type_tag(sql_type: SqlType) -> u8 {
    match sql_type {
        SqlType::Null => 0,
        SqlType::Unknown => 1,
        SqlType::Int64 => 2,
        SqlType::Decimal => 3,
        SqlType::Float64 => 4,
        SqlType::Text => 5,
        SqlType::Blob => 6,
        SqlType::Date => 7,
        SqlType::Timestamp => 8,
        SqlType::Bool => 9,
    }
}

const fn unary_tag(operator: crate::sql::bound::BoundUnaryOp) -> u8 {
    match operator {
        crate::sql::bound::BoundUnaryOp::Positive => 0,
        crate::sql::bound::BoundUnaryOp::Negative => 1,
        crate::sql::bound::BoundUnaryOp::Not => 2,
    }
}

const fn binary_tag(operator: crate::sql::bound::BoundBinaryOp) -> u8 {
    match operator {
        crate::sql::bound::BoundBinaryOp::Add => 0,
        crate::sql::bound::BoundBinaryOp::Subtract => 1,
        crate::sql::bound::BoundBinaryOp::Multiply => 2,
        crate::sql::bound::BoundBinaryOp::Divide => 3,
        crate::sql::bound::BoundBinaryOp::Modulo => 4,
        crate::sql::bound::BoundBinaryOp::Equal => 5,
        crate::sql::bound::BoundBinaryOp::NotEqual => 6,
        crate::sql::bound::BoundBinaryOp::LessThan => 7,
        crate::sql::bound::BoundBinaryOp::LessThanOrEqual => 8,
        crate::sql::bound::BoundBinaryOp::GreaterThan => 9,
        crate::sql::bound::BoundBinaryOp::GreaterThanOrEqual => 10,
        crate::sql::bound::BoundBinaryOp::And => 11,
        crate::sql::bound::BoundBinaryOp::Or => 12,
    }
}

const fn aggregate_tag(operator: crate::sql::bound::BoundAggregateFunction) -> u8 {
    match operator {
        crate::sql::bound::BoundAggregateFunction::Count => 0,
        crate::sql::bound::BoundAggregateFunction::Sum => 1,
        crate::sql::bound::BoundAggregateFunction::Avg => 2,
        crate::sql::bound::BoundAggregateFunction::Min => 3,
        crate::sql::bound::BoundAggregateFunction::Max => 4,
    }
}

fn structurally_equal(left: &BoundExpr, right: &BoundExpr) -> bool {
    let mut work = vec![(left, right)];
    while let Some((left, right)) = work.pop() {
        if left.sql_type != right.sql_type || left.nullable != right.nullable {
            return false;
        }
        match (&left.kind, &right.kind) {
            (BoundExprKind::Literal(left), BoundExprKind::Literal(right)) if left == right => {}
            (
                BoundExprKind::Column {
                    source_id: left_source,
                    table_id: left_table,
                    column_id: left_column,
                    ordinal: left_ordinal,
                },
                BoundExprKind::Column {
                    source_id: right_source,
                    table_id: right_table,
                    column_id: right_column,
                    ordinal: right_ordinal,
                },
            ) if left_source == right_source
                && left_table == right_table
                && left_column == right_column
                && left_ordinal == right_ordinal => {}
            (
                BoundExprKind::NewTableColumn {
                    ordinal: left_ordinal,
                },
                BoundExprKind::NewTableColumn {
                    ordinal: right_ordinal,
                },
            ) if left_ordinal == right_ordinal => {}
            (BoundExprKind::Parameter(left), BoundExprKind::Parameter(right)) if left == right => {}
            (
                BoundExprKind::Unary {
                    op: left_op,
                    expr: left_expr,
                },
                BoundExprKind::Unary {
                    op: right_op,
                    expr: right_expr,
                },
            ) if left_op == right_op => work.push((left_expr, right_expr)),
            (
                BoundExprKind::Binary {
                    left: left_left,
                    op: left_op,
                    right: left_right,
                },
                BoundExprKind::Binary {
                    left: right_left,
                    op: right_op,
                    right: right_right,
                },
            ) if left_op == right_op => {
                work.push((left_right, right_right));
                work.push((left_left, right_left));
            }
            (
                BoundExprKind::IsNull {
                    expr: left_expr,
                    negated: left_negated,
                },
                BoundExprKind::IsNull {
                    expr: right_expr,
                    negated: right_negated,
                },
            ) if left_negated == right_negated => work.push((left_expr, right_expr)),
            (
                BoundExprKind::InList {
                    expr: left_expr,
                    values: left_values,
                    negated: left_negated,
                },
                BoundExprKind::InList {
                    expr: right_expr,
                    values: right_values,
                    negated: right_negated,
                },
            ) if left_negated == right_negated && left_values.len() == right_values.len() => {
                work.push((left_expr, right_expr));
                work.extend(left_values.iter().zip(right_values));
            }
            (
                BoundExprKind::Between {
                    expr: left_expr,
                    low: left_low,
                    high: left_high,
                    negated: left_negated,
                },
                BoundExprKind::Between {
                    expr: right_expr,
                    low: right_low,
                    high: right_high,
                    negated: right_negated,
                },
            ) if left_negated == right_negated => {
                work.push((left_high, right_high));
                work.push((left_low, right_low));
                work.push((left_expr, right_expr));
            }
            (
                BoundExprKind::Like {
                    expr: left_expr,
                    pattern: left_pattern,
                    negated: left_negated,
                },
                BoundExprKind::Like {
                    expr: right_expr,
                    pattern: right_pattern,
                    negated: right_negated,
                },
            ) if left_negated == right_negated => {
                work.push((left_pattern, right_pattern));
                work.push((left_expr, right_expr));
            }
            (
                BoundExprKind::Aggregate {
                    function: left_function,
                    argument: left_argument,
                },
                BoundExprKind::Aggregate {
                    function: right_function,
                    argument: right_argument,
                },
            ) if left_function == right_function => match (left_argument, right_argument) {
                (None, None) => {}
                (Some(left), Some(right)) => work.push((left, right)),
                _ => return false,
            },
            _ => return false,
        }
    }
    true
}

fn expression_child_count(expression: &BoundExpr) -> usize {
    match &expression.kind {
        BoundExprKind::Literal(_)
        | BoundExprKind::Column { .. }
        | BoundExprKind::NewTableColumn { .. }
        | BoundExprKind::Parameter(_) => 0,
        BoundExprKind::Unary { .. } | BoundExprKind::IsNull { .. } => 1,
        BoundExprKind::Binary { .. } | BoundExprKind::Like { .. } => 2,
        BoundExprKind::InList { values, .. } => values.len().saturating_add(1),
        BoundExprKind::Between { .. } => 3,
        BoundExprKind::Aggregate { argument, .. } => usize::from(argument.is_some()),
    }
}

fn push_children<'a>(
    expression: &'a BoundExpr,
    beneath_aggregate: bool,
    work: &mut Vec<(&'a BoundExpr, bool)>,
) {
    match &expression.kind {
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            work.push((expr, beneath_aggregate));
        }
        BoundExprKind::Binary { left, right, .. } => {
            work.push((right, beneath_aggregate));
            work.push((left, beneath_aggregate));
        }
        BoundExprKind::InList { expr, values, .. } => {
            for value in values.iter().rev() {
                work.push((value, beneath_aggregate));
            }
            work.push((expr, beneath_aggregate));
        }
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            work.push((high, beneath_aggregate));
            work.push((low, beneath_aggregate));
            work.push((expr, beneath_aggregate));
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            work.push((pattern, beneath_aggregate));
            work.push((expr, beneath_aggregate));
        }
        BoundExprKind::Aggregate {
            argument: Some(argument),
            ..
        } => work.push((argument, beneath_aggregate)),
        BoundExprKind::Literal(_)
        | BoundExprKind::Column { .. }
        | BoundExprKind::NewTableColumn { .. }
        | BoundExprKind::Parameter(_)
        | BoundExprKind::Aggregate { argument: None, .. } => {}
    }
}

fn push_children_plain<'a>(expression: &'a BoundExpr, work: &mut Vec<GroupingFrame<'a>>) {
    match &expression.kind {
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            work.push(GroupingFrame::Visit(expr));
        }
        BoundExprKind::Binary { left, right, .. } => {
            work.push(GroupingFrame::Visit(right));
            work.push(GroupingFrame::Visit(left));
        }
        BoundExprKind::InList { expr, values, .. } => {
            for value in values.iter().rev() {
                work.push(GroupingFrame::Visit(value));
            }
            work.push(GroupingFrame::Visit(expr));
        }
        BoundExprKind::Between {
            expr, low, high, ..
        } => {
            work.push(GroupingFrame::Visit(high));
            work.push(GroupingFrame::Visit(low));
            work.push(GroupingFrame::Visit(expr));
        }
        BoundExprKind::Like { expr, pattern, .. } => {
            work.push(GroupingFrame::Visit(pattern));
            work.push(GroupingFrame::Visit(expr));
        }
        BoundExprKind::Aggregate {
            argument: Some(argument),
            ..
        } => work.push(GroupingFrame::Visit(argument)),
        BoundExprKind::Literal(_)
        | BoundExprKind::Column { .. }
        | BoundExprKind::NewTableColumn { .. }
        | BoundExprKind::Parameter(_)
        | BoundExprKind::Aggregate { argument: None, .. } => {}
    }
}

fn bind_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Bind, message)
}

#[cfg(test)]
mod tests {
    use super::GroupIndex;
    use crate::{
        ErrorCode, Value,
        api::{ColumnMetadata, DataType},
        sql::{
            Identifier, SqlSpan, SqlType,
            ast::{
                AstExpr, AstExprKind, AstQuery, BinaryOp, Join, JoinKind, NullPlacement,
                OrderDirection, OrderItem, SelectItem, TableSource,
            },
            bind::{
                BindingScope, BoundOrderExpression, ParameterCollector, bind_query,
                test_support::prepare_fixture,
            },
            bound::{BoundExpr, BoundExprKind, BoundStatementKind},
        },
        storage::test_support::phase_4b_catalog_fixture,
    };

    #[test]
    fn join_group_alias_and_order_bind_to_stable_ids() {
        // Production break caught: resolving joins by names or leaking SELECT
        // aliases outside ORDER BY would make a prepared query drift with the
        // catalog or reject its valid final ordering reference.
        let statement = prepare_fixture(
            "SELECT U.ID AS USER_ID, COUNT(O.ID) AS N \
             FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID \
             GROUP BY U.ID ORDER BY USER_ID",
        )
        .unwrap();

        assert_eq!(
            statement
                .columns()
                .iter()
                .map(ColumnMetadata::name)
                .collect::<Vec<_>>(),
            ["USER_ID", "N"]
        );
        assert_eq!(
            statement
                .columns()
                .iter()
                .map(|column| (
                    column.data_type(),
                    column.nullable(),
                    column.source_table_id(),
                    column.source_column_id(),
                ))
                .collect::<Vec<_>>(),
            [
                (DataType::Int64, false, Some(1), Some(2)),
                (DataType::Int64, false, None, None),
            ]
        );
        assert_eq!(
            statement
                .dependencies()
                .iter()
                .map(|dependency| (dependency.table_id.get(), dependency.schema_version.get()))
                .collect::<Vec<_>>(),
            [(1, 1), (6, 1)]
        );
    }

    #[test]
    fn aliases_do_not_leak_into_where_group_by_or_having() {
        // Production break caught: adding output aliases to the source scope
        // would silently change filtering and grouping semantics.
        for sql in [
            "SELECT ID AS X FROM USERS WHERE X = 1",
            "SELECT ID AS X FROM USERS GROUP BY X",
            "SELECT ID AS X FROM USERS HAVING X = 1",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
    }

    #[test]
    fn having_without_group_or_aggregate_enforces_grouping_rules() {
        // Production break caught: treating HAVING as a WHERE-like predicate
        // when no aggregate exists admits an ungrouped source column after the
        // grouping stage has logically completed.
        assert_eq!(
            prepare_fixture("SELECT U.ID FROM USERS U HAVING U.ID > 0")
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        for sql in [
            "SELECT 1 FROM USERS U HAVING 1 = 1",
            "SELECT COUNT(*) FROM USERS U HAVING COUNT(*) > 0",
        ] {
            assert!(prepare_fixture(sql).is_ok(), "{sql}");
        }
    }

    #[test]
    fn stars_expand_before_order_positions_with_catalog_source_metadata() {
        // Production break caught: assigning ORDER BY positions before star
        // expansion would select the wrong output, while losing source IDs
        // would invalidate schema dependency checks.
        let statement = prepare_fixture(
            "SELECT U.*, O.USER_ID AS ORDER_USER FROM USERS U \
             INNER JOIN ORDERS O ON O.USER_ID = U.ID ORDER BY 5",
        )
        .unwrap();

        assert_eq!(
            statement
                .columns()
                .iter()
                .map(ColumnMetadata::name)
                .collect::<Vec<_>>(),
            ["ID", "NAME", "AMOUNT", "FLOAT_COL", "ORDER_USER"]
        );
        assert_eq!(
            statement
                .columns()
                .iter()
                .map(|column| (column.source_table_id(), column.source_column_id()))
                .collect::<Vec<_>>(),
            [
                (Some(1), Some(2)),
                (Some(1), Some(3)),
                (Some(1), Some(4)),
                (Some(1), Some(5)),
                (Some(6), Some(8)),
            ]
        );
    }

    #[test]
    fn left_join_right_columns_are_nullable_in_direct_and_star_metadata() {
        // Production break caught: carrying catalog NOT NULL constraints through
        // a LEFT JOIN tells callers that a null-extended right-side output cannot
        // be NULL.
        let direct =
            prepare_fixture("SELECT O.ID FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID")
                .unwrap();
        assert_eq!(direct.columns()[0].source_table_id(), Some(6));
        assert_eq!(direct.columns()[0].source_column_id(), Some(7));
        assert!(direct.columns()[0].nullable());

        let star =
            prepare_fixture("SELECT O.* FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID")
                .unwrap();
        assert_eq!(
            star.columns()
                .iter()
                .map(|column| (
                    column.name(),
                    column.nullable(),
                    column.source_table_id(),
                    column.source_column_id(),
                ))
                .collect::<Vec<_>>(),
            [
                ("ID", true, Some(6), Some(7)),
                ("USER_ID", true, Some(6), Some(8)),
            ]
        );
    }

    #[test]
    fn distinct_repeated_sources_keep_projection_order_and_one_dependency() {
        // Production break caught: treating each alias as an independent schema
        // dependency duplicates validation work and makes dependency order depend
        // on repeated source references.
        let statement = prepare_fixture(
            "SELECT DISTINCT U.ID, M.ID AS MANAGER_ID FROM USERS U \
             INNER JOIN USERS M ON M.ID = U.ID ORDER BY 2",
        )
        .unwrap();

        assert_eq!(
            statement
                .columns()
                .iter()
                .map(ColumnMetadata::name)
                .collect::<Vec<_>>(),
            ["ID", "MANAGER_ID"]
        );
        assert_eq!(
            statement
                .dependencies()
                .iter()
                .map(|dependency| (dependency.table_id.get(), dependency.schema_version.get()))
                .collect::<Vec<_>>(),
            [(1, 1)]
        );
    }

    #[test]
    fn ordered_source_identities_deduplicate_wide_repeated_join_inputs() {
        // Production break caught: expanding every column just to discover
        // dependencies turns a wide repeated source list into avoidable clone
        // work and risks changing first-source dependency order.
        const SOURCES: usize = 128;
        let span = SqlSpan::new(0, 1).unwrap();
        let identifier = |name| Identifier::unquoted(name, span).unwrap();
        let query = AstQuery {
            distinct: false,
            projection: vec![SelectItem::Expr {
                expr: AstExpr {
                    kind: AstExprKind::Column(vec![identifier("U0"), identifier("ID")]),
                    span,
                },
                alias: None,
                span,
            }],
            from: Some(TableSource {
                table: identifier("USERS"),
                alias: Some(identifier("U0")),
                span,
            }),
            joins: (1..SOURCES)
                .map(|index| {
                    let orders = index % 2 == 1;
                    Join {
                        kind: JoinKind::Inner,
                        source: TableSource {
                            table: identifier(if orders { "ORDERS" } else { "USERS" }),
                            alias: Some(Identifier::unquoted(format!("T{index}"), span).unwrap()),
                            span,
                        },
                        condition: AstExpr {
                            kind: AstExprKind::Bool(true),
                            span,
                        },
                        span,
                    }
                })
                .collect(),
            selection: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            span,
        };
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let scope = BindingScope::from_query_sources(&catalog, &query).unwrap();

        assert_eq!(
            scope
                .source_identities()
                .map(|identity| identity.table_id.get())
                .collect::<Vec<_>>(),
            (0..SOURCES)
                .map(|index| if index % 2 == 0 { 1 } else { 6 })
                .collect::<Vec<_>>()
        );

        let bound = bind_query(
            &query,
            &catalog,
            database_uuid,
            &mut ParameterCollector::default(),
        )
        .unwrap();
        assert_eq!(
            bound
                .dependencies
                .iter()
                .map(|dependency| (dependency.table_id.get(), dependency.schema_version.get()))
                .collect::<Vec<_>>(),
            [(1, 1), (6, 1)]
        );
    }

    #[test]
    fn deep_projection_aliases_and_ordinals_reference_one_shared_output() {
        // Production break caught: cloning one deep projection for every ORDER
        // BY alias or ordinal can recurse during clone/drop and makes preparation
        // proportional to both expression depth and reference count.
        const DEPTH: usize = 16_384;
        const ORDER_REFERENCES: usize = 256;
        let span = SqlSpan::new(0, 1).unwrap();
        let identifier = |name| Identifier::unquoted(name, span).unwrap();
        let mut expression = AstExpr {
            kind: AstExprKind::Column(vec![identifier("U"), identifier("ID")]),
            span,
        };
        for _ in 0..DEPTH {
            expression = AstExpr {
                kind: AstExprKind::Binary {
                    left: Box::new(expression),
                    op: BinaryOp::Add,
                    right: Box::new(AstExpr {
                        kind: AstExprKind::Integer("1".to_owned()),
                        span,
                    }),
                },
                span,
            };
        }
        let mut order_by = Vec::with_capacity(ORDER_REFERENCES.saturating_mul(2));
        for _ in 0..ORDER_REFERENCES {
            order_by.push(OrderItem {
                expr: AstExpr {
                    kind: AstExprKind::Column(vec![identifier("DEEP")]),
                    span,
                },
                direction: OrderDirection::Default,
                nulls: NullPlacement::Default,
                span,
            });
            order_by.push(OrderItem {
                expr: AstExpr {
                    kind: AstExprKind::Integer("1".to_owned()),
                    span,
                },
                direction: OrderDirection::Default,
                nulls: NullPlacement::Default,
                span,
            });
        }
        let query = AstQuery {
            distinct: false,
            projection: vec![SelectItem::Expr {
                expr: expression,
                alias: Some(identifier("DEEP")),
                span,
            }],
            from: Some(TableSource {
                table: identifier("USERS"),
                alias: Some(identifier("U")),
                span,
            }),
            joins: Vec::new(),
            selection: None,
            group_by: Vec::new(),
            having: None,
            order_by,
            span,
        };
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let mut parameters = ParameterCollector::default();

        let bound = bind_query(&query, &catalog, database_uuid, &mut parameters).unwrap();

        assert_eq!(bound.order_by.len(), ORDER_REFERENCES * 2);
        assert!(
            bound
                .order_by
                .iter()
                .all(|order| matches!(&order.expression, BoundOrderExpression::Projection(0)))
        );

        drop(bound);
        drop(query);
    }

    #[test]
    fn group_index_confirms_structural_equality_inside_colliding_bucket() {
        // Production break caught: accepting a hash match without structural
        // confirmation would accept an unrelated GROUP BY expression whenever
        // two keys collide.
        let span = SqlSpan::new(0, 1).unwrap();
        let integer = |value| {
            BoundExpr::new(
                BoundExprKind::Literal(Value::Int64(value)),
                SqlType::Int64,
                false,
                span,
            )
        };
        let group_by = vec![integer(1)];
        let index = GroupIndex::with_key(&group_by, |_| 0);

        assert_eq!(index.buckets.len(), 1);
        assert!(index.contains(&integer(1)));
        assert!(!index.contains(&integer(2)));
    }

    #[test]
    fn large_group_projection_and_order_sets_bind_with_indexed_membership() {
        // Production break caught: scanning every GROUP BY tree for every
        // projection and ORDER BY expression grows preparation quadratically.
        const SCALE: usize = 4_096;
        let span = SqlSpan::new(0, 1).unwrap();
        let identifier = |name| Identifier::unquoted(name, span).unwrap();
        let group_expression = |offset: usize| AstExpr {
            kind: AstExprKind::Binary {
                left: Box::new(AstExpr {
                    kind: AstExprKind::Column(vec![identifier("U"), identifier("ID")]),
                    span,
                }),
                op: BinaryOp::Add,
                right: Box::new(AstExpr {
                    kind: AstExprKind::Integer(offset.to_string()),
                    span,
                }),
            },
            span,
        };
        let mut projection = (1..=SCALE)
            .map(|offset| SelectItem::Expr {
                expr: group_expression(offset),
                alias: None,
                span,
            })
            .collect::<Vec<_>>();
        projection.push(SelectItem::Expr {
            expr: AstExpr {
                kind: AstExprKind::Aggregate {
                    function: crate::sql::ast::AggregateFunction::Count,
                    argument: crate::sql::ast::AggregateArgument::Wildcard(span),
                },
                span,
            },
            alias: Some(identifier("N")),
            span,
        });
        let query = AstQuery {
            distinct: false,
            projection,
            from: Some(TableSource {
                table: identifier("USERS"),
                alias: Some(identifier("U")),
                span,
            }),
            joins: Vec::new(),
            selection: None,
            group_by: (1..=SCALE).map(group_expression).collect(),
            having: None,
            order_by: (1..=SCALE)
                .map(|offset| OrderItem {
                    expr: group_expression(offset),
                    direction: OrderDirection::Default,
                    nulls: NullPlacement::Default,
                    span,
                })
                .collect(),
            span,
        };
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let mut parameters = ParameterCollector::default();

        let bound = bind_query(&query, &catalog, database_uuid, &mut parameters).unwrap();

        assert_eq!(bound.columns.len(), SCALE + 1);
        assert_eq!(bound.order_by.len(), SCALE);
    }

    #[test]
    fn order_by_alias_expression_and_ordinal_obey_completed_projection_rules() {
        // Production break caught: binding ORDER BY before projection, or only
        // accepting bare columns, rejects supported alias and ordinal forms.
        for sql in [
            "SELECT U.ID AS USER_ID, U.NAME FROM USERS U ORDER BY USER_ID",
            "SELECT U.ID, U.NAME FROM USERS U ORDER BY 2 DESC NULLS LAST",
            "SELECT U.ID FROM USERS U ORDER BY U.ID + 1",
        ] {
            assert!(prepare_fixture(sql).is_ok(), "{sql}");
        }

        for sql in [
            "SELECT U.ID FROM USERS U ORDER BY 0",
            "SELECT U.ID FROM USERS U ORDER BY 2",
            "SELECT U.ID FROM USERS U ORDER BY 1.0",
            "SELECT U.ID FROM USERS U ORDER BY -1",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
    }

    #[test]
    fn grouped_expressions_require_structural_equivalence_not_column_membership() {
        // Production break caught: checking only referenced columns would
        // accept a computed projection whose grouping key has a different
        // value for every source row.
        let grouped = prepare_fixture(
            "SELECT U.ID + 1 AS GROUP_KEY, COUNT(O.ID) AS N \
             FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID \
             GROUP BY U.ID + 1 HAVING U.ID + 1 > 0 ORDER BY GROUP_KEY",
        );
        assert!(grouped.is_ok(), "{grouped:?}");
        for sql in [
            "SELECT U.ID + 1, COUNT(*) FROM USERS U GROUP BY U.ID",
            "SELECT U.ID, COUNT(*) FROM USERS U",
            "SELECT COUNT(*) FROM USERS U ORDER BY U.ID",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
    }

    #[test]
    fn nested_aggregates_and_aggregate_free_clauses_are_bind_errors() {
        // Production break caught: allowing a nested aggregate can produce an
        // execution tree with no meaningful input row scope.
        for sql in [
            "SELECT SUM(COUNT(U.ID)) FROM USERS U",
            "SELECT U.ID FROM USERS U WHERE COUNT(U.ID) > 0",
            "SELECT U.ID FROM USERS U GROUP BY COUNT(U.ID)",
            "SELECT U.ID FROM USERS U LEFT JOIN ORDERS O ON COUNT(O.ID) > 0",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
    }

    #[test]
    fn where_and_join_parameters_keep_their_real_catalog_constraints() {
        // Production break caught: skipping ON or WHERE binding leaves either
        // parameter untyped and defers a deterministic prepare-time failure.
        let statement = prepare_fixture(
            "SELECT U.ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = :join_id \
             WHERE U.ID = :user_id ORDER BY U.ID",
        )
        .unwrap();
        assert_eq!(
            statement
                .parameters()
                .iter()
                .map(|parameter| (parameter.identifier(), parameter.data_type()))
                .collect::<Vec<_>>(),
            [
                (
                    &crate::ParameterIdentifier::Name("JOIN_ID".to_owned()),
                    Some(DataType::Int64),
                ),
                (
                    &crate::ParameterIdentifier::Name("USER_ID".to_owned()),
                    Some(DataType::Int64),
                ),
            ]
        );
        assert!(matches!(statement.kind, BoundStatementKind::Query { .. }));
    }
}
