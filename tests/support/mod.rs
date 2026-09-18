use std::{
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ora_sqllite::{Connection, Database, ExecutionResult, Options, Parameters, Statement, Value};

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
