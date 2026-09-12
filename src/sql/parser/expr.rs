#![deny(clippy::redundant_clone)]

use sqlparser::ast::{
    BinaryOperator as ParserBinaryOp, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, ObjectNamePart, Spanned, UnaryOperator as ParserUnaryOp, Value,
    ValueWithSpan,
};
use sqlparser::dialect::OracleDialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, Tokenizer};

#[cfg(test)]
use std::cell::Cell;

use super::source::SourceMap;
use super::{AstBudget, unquoted_identifier};
use crate::sql::ast::{
    AggregateArgument, AggregateFunction, AstExpr, AstExprKind, BinaryOp, UnaryOp,
};
use crate::sql::{Identifier, ParameterKey, SqlSpan};
use crate::{DbError, ErrorCode, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceToken {
    kind: SourceTokenKind,
    span: SqlSpan,
    matching_right_parenthesis: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceTokenKind {
    LeftParenthesis,
    RightParenthesis,
    Plus,
    Minus,
    Star,
    Not,
    Is,
    Null,
    Other,
}

const SOURCE_TOKEN_KIND_COUNT: usize = 9;

impl SourceTokenKind {
    const fn index(self) -> usize {
        match self {
            Self::LeftParenthesis => 0,
            Self::RightParenthesis => 1,
            Self::Plus => 2,
            Self::Minus => 3,
            Self::Star => 4,
            Self::Not => 5,
            Self::Is => 6,
            Self::Null => 7,
            Self::Other => 8,
        }
    }
}

pub(super) struct SourceTokenIndex {
    tokens: Vec<SourceToken>,
    kind_indices: [Vec<usize>; SOURCE_TOKEN_KIND_COUNT],
}

impl SourceTokenIndex {
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
        let mut index = Self {
            tokens: Vec::new(),
            kind_indices: std::array::from_fn(|_| Vec::new()),
        };
        let mut open_parentheses = Vec::new();
        for token in raw_tokens {
            record_source_token_probe();
            if matches!(token.token, Token::Whitespace(_)) {
                continue;
            }
            let kind = source_token_kind(&token.token);
            let token_index = index.tokens.len();
            index.kind_indices[kind.index()].push(token_index);
            index.tokens.push(SourceToken {
                kind,
                span: source_map.span(token.span)?,
                matching_right_parenthesis: None,
            });
            match kind {
                SourceTokenKind::LeftParenthesis => open_parentheses.push(token_index),
                SourceTokenKind::RightParenthesis => {
                    let opening = open_parentheses.pop().ok_or_else(|| {
                        source_shape_error(index.tokens[token_index].span.start(), source_map)
                    })?;
                    index.tokens[opening].matching_right_parenthesis = Some(token_index);
                }
                _ => {}
            }
        }
        if let Some(opening) = open_parentheses.pop() {
            return Err(source_shape_error(
                index.tokens[opening].span.start(),
                source_map,
            ));
        }
        Ok(index)
    }

    fn token_before(
        &self,
        boundary: usize,
        expected: SourceTokenKind,
        source_map: &SourceMap<'_>,
    ) -> Result<SourceToken> {
        let index = lower_bound_by(&self.tokens, |token| token.span.end() <= boundary);
        index
            .checked_sub(1)
            .and_then(|index| self.tokens.get(index))
            .copied()
            .filter(|token| token.kind == expected)
            .ok_or_else(|| source_shape_error(boundary, source_map))
    }

    fn token_after(
        &self,
        boundary: usize,
        expected: SourceTokenKind,
        source_map: &SourceMap<'_>,
    ) -> Result<SourceToken> {
        let index = lower_bound_by(&self.tokens, |token| token.span.start() < boundary);
        self.tokens
            .get(index)
            .copied()
            .filter(|token| token.kind == expected)
            .ok_or_else(|| source_shape_error(boundary, source_map))
    }

    fn token_between(
        &self,
        start: usize,
        end: usize,
        expected: SourceTokenKind,
        source_map: &SourceMap<'_>,
    ) -> Result<SourceToken> {
        let indices = &self.kind_indices[expected.index()];
        let position = lower_bound_by(indices, |index| self.tokens[*index].span.start() < start);
        indices
            .get(position)
            .and_then(|index| self.tokens.get(*index))
            .copied()
            .filter(|token| token.span.end() <= end)
            .ok_or_else(|| source_shape_error(start, source_map))
    }

    fn matching_right_parenthesis(
        &self,
        opening: SourceToken,
        source_map: &SourceMap<'_>,
    ) -> Result<SourceToken> {
        record_source_token_probe();
        opening
            .matching_right_parenthesis
            .and_then(|index| self.tokens.get(index))
            .copied()
            .filter(|token| token.kind == SourceTokenKind::RightParenthesis)
            .ok_or_else(|| source_shape_error(opening.span.start(), source_map))
    }
}

fn lower_bound_by<T>(values: &[T], mut is_before_boundary: impl FnMut(&T) -> bool) -> usize {
    let mut left = 0;
    let mut right = values.len();
    while left < right {
        let middle = left + (right - left) / 2;
        record_source_token_probe();
        if is_before_boundary(&values[middle]) {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    left
}

#[cfg(test)]
thread_local! {
    static SOURCE_TOKEN_PROBES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_source_token_probes() {
    SOURCE_TOKEN_PROBES.set(0);
}

#[cfg(test)]
fn source_token_probes() -> usize {
    SOURCE_TOKEN_PROBES.get()
}

#[cfg(test)]
fn measure_source_token_probes<T>(action: impl FnOnce() -> T) -> (T, usize) {
    reset_source_token_probes();
    let result = action();
    (result, source_token_probes())
}

#[inline]
fn record_source_token_probe() {
    #[cfg(test)]
    SOURCE_TOKEN_PROBES.set(SOURCE_TOKEN_PROBES.get() + 1);
}

fn source_token_index<'a>(
    budget: &'a mut AstBudget,
    source_map: &SourceMap<'_>,
) -> Result<&'a SourceTokenIndex> {
    if budget.source_tokens.is_none() {
        budget.source_tokens = Some(SourceTokenIndex::new(source_map)?);
    }
    budget.source_tokens.as_ref().ok_or_else(|| {
        DbError::new(
            ErrorCode::Internal,
            "SQL source token index was not initialized",
        )
    })
}

fn source_token_kind(token: &Token) -> SourceTokenKind {
    match token {
        Token::LParen => SourceTokenKind::LeftParenthesis,
        Token::RParen => SourceTokenKind::RightParenthesis,
        Token::Plus => SourceTokenKind::Plus,
        Token::Minus => SourceTokenKind::Minus,
        Token::Mul => SourceTokenKind::Star,
        Token::Word(word) if word.keyword == Keyword::NOT => SourceTokenKind::Not,
        Token::Word(word) if word.keyword == Keyword::IS => SourceTokenKind::Is,
        Token::Word(word) if word.keyword == Keyword::NULL => SourceTokenKind::Null,
        _ => SourceTokenKind::Other,
    }
}

fn token_before(
    boundary: usize,
    expected: SourceTokenKind,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<SourceToken> {
    source_token_index(budget, source_map)?.token_before(boundary, expected, source_map)
}

fn token_after(
    boundary: usize,
    expected: SourceTokenKind,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<SourceToken> {
    source_token_index(budget, source_map)?.token_after(boundary, expected, source_map)
}

fn token_between(
    start: usize,
    end: usize,
    expected: SourceTokenKind,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<SourceToken> {
    source_token_index(budget, source_map)?.token_between(start, end, expected, source_map)
}

fn matching_right_parenthesis(
    opening: SourceToken,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<SourceToken> {
    source_token_index(budget, source_map)?.matching_right_parenthesis(opening, source_map)
}

fn source_shape_error(offset: usize, source_map: &SourceMap<'_>) -> DbError {
    let error = DbError::new(
        ErrorCode::Internal,
        "parsed SQL expression does not match its source tokens",
    );
    match source_map.location(offset) {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

struct MoveOnlyParserExpr(Expr);

impl MoveOnlyParserExpr {
    const fn new(expr: Expr) -> Self {
        Self(expr)
    }

    fn span(&self) -> sqlparser::tokenizer::Span {
        self.0.span()
    }

    fn into_inner(self) -> Expr {
        self.0
    }
}

pub(super) fn convert_expr(
    expr: Expr,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<AstExpr> {
    convert_move_only_expr(MoveOnlyParserExpr::new(expr), source_map, budget)
}

fn convert_move_only_expr(
    expr: MoveOnlyParserExpr,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<AstExpr> {
    let upstream_span = expr.span();
    let location = source_map
        .span(upstream_span)
        .and_then(|span| source_map.location(span.start()))
        .or_else(|_| source_map.location(0))?;
    budget.enter(location)?;
    let result = convert_expr_inner(expr, upstream_span, source_map, budget);
    budget.leave();
    result
}

fn convert_expr_inner(
    expr: MoveOnlyParserExpr,
    upstream_span: sqlparser::tokenizer::Span,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<AstExpr> {
    let partial_span = source_map.span(upstream_span);
    match expr.into_inner() {
        Expr::Identifier(ident) => {
            if ident.quote_style.is_none()
                && (ident.value.eq_ignore_ascii_case("true")
                    || ident.value.eq_ignore_ascii_case("false"))
            {
                return Ok(AstExpr {
                    span: source_map.span(ident.span)?,
                    kind: AstExprKind::Bool(ident.value.eq_ignore_ascii_case("true")),
                });
            }
            let identifier = convert_identifier(ident, source_map)?;
            Ok(AstExpr {
                span: identifier.span(),
                kind: AstExprKind::Column(vec![identifier]),
            })
        }
        Expr::CompoundIdentifier(idents) if idents.len() == 2 => {
            let mut parts = Vec::new();
            for ident in idents {
                parts.push(convert_identifier(ident, source_map)?);
            }
            let [first, second] = parts.as_slice() else {
                return Err(source_shape_error(0, source_map));
            };
            Ok(AstExpr {
                span: SqlSpan::new(first.span().start(), second.span().end())?,
                kind: AstExprKind::Column(parts),
            })
        }
        Expr::Value(value) => {
            let span = source_map.span(value.span)?;
            Ok(AstExpr {
                kind: convert_value(value, source_map)?,
                span,
            })
        }
        Expr::UnaryOp { op, expr } => {
            let (op, source_kind) = convert_unary_op(op)
                .ok_or_else(|| unsupported(source_map, partial_span.as_ref().ok()))?;
            let expr = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*expr),
                source_map,
                budget,
            )?);
            let operator = token_before(expr.span.start(), source_kind, source_map, budget)?;
            Ok(AstExpr {
                span: SqlSpan::new(operator.span.start(), expr.span.end())?,
                kind: AstExprKind::Unary { op, expr },
            })
        }
        Expr::BinaryOp { left, op, right } => {
            let op = convert_binary_op(&op)
                .ok_or_else(|| unsupported(source_map, partial_span.as_ref().ok()))?;
            let left = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*left),
                source_map,
                budget,
            )?);
            let right = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*right),
                source_map,
                budget,
            )?);
            Ok(AstExpr {
                span: SqlSpan::new(left.span.start(), right.span.end())?,
                kind: AstExprKind::Binary { left, op, right },
            })
        }
        Expr::IsNull(expr) => {
            convert_is_null(MoveOnlyParserExpr::new(*expr), false, source_map, budget)
        }
        Expr::IsNotNull(expr) => {
            convert_is_null(MoveOnlyParserExpr::new(*expr), true, source_map, budget)
        }
        Expr::InList {
            expr,
            list,
            negated,
        } if !list.is_empty() => {
            let expr = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*expr),
                source_map,
                budget,
            )?);
            let mut values = Vec::new();
            for value in list {
                values.push(convert_move_only_expr(
                    MoveOnlyParserExpr::new(value),
                    source_map,
                    budget,
                )?);
            }
            let Some(first) = values.first() else {
                return Err(source_shape_error(expr.span.end(), source_map));
            };
            let opening = token_between(
                expr.span.end(),
                first.span.start(),
                SourceTokenKind::LeftParenthesis,
                source_map,
                budget,
            )?;
            let closing = matching_right_parenthesis(opening, source_map, budget)?;
            Ok(AstExpr {
                span: SqlSpan::new(expr.span.start(), closing.span.end())?,
                kind: AstExprKind::InList {
                    expr,
                    values,
                    negated,
                },
            })
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let expr = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*expr),
                source_map,
                budget,
            )?);
            let low = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*low),
                source_map,
                budget,
            )?);
            let high = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*high),
                source_map,
                budget,
            )?);
            Ok(AstExpr {
                span: SqlSpan::new(expr.span.start(), high.span.end())?,
                kind: AstExprKind::Between {
                    expr,
                    low,
                    high,
                    negated,
                },
            })
        }
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char: None,
        } => {
            let expr = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*expr),
                source_map,
                budget,
            )?);
            let pattern = Box::new(convert_move_only_expr(
                MoveOnlyParserExpr::new(*pattern),
                source_map,
                budget,
            )?);
            Ok(AstExpr {
                span: SqlSpan::new(expr.span.start(), pattern.span.end())?,
                kind: AstExprKind::Like {
                    expr,
                    pattern,
                    negated,
                },
            })
        }
        Expr::Nested(expr) => {
            let mut expr =
                convert_move_only_expr(MoveOnlyParserExpr::new(*expr), source_map, budget)?;
            let opening = token_before(
                expr.span.start(),
                SourceTokenKind::LeftParenthesis,
                source_map,
                budget,
            )?;
            let closing = token_after(
                expr.span.end(),
                SourceTokenKind::RightParenthesis,
                source_map,
                budget,
            )?;
            expr.span = SqlSpan::new(opening.span.start(), closing.span.end())?;
            Ok(expr)
        }
        Expr::Function(function) => convert_aggregate(function, source_map, budget),
        _ => Err(unsupported(source_map, partial_span.as_ref().ok())),
    }
}

fn convert_is_null(
    expr: MoveOnlyParserExpr,
    negated: bool,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<AstExpr> {
    let expr = Box::new(convert_move_only_expr(expr, source_map, budget)?);
    let is = token_after(expr.span.end(), SourceTokenKind::Is, source_map, budget)?;
    let end = if negated {
        let not = token_after(is.span.end(), SourceTokenKind::Not, source_map, budget)?;
        token_after(not.span.end(), SourceTokenKind::Null, source_map, budget)?
            .span
            .end()
    } else {
        token_after(is.span.end(), SourceTokenKind::Null, source_map, budget)?
            .span
            .end()
    };
    Ok(AstExpr {
        span: SqlSpan::new(expr.span.start(), end)?,
        kind: AstExprKind::IsNull { expr, negated },
    })
}

fn convert_identifier(ident: Ident, source_map: &SourceMap<'_>) -> Result<Identifier> {
    let span = source_map.span(ident.span)?;
    match ident.quote_style {
        None => unquoted_identifier(ident.value, span, source_map),
        Some('"') => Identifier::quoted(ident.value, span),
        _ => Err(unsupported(source_map, Some(&span))),
    }
}

fn convert_value(value: ValueWithSpan, source_map: &SourceMap<'_>) -> Result<AstExprKind> {
    let span = source_map.span(value.span)?;
    match value.value {
        Value::Null => Ok(AstExprKind::Null),
        Value::Boolean(value) => Ok(AstExprKind::Bool(value)),
        Value::SingleQuotedString(value) => Ok(AstExprKind::Text(value)),
        Value::HexStringLiteral(value) => {
            decode_blob(&value, span, source_map).map(AstExprKind::Blob)
        }
        Value::Placeholder(value) => ParameterKey::parse(&value, span).map(AstExprKind::Parameter),
        Value::Number(value, long) => convert_number(value, long, span, source_map),
        _ => Err(unsupported(source_map, Some(&span))),
    }
}

fn convert_number(
    value: String,
    long: bool,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
) -> Result<AstExprKind> {
    if long || !valid_number(&value) {
        return Err(syntax_error("invalid numeric literal", span, source_map));
    }
    if value.bytes().any(|byte| matches!(byte, b'e' | b'E')) {
        Ok(AstExprKind::Float(value))
    } else if value.contains('.') {
        Ok(AstExprKind::Decimal(value))
    } else {
        Ok(AstExprKind::Integer(value))
    }
}

fn valid_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    let integer_digits = consume_digits(bytes, &mut index);
    let mut fractional_digits = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        fractional_digits = consume_digits(bytes, &mut index);
    }
    if integer_digits == 0 && fractional_digits == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        if consume_digits(bytes, &mut index) == 0 {
            return false;
        }
    }
    index == bytes.len()
}

fn consume_digits(bytes: &[u8], index: &mut usize) -> usize {
    let start = *index;
    while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        *index += 1;
    }
    *index - start
}

fn decode_blob(value: &str, span: SqlSpan, source_map: &SourceMap<'_>) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(syntax_error("invalid blob literal", span, source_map));
    }
    let mut bytes = Vec::new();
    for pair in value.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(pair)
            .map_err(|_| syntax_error("invalid blob literal", span, source_map))?;
        bytes.push(
            u8::from_str_radix(text, 16)
                .map_err(|_| syntax_error("invalid blob literal", span, source_map))?,
        );
    }
    Ok(bytes)
}

fn convert_unary_op(op: ParserUnaryOp) -> Option<(UnaryOp, SourceTokenKind)> {
    match op {
        ParserUnaryOp::Plus => Some((UnaryOp::Positive, SourceTokenKind::Plus)),
        ParserUnaryOp::Minus => Some((UnaryOp::Negative, SourceTokenKind::Minus)),
        ParserUnaryOp::Not => Some((UnaryOp::Not, SourceTokenKind::Not)),
        _ => None,
    }
}

fn convert_binary_op(op: &ParserBinaryOp) -> Option<BinaryOp> {
    match op {
        ParserBinaryOp::Plus => Some(BinaryOp::Add),
        ParserBinaryOp::Minus => Some(BinaryOp::Subtract),
        ParserBinaryOp::Multiply => Some(BinaryOp::Multiply),
        ParserBinaryOp::Divide => Some(BinaryOp::Divide),
        ParserBinaryOp::Modulo => Some(BinaryOp::Modulo),
        ParserBinaryOp::Eq => Some(BinaryOp::Equal),
        ParserBinaryOp::NotEq => Some(BinaryOp::NotEqual),
        ParserBinaryOp::Lt => Some(BinaryOp::LessThan),
        ParserBinaryOp::LtEq => Some(BinaryOp::LessThanOrEqual),
        ParserBinaryOp::Gt => Some(BinaryOp::GreaterThan),
        ParserBinaryOp::GtEq => Some(BinaryOp::GreaterThanOrEqual),
        ParserBinaryOp::And => Some(BinaryOp::And),
        ParserBinaryOp::Or => Some(BinaryOp::Or),
        _ => None,
    }
}

fn convert_aggregate(
    function: Function,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<AstExpr> {
    let function_span = source_map.span(function.span()).ok();
    let Function {
        name,
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(arguments),
        filter: None,
        null_treatment: None,
        over: None,
        within_group,
    } = function
    else {
        return Err(unsupported(source_map, function_span.as_ref()));
    };
    if !within_group.is_empty()
        || arguments.duplicate_treatment.is_some()
        || !arguments.clauses.is_empty()
    {
        return Err(unsupported(source_map, function_span.as_ref()));
    }
    let mut name_parts = name.0.into_iter();
    let name = match (name_parts.next(), name_parts.next()) {
        (Some(ObjectNamePart::Identifier(name)), None) => name,
        _ => return Err(unsupported(source_map, function_span.as_ref())),
    };
    if name.quote_style.is_some() {
        return Err(unsupported(source_map, function_span.as_ref()));
    }
    let name_span = source_map.span(name.span)?;
    let opening = token_after(
        name_span.end(),
        SourceTokenKind::LeftParenthesis,
        source_map,
        budget,
    )?;
    let closing = matching_right_parenthesis(opening, source_map, budget)?;
    let function = match name.value.to_ascii_uppercase().as_str() {
        "COUNT" => AggregateFunction::Count,
        "SUM" => AggregateFunction::Sum,
        "AVG" => AggregateFunction::Avg,
        "MIN" => AggregateFunction::Min,
        "MAX" => AggregateFunction::Max,
        _ => return Err(unsupported(source_map, function_span.as_ref())),
    };
    let mut args = arguments.args.into_iter();
    let argument = match (args.next(), args.next()) {
        (Some(FunctionArg::Unnamed(argument)), None) => argument,
        _ => return Err(unsupported(source_map, function_span.as_ref())),
    };
    let argument =
        match argument {
            FunctionArgExpr::Wildcard => AggregateArgument::Wildcard(
                token_after(
                    opening.span.end(),
                    SourceTokenKind::Star,
                    source_map,
                    budget,
                )?
                .span,
            ),
            FunctionArgExpr::Expr(expr) => AggregateArgument::Expr(Box::new(
                convert_move_only_expr(MoveOnlyParserExpr::new(expr), source_map, budget)?,
            )),
            _ => return Err(unsupported(source_map, function_span.as_ref())),
        };
    Ok(AstExpr {
        span: SqlSpan::new(name_span.start(), closing.span.end())?,
        kind: AstExprKind::Aggregate { function, argument },
    })
}

fn syntax_error(message: &str, span: SqlSpan, source_map: &SourceMap<'_>) -> DbError {
    let error = DbError::new(ErrorCode::Syntax, message);
    match source_map.location(span.start()) {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

fn unsupported(source_map: &SourceMap<'_>, span: Option<&SqlSpan>) -> DbError {
    let offset = span.map_or(0, SqlSpan::start);
    let error = DbError::new(ErrorCode::Unsupported, "SQL expression is not supported");
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
    use sqlparser::ast::{SelectItem, SetExpr, Statement};
    use sqlparser::dialect::OracleDialect;
    use sqlparser::parser::Parser;
    use sqlparser::tokenizer::{Location, Span};

    use super::*;
    use crate::sql::SqlLimits;
    use crate::sql::parser::tokens::tokenize;

    fn parse_expr_fragment(fragment: &str, limits: &SqlLimits) -> Result<AstExpr> {
        let sql = format!("SELECT {fragment} FROM T");
        let source_map = SourceMap::new(&sql)?;
        let tokens = tokenize(&sql, limits)?;
        let statements = Parser::new(&OracleDialect)
            .with_recursion_limit(limits.max_nesting() as usize)
            .with_tokens_with_locations(tokens)
            .parse_statements()
            .map_err(|error| DbError::new(ErrorCode::Syntax, error.to_string()))?;
        let mut statements = statements.into_iter();
        let query = match (statements.next(), statements.next()) {
            (Some(Statement::Query(query)), None) => query,
            _ => {
                return Err(DbError::new(
                    ErrorCode::Syntax,
                    "expression fixture did not parse as one query",
                ));
            }
        };
        let SetExpr::Select(select) = *query.body else {
            return Err(DbError::new(
                ErrorCode::Syntax,
                "expression fixture did not parse as SELECT",
            ));
        };
        let mut projection = select.projection.into_iter();
        let expr = match (projection.next(), projection.next()) {
            (Some(SelectItem::UnnamedExpr(expr)), None) => expr,
            _ => {
                return Err(DbError::new(
                    ErrorCode::Syntax,
                    "expression fixture did not produce one expression",
                ));
            }
        };
        let mut budget = AstBudget::new(limits);
        convert_expr(expr, &source_map, &mut budget)
    }

    #[test]
    fn supported_expressions_convert_to_owned_variants() {
        for expr in [
            "A + :1",
            "A BETWEEN 1 AND 3",
            "A IN (1, 2)",
            "A IS NOT NULL",
            "NAME LIKE 'A%'",
            "COUNT(*)",
            "X'00FF'",
        ] {
            assert!(
                parse_expr_fragment(expr, &SqlLimits::default()).is_ok(),
                "{expr}"
            );
        }
    }

    #[test]
    fn unsupported_expression_families_are_classified() {
        for expr in ["CASE WHEN A=1 THEN 2 END", "CAST(A AS INT)"] {
            assert_eq!(
                parse_expr_fragment(expr, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Unsupported
            );
        }
    }

    #[test]
    fn unsupported_binary_operator_is_rejected_before_converting_children() {
        let source_map = SourceMap::new("A || B").unwrap();
        let ident = |value, column| {
            Expr::Identifier(Ident {
                value: String::from(value),
                quote_style: None,
                span: Span::new(Location::new(1, column), Location::new(1, column + 1)),
            })
        };
        let expr = Expr::BinaryOp {
            left: Box::new(ident("A", 1)),
            op: ParserBinaryOp::StringConcat,
            right: Box::new(ident("B", 6)),
        };
        let limits = SqlLimits::default().with_max_ast_nodes(3).unwrap();
        let mut budget = AstBudget::new(&limits);

        assert_eq!(
            convert_expr(expr, &source_map, &mut budget)
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );
        assert_eq!(budget.remaining_nodes, 2);
        assert_eq!(budget.remaining_depth, limits.max_nesting());
    }

    #[test]
    fn literals_columns_parameters_and_operators_map_to_exact_owned_variants() {
        let integer = parse_expr_fragment("42", &SqlLimits::default()).unwrap();
        assert!(matches!(integer.kind, AstExprKind::Integer(ref value) if value == "42"));
        assert_eq!(integer.span, SqlSpan::new(7, 9).unwrap());

        let decimal = parse_expr_fragment("12.50", &SqlLimits::default()).unwrap();
        assert!(matches!(decimal.kind, AstExprKind::Decimal(ref value) if value == "12.50"));

        let float = parse_expr_fragment("1e2", &SqlLimits::default()).unwrap();
        assert!(matches!(float.kind, AstExprKind::Float(ref value) if value == "1e2"));

        let text = parse_expr_fragment("'A''B'", &SqlLimits::default()).unwrap();
        assert!(matches!(text.kind, AstExprKind::Text(ref value) if value == "A'B"));

        let blob = parse_expr_fragment("X'00aF'", &SqlLimits::default()).unwrap();
        assert!(matches!(blob.kind, AstExprKind::Blob(ref value) if value == &[0x00, 0xaf]));

        let column = parse_expr_fragment("T.\"Mixed\"", &SqlLimits::default()).unwrap();
        let AstExprKind::Column(parts) = &column.kind else {
            panic!("expected a column expression");
        };
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].as_str(), "T");
        assert_eq!(parts[1].as_str(), "Mixed");
        assert!(parts[1].is_quoted());

        let parameter = parse_expr_fragment(":name", &SqlLimits::default()).unwrap();
        assert!(matches!(
            parameter.kind,
            AstExprKind::Parameter(ParameterKey::Name(ref value)) if value == "NAME"
        ));

        for (fragment, expected) in [
            ("A + 1", BinaryOp::Add),
            ("A - 1", BinaryOp::Subtract),
            ("A * 1", BinaryOp::Multiply),
            ("A / 1", BinaryOp::Divide),
            ("A % 1", BinaryOp::Modulo),
            ("A = 1", BinaryOp::Equal),
            ("A <> 1", BinaryOp::NotEqual),
            ("A < 1", BinaryOp::LessThan),
            ("A <= 1", BinaryOp::LessThanOrEqual),
            ("A > 1", BinaryOp::GreaterThan),
            ("A >= 1", BinaryOp::GreaterThanOrEqual),
            ("A AND B", BinaryOp::And),
            ("A OR B", BinaryOp::Or),
        ] {
            let expression = parse_expr_fragment(fragment, &SqlLimits::default()).unwrap();
            assert!(
                matches!(expression.kind, AstExprKind::Binary { op, .. } if op == expected),
                "{fragment}"
            );
        }
    }

    #[test]
    fn malformed_upstream_numeric_and_blob_text_is_rejected_as_syntax() {
        fn malformed_value(value: Value, source: &str) -> ErrorCode {
            let source_map = SourceMap::new(source).unwrap();
            let expr = Expr::Value(value.with_span(Span::new(
                Location::new(1, 1),
                Location::new(1, u64::try_from(source.chars().count() + 1).unwrap()),
            )));
            let mut budget = AstBudget::new(&SqlLimits::default());
            convert_expr(expr, &source_map, &mut budget)
                .unwrap_err()
                .code()
        }

        for value in ["", ".", "1e", "1e+", "1_0", "1f"] {
            assert_eq!(
                malformed_value(Value::Number(String::from(value), false), value),
                ErrorCode::Syntax,
                "{value}"
            );
        }
        assert_eq!(
            malformed_value(Value::Number(String::from("1"), true), "1"),
            ErrorCode::Syntax
        );
        for value in ["0", "0xz1"] {
            assert_eq!(
                malformed_value(Value::HexStringLiteral(String::from(value)), value),
                ErrorCode::Syntax,
                "{value}"
            );
        }
    }

    #[test]
    fn ast_node_and_depth_limits_accept_exact_boundary_and_reject_one_less() {
        let source_map = SourceMap::new("A + B").unwrap();
        let ident = |value, column| {
            Expr::Identifier(Ident {
                value: String::from(value),
                quote_style: None,
                span: Span::new(Location::new(1, column), Location::new(1, column + 1)),
            })
        };
        let expression = Expr::BinaryOp {
            left: Box::new(ident("A", 1)),
            op: ParserBinaryOp::Plus,
            right: Box::new(ident("B", 5)),
        };

        for max_nodes in [2, 3, 4] {
            let limits = SqlLimits::default().with_max_ast_nodes(max_nodes).unwrap();
            let mut budget = AstBudget::new(&limits);
            let result = convert_expr(expression.clone(), &source_map, &mut budget);
            assert_eq!(result.is_ok(), max_nodes >= 3, "node limit {max_nodes}");
            assert_eq!(budget.remaining_depth, limits.max_nesting());
        }

        for max_depth in [1, 2, 3] {
            let limits = SqlLimits::default().with_max_nesting(max_depth).unwrap();
            let mut budget = AstBudget::new(&limits);
            let result = convert_expr(expression.clone(), &source_map, &mut budget);
            assert_eq!(result.is_ok(), max_depth >= 2, "depth limit {max_depth}");
            assert_eq!(budget.remaining_depth, limits.max_nesting());
        }
    }

    #[test]
    fn unknown_upstream_variant_uses_project_unsupported_and_fallback_location() {
        let source_map = SourceMap::new("X").unwrap();
        let mut budget = AstBudget::new(&SqlLimits::default());
        let error = convert_expr(Expr::Tuple(Vec::new()), &source_map, &mut budget).unwrap_err();

        assert_eq!(error.code(), ErrorCode::Unsupported);
        assert_eq!(
            error.sql_location(),
            Some(crate::error::SqlLocation::new(0, 1, 1).unwrap())
        );
    }

    #[test]
    fn complete_approved_expression_surface_is_accepted() {
        for fragment in [
            "NULL",
            "TRUE",
            "FALSE",
            ".5",
            "1.",
            "+A",
            "-A",
            "NOT A",
            "A != B",
            "A IS NULL",
            "A NOT IN (1)",
            "A NOT BETWEEN 1 AND 2",
            "A NOT LIKE 'X%'",
            "(A + B) * 2",
            "COUNT(A)",
            "SUM(A)",
            "AVG(A)",
            "MIN(A)",
            "MAX(A)",
        ] {
            assert!(
                parse_expr_fragment(fragment, &SqlLimits::default()).is_ok(),
                "{fragment}"
            );
        }
    }

    #[test]
    fn every_non_allow_list_family_reaches_a_stable_error_category() {
        for fragment in [
            "A || B",
            "LOWER(A)",
            "COUNT(DISTINCT A)",
            "COUNT(A, B)",
            "A IN (SELECT B FROM T2)",
            "A LIKE 'X' ESCAPE '!'",
        ] {
            assert_eq!(
                parse_expr_fragment(fragment, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Unsupported,
                "{fragment}"
            );
        }
    }

    #[test]
    fn truncated_expression_families_return_syntax_without_panicking() {
        for fragment in [
            "A +",
            "A BETWEEN 1 AND",
            "A IN (1,",
            "A IS NOT",
            "NAME LIKE",
            "COUNT(",
            "X'0",
        ] {
            let error = parse_expr_fragment(fragment, &SqlLimits::default()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Syntax, "{fragment}: {error:?}");
        }
    }

    #[test]
    fn source_aware_expression_spans_include_authored_operators_and_delimiters() {
        let count = parse_expr_fragment("COUNT(*)", &SqlLimits::default()).unwrap();
        assert_eq!(count.span, SqlSpan::new(7, 15).unwrap());
        assert!(matches!(
            count.kind,
            AstExprKind::Aggregate {
                function: AggregateFunction::Count,
                argument: AggregateArgument::Wildcard(span),
            } if span == SqlSpan::new(13, 14).unwrap()
        ));

        let not = parse_expr_fragment("NOT A", &SqlLimits::default()).unwrap();
        assert_eq!(not.span, SqlSpan::new(7, 12).unwrap());
        assert!(matches!(
            &not.kind,
            AstExprKind::Unary {
                op: UnaryOp::Not,
                expr,
            } if expr.span == SqlSpan::new(11, 12).unwrap()
        ));

        let negative = parse_expr_fragment("-A", &SqlLimits::default()).unwrap();
        assert_eq!(negative.span, SqlSpan::new(7, 9).unwrap());
        assert!(matches!(
            &negative.kind,
            AstExprKind::Unary {
                op: UnaryOp::Negative,
                expr,
            } if expr.span == SqlSpan::new(8, 9).unwrap()
        ));

        let is_not_null = parse_expr_fragment("A IS NOT NULL", &SqlLimits::default()).unwrap();
        assert_eq!(is_not_null.span, SqlSpan::new(7, 20).unwrap());
        assert!(matches!(
            &is_not_null.kind,
            AstExprKind::IsNull {
                negated: true,
                expr,
            } if expr.span == SqlSpan::new(7, 8).unwrap()
        ));

        let nested = parse_expr_fragment("(A + 1)", &SqlLimits::default()).unwrap();
        assert_eq!(nested.span, SqlSpan::new(7, 14).unwrap());
        assert!(matches!(
            &nested.kind,
            AstExprKind::Binary { left, right, .. }
                if left.span == SqlSpan::new(8, 9).unwrap()
                    && right.span == SqlSpan::new(12, 13).unwrap()
        ));

        let trivia =
            parse_expr_fragment("COUNT /*a*/ ( /*b*/ * /*c*/ )", &SqlLimits::default()).unwrap();
        assert_eq!(trivia.span, SqlSpan::new(7, 36).unwrap());
        assert!(matches!(
            trivia.kind,
            AstExprKind::Aggregate {
                argument: AggregateArgument::Wildcard(span),
                ..
            } if span == SqlSpan::new(27, 28).unwrap()
        ));
    }

    #[test]
    fn aggregate_argument_budget_is_entered_before_owned_child_allocation() {
        let limits = SqlLimits::default().with_max_ast_nodes(1).unwrap();
        let error = parse_expr_fragment("SUM(A + B)", &limits).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Syntax);
        assert_eq!(
            error.sql_location(),
            Some(crate::error::SqlLocation::new(11, 1, 12).unwrap())
        );

        let exact = SqlLimits::default().with_max_ast_nodes(4).unwrap();
        let aggregate = parse_expr_fragment("SUM(A + B)", &exact).unwrap();
        assert_eq!(aggregate.span, SqlSpan::new(7, 17).unwrap());
        assert!(matches!(
            &aggregate.kind,
            AstExprKind::Aggregate {
                function: AggregateFunction::Sum,
                argument: AggregateArgument::Expr(expr),
            } if matches!(expr.kind, AstExprKind::Binary { .. })
        ));
    }

    #[test]
    fn deep_balanced_aggregate_spans_preserve_comments_and_parentheses() {
        let mut fragment = String::from("A");
        for _ in 0..16 {
            fragment = format!("SUM(/*l*/{fragment}/*r*/)");
        }
        let limits = SqlLimits::default().with_max_nesting(256).unwrap();

        let expression = parse_expr_fragment(&fragment, &limits).unwrap();

        assert_eq!(expression.span, SqlSpan::new(7, 248).unwrap());
    }

    #[test]
    fn source_token_index_uses_logarithmic_boundary_lookups_at_limit_scale() {
        let mut index = SourceTokenIndex {
            tokens: Vec::with_capacity(100_000),
            kind_indices: std::array::from_fn(|_| Vec::new()),
        };
        for position in 0..100_000 {
            let kind = match position {
                1 => SourceTokenKind::LeftParenthesis,
                99_998 => SourceTokenKind::RightParenthesis,
                _ if position % 2 == 0 => SourceTokenKind::Plus,
                _ => SourceTokenKind::Other,
            };
            let token = SourceToken {
                kind,
                span: SqlSpan::new(position * 2, position * 2 + 1).unwrap(),
                matching_right_parenthesis: (position == 1).then_some(99_998),
            };
            index.kind_indices[kind.index()].push(position);
            index.tokens.push(token);
        }
        let source_map = SourceMap::new("X").unwrap();

        let (before, before_probes) = measure_source_token_probes(|| {
            index
                .token_before(200_000, SourceTokenKind::Other, &source_map)
                .unwrap()
        });
        let (after, after_probes) = measure_source_token_probes(|| {
            index
                .token_after(199_998, SourceTokenKind::Other, &source_map)
                .unwrap()
        });
        let (between, between_probes) = measure_source_token_probes(|| {
            index
                .token_between(199_992, 199_993, SourceTokenKind::Plus, &source_map)
                .unwrap()
        });
        let (closing, matching_probes) = measure_source_token_probes(|| {
            index
                .matching_right_parenthesis(index.tokens[1], &source_map)
                .unwrap()
        });

        assert_eq!(before.span, SqlSpan::new(199_998, 199_999).unwrap());
        assert_eq!(after.span, SqlSpan::new(199_998, 199_999).unwrap());
        assert_eq!(between.span, SqlSpan::new(199_992, 199_993).unwrap());
        assert_eq!(closing.span, SqlSpan::new(199_996, 199_997).unwrap());
        for (operation, probes) in [
            ("before", before_probes),
            ("after", after_probes),
            ("between", between_probes),
        ] {
            assert!(
                (15..=17).contains(&probes),
                "{operation} inspected {probes} candidates"
            );
        }
        assert_eq!(matching_probes, 1);
    }

    #[test]
    fn recursive_conversion_boundary_is_move_only() {
        trait AmbiguousIfClone<Marker> {
            fn marker() {}
        }
        impl<T: ?Sized> AmbiguousIfClone<()> for T {}
        struct ImplementsClone;
        impl<T: Clone> AmbiguousIfClone<ImplementsClone> for T {}

        let _ = <MoveOnlyParserExpr as AmbiguousIfClone<_>>::marker;
        let aggregate = parse_expr_fragment("SUM(A)", &SqlLimits::default()).unwrap();
        assert!(matches!(
            aggregate.kind,
            AstExprKind::Aggregate {
                argument: AggregateArgument::Expr(_),
                ..
            }
        ));
    }
}
