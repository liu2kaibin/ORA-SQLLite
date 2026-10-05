use std::num::NonZeroU64;

use crate::{DbError, ErrorCode, Result, oracle_type::OracleTypeDescriptor, storage::PageRef};

const MAX_CATALOG_NAME_BYTES: usize = 1013;
pub(super) const MAX_GLOBAL_CATALOG_NAME_BYTES: usize = 128;
pub(crate) const MAX_COLUMNS: u16 = 1000;
pub(crate) const MAX_CHECK_PROGRAM_BYTES: usize = 1024 * 1024;

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

    pub(super) fn with_root(mut self, root: PageRef) -> Self {
        self.root = root;
        self
    }

    pub(super) fn with_next_rowid(mut self, next_rowid: RowId) -> Self {
        self.next_rowid = next_rowid;
        self
    }

    pub(super) fn with_schema_version(mut self, schema_version: SchemaVersion) -> Self {
        self.schema_version = schema_version;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDef {
    id: ColumnId,
    table_id: TableId,
    ordinal: u16,
    column_type: ColumnType,
    nullable: bool,
    oracle_type: Option<OracleTypeDescriptor>,
    name: CatalogName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexOwnership {
    Explicit,
    Constraint(ConstraintId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexDef {
    id: IndexId,
    table_id: TableId,
    root: PageRef,
    column_count: u16,
    unique: bool,
    ownership: IndexOwnership,
    name: CatalogName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForeignKeyTarget {
    table_id: TableId,
    constraint_id: ConstraintId,
}

impl ForeignKeyTarget {
    pub(crate) const fn new(table_id: TableId, constraint_id: ConstraintId) -> Self {
        Self {
            table_id,
            constraint_id,
        }
    }

    pub(crate) const fn table_id(self) -> TableId {
        self.table_id
    }

    pub(crate) const fn constraint_id(self) -> ConstraintId {
        self.constraint_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintKind {
    PrimaryKey,
    Unique,
    NotNull,
    Check,
    ForeignKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConstraintDef {
    id: ConstraintId,
    table_id: TableId,
    kind: ConstraintKind,
    column_count: u16,
    index_id: Option<IndexId>,
    foreign_key_target: Option<ForeignKeyTarget>,
    check_program: Box<[u8]>,
    name: CatalogName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConstraintBundle {
    pub(crate) definition: ConstraintDef,
    pub(crate) columns: Box<[ColumnId]>,
    pub(crate) system_index: Option<(IndexDef, Box<[ColumnId]>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatalogRecord {
    CatalogMeta(CatalogMeta),
    TableName(TableId),
    TableDefinition(TableDef),
    ColumnName(ColumnId),
    ColumnOrdinal(ColumnId),
    ColumnDefinition(ColumnDef),
    IndexName(IndexId),
    IndexDefinition(IndexDef),
    TableIndex(IndexId),
    IndexColumn(ColumnId),
    ConstraintName(ConstraintId),
    ConstraintDefinition(ConstraintDef),
    TableConstraint(ConstraintId),
    ConstraintColumn(ColumnId),
    ReferencedBy(ConstraintId),
}

impl ConstraintDef {
    // Keep the validated catalog fields explicit at this on-disk boundary.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: ConstraintId,
        table_id: TableId,
        kind: ConstraintKind,
        column_count: u16,
        index_id: Option<IndexId>,
        foreign_key_target: Option<ForeignKeyTarget>,
        check_program: Box<[u8]>,
        name: CatalogName,
    ) -> Result<Self> {
        if check_program.len() > MAX_CHECK_PROGRAM_BYTES {
            return Err(DbError::new(
                ErrorCode::ResourceLimit,
                "check program must not exceed 1048576 bytes",
            ));
        }
        if name.as_bytes().len() > MAX_GLOBAL_CATALOG_NAME_BYTES {
            return Err(DbError::new(
                ErrorCode::Type,
                "constraint name must not exceed 128 UTF-8 bytes",
            ));
        }
        let valid_shape = match kind {
            ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
                (1..=MAX_COLUMNS).contains(&column_count)
                    && index_id.is_some()
                    && foreign_key_target.is_none()
                    && check_program.is_empty()
            }
            ConstraintKind::NotNull => {
                column_count == 1
                    && index_id.is_none()
                    && foreign_key_target.is_none()
                    && check_program.is_empty()
            }
            ConstraintKind::Check => {
                column_count == 0
                    && index_id.is_none()
                    && foreign_key_target.is_none()
                    && !check_program.is_empty()
            }
            ConstraintKind::ForeignKey => {
                (1..=MAX_COLUMNS).contains(&column_count)
                    && index_id.is_none()
                    && foreign_key_target.is_some()
                    && check_program.is_empty()
            }
        };
        if !valid_shape {
            return Err(DbError::new(
                ErrorCode::Type,
                "constraint fields do not match the constraint kind",
            ));
        }
        Ok(Self {
            id,
            table_id,
            kind,
            column_count,
            index_id,
            foreign_key_target,
            check_program,
            name,
        })
    }

    pub(crate) const fn id(&self) -> ConstraintId {
        self.id
    }
    pub(crate) const fn table_id(&self) -> TableId {
        self.table_id
    }
    pub(crate) const fn kind(&self) -> ConstraintKind {
        self.kind
    }
    pub(crate) const fn column_count(&self) -> u16 {
        self.column_count
    }
    pub(crate) const fn index_id(&self) -> Option<IndexId> {
        self.index_id
    }
    pub(crate) const fn foreign_key_target(&self) -> Option<ForeignKeyTarget> {
        self.foreign_key_target
    }
    pub(crate) const fn parent_table_id(&self) -> Option<TableId> {
        match self.foreign_key_target {
            Some(target) => Some(target.table_id()),
            None => None,
        }
    }
    pub(crate) const fn parent_constraint_id(&self) -> Option<ConstraintId> {
        match self.foreign_key_target {
            Some(target) => Some(target.constraint_id()),
            None => None,
        }
    }
    pub(crate) fn check_program(&self) -> &[u8] {
        &self.check_program
    }
    pub(crate) const fn name(&self) -> &CatalogName {
        &self.name
    }
}

impl IndexDef {
    pub(crate) fn new(
        id: IndexId,
        table_id: TableId,
        root: PageRef,
        column_count: u16,
        unique: bool,
        ownership: IndexOwnership,
        name: CatalogName,
    ) -> Result<Self> {
        if !(1..=MAX_COLUMNS).contains(&column_count) {
            return Err(DbError::new(
                ErrorCode::Type,
                "index column count must be between 1 and 1000",
            ));
        }
        Ok(Self {
            id,
            table_id,
            root,
            column_count,
            unique,
            ownership,
            name,
        })
    }

    pub(crate) const fn id(&self) -> IndexId {
        self.id
    }
    pub(crate) const fn table_id(&self) -> TableId {
        self.table_id
    }
    pub(crate) const fn root(&self) -> PageRef {
        self.root
    }
    pub(crate) const fn column_count(&self) -> u16 {
        self.column_count
    }
    pub(crate) const fn unique(&self) -> bool {
        self.unique
    }
    pub(crate) const fn ownership(&self) -> IndexOwnership {
        self.ownership
    }
    pub(crate) const fn name(&self) -> &CatalogName {
        &self.name
    }
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
            oracle_type: None,
            name,
        })
    }

    pub(crate) fn new_oracle(
        id: ColumnId,
        table_id: TableId,
        ordinal: u16,
        nullable: bool,
        name: CatalogName,
        oracle_type: OracleTypeDescriptor,
    ) -> Result<Self> {
        let oracle_type = oracle_type.validate()?;
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
            column_type: oracle_type.physical_type(),
            nullable,
            oracle_type: Some(oracle_type),
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
    pub(crate) const fn oracle_type(&self) -> Option<OracleTypeDescriptor> {
        self.oracle_type
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
        oracle_type::OracleTypeDescriptor,
        storage::{PageId, PageRef},
    };

    #[test]
    fn oracle_descriptor_limits_and_physical_types_are_exact() {
        let number = OracleTypeDescriptor::number(38, -38).unwrap();
        assert_eq!(number.physical_type(), ColumnType::Decimal);
        assert_eq!(number.precision(), Some(38));
        assert_eq!(number.scale(), Some(-38));
        assert!(OracleTypeDescriptor::number(1, 38).is_ok());
        assert_eq!(
            OracleTypeDescriptor::number(0, 0)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        assert_eq!(
            OracleTypeDescriptor::number(39, 0)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        assert_eq!(
            OracleTypeDescriptor::number(1, -39)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        assert_eq!(
            OracleTypeDescriptor::number(1, 39)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        let varchar = OracleTypeDescriptor::varchar2(32_767).unwrap();
        assert_eq!(varchar.physical_type(), ColumnType::Text);
        assert_eq!(varchar.length_chars(), Some(32_767));
        assert!(OracleTypeDescriptor::varchar2(1).is_ok());
        assert_eq!(
            OracleTypeDescriptor::varchar2(0).unwrap_err().oracle_code(),
            Some(902)
        );
        assert_eq!(
            OracleTypeDescriptor::varchar2(32_768)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        let char_type = OracleTypeDescriptor::char_type(2_000).unwrap();
        assert_eq!(char_type.physical_type(), ColumnType::Text);
        assert_eq!(char_type.length_chars(), Some(2_000));
        assert!(OracleTypeDescriptor::char_type(1).is_ok());
        assert_eq!(
            OracleTypeDescriptor::char_type(0)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        assert_eq!(
            OracleTypeDescriptor::char_type(2_001)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        let raw = OracleTypeDescriptor::raw(32_767).unwrap();
        assert_eq!(raw.physical_type(), ColumnType::Blob);
        assert_eq!(raw.length_bytes(), Some(32_767));
        assert!(OracleTypeDescriptor::raw(1).is_ok());
        for invalid_length in [0, 32_768] {
            assert_eq!(
                OracleTypeDescriptor::raw(invalid_length)
                    .unwrap_err()
                    .oracle_code(),
                Some(902)
            );
        }
        assert_eq!(
            OracleTypeDescriptor::date().physical_type(),
            ColumnType::Date
        );
        for precision in [0, 9] {
            let timestamp = OracleTypeDescriptor::timestamp(precision).unwrap();
            assert_eq!(timestamp.physical_type(), ColumnType::Timestamp);
            assert_eq!(timestamp.fractional_precision(), Some(precision));
            let timestamp_with_time_zone =
                OracleTypeDescriptor::timestamp_with_time_zone(precision).unwrap();
            assert_eq!(
                timestamp_with_time_zone.physical_type(),
                ColumnType::TimestampWithTimeZone
            );
            assert_eq!(
                timestamp_with_time_zone.fractional_precision(),
                Some(precision)
            );
        }
        assert_eq!(
            OracleTypeDescriptor::timestamp(10)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        assert_eq!(
            OracleTypeDescriptor::timestamp_with_time_zone(10)
                .unwrap_err()
                .oracle_code(),
            Some(902)
        );
        assert_eq!(OracleTypeDescriptor::date().precision(), None);
        assert_eq!(OracleTypeDescriptor::date().scale(), None);
        assert_eq!(OracleTypeDescriptor::date().length_chars(), None);
        assert_eq!(OracleTypeDescriptor::date().length_bytes(), None);
        assert_eq!(OracleTypeDescriptor::date().fractional_precision(), None);
    }

    #[test]
    fn oracle_column_rejects_directly_forged_invalid_descriptor() {
        let error = ColumnDef::new_oracle(
            ColumnId::new(3).unwrap(),
            TableId::new(2).unwrap(),
            0,
            false,
            CatalogName::new("AMOUNT").unwrap(),
            OracleTypeDescriptor::Number {
                precision: 0,
                scale: 0,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        assert_eq!(error.oracle_code(), Some(902));
    }

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
        assert_eq!(multibyte.as_str().len(), 1012);
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
        assert_eq!(column.oracle_type(), None);
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

        let descriptor = OracleTypeDescriptor::number(12, 2).unwrap();
        let oracle_column = ColumnDef::new_oracle(
            column_id,
            table_id,
            1,
            true,
            CatalogName::new("AMOUNT").unwrap(),
            descriptor,
        )
        .unwrap();
        assert_eq!(oracle_column.column_type(), ColumnType::Decimal);
        assert_eq!(oracle_column.oracle_type(), Some(descriptor));
        assert_eq!(
            ColumnDef::new_oracle(
                column_id,
                table_id,
                1000,
                true,
                CatalogName::new("AMOUNT").unwrap(),
                descriptor,
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn explicit_index_definition_enforces_nonzero_columns_and_explicit_owner() {
        let index = IndexDef::new(
            IndexId::new(5).unwrap(),
            TableId::new(2).unwrap(),
            PageRef::new(PageId(7), 1).unwrap(),
            2,
            true,
            IndexOwnership::Explicit,
            CatalogName::new("UX_T_AB").unwrap(),
        )
        .unwrap();

        assert_eq!(index.column_count(), 2);
        assert_eq!(index.ownership(), IndexOwnership::Explicit);
        assert_eq!(
            IndexDef::new(
                index.id(),
                index.table_id(),
                index.root(),
                0,
                false,
                IndexOwnership::Explicit,
                index.name().clone(),
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn constraint_def_enforces_kind_specific_shapes_and_program_limit() {
        let id = ConstraintId::new(7).unwrap();
        let table_id = TableId::new(3).unwrap();
        let index_id = IndexId::new(11).unwrap();
        let name = CatalogName::new("C_T").unwrap();

        for kind in [ConstraintKind::PrimaryKey, ConstraintKind::Unique] {
            let definition = ConstraintDef::new(
                id,
                table_id,
                kind,
                1,
                Some(index_id),
                None,
                Box::new([]),
                name.clone(),
            )
            .unwrap();
            assert_eq!(definition.id(), id);
            assert_eq!(definition.table_id(), table_id);
            assert_eq!(definition.kind(), kind);
            assert_eq!(definition.column_count(), 1);
            assert_eq!(definition.index_id(), Some(index_id));
            assert_eq!(definition.check_program(), &[]);
            assert_eq!(definition.name(), &name);

            for column_count in [0, MAX_COLUMNS + 1] {
                assert_eq!(
                    ConstraintDef::new(
                        id,
                        table_id,
                        kind,
                        column_count,
                        Some(index_id),
                        None,
                        Box::new([]),
                        name.clone(),
                    )
                    .unwrap_err()
                    .code(),
                    ErrorCode::Type
                );
            }
            assert_eq!(
                ConstraintDef::new(
                    id,
                    table_id,
                    kind,
                    1,
                    None,
                    None,
                    Box::new([]),
                    name.clone(),
                )
                .unwrap_err()
                .code(),
                ErrorCode::Type
            );
            assert_eq!(
                ConstraintDef::new(
                    id,
                    table_id,
                    kind,
                    1,
                    Some(index_id),
                    None,
                    vec![1].into_boxed_slice(),
                    name.clone(),
                )
                .unwrap_err()
                .code(),
                ErrorCode::Type
            );
        }

        assert!(
            ConstraintDef::new(
                id,
                table_id,
                ConstraintKind::NotNull,
                1,
                None,
                None,
                Box::new([]),
                name.clone(),
            )
            .is_ok()
        );
        assert_eq!(
            ConstraintDef::new(
                id,
                table_id,
                ConstraintKind::NotNull,
                2,
                None,
                None,
                Box::new([]),
                name.clone(),
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );
        assert_eq!(
            ConstraintDef::new(
                id,
                table_id,
                ConstraintKind::NotNull,
                1,
                None,
                None,
                vec![1].into_boxed_slice(),
                name.clone(),
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );
        assert_eq!(
            ConstraintDef::new(
                id,
                table_id,
                ConstraintKind::NotNull,
                1,
                Some(index_id),
                None,
                Box::new([]),
                name.clone(),
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );

        let check = ConstraintDef::new(
            id,
            table_id,
            ConstraintKind::Check,
            0,
            None,
            None,
            vec![0xaa].into_boxed_slice(),
            name.clone(),
        )
        .unwrap();
        assert_eq!(check.check_program(), &[0xaa]);
        for (column_count, index_id, program) in [
            (1, None, vec![1]),
            (0, Some(index_id), vec![1]),
            (0, None, Vec::new()),
        ] {
            assert_eq!(
                ConstraintDef::new(
                    id,
                    table_id,
                    ConstraintKind::Check,
                    column_count,
                    index_id,
                    None,
                    program.into_boxed_slice(),
                    name.clone(),
                )
                .unwrap_err()
                .code(),
                ErrorCode::Type
            );
        }
        assert_eq!(
            ConstraintDef::new(
                id,
                table_id,
                ConstraintKind::Check,
                0,
                None,
                None,
                vec![0; MAX_CHECK_PROGRAM_BYTES + 1].into_boxed_slice(),
                name.clone(),
            )
            .unwrap_err()
            .code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            ConstraintDef::new(
                id,
                table_id,
                ConstraintKind::Check,
                0,
                None,
                None,
                vec![1].into_boxed_slice(),
                CatalogName::new("a".repeat(129)).unwrap(),
            )
            .unwrap_err()
            .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn foreign_key_constraint_requires_one_complete_target_and_no_index_or_program() {
        let id = ConstraintId::new(7).unwrap();
        let table_id = TableId::new(3).unwrap();
        let target =
            ForeignKeyTarget::new(TableId::new(5).unwrap(), ConstraintId::new(11).unwrap());
        let name = CatalogName::new("FK_T_PARENT").unwrap();

        let definition = ConstraintDef::new(
            id,
            table_id,
            ConstraintKind::ForeignKey,
            2,
            None,
            Some(target),
            Box::new([]),
            name.clone(),
        )
        .unwrap();
        assert_eq!(definition.foreign_key_target(), Some(target));
        assert_eq!(definition.parent_table_id(), Some(target.table_id()));
        assert_eq!(
            definition.parent_constraint_id(),
            Some(target.constraint_id())
        );

        for (column_count, index_id, foreign_key_target, check_program) in [
            (0, None, Some(target), Vec::new()),
            (MAX_COLUMNS + 1, None, Some(target), Vec::new()),
            (1, Some(IndexId::new(13).unwrap()), Some(target), Vec::new()),
            (1, None, None, Vec::new()),
            (1, None, Some(target), vec![1]),
        ] {
            assert_eq!(
                ConstraintDef::new(
                    id,
                    table_id,
                    ConstraintKind::ForeignKey,
                    column_count,
                    index_id,
                    foreign_key_target,
                    check_program.into_boxed_slice(),
                    name.clone(),
                )
                .unwrap_err()
                .code(),
                ErrorCode::Type
            );
        }

        for kind in [
            ConstraintKind::PrimaryKey,
            ConstraintKind::Unique,
            ConstraintKind::NotNull,
            ConstraintKind::Check,
        ] {
            let (column_count, index_id, check_program) = match kind {
                ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
                    (1, Some(IndexId::new(13).unwrap()), Vec::new())
                }
                ConstraintKind::NotNull => (1, None, Vec::new()),
                ConstraintKind::Check => (0, None, vec![1]),
                ConstraintKind::ForeignKey => unreachable!(),
            };
            assert_eq!(
                ConstraintDef::new(
                    id,
                    table_id,
                    kind,
                    column_count,
                    index_id,
                    Some(target),
                    check_program.into_boxed_slice(),
                    name.clone(),
                )
                .unwrap_err()
                .code(),
                ErrorCode::Type
            );
        }
    }
}
