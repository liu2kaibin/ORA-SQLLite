use std::ops::Bound;

use crate::{DbError, ErrorCode, Result};

use super::types::{
    CatalogName, ColumnId, ConstraintId, IndexId, MAX_GLOBAL_CATALOG_NAME_BYTES, TableId,
};

const MAX_KEY_BYTES: usize = 1024;
const MAX_CATALOG_NAME_BYTES: usize = 1013;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum RecordKind {
    CatalogMeta = 0x00,
    TableName = 0x10,
    TableDefinition = 0x11,
    ColumnName = 0x12,
    ColumnOrdinal = 0x13,
    ColumnDefinition = 0x14,
    IndexName = 0x20,
    IndexDefinition = 0x21,
    TableIndex = 0x22,
    IndexColumn = 0x23,
    ConstraintName = 0x30,
    ConstraintDefinition = 0x31,
    TableConstraint = 0x32,
    ConstraintColumn = 0x33,
    ReferencedBy = 0x34,
}

impl RecordKind {
    pub(super) const fn byte(self) -> u8 {
        self as u8
    }

    pub(super) fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            0x00 => Ok(Self::CatalogMeta),
            0x10 => Ok(Self::TableName),
            0x11 => Ok(Self::TableDefinition),
            0x12 => Ok(Self::ColumnName),
            0x13 => Ok(Self::ColumnOrdinal),
            0x14 => Ok(Self::ColumnDefinition),
            0x20 => Ok(Self::IndexName),
            0x21 => Ok(Self::IndexDefinition),
            0x22 => Ok(Self::TableIndex),
            0x23 => Ok(Self::IndexColumn),
            0x30 => Ok(Self::ConstraintName),
            0x31 => Ok(Self::ConstraintDefinition),
            0x32 => Ok(Self::TableConstraint),
            0x33 => Ok(Self::ConstraintColumn),
            0x34 => Ok(Self::ReferencedBy),
            _ => Err(unsupported("unsupported catalog record family")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DecodedCatalogKey<'a> {
    CatalogMeta,
    TableName(&'a str),
    TableDefinition(TableId),
    ColumnName {
        table_id: TableId,
        name: &'a str,
    },
    ColumnOrdinal {
        table_id: TableId,
        ordinal: u16,
    },
    ColumnDefinition(ColumnId),
    IndexName(&'a str),
    IndexDefinition(IndexId),
    TableIndex {
        table_id: TableId,
        index_id: IndexId,
    },
    IndexColumn {
        index_id: IndexId,
        ordinal: u16,
    },
    ConstraintName(&'a str),
    ConstraintDefinition(ConstraintId),
    TableConstraint {
        table_id: TableId,
        constraint_id: ConstraintId,
    },
    ConstraintColumn {
        constraint_id: ConstraintId,
        ordinal: u16,
    },
    ReferencedBy {
        parent_id: ConstraintId,
        child_id: ConstraintId,
    },
}

pub(super) fn catalog_meta_key() -> Vec<u8> {
    vec![RecordKind::CatalogMeta.byte()]
}

pub(super) fn table_name_key(name: &CatalogName) -> Result<Vec<u8>> {
    name_key(RecordKind::TableName, &[], name)
}

pub(super) fn table_definition_key(id: TableId) -> Vec<u8> {
    id_key(RecordKind::TableDefinition, id.get())
}

pub(super) fn column_name_key(table: TableId, name: &CatalogName) -> Result<Vec<u8>> {
    name_key(RecordKind::ColumnName, &table.get().to_be_bytes(), name)
}

pub(super) fn column_ordinal_key(table: TableId, ordinal: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(11);
    key.push(RecordKind::ColumnOrdinal.byte());
    key.extend_from_slice(&table.get().to_be_bytes());
    key.extend_from_slice(&ordinal.to_be_bytes());
    key
}

pub(super) fn column_definition_key(id: ColumnId) -> Vec<u8> {
    id_key(RecordKind::ColumnDefinition, id.get())
}

pub(super) fn index_name_key(name: &CatalogName) -> Result<Vec<u8>> {
    global_name_key(RecordKind::IndexName, name)
}

pub(super) fn index_definition_key(id: IndexId) -> Vec<u8> {
    id_key(RecordKind::IndexDefinition, id.get())
}

pub(super) fn table_index_key(table: TableId, index: IndexId) -> Vec<u8> {
    two_id_key(RecordKind::TableIndex, table.get(), index.get())
}

pub(super) fn index_column_key(index: IndexId, ordinal: u16) -> Vec<u8> {
    id_ordinal_key(RecordKind::IndexColumn, index.get(), ordinal)
}

pub(super) fn constraint_name_key(name: &CatalogName) -> Result<Vec<u8>> {
    global_name_key(RecordKind::ConstraintName, name)
}

pub(super) fn constraint_definition_key(id: ConstraintId) -> Vec<u8> {
    id_key(RecordKind::ConstraintDefinition, id.get())
}

pub(super) fn table_constraint_key(table: TableId, constraint: ConstraintId) -> Vec<u8> {
    two_id_key(RecordKind::TableConstraint, table.get(), constraint.get())
}

pub(super) fn constraint_column_key(constraint: ConstraintId, ordinal: u16) -> Vec<u8> {
    id_ordinal_key(RecordKind::ConstraintColumn, constraint.get(), ordinal)
}

pub(super) fn referenced_by_key(parent: ConstraintId, child: ConstraintId) -> Vec<u8> {
    two_id_key(RecordKind::ReferencedBy, parent.get(), child.get())
}

pub(super) fn decode_key(bytes: &[u8]) -> Result<DecodedCatalogKey<'_>> {
    let family = *bytes
        .first()
        .ok_or_else(|| corruption("catalog key is empty"))?;
    let kind = RecordKind::from_byte(family)?;
    if bytes.len() > MAX_KEY_BYTES {
        return Err(corruption("catalog key exceeds 1024 bytes"));
    }

    match kind {
        RecordKind::CatalogMeta => {
            require_length(bytes, 1)?;
            Ok(DecodedCatalogKey::CatalogMeta)
        }
        RecordKind::TableName => {
            let name = decode_name(bytes, 1)?;
            Ok(DecodedCatalogKey::TableName(name))
        }
        RecordKind::TableDefinition => {
            require_length(bytes, 9)?;
            Ok(DecodedCatalogKey::TableDefinition(decode_table_id(
                &bytes[1..9],
            )?))
        }
        RecordKind::ColumnName => {
            if bytes.len() < 9 {
                return Err(corruption("column name key is truncated"));
            }
            let table_id = decode_table_id(&bytes[1..9])?;
            let name = decode_name(bytes, 9)?;
            Ok(DecodedCatalogKey::ColumnName { table_id, name })
        }
        RecordKind::ColumnOrdinal => {
            require_length(bytes, 11)?;
            let table_id = decode_table_id(&bytes[1..9])?;
            let ordinal = u16::from_be_bytes(
                bytes[9..11]
                    .try_into()
                    .map_err(|_| corruption("column ordinal key is truncated"))?,
            );
            Ok(DecodedCatalogKey::ColumnOrdinal { table_id, ordinal })
        }
        RecordKind::ColumnDefinition => {
            require_length(bytes, 9)?;
            Ok(DecodedCatalogKey::ColumnDefinition(decode_column_id(
                &bytes[1..9],
            )?))
        }
        RecordKind::IndexName => Ok(DecodedCatalogKey::IndexName(decode_global_name(bytes)?)),
        RecordKind::IndexDefinition => {
            require_length(bytes, 9)?;
            Ok(DecodedCatalogKey::IndexDefinition(decode_index_id(
                &bytes[1..9],
            )?))
        }
        RecordKind::TableIndex => {
            require_length(bytes, 17)?;
            Ok(DecodedCatalogKey::TableIndex {
                table_id: decode_table_id(&bytes[1..9])?,
                index_id: decode_index_id(&bytes[9..17])?,
            })
        }
        RecordKind::IndexColumn => {
            require_length(bytes, 11)?;
            Ok(DecodedCatalogKey::IndexColumn {
                index_id: decode_index_id(&bytes[1..9])?,
                ordinal: decode_ordinal(&bytes[9..11], "index column key is truncated")?,
            })
        }
        RecordKind::ConstraintName => Ok(DecodedCatalogKey::ConstraintName(decode_global_name(
            bytes,
        )?)),
        RecordKind::ConstraintDefinition => {
            require_length(bytes, 9)?;
            Ok(DecodedCatalogKey::ConstraintDefinition(
                decode_constraint_id(&bytes[1..9])?,
            ))
        }
        RecordKind::TableConstraint => {
            require_length(bytes, 17)?;
            Ok(DecodedCatalogKey::TableConstraint {
                table_id: decode_table_id(&bytes[1..9])?,
                constraint_id: decode_constraint_id(&bytes[9..17])?,
            })
        }
        RecordKind::ConstraintColumn => {
            require_length(bytes, 11)?;
            Ok(DecodedCatalogKey::ConstraintColumn {
                constraint_id: decode_constraint_id(&bytes[1..9])?,
                ordinal: decode_ordinal(&bytes[9..11], "constraint column key is truncated")?,
            })
        }
        RecordKind::ReferencedBy => {
            require_length(bytes, 17)?;
            Ok(DecodedCatalogKey::ReferencedBy {
                parent_id: decode_constraint_id(&bytes[1..9])?,
                child_id: decode_constraint_id(&bytes[9..17])?,
            })
        }
    }
}

pub(super) fn prefix_bounds(prefix: &[u8]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = Bound::Included(prefix.to_vec());
    let mut end = prefix.to_vec();
    let upper = match end.iter().rposition(|byte| *byte != 0xff) {
        Some(index) => {
            end[index] += 1;
            end.truncate(index + 1);
            Bound::Excluded(end)
        }
        None => Bound::Unbounded,
    };
    (start, upper)
}

fn id_key(kind: RecordKind, id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(9);
    key.push(kind.byte());
    key.extend_from_slice(&id.to_be_bytes());
    key
}

fn two_id_key(kind: RecordKind, first: u64, second: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(17);
    key.push(kind.byte());
    key.extend_from_slice(&first.to_be_bytes());
    key.extend_from_slice(&second.to_be_bytes());
    key
}

fn id_ordinal_key(kind: RecordKind, id: u64, ordinal: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(11);
    key.push(kind.byte());
    key.extend_from_slice(&id.to_be_bytes());
    key.extend_from_slice(&ordinal.to_be_bytes());
    key
}

fn global_name_key(kind: RecordKind, name: &CatalogName) -> Result<Vec<u8>> {
    if name.as_bytes().len() > MAX_GLOBAL_CATALOG_NAME_BYTES {
        return Err(DbError::new(
            ErrorCode::Type,
            "global catalog name must not exceed 128 UTF-8 bytes",
        ));
    }
    name_key(kind, &[], name)
}

fn name_key(kind: RecordKind, scope: &[u8], name: &CatalogName) -> Result<Vec<u8>> {
    let name_len = u16::try_from(name.as_bytes().len())
        .map_err(|_| DbError::new(ErrorCode::Type, "catalog name length exceeds u16"))?;
    let key_len = 1usize
        .checked_add(scope.len())
        .and_then(|length| length.checked_add(2))
        .and_then(|length| length.checked_add(usize::from(name_len)))
        .ok_or_else(|| DbError::new(ErrorCode::Type, "catalog key length overflow"))?;
    if key_len > MAX_KEY_BYTES {
        return Err(DbError::new(
            ErrorCode::Type,
            "catalog key must be at most 1024 bytes",
        ));
    }

    let mut key = Vec::with_capacity(key_len);
    key.push(kind.byte());
    key.extend_from_slice(scope);
    key.extend_from_slice(&name_len.to_be_bytes());
    key.extend_from_slice(name.as_bytes());
    Ok(key)
}

fn decode_name(bytes: &[u8], length_offset: usize) -> Result<&str> {
    decode_name_with_limit(bytes, length_offset, MAX_CATALOG_NAME_BYTES)
}

fn decode_global_name(bytes: &[u8]) -> Result<&str> {
    decode_name_with_limit(bytes, 1, MAX_GLOBAL_CATALOG_NAME_BYTES)
}

fn decode_name_with_limit(bytes: &[u8], length_offset: usize, max_len: usize) -> Result<&str> {
    let length_end = length_offset
        .checked_add(2)
        .ok_or_else(|| corruption("catalog name length offset overflow"))?;
    let length_bytes = bytes
        .get(length_offset..length_end)
        .ok_or_else(|| corruption("catalog name key is truncated"))?;
    let name_len = usize::from(u16::from_be_bytes(
        length_bytes
            .try_into()
            .map_err(|_| corruption("catalog name length is truncated"))?,
    ));
    if name_len == 0 || name_len > max_len {
        return Err(corruption("catalog name length is invalid"));
    }
    let name_end = length_end
        .checked_add(name_len)
        .ok_or_else(|| corruption("catalog name length overflow"))?;
    if name_end != bytes.len() {
        return Err(corruption(
            "catalog name key length does not match its prefix",
        ));
    }
    std::str::from_utf8(&bytes[length_end..name_end])
        .map_err(|_| corruption("catalog name key is not valid UTF-8"))
}

fn decode_table_id(bytes: &[u8]) -> Result<TableId> {
    let value = decode_u64(bytes)?;
    TableId::new(value).map_err(|_| corruption("catalog table id must be nonzero"))
}

fn decode_column_id(bytes: &[u8]) -> Result<ColumnId> {
    let value = decode_u64(bytes)?;
    ColumnId::new(value).map_err(|_| corruption("catalog column id must be nonzero"))
}

fn decode_index_id(bytes: &[u8]) -> Result<IndexId> {
    IndexId::new(decode_u64(bytes)?).map_err(|_| corruption("catalog index id must be nonzero"))
}

fn decode_constraint_id(bytes: &[u8]) -> Result<ConstraintId> {
    ConstraintId::new(decode_u64(bytes)?)
        .map_err(|_| corruption("catalog constraint id must be nonzero"))
}

fn decode_ordinal(bytes: &[u8], message: &'static str) -> Result<u16> {
    Ok(u16::from_be_bytes(
        bytes.try_into().map_err(|_| corruption(message))?,
    ))
}

fn decode_u64(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| {
        corruption("catalog id has an invalid length")
    })?))
}

fn require_length(bytes: &[u8], expected: usize) -> Result<()> {
    if bytes.len() != expected {
        return Err(corruption("catalog key has an invalid length"));
    }
    Ok(())
}

fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}

fn unsupported(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use super::*;
    use crate::{
        ErrorCode,
        storage::catalog::types::{CatalogName, ColumnId, ConstraintId, IndexId, TableId},
    };

    #[test]
    fn record_kind_codes_are_stable_and_checked() {
        let cases = [
            (RecordKind::CatalogMeta, 0x00),
            (RecordKind::TableName, 0x10),
            (RecordKind::TableDefinition, 0x11),
            (RecordKind::ColumnName, 0x12),
            (RecordKind::ColumnOrdinal, 0x13),
            (RecordKind::ColumnDefinition, 0x14),
            (RecordKind::IndexName, 0x20),
            (RecordKind::IndexDefinition, 0x21),
            (RecordKind::TableIndex, 0x22),
            (RecordKind::IndexColumn, 0x23),
            (RecordKind::ConstraintName, 0x30),
            (RecordKind::ConstraintDefinition, 0x31),
            (RecordKind::TableConstraint, 0x32),
            (RecordKind::ConstraintColumn, 0x33),
            (RecordKind::ReferencedBy, 0x34),
        ];

        for (kind, byte) in cases {
            assert_eq!(kind.byte(), byte);
            assert_eq!(RecordKind::from_byte(byte).unwrap(), kind);
        }
        for byte in [0x01, 0x0f, 0x15, 0x1f, 0x24, 0x2f, 0x35, 0x3f, 0x40, 0xff] {
            assert_eq!(
                RecordKind::from_byte(byte).unwrap_err().code(),
                ErrorCode::Unsupported
            );
        }
    }

    #[test]
    fn exact_key_builders_emit_hand_computed_bytes() {
        let table = TableId::new(0x0102_0304_0506_0708).unwrap();
        let column = ColumnId::new(0x1112_1314_1516_1718).unwrap();
        let table_name = CatalogName::new("EMP").unwrap();
        let column_name = CatalogName::new("表a").unwrap();

        assert_eq!(catalog_meta_key(), vec![0x00]);
        assert_eq!(
            table_name_key(&table_name).unwrap(),
            vec![0x10, 0, 3, b'E', b'M', b'P']
        );
        assert_eq!(
            table_definition_key(table),
            vec![0x11, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            column_name_key(table, &column_name).unwrap(),
            vec![0x12, 1, 2, 3, 4, 5, 6, 7, 8, 0, 4, 0xe8, 0xa1, 0xa8, b'a']
        );
        assert_eq!(
            column_definition_key(column),
            vec![0x14, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18]
        );
    }

    #[test]
    fn column_ordinal_key_is_big_endian_and_order_preserving() {
        let table = TableId::new(0x0102_0304_0506_0708).unwrap();
        assert_eq!(
            column_ordinal_key(table, 9),
            vec![0x13, 1, 2, 3, 4, 5, 6, 7, 8, 0, 9]
        );
        assert!(column_ordinal_key(table, 255) < column_ordinal_key(table, 256));
    }

    #[test]
    fn decode_key_round_trips_every_known_family() {
        let table = TableId::new(7).unwrap();
        let column = ColumnId::new(9).unwrap();
        let index = IndexId::new(11).unwrap();
        let constraint = ConstraintId::new(13).unwrap();
        let table_name = CatalogName::new("EMP").unwrap();
        let column_name = CatalogName::new("表a").unwrap();

        let cases = [
            (catalog_meta_key(), DecodedCatalogKey::CatalogMeta),
            (
                table_name_key(&table_name).unwrap(),
                DecodedCatalogKey::TableName("EMP"),
            ),
            (
                table_definition_key(table),
                DecodedCatalogKey::TableDefinition(table),
            ),
            (
                column_name_key(table, &column_name).unwrap(),
                DecodedCatalogKey::ColumnName {
                    table_id: table,
                    name: "表a",
                },
            ),
            (
                column_ordinal_key(table, 256),
                DecodedCatalogKey::ColumnOrdinal {
                    table_id: table,
                    ordinal: 256,
                },
            ),
            (
                column_definition_key(column),
                DecodedCatalogKey::ColumnDefinition(column),
            ),
            (
                index_name_key(&table_name).unwrap(),
                DecodedCatalogKey::IndexName("EMP"),
            ),
            (
                index_definition_key(index),
                DecodedCatalogKey::IndexDefinition(index),
            ),
            (
                table_index_key(table, index),
                DecodedCatalogKey::TableIndex {
                    table_id: table,
                    index_id: index,
                },
            ),
            (
                index_column_key(index, 2),
                DecodedCatalogKey::IndexColumn {
                    index_id: index,
                    ordinal: 2,
                },
            ),
            (
                constraint_name_key(&table_name).unwrap(),
                DecodedCatalogKey::ConstraintName("EMP"),
            ),
            (
                constraint_definition_key(constraint),
                DecodedCatalogKey::ConstraintDefinition(constraint),
            ),
            (
                table_constraint_key(table, constraint),
                DecodedCatalogKey::TableConstraint {
                    table_id: table,
                    constraint_id: constraint,
                },
            ),
            (
                constraint_column_key(constraint, 2),
                DecodedCatalogKey::ConstraintColumn {
                    constraint_id: constraint,
                    ordinal: 2,
                },
            ),
            (
                referenced_by_key(constraint, ConstraintId::new(15).unwrap()),
                DecodedCatalogKey::ReferencedBy {
                    parent_id: constraint,
                    child_id: ConstraintId::new(15).unwrap(),
                },
            ),
        ];

        for (bytes, expected) in &cases {
            assert_eq!(decode_key(bytes).unwrap(), *expected);
        }
    }

    #[test]
    fn known_families_reject_malformed_keys_as_corruption() {
        let malformed: &[&[u8]] = &[
            &[],
            &[0x00, 0],
            &[0x10],
            &[0x10, 0, 0],
            &[0x10, 0, 2, b'a'],
            &[0x10, 0, 1, b'a', b'b'],
            &[0x10, 0, 1, 0xff],
            &[0x11],
            &[0x11, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0x11, 0, 0, 0, 0, 0, 0, 0, 1, 0],
            &[0x12, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
            &[0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, b'a'],
            &[0x12, 0, 0, 0, 0, 0, 0, 0, 1, 0, 2, b'a'],
            &[0x12, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, b'a', b'b'],
            &[0x13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            &[0x13, 0, 0, 0, 0, 0, 0, 0, 1, 0],
            &[0x13, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0],
            &[0x14, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0x14, 0, 0, 0, 0, 0, 0, 0, 1, 0],
        ];

        for bytes in malformed {
            assert_eq!(decode_key(bytes).unwrap_err().code(), ErrorCode::Corruption);
        }

        let oversized = vec![0x10; 1025];
        assert_eq!(
            decode_key(&oversized).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn unknown_key_families_are_unsupported() {
        for bytes in [&[0x01][..], &[0x3f][..], &[0x40][..], &[0xff][..]] {
            assert_eq!(
                decode_key(bytes).unwrap_err().code(),
                ErrorCode::Unsupported
            );
        }

        let mut oversized = vec![0; 1025];
        oversized[0] = 0x40;
        assert_eq!(
            decode_key(&oversized).unwrap_err().code(),
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn prefix_bounds_use_the_shortest_exclusive_successor() {
        assert_eq!(
            prefix_bounds(&[0x13, 0xff, 0xff]),
            (
                Bound::Included(vec![0x13, 0xff, 0xff]),
                Bound::Excluded(vec![0x14])
            )
        );
        assert_eq!(
            prefix_bounds(&[0x12, 0x01, 0xff]),
            (
                Bound::Included(vec![0x12, 0x01, 0xff]),
                Bound::Excluded(vec![0x12, 0x02])
            )
        );
        assert_eq!(
            prefix_bounds(&[0xff, 0xff]),
            (Bound::Included(vec![0xff, 0xff]), Bound::Unbounded)
        );
        assert_eq!(
            prefix_bounds(&[]),
            (Bound::Included(vec![]), Bound::Unbounded)
        );
    }

    #[test]
    fn phase_4c2_catalog_keys_have_stable_big_endian_bytes() {
        let table = TableId::new(0x0102_0304_0506_0708).unwrap();
        let index = IndexId::new(0x1112_1314_1516_1718).unwrap();
        let constraint = ConstraintId::new(0x2122_2324_2526_2728).unwrap();
        let name = CatalogName::new("IX_A").unwrap();
        assert_eq!(index_name_key(&name).unwrap(), b"\x20\x00\x04IX_A");
        assert_eq!(
            index_definition_key(index),
            [vec![0x21], index.get().to_be_bytes().to_vec()].concat()
        );
        assert_eq!(
            table_index_key(table, index),
            [
                vec![0x22],
                table.get().to_be_bytes().to_vec(),
                index.get().to_be_bytes().to_vec()
            ]
            .concat()
        );
        assert_eq!(
            index_column_key(index, 2),
            [vec![0x23], index.get().to_be_bytes().to_vec(), vec![0, 2]].concat()
        );
        assert_eq!(constraint_name_key(&name).unwrap()[0], 0x30);
        assert_eq!(constraint_definition_key(constraint)[0], 0x31);
        assert_eq!(table_constraint_key(table, constraint)[0], 0x32);
        assert_eq!(constraint_column_key(constraint, 2)[0], 0x33);
        assert_eq!(
            referenced_by_key(constraint, ConstraintId::new(41).unwrap())[0],
            0x34
        );
    }

    #[test]
    fn phase_4c2_key_decoder_rejects_every_truncation_and_zero_id() {
        let table = TableId::new(7).unwrap();
        let index = IndexId::new(9).unwrap();
        let constraint = ConstraintId::new(11).unwrap();
        let name = CatalogName::new("IX_A").unwrap();
        let complete = [
            index_name_key(&name).unwrap(),
            index_definition_key(index),
            table_index_key(table, index),
            index_column_key(index, 2),
            constraint_name_key(&name).unwrap(),
            constraint_definition_key(constraint),
            table_constraint_key(table, constraint),
            constraint_column_key(constraint, 2),
            referenced_by_key(constraint, ConstraintId::new(13).unwrap()),
        ];
        for key in complete {
            for end in 0..key.len() {
                assert_eq!(
                    decode_key(&key[..end]).unwrap_err().code(),
                    ErrorCode::Corruption
                );
            }
        }

        for (mut key, range) in [
            (index_definition_key(index), 1..9),
            (table_index_key(table, index), 1..9),
            (table_index_key(table, index), 9..17),
            (index_column_key(index, 2), 1..9),
            (constraint_definition_key(constraint), 1..9),
            (table_constraint_key(table, constraint), 1..9),
            (table_constraint_key(table, constraint), 9..17),
            (constraint_column_key(constraint, 2), 1..9),
            (
                referenced_by_key(constraint, ConstraintId::new(13).unwrap()),
                1..9,
            ),
            (
                referenced_by_key(constraint, ConstraintId::new(13).unwrap()),
                9..17,
            ),
        ] {
            key[range].fill(0);
            assert_eq!(decode_key(&key).unwrap_err().code(), ErrorCode::Corruption);
        }
    }

    #[test]
    fn global_name_keys_reject_names_over_128_bytes() {
        let name = CatalogName::new("a".repeat(129)).unwrap();
        assert_eq!(index_name_key(&name).unwrap_err().code(), ErrorCode::Type);
        assert_eq!(
            constraint_name_key(&name).unwrap_err().code(),
            ErrorCode::Type
        );

        let mut encoded = vec![RecordKind::IndexName.byte(), 0, 129];
        encoded.extend(std::iter::repeat_n(b'a', 129));
        assert_eq!(
            decode_key(&encoded).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }

    #[test]
    fn table_index_and_index_column_keys_sort_by_owner_then_child() {
        let table = TableId::new(7).unwrap();
        assert!(
            table_index_key(table, IndexId::new(8).unwrap())
                < table_index_key(table, IndexId::new(9).unwrap())
        );
        let index = IndexId::new(11).unwrap();
        assert!(index_column_key(index, 255) < index_column_key(index, 256));
    }
}
