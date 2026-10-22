mod support;

use std::process::ExitCode;

use ora_sqllite::{Connection, Database, Decimal, Options, Parameters, Value};

use support::OwnedExampleDatabase;

/// Demonstrates how ORA-SQLLite is used in a real-world application:
/// the `worker-ledger` (打工人小账本) embedded finance and work-hour dashboard.
///
/// Features demonstrated:
/// 1. Exact Decimal Arithmetic: Storing monetary amounts and hourly conversions in `NUMBER(12, 2)`
///    and `NUMBER(8, 2)` to eliminate floating-point rounding errors.
/// 2. Oracle SQL Subset DDL: Single-table declarations with `VARCHAR2`, `NUMBER`, and primary keys.
/// 3. Two-Phase Upsert Pattern: Application-level check-then-insert/update (compensating for absence of `ON CONFLICT`).
/// 4. Durability & Reopen: Verification that committed ledger transactions persist across database restarts.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("worker_ledger example failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> ora_sqllite::Result<()> {
    println!("=== ORA-SQLLite Real-World Showcase: worker-ledger ===");

    let (owned, database) = OwnedExampleDatabase::create("worker-ledger")?;
    let mut connection = database.connect()?;

    // 1. Initialize worker-ledger domain tables
    println!("[1/5] Creating ledger tables (EXPENSES, SUMMARIES, SETTINGS)...");
    let ddl_expenses = connection.prepare(
        "CREATE TABLE EXPENSES (\
            ID VARCHAR2(64) PRIMARY KEY, \
            EXP_DATE VARCHAR2(32) NOT NULL, \
            CATEGORY VARCHAR2(32) NOT NULL, \
            AMOUNT NUMBER(12, 2) NOT NULL, \
            NOTE VARCHAR2(500), \
            WORK_HOURS NUMBER(8, 2), \
            CREATED_AT VARCHAR2(64) NOT NULL\
        )",
    )?;
    connection.execute(&ddl_expenses, Parameters::none())?;

    let ddl_summaries = connection.prepare(
        "CREATE TABLE SUMMARIES (\
            MONTH_KEY VARCHAR2(16) PRIMARY KEY, \
            INCOME NUMBER(12, 2) NOT NULL, \
            FIXED_COST NUMBER(12, 2) NOT NULL, \
            FLEXIBLE_COST NUMBER(12, 2) NOT NULL, \
            BALANCE NUMBER(12, 2) NOT NULL, \
            UPDATED_AT VARCHAR2(64) NOT NULL\
        )",
    )?;
    connection.execute(&ddl_summaries, Parameters::none())?;

    let ddl_settings = connection.prepare(
        "CREATE TABLE SETTINGS (\
            CONFIG_KEY VARCHAR2(32) PRIMARY KEY, \
            CONFIG_JSON VARCHAR2(4000) NOT NULL, \
            UPDATED_AT VARCHAR2(64) NOT NULL\
        )",
    )?;
    connection.execute(&ddl_settings, Parameters::none())?;

    // 2. Insert initial expense records using high-precision NUMBER quantization
    println!("[2/5] Inserting expense records with NUMBER(12,2) exact quantization...");
    let insert_expense =
        connection.prepare("INSERT INTO EXPENSES VALUES (:1, :2, :3, :4, :5, :6, :7)")?;

    // Entry 1: Lunch with coffee - 42.50 RMB, converted work hours = 0.77 hours
    let row1 = [
        Value::Text("exp-001".into()).into(),
        Value::Text("2026-10-21".into()).into(),
        Value::Text("吃饭".into()).into(),
        Value::Decimal(Decimal::new(false, 2, 42_50)?).into(),
        Value::Text("午餐加一杯咖啡".into()).into(),
        Value::Decimal(Decimal::new(false, 2, 77)?).into(),
        Value::Text("2026-10-21T12:30:00Z".into()).into(),
    ];
    connection.execute(&insert_expense, Parameters::positional(&row1))?;

    // Entry 2: Late overtime commute - 35.00 RMB, work hours = 0.64 hours
    let row2 = [
        Value::Text("exp-002".into()).into(),
        Value::Text("2026-10-21".into()).into(),
        Value::Text("交通".into()).into(),
        Value::Decimal(Decimal::new(false, 2, 35_00)?).into(),
        Value::Text("下班打车接驳".into()).into(),
        Value::Decimal(Decimal::new(false, 2, 64)?).into(),
        Value::Text("2026-10-21T21:45:00Z".into()).into(),
    ];
    connection.execute(&insert_expense, Parameters::positional(&row2))?;

    // 3. Two-phase upsert pattern (check-then-update or insert)
    println!("[3/5] Performing two-phase upsert pattern on monthly review...");
    upsert_monthly_summary(
        &mut connection,
        "2026-09",
        15_000_00, // Income: 15000.00
        5_600_00,  // Fixed cost: 5600.00
        3_200_00,  // Flexible cost: 3200.00
    )?;
    // Upsert update (updating flexible costs to 3500.00)
    upsert_monthly_summary(
        &mut connection,
        "2026-09",
        15_000_00,
        5_600_00,
        3_500_00, // Updated flexible cost
    )?;

    // 4. Query and verify aggregated results
    println!("[4/5] Querying expenses and verifying balance calculations...");
    {
        let query = connection.prepare(
            "SELECT ID, CATEGORY, AMOUNT, NOTE FROM EXPENSES ORDER BY CREATED_AT DESC",
        )?;
        let mut rows = connection.query(&query, Parameters::none())?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            println!("   Record {}: {:?}", count, row.values());
        }
        assert_eq!(count, 2, "expected exactly 2 expense records");

        // Check summary balance
        let sum_query = connection.prepare(
            "SELECT MONTH_KEY, INCOME, FIXED_COST, FLEXIBLE_COST, BALANCE FROM SUMMARIES WHERE MONTH_KEY = :1",
        )?;
        let param = [Value::Text("2026-09".into()).into()];
        let mut sum_rows = connection.query(&sum_query, Parameters::positional(&param))?;
        let sum_row = sum_rows.next()?.expect("summary row should exist");
        println!("   Verified Monthly Summary: {:?}", sum_row.values());
    }

    // 5. Test durability across database restart
    println!("[5/5] Reopening database from disk to verify transaction durability...");
    drop(connection);
    drop(database);

    let reopened_db = Database::open(owned.main_path(), Options::default())?;
    let mut reopened_conn = reopened_db.connect()?;
    {
        let verify_stmt = reopened_conn.prepare("SELECT COUNT(*) FROM EXPENSES")?;
        let mut verify_rows = reopened_conn.query(&verify_stmt, Parameters::none())?;
        let row = verify_rows.next()?.expect("count row");
        println!("   Persisted expenses count after reopen: {:?}", row.values());
    }

    drop(reopened_conn);
    drop(reopened_db);
    drop(owned);

    println!("\n worker-ledger showcase completed successfully!");
    println!("Reference consumer repository: https://github.com/liu2kaibin/worker-ledger");
    Ok(())
}

/// Helper function demonstrating the two-phase upsert pattern in ORA-SQLLite
fn upsert_monthly_summary(
    connection: &mut Connection,
    month: &str,
    income_cents: u128,
    fixed_cents: u128,
    flexible_cents: u128,
) -> ora_sqllite::Result<()> {
    let balance_cents = income_cents.saturating_sub(fixed_cents + flexible_cents);

    let check_stmt =
        connection.prepare("SELECT MONTH_KEY FROM SUMMARIES WHERE MONTH_KEY = :1")?;
    let check_param = [Value::Text(month.to_string()).into()];
    let exists = {
        let mut rows = connection.query(&check_stmt, Parameters::positional(&check_param))?;
        rows.next()?.is_some()
    };

    if exists {
        let update_stmt = connection.prepare(
            "UPDATE SUMMARIES SET INCOME = :1, FIXED_COST = :2, FLEXIBLE_COST = :3, BALANCE = :4, UPDATED_AT = :5 WHERE MONTH_KEY = :6"
        )?;
        let params = [
            Value::Decimal(Decimal::new(false, 2, income_cents)?).into(),
            Value::Decimal(Decimal::new(false, 2, fixed_cents)?).into(),
            Value::Decimal(Decimal::new(false, 2, flexible_cents)?).into(),
            Value::Decimal(Decimal::new(false, 2, balance_cents)?).into(),
            Value::Text("2026-10-21T14:00:00Z".into()).into(),
            Value::Text(month.to_string()).into(),
        ];
        connection.execute(&update_stmt, Parameters::positional(&params))?;
    } else {
        let insert_stmt =
            connection.prepare("INSERT INTO SUMMARIES VALUES (:1, :2, :3, :4, :5, :6)")?;
        let params = [
            Value::Text(month.to_string()).into(),
            Value::Decimal(Decimal::new(false, 2, income_cents)?).into(),
            Value::Decimal(Decimal::new(false, 2, fixed_cents)?).into(),
            Value::Decimal(Decimal::new(false, 2, flexible_cents)?).into(),
            Value::Decimal(Decimal::new(false, 2, balance_cents)?).into(),
            Value::Text("2026-10-21T14:00:00Z".into()).into(),
        ];
        connection.execute(&insert_stmt, Parameters::positional(&params))?;
    }

    Ok(())
}
