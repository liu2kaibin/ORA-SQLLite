# ORA-SQLLite

ORA-SQLLite is a pure-Rust, in-process, single-file-plus-WAL database with an Oracle-inspired CRUD subset.

## Developer preview

`v0.1.0` is a GitHub developer pre-release. It is not published on crates.io and is not Oracle wire compatible. Use it for evaluation and embedded development, not as a drop-in Oracle Database replacement.

Add the tagged Git dependency:

```toml
[dependencies]
ora_sqllite = { git = "https://github.com/liu2kaibin/ORA-SQLLite.git", tag = "v0.1.0" }
```

## Minimal embedded flow

`Database::create` requires a path that does not exist. It creates the main file and its WAL sidecar; the application owns those files.

```rust
use ora_sqllite::{Database, Options, Parameters, Value};

fn main() -> ora_sqllite::Result<()> {
    let database = Database::create("app.db", Options::default())?;
    let mut connection = database.connect()?;

    let create = connection.prepare(
        "CREATE TABLE PEOPLE (ID INT64 PRIMARY KEY, NAME TEXT NOT NULL)",
    )?;
    connection.execute(&create, Parameters::none())?;

    let insert = connection.prepare("INSERT INTO PEOPLE VALUES (:1, :2)")?;
    let values = [Value::Int64(1).into(), Value::Text("Ada".into()).into()];
    connection.execute(&insert, Parameters::positional(&values))?;

    let query = connection.prepare("SELECT ID, NAME FROM PEOPLE WHERE ID = :1")?;
    let id = [Value::Int64(1).into()];
    let mut rows = connection.query(&query, Parameters::positional(&id))?;
    while let Some(row) = rows.next()? {
        println!("{:?}", row.values());
    }

    Ok(())
}
```

## Run the examples

The examples create uniquely owned temporary databases, reopen them, verify results, and remove their files.

```console
cargo run --example quickstart
cargo run --example oracle_types
```

## Operating model and limits

- One process owns an open database. Within it, cloneable `Database` handles can create multiple independent `Connection` sessions.
- Reads use snapshots; multiple readers can coexist, while writes are serialized to one writer.
- The API is synchronous and embedded. There is no CLI, network server, Oracle protocol, async API, ORM integration, PL/SQL, or multi-process access.
- SQL, Oracle-oriented scalar types, constraints, and index use are deliberately bounded subsets. Check the compatibility matrix before adopting a feature.

## Developer documentation

- [Getting started](docs/getting-started.md)
- [Oracle compatibility matrix](docs/oracle-compatibility.md)
- Build the API documentation with `cargo doc --no-deps`.
- [Changelog](CHANGELOG.md)
- [`v0.1.0` release notes](docs/releases/v0.1.0.md)

Status: developer preview; MSRV: Rust 1.85; on-disk format: 2. The lightweight MVP evidence boundary uses focused release checks and an approved maximum 100,000-row scale run; the retained 4.7-million-row observation is partial evidence only, and no 10-million-row PASS is claimed.

## Project records

Historical rationale and implementation detail remain in the [architecture design](docs/superpowers/specs/2026-09-01-rust-oracle-compatible-embedded-db-design.md), the [Phase 6 developer-preview design](docs/superpowers/specs/2026-10-05-phase-6-rust-developer-preview-design.md), and the [MVP scale acceptance record](docs/superpowers/plans/2026-10-04-phase-4c2d-mvp-scale-acceptance.md).
