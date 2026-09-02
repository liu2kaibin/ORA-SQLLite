#![forbid(unsafe_code)]
#![doc = "ORA-SQLLite: a lightweight embedded database with an Oracle-compatible CRUD dialect."]

pub mod error;

pub use error::{DbError, ErrorCode, Result};
