#![allow(dead_code)]

mod support;

use ora_sqllite::{Database, ErrorCode, Options, Parameters, Value};

#[test]
fn drop_index_frees_tree_bumps_schema_and_never_clears_feature_bit() {
    // Storage tests inspect freed pages/feature bits; this public contract
    // catches a missing DROP executor, schema bump, or durable removal.
    let (files, database) = support::new_database("drop-last-index");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (7)");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
    let stale = connection.prepare("SELECT A FROM T").unwrap();
    assert_eq!(
        support::execute_sql(&mut connection, "DROP INDEX IX_T_A").rows_affected(),
        0
    );
    assert_eq!(
        connection
            .query(&stale, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::SchemaChanged
    );
    drop(connection);
    drop(database);
    let database = support::reopen(&files);
    let mut connection = database.connect().unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT A FROM T"),
        vec![vec![Value::Int64(7)]]
    );
    assert_eq!(
        connection.prepare("DROP INDEX IX_T_A").unwrap_err().code(),
        ErrorCode::Bind
    );
    support::execute_sql(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
}

#[test]
fn drop_table_removes_all_owned_explicit_indexes_atomically() {
    // Leaving any index record behind prevents valid reopen/name reuse.
    let (files, database) = support::new_database("drop-indexed-table");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64, B TEXT)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (1, 'one')");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T_B ON T (B)");
    support::execute_sql(&mut connection, "DROP TABLE T");
    drop(connection);
    drop(database);
    let database = support::reopen(&files);
    let mut connection = database.connect().unwrap();
    assert_eq!(
        connection.prepare("SELECT A FROM T").unwrap_err().code(),
        ErrorCode::Bind
    );
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64, B TEXT)");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T_B ON T (B)");
}

#[test]
fn drop_index_stale_plan_never_rebinds_recreated_name() {
    let (_files, database) = support::new_database("drop-index-stale-name");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64)");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T ON T (A)");
    let stale = connection.prepare("DROP INDEX IX_T").unwrap();
    support::execute_sql(&mut connection, "DROP INDEX IX_T");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T ON T (A)");
    assert_eq!(
        connection
            .execute(&stale, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::SchemaChanged
    );
    assert!(connection.prepare("DROP INDEX IX_T").is_ok());
    support::execute_sql(&mut connection, "DROP INDEX IX_T");
}

#[test]
fn drop_index_and_indexed_table_explicit_rollback_restore_prepared_statements() {
    for sql in ["DROP INDEX IX_T", "DROP TABLE T"] {
        let (_files, database) = support::new_database("drop-index-rollback");
        let mut connection = database.connect().unwrap();
        support::execute_sql(&mut connection, "CREATE TABLE T (A INT64)");
        support::execute_sql(&mut connection, "INSERT INTO T VALUES (7)");
        support::execute_sql(&mut connection, "CREATE INDEX IX_T ON T (A)");
        let retained = connection.prepare("SELECT A FROM T").unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let statement = transaction.prepare(sql).unwrap();
        transaction.execute(&statement, Parameters::none()).unwrap();
        transaction.rollback().unwrap();
        assert_eq!(
            support::collect_statement(&mut connection, &retained, Parameters::none()),
            vec![vec![Value::Int64(7)]]
        );
        assert!(connection.prepare("DROP INDEX IX_T").is_ok());
    }
}

#[test]
fn create_index_low_memory_rejects_indexed_and_unindexed_payloads_and_can_continue() {
    // Both a small physical key over a large unindexed row and an indexable
    // 900-byte text key must obey the connection's budget, not just key length.
    for (target, payload_len, limit) in [("B", 900, 10_000), ("A", 20_000, 16_384)] {
        let (files, database) = support::new_database("index-low-memory");
        let mut setup = database.connect().unwrap();
        support::execute_sql(&mut setup, "CREATE TABLE T (A INT64, B TEXT)");
        support::execute_sql(&mut setup, "INSERT INTO T VALUES (1, 'small')");
        support::execute_sql(
            &mut setup,
            &format!("INSERT INTO T VALUES (2, '{}')", "x".repeat(payload_len)),
        );
        drop(setup);
        drop(database);
        let database = Database::open(
            files.main(),
            Options::default().with_max_query_memory_bytes(limit),
        )
        .unwrap();
        let mut connection = database.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let retained = transaction
            .prepare("INSERT INTO T VALUES (3, 'after')")
            .unwrap();
        let create = transaction
            .prepare(&format!("CREATE INDEX IX_T ON T ({target})"))
            .unwrap();
        assert_eq!(
            transaction
                .execute(&create, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        assert!(
            transaction
                .prepare(&format!("CREATE INDEX IX_T ON T ({target})"))
                .is_ok()
        );
        transaction.execute(&retained, Parameters::none()).unwrap();
        transaction.commit().unwrap();
    }
}

#[test]
fn create_index_reuses_small_budget_for_each_row() {
    // The complete payload exceeds this budget. Each individual row and its
    // unique-key probe fit: the build must reuse, not accumulate, row charges.
    let (files, database) = support::new_database("index-budget-reuse");
    let mut setup = database.connect().unwrap();
    support::execute_sql(&mut setup, "CREATE TABLE T (A INT64, B TEXT)");
    let mut transaction = setup.begin_write().unwrap();
    for id in 0..120 {
        let insert = transaction
            .prepare(&format!(
                "INSERT INTO T VALUES ({id}, '{}')",
                "x".repeat(100)
            ))
            .unwrap();
        transaction.execute(&insert, Parameters::none()).unwrap();
    }
    transaction.commit().unwrap();
    drop(setup);
    drop(database);
    let database = Database::open(
        files.main(),
        Options::default().with_max_query_memory_bytes(10_000),
    )
    .unwrap();
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE UNIQUE INDEX IX_T ON T (A, B)");
    drop(connection);
    drop(database);
    let database = support::reopen(&files);
    let mut connection = database.connect().unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT A FROM T").len(),
        120
    );
    assert_eq!(
        connection
            .prepare("CREATE INDEX IX_T ON T (A)")
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
}

#[test]
fn create_index_backfills_commits_reopens_and_invalidates_old_table_plans() {
    // A missing executor, schema-version bump, or durable catalog publication
    // must fail through the real public API, not just a catalog constructor.
    let (files, database) = support::new_database("phase4c2-create-index");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64, B TEXT)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (2, 'B')");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (1, 'A')");
    let stale = connection.prepare("SELECT A FROM T").unwrap();
    assert_eq!(
        support::execute_sql(&mut connection, "CREATE INDEX IX_T_A ON T (A)").rows_affected(),
        0
    );
    assert_eq!(
        connection
            .query(&stale, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::SchemaChanged
    );
    drop(connection);
    drop(database);
    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT A FROM T ORDER BY A"),
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
    assert_eq!(
        connection
            .prepare("CREATE INDEX IX_T_A ON T (A)")
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
}

#[test]
fn create_index_unique_failure_keeps_explicit_transaction_usable() {
    // A duplicate found late in backfill must not consume the name or invalidate
    // existing table plans, and must not roll back earlier transaction writes.
    let (_files, database) = support::new_database("index-duplicate-rollback");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64)");
    let mut transaction = connection.begin_write().unwrap();
    for sql in ["INSERT INTO T VALUES (1)", "INSERT INTO T VALUES (1)"] {
        let statement = transaction.prepare(sql).unwrap();
        transaction.execute(&statement, Parameters::none()).unwrap();
    }
    let retained = transaction.prepare("INSERT INTO T VALUES (9)").unwrap();
    let create = transaction
        .prepare("CREATE UNIQUE INDEX UX_T_A ON T (A)")
        .unwrap();
    let error = transaction
        .execute(&create, Parameters::none())
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert!(error.to_string().contains("UX_T_A"));
    transaction.execute(&retained, Parameters::none()).unwrap();
    let create = transaction.prepare("CREATE INDEX UX_T_A ON T (A)").unwrap();
    transaction.execute(&create, Parameters::none()).unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT A FROM T ORDER BY A"),
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(1)],
            vec![Value::Int64(9)]
        ]
    );
}

#[test]
fn create_index_unique_null_composite_and_explicit_rollback() {
    // NULL-containing logical duplicates are allowed, and rolling a successful
    // build back must restore both the name namespace and the old schema version.
    let (_files, database) = support::new_database("index-null-rollback");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64, B INT64)");
    for row in [
        "NULL, NULL",
        "NULL, NULL",
        "1, NULL",
        "1, NULL",
        "NULL, 2",
        "NULL, 2",
        "1, 2",
        "2, 1",
    ] {
        support::execute_sql(&mut connection, &format!("INSERT INTO T VALUES ({row})"));
    }
    let retained = connection.prepare("SELECT A FROM T").unwrap();
    let mut transaction = connection.begin_write().unwrap();
    let create = transaction
        .prepare("CREATE UNIQUE INDEX UX_T_B_A ON T (B, A)")
        .unwrap();
    transaction.execute(&create, Parameters::none()).unwrap();
    transaction.rollback().unwrap();
    assert_eq!(
        support::collect_statement(&mut connection, &retained, Parameters::none()).len(),
        8
    );
    support::execute_sql(&mut connection, "CREATE UNIQUE INDEX UX_T_B_A ON T (B, A)");
}

#[test]
fn create_index_complete_key_boundary_is_atomic() {
    // Nonzero text: NULL tag (1) + payload + terminator (2) + ROWID (8).
    // Omitting ROWID from the 1024-byte limit would incorrectly accept 1014.
    for (text_len, expected) in [(1013, None), (1014, Some(ErrorCode::ResourceLimit))] {
        let (_files, database) = support::new_database("index-key-boundary");
        let mut connection = database.connect().unwrap();
        support::execute_sql(&mut connection, "CREATE TABLE T (A TEXT)");
        support::execute_sql(&mut connection, "INSERT INTO T VALUES ('small')");
        support::execute_sql(
            &mut connection,
            &format!("INSERT INTO T VALUES ('{}')", "x".repeat(text_len)),
        );
        let retained = connection.prepare("SELECT A FROM T").unwrap();
        let create = connection.prepare("CREATE INDEX IX_T_A ON T (A)").unwrap();
        let result = connection.execute(&create, Parameters::none());
        match expected {
            None => assert_eq!(result.unwrap().rows_affected(), 0),
            Some(code) => {
                assert_eq!(result.unwrap_err().code(), code);
                assert_eq!(
                    support::collect_statement(&mut connection, &retained, Parameters::none())
                        .len(),
                    2
                );
                assert!(connection.prepare("CREATE INDEX IX_T_A ON T (A)").is_ok());
                support::execute_sql(&mut connection, "INSERT INTO T VALUES ('after')");
            }
        }
    }
}

#[test]
fn create_index_read_only_prepare_and_execute_do_not_change_files() {
    // Preparing DDL must not publish the feature, and rejected execution must
    // not append either file. Byte equality is stronger than length equality.
    let (files, database) = support::new_database("index-read-only");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64)");
    drop(connection);
    drop(database);
    let before = (
        std::fs::read(files.main()).unwrap(),
        std::fs::read(files.wal()).unwrap(),
    );
    let readonly = Database::open(files.main(), Options::default().read_only()).unwrap();
    let mut connection = readonly.connect().unwrap();
    let create = connection.prepare("CREATE INDEX IX_T_A ON T (A)").unwrap();
    assert_eq!(
        connection
            .execute(&create, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Unsupported
    );
    drop(connection);
    drop(readonly);
    assert_eq!(
        (
            std::fs::read(files.main()).unwrap(),
            std::fs::read(files.wal()).unwrap()
        ),
        before
    );
}

#[test]
fn every_dml_keeps_one_index_entry_per_visible_row() {
    // Production break caught: UPDATE/DELETE that only change the table tree
    // leave old explicit-index keys behind, so subsequently reusable unique
    // keys incorrectly remain blocked.
    let (_files, database) = support::new_database("dml-index-maintenance");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64, B TEXT)");
    support::execute_sql(&mut connection, "CREATE UNIQUE INDEX UX_T_A ON T (A)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (1, 'A')");
    support::execute_sql(&mut connection, "UPDATE T SET A = 2, B = 'B' WHERE A = 1");
    let duplicate = connection
        .prepare("INSERT INTO T VALUES (2, 'duplicate')")
        .unwrap();
    assert_eq!(
        connection
            .execute(&duplicate, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Constraint
    );
    support::execute_sql(&mut connection, "DELETE FROM T WHERE A = 2");
    support::execute_sql(
        &mut connection,
        "INSERT INTO T VALUES (2, 'reused-after-delete')",
    );
    assert_eq!(
        support::collect(&mut connection, "SELECT A, B FROM T ORDER BY A"),
        vec![vec![
            Value::Int64(2),
            Value::Text("reused-after-delete".into())
        ],]
    );
}

#[test]
fn dml_late_unique_failure_restores_earlier_rows_and_keeps_transaction_usable() {
    // Production break caught: a later row's unique conflict must restore an
    // earlier row update from the same SQL statement, while retaining the
    // caller's explicit transaction for its next statement.
    let (_files, database) = support::new_database("dml-late-unique-rollback");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (A INT64)");
    support::execute_sql(&mut connection, "CREATE UNIQUE INDEX UX_T_A ON T (A)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (1)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (2)");
    let mut transaction = connection.begin_write().unwrap();
    let update = transaction.prepare("UPDATE T SET A = 3").unwrap();
    assert_eq!(
        transaction
            .execute(&update, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Constraint
    );
    let retained = transaction.prepare("INSERT INTO T VALUES (4)").unwrap();
    transaction.execute(&retained, Parameters::none()).unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT A FROM T ORDER BY A"),
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(4)],
        ]
    );
}
