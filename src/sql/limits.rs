use crate::{DbError, ErrorCode, Result};

const DEFAULT_MAX_SQL_BYTES: u32 = 1_048_576;
const DEFAULT_MAX_TOKENS: u32 = 100_000;
const DEFAULT_MAX_AST_NODES: u32 = 100_000;
const DEFAULT_MAX_NESTING: u16 = 128;
const DEFAULT_MAX_PARAMETERS: u16 = 65_535;
const DEFAULT_MAX_JOINS: u16 = 64;

/// Resource bounds enforced while preparing a SQL statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLimits {
    max_sql_bytes: u32,
    max_tokens: u32,
    max_ast_nodes: u32,
    max_nesting: u16,
    max_parameters: u16,
    max_joins: u16,
}

impl Default for SqlLimits {
    fn default() -> Self {
        Self {
            max_sql_bytes: DEFAULT_MAX_SQL_BYTES,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_ast_nodes: DEFAULT_MAX_AST_NODES,
            max_nesting: DEFAULT_MAX_NESTING,
            max_parameters: DEFAULT_MAX_PARAMETERS,
            max_joins: DEFAULT_MAX_JOINS,
        }
    }
}

impl SqlLimits {
    pub const fn max_sql_bytes(&self) -> u32 {
        self.max_sql_bytes
    }

    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    pub const fn max_ast_nodes(&self) -> u32 {
        self.max_ast_nodes
    }

    pub const fn max_nesting(&self) -> u16 {
        self.max_nesting
    }

    pub const fn max_parameters(&self) -> u16 {
        self.max_parameters
    }

    pub const fn max_joins(&self) -> u16 {
        self.max_joins
    }

    pub fn with_max_sql_bytes(mut self, value: usize) -> Result<Self> {
        self.max_sql_bytes = checked_u32_limit(value, "maximum SQL bytes")?;
        Ok(self)
    }

    pub fn with_max_tokens(mut self, value: usize) -> Result<Self> {
        self.max_tokens = checked_u32_limit(value, "maximum token count")?;
        Ok(self)
    }

    pub fn with_max_ast_nodes(mut self, value: usize) -> Result<Self> {
        self.max_ast_nodes = checked_u32_limit(value, "maximum AST node count")?;
        Ok(self)
    }

    pub fn with_max_nesting(mut self, value: usize) -> Result<Self> {
        self.max_nesting = checked_u16_limit(value, "maximum nesting")?;
        Ok(self)
    }

    pub fn with_max_parameters(mut self, value: usize) -> Result<Self> {
        self.max_parameters = checked_u16_limit(value, "maximum parameter count")?;
        Ok(self)
    }

    pub fn with_max_joins(mut self, value: usize) -> Result<Self> {
        self.max_joins = checked_u16_limit(value, "maximum join count")?;
        Ok(self)
    }
}

fn checked_u32_limit(value: usize, name: &str) -> Result<u32> {
    if value == 0 || value > u32::MAX as usize {
        return Err(invalid_limit(name));
    }
    Ok(value as u32)
}

fn checked_u16_limit(value: usize, name: &str) -> Result<u16> {
    if value == 0 || value > u16::MAX as usize {
        return Err(invalid_limit(name));
    }
    Ok(value as u16)
}

fn invalid_limit(name: &str) -> DbError {
    DbError::new(
        ErrorCode::Internal,
        format!("{name} must be positive and within its implementation bound"),
    )
}

#[cfg(test)]
mod tests {
    use super::SqlLimits;
    use crate::ErrorCode;

    #[test]
    fn sql_limits_have_the_approved_defaults() {
        let limits = SqlLimits::default();
        assert_eq!(limits.max_sql_bytes(), 1_048_576);
        assert_eq!(limits.max_tokens(), 100_000);
        assert_eq!(limits.max_ast_nodes(), 100_000);
        assert_eq!(limits.max_nesting(), 128);
        assert_eq!(limits.max_parameters(), 65_535);
        assert_eq!(limits.max_joins(), 64);
    }

    #[test]
    fn limit_builders_reject_zero_and_implementation_overflow() {
        assert_eq!(
            SqlLimits::default()
                .with_max_sql_bytes(0)
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
        assert_eq!(
            SqlLimits::default()
                .with_max_tokens((u32::MAX as usize) + 1)
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
        assert_eq!(
            SqlLimits::default()
                .with_max_nesting((u16::MAX as usize) + 1)
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
        assert_eq!(
            SqlLimits::default().with_max_joins(64).unwrap().max_joins(),
            64
        );
    }
}
