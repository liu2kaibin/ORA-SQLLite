#![allow(dead_code)]

mod support;

use ora_sqllite::{ErrorCode, Parameters, Value};

#[test]
#[ignore = "run by the dedicated Phase 4C2-A CI job"]
fn phase_4c2a_hundred_thousand_row_index_smoke() {
    // Breaks caught: losing an indexed row during sustained DML, accepting a
    // duplicate unique key, publishing rolled-back index state, or reopening a
    // catalog/index image whose physical entries no longer name the expected
    // logical keys and ROWIDs.
    let (files, database) = support::new_database("phase4c2-index-scale");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID INT64, G INT64, PAYLOAD TEXT)",
    );
    support::execute_sql(&mut connection, "BEGIN");
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3)")
        .unwrap();
    for id in 0..100_000_i64 {
        support::insert_scale_row(&mut connection, &insert, id, id % 101);
    }
    support::execute_sql(&mut connection, "COMMIT");
    support::execute_sql(&mut connection, "CREATE UNIQUE INDEX UX_T_ID ON T (ID)");
    support::execute_sql(&mut connection, "CREATE INDEX IX_T_G_ID ON T (G, ID)");
    assert_eq!(
        support::execute_sql(&mut connection, "UPDATE T SET G = G + 1 WHERE ID >= 99000")
            .rows_affected(),
        1_000
    );
    assert_eq!(
        support::execute_sql(&mut connection, "DELETE FROM T WHERE ID < 1000").rows_affected(),
        1_000
    );

    let duplicate = connection
        .prepare("INSERT INTO T VALUES (50000, 0, 'duplicate')")
        .unwrap();
    assert_eq!(
        connection
            .execute(&duplicate, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Constraint
    );
    let mut transaction = connection.begin_write().unwrap();
    let rolled_back = transaction
        .prepare("INSERT INTO T VALUES (100000, 0, 'rolled-back')")
        .unwrap();
    transaction
        .execute(&rolled_back, Parameters::none())
        .unwrap();
    transaction.rollback().unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT ID FROM T WHERE ID = 100000"),
        Vec::<Vec<Value>>::new()
    );

    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    support::assert_index_model(&files, 99_000, &["UX_T_ID", "IX_T_G_ID"]);

    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    let duplicate_after_reopen = connection
        .prepare("INSERT INTO T VALUES (50000, 0, 'duplicate-after-reopen')")
        .unwrap();
    assert_eq!(
        connection
            .execute(&duplicate_after_reopen, Parameters::none())
            .unwrap_err()
            .code(),
        ErrorCode::Constraint
    );
    assert_eq!(
        support::collect(&mut connection, "SELECT COUNT(*) FROM T"),
        vec![vec![Value::Int64(99_000)]]
    );
    assert_eq!(
        support::collect(
            &mut connection,
            "SELECT ID FROM T WHERE ID IN (1000, 99000, 99999) ORDER BY ID"
        ),
        vec![
            vec![Value::Int64(1000)],
            vec![Value::Int64(99000)],
            vec![Value::Int64(99999)],
        ]
    );
    drop(connection);
    drop(reopened);
    support::checkpoint_accepted_state(&files);
    support::assert_index_model(&files, 99_000, &["UX_T_ID", "IX_T_G_ID"]);
}

#[test]
fn format2_verifier_rejects_a_real_index_page_checksum_corruption() {
    // Breaks caught: a verifier that merely counts SQL rows, or trusts a page
    // with a damaged checksum, would accept this physically changed index page.
    let (files, database) = support::new_database("phase4c2-verifier-corruption");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID INT64, G INT64, PAYLOAD TEXT)",
    );
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3)")
        .unwrap();
    support::insert_scale_row(&mut connection, &insert, 7, 3);
    support::execute_sql(&mut connection, "CREATE UNIQUE INDEX UX_T_ID ON T (ID)");
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);

    let report = support::verify_index_model(&files, 1, &["UX_T_ID"]).unwrap();
    support::flip_physical_page_byte(&files, report.index_page_ids[0], 80);
    assert!(support::verify_index_model(&files, 1, &["UX_T_ID"]).is_err());
}

#[test]
fn format2_verifier_rejects_a_checksum_valid_wrong_parent_separator() {
    // Breaks caught: local page ordering and checksums alone are insufficient
    // when an internal separator no longer equals its right child's minimum.
    let (files, database) = support::new_database("phase4c2-verifier-separator");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID INT64, G INT64, PAYLOAD TEXT)",
    );
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3)")
        .unwrap();
    for id in 0..1_000_i64 {
        support::insert_scale_row(&mut connection, &insert, id, id % 101);
    }
    support::execute_sql(&mut connection, "CREATE UNIQUE INDEX UX_T_ID ON T (ID)");
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);

    support::verify_index_model(&files, 1_000, &["UX_T_ID"]).unwrap();
    support::corrupt_first_index_separator_with_valid_checksum(&files, "UX_T_ID");
    assert!(support::verify_index_model(&files, 1_000, &["UX_T_ID"]).is_err());
}
