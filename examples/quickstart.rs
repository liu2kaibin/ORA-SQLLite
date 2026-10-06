mod support;

use std::process::ExitCode;

use ora_sqllite::{
    Connection, Database, DbError, Decimal, Options, ParameterValue, Parameters, Value,
};

use support::OwnedExampleDatabase;

#[derive(Debug, PartialEq, Eq)]
struct Account {
    id: Decimal,
    name: String,
    amount: Decimal,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error);
            ExitCode::FAILURE
        }
    }
}

fn run() -> ora_sqllite::Result<()> {
    let (owned, database) = OwnedExampleDatabase::create("quickstart")?;
    let mut connection = database.connect()?;

    let create = connection.prepare(
        "CREATE TABLE ACCOUNTS (ID NUMBER(10,0) PRIMARY KEY, NAME VARCHAR2(100) NOT NULL, AMOUNT NUMBER(12,2))",
    )?;
    connection.execute(&create, Parameters::none())?;

    let insert = connection.prepare("INSERT INTO ACCOUNTS VALUES (:1, :2, :3)")?;
    let first = [
        Value::Decimal(Decimal::new(false, 0, 1)?).into(),
        Value::Text("Ada".into()).into(),
        Value::Decimal(Decimal::new(false, 2, 12_50)?).into(),
    ];
    assert_eq!(
        connection
            .execute(&insert, Parameters::positional(&first))?
            .rows_affected(),
        1
    );

    let named_insert = connection.prepare("INSERT INTO ACCOUNTS VALUES (:id, :name, :amount)")?;
    let second: [(&str, ParameterValue); 3] = [
        ("id", Value::Decimal(Decimal::new(false, 0, 2)?).into()),
        ("name", Value::Text("Grace".into()).into()),
        (
            "amount",
            Value::Decimal(Decimal::new(false, 2, 20_00)?).into(),
        ),
    ];
    assert_eq!(
        connection
            .execute(&named_insert, Parameters::named(&second))?
            .rows_affected(),
        1
    );

    assert_eq!(
        find_account(&mut connection, 1)?,
        Some(Account {
            id: Decimal::new(false, 0, 1)?,
            name: "Ada".into(),
            amount: Decimal::new(false, 2, 12_50)?,
        })
    );
    assert_eq!(
        find_account(&mut connection, 2)?,
        Some(Account {
            id: Decimal::new(false, 0, 2)?,
            name: "Grace".into(),
            amount: Decimal::new(false, 2, 20_00)?,
        })
    );

    let update = connection.prepare("UPDATE ACCOUNTS SET AMOUNT = :1 WHERE ID = :2")?;
    let update_values = [
        Value::Decimal(Decimal::new(false, 2, 15_75)?).into(),
        Value::Decimal(Decimal::new(false, 0, 1)?).into(),
    ];
    assert_eq!(
        connection
            .execute(&update, Parameters::positional(&update_values))?
            .rows_affected(),
        1
    );

    let delete = connection.prepare("DELETE FROM ACCOUNTS WHERE ID = :id")?;
    let delete_values = [("id", Value::Decimal(Decimal::new(false, 0, 2)?).into())];
    assert_eq!(
        connection
            .execute(&delete, Parameters::named(&delete_values))?
            .rows_affected(),
        1
    );

    let mut committed = connection.begin_write()?;
    let insert_three = committed.prepare("INSERT INTO ACCOUNTS VALUES (3, 'Linus', 30.00)")?;
    committed.execute(&insert_three, Parameters::none())?;
    committed.commit()?;

    {
        let mut dropped = connection.begin_write()?;
        let insert_four = dropped.prepare("INSERT INTO ACCOUNTS VALUES (4, 'Drop', 40.00)")?;
        dropped.execute(&insert_four, Parameters::none())?;
    }

    let mut rolled_back = connection.begin_write()?;
    let insert_five = rolled_back.prepare("INSERT INTO ACCOUNTS VALUES (5, 'Rollback', 50.00)")?;
    rolled_back.execute(&insert_five, Parameters::none())?;
    rolled_back.rollback()?;
    assert_eq!(find_account(&mut connection, 5)?, None);

    drop(connection);
    drop(database);

    let reopened_database = Database::open(owned.main_path(), Options::default())?;
    let mut reopened_connection = reopened_database.connect()?;
    assert_eq!(
        find_account(&mut reopened_connection, 1)?,
        Some(Account {
            id: Decimal::new(false, 0, 1)?,
            name: "Ada".into(),
            amount: Decimal::new(false, 2, 15_75)?,
        })
    );
    assert_eq!(
        find_account(&mut reopened_connection, 3)?,
        Some(Account {
            id: Decimal::new(false, 0, 3)?,
            name: "Linus".into(),
            amount: Decimal::new(false, 2, 30_00)?,
        })
    );
    for absent_id in [2, 4, 5] {
        assert_eq!(find_account(&mut reopened_connection, absent_id)?, None);
    }

    drop(reopened_connection);
    drop(reopened_database);
    drop(owned);

    println!("quickstart complete: retained IDs 1 and 3; IDs 2, 4, and 5 are absent");
    Ok(())
}

fn find_account(connection: &mut Connection, id: u128) -> ora_sqllite::Result<Option<Account>> {
    let query = connection.prepare("SELECT ID, NAME, AMOUNT FROM ACCOUNTS WHERE ID = :1")?;
    let id = [Value::Decimal(Decimal::new(false, 0, id)?).into()];
    let mut rows = connection.query(&query, Parameters::positional(&id))?;
    let account = match rows.next()? {
        Some(row) => match row.values() {
            [
                Value::Decimal(id),
                Value::Text(name),
                Value::Decimal(amount),
            ] => Some(Account {
                id: *id,
                name: name.clone(),
                amount: *amount,
            }),
            values => {
                return Err(DbError::new(
                    ora_sqllite::ErrorCode::Type,
                    format!("expected DECIMAL, TEXT, DECIMAL account row, got {values:?}"),
                ));
            }
        },
        None => None,
    };
    assert!(
        rows.next()?.is_none(),
        "primary-key lookup returned two rows"
    );
    Ok(account)
}

fn print_error(error: &DbError) {
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
