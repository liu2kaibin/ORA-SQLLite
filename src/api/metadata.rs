use crate::storage::catalog::types::{ColumnId, TableId};

/// A stable logical SQL type exposed by prepared-statement metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int64,
    Decimal,
    Float64,
    Text,
    Blob,
    Date,
    Timestamp,
    Bool,
}

/// The normalized identity of a bind parameter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ParameterIdentifier {
    Position(u16),
    Name(String),
}

/// Immutable metadata for one logical bind parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterMetadata {
    pub(crate) identifier: ParameterIdentifier,
    pub(crate) data_type: Option<DataType>,
}

impl ParameterMetadata {
    pub fn identifier(&self) -> &ParameterIdentifier {
        &self.identifier
    }

    pub const fn data_type(&self) -> Option<DataType> {
        self.data_type
    }
}

/// Immutable metadata for one result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMetadata {
    pub(crate) name: String,
    pub(crate) data_type: DataType,
    pub(crate) nullable: bool,
    pub(crate) source_table_id: Option<TableId>,
    pub(crate) source_column_id: Option<ColumnId>,
}

impl ColumnMetadata {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn data_type(&self) -> DataType {
        self.data_type
    }

    pub const fn nullable(&self) -> bool {
        self.nullable
    }

    pub const fn source_table_id(&self) -> Option<u64> {
        match self.source_table_id {
            Some(id) => Some(id.get()),
            None => None,
        }
    }

    pub const fn source_column_id(&self) -> Option<u64> {
        match self.source_column_id {
            Some(id) => Some(id.get()),
            None => None,
        }
    }
}
