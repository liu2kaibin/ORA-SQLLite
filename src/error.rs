use std::fmt;

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    Syntax = 1000,
    Bind = 1100,
    SchemaChanged = 1150,
    Type = 1200,
    Constraint = 1300,
    Busy = 1400,
    Transaction = 1500,
    Unsupported = 1600,
    ResourceLimit = 1650,
    Corruption = 1700,
    Io = 1800,
    Internal = 1900,
}

impl ErrorCode {
    pub const fn as_u32(&self) -> u32 {
        *self as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLocation {
    byte_offset: usize,
    line: usize,
    column: usize,
}

impl SqlLocation {
    pub fn new(byte_offset: usize, line: usize, column: usize) -> Result<Self> {
        if line == 0 || column == 0 {
            return Err(DbError::new(
                ErrorCode::Internal,
                "SQL line and column must be one-based",
            ));
        }

        Ok(Self {
            byte_offset,
            line,
            column,
        })
    }

    pub const fn byte_offset(&self) -> usize {
        self.byte_offset
    }

    pub const fn line(&self) -> usize {
        self.line
    }

    pub const fn column(&self) -> usize {
        self.column
    }
}

#[derive(Debug)]
pub struct DbError {
    code: ErrorCode,
    message: String,
    oracle_code: Option<u16>,
    sql_location: Option<SqlLocation>,
}

impl DbError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            oracle_code: None,
            sql_location: None,
        }
    }

    #[allow(
        dead_code,
        reason = "Oracle normalization errors are wired into execution by later phase 5 tasks"
    )]
    pub(crate) fn oracle(code: ErrorCode, oracle_code: u16, message: impl Into<String>) -> Self {
        Self::new(code, message).with_oracle_code(oracle_code)
    }

    pub const fn with_oracle_code(mut self, oracle_code: u16) -> Self {
        self.oracle_code = Some(oracle_code);
        self
    }

    pub const fn with_sql_location(mut self, sql_location: SqlLocation) -> Self {
        self.sql_location = Some(sql_location);
        self
    }

    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    pub const fn oracle_code(&self) -> Option<u16> {
        self.oracle_code
    }

    pub const fn sql_location(&self) -> Option<SqlLocation> {
        self.sql_location
    }

    pub(crate) const fn is_fatal(&self) -> bool {
        matches!(
            self.code,
            ErrorCode::Corruption | ErrorCode::Io | ErrorCode::Internal
        )
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_u32(), self.message)
    }
}

impl std::error::Error for DbError {}

#[cfg(test)]
mod tests {
    use super::{DbError, ErrorCode, SqlLocation};

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(ErrorCode::Type.as_u32(), 1200);
        assert_eq!(ErrorCode::Corruption.as_u32(), 1700);
    }

    #[test]
    fn oracle_code_is_optional_metadata() {
        let error = DbError::new(ErrorCode::Constraint, "duplicate key").with_oracle_code(1);
        assert_eq!(error.code(), ErrorCode::Constraint);
        assert_eq!(error.oracle_code(), Some(1));
        assert_eq!(error.to_string(), "[1300] duplicate key");
    }

    #[test]
    fn oracle_error_preserves_error_class_message_and_fatality() {
        let type_error = DbError::oracle(ErrorCode::Type, 1722, "invalid number");
        assert_eq!(type_error.code(), ErrorCode::Type);
        assert_eq!(type_error.oracle_code(), Some(1722));
        assert_eq!(type_error.to_string(), "[1200] invalid number");
        assert!(!type_error.is_fatal());

        let fatal_error = DbError::oracle(ErrorCode::Internal, 600, "internal failure");
        assert_eq!(fatal_error.oracle_code(), Some(600));
        assert!(fatal_error.is_fatal());
    }

    #[test]
    fn schema_changed_and_sql_locations_are_stable() {
        assert_eq!(ErrorCode::SchemaChanged.as_u32(), 1150);
        let location = SqlLocation::new(9, 2, 4).unwrap();
        let error = DbError::new(ErrorCode::Syntax, "bad token").with_sql_location(location);
        assert_eq!(error.sql_location(), Some(location));
        assert_eq!(location.byte_offset(), 9);
        assert_eq!(location.line(), 2);
        assert_eq!(location.column(), 4);
    }

    #[test]
    fn sql_locations_reject_zero_line_or_column() {
        assert_eq!(
            SqlLocation::new(0, 0, 1).unwrap_err().code(),
            ErrorCode::Internal
        );
        assert_eq!(
            SqlLocation::new(0, 1, 0).unwrap_err().code(),
            ErrorCode::Internal
        );
    }
}
