use crate::{
    DbError, Decimal, ErrorCode, Result, Value,
    scalar::decimal_cmp,
    storage::catalog::types::{ColumnDef, ColumnType},
};
use std::cmp::Ordering;

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

/// The immutable Oracle semantics selected by binding a comparison.  Both
/// sides are retained so persisted CHECK bytecode cannot accidentally inherit
/// a later catalog interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct OracleComparisonContract {
    left: Option<OracleTypeDescriptor>,
    right: Option<OracleTypeDescriptor>,
}

impl OracleComparisonContract {
    pub(crate) fn new(
        left: Option<OracleTypeDescriptor>,
        right: Option<OracleTypeDescriptor>,
    ) -> Result<Self> {
        let (Some(left_descriptor), Some(right_descriptor)) = (left, right) else {
            return Err(DbError::new(
                ErrorCode::Type,
                "Oracle comparison requires descriptors on both operands",
            ));
        };
        left_descriptor.validate()?;
        right_descriptor.validate()?;
        if !comparison_family_is_compatible(left_descriptor, right_descriptor) {
            return Err(DbError::new(
                ErrorCode::Type,
                "Oracle comparison operands have incompatible type families",
            ));
        }
        Ok(Self { left, right })
    }

    pub(crate) const fn left(self) -> OracleTypeDescriptor {
        match self.left {
            Some(descriptor) => descriptor,
            None => unreachable!(),
        }
    }

    pub(crate) const fn right(self) -> OracleTypeDescriptor {
        match self.right {
            Some(descriptor) => descriptor,
            None => unreachable!(),
        }
    }
}

pub(crate) fn compare_for_contract(
    contract: OracleComparisonContract,
    left: &Value,
    right: &Value,
) -> Result<Ordering> {
    let left_descriptor = contract.left();
    let right_descriptor = contract.right();
    match (left_descriptor, right_descriptor) {
        (OracleTypeDescriptor::Number { .. }, OracleTypeDescriptor::Number { .. }) => {
            let left = decimal_for_comparison(left)?;
            let right = decimal_for_comparison(right)?;
            decimal_cmp(left, right)
        }
        (OracleTypeDescriptor::Varchar2 { .. }, OracleTypeDescriptor::Varchar2 { .. })
        | (OracleTypeDescriptor::Char { .. }, OracleTypeDescriptor::Varchar2 { .. })
        | (OracleTypeDescriptor::Varchar2 { .. }, OracleTypeDescriptor::Char { .. }) => {
            Ok(text_for_comparison(left)?.cmp(text_for_comparison(right)?))
        }
        (
            OracleTypeDescriptor::Char {
                length_chars: left_length,
            },
            OracleTypeDescriptor::Char {
                length_chars: right_length,
            },
        ) => {
            let length = usize::try_from(left_length.max(right_length))
                .map_err(|_| comparison_type_error())?;
            let left = pad_to_char_length(text_for_comparison(left)?, length);
            let right = pad_to_char_length(text_for_comparison(right)?, length);
            Ok(left.cmp(&right))
        }
        (OracleTypeDescriptor::Date, OracleTypeDescriptor::Date) => {
            let Value::Date(left) = left else {
                return Err(comparison_type_error());
            };
            let Value::Date(right) = right else {
                return Err(comparison_type_error());
            };
            Ok(date_fields(*left).cmp(&date_fields(*right)))
        }
        (OracleTypeDescriptor::Timestamp { .. }, OracleTypeDescriptor::Timestamp { .. }) => {
            let left = local_timestamp_fields(left)?;
            let right = local_timestamp_fields(right)?;
            Ok(left.cmp(&right))
        }
        (
            OracleTypeDescriptor::TimestampWithTimeZone { .. },
            OracleTypeDescriptor::TimestampWithTimeZone { .. },
        ) => {
            let left = utc_timestamp_fields(left)?;
            let right = utc_timestamp_fields(right)?;
            Ok(left.cmp(&right))
        }
        _ => Err(comparison_type_error()),
    }
}

pub(crate) fn encode_comparison_descriptor(descriptor: OracleTypeDescriptor) -> [u8; 8] {
    let (kind, precision, scale, length) = match descriptor {
        OracleTypeDescriptor::Number { precision, scale } => (1, precision, scale as u8, 0),
        OracleTypeDescriptor::Varchar2 { length_chars } => (2, 0, 0, length_chars),
        OracleTypeDescriptor::Char { length_chars } => (3, 0, 0, length_chars),
        OracleTypeDescriptor::Raw { length_bytes } => (4, 0, 0, length_bytes),
        OracleTypeDescriptor::Date => (5, 0, 0, 0),
        OracleTypeDescriptor::Timestamp {
            fractional_precision,
        } => (6, fractional_precision, 0, 0),
        OracleTypeDescriptor::TimestampWithTimeZone {
            fractional_precision,
        } => (7, fractional_precision, 0, 0),
    };
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&[kind, precision, scale, 0]);
    bytes[4..].copy_from_slice(&length.to_le_bytes());
    bytes
}

pub(crate) fn decode_comparison_descriptor(bytes: &[u8]) -> Result<OracleTypeDescriptor> {
    let [kind, precision, scale, reserved, length @ ..] = bytes else {
        return Err(DbError::new(
            ErrorCode::Type,
            "Oracle descriptor has invalid length",
        ));
    };
    if *reserved != 0 || length.len() != 4 {
        return Err(DbError::new(
            ErrorCode::Type,
            "Oracle descriptor reserved bytes are invalid",
        ));
    }
    let length = u32::from_le_bytes(
        length
            .try_into()
            .map_err(|_| DbError::new(ErrorCode::Type, "Oracle descriptor length is invalid"))?,
    );
    match (*kind, *precision, *scale, length) {
        (1, precision, scale, 0) => OracleTypeDescriptor::number(precision, scale as i8),
        (2, 0, 0, length) => OracleTypeDescriptor::varchar2(length),
        (3, 0, 0, length) => OracleTypeDescriptor::char_type(length),
        (4, 0, 0, length) => OracleTypeDescriptor::raw(length),
        (5, 0, 0, 0) => Ok(OracleTypeDescriptor::date()),
        (6, precision, 0, 0) => OracleTypeDescriptor::timestamp(precision),
        (7, precision, 0, 0) => OracleTypeDescriptor::timestamp_with_time_zone(precision),
        _ => Err(DbError::new(
            ErrorCode::Type,
            "Oracle descriptor is invalid",
        )),
    }
}

fn comparison_family_is_compatible(
    left: OracleTypeDescriptor,
    right: OracleTypeDescriptor,
) -> bool {
    matches!(
        (left, right),
        (
            OracleTypeDescriptor::Number { .. },
            OracleTypeDescriptor::Number { .. }
        ) | (
            OracleTypeDescriptor::Varchar2 { .. },
            OracleTypeDescriptor::Varchar2 { .. }
        ) | (
            OracleTypeDescriptor::Char { .. },
            OracleTypeDescriptor::Char { .. }
        ) | (
            OracleTypeDescriptor::Char { .. },
            OracleTypeDescriptor::Varchar2 { .. }
        ) | (
            OracleTypeDescriptor::Varchar2 { .. },
            OracleTypeDescriptor::Char { .. }
        ) | (OracleTypeDescriptor::Date, OracleTypeDescriptor::Date)
            | (
                OracleTypeDescriptor::Timestamp { .. },
                OracleTypeDescriptor::Timestamp { .. }
            )
            | (
                OracleTypeDescriptor::TimestampWithTimeZone { .. },
                OracleTypeDescriptor::TimestampWithTimeZone { .. }
            )
    )
}

fn decimal_for_comparison(value: &Value) -> Result<Decimal> {
    match value {
        Value::Decimal(value) => Ok(*value),
        Value::Int64(value) => {
            Decimal::new(value.is_negative(), 0, u128::from(value.unsigned_abs()))
        }
        _ => Err(comparison_type_error()),
    }
}

fn text_for_comparison(value: &Value) -> Result<&str> {
    match value {
        Value::Text(value) => Ok(value),
        _ => Err(comparison_type_error()),
    }
}

fn pad_to_char_length(value: &str, length: usize) -> String {
    let mut padded = value.to_owned();
    padded.extend(std::iter::repeat_n(
        ' ',
        length.saturating_sub(value.chars().count()),
    ));
    padded
}

fn date_fields(date: crate::OracleDate) -> (u16, u8, u8, u8, u8, u8) {
    (
        date.year(),
        date.month(),
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
    )
}

fn local_timestamp_fields(value: &Value) -> Result<((u16, u8, u8, u8, u8, u8), u32)> {
    let Value::Timestamp(value) = value else {
        return Err(comparison_type_error());
    };
    if value.offset_minutes().is_some() {
        return Err(comparison_type_error());
    }
    Ok((date_fields(value.date()), value.nanoseconds()))
}

fn utc_timestamp_fields(value: &Value) -> Result<(i32, u64)> {
    let Value::Timestamp(value) = value else {
        return Err(comparison_type_error());
    };
    if value.offset_minutes().is_none() {
        return Err(comparison_type_error());
    }
    value.utc_day_and_nanos()
}

fn comparison_type_error() -> DbError {
    DbError::new(
        ErrorCode::Type,
        "values do not match the Oracle comparison contract",
    )
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
            fractional_precision,
        } => normalize_timestamp(value, fractional_precision, false),
        OracleTypeDescriptor::TimestampWithTimeZone {
            fractional_precision,
        } => normalize_timestamp(value, fractional_precision, true),
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

fn normalize_date(value: Value) -> Result<Value> {
    match value {
        Value::Date(_) => Ok(value),
        Value::Text(text) => parse_date_text(&text).map(Value::Date),
        _ => Err(type_shape_error("DATE")),
    }
}

fn normalize_timestamp(
    value: Value,
    fractional_precision: u8,
    with_time_zone: bool,
) -> Result<Value> {
    match value {
        Value::Timestamp(timestamp) if timestamp.offset_minutes().is_some() == with_time_zone => {
            round_timestamp(timestamp, fractional_precision).map(Value::Timestamp)
        }
        Value::Text(text) => parse_timestamp_text(&text, with_time_zone)
            .and_then(|timestamp| round_timestamp(timestamp, fractional_precision))
            .map(Value::Timestamp),
        _ if with_time_zone => Err(type_shape_error("TIMESTAMP WITH TIME ZONE")),
        _ => Err(type_shape_error("TIMESTAMP")),
    }
}

fn parse_date_text(text: &str) -> Result<crate::OracleDate> {
    let bytes = text.as_bytes();
    match bytes.len() {
        10 => parse_date_fields(bytes, false),
        19 => parse_date_fields(bytes, true),
        _ => Err(datetime_literal_error()),
    }
}

fn parse_date_fields(bytes: &[u8], with_time: bool) -> Result<crate::OracleDate> {
    let expected_len = if with_time { 19 } else { 10 };
    if bytes.len() != expected_len
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (with_time && (bytes[10] != b' ' || bytes[13] != b':' || bytes[16] != b':'))
    {
        return Err(datetime_literal_error());
    }
    let year = parse_ascii_digits(bytes, 0, 4)? as u16;
    let month = parse_ascii_digits(bytes, 5, 2)? as u8;
    let day = parse_ascii_digits(bytes, 8, 2)? as u8;
    let (hour, minute, second) = if with_time {
        (
            parse_ascii_digits(bytes, 11, 2)? as u8,
            parse_ascii_digits(bytes, 14, 2)? as u8,
            parse_ascii_digits(bytes, 17, 2)? as u8,
        )
    } else {
        (0, 0, 0)
    };
    crate::OracleDate::new(year, month, day, hour, minute, second)
        .map_err(|_| datetime_literal_error())
}

fn parse_timestamp_text(text: &str, with_time_zone: bool) -> Result<crate::OracleTimestamp> {
    let bytes = text.as_bytes();
    let (local, offset) = if with_time_zone {
        if bytes.len() < 26 {
            return Err(datetime_literal_error());
        }
        let split = bytes.len() - 7;
        if bytes[split] != b' ' {
            return Err(datetime_literal_error());
        }
        (
            &bytes[..split],
            Some(parse_fixed_offset(&bytes[split + 1..])?),
        )
    } else {
        (bytes, None)
    };

    if local.len() < 19 {
        return Err(datetime_literal_error());
    }
    let date = parse_date_fields(&local[..19], true)?;
    let nanos = match local.len() {
        19 => 0,
        21..=29 if local[19] == b'.' => {
            let digits = &local[20..];
            let value = parse_ascii_digits(digits, 0, digits.len())?;
            let padding = u32::try_from(9 - digits.len()).map_err(|_| datetime_literal_error())?;
            value
                .checked_mul(10_u32.pow(padding))
                .ok_or_else(datetime_literal_error)?
        }
        _ => return Err(datetime_literal_error()),
    };
    crate::OracleTimestamp::from_nanoseconds(date, nanos, offset)
        .map_err(|_| datetime_literal_error())
}

fn parse_fixed_offset(bytes: &[u8]) -> Result<i16> {
    if bytes.len() != 6 || !matches!(bytes[0], b'+' | b'-') || bytes[3] != b':' {
        return Err(datetime_literal_error());
    }
    let hours = parse_ascii_digits(bytes, 1, 2)? as i16;
    let minutes = parse_ascii_digits(bytes, 4, 2)? as i16;
    if hours > 14 || minutes > 59 || (hours == 14 && minutes != 0) {
        return Err(datetime_literal_error());
    }
    let total = hours * 60 + minutes;
    Ok(if bytes[0] == b'-' { -total } else { total })
}

fn parse_ascii_digits(bytes: &[u8], start: usize, len: usize) -> Result<u32> {
    let digits = bytes
        .get(start..start.checked_add(len).ok_or_else(datetime_literal_error)?)
        .ok_or_else(datetime_literal_error)?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(datetime_literal_error());
    }
    digits.iter().try_fold(0_u32, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or_else(datetime_literal_error)
    })
}

pub(crate) fn round_timestamp(
    timestamp: crate::OracleTimestamp,
    fractional_precision: u8,
) -> Result<crate::OracleTimestamp> {
    if fractional_precision > 9 {
        return Err(datetime_literal_error());
    }
    let divisor = 10_u32.pow(u32::from(9 - fractional_precision));
    let nanos = timestamp.nanoseconds();
    let remainder = nanos % divisor;
    let mut rounded = nanos - remainder;
    if remainder != 0 && remainder >= divisor / 2 {
        rounded = rounded
            .checked_add(divisor)
            .ok_or_else(datetime_literal_error)?;
    }
    let date = if rounded == 1_000_000_000 {
        rounded = 0;
        next_second(timestamp.date())?
    } else {
        timestamp.date()
    };
    crate::OracleTimestamp::from_nanoseconds(date, rounded, timestamp.offset_minutes())
        .map_err(|_| datetime_literal_error())
}

fn next_second(date: crate::OracleDate) -> Result<crate::OracleDate> {
    if date.second() < 59 {
        return crate::OracleDate::new(
            date.year(),
            date.month(),
            date.day(),
            date.hour(),
            date.minute(),
            date.second() + 1,
        );
    }
    if date.minute() < 59 {
        return crate::OracleDate::new(
            date.year(),
            date.month(),
            date.day(),
            date.hour(),
            date.minute() + 1,
            0,
        );
    }
    if date.hour() < 23 {
        return crate::OracleDate::new(
            date.year(),
            date.month(),
            date.day(),
            date.hour() + 1,
            0,
            0,
        );
    }
    let (year, month, day) = next_day(date.year(), date.month(), date.day())?;
    crate::OracleDate::new(year, month, day, 0, 0, 0).map_err(|_| datetime_literal_error())
}

fn next_day(year: u16, month: u8, day: u8) -> Result<(u16, u8, u8)> {
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => return Err(datetime_literal_error()),
    };
    if day < days_in_month {
        Ok((year, month, day + 1))
    } else if month < 12 {
        Ok((year, month + 1, 1))
    } else if year < 9999 {
        Ok((year + 1, 1, 1))
    } else {
        Err(datetime_literal_error())
    }
}

fn datetime_literal_error() -> DbError {
    DbError::oracle(
        ErrorCode::Type,
        1861,
        "literal does not match the DATE or TIMESTAMP format",
    )
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
    use std::cmp::Ordering;

    use super::{
        OracleComparisonContract, OracleTypeDescriptor, compare_for_contract, normalize_for_column,
        normalize_for_type, round_timestamp,
    };
    use crate::{
        Decimal, ErrorCode, OracleDate, OracleTimestamp, Value,
        storage::catalog::types::{CatalogName, ColumnDef, ColumnId, TableId},
    };

    fn decimal_value(negative: bool, magnitude: u128, scale: i8) -> Value {
        Value::Decimal(Decimal::new(negative, scale, magnitude).unwrap())
    }

    #[test]
    fn oracle_comparisons_are_contract_aware_without_changing_legacy_timestamp() {
        let char2 = OracleTypeDescriptor::char_type(2).unwrap();
        let char3 = OracleTypeDescriptor::char_type(3).unwrap();
        let char_contract = OracleComparisonContract::new(Some(char2), Some(char3)).unwrap();
        assert_eq!(
            compare_for_contract(
                char_contract,
                &Value::Text("A ".into()),
                &Value::Text("A  ".into()),
            )
            .unwrap(),
            Ordering::Equal
        );

        let date = OracleDate::new(2026, 1, 1, 8, 0, 0).unwrap();
        let offset =
            Value::Timestamp(OracleTimestamp::from_nanoseconds(date, 0, Some(480)).unwrap());
        let utc = Value::Timestamp(
            OracleTimestamp::from_nanoseconds(
                OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap(),
                0,
                Some(0),
            )
            .unwrap(),
        );
        let descriptor = OracleTypeDescriptor::timestamp_with_time_zone(9).unwrap();
        let contract = OracleComparisonContract::new(Some(descriptor), Some(descriptor)).unwrap();
        assert_eq!(
            compare_for_contract(contract, &offset, &utc).unwrap(),
            Ordering::Equal
        );
        assert_ne!(
            crate::scalar::sql_total_cmp(&offset, &utc).unwrap(),
            Ordering::Equal
        );
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

    fn timestamp(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        nanos: u32,
        offset: Option<i16>,
    ) -> OracleTimestamp {
        OracleTimestamp::from_nanoseconds(
            OracleDate::new(year, month, day, hour, minute, second).unwrap(),
            nanos,
            offset,
        )
        .unwrap()
    }

    #[test]
    fn date_text_accepts_only_the_two_approved_ascii_forms() {
        let date = OracleDate::new(2026, 10, 5, 0, 0, 0).unwrap();
        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::date(),
                Value::Text("2026-10-05".into()),
            )
            .unwrap(),
            Value::Date(date)
        );
        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::date(),
                Value::Text("2026-10-05 23:59:58".into()),
            )
            .unwrap(),
            Value::Date(OracleDate::new(2026, 10, 5, 23, 59, 58).unwrap())
        );
        for text in [
            "2026-1-05",
            "2026/10/05",
            "2026-10-05T00:00:00",
            " 2026-10-05",
            "2026-10-05 ",
            "２０２６-10-05",
            "2025-02-29",
        ] {
            let error = normalize_for_type(OracleTypeDescriptor::date(), Value::Text(text.into()))
                .unwrap_err();
            assert_eq!(
                (error.code(), error.oracle_code()),
                (ErrorCode::Type, Some(1861))
            );
        }
    }

    #[test]
    fn timestamp_text_rounds_half_away_and_preserves_fixed_offset() {
        let timestamp_type = OracleTypeDescriptor::timestamp(6).unwrap();
        assert_eq!(
            normalize_for_type(
                timestamp_type,
                Value::Text("2026-01-02 03:04:05.1234565".into()),
            )
            .unwrap(),
            Value::Timestamp(timestamp(2026, 1, 2, 3, 4, 5, 123_457_000, None))
        );
        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::timestamp_with_time_zone(9).unwrap(),
                Value::Text("2026-01-02 03:04:05.1 -14:00".into()),
            )
            .unwrap(),
            Value::Timestamp(timestamp(2026, 1, 2, 3, 4, 5, 100_000_000, Some(-840),))
        );
        let direct = timestamp(2026, 1, 2, 3, 4, 5, 123_456_789, Some(480));
        assert_eq!(
            normalize_for_type(
                OracleTypeDescriptor::timestamp_with_time_zone(4).unwrap(),
                Value::Timestamp(direct),
            )
            .unwrap(),
            Value::Timestamp(timestamp(2026, 1, 2, 3, 4, 5, 123_500_000, Some(480),))
        );
    }

    #[test]
    fn timestamp_text_rejects_unapproved_shapes_fractions_and_offsets() {
        let plain = OracleTypeDescriptor::timestamp(9).unwrap();
        for text in [
            "2026-01-02",
            "2026-01-02T03:04:05",
            "2026-01-02 03:04:05.",
            "2026-01-02 03:04:05.1234567890",
            "2026-01-02 03:04:05 +00:00",
            "2026-01-02 03:04:05Z",
        ] {
            assert_eq!(
                normalize_for_type(plain, Value::Text(text.into()))
                    .unwrap_err()
                    .oracle_code(),
                Some(1861)
            );
        }

        let zoned = OracleTypeDescriptor::timestamp_with_time_zone(9).unwrap();
        for text in [
            "2026-01-02 03:04:05",
            "2026-01-02 03:04:05 +14:01",
            "2026-01-02 03:04:05 -14:01",
            "2026-01-02 03:04:05 +15:00",
            "2026-01-02 03:04:05 UTC",
        ] {
            assert_eq!(
                normalize_for_type(zoned, Value::Text(text.into()))
                    .unwrap_err()
                    .oracle_code(),
                Some(1861)
            );
        }
    }

    #[test]
    fn timestamp_rounding_carries_across_the_calendar_and_checks_maximum() {
        let rounded =
            round_timestamp(timestamp(2024, 12, 31, 23, 59, 59, 999_999_999, None), 0).unwrap();
        assert_eq!(
            rounded.date(),
            OracleDate::new(2025, 1, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(rounded.nanoseconds(), 0);

        let leap =
            round_timestamp(timestamp(2024, 2, 28, 23, 59, 59, 999_999_999, Some(60)), 0).unwrap();
        assert_eq!(leap.date(), OracleDate::new(2024, 2, 29, 0, 0, 0).unwrap());
        assert_eq!(leap.offset_minutes(), Some(60));

        let maximum = timestamp(9999, 12, 31, 23, 59, 59, 999_999_999, None);
        assert_eq!(
            round_timestamp(maximum, 0).unwrap_err().oracle_code(),
            Some(1861)
        );
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
