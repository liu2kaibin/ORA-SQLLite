use crate::{
    DbError, ErrorCode, Result,
    api::{ColumnMetadata, DataType},
    sql::{
        Identifier, SqlType,
        ast::{AstExpr, AstExprKind, AstQuery, NullPlacement, OrderDirection, SelectItem},
        bound::{BoundExpr, BoundExprKind, SchemaDependency},
    },
    storage::catalog::CatalogReader,
};

use super::{BindingScope, ParameterCollector, ResolvedColumn, bind_expr};

/// Owned, name-free semantics for one relational query.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundQuery {
    pub(crate) distinct: bool,
    pub(crate) join_conditions: Vec<BoundExpr>,
    pub(crate) selection: Option<BoundExpr>,
    pub(crate) group_by: Vec<BoundExpr>,
    pub(crate) having: Option<BoundExpr>,
    pub(crate) projection: Vec<BoundExpr>,
    pub(crate) order_by: Vec<BoundOrderItem>,
    pub(crate) columns: Vec<ColumnMetadata>,
    pub(crate) dependencies: Vec<SchemaDependency>,
}

/// A bound ORDER BY item, including its parsed direction and NULL placement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundOrderItem {
    pub(crate) expression: BoundExpr,
    pub(crate) direction: OrderDirection,
    pub(crate) nulls: NullPlacement,
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
    let scope = BindingScope::from_query_sources(catalog, query)?;
    let dependencies = dependencies_for_scope(&scope, database_uuid)?;

    let mut join_conditions = Vec::with_capacity(query.joins.len());
    for join in &query.joins {
        let condition = bind_condition(&join.condition, &scope, parameters)?;
        reject_aggregate(&condition, "JOIN ON")?;
        join_conditions.push(condition);
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
    let mut aliases = Vec::new();
    for item in &query.projection {
        match item {
            SelectItem::Expr { expr, alias, .. } => {
                let bound = bind_expr(expr, &scope, None, parameters)?;
                let name = projection_name(alias.as_ref(), expr, &scope, columns.len())?;
                aliases.push(alias.as_ref().map(|alias| alias.as_str().to_owned()));
                columns.push(column_metadata(name, &bound));
                projection.push(bound);
            }
            SelectItem::Wildcard { qualifier, span } => {
                for column in scope.expand_star(qualifier.as_ref())? {
                    projection.push(bound_column(&column, *span));
                    columns.push(column_metadata(
                        column.name.clone(),
                        projection.last().unwrap(),
                    ));
                    aliases.push(None);
                }
            }
        }
    }

    let mut order_by = Vec::with_capacity(query.order_by.len());
    for item in &query.order_by {
        let expression = if let Some(position) = order_ordinal(&item.expr, projection.len())? {
            projection[position].clone()
        } else if let Some(expression) = order_alias(&item.expr, &aliases, &projection)? {
            expression
        } else {
            bind_expr(&item.expr, &scope, None, parameters)?
        };
        aggregate_profile(&expression)?;
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
        .chain(order_by.iter().map(|item| &item.expression))
        .map(aggregate_profile)
        .try_fold(false, |found, aggregate| {
            aggregate.map(|aggregate| found || aggregate)
        })?;
    if !group_by.is_empty() || has_aggregate {
        if let Some(having) = &having {
            validate_having_grouping(having, &group_by)?;
        }
        for expression in &projection {
            validate_grouping(expression, &group_by)?;
        }
        for item in &order_by {
            validate_grouping(&item.expression, &group_by)?;
        }
    }

    Ok(BoundQuery {
        distinct: query.distinct,
        join_conditions,
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
    scope: &BindingScope,
    parameters: &mut ParameterCollector,
) -> Result<BoundExpr> {
    bind_expr(expression, scope, Some(SqlType::Bool), parameters)
}

fn dependencies_for_scope(
    scope: &BindingScope,
    database_uuid: [u8; 16],
) -> Result<Vec<SchemaDependency>> {
    let mut dependencies = Vec::new();
    for column in scope.expand_star(None)? {
        let dependency = SchemaDependency {
            database_uuid,
            table_id: column.table_id,
            schema_version: column.schema_version,
        };
        if !dependencies.contains(&dependency) {
            dependencies.push(dependency);
        }
    }
    Ok(dependencies)
}

fn bound_column(column: &ResolvedColumn, span: crate::sql::SqlSpan) -> BoundExpr {
    BoundExpr::new(
        BoundExprKind::Column {
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
    aliases: &[Option<String>],
    projection: &[BoundExpr],
) -> Result<Option<BoundExpr>> {
    let AstExprKind::Column(parts) = &expression.kind else {
        return Ok(None);
    };
    let [identifier] = parts.as_slice() else {
        return Ok(None);
    };
    let mut matches = aliases
        .iter()
        .enumerate()
        .filter(|(_, alias)| alias.as_deref() == Some(identifier.as_str()))
        .map(|(index, _)| index);
    let Some(index) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(bind_error("ORDER BY alias is ambiguous"));
    }
    Ok(Some(projection[index].clone()))
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

fn validate_having_grouping(expression: &BoundExpr, group_by: &[BoundExpr]) -> Result<()> {
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
            _ => validate_grouping(expression, group_by)?,
        }
    }
    Ok(())
}

fn validate_grouping(expression: &BoundExpr, group_by: &[BoundExpr]) -> Result<()> {
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
                            validate_ungrouped_subtree(child, group_by)?;
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
        validate_ungrouped_subtree(root, group_by)?;
    }
    Ok(())
}

fn validate_ungrouped_subtree(expression: &BoundExpr, group_by: &[BoundExpr]) -> Result<()> {
    if is_constant(expression)
        || group_by
            .iter()
            .any(|group| structurally_equal(expression, group))
    {
        return Ok(());
    }
    Err(bind_error(
        "ungrouped expression must be structurally equivalent to GROUP BY",
    ))
}

fn is_constant(root: &BoundExpr) -> bool {
    let mut work = vec![(root, false)];
    while let Some((expression, _)) = work.pop() {
        if matches!(expression.kind, BoundExprKind::Column { .. }) {
            return false;
        }
        push_children(expression, false, &mut work);
    }
    true
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
                    table_id: left_table,
                    column_id: left_column,
                    ordinal: left_ordinal,
                },
                BoundExprKind::Column {
                    table_id: right_table,
                    column_id: right_column,
                    ordinal: right_ordinal,
                },
            ) if left_table == right_table
                && left_column == right_column
                && left_ordinal == right_ordinal => {}
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
        BoundExprKind::Literal(_) | BoundExprKind::Column { .. } | BoundExprKind::Parameter(_) => 0,
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
        | BoundExprKind::Parameter(_)
        | BoundExprKind::Aggregate { argument: None, .. } => {}
    }
}

fn bind_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Bind, message)
}

#[cfg(test)]
mod tests {
    use crate::{
        ErrorCode,
        api::{ColumnMetadata, DataType},
        sql::{bind::test_support::prepare_fixture, bound::BoundStatementKind},
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
