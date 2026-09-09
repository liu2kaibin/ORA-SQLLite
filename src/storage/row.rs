use crate::{DbError, Decimal, ErrorCode, OracleDate, OracleTimestamp, Result, Value};

use super::{
    catalog::types::{ColumnDef, ColumnType, MAX_COLUMNS, RowId, TableDef},
    codec::{Decoder, Encoder},
};

const ROW_MAGIC: &[u8; 4] = b"OROW";
const ROW_FORMAT_VERSION: u16 = 1;
const ROW_HEADER_LEN: usize = 32;
const DIRECTORY_ENTRY_LEN: usize = 8;
const INT64_LEN: usize = 8;
const DECIMAL_LEN: usize = 20;
const FLOAT64_LEN: usize = 8;
const DATE_LEN: usize = 8;
const TIMESTAMP_LEN: usize = 12;
const TIMESTAMP_WITH_TIME_ZONE_LEN: usize = 16;
const BOOL_LEN: usize = 1;

pub(crate) struct RowCodec;

#[derive(Debug, Clone, Copy)]
struct RowLayout {
    bitmap_len: usize,
    directory_len: usize,
    payload_len: usize,
    total_len: usize,
}

impl RowCodec {
    pub(crate) fn encode(
        table: &TableDef,
        columns: &[ColumnDef],
        values: &[Value],
        max_row_bytes: usize,
    ) -> Result<Vec<u8>> {
        let layout = preflight_row_layout(table, columns, values, max_row_bytes)?;
        let mut payloads = Vec::with_capacity(columns.len());

        for (column, value) in columns.iter().zip(values) {
            if matches!(value, Value::Null) {
                payloads.push(None);
                continue;
            }

            let payload = encode_value(column.column_type(), value)?;
            payloads.push(Some(payload));
        }

        let bitmap_len_u16 = u16::try_from(layout.bitmap_len)
            .map_err(|_| type_error("row NULL bitmap length exceeds the disk format"))?;
        let directory_len_u32 = u32::try_from(layout.directory_len)
            .map_err(|_| type_error("row directory length exceeds the disk format"))?;
        let payload_len_u32 = u32::try_from(layout.payload_len)
            .map_err(|_| type_error("row payload length exceeds the disk format"))?;

        let mut bitmap = vec![0_u8; layout.bitmap_len];
        for (index, payload) in payloads.iter().enumerate() {
            if payload.is_none() {
                bitmap[index / 8] |= 1 << (index % 8);
            }
        }

        let mut bytes = Vec::with_capacity(layout.total_len);
        bytes.extend_from_slice(ROW_MAGIC);
        bytes.extend_from_slice(&ROW_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&table.schema_version().get().to_le_bytes());
        bytes.extend_from_slice(&table.column_count().to_le_bytes());
        bytes.extend_from_slice(&bitmap_len_u16.to_le_bytes());
        bytes.extend_from_slice(&directory_len_u32.to_le_bytes());
        bytes.extend_from_slice(&payload_len_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&bitmap);

        let mut payload_offset = 0_usize;
        for payload in &payloads {
            match payload {
                None => {
                    bytes.extend_from_slice(&0_u32.to_le_bytes());
                    bytes.extend_from_slice(&0_u32.to_le_bytes());
                }
                Some(payload) => {
                    let offset = u32::try_from(payload_offset)
                        .map_err(|_| type_error("row payload offset exceeds the disk format"))?;
                    let len = u32::try_from(payload.len())
                        .map_err(|_| type_error("row value length exceeds the disk format"))?;
                    bytes.extend_from_slice(&offset.to_le_bytes());
                    bytes.extend_from_slice(&len.to_le_bytes());
                    payload_offset = payload_offset
                        .checked_add(payload.len())
                        .ok_or_else(|| type_error("row payload offset overflows"))?;
                }
            }
        }
        for payload in payloads.iter().flatten() {
            bytes.extend_from_slice(payload);
        }

        if bytes.len() != layout.total_len || payload_offset != layout.payload_len {
            return Err(DbError::new(
                ErrorCode::Internal,
                "row encoder produced an inconsistent length",
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn decode(
        table: &TableDef,
        columns: &[ColumnDef],
        bytes: &[u8],
        max_row_bytes: usize,
    ) -> Result<Vec<Value>> {
        validate_caller_schema(table, columns)?;
        if bytes.len() > max_row_bytes {
            return Err(type_error("row exceeds the caller resource limit"));
        }

        let mut decoder = Decoder::new(bytes);
        if decoder.get_bytes(ROW_MAGIC.len())? != ROW_MAGIC {
            return Err(corruption("row magic is invalid"));
        }
        let format_version = decoder.get_u16()?;
        if format_version == 0 {
            return Err(corruption("row format version is invalid"));
        }
        if format_version > ROW_FORMAT_VERSION {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "row format version is newer than this implementation",
            ));
        }
        if decoder.get_u16()? != 0 {
            return Err(corruption("row flags or reserved bits are nonzero"));
        }

        let schema_version = decoder.get_u64()?;
        let column_count = usize::from(decoder.get_u16()?);
        let stored_bitmap_len = usize::from(decoder.get_u16()?);
        let stored_directory_len = usize::try_from(decoder.get_u32()?)
            .map_err(|_| corruption("row directory length exceeds the platform"))?;
        let payload_len = usize::try_from(decoder.get_u32()?)
            .map_err(|_| corruption("row payload length exceeds the platform"))?;
        if decoder.get_u32()? != 0 {
            return Err(corruption("row header reserved bytes are nonzero"));
        }

        if schema_version == 0 || schema_version != table.schema_version().get() {
            return Err(corruption("row schema version does not match the table"));
        }
        if column_count > usize::from(MAX_COLUMNS)
            || column_count != usize::from(table.column_count())
        {
            return Err(corruption("row column count does not match the table"));
        }
        let expected_bitmap_len = bitmap_len(column_count)
            .ok_or_else(|| corruption("row NULL bitmap length overflows"))?;
        if stored_bitmap_len != expected_bitmap_len {
            return Err(corruption("row NULL bitmap length is invalid"));
        }
        let expected_directory_len = column_count
            .checked_mul(DIRECTORY_ENTRY_LEN)
            .ok_or_else(|| corruption("row directory length overflows"))?;
        if stored_directory_len != expected_directory_len {
            return Err(corruption("row directory length is invalid"));
        }
        let total_len = checked_total_len(stored_bitmap_len, stored_directory_len, payload_len)
            .ok_or_else(|| corruption("row length overflows"))?;
        if u32::try_from(total_len).is_err() {
            return Err(corruption("row length exceeds the disk format"));
        }
        if total_len != bytes.len() {
            return Err(corruption("row length does not match its header"));
        }

        let bitmap = decoder.get_bytes(stored_bitmap_len)?;
        validate_bitmap_high_bits(bitmap, column_count)?;
        let directory = decoder.get_bytes(stored_directory_len)?;
        let payload = decoder.get_bytes(payload_len)?;
        if decoder.remaining() != 0 {
            return Err(corruption("row contains trailing bytes"));
        }

        let entries = validate_directory(columns, bitmap, directory, payload_len)?;
        let mut values = Vec::with_capacity(column_count);
        for (column, entry) in columns.iter().zip(entries) {
            match entry {
                None => values.push(Value::Null),
                Some((offset, len)) => {
                    let end = offset
                        .checked_add(len)
                        .ok_or_else(|| corruption("row value range overflows"))?;
                    let value_bytes = payload
                        .get(offset..end)
                        .ok_or_else(|| corruption("row value range exceeds the payload"))?;
                    values.push(decode_value(column.column_type(), value_bytes)?);
                }
            }
        }
        Ok(values)
    }
}

fn preflight_row_layout(
    table: &TableDef,
    columns: &[ColumnDef],
    values: &[Value],
    max_row_bytes: usize,
) -> Result<RowLayout> {
    validate_caller_schema(table, columns)?;
    if values.len() != columns.len() {
        return Err(type_error(
            "row value count does not match the table schema",
        ));
    }

    let bitmap_len =
        bitmap_len(columns.len()).ok_or_else(|| type_error("row NULL bitmap length overflows"))?;
    let directory_len = columns
        .len()
        .checked_mul(DIRECTORY_ENTRY_LEN)
        .ok_or_else(|| type_error("row directory length overflows"))?;
    let mut payload_len = 0_usize;

    for (column, value) in columns.iter().zip(values) {
        if matches!(value, Value::Null) {
            if !column.nullable() {
                return Err(type_error("NULL is not valid for a non-nullable column"));
            }
            continue;
        }
        payload_len = payload_len
            .checked_add(encoded_value_len(column.column_type(), value)?)
            .ok_or_else(|| type_error("row payload length overflows"))?;
    }

    let total_len = checked_total_len(bitmap_len, directory_len, payload_len)
        .ok_or_else(|| type_error("row length overflows"))?;
    require_encodable_row_len(total_len, max_row_bytes)?;
    Ok(RowLayout {
        bitmap_len,
        directory_len,
        payload_len,
        total_len,
    })
}

fn encoded_value_len(column_type: ColumnType, value: &Value) -> Result<usize> {
    match (column_type, value) {
        (ColumnType::Int64, Value::Int64(_)) => Ok(INT64_LEN),
        (ColumnType::Decimal, Value::Decimal(_)) => Ok(DECIMAL_LEN),
        (ColumnType::Float64, Value::Float64(_)) => Ok(FLOAT64_LEN),
        (ColumnType::Text, Value::Text(value)) => Ok(value.len()),
        (ColumnType::Blob, Value::Blob(value)) => Ok(value.len()),
        (ColumnType::Date, Value::Date(_)) => Ok(DATE_LEN),
        (ColumnType::Timestamp, Value::Timestamp(value)) if value.offset_minutes().is_none() => {
            Ok(TIMESTAMP_LEN)
        }
        (ColumnType::TimestampWithTimeZone, Value::Timestamp(value))
            if value.offset_minutes().is_some() =>
        {
            Ok(TIMESTAMP_WITH_TIME_ZONE_LEN)
        }
        (ColumnType::Bool, Value::Bool(_)) => Ok(BOOL_LEN),
        _ => Err(type_mismatch()),
    }
}

pub(crate) fn encode_rowid_key(rowid: RowId) -> [u8; 8] {
    rowid.get().to_be_bytes()
}

pub(crate) fn decode_rowid_key(bytes: &[u8]) -> Result<RowId> {
    let raw: [u8; 8] = bytes
        .try_into()
        .map_err(|_| corruption("ROWID key must contain exactly eight bytes"))?;
    RowId::new(u64::from_be_bytes(raw))
        .map_err(|_| corruption("ROWID key contains an invalid zero value"))
}

fn validate_caller_schema(table: &TableDef, columns: &[ColumnDef]) -> Result<()> {
    if columns.len() != usize::from(table.column_count())
        || columns.len() > usize::from(MAX_COLUMNS)
    {
        return Err(type_error(
            "column definitions do not match the table column count",
        ));
    }
    for (index, column) in columns.iter().enumerate() {
        if column.table_id() != table.id() || usize::from(column.ordinal()) != index {
            return Err(type_error(
                "column definitions are not ordered for the requested table",
            ));
        }
    }
    Ok(())
}

fn bitmap_len(column_count: usize) -> Option<usize> {
    column_count.checked_add(7).map(|count| count / 8)
}

fn checked_total_len(bitmap_len: usize, directory_len: usize, payload_len: usize) -> Option<usize> {
    ROW_HEADER_LEN
        .checked_add(bitmap_len)?
        .checked_add(directory_len)?
        .checked_add(payload_len)
}

fn require_encodable_row_len(total_len: usize, max_row_bytes: usize) -> Result<()> {
    if u32::try_from(total_len).is_err() {
        return Err(type_error("row length exceeds the disk format"));
    }
    if total_len > max_row_bytes {
        return Err(type_error("row exceeds the caller resource limit"));
    }
    Ok(())
}

fn validate_bitmap_high_bits(bitmap: &[u8], column_count: usize) -> Result<()> {
    let used_bits = column_count % 8;
    if used_bits == 0 {
        return Ok(());
    }
    let last = bitmap
        .last()
        .copied()
        .ok_or_else(|| corruption("row NULL bitmap is missing"))?;
    let allowed = (1_u8 << used_bits) - 1;
    if last & !allowed != 0 {
        return Err(corruption("row NULL bitmap has nonzero high bits"));
    }
    Ok(())
}

fn validate_directory(
    columns: &[ColumnDef],
    bitmap: &[u8],
    directory: &[u8],
    payload_len: usize,
) -> Result<Vec<Option<(usize, usize)>>> {
    let mut entries = Vec::with_capacity(columns.len());
    let mut expected_offset = 0_usize;
    let mut decoder = Decoder::new(directory);

    for (index, column) in columns.iter().enumerate() {
        let offset = usize::try_from(decoder.get_u32()?)
            .map_err(|_| corruption("row value offset exceeds the platform"))?;
        let len = usize::try_from(decoder.get_u32()?)
            .map_err(|_| corruption("row value length exceeds the platform"))?;
        let is_null = bitmap[index / 8] & (1 << (index % 8)) != 0;

        if is_null {
            if !column.nullable() {
                return Err(corruption("row contains NULL for a non-nullable column"));
            }
            if offset != 0 || len != 0 {
                return Err(corruption("NULL row directory entry is not zero"));
            }
            entries.push(None);
            continue;
        }

        if offset != expected_offset {
            return Err(corruption("row value ranges are not contiguous"));
        }
        expected_offset = offset
            .checked_add(len)
            .ok_or_else(|| corruption("row value range overflows"))?;
        if expected_offset > payload_len {
            return Err(corruption("row value range exceeds the payload"));
        }
        entries.push(Some((offset, len)));
    }
    if decoder.remaining() != 0 || expected_offset != payload_len {
        return Err(corruption(
            "row directory does not account for the complete payload",
        ));
    }
    Ok(entries)
}

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

fn type_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Type, message)
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::{
        RowCodec, decode_rowid_key, decode_value, encode_rowid_key, encode_value,
        preflight_row_layout,
    };
    use crate::storage::catalog::types::{
        CatalogName, ColumnDef, ColumnId, ColumnType, RowId, SchemaVersion, TableDef, TableId,
    };
    use crate::storage::{PageId, PageRef};
    use crate::{Decimal, ErrorCode, OracleDate, OracleTimestamp, Value};

    const MAX_ROW_BYTES: usize = 1 << 20;

    fn schema(types: &[(ColumnType, bool)]) -> (TableDef, Vec<ColumnDef>) {
        let table_id = TableId::new(11).unwrap();
        let table = TableDef::new(
            table_id,
            SchemaVersion::new(1).unwrap(),
            PageRef::new(PageId(12), 1).unwrap(),
            RowId::new(1).unwrap(),
            u16::try_from(types.len()).unwrap(),
            CatalogName::new("T").unwrap(),
        )
        .unwrap();
        let columns = types
            .iter()
            .enumerate()
            .map(|(ordinal, &(column_type, nullable))| {
                ColumnDef::new(
                    ColumnId::new(u64::try_from(ordinal).unwrap() + 100).unwrap(),
                    table_id,
                    u16::try_from(ordinal).unwrap(),
                    column_type,
                    nullable,
                    CatalogName::new(format!("C{ordinal}")).unwrap(),
                )
                .unwrap()
            })
            .collect();
        (table, columns)
    }

    fn nullable_text_schema(count: usize) -> (TableDef, Vec<ColumnDef>, Vec<Value>) {
        let types = vec![(ColumnType::Text, true); count];
        let (table, columns) = schema(&types);
        let values = vec![Value::Null; count];
        (table, columns, values)
    }

    fn assert_corruption(table: &TableDef, columns: &[ColumnDef], bytes: &[u8]) {
        assert_eq!(
            RowCodec::decode(table, columns, bytes, MAX_ROW_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn rowid_key_is_big_endian_and_numeric_ordered() {
        assert_eq!(
            encode_rowid_key(RowId::new(0x0102_0304_0506_0708).unwrap()),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert!(
            encode_rowid_key(RowId::new(255).unwrap()) < encode_rowid_key(RowId::new(256).unwrap())
        );
        assert_eq!(
            decode_rowid_key(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap().get(),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            decode_rowid_key(&[0; 8]).unwrap_err().code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            decode_rowid_key(&[1; 7]).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn complete_row_has_the_hand_computed_v1_layout() {
        let (table, columns) = schema(&[
            (ColumnType::Int64, false),
            (ColumnType::Text, true),
            (ColumnType::Blob, false),
            (ColumnType::Bool, false),
        ]);
        let values = [
            Value::Int64(0x0102_0304_0506_0708),
            Value::Null,
            Value::Blob(Vec::new()),
            Value::Bool(true),
        ];
        let expected = vec![
            b'O',
            b'R',
            b'O',
            b'W', // magic
            1,
            0, // row format version
            0,
            0, // flags
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // schema version
            4,
            0, // column count
            1,
            0, // bitmap length
            32,
            0,
            0,
            0, // directory length
            9,
            0,
            0,
            0, // payload length
            0,
            0,
            0,
            0,           // reserved
            0b0000_0010, // NULL bitmap
            0,
            0,
            0,
            0,
            8,
            0,
            0,
            0, // int offset and length
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // NULL directory entry
            8,
            0,
            0,
            0,
            0,
            0,
            0,
            0, // empty blob at the current end
            8,
            0,
            0,
            0,
            1,
            0,
            0,
            0, // bool offset and length
            8,
            7,
            6,
            5,
            4,
            3,
            2,
            1,
            1, // payload
        ];

        let encoded = RowCodec::encode(&table, &columns, &values, MAX_ROW_BYTES).unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(
            RowCodec::decode(&table, &columns, &expected, MAX_ROW_BYTES).unwrap(),
            values
        );
    }

    #[test]
    fn column_count_and_bitmap_boundaries_round_trip() {
        for count in [0, 1, 8, 9, 999, 1000] {
            let (table, columns, values) = nullable_text_schema(count);
            let encoded = RowCodec::encode(&table, &columns, &values, MAX_ROW_BYTES).unwrap();
            assert_eq!(
                u16::from_le_bytes([encoded[16], encoded[17]]) as usize,
                count
            );
            assert_eq!(
                u16::from_le_bytes([encoded[18], encoded[19]]) as usize,
                count.div_ceil(8)
            );
            assert_eq!(
                RowCodec::decode(&table, &columns, &encoded, MAX_ROW_BYTES).unwrap(),
                values
            );
        }
    }

    #[test]
    fn null_empty_and_all_scalar_payloads_round_trip_without_conflation() {
        let date = OracleDate::new(2024, 2, 29, 23, 59, 58).unwrap();
        let timestamp = OracleTimestamp::new(date, 123_456, None).unwrap();
        let timestamp_tz = OracleTimestamp::new(date, 654_321, Some(480)).unwrap();
        let (table, columns) = schema(&[
            (ColumnType::Text, true),
            (ColumnType::Text, false),
            (ColumnType::Blob, false),
            (ColumnType::Int64, false),
            (ColumnType::Decimal, false),
            (ColumnType::Float64, false),
            (ColumnType::Date, false),
            (ColumnType::Timestamp, false),
            (ColumnType::TimestampWithTimeZone, false),
            (ColumnType::Bool, false),
        ]);
        let values = vec![
            Value::Null,
            Value::Text(String::new()),
            Value::Blob(Vec::new()),
            Value::Int64(-7),
            Value::Decimal(Decimal::new(true, -2, 123).unwrap()),
            Value::Float64(1.5),
            Value::Date(date),
            Value::Timestamp(timestamp),
            Value::Timestamp(timestamp_tz),
            Value::Bool(false),
        ];

        let encoded = RowCodec::encode(&table, &columns, &values, MAX_ROW_BYTES).unwrap();
        assert_eq!(
            RowCodec::decode(&table, &columns, &encoded, MAX_ROW_BYTES).unwrap(),
            values
        );
        let directory_start = 32 + 2;
        assert_eq!(&encoded[directory_start..directory_start + 8], &[0; 8]);
        assert_eq!(
            &encoded[directory_start + 8..directory_start + 24],
            &[0; 16]
        );
    }

    #[test]
    fn caller_schema_value_and_resource_mismatches_are_type_errors() {
        let (table, columns) = schema(&[(ColumnType::Int64, false)]);
        let good = [Value::Int64(1)];
        let exact = RowCodec::encode(&table, &columns, &good, MAX_ROW_BYTES).unwrap();

        assert_eq!(
            RowCodec::encode(&table, &[], &good, MAX_ROW_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            RowCodec::encode(&table, &columns, &[], MAX_ROW_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            RowCodec::encode(&table, &columns, &[Value::Null], MAX_ROW_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            RowCodec::encode(
                &table,
                &columns,
                &[Value::Text("1".to_owned())],
                MAX_ROW_BYTES,
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );
        assert_eq!(
            RowCodec::encode(&table, &columns, &good, exact.len() - 1)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            RowCodec::decode(&table, &columns, &exact, exact.len() - 1)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );

        let (_, mut wrong_columns) = schema(&[(ColumnType::Int64, false)]);
        wrong_columns[0] = ColumnDef::new(
            ColumnId::new(999).unwrap(),
            TableId::new(13).unwrap(),
            0,
            ColumnType::Int64,
            false,
            CatalogName::new("C0").unwrap(),
        )
        .unwrap();
        assert_eq!(
            RowCodec::encode(&table, &wrong_columns, &good, MAX_ROW_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn oversized_text_and_blob_fail_size_preflight_before_encoding() {
        let cases = [
            (ColumnType::Text, Value::Text("x".repeat(4096))),
            (ColumnType::Blob, Value::Blob(vec![0xa5; 4096])),
        ];

        for (column_type, value) in cases {
            let (table, columns) = schema(&[(column_type, false)]);
            assert_eq!(
                preflight_row_layout(&table, &columns, std::slice::from_ref(&value), 41)
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
            assert_eq!(
                RowCodec::encode(&table, &columns, &[value], 41)
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
        }
    }

    #[test]
    fn future_row_format_is_unsupported_but_v1_header_corruption_is_rejected() {
        let (table, columns) = schema(&[(ColumnType::Text, false)]);
        let encoded = RowCodec::encode(
            &table,
            &columns,
            &[Value::Text("x".to_owned())],
            MAX_ROW_BYTES,
        )
        .unwrap();

        let mut future = encoded.clone();
        future[4..6].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            RowCodec::decode(&table, &columns, &future, MAX_ROW_BYTES)
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );

        let mutations: &[(usize, &[u8])] = &[
            (0, b"X"),
            (4, &0_u16.to_le_bytes()),
            (6, &1_u16.to_le_bytes()),
            (8, &2_u64.to_le_bytes()),
            (16, &2_u16.to_le_bytes()),
            (18, &2_u16.to_le_bytes()),
            (20, &0_u32.to_le_bytes()),
            (24, &0_u32.to_le_bytes()),
            (28, &1_u32.to_le_bytes()),
        ];
        for &(offset, replacement) in mutations {
            let mut malformed = encoded.clone();
            malformed[offset..offset + replacement.len()].copy_from_slice(replacement);
            assert_corruption(&table, &columns, &malformed);
        }
    }

    #[test]
    fn bitmap_directory_payload_and_trailing_corruption_are_rejected() {
        let (table, columns) = schema(&[(ColumnType::Text, false)]);
        let encoded = RowCodec::encode(
            &table,
            &columns,
            &[Value::Text("x".to_owned())],
            MAX_ROW_BYTES,
        )
        .unwrap();

        let mut bitmap_high_bit = encoded.clone();
        bitmap_high_bit[32] = 0x80;
        assert_corruption(&table, &columns, &bitmap_high_bit);

        let mut wrong_offset = encoded.clone();
        wrong_offset[33..37].copy_from_slice(&1_u32.to_le_bytes());
        assert_corruption(&table, &columns, &wrong_offset);

        let mut wrong_length = encoded.clone();
        wrong_length[37..41].copy_from_slice(&2_u32.to_le_bytes());
        assert_corruption(&table, &columns, &wrong_length);

        let mut invalid_utf8 = encoded.clone();
        invalid_utf8[41] = 0xff;
        assert_corruption(&table, &columns, &invalid_utf8);

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_corruption(&table, &columns, &trailing);

        let (date_table, date_columns) = schema(&[(ColumnType::Date, false)]);
        let date = OracleDate::new(2024, 1, 1, 0, 0, 0).unwrap();
        let mut reserved = RowCodec::encode(
            &date_table,
            &date_columns,
            &[Value::Date(date)],
            MAX_ROW_BYTES,
        )
        .unwrap();
        reserved[41 + 7] = 1;
        assert_corruption(&date_table, &date_columns, &reserved);

        assert_corruption(&table, &columns, &encoded[..encoded.len() - 1]);

        let (null_table, null_columns) = schema(&[(ColumnType::Text, true)]);
        let mut null_directory =
            RowCodec::encode(&null_table, &null_columns, &[Value::Null], MAX_ROW_BYTES).unwrap();
        null_directory[33] = 1;
        assert_corruption(&null_table, &null_columns, &null_directory);
    }

    #[test]
    fn decoder_mutations_and_every_truncation_are_controlled() {
        let (table, columns) = schema(&[
            (ColumnType::Int64, false),
            (ColumnType::Text, true),
            (ColumnType::Blob, false),
            (ColumnType::Bool, false),
        ]);
        let values = [
            Value::Int64(42),
            Value::Text("row".to_owned()),
            Value::Blob(vec![0, 0xff]),
            Value::Bool(true),
        ];
        let encoded = RowCodec::encode(&table, &columns, &values, MAX_ROW_BYTES).unwrap();

        for index in 0..encoded.len() {
            let mut mutated = encoded.clone();
            mutated[index] ^= 0xff;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                RowCodec::decode(&table, &columns, &mutated, MAX_ROW_BYTES)
            }));
            match outcome {
                Ok(Ok(decoded)) => assert_eq!(decoded.len(), columns.len()),
                Ok(Err(_)) => {}
                Err(_) => panic!("decoder panicked after mutating byte {index}"),
            }
        }

        for end in 0..encoded.len() {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                RowCodec::decode(&table, &columns, &encoded[..end], MAX_ROW_BYTES)
            }));
            match outcome {
                Ok(Err(error)) => assert_eq!(error.code(), ErrorCode::Corruption),
                Ok(Ok(_)) => panic!("decoder accepted truncation at byte {end}"),
                Err(_) => panic!("decoder panicked on truncation at byte {end}"),
            }
        }
    }

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
