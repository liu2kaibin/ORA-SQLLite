mod support;

use ora_sqllite::Value;

#[test]
fn large_scale_gate_requires_explicit_opt_in_and_eighty_gb() {
    // Break caught: an opt-in typo or insufficient disk must not grant a
    // destructive-to-capacity multi-gigabyte import.
    assert!(support::validate_large_scale_gate(None, u64::MAX).is_err());
    assert!(support::validate_large_scale_gate(Some("true"), u64::MAX).is_err());
    assert!(support::validate_large_scale_gate(Some("1"), 30_000_000_000).is_err());
    assert!(support::validate_large_scale_gate(Some("1"), 79_999_999_999).is_err());
    assert!(support::validate_large_scale_gate(Some("1"), 80_000_000_000).is_ok());
}

#[test]
fn large_scale_budget_contains_the_fixture_and_reserves_wal_and_working_space() {
    // Break caught: a gate that admits a fixture which the configured database
    // limit cannot store, or whose database + WAL can exhaust the admitted disk.
    // Independent page-count finding, not derived from these helper constants.
    const TABLE_LEAF_AND_OVERFLOW_MIN_BYTES: u64 = 40_959_991_808;
    let options = support::scale_options();
    assert!(
        options.max_database_bytes()
            >= TABLE_LEAF_AND_OVERFLOW_MIN_BYTES
                .checked_add(20_000_000_000)
                .unwrap()
    );
    let peak_with_reserve = options
        .max_database_bytes()
        .checked_add(options.max_wal_bytes())
        .unwrap()
        .checked_add(10_000_000_000)
        .unwrap();
    assert!(peak_with_reserve <= 80_000_000_000);
    assert!(support::validate_large_scale_gate(Some("1"), 80_000_000_000).is_ok());
}

#[test]
fn scale_loader_populates_the_public_indexed_table() {
    // Break caught: a scale harness that omits the real import can otherwise
    // report timings and file sizes without exercising any populated index.
    let mut db = support::scale_database("phase4c2d-loader-contract");
    support::load_rows_and_indexes(&mut db, 100, 64);
    let mut connection = db.connect();
    assert_eq!(
        support::collect(&mut connection, "SELECT COUNT(*) FROM SCALE_T"),
        vec![vec![Value::Int64(100)]]
    );
    drop(connection);
    support::assert_point_queries(&mut db, [0, 1, 50, 100, 101]);
    assert!(db.main_file_bytes() > 0);
    assert!(db.wal_peak_bytes() > 0);
}

#[test]
#[ignore = "run by the manual Phase 4C2-D Deep CI job"]
fn phase_4c2d_hundred_thousand_row_index_scan_smoke() {
    // Breaks caught: incorrect point/range fences across split trees, stale
    // mutation keys, wrong affected counts, and loss/corruption after reopen.
    const ROWS: i64 = 100_000;
    let mut db = support::scale_database("phase4c2d-100k");
    support::load_rows_and_indexes(&mut db, ROWS, 64);
    support::assert_point_queries(&mut db, [1, 50_000, 100_000]);
    support::assert_composite_ranges(&mut db, ROWS);
    support::mutate_indexed_rows(&mut db, ROWS);
    support::checkpoint_reopen_and_repeat(&mut db);
    db.print_machine_readable_metrics();
}

#[test]
#[ignore = "requires explicit ORA_SQLLITE_LARGE_SCALE=1 and at least 80 GB free"]
fn phase_4c2d_ten_million_row_ten_gb_acceptance() {
    // This gate is checked before constructing/owning any temporary database.
    support::require_large_scale_gate();
    const ROWS: i64 = 10_000_000;
    const PAYLOAD_BYTES: usize = 1_024;
    let mut db = support::scale_database("phase4c2d-10m-10gb");
    support::load_rows_and_indexes(&mut db, ROWS, PAYLOAD_BYTES);
    assert!(
        db.main_file_bytes() >= 10_000_000_000,
        "large gate requires actual persisted main-file bytes"
    );
    support::run_large_acceptance_queries_and_mutations(&mut db, ROWS);
    support::checkpoint_reopen_and_repeat(&mut db);
    db.print_machine_readable_metrics();
}
