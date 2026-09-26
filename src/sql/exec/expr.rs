use super::{EvalRow, ParameterFrame};
use crate::sql::{
    bound::{BoundBinaryOp, BoundUnaryOp},
    plan::{ExecExpr, ExecExprKind},
};
use crate::{
    DbError, Decimal, ErrorCode, Result, Value,
    scalar::{self, ScalarBinaryOp, ScalarUnaryOp},
};

use crate::scalar::decimal_cmp;
pub(super) use crate::scalar::int_decimal;
pub(crate) use crate::scalar::{
    TruthValue, like, sql_equal, sql_total_cmp, validate_sql_float, validate_value,
};

/// Explicit postorder stacks: neither normal evaluation nor errors recurse.
pub(crate) fn eval(expr: &ExecExpr, row: &EvalRow, params: &ParameterFrame) -> Result<Value> {
    let mut work = vec![(expr, false)];
    let mut values = Vec::new();
    while let Some((expr, ready)) = work.pop() {
        if !ready {
            work.push((expr, true));
            match expr.kind() {
                ExecExprKind::Unary { expr, .. } | ExecExprKind::IsNull { expr, .. } => {
                    work.push((expr, false))
                }
                ExecExprKind::Binary { left, right, .. } => {
                    work.push((right, false));
                    work.push((left, false));
                }
                ExecExprKind::InList { expr, values, .. } => {
                    work.extend(values.iter().rev().map(|value| (value, false)));
                    work.push((expr, false));
                }
                ExecExprKind::Between {
                    expr, low, high, ..
                } => {
                    work.push((high, false));
                    work.push((low, false));
                    work.push((expr, false));
                }
                ExecExprKind::Like { expr, pattern, .. } => {
                    work.push((pattern, false));
                    work.push((expr, false));
                }
                ExecExprKind::Literal(_)
                | ExecExprKind::Column { .. }
                | ExecExprKind::Parameter { .. }
                | ExecExprKind::Aggregate { .. } => {}
            }
            continue;
        }
        let value = eval_ready(expr.kind(), row, params, &mut values)
            .and_then(|value| {
                validate_value(&value)?;
                Ok(value)
            })
            .map_err(|error| error.with_sql_location(expr.location()))?;
        values.push(value);
    }
    if values.len() != 1 {
        return Err(invalid_eval());
    }
    pop(&mut values)
}

fn eval_ready(
    kind: &ExecExprKind,
    row: &EvalRow,
    params: &ParameterFrame,
    values: &mut Vec<Value>,
) -> Result<Value> {
    match kind {
        ExecExprKind::Literal(value) => Ok(value.clone()),
        ExecExprKind::Column { source, ordinal } => row
            .source_value(*source, *ordinal)
            .cloned()
            .ok_or_else(invalid_eval),
        ExecExprKind::Parameter { slot } => Ok(params.checked_get(*slot)?.clone()),
        ExecExprKind::Aggregate { slot } => row
            .aggregate_value(*slot)?
            .cloned()
            .ok_or_else(invalid_eval),
        ExecExprKind::Unary { op, .. } => unary(*op, pop(values)?),
        ExecExprKind::Binary { op, .. } => {
            let right = pop(values)?;
            let left = pop(values)?;
            binary(*op, &left, &right)
        }
        ExecExprKind::IsNull { negated, .. } => {
            Ok(Value::Bool(matches!(pop(values)?, Value::Null) != *negated))
        }
        ExecExprKind::InList {
            values: list,
            negated,
            ..
        } => {
            let start = values
                .len()
                .checked_sub(list.len())
                .ok_or_else(invalid_eval)?;
            let operand = start.checked_sub(1).ok_or_else(invalid_eval)?;
            let mut truth = TruthValue::False;
            for candidate in &values[start..] {
                truth = truth.or(sql_equal(&values[operand], candidate)?);
            }
            values.truncate(operand);
            Ok(if *negated { truth.not() } else { truth }.into_value())
        }
        ExecExprKind::Between { negated, .. } => {
            let high = pop(values)?;
            let low = pop(values)?;
            let operand = pop(values)?;
            let lower = TruthValue::from_value(&binary(
                BoundBinaryOp::GreaterThanOrEqual,
                &operand,
                &low,
            )?)?;
            let upper =
                TruthValue::from_value(&binary(BoundBinaryOp::LessThanOrEqual, &operand, &high)?)?;
            let truth = lower.and(upper);
            Ok(if *negated { truth.not() } else { truth }.into_value())
        }
        ExecExprKind::Like { negated, .. } => {
            let pattern = pop(values)?;
            let operand = pop(values)?;
            match (&operand, &pattern) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pattern)) => {
                    Ok(Value::Bool(like(text, pattern) != *negated))
                }
                _ => Err(DbError::new(ErrorCode::Type, "LIKE requires Text operands")),
            }
        }
    }
}

fn pop(values: &mut Vec<Value>) -> Result<Value> {
    values.pop().ok_or_else(invalid_eval)
}
fn invalid_eval() -> DbError {
    DbError::new(
        ErrorCode::Internal,
        "expression operand or row slot is absent",
    )
}
pub(crate) fn sql_group_equal(left: &Value, right: &Value) -> Result<bool> {
    Ok(sql_total_cmp(left, right)? == std::cmp::Ordering::Equal)
}

fn unary(op: BoundUnaryOp, value: Value) -> Result<Value> {
    scalar::eval_unary(
        match op {
            BoundUnaryOp::Positive => ScalarUnaryOp::Plus,
            BoundUnaryOp::Negative => ScalarUnaryOp::Minus,
            BoundUnaryOp::Not => ScalarUnaryOp::Not,
        },
        value,
    )
}

fn binary(op: BoundBinaryOp, left: &Value, right: &Value) -> Result<Value> {
    scalar::eval_binary(
        match op {
            BoundBinaryOp::Add => ScalarBinaryOp::Add,
            BoundBinaryOp::Subtract => ScalarBinaryOp::Subtract,
            BoundBinaryOp::Multiply => ScalarBinaryOp::Multiply,
            BoundBinaryOp::Divide => ScalarBinaryOp::Divide,
            BoundBinaryOp::Modulo => ScalarBinaryOp::Modulo,
            BoundBinaryOp::Equal => ScalarBinaryOp::Equal,
            BoundBinaryOp::NotEqual => ScalarBinaryOp::NotEqual,
            BoundBinaryOp::LessThan => ScalarBinaryOp::LessThan,
            BoundBinaryOp::LessThanOrEqual => ScalarBinaryOp::LessThanOrEqual,
            BoundBinaryOp::GreaterThan => ScalarBinaryOp::GreaterThan,
            BoundBinaryOp::GreaterThanOrEqual => ScalarBinaryOp::GreaterThanOrEqual,
            BoundBinaryOp::And => ScalarBinaryOp::And,
            BoundBinaryOp::Or => ScalarBinaryOp::Or,
        },
        left,
        right,
    )
}

pub(crate) fn decimal_arithmetic(op: BoundBinaryOp, a: Decimal, b: Decimal) -> Result<Decimal> {
    scalar::decimal_arithmetic(
        match op {
            BoundBinaryOp::Add => ScalarBinaryOp::Add,
            BoundBinaryOp::Subtract => ScalarBinaryOp::Subtract,
            BoundBinaryOp::Multiply => ScalarBinaryOp::Multiply,
            BoundBinaryOp::Divide => ScalarBinaryOp::Divide,
            BoundBinaryOp::Modulo => ScalarBinaryOp::Modulo,
            _ => {
                return Err(DbError::new(
                    ErrorCode::Type,
                    "not a Decimal arithmetic operator",
                ));
            }
        },
        a,
        b,
    )
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;
    use crate::sql::{
        SqlSpan, SqlType,
        bound::{BoundExpr, BoundExprKind, BoundStatementKind},
        exec::tests::{bound_sql, empty_frame, eval_sql_expr, plan_sql},
        plan::{Plan, PlanKind},
    };
    use crate::{Decimal, ErrorCode, OracleDate, OracleTimestamp};

    #[test]
    fn all_boolean_truth_table_cells_and_not_are_sql_three_valued() {
        let names = ["TRUE", "FALSE", "NULL"];
        let and = [
            [Some(true), Some(false), None],
            [Some(false), Some(false), Some(false)],
            [None, Some(false), None],
        ];
        let or = [
            [Some(true), Some(true), Some(true)],
            [Some(true), Some(false), None],
            [Some(true), None, None],
        ];
        for (i, a) in names.iter().enumerate() {
            for (j, b) in names.iter().enumerate() {
                for (op, expected) in [("AND", and[i][j]), ("OR", or[i][j])] {
                    assert_eq!(
                        eval_sql_expr(&format!("{a} {op} {b}")).unwrap(),
                        expected.map_or(Value::Null, Value::Bool)
                    );
                }
            }
        }
        for (sql, value) in [
            ("NOT TRUE", Value::Bool(false)),
            ("NOT FALSE", Value::Bool(true)),
            ("NOT NULL", Value::Null),
        ] {
            assert_eq!(eval_sql_expr(sql).unwrap(), value);
        }
    }

    #[test]
    fn every_predicate_family_propagates_null_and_negation() {
        let cases = [
            ("NULL = NULL", None),
            ("NULL <> 1", None),
            ("1 = 1", Some(true)),
            ("1 <> 2", Some(true)),
            ("1 < 2", Some(true)),
            ("2 <= 2", Some(true)),
            ("3 > 2", Some(true)),
            ("3 >= 3", Some(true)),
            ("NULL IS NULL", Some(true)),
            ("NULL IS NOT NULL", Some(false)),
            ("1 IS NOT NULL", Some(true)),
            ("1 IN (NULL, 1)", Some(true)),
            ("1 IN (NULL, 2)", None),
            ("1 NOT IN (2, 3)", Some(true)),
            ("NULL NOT IN (1, 2)", None),
            ("2 BETWEEN 1 AND 3", Some(true)),
            ("2 NOT BETWEEN 1 AND 3", Some(false)),
            ("2 BETWEEN NULL AND 1", Some(false)),
            ("2 BETWEEN NULL AND 3", None),
            ("NULL BETWEEN 1 AND 3", None),
            ("NULL LIKE '%'", None),
            ("'a' NOT LIKE 'b'", Some(true)),
            ("X'0102' = X'0102'", Some(true)),
            ("X'0102' <> X'01'", Some(true)),
        ];
        for (sql, want) in cases {
            assert_eq!(
                eval_sql_expr(sql).unwrap(),
                want.map_or(Value::Null, Value::Bool),
                "{sql}"
            );
        }
    }

    #[test]
    fn like_matches_unicode_scalars_and_has_no_implicit_escape_or_case_folding() {
        for (text, pattern, expected) in [
            ("", "", true),
            ("", "%", true),
            ("", "_", false),
            ("界", "_", true),
            ("😀", "_", true),
            ("e\u{301}", "_", false),
            ("e\u{301}", "__", true),
            ("Ab", "a%", false),
            ("abc", "a%c", true),
            ("abc", "a%bc%", true),
            ("abbbc", "%b_c", true),
            ("abc", "a_d", false),
            ("a\\bc", "a\\%", true),
            ("abc", "a\\%", false),
            ("abc", "%%%", true),
        ] {
            assert_eq!(like(text, pattern), expected, "{text:?} {pattern:?}");
        }
        assert!(like(&"界".repeat(20_000), "%_"));
        assert!(!like(&"a".repeat(500), &format!("{}b", "%a".repeat(100))));
    }

    #[test]
    fn checked_int64_arithmetic_and_null_propagation() {
        for (sql, want) in [
            ("+7", 7),
            ("-7", -7),
            ("7 + 2", 9),
            ("7 - 2", 5),
            ("7 * 2", 14),
            ("-7 / 2", -3),
            ("-7 % 2", -1),
        ] {
            assert_eq!(eval_sql_expr(sql).unwrap(), Value::Int64(want));
        }
        for sql in [
            "NULL + 1", "1 - NULL", "NULL * 1", "NULL / 0", "NULL % 0", "-NULL",
        ] {
            assert_eq!(eval_sql_expr(sql).unwrap(), Value::Null, "{sql}");
        }
        for sql in [
            "9223372036854775807 + 1",
            "(-9223372036854775807 - 1) - 1",
            "9223372036854775807 * 2",
            "-(-9223372036854775807 - 1)",
            "(-9223372036854775807 - 1) / -1",
            "(-9223372036854775807 - 1) % -1",
            "1 / 0",
            "1 % 0",
        ] {
            let error = eval_sql_expr(sql).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Type, "{sql}");
            assert!(error.sql_location().is_some());
        }
    }

    #[test]
    fn decimal_arithmetic_is_exact_or_errors_without_rounding() {
        for (sql, magnitude, scale, negative) in [
            ("1.20 + 2", 32, 1, false),
            ("1.2 - 2.5", 13, 1, true),
            ("1.25 * 0.8", 1, 0, false),
            ("1.0 / 8", 125, 3, false),
            ("-7.5 % 2", 15, 1, true),
            ("1.2 + -1.2", 0, 0, false),
            ("1.00 = 1", 0, 0, false),
        ] {
            if sql.contains('=') {
                assert_eq!(eval_sql_expr(sql).unwrap(), Value::Bool(true));
                continue;
            }
            let value = Value::Decimal(Decimal::new(negative, scale, magnitude).unwrap());
            assert!(
                sql_group_equal(&eval_sql_expr(sql).unwrap(), &value).unwrap(),
                "{sql}"
            );
        }
        for sql in [
            "1.0 / 3",
            "1.0 / 0",
            "1.0 % 0",
            "99999999999999999999999999999999999999 + 2.0",
            "99999999999999999999999999999999999999 * 9.0",
        ] {
            assert_eq!(
                eval_sql_expr(sql).unwrap_err().code(),
                ErrorCode::Type,
                "{sql}"
            );
        }
    }

    #[test]
    fn float_arithmetic_accepts_only_finite_results_and_comparison_keys() {
        for (sql, want) in [
            ("1e0 + 2e0", 3.0),
            ("7e0 - 2e0", 5.0),
            ("2e0 * 3e0", 6.0),
            ("3e0 / 2e0", 1.5),
            ("7e0 % 2e0", 1.0),
            ("-2e0", -2.0),
        ] {
            assert_eq!(eval_sql_expr(sql).unwrap(), Value::Float64(want));
        }
        for sql in ["1e308 * 2e0", "1e0 / 0e0", "1e0 % 0e0"] {
            assert_eq!(eval_sql_expr(sql).unwrap_err().code(), ErrorCode::Type);
        }
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for other in [Value::Null, Value::Float64(1.0)] {
                assert_eq!(
                    sql_equal(&Value::Float64(bad), &other).unwrap_err().code(),
                    ErrorCode::Type
                );
                assert_eq!(
                    sql_total_cmp(&other, &Value::Float64(bad))
                        .unwrap_err()
                        .code(),
                    ErrorCode::Type
                );
            }
        }
        assert!(sql_group_equal(&Value::Float64(-0.0), &Value::Float64(0.0)).unwrap());
    }

    #[test]
    fn total_comparison_is_numeric_for_decimal_and_fieldwise_for_temporal_values() {
        let date = OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap();
        let timestamp =
            |micros, offset| Value::Timestamp(OracleTimestamp::new(date, micros, offset).unwrap());
        let pairs = [
            (Value::Null, Value::Bool(false)),
            (Value::Bool(false), Value::Bool(true)),
            (Value::Text("A".into()), Value::Text("a".into())),
            (Value::Text("界".into()), Value::Text("😀".into())),
            (Value::Blob(vec![0, 255]), Value::Blob(vec![1])),
            (
                Value::Date(date),
                Value::Date(OracleDate::new(2026, 1, 2, 0, 0, 0).unwrap()),
            ),
            (timestamp(0, None), timestamp(0, Some(-840))),
            (timestamp(0, Some(-840)), timestamp(0, Some(840))),
            (timestamp(0, Some(840)), timestamp(1, Some(-840))),
            (
                Value::Decimal(Decimal::new(false, 38, 1).unwrap()),
                Value::Decimal(Decimal::new(false, -38, 1).unwrap()),
            ),
        ];
        for (left, right) in pairs {
            assert_eq!(sql_total_cmp(&left, &right).unwrap(), Ordering::Less);
            assert_eq!(sql_total_cmp(&right, &left).unwrap(), Ordering::Greater);
        }
        assert!(sql_group_equal(&Value::Null, &Value::Null).unwrap());
        assert_eq!(
            sql_equal(&Value::Null, &Value::Null).unwrap(),
            TruthValue::Unknown
        );
        assert!(
            sql_group_equal(
                &Value::Int64(i64::MIN),
                &Value::Decimal(Decimal::new(true, 0, 9_223_372_036_854_775_808).unwrap())
            )
            .unwrap()
        );
        for other in [
            Value::Float64(1.0),
            Value::Text("1".into()),
            Value::Bool(true),
        ] {
            assert_eq!(
                sql_total_cmp(&Value::Int64(1), &other).unwrap_err().code(),
                ErrorCode::Type
            );
        }
    }

    #[test]
    fn evaluation_uses_source_ordinals_parameter_slots_and_aggregate_slots() {
        let plan = plan_sql(
            "SELECT o.ID, u.ID, :x, :x FROM USERS u INNER JOIN ORDERS o ON u.ID = o.USER_ID",
        );
        let params = ParameterFrame::bind(
            plan.parameters(),
            crate::Parameters::named(&[("X", Value::Text("v".into()).into())]),
            &mut super::super::WorkMemory::new(4096).unwrap(),
        )
        .unwrap();
        let a = [Value::Int64(10)];
        let b = [Value::Int64(20)];
        let sources = [&a[..], &b[..]];
        let row = EvalRow::from_sources_for_test(&sources);
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        for (expr, want) in query.projection().iter().zip([
            Value::Int64(20),
            Value::Int64(10),
            Value::Text("v".into()),
            Value::Text("v".into()),
        ]) {
            assert_eq!(eval(expr, &row, &params).unwrap(), want);
        }
        assert_eq!(
            eval(&query.projection()[0], &EvalRow::default(), &params)
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
        assert_eq!(
            eval(&query.projection()[2], &row, &empty_frame())
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
        let plan = plan_sql("SELECT SUM(ID), COUNT(*) FROM USERS");
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        let aggregates = [
            Value::Decimal(Decimal::new(false, 0, 40).unwrap()),
            Value::Int64(3),
        ];
        let row = EvalRow::from_aggregates_for_test(&aggregates);
        for (expr, want) in query.projection().iter().zip(aggregates.iter()) {
            assert_eq!(eval(expr, &row, &empty_frame()).unwrap(), *want);
        }
        assert_eq!(
            eval(&query.projection()[0], &EvalRow::default(), &empty_frame())
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn deep_tree_evaluation_and_drop_do_not_recurse() {
        let mut bound = bound_sql("SELECT 1 FROM USERS");
        let mut expression = BoundExpr::new(
            BoundExprKind::Literal(Value::Int64(1)),
            SqlType::Int64,
            false,
            SqlSpan::new(0, 1).unwrap(),
        );
        for _ in 0..20_000 {
            expression = BoundExpr::new(
                BoundExprKind::Unary {
                    op: BoundUnaryOp::Negative,
                    expr: Box::new(expression),
                },
                SqlType::Int64,
                false,
                SqlSpan::new(0, 1).unwrap(),
            );
        }
        let BoundStatementKind::Query { query } = &mut bound.kind else {
            panic!("query")
        };
        query.projection = vec![expression];
        let plan = Plan::from_bound(&bound, "SELECT 1 FROM USERS").unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        assert_eq!(
            eval(&query.projection()[0], &EvalRow::default(), &empty_frame()).unwrap(),
            Value::Int64(1)
        );
    }

    // Checked-operator regressions must report the failing child, not its parent.
    #[test]
    fn nested_runtime_errors_keep_exact_multiline_locations() {
        let plan = plan_sql("SELECT '界',\r\n 2 + (1 / :1) FROM USERS");
        let params = ParameterFrame::bind(
            plan.parameters(),
            crate::Parameters::positional(&[Value::Int64(0).into()]),
            &mut super::super::WorkMemory::new(4096).unwrap(),
        )
        .unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        let error = eval(&query.projection()[1], &EvalRow::default(), &params).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        // Phase 4B includes the opening parenthesis in this child's span.
        assert_eq!(
            error.sql_location(),
            Some(crate::error::SqlLocation::new(20, 2, 6).unwrap())
        );
    }

    #[test]
    fn runtime_unknown_values_do_not_bypass_type_or_blob_predicate_rules() {
        for (sql, a, b) in [
            (
                "SELECT :1 + :2 FROM USERS",
                Value::Text("x".into()),
                Value::Text("y".into()),
            ),
            (
                "SELECT :1 + :2 FROM USERS",
                Value::Float64(1.0),
                Value::Int64(2),
            ),
            (
                "SELECT :1 < :2 FROM USERS",
                Value::Blob(vec![1]),
                Value::Blob(vec![2]),
            ),
            (
                "SELECT :1 BETWEEN :2 AND :2 FROM USERS",
                Value::Blob(vec![1]),
                Value::Blob(vec![2]),
            ),
        ] {
            let plan = plan_sql(sql);
            // Independent unknown frames allow invalid dynamic operands to reach
            // evaluation without weakening the real binder's static checks.
            let metadata = super::super::tests::positional_metadata(&[None, None]);
            let params = ParameterFrame::bind(
                &metadata,
                crate::Parameters::positional(&[a.into(), b.into()]),
                &mut super::super::WorkMemory::new(4096).unwrap(),
            )
            .unwrap();
            let PlanKind::Query(query) = plan.kind() else {
                panic!("query")
            };
            let error = eval(&query.projection()[0], &EvalRow::default(), &params).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Type, "{sql}");
            assert_eq!(error.sql_location().unwrap().column(), 8);
        }
    }

    #[test]
    fn non_finite_column_literal_and_aggregate_values_fail_at_the_leaf() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let plan = plan_sql("SELECT FLOAT_COL + 1e0 FROM USERS");
            let source = [Value::Null, Value::Null, Value::Null, Value::Float64(bad)];
            let sources = [&source[..]];
            let row = EvalRow::from_sources_for_test(&sources);
            let PlanKind::Query(query) = plan.kind() else {
                panic!("query")
            };
            let error = eval(&query.projection()[0], &row, &empty_frame()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Type);
            assert_eq!(error.sql_location().unwrap().column(), 8);
            let mut bound = bound_sql("SELECT 1 FROM USERS");
            let BoundStatementKind::Query { query } = &mut bound.kind else {
                panic!("query")
            };
            query.projection[0].kind = BoundExprKind::Literal(Value::Float64(bad));
            let plan = Plan::from_bound(&bound, "SELECT 1 FROM USERS").unwrap();
            let PlanKind::Query(query) = plan.kind() else {
                panic!("query")
            };
            assert_eq!(
                eval(&query.projection()[0], &EvalRow::default(), &empty_frame())
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
            let plan = plan_sql("SELECT SUM(FLOAT_COL) FROM USERS");
            let PlanKind::Query(query) = plan.kind() else {
                panic!("query")
            };
            let aggregate = [Value::Float64(bad)];
            let error = eval(
                &query.projection()[0],
                &EvalRow::from_aggregates_for_test(&aggregate),
                &empty_frame(),
            )
            .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Type);
            assert_eq!(error.sql_location().unwrap().column(), 8);
        }
    }

    #[test]
    fn decimal_full_scale_boundaries_are_checked_without_comparison_overflow() {
        let decimal = |magnitude, scale| Decimal::new(false, scale, magnitude).unwrap();
        let maximum = decimal(99_999_999_999_999_999_999_999_999_999_999_999_999, -38);
        let minimum = decimal(1, 38);
        assert_eq!(decimal_cmp(minimum, maximum).unwrap(), Ordering::Less);
        assert_eq!(
            decimal_cmp(decimal(1, -38), decimal(10, -37)).unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            decimal_cmp(decimal(0, -38), decimal(0, 38)).unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Divide, minimum, decimal(1, 0)).unwrap(),
            minimum
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Multiply, minimum, decimal(1, 0)).unwrap(),
            minimum
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Divide, decimal(1, -38), minimum)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Divide, minimum, decimal(2, 0))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Multiply, maximum, decimal(2, 0))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Add, maximum, minimum)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Multiply, minimum, minimum)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            decimal_arithmetic(BoundBinaryOp::Divide, decimal(3, 0), decimal(6, 0)).unwrap(),
            decimal(5, 1)
        );
        assert_eq!(
            decimal_arithmetic(
                BoundBinaryOp::Add,
                decimal(99_999_999_999_999_999_999_999_999_999_999_999_999, 0),
                decimal(1, 0)
            )
            .unwrap(),
            decimal(1, -38)
        );
    }

    #[test]
    fn deterministic_decimal_order_and_arithmetic_properties_match_scaled_integer_oracle() {
        let decimal = |value: i64, scale| {
            Decimal::new(value < 0, scale, u128::from(value.unsigned_abs())).unwrap()
        };
        let reference = |value: Decimal| {
            let integer = i128::try_from(value.magnitude()).unwrap()
                * 10_i128.pow(u32::try_from(8 - i16::from(value.scale())).unwrap());
            if value.negative() { -integer } else { integer }
        };
        let mut state = 0x521e_a419_u64;
        for _ in 0..1024 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = decimal(
                i64::try_from(state % 20001).unwrap() - 10000,
                i8::try_from((state >> 20) % 5).unwrap(),
            );
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let b = decimal(
                i64::try_from(state % 20001).unwrap() - 10000,
                i8::try_from((state >> 20) % 5).unwrap(),
            );
            let left = reference(a);
            let right = reference(b);
            assert_eq!(decimal_cmp(a, b).unwrap(), left.cmp(&right));
            assert_eq!(
                decimal_cmp(a, b).unwrap(),
                decimal_cmp(b, a).unwrap().reverse()
            );
            assert_eq!(
                reference(decimal_arithmetic(BoundBinaryOp::Add, a, b).unwrap()),
                left + right
            );
            assert_eq!(
                reference(decimal_arithmetic(BoundBinaryOp::Subtract, a, b).unwrap()),
                left - right
            );
            assert_eq!(
                reference(decimal_arithmetic(BoundBinaryOp::Multiply, a, b).unwrap()),
                left * right / 100_000_000
            );
            if right != 0 {
                assert_eq!(
                    reference(decimal_arithmetic(BoundBinaryOp::Modulo, a, b).unwrap()),
                    left % right
                );
            }
        }
    }

    #[test]
    fn deterministic_like_properties_match_independent_dynamic_program() {
        fn reference(text: &str, pattern: &str) -> bool {
            let text: Vec<_> = text.chars().collect();
            let pattern: Vec<_> = pattern.chars().collect();
            let mut table = vec![vec![false; pattern.len() + 1]; text.len() + 1];
            table[0][0] = true;
            for j in 1..=pattern.len() {
                table[0][j] = pattern[j - 1] == '%' && table[0][j - 1];
            }
            for i in 1..=text.len() {
                for j in 1..=pattern.len() {
                    table[i][j] = match pattern[j - 1] {
                        '%' => table[i][j - 1] || table[i - 1][j],
                        '_' => table[i - 1][j - 1],
                        character => character == text[i - 1] && table[i - 1][j - 1],
                    };
                }
            }
            table[text.len()][pattern.len()]
        }
        let mut state = 17_u64;
        for _ in 0..2000 {
            let mut text = String::new();
            let mut pattern = String::new();
            for i in 0..12 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                if i < 8 {
                    text.push(['a', '界', '😀'][(state >> 8) as usize % 3]);
                }
                pattern.push(['a', '界', '😀', '%', '_'][(state >> 16) as usize % 5]);
            }
            assert_eq!(
                like(&text, &pattern),
                reference(&text, &pattern),
                "{text:?} {pattern:?}"
            );
        }
    }
}
