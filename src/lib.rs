#![forbid(unsafe_code)]
#![doc = "ORA-SQLLite: a lightweight embedded database with an Oracle-compatible CRUD dialect."]

mod api;
mod datetime;
mod decimal;
pub mod error;
#[allow(
    dead_code,
    reason = "fault boundaries are consumed by later storage tasks"
)]
mod fault;
mod sql;
#[allow(
    dead_code,
    reason = "phase 1 primitives are consumed by later storage tasks"
)]
mod storage;
mod value;

pub use api::{
    AccessMode, ColumnMetadata, DataType, Database, Options, ParameterIdentifier,
    ParameterMetadata, Statement,
};
pub use datetime::{OracleDate, OracleTimestamp};
pub use decimal::Decimal;
pub use error::{DbError, ErrorCode, Result};
pub use sql::SqlLimits;
pub use value::Value;
