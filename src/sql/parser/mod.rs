mod expr;
mod query;
mod source;
mod tokens;

use crate::error::SqlLocation;
use crate::sql::SqlLimits;
use crate::{DbError, ErrorCode, Result};

pub(super) struct AstBudget {
    remaining_nodes: u32,
    remaining_depth: u16,
    source_tokens: Option<expr::SourceTokenIndex>,
}

impl AstBudget {
    pub(super) const fn new(limits: &SqlLimits) -> Self {
        Self {
            remaining_nodes: limits.max_ast_nodes(),
            remaining_depth: limits.max_nesting(),
            source_tokens: None,
        }
    }

    fn enter(&mut self, location: SqlLocation) -> Result<()> {
        let Some(remaining_nodes) = self.remaining_nodes.checked_sub(1) else {
            return Err(ast_limit_error(location));
        };
        let Some(remaining_depth) = self.remaining_depth.checked_sub(1) else {
            return Err(ast_limit_error(location));
        };
        self.remaining_nodes = remaining_nodes;
        self.remaining_depth = remaining_depth;
        Ok(())
    }

    fn leave(&mut self) {
        self.remaining_depth = self
            .remaining_depth
            .checked_add(1)
            .expect("AST depth cannot exceed its configured starting value");
    }
}

fn ast_limit_error(location: SqlLocation) -> DbError {
    DbError::new(
        ErrorCode::Syntax,
        "SQL expression exceeds the configured AST resource limits",
    )
    .with_sql_location(location)
}
