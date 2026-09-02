#![forbid(unsafe_code)]
#![doc = "ORA-SQLLite: a lightweight embedded database with an Oracle-compatible CRUD dialect."]

mod datetime;
mod decimal;
pub mod error;
#[allow(
    dead_code,
    reason = "fault boundaries are consumed by later storage tasks"
)]
mod fault;
#[allow(
    dead_code,
    reason = "phase 1 primitives are consumed by later storage tasks"
)]
mod storage;
mod value;

pub use datetime::{OracleDate, OracleTimestamp};
pub use decimal::Decimal;
pub use error::{DbError, ErrorCode, Result};
pub use value::Value;
