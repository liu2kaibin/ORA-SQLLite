mod support;

use ora_sqllite::{Connection, Decimal, ParameterValue, Parameters, Value};

fn decimal_value(magnitude: u128, scale: i8) -> Value {
    Value::Decimal(Decimal::new(false, scale, magnitude).unwrap())
}

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
