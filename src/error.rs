use std::fmt;

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    Syntax = 1000,
    Bind = 1100,
    Type = 1200,
    Constraint = 1300,
    Busy = 1400,
    Transaction = 1500,
    Unsupported = 1600,
    Corruption = 1700,
    Io = 1800,
    Internal = 1900,
}

impl ErrorCode {
    pub const fn as_u32(&self) -> u32 {
        *self as u32
    }
}

#[derive(Debug)]
pub struct DbError {
    code: ErrorCode,
    message: String,
    oracle_code: Option<u16>,
}

impl DbError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            oracle_code: None,
        }
    }

    pub const fn with_oracle_code(mut self, oracle_code: u16) -> Self {
        self.oracle_code = Some(oracle_code);
        self
    }

    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    pub const fn oracle_code(&self) -> Option<u16> {
        self.oracle_code
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
    use super::{DbError, ErrorCode};

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
}
