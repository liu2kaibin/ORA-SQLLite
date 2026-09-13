use crate::{DataType, DbError, ErrorCode, Result, Value};

/// A value supplied for a SQL bind parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum ParameterValue {
    Value(Value),
    TypedNull(DataType),
}

impl ParameterValue {
    /// Creates a NULL bind parameter with an explicit concrete type.
    pub fn typed_null(data_type: DataType) -> Result<Self> {
        match data_type {
            DataType::Null | DataType::Unknown => Err(DbError::new(
                ErrorCode::Type,
                "typed NULL requires a concrete data type",
            )),
            concrete => Ok(Self::TypedNull(concrete)),
        }
    }

    pub fn value(&self) -> &Value {
        match self {
            Self::Value(value) => value,
            Self::TypedNull(_) => &Value::Null,
        }
    }

    pub const fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Value(_) => None,
            Self::TypedNull(data_type) => Some(*data_type),
        }
    }
}

impl From<Value> for ParameterValue {
    fn from(value: Value) -> Self {
        Self::Value(value)
    }
}

/// Bind parameters supplied to a statement execution.
pub enum Parameters<'a> {
    None,
    Positional(&'a [ParameterValue]),
    Named(&'a [(&'a str, ParameterValue)]),
}

impl<'a> Parameters<'a> {
    pub const fn none() -> Self {
        Self::None
    }

    pub const fn positional(values: &'a [ParameterValue]) -> Self {
        Self::Positional(values)
    }

    pub const fn named(values: &'a [(&'a str, ParameterValue)]) -> Self {
        Self::Named(values)
    }
}
