mod common;

use std::{
    fs::{self, OpenOptions},
    io::{Seek, SeekFrom, Write},
};

use ora_sqllite::{
    DataType, Database, Decimal, ErrorCode, Options, OracleDate, OracleTimestamp, ParameterValue,
    Parameters, Value,
};

use common::{collect, collect_params, execute_sql};

struct IndexedFixture {
    _files: common::TempPaths,
    database: Database,
}

impl IndexedFixture {
    fn new(label: &str) -> Self {
        let (files, database) = common::new_database(label);
        let mut connection = database.connect().expect("connect indexed fixture");
        execute_sql(
            &mut connection,
            "CREATE TABLE T (
                ID INT64 PRIMARY KEY,
                A INT64,
                B TEXT,
                C DECIMAL,
                F FLOAT64,
                RAW BLOB,
                D DATE,
                TS TIMESTAMP,
                FLAG BOOL,
                U INT64 UNIQUE
            )",
        );
        execute_sql(&mut connection, "CREATE INDEX IX_T_A_B ON T(A, B)");
        execute_sql(&mut connection, "CREATE INDEX IX_T_C ON T(C)");
        execute_sql(&mut connection, "CREATE INDEX IX_T_RAW ON T(RAW)");
        execute_sql(
            &mut connection,
            "CREATE TABLE J (ID INT64 PRIMARY KEY, NOTE TEXT)",
        );

        let insert = connection
            .prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5, :6, :7, :8, :9, :10)")
            .expect("prepare indexed fixture insert");
        for values in fixture_rows() {
            connection
                .execute(&insert, Parameters::positional(&values))
                .expect("insert indexed fixture row");
        }
        for sql in [
            "INSERT INTO J VALUES (4, 'four')",
            "INSERT INTO J VALUES (5, 'five')",
            "INSERT INTO J VALUES (6, 'six')",
        ] {
            execute_sql(&mut connection, sql);
        }
        drop(connection);
        Self {
            _files: files,
            database,
        }
    }

    fn connect(&self) -> ora_sqllite::Connection {
        self.database.connect().expect("connect indexed fixture")
    }
}

struct ScalarIndexedFixture {
    _files: common::TempPaths,
    database: Database,
}

impl ScalarIndexedFixture {
    fn new(label: &str) -> Self {
        let (files, database) = common::new_database(label);
        let mut connection = database.connect().expect("connect scalar-index fixture");
        execute_sql(
            &mut connection,
            "CREATE TABLE S (
                ID INT64 PRIMARY KEY,
                F FLOAT64,
                D DATE,
                TS TIMESTAMP,
                FLAG BOOL
            )",
        );
        for sql in [
            "CREATE INDEX IX_S_F ON S(F)",
            "CREATE INDEX IX_S_D ON S(D)",
            "CREATE INDEX IX_S_TS ON S(TS)",
            "CREATE INDEX IX_S_FLAG ON S(FLAG)",
        ] {
            execute_sql(&mut connection, sql);
        }
        let insert = connection
            .prepare("INSERT INTO S VALUES (:1, :2, :3, :4, :5)")
            .expect("prepare scalar-index insert");
        let date = |day| OracleDate::new(2026, 3, day, 0, 0, 0).unwrap();
        for (id, float, day, flag) in [(1, -1.5, 1, false), (2, 0.0, 2, true), (3, 1.5, 3, false)] {
            let timestamp = OracleTimestamp::new(date(day), u32::from(day) * 10, None).unwrap();
            let values = [
                Value::Int64(id).into(),
                Value::Float64(float).into(),
                Value::Date(date(day)).into(),
                Value::Timestamp(timestamp).into(),
                Value::Bool(flag).into(),
            ];
            connection
                .execute(&insert, Parameters::positional(&values))
                .expect("insert scalar-index row");
        }
        drop(connection);
        Self {
            _files: files,
            database,
        }
    }

    fn connect(&self) -> ora_sqllite::Connection {
        self.database
            .connect()
            .expect("connect scalar-index fixture")
    }
}

#[test]
fn arithmetic_equivalence_preserves_public_rows_and_errors_with_indexes() {
    // Break caught: adding an index must neither hide arithmetic errors nor
    // change the rows already emitted before an eventual arithmetic error.
    let (_files, database) = common::new_database("arithmetic-index-equivalence");
    let mut connection = database.connect().unwrap();
    execute_sql(
        &mut connection,
        "CREATE TABLE T (ID INT64, A INT64, D INT64, F FLOAT64)",
    );
    execute_sql(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
    let insert = connection
        .prepare("INSERT INTO T VALUES (:1, :2, :3, :4)")
        .unwrap();
    for (id, key, divisor, value) in [
        (1, 3, 1, f64::MAX),
        (2, 2, 0, f64::MAX),
        (3, 1, 1, -f64::MAX),
    ] {
        connection
            .execute(
                &insert,
                Parameters::positional(&[
                    Value::Int64(id).into(),
                    Value::Int64(key).into(),
                    Value::Int64(divisor).into(),
                    Value::Float64(value).into(),
                ]),
            )
            .unwrap();
    }

    fn outcome(
        connection: &mut ora_sqllite::Connection,
        sql: &str,
    ) -> (Vec<Vec<Value>>, Option<ErrorCode>) {
        let statement = connection.prepare(sql).unwrap();
        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        let mut output = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => output.push(row.values().to_vec()),
                Ok(None) => return (output, None),
                Err(error) => return (output, Some(error.code())),
            }
        }
    }

    for (projection, predicate, emitted) in [
        ("ID", "A = 999 AND 1 / 0 = 0", vec![]),
        ("ID", "A = 3 AND 1 / D = 1", vec![vec![Value::Int64(1)]]),
        (
            "ID, 1 / D",
            "A >= 1",
            vec![vec![Value::Int64(1), Value::Int64(1)]],
        ),
        ("SUM(F)", "A >= 1", vec![]),
        ("AVG(F)", "A >= 1", vec![]),
    ] {
        let reference = outcome(
            &mut connection,
            &format!("SELECT {projection} FROM T WHERE ({predicate}) OR FALSE"),
        );
        assert_eq!(reference, (emitted, Some(ErrorCode::Type)));
        assert_eq!(
            outcome(
                &mut connection,
                &format!("SELECT {projection} FROM T WHERE {predicate}"),
            ),
            reference,
            "{projection} WHERE {predicate}",
        );
    }
    execute_sql(&mut connection, "DELETE FROM T");
    assert_eq!(
        outcome(
            &mut connection,
            "SELECT ID FROM T WHERE A = 999 AND 1 / 0 = 0",
        ),
        (vec![], None),
        "an empty table must not eagerly evaluate a row predicate",
    );
}

fn fixture_rows() -> Vec<[ParameterValue; 10]> {
    let date = |day| OracleDate::new(2026, 1, day, 12, 0, 0).unwrap();
    let stamp = |day| OracleTimestamp::new(date(day), u32::from(day) * 1_000, None).unwrap();
    vec![
        [
            Value::Int64(1).into(),
            Value::Null.into(),
            Value::Text("null\0first".into()).into(),
            Value::Null.into(),
            Value::Null.into(),
            Value::Null.into(),
            Value::Null.into(),
            Value::Null.into(),
            Value::Null.into(),
            Value::Null.into(),
        ],
        [
            Value::Int64(2).into(),
            Value::Int64(-1).into(),
            Value::Text("alpha\0negative".into()).into(),
            Value::Decimal(decimal(true, 2, 150)).into(),
            Value::Float64(-1.5).into(),
            Value::Blob(vec![0, 0xff]).into(),
            Value::Date(date(2)).into(),
            Value::Timestamp(stamp(2)).into(),
            Value::Bool(false).into(),
            Value::Int64(20).into(),
        ],
        [
            Value::Int64(3).into(),
            Value::Int64(0).into(),
            Value::Text("alpha".into()).into(),
            Value::Decimal(decimal(false, 0, 0)).into(),
            Value::Float64(0.0).into(),
            Value::Blob(vec![0]).into(),
            Value::Date(date(3)).into(),
            Value::Timestamp(stamp(3)).into(),
            Value::Bool(true).into(),
            Value::Int64(30).into(),
        ],
        [
            Value::Int64(4).into(),
            Value::Int64(1).into(),
            Value::Text("beta".into()).into(),
            Value::Decimal(decimal(false, 2, 125)).into(),
            Value::Float64(1.25).into(),
            Value::Blob(vec![0, 1, 2]).into(),
            Value::Date(date(4)).into(),
            Value::Timestamp(stamp(4)).into(),
            Value::Bool(false).into(),
            Value::Int64(40).into(),
        ],
        [
            Value::Int64(5).into(),
            Value::Int64(1).into(),
            Value::Text("beta\0tail".into()).into(),
            Value::Decimal(decimal(false, 2, 150)).into(),
            Value::Float64(1.5).into(),
            Value::Blob(vec![0, 1, 2, 3]).into(),
            Value::Date(date(5)).into(),
            Value::Timestamp(stamp(5)).into(),
            Value::Bool(true).into(),
            Value::Int64(50).into(),
        ],
        [
            Value::Int64(6).into(),
            Value::Int64(2).into(),
            Value::Text("zeta".into()).into(),
            Value::Decimal(decimal(false, 0, 2)).into(),
            Value::Float64(2.0).into(),
            Value::Blob(vec![0xff, 0]).into(),
            Value::Date(date(6)).into(),
            Value::Timestamp(stamp(6)).into(),
            Value::Bool(false).into(),
            Value::Null.into(),
        ],
        [
            Value::Int64(7).into(),
            Value::Null.into(),
            Value::Text("null\0second".into()).into(),
            Value::Decimal(decimal(false, 0, 7)).into(),
            Value::Float64(7.0).into(),
            Value::Blob(vec![0, 0, 7]).into(),
            Value::Date(date(7)).into(),
            Value::Timestamp(stamp(7)).into(),
            Value::Bool(true).into(),
            Value::Null.into(),
        ],
        [
            Value::Int64(8).into(),
            Value::Int64(1).into(),
            Value::Null.into(),
            Value::Decimal(decimal(false, 0, 8)).into(),
            Value::Float64(8.0).into(),
            Value::Blob(vec![8]).into(),
            Value::Date(date(8)).into(),
            Value::Timestamp(stamp(8)).into(),
            Value::Bool(false).into(),
            Value::Int64(800).into(),
        ],
    ]
}

fn decimal(negative: bool, scale: i8, magnitude: u128) -> Decimal {
    Decimal::new(negative, scale, magnitude).expect("fixture decimal")
}

fn ids(values: &[i64]) -> Vec<Vec<Value>> {
    values.iter().map(|id| vec![Value::Int64(*id)]).collect()
}

fn collect_outcome(
    connection: &mut ora_sqllite::Connection,
    sql: &str,
    parameters: &[ParameterValue],
) -> Result<Vec<Vec<Value>>, ErrorCode> {
    let statement = connection.prepare(sql).map_err(|error| error.code())?;
    let mut rows = connection
        .query(&statement, Parameters::positional(parameters))
        .map_err(|error| error.code())?;
    let mut values = Vec::new();
    loop {
        match rows.next().map_err(|error| error.code())? {
            Some(row) => values.push(row.values().to_vec()),
            None => return Ok(values),
        }
    }
}

fn assert_query_ids(
    connection: &mut ora_sqllite::Connection,
    sql: &str,
    parameters: &[ParameterValue],
    expected: &[i64],
) {
    let statement = connection.prepare(sql).expect("prepare indexed query");
    assert_eq!(
        collect_params(connection, &statement, parameters),
        ids(expected),
        "{sql}"
    );
}

#[test]
fn indexed_point_range_null_and_residual_queries_match_expected_rows() {
    // Production breaks caught: wrong index fence, NULL treatment, reversed
    // comparison, or loss of the residual Filter returns a hand-derived ID set.
    let fixture = IndexedFixture::new("phase4c2d-public-results");
    let mut connection = fixture.connect();
    let date = OracleDate::new(2026, 1, 5, 12, 0, 0).unwrap();
    let timestamp = OracleTimestamp::new(date, 5_000, None).unwrap();
    let typed_null = ParameterValue::typed_null(DataType::Int64).unwrap();
    for (sql, parameters, expected) in [
        (
            "SELECT ID FROM T WHERE A = :1 ORDER BY ID",
            vec![Value::Int64(1).into()],
            vec![4, 5, 8],
        ),
        (
            "SELECT ID FROM T WHERE A = :1 AND B >= :2 AND B < :3 ORDER BY ID",
            vec![
                Value::Int64(1).into(),
                Value::Text("beta".into()).into(),
                Value::Text("z".into()).into(),
            ],
            vec![4, 5],
        ),
        (
            "SELECT ID FROM T WHERE A = 1 AND B > :1 AND B <= :2 ORDER BY ID",
            vec![
                Value::Text("beta".into()).into(),
                Value::Text("beta\0tail".into()).into(),
            ],
            vec![5],
        ),
        (
            "SELECT ID FROM T WHERE A BETWEEN :1 AND :2 ORDER BY ID",
            vec![Value::Int64(0).into(), Value::Int64(1).into()],
            vec![3, 4, 5, 8],
        ),
        (
            "SELECT ID FROM T WHERE A IS NULL ORDER BY ID",
            vec![],
            vec![1, 7],
        ),
        (
            "SELECT ID FROM T WHERE A = :1 ORDER BY ID",
            vec![typed_null],
            vec![],
        ),
        (
            "SELECT ID FROM T WHERE :1 < A ORDER BY ID",
            vec![Value::Int64(0).into()],
            vec![4, 5, 6, 8],
        ),
        (
            "SELECT ID FROM T WHERE C >= :1 AND C < :2 ORDER BY ID",
            vec![Value::Int64(1).into(), Value::Int64(2).into()],
            vec![4, 5],
        ),
        (
            "SELECT ID FROM T WHERE RAW = :1 ORDER BY ID",
            vec![Value::Blob(vec![0, 1, 2]).into()],
            vec![4],
        ),
        (
            "SELECT ID FROM T WHERE F >= :1 ORDER BY ID",
            vec![Value::Float64(1.5).into()],
            vec![5, 6, 7, 8],
        ),
        (
            "SELECT ID FROM T WHERE A = 1 AND B <> :1 ORDER BY ID",
            vec![Value::Text("beta".into()).into()],
            vec![5],
        ),
    ] {
        assert_query_ids(&mut connection, sql, &parameters, &expected);
    }
    assert_query_ids(
        &mut connection,
        "SELECT ID FROM T WHERE A = 1 AND B IS NULL ORDER BY ID",
        &[],
        &[8],
    );
    assert_query_ids(
        &mut connection,
        "SELECT ID FROM T WHERE D = :1 ORDER BY ID",
        &[Value::Date(date).into()],
        &[5],
    );
    assert_query_ids(
        &mut connection,
        "SELECT ID FROM T WHERE TS = :1 ORDER BY ID",
        &[Value::Timestamp(timestamp).into()],
        &[5],
    );
    assert_query_ids(
        &mut connection,
        "SELECT ID FROM T WHERE FLAG = :1 ORDER BY ID",
        &[Value::Bool(true).into()],
        &[3, 5, 7],
    );
}

#[test]
fn dedicated_scalar_indexes_preserve_float_date_timestamp_and_bool_results() {
    // Production breaks caught: a scalar-specific encoded index key or range
    // returns a different public row set than the typed predicate requires.
    let fixture = ScalarIndexedFixture::new("phase4c2d-public-scalar-indexes");
    let mut connection = fixture.connect();
    let date = OracleDate::new(2026, 3, 2, 0, 0, 0).unwrap();
    let timestamp =
        OracleTimestamp::new(OracleDate::new(2026, 3, 3, 0, 0, 0).unwrap(), 30, None).unwrap();
    for (sql, parameters, expected) in [
        (
            "SELECT ID FROM S WHERE F >= :1 ORDER BY ID",
            vec![Value::Float64(0.0).into()],
            vec![2, 3],
        ),
        (
            "SELECT ID FROM S WHERE D = :1 ORDER BY ID",
            vec![Value::Date(date).into()],
            vec![2],
        ),
        (
            "SELECT ID FROM S WHERE TS = :1 ORDER BY ID",
            vec![Value::Timestamp(timestamp).into()],
            vec![3],
        ),
        (
            "SELECT ID FROM S WHERE FLAG = :1 ORDER BY ID",
            vec![Value::Bool(false).into()],
            vec![1, 3],
        ),
    ] {
        assert_query_ids(&mut connection, sql, &parameters, &expected);
    }
}

#[test]
fn unsupported_predicates_match_an_explicit_index_free_database() {
    // Production break caught: allowing a non-search predicate to change the
    // result or error only when an explicit index is present.
    let indexed = IndexedFixture::new("phase4c2d-public-fallback-indexed");
    let plain = IndexedFixture::new("phase4c2d-public-fallback-plain");
    let mut indexed_connection = indexed.connect();
    let mut plain_connection = plain.connect();
    for sql in [
        "DROP INDEX IX_T_A_B",
        "DROP INDEX IX_T_C",
        "DROP INDEX IX_T_RAW",
    ] {
        execute_sql(&mut plain_connection, sql);
    }
    for (sql, parameters) in [
        ("SELECT ID FROM T WHERE A = 1 OR A = 2 ORDER BY ID", vec![]),
        ("SELECT ID FROM T WHERE NOT (A = 1) ORDER BY ID", vec![]),
        ("SELECT ID FROM T WHERE A != 1 ORDER BY ID", vec![]),
        ("SELECT ID FROM T WHERE A IN (0, 1) ORDER BY ID", vec![]),
        ("SELECT ID FROM T WHERE B LIKE 'beta%' ORDER BY ID", vec![]),
        ("SELECT ID FROM T WHERE A + 1 = 2 ORDER BY ID", vec![]),
        ("SELECT ID FROM T WHERE B = 'beta' ORDER BY ID", vec![]),
        (
            "SELECT ID FROM T WHERE RAW > :1 ORDER BY ID",
            vec![Value::Blob(vec![0]).into()],
        ),
        (
            "SELECT T.ID FROM T JOIN J ON T.ID = J.ID WHERE T.A = 1 ORDER BY T.ID",
            vec![],
        ),
        ("SELECT ID FROM T WHERE A = U ORDER BY ID", vec![]),
    ] {
        assert_eq!(
            collect_outcome(&mut indexed_connection, sql, &parameters),
            collect_outcome(&mut plain_connection, sql, &parameters),
            "{sql}"
        );
    }
}

#[test]
fn public_bind_resource_schema_and_corruption_errors_remain_stable() {
    // Production breaks caught: query validation changing its public error
    // class, or an oversized decoded indexed row escaping ResourceLimit.
    let fixture = IndexedFixture::new("phase4c2d-public-errors");
    let mut connection = fixture.connect();
    let point = connection
        .prepare("SELECT ID FROM T WHERE A = :1")
        .expect("prepare point query");
    assert_eq!(
        connection
            .query(&point, Parameters::none())
            .expect_err("missing bind must be rejected")
            .code(),
        ErrorCode::Bind
    );
    let wrong = [Value::Text("wrong type".into()).into()];
    assert_eq!(
        connection
            .query(&point, Parameters::positional(&wrong))
            .expect_err("wrong bind type must be rejected")
            .code(),
        ErrorCode::Type
    );
    let stale = connection
        .prepare("SELECT ID FROM T WHERE A = :1")
        .expect("prepare stale point query");
    execute_sql(&mut connection, "DROP INDEX IX_T_A_B");
    assert_eq!(
        connection
            .query(&stale, Parameters::positional(&[Value::Int64(1).into()]))
            .expect_err("dropped selected index invalidates statement")
            .code(),
        ErrorCode::SchemaChanged
    );

    let (files, database) = common::new_database("phase4c2d-public-row-limit");
    let mut setup = database.connect().expect("connect row limit fixture");
    execute_sql(
        &mut setup,
        "CREATE TABLE BIG (ID INT64 PRIMARY KEY, A INT64, PAYLOAD TEXT)",
    );
    execute_sql(&mut setup, "CREATE INDEX IX_BIG_A ON BIG(A)");
    let insert = setup
        .prepare("INSERT INTO BIG VALUES (:1, :2, :3)")
        .unwrap();
    let values = [
        Value::Int64(1).into(),
        Value::Int64(1).into(),
        Value::Text("x".repeat(20_000)).into(),
    ];
    setup
        .execute(&insert, Parameters::positional(&values))
        .unwrap();
    drop(setup);
    drop(database);
    let limited = Database::open(
        files.main(),
        Options::default().with_max_query_memory_bytes(256),
    )
    .expect("open bounded row fixture");
    let mut limited_connection = limited.connect().expect("connect bounded row fixture");
    let query = limited_connection
        .prepare("SELECT PAYLOAD FROM BIG WHERE A = :1")
        .unwrap();
    let bound = [Value::Int64(1).into()];
    let mut rows = limited_connection
        .query(&query, Parameters::positional(&bound))
        .expect("start bounded query");
    assert_eq!(
        rows.next()
            .expect_err("decoded row over memory budget must fail")
            .code(),
        ErrorCode::ResourceLimit
    );
}

#[test]
fn selected_index_and_table_descendant_corruption_return_corruption() {
    // Production break caught: IndexScan must propagate a corrupt selected
    // index/table descendant before it can return a far-side matching row.
    let reference = corruption_fixture("phase4c2d-public-corrupt-reference");
    let page_count = physical_page_count(reference.main());
    let far_value = Value::Text(split_text("corrupt", 16));
    let mut index_page = None;
    let mut table_page = None;

    let undamaged = copied_database(&reference, "candidate-undamaged.db");
    let database = Database::open(&undamaged, Options::default())
        .expect("open undamaged copied main and WAL pair");
    let mut connection = database
        .connect()
        .expect("connect undamaged copied fixture");
    assert_eq!(
        collect_outcome(
            &mut connection,
            "SELECT ID FROM C WHERE A = :1",
            &[far_value.clone().into()]
        ),
        Ok(ids(&[16])),
        "undamaged equality query must establish the IndexScan baseline"
    );
    assert_eq!(
        collect_outcome(
            &mut connection,
            "SELECT ID FROM C WHERE A = :1 OR FALSE",
            &[far_value.clone().into()]
        ),
        Ok(ids(&[16])),
        "undamaged OR FALSE query must establish the TableScan baseline"
    );
    drop(connection);
    drop(database);
    remove_corruption_candidate(&undamaged);

    for page in 1..page_count {
        let candidate = copied_database(&reference, &format!("candidate-page-{page}.db"));
        flip_page_data_byte(&candidate, page);
        let outcome = match Database::open(&candidate, Options::default()) {
            Ok(database) => {
                let mut connection = database.connect().expect("connect corrupt fixture");
                let indexed = collect_outcome(
                    &mut connection,
                    "SELECT ID FROM C WHERE A = :1",
                    &[far_value.clone().into()],
                );
                let table_scan = collect_outcome(
                    &mut connection,
                    "SELECT ID FROM C WHERE A = :1 OR FALSE",
                    &[far_value.clone().into()],
                );
                drop(connection);
                drop(database);
                Ok((indexed, table_scan))
            }
            Err(error) => Err(error.code()),
        };
        remove_corruption_candidate(&candidate);
        let Ok((indexed, table_scan)) = outcome else {
            assert_eq!(outcome, Err(ErrorCode::Corruption), "page {page}");
            continue;
        };
        match (indexed, table_scan) {
            (Err(ErrorCode::Corruption), Ok(rows)) if rows == ids(&[16]) => {
                index_page.get_or_insert(page);
            }
            (Err(ErrorCode::Corruption), Err(ErrorCode::Corruption)) => {
                table_page.get_or_insert(page);
            }
            _ => {}
        }
    }
    assert!(
        index_page.is_some(),
        "a corrupt index descendant must survive open and fail only IndexScan"
    );
    assert!(
        table_page.is_some(),
        "a corrupt table descendant must survive open and fail both IndexScan and TableScan"
    );
}

fn corruption_fixture(label: &str) -> common::TempPaths {
    let (files, database) = common::new_database(label);
    let mut setup = database.connect().expect("connect corruption fixture");
    execute_sql(&mut setup, "CREATE TABLE C (ID INT64 PRIMARY KEY, A TEXT)");
    let mut write = setup.begin_write().expect("begin corruption setup writer");
    let insert = write.prepare("INSERT INTO C VALUES (:1, :2)").unwrap();
    for id in 1..=16 {
        let values = [
            Value::Int64(id).into(),
            Value::Text(split_text("corrupt", id)).into(),
        ];
        write
            .execute(&insert, Parameters::positional(&values))
            .unwrap();
    }
    write.commit().unwrap();
    execute_sql(&mut setup, "CREATE INDEX IX_C_A ON C(A)");
    drop(setup);
    drop(database);
    common::checkpoint(&files);
    files
}

fn physical_page_count(path: &std::path::Path) -> u64 {
    let length = fs::metadata(path)
        .expect("read corruption fixture metadata")
        .len();
    assert_eq!(length % 4_096, 0, "default database uses complete pages");
    length / 4_096
}

fn flip_page_data_byte(path: &std::path::Path, page: u64) {
    let offset = page
        .checked_mul(4_096)
        .and_then(|offset| offset.checked_add(80))
        .expect("page corruption offset fits");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open corruption fixture for byte flip");
    file.seek(SeekFrom::Start(offset))
        .expect("seek final corruption byte");
    let mut byte = [0];
    std::io::Read::read_exact(&mut file, &mut byte).expect("read corruption byte");
    file.seek(SeekFrom::Start(offset))
        .expect("rewind corruption byte");
    file.write_all(&[byte[0] ^ 0x01])
        .expect("write corruption byte");
    file.sync_all().expect("sync corruption byte");
}

fn copied_database(files: &common::TempPaths, name: &str) -> std::path::PathBuf {
    let candidate = files.main().with_file_name(name);
    fs::copy(files.main(), &candidate).expect("copy owned corruption fixture");
    if files.wal().is_file() {
        fs::copy(files.wal(), corruption_wal_path(&candidate)).expect("copy owned corruption WAL");
    }
    candidate
}

fn remove_corruption_candidate(path: &std::path::Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(corruption_wal_path(path));
}

fn corruption_wal_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    std::path::PathBuf::from(wal)
}

#[test]
fn prepared_index_scan_reopens_current_roots_but_rejects_index_ddl() {
    // Production breaks caught: retaining a root page in Statement, or
    // rebinding a stale selected-index identity after DDL.
    let fixture = IndexedFixture::new("phase4c2d-public-prepared-roots");
    let mut connection = fixture.connect();
    let statement = connection
        .prepare("SELECT ID FROM T WHERE A = :1 ORDER BY ID")
        .expect("prepare indexed point query");
    let mut transaction = connection.begin_write().expect("begin split writer");
    let insert = transaction
        .prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5, :6, :7, :8, :9, :10)")
        .unwrap();
    let date = OracleDate::new(2026, 2, 1, 0, 0, 0).unwrap();
    let timestamp = OracleTimestamp::new(date, 0, None).unwrap();
    // Sixteen 720-byte second-key values overflow both the table's row leaves
    // and IX_T_A_B's physical key leaves at the default 4096-byte page size.
    // The far-side point probe below must therefore reopen descendant roots.
    for id in 9_000..9_016 {
        let values = [
            Value::Int64(id).into(),
            Value::Int64(id).into(),
            Value::Text(split_text("roots", id)).into(),
            Value::Decimal(decimal(false, 0, id as u128)).into(),
            Value::Float64(id as f64).into(),
            Value::Blob(id.to_le_bytes().to_vec()).into(),
            Value::Date(date).into(),
            Value::Timestamp(timestamp).into(),
            Value::Bool(id % 2 == 0).into(),
            Value::Int64(id).into(),
        ];
        transaction
            .execute(&insert, Parameters::positional(&values))
            .unwrap();
    }
    transaction.commit().unwrap();
    assert_eq!(
        collect_params(&mut connection, &statement, &[Value::Int64(9_015).into()]),
        ids(&[9_015])
    );
    execute_sql(&mut connection, "DROP INDEX IX_T_A_B");
    assert_eq!(
        connection
            .query(
                &statement,
                Parameters::positional(&[Value::Int64(9_015).into()])
            )
            .expect_err("drop must invalidate selected-index statement")
            .code(),
        ErrorCode::SchemaChanged
    );
}

#[test]
fn prepared_index_identity_never_rebinds_after_drop_and_recreate() {
    // Production break caught: looking up the selected index by its old name
    // lets a retained query silently target a newly created index identity.
    let fixture = IndexedFixture::new("phase4c2d-public-recreated-index");
    let mut connection = fixture.connect();
    let statement = connection
        .prepare("SELECT ID FROM T WHERE A = :1")
        .expect("prepare selected-index query");
    execute_sql(&mut connection, "DROP INDEX IX_T_A_B");
    execute_sql(&mut connection, "CREATE INDEX IX_T_A_B ON T(A, B)");
    assert_eq!(
        connection
            .query(
                &statement,
                Parameters::positional(&[Value::Int64(1).into()])
            )
            .expect_err("recreated name must not rebind stale index identity")
            .code(),
        ErrorCode::SchemaChanged
    );
}

#[test]
fn write_batches_and_read_snapshots_keep_their_public_indexed_views() {
    // Production breaks caught: IndexScan reading committed roots instead of a
    // write batch, or an old reader observing a later table/index root pair.
    let fixture = IndexedFixture::new("phase4c2d-public-transaction-snapshot");
    let mut connection = fixture.connect();
    let mut write = connection.begin_write().expect("begin indexed write batch");
    let insert = write
        .prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5, :6, :7, :8, :9, :10)")
        .unwrap();
    let inserted = [
        Value::Int64(80).into(),
        Value::Int64(8).into(),
        Value::Text("batch".into()).into(),
        Value::Null.into(),
        Value::Float64(8.0).into(),
        Value::Blob(vec![8]).into(),
        Value::Null.into(),
        Value::Null.into(),
        Value::Bool(true).into(),
        Value::Int64(80).into(),
    ];
    write
        .execute(&insert, Parameters::positional(&inserted))
        .unwrap();
    let update = write.prepare("UPDATE T SET A = 9 WHERE ID = 4").unwrap();
    write.execute(&update, Parameters::none()).unwrap();
    let delete = write.prepare("DELETE FROM T WHERE ID = 5").unwrap();
    write.execute(&delete, Parameters::none()).unwrap();
    let query = write
        .prepare("SELECT ID FROM T WHERE A >= :1 ORDER BY ID")
        .unwrap();
    let rows = {
        let parameters = [Value::Int64(8).into()];
        let mut rows = write
            .query(&query, Parameters::positional(&parameters))
            .unwrap();
        let mut values = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            values.push(row.values().to_vec());
        }
        values
    };
    assert_eq!(rows, ids(&[4, 80]));
    let affected_bucket = write
        .prepare("SELECT ID FROM T WHERE A = :1 ORDER BY ID")
        .unwrap();
    assert_eq!(
        {
            let parameters = [Value::Int64(1).into()];
            let mut rows = write
                .query(&affected_bucket, Parameters::positional(&parameters))
                .unwrap();
            let mut values = Vec::new();
            while let Some(row) = rows.next().unwrap() {
                values.push(row.values().to_vec());
            }
            values
        },
        ids(&[8]),
        "the indexed A=1 bucket must exclude both updated ID 4 and deleted ID 5"
    );
    write.rollback().unwrap();
    assert_eq!(
        collect(
            &mut connection,
            "SELECT ID FROM T WHERE ID IN (4, 5, 80) ORDER BY ID"
        ),
        ids(&[4, 5])
    );
    assert_eq!(
        collect(&mut connection, "SELECT ID FROM T WHERE A = 1 ORDER BY ID"),
        ids(&[4, 5, 8]),
        "rollback must restore both the deleted row and the updated index entry"
    );

    let mut old_connection = fixture.connect();
    let mut old = old_connection
        .begin_read()
        .expect("begin fixed old snapshot");
    let before = collect_read(&mut old, "SELECT ID FROM T WHERE A = 1 ORDER BY ID");
    let mut writer = fixture.connect();
    let mut root_writer = writer.begin_write().expect("begin snapshot root writer");
    let update = root_writer
        .prepare("UPDATE T SET A = 11 WHERE ID = 4")
        .unwrap();
    root_writer.execute(&update, Parameters::none()).unwrap();
    let insert = root_writer
        .prepare("INSERT INTO T VALUES (:1, 11, :2, NULL, NULL, X'01', NULL, NULL, TRUE, :3)")
        .unwrap();
    // The long B values are overflow-backed table fields. Sixteen rows occupy
    // 4054 bytes and can remain one leaf; 32 rows exceed that leaf even when
    // each payload is only a small overflow reference. Their full index keys
    // still split IX_T_A_B much sooner.
    for id in 10_000..10_032 {
        let values = [
            Value::Int64(id).into(),
            Value::Text(split_text("snapshot", id)).into(),
            Value::Int64(id).into(),
        ];
        root_writer
            .execute(&insert, Parameters::positional(&values))
            .unwrap();
    }
    root_writer.commit().expect("commit snapshot root writer");
    assert_eq!(before, ids(&[4, 5, 8]));
    assert_eq!(
        collect_read(&mut old, "SELECT ID FROM T WHERE A = 1 ORDER BY ID"),
        ids(&[4, 5, 8]),
        "held read transaction must retain its old complete indexed view"
    );
    let mut fresh = fixture.connect();
    assert_eq!(
        collect(&mut fresh, "SELECT ID FROM T WHERE A = 11 ORDER BY ID"),
        ids(&std::iter::once(4).chain(10_000..10_032).collect::<Vec<_>>())
    );
    assert_query_ids(
        &mut fresh,
        "SELECT ID FROM T WHERE A = 11 AND B = :1 ORDER BY ID",
        &[Value::Text(split_text("snapshot", 10_031)).into()],
        &[10_031],
    );
}

fn split_text(prefix: &str, id: i64) -> String {
    format!("{prefix}-{id:04}-{}", "x".repeat(704))
}

fn collect_read(transaction: &mut ora_sqllite::ReadTransaction<'_>, sql: &str) -> Vec<Vec<Value>> {
    let statement = transaction
        .prepare(sql)
        .expect("prepare fixed snapshot query");
    let mut rows = transaction
        .query(&statement, Parameters::none())
        .expect("run fixed snapshot query");
    let mut values = Vec::new();
    while let Some(row) = rows.next().expect("advance fixed snapshot rows") {
        values.push(row.values().to_vec());
    }
    values
}
