//! Canonical, order-preserving IndexKey V1 components followed by a ROWID.
use std::ops::Bound;

use crate::storage::catalog::types::{ColumnDef, ColumnType, RowId};
use crate::{
    DbError, Decimal, ErrorCode, OracleDate, OracleTimestamp, Result, Value,
    oracle_type::OracleTypeDescriptor,
};

pub(crate) const MAX_INDEX_KEY_BYTES: usize = 1024;
// A final fixed-width component can cross the logical limit before the
// component check, but never by 64 bytes. Reserve once; never grow a large key.
pub(crate) const INDEX_KEY_BUFFER_BYTES: usize = MAX_INDEX_KEY_BYTES + 64;
const ROWID_BYTES: usize = 8;

#[derive(Debug)]
pub(crate) struct EncodedIndexKey {
    physical: Vec<u8>,
    logical_len: usize,
    contains_null: bool,
    rowid: RowId,
}

impl EncodedIndexKey {
    pub(crate) fn physical(&self) -> &[u8] {
        &self.physical
    }
    pub(crate) fn logical(&self) -> &[u8] {
        &self.physical[..self.logical_len]
    }
    pub(crate) const fn contains_null(&self) -> bool {
        self.contains_null
    }
    pub(crate) const fn rowid(&self) -> RowId {
        self.rowid
    }
    pub(crate) fn into_physical(self) -> Vec<u8> {
        self.physical
    }
}

#[derive(Debug)]
pub(crate) struct DecodedIndexKey {
    components: Box<[IndexScalar]>,
    rowid: RowId,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IndexScalar {
    Value(Value),
    /// UTC prefix values at the calendar edges can be one day outside the
    /// public timestamp domain. They are valid physical index data, but are
    /// deliberately not reconstructed as a user-visible timestamp.
    UtcTimestamp {
        day: i32,
        nanos: u64,
    },
}

impl DecodedIndexKey {
    pub(crate) fn values(&self) -> Option<Vec<Value>> {
        self.components
            .iter()
            .map(|component| match component {
                IndexScalar::Value(value) => Some(value.clone()),
                IndexScalar::UtcTimestamp { .. } => None,
            })
            .collect()
    }
    pub(crate) fn components(&self) -> &[IndexScalar] {
        &self.components
    }
    pub(crate) const fn rowid(&self) -> RowId {
        self.rowid
    }
}

pub(crate) struct IndexKeyCodec;

impl IndexKeyCodec {
    /// Whether a query prefix fits the bounded stored-key encoder. An oversized
    /// comparison operand is legal SQL, so report it separately from errors.
    /// This does not change encode/encode_prefix's persisted-key size contract.
    pub(crate) fn prefix_is_representable(
        columns: &[ColumnDef],
        values: &[&Value],
    ) -> Result<bool> {
        if columns.is_empty() || columns.len() != values.len() {
            return Err(DbError::new(
                ErrorCode::Type,
                "index prefix requires one value per column",
            ));
        }
        let limit = MAX_INDEX_KEY_BYTES - ROWID_BYTES;
        let mut prefix = Vec::with_capacity(INDEX_KEY_BUFFER_BYTES);
        for (column, value) in columns.iter().zip(values) {
            if matches!(value, Value::Null) {
                prefix.push(1);
            } else {
                prefix.push(0);
                let bytes = match (column.column_type(), value) {
                    (ColumnType::Text, Value::Text(value)) => Some(value.as_bytes()),
                    (ColumnType::Blob, Value::Blob(value)) => Some(value.as_slice()),
                    _ => None,
                };
                if let Some(bytes) = bytes {
                    // Check before allocating/encoding the payload, including
                    // zero-byte escaping and the two-byte component terminator.
                    if bytes.len() > limit {
                        return Ok(false);
                    }
                    let encoded_len =
                        bytes.len() + bytes.iter().filter(|byte| **byte == 0).count() + 2;
                    if encoded_len > limit.saturating_sub(prefix.len()) {
                        return Ok(false);
                    }
                }
                encode_scalar(column, value, &mut prefix)?;
            }
            if prefix.len() > limit {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn encode_prefix(columns: &[ColumnDef], values: &[Value]) -> Result<Vec<u8>> {
        if columns.is_empty() {
            return Err(DbError::new(
                ErrorCode::Type,
                "index key prefix requires at least one column",
            ));
        }
        let rowid = RowId::new(1).map_err(|_| corruption("index probe ROWID is invalid"))?;
        let encoded = Self::encode(columns, values, rowid)?;
        Ok(encoded.logical().to_vec())
    }

    pub(crate) fn encode(
        columns: &[ColumnDef],
        values: &[Value],
        rowid: RowId,
    ) -> Result<EncodedIndexKey> {
        if columns.len() != values.len() {
            return Err(DbError::new(
                ErrorCode::Type,
                "index key requires one value per column",
            ));
        }
        Self::encode_borrowed(columns, values.iter().map(Ok), rowid)
    }

    pub(crate) fn encode_borrowed<'a>(
        columns: &[ColumnDef],
        values: impl IntoIterator<Item = Result<&'a Value>>,
        rowid: RowId,
    ) -> Result<EncodedIndexKey> {
        let mut values = values.into_iter();
        let mut physical = Vec::with_capacity(INDEX_KEY_BUFFER_BYTES);
        let mut contains_null = false;
        for column in columns {
            let value = values.next().ok_or_else(|| {
                DbError::new(ErrorCode::Type, "index key requires one value per column")
            })??;
            if matches!(value, Value::Null) {
                physical.push(1);
                contains_null = true;
            } else {
                physical.push(0);
                encode_scalar(column, value, &mut physical)?;
            }
            check_encoded_size(physical.len())?;
        }
        if values.next().is_some() {
            return Err(DbError::new(
                ErrorCode::Type,
                "index key requires one value per column",
            ));
        }
        let logical_len = physical.len();
        physical.extend_from_slice(&rowid.get().to_be_bytes());
        Ok(EncodedIndexKey {
            physical,
            logical_len,
            contains_null,
            rowid,
        })
    }

    pub(crate) fn decode(columns: &[ColumnDef], bytes: &[u8]) -> Result<DecodedIndexKey> {
        if !(ROWID_BYTES..=MAX_INDEX_KEY_BYTES).contains(&bytes.len()) {
            return Err(corruption("invalid index key length"));
        }
        // Keep the suffix outside the component reader so a malformed payload
        // cannot consume ROWID bytes as its terminator or fixed-width fields.
        let logical_len = bytes.len() - ROWID_BYTES;
        let mut reader = Reader {
            bytes: &bytes[..logical_len],
        };
        let mut components = Vec::with_capacity(columns.len());
        for column in columns {
            components.push(match reader.byte()? {
                0 => decode_scalar(column, &mut reader)?,
                1 => IndexScalar::Value(Value::Null),
                _ => return Err(corruption("invalid index key NULL flag")),
            });
        }
        if !reader.bytes.is_empty() {
            return Err(corruption("trailing index key component bytes"));
        }
        let mut suffix = Reader {
            bytes: &bytes[logical_len..],
        };
        let rowid = RowId::new(u64::from_be_bytes(suffix.array()?))
            .map_err(|_| corruption("index key ROWID must be nonzero"))?;
        Ok(DecodedIndexKey {
            components: components.into_boxed_slice(),
            rowid,
        })
    }

    pub(crate) fn prefix_bounds(logical: &[u8]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
        (
            Bound::Included(logical.to_vec()),
            Self::prefix_successor(logical).map_or(Bound::Unbounded, Bound::Excluded),
        )
    }

    /// The least byte prefix strictly beyond every extension of `logical`.
    /// None means no finite successor; callers choose empty vs. unbounded
    /// according to whether this is a strict lower or inclusive upper fence.
    pub(crate) fn prefix_successor(logical: &[u8]) -> Option<Vec<u8>> {
        let mut upper = logical.to_vec();
        while let Some(last) = upper.pop() {
            if let Some(next) = last.checked_add(1) {
                upper.push(next);
                return Some(upper);
            }
        }
        None
    }

    // Preserve the standard lower/upper Bound pair shared with prefix_bounds.
    #[allow(clippy::type_complexity)]
    pub(crate) fn checked_prefix_bounds(
        logical: &[u8],
    ) -> Result<(Bound<Vec<u8>>, Bound<Vec<u8>>)> {
        if logical.is_empty() {
            return Err(corruption("index logical key prefix is empty"));
        }
        let bounds = Self::prefix_bounds(logical);
        if matches!(bounds.1, Bound::Unbounded) {
            return Err(corruption("index logical key prefix has no upper bound"));
        }
        Ok(bounds)
    }
}

fn check_encoded_size(logical_len: usize) -> Result<()> {
    if logical_len > MAX_INDEX_KEY_BYTES - ROWID_BYTES {
        return Err(DbError::new(
            ErrorCode::ResourceLimit,
            "index key exceeds 1024 bytes",
        ));
    }
    Ok(())
}

fn encode_scalar(column: &ColumnDef, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match (column.oracle_type(), column.column_type(), value) {
        (
            Some(OracleTypeDescriptor::Timestamp { .. }),
            ColumnType::Timestamp,
            Value::Timestamp(timestamp),
        ) if timestamp.offset_minutes().is_none() => {
            encode_date(timestamp.date(), out);
            out.extend_from_slice(&timestamp.nanoseconds().to_be_bytes());
            return Ok(());
        }
        (
            Some(OracleTypeDescriptor::TimestampWithTimeZone { .. }),
            ColumnType::TimestampWithTimeZone,
            Value::Timestamp(timestamp),
        ) if timestamp.offset_minutes().is_some() => {
            let (day, nanos) = timestamp.utc_day_and_nanos()?;
            out.extend_from_slice(&((day as u32) ^ 0x8000_0000).to_be_bytes());
            out.extend_from_slice(&nanos.to_be_bytes());
            return Ok(());
        }
        _ => {}
    }
    encode_v1_scalar(column.column_type(), value, out)
}

fn encode_v1_scalar(ty: ColumnType, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match (ty, value) {
        (ColumnType::Int64, Value::Int64(value)) => {
            out.extend_from_slice(&(*value as u64 ^ (1_u64 << 63)).to_be_bytes())
        }
        (ColumnType::Decimal, Value::Decimal(value)) => encode_decimal(*value, out)?,
        (ColumnType::Float64, Value::Float64(value)) => {
            if !value.is_finite() {
                return Err(DbError::new(
                    ErrorCode::Type,
                    "indexed Float64 must be finite",
                ));
            }
            let bits = if *value == 0.0 { 0 } else { value.to_bits() };
            let ordered = if bits >> 63 == 1 {
                !bits
            } else {
                bits ^ (1_u64 << 63)
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        (ColumnType::Text, Value::Text(value)) => encode_escaped(value.as_bytes(), out)?,
        (ColumnType::Blob, Value::Blob(value)) => encode_escaped(value, out)?,
        (ColumnType::Date, Value::Date(value)) => encode_date(*value, out),
        (ColumnType::Timestamp | ColumnType::TimestampWithTimeZone, Value::Timestamp(value))
            if value.offset_minutes().is_some() == (ty == ColumnType::TimestampWithTimeZone) =>
        {
            encode_date(value.date(), out);
            out.extend_from_slice(&value.micros().to_be_bytes());
            if let Some(offset) = value.offset_minutes() {
                out.extend_from_slice(&(offset as u16 ^ 0x8000).to_be_bytes());
            }
        }
        (ColumnType::Bool, Value::Bool(value)) => out.push(u8::from(*value)),
        _ => {
            return Err(DbError::new(
                ErrorCode::Type,
                "index value does not match column type",
            ));
        }
    }
    Ok(())
}

fn encode_escaped(bytes: &[u8], out: &mut Vec<u8>) -> Result<()> {
    for byte in bytes {
        if *byte == 0 {
            out.extend_from_slice(&[0, 255]);
        } else {
            out.push(*byte);
        }
        check_encoded_size(out.len())?;
    }
    out.extend_from_slice(&[0, 0]);
    Ok(())
}

fn encode_date(date: OracleDate, out: &mut Vec<u8>) {
    out.extend_from_slice(&date.year().to_be_bytes());
    out.extend_from_slice(&[
        date.month(),
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
    ]);
}

fn encode_decimal(value: Decimal, out: &mut Vec<u8>) -> Result<()> {
    let mut magnitude = value.magnitude();
    if magnitude == 0 {
        out.push(1);
        return Ok(());
    }
    // The wire's canonical scale can be below Decimal's public -38 bound.
    let mut scale = i16::from(value.scale());
    while magnitude % 10 == 0 {
        magnitude /= 10;
        scale -= 1;
    }
    let digits = magnitude.to_string();
    let exponent = i16::try_from(digits.len())
        .ok()
        .and_then(|len| len.checked_sub(scale))
        .filter(|exponent| (-37..=76).contains(exponent))
        .ok_or_else(|| DbError::new(ErrorCode::Type, "invalid indexed Decimal exponent"))?;
    out.push(if value.negative() { 0 } else { 2 });
    let tail_start = out.len();
    out.extend_from_slice(&(exponent as u16 ^ 0x8000).to_be_bytes());
    out.extend(digits.bytes().map(|byte| byte - b'0' + 1));
    out.push(0);
    if value.negative() {
        for byte in &mut out[tail_start..] {
            *byte = !*byte;
        }
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self
            .bytes
            .get(..N)
            .ok_or_else(|| corruption("truncated index key component"))?;
        let mut array = [0; N];
        array.copy_from_slice(slice);
        self.bytes = &self.bytes[N..];
        Ok(array)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn escaped(&mut self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            match self.byte()? {
                0 => match self.byte()? {
                    0 => return Ok(bytes),
                    255 => bytes.push(0),
                    _ => return Err(corruption("invalid index key escape")),
                },
                byte => bytes.push(byte),
            }
        }
    }

    fn date(&mut self) -> Result<OracleDate> {
        let year = u16::from_be_bytes(self.array()?);
        OracleDate::new(
            year,
            self.byte()?,
            self.byte()?,
            self.byte()?,
            self.byte()?,
            self.byte()?,
        )
        .map_err(|_| corruption("invalid indexed Date fields"))
    }
}

fn decode_scalar(column: &ColumnDef, reader: &mut Reader<'_>) -> Result<IndexScalar> {
    match (column.oracle_type(), column.column_type()) {
        (Some(OracleTypeDescriptor::Timestamp { .. }), ColumnType::Timestamp) => {
            let date = reader.date()?;
            let nanos = u32::from_be_bytes(reader.array()?);
            return Ok(IndexScalar::Value(Value::Timestamp(
                OracleTimestamp::from_nanoseconds(date, nanos, None)
                    .map_err(|_| corruption("invalid indexed Timestamp fields"))?,
            )));
        }
        (
            Some(OracleTypeDescriptor::TimestampWithTimeZone { .. }),
            ColumnType::TimestampWithTimeZone,
        ) => {
            let day = (u32::from_be_bytes(reader.array::<4>()?) ^ 0x8000_0000) as i32;
            let nanos = u64::from_be_bytes(reader.array()?);
            if !(-1..=3_652_059).contains(&day) || nanos >= 86_400_000_000_000 {
                return Err(corruption("invalid indexed UTC Timestamp fields"));
            }
            return Ok(IndexScalar::UtcTimestamp { day, nanos });
        }
        _ => {}
    }
    let ty = column.column_type();
    Ok(IndexScalar::Value(match ty {
        ColumnType::Int64 => {
            Value::Int64((u64::from_be_bytes(reader.array()?) ^ (1_u64 << 63)) as i64)
        }
        ColumnType::Decimal => Value::Decimal(decode_decimal(reader)?),
        ColumnType::Float64 => {
            let ordered = u64::from_be_bytes(reader.array()?);
            let bits = if ordered >> 63 == 1 {
                ordered ^ (1_u64 << 63)
            } else {
                !ordered
            };
            let value = f64::from_bits(bits);
            if !value.is_finite() || bits == (1_u64 << 63) {
                return Err(corruption("noncanonical indexed Float64"));
            }
            Value::Float64(value)
        }
        ColumnType::Text => Value::Text(
            String::from_utf8(reader.escaped()?)
                .map_err(|_| corruption("invalid indexed UTF-8"))?,
        ),
        ColumnType::Blob => Value::Blob(reader.escaped()?),
        ColumnType::Date => Value::Date(reader.date()?),
        ColumnType::Timestamp | ColumnType::TimestampWithTimeZone => {
            let date = reader.date()?;
            let micros = u32::from_be_bytes(reader.array()?);
            let offset = if ty == ColumnType::TimestampWithTimeZone {
                Some((u16::from_be_bytes(reader.array()?) ^ 0x8000) as i16)
            } else {
                None
            };
            Value::Timestamp(
                OracleTimestamp::new(date, micros, offset)
                    .map_err(|_| corruption("invalid indexed Timestamp fields"))?,
            )
        }
        ColumnType::Bool => Value::Bool(match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err(corruption("invalid indexed Bool")),
        }),
    }))
}

fn decode_decimal(reader: &mut Reader<'_>) -> Result<Decimal> {
    let negative = match reader.byte()? {
        0 => true,
        1 => return Decimal::new(false, 0, 0),
        2 => false,
        _ => return Err(corruption("invalid indexed Decimal sign")),
    };
    let undo_sign = |byte: u8| if negative { !byte } else { byte };
    let encoded_exponent = reader.array::<2>()?.map(undo_sign);
    let exponent = (u16::from_be_bytes(encoded_exponent) ^ 0x8000) as i16;
    if !(-37..=76).contains(&exponent) {
        return Err(corruption("invalid indexed Decimal exponent"));
    }
    let mut magnitude = 0_u128;
    let mut digit_count = 0_i16;
    let mut last_digit = 0;
    loop {
        let byte = undo_sign(reader.byte()?);
        if byte == 0 {
            break;
        }
        if !(1..=10).contains(&byte) || digit_count == 38 || (digit_count == 0 && byte == 1) {
            return Err(corruption("invalid indexed Decimal digit"));
        }
        last_digit = byte - 1;
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|m| m.checked_add(u128::from(last_digit)))
            .ok_or_else(|| corruption("indexed Decimal magnitude overflow"))?;
        digit_count += 1;
    }
    if digit_count == 0 || last_digit == 0 {
        return Err(corruption("noncanonical indexed Decimal digits"));
    }
    let mut scale = digit_count
        .checked_sub(exponent)
        .ok_or_else(|| corruption("indexed Decimal scale overflow"))?;
    // Restore only the zeros required for a representable public Decimal.
    // Re-encoding removes them again, preserving a single canonical wire form.
    while scale < -38 {
        magnitude = magnitude
            .checked_mul(10)
            .ok_or_else(|| corruption("indexed Decimal magnitude overflow"))?;
        scale += 1;
    }
    let scale = i8::try_from(scale).map_err(|_| corruption("invalid indexed Decimal scale"))?;
    Decimal::new(negative, scale, magnitude)
        .map_err(|_| corruption("indexed Decimal outside value domain"))
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::exec::sql_total_cmp;
    use crate::storage::catalog::types::{
        CatalogName, ColumnDef, ColumnId, ColumnType, RowId, TableId,
    };
    use crate::{
        Decimal, ErrorCode, OracleDate, OracleTimestamp, Value, oracle_type::OracleTypeDescriptor,
    };
    use std::ops::Bound;

    fn columns(types: &[ColumnType]) -> Vec<ColumnDef> {
        types
            .iter()
            .enumerate()
            .map(|(ordinal, ty)| {
                ColumnDef::new(
                    ColumnId::new(ordinal as u64 + 1).unwrap(),
                    TableId::new(1).unwrap(),
                    ordinal as u16,
                    *ty,
                    true,
                    CatalogName::new(format!("C{ordinal}")).unwrap(),
                )
                .unwrap()
            })
            .collect()
    }

    fn encode_for(ty: ColumnType, value: &Value, rowid: u64) -> crate::Result<EncodedIndexKey> {
        IndexKeyCodec::encode(
            &columns(&[ty]),
            std::slice::from_ref(value),
            RowId::new(rowid).unwrap(),
        )
    }

    fn oracle_timestamp_column(descriptor: OracleTypeDescriptor) -> ColumnDef {
        ColumnDef::new_oracle(
            ColumnId::new(1).unwrap(),
            TableId::new(1).unwrap(),
            0,
            true,
            CatalogName::new("TS").unwrap(),
            descriptor,
        )
        .unwrap()
    }

    #[test]
    fn v2_timestamp_keys_keep_nanoseconds_and_tstz_uses_utc_prefixes() {
        let local = oracle_timestamp_column(OracleTypeDescriptor::timestamp(9).unwrap());
        for nanos in [0, 999_999_999] {
            let value = Value::Timestamp(
                OracleTimestamp::from_nanoseconds(date(2026, 1, 1, 0, 0, 0), nanos, None).unwrap(),
            );
            let encoded =
                IndexKeyCodec::encode(&[local.clone()], &[value.clone()], RowId::new(1).unwrap())
                    .unwrap();
            assert_eq!(encoded.logical().len(), 1 + 7 + 4);
            assert_eq!(
                IndexKeyCodec::decode(&[local.clone()], encoded.physical())
                    .unwrap()
                    .values(),
                Some(vec![value]),
            );
        }

        let zoned =
            oracle_timestamp_column(OracleTypeDescriptor::timestamp_with_time_zone(9).unwrap());
        let plus_eight = Value::Timestamp(
            OracleTimestamp::from_nanoseconds(date(2026, 1, 1, 8, 0, 0), 123_456_789, Some(480))
                .unwrap(),
        );
        let utc = Value::Timestamp(
            OracleTimestamp::from_nanoseconds(date(2026, 1, 1, 0, 0, 0), 123_456_789, Some(0))
                .unwrap(),
        );
        let left =
            IndexKeyCodec::encode(&[zoned.clone()], &[plus_eight], RowId::new(1).unwrap()).unwrap();
        let right =
            IndexKeyCodec::encode(&[zoned.clone()], &[utc], RowId::new(2).unwrap()).unwrap();
        assert_eq!(left.logical(), right.logical());
        assert_eq!(
            IndexKeyCodec::decode(&[zoned], left.physical())
                .unwrap()
                .components(),
            &[IndexScalar::UtcTimestamp {
                day: 739_616,
                nanos: 123_456_789
            }],
        );
    }

    #[test]
    fn v2_tstz_edge_days_and_corruption_checks_are_strict() {
        let zoned =
            oracle_timestamp_column(OracleTypeDescriptor::timestamp_with_time_zone(9).unwrap());
        let earliest = Value::Timestamp(
            OracleTimestamp::from_nanoseconds(date(1, 1, 1, 0, 0, 0), 0, Some(840)).unwrap(),
        );
        let latest = Value::Timestamp(
            OracleTimestamp::from_nanoseconds(
                date(9999, 12, 31, 23, 59, 59),
                999_999_999,
                Some(-840),
            )
            .unwrap(),
        );
        for (value, component) in [
            (
                earliest,
                IndexScalar::UtcTimestamp {
                    day: -1,
                    nanos: 36_000_000_000_000,
                },
            ),
            (
                latest,
                IndexScalar::UtcTimestamp {
                    day: 3_652_059,
                    nanos: 50_399_999_999_999,
                },
            ),
        ] {
            let encoded =
                IndexKeyCodec::encode(&[zoned.clone()], &[value], RowId::new(1).unwrap()).unwrap();
            assert_eq!(
                IndexKeyCodec::decode(&[zoned.clone()], encoded.physical())
                    .unwrap()
                    .components(),
                &[component]
            );
            for length in 0..encoded.physical().len() {
                assert_eq!(
                    IndexKeyCodec::decode(&[zoned.clone()], &encoded.physical()[..length])
                        .unwrap_err()
                        .code(),
                    ErrorCode::Corruption,
                );
            }
        }
        let valid = IndexKeyCodec::encode(
            &[zoned.clone()],
            &[Value::Timestamp(
                OracleTimestamp::from_nanoseconds(date(2026, 1, 1, 0, 0, 0), 0, Some(0)).unwrap(),
            )],
            RowId::new(1).unwrap(),
        )
        .unwrap();
        for mutation in [
            (1..5, [0, 0, 0, 0].as_slice()),
            (5..13, [255; 8].as_slice()),
        ] {
            let mut bytes = valid.physical().to_vec();
            bytes[mutation.0].copy_from_slice(mutation.1);
            assert_eq!(
                IndexKeyCodec::decode(&[zoned.clone()], &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn index_key_limit_accepts_1024_bytes_and_rejects_1025() {
        let column = columns(&[ColumnType::Text]);
        assert_eq!(
            IndexKeyCodec::encode(
                &column,
                &[Value::Text("x".repeat(1013))],
                RowId::new(1).unwrap()
            )
            .unwrap()
            .physical()
            .len(),
            MAX_INDEX_KEY_BYTES,
        );
        assert_eq!(
            IndexKeyCodec::encode(
                &column,
                &[Value::Text("x".repeat(1014))],
                RowId::new(1).unwrap()
            )
            .unwrap_err()
            .code(),
            ErrorCode::ResourceLimit,
        );
    }

    fn decimal(negative: bool, scale: i8, magnitude: u128) -> Value {
        Value::Decimal(Decimal::new(negative, scale, magnitude).unwrap())
    }

    #[test]
    fn logical_prefix_encoding_checks_arity_nonempty_and_eventual_rowid_bound() {
        // Production breaks caught: an empty prefix makes an unbounded probe,
        // mismatched arity changes component meaning, and omitting the eventual
        // ROWID allowance admits physical keys beyond the 1024-byte contract.
        let cols = columns(&[ColumnType::Int64, ColumnType::Text]);
        let prefix = IndexKeyCodec::encode_prefix(&cols[..1], &[Value::Int64(7)]).unwrap();
        let complete =
            IndexKeyCodec::encode(&cols[..1], &[Value::Int64(7)], RowId::new(1).unwrap()).unwrap();
        assert_eq!(prefix, complete.logical());
        assert_eq!(
            IndexKeyCodec::encode_prefix(&[], &[]).unwrap_err().code(),
            ErrorCode::Type
        );
        assert_eq!(
            IndexKeyCodec::encode_prefix(&cols, &[Value::Int64(7)])
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(
            IndexKeyCodec::encode_prefix(
                &columns(&[ColumnType::Text]),
                &[Value::Text("x".repeat(MAX_INDEX_KEY_BYTES))],
            )
            .unwrap_err()
            .code(),
            ErrorCode::ResourceLimit
        );
    }

    fn date(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> OracleDate {
        OracleDate::new(year, month, day, hour, minute, second).unwrap()
    }

    fn ordered_scalar_cases() -> Vec<(ColumnType, Vec<Value>)> {
        let first = date(1, 1, 1, 0, 0, 0);
        let last = date(9999, 12, 31, 23, 59, 59);
        let stamp = |d, us, offset| Value::Timestamp(OracleTimestamp::new(d, us, offset).unwrap());
        vec![
            (
                ColumnType::Int64,
                [i64::MIN, -1, 0, 1, i64::MAX]
                    .into_iter()
                    .map(Value::Int64)
                    .collect(),
            ),
            (
                ColumnType::Decimal,
                vec![
                    decimal(
                        true,
                        -38,
                        99_999_999_999_999_999_999_999_999_999_999_999_999,
                    ),
                    decimal(true, -38, 10),
                    decimal(true, -38, 1),
                    decimal(true, 0, 12),
                    decimal(true, 1, 12),
                    decimal(true, 0, 1),
                    decimal(true, 38, 1),
                    decimal(false, 38, 0),
                    decimal(false, 38, 1),
                    decimal(false, 0, 1),
                    decimal(false, 1, 12),
                    decimal(false, 0, 12),
                    decimal(false, -38, 1),
                    decimal(false, -38, 10),
                    decimal(
                        false,
                        -38,
                        99_999_999_999_999_999_999_999_999_999_999_999_999,
                    ),
                ],
            ),
            (
                ColumnType::Float64,
                [
                    -f64::MAX,
                    -1.0,
                    -f64::MIN_POSITIVE,
                    -f64::from_bits(1),
                    0.0,
                    f64::from_bits(1),
                    f64::MIN_POSITIVE,
                    1.0,
                    f64::MAX,
                ]
                .into_iter()
                .map(Value::Float64)
                .collect(),
            ),
            (
                ColumnType::Text,
                ["", "\0", "\0a", "a", "a\0", "aa", "é", "中", "😀"]
                    .into_iter()
                    .map(|s| Value::Text(s.into()))
                    .collect(),
            ),
            (
                ColumnType::Blob,
                vec![
                    vec![],
                    vec![0],
                    vec![0, 0],
                    vec![0, 1],
                    vec![1],
                    vec![255],
                    vec![255, 0],
                ]
                .into_iter()
                .map(Value::Blob)
                .collect(),
            ),
            (
                ColumnType::Date,
                vec![
                    first,
                    date(2024, 2, 29, 0, 0, 0),
                    date(2024, 3, 1, 0, 0, 0),
                    date(2024, 3, 1, 0, 0, 1),
                    date(2024, 3, 1, 0, 1, 0),
                    date(2024, 3, 1, 1, 0, 0),
                    last,
                ]
                .into_iter()
                .map(Value::Date)
                .collect(),
            ),
            (
                ColumnType::Timestamp,
                vec![
                    stamp(first, 0, None),
                    stamp(first, 1, None),
                    stamp(first, 999_999, None),
                    stamp(last, 0, None),
                ],
            ),
            (
                ColumnType::TimestampWithTimeZone,
                vec![
                    stamp(first, 0, Some(-840)),
                    stamp(first, 0, Some(-1)),
                    stamp(first, 0, Some(0)),
                    stamp(first, 0, Some(840)),
                    stamp(first, 1, Some(-840)),
                    stamp(last, 999_999, Some(840)),
                ],
            ),
            (
                ColumnType::Bool,
                vec![Value::Bool(false), Value::Bool(true)],
            ),
        ]
    }

    #[test]
    fn index_key_byte_order_matches_sql_total_order() {
        for (ty, mut values) in ordered_scalar_cases() {
            for pair in values.windows(2) {
                assert_eq!(
                    sql_total_cmp(&pair[0], &pair[1]).unwrap(),
                    std::cmp::Ordering::Less
                );
            }
            // IndexKey V1 explicitly places NULL last, independently of SQL Sort.
            values.push(Value::Null);
            for pair in values.windows(2) {
                assert!(
                    encode_for(ty, &pair[0], 1).unwrap().physical()
                        < encode_for(ty, &pair[1], 1).unwrap().physical(),
                    "{ty:?}: {pair:?}"
                );
            }
        }
    }

    #[test]
    fn decimal_equal_values_and_signed_zero_have_one_encoding() {
        for negative in [false, true] {
            for (left, right) in [
                (decimal(negative, 1, 10), decimal(negative, 0, 1)),
                (decimal(negative, -37, 10), decimal(negative, -38, 1)),
                (decimal(negative, -37, 100), decimal(negative, -38, 10)),
                (decimal(negative, 38, 0), decimal(negative, -38, 0)),
            ] {
                assert_eq!(
                    encode_for(ColumnType::Decimal, &left, 1)
                        .unwrap()
                        .physical(),
                    encode_for(ColumnType::Decimal, &right, 1)
                        .unwrap()
                        .physical()
                );
            }
        }
        assert_eq!(
            encode_for(ColumnType::Float64, &Value::Float64(-0.0), 1)
                .unwrap()
                .physical(),
            encode_for(ColumnType::Float64, &Value::Float64(0.0), 1)
                .unwrap()
                .physical()
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                encode_for(ColumnType::Float64, &Value::Float64(bad), 1)
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
        }
    }

    #[test]
    fn extreme_decimal_scales_round_trip_to_representable_canonical_values() {
        for negative in [false, true] {
            for scale in [-38, 38] {
                for magnitude in [
                    0,
                    1,
                    10,
                    100,
                    10_u128.pow(37),
                    99_999_999_999_999_999_999_999_999_999_999_999_990,
                ] {
                    let value = decimal(negative, scale, magnitude);
                    let encoded = encode_for(ColumnType::Decimal, &value, 1).unwrap();
                    let cols = columns(&[ColumnType::Decimal]);
                    let decoded = IndexKeyCodec::decode(&cols, encoded.physical()).unwrap();
                    let values = decoded.values().expect("Decimal is reconstructible");
                    assert_eq!(values, [canonical(&value)]);
                    assert_eq!(
                        sql_total_cmp(&value, &values[0]).unwrap(),
                        std::cmp::Ordering::Equal
                    );
                    assert_eq!(
                        IndexKeyCodec::encode(&cols, &values, decoded.rowid())
                            .unwrap()
                            .physical(),
                        encoded.physical()
                    );
                }
            }
        }
    }

    #[test]
    fn composite_order_null_markers_and_rowid_tiebreak_are_stable() {
        let cols = columns(&[ColumnType::Text, ColumnType::Int64]);
        let cases = [
            (vec![Value::Text("a".into()), Value::Int64(1)], 1),
            (vec![Value::Text("a".into()), Value::Int64(1)], 256),
            (vec![Value::Text("a".into()), Value::Int64(1)], u64::MAX),
            (vec![Value::Text("a".into()), Value::Null], 1),
            (vec![Value::Text("a\0".into()), Value::Int64(i64::MIN)], 1),
            (vec![Value::Null, Value::Int64(i64::MIN)], 1),
            (vec![Value::Null, Value::Null], 1),
        ];
        let encoded: Vec<_> = cases
            .iter()
            .map(|(values, rowid)| {
                IndexKeyCodec::encode(&cols, values, RowId::new(*rowid).unwrap()).unwrap()
            })
            .collect();
        assert_eq!(encoded[0].logical(), encoded[2].logical());
        for pair in encoded.windows(2) {
            assert!(pair[0].physical() < pair[1].physical());
        }
        for ((values, rowid), key) in cases.iter().zip(encoded) {
            assert_eq!(key.contains_null(), values.contains(&Value::Null));
            let decoded = IndexKeyCodec::decode(&cols, key.physical()).unwrap();
            assert_eq!(decoded.values().expect("V1 components"), *values);
            assert_eq!(decoded.rowid().get(), *rowid);
        }
    }

    #[test]
    fn scalar_wire_fixtures_and_rowid_suffix_are_exact() {
        let fixtures = [
            (ColumnType::Int64, Value::Int64(i64::MIN), vec![0; 9]),
            (
                ColumnType::Int64,
                Value::Int64(0),
                vec![0, 128, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                ColumnType::Decimal,
                decimal(false, 0, 1),
                vec![0, 2, 128, 1, 2, 0],
            ),
            (
                ColumnType::Decimal,
                decimal(true, 0, 1),
                vec![0, 0, 127, 254, 253, 255],
            ),
            (ColumnType::Decimal, decimal(false, -38, 0), vec![0, 1]),
            (
                ColumnType::Float64,
                Value::Float64(-0.0),
                vec![0, 128, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                ColumnType::Text,
                Value::Text("a\0中".into()),
                vec![0, 97, 0, 255, 228, 184, 173, 0, 0],
            ),
            (
                ColumnType::Blob,
                Value::Blob(vec![0, 255]),
                vec![0, 0, 255, 255, 0, 0],
            ),
            (
                ColumnType::Date,
                Value::Date(date(2024, 2, 29, 23, 59, 58)),
                vec![0, 7, 232, 2, 29, 23, 59, 58],
            ),
            (
                ColumnType::TimestampWithTimeZone,
                Value::Timestamp(
                    OracleTimestamp::new(date(1, 1, 1, 0, 0, 0), 1, Some(-1)).unwrap(),
                ),
                vec![0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 127, 255],
            ),
            (ColumnType::Bool, Value::Bool(true), vec![0, 1]),
            (ColumnType::Bool, Value::Null, vec![1]),
        ];
        for (ty, value, logical) in fixtures {
            let encoded = encode_for(ty, &value, 0x0102_0304_0506_0708).unwrap();
            assert_eq!(encoded.logical(), logical);
            assert_eq!(
                &encoded.physical()[logical.len()..],
                &[1, 2, 3, 4, 5, 6, 7, 8]
            );
            assert_eq!(encoded.rowid().get(), 0x0102_0304_0506_0708);
            assert_eq!(encoded.contains_null(), matches!(value, Value::Null));
        }
    }

    #[test]
    fn composite_text_boundaries_and_key_limit_are_unambiguous() {
        let cols = columns(&[ColumnType::Text, ColumnType::Text]);
        let a = IndexKeyCodec::encode(
            &cols,
            &[Value::Text("a".into()), Value::Text("b".into())],
            RowId::new(1).unwrap(),
        )
        .unwrap();
        let b = IndexKeyCodec::encode(
            &cols,
            &[Value::Text("a\0".into()), Value::Text("b".into())],
            RowId::new(1).unwrap(),
        )
        .unwrap();
        assert!(a.physical() < b.physical());
        assert_eq!(
            IndexKeyCodec::decode(&cols, a.physical())
                .unwrap()
                .rowid()
                .get(),
            1
        );
        let exact = encode_for(
            ColumnType::Text,
            &Value::Text("x".repeat(MAX_INDEX_KEY_BYTES - 11)),
            1,
        )
        .unwrap();
        assert_eq!(exact.physical().len(), MAX_INDEX_KEY_BYTES);
        assert!(IndexKeyCodec::decode(&columns(&[ColumnType::Text]), exact.physical()).is_ok());
        assert_eq!(
            encode_for(
                ColumnType::Text,
                &Value::Text("x".repeat(MAX_INDEX_KEY_BYTES - 10)),
                1
            )
            .unwrap_err()
            .code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            encode_for(ColumnType::Blob, &Value::Blob(vec![0; 507]), 1)
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        let mut oversized = exact.physical().to_vec();
        oversized.insert(1, b'x');
        assert_corruption(ColumnType::Text, &oversized);
    }

    #[test]
    fn prefix_successor_carries_and_reports_overflow_without_wrapping() {
        assert_eq!(IndexKeyCodec::prefix_successor(&[1, 5]), Some(vec![1, 6]));
        assert_eq!(
            IndexKeyCodec::prefix_successor(&[0, 255, 255]),
            Some(vec![1])
        );
        assert_eq!(IndexKeyCodec::prefix_successor(&[255, 255]), None);
        assert_eq!(IndexKeyCodec::prefix_successor(&[]), None);
    }

    #[test]
    fn prefix_bounds_cover_all_rowids_and_exclude_neighbors() {
        let value = Value::Text("a\0".into());
        let key = encode_for(ColumnType::Text, &value, 1).unwrap();
        let (lower, upper) = IndexKeyCodec::prefix_bounds(key.logical());
        assert_eq!(lower, Bound::Included(key.logical().to_vec()));
        for rowid in [1, 256, u64::MAX] {
            let encoded = encode_for(ColumnType::Text, &value, rowid).unwrap();
            assert!(within_bounds(encoded.physical(), &lower, &upper));
        }
        for other in ["a", "a\0\0", "b"] {
            assert!(!within_bounds(
                encode_for(ColumnType::Text, &Value::Text(other.into()), 1)
                    .unwrap()
                    .physical(),
                &lower,
                &upper
            ));
        }
        assert_eq!(
            IndexKeyCodec::prefix_bounds(&[1, 255]),
            (Bound::Included(vec![1, 255]), Bound::Excluded(vec![2]))
        );
        assert_eq!(
            IndexKeyCodec::prefix_bounds(&[255]),
            (Bound::Included(vec![255]), Bound::Unbounded)
        );
        assert_eq!(
            IndexKeyCodec::prefix_bounds(&[]),
            (Bound::Included(vec![]), Bound::Unbounded)
        );
    }

    fn within_bounds(bytes: &[u8], lower: &Bound<Vec<u8>>, upper: &Bound<Vec<u8>>) -> bool {
        let lower_ok = match lower {
            Bound::Included(v) => bytes >= v,
            Bound::Excluded(v) => bytes > v,
            Bound::Unbounded => true,
        };
        let upper_ok = match upper {
            Bound::Included(v) => bytes <= v,
            Bound::Excluded(v) => bytes < v,
            Bound::Unbounded => true,
        };
        lower_ok && upper_ok
    }

    #[test]
    fn input_arity_and_exact_scalar_types_are_checked() {
        let cols = columns(&[ColumnType::Int64]);
        for values in [
            vec![],
            vec![Value::Int64(1), Value::Int64(2)],
            vec![decimal(false, 0, 1)],
        ] {
            assert_eq!(
                IndexKeyCodec::encode(&cols, &values, RowId::new(1).unwrap())
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
        }
        for (ty, values) in ordered_scalar_cases() {
            for (wrong, _) in ordered_scalar_cases()
                .into_iter()
                .filter(|(other, _)| *other != ty)
            {
                assert_eq!(
                    encode_for(wrong, &values[0], 1).unwrap_err().code(),
                    ErrorCode::Type,
                    "{ty:?} as {wrong:?}"
                );
            }
        }
    }

    fn assert_corruption(ty: ColumnType, bytes: &[u8]) {
        assert_eq!(
            IndexKeyCodec::decode(&columns(&[ty]), bytes)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption,
            "{ty:?} {bytes:?}"
        );
    }

    fn with_rowid(mut component: Vec<u8>) -> Vec<u8> {
        component.extend_from_slice(&1_u64.to_be_bytes());
        component
    }

    #[test]
    fn strict_decode_rejects_malformed_scalar_encodings() {
        let malformed = [
            (ColumnType::Bool, vec![2]),
            (ColumnType::Bool, vec![0, 2]),
            (ColumnType::Text, vec![0, 255, 0, 0]),
            (ColumnType::Text, vec![0, b'a', 0, 1]),
            (ColumnType::Text, vec![0, b'a']),
            (ColumnType::Blob, vec![0, 0, 2]),
            (ColumnType::Decimal, vec![0, 3]),
            (ColumnType::Decimal, vec![0, 2, 128, 1, 0]),
            (ColumnType::Decimal, vec![0, 2, 128, 1, 1, 2, 0]),
            (ColumnType::Decimal, vec![0, 2, 128, 1, 2, 1, 0]),
            (ColumnType::Decimal, vec![0, 2, 128, 1, 11, 0]),
            (ColumnType::Decimal, vec![0, 2, 128, 77, 2, 0]),
            (ColumnType::Decimal, vec![0, 2, 127, 218, 2, 0]),
            (ColumnType::Decimal, vec![0, 2, 127, 219, 2, 2, 0]),
            (ColumnType::Date, vec![0, 0, 0, 1, 1, 0, 0, 0]),
            (ColumnType::Date, vec![0, 7, 231, 2, 29, 0, 0, 0]),
        ];
        for (ty, component) in malformed {
            if ty == ColumnType::Decimal && component.get(1) == Some(&2) {
                let mut negative = component.clone();
                negative[1] = 0;
                for byte in &mut negative[2..] {
                    *byte = !*byte;
                }
                assert_corruption(ty, &with_rowid(negative));
            }
            assert_corruption(ty, &with_rowid(component));
        }
        let mut too_many_digits = vec![0, 2, 128, 38];
        too_many_digits.extend_from_slice(&[2; 39]);
        too_many_digits.push(0);
        assert_corruption(ColumnType::Decimal, &with_rowid(too_many_digits));
        for bits in [
            0x7ff0_0000_0000_0000_u64,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0001,
            0x8000_0000_0000_0000,
        ] {
            let ordered = if bits >> 63 != 0 {
                !bits
            } else {
                bits ^ (1 << 63)
            };
            let mut component = vec![0];
            component.extend_from_slice(&ordered.to_be_bytes());
            assert_corruption(ColumnType::Float64, &with_rowid(component));
        }
        for (ty, offset) in [
            (ColumnType::Timestamp, None),
            (ColumnType::TimestampWithTimeZone, Some(0)),
        ] {
            let timestamp = Value::Timestamp(
                OracleTimestamp::new(date(2024, 1, 1, 0, 0, 0), 0, offset).unwrap(),
            );
            let mut bytes = encode_for(ty, &timestamp, 1).unwrap().physical().to_vec();
            bytes[8..12].copy_from_slice(&1_000_000_u32.to_be_bytes());
            assert_corruption(ty, &bytes);
            if offset.is_some() {
                bytes[8..12].fill(0);
                bytes[12..14].copy_from_slice(&(841_u16 ^ 0x8000).to_be_bytes());
                assert_corruption(ty, &bytes);
            }
        }
    }

    #[test]
    fn every_scalar_and_composite_truncation_and_strict_eof_are_rejected() {
        let mut composite_types = Vec::new();
        let mut composite_values = Vec::new();
        for (ty, mut values) in ordered_scalar_cases() {
            values.push(Value::Null);
            for value in &values {
                let key = encode_for(ty, value, 1).unwrap();
                for end in 0..key.physical().len() {
                    assert_corruption(ty, &key.physical()[..end]);
                }
                let mut trailing = key.physical().to_vec();
                trailing.push(1);
                assert_corruption(ty, &trailing);
                let mut zero_rowid = key.physical().to_vec();
                zero_rowid[key.logical().len()..].fill(0);
                assert_corruption(ty, &zero_rowid);
            }
            composite_types.push(ty);
            composite_values.push(values[0].clone());
        }
        let cols = columns(&composite_types);
        let key =
            IndexKeyCodec::encode(&cols, &composite_values, RowId::new(u64::MAX).unwrap()).unwrap();
        for end in 0..key.physical().len() {
            assert_eq!(
                IndexKeyCodec::decode(&cols, &key.physical()[..end])
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }
        assert_eq!(
            IndexKeyCodec::decode(&cols[..cols.len() - 1], key.physical())
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        let one = encode_for(ColumnType::Int64, &Value::Int64(1), 1).unwrap();
        assert_eq!(
            IndexKeyCodec::decode(
                &columns(&[ColumnType::Int64, ColumnType::Int64]),
                one.physical()
            )
            .unwrap_err()
            .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn scalar_byte_mutations_only_decode_to_canonical_reencodings() {
        for (ty, values) in ordered_scalar_cases() {
            let cols = columns(&[ty]);
            for value in values {
                let encoded = encode_for(ty, &value, 1).unwrap();
                let decoded = IndexKeyCodec::decode(&cols, encoded.physical()).unwrap();
                let values = decoded.values().expect("V1 components");
                assert_eq!(values, [canonical(&value)]);
                if let Value::Float64(f) = values[0] {
                    if f == 0.0 {
                        assert_eq!(f.to_bits(), 0);
                    }
                }
                for index in 0..encoded.physical().len() {
                    let mut mutated = encoded.physical().to_vec();
                    mutated[index] ^= 255;
                    match IndexKeyCodec::decode(&cols, &mutated) {
                        Ok(decoded) => assert_eq!(
                            IndexKeyCodec::encode(
                                &cols,
                                &decoded.values().expect("V1 components"),
                                decoded.rowid(),
                            )
                            .unwrap()
                            .physical(),
                            mutated
                        ),
                        Err(error) => assert_eq!(error.code(), ErrorCode::Corruption),
                    }
                }
            }
        }
    }

    struct DeterministicRng(u64);
    impl DeterministicRng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn value(&mut self, ty: ColumnType) -> Value {
            match ty {
                ColumnType::Int64 => Value::Int64(self.next() as i64),
                ColumnType::Decimal => {
                    let magnitude = ((u128::from(self.next()) << 64) | u128::from(self.next()))
                        % 100_000_000_000_000_000_000_000_000_000_000_000_000;
                    decimal(
                        self.next() & 1 != 0,
                        (self.next() % 77) as i8 - 38,
                        magnitude,
                    )
                }
                ColumnType::Float64 => {
                    let bits = self.next();
                    let bits = if bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
                        bits ^ (1 << 52)
                    } else {
                        bits
                    };
                    Value::Float64(f64::from_bits(bits))
                }
                ColumnType::Text => {
                    let alphabet = ['\0', 'a', 'z', 'é', '中', '😀'];
                    Value::Text(
                        (0..self.next() % 20)
                            .map(|_| alphabet[(self.next() % 6) as usize])
                            .collect(),
                    )
                }
                ColumnType::Blob => {
                    Value::Blob((0..self.next() % 20).map(|_| self.next() as u8).collect())
                }
                ColumnType::Date | ColumnType::Timestamp | ColumnType::TimestampWithTimeZone => {
                    let d = date(
                        (self.next() % 9999 + 1) as u16,
                        (self.next() % 12 + 1) as u8,
                        (self.next() % 28 + 1) as u8,
                        (self.next() % 24) as u8,
                        (self.next() % 60) as u8,
                        (self.next() % 60) as u8,
                    );
                    if ty == ColumnType::Date {
                        Value::Date(d)
                    } else {
                        let offset = (ty == ColumnType::TimestampWithTimeZone)
                            .then(|| (self.next() % 1681) as i16 - 840);
                        Value::Timestamp(
                            OracleTimestamp::new(d, (self.next() % 1_000_000) as u32, offset)
                                .unwrap(),
                        )
                    }
                }
                ColumnType::Bool => Value::Bool(self.next() & 1 != 0),
            }
        }
    }

    fn canonical(value: &Value) -> Value {
        match value {
            Value::Decimal(d) => {
                let (mut magnitude, mut scale) = (d.magnitude(), d.scale());
                if magnitude == 0 {
                    return decimal(false, 0, 0);
                }
                while magnitude % 10 == 0 && scale > -38 {
                    magnitude /= 10;
                    scale -= 1;
                }
                decimal(d.negative(), scale, magnitude)
            }
            Value::Float64(f) if *f == 0.0 => Value::Float64(0.0),
            _ => value.clone(),
        }
    }

    #[test]
    fn fixed_seed_encodings_preserve_comparison_and_round_trip() {
        let mut rng = DeterministicRng(0x4c2a_2026_0918);
        let types: Vec<_> = ordered_scalar_cases()
            .into_iter()
            .map(|(ty, _)| ty)
            .collect();
        for iteration in 0..20_000 {
            let ty = types[iteration % types.len()];
            let left = rng.value(ty);
            let right = rng.value(ty);
            let encoded_left = encode_for(ty, &left, 1).unwrap();
            let encoded_right = encode_for(ty, &right, 1).unwrap();
            assert_eq!(
                encoded_left.physical().cmp(encoded_right.physical()),
                sql_total_cmp(&left, &right).unwrap(),
                "{left:?} {right:?}"
            );
            for (value, encoded) in [(&left, encoded_left), (&right, encoded_right)] {
                let decoded = IndexKeyCodec::decode(&columns(&[ty]), encoded.physical()).unwrap();
                let values = decoded.values().expect("V1 components");
                assert_eq!(values, [canonical(value)]);
                assert_eq!(decoded.rowid().get(), 1);
                assert_eq!(
                    IndexKeyCodec::encode(&columns(&[ty]), &values, decoded.rowid())
                        .unwrap()
                        .physical(),
                    encoded.physical()
                );
            }
        }
    }
}
