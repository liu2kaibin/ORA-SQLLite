use crate::{
    DbError, Decimal, ErrorCode, Result, Value,
    storage::catalog::types::{ColumnDef, ColumnType},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OracleTypeDescriptor {
    Number { precision: u8, scale: i8 },
    Varchar2 { length_chars: u32 },
    Char { length_chars: u32 },
    Raw { length_bytes: u32 },
    Date,
    Timestamp { fractional_precision: u8 },
    TimestampWithTimeZone { fractional_precision: u8 },
}

impl OracleTypeDescriptor {
    pub(crate) fn number(precision: u8, scale: i8) -> Result<Self> {
        Self::Number { precision, scale }.validate()
    }

    pub(crate) fn varchar2(length_chars: u32) -> Result<Self> {
        Self::Varchar2 { length_chars }.validate()
    }

    pub(crate) fn char_type(length_chars: u32) -> Result<Self> {
        Self::Char { length_chars }.validate()
    }

    pub(crate) fn raw(length_bytes: u32) -> Result<Self> {
        Self::Raw { length_bytes }.validate()
    }

    pub(crate) const fn date() -> Self {
        Self::Date
    }

    pub(crate) fn timestamp(fractional_precision: u8) -> Result<Self> {
        Self::Timestamp {
            fractional_precision,
        }
        .validate()
    }

    pub(crate) fn timestamp_with_time_zone(fractional_precision: u8) -> Result<Self> {
        Self::TimestampWithTimeZone {
            fractional_precision,
        }
        .validate()
    }

    pub(crate) fn validate(self) -> Result<Self> {
        let error_message = match self {
            Self::Number { precision, scale }
                if !(1..=38).contains(&precision) || !(-38..=38).contains(&scale) =>
            {
                Some("invalid NUMBER precision or scale")
            }
            Self::Varchar2 { length_chars } if !(1..=32_767).contains(&length_chars) => {
                Some("invalid VARCHAR2 length")
            }
            Self::Char { length_chars } if !(1..=2_000).contains(&length_chars) => {
                Some("invalid CHAR length")
            }
            Self::Raw { length_bytes } if !(1..=32_767).contains(&length_bytes) => {
                Some("invalid RAW length")
            }
            Self::Timestamp {
                fractional_precision,
            } if fractional_precision > 9 => Some("invalid TIMESTAMP fractional precision"),
            Self::TimestampWithTimeZone {
                fractional_precision,
            } if fractional_precision > 9 => {
                Some("invalid TIMESTAMP WITH TIME ZONE fractional precision")
            }
            _ => None,
        };
        match error_message {
            Some(message) => Err(invalid_declaration(message)),
            None => Ok(self),
        }
    }

    pub(crate) const fn physical_type(self) -> ColumnType {
        match self {
            Self::Number { .. } => ColumnType::Decimal,
            Self::Varchar2 { .. } | Self::Char { .. } => ColumnType::Text,
            Self::Raw { .. } => ColumnType::Blob,
            Self::Date => ColumnType::Date,
            Self::Timestamp { .. } => ColumnType::Timestamp,
            Self::TimestampWithTimeZone { .. } => ColumnType::TimestampWithTimeZone,
        }
    }

    pub(crate) const fn precision(self) -> Option<u8> {
        match self {
            Self::Number { precision, .. } => Some(precision),
            _ => None,
        }
    }

    pub(crate) const fn scale(self) -> Option<i8> {
        match self {
            Self::Number { scale, .. } => Some(scale),
            _ => None,
        }
    }

    pub(crate) const fn length_chars(self) -> Option<u32> {
        match self {
            Self::Varchar2 { length_chars } | Self::Char { length_chars } => Some(length_chars),
            _ => None,
        }
    }

    pub(crate) const fn length_bytes(self) -> Option<u32> {
        match self {
            Self::Raw { length_bytes } => Some(length_bytes),
            _ => None,
        }
    }

    pub(crate) const fn fractional_precision(self) -> Option<u8> {
        match self {
            Self::Timestamp {
                fractional_precision,
            }
            | Self::TimestampWithTimeZone {
                fractional_precision,
            } => Some(fractional_precision),
            _ => None,
        }
    }
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
pub(crate) fn normalize_for_type(descriptor: OracleTypeDescriptor, value: Value) -> Result<Value> {
    if value == Value::Null || matches!(&value, Value::Text(text) if text.is_empty()) {
        return Ok(Value::Null);
    }

    let descriptor = descriptor.validate()?;
    match descriptor {
        OracleTypeDescriptor::Number { precision, scale } => {
            normalize_number(value, precision, scale).map(Value::Decimal)
        }
        OracleTypeDescriptor::Varchar2 { length_chars } => normalize_varchar(value, length_chars),
        OracleTypeDescriptor::Char { length_chars } => normalize_char(value, length_chars),
        OracleTypeDescriptor::Raw { length_bytes } => normalize_raw(value, length_bytes),
        OracleTypeDescriptor::Date => normalize_date(value),
        OracleTypeDescriptor::Timestamp {
            fractional_precision: _,
        } => normalize_timestamp(value, false),
        OracleTypeDescriptor::TimestampWithTimeZone {
            fractional_precision: _,
        } => normalize_timestamp(value, true),
    }
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
pub(crate) fn normalize_for_column(column: &ColumnDef, value: Value) -> Result<Value> {
    let normalized = match column.oracle_type() {
        Some(descriptor) => normalize_for_type(descriptor, value)?,
        None => value,
    };
    if !column.nullable() && normalized == Value::Null {
        return Err(DbError::oracle(
            ErrorCode::Constraint,
            1400,
            format!("column {} cannot be NULL", column.name().as_str()),
        ));
    }
    Ok(normalized)
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
fn normalize_number(value: Value, precision: u8, scale: i8) -> Result<Decimal> {
    match value {
        Value::Int64(value) => {
            Decimal::new(value.is_negative(), 0, u128::from(value.unsigned_abs()))
                .and_then(|value| value.quantize(precision, scale))
        }
        Value::Decimal(value) => value.quantize(precision, scale),
        Value::Text(value) => Decimal::parse_ascii_quantized(&value, precision, scale),
        _ => Err(DbError::oracle(
            ErrorCode::Type,
            1722,
            format!("value is not a valid NUMBER({precision},{scale})"),
        )),
    }
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
fn normalize_varchar(value: Value, length_chars: u32) -> Result<Value> {
    let Value::Text(text) = value else {
        return Err(type_shape_error(format!("VARCHAR2({length_chars})")));
    };
    if text.chars().count() > length_chars as usize {
        return Err(value_too_large(format!("VARCHAR2({length_chars})")));
    }
    Ok(Value::Text(text))
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
fn normalize_char(value: Value, length_chars: u32) -> Result<Value> {
    let Value::Text(mut text) = value else {
        return Err(type_shape_error(format!("CHAR({length_chars})")));
    };
    let character_count = text.chars().count();
    if character_count > length_chars as usize {
        return Err(value_too_large(format!("CHAR({length_chars})")));
    }
    text.extend(std::iter::repeat_n(
        ' ',
        length_chars as usize - character_count,
    ));
    Ok(Value::Text(text))
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
fn normalize_raw(value: Value, length_bytes: u32) -> Result<Value> {
    let Value::Blob(bytes) = value else {
        return Err(type_shape_error(format!("RAW({length_bytes})")));
    };
    if bytes.len() > length_bytes as usize {
        return Err(value_too_large(format!("RAW({length_bytes})")));
    }
    Ok(Value::Blob(bytes))
}

#[allow(
    dead_code,
    reason = "date parsing and normalization are implemented by phase 5 task 4"
)]
fn normalize_date(value: Value) -> Result<Value> {
    match value {
        Value::Date(_) => Ok(value),
        _ => Err(type_shape_error("DATE")),
    }
}

#[allow(
    dead_code,
    reason = "timestamp parsing and normalization are implemented by phase 5 task 4"
)]
fn normalize_timestamp(value: Value, with_time_zone: bool) -> Result<Value> {
    match &value {
        Value::Timestamp(timestamp) if timestamp.offset_minutes().is_some() == with_time_zone => {
            Ok(value)
        }
        _ if with_time_zone => Err(type_shape_error("TIMESTAMP WITH TIME ZONE")),
        _ => Err(type_shape_error("TIMESTAMP")),
    }
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
fn type_shape_error(declaration: impl AsRef<str>) -> DbError {
    DbError::new(
        ErrorCode::Type,
        format!("value does not match {}", declaration.as_ref()),
    )
}

#[allow(
    dead_code,
    reason = "central normalization is wired into execution by later phase 5 tasks"
)]
fn value_too_large(declaration: impl AsRef<str>) -> DbError {
    DbError::oracle(
        ErrorCode::Type,
        12899,
        format!("value is too large for {}", declaration.as_ref()),
    )
}

fn invalid_declaration(message: &'static str) -> DbError {
    DbError::oracle(ErrorCode::Type, 902, message)
}

#[cfg(test)]
mod tests {
    use super::{OracleTypeDescriptor, normalize_for_column, normalize_for_type};
    use crate::{
        Decimal, ErrorCode, Value,
        storage::catalog::types::{CatalogName, ColumnDef, ColumnId, TableId},
    };

    fn decimal_value(negative: bool, magnitude: u128, scale: i8) -> Value {
        Value::Decimal(Decimal::new(negative, scale, magnitude).unwrap())
    }

    #[test]
    fn number_rounds_before_precision_and_reports_oracle_errors() {
        let ty = OracleTypeDescriptor::number(5, 2).unwrap();
        assert_eq!(
            normalize_for_type(ty, Value::Text("12.345".into())).unwrap(),
            decimal_value(false, 1235, 2)
        );
        assert_eq!(
            normalize_for_type(ty, Value::Text("-12.345".into())).unwrap(),
            decimal_value(true, 1235, 2)
        );

        let carry = OracleTypeDescriptor::number(2, 1).unwrap();
        let error = normalize_for_type(carry, Value::Text("9.99".into())).unwrap_err();
        assert_eq!(
            (error.code(), error.oracle_code()),
            (ErrorCode::Type, Some(1438))
        );
        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::number(2, -1).unwrap(),
                Value::Int64(149),
            )
            .unwrap(),
            decimal_value(false, 15, -1)
        );
        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::number(4, 5).unwrap(),
                Value::Text("0.01234".into()),
            )
            .unwrap(),
            decimal_value(false, 1234, 5)
        );
    }

    #[test]
    fn number_text_parser_is_ascii_exact_and_rejects_invalid_sources() {
        let ty = OracleTypeDescriptor::number(4, 2).unwrap();
        for text in ["", "+", "-", ".", "1.2.3", " 1", "1 ", "1e2", "１.2"] {
            let value = if text.is_empty() {
                Value::Blob(Vec::new())
            } else {
                Value::Text(text.into())
            };
            let error = normalize_for_type(ty, value).unwrap_err();
            assert_eq!(
                (error.code(), error.oracle_code()),
                (ErrorCode::Type, Some(1722))
            );
            assert!(error.to_string().contains("NUMBER(4,2)"));
        }

        assert_eq!(
            normalize_for_type(ty, Value::Text("+.5".into())).unwrap(),
            decimal_value(false, 50, 2)
        );
        assert_eq!(
            normalize_for_type(ty, Value::Text("5.".into())).unwrap(),
            decimal_value(false, 500, 2)
        );
        assert_eq!(
            normalize_for_type(ty, Value::Decimal(Decimal::new(false, 3, 12_345).unwrap()))
                .unwrap(),
            decimal_value(false, 1235, 2)
        );
        assert_eq!(
            normalize_for_type(ty, Value::Float64(1.25))
                .unwrap_err()
                .oracle_code(),
            Some(1722)
        );
    }

    #[test]
    fn number_handles_checked_extremes_and_rounding_to_zero() {
        let maximum = "99999999999999999999999999999999999999";
        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::number(38, 0).unwrap(),
                Value::Text(maximum.into()),
            )
            .unwrap(),
            decimal_value(false, 99_999_999_999_999_999_999_999_999_999_999_999_999, 0)
        );

        let carry = normalize_for_type(
            OracleTypeDescriptor::number(38, 0).unwrap(),
            Value::Text("99999999999999999999999999999999999999.5".into()),
        )
        .unwrap_err();
        assert_eq!(carry.oracle_code(), Some(1438));

        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::number(1, -38).unwrap(),
                Value::Text("0.49999999999999999999999999999999999999".into()),
            )
            .unwrap(),
            decimal_value(false, 0, -38)
        );
    }

    #[test]
    fn empty_text_is_null_but_existing_null_empty_blob_and_space_are_distinct() {
        let forged = OracleTypeDescriptor::Varchar2 { length_chars: 0 };
        assert_eq!(
            normalize_for_type(forged, Value::Null).unwrap(),
            Value::Null
        );
        assert_eq!(
            normalize_for_type(forged, Value::Text(String::new())).unwrap(),
            Value::Null
        );

        let varchar = OracleTypeDescriptor::varchar2(3).unwrap();
        assert_eq!(
            normalize_for_type(varchar, Value::Text(String::new())).unwrap(),
            Value::Null
        );
        assert_eq!(
            normalize_for_type(varchar, Value::Text(" ".into())).unwrap(),
            Value::Text(" ".into())
        );
        let raw = OracleTypeDescriptor::raw(3).unwrap();
        assert_eq!(
            normalize_for_type(raw, Value::Blob(Vec::new())).unwrap(),
            Value::Blob(Vec::new())
        );
    }

    #[test]
    fn unicode_lengths_and_char_padding_use_scalar_values() {
        let varchar = OracleTypeDescriptor::varchar2(2).unwrap();
        assert_eq!(
            normalize_for_type(varchar, Value::Text("表A".into())).unwrap(),
            Value::Text("表A".into())
        );
        let too_long = normalize_for_type(varchar, Value::Text("表AB".into())).unwrap_err();
        assert_eq!(
            (too_long.code(), too_long.oracle_code()),
            (ErrorCode::Type, Some(12899))
        );

        let char_type = OracleTypeDescriptor::char_type(3).unwrap();
        assert_eq!(
            normalize_for_type(char_type, Value::Text("表".into())).unwrap(),
            Value::Text("表  ".into())
        );
    }

    #[test]
    fn character_and_raw_contracts_reject_wrong_shapes_and_overlength_values() {
        let varchar = OracleTypeDescriptor::varchar2(2).unwrap();
        let varchar_shape = normalize_for_type(varchar, Value::Blob(vec![1])).unwrap_err();
        assert_eq!(varchar_shape.code(), ErrorCode::Type);
        assert!(varchar_shape.to_string().contains("VARCHAR2(2)"));

        let raw = OracleTypeDescriptor::raw(2).unwrap();
        let raw_length = normalize_for_type(raw, Value::Blob(vec![1, 2, 3])).unwrap_err();
        assert_eq!(raw_length.oracle_code(), Some(12899));
        let raw_shape = normalize_for_type(raw, Value::Text("00".into())).unwrap_err();
        assert_eq!(raw_shape.code(), ErrorCode::Type);
        assert!(raw_shape.to_string().contains("RAW(2)"));
    }

    #[test]
    fn column_normalization_enforces_nullability_after_empty_text_conversion() {
        let nullable = ColumnDef::new_oracle(
            ColumnId::new(1).unwrap(),
            TableId::new(2).unwrap(),
            0,
            true,
            CatalogName::new("OPTIONAL_CODE").unwrap(),
            OracleTypeDescriptor::varchar2(3).unwrap(),
        )
        .unwrap();
        assert_eq!(
            normalize_for_column(&nullable, Value::Null).unwrap(),
            Value::Null
        );

        let required = ColumnDef::new_oracle(
            ColumnId::new(3).unwrap(),
            TableId::new(2).unwrap(),
            1,
            false,
            CatalogName::new("REQUIRED_CODE").unwrap(),
            OracleTypeDescriptor::varchar2(3).unwrap(),
        )
        .unwrap();
        for value in [Value::Null, Value::Text(String::new())] {
            let error = normalize_for_column(&required, value).unwrap_err();
            assert_eq!(
                (error.code(), error.oracle_code()),
                (ErrorCode::Constraint, Some(1400))
            );
            assert!(error.to_string().contains("REQUIRED_CODE"));
            assert!(!error.is_fatal());
        }
    }
}
