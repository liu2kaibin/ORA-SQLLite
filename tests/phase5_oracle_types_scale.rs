mod support;

use std::time::Instant;

use ora_sqllite::{
    Connection, Database, Decimal, OracleDate, OracleTimestamp, ParameterValue, Parameters, Value,
};

const ROWS: i64 = 100_000;

fn decimal_value(magnitude: u128, scale: i8) -> Value {
    Value::Decimal(Decimal::new(false, scale, magnitude).unwrap())
}

fn scale_parameters(id: i64) -> [ParameterValue; 5] {
    let date = OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap();
    let timestamp =
        OracleTimestamp::from_nanoseconds(date, u32::try_from(id % 1_000_000_000).unwrap(), None)
            .unwrap();
    [
        Value::Text(id.to_string()).into(),
        Value::Text(format!("{}.25", id % 10_000)).into(),
        Value::Text(format!("C{id:07}")).into(),
        Value::Timestamp(timestamp).into(),
        Value::Blob(id.to_be_bytes().repeat(2)).into(),
    ]
}

fn assert_count(connection: &mut Connection, expected: i64) {
    assert_eq!(
        support::collect(connection, "SELECT COUNT(*) FROM T"),
        vec![vec![Value::Int64(expected)]]
    );
}

#[test]
#[ignore = "manual Phase 5 MVP scale acceptance"]
fn phase5_hundred_thousand_row_oracle_type_smoke() {
    let started = Instant::now();
    let (files, database) = support::new_database("phase5-100k");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID NUMBER(10,0) PRIMARY KEY, N NUMBER(12,2), C CHAR(8), TS TIMESTAMP(9), R RAW(16))",
    );
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1,:2,:3,:4,:5)")
        .unwrap();
    for id in 0..ROWS {
        let values = scale_parameters(id);
        connection
            .execute(&insert, Parameters::positional(&values))
            .unwrap();
    }
    assert_count(&mut connection, ROWS);
    support::execute_sql(
        &mut connection,
        "UPDATE T SET N = N + 1 WHERE ID BETWEEN 40000 AND 49999",
    );
    support::execute_sql(&mut connection, "DELETE FROM T WHERE ID >= 90000");
    assert_count(&mut connection, 90_000);
    drop(connection);
    drop(database);
    let reopened = Database::open(files.main(), support::scale_options()).unwrap();
    let mut connection = reopened.connect().unwrap();
    assert_count(&mut connection, 90_000);
    assert_eq!(
        support::collect(&mut connection, "SELECT ID FROM T WHERE ID = 45000"),
        vec![vec![decimal_value(45_000, 0)]]
    );
    assert!(support::collect(&mut connection, "SELECT ID FROM T WHERE ID = 95000").is_empty());
    eprintln!(
        "phase5_100k processed_rows={ROWS} reopened_rows=90000 duration_ms={}",
        started.elapsed().as_millis()
    );
}
