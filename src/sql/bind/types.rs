use std::collections::HashMap;

use crate::{
    DbError, Decimal, ErrorCode, Result, Value,
    api::{ParameterIdentifier, ParameterMetadata},
    sql::{
        ParameterKey, SqlType,
        ast::{AggregateArgument, AggregateFunction, AstExpr, AstExprKind, BinaryOp, UnaryOp},
        bound::{BoundAggregateFunction, BoundBinaryOp, BoundExpr, BoundExprKind, BoundUnaryOp},
    },
};

use super::BindingScope;

#[derive(Debug, Default)]
pub(crate) struct ParameterCollector {
    entries: Vec<ParameterEntry>,
    positions: HashMap<ParameterIdentifier, usize>,
}

#[derive(Debug)]
struct ParameterEntry {
    identifier: ParameterIdentifier,
    sql_type: SqlType,
}

impl ParameterCollector {
    pub(crate) fn constrain(&mut self, key: &ParameterKey, constraint: SqlType) -> Result<SqlType> {
        let identifier = parameter_identifier(key);
        self.constrain_identifier(identifier, constraint)
    }

    fn constrain_identifier(
        &mut self,
        identifier: ParameterIdentifier,
        constraint: SqlType,
    ) -> Result<SqlType> {
        if let Some(index) = self.positions.get(&identifier).copied() {
            let entry = &mut self.entries[index];
            entry.sql_type = entry.sql_type.unify(constraint)?;
            return Ok(entry.sql_type);
        }
        self.positions
            .insert(identifier.clone(), self.entries.len());
        self.entries.push(ParameterEntry {
            identifier,
            sql_type: constraint,
        });
        Ok(constraint)
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
        }
        self.entries
            .into_iter()
            .map(|entry| ParameterMetadata::inferred(entry.identifier, entry.sql_type))
            .collect()
    }
}

pub(crate) fn bind_expr(
    expr: &AstExpr,
    scope: &BindingScope,
    expected: Option<SqlType>,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    let mut bound = bind_expr_without_expected(expr, scope, context)?;
    if let Some(expected) = expected {
        apply_expected(&mut bound, expected, context)?;
    }
    Ok(bound)
}

fn bind_expr_without_expected(
    expr: &AstExpr,
    scope: &BindingScope,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    let (kind, sql_type, nullable) = match &expr.kind {
        AstExprKind::Null => (BoundExprKind::Literal(Value::Null), SqlType::Null, true),
        AstExprKind::Bool(value) => (
            BoundExprKind::Literal(Value::Bool(*value)),
            SqlType::Bool,
            false,
        ),
        AstExprKind::Integer(value) => {
            let (value, sql_type) = parse_integer(value)?;
            (BoundExprKind::Literal(value), sql_type, false)
        }
        AstExprKind::Decimal(value) => (
            BoundExprKind::Literal(Value::Decimal(parse_decimal(value)?)),
            SqlType::Decimal,
            false,
        ),
        AstExprKind::Float(value) => (
            BoundExprKind::Literal(Value::Float64(parse_float(value)?)),
            SqlType::Float64,
            false,
        ),
        AstExprKind::Text(value) => (
            BoundExprKind::Literal(Value::Text(value.clone())),
            SqlType::Text,
            false,
        ),
        AstExprKind::Blob(value) => (
            BoundExprKind::Literal(Value::Blob(value.clone())),
            SqlType::Blob,
            false,
        ),
        AstExprKind::Column(parts) => {
            let column = scope.resolve_column(parts)?;
            (
                BoundExprKind::Column {
                    table_id: column.table_id,
                    column_id: column.column_id,
                    ordinal: column.ordinal,
                },
                column.sql_type,
                column.nullable,
            )
        }
        AstExprKind::Parameter(key) => {
            let sql_type = context.constrain(key, SqlType::Unknown)?;
            (
                BoundExprKind::Parameter(parameter_identifier(key)),
                sql_type,
                true,
            )
        }
        AstExprKind::Unary { op, expr: operand } => {
            let operand = bind_expr(operand, scope, unary_expected(*op), context)?;
            bind_unary(*op, operand)?
        }
        AstExprKind::Binary { left, op, right } => {
            return bind_binary(expr, left, *op, right, scope, context);
        }
        AstExprKind::IsNull {
            expr: operand,
            negated,
        } => {
            let operand = bind_expr(operand, scope, None, context)?;
            (
                BoundExprKind::IsNull {
                    expr: Box::new(operand),
                    negated: *negated,
                },
                SqlType::Bool,
                false,
            )
        }
        AstExprKind::Aggregate { function, argument } => {
            return bind_aggregate(expr, *function, argument, scope, context);
        }
        AstExprKind::InList {
            expr: operand,
            values,
            negated,
        } => {
            let mut expressions = Vec::with_capacity(values.len().saturating_add(1));
            expressions.push(operand.as_ref());
            expressions.extend(values.iter());
            let mut expressions = bind_common_type(expressions, scope, context)?;
            let operand = expressions.remove(0);
            let nullable = operand.nullable || expressions.iter().any(|value| value.nullable);
            (
                BoundExprKind::InList {
                    expr: Box::new(operand),
                    values: expressions,
                    negated: *negated,
                },
                SqlType::Bool,
                nullable,
            )
        }
        AstExprKind::Between {
            expr: operand,
            low,
            high,
            negated,
        } => {
            let mut expressions = bind_common_type(
                vec![operand.as_ref(), low.as_ref(), high.as_ref()],
                scope,
                context,
            )?;
            if expressions[0].sql_type == SqlType::Blob {
                return Err(type_error("Blob supports only equality comparisons"));
            }
            let operand = expressions.remove(0);
            let low = expressions.remove(0);
            let high = expressions.remove(0);
            let nullable = operand.nullable || low.nullable || high.nullable;
            (
                BoundExprKind::Between {
                    expr: Box::new(operand),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated: *negated,
                },
                SqlType::Bool,
                nullable,
            )
        }
        AstExprKind::Like {
            expr: operand,
            pattern,
            negated,
        } => {
            let operand = bind_expr(operand, scope, Some(SqlType::Text), context)?;
            let pattern = bind_expr(pattern, scope, Some(SqlType::Text), context)?;
            let nullable = operand.nullable || pattern.nullable;
            (
                BoundExprKind::Like {
                    expr: Box::new(operand),
                    pattern: Box::new(pattern),
                    negated: *negated,
                },
                SqlType::Bool,
                nullable,
            )
        }
    };
    Ok(BoundExpr::new(kind, sql_type, nullable, expr.span))
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

fn bind_binary(
    source: &AstExpr,
    left: &AstExpr,
    op: BinaryOp,
    right: &AstExpr,
    scope: &BindingScope,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    if matches!(op, BinaryOp::And | BinaryOp::Or) {
        let left = bind_expr(left, scope, Some(SqlType::Bool), context)?;
        let right = bind_expr(right, scope, Some(SqlType::Bool), context)?;
        let nullable = left.nullable || right.nullable;
        return Ok(BoundExpr::new(
            BoundExprKind::Binary {
                left: Box::new(left),
                op: map_binary_op(op),
                right: Box::new(right),
            },
            SqlType::Bool,
            nullable,
            source.span,
        ));
    }

    let mut left = bind_expr(left, scope, None, context)?;
    let right_expected = concrete_hint(left.sql_type);
    let mut right = bind_expr(right, scope, right_expected, context)?;
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
        source.span,
    ))
}

fn bind_aggregate(
    source: &AstExpr,
    function: AggregateFunction,
    argument: &AggregateArgument,
    scope: &BindingScope,
    context: &mut ParameterCollector,
) -> Result<BoundExpr> {
    let argument = match argument {
        AggregateArgument::Wildcard(_) if function == AggregateFunction::Count => None,
        AggregateArgument::Wildcard(_) => {
            return Err(type_error("only COUNT accepts a wildcard argument"));
        }
        AggregateArgument::Expr(expr) => Some(Box::new(bind_expr(expr, scope, None, context)?)),
    };
    let (sql_type, nullable) = match function {
        AggregateFunction::Count => (SqlType::Int64, false),
        AggregateFunction::Sum | AggregateFunction::Avg => {
            let input = argument.as_ref().expect("SUM and AVG have an expression");
            match input.sql_type {
                SqlType::Int64 | SqlType::Decimal => (SqlType::Decimal, true),
                SqlType::Float64 => (SqlType::Float64, true),
                _ => return Err(type_error("SUM and AVG require a numeric input")),
            }
        }
        AggregateFunction::Min | AggregateFunction::Max => {
            let input = argument.as_ref().expect("MIN and MAX have an expression");
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
            argument,
        },
        sql_type,
        nullable,
        source.span,
    ))
}

fn bind_common_type(
    expressions: Vec<&AstExpr>,
    scope: &BindingScope,
    context: &mut ParameterCollector,
) -> Result<Vec<BoundExpr>> {
    let mut common = SqlType::Unknown;
    let mut bound = Vec::with_capacity(expressions.len());
    for expression in expressions {
        let current = bind_expr(expression, scope, concrete_hint(common), context)?;
        common = common.unify(current.sql_type)?;
        bound.push(current);
    }
    for expression in &mut bound {
        apply_expected(expression, common, context)?;
    }
    Ok(bound)
}

fn apply_expected(
    expression: &mut BoundExpr,
    expected: SqlType,
    context: &mut ParameterCollector,
) -> Result<()> {
    let common = expression.sql_type.unify(expected)?;
    match &mut expression.kind {
        BoundExprKind::Parameter(identifier) => {
            expression.sql_type = context.constrain_identifier(identifier.clone(), common)?;
            return Ok(());
        }
        BoundExprKind::Unary {
            op: BoundUnaryOp::Positive | BoundUnaryOp::Negative,
            expr,
        } => {
            apply_expected(expr, common, context)?;
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
            apply_expected(left, common, context)?;
            apply_expected(right, common, context)?;
        }
        BoundExprKind::Aggregate {
            function: BoundAggregateFunction::Min | BoundAggregateFunction::Max,
            argument: Some(argument),
        } => {
            apply_expected(argument, common, context)?;
        }
        _ => {}
    }
    expression.sql_type = common;
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

const fn concrete_hint(sql_type: SqlType) -> Option<SqlType> {
    match sql_type {
        SqlType::Null | SqlType::Unknown => None,
        value => Some(value),
    }
}

const fn unary_expected(op: UnaryOp) -> Option<SqlType> {
    match op {
        UnaryOp::Not => Some(SqlType::Bool),
        UnaryOp::Positive | UnaryOp::Negative => None,
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
}
