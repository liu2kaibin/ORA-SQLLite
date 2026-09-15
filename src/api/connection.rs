use std::{cell::Cell, fmt, marker::PhantomData, mem};

use crate::sql::{
    exec::{
        prepare_query, validate_control_execution, validate_control_parameters,
        validate_write_before_writer,
    },
    plan::PlanKind,
};
use crate::{
    DbError, ErrorCode, ExecutionResult, Parameters, Result, Rows, SqlLimits,
    storage::{CommitFailure, Pager, WriteBatch},
};

use super::{
    ReadTransaction, Statement, WriteTransaction, statement::prepare_with_catalog,
    transaction::execute_write_batch,
};

enum ConnectionState {
    Idle,
    SqlWrite(WriteBatch),
    SqlWriteFailed(WriteBatch),
    NeedsReopen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionStateKind {
    Idle,
    SqlWrite,
    SqlWriteFailed,
    NeedsReopen,
}

/// A single database session used to start transactions.
pub struct Connection {
    pager: Pager,
    sql_limits: SqlLimits,
    max_query_memory_bytes: u64,
    state: ConnectionState,
    _not_sync: PhantomData<Cell<()>>,
}

impl Connection {
    pub(super) const fn new(
        pager: Pager,
        sql_limits: SqlLimits,
        max_query_memory_bytes: u64,
    ) -> Self {
        Self {
            pager,
            sql_limits,
            max_query_memory_bytes,
            state: ConnectionState::Idle,
            _not_sync: PhantomData,
        }
    }

    /// Prepares one SQL statement against the connection's current catalog view.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        if self.state_kind() == ConnectionStateKind::SqlWrite {
            self.ensure_sql_write_usable()?;
        }
        match &mut self.state {
            ConnectionState::Idle => {
                let database_uuid = self.pager.database_uuid()?;
                let mut snapshot = self.pager.begin_read()?;
                let catalog = snapshot.catalog_reader()?;
                prepare_with_catalog(sql, &self.sql_limits, &catalog, database_uuid)
            }
            ConnectionState::SqlWrite(batch) => {
                let database_uuid = batch.database_uuid();
                let catalog = batch.catalog_reader()?;
                prepare_with_catalog(sql, &self.sql_limits, &catalog, database_uuid)
            }
            ConnectionState::SqlWriteFailed(_) | ConnectionState::NeedsReopen => {
                Err(transaction_state_error())
            }
        }
    }

    /// Starts a pull-based query at the connection's current transaction view.
    pub fn query<'connection>(
        &'connection mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<Rows<'connection>> {
        if self.state_kind() == ConnectionStateKind::SqlWrite {
            self.ensure_sql_write_usable()?;
        }
        match self.state_kind() {
            ConnectionStateKind::Idle => {
                let database_uuid = self.pager.database_uuid()?;
                let mut snapshot = self.pager.begin_read()?;
                let catalog = snapshot.catalog_reader()?;
                let execution = prepare_query(
                    statement,
                    parameters,
                    &catalog,
                    database_uuid,
                    self.max_query_memory_bytes,
                )?;
                Ok(Rows::owned_read(snapshot, execution))
            }
            ConnectionStateKind::SqlWrite => {
                let execution = {
                    let ConnectionState::SqlWrite(batch) = &mut self.state else {
                        return Err(transaction_state_error());
                    };
                    let database_uuid = batch.database_uuid();
                    match batch.catalog_reader() {
                        Ok(catalog) => prepare_query(
                            statement,
                            parameters,
                            &catalog,
                            database_uuid,
                            self.max_query_memory_bytes,
                        ),
                        Err(error) => Err(error),
                    }
                };
                match execution {
                    Ok(execution) => {
                        let ConnectionState::SqlWrite(batch) = &mut self.state else {
                            return Err(transaction_state_error());
                        };
                        Ok(Rows::borrowed_write(batch, execution))
                    }
                    Err(error) => {
                        if error.is_fatal() {
                            self.fail_sql_write();
                        }
                        Err(error)
                    }
                }
            }
            ConnectionStateKind::SqlWriteFailed | ConnectionStateKind::NeedsReopen => {
                Err(transaction_state_error())
            }
        }
    }

    /// Executes a non-row-producing statement in this connection.
    pub fn execute(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        match self.state_kind() {
            ConnectionStateKind::Idle => match statement.plan().kind() {
                PlanKind::Query(_) => Err(wrong_execute_kind()),
                PlanKind::Begin => self.start_sql_write(statement, parameters),
                PlanKind::Commit | PlanKind::Rollback => Err(transaction_state_error()),
                PlanKind::DeferredUnsupported { .. }
                | PlanKind::Insert(_)
                | PlanKind::Update(_)
                | PlanKind::Delete(_)
                | PlanKind::CreateTable(_)
                | PlanKind::DropTable { .. } => self.execute_autocommit(statement, parameters),
            },
            ConnectionStateKind::SqlWrite => match statement.plan().kind() {
                PlanKind::Begin => Err(transaction_state_error()),
                PlanKind::Commit => self.commit_sql_write(statement, parameters),
                PlanKind::Rollback => self.rollback_sql_write(statement, parameters),
                _ => self.execute_in_sql_write(statement, parameters),
            },
            ConnectionStateKind::SqlWriteFailed => match statement.plan().kind() {
                PlanKind::Rollback => self.rollback_sql_write(statement, parameters),
                _ => Err(transaction_state_error()),
            },
            ConnectionStateKind::NeedsReopen => Err(transaction_state_error()),
        }
    }

    /// Starts a read transaction at the current committed snapshot.
    ///
    /// A transaction keeps this connection mutably borrowed, so another
    /// transaction cannot start until the first one ends.
    ///
    /// ```compile_fail
    /// use ora_sqllite::Connection;
    ///
    /// fn start_two_transactions(connection: &mut Connection) {
    ///     let first = connection.begin_read().unwrap();
    ///     let second = connection.begin_write().unwrap();
    ///     drop((first, second));
    /// }
    /// ```
    pub fn begin_read(&mut self) -> Result<ReadTransaction<'_>> {
        if self.state_kind() != ConnectionStateKind::Idle {
            return Err(transaction_state_error());
        }
        let database_uuid = self.pager.database_uuid()?;
        Ok(ReadTransaction::new(
            self.pager.begin_read()?,
            self.sql_limits,
            database_uuid,
            self.max_query_memory_bytes,
        ))
    }

    /// Starts an exclusive write transaction.
    pub fn begin_write(&mut self) -> Result<WriteTransaction<'_>> {
        if self.state_kind() != ConnectionStateKind::Idle {
            return Err(transaction_state_error());
        }
        let batch = self.pager.try_begin_write()?;
        let database_uuid = batch.database_uuid();
        Ok(WriteTransaction::new(
            batch,
            self.sql_limits,
            database_uuid,
            self.max_query_memory_bytes,
        ))
    }

    fn state_kind(&self) -> ConnectionStateKind {
        match self.state {
            ConnectionState::Idle => ConnectionStateKind::Idle,
            ConnectionState::SqlWrite(_) => ConnectionStateKind::SqlWrite,
            ConnectionState::SqlWriteFailed(_) => ConnectionStateKind::SqlWriteFailed,
            ConnectionState::NeedsReopen => ConnectionStateKind::NeedsReopen,
        }
    }

    fn ensure_sql_write_usable(&mut self) -> Result<()> {
        let error = match &mut self.state {
            ConnectionState::SqlWrite(batch) => batch.catalog_reader().map(drop).err(),
            _ => return Ok(()),
        };
        if let Some(error) = error {
            if error.code() == ErrorCode::Transaction || error.is_fatal() {
                self.fail_sql_write();
            }
            Err(error)
        } else {
            Ok(())
        }
    }

    fn start_sql_write(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        let database_uuid = self.pager.database_uuid()?;
        let mut snapshot = self.pager.begin_read()?;
        let catalog = snapshot.catalog_reader()?;
        validate_control_execution(
            statement,
            parameters,
            &catalog,
            database_uuid,
            self.max_query_memory_bytes,
        )?;
        drop(catalog);
        drop(snapshot);
        let batch = self.pager.try_begin_write()?;
        self.state = ConnectionState::SqlWrite(batch);
        Ok(ExecutionResult::new(0))
    }

    fn execute_autocommit(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        let database_uuid = self.pager.database_uuid()?;
        let mut snapshot = self.pager.begin_read()?;
        let catalog = snapshot.catalog_reader()?;
        validate_write_before_writer(
            statement,
            copy_parameters(&parameters),
            &catalog,
            database_uuid,
            self.max_query_memory_bytes,
        )?;
        drop(catalog);
        drop(snapshot);
        let mut batch = self.pager.try_begin_write()?;
        let result = execute_write_batch(
            &mut batch,
            statement,
            parameters,
            self.max_query_memory_bytes,
        );
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = batch.rollback();
                return Err(error);
            }
        };
        match batch.commit_with_outcome() {
            Ok(()) => Ok(result),
            Err(CommitFailure::RetryableBusy { error, .. }) => Err(error),
            Err(CommitFailure::PreDurability(error)) => {
                if error.is_fatal() {
                    self.state = ConnectionState::NeedsReopen;
                }
                Err(error)
            }
            Err(CommitFailure::NeedsReopen(error)) => {
                self.state = ConnectionState::NeedsReopen;
                Err(error)
            }
        }
    }

    fn execute_in_sql_write(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        self.ensure_sql_write_usable()?;
        let result = {
            let ConnectionState::SqlWrite(batch) = &mut self.state else {
                return Err(transaction_state_error());
            };
            execute_write_batch(batch, statement, parameters, self.max_query_memory_bytes)
        };
        if let Err(error) = &result {
            if error.is_fatal() {
                self.fail_sql_write();
            }
        }
        result
    }

    fn commit_sql_write(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        self.ensure_sql_write_usable()?;
        let validation = {
            let ConnectionState::SqlWrite(batch) = &mut self.state else {
                return Err(transaction_state_error());
            };
            let database_uuid = batch.database_uuid();
            match batch.catalog_reader() {
                Ok(catalog) => validate_control_execution(
                    statement,
                    parameters,
                    &catalog,
                    database_uuid,
                    self.max_query_memory_bytes,
                ),
                Err(error) => Err(error),
            }
        };
        if let Err(error) = validation {
            if error.is_fatal() {
                self.fail_sql_write();
            }
            return Err(error);
        }

        let state = mem::replace(&mut self.state, ConnectionState::NeedsReopen);
        let ConnectionState::SqlWrite(batch) = state else {
            return Err(transaction_state_error());
        };
        match batch.commit_with_outcome() {
            Ok(()) => {
                self.state = ConnectionState::Idle;
                Ok(ExecutionResult::new(0))
            }
            Err(CommitFailure::RetryableBusy { error, batch }) => {
                self.state = ConnectionState::SqlWrite(*batch);
                Err(error)
            }
            Err(CommitFailure::PreDurability(error)) => {
                if !error.is_fatal() {
                    self.state = ConnectionState::Idle;
                }
                Err(error)
            }
            Err(CommitFailure::NeedsReopen(error)) => Err(error),
        }
    }

    fn rollback_sql_write(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        validate_control_parameters(statement, parameters, self.max_query_memory_bytes)?;
        if self.state_kind() == ConnectionStateKind::SqlWrite {
            let validate = match self.ensure_sql_write_usable() {
                Ok(()) => true,
                Err(error)
                    if error.code() == ErrorCode::Transaction
                        && self.state_kind() == ConnectionStateKind::SqlWriteFailed =>
                {
                    false
                }
                Err(error) => return Err(error),
            };
            if validate {
                let validation = {
                    let ConnectionState::SqlWrite(batch) = &mut self.state else {
                        return Err(transaction_state_error());
                    };
                    let database_uuid = batch.database_uuid();
                    match batch.catalog_reader() {
                        Ok(catalog) => statement.validate_schema(database_uuid, &catalog),
                        Err(error) => Err(error),
                    }
                };
                if let Err(error) = validation {
                    if error.is_fatal() {
                        self.fail_sql_write();
                    }
                    return Err(error);
                }
            }
        }
        let state = mem::replace(&mut self.state, ConnectionState::Idle);
        let mut batch = match state {
            ConnectionState::SqlWrite(batch) | ConnectionState::SqlWriteFailed(batch) => batch,
            other => {
                self.state = other;
                return Err(transaction_state_error());
            }
        };
        batch.rollback()?;
        Ok(ExecutionResult::new(0))
    }

    fn fail_sql_write(&mut self) {
        let state = mem::replace(&mut self.state, ConnectionState::NeedsReopen);
        self.state = match state {
            ConnectionState::SqlWrite(mut batch) => {
                batch.mark_rollback_only();
                ConnectionState::SqlWriteFailed(batch)
            }
            other => other,
        };
    }
}

fn transaction_state_error() -> DbError {
    DbError::new(
        ErrorCode::Transaction,
        "connection transaction state does not allow this operation",
    )
}

fn wrong_execute_kind() -> DbError {
    DbError::new(
        ErrorCode::Type,
        "execute requires a non-row-producing statement",
    )
}

fn copy_parameters<'a>(parameters: &Parameters<'a>) -> Parameters<'a> {
    match parameters {
        Parameters::None => Parameters::none(),
        Parameters::Positional(values) => Parameters::positional(values),
        Parameters::Named(values) => Parameters::named(values),
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Connection").finish_non_exhaustive()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let state = mem::replace(&mut self.state, ConnectionState::Idle);
        if let ConnectionState::SqlWrite(mut batch) | ConnectionState::SqlWriteFailed(mut batch) =
            state
        {
            let _ = batch.rollback();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::{self, RecvTimeoutError},
        time::Duration,
    };

    use crate::{
        Database, ErrorCode, Options, Parameters, SqlLimits, Value,
        storage::test_support::{
            TempDatabase, phase_4b_catalog_fixture, phase_4c1_commit_io_fault_pager,
            phase_4c1_corrupt_batch_catalog, phase_4c1_corrupt_query_relation_fixture,
        },
    };

    use super::{Connection, ConnectionState, ConnectionStateKind};

    fn execute_sql(
        connection: &mut Connection,
        sql: &str,
    ) -> crate::Result<crate::ExecutionResult> {
        let statement = connection.prepare(sql)?;
        connection.execute(&statement, Parameters::none())
    }

    #[test]
    fn fatal_sql_write_error_enters_rollback_only_state() {
        // Production break caught: retaining SqlWrite after fatal catalog
        // corruption would allow prepare/execute/commit instead of rollback only.
        let fixture = phase_4b_catalog_fixture(4096);
        let database = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = database.connect().unwrap();
        let create = connection
            .prepare("CREATE TABLE NEW_TABLE (ID INT64)")
            .unwrap();
        let select = connection.prepare("SELECT 1").unwrap();
        let rollback = connection.prepare("ROLLBACK").unwrap();
        execute_sql(&mut connection, "BEGIN").unwrap();
        let ConnectionState::SqlWrite(batch) = &mut connection.state else {
            panic!("BEGIN did not retain an active SQL write batch");
        };
        phase_4c1_corrupt_batch_catalog(batch);

        assert_eq!(
            connection
                .execute(&create, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(connection.state_kind(), ConnectionStateKind::SqlWriteFailed);
        assert_eq!(
            connection.prepare("SELECT 1").unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection
                .query(&select, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection
                .execute(&create, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection.begin_read().unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection.begin_write().unwrap_err().code(),
            ErrorCode::Transaction
        );
        let unexpected = [Value::Int64(1).into()];
        assert_eq!(
            connection
                .execute(&rollback, Parameters::positional(&unexpected))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert_eq!(connection.state_kind(), ConnectionStateKind::SqlWriteFailed);
        assert_eq!(
            connection
                .execute(&rollback, Parameters::none())
                .unwrap()
                .rows_affected(),
            0
        );
        assert_eq!(connection.state_kind(), ConnectionStateKind::Idle);
        execute_sql(&mut connection, "CREATE TABLE NEW_TABLE (ID INT64)").unwrap();
    }

    #[test]
    fn fatal_streaming_query_error_is_observed_as_sql_write_failed() {
        // Production break caught: Rows can discover corruption after query()
        // returns; the next connection operation must promote rollback-only
        // batch state and reject everything except ROLLBACK.
        let fixture = phase_4c1_corrupt_query_relation_fixture();
        let database = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = database.connect().unwrap();
        let select = connection.prepare("SELECT ID FROM USERS").unwrap();
        let rollback = connection.prepare("ROLLBACK").unwrap();
        execute_sql(&mut connection, "BEGIN").unwrap();
        let mut rows = connection.query(&select, Parameters::none()).unwrap();
        assert_eq!(rows.next().unwrap_err().code(), ErrorCode::Corruption);
        drop(rows);

        assert_eq!(
            connection.prepare("SELECT 1").unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(connection.state_kind(), ConnectionStateKind::SqlWriteFailed);
        connection.execute(&rollback, Parameters::none()).unwrap();
        assert_eq!(connection.state_kind(), ConnectionStateKind::Idle);
    }

    #[test]
    fn fatal_streaming_query_can_be_rolled_back_immediately() {
        // Production break caught: a Rows error marks the borrowed batch
        // rollback-only before Connection can promote its enum state. The
        // first ROLLBACK after Rows drops must consume that batch directly.
        let fixture = phase_4c1_corrupt_query_relation_fixture();
        let database = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = database.connect().unwrap();
        let select = connection.prepare("SELECT ID FROM USERS").unwrap();
        let rollback = connection.prepare("ROLLBACK").unwrap();
        execute_sql(&mut connection, "BEGIN").unwrap();
        let mut rows = connection.query(&select, Parameters::none()).unwrap();
        assert_eq!(rows.next().unwrap_err().code(), ErrorCode::Corruption);
        drop(rows);

        assert_eq!(connection.state_kind(), ConnectionStateKind::SqlWrite);
        connection.execute(&rollback, Parameters::none()).unwrap();
        assert_eq!(connection.state_kind(), ConnectionStateKind::Idle);
    }

    #[test]
    fn sql_commit_io_error_consumes_batch_and_requires_reopen() {
        // Production break caught: returning to Idle after an ambiguous commit
        // would let the same connection read/write despite pager recovery state.
        let temp = TempDatabase::new("connection-sql-commit-io");
        let pager = phase_4c1_commit_io_fault_pager(&temp);
        let mut connection = Connection::new(
            pager,
            SqlLimits::default(),
            Options::default().max_query_memory_bytes(),
        );
        let begin = connection.prepare("BEGIN").unwrap();
        let create = connection.prepare("CREATE TABLE T (ID INT64)").unwrap();
        let commit = connection.prepare("COMMIT").unwrap();
        let rollback = connection.prepare("ROLLBACK").unwrap();
        let select = connection.prepare("SELECT 1").unwrap();
        connection.execute(&begin, Parameters::none()).unwrap();
        connection.execute(&create, Parameters::none()).unwrap();
        assert_eq!(
            connection
                .execute(&commit, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Io
        );
        assert_eq!(connection.state_kind(), ConnectionStateKind::NeedsReopen);
        assert_eq!(
            connection.prepare("SELECT 1").unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection
                .query(&select, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection
                .execute(&rollback, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection.begin_read().unwrap_err().code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            connection.begin_write().unwrap_err().code(),
            ErrorCode::Transaction
        );
        drop(connection);

        let database = Database::open(temp.path(), Options::default()).unwrap();
        let mut reopened = database.connect().unwrap();
        assert_eq!(
            reopened.prepare("SELECT ID FROM T").unwrap_err().code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn autocommit_io_error_also_requires_reopen_and_is_not_reported_visible() {
        // Production break caught: auto-commit uses the same durability boundary
        // and cannot silently return to Idle after WAL I/O failure.
        let temp = TempDatabase::new("connection-autocommit-io");
        let pager = phase_4c1_commit_io_fault_pager(&temp);
        let mut connection = Connection::new(
            pager,
            SqlLimits::default(),
            Options::default().max_query_memory_bytes(),
        );
        let create = connection.prepare("CREATE TABLE T (ID INT64)").unwrap();
        assert_eq!(
            connection
                .execute(&create, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Io
        );
        assert_eq!(connection.state_kind(), ConnectionStateKind::NeedsReopen);
        assert_eq!(
            connection.prepare("SELECT 1").unwrap_err().code(),
            ErrorCode::Transaction
        );
        drop(connection);

        let database = Database::open(temp.path(), Options::default()).unwrap();
        let mut reopened = database.connect().unwrap();
        assert_eq!(
            reopened.prepare("SELECT ID FROM T").unwrap_err().code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn begin_write_returns_busy_without_waiting_for_published_state() {
        let temp = TempDatabase::new("connection-immediate-busy");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut owner = db.connect().unwrap();
        let mut contender = db.connect().unwrap();
        let owner_write = owner.begin_write().unwrap();
        let published = db.pager.hold_published_write_for_test();
        let (starting_tx, starting_rx) = mpsc::sync_channel(0);
        let (result_tx, result_rx) = mpsc::sync_channel(0);
        let contender_thread = std::thread::spawn(move || {
            starting_tx.send(()).unwrap();
            result_tx
                .send(
                    contender
                        .begin_write()
                        .map(|_| ())
                        .map_err(|error| error.code()),
                )
                .unwrap();
        });
        starting_rx.recv().unwrap();

        let outcome = result_rx.recv_timeout(Duration::from_secs(1));
        drop(published);
        let waited_for_published_state = matches!(outcome, Err(RecvTimeoutError::Timeout));
        let result = match outcome {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => result_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("contender returns after the publication lock is released"),
            Err(RecvTimeoutError::Disconnected) => panic!("contender disconnected"),
        };
        owner_write.rollback().unwrap();
        contender_thread.join().unwrap();

        assert!(
            !waited_for_published_state,
            "public begin_write waited for published state before returning Busy"
        );
        assert_eq!(result, Err(ErrorCode::Busy));
    }

    #[test]
    fn database_create_and_open_propagate_custom_query_memory_to_connections() {
        // Production break caught: dropping the option at either Database
        // boundary would make execution silently use the default limit.
        let temp = TempDatabase::new("connection-query-memory-create-open");
        let options = Options::default().with_max_query_memory_bytes(12_345);
        let created = Database::create(temp.path(), options).unwrap();
        assert_eq!(created.connect().unwrap().max_query_memory_bytes, 12_345);
        drop(created);
        let opened = Database::open(temp.path(), options).unwrap();
        assert_eq!(opened.connect().unwrap().max_query_memory_bytes, 12_345);
    }
}
