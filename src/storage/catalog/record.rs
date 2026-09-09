use crate::{
    DbError, ErrorCode, Result,
    storage::{
        PageId, PageRef,
        codec::{Decoder, Encoder},
    },
};

use super::{
    key::RecordKind,
    types::{
        CatalogMeta, CatalogName, CatalogRecord, ColumnDef, ColumnId, ColumnType, ObjectId, RowId,
        SchemaVersion, TableDef, TableId,
    },
};

const RECORD_VERSION_V1: u8 = 1;
const CATALOG_FORMAT_VERSION_V1: u16 = 1;
const ENVELOPE_LEN: usize = 8;
const MAPPING_PAYLOAD_LEN: usize = 8;

pub(super) fn encode_catalog_meta(meta: &CatalogMeta) -> Vec<u8> {
    let mut payload = Encoder::new();
    payload.put_u16(CATALOG_FORMAT_VERSION_V1);
    payload.put_u16(0);
    payload.put_u64(meta.next_object_id().get());
    encode_envelope(RecordKind::CatalogMeta, payload.into_bytes())
}

pub(super) fn encode_table_name_mapping(id: TableId) -> Vec<u8> {
    encode_mapping(RecordKind::TableName, id.get())
}

pub(super) fn encode_table_definition(table: &TableDef) -> Vec<u8> {
    let mut payload = Encoder::new();
    payload.put_u64(table.id().get());
    payload.put_u64(table.schema_version().get());
    payload.put_u64(table.root().page_id().0);
    payload.put_u64(table.root().generation());
    payload.put_u64(table.next_rowid().get());
    payload.put_u16(table.column_count());
    put_name(&mut payload, table.name());
    encode_envelope(RecordKind::TableDefinition, payload.into_bytes())
}

pub(super) fn encode_column_name_mapping(id: ColumnId) -> Vec<u8> {
    encode_mapping(RecordKind::ColumnName, id.get())
}

pub(super) fn encode_column_ordinal_mapping(id: ColumnId) -> Vec<u8> {
    encode_mapping(RecordKind::ColumnOrdinal, id.get())
}

pub(super) fn encode_column_definition(column: &ColumnDef) -> Vec<u8> {
    let mut payload = Encoder::new();
    payload.put_u64(column.id().get());
    payload.put_u64(column.table_id().get());
    payload.put_u16(column.ordinal());
    payload.put_u8(column.column_type().code());
    payload.put_u8(u8::from(column.nullable()));
    let name_len = u16::try_from(column.name().as_bytes().len())
        .expect("checked catalog name length fits u16");
    payload.put_u16(name_len);
    payload.put_u16(0);
    payload.put_bytes(column.name().as_bytes());
    encode_envelope(RecordKind::ColumnDefinition, payload.into_bytes())
}

pub(super) fn decode_record(expected: RecordKind, bytes: &[u8]) -> Result<CatalogRecord> {
    let payload = decode_envelope(expected, bytes)?;
    match expected {
        RecordKind::CatalogMeta => {
            decode_catalog_meta_payload(payload).map(CatalogRecord::CatalogMeta)
        }
        RecordKind::TableName => decode_table_id_mapping(payload).map(CatalogRecord::TableName),
        RecordKind::TableDefinition => {
            decode_table_definition_payload(payload).map(CatalogRecord::TableDefinition)
        }
        RecordKind::ColumnName => decode_column_id_mapping(payload).map(CatalogRecord::ColumnName),
        RecordKind::ColumnOrdinal => {
            decode_column_id_mapping(payload).map(CatalogRecord::ColumnOrdinal)
        }
        RecordKind::ColumnDefinition => {
            decode_column_definition_payload(payload).map(CatalogRecord::ColumnDefinition)
        }
    }
}

pub(super) fn decode_catalog_meta(bytes: &[u8]) -> Result<CatalogMeta> {
    match decode_record(RecordKind::CatalogMeta, bytes)? {
        CatalogRecord::CatalogMeta(meta) => Ok(meta),
        _ => Err(corruption(
            "catalog meta decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_table_name_mapping(bytes: &[u8]) -> Result<TableId> {
    match decode_record(RecordKind::TableName, bytes)? {
        CatalogRecord::TableName(id) => Ok(id),
        _ => Err(corruption(
            "table name decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_table_definition(expected_id: TableId, bytes: &[u8]) -> Result<TableDef> {
    match decode_record(RecordKind::TableDefinition, bytes)? {
        CatalogRecord::TableDefinition(table) if table.id() == expected_id => Ok(table),
        CatalogRecord::TableDefinition(_) => Err(corruption(
            "table definition key id does not match its payload id",
        )),
        _ => Err(corruption(
            "table definition decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_column_name_mapping(bytes: &[u8]) -> Result<ColumnId> {
    match decode_record(RecordKind::ColumnName, bytes)? {
        CatalogRecord::ColumnName(id) => Ok(id),
        _ => Err(corruption(
            "column name decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_column_ordinal_mapping(bytes: &[u8]) -> Result<ColumnId> {
    match decode_record(RecordKind::ColumnOrdinal, bytes)? {
        CatalogRecord::ColumnOrdinal(id) => Ok(id),
        _ => Err(corruption(
            "column ordinal decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_column_definition(expected_id: ColumnId, bytes: &[u8]) -> Result<ColumnDef> {
    match decode_record(RecordKind::ColumnDefinition, bytes)? {
        CatalogRecord::ColumnDefinition(column) if column.id() == expected_id => Ok(column),
        CatalogRecord::ColumnDefinition(_) => Err(corruption(
            "column definition key id does not match its payload id",
        )),
        _ => Err(corruption(
            "column definition decoder received a different record kind",
        )),
    }
}

fn encode_mapping(kind: RecordKind, id: u64) -> Vec<u8> {
    let mut payload = Encoder::new();
    payload.put_u64(id);
    encode_envelope(kind, payload.into_bytes())
}

fn encode_envelope(kind: RecordKind, payload: Vec<u8>) -> Vec<u8> {
    let payload_len = u32::try_from(payload.len()).expect("catalog V1 payload length fits u32");
    let mut envelope = Encoder::new();
    envelope.put_u8(kind.byte());
    envelope.put_u8(RECORD_VERSION_V1);
    envelope.put_u16(0);
    envelope.put_u32(payload_len);
    envelope.put_bytes(&payload);
    envelope.into_bytes()
}

fn put_name(payload: &mut Encoder, name: &CatalogName) {
    let name_len =
        u16::try_from(name.as_bytes().len()).expect("checked catalog name length fits u16");
    payload.put_u16(name_len);
    payload.put_bytes(name.as_bytes());
}

fn decode_envelope(expected: RecordKind, bytes: &[u8]) -> Result<&[u8]> {
    let kind = RecordKind::from_byte(*bytes.first().ok_or_else(truncated_record)?)?;
    if kind != expected {
        return Err(corruption("catalog key and record kind disagree"));
    }

    let version = *bytes.get(1).ok_or_else(truncated_record)?;
    if version > RECORD_VERSION_V1 {
        return Err(DbError::new(
            ErrorCode::Unsupported,
            "catalog record version is newer than V1",
        ));
    }
    if version != RECORD_VERSION_V1 {
        return Err(corruption("catalog record version zero is invalid"));
    }

    strict_payload(bytes)
}

fn strict_payload(bytes: &[u8]) -> Result<&[u8]> {
    let reserved = u16::from_le_bytes(
        bytes
            .get(2..4)
            .ok_or_else(truncated_record)?
            .try_into()
            .map_err(|_| truncated_record())?,
    );
    if reserved != 0 {
        return Err(corruption(
            "catalog record envelope reserved bytes are nonzero",
        ));
    }

    let payload_len = u32::from_le_bytes(
        bytes
            .get(4..ENVELOPE_LEN)
            .ok_or_else(truncated_record)?
            .try_into()
            .map_err(|_| truncated_record())?,
    );
    let payload_len = usize::try_from(payload_len)
        .map_err(|_| corruption("catalog record payload length exceeds this platform"))?;
    let total_len = ENVELOPE_LEN
        .checked_add(payload_len)
        .ok_or_else(|| corruption("catalog record payload length overflow"))?;
    if bytes.len() != total_len {
        return Err(corruption(
            "catalog record length does not match its payload length",
        ));
    }
    bytes
        .get(ENVELOPE_LEN..total_len)
        .ok_or_else(truncated_record)
}

fn decode_catalog_meta_payload(payload: &[u8]) -> Result<CatalogMeta> {
    let mut decoder = Decoder::new(payload);
    if decoder.get_u16()? != CATALOG_FORMAT_VERSION_V1 {
        return Err(corruption("catalog format version is not V1"));
    }
    if decoder.get_u16()? != 0 {
        return Err(corruption("catalog meta reserved bytes are nonzero"));
    }
    let next_object_id = decode_object_id(decoder.get_u64()?)?;
    require_eof(&decoder)?;
    Ok(CatalogMeta::new(next_object_id))
}

fn decode_table_id_mapping(payload: &[u8]) -> Result<TableId> {
    require_mapping_length(payload)?;
    let mut decoder = Decoder::new(payload);
    decode_table_id(decoder.get_u64()?)
}

fn decode_column_id_mapping(payload: &[u8]) -> Result<ColumnId> {
    require_mapping_length(payload)?;
    let mut decoder = Decoder::new(payload);
    decode_column_id(decoder.get_u64()?)
}

fn require_mapping_length(payload: &[u8]) -> Result<()> {
    if payload.len() != MAPPING_PAYLOAD_LEN {
        return Err(corruption(
            "catalog mapping payload must contain exactly eight bytes",
        ));
    }
    Ok(())
}

fn decode_table_definition_payload(payload: &[u8]) -> Result<TableDef> {
    let mut decoder = Decoder::new(payload);
    let id = decode_table_id(decoder.get_u64()?)?;
    let schema_version = decode_schema_version(decoder.get_u64()?)?;
    let page_id = PageId(decoder.get_u64()?);
    let generation = decoder.get_u64()?;
    let root = PageRef::new(page_id, generation)
        .map_err(|_| corruption("catalog table root page reference is invalid"))?;
    let next_rowid = decode_rowid(decoder.get_u64()?)?;
    let column_count = decoder.get_u16()?;
    let name_len = usize::from(decoder.get_u16()?);
    let name = decode_name(&mut decoder, name_len)?;
    require_eof(&decoder)?;
    TableDef::new(id, schema_version, root, next_rowid, column_count, name)
        .map_err(|_| corruption("catalog table definition fields are invalid"))
}

fn decode_column_definition_payload(payload: &[u8]) -> Result<ColumnDef> {
    let mut decoder = Decoder::new(payload);
    let id = decode_column_id(decoder.get_u64()?)?;
    let table_id = decode_table_id(decoder.get_u64()?)?;
    let ordinal = decoder.get_u16()?;
    let column_type = ColumnType::from_code(decoder.get_u8()?)
        .map_err(|_| corruption("catalog column type code is invalid"))?;
    let flags = decoder.get_u8()?;
    if flags & !1 != 0 {
        return Err(corruption("catalog column flags contain reserved bits"));
    }
    let nullable = flags & 1 != 0;
    let name_len = usize::from(decoder.get_u16()?);
    if decoder.get_u16()? != 0 {
        return Err(corruption(
            "catalog column definition reserved bytes are nonzero",
        ));
    }
    let name = decode_name(&mut decoder, name_len)?;
    require_eof(&decoder)?;
    ColumnDef::new(id, table_id, ordinal, column_type, nullable, name)
        .map_err(|_| corruption("catalog column definition fields are invalid"))
}

fn decode_name(decoder: &mut Decoder<'_>, name_len: usize) -> Result<CatalogName> {
    let bytes = decoder.get_bytes(name_len)?;
    let name = std::str::from_utf8(bytes)
        .map_err(|_| corruption("catalog record name is not valid UTF-8"))?;
    CatalogName::new(name.to_owned())
        .map_err(|_| corruption("catalog record name length is invalid"))
}

fn require_eof(decoder: &Decoder<'_>) -> Result<()> {
    if decoder.remaining() != 0 {
        return Err(corruption("catalog record payload contains trailing bytes"));
    }
    Ok(())
}

fn decode_object_id(value: u64) -> Result<ObjectId> {
    ObjectId::new(value).map_err(|_| corruption("catalog object id must be nonzero"))
}

fn decode_table_id(value: u64) -> Result<TableId> {
    TableId::new(value).map_err(|_| corruption("catalog table id must be nonzero"))
}

fn decode_column_id(value: u64) -> Result<ColumnId> {
    ColumnId::new(value).map_err(|_| corruption("catalog column id must be nonzero"))
}

fn decode_schema_version(value: u64) -> Result<SchemaVersion> {
    SchemaVersion::new(value).map_err(|_| corruption("catalog schema version must be nonzero"))
}

fn decode_rowid(value: u64) -> Result<RowId> {
    RowId::new(value).map_err(|_| corruption("catalog next rowid must be nonzero"))
}

fn truncated_record() -> DbError {
    corruption("catalog record is truncated")
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ErrorCode,
        storage::{
            PageId, PageRef,
            catalog::{
                key::RecordKind,
                types::{
                    CatalogMeta, CatalogName, CatalogRecord, ColumnDef, ColumnId, ColumnType,
                    ObjectId, RowId, SchemaVersion, TableDef, TableId,
                },
            },
        },
    };

    fn table_definition() -> TableDef {
        TableDef::new(
            TableId::new(0x0102_0304_0506_0708).unwrap(),
            SchemaVersion::new(0x1112_1314_1516_1718).unwrap(),
            PageRef::new(PageId(0x2122_2324_2526_2728), 0x3132_3334_3536_3738).unwrap(),
            RowId::new(0x4142_4344_4546_4748).unwrap(),
            0x0201,
            CatalogName::new("T表").unwrap(),
        )
        .unwrap()
    }

    fn column_definition() -> ColumnDef {
        ColumnDef::new(
            ColumnId::new(0x0102_0304_0506_0708).unwrap(),
            TableId::new(0x1112_1314_1516_1718).unwrap(),
            0x0201,
            ColumnType::TimestampWithTimeZone,
            true,
            CatalogName::new("C表").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn catalog_meta_has_exact_v1_envelope_and_payload() {
        let bytes = encode_catalog_meta(&CatalogMeta::new(
            ObjectId::new(0x0102_0304_0506_0708).unwrap(),
        ));
        assert_eq!(
            bytes,
            vec![
                0x00, 1, 0, 0, 12, 0, 0, 0, 1, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1,
            ]
        );
    }

    #[test]
    fn table_name_mapping_has_envelope_and_exact_eight_byte_payload() {
        let bytes = encode_table_name_mapping(TableId::new(0x0102_0304_0506_0708).unwrap());
        assert_eq!(
            bytes,
            vec![0x10, 1, 0, 0, 8, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1,]
        );
    }

    #[test]
    fn table_definition_has_exact_v1_envelope_and_payload() {
        let bytes = encode_table_definition(&table_definition());
        assert_eq!(
            bytes,
            vec![
                0x11, 1, 0, 0, 48, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1, 0x18, 0x17, 0x16, 0x15, 0x14,
                0x13, 0x12, 0x11, 0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21, 0x38, 0x37, 0x36,
                0x35, 0x34, 0x33, 0x32, 0x31, 0x48, 0x47, 0x46, 0x45, 0x44, 0x43, 0x42, 0x41, 1, 2,
                4, 0, b'T', 0xe8, 0xa1, 0xa8,
            ]
        );
    }

    #[test]
    fn column_name_mapping_has_envelope_and_exact_eight_byte_payload() {
        let bytes = encode_column_name_mapping(ColumnId::new(0x0102_0304_0506_0708).unwrap());
        assert_eq!(
            bytes,
            vec![0x12, 1, 0, 0, 8, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1,]
        );
    }

    #[test]
    fn column_ordinal_mapping_has_envelope_and_exact_eight_byte_payload() {
        let bytes = encode_column_ordinal_mapping(ColumnId::new(0x0102_0304_0506_0708).unwrap());
        assert_eq!(
            bytes,
            vec![0x13, 1, 0, 0, 8, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1,]
        );
    }

    #[test]
    fn column_definition_has_exact_v1_envelope_and_payload() {
        let bytes = encode_column_definition(&column_definition());
        assert_eq!(
            bytes,
            vec![
                0x14, 1, 0, 0, 28, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1, 0x18, 0x17, 0x16, 0x15, 0x14,
                0x13, 0x12, 0x11, 1, 2, 8, 1, 4, 0, 0, 0, b'C', 0xe8, 0xa1, 0xa8,
            ]
        );
    }

    #[test]
    fn all_v1_record_variants_decode_to_checked_values() {
        let meta = CatalogMeta::new(ObjectId::new(7).unwrap());
        assert_eq!(
            decode_catalog_meta(&encode_catalog_meta(&meta)).unwrap(),
            meta
        );

        let table_id = TableId::new(8).unwrap();
        assert_eq!(
            decode_table_name_mapping(&encode_table_name_mapping(table_id)).unwrap(),
            table_id
        );
        let table = table_definition();
        assert_eq!(
            decode_table_definition(table.id(), &encode_table_definition(&table)).unwrap(),
            table
        );

        let column_id = ColumnId::new(9).unwrap();
        assert_eq!(
            decode_column_name_mapping(&encode_column_name_mapping(column_id)).unwrap(),
            column_id
        );
        assert_eq!(
            decode_column_ordinal_mapping(&encode_column_ordinal_mapping(column_id)).unwrap(),
            column_id
        );
        let column = column_definition();
        assert_eq!(
            decode_column_definition(column.id(), &encode_column_definition(&column)).unwrap(),
            column
        );

        assert!(matches!(
            decode_record(RecordKind::TableName, &encode_table_name_mapping(table_id)).unwrap(),
            CatalogRecord::TableName(id) if id == table_id
        ));
    }

    #[test]
    fn every_truncation_of_a_v1_record_is_corruption() {
        let records = [
            encode_catalog_meta(&CatalogMeta::new(ObjectId::new(1).unwrap())),
            encode_table_name_mapping(TableId::new(1).unwrap()),
            encode_table_definition(&table_definition()),
            encode_column_name_mapping(ColumnId::new(1).unwrap()),
            encode_column_ordinal_mapping(ColumnId::new(1).unwrap()),
            encode_column_definition(&column_definition()),
        ];
        let kinds = [
            RecordKind::CatalogMeta,
            RecordKind::TableName,
            RecordKind::TableDefinition,
            RecordKind::ColumnName,
            RecordKind::ColumnOrdinal,
            RecordKind::ColumnDefinition,
        ];

        for (kind, record) in kinds.into_iter().zip(records) {
            for length in 0..record.len() {
                let error = decode_record(kind, &record[..length]).unwrap_err();
                assert_eq!(
                    error.code(),
                    ErrorCode::Corruption,
                    "{kind:?} length {length}"
                );
            }
        }
    }

    #[test]
    fn envelope_rejects_wrong_kind_reserved_length_and_trailing_bytes() {
        let valid = encode_table_name_mapping(TableId::new(1).unwrap());
        assert_eq!(
            decode_record(RecordKind::ColumnName, &valid)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        for index in [2, 3] {
            let mut bytes = valid.clone();
            bytes[index] = 1;
            assert_eq!(
                decode_record(RecordKind::TableName, &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }

        for declared_length in [7_u32, 9] {
            let mut bytes = valid.clone();
            bytes[4..8].copy_from_slice(&declared_length.to_le_bytes());
            assert_eq!(
                decode_record(RecordKind::TableName, &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }

        let mut trailing = valid;
        trailing.push(0);
        assert_eq!(
            decode_record(RecordKind::TableName, &trailing)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn known_future_versions_are_unsupported_before_v1_shape_validation() {
        for kind in [
            RecordKind::CatalogMeta,
            RecordKind::TableName,
            RecordKind::TableDefinition,
            RecordKind::ColumnName,
            RecordKind::ColumnOrdinal,
            RecordKind::ColumnDefinition,
        ] {
            assert_eq!(
                decode_record(kind, &[kind.byte(), 2]).unwrap_err().code(),
                ErrorCode::Unsupported
            );
            assert_eq!(
                decode_record(kind, &[kind.byte(), u8::MAX, 1, 1])
                    .unwrap_err()
                    .code(),
                ErrorCode::Unsupported
            );
        }
        assert_eq!(
            decode_record(RecordKind::CatalogMeta, &[0x20, 2])
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );

        let mut zero_version = encode_table_name_mapping(TableId::new(1).unwrap());
        zero_version[1] = 0;
        assert_eq!(
            decode_record(RecordKind::TableName, &zero_version)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn malformed_mapping_payloads_and_zero_ids_are_corruption() {
        for (kind, mut bytes) in [
            (
                RecordKind::TableName,
                encode_table_name_mapping(TableId::new(1).unwrap()),
            ),
            (
                RecordKind::ColumnName,
                encode_column_name_mapping(ColumnId::new(1).unwrap()),
            ),
            (
                RecordKind::ColumnOrdinal,
                encode_column_ordinal_mapping(ColumnId::new(1).unwrap()),
            ),
        ] {
            bytes[8..16].fill(0);
            assert_eq!(
                decode_record(kind, &bytes).unwrap_err().code(),
                ErrorCode::Corruption
            );

            let mut short_payload = bytes[..15].to_vec();
            short_payload[4..8].copy_from_slice(&7_u32.to_le_bytes());
            assert_eq!(
                decode_record(kind, &short_payload).unwrap_err().code(),
                ErrorCode::Corruption
            );
        }
    }

    #[test]
    fn catalog_meta_revalidates_format_reserved_and_next_object_id() {
        let valid = encode_catalog_meta(&CatalogMeta::new(ObjectId::new(1).unwrap()));
        for (offset, value) in [(8, 2_u16), (10, 1_u16)] {
            let mut bytes = valid.clone();
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
            assert_eq!(
                decode_record(RecordKind::CatalogMeta, &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }

        let mut zero_id = valid;
        zero_id[12..20].fill(0);
        assert_eq!(
            decode_record(RecordKind::CatalogMeta, &zero_id)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn table_definition_revalidates_ids_page_ref_counts_names_and_key_id() {
        let valid = encode_table_definition(&table_definition());
        for range in [8..16, 16..24, 24..32, 32..40, 40..48] {
            let mut bytes = valid.clone();
            bytes[range].fill(0);
            assert_eq!(
                decode_record(RecordKind::TableDefinition, &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }

        let mut too_many_columns = valid.clone();
        too_many_columns[48..50].copy_from_slice(&1001_u16.to_le_bytes());
        assert_eq!(
            decode_record(RecordKind::TableDefinition, &too_many_columns)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut invalid_utf8 = valid.clone();
        invalid_utf8[52] = 0xff;
        assert_eq!(
            decode_record(RecordKind::TableDefinition, &invalid_utf8)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut wrong_name_length = valid.clone();
        wrong_name_length[50..52].copy_from_slice(&3_u16.to_le_bytes());
        assert_eq!(
            decode_record(RecordKind::TableDefinition, &wrong_name_length)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        assert_eq!(
            decode_table_definition(TableId::new(99).unwrap(), &valid)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn column_definition_revalidates_ids_ordinal_enum_flags_reserved_name_and_key_id() {
        let valid = encode_column_definition(&column_definition());
        for range in [8..16, 16..24] {
            let mut bytes = valid.clone();
            bytes[range].fill(0);
            assert_eq!(
                decode_record(RecordKind::ColumnDefinition, &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }

        for (offset, replacement) in [
            (24, 1000_u16.to_le_bytes().to_vec()),
            (26, vec![0]),
            (27, vec![2]),
            (30, 1_u16.to_le_bytes().to_vec()),
        ] {
            let mut bytes = valid.clone();
            bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert_eq!(
                decode_record(RecordKind::ColumnDefinition, &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }

        let mut invalid_utf8 = valid.clone();
        invalid_utf8[32] = 0xff;
        assert_eq!(
            decode_record(RecordKind::ColumnDefinition, &invalid_utf8)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut wrong_name_length = valid.clone();
        wrong_name_length[28..30].copy_from_slice(&3_u16.to_le_bytes());
        assert_eq!(
            decode_record(RecordKind::ColumnDefinition, &wrong_name_length)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        assert_eq!(
            decode_column_definition(ColumnId::new(99).unwrap(), &valid)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }
}
