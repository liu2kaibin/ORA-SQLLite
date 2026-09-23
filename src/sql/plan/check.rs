use super::{Locations, invalid_plan};
use crate::{
    Result,
    error::SqlLocation,
    sql::{
        SqlType,
        bound::{BoundBinaryOp, BoundExpr, BoundExprKind, BoundUnaryOp},
    },
    storage::constraint::program::{
        CheckBinaryOp, CheckProgramTemplate, CheckTemplateInstruction, CheckType, CheckUnaryOp,
    },
};

/// Compiles a new-table CHECK expression into its owned, identity-free
/// postorder template. The catalog writer later resolves its column ordinals.
pub(super) fn compile_check(
    expression: &BoundExpr,
    locations: &Locations<'_>,
) -> Result<(CheckProgramTemplate, SqlLocation)> {
    let root_location = locations.resolve(expression.span)?;
    let mut work = vec![(expression, false)];
    let mut instructions = Vec::new();
    while let Some((expression, ready)) = work.pop() {
        if !ready {
            work.push((expression, true));
            match &expression.kind {
                BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                    work.push((expr, false));
                }
                BoundExprKind::Binary { left, right, .. } => {
                    work.push((right, false));
                    work.push((left, false));
                }
                BoundExprKind::InList { expr, values, .. } => {
                    work.extend(values.iter().rev().map(|value| (value, false)));
                    work.push((expr, false));
                }
                BoundExprKind::Between {
                    expr, low, high, ..
                } => {
                    work.push((high, false));
                    work.push((low, false));
                    work.push((expr, false));
                }
                BoundExprKind::Like { expr, pattern, .. } => {
                    work.push((pattern, false));
                    work.push((expr, false));
                }
                BoundExprKind::Literal(_)
                | BoundExprKind::NewTableColumn { .. }
                | BoundExprKind::Parameter(_)
                | BoundExprKind::Column { .. }
                | BoundExprKind::Aggregate { .. } => {}
            }
            continue;
        }

        let result = check_type(expression.sql_type)?;
        let instruction = match &expression.kind {
            BoundExprKind::Literal(value) => CheckTemplateInstruction::Literal {
                value: value.clone(),
                result,
            },
            BoundExprKind::NewTableColumn { ordinal } => CheckTemplateInstruction::Column {
                ordinal: *ordinal,
                result,
            },
            BoundExprKind::Unary { op, .. } => CheckTemplateInstruction::Unary {
                op: check_unary(*op),
                result,
            },
            BoundExprKind::Binary { op, .. } => CheckTemplateInstruction::Binary {
                op: check_binary(*op)?,
                result,
            },
            BoundExprKind::IsNull { negated, .. } => CheckTemplateInstruction::IsNull {
                negated: *negated,
                result,
            },
            BoundExprKind::InList {
                values, negated, ..
            } => CheckTemplateInstruction::InList {
                value_count: u16::try_from(values.len())
                    .map_err(|_| invalid_plan("CHECK IN list exceeds u16"))?,
                negated: *negated,
                result,
            },
            BoundExprKind::Between { negated, .. } => CheckTemplateInstruction::Between {
                negated: *negated,
                result,
            },
            BoundExprKind::Like { negated, .. } => CheckTemplateInstruction::Like {
                negated: *negated,
                result,
            },
            BoundExprKind::Parameter(_) => {
                return Err(invalid_plan(
                    "CHECK parameters must have been rejected by binding",
                ));
            }
            BoundExprKind::Aggregate { .. } => {
                return Err(invalid_plan(
                    "CHECK aggregates must have been rejected by binding",
                ));
            }
            BoundExprKind::Column { .. } => {
                return Err(invalid_plan(
                    "CHECK foreign columns must have been rejected by binding",
                ));
            }
        };
        instructions.push(instruction);
    }
    Ok((CheckProgramTemplate::new(instructions)?, root_location))
}

const fn check_unary(op: BoundUnaryOp) -> CheckUnaryOp {
    match op {
        BoundUnaryOp::Positive => CheckUnaryOp::Plus,
        BoundUnaryOp::Negative => CheckUnaryOp::Minus,
        BoundUnaryOp::Not => CheckUnaryOp::Not,
    }
}

fn check_binary(op: BoundBinaryOp) -> Result<CheckBinaryOp> {
    Ok(match op {
        BoundBinaryOp::Add => CheckBinaryOp::Add,
        BoundBinaryOp::Subtract => CheckBinaryOp::Subtract,
        BoundBinaryOp::Multiply => CheckBinaryOp::Multiply,
        BoundBinaryOp::Divide => CheckBinaryOp::Divide,
        BoundBinaryOp::Equal => CheckBinaryOp::Equal,
        BoundBinaryOp::NotEqual => CheckBinaryOp::NotEqual,
        BoundBinaryOp::LessThan => CheckBinaryOp::LessThan,
        BoundBinaryOp::LessThanOrEqual => CheckBinaryOp::LessThanOrEqual,
        BoundBinaryOp::GreaterThan => CheckBinaryOp::GreaterThan,
        BoundBinaryOp::GreaterThanOrEqual => CheckBinaryOp::GreaterThanOrEqual,
        BoundBinaryOp::And => CheckBinaryOp::And,
        BoundBinaryOp::Or => CheckBinaryOp::Or,
        BoundBinaryOp::Modulo => {
            return Err(invalid_plan(
                "CHECK modulo must have been rejected by binding",
            ));
        }
    })
}

fn check_type(sql_type: SqlType) -> Result<CheckType> {
    match sql_type {
        SqlType::Int64 => Ok(CheckType::Int64),
        SqlType::Decimal => Ok(CheckType::Decimal),
        SqlType::Float64 => Ok(CheckType::Float64),
        SqlType::Text => Ok(CheckType::Text),
        SqlType::Blob => Ok(CheckType::Blob),
        SqlType::Date => Ok(CheckType::Date),
        SqlType::Timestamp => Ok(CheckType::Timestamp),
        SqlType::Bool => Ok(CheckType::Bool),
        SqlType::Null | SqlType::Unknown => Err(invalid_plan(
            "CHECK expression has no persistent result type",
        )),
    }
}
