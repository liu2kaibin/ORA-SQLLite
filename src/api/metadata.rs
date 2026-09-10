use crate::storage::catalog::types::{ColumnId, TableId};

/// A stable logical SQL type exposed by prepared-statement metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Null,
    Unknown,
    Int64,
    Decimal,
    Float64,
    Text,
    Blob,
    Date,
    Timestamp,
    Bool,
}

impl DataType {
    #[allow(
        dead_code,
        reason = "the Phase 4B type binder is added in a later task"
    )]
    pub(crate) const fn from_sql_type(sql_type: crate::sql::SqlType) -> Self {
        match sql_type {
            crate::sql::SqlType::Null => Self::Null,
            crate::sql::SqlType::Unknown => Self::Unknown,
            crate::sql::SqlType::Int64 => Self::Int64,
            crate::sql::SqlType::Decimal => Self::Decimal,
            crate::sql::SqlType::Float64 => Self::Float64,
            crate::sql::SqlType::Text => Self::Text,
            crate::sql::SqlType::Blob => Self::Blob,
            crate::sql::SqlType::Date => Self::Date,
            crate::sql::SqlType::Timestamp => Self::Timestamp,
            crate::sql::SqlType::Bool => Self::Bool,
        }
    }
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

#[cfg(test)]
mod tests {
    use crate::sql::SqlType;

    use super::DataType;

    #[test]
    fn every_internal_sql_type_has_a_lossless_public_mapping() {
        let cases = [
            (SqlType::Null, DataType::Null),
            (SqlType::Unknown, DataType::Unknown),
            (SqlType::Int64, DataType::Int64),
            (SqlType::Decimal, DataType::Decimal),
            (SqlType::Float64, DataType::Float64),
            (SqlType::Text, DataType::Text),
            (SqlType::Blob, DataType::Blob),
            (SqlType::Date, DataType::Date),
            (SqlType::Timestamp, DataType::Timestamp),
            (SqlType::Bool, DataType::Bool),
        ];

        for (sql_type, expected) in cases {
            assert_eq!(DataType::from_sql_type(sql_type), expected);
        }
    }
}
