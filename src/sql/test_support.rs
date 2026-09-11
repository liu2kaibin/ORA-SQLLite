use crate::{
    DataType, Database, ErrorCode, Options, ParameterIdentifier, Result,
    sql::{SqlLimits, bind::bind_statement, bound::BoundStatement, parser::parse_statement},
    storage::test_support::phase_4b_catalog_fixture,
};

/// Prepares one full SQL statement against the Task 9 catalog fixture.
///
/// The fixture remains alive until parsing and binding have both completed, so
/// catalog IDs and the database identity always originate from a real snapshot.
pub(crate) fn prepare_fixture(sql: &str) -> Result<BoundStatement> {
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    let database_uuid = pager.database_uuid()?;
    let mut snapshot = pager.begin_read()?;
    let catalog = snapshot.catalog_reader()?;
    let statement = parse_statement(sql, &SqlLimits::default())?;
    bind_statement(&statement, &catalog, database_uuid)
}

#[test]
fn prepare_returns_metadata_without_writing_database_or_wal() {
    // Production break caught: omitting any public prepare entry point, using a
    // different catalog view, or publishing prepare-time storage changes would
    // fail this real database/WAL and metadata contract.
    let fixture = phase_4b_catalog_fixture(4096);
    let before_main = std::fs::read(fixture.main_path()).unwrap();
    let before_wal = std::fs::read(fixture.wal_path()).unwrap();
    let sql = "SELECT ID, NAME FROM USERS WHERE ID = :1";

    let statement = {
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let conn = db.connect().unwrap();
        conn.prepare(sql).unwrap()
    };
    assert_eq!(statement.parameters().len(), 1);
    assert_eq!(
        statement.parameters()[0].identifier(),
        &ParameterIdentifier::Position(1)
    );
    assert_eq!(statement.parameters()[0].data_type(), Some(DataType::Int64));
    assert_eq!(statement.columns().len(), 2);
    assert_eq!(statement.columns()[0].name(), "ID");
    assert_eq!(statement.columns()[0].data_type(), DataType::Int64);
    assert!(!statement.columns()[0].nullable());
    assert!(statement.columns()[0].source_table_id().is_some());
    assert!(statement.columns()[0].source_column_id().is_some());
    assert_eq!(statement.columns()[1].name(), "NAME");
    assert_eq!(statement.columns()[1].data_type(), DataType::Text);
    assert!(statement.columns()[1].nullable());
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);

    {
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut conn = db.connect().unwrap();
        let mut read = conn.begin_read().unwrap();
        let statement = read.prepare(sql).unwrap();
        assert_eq!(statement.parameters().len(), 1);
        assert_eq!(statement.columns().len(), 2);
    }
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);

    {
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut conn = db.connect().unwrap();
        let mut write = conn.begin_write().unwrap();
        let statement = write.prepare(sql).unwrap();
        assert_eq!(statement.parameters().len(), 1);
        assert_eq!(statement.columns().len(), 2);
        write.rollback().unwrap();
    }
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);

    {
        let db = Database::open(fixture.main_path(), Options::default().read_only()).unwrap();
        let conn = db.connect().unwrap();
        let statement = conn.prepare(sql).unwrap();
        assert_eq!(statement.parameters().len(), 1);
        assert_eq!(statement.columns().len(), 2);
    }
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);
}

#[test]
fn prepare_rejects_first_logical_parameter_over_limit() {
    // Production break caught: retaining max_parameters as configuration-only
    // state lets a statement allocate and expose more logical binds than allowed.
    let fixture = phase_4b_catalog_fixture(4096);
    let limits = SqlLimits::default().with_max_parameters(1).unwrap();
    let db = Database::open(
        fixture.main_path(),
        Options::default().with_sql_limits(limits),
    )
    .unwrap();
    let conn = db.connect().unwrap();

    assert_eq!(
        conn.prepare("SELECT :only FROM USERS")
            .unwrap()
            .parameters()
            .len(),
        1
    );
    assert_eq!(
        conn.prepare("SELECT :first, :second FROM USERS")
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
}

fn public_prepare_with_limits(
    fixture: &crate::storage::test_support::Phase4bCatalogFixture,
    limits: SqlLimits,
    sql: &str,
) -> Result<crate::Statement> {
    let db = Database::open(
        fixture.main_path(),
        Options::default().with_sql_limits(limits),
    )?;
    db.connect()?.prepare(sql)
}

#[test]
fn phase_4b_sql_corpus() {
    // Production break caught: bypassing any parser, normalization, catalog
    // binding, static typing, or metadata stage changes an approved statement's
    // result or an excluded statement's stable error classification.
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();

    let accepted = [
        "SELECT ID, NAME FROM USERS WHERE ID = :1",
        " -- lead\n SELECT /* projection */ \"ID\" FROM \"USERS\"; -- tail",
        "SELECT * FROM USERS",
        "SELECT U.*, O.USER_ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID",
        "SELECT U.ID, O.ID FROM USERS U LEFT OUTER JOIN ORDERS O ON O.USER_ID = U.ID",
        "SELECT DISTINCT NAME FROM USERS WHERE NAME IS NOT NULL ORDER BY NAME",
        "SELECT USER_ID, COUNT(*), SUM(ID), AVG(ID), MIN(ID), MAX(ID) FROM ORDERS GROUP BY USER_ID HAVING COUNT(*) > 0 ORDER BY 1",
        "SELECT NULL, TRUE, 1, 1.5, 1E2, 'A''B', X'ABCD' FROM USERS",
        "SELECT ID FROM USERS WHERE NOT (ID BETWEEN 1 AND 10) OR (NAME LIKE 'A%' AND ID IN (1, 2) AND NAME IS NOT NULL)",
        "INSERT INTO USERS (ID, NAME) VALUES (:id, :name)",
        "UPDATE USERS U SET NAME = U.NAME, ID = ID + :delta WHERE U.ID = :id",
        "DELETE FROM USERS U WHERE U.ID = :id",
        "CREATE TABLE CHILD (ID INT64 PRIMARY KEY, USER_ID INT64 REFERENCES USERS (ID), NAME TEXT NOT NULL, AMOUNT DECIMAL, SCORE FLOAT64, RAW_VALUE BLOB, CREATED DATE, UPDATED TIMESTAMP, ENABLED BOOL, CONSTRAINT CHILD_CK CHECK (ID > 0), UNIQUE (NAME))",
        "CREATE UNIQUE INDEX USERS_NAME_UK ON USERS (NAME)",
        "DROP TABLE ORDERS",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
    ];
    for sql in accepted {
        conn.prepare(sql)
            .unwrap_or_else(|error| panic!("accepted corpus rejected {sql:?}: {error}"));
    }

    let rejected = [
        ("", ErrorCode::Syntax),
        ("SELECT ID FROM USERS; DELETE FROM USERS", ErrorCode::Syntax),
        ("SELECT FROM USERS", ErrorCode::Syntax),
        ("SELECT ID FROM USERS U WHERE USERS.ID = 1", ErrorCode::Bind),
        (
            "SELECT ID FROM USERS U INNER JOIN ORDERS O ON O.ID = U.ID",
            ErrorCode::Bind,
        ),
        ("SELECT MISSING FROM USERS", ErrorCode::Bind),
        ("SELECT USERS.ID.VALUE FROM USERS", ErrorCode::Unsupported),
        ("SELECT :1, :name FROM USERS", ErrorCode::Bind),
        ("SELECT :01 FROM USERS", ErrorCode::Bind),
        ("SELECT :1, :3 FROM USERS", ErrorCode::Bind),
        (
            "SELECT ID FROM (SELECT ID FROM USERS) X",
            ErrorCode::Unsupported,
        ),
        (
            "WITH X AS (SELECT ID FROM USERS) SELECT ID FROM X",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT ID FROM USERS UNION SELECT ID FROM ORDERS",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT U.ID FROM USERS U RIGHT JOIN ORDERS O ON O.USER_ID = U.ID",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT U.ID FROM USERS U CROSS JOIN ORDERS O",
            ErrorCode::Unsupported,
        ),
        ("SELECT ID FROM USERS LIMIT 1", ErrorCode::Unsupported),
        (
            "SELECT ID FROM USERS WHERE ROWNUM <= 1",
            ErrorCode::Unsupported,
        ),
        ("SELECT SUM(ID) OVER () FROM USERS", ErrorCode::Unsupported),
        (
            "SELECT ID FROM USERS MODEL RETURN UPDATED ROWS",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT CASE WHEN ID = 1 THEN 1 ELSE 0 END FROM USERS",
            ErrorCode::Unsupported,
        ),
        ("SELECT CAST(ID AS TEXT) FROM USERS", ErrorCode::Unsupported),
        ("SELECT NAME || NAME FROM USERS", ErrorCode::Unsupported),
        ("SELECT ID + NAME FROM USERS", ErrorCode::Type),
        ("CREATE TABLE USERS (ID INT64)", ErrorCode::Bind),
        (
            "CREATE TABLE ORACLE_TYPES (N NUMBER(10,2))",
            ErrorCode::Unsupported,
        ),
        (
            "CREATE TABLE BAD_CHECK (NAME TEXT, CHECK (NAME))",
            ErrorCode::Type,
        ),
        ("DROP TABLE MISSING", ErrorCode::Bind),
        ("DROP INDEX MISSING", ErrorCode::Bind),
        (
            "MERGE INTO USERS U USING ORDERS O ON (U.ID = O.USER_ID) WHEN MATCHED THEN UPDATE SET U.NAME = 'X'",
            ErrorCode::Unsupported,
        ),
        ("TRUNCATE TABLE USERS", ErrorCode::Unsupported),
        ("BEGIN TRANSACTION", ErrorCode::Unsupported),
    ];
    for (sql, expected) in rejected {
        let error = conn.prepare(sql).unwrap_err();
        assert_eq!(
            error.code(),
            expected,
            "wrong classification for {sql:?}: {error}"
        );
    }
}

#[test]
fn phase_4b_prepare_limits_cover_below_exact_and_above_boundaries() {
    // Production break caught: off-by-one comparisons at any public preparation
    // budget would reject the exact boundary or accept the first excess unit.
    let fixture = phase_4b_catalog_fixture(4096);
    let simple = "SELECT ID FROM USERS";

    for (limit, accepted) in [
        (simple.len() - 1, false),
        (simple.len(), true),
        (simple.len() + 1, true),
    ] {
        let limits = SqlLimits::default().with_max_sql_bytes(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "SQL byte limit {limit}"
        );
    }
    for (limit, accepted) in [(3, false), (4, true), (5, true)] {
        let limits = SqlLimits::default().with_max_tokens(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "token limit {limit}"
        );
    }
    for (limit, accepted) in [(3, false), (4, true), (5, true)] {
        let limits = SqlLimits::default().with_max_ast_nodes(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "AST node limit {limit}"
        );
    }
    for (limit, accepted) in [(2, false), (3, true), (4, true)] {
        let limits = SqlLimits::default().with_max_nesting(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "nesting limit {limit}"
        );
    }

    let two_parameters = "SELECT :first, :second FROM USERS";
    for (limit, accepted) in [(1, false), (2, true), (3, true)] {
        let limits = SqlLimits::default().with_max_parameters(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, two_parameters).is_ok(),
            accepted,
            "parameter limit {limit}"
        );
    }

    let two_joins = "SELECT U.ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID LEFT JOIN USERS U2 ON U2.ID = O.USER_ID";
    for (limit, accepted) in [(1, false), (2, true), (3, true)] {
        let limits = SqlLimits::default().with_max_joins(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, two_joins).is_ok(),
            accepted,
            "JOIN limit {limit}"
        );
    }
}

#[test]
fn phase_4b_random_sql_never_panics() {
    // Production break caught: an unchecked tokenizer/parser/owned-AST branch
    // that panics for bounded arbitrary bytes or valid Unicode fails this model.
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    let mut state = 0x6a09_e667_f3bc_c909_u64;

    for case in 0..10_000 {
        let length = (xorshift64(&mut state) as usize) % 65;
        let sql = if case % 2 == 0 {
            let bytes = (0..length)
                .map(|_| xorshift64(&mut state) as u8)
                .collect::<Vec<_>>();
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            let mut value = String::new();
            for _ in 0..length {
                let mut scalar = (xorshift64(&mut state) % 0x11_0000) as u32;
                if (0xd800..=0xdfff).contains(&scalar) {
                    scalar = 0xfffd;
                }
                value.push(char::from_u32(scalar).unwrap());
            }
            value
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| conn.prepare(&sql)));
        assert!(
            result.is_ok(),
            "prepare panicked for deterministic case {case}: {sql:?}"
        );
    }
}

fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
