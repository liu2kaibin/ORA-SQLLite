use crate::{DbError, ErrorCode, Result};

/// A byte range in the original SQL text, with an exclusive end offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SqlSpan {
    start: usize,
    end: usize,
}

impl SqlSpan {
    pub(crate) fn new(start: usize, end: usize) -> Result<Self> {
        if start > end {
            return Err(DbError::new(
                ErrorCode::Internal,
                "SQL span start must not exceed its exclusive end",
            ));
        }
        Ok(Self { start, end })
    }

    pub(crate) const fn start(&self) -> usize {
        self.start
    }

    pub(crate) const fn end(&self) -> usize {
        self.end
    }
}

/// The frontend's logical SQL types, independent of on-disk column encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SqlType {
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

impl SqlType {
    pub(crate) fn unify(self, other: Self) -> Result<Self> {
        match (self, other) {
            (Self::Null, Self::Unknown) | (Self::Unknown, Self::Null) => Ok(Self::Unknown),
            (Self::Null, value) | (Self::Unknown, value) => Ok(value),
            (value, Self::Null) | (value, Self::Unknown) => Ok(value),
            (Self::Int64, Self::Decimal) | (Self::Decimal, Self::Int64) => Ok(Self::Decimal),
            (left, right) if left == right => Ok(left),
            _ => Err(DbError::new(
                ErrorCode::Type,
                "SQL types do not have a compatible common type",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SqlSpan, SqlType};
    use crate::ErrorCode;

    #[test]
    fn span_rejects_an_inverted_range_and_uses_an_exclusive_end() {
        let span = SqlSpan::new(3, 7).unwrap();
        assert_eq!(span.start(), 3);
        assert_eq!(span.end(), 7);
        assert_eq!(SqlSpan::new(7, 3).unwrap_err().code(), ErrorCode::Internal);
    }

    #[test]
    fn sql_types_unify_only_known_compatible_types() {
        assert_eq!(
            SqlType::Int64.unify(SqlType::Decimal).unwrap(),
            SqlType::Decimal
        );
        assert_eq!(SqlType::Null.unify(SqlType::Text).unwrap(), SqlType::Text);
        assert_eq!(
            SqlType::Unknown.unify(SqlType::Unknown).unwrap(),
            SqlType::Unknown
        );
        assert_eq!(
            SqlType::Decimal.unify(SqlType::Float64).unwrap_err().code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn sql_type_unification_is_symmetric_for_every_supported_pair() {
        let all = [
            SqlType::Null,
            SqlType::Unknown,
            SqlType::Int64,
            SqlType::Decimal,
            SqlType::Float64,
            SqlType::Text,
            SqlType::Blob,
            SqlType::Date,
            SqlType::Timestamp,
            SqlType::Bool,
        ];

        for left in all {
            for right in all {
                let forward = left.unify(right).map_err(|error| error.code());
                let reverse = right.unify(left).map_err(|error| error.code());
                assert_eq!(forward, reverse, "{left:?} and {right:?} must be symmetric");
            }
        }
    }

    #[test]
    fn every_sql_type_variant_is_constructible() {
        let all = [
            SqlType::Null,
            SqlType::Unknown,
            SqlType::Int64,
            SqlType::Decimal,
            SqlType::Float64,
            SqlType::Text,
            SqlType::Blob,
            SqlType::Date,
            SqlType::Timestamp,
            SqlType::Bool,
        ];
        assert_eq!(all.len(), 10);
    }
}
