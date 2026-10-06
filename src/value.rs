/// A scalar SQL value.
///
/// SQL `NULL` is represented only by [`Value::Null`]; it is distinct from every concrete value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`, with no concrete type attached.
    Null,
    /// A signed 64-bit integer.
    Int64(i64),
    /// An exact base-10 decimal.
    Decimal(crate::Decimal),
    /// An IEEE-754 double-precision floating-point value.
    Float64(f64),
    /// Owned UTF-8 text.
    Text(String),
    /// Owned binary data.
    Blob(Vec<u8>),
    /// A calendar date and whole-second time without a time-zone offset.
    Date(crate::OracleDate),
    /// A date and time with nanosecond precision and an optional fixed offset.
    Timestamp(crate::OracleTimestamp),
    /// A Boolean value.
    Bool(bool),
}

#[cfg(test)]
mod tests {
    use super::Value;
    use crate::{Decimal, OracleDate, OracleTimestamp};

    #[test]
    fn carries_every_supported_scalar_value() {
        let decimal = Decimal::new(false, 2, 123).expect("valid decimal");
        let date = OracleDate::new(2026, 9, 1, 12, 0, 0).expect("valid date");
        let timestamp = OracleTimestamp::new(date, 123_456, None).expect("valid timestamp");
        let values = [
            Value::Null,
            Value::Int64(42),
            Value::Decimal(decimal),
            Value::Float64(1.5),
            Value::Text("text".to_owned()),
            Value::Blob(vec![0, 1]),
            Value::Date(date),
            Value::Timestamp(timestamp),
            Value::Bool(true),
        ];

        assert_eq!(values, values.clone());
    }
}
