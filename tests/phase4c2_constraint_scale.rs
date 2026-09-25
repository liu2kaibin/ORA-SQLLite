#![allow(dead_code)]

mod support;

use std::collections::BTreeSet;

use ora_sqllite::{Connection, ErrorCode, ParameterValue, Parameters, Value};

const SCALE_ROWS: i64 = 100_000;
const BATCH_ROWS: i64 = 10_000;

#[test]
#[ignore = "run by the dedicated Phase 4C2-B CI job"]
fn phase_4c2b_hundred_thousand_row_constraint_smoke() {
    // Breaks caught: losing rows or any table/system/explicit-index state
    // during sustained constrained DML; failing to restore an explicit
    // rollback; or reopening a checkpointed graph with missing constraint
    // ownership, keys, or roots.
    let (files, database) = support::new_database("phase4c2-constraint-scale");
    let mut connection = database.connect().expect("connect scale database");

    create_scale_constraint_table(&mut connection);
    insert_constraint_rows(&mut connection, SCALE_ROWS);
    assert_eq!(count_rows(&mut connection), SCALE_ROWS);
    assert_eq!(count_null_unique_keys(&mut connection), SCALE_ROWS / 10);

    update_constraint_rows(&mut connection, SCALE_ROWS);
    delete_constraint_rows(&mut connection, SCALE_ROWS);
    assert_constraint_rows_and_indexes(&mut connection, SCALE_ROWS);
    assert_duplicate_null_and_check_boundaries(&mut connection, SCALE_ROWS);

    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);

    assert_persisted_constraint_graph(&files, SCALE_ROWS);

    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().expect("connect reopened scale database");
    assert_constraint_rows_and_indexes(&mut connection, SCALE_ROWS);
    assert_reopened_system_indexes_reject_duplicates(&mut connection);
    drop(connection);
    drop(reopened);

    support::checkpoint_accepted_state(&files);
    assert_persisted_constraint_graph(&files, SCALE_ROWS);
}

#[test]
fn phase_4c2b_stale_format2_oracle_does_not_reject_persisted_freelist_after_constrained_update() {
    // Break caught: a stale format-2 oracle literal rejects a valid persisted
    // freelist after constrained UPDATE frees and reuses an overflow page.
    const ROWS: i64 = 100;

    let (files, database) = support::new_database("phase4c2-freelist-oracle");
    let mut connection = database
        .connect()
        .expect("connect freelist oracle database");
    create_scale_constraint_table(&mut connection);
    insert_constraint_rows(&mut connection, ROWS);
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    support::format2::raw_catalog_capture(files.main())
        .expect("capture the checkpointed constrained insert");

    let reopened = support::reopen(&files);
    let mut connection = reopened
        .connect()
        .expect("reopen before constrained update");
    update_constraint_rows(&mut connection, ROWS);
    drop(connection);
    drop(reopened);
    support::checkpoint_accepted_state(&files);

    let capture = support::format2::raw_catalog_capture(files.main())
        .expect("accept the valid persisted freelist after constrained update");
    assert!(
        !capture.freelist.is_empty(),
        "the constrained update must leave a persisted freelist for this regression"
    );
    assert_persisted_constraint_graph(&files, ROWS);
}

fn create_scale_constraint_table(connection: &mut Connection) {
    support::execute_sql(
        connection,
        "CREATE TABLE T (
            ID INT64 CONSTRAINT PK_T PRIMARY KEY,
            UQ_A INT64,
            UQ_B INT64,
            G INT64,
            PAYLOAD TEXT CONSTRAINT NN_T NOT NULL,
            SCORE INT64,
            CONSTRAINT UQ_T UNIQUE (UQ_A, UQ_B),
            CONSTRAINT CK_T CHECK (SCORE BETWEEN 0 AND 100)
        )",
    );
    support::execute_sql(connection, "CREATE INDEX IX_T_G_UQ_B ON T (G, UQ_B)");
}

fn insert_constraint_rows(connection: &mut Connection, rows: i64) {
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5, :6)")
        .expect("prepare constrained scale insert");
    for batch_start in (0..rows).step_by(BATCH_ROWS as usize) {
        support::execute_sql(connection, "BEGIN");
        let batch_end = (batch_start + BATCH_ROWS).min(rows);
        for id in batch_start..batch_end {
            let values = insert_values(id);
            assert_eq!(
                connection
                    .execute(&insert, Parameters::positional(&values))
                    .expect("insert constrained scale row")
                    .rows_affected(),
                1,
                "insert ID {id}"
            );
        }
        support::execute_sql(connection, "COMMIT");
    }
}

fn update_constraint_rows(connection: &mut Connection, rows: i64) {
    // UPDATE/DELETE discovery deliberately remains a ROWID TableScan in
    // Phase 4C2-B. A single set-based update drives every indexed column
    // through that real path once without smuggling an IndexScan scale claim
    // into this DDL/DML correctness gate.
    support::execute_sql(connection, "BEGIN");
    assert_eq!(
        support::execute_sql(
            connection,
            &format!(
                "UPDATE T
                 SET ID = ID + {rows},
                     UQ_A = UQ_A + {rows},
                     UQ_B = UQ_B + {rows},
                     G = G + 1000,
                     SCORE = 100 - SCORE"
            ),
        )
        .rows_affected(),
        rows as u64,
        "update every primary, composite-UNIQUE, and explicit-index column"
    );
    support::execute_sql(connection, "COMMIT");
}

fn delete_constraint_rows(connection: &mut Connection, rows: i64) {
    support::execute_sql(connection, "BEGIN");
    assert_eq!(
        support::execute_sql(
            connection,
            &format!("DELETE FROM T WHERE ID >= {}", rows + rows / 2),
        )
        .rows_affected(),
        (rows / 2) as u64,
        "delete half the scale table to force table and index contraction"
    );
    support::execute_sql(connection, "COMMIT");
}

fn assert_constraint_rows_and_indexes(connection: &mut Connection, rows: i64) {
    let retained_rows = rows / 2;
    assert_eq!(count_rows(connection), retained_rows);
    assert_eq!(count_null_unique_keys(connection), retained_rows / 10);
    assert_eq!(
        support::collect(
            connection,
            "SELECT ID, UQ_A, UQ_B, G, PAYLOAD, SCORE
             FROM T
             WHERE ID IN (100000, 100001, 149999)
             ORDER BY ID",
        ),
        vec![
            vec![
                Value::Int64(100_000),
                Value::Null,
                Value::Int64(100_000),
                Value::Int64(1_000),
                Value::Text("payload-0".into()),
                Value::Int64(100),
            ],
            vec![
                Value::Int64(100_001),
                Value::Int64(100_001),
                Value::Int64(100_001),
                Value::Int64(1_001),
                Value::Text("payload-1".into()),
                Value::Int64(99),
            ],
            vec![
                Value::Int64(149_999),
                Value::Int64(149_999),
                Value::Int64(100_004),
                Value::Int64(1_004),
                Value::Text("payload-49999".into()),
                Value::Int64(96),
            ],
        ]
    );

    support::execute_sql(connection, "BEGIN");
    let rollback = connection
        .prepare("INSERT INTO T VALUES (200000, 200000, 200000, 2000, 'rolled-back', 100)")
        .expect("prepare rollback row");
    assert_eq!(
        connection
            .execute(&rollback, Parameters::none())
            .expect("insert rollback row")
            .rows_affected(),
        1
    );
    support::execute_sql(connection, "ROLLBACK");
    assert_eq!(
        support::collect(connection, "SELECT ID FROM T WHERE ID = 200000"),
        Vec::<Vec<Value>>::new(),
        "rollback must restore the constrained table and every index"
    );
}

fn assert_duplicate_null_and_check_boundaries(connection: &mut Connection, rows: i64) {
    support::execute_sql(connection, "BEGIN");
    let null_unique = connection
        .prepare("INSERT INTO T VALUES (200001, NULL, 100000, 2001, 'null-is-distinct', 0)")
        .expect("prepare composite-null unique boundary");
    assert_eq!(
        connection
            .execute(&null_unique, Parameters::none())
            .expect("NULL component must not collide in composite UNIQUE")
            .rows_affected(),
        1
    );
    support::execute_sql(connection, "ROLLBACK");

    assert_named_constraint(
        connection,
        "INSERT INTO T VALUES (200002, 100001, 100001, 2002, 'duplicate-unique', 0)",
        "UQ_T",
    );
    assert_named_constraint(
        connection,
        "INSERT INTO T VALUES (200003, 200003, 200003, 2003, NULL, 0)",
        "NN_T",
    );
    assert_named_constraint(
        connection,
        "INSERT INTO T VALUES (200004, 200004, 200004, 2004, 'bad-check', 101)",
        "CK_T",
    );
    assert_eq!(count_rows(connection), rows / 2);
}

fn assert_reopened_system_indexes_reject_duplicates(connection: &mut Connection) {
    assert_named_constraint(
        connection,
        "INSERT INTO T VALUES (100000, 300000, 300000, 3000, 'duplicate-primary', 0)",
        "PK_T",
    );
    assert_named_constraint(
        connection,
        "INSERT INTO T VALUES (200005, 100001, 100001, 2005, 'reopened-unique', 0)",
        "UQ_T",
    );
}

fn assert_persisted_constraint_graph(files: &support::TestFiles, rows: i64) {
    let capture = support::format2::raw_catalog_capture(files.main())
        .expect("capture checkpointed constrained scale catalog");
    let table = capture.tables.get("T").expect("capture scale table");
    let column_ids = table
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column.id))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        table
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["ID", "UQ_A", "UQ_B", "G", "PAYLOAD", "SCORE"]
    );
    assert_eq!(table.next_rowid, (rows + 1) as u64);
    assert!(matches!(table.root_page_type, 2 | 3));

    let primary = capture.constraints.get("PK_T").expect("capture PK_T");
    let unique = capture.constraints.get("UQ_T").expect("capture UQ_T");
    let not_null = capture.constraints.get("NN_T").expect("capture NN_T");
    let check = capture.constraints.get("CK_T").expect("capture CK_T");
    assert_eq!(capture.constraints.len(), 4);
    assert_eq!(
        (primary.kind, unique.kind, not_null.kind, check.kind),
        (1, 2, 3, 4)
    );
    assert_eq!(primary.columns, vec![column_ids["ID"]]);
    assert_eq!(unique.columns, vec![column_ids["UQ_A"], column_ids["UQ_B"]]);
    assert_eq!(not_null.columns, vec![column_ids["PAYLOAD"]]);
    assert!(check.columns.is_empty());
    assert!(check.check_program_len > 0);
    assert_eq!(check.check_program_version, Some(1));
    assert!(not_null.index_id.is_none());
    assert!(check.index_id.is_none());

    let explicit = capture
        .indexes
        .get("IX_T_G_UQ_B")
        .expect("capture explicit scale index");
    assert_eq!(capture.indexes.len(), 3);
    assert_eq!(explicit.table_id, table.id);
    assert!(!explicit.unique);
    assert_eq!(explicit.owner_constraint_id, None);
    assert_eq!(explicit.columns, vec![column_ids["G"], column_ids["UQ_B"]]);

    let system_indexes = capture
        .indexes
        .values()
        .filter(|index| index.owner_constraint_id.is_some())
        .collect::<Vec<_>>();
    assert_eq!(system_indexes.len(), 2);
    assert!(system_indexes.iter().all(|index| index.unique));
    assert!(
        system_indexes
            .iter()
            .all(|index| index.table_id == table.id)
    );
    assert!(
        system_indexes
            .iter()
            .all(|index| matches!(index.root_page_type, 4 | 5))
    );
    assert_eq!(
        system_indexes
            .iter()
            .map(|index| index.owner_constraint_id.expect("system index owner"))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([primary.id, unique.id])
    );
    let roots = capture
        .indexes
        .values()
        .map(|index| index.root.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(roots.len(), 3);
    assert!(!roots.contains(&table.root.id));
    let table_constraints = &capture.table_constraints[&table.id];
    assert_eq!(table_constraints.len(), 4);
    assert_eq!(
        table_constraints.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([primary.id, unique.id, not_null.id, check.id])
    );
}

fn count_rows(connection: &mut Connection) -> i64 {
    match support::collect(connection, "SELECT COUNT(*) FROM T").as_slice() {
        [row] if matches!(row.as_slice(), [Value::Int64(_)]) => match row.as_slice() {
            [Value::Int64(count)] => *count,
            _ => unreachable!("guard proves the expected count shape"),
        },
        values => panic!("unexpected COUNT(*) result: {values:?}"),
    }
}

fn count_null_unique_keys(connection: &mut Connection) -> i64 {
    match support::collect(connection, "SELECT COUNT(*) FROM T WHERE UQ_A IS NULL").as_slice() {
        [row] if matches!(row.as_slice(), [Value::Int64(_)]) => match row.as_slice() {
            [Value::Int64(count)] => *count,
            _ => unreachable!("guard proves the expected count shape"),
        },
        values => panic!("unexpected NULL unique-key count: {values:?}"),
    }
}

fn assert_named_constraint(connection: &mut Connection, sql: &str, name: &str) {
    let statement = connection.prepare(sql).expect("prepare expected violation");
    let error = connection
        .execute(&statement, Parameters::none())
        .expect_err("statement must violate its named local constraint");
    assert_eq!(error.code(), ErrorCode::Constraint, "{sql}: {error}");
    assert!(
        error.to_string().contains(name),
        "{sql}: expected named constraint {name:?} in {error}"
    );
}

fn insert_values(id: i64) -> [ParameterValue; 6] {
    [
        Value::Int64(id).into(),
        unique_a(id).into(),
        Value::Int64(id % 101).into(),
        Value::Int64(id % 101).into(),
        Value::Text(format!("payload-{id}")).into(),
        Value::Int64(id % 101).into(),
    ]
}

fn unique_a(id: i64) -> Value {
    if id % 10 == 0 {
        Value::Null
    } else {
        Value::Int64(id)
    }
}
