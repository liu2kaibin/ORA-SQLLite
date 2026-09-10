use sqlparser::ast::{
    BinaryOperator as ParserBinaryOp, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, ObjectNamePart, Spanned, UnaryOperator as ParserUnaryOp, Value,
    ValueWithSpan,
};

use super::AstBudget;
use super::source::SourceMap;
use crate::sql::ast::{
    AggregateArgument, AggregateFunction, AstExpr, AstExprKind, BinaryOp, UnaryOp,
};
use crate::sql::{Identifier, ParameterKey, SqlSpan};
use crate::{DbError, ErrorCode, Result};

pub(super) fn convert_expr(
    expr: Expr,
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
    expr: Expr,
    upstream_span: sqlparser::tokenizer::Span,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<AstExpr> {
    let span = source_map.span(upstream_span);
    let kind = match expr {
        Expr::Identifier(ident) => {
            AstExprKind::Column(vec![convert_identifier(ident, source_map)?])
        }
        Expr::CompoundIdentifier(idents) if idents.len() == 2 => {
            let mut parts = Vec::new();
            for ident in idents {
                parts.push(convert_identifier(ident, source_map)?);
            }
            AstExprKind::Column(parts)
        }
        Expr::Value(value) => convert_value(value, source_map)?,
        Expr::UnaryOp { op, expr } => AstExprKind::Unary {
            op: convert_unary_op(op).ok_or_else(|| unsupported(source_map, span.as_ref().ok()))?,
            expr: Box::new(convert_expr(*expr, source_map, budget)?),
        },
        Expr::BinaryOp { left, op, right } => {
            let op = convert_binary_op(&op)
                .ok_or_else(|| unsupported(source_map, span.as_ref().ok()))?;
            AstExprKind::Binary {
                left: Box::new(convert_expr(*left, source_map, budget)?),
                op,
                right: Box::new(convert_expr(*right, source_map, budget)?),
            }
        }
        Expr::IsNull(expr) => AstExprKind::IsNull {
            expr: Box::new(convert_expr(*expr, source_map, budget)?),
            negated: false,
        },
        Expr::IsNotNull(expr) => AstExprKind::IsNull {
            expr: Box::new(convert_expr(*expr, source_map, budget)?),
            negated: true,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } if !list.is_empty() => {
            let expr = Box::new(convert_expr(*expr, source_map, budget)?);
            let mut values = Vec::new();
            for value in list {
                values.push(convert_expr(value, source_map, budget)?);
            }
            AstExprKind::InList {
                expr,
                values,
                negated,
            }
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => AstExprKind::Between {
            expr: Box::new(convert_expr(*expr, source_map, budget)?),
            low: Box::new(convert_expr(*low, source_map, budget)?),
            high: Box::new(convert_expr(*high, source_map, budget)?),
            negated,
        },
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char: None,
        } => AstExprKind::Like {
            expr: Box::new(convert_expr(*expr, source_map, budget)?),
            pattern: Box::new(convert_expr(*pattern, source_map, budget)?),
            negated,
        },
        Expr::Nested(expr) => return convert_expr(*expr, source_map, budget),
        Expr::Function(function) => convert_aggregate(function, source_map, budget)?,
        _ => return Err(unsupported(source_map, span.as_ref().ok())),
    };

    Ok(AstExpr { kind, span: span? })
}

fn convert_identifier(ident: Ident, source_map: &SourceMap<'_>) -> Result<Identifier> {
    let span = source_map.span(ident.span)?;
    match ident.quote_style {
        None => Identifier::unquoted(ident.value, span),
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

fn convert_unary_op(op: ParserUnaryOp) -> Option<UnaryOp> {
    match op {
        ParserUnaryOp::Plus => Some(UnaryOp::Positive),
        ParserUnaryOp::Minus => Some(UnaryOp::Negative),
        ParserUnaryOp::Not => Some(UnaryOp::Not),
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
) -> Result<AstExprKind> {
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
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return Err(unsupported(source_map, function_span.as_ref()));
    };
    if name.quote_style.is_some() {
        return Err(unsupported(source_map, function_span.as_ref()));
    }
    let function = match name.value.to_ascii_uppercase().as_str() {
        "COUNT" => AggregateFunction::Count,
        "SUM" => AggregateFunction::Sum,
        "AVG" => AggregateFunction::Avg,
        "MIN" => AggregateFunction::Min,
        "MAX" => AggregateFunction::Max,
        _ => return Err(unsupported(source_map, function_span.as_ref())),
    };
    let [FunctionArg::Unnamed(argument)] = arguments.args.as_slice() else {
        return Err(unsupported(source_map, function_span.as_ref()));
    };
    let argument = match argument {
        FunctionArgExpr::Wildcard => {
            AggregateArgument::Wildcard(function_span.ok_or_else(|| unsupported(source_map, None))?)
        }
        FunctionArgExpr::Expr(expr) => {
            AggregateArgument::Expr(Box::new(convert_expr(expr.clone(), source_map, budget)?))
        }
        _ => return Err(unsupported(source_map, function_span.as_ref())),
    };
    Ok(AstExprKind::Aggregate { function, argument })
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
        let [Statement::Query(query)] = statements.as_slice() else {
            return Err(DbError::new(
                ErrorCode::Syntax,
                "expression fixture did not parse as one query",
            ));
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Err(DbError::new(
                ErrorCode::Syntax,
                "expression fixture did not parse as SELECT",
            ));
        };
        let [SelectItem::UnnamedExpr(expr)] = select.projection.as_slice() else {
            return Err(DbError::new(
                ErrorCode::Syntax,
                "expression fixture did not produce one expression",
            ));
        };
        let mut budget = AstBudget::new(limits);
        convert_expr(expr.clone(), &source_map, &mut budget)
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
        let AstExprKind::Column(parts) = column.kind else {
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
}
