#[allow(dead_code)]
mod support;

use ora_sqllite::{Connection, Parameters, Value};

use support::{collect, execute_sql};

fn scalar_i64(connection: &mut Connection, sql: &str) -> i64 {
    let rows = collect(connection, sql);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    match rows[0][0] {
        Value::Int64(value) => value,
        ref value => panic!("expected INT64 scalar, got {value:?}"),
    }
}

#[test]
#[ignore = "100,000-row Phase 4C1 smoke"]
fn phase_4c1_hundred_thousand_row_smoke() {
    // Breaks caught: failing to sustain deterministic parameterized writes in
    // one SQL transaction, incorrect grouping/order or affected-row counts,
    // and failure to retain the committed post-mutation count after reopen.
    let (_files, database) = support::new_database("phase4c-scale");
    let mut connection = database.connect().unwrap();

    execute_sql(&mut connection, "BEGIN");
    execute_sql(
        &mut connection,
        "CREATE TABLE T (ID INT64, G INT64, PAYLOAD TEXT)",
    );
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3)")
        .unwrap();
    for id in 0i64..100_000 {
        let values = [
            Value::Int64(id).into(),
            Value::Int64(id % 100).into(),
            Value::Text(format!("row-{id}")).into(),
        ];
        connection
            .execute(&insert, Parameters::positional(&values))
            .unwrap();
    }
    execute_sql(&mut connection, "COMMIT");

    assert_eq!(
        scalar_i64(&mut connection, "SELECT COUNT(*) FROM T"),
        100_000
    );
    let grouped = collect(
        &mut connection,
        "SELECT G, COUNT(*) FROM T GROUP BY G ORDER BY G",
    );
    assert_eq!(grouped.len(), 100);
    for (group, row) in (0i64..100).zip(grouped) {
        assert_eq!(row, vec![Value::Int64(group), Value::Int64(1_000)]);
    }
    assert_eq!(
        execute_sql(&mut connection, "UPDATE T SET G = G + 1 WHERE ID < 1000").rows_affected(),
        1_000
    );
    assert_eq!(
        execute_sql(&mut connection, "DELETE FROM T WHERE ID >= 99000").rows_affected(),
        1_000
    );

    drop(connection);
    let mut reopened = database.connect().unwrap();
    assert_eq!(scalar_i64(&mut reopened, "SELECT COUNT(*) FROM T"), 99_000);
}
