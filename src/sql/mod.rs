#[allow(dead_code, reason = "later SQL frontend layers consume the owned AST")]
mod ast;
#[allow(
    dead_code,
    unused_imports,
    reason = "later Phase 4B binders consume catalog binding scopes"
)]
pub(crate) mod bind;
#[allow(
    dead_code,
    unused_imports,
    reason = "later Phase 4B binders and Phase 4C execution consume the bound IR"
)]
pub(crate) mod bound;
#[allow(
    dead_code,
    unused_imports,
    reason = "subsequent Phase 4C1 operators consume execution primitives"
)]
pub(crate) mod exec;
mod limits;
#[allow(
    dead_code,
    reason = "later SQL frontend layers consume checked name primitives"
)]
mod name;
#[allow(
    dead_code,
    reason = "later SQL frontend layers consume source-mapped parser tokens"
)]
mod parser;
#[allow(
    dead_code,
    unused_imports,
    reason = "subsequent Phase 4C1 execution tasks consume immutable rule plans"
)]
pub(crate) mod plan;
#[allow(
    dead_code,
    reason = "later SQL frontend layers consume checked type primitives"
)]
mod types;

pub use limits::SqlLimits;
#[allow(
    unused_imports,
    reason = "later SQL frontend layers import these shared name primitives"
)]
pub(crate) use name::{Identifier, ParameterKey, ParameterStyle};
#[allow(
    unused_imports,
    reason = "later SQL frontend layers import these shared type primitives"
)]
pub(crate) use types::{SqlSpan, SqlType};

use crate::{DbError, Result, error::SqlLocation, storage::catalog::CatalogReader};

/// Parses and binds one SQL statement against an immutable catalog image.
pub(crate) fn bind_sql(
    sql: &str,
    limits: &SqlLimits,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
) -> Result<bound::BoundStatement> {
    let statement = parser::parse_statement(sql, limits)?;
    match bind::bind_statement_with_limits(&statement, catalog, database_uuid, limits) {
        Ok(statement) => Ok(statement),
        Err(error) => {
            let Some(location) = error.sql_location() else {
                return Err(error);
            };
            match source_location(sql, location.byte_offset()) {
                Ok(location) => Err(error.with_sql_location(location)),
                Err(_) => Err(error),
            }
        }
    }
}

fn source_location(sql: &str, byte_offset: usize) -> Result<SqlLocation> {
    let preceding_source = sql.get(..byte_offset).ok_or_else(|| {
        DbError::new(
            crate::ErrorCode::Internal,
            "SQL source location is outside the original text",
        )
    })?;
    let line = preceding_source
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .checked_add(1)
        .ok_or_else(|| DbError::new(crate::ErrorCode::Internal, "SQL line overflow"))?;
    let column = preceding_source
        .rsplit_once('\n')
        .map_or(preceding_source, |(_, line)| line)
        .chars()
        .count()
        .checked_add(1)
        .ok_or_else(|| DbError::new(crate::ErrorCode::Internal, "SQL column overflow"))?;
    SqlLocation::new(byte_offset, line, column)
}
