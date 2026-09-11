use sqlparser::ast::{
    ConnectByKind, Distinct, Expr, GroupByExpr, GroupByWithModifier, Ident, JoinConstraint,
    JoinOperator, ObjectName, ObjectNamePart, OrderByKind, Query, Select, SelectFlavor,
    SelectItem as ParserSelectItem, SelectItemQualifiedWildcardKind, SetOperator, Spanned,
    TableAlias, TableFactor, WildcardAdditionalOptions,
};
use sqlparser::dialect::OracleDialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

use super::expr::convert_expr;
use super::source::SourceMap;
use super::{AstBudget, unquoted_identifier};
use crate::sql::ast::{
    AggregateArgument, AstExpr, AstExprKind, AstQuery, Join, JoinKind, NullPlacement,
    OrderDirection, OrderItem, SelectItem, TableSource,
};
use crate::sql::{Identifier, SqlLimits, SqlSpan};
use crate::{DbError, ErrorCode, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryTokenKind {
    Comma,
    Dot,
    LeftParenthesis,
    RightParenthesis,
    Semicolon,
    Word,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelWord {
    All,
    By,
    Dimension,
    Ignore,
    Keep,
    Main,
    Model,
    Nav,
    On,
    Partition,
    Reference,
    Return,
    Rows,
    Single,
    Unique,
    Updated,
}

impl ModelWord {
    fn from_unquoted(value: &str) -> Option<Self> {
        [
            ("ALL", Self::All),
            ("BY", Self::By),
            ("DIMENSION", Self::Dimension),
            ("IGNORE", Self::Ignore),
            ("KEEP", Self::Keep),
            ("MAIN", Self::Main),
            ("MODEL", Self::Model),
            ("NAV", Self::Nav),
            ("ON", Self::On),
            ("PARTITION", Self::Partition),
            ("REFERENCE", Self::Reference),
            ("RETURN", Self::Return),
            ("ROWS", Self::Rows),
            ("SINGLE", Self::Single),
            ("UNIQUE", Self::Unique),
            ("UPDATED", Self::Updated),
        ]
        .into_iter()
        .find_map(|(candidate, word)| value.eq_ignore_ascii_case(candidate).then_some(word))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueryToken {
    kind: QueryTokenKind,
    keyword: Keyword,
    model_word: Option<ModelWord>,
    depth: usize,
    matching_left_parenthesis: Option<usize>,
    matching_right_parenthesis: Option<usize>,
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
        Self::from_tokens(&raw_tokens, source_map)
    }

    fn from_tokens(raw_tokens: &[TokenWithSpan], source_map: &SourceMap<'_>) -> Result<Self> {
        let mut tokens = Vec::with_capacity(raw_tokens.len());
        let mut depth = 0usize;
        let mut open_parentheses = Vec::new();
        for token in raw_tokens {
            if matches!(token.token, Token::Whitespace(_)) {
                continue;
            }
            if matches!(token.token, Token::RParen) {
                depth = depth.saturating_sub(1);
            }
            let (keyword, model_word) = match &token.token {
                Token::Word(word) => (
                    word.keyword,
                    word.quote_style
                        .is_none()
                        .then(|| ModelWord::from_unquoted(&word.value))
                        .flatten(),
                ),
                _ => (Keyword::NoKeyword, None),
            };
            let kind = match &token.token {
                Token::Comma => QueryTokenKind::Comma,
                Token::Period => QueryTokenKind::Dot,
                Token::LParen => QueryTokenKind::LeftParenthesis,
                Token::RParen => QueryTokenKind::RightParenthesis,
                Token::SemiColon => QueryTokenKind::Semicolon,
                Token::Word(_) => QueryTokenKind::Word,
                _ => QueryTokenKind::Other,
            };
            let matching_left_parenthesis = if matches!(token.token, Token::RParen) {
                open_parentheses.pop()
            } else {
                None
            };
            let token_index = tokens.len();
            tokens.push(QueryToken {
                kind,
                keyword,
                model_word,
                depth,
                matching_left_parenthesis,
                matching_right_parenthesis: None,
                span: source_map.span(token.span)?,
            });
            if let Some(opening) = matching_left_parenthesis {
                tokens[opening].matching_right_parenthesis = Some(token_index);
            }
            if matches!(token.token, Token::LParen) {
                depth = depth.saturating_add(1);
                open_parentheses.push(token_index);
            }
        }
        Ok(Self { tokens })
    }

    fn join_start_before(&self, relation_start: usize) -> Option<usize> {
        let index = self
            .tokens
            .partition_point(|token| token.span.end() <= relation_start);
        let join_index = index.checked_sub(1)?;
        let join = self.tokens.get(join_index)?;
        if join.keyword != Keyword::JOIN {
            return None;
        }
        let Some(prefix_index) = join_index.checked_sub(1) else {
            return Some(join.span.start());
        };
        let prefix = self.tokens.get(prefix_index)?;
        match prefix.keyword {
            Keyword::INNER
            | Keyword::LEFT
            | Keyword::RIGHT
            | Keyword::FULL
            | Keyword::CROSS
            | Keyword::NATURAL => Some(prefix.span.start()),
            Keyword::OUTER => prefix_index
                .checked_sub(1)
                .and_then(|outer_prefix_index| self.tokens.get(outer_prefix_index))
                .filter(|outer_prefix| {
                    matches!(
                        outer_prefix.keyword,
                        Keyword::LEFT | Keyword::RIGHT | Keyword::FULL
                    )
                })
                .map_or(Some(prefix.span.start()), |outer_prefix| {
                    Some(outer_prefix.span.start())
                }),
            _ => Some(join.span.start()),
        }
    }

    fn keyword_span_after(&self, keyword: Keyword, start: usize) -> Option<SqlSpan> {
        self.tokens
            .iter()
            .find(|token| {
                token.depth == 0 && token.keyword == keyword && token.span.start() >= start
            })
            .map(|token| token.span)
    }

    fn first_keyword_span_after(&self, keywords: &[Keyword], start: usize) -> Option<SqlSpan> {
        self.tokens
            .iter()
            .find(|token| {
                token.depth == 0 && token.span.start() >= start && keywords.contains(&token.keyword)
            })
            .map(|token| token.span)
    }

    fn keyword_span_before(&self, keyword: Keyword, start: usize, end: usize) -> Option<SqlSpan> {
        self.tokens
            .iter()
            .rev()
            .find(|token| {
                token.depth == 0
                    && token.keyword == keyword
                    && token.span.start() >= start
                    && token.span.end() <= end
            })
            .map(|token| token.span)
    }

    fn keyword_span_between(&self, keyword: Keyword, start: usize, end: usize) -> Option<SqlSpan> {
        self.tokens
            .iter()
            .find(|token| {
                token.depth == 0
                    && token.keyword == keyword
                    && token.span.start() >= start
                    && token.span.end() <= end
            })
            .map(|token| token.span)
    }

    fn keyword_pair_end(
        &self,
        first: Keyword,
        second: Keyword,
        start: usize,
        end: usize,
    ) -> Option<usize> {
        self.tokens.windows(2).find_map(|pair| {
            let [left, right] = pair else {
                return None;
            };
            (left.depth == 0
                && right.depth == 0
                && left.keyword == first
                && right.keyword == second
                && left.span.start() >= start
                && right.span.end() <= end)
                .then_some(right.span.end())
        })
    }

    fn model_clause_span(&self) -> Option<SqlSpan> {
        self.tokens.iter().enumerate().find_map(|(index, model)| {
            (model.depth == 0
                && model.model_word == Some(ModelWord::Model)
                && self.model_can_start_clause(index)
                && self.has_model_clause_prefix(index + 1, model.depth))
            .then_some(model.span)
        })
    }

    fn model_can_start_clause(&self, model_index: usize) -> bool {
        let Some(previous_index) = model_index.checked_sub(1) else {
            return true;
        };
        let Some(previous) = self.tokens.get(previous_index) else {
            return true;
        };
        !matches!(previous.kind, QueryTokenKind::Comma | QueryTokenKind::Dot)
            && !matches!(
                previous.keyword,
                Keyword::FROM | Keyword::JOIN | Keyword::AS
            )
    }

    fn has_model_clause_prefix(&self, start: usize, depth: usize) -> bool {
        if self.has_main_model_dimension_prefix(start, depth) {
            return true;
        }

        match self.model_word_at(start, depth) {
            Some(ModelWord::Return) => {
                matches!(
                    self.model_word_at(start + 1, depth),
                    Some(ModelWord::All | ModelWord::Updated)
                ) && self.model_word_at(start + 2, depth) == Some(ModelWord::Rows)
            }
            Some(ModelWord::Ignore | ModelWord::Keep) => {
                self.model_word_at(start + 1, depth) == Some(ModelWord::Nav)
            }
            Some(ModelWord::Unique) => {
                self.model_word_at(start + 1, depth) == Some(ModelWord::Dimension)
                    || (self.model_word_at(start + 1, depth) == Some(ModelWord::Single)
                        && self.model_word_at(start + 2, depth) == Some(ModelWord::Reference))
            }
            Some(ModelWord::Reference) => {
                self.model_identifier_at(start + 1, depth)
                    && self.model_word_at(start + 2, depth) == Some(ModelWord::On)
                    && self.model_kind_at(start + 3, depth) == Some(QueryTokenKind::LeftParenthesis)
            }
            _ => false,
        }
    }

    fn has_main_model_dimension_prefix(&self, start: usize, depth: usize) -> bool {
        let mut index = start;
        if self.model_word_at(index, depth) == Some(ModelWord::Main) {
            if !self.model_identifier_at(index + 1, depth) {
                return false;
            }
            index += 2;
        }

        if self.model_word_at(index, depth) == Some(ModelWord::Partition) {
            if self.model_word_at(index + 1, depth) != Some(ModelWord::By) {
                return false;
            }
            let Some(partition_end) = self.model_parenthesized_end(index + 2, depth) else {
                return false;
            };
            index = partition_end + 1;
        }

        self.model_word_at(index, depth) == Some(ModelWord::Dimension)
            && self.model_word_at(index + 1, depth) == Some(ModelWord::By)
            && self.model_kind_at(index + 2, depth) == Some(QueryTokenKind::LeftParenthesis)
    }

    fn model_word_at(&self, index: usize, depth: usize) -> Option<ModelWord> {
        self.tokens
            .get(index)
            .filter(|token| token.depth == depth)
            .and_then(|token| token.model_word)
    }

    fn model_kind_at(&self, index: usize, depth: usize) -> Option<QueryTokenKind> {
        self.tokens
            .get(index)
            .filter(|token| token.depth == depth)
            .map(|token| token.kind)
    }

    fn model_identifier_at(&self, index: usize, depth: usize) -> bool {
        self.model_kind_at(index, depth) == Some(QueryTokenKind::Word)
    }

    fn model_parenthesized_end(&self, index: usize, depth: usize) -> Option<usize> {
        let opening = self.tokens.get(index).filter(|token| {
            token.depth == depth && token.kind == QueryTokenKind::LeftParenthesis
        })?;
        let closing_index = opening.matching_right_parenthesis?;
        self.tokens
            .get(closing_index)
            .filter(|token| token.depth == depth && token.kind == QueryTokenKind::RightParenthesis)
            .map(|_| closing_index)
    }

    fn window_clause_span(&self) -> Option<SqlSpan> {
        self.tokens.windows(3).find_map(|window| {
            let [function_end, over, window_spec] = window else {
                return None;
            };
            let function_name = function_end
                .matching_left_parenthesis
                .and_then(|opening| opening.checked_sub(1))
                .and_then(|name| self.tokens.get(name));
            let has_function_name = function_name.is_some_and(|name| {
                name.kind == QueryTokenKind::Word
                    && !matches!(
                        name.keyword,
                        Keyword::SELECT
                            | Keyword::FROM
                            | Keyword::WHERE
                            | Keyword::GROUP
                            | Keyword::HAVING
                            | Keyword::ORDER
                    )
            });
            let has_window_spec = window_spec.kind == QueryTokenKind::LeftParenthesis
                || (window_spec.kind == QueryTokenKind::Word
                    && window_spec.keyword == Keyword::NoKeyword);
            (has_function_name
                && over.keyword == Keyword::OVER
                && function_end.depth == over.depth
                && window_spec.depth == over.depth
                && has_window_spec)
                .then_some(over.span)
        })
    }

    fn kind_span_between(&self, kind: QueryTokenKind, start: usize, end: usize) -> Option<SqlSpan> {
        self.tokens
            .iter()
            .find(|token| {
                token.kind == kind && token.span.start() >= start && token.span.end() <= end
            })
            .map(|token| token.span)
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

pub(super) fn preflight_unparsed_query_syntax(
    tokens: &[TokenWithSpan],
    source_map: &SourceMap<'_>,
) -> Result<()> {
    let token_index = QueryTokenIndex::from_tokens(tokens, source_map)?;
    match (
        token_index.model_clause_span(),
        token_index.window_clause_span(),
    ) {
        (Some(model), Some(window)) if window.start() < model.start() => {
            Err(unsupported(source_map, Some(window)))
        }
        (Some(model), _) => Err(unsupported(source_map, Some(model))),
        (None, Some(window)) => Err(unsupported(source_map, Some(window))),
        (None, None) => Ok(()),
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
    let body_partial_span = source_map.span(query.body.span()).ok();
    let body_end = body_partial_span.map_or(query_start, |span| span.end());
    let tail_clause_start = query
        .order_by
        .as_ref()
        .and_then(|order_by| source_map.span(order_by.span()).ok())
        .map_or(body_end, |span| span.end());

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
        return Err(unsupported(
            source_map,
            token_index
                .first_keyword_span_after(&[Keyword::LIMIT, Keyword::OFFSET], tail_clause_start)
                .or_else(|| source_map.span(limit.span()).ok()),
        ));
    }
    if let Some(fetch) = fetch {
        return Err(unsupported(
            source_map,
            token_index
                .keyword_span_after(Keyword::FETCH, tail_clause_start)
                .or_else(|| source_map.span(fetch.span()).ok()),
        ));
    }
    if !locks.is_empty() {
        return Err(unsupported(
            source_map,
            token_index
                .keyword_span_after(Keyword::FOR, tail_clause_start)
                .or(query_partial_span),
        ));
    }
    if for_clause.is_some()
        || settings.is_some()
        || format_clause.is_some()
        || !pipe_operators.is_empty()
    {
        return Err(unsupported(source_map, query_partial_span));
    }

    let select = match *body {
        sqlparser::ast::SetExpr::Select(select) => select,
        sqlparser::ast::SetExpr::SetOperation {
            left,
            op,
            set_quantifier: _,
            right,
        } => {
            let keyword = match op {
                SetOperator::Union => Keyword::UNION,
                SetOperator::Except => Keyword::EXCEPT,
                SetOperator::Intersect => Keyword::INTERSECT,
                SetOperator::Minus => Keyword::MINUS,
            };
            return Err(unsupported(
                source_map,
                token_index
                    .keyword_span_between(
                        keyword,
                        source_map.span(left.span())?.end(),
                        source_map.span(right.span())?.start(),
                    )
                    .or(body_partial_span),
            ));
        }
        body => return Err(unsupported(source_map, source_map.span(body.span()).ok())),
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
            body_end,
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

    if !connect_by.is_empty() {
        let span = match &connect_by[0] {
            ConnectByKind::ConnectBy {
                connect_token,
                nocycle: _,
                relationships: _,
            } => source_map.span(connect_token.0.span).ok(),
            ConnectByKind::StartWith {
                start_token,
                condition: _,
            } => source_map.span(start_token.0.span).ok(),
        };
        return Err(unsupported(source_map, span.or(select_span)));
    }
    if let Some(qualify) = qualify {
        let qualify_start = source_map.span(qualify.span())?.start();
        return Err(unsupported(
            source_map,
            token_index
                .keyword_span_before(Keyword::QUALIFY, 0, qualify_start)
                .or(select_span),
        ));
    }
    if let Some(span) = token_index.window_clause_span() {
        return Err(unsupported(source_map, Some(span)));
    }
    if !optimizer_hints.is_empty()
        || select_modifiers.is_some()
        || top.is_some()
        || exclude.is_some()
        || into.is_some()
        || !lateral_views.is_empty()
        || prewhere.is_some()
        || !cluster_by.is_empty()
        || !distribute_by.is_empty()
        || !sort_by.is_empty()
        || !named_window.is_empty()
        || value_table_mode.is_some()
        || *flavor != SelectFlavor::Standard
    {
        return Err(unsupported(source_map, select_span));
    }
    if !matches!(distinct, None | Some(Distinct::Distinct)) {
        return Err(unsupported(source_map, select_span));
    }
    if projection.is_empty() || from.is_empty() {
        return Err(unsupported(source_map, select_span));
    }
    if from.len() != 1 {
        let first_end = source_map.span(from[0].span())?.end();
        let second_start = source_map.span(from[1].span())?.start();
        return Err(unsupported(
            source_map,
            token_index
                .kind_span_between(QueryTokenKind::Comma, first_end, second_start)
                .or(select_span),
        ));
    }
    if !matches!(group_by, GroupByExpr::Expressions(_, modifiers) if modifiers.is_empty()) {
        let select_end = select_span.map_or(source_map.sql().len(), |span| span.end());
        let group_clause_start = token_index
            .keyword_pair_end(Keyword::GROUP, Keyword::BY, 0, select_end)
            .unwrap_or(0);
        let group_span = match group_by {
            GroupByExpr::All(_) => token_index.keyword_span_after(Keyword::ALL, group_clause_start),
            GroupByExpr::Expressions(expressions, modifiers) => {
                let expressions_end = expressions
                    .last()
                    .and_then(|expression| source_map.span(expression.span()).ok())
                    .map_or(group_clause_start, |span| span.end());
                match modifiers.first() {
                    Some(
                        GroupByWithModifier::Rollup
                        | GroupByWithModifier::Cube
                        | GroupByWithModifier::Totals,
                    ) => token_index.keyword_span_after(Keyword::WITH, expressions_end),
                    Some(GroupByWithModifier::GroupingSets(_)) => {
                        token_index.keyword_span_after(Keyword::GROUPING, expressions_end)
                    }
                    None => source_map.span(group_by.span()).ok(),
                }
            }
        };
        return Err(unsupported(source_map, group_span.or(select_span)));
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
    body_end: usize,
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

    let converted_from =
        convert_table_source(table_with_joins.relation, source_map, budget, token_index)?;
    let mut converted_joins = Vec::with_capacity(table_with_joins.joins.len());
    for join in table_with_joins.joins {
        let relation_start = source_map.span(join.relation.span())?.start();
        let start = token_index
            .join_start_before(relation_start)
            .ok_or_else(|| source_shape_error(relation_start, source_map))?;
        let converted = convert_join(join, start, source_map, budget, token_index)?;
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
    let order_by = convert_order_by(
        order_by,
        body_end,
        query_span.end(),
        source_map,
        budget,
        token_index,
    )?;

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
    token_index: &QueryTokenIndex,
) -> Result<TableSource> {
    let span = source_map.span(relation.span())?;
    if matches!(&relation, TableFactor::Pivot { .. }) {
        return Err(unsupported(
            source_map,
            token_index
                .keyword_span_between(Keyword::PIVOT, span.start(), span.end())
                .or(Some(span)),
        ));
    }
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
                    .map(|alias| convert_table_alias(alias, span.start(), source_map, token_index))
                    .transpose()?,
                span,
            })
        }
        _ => Err(unsupported(source_map, Some(span))),
    })
}

fn convert_table_alias(
    alias: TableAlias,
    relation_start: usize,
    source_map: &SourceMap<'_>,
    token_index: &QueryTokenIndex,
) -> Result<Identifier> {
    let TableAlias {
        explicit,
        name,
        columns,
        at,
    } = alias;
    if explicit || !columns.is_empty() || at.is_some() {
        let name_span = source_map.span(name.span).ok();
        let explicit_span = name_span.and_then(|span| {
            token_index.keyword_span_before(Keyword::AS, relation_start, span.start())
        });
        return Err(unsupported(source_map, explicit_span.or(name_span)));
    }
    convert_identifier(name, source_map)
}

fn convert_join(
    join: sqlparser::ast::Join,
    start: usize,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
    token_index: &QueryTokenIndex,
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
        _ => {
            return Err(unsupported(
                source_map,
                SqlSpan::new(start, partial_span.end()).ok(),
            ));
        }
    };
    let span = SqlSpan::new(start, partial_span.end())?;
    with_ast_node(span, source_map, budget, |budget| {
        let source = convert_table_source(relation, source_map, budget, token_index)?;
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
    body_end: usize,
    query_end: usize,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
    token_index: &QueryTokenIndex,
) -> Result<Vec<OrderItem>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    let order_items_start = token_index
        .keyword_pair_end(Keyword::ORDER, Keyword::BY, body_end, query_end)
        .unwrap_or(body_end);
    if let Some(interpolate) = &order_by.interpolate {
        let interpolate_content_start = source_map
            .span(interpolate.span())
            .ok()
            .filter(|span| span.start() < span.end())
            .map_or(query_end, |span| span.start());
        return Err(unsupported(
            source_map,
            token_index.keyword_span_before(
                Keyword::INTERPOLATE,
                order_items_start,
                interpolate_content_start,
            ),
        ));
    }
    let OrderByKind::Expressions(items) = order_by.kind else {
        return Err(unsupported(
            source_map,
            token_index.keyword_span_after(Keyword::ALL, order_items_start),
        ));
    };

    let starts = items
        .iter()
        .map(|item| source_map.span(item.expr.span()).map(|span| span.start()))
        .collect::<Result<Vec<_>>>()?;
    let mut converted = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        if item.with_fill.is_some() {
            let expr_span = source_map.span(item.expr.span())?;
            let boundary = starts.get(index + 1).copied().unwrap_or(query_end);
            return Err(unsupported(
                source_map,
                token_index.keyword_span_between(Keyword::WITH, expr_span.end(), boundary),
            ));
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
        None => unquoted_identifier(identifier.value, span, source_map),
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
    use sqlparser::ast::{OrderByKind, Query, Statement};
    use sqlparser::dialect::{ClickHouseDialect, Dialect, GenericDialect, OracleDialect};
    use sqlparser::parser::Parser;
    use sqlparser::tokenizer::Tokenizer;

    use super::{convert_query, preflight_unparsed_query_syntax};
    use crate::error::SqlLocation;
    use crate::sql::ast::{
        AggregateArgument, AggregateFunction, AstExpr, AstExprKind, AstQuery, BinaryOp, JoinKind,
        NullPlacement, OrderDirection, SelectItem,
    };
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
        convert_parsed_query(&source_map, limits, &OracleDialect, tokens)
    }

    fn parse_query_with_dialect(sql: &str, dialect: &dyn Dialect) -> Result<AstQuery> {
        let limits = SqlLimits::default();
        let source_map = SourceMap::new(sql)?;
        let tokens = Tokenizer::new(dialect, sql)
            .tokenize_with_location()
            .map_err(|error| DbError::new(ErrorCode::Syntax, error.to_string()))?;
        convert_parsed_query(&source_map, &limits, dialect, tokens)
    }

    fn parse_upstream_query(sql: &str, dialect: &dyn Dialect) -> Query {
        let statements = Parser::parse_sql(dialect, sql).unwrap();
        let [Statement::Query(query)] = statements.as_slice() else {
            panic!("expected exactly one upstream query: {statements:?}");
        };
        (**query).clone()
    }

    fn convert_upstream_query(sql: &str, query: Query) -> Result<AstQuery> {
        let limits = SqlLimits::default();
        let source_map = SourceMap::new(sql)?;
        let mut budget = AstBudget::new(&limits);
        convert_query(query, &source_map, &mut budget, &limits)
    }

    fn preflight_only(sql: &str) -> Result<()> {
        let source_map = SourceMap::new(sql)?;
        let tokens = tokenize(sql, &SqlLimits::default())?;
        preflight_unparsed_query_syntax(&tokens, &source_map)
    }

    fn convert_parsed_query(
        source_map: &SourceMap<'_>,
        limits: &SqlLimits,
        dialect: &dyn Dialect,
        tokens: Vec<sqlparser::tokenizer::TokenWithSpan>,
    ) -> Result<AstQuery> {
        preflight_unparsed_query_syntax(&tokens, source_map)?;
        let statements = Parser::new(dialect)
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
        convert_query(query, source_map, &mut budget, limits)
    }

    fn assert_column(expression: &AstExpr, parts: &[&str], span: SqlSpan) {
        assert_eq!(expression.span(), span);
        let AstExprKind::Column(actual) = &expression.kind else {
            panic!("expected column expression, got {expression:?}");
        };
        assert_eq!(
            actual.iter().map(|part| part.as_str()).collect::<Vec<_>>(),
            parts
        );
    }

    fn assert_unsupported_at<T: std::fmt::Debug>(result: Result<T>, family: &str, offset: usize) {
        let error = result.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unsupported, "{family}: {error:?}");
        assert_eq!(
            error.sql_location(),
            Some(SqlLocation::new(offset, 1, offset + 1).unwrap()),
            "{family}: {error:?}"
        );
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
        for (family, sql, offset) in [
            (
                "derived subquery",
                "SELECT X.ID FROM (SELECT ID FROM T) X",
                18,
            ),
            (
                "scalar subquery",
                "SELECT (SELECT ID FROM T) AS X FROM U",
                8,
            ),
            ("cte", "WITH X AS (SELECT ID FROM T) SELECT ID FROM X", 0),
            (
                "recursive cte",
                "WITH RECURSIVE X AS (SELECT ID FROM T) SELECT ID FROM X",
                0,
            ),
            ("union", "SELECT ID FROM A UNION SELECT ID FROM B", 17),
            (
                "intersect",
                "SELECT ID FROM A INTERSECT SELECT ID FROM B",
                17,
            ),
            ("minus", "SELECT ID FROM A MINUS SELECT ID FROM B", 17),
            ("window", "SELECT SUM(A) OVER () FROM T", 14),
            ("qualify", "SELECT A FROM T QUALIFY A = 1", 16),
            (
                "pivot",
                "SELECT * FROM SALES PIVOT(SUM(AMOUNT) FOR QUARTER IN ('Q1'))",
                20,
            ),
            ("model", "SELECT A FROM T MODEL RETURN UPDATED ROWS", 16),
            ("connect by", "SELECT A FROM T CONNECT BY PRIOR A = B", 16),
            ("natural join", "SELECT A.ID FROM A NATURAL JOIN B", 19),
            (
                "using join",
                "SELECT A.ID FROM A INNER JOIN B USING (ID)",
                19,
            ),
            ("cross join", "SELECT A.ID FROM A CROSS JOIN B", 19),
            (
                "full join",
                "SELECT A.ID FROM A FULL OUTER JOIN B ON B.ID = A.ID",
                19,
            ),
            (
                "right join",
                "SELECT A.ID FROM A RIGHT JOIN B ON B.ID = A.ID",
                19,
            ),
            ("join without on", "SELECT A.ID FROM A INNER JOIN B", 19),
            ("table alias with as", "SELECT U.ID FROM USERS AS U", 23),
            ("multiple base tables", "SELECT A.ID FROM A, B", 18),
            ("missing base table", "SELECT 1", 0),
            ("limit", "SELECT ID FROM A LIMIT 1", 17),
            ("offset", "SELECT ID FROM A OFFSET 1 ROWS", 17),
            ("fetch", "SELECT ID FROM A FETCH FIRST 1 ROWS ONLY", 17),
            ("rownum", "SELECT ID FROM A WHERE ROWNUM <= 1", 23),
            ("select for update", "SELECT ID FROM A FOR UPDATE", 17),
        ] {
            assert_unsupported_at(parse_query(sql), family, offset);
        }

        for (family, sql, offset, dialect) in [
            (
                "except",
                "SELECT ID FROM A EXCEPT SELECT ID FROM B",
                17,
                &GenericDialect as &dyn Dialect,
            ),
            (
                "order by all",
                "SELECT A FROM T ORDER BY ALL",
                25,
                &ClickHouseDialect {} as &dyn Dialect,
            ),
            (
                "order by with fill",
                "SELECT A FROM T ORDER BY A WITH FILL",
                27,
                &ClickHouseDialect {} as &dyn Dialect,
            ),
            (
                "order by interpolate",
                "SELECT A FROM T ORDER BY A WITH FILL INTERPOLATE",
                37,
                &ClickHouseDialect {} as &dyn Dialect,
            ),
            (
                "group by modifier",
                "SELECT A FROM T GROUP BY A WITH ROLLUP",
                27,
                &ClickHouseDialect {} as &dyn Dialect,
            ),
        ] {
            assert_unsupported_at(parse_query_with_dialect(sql, dialect), family, offset);
        }
    }

    #[test]
    fn every_group_by_all_and_modifier_shape_is_rejected_at_its_authored_start() {
        for (family, sql, offset) in [
            ("group by all", "SELECT A FROM T GROUP BY ALL", 25),
            (
                "group by rollup",
                "SELECT A FROM T GROUP BY A WITH ROLLUP",
                27,
            ),
            ("group by cube", "SELECT A FROM T GROUP BY A WITH CUBE", 27),
            (
                "group by totals",
                "SELECT A FROM T GROUP BY A WITH TOTALS",
                27,
            ),
            (
                "grouping sets modifier",
                "SELECT A FROM T GROUP BY A GROUPING SETS ((A))",
                27,
            ),
        ] {
            assert_unsupported_at(
                parse_query_with_dialect(sql, &ClickHouseDialect {}),
                family,
                offset,
            );
        }
    }

    #[test]
    fn order_by_all_reaches_the_upstream_all_variant_before_conversion_rejects_it() {
        let sql = "SELECT T.ALL FROM T ORDER BY ALL";
        let query = parse_upstream_query(sql, &ClickHouseDialect {});
        assert!(matches!(
            query.order_by.as_ref().map(|order_by| &order_by.kind),
            Some(OrderByKind::All(_))
        ));

        assert_unsupported_at(convert_upstream_query(sql, query), "order by all", 29);
    }

    #[test]
    fn repeated_keyword_identifiers_do_not_steal_clause_error_locations() {
        for (family, sql, offset) in [
            ("for update", "SELECT T.FOR FROM T FOR UPDATE", 20),
            ("limit", "SELECT T.LIMIT FROM T LIMIT 1", 22),
            ("offset", "SELECT T.OFFSET FROM T OFFSET 1 ROWS", 23),
            (
                "qualify",
                "SELECT T.QUALIFY FROM T QUALIFY T.QUALIFY = 1",
                24,
            ),
            (
                "connect by",
                "SELECT T.CONNECT FROM T CONNECT BY PRIOR T.CONNECT = A",
                24,
            ),
            (
                "union",
                "SELECT T.UNION FROM T UNION SELECT U.UNION FROM U",
                22,
            ),
        ] {
            assert_unsupported_at(parse_query(sql), family, offset);
        }

        for (family, sql, offset) in [
            (
                "order by interpolate",
                "SELECT T.INTERPOLATE FROM T ORDER BY A WITH FILL INTERPOLATE",
                49,
            ),
            (
                "order by with fill",
                "SELECT T.WITH FROM T ORDER BY A WITH FILL",
                32,
            ),
            (
                "group by with cube",
                "SELECT T.WITH FROM T GROUP BY A WITH CUBE",
                32,
            ),
        ] {
            assert_unsupported_at(
                parse_query_with_dialect(sql, &ClickHouseDialect {}),
                family,
                offset,
            );
        }
    }

    #[test]
    fn order_extensions_and_limit_clauses_report_the_first_authored_clause() {
        assert_unsupported_at(
            parse_query_with_dialect(
                "SELECT A FROM T ORDER BY INTERPOLATE WITH FILL INTERPOLATE",
                &ClickHouseDialect {},
            ),
            "interpolate after same-named order expression",
            47,
        );

        for (family, sql, offset) in [
            (
                "offset before limit",
                "SELECT T.OFFSET FROM T OFFSET 1 ROWS LIMIT 2",
                23,
            ),
            (
                "limit before offset",
                "SELECT T.LIMIT FROM T LIMIT 2 OFFSET 1 ROWS",
                22,
            ),
        ] {
            assert_unsupported_at(parse_query(sql), family, offset);
        }
    }

    #[test]
    fn model_and_over_identifiers_are_not_raw_syntax_false_positives() {
        for sql in [
            "SELECT MODEL FROM T",
            "SELECT T.MODEL FROM T",
            "SELECT OVER FROM T",
            "SELECT T.OVER FROM T",
            "SELECT A FROM T WHERE MODEL = 1",
            "SELECT A FROM T WHERE OVER = 1",
        ] {
            assert!(preflight_only(sql).is_ok(), "raw preflight rejected {sql}");
            assert!(parse_query(sql).is_ok(), "query conversion rejected {sql}");
        }

        let join = parse_query("SELECT T.ID FROM T INNER JOIN U ON MODEL = U.ID").unwrap();
        assert_column(
            match &join.joins[0].condition.kind {
                AstExprKind::Binary { left, .. } => left,
                other => panic!("expected binary JOIN condition, got {other:?}"),
            },
            &["MODEL"],
            SqlSpan::new(35, 40).unwrap(),
        );

        let alias = parse_query("SELECT (A) OVER FROM T").unwrap();
        let SelectItem::Expr {
            alias: Some(alias), ..
        } = &alias.projection[0]
        else {
            panic!("expected OVER implicit alias");
        };
        assert_eq!(alias.as_str(), "OVER");
        assert_eq!(alias.span(), SqlSpan::new(11, 15).unwrap());
    }

    #[test]
    fn model_clause_words_remain_valid_implicit_aliases_without_their_grammar_paths() {
        for alias in [
            "RULES",
            "MEASURES",
            "MAIN",
            "PARTITION",
            "DIMENSION",
            "RETURN",
            "REFERENCE",
            "IGNORE",
            "KEEP",
            "UNIQUE",
            "AUTOMATIC",
            "SEQUENTIAL",
        ] {
            let sql = format!("SELECT MODEL {alias} FROM T");
            assert!(preflight_only(&sql).is_ok(), "raw preflight rejected {sql}");

            let query = parse_query(&sql)
                .unwrap_or_else(|error| panic!("query conversion rejected {sql}: {error:?}"));
            let SelectItem::Expr {
                expr,
                alias: Some(actual_alias),
                ..
            } = &query.projection[0]
            else {
                panic!("expected MODEL with implicit alias {alias}: {query:?}");
            };
            assert_column(expr, &["MODEL"], SqlSpan::new(7, 12).unwrap());
            assert_eq!(actual_alias.as_str(), alias);
            assert_eq!(
                actual_alias.span(),
                SqlSpan::new(13, 13 + alias.len()).unwrap()
            );
        }

        let qualified = parse_query("SELECT T.MODEL RULES FROM T").unwrap();
        let SelectItem::Expr {
            expr,
            alias: Some(alias),
            ..
        } = &qualified.projection[0]
        else {
            panic!("expected qualified MODEL with RULES alias: {qualified:?}");
        };
        assert_column(expr, &["T", "MODEL"], SqlSpan::new(7, 14).unwrap());
        assert_eq!(alias.as_str(), "RULES");
    }

    #[test]
    fn raw_preflight_recognizes_structured_oracle_model_clause_prefixes() {
        for (family, sql) in [
            (
                "model main",
                "SELECT A FROM T MODEL MAIN FORECAST DIMENSION BY (D)",
            ),
            (
                "model partition",
                "SELECT A FROM T MODEL PARTITION BY (P, COALESCE(Q, 0)) DIMENSION BY (D)",
            ),
            (
                "model main partition",
                "SELECT A FROM T MODEL MAIN FORECAST PARTITION BY (P) DIMENSION BY (D)",
            ),
            ("model dimension", "SELECT A FROM T MODEL DIMENSION BY (D)"),
        ] {
            assert_unsupported_at(preflight_only(sql), family, 16);
        }

        for (family, sql, model_offset) in [
            (
                "model after RULES alias",
                "SELECT MODEL RULES FROM T MODEL DIMENSION BY (D)",
                26,
            ),
            (
                "model after MEASURES alias",
                "SELECT MODEL MEASURES FROM T MODEL DIMENSION BY (D)",
                29,
            ),
        ] {
            assert_unsupported_at(preflight_only(sql), family, model_offset);
        }
    }

    #[test]
    fn raw_preflight_rejects_only_actual_model_and_window_syntax_at_exact_tokens() {
        for (family, sql, offset) in [
            (
                "model after from",
                "SELECT A FROM T MODEL RETURN UPDATED ROWS",
                16,
            ),
            (
                "model after where",
                "SELECT A FROM T WHERE A = 1 MODEL RETURN UPDATED ROWS",
                28,
            ),
            (
                "model after group by",
                "SELECT A FROM T GROUP BY A MODEL RETURN UPDATED ROWS",
                27,
            ),
            (
                "model after having",
                "SELECT A FROM T GROUP BY A HAVING COUNT(*) > 0 MODEL RETURN UPDATED ROWS",
                47,
            ),
        ] {
            assert_unsupported_at(preflight_only(sql), family, offset);
        }
        assert_unsupported_at(
            preflight_only("SELECT SUM(A) OVER () FROM T"),
            "window preflight",
            14,
        );
        assert_unsupported_at(
            preflight_only("SELECT (SUM(A) OVER ()) FROM T"),
            "nested window preflight",
            15,
        );
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
    fn full_query_preserves_owned_expression_shapes_aliases_and_clause_spans() {
        let query = parse_query(
            "SELECT DISTINCT U.ID AS USER_ID, COUNT(O.ID) AS N FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID WHERE U.ACTIVE = TRUE GROUP BY U.ID HAVING COUNT(O.ID) > 0 ORDER BY USER_ID DESC NULLS FIRST",
        )
        .unwrap();

        assert_eq!(query.span(), SqlSpan::new(0, 194).unwrap());
        let SelectItem::Expr {
            expr: user_id,
            alias: Some(user_id_alias),
            span: user_id_item_span,
        } = &query.projection[0]
        else {
            panic!("expected aliased USER_ID projection");
        };
        assert_column(user_id, &["U", "ID"], SqlSpan::new(16, 20).unwrap());
        assert_eq!(user_id_alias.as_str(), "USER_ID");
        assert_eq!(user_id_alias.span(), SqlSpan::new(24, 31).unwrap());
        assert_eq!(*user_id_item_span, SqlSpan::new(16, 31).unwrap());

        let SelectItem::Expr {
            expr: count,
            alias: Some(count_alias),
            span: count_item_span,
        } = &query.projection[1]
        else {
            panic!("expected aliased COUNT projection");
        };
        assert!(matches!(
            &count.kind,
            AstExprKind::Aggregate {
                function: AggregateFunction::Count,
                argument: AggregateArgument::Expr(argument),
            } if {
                assert_column(argument, &["O", "ID"], SqlSpan::new(39, 43).unwrap());
                true
            }
        ));
        assert_eq!(count.span(), SqlSpan::new(33, 44).unwrap());
        assert_eq!(count_alias.as_str(), "N");
        assert_eq!(count_alias.span(), SqlSpan::new(48, 49).unwrap());
        assert_eq!(*count_item_span, SqlSpan::new(33, 49).unwrap());

        assert_eq!(query.from.span(), SqlSpan::new(55, 62).unwrap());
        assert_eq!(query.from.table.span(), SqlSpan::new(55, 60).unwrap());
        assert_eq!(
            query.from.alias.as_ref().unwrap().span(),
            SqlSpan::new(61, 62).unwrap()
        );
        assert_eq!(query.joins[0].span(), SqlSpan::new(63, 101).unwrap());
        assert_eq!(query.joins[0].source.span(), SqlSpan::new(73, 81).unwrap());
        let AstExprKind::Binary {
            left,
            op: BinaryOp::Equal,
            right,
        } = &query.joins[0].condition.kind
        else {
            panic!("expected equality JOIN condition");
        };
        assert_eq!(
            query.joins[0].condition.span(),
            SqlSpan::new(85, 101).unwrap()
        );
        assert_column(left, &["O", "USER_ID"], SqlSpan::new(85, 94).unwrap());
        assert_column(right, &["U", "ID"], SqlSpan::new(97, 101).unwrap());

        let selection = query.selection.as_ref().unwrap();
        let AstExprKind::Binary {
            left,
            op: BinaryOp::Equal,
            right,
        } = &selection.kind
        else {
            panic!("expected equality WHERE condition");
        };
        assert_eq!(selection.span(), SqlSpan::new(108, 123).unwrap());
        assert_column(left, &["U", "ACTIVE"], SqlSpan::new(108, 116).unwrap());
        assert!(matches!(right.kind, AstExprKind::Bool(true)));
        assert_eq!(right.span(), SqlSpan::new(119, 123).unwrap());

        assert_column(
            &query.group_by[0],
            &["U", "ID"],
            SqlSpan::new(133, 137).unwrap(),
        );
        let having = query.having.as_ref().unwrap();
        let AstExprKind::Binary {
            left,
            op: BinaryOp::GreaterThan,
            right,
        } = &having.kind
        else {
            panic!("expected greater-than HAVING condition");
        };
        assert_eq!(having.span(), SqlSpan::new(145, 160).unwrap());
        assert!(matches!(
            left.kind,
            AstExprKind::Aggregate {
                function: AggregateFunction::Count,
                ..
            }
        ));
        assert_eq!(left.span(), SqlSpan::new(145, 156).unwrap());
        assert!(matches!(right.kind, AstExprKind::Integer(ref value) if value == "0"));
        assert_eq!(right.span(), SqlSpan::new(159, 160).unwrap());

        assert_column(
            &query.order_by[0].expr,
            &["USER_ID"],
            SqlSpan::new(170, 177).unwrap(),
        );
        assert_eq!(query.order_by[0].span(), SqlSpan::new(170, 194).unwrap());
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
