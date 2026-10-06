mod support;

use std::process::ExitCode;

use ora_sqllite::{
    Connection, Database, DbError, Decimal, ErrorCode, Options, OracleDate, OracleTimestamp,
    ParameterValue, Parameters, Value,
};

use support::OwnedExampleDatabase;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("oracle_types failed");
            print_diagnostics(&error);
            ExitCode::FAILURE
        }
    }
}

fn run() -> ora_sqllite::Result<()> {
    let (owned, database) = OwnedExampleDatabase::create("oracle-types")?;
    let mut connection = database.connect()?;

    let create = connection.prepare(
        "CREATE TABLE ORACLE_VALUES (\
           ID NUMBER(10,0), N NUMBER(12,2), V VARCHAR2(20), C CHAR(4), R RAW(8), \
           D DATE, TS TIMESTAMP(9), TZ TIMESTAMP(9) WITH TIME ZONE, \
           CONSTRAINT PK_ORACLE_VALUES PRIMARY KEY (ID))",
    )?;
    connection.execute(&create, Parameters::none())?;

    let date = OracleDate::new(2026, 10, 5, 9, 30, 15)?;
    let timestamp = OracleTimestamp::from_nanoseconds(date, 123_456_789, None)?;
    let zoned = OracleTimestamp::from_nanoseconds(date, 987_654_321, Some(480))?;
    let values: [ParameterValue; 8] = [
        Value::Decimal(Decimal::new(false, 0, 1)?).into(),
        Value::Decimal(Decimal::new(false, 2, 123_45)?).into(),
        Value::Text(String::new()).into(),
        Value::Text("A".into()).into(),
        Value::Blob(vec![0x00, 0x7f, 0xff]).into(),
        Value::Date(date).into(),
        Value::Timestamp(timestamp).into(),
        Value::Timestamp(zoned).into(),
    ];
    let insert =
        connection.prepare("INSERT INTO ORACLE_VALUES VALUES (:1, :2, :3, :4, :5, :6, :7, :8)")?;
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&values))?
            .rows_affected(),
        1
    );
    assert_normalized_row(&mut connection, date, timestamp, zoned)?;

    let error = connection
        .execute(&insert, Parameters::positional(&values))
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert_eq!(error.oracle_code(), Some(1));
    print_error("PK_ORACLE_VALUES", &error);

    drop(connection);
    drop(database);

    let reopened_database = Database::open(owned.main_path(), Options::default())?;
    let mut reopened_connection = reopened_database.connect()?;
    assert_normalized_row(&mut reopened_connection, date, timestamp, zoned)?;

    drop(reopened_connection);
    drop(reopened_database);
    drop(owned);

    println!("oracle_types complete: Oracle values and structured errors verified after reopen");
    Ok(())
}

fn assert_normalized_row(
    connection: &mut Connection,
    date: OracleDate,
    timestamp: OracleTimestamp,
    zoned: OracleTimestamp,
) -> ora_sqllite::Result<()> {
    let query =
        connection.prepare("SELECT N, V, C, R, D, TS, TZ FROM ORACLE_VALUES WHERE ID = :1")?;
    let id = [Value::Decimal(Decimal::new(false, 0, 1)?).into()];
    let mut rows = connection.query(&query, Parameters::positional(&id))?;
    let row = rows
        .next()?
        .ok_or_else(|| DbError::new(ErrorCode::Internal, "expected ORACLE_VALUES row"))?;

    assert_eq!(
        row.get(0),
        Some(&Value::Decimal(Decimal::new(false, 2, 123_45)?))
    );
    assert_eq!(row.get(1), Some(&Value::Null));
    assert_eq!(row.get(2), Some(&Value::Text("A   ".into())));
    assert_eq!(row.get(3), Some(&Value::Blob(vec![0x00, 0x7f, 0xff])));
    assert_eq!(row.get(4), Some(&Value::Date(date)));

    let Some(Value::Timestamp(actual_timestamp)) = row.get(5) else {
        panic!("TIMESTAMP column did not return Value::Timestamp");
    };
    assert_eq!(actual_timestamp.date(), date);
    assert_eq!(actual_timestamp.nanoseconds(), 123_456_789);
    assert_eq!(actual_timestamp.offset_minutes(), None);
    assert_eq!(*actual_timestamp, timestamp);

    let Some(Value::Timestamp(actual_zoned)) = row.get(6) else {
        panic!("TIMESTAMP WITH TIME ZONE column did not return Value::Timestamp");
    };
    assert_eq!(actual_zoned.date(), date);
    assert_eq!(actual_zoned.nanoseconds(), 987_654_321);
    assert_eq!(actual_zoned.offset_minutes(), Some(480));
    assert_eq!(*actual_zoned, zoned);

    assert!(
        rows.next()?.is_none(),
        "primary-key lookup returned two rows"
    );
    drop(rows);
    Ok(())
}

fn print_error(constraint: &str, error: &DbError) {
    eprintln!("Constraint: {constraint}");
    print_diagnostics(error);
}

fn print_diagnostics(error: &DbError) {
    eprintln!("ErrorCode: {:?} ({})", error.code(), error.code().as_u32());
    if let Some(oracle_code) = error.oracle_code() {
        eprintln!("Oracle code: {oracle_code}");
    }
    if let Some(location) = error.sql_location() {
        eprintln!(
            "SQL location: byte {}, line {}, column {}",
            location.byte_offset(),
            location.line(),
            location.column()
        );
    }
    eprintln!("{error}");
}
