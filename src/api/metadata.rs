use crate::storage::catalog::types::{ColumnId, TableId};

/// A stable logical SQL type exposed by prepared-statement metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// The SQL `NULL` pseudo-type.
    Null,
    /// A type that could not be inferred during preparation.
    Unknown,
    /// A signed 64-bit integer.
    Int64,
    /// An exact base-10 [`crate::Decimal`].
    Decimal,
    /// An IEEE-754 double-precision floating-point value.
    Float64,
    /// UTF-8 text.
    Text,
    /// Binary data.
    Blob,
    /// An [`crate::OracleDate`].
    Date,
    /// An [`crate::OracleTimestamp`].
    Timestamp,
    /// A Boolean value.
    Bool,
}

impl DataType {
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
    /// A one-based positional parameter number.
    Position(u16),
    /// A normalized named parameter without its SQL sigil.
    Name(String),
}

/// Immutable metadata for one logical bind parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterMetadata {
    pub(crate) identifier: ParameterIdentifier,
    pub(crate) data_type: Option<DataType>,
    /// Text is retained only when a V2 Oracle NUMBER/DATE/TIMESTAMP use will
    /// normalize it at execution. This is deliberately not public metadata.
    pub(crate) allow_oracle_text_source: bool,
}

impl ParameterMetadata {
    pub(crate) const fn inferred(
        identifier: ParameterIdentifier,
        sql_type: crate::sql::SqlType,
        allow_oracle_text_source: bool,
    ) -> Self {
        let data_type = match sql_type {
            crate::sql::SqlType::Null | crate::sql::SqlType::Unknown => None,
            value => Some(DataType::from_sql_type(value)),
        };
        Self {
            identifier,
            data_type,
            allow_oracle_text_source,
        }
    }

    /// Returns the normalized positional or named identity of this parameter.
    pub fn identifier(&self) -> &ParameterIdentifier {
        &self.identifier
    }

    /// Returns the inferred concrete type, or `None` when preparation could not infer one.
    ///
    /// `None` does not itself mean that a supplied value is SQL `NULL`.
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
    pub(crate) fn new(
        name: String,
        data_type: DataType,
        nullable: bool,
        source_table_id: Option<TableId>,
        source_column_id: Option<ColumnId>,
    ) -> Self {
        Self {
            name,
            data_type,
            nullable,
            source_table_id,
            source_column_id,
        }
    }

    /// Returns the result-column name exposed by the prepared statement.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the logical type of values in this result column.
    pub const fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Returns whether this result column can contain SQL `NULL`.
    pub const fn nullable(&self) -> bool {
        self.nullable
    }

    /// Returns the stable catalog table identifier when this is a direct source column.
    pub const fn source_table_id(&self) -> Option<u64> {
        match self.source_table_id {
            Some(id) => Some(id.get()),
            None => None,
        }
    }

    /// Returns the stable catalog column identifier when this is a direct source column.
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
