use crate::{DbError, Decimal, ErrorCode, OracleDate, OracleTimestamp, Result, Value};

use super::{
    catalog::types::ColumnType,
    codec::{Decoder, Encoder},
};

const INT64_LEN: usize = 8;
const DECIMAL_LEN: usize = 20;
const FLOAT64_LEN: usize = 8;
const DATE_LEN: usize = 8;
const TIMESTAMP_LEN: usize = 12;
const TIMESTAMP_WITH_TIME_ZONE_LEN: usize = 16;
const BOOL_LEN: usize = 1;

pub(super) fn encode_value(column_type: ColumnType, value: &Value) -> Result<Vec<u8>> {
    match (column_type, value) {
        (ColumnType::Int64, Value::Int64(value)) => Ok(value.to_le_bytes().to_vec()),
        (ColumnType::Decimal, Value::Decimal(value)) => {
            let mut encoder = Encoder::new();
            encoder.put_u8(u8::from(value.negative()));
            encoder.put_u8(value.scale() as u8);
            encoder.put_u16(0);
            encoder.put_u128(value.magnitude());
            Ok(encoder.into_bytes())
        }
        (ColumnType::Float64, Value::Float64(value)) => Ok(value.to_bits().to_le_bytes().to_vec()),
        (ColumnType::Text, Value::Text(value)) => Ok(value.as_bytes().to_vec()),
        (ColumnType::Blob, Value::Blob(value)) => Ok(value.clone()),
        (ColumnType::Date, Value::Date(value)) => Ok(encode_date(*value)),
        (ColumnType::Timestamp, Value::Timestamp(value)) if value.offset_minutes().is_none() => {
            let mut bytes = encode_date(value.date());
            bytes.extend_from_slice(&value.micros().to_le_bytes());
            Ok(bytes)
        }
        (ColumnType::TimestampWithTimeZone, Value::Timestamp(value)) => {
            let offset = value.offset_minutes().ok_or_else(type_mismatch)?;
            let mut bytes = encode_date(value.date());
            bytes.extend_from_slice(&value.micros().to_le_bytes());
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            Ok(bytes)
        }
        (ColumnType::Bool, Value::Bool(value)) => Ok(vec![u8::from(*value)]),
        _ => Err(type_mismatch()),
    }
}

pub(super) fn decode_value(column_type: ColumnType, bytes: &[u8]) -> Result<Value> {
    match column_type {
        ColumnType::Int64 => {
            require_length(bytes, INT64_LEN)?;
            let mut decoder = Decoder::new(bytes);
            Ok(Value::Int64(decoder.get_u64()? as i64))
        }
        ColumnType::Decimal => decode_decimal(bytes).map(Value::Decimal),
        ColumnType::Float64 => {
            require_length(bytes, FLOAT64_LEN)?;
            let mut decoder = Decoder::new(bytes);
            Ok(Value::Float64(f64::from_bits(decoder.get_u64()?)))
        }
        ColumnType::Text => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| corruption("text value is not valid UTF-8"))?;
            Ok(Value::Text(text.to_owned()))
        }
        ColumnType::Blob => Ok(Value::Blob(bytes.to_vec())),
        ColumnType::Date => decode_date(bytes).map(Value::Date),
        ColumnType::Timestamp => decode_timestamp(bytes, None, TIMESTAMP_LEN),
        ColumnType::TimestampWithTimeZone => {
            require_length(bytes, TIMESTAMP_WITH_TIME_ZONE_LEN)?;
            let mut decoder = Decoder::new(bytes);
            let date_bytes = decoder.get_bytes(DATE_LEN)?;
            let date = decode_date(date_bytes)?;
            let micros = decoder.get_u32()?;
            let offset = decoder.get_i16()?;
            if decoder.get_u16()? != 0 {
                return Err(corruption(
                    "timestamp with time zone reserved bytes are nonzero",
                ));
            }
            decode_timestamp_fields(date, micros, Some(offset))
        }
        ColumnType::Bool => {
            require_length(bytes, BOOL_LEN)?;
            match bytes[0] {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                _ => Err(corruption("boolean payload must contain zero or one")),
            }
        }
    }
}

fn encode_date(date: OracleDate) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.put_u16(date.year());
    encoder.put_u8(date.month());
    encoder.put_u8(date.day());
    encoder.put_u8(date.hour());
    encoder.put_u8(date.minute());
    encoder.put_u8(date.second());
    encoder.put_u8(0);
    encoder.into_bytes()
}

fn decode_decimal(bytes: &[u8]) -> Result<Decimal> {
    require_length(bytes, DECIMAL_LEN)?;
    let mut decoder = Decoder::new(bytes);
    let flags = decoder.get_u8()?;
    if flags & !1 != 0 {
        return Err(corruption("decimal flags contain reserved bits"));
    }
    let scale = decoder.get_u8()? as i8;
    if decoder.get_u16()? != 0 {
        return Err(corruption("decimal reserved bytes are nonzero"));
    }
    let magnitude = decoder.get_u128()?;
    Decimal::new(flags & 1 != 0, scale, magnitude)
        .map_err(|_| corruption("decimal payload fields are invalid"))
}

fn decode_date(bytes: &[u8]) -> Result<OracleDate> {
    require_length(bytes, DATE_LEN)?;
    let mut decoder = Decoder::new(bytes);
    let year = decoder.get_u16()?;
    let month = decoder.get_u8()?;
    let day = decoder.get_u8()?;
    let hour = decoder.get_u8()?;
    let minute = decoder.get_u8()?;
    let second = decoder.get_u8()?;
    if decoder.get_u8()? != 0 {
        return Err(corruption("date reserved byte is nonzero"));
    }
    OracleDate::new(year, month, day, hour, minute, second)
        .map_err(|_| corruption("date payload fields are invalid"))
}

fn decode_timestamp(bytes: &[u8], offset: Option<i16>, exact_len: usize) -> Result<Value> {
    require_length(bytes, exact_len)?;
    let mut decoder = Decoder::new(bytes);
    let date_bytes = decoder.get_bytes(DATE_LEN)?;
    let date = decode_date(date_bytes)?;
    let micros = decoder.get_u32()?;
    decode_timestamp_fields(date, micros, offset)
}

fn decode_timestamp_fields(date: OracleDate, micros: u32, offset: Option<i16>) -> Result<Value> {
    OracleTimestamp::new(date, micros, offset)
        .map(Value::Timestamp)
        .map_err(|_| corruption("timestamp payload fields are invalid"))
}

fn require_length(bytes: &[u8], exact_len: usize) -> Result<()> {
    if bytes.len() != exact_len {
        return Err(corruption("scalar payload has an invalid fixed length"));
    }
    Ok(())
}

fn type_mismatch() -> DbError {
    DbError::new(
        ErrorCode::Type,
        "value does not match its bound column type",
    )
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::{decode_value, encode_value};
    use crate::storage::catalog::types::ColumnType;
    use crate::{Decimal, ErrorCode, OracleDate, OracleTimestamp, Value};

    #[test]
    fn fixed_width_scalar_payloads_are_exact() {
        assert_eq!(
            encode_value(ColumnType::Int64, &Value::Int64(-2)).unwrap(),
            vec![0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );

        let decimal = Decimal::new(true, -2, 123).unwrap();
        assert_eq!(
            encode_value(ColumnType::Decimal, &Value::Decimal(decimal)).unwrap(),
            vec![
                1, 0xfe, 0, 0, 123, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        );

        assert_eq!(
            encode_value(ColumnType::Bool, &Value::Bool(true)).unwrap(),
            vec![1]
        );
        assert_eq!(
            encode_value(ColumnType::Bool, &Value::Bool(false)).unwrap(),
            vec![0]
        );
    }

    #[test]
    fn decimal_negative_zero_is_encoded_canonically() {
        let decimal = Decimal::new(true, -38, 0).unwrap();
        let mut disk_negative_zero = [0_u8; 20];
        disk_negative_zero[0] = 1;
        disk_negative_zero[1] = 0xda;

        assert_eq!(
            encode_value(ColumnType::Decimal, &Value::Decimal(decimal)).unwrap(),
            vec![
                0, 0xda, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
        assert_eq!(
            decode_value(ColumnType::Decimal, &disk_negative_zero).unwrap(),
            Value::Decimal(decimal)
        );
    }

    #[test]
    fn float_nan_payload_bits_are_preserved() {
        let bits = 0x7ff8_0000_0000_0042_u64;
        let value = Value::Float64(f64::from_bits(bits));

        assert_eq!(
            encode_value(ColumnType::Float64, &value).unwrap(),
            vec![0x42, 0, 0, 0, 0, 0, 0xf8, 0x7f]
        );
        let Value::Float64(decoded) =
            decode_value(ColumnType::Float64, &[0x42, 0, 0, 0, 0, 0, 0xf8, 0x7f]).unwrap()
        else {
            panic!("decoded float payload as the wrong value type");
        };
        assert_eq!(decoded.to_bits(), bits);
    }

    #[test]
    fn text_and_blob_payloads_preserve_empty_and_raw_bytes() {
        assert_eq!(
            encode_value(ColumnType::Text, &Value::Text(String::new())).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            decode_value(ColumnType::Text, &[]).unwrap(),
            Value::Text(String::new())
        );
        assert_eq!(
            encode_value(ColumnType::Text, &Value::Text("表A".to_owned())).unwrap(),
            vec![0xe8, 0xa1, 0xa8, b'A']
        );
        assert_eq!(
            encode_value(ColumnType::Blob, &Value::Blob(Vec::new())).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            decode_value(ColumnType::Blob, &[]).unwrap(),
            Value::Blob(Vec::new())
        );
        assert_eq!(
            decode_value(ColumnType::Blob, &[0, 0xff, 7]).unwrap(),
            Value::Blob(vec![0, 0xff, 7])
        );
        assert_eq!(
            encode_value(ColumnType::Blob, &Value::Blob(vec![0, 0xff, 7])).unwrap(),
            vec![0, 0xff, 7]
        );
    }

    #[test]
    fn leap_day_date_payload_is_exact_and_validated() {
        let date = OracleDate::new(2024, 2, 29, 23, 59, 58).unwrap();
        let bytes = vec![0xe8, 0x07, 2, 29, 23, 59, 58, 0];

        assert_eq!(
            encode_value(ColumnType::Date, &Value::Date(date)).unwrap(),
            bytes
        );
        assert_eq!(
            decode_value(ColumnType::Date, &bytes).unwrap(),
            Value::Date(date)
        );

        let invalid = [0xe7, 0x07, 2, 29, 0, 0, 0, 0];
        assert_eq!(
            decode_value(ColumnType::Date, &invalid).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn timestamp_without_time_zone_payload_is_exact() {
        let date = OracleDate::new(2026, 9, 9, 12, 34, 56).unwrap();
        let timestamp = OracleTimestamp::new(date, 123_456, None).unwrap();
        let bytes = vec![0xea, 0x07, 9, 9, 12, 34, 56, 0, 0x40, 0xe2, 0x01, 0x00];

        assert_eq!(
            encode_value(ColumnType::Timestamp, &Value::Timestamp(timestamp)).unwrap(),
            bytes
        );
        assert_eq!(
            decode_value(ColumnType::Timestamp, &bytes).unwrap(),
            Value::Timestamp(timestamp)
        );
    }

    #[test]
    fn timestamp_with_time_zone_payload_is_exact() {
        let date = OracleDate::new(2026, 9, 9, 12, 34, 56).unwrap();
        let value = Value::Timestamp(OracleTimestamp::new(date, 123_456, Some(480)).unwrap());
        assert_eq!(
            encode_value(ColumnType::TimestampWithTimeZone, &value).unwrap(),
            vec![
                0xea, 0x07, 9, 9, 12, 34, 56, 0, 0x40, 0xe2, 0x01, 0x00, 0xe0, 0x01, 0, 0,
            ]
        );
    }

    #[test]
    fn timestamp_offset_boundaries_round_trip() {
        let date = OracleDate::new(2026, 9, 9, 0, 0, 0).unwrap();
        for offset in [-840_i16, 840_i16] {
            let timestamp = OracleTimestamp::new(date, 999_999, Some(offset)).unwrap();
            let encoded = encode_value(
                ColumnType::TimestampWithTimeZone,
                &Value::Timestamp(timestamp),
            )
            .unwrap();

            assert_eq!(
                decode_value(ColumnType::TimestampWithTimeZone, &encoded).unwrap(),
                Value::Timestamp(timestamp)
            );
        }
    }

    #[test]
    fn fixed_width_payloads_reject_every_wrong_length() {
        let cases = [
            (ColumnType::Int64, 8),
            (ColumnType::Decimal, 20),
            (ColumnType::Float64, 8),
            (ColumnType::Date, 8),
            (ColumnType::Timestamp, 12),
            (ColumnType::TimestampWithTimeZone, 16),
            (ColumnType::Bool, 1),
        ];

        for (column_type, exact_len) in cases {
            for wrong_len in [exact_len - 1, exact_len + 1] {
                assert_eq!(
                    decode_value(column_type, &vec![0; wrong_len])
                        .unwrap_err()
                        .code(),
                    ErrorCode::Corruption
                );
            }
        }
    }

    #[test]
    fn malformed_scalar_fields_are_corruption() {
        let malformed = [
            (ColumnType::Decimal, vec![2; 20]),
            (ColumnType::Decimal, {
                let mut bytes = vec![0; 20];
                bytes[2] = 1;
                bytes
            }),
            (ColumnType::Decimal, {
                let mut bytes = vec![0; 20];
                bytes[1] = 39;
                bytes
            }),
            (ColumnType::Date, vec![0xe8, 0x07, 1, 1, 0, 0, 0, 1]),
            (ColumnType::Timestamp, {
                let mut bytes = vec![0xe8, 0x07, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0];
                bytes[8..12].copy_from_slice(&1_000_000_u32.to_le_bytes());
                bytes
            }),
            (ColumnType::TimestampWithTimeZone, {
                let mut bytes = vec![0xe8, 0x07, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                bytes[12..14].copy_from_slice(&841_i16.to_le_bytes());
                bytes
            }),
            (ColumnType::TimestampWithTimeZone, {
                let mut bytes = vec![0xe8, 0x07, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0];
                bytes[12..14].copy_from_slice(&480_i16.to_le_bytes());
                bytes
            }),
            (ColumnType::Bool, vec![2]),
            (ColumnType::Text, vec![0xff]),
        ];

        for (column_type, bytes) in malformed {
            assert_eq!(
                decode_value(column_type, &bytes).unwrap_err().code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn caller_value_and_column_type_mismatches_are_type_errors() {
        let date = OracleDate::new(2026, 9, 9, 12, 34, 56).unwrap();
        let without_offset = OracleTimestamp::new(date, 0, None).unwrap();
        let with_offset = OracleTimestamp::new(date, 0, Some(0)).unwrap();
        let cases = [
            (ColumnType::Int64, Value::Text("1".to_owned())),
            (ColumnType::Int64, Value::Null),
            (ColumnType::Timestamp, Value::Timestamp(with_offset)),
            (
                ColumnType::TimestampWithTimeZone,
                Value::Timestamp(without_offset),
            ),
        ];

        for (column_type, value) in cases {
            assert_eq!(
                encode_value(column_type, &value).unwrap_err().code(),
                ErrorCode::Type
            );
        }
    }
}
