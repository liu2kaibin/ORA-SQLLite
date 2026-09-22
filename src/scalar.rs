//! Storage-independent SQL scalar semantics shared by expressions and checks.
use std::cmp::Ordering;

use crate::{DbError, Decimal, ErrorCode, OracleDate, Result, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TruthValue {
    True,
    False,
    Unknown,
}
impl TruthValue {
    pub(crate) fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Bool(true) => Ok(Self::True),
            Value::Bool(false) => Ok(Self::False),
            Value::Null => Ok(Self::Unknown),
            _ => Err(type_error("boolean value required")),
        }
    }
    pub(crate) const fn into_value(self) -> Value {
        match self {
            Self::True => Value::Bool(true),
            Self::False => Value::Bool(false),
            Self::Unknown => Value::Null,
        }
    }
    pub(crate) const fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
    pub(crate) const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }
    pub(crate) const fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }
}
impl From<bool> for TruthValue {
    fn from(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarUnaryOp {
    Plus,
    Minus,
    Not,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Concat,
    And,
    Or,
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

fn type_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Type, message)
}
fn numeric_error() -> DbError {
    type_error("arithmetic result is not exactly representable")
}
pub(crate) fn validate_sql_float(value: f64) -> Result<()> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(type_error("SQL requires finite Float64 values"))
    }
}
pub(crate) fn validate_value(value: &Value) -> Result<()> {
    if let Value::Float64(value) = value {
        validate_sql_float(*value)
    } else {
        Ok(())
    }
}
pub(crate) fn int_decimal(value: i64) -> Result<Decimal> {
    Decimal::new(value < 0, 0, u128::from(value.unsigned_abs()))
}
pub(crate) fn sql_equal(left: &Value, right: &Value) -> Result<TruthValue> {
    validate_value(left)?;
    validate_value(right)?;
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(TruthValue::Unknown);
    }
    Ok((sql_total_cmp(left, right)? == Ordering::Equal).into())
}
pub(crate) fn sql_total_cmp(left: &Value, right: &Value) -> Result<Ordering> {
    validate_value(left)?;
    validate_value(right)?;
    match (left, right) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => Ok(Ordering::Less),
        (_, Value::Null) => Ok(Ordering::Greater),
        (Value::Int64(a), Value::Int64(b)) => Ok(a.cmp(b)),
        (Value::Decimal(a), Value::Decimal(b)) => decimal_cmp(*a, *b),
        (Value::Int64(a), Value::Decimal(b)) => decimal_cmp(int_decimal(*a)?, *b),
        (Value::Decimal(a), Value::Int64(b)) => decimal_cmp(*a, int_decimal(*b)?),
        (Value::Float64(a), Value::Float64(b)) => a.partial_cmp(b).ok_or_else(numeric_error),
        (Value::Text(a), Value::Text(b)) => Ok(a.cmp(b)),
        (Value::Blob(a), Value::Blob(b)) => Ok(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Ok(date_tuple(*a).cmp(&date_tuple(*b))),
        (Value::Timestamp(a), Value::Timestamp(b)) => Ok((
            date_tuple(a.date()),
            a.micros(),
            a.offset_minutes(),
        )
            .cmp(&(date_tuple(b.date()), b.micros(), b.offset_minutes()))),
        _ => Err(type_error("values have incompatible SQL comparison types")),
    }
}
fn date_tuple(date: OracleDate) -> (u16, u8, u8, u8, u8, u8) {
    (
        date.year(),
        date.month(),
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
    )
}
pub(crate) fn decimal_cmp(a: Decimal, b: Decimal) -> Result<Ordering> {
    if a.negative() != b.negative() {
        return Ok(b.negative().cmp(&a.negative()));
    }
    let order = if a.magnitude() == 0 || b.magnitude() == 0 {
        a.magnitude().cmp(&b.magnitude())
    } else {
        let ad = a.magnitude().ilog10() + 1;
        let bd = b.magnitude().ilog10() + 1;
        let order =
            (i64::from(ad) - i64::from(a.scale())).cmp(&(i64::from(bd) - i64::from(b.scale())));
        if order == Ordering::Equal {
            let d = ad.max(bd);
            scale_magnitude(a.magnitude(), d - ad)?.cmp(&scale_magnitude(b.magnitude(), d - bd)?)
        } else {
            order
        }
    };
    Ok(if a.negative() { order.reverse() } else { order })
}
pub(crate) fn eval_unary(op: ScalarUnaryOp, value: Value) -> Result<Value> {
    match op {
        ScalarUnaryOp::Not => Ok(TruthValue::from_value(&value)?.not().into_value()),
        ScalarUnaryOp::Plus | ScalarUnaryOp::Minus => {
            let negative = op == ScalarUnaryOp::Minus;
            match value {
                Value::Null => Ok(Value::Null),
                Value::Int64(value) => Ok(Value::Int64(if negative {
                    value.checked_neg().ok_or_else(numeric_error)?
                } else {
                    value
                })),
                Value::Decimal(value) => Ok(Value::Decimal(Decimal::new(
                    value.negative() != negative,
                    value.scale(),
                    value.magnitude(),
                )?)),
                Value::Float64(value) => {
                    let value = if negative { -value } else { value };
                    validate_sql_float(value)?;
                    Ok(Value::Float64(value))
                }
                _ => Err(type_error("numeric operand required")),
            }
        }
    }
}
pub(crate) fn eval_binary(op: ScalarBinaryOp, left: &Value, right: &Value) -> Result<Value> {
    use ScalarBinaryOp::*;
    match op {
        And | Or => {
            let a = TruthValue::from_value(left)?;
            let b = TruthValue::from_value(right)?;
            Ok(if op == And { a.and(b) } else { a.or(b) }.into_value())
        }
        Equal | NotEqual => {
            let truth = sql_equal(left, right)?;
            Ok(if op == NotEqual { truth.not() } else { truth }.into_value())
        }
        LessThan | LessThanOrEqual | GreaterThan | GreaterThanOrEqual => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                return Ok(Value::Null);
            }
            if matches!(left, Value::Blob(_)) || matches!(right, Value::Blob(_)) {
                return Err(type_error("Blob supports only equality predicates"));
            }
            let order = sql_total_cmp(left, right)?;
            Ok(Value::Bool(match op {
                LessThan => order.is_lt(),
                LessThanOrEqual => order.is_le(),
                GreaterThan => order.is_gt(),
                GreaterThanOrEqual => order.is_ge(),
                _ => unreachable!(),
            }))
        }
        Concat => match (left, right) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::Text(a), Value::Text(b)) => Ok(Value::Text(format!("{a}{b}"))),
            _ => Err(type_error("CONCAT requires Text operands")),
        },
        Add | Subtract | Multiply | Divide | Modulo => numeric_binary(op, left, right),
    }
}
fn numeric_binary(op: ScalarBinaryOp, left: &Value, right: &Value) -> Result<Value> {
    use ScalarBinaryOp::*;
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    match (left, right) {
        (Value::Int64(a), Value::Int64(b)) => {
            let value = match op {
                Add => a.checked_add(*b),
                Subtract => a.checked_sub(*b),
                Multiply => a.checked_mul(*b),
                Divide => a.checked_div(*b),
                Modulo => a.checked_rem(*b),
                _ => None,
            };
            Ok(Value::Int64(value.ok_or_else(numeric_error)?))
        }
        (Value::Decimal(a), Value::Decimal(b)) => {
            Ok(Value::Decimal(decimal_arithmetic(op, *a, *b)?))
        }
        (Value::Int64(a), Value::Decimal(b)) => Ok(Value::Decimal(decimal_arithmetic(
            op,
            int_decimal(*a)?,
            *b,
        )?)),
        (Value::Decimal(a), Value::Int64(b)) => Ok(Value::Decimal(decimal_arithmetic(
            op,
            *a,
            int_decimal(*b)?,
        )?)),
        (Value::Float64(a), Value::Float64(b)) => {
            if matches!(op, Divide | Modulo) && *b == 0.0 {
                return Err(type_error("division by zero"));
            }
            let value = match op {
                Add => a + b,
                Subtract => a - b,
                Multiply => a * b,
                Divide => a / b,
                Modulo => a % b,
                _ => unreachable!(),
            };
            validate_sql_float(value)?;
            Ok(Value::Float64(value))
        }
        _ => Err(type_error("numeric operands have incompatible types")),
    }
}
fn scale_magnitude(value: u128, digits: u32) -> Result<u128> {
    if value == 0 {
        return Ok(0);
    }
    value
        .checked_mul(10_u128.checked_pow(digits).ok_or_else(numeric_error)?)
        .ok_or_else(numeric_error)
}
fn decimal_result(negative: bool, mut magnitude: u128, mut scale: i16) -> Result<Decimal> {
    if magnitude == 0 {
        return Decimal::new(false, 0, 0);
    }
    while magnitude % 10 == 0 && scale > -38 {
        magnitude /= 10;
        scale -= 1
    }
    if scale < -38 {
        magnitude = scale_magnitude(
            magnitude,
            u32::try_from(-38 - scale).map_err(|_| numeric_error())?,
        )?;
        scale = -38
    }
    Decimal::new(
        negative,
        i8::try_from(scale).map_err(|_| numeric_error())?,
        magnitude,
    )
}
pub(crate) fn decimal_arithmetic(op: ScalarBinaryOp, a: Decimal, b: Decimal) -> Result<Decimal> {
    use ScalarBinaryOp::*;
    match op {
        Add | Subtract | Modulo => {
            if op == Modulo && b.magnitude() == 0 {
                return Err(type_error("division by zero"));
            }
            let scale = i16::from(a.scale().max(b.scale()));
            let left = scale_magnitude(
                a.magnitude(),
                u32::try_from(scale - i16::from(a.scale())).map_err(|_| numeric_error())?,
            )?;
            let right = scale_magnitude(
                b.magnitude(),
                u32::try_from(scale - i16::from(b.scale())).map_err(|_| numeric_error())?,
            )?;
            if op == Modulo {
                return decimal_result(
                    a.negative(),
                    left.checked_rem(right).ok_or_else(numeric_error)?,
                    scale,
                );
            }
            let bn = b.negative() != (op == Subtract);
            let (negative, magnitude) = if a.negative() == bn {
                (
                    a.negative(),
                    left.checked_add(right).ok_or_else(numeric_error)?,
                )
            } else if left >= right {
                (
                    a.negative(),
                    left.checked_sub(right).ok_or_else(numeric_error)?,
                )
            } else {
                (bn, right.checked_sub(left).ok_or_else(numeric_error)?)
            };
            decimal_result(negative, magnitude, scale)
        }
        Multiply => decimal_result(
            a.negative() != b.negative(),
            a.magnitude()
                .checked_mul(b.magnitude())
                .ok_or_else(numeric_error)?,
            i16::from(a.scale()) + i16::from(b.scale()),
        ),
        Divide => decimal_divide(a, b),
        _ => Err(type_error("not a Decimal arithmetic operator")),
    }
}
fn decimal_divide(a: Decimal, b: Decimal) -> Result<Decimal> {
    if b.magnitude() == 0 {
        return Err(type_error("division by zero"));
    }
    if a.magnitude() == 0 {
        return Decimal::new(false, 0, 0);
    }
    let (mut x, mut y) = (a.magnitude(), b.magnitude());
    while y != 0 {
        let r = x % y;
        x = y;
        y = r
    }
    let mut numerator = a.magnitude() / x;
    let mut denominator = b.magnitude() / x;
    let (mut twos, mut fives) = (0_u32, 0_u32);
    while denominator % 2 == 0 {
        denominator /= 2;
        twos += 1
    }
    while denominator % 5 == 0 {
        denominator /= 5;
        fives += 1
    }
    if denominator != 1 {
        return Err(numeric_error());
    }
    let digits = twos.max(fives);
    numerator = numerator
        .checked_mul(
            2_u128
                .checked_pow(digits - twos)
                .ok_or_else(numeric_error)?,
        )
        .ok_or_else(numeric_error)?;
    numerator = numerator
        .checked_mul(
            5_u128
                .checked_pow(digits - fives)
                .ok_or_else(numeric_error)?,
        )
        .ok_or_else(numeric_error)?;
    decimal_result(
        a.negative() != b.negative(),
        numerator,
        i16::from(a.scale()) - i16::from(b.scale())
            + i16::try_from(digits).map_err(|_| numeric_error())?,
    )
}
/// Greedy Unicode-scalar wildcard matching; backslash is ordinary text.
pub(crate) fn like(text: &str, pattern: &str) -> bool {
    let mut text = text.chars();
    let mut pattern = pattern.chars();
    let mut star = None;
    loop {
        let nt = text.clone().next();
        let np = pattern.clone().next();
        if np == Some('%') {
            pattern.next();
            star = Some((pattern.clone(), text.clone()))
        } else if nt.is_some() && (np == Some('_') || nt == np) {
            let _ = text.next();
            let _ = pattern.next();
        } else if nt.is_none() && np.is_none() {
            return true;
        } else if let Some((saved_pattern, saved_text)) = &mut star {
            if saved_text.next().is_none() {
                return false;
            }
            text = saved_text.clone();
            pattern = saved_pattern.clone()
        } else {
            return false;
        }
    }
}
