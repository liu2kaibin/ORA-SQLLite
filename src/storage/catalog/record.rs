use crate::{
    DbError, ErrorCode, Result,
    oracle_type::OracleTypeDescriptor,
    storage::{
        PageId, PageRef,
        codec::{Decoder, Encoder},
    },
};

use super::{
    key::RecordKind,
    types::{
        CatalogMeta, CatalogName, CatalogRecord, ColumnDef, ColumnId, ColumnType, ConstraintDef,
        ConstraintId, ConstraintKind, ForeignKeyTarget, IndexDef, IndexId, IndexOwnership,
        MAX_CHECK_PROGRAM_BYTES, MAX_GLOBAL_CATALOG_NAME_BYTES, ObjectId, RowId, SchemaVersion,
        TableDef, TableId,
    },
};

const RECORD_VERSION_V1: u8 = 1;
const RECORD_VERSION_V2: u8 = 2;
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
    match column.oracle_type() {
        None => encode_column_definition_v1(column),
        Some(descriptor) => encode_column_definition_v2(column, descriptor),
    }
}

fn encode_column_definition_v1(column: &ColumnDef) -> Vec<u8> {
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

fn encode_column_definition_v2(column: &ColumnDef, descriptor: OracleTypeDescriptor) -> Vec<u8> {
    let (kind, precision, scale, length) = match descriptor {
        OracleTypeDescriptor::Number { .. } => (
            1,
            descriptor.precision().expect("NUMBER has precision"),
            descriptor.scale().expect("NUMBER has scale"),
            0,
        ),
        OracleTypeDescriptor::Varchar2 { .. } => (
            2,
            0,
            0,
            descriptor
                .length_chars()
                .expect("VARCHAR2 has character length"),
        ),
        OracleTypeDescriptor::Char { .. } => (
            3,
            0,
            0,
            descriptor
                .length_chars()
                .expect("CHAR has character length"),
        ),
        OracleTypeDescriptor::Raw { .. } => (
            4,
            0,
            0,
            descriptor.length_bytes().expect("RAW has byte length"),
        ),
        OracleTypeDescriptor::Date => (5, 0, 0, 0),
        OracleTypeDescriptor::Timestamp { .. } => (
            6,
            descriptor
                .fractional_precision()
                .expect("TIMESTAMP has fractional precision"),
            0,
            0,
        ),
        OracleTypeDescriptor::TimestampWithTimeZone { .. } => (
            7,
            descriptor
                .fractional_precision()
                .expect("TIMESTAMP WITH TIME ZONE has fractional precision"),
            0,
            0,
        ),
    };
    let mut payload = Encoder::new();
    payload.put_u64(column.id().get());
    payload.put_u64(column.table_id().get());
    payload.put_u16(column.ordinal());
    payload.put_u8(column.column_type().code());
    payload.put_u8(u8::from(column.nullable()));
    payload.put_u8(kind);
    payload.put_u8(precision);
    payload.put_u8(scale as u8);
    payload.put_u8(0);
    payload.put_u32(length);
    let name_len = u16::try_from(column.name().as_bytes().len())
        .expect("checked catalog name length fits u16");
    payload.put_u16(name_len);
    payload.put_u16(0);
    payload.put_bytes(column.name().as_bytes());
    encode_envelope_with_version(
        RecordKind::ColumnDefinition,
        RECORD_VERSION_V2,
        payload.into_bytes(),
    )
}

pub(super) fn encode_index_name_mapping(id: IndexId) -> Vec<u8> {
    encode_mapping(RecordKind::IndexName, id.get())
}

pub(super) fn encode_table_index_mapping(id: IndexId) -> Vec<u8> {
    encode_mapping(RecordKind::TableIndex, id.get())
}

pub(super) fn encode_index_column_mapping(id: ColumnId) -> Vec<u8> {
    encode_mapping(RecordKind::IndexColumn, id.get())
}

pub(super) fn encode_index_definition(index: &IndexDef) -> Vec<u8> {
    let mut payload = Encoder::new();
    payload.put_u64(index.id().get());
    payload.put_u64(index.table_id().get());
    payload.put_u64(index.root().page_id().0);
    payload.put_u64(index.root().generation());
    payload.put_u16(index.column_count());
    payload.put_u8(u8::from(index.unique()));
    let owner_id = match index.ownership() {
        IndexOwnership::Explicit => {
            payload.put_u8(0);
            0
        }
        IndexOwnership::Constraint(id) => {
            payload.put_u8(1);
            id.get()
        }
    };
    payload.put_u16(0);
    payload.put_u64(owner_id);
    put_name(&mut payload, index.name());
    encode_envelope(RecordKind::IndexDefinition, payload.into_bytes())
}

pub(super) fn encode_constraint_name_mapping(id: ConstraintId) -> Vec<u8> {
    encode_mapping(RecordKind::ConstraintName, id.get())
}

pub(super) fn encode_table_constraint_mapping(id: ConstraintId) -> Vec<u8> {
    encode_mapping(RecordKind::TableConstraint, id.get())
}

pub(super) fn encode_constraint_column_mapping(id: ColumnId) -> Vec<u8> {
    encode_mapping(RecordKind::ConstraintColumn, id.get())
}

pub(super) fn encode_referenced_by_mapping(id: ConstraintId) -> Vec<u8> {
    encode_envelope(RecordKind::ReferencedBy, id.get().to_be_bytes().to_vec())
}

pub(super) fn encode_constraint_definition(definition: &ConstraintDef) -> Vec<u8> {
    let mut payload = Encoder::new();
    payload.put_u64(definition.id().get());
    payload.put_u64(definition.table_id().get());
    payload.put_u8(constraint_kind_code(definition.kind()));
    payload.put_u8(0);
    payload.put_u16(definition.column_count());
    payload.put_u64(definition.index_id().map_or(0, IndexId::get));
    payload.put_bytes(
        &definition
            .parent_table_id()
            .map_or(0, TableId::get)
            .to_be_bytes(),
    );
    payload.put_bytes(
        &definition
            .parent_constraint_id()
            .map_or(0, ConstraintId::get)
            .to_be_bytes(),
    );
    payload.put_u8(0);
    payload.put_u8(0);
    payload.put_u8(0);
    payload.put_u8(0);
    let check_program_len = u32::try_from(definition.check_program().len())
        .expect("checked constraint program length fits u32");
    payload.put_u32(check_program_len);
    let name_len = u16::try_from(definition.name().as_bytes().len())
        .expect("checked constraint name length fits u16");
    payload.put_u16(name_len);
    payload.put_u16(0);
    payload.put_bytes(definition.check_program());
    payload.put_bytes(definition.name().as_bytes());
    encode_envelope(RecordKind::ConstraintDefinition, payload.into_bytes())
}

pub(super) fn decode_record(expected: RecordKind, bytes: &[u8]) -> Result<CatalogRecord> {
    if expected == RecordKind::ReferencedBy
        && bytes
            .first()
            .is_some_and(|kind| *kind != RecordKind::ReferencedBy.byte())
    {
        return Err(corruption("catalog key and record kind disagree"));
    }
    let (version, payload) = decode_envelope(expected, bytes)?;
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
        RecordKind::ColumnDefinition => match version {
            RECORD_VERSION_V1 => decode_column_definition_v1_payload(payload),
            RECORD_VERSION_V2 => decode_column_definition_v2_payload(payload),
            _ => unreachable!("decode_envelope validates record versions"),
        }
        .map(CatalogRecord::ColumnDefinition),
        RecordKind::IndexName => decode_index_id_mapping(payload).map(CatalogRecord::IndexName),
        RecordKind::IndexDefinition => {
            decode_index_definition_payload(payload).map(CatalogRecord::IndexDefinition)
        }
        RecordKind::TableIndex => decode_index_id_mapping(payload).map(CatalogRecord::TableIndex),
        RecordKind::IndexColumn => {
            decode_column_id_mapping(payload).map(CatalogRecord::IndexColumn)
        }
        RecordKind::ConstraintName => {
            decode_constraint_id_mapping(payload).map(CatalogRecord::ConstraintName)
        }
        RecordKind::ConstraintDefinition => {
            decode_constraint_definition_payload(payload).map(CatalogRecord::ConstraintDefinition)
        }
        RecordKind::TableConstraint => {
            decode_constraint_id_mapping(payload).map(CatalogRecord::TableConstraint)
        }
        RecordKind::ConstraintColumn => {
            decode_column_id_mapping(payload).map(CatalogRecord::ConstraintColumn)
        }
        RecordKind::ReferencedBy => {
            decode_referenced_by_payload(payload).map(CatalogRecord::ReferencedBy)
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

pub(super) fn decode_index_name_mapping(bytes: &[u8]) -> Result<IndexId> {
    match decode_record(RecordKind::IndexName, bytes)? {
        CatalogRecord::IndexName(id) => Ok(id),
        _ => Err(corruption(
            "index name decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_table_index_mapping(bytes: &[u8]) -> Result<IndexId> {
    match decode_record(RecordKind::TableIndex, bytes)? {
        CatalogRecord::TableIndex(id) => Ok(id),
        _ => Err(corruption(
            "table index decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_index_column_mapping(bytes: &[u8]) -> Result<ColumnId> {
    match decode_record(RecordKind::IndexColumn, bytes)? {
        CatalogRecord::IndexColumn(id) => Ok(id),
        _ => Err(corruption(
            "index column decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_index_definition(expected_id: IndexId, bytes: &[u8]) -> Result<IndexDef> {
    match decode_record(RecordKind::IndexDefinition, bytes)? {
        CatalogRecord::IndexDefinition(index) if index.id() == expected_id => Ok(index),
        CatalogRecord::IndexDefinition(_) => Err(corruption(
            "index definition key id does not match its payload id",
        )),
        _ => Err(corruption(
            "index definition decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_constraint_name_mapping(bytes: &[u8]) -> Result<ConstraintId> {
    match decode_record(RecordKind::ConstraintName, bytes)? {
        CatalogRecord::ConstraintName(id) => Ok(id),
        _ => Err(corruption(
            "constraint name decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_table_constraint_mapping(bytes: &[u8]) -> Result<ConstraintId> {
    match decode_record(RecordKind::TableConstraint, bytes)? {
        CatalogRecord::TableConstraint(id) => Ok(id),
        _ => Err(corruption(
            "table constraint decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_constraint_column_mapping(bytes: &[u8]) -> Result<ColumnId> {
    match decode_record(RecordKind::ConstraintColumn, bytes)? {
        CatalogRecord::ConstraintColumn(id) => Ok(id),
        _ => Err(corruption(
            "constraint column decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_referenced_by_mapping(bytes: &[u8]) -> Result<ConstraintId> {
    match decode_record(RecordKind::ReferencedBy, bytes)? {
        CatalogRecord::ReferencedBy(id) => Ok(id),
        _ => Err(corruption(
            "referenced-by decoder received a different record kind",
        )),
    }
}

pub(super) fn decode_constraint_definition(
    expected_id: ConstraintId,
    bytes: &[u8],
) -> Result<ConstraintDef> {
    match decode_record(RecordKind::ConstraintDefinition, bytes)? {
        CatalogRecord::ConstraintDefinition(definition) if definition.id() == expected_id => {
            Ok(definition)
        }
        CatalogRecord::ConstraintDefinition(_) => Err(corruption(
            "constraint definition key id does not match its payload id",
        )),
        _ => Err(corruption(
            "constraint definition decoder received a different record kind",
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
    encode_envelope_with_version(kind, RECORD_VERSION_V1, payload)
}

fn encode_envelope_with_version(kind: RecordKind, version: u8, payload: Vec<u8>) -> Vec<u8> {
    let payload_len = u32::try_from(payload.len()).expect("catalog V1 payload length fits u32");
    let mut envelope = Encoder::new();
    envelope.put_u8(kind.byte());
    envelope.put_u8(version);
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

fn decode_envelope(expected: RecordKind, bytes: &[u8]) -> Result<(u8, &[u8])> {
    let kind = RecordKind::from_byte(*bytes.first().ok_or_else(truncated_record)?)?;
    let version = *bytes.get(1).ok_or_else(truncated_record)?;
    let maximum_version = if expected == RecordKind::ColumnDefinition {
        RECORD_VERSION_V2
    } else {
        RECORD_VERSION_V1
    };
    if version > maximum_version {
        return Err(DbError::new(
            ErrorCode::Unsupported,
            "catalog record version is newer than this engine supports",
        ));
    }
    if version == 0 {
        return Err(corruption("catalog record version zero is invalid"));
    }
    if kind != expected {
        return Err(corruption("catalog key and record kind disagree"));
    }

    strict_payload(bytes).map(|payload| (version, payload))
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

fn decode_index_id_mapping(payload: &[u8]) -> Result<IndexId> {
    require_mapping_length(payload)?;
    let mut decoder = Decoder::new(payload);
    decode_index_id(decoder.get_u64()?)
}

fn decode_constraint_id_mapping(payload: &[u8]) -> Result<ConstraintId> {
    require_mapping_length(payload)?;
    let mut decoder = Decoder::new(payload);
    decode_constraint_id(decoder.get_u64()?)
}

fn decode_referenced_by_payload(payload: &[u8]) -> Result<ConstraintId> {
    require_mapping_length(payload)?;
    let value = u64::from_be_bytes(
        payload
            .try_into()
            .map_err(|_| corruption("referenced-by mapping id has an invalid length"))?,
    );
    decode_constraint_id(value)
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

fn decode_column_definition_v1_payload(payload: &[u8]) -> Result<ColumnDef> {
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

fn decode_column_definition_v2_payload(payload: &[u8]) -> Result<ColumnDef> {
    let mut decoder = Decoder::new(payload);
    let id = decode_column_id(decoder.get_u64()?)?;
    let table_id = decode_table_id(decoder.get_u64()?)?;
    let ordinal = decoder.get_u16()?;
    let column_type = ColumnType::from_code(decoder.get_u8()?)
        .map_err(|_| corruption("catalog column type code is invalid"))?;
    let nullable = match decoder.get_u8()? {
        0 => false,
        1 => true,
        _ => return Err(corruption("catalog column nullable flag is invalid")),
    };
    let kind = decoder.get_u8()?;
    let precision = decoder.get_u8()?;
    let scale = decoder.get_u8()? as i8;
    if decoder.get_u8()? != 0 {
        return Err(corruption("catalog Oracle type flags are nonzero"));
    }
    let length = decoder.get_u32()?;
    let name_len = usize::from(decoder.get_u16()?);
    if decoder.get_u16()? != 0 {
        return Err(corruption(
            "catalog column definition reserved bytes are nonzero",
        ));
    }
    let descriptor = decode_oracle_descriptor(kind, precision, scale, length)?;
    if column_type != descriptor.physical_type() {
        return Err(corruption(
            "catalog Oracle type does not match its physical column type",
        ));
    }
    let name = decode_name(&mut decoder, name_len)?;
    require_eof(&decoder)?;
    ColumnDef::new_oracle(id, table_id, ordinal, nullable, name, descriptor)
        .map_err(|_| corruption("catalog column definition fields are invalid"))
}

fn decode_oracle_descriptor(
    kind: u8,
    precision: u8,
    scale: i8,
    length: u32,
) -> Result<OracleTypeDescriptor> {
    let descriptor = match kind {
        1 if length == 0 => OracleTypeDescriptor::number(precision, scale),
        2 if precision == 0 && scale == 0 => OracleTypeDescriptor::varchar2(length),
        3 if precision == 0 && scale == 0 => OracleTypeDescriptor::char_type(length),
        4 if precision == 0 && scale == 0 => OracleTypeDescriptor::raw(length),
        5 if precision == 0 && scale == 0 && length == 0 => Ok(OracleTypeDescriptor::date()),
        6 if scale == 0 && length == 0 => OracleTypeDescriptor::timestamp(precision),
        7 if scale == 0 && length == 0 => OracleTypeDescriptor::timestamp_with_time_zone(precision),
        1..=7 => return Err(corruption("catalog Oracle type parameters are invalid")),
        _ => return Err(corruption("catalog Oracle type kind is invalid")),
    };
    descriptor.map_err(|_| corruption("catalog Oracle type parameters are invalid"))
}

fn decode_index_definition_payload(payload: &[u8]) -> Result<IndexDef> {
    let mut decoder = Decoder::new(payload);
    let id = decode_index_id(decoder.get_u64()?)?;
    let table_id = decode_table_id(decoder.get_u64()?)?;
    let root = PageRef::new(PageId(decoder.get_u64()?), decoder.get_u64()?)
        .map_err(|_| corruption("catalog index root page reference is invalid"))?;
    let column_count = decoder.get_u16()?;
    let unique = match decoder.get_u8()? {
        0 => false,
        1 => true,
        _ => return Err(corruption("catalog index unique flag is invalid")),
    };
    let owner_kind = decoder.get_u8()?;
    if decoder.get_u16()? != 0 {
        return Err(corruption(
            "catalog index definition reserved bytes are nonzero",
        ));
    }
    let owner_id = decoder.get_u64()?;
    let ownership = match (owner_kind, owner_id) {
        (0, 0) => IndexOwnership::Explicit,
        (0, _) => return Err(corruption("explicit index owner id must be zero")),
        (1, 0) => return Err(corruption("constraint index owner id must be nonzero")),
        (1, id) => IndexOwnership::Constraint(decode_constraint_id(id)?),
        _ => return Err(corruption("catalog index owner kind is invalid")),
    };
    let name_len = usize::from(decoder.get_u16()?);
    let name = decode_name(&mut decoder, name_len)?;
    require_eof(&decoder)?;
    IndexDef::new(id, table_id, root, column_count, unique, ownership, name)
        .map_err(|_| corruption("catalog index definition fields are invalid"))
}

fn decode_constraint_definition_payload(payload: &[u8]) -> Result<ConstraintDef> {
    let mut decoder = Decoder::new(payload);
    let id = decode_constraint_id(decoder.get_u64()?)?;
    let table_id = decode_table_id(decoder.get_u64()?)?;
    let kind = decode_constraint_kind(decoder.get_u8()?)?;
    if decoder.get_u8()? != 0 {
        return Err(corruption("catalog constraint flags are nonzero"));
    }
    let column_count = decoder.get_u16()?;
    let index_id = match decoder.get_u64()? {
        0 => None,
        value => Some(decode_index_id(value)?),
    };
    let parent_table_id = u64::from_be_bytes(
        decoder
            .get_bytes(8)?
            .try_into()
            .map_err(|_| corruption("catalog parent table id is truncated"))?,
    );
    let parent_constraint_id = u64::from_be_bytes(
        decoder
            .get_bytes(8)?
            .try_into()
            .map_err(|_| corruption("catalog parent constraint id is truncated"))?,
    );
    let foreign_key_target = match (parent_table_id, parent_constraint_id) {
        (0, 0) => None,
        (0, _) | (_, 0) => {
            return Err(corruption(
                "catalog foreign-key parent ids must both be zero or both be nonzero",
            ));
        }
        (table_id, constraint_id) => Some(ForeignKeyTarget::new(
            decode_table_id(table_id)?,
            decode_constraint_id(constraint_id)?,
        )),
    };
    if decoder.get_u8()? != 0 {
        return Err(corruption("catalog constraint deferred mode is nonzero"));
    }
    if decoder.get_u8()? != 0 {
        return Err(corruption("catalog constraint update action is nonzero"));
    }
    if decoder.get_u8()? != 0 {
        return Err(corruption("catalog constraint delete action is nonzero"));
    }
    if decoder.get_u8()? != 0 {
        return Err(corruption("catalog constraint reserved byte is nonzero"));
    }
    let check_program_len = usize::try_from(decoder.get_u32()?)
        .map_err(|_| corruption("catalog check program length exceeds this platform"))?;
    if check_program_len > MAX_CHECK_PROGRAM_BYTES {
        return Err(corruption("catalog check program length exceeds the limit"));
    }
    let name_len = usize::from(decoder.get_u16()?);
    if decoder.get_u16()? != 0 {
        return Err(corruption(
            "catalog constraint definition reserved bytes are nonzero",
        ));
    }
    let check_program = decoder
        .get_bytes(check_program_len)?
        .to_vec()
        .into_boxed_slice();
    let name = decode_name(&mut decoder, name_len)?;
    if name.as_bytes().len() > MAX_GLOBAL_CATALOG_NAME_BYTES {
        return Err(corruption("catalog constraint name length is invalid"));
    }
    require_eof(&decoder)?;
    ConstraintDef::new(
        id,
        table_id,
        kind,
        column_count,
        index_id,
        foreign_key_target,
        check_program,
        name,
    )
    .map_err(|_| corruption("catalog constraint definition fields are invalid"))
}

const fn constraint_kind_code(kind: ConstraintKind) -> u8 {
    match kind {
        ConstraintKind::PrimaryKey => 1,
        ConstraintKind::Unique => 2,
        ConstraintKind::NotNull => 3,
        ConstraintKind::Check => 4,
        ConstraintKind::ForeignKey => 5,
    }
}

fn decode_constraint_kind(code: u8) -> Result<ConstraintKind> {
    match code {
        1 => Ok(ConstraintKind::PrimaryKey),
        2 => Ok(ConstraintKind::Unique),
        3 => Ok(ConstraintKind::NotNull),
        4 => Ok(ConstraintKind::Check),
        5 => Ok(ConstraintKind::ForeignKey),
        _ => Err(corruption("catalog constraint kind is invalid")),
    }
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

fn decode_index_id(value: u64) -> Result<IndexId> {
    IndexId::new(value).map_err(|_| corruption("catalog index id must be nonzero"))
}

fn decode_constraint_id(value: u64) -> Result<ConstraintId> {
    ConstraintId::new(value).map_err(|_| corruption("catalog constraint id must be nonzero"))
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
    use super::super::types::ForeignKeyTarget;
    use super::*;
    use crate::{
        ErrorCode,
        oracle_type::OracleTypeDescriptor,
        storage::{
            PageId, PageRef,
            catalog::{
                key::RecordKind,
                types::{
                    CatalogMeta, CatalogName, CatalogRecord, ColumnDef, ColumnId, ColumnType,
                    ConstraintDef, ConstraintId, ConstraintKind, IndexDef, IndexId, IndexOwnership,
                    MAX_COLUMNS, ObjectId, RowId, SchemaVersion, TableDef, TableId,
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

    fn oracle_number_column() -> ColumnDef {
        ColumnDef::new_oracle(
            ColumnId::new(3).unwrap(),
            TableId::new(2).unwrap(),
            0,
            false,
            CatalogName::new("AMOUNT").unwrap(),
            OracleTypeDescriptor::number(12, 2).unwrap(),
        )
        .unwrap()
    }

    fn index_definition_fixture() -> IndexDef {
        IndexDef::new(
            IndexId::new(0x0102_0304_0506_0708).unwrap(),
            TableId::new(0x1112_1314_1516_1718).unwrap(),
            PageRef::new(PageId(0x2122_2324_2526_2728), 0x3132_3334_3536_3738).unwrap(),
            2,
            true,
            IndexOwnership::Constraint(ConstraintId::new(0x4142_4344_4546_4748).unwrap()),
            CatalogName::new("IX_表").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn index_record_v1_has_hand_computed_envelope_and_strict_round_trip() {
        let index = index_definition_fixture();
        let encoded = encode_index_definition(&index);
        assert_eq!(&encoded[0..4], &[0x21, 1, 0, 0]);
        assert_eq!(
            u32::from_le_bytes(encoded[4..8].try_into().unwrap()) as usize,
            encoded.len() - 8
        );
        assert_eq!(
            decode_index_definition(index.id(), &encoded).unwrap(),
            index
        );
        let mut reserved = encoded.clone();
        reserved[2] = 1;
        assert_eq!(
            decode_index_definition(index.id(), &reserved)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn index_record_v1_has_hand_computed_payload_bytes() {
        assert_eq!(
            encode_index_definition(&index_definition_fixture()),
            vec![
                0x21, 1, 0, 0, 54, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1, 0x18, 0x17, 0x16, 0x15, 0x14,
                0x13, 0x12, 0x11, 0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21, 0x38, 0x37, 0x36,
                0x35, 0x34, 0x33, 0x32, 0x31, 2, 0, 1, 1, 0, 0, 0x48, 0x47, 0x46, 0x45, 0x44, 0x43,
                0x42, 0x41, 6, 0, b'I', b'X', b'_', 0xe8, 0xa1, 0xa8,
            ]
        );
    }

    #[test]
    fn index_definition_rejects_every_truncation_and_trailing_byte() {
        let index = index_definition_fixture();
        let encoded = encode_index_definition(&index);
        for end in 0..encoded.len() {
            assert_eq!(
                decode_index_definition(index.id(), &encoded[..end])
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }
        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            decode_index_definition(index.id(), &trailing)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn index_records_reject_invalid_fields_and_round_trip_mappings() {
        let index = index_definition_fixture();
        let index_id = index.id();
        let column_id = ColumnId::new(9).unwrap();
        assert_eq!(
            decode_index_name_mapping(&encode_index_name_mapping(index_id)).unwrap(),
            index_id
        );
        assert_eq!(
            decode_table_index_mapping(&encode_table_index_mapping(index_id)).unwrap(),
            index_id
        );
        assert_eq!(
            decode_index_column_mapping(&encode_index_column_mapping(column_id)).unwrap(),
            column_id
        );

        let valid = encode_index_definition(&index);
        assert_eq!(
            decode_index_definition(IndexId::new(10).unwrap(), &valid)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        for (range, bytes) in [
            (43..44, vec![2]),
            (46..54, vec![0; 8]),
            (42..43, vec![2]),
            (24..32, vec![0; 8]),
            (32..40, vec![0; 8]),
            (40..42, vec![0, 0]),
            (44..46, vec![1, 0]),
            (54..56, vec![7, 0]),
            (8..16, vec![0; 8]),
            (4..8, 53_u32.to_le_bytes().to_vec()),
        ] {
            let mut encoded = valid.clone();
            encoded[range].copy_from_slice(&bytes);
            assert_eq!(
                decode_index_definition(index_id, &encoded)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            );
        }

        let mut explicit_with_owner = valid.clone();
        explicit_with_owner[43] = 0;
        assert_eq!(
            decode_index_definition(index_id, &explicit_with_owner)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut invalid_utf8 = encode_index_definition(&index);
        invalid_utf8[60] = 0xff;
        assert_eq!(
            decode_index_definition(index_id, &invalid_utf8)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut version = valid;
        version[1] = 2;
        assert_eq!(
            decode_index_definition(index_id, &version)
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );
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
    fn column_definition_v2_has_the_stable_number_payload() {
        let column = oracle_number_column();
        let encoded = encode_column_definition(&column);
        assert_eq!(encoded[1], 2);
        assert_eq!(&encoded[8 + 20..8 + 28], &[1, 12, 2, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_column_definition(column.id(), &encoded).unwrap(),
            column
        );
    }

    #[test]
    fn column_definition_v2_rejects_nonzero_reserved_and_trailing_bytes() {
        let mut reserved = encode_column_definition(&oracle_number_column());
        reserved[8 + 30] = 1;
        assert_eq!(
            decode_column_definition(ColumnId::new(3).unwrap(), &reserved)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        let mut trailing = encode_column_definition(&oracle_number_column());
        trailing.push(0);
        assert_eq!(
            decode_column_definition(ColumnId::new(3).unwrap(), &trailing)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn column_definition_v2_rejects_invalid_descriptor_fields() {
        let valid = encode_column_definition(&oracle_number_column());
        for (offset, replacement) in [
            (19, 2_u8),
            (20, 0_u8),
            (20, 8_u8),
            (21, 0_u8),
            (21, 39_u8),
            (22, 39_u8),
            (23, 1_u8),
            (24, 1_u8),
            (18, ColumnType::Text.code()),
        ] {
            let mut bytes = valid.clone();
            bytes[8 + offset] = replacement;
            assert_eq!(
                decode_column_definition(ColumnId::new(3).unwrap(), &bytes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption,
                "payload offset {offset}"
            );
        }
    }

    #[test]
    fn every_oracle_descriptor_has_stable_v2_fields_and_round_trips() {
        let cases = [
            (
                OracleTypeDescriptor::varchar2(32_767).unwrap(),
                [2, 0, 0, 0],
                32_767_u32,
            ),
            (
                OracleTypeDescriptor::char_type(2_000).unwrap(),
                [3, 0, 0, 0],
                2_000,
            ),
            (
                OracleTypeDescriptor::raw(32_767).unwrap(),
                [4, 0, 0, 0],
                32_767,
            ),
            (OracleTypeDescriptor::date(), [5, 0, 0, 0], 0),
            (OracleTypeDescriptor::timestamp(9).unwrap(), [6, 9, 0, 0], 0),
            (
                OracleTypeDescriptor::timestamp_with_time_zone(9).unwrap(),
                [7, 9, 0, 0],
                0,
            ),
        ];
        for (descriptor, parameter_bytes, length) in cases {
            let column = ColumnDef::new_oracle(
                ColumnId::new(3).unwrap(),
                TableId::new(2).unwrap(),
                0,
                false,
                CatalogName::new("C").unwrap(),
                descriptor,
            )
            .unwrap();
            let encoded = encode_column_definition(&column);
            assert_eq!(&encoded[28..32], &parameter_bytes);
            assert_eq!(&encoded[32..36], &length.to_le_bytes());
            assert_eq!(
                decode_column_definition(column.id(), &encoded).unwrap(),
                column
            );
        }
    }

    #[test]
    fn column_definition_v2_rejects_unused_parameters_invalid_utf8_and_truncation() {
        let descriptors = [
            OracleTypeDescriptor::varchar2(10).unwrap(),
            OracleTypeDescriptor::char_type(10).unwrap(),
            OracleTypeDescriptor::raw(10).unwrap(),
            OracleTypeDescriptor::date(),
            OracleTypeDescriptor::timestamp(6).unwrap(),
            OracleTypeDescriptor::timestamp_with_time_zone(6).unwrap(),
        ];
        for descriptor in descriptors {
            let column = ColumnDef::new_oracle(
                ColumnId::new(3).unwrap(),
                TableId::new(2).unwrap(),
                0,
                false,
                CatalogName::new("C").unwrap(),
                descriptor,
            )
            .unwrap();
            let encoded = encode_column_definition(&column);
            let unused_offsets: &[usize] = match descriptor {
                OracleTypeDescriptor::Varchar2 { .. }
                | OracleTypeDescriptor::Char { .. }
                | OracleTypeDescriptor::Raw { .. } => &[21, 22],
                OracleTypeDescriptor::Date => &[21, 22, 24],
                OracleTypeDescriptor::Timestamp { .. }
                | OracleTypeDescriptor::TimestampWithTimeZone { .. } => &[22, 24],
                OracleTypeDescriptor::Number { .. } => unreachable!(),
            };
            for offset in unused_offsets {
                let mut nonzero_unused = encoded.clone();
                nonzero_unused[8 + offset] = 1;
                assert_eq!(
                    decode_column_definition(column.id(), &nonzero_unused)
                        .unwrap_err()
                        .code(),
                    ErrorCode::Corruption,
                    "{descriptor:?} unused payload offset {offset}"
                );
            }
            for end in 0..encoded.len() {
                assert_eq!(
                    decode_column_definition(column.id(), &encoded[..end])
                        .unwrap_err()
                        .code(),
                    ErrorCode::Corruption,
                    "{descriptor:?} truncation at {end}"
                );
            }
        }

        let mut invalid_utf8 = encode_column_definition(&oracle_number_column());
        invalid_utf8[8 + 32] = 0xff;
        assert_eq!(
            decode_column_definition(ColumnId::new(3).unwrap(), &invalid_utf8)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
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
    fn known_future_versions_are_unsupported_before_shape_validation() {
        for kind in [
            RecordKind::CatalogMeta,
            RecordKind::TableName,
            RecordKind::TableDefinition,
            RecordKind::ColumnName,
            RecordKind::ColumnOrdinal,
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
            decode_record(
                RecordKind::ColumnDefinition,
                &[RecordKind::ColumnDefinition.byte(), 3]
            )
            .unwrap_err()
            .code(),
            ErrorCode::Unsupported
        );
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

    #[test]
    fn constraint_def_v1_has_exact_payload_and_round_trips() {
        let definition = ConstraintDef::new(
            ConstraintId::new(0x0102_0304_0506_0708).unwrap(),
            TableId::new(0x1112_1314_1516_1718).unwrap(),
            ConstraintKind::PrimaryKey,
            2,
            Some(IndexId::new(0x2122_2324_2526_2728).unwrap()),
            None,
            Box::new([]),
            CatalogName::new("PK_T").unwrap(),
        )
        .unwrap();
        let expected = vec![
            0x31, 0x01, 0x00, 0x00, 0x3c, 0x00, 0x00, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03,
            0x02, 0x01, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, 0x01, 0x00, 0x02, 0x00,
            0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, b'P', b'K', b'_', b'T',
        ];
        let encoded = encode_constraint_definition(&definition);
        assert_eq!(encoded, expected);
        assert_eq!(
            decode_constraint_definition(definition.id(), &encoded).unwrap(),
            definition
        );
    }

    #[test]
    fn foreign_key_constraint_v1_uses_reserved_parent_fields_and_round_trips() {
        let target = ForeignKeyTarget::new(
            TableId::new(0x3132_3334_3536_3738).unwrap(),
            ConstraintId::new(0x4142_4344_4546_4748).unwrap(),
        );
        let definition = ConstraintDef::new(
            ConstraintId::new(0x0102_0304_0506_0708).unwrap(),
            TableId::new(0x1112_1314_1516_1718).unwrap(),
            ConstraintKind::ForeignKey,
            2,
            None,
            Some(target),
            Box::new([]),
            CatalogName::new("FK_CHILD_PARENT").unwrap(),
        )
        .unwrap();
        let encoded = encode_constraint_definition(&definition);
        assert_eq!(encoded[0], RecordKind::ConstraintDefinition.byte());
        assert_eq!(encoded[1], 1);
        assert_eq!(&encoded[36..44], &target.table_id().get().to_be_bytes());
        assert_eq!(
            &encoded[44..52],
            &target.constraint_id().get().to_be_bytes()
        );
        assert_eq!(
            decode_constraint_definition(definition.id(), &encoded).unwrap(),
            definition
        );
    }

    #[test]
    fn referenced_by_mapping_is_exact_and_rejects_wrong_kind_or_trailing_bytes() {
        let child = ConstraintId::new(9).unwrap();
        let bytes = encode_referenced_by_mapping(child);
        assert_eq!(bytes, [0x34, 1, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);
        assert_eq!(decode_referenced_by_mapping(&bytes).unwrap(), child);

        let mut wrong_kind = bytes.clone();
        wrong_kind[0] = RecordKind::ConstraintColumn.byte();
        assert_eq!(
            decode_referenced_by_mapping(&wrong_kind)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut unknown_kind = bytes.clone();
        unknown_kind[0] = 0xff;
        assert_eq!(
            decode_referenced_by_mapping(&unknown_kind)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );

        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            decode_referenced_by_mapping(&trailing).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn constraint_mappings_have_exact_complete_v1_bytes() {
        let constraint = ConstraintId::new(0x0102_0304_0506_0708).unwrap();
        let expected_constraint = vec![
            0x30, 0x01, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03,
            0x02, 0x01,
        ];
        assert_eq!(
            encode_constraint_name_mapping(constraint),
            expected_constraint
        );
        assert_eq!(
            decode_constraint_name_mapping(&expected_constraint).unwrap(),
            constraint
        );

        let mut expected_table = expected_constraint.clone();
        expected_table[0] = 0x32;
        assert_eq!(encode_table_constraint_mapping(constraint), expected_table);
        assert_eq!(
            decode_table_constraint_mapping(&expected_table).unwrap(),
            constraint
        );

        let column = ColumnId::new(0x1112_1314_1516_1718).unwrap();
        let expected_column = vec![
            0x33, 0x01, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13,
            0x12, 0x11,
        ];
        assert_eq!(encode_constraint_column_mapping(column), expected_column);
        assert_eq!(
            decode_constraint_column_mapping(&expected_column).unwrap(),
            column
        );
    }

    #[test]
    fn every_local_constraint_kind_round_trips() {
        let id = ConstraintId::new(7).unwrap();
        let table_id = TableId::new(3).unwrap();
        let index_id = IndexId::new(11).unwrap();
        let cases = [
            (ConstraintKind::PrimaryKey, 1, Some(index_id), Vec::new()),
            (ConstraintKind::Unique, 2, Some(index_id), Vec::new()),
            (ConstraintKind::NotNull, 1, None, Vec::new()),
            (ConstraintKind::Check, 0, None, vec![0xaa]),
        ];
        for (kind, column_count, index_id, check_program) in cases {
            let definition = ConstraintDef::new(
                id,
                table_id,
                kind,
                column_count,
                index_id,
                None,
                check_program.into_boxed_slice(),
                CatalogName::new("C_T").unwrap(),
            )
            .unwrap();
            let bytes = encode_constraint_definition(&definition);
            assert_eq!(
                decode_constraint_definition(id, &bytes).unwrap(),
                definition
            );
        }
    }

    #[test]
    fn constraint_mappings_reject_every_truncation_zero_id_and_future_version() {
        let records = [
            (
                RecordKind::ConstraintName,
                encode_constraint_name_mapping(ConstraintId::new(7).unwrap()),
            ),
            (
                RecordKind::TableConstraint,
                encode_table_constraint_mapping(ConstraintId::new(7).unwrap()),
            ),
            (
                RecordKind::ConstraintColumn,
                encode_constraint_column_mapping(ColumnId::new(9).unwrap()),
            ),
        ];
        for (kind, bytes) in records {
            for end in 0..bytes.len() {
                assert_eq!(
                    decode_record(kind, &bytes[..end]).unwrap_err().code(),
                    ErrorCode::Corruption
                );
            }
            let mut zero_id = bytes.clone();
            zero_id[8..16].fill(0);
            assert_eq!(
                decode_record(kind, &zero_id).unwrap_err().code(),
                ErrorCode::Corruption
            );
            let mut future = bytes;
            future[1] = 2;
            assert_eq!(
                decode_record(kind, &future).unwrap_err().code(),
                ErrorCode::Unsupported
            );
        }
    }

    fn encoded_check_constraint() -> Vec<u8> {
        encode_constraint_definition(
            &ConstraintDef::new(
                ConstraintId::new(7).unwrap(),
                TableId::new(3).unwrap(),
                ConstraintKind::Check,
                0,
                None,
                None,
                vec![0xaa, 0xbb].into_boxed_slice(),
                CatalogName::new("CK_T").unwrap(),
            )
            .unwrap(),
        )
    }

    fn encoded_foreign_key_constraint() -> Vec<u8> {
        encode_constraint_definition(
            &ConstraintDef::new(
                ConstraintId::new(7).unwrap(),
                TableId::new(3).unwrap(),
                ConstraintKind::ForeignKey,
                2,
                None,
                Some(ForeignKeyTarget::new(
                    TableId::new(5).unwrap(),
                    ConstraintId::new(11).unwrap(),
                )),
                Box::new([]),
                CatalogName::new("FK_T_PARENT").unwrap(),
            )
            .unwrap(),
        )
    }

    fn foreign_key_shape_conflicts(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut conflicts = Vec::new();

        let mut wrong_key_id = bytes.to_vec();
        wrong_key_id[8..16].copy_from_slice(&8_u64.to_le_bytes());
        conflicts.push(wrong_key_id);

        for range in [36..44, 44..52] {
            let mut missing_parent_id = bytes.to_vec();
            missing_parent_id[range].fill(0);
            conflicts.push(missing_parent_id);
        }
        let mut missing_target = bytes.to_vec();
        missing_target[36..52].fill(0);
        conflicts.push(missing_target);

        let mut system_index = bytes.to_vec();
        system_index[28..36].copy_from_slice(&13_u64.to_le_bytes());
        conflicts.push(system_index);

        let mut check_program = bytes.to_vec();
        check_program[56..60].copy_from_slice(&1_u32.to_le_bytes());
        check_program.insert(64, 0xaa);
        let payload_len = u32::from_le_bytes(check_program[4..8].try_into().unwrap()) + 1;
        check_program[4..8].copy_from_slice(&payload_len.to_le_bytes());
        conflicts.push(check_program);

        for offset in [52, 53, 54, 55, 62] {
            let mut nonzero_mode_action_or_reserved = bytes.to_vec();
            nonzero_mode_action_or_reserved[offset] = 1;
            conflicts.push(nonzero_mode_action_or_reserved);
        }

        for column_count in [0, MAX_COLUMNS + 1] {
            let mut invalid_column_count = bytes.to_vec();
            invalid_column_count[26..28].copy_from_slice(&column_count.to_le_bytes());
            conflicts.push(invalid_column_count);
        }

        conflicts
    }

    #[test]
    fn foreign_key_definition_rejects_every_truncation_and_invalid_shape() {
        let bytes = encoded_foreign_key_constraint();
        for end in 0..bytes.len() {
            assert_eq!(
                decode_constraint_definition(ConstraintId::new(7).unwrap(), &bytes[..end])
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption,
                "truncation at {end}"
            );
        }
        for (case, mutation) in foreign_key_shape_conflicts(&bytes).into_iter().enumerate() {
            assert_eq!(
                decode_constraint_definition(ConstraintId::new(7).unwrap(), &mutation)
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption,
                "shape conflict {case}"
            );
        }

        let mut trailing = bytes;
        trailing.push(0);
        assert_constraint_corruption(trailing);
    }

    fn assert_constraint_corruption(bytes: Vec<u8>) {
        assert_eq!(
            decode_constraint_definition(ConstraintId::new(7).unwrap(), &bytes)
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn constraint_definition_rejects_every_truncation_reserved_byte_and_shape_conflict() {
        let bytes = encoded_check_constraint();
        for end in 0..bytes.len() {
            assert_eq!(
                decode_constraint_definition(ConstraintId::new(7).unwrap(), &bytes[..end])
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption,
                "truncation at {end}"
            );
        }
        for offset in [25, 52, 55, 62, 63] {
            let mut mutated = bytes.clone();
            mutated[offset] = 1;
            assert_constraint_corruption(mutated);
        }
        for (range, replacement) in [
            (26..28, 1_u16.to_le_bytes().to_vec()),
            (28..36, 1_u64.to_le_bytes().to_vec()),
        ] {
            let mut mutated = bytes.clone();
            mutated[range].copy_from_slice(&replacement);
            assert_constraint_corruption(mutated);
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert_constraint_corruption(trailing);
    }

    #[test]
    fn constraint_definition_rejects_invalid_ids_kind_name_lengths_and_program_lengths() {
        let valid = encoded_check_constraint();
        for range in [8..16, 16..24] {
            let mut mutated = valid.clone();
            mutated[range].fill(0);
            assert_constraint_corruption(mutated);
        }
        for kind in [0, 6, u8::MAX] {
            let mut mutated = valid.clone();
            mutated[24] = kind;
            assert_constraint_corruption(mutated);
        }

        let mut wrong_id = valid.clone();
        wrong_id[8..16].copy_from_slice(&8_u64.to_le_bytes());
        assert_constraint_corruption(wrong_id);

        let mut invalid_utf8 = valid.clone();
        invalid_utf8[66] = 0xff;
        assert_constraint_corruption(invalid_utf8);

        let mut long_name = valid.clone();
        long_name[60..62].copy_from_slice(&129_u16.to_le_bytes());
        long_name.truncate(66);
        long_name.extend(std::iter::repeat_n(b'a', 129));
        long_name[4..8].copy_from_slice(&187_u32.to_le_bytes());
        assert_constraint_corruption(long_name);

        for program_len in [0_u32, 3, 1_048_577, u32::MAX] {
            let mut mutated = valid.clone();
            mutated[56..60].copy_from_slice(&program_len.to_le_bytes());
            assert_constraint_corruption(mutated);
        }

        let mut future = valid;
        future[1] = 2;
        assert_eq!(
            decode_constraint_definition(ConstraintId::new(7).unwrap(), &future)
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn local_constraints_reject_foreign_key_fields_and_actions_as_corruption() {
        let mut bytes = encoded_check_constraint();
        bytes[24] = 5;
        assert_constraint_corruption(bytes);

        for range in [36..44, 44..52] {
            let mut mutated = encoded_check_constraint();
            mutated[range].copy_from_slice(&1_u64.to_be_bytes());
            assert_constraint_corruption(mutated);
        }

        for offset in [53, 54] {
            let mut mutated = encoded_check_constraint();
            mutated[offset] = 1;
            assert_constraint_corruption(mutated);
        }
    }

    #[test]
    fn referenced_by_mapping_rejects_every_truncation_zero_id_and_future_version() {
        let bytes = encode_referenced_by_mapping(ConstraintId::new(7).unwrap());
        for end in 0..bytes.len() {
            assert_eq!(
                decode_referenced_by_mapping(&bytes[..end])
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption,
                "truncation at {end}"
            );
        }

        let mut zero_id = bytes.clone();
        zero_id[8..16].fill(0);
        assert_eq!(
            decode_referenced_by_mapping(&zero_id).unwrap_err().code(),
            ErrorCode::Corruption
        );

        let mut future = bytes;
        future[1] = 2;
        assert_eq!(
            decode_referenced_by_mapping(&future).unwrap_err().code(),
            ErrorCode::Unsupported
        );
    }
}
