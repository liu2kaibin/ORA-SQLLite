# Getting started

ORA-SQLLite is a synchronous embedded Rust library. The examples below use only symbols exported by `ora_sqllite`; the complete executable flows are in [`quickstart.rs`](../examples/quickstart.rs) and [`oracle_types.rs`](../examples/oracle_types.rs).

## Create, open, and own the files

`Database::create` exclusively creates a new database and never overwrites an existing main-file path. `Database::open` opens an existing database and returns `ErrorCode::Io` if the path is missing; it does not create a missing file. The application chooses and owns the path, and dropping Rust handles does not delete the main file or WAL sidecar.

```rust
use std::path::Path;
use ora_sqllite::{Database, Options};

fn create_then_reopen(path: &Path) -> ora_sqllite::Result<()> {
    let database = Database::create(path, Options::default())?;
    drop(database);

    let reopened = Database::open(path, Options::default())?;
    drop(reopened);
    Ok(())
}
```

Keep the main file and WAL together. Do not rename, replace, or delete either while the database is open. Creation and durable commits depend on the operating system, filesystem, and storage honoring successful flushes.

## Share `Database`; mutate `Connection`

`Database` is a cloneable handle to shared in-process pager, cache, writer coordination, and published state. `Database::connect` creates an independent synchronous `Connection`. Preparing and executing statements changes session state, so those operations require `&mut Connection`.

```rust
use ora_sqllite::Database;

fn two_sessions(database: &Database) -> ora_sqllite::Result<()> {
    let shared = database.clone();
    let mut first = database.connect()?;
    let mut second = shared.connect()?;
    let _first_statement = first.prepare("SELECT 1")?;
    let _second_statement = second.prepare("SELECT 2")?;
    Ok(())
}
```

## Prepare and reuse statements

`Connection::prepare` returns an owned `Statement` with parameter and result metadata. Reuse it with new parameter arrays instead of reparsing the SQL. At execution, ORA-SQLLite revalidates the database identity and retained schema dependencies. If a relevant table, constraint, or selected index no longer matches, execution returns `ErrorCode::SchemaChanged`; prepare the statement again against the current catalog.

```rust
use ora_sqllite::{Connection, ParameterValue, Parameters, Value};

fn insert_people(connection: &mut Connection) -> ora_sqllite::Result<()> {
    let insert = connection.prepare("INSERT INTO PEOPLE VALUES (:1, :2)")?;
    for (id, name) in [(1, "Ada"), (2, "Grace")] {
        let values: [ParameterValue; 2] = [
            Value::Int64(id).into(),
            Value::Text(name.into()).into(),
        ];
        connection.execute(&insert, Parameters::positional(&values))?;
    }
    Ok(())
}
```

## Bind positional and named parameters

Positional parameters are one-based in SQL and match the slice order. Named parameters match case-insensitively after SQL-name normalization. Both constructors borrow arrays of `ParameterValue`; keep the array alive through the call. Use `ParameterValue::typed_null` when a `NULL` needs a concrete `DataType`.

```rust
use ora_sqllite::{Connection, ParameterValue, Parameters, Value};

fn bind_both_forms(connection: &mut Connection) -> ora_sqllite::Result<()> {
    let positional = connection.prepare("INSERT INTO PEOPLE VALUES (:1, :2)")?;
    let positional_values: [ParameterValue; 2] = [
        Value::Int64(3).into(),
        Value::Text("Linus".into()).into(),
    ];
    connection.execute(&positional, Parameters::positional(&positional_values))?;

    let named = connection.prepare("INSERT INTO PEOPLE VALUES (:id, :name)")?;
    let named_values: [(&str, ParameterValue); 2] = [
        ("id", Value::Int64(4).into()),
        ("name", Value::Text("Margaret".into()).into()),
    ];
    connection.execute(&named, Parameters::named(&named_values))?;
    Ok(())
}
```

## Pull rows and match indexed values

`Connection::query` returns pull-based `Rows`. Each `next` call returns `Result<Option<Row>>`; a `Row` owns its values. `Row::get` uses a zero-based index, and `Row::values` exposes the full ordered slice. Match the `Value` variant that corresponds to the prepared result metadata.

```rust
use ora_sqllite::{Connection, Parameters, Value};

fn print_people(connection: &mut Connection) -> ora_sqllite::Result<()> {
    let query = connection.prepare("SELECT ID, NAME FROM PEOPLE ORDER BY ID")?;
    let mut rows = connection.query(&query, Parameters::none())?;
    while let Some(row) = rows.next()? {
        match (row.get(0), row.get(1)) {
            (Some(Value::Int64(id)), Some(Value::Text(name))) => {
                println!("{id}: {name}");
            }
            values => println!("unexpected row values: {values:?}"),
        }
    }
    Ok(())
}
```

`Rows` keeps its connection or transaction mutably borrowed. Exhaust or drop it before preparing or executing through that owner again.

## Use snapshots and explicit writes

`begin_read` captures a fixed committed snapshot that does not advance when another connection commits. `begin_write` acquires the single writer without blocking. A write becomes visible only after `commit`; `rollback` explicitly discards it and reports rollback errors. Dropping an unfinished `WriteTransaction` attempts rollback and never publishes its changes. Outside an explicit transaction, each successful DDL or DML `Connection::execute` auto-commits. Validation or execution errors returned before durability publish no changes. A commit/WAL I/O error can instead occur after durable sync, so the commit outcome is uncertain rather than known successful or rolled back. Reopen the database so recovery runs, then reconcile the observed database state before retrying the business write; a blind retry can duplicate it.

```rust
use ora_sqllite::{Connection, Parameters};

fn transaction_lifecycle(connection: &mut Connection) -> ora_sqllite::Result<()> {
    {
        let mut read = connection.begin_read()?;
        let query = read.prepare("SELECT ID FROM PEOPLE ORDER BY ID")?;
        let mut rows = read.query(&query, Parameters::none())?;
        while rows.next()?.is_some() {}
    }

    {
        let mut write = connection.begin_write()?;
        let insert = write.prepare("INSERT INTO PEOPLE VALUES (5, 'Committed')")?;
        write.execute(&insert, Parameters::none())?;
        write.commit()?;
    }

    {
        let mut write = connection.begin_write()?;
        let insert = write.prepare("INSERT INTO PEOPLE VALUES (6, 'Rolled back')")?;
        write.execute(&insert, Parameters::none())?;
        write.rollback()?;
    }

    {
        let mut write = connection.begin_write()?;
        let insert = write.prepare("INSERT INTO PEOPLE VALUES (7, 'Dropped')")?;
        write.execute(&insert, Parameters::none())?;
    }

    Ok(())
}
```

## Open read-only and set resource limits

`Options::read_only` applies to `Database::open`; `Database::create` always creates a writable database. A read-only database rejects writes with `ErrorCode::Unsupported`. Builders bound the cache, database and WAL sizes, per-query work memory, and SQL preparation. Page size applies only to creation; an existing database uses its stored page size.

```rust
use std::path::Path;
use ora_sqllite::{Database, Options, SqlLimits};

fn open_bounded_read_only(path: &Path) -> ora_sqllite::Result<Database> {
    let sql_limits = SqlLimits::default()
        .with_max_sql_bytes(64 * 1024)?
        .with_max_parameters(1_024)?
        .with_max_joins(16)?;
    let options = Options::default()
        .read_only()
        .with_cache_pages(256)
        .with_max_database_bytes(2 * 1024 * 1024 * 1024)
        .with_checkpoint_wal_bytes(16 * 1024 * 1024)
        .with_max_wal_bytes(256 * 1024 * 1024)
        .with_max_query_memory_bytes(16 * 1024 * 1024)
        .with_sql_limits(sql_limits);
    Database::open(path, options)
}
```

## Handle structured errors

Branch on the stable `ErrorCode` and its stable numeric `as_u32` value. `DbError::oracle_code` and `DbError::sql_location` provide optional structured Oracle-oriented diagnostics. A SQL location has a zero-based UTF-8 byte offset and one-based line and column. The text produced by `Display` is diagnostic and unstable; do not parse or compare it as an API contract.

```rust
use ora_sqllite::DbError;

fn report(error: &DbError) {
    eprintln!("category {:?} ({})", error.code(), error.code().as_u32());
    if let Some(code) = error.oracle_code() {
        eprintln!("Oracle code: {code}");
    }
    if let Some(location) = error.sql_location() {
        eprintln!(
            "SQL byte {}, line {}, column {}",
            location.byte_offset(),
            location.line(),
            location.column()
        );
    }
    eprintln!("diagnostic only: {error}");
}
```

## Main file, WAL, and concurrency

The durable database consists of one main file plus its WAL sidecar. An initialized database does not recreate a missing WAL. Open both through one owning process: a second process receives `ErrorCode::Busy`. Within the owning process, shared `Database` handles permit multiple snapshot readers and independent connections, but only one writer at a time; a competing writer receives `Busy` instead of waiting.

See the [Oracle compatibility matrix](oracle-compatibility.md) for the exact SQL and type boundaries and the [`v0.1.0` release notes](releases/v0.1.0.md) for the current evidence state.
