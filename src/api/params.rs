use crate::{DataType, DbError, ErrorCode, Result, Value};

/// A value supplied for a SQL bind parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum ParameterValue {
    /// A scalar value; [`Value::Null`] is an untyped SQL `NULL`.
    Value(Value),
    /// SQL `NULL` carrying the concrete type expected by the statement.
    TypedNull(DataType),
}

impl ParameterValue {
    /// Creates a `NULL` bind parameter with an explicit concrete type.
    ///
    /// Returns [`ErrorCode::Type`] for [`DataType::Null`] or [`DataType::Unknown`].
    pub fn typed_null(data_type: DataType) -> Result<Self> {
        match data_type {
            DataType::Null | DataType::Unknown => Err(DbError::new(
                ErrorCode::Type,
                "typed NULL requires a concrete data type",
            )),
            concrete => Ok(Self::TypedNull(concrete)),
        }
    }

    /// Returns the scalar value, mapping a typed `NULL` to [`Value::Null`].
    pub fn value(&self) -> &Value {
        match self {
            Self::Value(value) => value,
            Self::TypedNull(_) => &Value::Null,
        }
    }

    /// Returns the explicit type of a typed `NULL`, or `None` for [`ParameterValue::Value`].
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
    /// No bind parameters.
    None,
    /// Values matched to one-based positional parameters in slice order.
    Positional(&'a [ParameterValue]),
    /// Name/value pairs matched case-insensitively after SQL-name normalization.
    Named(&'a [(&'a str, ParameterValue)]),
}

impl<'a> Parameters<'a> {
    /// Constructs an empty parameter set.
    pub const fn none() -> Self {
        Self::None
    }

    /// Constructs a borrowed positional parameter set.
    pub const fn positional(values: &'a [ParameterValue]) -> Self {
        Self::Positional(values)
    }

    /// Constructs a borrowed named parameter set.
    pub const fn named(values: &'a [(&'a str, ParameterValue)]) -> Self {
        Self::Named(values)
    }
}
