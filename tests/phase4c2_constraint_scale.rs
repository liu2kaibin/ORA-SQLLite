#![allow(dead_code)]

mod support;

use std::{collections::BTreeSet, time::Instant};

use ora_sqllite::{Connection, ErrorCode, ParameterValue, Parameters, Value};

const SCALE_ROWS: i64 = 100_000;
const BATCH_ROWS: i64 = 10_000;
const FAST_COLLAPSE_ROWS: i64 = 300;
const FINAL_SURVIVOR_ROWS: i64 = 3;

#[test]
#[ignore = "run by the dedicated Phase 4C2-B CI job"]
fn phase_4c2b_hundred_thousand_row_constraint_smoke() {
    // Breaks caught: losing rows or any table/system/explicit-index state
    // during sustained constrained DML; failing to restore an explicit
    // rollback; or reopening a checkpointed graph with missing constraint
    // ownership, keys, or roots.
    let started = Instant::now();
    let (files, database) = support::new_database("phase4c2-constraint-scale");
    let mut connection = database.connect().expect("connect scale database");

    create_scale_constraint_table(&mut connection);
    insert_constraint_rows(&mut connection, SCALE_ROWS);
    assert_eq!(count_rows(&mut connection), SCALE_ROWS);
    assert_eq!(count_null_unique_keys(&mut connection), SCALE_ROWS / 10);

    update_constraint_rows(&mut connection, SCALE_ROWS);
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    let before_delete = assert_persisted_constraint_graph(&files, SCALE_ROWS, SCALE_ROWS);
    assert_constraint_trees_are_internal(&before_delete, "before deterministic delete");

    let reopened = support::reopen(&files);
    let mut connection = reopened
        .connect()
        .expect("connect before deterministic delete");
    delete_constraint_rows(&mut connection, SCALE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_constraint_rows_and_indexes(&mut connection, SCALE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_duplicate_null_and_check_boundaries(&mut connection, SCALE_ROWS, FINAL_SURVIVOR_ROWS);
    drop(connection);
    drop(reopened);
    support::checkpoint_accepted_state(&files);
    let after_delete = assert_persisted_constraint_graph(&files, SCALE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_constraint_trees_collapsed_to_leaves(&before_delete, &after_delete);

    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().expect("connect reopened scale database");
    assert_constraint_rows_and_indexes(&mut connection, SCALE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_reopened_system_indexes_reject_duplicates(&mut connection, SCALE_ROWS);
    drop(connection);
    drop(reopened);

    support::checkpoint_accepted_state(&files);
    let after_reopen = assert_persisted_constraint_graph(&files, SCALE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_constraint_trees_collapsed_to_leaves(&before_delete, &after_reopen);
    report_scale_gate(
        &before_delete,
        &after_reopen,
        FINAL_SURVIVOR_ROWS,
        started.elapsed(),
    );
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
    assert_persisted_constraint_graph(&files, ROWS, ROWS);
}

#[test]
fn phase_4c2b_fast_constrained_raw_indexes_survive_reopen_and_collapse_to_leaves() {
    // Break caught: stale or missing leaf entries in either constraint-owned
    // system index pass a logical TableScan-only acceptance. A mere root
    // replacement after DELETE also does not prove all three tree families
    // contracted to leaf roots.
    let (files, database) = support::new_database("phase4c2-constraint-raw-witness");
    let mut connection = database.connect().expect("connect raw witness database");
    create_scale_constraint_table(&mut connection);
    insert_constraint_rows(&mut connection, FAST_COLLAPSE_ROWS);
    update_constraint_rows(&mut connection, FAST_COLLAPSE_ROWS);
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    let before_delete =
        assert_persisted_constraint_graph(&files, FAST_COLLAPSE_ROWS, FAST_COLLAPSE_ROWS);
    assert_constraint_trees_are_internal(&before_delete, "before fast deterministic delete");

    let reopened = support::reopen(&files);
    let mut connection = reopened
        .connect()
        .expect("connect before fast deterministic delete");
    delete_constraint_rows(&mut connection, FAST_COLLAPSE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_constraint_rows_and_indexes(&mut connection, FAST_COLLAPSE_ROWS, FINAL_SURVIVOR_ROWS);
    drop(connection);
    drop(reopened);
    support::checkpoint_accepted_state(&files);
    let after_delete =
        assert_persisted_constraint_graph(&files, FAST_COLLAPSE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_constraint_trees_collapsed_to_leaves(&before_delete, &after_delete);

    let reopened = support::reopen(&files);
    let mut connection = reopened
        .connect()
        .expect("reopen fast raw witness database");
    assert_constraint_rows_and_indexes(&mut connection, FAST_COLLAPSE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_reopened_system_indexes_reject_duplicates(&mut connection, FAST_COLLAPSE_ROWS);
    drop(connection);
    drop(reopened);
    support::checkpoint_accepted_state(&files);
    let after_reopen =
        assert_persisted_constraint_graph(&files, FAST_COLLAPSE_ROWS, FINAL_SURVIVOR_ROWS);
    assert_constraint_trees_collapsed_to_leaves(&before_delete, &after_reopen);
}

#[test]
fn phase_4c2b_raw_table_shape_validates_overflow_values_without_relaxing_index_leaves() {
    // Break caught: a byte-level table-shape witness rejects a valid table row
    // merely because its value lives in a checksum-protected overflow chain.
    const OVERFLOW_PAYLOAD_LEN: usize = 9_000;

    let (files, database) = support::new_database("phase4c2-constraint-overflow-witness");
    let mut connection = database
        .connect()
        .expect("connect overflow witness database");
    create_scale_constraint_table(&mut connection);
    let payload = "x".repeat(OVERFLOW_PAYLOAD_LEN);
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5, :6)")
        .expect("prepare overflow constrained row");
    let values = [
        Value::Int64(1).into(),
        Value::Int64(1).into(),
        Value::Int64(1).into(),
        Value::Int64(1).into(),
        Value::Text(payload.clone()).into(),
        Value::Int64(1).into(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&values))
            .expect("insert overflow constrained row")
            .rows_affected(),
        1
    );
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);

    let capture = support::format2::raw_catalog_capture(files.main())
        .expect("capture overflow constrained table");
    let table = capture
        .tables
        .get("T")
        .expect("capture overflow table root");
    let tree = support::format2::raw_table_tree_capture(files.main(), table.root)
        .expect("validate table shape with overflow value chain");
    assert_eq!(tree.root_page_type, 3);
    assert_eq!(tree.height, 0);
    let [overflow] = tree.overflow_values.as_slice() else {
        panic!("the deterministic payload must use exactly one overflow value");
    };
    assert_eq!(overflow.storage_code, 1);
    assert_eq!(overflow.inline_len, 0);
    assert!(overflow.total_value_len > OVERFLOW_PAYLOAD_LEN as u64);
    assert_eq!(overflow.chain.len(), 3);
    eprintln!(
        "overflow witness: storage_code={} inline_len={} total_value_len={} chain={:?}",
        overflow.storage_code, overflow.inline_len, overflow.total_value_len, overflow.chain
    );

    let reopened = support::reopen(&files);
    let mut connection = reopened
        .connect()
        .expect("reopen overflow witness database");
    assert_eq!(
        support::collect(&mut connection, "SELECT PAYLOAD FROM T"),
        vec![vec![Value::Text(payload)]]
    );
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

fn delete_constraint_rows(connection: &mut Connection, rows: i64, survivors: i64) {
    support::execute_sql(connection, "BEGIN");
    assert_eq!(
        support::execute_sql(
            connection,
            &format!("DELETE FROM T WHERE ID >= {}", rows + survivors),
        )
        .rows_affected(),
        (rows - survivors) as u64,
        "delete every but the deterministic survivor set to force tree collapse"
    );
    support::execute_sql(connection, "COMMIT");
}

fn assert_constraint_rows_and_indexes(
    connection: &mut Connection,
    allocated_rows: i64,
    survivors: i64,
) {
    assert_eq!(count_rows(connection), survivors);
    assert_eq!(count_null_unique_keys(connection), 1);
    assert_eq!(
        support::collect(
            connection,
            &format!(
                "SELECT ID, UQ_A, UQ_B, G, PAYLOAD, SCORE
                 FROM T
                 WHERE ID IN ({allocated_rows}, {}, {})
                 ORDER BY ID",
                allocated_rows + 1,
                allocated_rows + 2,
            ),
        ),
        vec![
            vec![
                Value::Int64(allocated_rows),
                Value::Null,
                Value::Int64(allocated_rows),
                Value::Int64(1_000),
                Value::Text("payload-0".into()),
                Value::Int64(100),
            ],
            vec![
                Value::Int64(allocated_rows + 1),
                Value::Int64(allocated_rows + 1),
                Value::Int64(allocated_rows + 1),
                Value::Int64(1_001),
                Value::Text("payload-1".into()),
                Value::Int64(99),
            ],
            vec![
                Value::Int64(allocated_rows + 2),
                Value::Int64(allocated_rows + 2),
                Value::Int64(allocated_rows + 2),
                Value::Int64(1_002),
                Value::Text("payload-2".into()),
                Value::Int64(98),
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

fn assert_duplicate_null_and_check_boundaries(
    connection: &mut Connection,
    allocated_rows: i64,
    survivors: i64,
) {
    support::execute_sql(connection, "BEGIN");
    let null_unique = connection
        .prepare(&format!(
            "INSERT INTO T VALUES (200001, NULL, {allocated_rows}, 2001, 'null-is-distinct', 0)"
        ))
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
        &format!(
            "INSERT INTO T VALUES (200002, {}, {}, 2002, 'duplicate-unique', 0)",
            allocated_rows + 1,
            allocated_rows + 1,
        ),
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
    assert_eq!(count_rows(connection), survivors);
}

fn assert_reopened_system_indexes_reject_duplicates(
    connection: &mut Connection,
    allocated_rows: i64,
) {
    assert_named_constraint(
        connection,
        &format!(
            "INSERT INTO T VALUES ({allocated_rows}, 300000, 300000, 3000, 'duplicate-primary', 0)"
        ),
        "PK_T",
    );
    assert_named_constraint(
        connection,
        &format!(
            "INSERT INTO T VALUES (200005, {}, {}, 2005, 'reopened-unique', 0)",
            allocated_rows + 1,
            allocated_rows + 1,
        ),
        "UQ_T",
    );
}

#[derive(Debug)]
struct ConstraintTreeState {
    table: support::format2::RawTreeCapture,
    primary: support::format2::RawTreeCapture,
    unique: support::format2::RawTreeCapture,
    explicit: support::format2::RawTreeCapture,
}

fn assert_persisted_constraint_graph(
    files: &support::TestFiles,
    allocated_rows: i64,
    survivors: i64,
) -> ConstraintTreeState {
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
    assert_eq!(table.next_rowid, (allocated_rows + 1) as u64);
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

    let primary_index_id = primary.index_id.expect("PK_T owns a system index");
    let unique_index_id = unique.index_id.expect("UQ_T owns a system index");
    let primary_index = capture
        .indexes
        .values()
        .find(|index| index.id == primary_index_id)
        .expect("raw catalog has the PK_T system index");
    let unique_index = capture
        .indexes
        .values()
        .find(|index| index.id == unique_index_id)
        .expect("raw catalog has the UQ_T system index");
    assert_constraint_index_mapping(primary, primary_index, "PK_T");
    assert_constraint_index_mapping(unique, unique_index, "UQ_T");

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

    let table_tree = support::format2::raw_table_tree_capture(files.main(), table.root)
        .expect("walk table tree from raw catalog root");
    let primary_tree = support::format2::raw_index_tree_capture(files.main(), primary_index.root)
        .expect("walk PK_T system index from raw catalog root");
    let unique_tree = support::format2::raw_index_tree_capture(files.main(), unique_index.root)
        .expect("walk UQ_T system index from raw catalog root");
    let explicit_tree = support::format2::raw_index_tree_capture(files.main(), explicit.root)
        .expect("walk IX_T_G_UQ_B explicit index from raw catalog root");
    let expected = expected_post_update_index_keys(allocated_rows, survivors);
    assert_exact_physical_entries("PK_T system index", &primary_tree, &expected.primary);
    assert_exact_physical_entries("UQ_T system index", &unique_tree, &expected.unique);
    assert_exact_physical_entries(
        "IX_T_G_UQ_B explicit index",
        &explicit_tree,
        &expected.explicit,
    );

    ConstraintTreeState {
        table: table_tree,
        primary: primary_tree,
        unique: unique_tree,
        explicit: explicit_tree,
    }
}

fn assert_constraint_index_mapping(
    constraint: &support::format2::RawConstraint,
    index: &support::format2::RawIndex,
    name: &str,
) {
    assert_eq!(
        constraint.index_id,
        Some(index.id),
        "{name} must name its raw system-index definition"
    );
    assert_eq!(
        index.owner_constraint_id,
        Some(constraint.id),
        "{name} system index must point back to its owning constraint"
    );
}

struct ExpectedIndexKeys {
    primary: Vec<Vec<u8>>,
    unique: Vec<Vec<u8>>,
    explicit: Vec<Vec<u8>>,
}

fn expected_post_update_index_keys(allocated_rows: i64, survivors: i64) -> ExpectedIndexKeys {
    let mut primary = Vec::with_capacity(survivors as usize);
    let mut unique = Vec::with_capacity(survivors as usize);
    let mut explicit = Vec::with_capacity(survivors as usize);
    for original_id in 0..survivors {
        let rowid = u64::try_from(original_id + 1).expect("deterministic ROWID fits");
        let residue = original_id % 101;
        primary.push(hand_derived_int_index_key(
            &[Some(allocated_rows + original_id)],
            rowid,
        ));
        unique.push(hand_derived_int_index_key(
            &[
                (original_id % 10 != 0).then_some(allocated_rows + original_id),
                Some(allocated_rows + residue),
            ],
            rowid,
        ));
        explicit.push(hand_derived_int_index_key(
            &[Some(1_000 + residue), Some(allocated_rows + residue)],
            rowid,
        ));
    }
    primary.sort_unstable();
    unique.sort_unstable();
    explicit.sort_unstable();
    ExpectedIndexKeys {
        primary,
        unique,
        explicit,
    }
}

fn hand_derived_int_index_key(values: &[Option<i64>], rowid: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(values.len() * 9 + 8);
    for value in values {
        match value {
            // Index-key V1 encodes NULL as its own one-byte, sort-last component.
            None => key.push(1),
            Some(value) => {
                key.push(0);
                key.extend_from_slice(&((*value as u64 ^ (1_u64 << 63)).to_be_bytes()));
            }
        }
    }
    key.extend_from_slice(&rowid.to_be_bytes());
    key
}

fn assert_exact_physical_entries(
    name: &str,
    tree: &support::format2::RawTreeCapture,
    expected: &[Vec<u8>],
) {
    assert_eq!(
        tree.entry_keys.len(),
        expected.len(),
        "{name} physical entry count"
    );
    let first_difference = tree
        .entry_keys
        .iter()
        .zip(expected)
        .position(|(actual, expected)| actual != expected);
    if let Some(position) = first_difference {
        assert_eq!(
            tree.entry_keys[position], expected[position],
            "{name} physical keys differ at leaf entry {position}"
        );
    }
}

fn assert_constraint_trees_are_internal(state: &ConstraintTreeState, phase: &str) {
    assert_eq!(
        state.table.root_page_type, 2,
        "{phase}: table root page type"
    );
    assert!(
        state.table.height > 0,
        "{phase}: table root must have children"
    );
    for (name, tree) in [("PK_T", &state.primary), ("UQ_T", &state.unique)] {
        assert_eq!(tree.root_page_type, 4, "{phase}: {name} root page type");
        assert!(tree.height > 0, "{phase}: {name} root must have children");
    }
}

fn assert_constraint_trees_collapsed_to_leaves(
    before: &ConstraintTreeState,
    after: &ConstraintTreeState,
) {
    assert_eq!(
        after.table.root_page_type, 3,
        "post-delete table root page type"
    );
    assert_eq!(after.table.height, 0, "post-delete table tree height");
    assert_ne!(
        before.table.root, after.table.root,
        "table root must transition"
    );
    for (name, before, after) in [
        ("PK_T", &before.primary, &after.primary),
        ("UQ_T", &before.unique, &after.unique),
    ] {
        assert_eq!(after.root_page_type, 5, "post-delete {name} root page type");
        assert_eq!(after.height, 0, "post-delete {name} tree height");
        assert_ne!(before.root, after.root, "{name} root must transition");
    }
}

fn report_scale_gate(
    before: &ConstraintTreeState,
    after: &ConstraintTreeState,
    final_rows: i64,
    elapsed: std::time::Duration,
) {
    eprintln!(
        "phase 4C2-B 100k: final_rows={final_rows}; table pre=type{}/height{} post=type{}/height{}; PK pre=type{}/height{} post=type{}/height{}; UQ pre=type{}/height{} post=type{}/height{}; physical_entries explicit={} pk={} uq={}; elapsed={:.2}s",
        before.table.root_page_type,
        before.table.height,
        after.table.root_page_type,
        after.table.height,
        before.primary.root_page_type,
        before.primary.height,
        after.primary.root_page_type,
        after.primary.height,
        before.unique.root_page_type,
        before.unique.height,
        after.unique.root_page_type,
        after.unique.height,
        after.explicit.entry_keys.len(),
        after.primary.entry_keys.len(),
        after.unique.entry_keys.len(),
        elapsed.as_secs_f64(),
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
