mod common;

use ora_sqllite::{DataType, ErrorCode, Parameters, Value};

use common::{assert_constraint, assert_no_table, collect, execute_sql};

fn assert_unsupported_without_mutation(connection: &mut ora_sqllite::Connection, sql: &str) {
    match connection.prepare(sql) {
        Ok(statement) => {
            let error = connection
                .execute(&statement, Parameters::none())
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported, "{sql}: {error}");
        }
        Err(error) => assert_eq!(error.code(), ErrorCode::Unsupported, "{sql}: {error}"),
    }
}

#[test]
fn local_constraints_create_crud_commit_rollback_and_reopen() {
    // Breaks caught: a public CREATE that omits constraint metadata, violations
    // mapped to the wrong stable name, or a failed statement that poisons the
    // surrounding SQL transaction instead of restoring just that statement.
    let (files, database) = common::new_database("phase4c2-local-constraints");
    let mut connection = database.connect().unwrap();
    let create = connection
        .prepare(
            "CREATE TABLE T (
                ID INT64 CONSTRAINT PK_T PRIMARY KEY,
                EMAIL TEXT CONSTRAINT UQ_T UNIQUE,
                SCORE INT64 CONSTRAINT NN_T NOT NULL,
                CONSTRAINT CK_T CHECK (SCORE BETWEEN 0 AND 100)
            )",
        )
        .unwrap();
    assert!(create.parameters().is_empty());
    assert!(create.columns().is_empty());
    assert_eq!(
        connection
            .execute(&create, Parameters::none())
            .unwrap()
            .rows_affected(),
        0
    );

    let insert = connection
        .prepare("INSERT INTO T (ID, EMAIL, SCORE) VALUES (:1, :2, :3)")
        .unwrap();
    assert_eq!(insert.parameters().len(), 3);
    assert!(
        insert
            .parameters()
            .iter()
            .all(|parameter| parameter.data_type() != Some(DataType::Bool))
    );
    assert!(insert.columns().is_empty());
    let initial = [
        Value::Int64(1).into(),
        Value::Text("a@example".into()).into(),
        Value::Int64(50).into(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&initial))
            .unwrap()
            .rows_affected(),
        1
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO T VALUES (1, 'b@example', 20)",
        "PK_T",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO T VALUES (2, 'a@example', 20)",
        "UQ_T",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO T VALUES (2, 'b@example', NULL)",
        "NN_T",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO T VALUES (2, 'b@example', 101)",
        "CK_T",
    );

    assert_eq!(execute_sql(&mut connection, "BEGIN").rows_affected(), 0);
    let committed = [
        Value::Int64(2).into(),
        Value::Text("b@example".into()).into(),
        Value::Int64(20).into(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&committed))
            .unwrap()
            .rows_affected(),
        1
    );
    let duplicate = [
        Value::Int64(3).into(),
        Value::Text("a@example".into()).into(),
        Value::Int64(30).into(),
    ];
    let error = connection
        .execute(&insert, Parameters::positional(&duplicate))
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert!(error.to_string().contains("UQ_T"));
    let after_error = [
        Value::Int64(3).into(),
        Value::Text("c@example".into()).into(),
        Value::Int64(30).into(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&after_error))
            .unwrap()
            .rows_affected(),
        1
    );
    assert_eq!(execute_sql(&mut connection, "COMMIT").rows_affected(), 0);
    let select = connection.prepare("SELECT ID FROM T ORDER BY ID").unwrap();
    assert_eq!(select.columns().len(), 1);
    assert_eq!(select.columns()[0].name(), "ID");
    assert_eq!(select.columns()[0].data_type(), DataType::Int64);
    let mut rows = connection.query(&select, Parameters::none()).unwrap();
    assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(1)]);
    assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(2)]);
    assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(3)]);
    assert!(rows.next().unwrap().is_none());
    drop(rows);

    let mut rust_transaction = connection.begin_write().unwrap();
    let rust_insert = rust_transaction
        .prepare("INSERT INTO T VALUES (:1, :2, :3)")
        .unwrap();
    let rolled_back = [
        Value::Int64(5).into(),
        Value::Text("rust-rollback@example".into()).into(),
        Value::Int64(50).into(),
    ];
    assert_eq!(
        rust_transaction
            .execute(&rust_insert, Parameters::positional(&rolled_back))
            .unwrap()
            .rows_affected(),
        1
    );
    rust_transaction.rollback().unwrap();
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM T WHERE ID = 5"),
        Vec::<Vec<Value>>::new()
    );

    assert_eq!(execute_sql(&mut connection, "BEGIN").rows_affected(), 0);
    assert_eq!(
        execute_sql(
            &mut connection,
            "CREATE TABLE ROLLED (
                ID INT64 CONSTRAINT PK_ROLLED PRIMARY KEY,
                SCORE INT64 CONSTRAINT NN_ROLLED NOT NULL
            )",
        )
        .rows_affected(),
        0
    );
    assert_eq!(
        execute_sql(&mut connection, "INSERT INTO ROLLED VALUES (1, 1)").rows_affected(),
        1
    );
    assert_eq!(execute_sql(&mut connection, "ROLLBACK").rows_affected(), 0);
    assert_no_table(&mut connection, "ROLLED");

    let automatic = [
        Value::Int64(4).into(),
        Value::Text("d@example".into()).into(),
        Value::Int64(40).into(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&automatic))
            .unwrap()
            .rows_affected(),
        1
    );
    drop(connection);
    drop(database);

    let reopened = common::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM T ORDER BY ID"),
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
        ]
    );
}

#[test]
fn prepared_constrained_create_rechecks_table_and_explicit_constraint_names() {
    // Breaks caught: retaining only the table name lets a prepared CREATE claim
    // an explicit constraint that a later committed CREATE already owns.
    let (_files, database) = common::new_database("phase4c2-prepared-name-race");
    let mut connection = database.connect().unwrap();
    let prepared = connection
        .prepare("CREATE TABLE A (ID INT64 CONSTRAINT PK_SHARED PRIMARY KEY)")
        .unwrap();
    execute_sql(
        &mut connection,
        "CREATE TABLE B (ID INT64 CONSTRAINT PK_SHARED PRIMARY KEY)",
    );
    let error = connection
        .execute(&prepared, Parameters::none())
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::SchemaChanged);
    assert_no_table(&mut connection, "A");

    let prepared = connection
        .prepare("CREATE TABLE TABLE_RACE (ID INT64 CONSTRAINT PK_TABLE_RACE PRIMARY KEY)")
        .unwrap();
    execute_sql(&mut connection, "CREATE TABLE TABLE_RACE (ID INT64)");
    let error = connection
        .execute(&prepared, Parameters::none())
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::SchemaChanged);
    execute_sql(&mut connection, "DROP TABLE TABLE_RACE");
    assert_eq!(
        execute_sql(
            &mut connection,
            "CREATE TABLE TABLE_RACE (ID INT64 CONSTRAINT PK_TABLE_RACE PRIMARY KEY)",
        )
        .rows_affected(),
        0
    );

    // Generated constraint names are selected at mutation time; unrelated
    // unnamed declarations must not become stale Statement name dependencies.
    let generated = connection
        .prepare("CREATE TABLE GENERATED_A (ID INT64 PRIMARY KEY)")
        .unwrap();
    execute_sql(
        &mut connection,
        "CREATE TABLE GENERATED_B (ID INT64 PRIMARY KEY)",
    );
    assert_eq!(
        connection
            .execute(&generated, Parameters::none())
            .unwrap()
            .rows_affected(),
        0
    );
}

#[test]
fn declaration_forms_keep_null_empty_text_and_check_semantics_distinct() {
    // Breaks caught: losing quoted/composite identities, converting an empty
    // text to NULL, or accepting FALSE local CHECK results after persistence.
    let (_files, database) = common::new_database("phase4c2-declaration-forms");
    let mut connection = database.connect().unwrap();
    execute_sql(
        &mut connection,
        "CREATE TABLE \"Composite Table\" (
            \"Left\" INT64,
            \"Right\" INT64,
            LABEL TEXT,
            TAG TEXT,
            CONSTRAINT \"PK Composite\" PRIMARY KEY (\"Left\", \"Right\"),
            CONSTRAINT \"UQ Composite\" UNIQUE (LABEL, TAG)
        )",
    );
    execute_sql(
        &mut connection,
        "INSERT INTO \"Composite Table\" VALUES (1, 1, 'label', 'tag')",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO \"Composite Table\" VALUES (1, 1, 'other', 'other')",
        "PK Composite",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO \"Composite Table\" VALUES (2, 2, 'label', 'tag')",
        "UQ Composite",
    );

    execute_sql(
        &mut connection,
        "CREATE TABLE COLUMN_FORMS (
            ID INT64 CONSTRAINT NN_ID NOT NULL CONSTRAINT PK_COL PRIMARY KEY,
            U_NAMED TEXT CONSTRAINT UQ_NAMED UNIQUE,
            U_ANON TEXT UNIQUE,
            NN_NAMED TEXT CONSTRAINT NN_NAMED NOT NULL,
            NN_ANON TEXT NOT NULL,
            CK_NAMED INT64 CONSTRAINT CK_NAMED CHECK (CK_NAMED > 0),
            CK_ANON INT64 CHECK (CK_ANON < 10)
        )",
    );
    execute_sql(
        &mut connection,
        "INSERT INTO COLUMN_FORMS VALUES (1, 'named', 'anon', 'required', 'also', 1, 1)",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO COLUMN_FORMS VALUES (1, 'other', 'other', 'required', 'also', 1, 1)",
        "PK_COL",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO COLUMN_FORMS VALUES (2, 'named', 'other', 'required', 'also', 1, 1)",
        "UQ_NAMED",
    );
    for sql in [
        "INSERT INTO COLUMN_FORMS VALUES (2, 'other', 'anon', 'required', 'also', 1, 1)",
        "INSERT INTO COLUMN_FORMS VALUES (2, 'other', 'unique', NULL, 'also', 1, 1)",
        "INSERT INTO COLUMN_FORMS VALUES (2, 'other', 'unique', 'required', NULL, 1, 1)",
        "INSERT INTO COLUMN_FORMS VALUES (2, 'other', 'unique', 'required', 'also', 0, 1)",
        "INSERT INTO COLUMN_FORMS VALUES (2, 'other', 'unique', 'required', 'also', 1, 10)",
    ] {
        let statement = connection.prepare(sql).unwrap();
        assert_eq!(
            connection
                .execute(&statement, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Constraint,
            "{sql}"
        );
    }
    execute_sql(
        &mut connection,
        "CREATE TABLE ANON_PRIMARY (ID INT64 PRIMARY KEY)",
    );
    execute_sql(&mut connection, "INSERT INTO ANON_PRIMARY VALUES (1)");
    let duplicate_primary = connection
        .prepare("INSERT INTO ANON_PRIMARY VALUES (1)")
        .unwrap();
    assert_eq!(
        connection
            .execute(&duplicate_primary, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Constraint
    );

    execute_sql(
        &mut connection,
        "CREATE TABLE UNIQUE_TEXT (ID INT64, TOKEN TEXT CONSTRAINT UQ_TOKEN UNIQUE)",
    );
    for id in [1, 2] {
        execute_sql(
            &mut connection,
            &format!("INSERT INTO UNIQUE_TEXT VALUES ({id}, NULL)"),
        );
    }
    execute_sql(&mut connection, "INSERT INTO UNIQUE_TEXT VALUES (3, '')");
    assert_constraint(
        &mut connection,
        "INSERT INTO UNIQUE_TEXT VALUES (4, '')",
        "UQ_TOKEN",
    );
    assert_eq!(
        collect(
            &mut connection,
            "SELECT TOKEN FROM UNIQUE_TEXT WHERE ID = 3"
        ),
        vec![vec![Value::Text(String::new())]]
    );
    execute_sql(
        &mut connection,
        "CREATE UNIQUE INDEX UX_UNIQUE_TEXT_ID ON UNIQUE_TEXT (ID)",
    );
    assert_constraint(
        &mut connection,
        "INSERT INTO UNIQUE_TEXT VALUES (3, 'different')",
        "UX_UNIQUE_TEXT_ID",
    );

    execute_sql(
        &mut connection,
        "CREATE TABLE CHECK_FORMS (
            N INT64,
            TEXT_VALUE TEXT,
            CONSTRAINT CK_ALL CHECK (
                (N + 1) >= 2
                AND N BETWEEN 1 AND 7
                AND N IN (1, 2, 3, 4, 5, 6, 7)
                AND (TEXT_VALUE IS NULL OR TEXT_VALUE LIKE 'ok%')
            )
        )",
    );
    execute_sql(&mut connection, "INSERT INTO CHECK_FORMS VALUES (1, 'ok')");
    execute_sql(&mut connection, "INSERT INTO CHECK_FORMS VALUES (2, NULL)");
    for sql in [
        "INSERT INTO CHECK_FORMS VALUES (0, 'ok')",
        "INSERT INTO CHECK_FORMS VALUES (1, '')",
        "INSERT INTO CHECK_FORMS VALUES (8, 'okay')",
        "INSERT INTO CHECK_FORMS VALUES (1, 'not-ok')",
    ] {
        assert_constraint(&mut connection, sql, "CK_ALL");
    }
}

#[test]
fn constrained_prepared_dml_stays_usable_after_root_splits() {
    // Breaks caught: ordinary table or system-index root changes invalidating a
    // schema-stable DML Statement, or forgetting a constraint index on later rows.
    let (files, database) = common::new_database("phase4c2-prepared-dml-splits");
    let mut connection = database.connect().unwrap();
    execute_sql(
        &mut connection,
        "CREATE TABLE T (
            ID INT64 CONSTRAINT PK_T PRIMARY KEY,
            VALUE TEXT CONSTRAINT UQ_T UNIQUE
        )",
    );
    let insert = connection.prepare("INSERT INTO T VALUES (:1, :2)").unwrap();
    for id in 0..128 {
        let values = [
            Value::Int64(id).into(),
            Value::Text(format!("value-{id:03}-{}", "x".repeat(96))).into(),
        ];
        assert_eq!(
            connection
                .execute(&insert, Parameters::positional(&values))
                .unwrap()
                .rows_affected(),
            1
        );
    }
    let values = [
        Value::Int64(128).into(),
        Value::Text("after-split".into()).into(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&values))
            .unwrap()
            .rows_affected(),
        1
    );
    let update = connection
        .prepare("UPDATE T SET VALUE = :1 WHERE ID = :2")
        .unwrap();
    let update_values = [
        Value::Text("updated".into()).into(),
        Value::Int64(128).into(),
    ];
    assert_eq!(
        connection
            .execute(&update, Parameters::positional(&update_values))
            .unwrap()
            .rows_affected(),
        1
    );
    let delete = connection.prepare("DELETE FROM T WHERE ID = :1").unwrap();
    let delete_values = [Value::Int64(128).into()];
    assert_eq!(
        connection
            .execute(&delete, Parameters::positional(&delete_values))
            .unwrap()
            .rows_affected(),
        1
    );
    drop(connection);
    drop(database);
    let reopened = common::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    assert_eq!(collect(&mut connection, "SELECT ID FROM T").len(), 128);
}

#[test]
fn unsupported_constraint_and_phase5_forms_leave_catalog_unchanged() {
    // Breaks caught: treating future syntax as a partial CREATE and publishing
    // a table before rejecting foreign-key actions, deferred checks, or Phase 5.
    let (_files, database) = common::new_database("phase4c2-unsupported-contract");
    let mut connection = database.connect().unwrap();
    execute_sql(
        &mut connection,
        "CREATE TABLE PARENT (ID INT64 CONSTRAINT PK_PARENT PRIMARY KEY)",
    );
    for (sql, absent) in [
        (
            "CREATE TABLE FOREIGN_KEY (ID INT64, PARENT_ID INT64, CONSTRAINT FK FOREIGN KEY (PARENT_ID) REFERENCES PARENT (ID))",
            "FOREIGN_KEY",
        ),
        (
            "CREATE TABLE SELF_REF (ID INT64 PRIMARY KEY, PARENT_ID INT64 REFERENCES SELF_REF (ID))",
            "SELF_REF",
        ),
        ("ALTER TABLE PARENT ADD EXTRA INT64", "PARENT"),
        (
            "CREATE TABLE DEFERRED_T (ID INT64 CONSTRAINT PK_DEFERRED PRIMARY KEY DEFERRABLE)",
            "DEFERRED_T",
        ),
        (
            "CREATE TABLE DELETE_ACTION (ID INT64, PARENT_ID INT64 REFERENCES PARENT (ID) ON DELETE CASCADE)",
            "DELETE_ACTION",
        ),
        (
            "CREATE TABLE UPDATE_ACTION (ID INT64, PARENT_ID INT64 REFERENCES PARENT (ID) ON UPDATE SET NULL)",
            "UPDATE_ACTION",
        ),
        ("CREATE TABLE NUMBER_T (N NUMBER(10, 2))", "NUMBER_T"),
        ("CREATE TABLE VARCHAR_T (V VARCHAR2(30))", "VARCHAR_T"),
        (
            "CREATE TABLE FUNCTION_T (V INT64 CHECK (ABS(V) > 0))",
            "FUNCTION_T",
        ),
    ] {
        assert_unsupported_without_mutation(&mut connection, sql);
        if absent != "PARENT" {
            assert_no_table(&mut connection, absent);
        }
    }
    let missing_external = connection
        .prepare(
            "CREATE TABLE EXTERNAL_MISSING (ID INT64, PARENT_ID INT64 REFERENCES MISSING (ID))",
        )
        .unwrap_err();
    assert_eq!(missing_external.code(), ErrorCode::Bind);
    assert_no_table(&mut connection, "EXTERNAL_MISSING");
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM PARENT"),
        Vec::<Vec<Value>>::new()
    );
}
