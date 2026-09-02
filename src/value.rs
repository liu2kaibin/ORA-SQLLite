#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Decimal(crate::Decimal),
    Float64(f64),
    Text(String),
    Blob(Vec<u8>),
    Date(crate::OracleDate),
    Timestamp(crate::OracleTimestamp),
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
