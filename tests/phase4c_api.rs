mod support;

use std::fs;

use ora_sqllite::{DataType, Database, ErrorCode, Options, ParameterValue, Parameters, Value};

use support::{collect, collect_statement, execute_sql, execute_statement};

#[test]
fn sql_transaction_commits_rolls_back_reads_own_writes_and_drops_uncommitted_work() {
    // Breaks caught: using committed catalog/data inside SqlWrite, publishing
    // ROLLBACK or Drop changes, or failing to publish COMMIT.
    let (files, database) = support::new_database("sql-transaction-flow");
    let mut connection = database.connect().unwrap();

    assert_eq!(execute_sql(&mut connection, "BEGIN").rows_affected(), 0);
    execute_sql(&mut connection, "CREATE TABLE ROLLED_BACK (ID INT64)");
    execute_sql(&mut connection, "INSERT INTO ROLLED_BACK VALUES (1)");
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM ROLLED_BACK"),
        vec![vec![Value::Int64(1)]]
    );
    assert_eq!(execute_sql(&mut connection, "ROLLBACK").rows_affected(), 0);
    assert_eq!(
        connection
            .prepare("SELECT ID FROM ROLLED_BACK")
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );

    execute_sql(&mut connection, "BEGIN");
    execute_sql(&mut connection, "CREATE TABLE COMMITTED (ID INT64)");
    execute_sql(&mut connection, "INSERT INTO COMMITTED VALUES (7)");
    let mut observer = database.connect().unwrap();
    assert_eq!(
        observer
            .prepare("SELECT ID FROM COMMITTED")
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
    assert_eq!(execute_sql(&mut connection, "COMMIT").rows_affected(), 0);
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM COMMITTED"),
        vec![vec![Value::Int64(7)]]
    );
    assert_eq!(
        collect(&mut observer, "SELECT ID FROM COMMITTED"),
        vec![vec![Value::Int64(7)]]
    );
    drop(observer);

    execute_sql(&mut connection, "BEGIN");
    execute_sql(&mut connection, "INSERT INTO COMMITTED VALUES (9)");
    drop(connection);
    drop(database);

    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM COMMITTED ORDER BY ID"),
        vec![vec![Value::Int64(7)]]
    );
}

#[test]
fn autocommit_crud_counts_and_visibility_survive_reopen() {
    // Breaks caught: not committing the complete write batch, returning wrong
    // affected counts, or making a failed statement partially visible.
    let (files, database) = support::new_database("autocommit-crud");
    let mut connection = database.connect().unwrap();
    assert_eq!(
        execute_sql(&mut connection, "CREATE TABLE T (ID INT64, V INT64)").rows_affected(),
        0
    );
    assert_eq!(
        execute_sql(&mut connection, "INSERT INTO T VALUES (1, 10)").rows_affected(),
        1
    );
    assert_eq!(
        execute_sql(&mut connection, "INSERT INTO T VALUES (2, 20)").rows_affected(),
        1
    );
    assert_eq!(
        execute_sql(&mut connection, "UPDATE T SET V = V + 1 WHERE ID = 2").rows_affected(),
        1
    );
    assert_eq!(
        execute_sql(&mut connection, "DELETE FROM T WHERE ID = 1").rows_affected(),
        1
    );

    let insert = connection.prepare("INSERT INTO T VALUES (:1, :2)").unwrap();
    let invalid = [
        Value::Int64(3).into(),
        ParameterValue::typed_null(DataType::Text).unwrap(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&invalid))
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );
    assert_eq!(
        collect(&mut connection, "SELECT ID, V FROM T ORDER BY ID"),
        vec![vec![Value::Int64(2), Value::Int64(21)]]
    );
    drop(connection);
    drop(database);

    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    assert_eq!(
        collect(&mut connection, "SELECT ID, V FROM T"),
        vec![vec![Value::Int64(2), Value::Int64(21)]]
    );
}

#[test]
fn ordinary_sql_transaction_error_restores_statement_and_allows_commit() {
    // Breaks caught: poisoning SqlWrite for an ordinary Type error or keeping
    // partial statement changes instead of the statement checkpoint image.
    let (_files, database) = support::new_database("sql-ordinary-continuation");
    let mut connection = database.connect().unwrap();
    execute_sql(&mut connection, "CREATE TABLE T (ID INT64, V INT64)");
    execute_sql(&mut connection, "INSERT INTO T VALUES (1, 2)");
    execute_sql(&mut connection, "INSERT INTO T VALUES (2, 0)");

    execute_sql(&mut connection, "BEGIN");
    let failing = connection.prepare("UPDATE T SET V = 100 / V").unwrap();
    assert_eq!(
        connection
            .execute(&failing, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );
    assert_eq!(
        collect(&mut connection, "SELECT ID, V FROM T ORDER BY ID"),
        vec![
            vec![Value::Int64(1), Value::Int64(2)],
            vec![Value::Int64(2), Value::Int64(0)],
        ]
    );
    execute_sql(&mut connection, "INSERT INTO T VALUES (3, 3)");
    execute_sql(&mut connection, "COMMIT");
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM T ORDER BY ID"),
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
        ]
    );
}

#[test]
fn failed_autocommit_restores_earlier_row_mutations_and_releases_the_writer() {
    // Breaks caught: publishing row 1 before row 2 fails, or retaining the
    // abandoned writer lease after the auto-commit batch is discarded.
    let (_files, database) = support::new_database("autocommit-late-error");
    let mut connection = database.connect().unwrap();
    execute_sql(&mut connection, "CREATE TABLE T (ID INT64, V INT64)");
    execute_sql(&mut connection, "INSERT INTO T VALUES (1, 2)");
    execute_sql(&mut connection, "INSERT INTO T VALUES (2, 0)");
    let update = connection.prepare("UPDATE T SET V = 100 / V").unwrap();
    assert_eq!(
        connection
            .execute(&update, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );
    assert_eq!(
        collect(&mut connection, "SELECT ID, V FROM T ORDER BY ID"),
        vec![
            vec![Value::Int64(1), Value::Int64(2)],
            vec![Value::Int64(2), Value::Int64(0)],
        ]
    );
    let mut second = database.connect().unwrap();
    let transaction = second.begin_write().unwrap();
    transaction.rollback().unwrap();
}

#[test]
fn invalid_and_nested_controls_preserve_the_existing_state() {
    // Breaks caught: accepting control in the wrong state or accidentally
    // discarding the active SQL batch after a rejected nested BEGIN/COMMIT.
    let (_files, database) = support::new_database("sql-invalid-controls");
    let mut connection = database.connect().unwrap();
    let begin = connection.prepare("BEGIN").unwrap();
    let commit = connection.prepare("COMMIT").unwrap();
    let rollback = connection.prepare("ROLLBACK").unwrap();
    assert_eq!(
        connection
            .execute(&commit, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Transaction
    );
    assert_eq!(
        connection
            .execute(&rollback, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Transaction
    );

    execute_statement(&mut connection, &begin, Parameters::none());
    assert_eq!(
        connection
            .execute(&begin, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Transaction
    );
    assert_eq!(
        connection.begin_read().unwrap_err().code(),
        ErrorCode::Transaction
    );
    assert_eq!(
        connection.begin_write().unwrap_err().code(),
        ErrorCode::Transaction
    );
    let unexpected = [Value::Int64(1).into()];
    assert_eq!(
        connection
            .execute(&rollback, Parameters::positional(&unexpected))
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
    execute_sql(&mut connection, "CREATE TABLE T (ID INT64)");
    execute_statement(&mut connection, &rollback, Parameters::none());
    assert_eq!(
        connection.prepare("SELECT ID FROM T").unwrap_err().code(),
        ErrorCode::Bind
    );
}

#[test]
fn wrong_connection_api_kinds_are_rejected_without_changing_state() {
    // Breaks caught: routing queries through the write sink or DDL/control
    // through Rows would bypass the public result and ownership contracts.
    let (_files, database) = support::new_database("connection-api-kinds");
    let mut connection = database.connect().unwrap();
    let query = connection.prepare("SELECT 1").unwrap();
    assert_eq!(
        connection
            .execute(&query, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );
    let create = connection.prepare("CREATE TABLE T (ID INT64)").unwrap();
    assert_eq!(
        connection
            .query(&create, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );
    let begin = connection.prepare("BEGIN").unwrap();
    assert_eq!(
        connection
            .query(&begin, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );
    execute_statement(&mut connection, &create, Parameters::none());
}

#[test]
fn begin_and_autocommit_are_nonblocking_busy_but_validation_precedes_writer_acquisition() {
    // Breaks caught: blocking/acquiring the writer before deciding BEGIN/DDL,
    // returning Busy before parameter validation, or retaining the Task 4
    // temporary constrained-CREATE rejection after that boundary is removed.
    let (_files, database) = support::new_database("connection-writer-conflicts");
    let mut setup = database.connect().unwrap();
    execute_sql(&mut setup, "CREATE TABLE T (ID INT64)");
    let create_index = setup.prepare("CREATE INDEX IX_T ON T (ID)").unwrap();
    let constrained = setup
        .prepare("CREATE TABLE PENDING (ID INT64 PRIMARY KEY)")
        .unwrap();
    let insert = setup.prepare("INSERT INTO T VALUES (:1)").unwrap();
    drop(setup);

    let mut owner = database.connect().unwrap();
    let mut contender = database.connect().unwrap();
    let owner_write = owner.begin_write().unwrap();
    let begin = contender.prepare("BEGIN").unwrap();
    assert_eq!(
        contender
            .execute(&begin, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Busy
    );
    assert_eq!(
        contender
            .execute(&create_index, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Busy
    );
    assert_eq!(
        contender
            .execute(&constrained, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Busy
    );
    assert_eq!(
        contender
            .execute(&insert, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
    let value = [Value::Int64(1).into()];
    assert_eq!(
        contender
            .execute(&insert, Parameters::positional(&value))
            .unwrap_err()
            .code(),
        ErrorCode::Busy
    );
    owner_write.rollback().unwrap();
    assert_eq!(
        execute_statement(&mut contender, &create_index, Parameters::none()).rows_affected(),
        0
    );

    execute_sql(&mut contender, "BEGIN");
    let mut second = database.connect().unwrap();
    assert_eq!(second.begin_write().unwrap_err().code(), ErrorCode::Busy);
    execute_sql(&mut contender, "ROLLBACK");
}

#[test]
fn read_only_connection_rejects_sql_begin_and_autocommit_writes() {
    // Break caught: routing SQL writes around Pager access mode would make the
    // new Connection path less restrictive than the Rust transaction API.
    let (files, database) = support::new_database("read-only-sql-write");
    drop(database);
    let read_only =
        ora_sqllite::Database::open(files.main(), ora_sqllite::Options::default().read_only())
            .unwrap();
    let mut connection = read_only.connect().unwrap();
    let begin = connection.prepare("BEGIN").unwrap();
    let create = connection.prepare("CREATE TABLE T (ID INT64)").unwrap();
    assert_eq!(
        connection
            .execute(&begin, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
    assert_eq!(
        connection
            .execute(&create, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
    assert_eq!(
        collect(&mut connection, "SELECT 1"),
        vec![vec![Value::Int64(1)]]
    );
}

#[test]
fn retained_statement_schema_and_sql_rust_transaction_boundaries_remain_sound() {
    // Breaks caught: recompiling against the wrong catalog, losing immutable
    // metadata, or allowing SQL controls inside Rust transaction ownership.
    let (_files, database) = support::new_database("retained-statement-boundaries");
    let mut connection = database.connect().unwrap();
    execute_sql(&mut connection, "CREATE TABLE T (ID INT64)");
    let select = connection.prepare("SELECT ID FROM T ORDER BY ID").unwrap();
    let cloned = select.clone();
    let begin = connection.prepare("BEGIN").unwrap();

    let mut rust_write = connection.begin_write().unwrap();
    assert_eq!(
        rust_write
            .execute(&begin, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Transaction
    );
    rust_write.rollback().unwrap();

    execute_statement(&mut connection, &begin, Parameters::none());
    execute_sql(&mut connection, "INSERT INTO T VALUES (8)");
    assert_eq!(
        collect_statement(&mut connection, &cloned, Parameters::none()),
        vec![vec![Value::Int64(8)]]
    );
    assert_eq!(select.columns(), cloned.columns());
    execute_sql(&mut connection, "COMMIT");
}

#[test]
fn fixed_read_snapshots_coexist_with_a_writer_and_do_not_observe_future_commits() {
    // Breaks caught: reopening the snapshot per row, preventing a writer while
    // readers exist, or allowing an old snapshot to observe future values.
    let (_files, database) = support::new_database("fixed-snapshot-concurrency");
    let mut setup = database.connect().unwrap();
    execute_sql(&mut setup, "CREATE TABLE T (ID INT64, V INT64)");
    execute_sql(&mut setup, "INSERT INTO T VALUES (1, 10)");
    execute_sql(&mut setup, "INSERT INTO T VALUES (2, 20)");
    drop(setup);

    let mut first_connection = database.connect().unwrap();
    let mut second_connection = database.connect().unwrap();
    let mut writer = database.connect().unwrap();
    let mut first_read = first_connection.begin_read().unwrap();
    let mut second_read = second_connection.begin_read().unwrap();
    let first_statement = first_read
        .prepare("SELECT ID, V FROM T ORDER BY ID")
        .unwrap();
    let second_statement = second_read.prepare("SELECT V FROM T WHERE ID = 2").unwrap();
    let mut first_rows = first_read
        .query(&first_statement, Parameters::none())
        .unwrap();
    assert_eq!(
        first_rows.next().unwrap().unwrap().values(),
        &[Value::Int64(1), Value::Int64(10)]
    );

    assert_eq!(
        execute_sql(&mut writer, "UPDATE T SET V = 99 WHERE ID = 2").rows_affected(),
        1
    );
    assert_eq!(
        first_rows.next().unwrap().unwrap().values(),
        &[Value::Int64(2), Value::Int64(20)]
    );
    drop(first_rows);
    let mut second_rows = second_read
        .query(&second_statement, Parameters::none())
        .unwrap();
    assert_eq!(
        second_rows.next().unwrap().unwrap().values(),
        &[Value::Int64(20)]
    );
    drop(second_rows);
    drop((first_read, second_read));
    assert_eq!(
        collect(&mut writer, "SELECT V FROM T WHERE ID = 2"),
        vec![vec![Value::Int64(99)]]
    );
}

#[test]
fn retryable_commit_busy_preserves_sql_transaction_until_later_commit() {
    // Production break caught: moving the batch out before COMMIT and mapping
    // a pre-WAL Busy error to Idle loses uncommitted work and prevents retry.
    let (files, database) = support::new_database("retryable-sql-commit");
    let mut setup = database.connect().unwrap();
    execute_sql(&mut setup, "CREATE TABLE T (ID INT64)");
    execute_sql(&mut setup, "INSERT INTO T VALUES (1)");
    drop(setup);
    drop(database);

    let wal_len = fs::metadata(files.wal()).unwrap().len();
    let options = Options::default()
        .with_checkpoint_wal_bytes(1_000_000)
        .with_max_wal_bytes(wal_len.checked_add(1).unwrap());
    let database = Database::open(files.main(), options).unwrap();
    let mut blocker_connection = database.connect().unwrap();
    let blocker = blocker_connection.begin_read().unwrap();
    let mut writer = database.connect().unwrap();

    execute_sql(&mut writer, "BEGIN");
    execute_sql(&mut writer, "INSERT INTO T VALUES (2)");
    assert_eq!(
        collect(&mut writer, "SELECT ID FROM T ORDER BY ID"),
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
    let commit = writer.prepare("COMMIT").unwrap();
    assert_eq!(
        writer
            .execute(&commit, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Busy
    );
    assert_eq!(
        collect(&mut writer, "SELECT ID FROM T ORDER BY ID"),
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );

    drop(blocker);
    drop(blocker_connection);
    assert_eq!(
        writer
            .execute(&commit, Parameters::none())
            .unwrap()
            .rows_affected(),
        0
    );
    let mut observer = database.connect().unwrap();
    assert_eq!(
        collect(&mut observer, "SELECT ID FROM T ORDER BY ID"),
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
}
