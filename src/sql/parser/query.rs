use sqlparser::ast::{
    Distinct, Expr, GroupByExpr, Ident, JoinConstraint, JoinOperator, ObjectName, ObjectNamePart,
    OrderByKind, Query, Select, SelectFlavor, SelectItem as ParserSelectItem,
    SelectItemQualifiedWildcardKind, Spanned, TableAlias, TableFactor, WildcardAdditionalOptions,
};
use sqlparser::dialect::OracleDialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, Tokenizer};

use super::AstBudget;
use super::expr::convert_expr;
use super::source::SourceMap;
use crate::sql::ast::{
    AggregateArgument, AstExpr, AstExprKind, AstQuery, Join, JoinKind, NullPlacement,
    OrderDirection, OrderItem, SelectItem, TableSource,
};
use crate::sql::{Identifier, SqlLimits, SqlSpan};
use crate::{DbError, ErrorCode, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryTokenKind {
    Comma,
    Inner,
    Join,
    Left,
    Outer,
    Semicolon,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueryToken {
    kind: QueryTokenKind,
    span: SqlSpan,
}

struct QueryTokenIndex {
    tokens: Vec<QueryToken>,
}

impl QueryTokenIndex {
    fn new(source_map: &SourceMap<'_>) -> Result<Self> {
        let raw_tokens = Tokenizer::new(&OracleDialect, source_map.sql())
            .tokenize_with_location()
            .map_err(|error| {
                source_map
                    .sqlparser_location(error.location)
                    .map(|location| {
                        DbError::new(ErrorCode::Syntax, error.message).with_sql_location(location)
                    })
                    .unwrap_or_else(|mapping_error| mapping_error)
            })?;
        let mut tokens = Vec::with_capacity(raw_tokens.len());
        for token in raw_tokens {
            if matches!(token.token, Token::Whitespace(_)) {
                continue;
            }
            let kind = match token.token {
                Token::Comma => QueryTokenKind::Comma,
                Token::SemiColon => QueryTokenKind::Semicolon,
                Token::Word(word) if word.keyword == Keyword::INNER => QueryTokenKind::Inner,
                Token::Word(word) if word.keyword == Keyword::JOIN => QueryTokenKind::Join,
                Token::Word(word) if word.keyword == Keyword::LEFT => QueryTokenKind::Left,
                Token::Word(word) if word.keyword == Keyword::OUTER => QueryTokenKind::Outer,
                _ => QueryTokenKind::Other,
            };
            tokens.push(QueryToken {
                kind,
                span: source_map.span(token.span)?,
            });
        }
        Ok(Self { tokens })
    }

    fn join_start_before(&self, relation_start: usize) -> Option<usize> {
        let index = self
            .tokens
            .partition_point(|token| token.span.end() <= relation_start);
        let join_index = index.checked_sub(1)?;
        let join = self.tokens.get(join_index)?;
        if join.kind != QueryTokenKind::Join {
            return None;
        }
        let Some(prefix_index) = join_index.checked_sub(1) else {
            return Some(join.span.start());
        };
        let prefix = self.tokens.get(prefix_index)?;
        match prefix.kind {
            QueryTokenKind::Inner | QueryTokenKind::Left => Some(prefix.span.start()),
            QueryTokenKind::Outer => prefix_index
                .checked_sub(1)
                .and_then(|left_index| self.tokens.get(left_index))
                .filter(|left| left.kind == QueryTokenKind::Left)
                .map_or(Some(prefix.span.start()), |left| Some(left.span.start())),
            _ => Some(join.span.start()),
        }
    }

    fn item_end_before(&self, boundary: usize) -> Option<usize> {
        let mut index = self
            .tokens
            .partition_point(|token| token.span.start() < boundary);
        while let Some(previous) = index.checked_sub(1) {
            let token = self.tokens.get(previous)?;
            if token.kind != QueryTokenKind::Comma {
                return Some(token.span.end());
            }
            index = previous;
        }
        None
    }

    fn query_end(&self) -> Option<usize> {
        self.tokens
            .iter()
            .rev()
            .find(|token| token.kind != QueryTokenKind::Semicolon)
            .map(|token| token.span.end())
    }
}

pub(super) fn convert_query(
    query: Query,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
    limits: &SqlLimits,
) -> Result<AstQuery> {
    let token_index = QueryTokenIndex::new(source_map)?;
    let query_partial_span = source_map.span(query.span()).ok();
    let query_start = query_partial_span.map_or(0, |span| span.start());

    let Query {
        with,
        body,
        order_by,
        limit_clause,
        fetch,
        locks,
        for_clause,
        settings,
        format_clause,
        pipe_operators,
    } = query;

    if let Some(with) = with {
        return Err(unsupported(source_map, source_map.span(with.span()).ok()));
    }
    if let Some(limit) = limit_clause {
        return Err(unsupported(source_map, source_map.span(limit.span()).ok()));
    }
    if let Some(fetch) = fetch {
        return Err(unsupported(source_map, source_map.span(fetch.span()).ok()));
    }
    if !locks.is_empty()
        || for_clause.is_some()
        || settings.is_some()
        || format_clause.is_some()
        || !pipe_operators.is_empty()
    {
        return Err(unsupported(source_map, query_partial_span));
    }

    let sqlparser::ast::SetExpr::Select(select) = *body else {
        return Err(unsupported(source_map, source_map.span(body.span()).ok()));
    };
    preflight_select(&select, source_map, limits, &token_index)?;

    let query_end = token_index
        .query_end()
        .ok_or_else(|| source_shape_error(query_start, source_map))?;
    let query_span = SqlSpan::new(query_start, query_end)?;
    with_ast_node(query_span, source_map, budget, |budget| {
        convert_select(
            *select,
            order_by,
            query_span,
            source_map,
            budget,
            &token_index,
        )
    })
}

fn preflight_select(
    select: &Select,
    source_map: &SourceMap<'_>,
    limits: &SqlLimits,
    token_index: &QueryTokenIndex,
) -> Result<()> {
    let select_span = source_map.span(select.span()).ok();
    let Select {
        select_token: _,
        optimizer_hints,
        distinct,
        select_modifiers,
        top,
        top_before_distinct: _,
        projection,
        exclude,
        into,
        from,
        lateral_views,
        prewhere,
        selection: _,
        connect_by,
        group_by,
        cluster_by,
        distribute_by,
        sort_by,
        having: _,
        named_window,
        qualify,
        window_before_qualify: _,
        value_table_mode,
        flavor,
    } = select;

    if !optimizer_hints.is_empty()
        || select_modifiers.is_some()
        || top.is_some()
        || exclude.is_some()
        || into.is_some()
        || !lateral_views.is_empty()
        || prewhere.is_some()
        || !connect_by.is_empty()
        || !cluster_by.is_empty()
        || !distribute_by.is_empty()
        || !sort_by.is_empty()
        || !named_window.is_empty()
        || qualify.is_some()
        || value_table_mode.is_some()
        || *flavor != SelectFlavor::Standard
    {
        return Err(unsupported(source_map, select_span));
    }
    if !matches!(distinct, None | Some(Distinct::Distinct)) {
        return Err(unsupported(source_map, select_span));
    }
    if projection.is_empty() || from.len() != 1 {
        return Err(unsupported(source_map, select_span));
    }
    if !matches!(group_by, GroupByExpr::Expressions(_, modifiers) if modifiers.is_empty()) {
        return Err(unsupported(
            source_map,
            source_map.span(group_by.span()).ok().or(select_span),
        ));
    }

    let table_with_joins = &from[0];
    if table_with_joins.joins.len() > limits.max_joins() as usize {
        let excess_index = limits.max_joins() as usize;
        let excess = &table_with_joins.joins[excess_index];
        let relation_span = source_map.span(excess.relation.span())?;
        let offset = token_index
            .join_start_before(relation_span.start())
            .unwrap_or(relation_span.start());
        return Err(join_limit_error(offset, source_map));
    }
    Ok(())
}

fn convert_select(
    select: Select,
    order_by: Option<sqlparser::ast::OrderBy>,
    query_span: SqlSpan,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
    token_index: &QueryTokenIndex,
) -> Result<AstQuery> {
    let Select {
        select_token: _,
        optimizer_hints: _,
        distinct,
        select_modifiers: _,
        top: _,
        top_before_distinct: _,
        projection,
        exclude: _,
        into: _,
        mut from,
        lateral_views: _,
        prewhere: _,
        selection,
        connect_by: _,
        group_by,
        cluster_by: _,
        distribute_by: _,
        sort_by: _,
        having,
        named_window: _,
        qualify: _,
        window_before_qualify: _,
        value_table_mode: _,
        flavor: _,
    } = select;
    let table_with_joins = from
        .pop()
        .ok_or_else(|| source_shape_error(query_span.start(), source_map))?;

    let mut converted_projection = Vec::with_capacity(projection.len());
    for item in projection {
        converted_projection.push(convert_select_item(item, source_map, budget)?);
    }

    let converted_from = convert_table_source(table_with_joins.relation, source_map, budget)?;
    let mut converted_joins = Vec::with_capacity(table_with_joins.joins.len());
    for join in table_with_joins.joins {
        let relation_start = source_map.span(join.relation.span())?.start();
        let start = token_index
            .join_start_before(relation_start)
            .ok_or_else(|| source_shape_error(relation_start, source_map))?;
        let converted = convert_join(join, start, source_map, budget)?;
        converted_joins.push(converted);
    }

    let selection = selection
        .map(|expr| convert_query_expr(expr, source_map, budget))
        .transpose()?;
    let group_by = match group_by {
        GroupByExpr::Expressions(expressions, _) => {
            convert_expr_list(expressions, source_map, budget)?
        }
        GroupByExpr::All(_) => return Err(unsupported(source_map, Some(query_span))),
    };
    let having = having
        .map(|expr| convert_query_expr(expr, source_map, budget))
        .transpose()?;
    let order_by = convert_order_by(order_by, query_span.end(), source_map, budget, token_index)?;

    Ok(AstQuery {
        distinct: matches!(distinct, Some(Distinct::Distinct)),
        projection: converted_projection,
        from: converted_from,
        joins: converted_joins,
        selection,
        group_by,
        having,
        order_by,
        span: query_span,
    })
}

fn convert_select_item(
    item: ParserSelectItem,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<SelectItem> {
    let span = source_map.span(item.span())?;
    with_ast_node(span, source_map, budget, |budget| match item {
        ParserSelectItem::UnnamedExpr(expr) => Ok(SelectItem::Expr {
            expr: convert_query_expr(expr, source_map, budget)?,
            alias: None,
            span,
        }),
        ParserSelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::Expr {
            expr: convert_query_expr(expr, source_map, budget)?,
            alias: Some(convert_identifier(alias, source_map)?),
            span,
        }),
        ParserSelectItem::Wildcard(options) if !wildcard_has_options(&options) => {
            Ok(SelectItem::Wildcard {
                qualifier: None,
                span,
            })
        }
        ParserSelectItem::QualifiedWildcard(
            SelectItemQualifiedWildcardKind::ObjectName(name),
            options,
        ) if !wildcard_has_options(&options) => Ok(SelectItem::Wildcard {
            qualifier: Some(convert_single_name(name, source_map)?),
            span,
        }),
        _ => Err(unsupported(source_map, Some(span))),
    })
}

fn wildcard_has_options(options: &WildcardAdditionalOptions) -> bool {
    let WildcardAdditionalOptions {
        wildcard_token: _,
        opt_ilike,
        opt_exclude,
        opt_except,
        opt_replace,
        opt_rename,
        opt_alias,
    } = options;
    opt_ilike.is_some()
        || opt_exclude.is_some()
        || opt_except.is_some()
        || opt_replace.is_some()
        || opt_rename.is_some()
        || opt_alias.is_some()
}

fn convert_table_source(
    relation: TableFactor,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<TableSource> {
    let span = source_map.span(relation.span())?;
    with_ast_node(span, source_map, budget, |_| match relation {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } if args.is_none()
            && with_hints.is_empty()
            && version.is_none()
            && !with_ordinality
            && partitions.is_empty()
            && json_path.is_none()
            && sample.is_none()
            && index_hints.is_empty() =>
        {
            Ok(TableSource {
                table: convert_single_name(name, source_map)?,
                alias: alias
                    .map(|alias| convert_table_alias(alias, source_map))
                    .transpose()?,
                span,
            })
        }
        _ => Err(unsupported(source_map, Some(span))),
    })
}

fn convert_table_alias(alias: TableAlias, source_map: &SourceMap<'_>) -> Result<Identifier> {
    let TableAlias {
        explicit,
        name,
        columns,
        at,
    } = alias;
    if explicit || !columns.is_empty() || at.is_some() {
        return Err(unsupported(source_map, source_map.span(name.span).ok()));
    }
    convert_identifier(name, source_map)
}

fn convert_join(
    join: sqlparser::ast::Join,
    start: usize,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<Join> {
    let partial_span = source_map.span(join.span())?;
    let sqlparser::ast::Join {
        relation,
        global,
        join_operator,
    } = join;
    if global {
        return Err(unsupported(source_map, Some(partial_span)));
    }
    let (kind, condition) = match join_operator {
        JoinOperator::Inner(JoinConstraint::On(condition)) => (JoinKind::Inner, condition),
        JoinOperator::Left(JoinConstraint::On(condition))
        | JoinOperator::LeftOuter(JoinConstraint::On(condition)) => (JoinKind::Left, condition),
        _ => return Err(unsupported(source_map, Some(partial_span))),
    };
    let span = SqlSpan::new(start, partial_span.end())?;
    with_ast_node(span, source_map, budget, |budget| {
        let source = convert_table_source(relation, source_map, budget)?;
        let condition = convert_query_expr(condition, source_map, budget)?;
        let end = condition.span.end();
        Ok(Join {
            kind,
            source,
            condition,
            span: SqlSpan::new(start, end)?,
        })
    })
}

fn convert_order_by(
    order_by: Option<sqlparser::ast::OrderBy>,
    query_end: usize,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
    token_index: &QueryTokenIndex,
) -> Result<Vec<OrderItem>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    if order_by.interpolate.is_some() {
        return Err(unsupported(
            source_map,
            source_map.span(order_by.span()).ok(),
        ));
    }
    let OrderByKind::Expressions(items) = order_by.kind else {
        return Err(unsupported(
            source_map,
            source_map.span(order_by.span()).ok(),
        ));
    };

    let starts = items
        .iter()
        .map(|item| source_map.span(item.expr.span()).map(|span| span.start()))
        .collect::<Result<Vec<_>>>()?;
    let mut converted = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        if item.with_fill.is_some() {
            return Err(unsupported(source_map, source_map.span(item.span()).ok()));
        }
        let start = starts[index];
        let boundary = starts.get(index + 1).copied().unwrap_or(query_end);
        let end = token_index
            .item_end_before(boundary)
            .ok_or_else(|| source_shape_error(start, source_map))?;
        let span = SqlSpan::new(start, end)?;
        converted.push(with_ast_node(span, source_map, budget, |budget| {
            Ok(OrderItem {
                expr: convert_query_expr(item.expr, source_map, budget)?,
                direction: match item.options.asc {
                    None => OrderDirection::Default,
                    Some(true) => OrderDirection::Ascending,
                    Some(false) => OrderDirection::Descending,
                },
                nulls: match item.options.nulls_first {
                    None => NullPlacement::Default,
                    Some(true) => NullPlacement::First,
                    Some(false) => NullPlacement::Last,
                },
                span,
            })
        })?);
    }
    Ok(converted)
}

fn convert_expr_list(
    expressions: Vec<Expr>,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<Vec<AstExpr>> {
    let mut converted = Vec::with_capacity(expressions.len());
    for expression in expressions {
        converted.push(convert_query_expr(expression, source_map, budget)?);
    }
    Ok(converted)
}

fn convert_query_expr(
    expression: Expr,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<AstExpr> {
    let converted = convert_expr(expression, source_map, budget)?;
    if let Some(span) = rownum_span(&converted) {
        return Err(unsupported(source_map, Some(span)));
    }
    Ok(converted)
}

fn rownum_span(expression: &AstExpr) -> Option<SqlSpan> {
    match &expression.kind {
        AstExprKind::Column(parts) => parts
            .iter()
            .find(|part| !part.is_quoted() && part.as_str() == "ROWNUM")
            .map(Identifier::span),
        AstExprKind::Unary { expr, .. } | AstExprKind::IsNull { expr, .. } => rownum_span(expr),
        AstExprKind::Binary { left, right, .. } => rownum_span(left).or_else(|| rownum_span(right)),
        AstExprKind::InList { expr, values, .. } => {
            rownum_span(expr).or_else(|| values.iter().find_map(rownum_span))
        }
        AstExprKind::Between {
            expr, low, high, ..
        } => rownum_span(expr)
            .or_else(|| rownum_span(low))
            .or_else(|| rownum_span(high)),
        AstExprKind::Like { expr, pattern, .. } => {
            rownum_span(expr).or_else(|| rownum_span(pattern))
        }
        AstExprKind::Aggregate {
            argument: AggregateArgument::Expr(expr),
            ..
        } => rownum_span(expr),
        AstExprKind::Null
        | AstExprKind::Bool(_)
        | AstExprKind::Integer(_)
        | AstExprKind::Decimal(_)
        | AstExprKind::Float(_)
        | AstExprKind::Text(_)
        | AstExprKind::Blob(_)
        | AstExprKind::Parameter(_)
        | AstExprKind::Aggregate {
            argument: AggregateArgument::Wildcard(_),
            ..
        } => None,
    }
}

fn convert_single_name(name: ObjectName, source_map: &SourceMap<'_>) -> Result<Identifier> {
    let span = source_map.span(name.span()).ok();
    let mut parts = name.0.into_iter();
    match (parts.next(), parts.next()) {
        (Some(ObjectNamePart::Identifier(identifier)), None) => {
            convert_identifier(identifier, source_map)
        }
        _ => Err(unsupported(source_map, span)),
    }
}

fn convert_identifier(identifier: Ident, source_map: &SourceMap<'_>) -> Result<Identifier> {
    let span = source_map.span(identifier.span)?;
    match identifier.quote_style {
        None => Identifier::unquoted(identifier.value, span),
        Some('"') => Identifier::quoted(identifier.value, span),
        _ => Err(unsupported(source_map, Some(span))),
    }
}

fn with_ast_node<T>(
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
    convert: impl FnOnce(&mut AstBudget) -> Result<T>,
) -> Result<T> {
    let location = source_map
        .location(span.start())
        .or_else(|_| source_map.location(0))?;
    budget.enter(location)?;
    let result = convert(budget);
    budget.leave();
    result
}

fn join_limit_error(offset: usize, source_map: &SourceMap<'_>) -> DbError {
    let error = DbError::new(
        ErrorCode::Syntax,
        "SQL query exceeds the configured join limit",
    );
    match source_map
        .location(offset)
        .or_else(|_| source_map.location(0))
    {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

fn unsupported(source_map: &SourceMap<'_>, span: Option<SqlSpan>) -> DbError {
    let offset = span.map_or(0, |span| span.start());
    let error = DbError::new(ErrorCode::Unsupported, "SQL query is not supported");
    match source_map
        .location(offset)
        .or_else(|_| source_map.location(0))
    {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

fn source_shape_error(offset: usize, source_map: &SourceMap<'_>) -> DbError {
    let error = DbError::new(
        ErrorCode::Internal,
        "parsed SQL query does not match its source tokens",
    );
    match source_map
        .location(offset)
        .or_else(|_| source_map.location(0))
    {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::ast::Statement;
    use sqlparser::dialect::OracleDialect;
    use sqlparser::parser::Parser;

    use super::convert_query;
    use crate::error::SqlLocation;
    use crate::sql::ast::{AstExprKind, AstQuery, JoinKind, NullPlacement, OrderDirection};
    use crate::sql::parser::AstBudget;
    use crate::sql::parser::source::SourceMap;
    use crate::sql::parser::tokens::tokenize;
    use crate::sql::{SqlLimits, SqlSpan};
    use crate::{DbError, ErrorCode, Result};

    fn parse_query(sql: &str) -> Result<AstQuery> {
        parse_query_with_limits(sql, &SqlLimits::default())
    }

    fn parse_query_with_limits(sql: &str, limits: &SqlLimits) -> Result<AstQuery> {
        let source_map = SourceMap::new(sql)?;
        let tokens = tokenize(sql, limits)?;
        let statements = Parser::new(&OracleDialect)
            .with_recursion_limit(limits.max_nesting() as usize)
            .with_tokens_with_locations(tokens)
            .parse_statements()
            .map_err(|error| DbError::new(ErrorCode::Syntax, error.to_string()))?;
        let mut statements = statements.into_iter();
        let query = match (statements.next(), statements.next()) {
            (Some(Statement::Query(query)), None) => *query,
            _ => {
                return Err(DbError::new(
                    ErrorCode::Syntax,
                    "query fixture did not parse as exactly one query",
                ));
            }
        };
        let mut budget = AstBudget::new(limits);
        convert_query(query, &source_map, &mut budget, limits)
    }

    #[test]
    fn query_surface_converts_without_dependency_types() {
        let query = parse_query(
            "SELECT DISTINCT U.ID AS USER_ID, COUNT(O.ID) AS N \
             FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID \
             WHERE U.ACTIVE = TRUE GROUP BY U.ID HAVING COUNT(O.ID) > 0 \
             ORDER BY USER_ID DESC NULLS FIRST",
        )
        .unwrap();

        assert!(query.distinct);
        assert_eq!(query.projection.len(), 2);
        assert_eq!(query.from.table.as_str(), "USERS");
        assert_eq!(query.from.alias.as_ref().unwrap().as_str(), "U");
        assert_eq!(query.joins.len(), 1);
        assert_eq!(query.joins[0].kind, JoinKind::Left);
        assert_eq!(query.group_by.len(), 1);
        assert_eq!(query.order_by.len(), 1);
        assert_eq!(query.order_by[0].direction, OrderDirection::Descending);
        assert_eq!(query.order_by[0].nulls, NullPlacement::First);
    }

    #[test]
    fn wildcards_alias_forms_inner_join_and_positive_order_ordinal_are_accepted() {
        for sql in [
            "SELECT * FROM USERS",
            "SELECT U.* FROM USERS U",
            "SELECT U.ID USER_ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID ORDER BY 1 ASC NULLS LAST",
        ] {
            assert!(parse_query(sql).is_ok(), "{sql}");
        }

        let query = parse_query(
            "SELECT U.ID USER_ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID ORDER BY 1",
        )
        .unwrap();
        assert!(matches!(
            query.order_by[0].expr.kind,
            AstExprKind::Integer(ref value) if value == "1"
        ));
    }

    #[test]
    fn every_excluded_query_family_is_project_unsupported_with_a_location() {
        for (family, sql) in [
            ("derived subquery", "SELECT X.ID FROM (SELECT ID FROM T) X"),
            ("cte", "WITH X AS (SELECT ID FROM T) SELECT ID FROM X"),
            ("union", "SELECT ID FROM A UNION SELECT ID FROM B"),
            ("intersect", "SELECT ID FROM A INTERSECT SELECT ID FROM B"),
            ("minus", "SELECT ID FROM A MINUS SELECT ID FROM B"),
            ("natural join", "SELECT A.ID FROM A NATURAL JOIN B"),
            ("using join", "SELECT A.ID FROM A INNER JOIN B USING (ID)"),
            ("cross join", "SELECT A.ID FROM A CROSS JOIN B"),
            (
                "full join",
                "SELECT A.ID FROM A FULL OUTER JOIN B ON B.ID = A.ID",
            ),
            (
                "right join",
                "SELECT A.ID FROM A RIGHT JOIN B ON B.ID = A.ID",
            ),
            ("join without on", "SELECT A.ID FROM A INNER JOIN B"),
            ("table alias with as", "SELECT U.ID FROM USERS AS U"),
            ("multiple base tables", "SELECT A.ID FROM A, B"),
            ("missing base table", "SELECT 1"),
            ("limit", "SELECT ID FROM A LIMIT 1"),
            ("offset", "SELECT ID FROM A OFFSET 1 ROWS"),
            ("fetch", "SELECT ID FROM A FETCH FIRST 1 ROWS ONLY"),
            ("rownum", "SELECT ID FROM A WHERE ROWNUM <= 1"),
            ("select for update", "SELECT ID FROM A FOR UPDATE"),
        ] {
            let error = parse_query(sql).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported, "{family}: {error:?}");
            assert!(error.sql_location().is_some(), "{family}: {error:?}");
        }
    }

    #[test]
    fn query_fixture_requires_exactly_one_query_statement() {
        for sql in ["DELETE FROM T", "SELECT A FROM T; SELECT B FROM U"] {
            assert_eq!(
                parse_query(sql).unwrap_err().code(),
                ErrorCode::Syntax,
                "{sql}"
            );
        }
    }

    #[test]
    fn exact_authored_query_component_spans_are_preserved() {
        let query = parse_query(
            "SELECT U.*, U.ID USER_ID FROM USERS U LEFT OUTER JOIN ORDERS O ON O.USER_ID = U.ID ORDER BY 2 DESC NULLS LAST",
        )
        .unwrap();

        assert_eq!(query.span(), SqlSpan::new(0, 109).unwrap());
        assert_eq!(query.projection[0].span(), SqlSpan::new(7, 10).unwrap());
        assert_eq!(query.projection[1].span(), SqlSpan::new(12, 24).unwrap());
        assert_eq!(query.from.span(), SqlSpan::new(30, 37).unwrap());
        assert_eq!(query.joins[0].source.span(), SqlSpan::new(54, 62).unwrap());
        assert_eq!(query.joins[0].span(), SqlSpan::new(38, 82).unwrap());
        assert_eq!(query.order_by[0].span(), SqlSpan::new(92, 109).unwrap());
    }

    #[test]
    fn join_limit_is_checked_at_the_first_excess_join_before_expression_conversion() {
        let sql = "SELECT A.ID FROM A INNER JOIN B ON CASE WHEN 1 = 1 THEN 1 END = 1 LEFT JOIN C ON C.ID = A.ID";
        let limits = SqlLimits::default().with_max_joins(1).unwrap();
        let error = parse_query_with_limits(sql, &limits).unwrap_err();

        assert_eq!(error.code(), ErrorCode::Syntax);
        assert_eq!(
            error.sql_location(),
            Some(SqlLocation::new(66, 1, 67).unwrap())
        );
    }

    #[test]
    fn join_limit_location_does_not_depend_on_the_previous_expression_partial_span() {
        let sql = "SELECT A.ID FROM A INNER JOIN B ON B.ID IS NULL LEFT JOIN C ON C.ID = A.ID";
        let limits = SqlLimits::default().with_max_joins(1).unwrap();
        let error = parse_query_with_limits(sql, &limits).unwrap_err();

        assert_eq!(error.code(), ErrorCode::Syntax);
        assert_eq!(
            error.sql_location(),
            Some(SqlLocation::new(48, 1, 49).unwrap())
        );
    }

    #[test]
    fn query_structural_nodes_share_the_ast_budget() {
        let insufficient = SqlLimits::default().with_max_ast_nodes(3).unwrap();
        let error = parse_query_with_limits("SELECT A FROM T", &insufficient).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Syntax);

        let exact = SqlLimits::default().with_max_ast_nodes(4).unwrap();
        assert!(parse_query_with_limits("SELECT A FROM T", &exact).is_ok());
    }
}
