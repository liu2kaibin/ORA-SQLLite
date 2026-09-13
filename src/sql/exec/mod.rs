//! Storage-independent runtime values and scalar expression evaluation.
mod expr;
mod memory;
mod params;

pub(crate) use expr::{
    EvalRow, TruthValue, eval, sql_equal, sql_group_equal, sql_total_cmp, validate_sql_float,
};
pub(crate) use memory::WorkMemory;
pub(crate) use params::ParameterFrame;

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;
    use crate::sql::{
        SqlLimits, SqlSpan, bind_sql,
        bound::{BoundExpr, BoundExprKind, BoundStatement, BoundStatementKind},
        plan::{Plan, PlanKind},
    };
    use crate::storage::test_support::phase_4b_catalog_fixture;
    use crate::{
        DataType, ErrorCode, ParameterIdentifier, ParameterMetadata, ParameterValue, Parameters,
        Result, Value,
    };

    pub(super) fn positional_metadata(types: &[Option<DataType>]) -> Vec<ParameterMetadata> {
        types
            .iter()
            .enumerate()
            .map(|(index, data_type)| ParameterMetadata {
                identifier: ParameterIdentifier::Position(u16::try_from(index + 1).unwrap()),
                data_type: *data_type,
            })
            .collect()
    }

    pub(super) fn bound_sql(sql: &str) -> BoundStatement {
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut read = pager.begin_read().unwrap();
        let catalog = read.catalog_reader().unwrap();
        bind_sql(
            sql,
            &SqlLimits::default(),
            &catalog,
            pager.database_uuid().unwrap(),
        )
        .unwrap()
    }

    pub(super) fn plan_sql(sql: &str) -> Plan {
        Plan::from_bound(&bound_sql(sql), sql).unwrap()
    }

    pub(super) fn empty_frame() -> ParameterFrame {
        ParameterFrame::bind(&[], Parameters::none(), &mut WorkMemory::new(4096).unwrap()).unwrap()
    }

    pub(super) fn eval_sql_expr(source: &str) -> Result<Value> {
        let mut bound = bound_sql(&format!("SELECT {source} FROM USERS"));
        // The fixture is expression-relative: preserve every real bound span,
        // removing only the SELECT prefix before compiling against the expression.
        bound.span = SqlSpan::new(0, source.len()).unwrap();
        let BoundStatementKind::Query { query } = &mut bound.kind else {
            panic!("query")
        };
        let mut work: Vec<&mut BoundExpr> = query.projection.iter_mut().collect();
        while let Some(expr) = work.pop() {
            expr.span = SqlSpan::new(expr.span.start() - 7, expr.span.end() - 7).unwrap();
            match &mut expr.kind {
                BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                    work.push(expr)
                }
                BoundExprKind::Binary { left, right, .. } => {
                    work.push(left);
                    work.push(right);
                }
                BoundExprKind::InList { expr, values, .. } => {
                    work.push(expr);
                    work.extend(values.iter_mut());
                }
                BoundExprKind::Between {
                    expr, low, high, ..
                } => {
                    work.push(expr);
                    work.push(low);
                    work.push(high);
                }
                BoundExprKind::Like { expr, pattern, .. } => {
                    work.push(expr);
                    work.push(pattern);
                }
                BoundExprKind::Aggregate {
                    argument: Some(expr),
                    ..
                } => work.push(expr),
                _ => {}
            }
        }
        let plan = Plan::from_bound(&bound, source).unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        eval(&query.projection()[0], &EvalRow::default(), &empty_frame())
    }

    #[test]
    fn runtime_parameter_contract_is_exact_and_typed() {
        let metadata = positional_metadata(&[Some(DataType::Decimal), None]);
        let values = [
            Value::Int64(7).into(),
            ParameterValue::typed_null(DataType::Text).unwrap(),
        ];
        let mut memory = WorkMemory::new(4096).unwrap();
        let frame =
            ParameterFrame::bind(&metadata, Parameters::positional(&values), &mut memory).unwrap();
        assert!(matches!(frame.get(0), Value::Decimal(_)));
        assert_eq!(frame.get(1), &Value::Null);
        assert_eq!(
            ParameterFrame::bind(&metadata, Parameters::none(), &mut memory)
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn scalar_evaluator_uses_three_valued_logic_and_locations() {
        assert_eq!(eval_sql_expr("TRUE AND NULL").unwrap(), Value::Null);
        assert_eq!(eval_sql_expr("FALSE AND NULL").unwrap(), Value::Bool(false));
        assert_eq!(eval_sql_expr("NULL = NULL").unwrap(), Value::Null);
        let error = eval_sql_expr("1 / 0").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        assert_eq!(error.sql_location().unwrap().column(), 1);
    }

    #[test]
    fn non_finite_float_is_rejected_and_signed_zero_is_one_key() {
        assert_eq!(
            validate_sql_float(f64::NAN).unwrap_err().code(),
            ErrorCode::Type
        );
        assert_eq!(
            sql_total_cmp(&Value::Float64(-0.0), &Value::Float64(0.0)).unwrap(),
            Ordering::Equal
        );
    }
}
