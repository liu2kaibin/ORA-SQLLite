use crate::{DbError, ErrorCode, Result, storage::catalog::types::ColumnType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OracleTypeDescriptor {
    Number { precision: u8, scale: i8 },
    Varchar2 { length_chars: u32 },
    Char { length_chars: u32 },
    Raw { length_bytes: u32 },
    Date,
    Timestamp { fractional_precision: u8 },
    TimestampWithTimeZone { fractional_precision: u8 },
}

impl OracleTypeDescriptor {
    pub(crate) fn number(precision: u8, scale: i8) -> Result<Self> {
        if !(1..=38).contains(&precision) || !(-38..=38).contains(&scale) {
            return Err(invalid_declaration("invalid NUMBER precision or scale"));
        }
        Ok(Self::Number { precision, scale })
    }

    pub(crate) fn varchar2(length_chars: u32) -> Result<Self> {
        if !(1..=32_767).contains(&length_chars) {
            return Err(invalid_declaration("invalid VARCHAR2 length"));
        }
        Ok(Self::Varchar2 { length_chars })
    }

    pub(crate) fn char_type(length_chars: u32) -> Result<Self> {
        if !(1..=2_000).contains(&length_chars) {
            return Err(invalid_declaration("invalid CHAR length"));
        }
        Ok(Self::Char { length_chars })
    }

    pub(crate) fn raw(length_bytes: u32) -> Result<Self> {
        if !(1..=32_767).contains(&length_bytes) {
            return Err(invalid_declaration("invalid RAW length"));
        }
        Ok(Self::Raw { length_bytes })
    }

    pub(crate) const fn date() -> Self {
        Self::Date
    }

    pub(crate) fn timestamp(fractional_precision: u8) -> Result<Self> {
        if fractional_precision > 9 {
            return Err(invalid_declaration(
                "invalid TIMESTAMP fractional precision",
            ));
        }
        Ok(Self::Timestamp {
            fractional_precision,
        })
    }

    pub(crate) fn timestamp_with_time_zone(fractional_precision: u8) -> Result<Self> {
        if fractional_precision > 9 {
            return Err(invalid_declaration(
                "invalid TIMESTAMP WITH TIME ZONE fractional precision",
            ));
        }
        Ok(Self::TimestampWithTimeZone {
            fractional_precision,
        })
    }

    pub(crate) const fn physical_type(self) -> ColumnType {
        match self {
            Self::Number { .. } => ColumnType::Decimal,
            Self::Varchar2 { .. } | Self::Char { .. } => ColumnType::Text,
            Self::Raw { .. } => ColumnType::Blob,
            Self::Date => ColumnType::Date,
            Self::Timestamp { .. } => ColumnType::Timestamp,
            Self::TimestampWithTimeZone { .. } => ColumnType::TimestampWithTimeZone,
        }
    }

    pub(crate) const fn precision(self) -> Option<u8> {
        match self {
            Self::Number { precision, .. } => Some(precision),
            _ => None,
        }
    }

    pub(crate) const fn scale(self) -> Option<i8> {
        match self {
            Self::Number { scale, .. } => Some(scale),
            _ => None,
        }
    }

    pub(crate) const fn length_chars(self) -> Option<u32> {
        match self {
            Self::Varchar2 { length_chars } | Self::Char { length_chars } => Some(length_chars),
            _ => None,
        }
    }

    pub(crate) const fn length_bytes(self) -> Option<u32> {
        match self {
            Self::Raw { length_bytes } => Some(length_bytes),
            _ => None,
        }
    }

    pub(crate) const fn fractional_precision(self) -> Option<u8> {
        match self {
            Self::Timestamp {
                fractional_precision,
            }
            | Self::TimestampWithTimeZone {
                fractional_precision,
            } => Some(fractional_precision),
            _ => None,
        }
    }
}

fn invalid_declaration(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Type, message).with_oracle_code(902)
}
