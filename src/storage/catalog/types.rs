use std::num::NonZeroU64;

use crate::{DbError, ErrorCode, Result, storage::PageRef};

const MAX_CATALOG_NAME_BYTES: usize = 1013;
const MAX_COLUMNS: u16 = 1000;

macro_rules! nonzero_id {
    ($name:ident, $message:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) struct $name(NonZeroU64);

        impl $name {
            pub(crate) fn new(value: u64) -> Result<Self> {
                NonZeroU64::new(value)
                    .map(Self)
                    .ok_or_else(|| DbError::new(ErrorCode::Type, $message))
            }

            pub(crate) const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

nonzero_id!(ObjectId, "object id must be nonzero");
nonzero_id!(TableId, "table id must be nonzero");
nonzero_id!(ColumnId, "column id must be nonzero");
nonzero_id!(IndexId, "index id must be nonzero");
nonzero_id!(ConstraintId, "constraint id must be nonzero");
nonzero_id!(SchemaVersion, "schema version must be nonzero");
nonzero_id!(RowId, "row id must be nonzero");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Table,
    Column,
    Index,
    Constraint,
}

macro_rules! object_id_conversion {
    ($typed:ident, $kind:ident, $message:literal) => {
        impl From<$typed> for ObjectId {
            fn from(value: $typed) -> Self {
                Self(value.0)
            }
        }

        impl $typed {
            pub(crate) fn from_object(object: ObjectId, kind: ObjectKind) -> Result<Self> {
                if kind != ObjectKind::$kind {
                    return Err(DbError::new(ErrorCode::Type, $message));
                }
                Ok(Self(object.0))
            }
        }
    };
}

object_id_conversion!(TableId, Table, "object kind does not match table id");
object_id_conversion!(ColumnId, Column, "object kind does not match column id");
object_id_conversion!(IndexId, Index, "object kind does not match index id");
object_id_conversion!(
    ConstraintId,
    Constraint,
    "object kind does not match constraint id"
);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CatalogName(String);

impl CatalogName {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(DbError::new(
                ErrorCode::Type,
                "catalog name must not be empty",
            ));
        }
        if value.len() > MAX_CATALOG_NAME_BYTES {
            return Err(DbError::new(
                ErrorCode::Type,
                "catalog name must not exceed 1013 UTF-8 bytes",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum ColumnType {
    Int64 = 1,
    Decimal = 2,
    Float64 = 3,
    Text = 4,
    Blob = 5,
    Date = 6,
    Timestamp = 7,
    TimestampWithTimeZone = 8,
    Bool = 9,
}

impl ColumnType {
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    pub(crate) fn from_code(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::Int64),
            2 => Ok(Self::Decimal),
            3 => Ok(Self::Float64),
            4 => Ok(Self::Text),
            5 => Ok(Self::Blob),
            6 => Ok(Self::Date),
            7 => Ok(Self::Timestamp),
            8 => Ok(Self::TimestampWithTimeZone),
            9 => Ok(Self::Bool),
            _ => Err(DbError::new(ErrorCode::Type, "unknown column type code")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogMeta {
    next_object_id: ObjectId,
}

impl CatalogMeta {
    pub(crate) const fn new(next_object_id: ObjectId) -> Self {
        Self { next_object_id }
    }

    pub(crate) const fn next_object_id(&self) -> ObjectId {
        self.next_object_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableDef {
    id: TableId,
    schema_version: SchemaVersion,
    root: PageRef,
    next_rowid: RowId,
    column_count: u16,
    name: CatalogName,
}

impl TableDef {
    pub(crate) fn new(
        id: TableId,
        schema_version: SchemaVersion,
        root: PageRef,
        next_rowid: RowId,
        column_count: u16,
        name: CatalogName,
    ) -> Result<Self> {
        if column_count > MAX_COLUMNS {
            return Err(DbError::new(
                ErrorCode::Type,
                "table column count must not exceed 1000",
            ));
        }
        Ok(Self {
            id,
            schema_version,
            root,
            next_rowid,
            column_count,
            name,
        })
    }

    pub(crate) const fn id(&self) -> TableId {
        self.id
    }
    pub(crate) const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }
    pub(crate) const fn root(&self) -> PageRef {
        self.root
    }
    pub(crate) const fn next_rowid(&self) -> RowId {
        self.next_rowid
    }
    pub(crate) const fn column_count(&self) -> u16 {
        self.column_count
    }
    pub(crate) const fn name(&self) -> &CatalogName {
        &self.name
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDef {
    id: ColumnId,
    table_id: TableId,
    ordinal: u16,
    column_type: ColumnType,
    nullable: bool,
    name: CatalogName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatalogRecord {
    CatalogMeta(CatalogMeta),
    TableName(TableId),
    TableDefinition(TableDef),
    ColumnName(ColumnId),
    ColumnOrdinal(ColumnId),
    ColumnDefinition(ColumnDef),
}

impl ColumnDef {
    pub(crate) fn new(
        id: ColumnId,
        table_id: TableId,
        ordinal: u16,
        column_type: ColumnType,
        nullable: bool,
        name: CatalogName,
    ) -> Result<Self> {
        if ordinal >= MAX_COLUMNS {
            return Err(DbError::new(
                ErrorCode::Type,
                "column ordinal must be below 1000",
            ));
        }
        Ok(Self {
            id,
            table_id,
            ordinal,
            column_type,
            nullable,
            name,
        })
    }

    pub(crate) const fn id(&self) -> ColumnId {
        self.id
    }
    pub(crate) const fn table_id(&self) -> TableId {
        self.table_id
    }
    pub(crate) const fn ordinal(&self) -> u16 {
        self.ordinal
    }
    pub(crate) const fn column_type(&self) -> ColumnType {
        self.column_type
    }
    pub(crate) const fn nullable(&self) -> bool {
        self.nullable
    }
    pub(crate) const fn name(&self) -> &CatalogName {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ErrorCode,
        storage::{PageId, PageRef},
    };

    #[test]
    fn stable_ids_reject_zero_and_require_explicit_kinds() {
        assert_eq!(ObjectId::new(0).unwrap_err().code(), ErrorCode::Type);
        assert_eq!(TableId::new(0).unwrap_err().code(), ErrorCode::Type);
        assert_eq!(ColumnId::new(0).unwrap_err().code(), ErrorCode::Type);
        assert_eq!(IndexId::new(0).unwrap_err().code(), ErrorCode::Type);
        assert_eq!(ConstraintId::new(0).unwrap_err().code(), ErrorCode::Type);
        assert_eq!(SchemaVersion::new(0).unwrap_err().code(), ErrorCode::Type);
        assert_eq!(RowId::new(0).unwrap_err().code(), ErrorCode::Type);

        assert_eq!(ObjectId::new(3).unwrap().get(), 3);
        assert_eq!(TableId::new(4).unwrap().get(), 4);
        assert_eq!(ColumnId::new(5).unwrap().get(), 5);
        assert_eq!(IndexId::new(6).unwrap().get(), 6);
        assert_eq!(ConstraintId::new(7).unwrap().get(), 7);
        assert_eq!(SchemaVersion::new(8).unwrap().get(), 8);
        assert_eq!(RowId::new(9).unwrap().get(), 9);

        let table = TableId::new(9).unwrap();
        let object: ObjectId = table.into();
        assert_eq!(
            TableId::from_object(object, ObjectKind::Table).unwrap(),
            table
        );
        assert_eq!(
            ColumnId::from_object(object, ObjectKind::Table)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );

        let column = ColumnId::new(10).unwrap();
        assert_eq!(
            ColumnId::from_object(column.into(), ObjectKind::Column).unwrap(),
            column
        );
        let index = IndexId::new(11).unwrap();
        assert_eq!(
            IndexId::from_object(index.into(), ObjectKind::Index).unwrap(),
            index
        );
        let constraint = ConstraintId::new(12).unwrap();
        assert_eq!(
            ConstraintId::from_object(constraint.into(), ObjectKind::Constraint).unwrap(),
            constraint
        );
    }

    #[test]
    fn catalog_names_enforce_the_conservative_utf8_key_limit() {
        assert_eq!(CatalogName::new("").unwrap_err().code(), ErrorCode::Type);

        let maximum = CatalogName::new("a".repeat(1013)).unwrap();
        assert_eq!(maximum.as_str().len(), 1013);
        assert_eq!(
            CatalogName::new("a".repeat(1014)).unwrap_err().code(),
            ErrorCode::Type
        );

        let multibyte = CatalogName::new(format!("{}a", "表".repeat(337))).unwrap();
        assert_eq!(multibyte.as_str().as_bytes().len(), 1012);
        assert_eq!(
            CatalogName::new("表".repeat(338)).unwrap_err().code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn column_type_codes_are_stable_and_checked() {
        let cases = [
            (ColumnType::Int64, 1),
            (ColumnType::Decimal, 2),
            (ColumnType::Float64, 3),
            (ColumnType::Text, 4),
            (ColumnType::Blob, 5),
            (ColumnType::Date, 6),
            (ColumnType::Timestamp, 7),
            (ColumnType::TimestampWithTimeZone, 8),
            (ColumnType::Bool, 9),
        ];

        for (column_type, code) in cases {
            assert_eq!(column_type.code(), code);
            assert_eq!(ColumnType::from_code(code).unwrap(), column_type);
        }
        assert_eq!(
            ColumnType::from_code(0).unwrap_err().code(),
            ErrorCode::Type
        );
        assert_eq!(
            ColumnType::from_code(10).unwrap_err().code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn definitions_are_immutable_checked_values() {
        let next_object_id = ObjectId::new(20).unwrap();
        let meta = CatalogMeta::new(next_object_id);
        assert_eq!(meta.next_object_id(), next_object_id);

        let table_id = TableId::new(21).unwrap();
        let schema_version = SchemaVersion::new(1).unwrap();
        let root = PageRef::new(PageId(22), 2).unwrap();
        let next_rowid = RowId::new(1).unwrap();
        let table_name = CatalogName::new("EMP").unwrap();
        assert_eq!(
            TableDef::new(
                table_id,
                schema_version,
                root,
                next_rowid,
                0,
                table_name.clone(),
            )
            .unwrap()
            .column_count(),
            0
        );
        let table = TableDef::new(
            table_id,
            schema_version,
            root,
            next_rowid,
            1000,
            table_name.clone(),
        )
        .unwrap();
        assert_eq!(table.id(), table_id);
        assert_eq!(table.schema_version(), schema_version);
        assert_eq!(table.root(), root);
        assert_eq!(table.next_rowid(), next_rowid);
        assert_eq!(table.column_count(), 1000);
        assert_eq!(table.name(), &table_name);
        assert_eq!(
            TableDef::new(table_id, schema_version, root, next_rowid, 1001, table_name)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );

        let column_id = ColumnId::new(23).unwrap();
        let column_name = CatalogName::new("ID").unwrap();
        assert_eq!(
            ColumnDef::new(
                column_id,
                table_id,
                0,
                ColumnType::Int64,
                true,
                column_name.clone(),
            )
            .unwrap()
            .ordinal(),
            0
        );
        let column = ColumnDef::new(
            column_id,
            table_id,
            999,
            ColumnType::Int64,
            false,
            column_name.clone(),
        )
        .unwrap();
        assert_eq!(column.id(), column_id);
        assert_eq!(column.table_id(), table_id);
        assert_eq!(column.ordinal(), 999);
        assert_eq!(column.column_type(), ColumnType::Int64);
        assert!(!column.nullable());
        assert_eq!(column.name(), &column_name);
        assert_eq!(
            ColumnDef::new(
                column_id,
                table_id,
                1000,
                ColumnType::Int64,
                true,
                column_name
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );
    }
}
