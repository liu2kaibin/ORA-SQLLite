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
    AccessMode, ColumnMetadata, Connection, DataType, Database, ExecutionResult, Options,
    ParameterIdentifier, ParameterMetadata, ParameterValue, Parameters, ReadTransaction, Row, Rows,
    Statement, WriteTransaction,
};
pub use datetime::{OracleDate, OracleTimestamp};
pub use decimal::Decimal;
pub use error::{DbError, ErrorCode, Result};
pub use sql::SqlLimits;
pub use value::Value;

#[cfg(test)]
#[test]
fn phase_4c1_public_primitives_have_stable_contracts() {
    assert_eq!(ErrorCode::ResourceLimit.as_u32(), 1650);
    let options = Options::default();
    assert_eq!(options.max_query_memory_bytes(), 64 * 1024 * 1024);
    assert_eq!(
        options
            .with_max_query_memory_bytes(4096)
            .max_query_memory_bytes(),
        4096
    );

    let null = ParameterValue::typed_null(DataType::Text).unwrap();
    assert_eq!(null.data_type(), Some(DataType::Text));
    assert_eq!(null.value(), &Value::Null);
    assert_eq!(
        ParameterValue::typed_null(DataType::Unknown)
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );

    let row = Row::from_values_for_test(vec![Value::Int64(7), Value::Text("N".into())]);
    assert_eq!(row.len(), 2);
    assert_eq!(row.get(0), Some(&Value::Int64(7)));
    assert_eq!(ExecutionResult::new_for_test(3).rows_affected(), 3);
}
