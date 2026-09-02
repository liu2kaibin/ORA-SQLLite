#![forbid(unsafe_code)]
#![doc = "ORA-SQLLite: a lightweight embedded database with an Oracle-compatible CRUD dialect."]

mod datetime;
mod decimal;
pub mod error;
mod value;

pub use datetime::{OracleDate, OracleTimestamp};
pub use decimal::Decimal;
pub use error::{DbError, ErrorCode, Result};
pub use value::Value;
