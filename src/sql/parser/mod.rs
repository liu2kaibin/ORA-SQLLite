mod dialect;
mod expr;
mod query;
mod source;
mod statement;
mod tokens;

#[allow(
    unused_imports,
    reason = "the binder task consumes the top-level statement parser"
)]
pub(super) use statement::parse_statement;

use crate::error::SqlLocation;
use crate::sql::{Identifier, SqlLimits, SqlSpan};
use crate::{DbError, ErrorCode, Result};

fn unquoted_identifier(
    value: impl AsRef<str>,
    span: SqlSpan,
    source_map: &source::SourceMap<'_>,
) -> Result<Identifier> {
    use sqlparser::keywords::{ALL_KEYWORDS, ALL_KEYWORDS_INDEX, RESERVED_FOR_IDENTIFIER};

    let value = value.as_ref();
    if value.contains('@') {
        return Err(DbError::new(
            ErrorCode::Unsupported,
            "database-link identifiers are outside the Phase 4B SQL surface",
        )
        .with_sql_location(source_map.location(span.start())?));
    }
    let normalized = value.to_ascii_uppercase();
    let reserved = ALL_KEYWORDS
        .binary_search(&normalized.as_str())
        .ok()
        .is_some_and(|index| RESERVED_FOR_IDENTIFIER.contains(&ALL_KEYWORDS_INDEX[index]));
    if reserved {
        return Err(DbError::new(
            ErrorCode::Bind,
            "reserved word must be quoted when used as an identifier",
        )
        .with_sql_location(source_map.location(span.start())?));
    }
    Identifier::unquoted(value, span)
}

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
