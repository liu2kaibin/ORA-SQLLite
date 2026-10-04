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
        Self::Number { precision, scale }.validate()
    }

    pub(crate) fn varchar2(length_chars: u32) -> Result<Self> {
        Self::Varchar2 { length_chars }.validate()
    }

    pub(crate) fn char_type(length_chars: u32) -> Result<Self> {
        Self::Char { length_chars }.validate()
    }

    pub(crate) fn raw(length_bytes: u32) -> Result<Self> {
        Self::Raw { length_bytes }.validate()
    }

    pub(crate) const fn date() -> Self {
        Self::Date
    }

    pub(crate) fn timestamp(fractional_precision: u8) -> Result<Self> {
        Self::Timestamp {
            fractional_precision,
        }
        .validate()
    }

    pub(crate) fn timestamp_with_time_zone(fractional_precision: u8) -> Result<Self> {
        Self::TimestampWithTimeZone {
            fractional_precision,
        }
        .validate()
    }

    pub(crate) fn validate(self) -> Result<Self> {
        let error_message = match self {
            Self::Number { precision, scale }
                if !(1..=38).contains(&precision) || !(-38..=38).contains(&scale) =>
            {
                Some("invalid NUMBER precision or scale")
            }
            Self::Varchar2 { length_chars } if !(1..=32_767).contains(&length_chars) => {
                Some("invalid VARCHAR2 length")
            }
            Self::Char { length_chars } if !(1..=2_000).contains(&length_chars) => {
                Some("invalid CHAR length")
            }
            Self::Raw { length_bytes } if !(1..=32_767).contains(&length_bytes) => {
                Some("invalid RAW length")
            }
            Self::Timestamp {
                fractional_precision,
            } if fractional_precision > 9 => Some("invalid TIMESTAMP fractional precision"),
            Self::TimestampWithTimeZone {
                fractional_precision,
            } if fractional_precision > 9 => {
                Some("invalid TIMESTAMP WITH TIME ZONE fractional precision")
            }
            _ => None,
        };
        match error_message {
            Some(message) => Err(invalid_declaration(message)),
            None => Ok(self),
        }
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
