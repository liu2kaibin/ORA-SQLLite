#![allow(dead_code)]

use std::{
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use ora_sqllite::{Connection, Database, ExecutionResult, Options, Parameters, Statement, Value};

pub(crate) mod format2;

#[allow(unused_imports)]
pub use format2::{
    assert_index_model, corrupt_first_index_separator_with_valid_checksum, flip_physical_page_byte,
    verify_index_model,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub struct TestFiles {
    directory: PathBuf,
    main: PathBuf,
    wal: PathBuf,
}

impl TestFiles {
    fn new(label: &str) -> Self {
        let label: String = label
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "ora-sqllite-phase4c-api-{}-{label}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    let main = directory.join("database.db");
                    let mut wal = OsString::from(main.as_os_str());
                    wal.push("-wal");
                    return Self {
                        directory,
                        main,
                        wal: PathBuf::from(wal),
                    };
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated API test directory: {error}"),
            }
        }
    }

    pub fn main(&self) -> &Path {
        &self.main
    }

    pub fn wal(&self) -> &Path {
        &self.wal
    }
}

impl Drop for TestFiles {
    fn drop(&mut self) {
        for path in [&self.main, &self.wal] {
            if let Err(error) = fs::remove_file(path) {
                if error.kind() != ErrorKind::NotFound {
                    // Best-effort cleanup is deliberately limited to owned files.
                }
            }
        }
        if let Err(error) = fs::remove_dir(&self.directory) {
            if !matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
            ) {
                // Never broaden cleanup to a recursive directory removal.
            }
        }
    }
}

pub fn new_database(label: &str) -> (TestFiles, Database) {
    let files = TestFiles::new(label);
    let database = Database::create(files.main(), Options::default()).unwrap();
    (files, database)
}

pub fn reopen(files: &TestFiles) -> Database {
    Database::open(files.main(), Options::default()).unwrap()
}

pub fn execute_sql(connection: &mut Connection, sql: &str) -> ExecutionResult {
    let statement = connection.prepare(sql).unwrap();
    connection.execute(&statement, Parameters::none()).unwrap()
}

pub fn execute_statement(
    connection: &mut Connection,
    statement: &Statement,
    parameters: Parameters<'_>,
) -> ExecutionResult {
    connection.execute(statement, parameters).unwrap()
}

pub fn collect(connection: &mut Connection, sql: &str) -> Vec<Vec<Value>> {
    let statement = connection.prepare(sql).unwrap();
    collect_statement(connection, &statement, Parameters::none())
}

pub fn collect_statement(
    connection: &mut Connection,
    statement: &Statement,
    parameters: Parameters<'_>,
) -> Vec<Vec<Value>> {
    let mut rows = connection.query(statement, parameters).unwrap();
    let mut values = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        values.push(row.values().to_vec());
    }
    values
}

pub fn insert_scale_row(connection: &mut Connection, statement: &Statement, id: i64, group: i64) {
    // This is deliberately a public-API insert so the scale gate exercises the
    // same executor and DML-maintained indexes as callers do. Reusing the
    // prepared statement keeps the gate about storage correctness, not parsing.
    let values = [
        Value::Int64(id).into(),
        Value::Int64(group).into(),
        Value::Text(format!("row-{id}")).into(),
    ];
    connection
        .execute(statement, Parameters::positional(&values))
        .unwrap();
}

pub fn checkpoint_accepted_state(files: &TestFiles) {
    // A commit is WAL-first. Reopen through the public API and acquire a writer
    // with a one-byte checkpoint threshold so the already accepted WAL image is
    // materialized before the independent verifier reads only the main file.
    let database = Database::open(
        files.main(),
        Options::default().with_checkpoint_wal_bytes(1),
    )
    .unwrap();
    let mut connection = database.connect().unwrap();
    connection.begin_write().unwrap().rollback().unwrap();
    drop(connection);
    drop(database);
}

pub struct ScaleDatabase {
    // Drop the last database handle before attempting owned-file cleanup.
    database: Option<Database>,
    files: TestFiles,
    label: String,
    rows: i64,
    payload_bytes: usize,
    mutated: bool,
    metrics: ScaleMetrics,
    started: Instant,
}

#[derive(Default)]
struct ScaleMetrics {
    import: Duration,
    point: Duration,
    range: Duration,
    update: Duration,
    delete: Duration,
    checkpoint_reopen: Duration,
    point_queries: u64,
    point_rows: u64,
    range_queries: u64,
    range_rows: u64,
    updated_rows: u64,
    deleted_rows: u64,
    reopened_rows: u64,
    reopen_digest: u64,
    wal_peak_bytes: u64,
}

pub fn validate_large_scale_gate(
    opt_in: Option<&str>,
    available_bytes: u64,
) -> Result<(), &'static str> {
    if opt_in != Some("1") {
        return Err("requires explicit ORA_SQLLITE_LARGE_SCALE=1");
    }
    if available_bytes < 30_000_000_000 {
        return Err("requires at least 30 GB available on the temporary-file volume");
    }
    Ok(())
}

pub fn require_large_scale_gate() {
    let opt_in = std::env::var("ORA_SQLLITE_LARGE_SCALE").ok();
    // Reject missing opt-in even before probing the filesystem. Both checks
    // precede scale_database(), so refusal cannot create a database.
    assert_eq!(opt_in.as_deref(), Some("1"), "large acceptance is opt-in");
    let available =
        fs4::available_space(std::env::temp_dir()).expect("probe temporary volume capacity");
    validate_large_scale_gate(opt_in.as_deref(), available)
        .expect("large acceptance capacity gate");
    eprintln!("phase4c2d_large_gate available_bytes={available} required_bytes=30000000000");
}

fn scale_options() -> Options {
    // Payload alone exceeds the default 10-GiB limit after page/index overhead.
    // Bounded batches and a 64-MiB checkpoint threshold keep WAL growth bounded.
    Options::default().with_max_database_bytes(25_000_000_000)
}

impl ScaleDatabase {
    pub fn connect(&self) -> Connection {
        self.database.as_ref().unwrap().connect().unwrap()
    }

    pub fn main_file_bytes(&self) -> u64 {
        fs::metadata(self.files.main())
            .expect("stat owned scale database")
            .len()
    }

    pub fn wal_peak_bytes(&self) -> u64 {
        self.metrics.wal_peak_bytes
    }

    fn sample_file_sizes(&mut self) {
        let wal = fs::metadata(self.files.wal())
            .expect("stat owned scale WAL")
            .len();
        self.metrics.wal_peak_bytes = self.metrics.wal_peak_bytes.max(wal);
        self.main_file_bytes()
            .checked_add(wal)
            .expect("combined file bytes fit u64");
    }

    pub fn checkpoint(&mut self) {
        self.sample_file_sizes();
        drop(self.database.take());
        let database = Database::open(
            self.files.main(),
            scale_options().with_checkpoint_wal_bytes(1),
        )
        .expect("open for scale checkpoint");
        let mut connection = database.connect().unwrap();
        connection.begin_write().unwrap().rollback().unwrap();
        drop(connection);
        drop(database);
        self.database = Some(
            Database::open(self.files.main(), scale_options()).expect("reopen scale database"),
        );
        self.sample_file_sizes();
    }

    pub fn print_machine_readable_metrics(&self) {
        let m = &self.metrics;
        // Stable names/units, one block per acceptance run. Durations are
        // observations, never pass/fail thresholds on a shared CI runner.
        println!("phase4c2d_scale_metrics_begin");
        println!("label={}", self.label);
        println!("input_rows={}", self.rows);
        println!("payload_bytes={}", self.payload_bytes);
        println!("point_queries={}", m.point_queries);
        println!("point_rows={}", m.point_rows);
        println!("range_queries={}", m.range_queries);
        println!("range_rows={}", m.range_rows);
        println!("updated_rows={}", m.updated_rows);
        println!("deleted_rows={}", m.deleted_rows);
        println!("reopened_rows={}", m.reopened_rows);
        println!("reopen_digest_fnv1a64={:016x}", m.reopen_digest);
        println!("main_file_bytes={}", self.main_file_bytes());
        println!("wal_peak_bytes={}", self.wal_peak_bytes());
        println!("wal_peak_sampling=after_each_commit_before_next_writer");
        for (name, duration) in [
            ("import", m.import),
            ("point_query", m.point),
            ("range_query", m.range),
            ("update", m.update),
            ("delete", m.delete),
            ("checkpoint_reopen", m.checkpoint_reopen),
            ("total", self.started.elapsed()),
        ] {
            println!("{name}_elapsed_ms={}", duration.as_millis());
        }
        println!("phase4c2d_scale_metrics_end");
    }
}

pub fn scale_database(label: &str) -> ScaleDatabase {
    let files = TestFiles::new(label);
    let database =
        Database::create(files.main(), scale_options()).expect("create owned scale database");
    ScaleDatabase {
        database: Some(database),
        files,
        label: label.to_owned(),
        rows: 0,
        payload_bytes: 0,
        mutated: false,
        metrics: ScaleMetrics::default(),
        started: Instant::now(),
    }
}

pub fn load_rows_and_indexes(db: &mut ScaleDatabase, rows: i64, payload_bytes: usize) {
    assert!(rows > 0, "scale import must contain rows");
    let started = Instant::now();
    db.rows = rows;
    db.payload_bytes = payload_bytes;
    let mut connection = db.connect();
    execute_sql(
        &mut connection,
        "CREATE TABLE SCALE_T (ID INT64, G INT64, SEQ INT64, PAYLOAD TEXT)",
    );
    db.sample_file_sizes();
    let insert = connection
        .prepare("INSERT INTO SCALE_T VALUES (:1, :2, :3, :4)")
        .unwrap();
    let payload = "x".repeat(payload_bytes);
    for first in (1..=rows).step_by(10_000) {
        let last = first.checked_add(9_999).unwrap().min(rows);
        let mut transaction = connection.begin_write().unwrap();
        for id in first..=last {
            let values = [
                Value::Int64(id).into(),
                Value::Int64(id % 100).into(),
                Value::Int64(id).into(),
                Value::Text(payload.clone()).into(),
            ];
            assert_eq!(
                transaction
                    .execute(&insert, Parameters::positional(&values))
                    .unwrap()
                    .rows_affected(),
                1
            );
        }
        transaction.commit().unwrap();
        db.sample_file_sizes();
        if rows <= 100_000 || last % 100_000 == 0 || last == rows {
            eprintln!("phase4c2d_import_progress committed_rows={last} target_rows={rows}");
        }
    }
    // Match the existing scale-import contract: bulk load through public DML,
    // then build populated indexes through public DDL. The mutation stage
    // independently checks DML maintenance of both populated index trees.
    execute_sql(
        &mut connection,
        "CREATE UNIQUE INDEX UX_SCALE_ID ON SCALE_T(ID)",
    );
    db.sample_file_sizes();
    execute_sql(
        &mut connection,
        "CREATE INDEX IX_SCALE_G_SEQ ON SCALE_T(G, SEQ)",
    );
    db.sample_file_sizes();
    drop(connection);
    // Include materialization in import cost; the large gate measures a real
    // >=10-GB main file, not payload arithmetic or sparse preallocation.
    db.checkpoint();
    db.metrics.import = started.elapsed();
    eprintln!(
        "phase4c2d_import rows={rows} payload_bytes={payload_bytes} main_file_bytes={}",
        db.main_file_bytes()
    );
}

pub fn assert_point_queries(db: &mut ScaleDatabase, ids: impl IntoIterator<Item = i64>) {
    let started = Instant::now();
    let mut connection = db.connect();
    let statement = connection
        .prepare("SELECT ID, G, SEQ, PAYLOAD FROM SCALE_T WHERE ID = :1")
        .unwrap();
    for id in ids {
        let values = [Value::Int64(id).into()];
        let actual =
            collect_statement(&mut connection, &statement, Parameters::positional(&values));
        let expected = if id < 1 || id > db.rows || (db.mutated && id > db.rows - 100) {
            vec![]
        } else {
            let moved = db.mutated && id <= 100;
            vec![vec![
                Value::Int64(id),
                Value::Int64(id % 100 + if moved { 1_000 } else { 0 }),
                Value::Int64(id + if moved { db.rows } else { 0 }),
                Value::Text("x".repeat(db.payload_bytes)),
            ]]
        };
        assert_eq!(actual, expected, "indexed point id={id}");
        db.metrics.point_queries = db.metrics.point_queries.checked_add(1).unwrap();
        db.metrics.point_rows = db
            .metrics
            .point_rows
            .checked_add(u64::try_from(actual.len()).unwrap())
            .unwrap();
    }
    db.metrics.point += started.elapsed();
}

pub fn assert_composite_ranges(db: &mut ScaleDatabase, rows: i64) {
    assert_eq!(rows, db.rows);
    assert!(
        rows >= 1_000 && rows % 200 == 0,
        "range fixture needs disjoint hundred-row bands"
    );
    let started = Instant::now();
    let mut connection = db.connect();
    let statement = connection
        .prepare("SELECT ID FROM SCALE_T WHERE G = :1 AND SEQ >= :2 AND SEQ < :3 ORDER BY ID")
        .unwrap();
    // Hand-derived G=42 fixture members, including both fences, moved old
    // keys, and deleted tail keys. Expected lists never use the query engine.
    let cases = [
        (
            42,
            42,
            242,
            if db.mutated { vec![142] } else { vec![42, 142] },
        ),
        (
            42,
            rows / 2,
            rows / 2 + 300,
            vec![rows / 2 + 42, rows / 2 + 142, rows / 2 + 242],
        ),
        (
            42,
            rows - 299,
            rows + 1,
            if db.mutated {
                vec![rows - 258, rows - 158]
            } else {
                vec![rows - 258, rows - 158, rows - 58]
            },
        ),
        (
            1_042,
            rows + 1,
            rows + 101,
            if db.mutated { vec![42] } else { vec![] },
        ),
    ];
    for (group, lower, upper, expected) in cases {
        let values = [
            Value::Int64(group).into(),
            Value::Int64(lower).into(),
            Value::Int64(upper).into(),
        ];
        let actual =
            collect_statement(&mut connection, &statement, Parameters::positional(&values));
        let expected: Vec<_> = expected
            .into_iter()
            .map(|id| vec![Value::Int64(id)])
            .collect();
        assert_eq!(
            actual, expected,
            "composite group={group} range=[{lower},{upper})"
        );
        db.metrics.range_queries = db.metrics.range_queries.checked_add(1).unwrap();
        db.metrics.range_rows = db
            .metrics
            .range_rows
            .checked_add(u64::try_from(actual.len()).unwrap())
            .unwrap();
    }
    db.metrics.range += started.elapsed();
}

pub fn mutate_indexed_rows(db: &mut ScaleDatabase, rows: i64) {
    assert_eq!(rows, db.rows);
    assert!(!db.mutated);
    let mut connection = db.connect();
    let started = Instant::now();
    db.metrics.updated_rows = execute_sql(
        &mut connection,
        &format!("UPDATE SCALE_T SET G = G + 1000, SEQ = SEQ + {rows} WHERE ID <= 100"),
    )
    .rows_affected();
    assert_eq!(db.metrics.updated_rows, 100);
    db.metrics.update = started.elapsed();
    db.sample_file_sizes();
    let started = Instant::now();
    db.metrics.deleted_rows = execute_sql(
        &mut connection,
        &format!("DELETE FROM SCALE_T WHERE ID > {}", rows - 100),
    )
    .rows_affected();
    assert_eq!(db.metrics.deleted_rows, 100);
    db.metrics.delete = started.elapsed();
    db.sample_file_sizes();
    db.mutated = true;
    assert_point_queries(
        db,
        [
            0,
            1,
            100,
            101,
            rows / 2,
            rows - 100,
            rows - 99,
            rows,
            rows + 1,
        ],
    );
    assert_composite_ranges(db, rows);
}

fn full_scale_witness(db: &ScaleDatabase) -> (u64, u64) {
    let mut connection = db.connect();
    // Stream in ROWID order: the fixture inserts increasing IDs and never
    // changes ID. No result materialization or 10-million-row sorting needed.
    let statement = connection
        .prepare("SELECT ID, G, SEQ, PAYLOAD FROM SCALE_T")
        .unwrap();
    let mut result = connection.query(&statement, Parameters::none()).unwrap();
    let mut count = 0_u64;
    let mut digest = 0xcbf29ce484222325_u64;
    let payload = Value::Text("x".repeat(db.payload_bytes));
    while let Some(row) = result.next().unwrap() {
        count = count.checked_add(1).unwrap();
        let id = i64::try_from(count).unwrap();
        let moved = db.mutated && id <= 100;
        let expected = [
            Value::Int64(id),
            Value::Int64(id % 100 + if moved { 1_000 } else { 0 }),
            Value::Int64(id + if moved { db.rows } else { 0 }),
            payload.clone(),
        ];
        assert_eq!(
            row.values(),
            expected.as_slice(),
            "streamed fixture row {id}"
        );
        // FNV-1a's modular multiplication is intentional, not size arithmetic.
        // Payload bytes are asserted above; include their length in the digest.
        for number in [
            id,
            id % 100 + if moved { 1_000 } else { 0 },
            id + if moved { db.rows } else { 0 },
            i64::try_from(db.payload_bytes).unwrap(),
        ] {
            for byte in number.to_le_bytes() {
                digest = (digest ^ u64::from(byte)).wrapping_mul(0x100000001b3);
            }
        }
    }
    assert_eq!(
        count,
        u64::try_from(db.rows - if db.mutated { 100 } else { 0 }).unwrap()
    );
    (count, digest)
}

pub fn checkpoint_reopen_and_repeat(db: &mut ScaleDatabase) {
    let before = full_scale_witness(db);
    let started = Instant::now();
    db.checkpoint();
    db.metrics.checkpoint_reopen = started.elapsed();
    let after = full_scale_witness(db);
    assert_eq!(
        after, before,
        "checkpoint/reopen preserves every row and result digest"
    );
    db.metrics.reopened_rows = after.0;
    db.metrics.reopen_digest = after.1;
    assert_point_queries(
        db,
        [
            0,
            1,
            100,
            101,
            db.rows / 2,
            db.rows - 100,
            db.rows - 99,
            db.rows,
            db.rows + 1,
        ],
    );
    assert_composite_ranges(db, db.rows);
}

pub fn run_large_acceptance_queries_and_mutations(db: &mut ScaleDatabase, rows: i64) {
    assert_point_queries(db, [0, 1, rows / 2, rows, rows + 1]);
    assert_composite_ranges(db, rows);
    mutate_indexed_rows(db, rows);
}
