mod support;

use ora_sqllite::{
    Connection, Decimal, OracleDate, OracleTimestamp, ParameterValue, Parameters, Value,
};

#[allow(dead_code, reason = "shared by later Phase 5 Oracle type API tests")]
fn decimal_value(magnitude: u128, scale: i8) -> Value {
    Value::Decimal(Decimal::new(false, scale, magnitude).unwrap())
}

#[allow(dead_code, reason = "shared by later Phase 5 timestamp API tests")]
fn timestamp_value(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanos: u32,
    offset: Option<i16>,
) -> Value {
    let date = OracleDate::new(year, month, day, hour, minute, second).unwrap();
    Value::Timestamp(OracleTimestamp::from_nanoseconds(date, nanos, offset).unwrap())
}

#[allow(dead_code, reason = "shared by later Phase 5 Oracle type API tests")]
fn collect_with_params(
    connection: &mut Connection,
    sql: &str,
    values: &[ParameterValue],
) -> Vec<Vec<Value>> {
    let statement = connection.prepare(sql).unwrap();
    support::collect_statement(connection, &statement, Parameters::positional(values))
}

#[test]
fn creates_every_oracle_type_and_persists_its_arguments() {
    let (files, database) = support::new_database("phase5-types-ddl");
    let mut connection = database.connect().unwrap();
    let create = connection.prepare(
        "CREATE TABLE T (N NUMBER(12,2), V VARCHAR2(20), C CHAR(4), R RAW(16), D DATE, TS TIMESTAMP(9), TZ TIMESTAMP(3) WITH TIME ZONE)"
    ).unwrap();
    connection.execute(&create, Parameters::none()).unwrap();
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    let capture = support::format2::raw_catalog_capture(files.main()).unwrap();
    assert_eq!(capture.feature_bits & 4, 4);
    assert_eq!(capture.oracle_columns["T.N"].precision, 12);
    assert_eq!(capture.oracle_columns["T.N"].scale, 2);
    assert_eq!(capture.oracle_columns["T.V"].length, 20);
    assert_eq!(capture.oracle_columns["T.TS"].precision, 9);
}

#[test]
fn rejects_bare_number_and_local_timezone_before_catalog_mutation() {
    let (files, database) = support::new_database("phase5-types-reject");
    let mut connection = database.connect().unwrap();
    for sql in [
        "CREATE TABLE A (N NUMBER)",
        "CREATE TABLE B (N NUMBER(0,0))",
        "CREATE TABLE C (V VARCHAR2(0))",
        "CREATE TABLE D (T TIMESTAMP(6) WITH LOCAL TIME ZONE)",
    ] {
        let error = connection.prepare(sql).unwrap_err();
        assert_eq!(error.oracle_code(), Some(902));
        assert!(error.sql_location().is_some());
    }
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    assert_eq!(
        support::format2::raw_catalog_capture(files.main())
            .unwrap()
            .feature_bits
            & 4,
        0
    );
}

#[test]
fn project_native_declarations_remain_v1_and_plain_timestamp_defaults_to_six() {
    let (files, database) = support::new_database("phase5-types-v1");
    let mut connection = database.connect().unwrap();
    let core = connection
        .prepare("CREATE TABLE CORE_TYPES (I INT64, N DECIMAL, F FLOAT64, T TEXT, B BLOB, X BOOL)")
        .unwrap();
    connection.execute(&core, Parameters::none()).unwrap();
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    let capture = support::format2::raw_catalog_capture(files.main()).unwrap();
    assert_eq!(capture.feature_bits & 4, 0);
    assert!(capture.oracle_columns.is_empty());

    let database = support::reopen(&files);
    let mut connection = database.connect().unwrap();
    let timestamp = connection
        .prepare("CREATE TABLE ORACLE_TS (TS TIMESTAMP)")
        .unwrap();
    connection.execute(&timestamp, Parameters::none()).unwrap();
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    let capture = support::format2::raw_catalog_capture(files.main()).unwrap();
    assert_eq!(capture.oracle_columns["ORACLE_TS.TS"].kind, 6);
    assert_eq!(capture.oracle_columns["ORACLE_TS.TS"].precision, 6);
}

#[test]
fn foreign_keys_require_exact_oracle_descriptors_and_reject_v1_v2_mixes() {
    let (_files, database) = support::new_database("phase5-types-fk");
    let mut connection = database.connect().unwrap();
    let parent = connection
        .prepare("CREATE TABLE P (N NUMBER(12,2) PRIMARY KEY)")
        .unwrap();
    connection.execute(&parent, Parameters::none()).unwrap();

    let matching = connection
        .prepare("CREATE TABLE C_OK (N NUMBER(12,2) REFERENCES P (N))")
        .unwrap();
    connection.execute(&matching, Parameters::none()).unwrap();

    for sql in [
        "CREATE TABLE C_SCALE (N NUMBER(12,3) REFERENCES P (N))",
        "CREATE TABLE C_V1 (N DECIMAL REFERENCES P (N))",
    ] {
        assert_eq!(connection.prepare(sql).unwrap_err().code() as u32, 1200);
    }
}

#[test]
fn invalid_oracle_declaration_reports_exact_multiline_unicode_location() {
    let (_files, database) = support::new_database("phase5-types-location");
    let mut connection = database.connect().unwrap();
    let sql = "CREATE TABLE T (\n  名 NUMBER\n)";
    let error = connection.prepare(sql).unwrap_err();
    let location = error.sql_location().unwrap();
    assert_eq!(error.oracle_code(), Some(902));
    assert_eq!(location.byte_offset(), sql.find("NUMBER").unwrap());
    assert_eq!(location.line(), 2);
    assert_eq!(location.column(), 5);
}

#[test]
fn crud_uses_one_normalized_row_for_literals_and_parameters() {
    let (_files, database) = support::new_database("phase5-crud");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID NUMBER(4,0) PRIMARY KEY, N NUMBER(5,2), C CHAR(3), V VARCHAR2(3), R RAW(2))",
    );
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5)")
        .unwrap();
    connection
        .execute(
            &insert,
            Parameters::positional(&[
                Value::Text("1".into()).into(),
                Value::Text("12.345".into()).into(),
                Value::Text("A".into()).into(),
                Value::Text("表A".into()).into(),
                Value::Blob(vec![0, 255]).into(),
            ]),
        )
        .unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT N,C,V,R FROM T"),
        vec![vec![
            decimal_value(1235, 2),
            Value::Text("A  ".into()),
            Value::Text("表A".into()),
            Value::Blob(vec![0, 255]),
        ]]
    );
}

#[test]
fn primary_key_conflicts_keep_the_constraint_name_and_gain_ora_00001() {
    let (_files, database) = support::new_database("phase5-unique-ora");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID NUMBER(3,0) CONSTRAINT PK_T PRIMARY KEY)",
    );
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (1)");
    let duplicate = connection.prepare("INSERT INTO T VALUES (1)").unwrap();
    let error = connection
        .execute(&duplicate, Parameters::none())
        .unwrap_err();
    assert_eq!(
        (error.code(), error.oracle_code()),
        (ora_sqllite::ErrorCode::Constraint, Some(1))
    );
    assert!(error.to_string().contains("PK_T"));
}

#[test]
fn failed_normalization_leaves_rowid_and_indexes_unchanged() {
    let (files, database) = support::new_database("phase5-rollback");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID NUMBER(2,0) PRIMARY KEY, N NUMBER(2,1), V VARCHAR2(1) NOT NULL)",
    );
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    let before = support::format2::raw_catalog_capture(files.main()).unwrap();
    let database = support::reopen(&files);
    let mut connection = database.connect().unwrap();
    let insert = connection
        .prepare("INSERT INTO T VALUES (1, 9.99, '')")
        .unwrap();
    let error = connection.execute(&insert, Parameters::none()).unwrap_err();
    assert_eq!(
        (error.code(), error.oracle_code()),
        (ora_sqllite::ErrorCode::Type, Some(1438))
    );
    drop(connection);
    drop(database);
    support::checkpoint_accepted_state(&files);
    let after = support::format2::raw_catalog_capture(files.main()).unwrap();
    assert_eq!(after.tables["T"].next_rowid, before.tables["T"].next_rowid);
    assert_eq!(after.indexes, before.indexes);
}

#[test]
fn empty_text_hits_not_null_but_empty_blob_and_space_survive() {
    let (_files, database) = support::new_database("phase5-empty");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (V VARCHAR2(2) NOT NULL, R RAW(2))",
    );
    let insert = connection.prepare("INSERT INTO T VALUES (:1, :2)").unwrap();
    let empty = [
        Value::Text(String::new()).into(),
        Value::Blob(Vec::new()).into(),
    ];
    let error = connection
        .execute(&insert, Parameters::positional(&empty))
        .unwrap_err();
    assert_eq!(
        (error.code(), error.oracle_code()),
        (ora_sqllite::ErrorCode::Constraint, Some(1400))
    );
    let space = [
        Value::Text(" ".into()).into(),
        Value::Blob(Vec::new()).into(),
    ];
    connection
        .execute(&insert, Parameters::positional(&space))
        .unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT V,R FROM T"),
        vec![vec![Value::Text(" ".into()), Value::Blob(Vec::new())]]
    );
}

#[test]
fn oracle_text_parameter_permission_is_limited_to_a_direct_assignment() {
    let (_files, database) = support::new_database("phase5-direct-oracle-text");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (N NUMBER(5,2))");

    let direct = connection.prepare("INSERT INTO T VALUES (:1)").unwrap();
    connection
        .execute(
            &direct,
            Parameters::positional(&[Value::Text("12.345".into()).into()]),
        )
        .unwrap();

    let arithmetic = connection.prepare("INSERT INTO T VALUES (:1 + 1)").unwrap();
    let error = connection
        .execute(
            &arithmetic,
            Parameters::positional(&[Value::Text("12.345".into()).into()]),
        )
        .unwrap_err();
    assert_eq!(error.code(), ora_sqllite::ErrorCode::Type);
    assert!(
        error
            .to_string()
            .contains("runtime parameter requires a compatible concrete type"),
        "{error}"
    );
}

#[test]
fn char_check_normalizes_its_persisted_constant() {
    let (_files, database) = support::new_database("phase5-check-contract");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (C CHAR(3) CHECK (C = 'A'))",
    );
    support::execute_sql(&mut connection, "INSERT INTO T VALUES ('A')");
    assert_eq!(
        support::collect(&mut connection, "SELECT C FROM T"),
        vec![vec![Value::Text("A  ".into())]]
    );
}

#[test]
fn number_check_normalizes_its_persisted_constant() {
    let (_files, database) = support::new_database("phase5-number-check-contract");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (N NUMBER(4,2) CHECK (N = '12.345'))",
    );
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (12.345)");
    assert_eq!(
        support::collect(&mut connection, "SELECT N FROM T"),
        vec![vec![decimal_value(1235, 2)]]
    );
}

#[test]
fn timestamp_with_time_zone_check_uses_utc_contract() {
    let (_files, database) = support::new_database("phase5-tstz-check-contract");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (TZ TIMESTAMP(9) WITH TIME ZONE CHECK (TZ = '2026-01-01 00:00:00 +00:00'))",
    );
    let insert = connection.prepare("INSERT INTO T VALUES (:1)").unwrap();
    connection
        .execute(
            &insert,
            Parameters::positional(&[timestamp_value(2026, 1, 1, 8, 0, 0, 0, Some(480)).into()]),
        )
        .unwrap();
}

#[test]
fn oracle_comparisons_normalize_literals_by_column_contract() {
    let (_files, database) = support::new_database("phase5-comparison-contract");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (N NUMBER(5,2), D DATE, TS TIMESTAMP(9), TZ TIMESTAMP(9) WITH TIME ZONE)",
    );
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3, :4)")
        .unwrap();
    connection
        .execute(
            &insert,
            Parameters::positional(&[
                Value::Text("12.345".into()).into(),
                Value::Date(OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap()).into(),
                timestamp_value(2026, 1, 1, 8, 0, 0, 123_456_789, None).into(),
                timestamp_value(2026, 1, 1, 8, 0, 0, 0, Some(480)).into(),
            ]),
        )
        .unwrap();
    assert_eq!(
        support::collect(&mut connection, "SELECT N FROM T WHERE N = '12.345'"),
        vec![vec![decimal_value(1235, 2)]]
    );
    assert_eq!(
        collect_with_params(
            &mut connection,
            "SELECT N FROM T WHERE N = :1",
            &[Value::Text("12.345".into()).into()],
        ),
        vec![vec![decimal_value(1235, 2)]]
    );
    assert_eq!(
        support::collect(&mut connection, "SELECT D FROM T WHERE D = '2026-01-01'"),
        vec![vec![Value::Date(
            OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap()
        )]]
    );
    assert_eq!(
        support::collect(
            &mut connection,
            "SELECT TS FROM T WHERE TS = '2026-01-01 08:00:00.123456789'"
        ),
        vec![vec![timestamp_value(
            2026,
            1,
            1,
            8,
            0,
            0,
            123_456_789,
            None
        )]]
    );
    assert_eq!(
        support::collect(
            &mut connection,
            "SELECT TZ FROM T WHERE TZ = '2026-01-01 00:00:00 +00:00'"
        ),
        vec![vec![timestamp_value(2026, 1, 1, 8, 0, 0, 0, Some(480))]]
    );
}

#[test]
fn oracle_comparisons_reject_legacy_mixes_and_different_families() {
    let (_files, database) = support::new_database("phase5-comparison-reject");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (V INT64, N NUMBER(3,0), TS TIMESTAMP(9), TZ TIMESTAMP(9) WITH TIME ZONE)",
    );
    for sql in [
        "SELECT N FROM T WHERE N = V",
        "SELECT TS FROM T WHERE TS = TZ",
    ] {
        assert_eq!(
            connection.prepare(sql).unwrap_err().code(),
            ora_sqllite::ErrorCode::Type
        );
    }
}

#[test]
fn timezone_index_uses_utc_prefix_but_rows_keep_offsets() {
    let (_files, database) = support::new_database("phase5-tz-index");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (TS TIMESTAMP(9) WITH TIME ZONE UNIQUE)",
    );
    let insert = connection.prepare("INSERT INTO T VALUES (:1)").unwrap();
    connection
        .execute(
            &insert,
            Parameters::positional(&[
                timestamp_value(2026, 1, 1, 8, 0, 0, 123_456_789, Some(480)).into()
            ]),
        )
        .unwrap();
    let error = connection
        .execute(
            &insert,
            Parameters::positional(&[
                timestamp_value(2026, 1, 1, 0, 0, 0, 123_456_789, Some(0)).into()
            ]),
        )
        .unwrap_err();
    assert_eq!(
        (error.code(), error.oracle_code()),
        (ora_sqllite::ErrorCode::Constraint, Some(1))
    );
    assert_eq!(
        support::collect(&mut connection, "SELECT TS FROM T"),
        vec![vec![timestamp_value(
            2026,
            1,
            1,
            8,
            0,
            0,
            123_456_789,
            Some(480),
        )]]
    );
}

#[test]
fn scan_and_filter_share_number_char_and_timestamp_normalization() {
    let (_files, database) = support::new_database("phase5-index-parity");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (ID NUMBER(4,0), N NUMBER(5,2), C CHAR(3), TZ TIMESTAMP(9) WITH TIME ZONE)",
    );
    for sql in [
        "CREATE INDEX T_N ON T(N)",
        "CREATE INDEX T_C ON T(C)",
        "CREATE INDEX T_TZ ON T(TZ)",
    ] {
        support::execute_sql(&mut connection, sql);
    }
    let insert = connection
        .prepare("INSERT INTO T VALUES (1, 1.2, 'A', :1)")
        .unwrap();
    connection
        .execute(
            &insert,
            Parameters::positional(&[
                timestamp_value(2026, 1, 1, 8, 0, 0, 100_000_000, Some(480)).into()
            ]),
        )
        .unwrap();

    for (indexed, scanned, value) in [
        (
            "SELECT ID FROM T WHERE N = :1",
            "SELECT ID FROM T WHERE N = :1 OR FALSE",
            Value::Text("1.20".into()),
        ),
        (
            "SELECT ID FROM T WHERE C = :1",
            "SELECT ID FROM T WHERE C = :1 OR FALSE",
            Value::Text("A".into()),
        ),
        (
            "SELECT ID FROM T WHERE TZ = :1",
            "SELECT ID FROM T WHERE TZ = :1 OR FALSE",
            Value::Text("2026-01-01 00:00:00.1 +00:00".into()),
        ),
    ] {
        let parameters = [value.into()];
        assert_eq!(
            collect_with_params(&mut connection, indexed, &parameters),
            collect_with_params(&mut connection, scanned, &parameters),
        );
    }
}
