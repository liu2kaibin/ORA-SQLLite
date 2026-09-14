use std::{fmt, marker::PhantomData};

use crate::{
    DbError, ErrorCode, ExecutionResult, Parameters, Result, Rows, SqlLimits,
    sql::exec::{execute_validated_write_plan, prepare_query, validate_write_execution},
    storage::{ReadSnapshot, WriteBatch},
};

use super::{Connection, Statement, statement::prepare_with_catalog};

/// A stable, read-only view of committed database state.
pub struct ReadTransaction<'connection> {
    snapshot: ReadSnapshot,
    sql_limits: SqlLimits,
    database_uuid: [u8; 16],
    max_query_memory_bytes: u64,
    _connection: PhantomData<&'connection mut Connection>,
}

impl<'connection> ReadTransaction<'connection> {
    pub(super) const fn new(
        snapshot: ReadSnapshot,
        sql_limits: SqlLimits,
        database_uuid: [u8; 16],
        max_query_memory_bytes: u64,
    ) -> Self {
        Self {
            snapshot,
            sql_limits,
            database_uuid,
            max_query_memory_bytes,
            _connection: PhantomData,
        }
    }

    /// Prepares one SQL statement against this transaction's fixed snapshot.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let catalog = self.snapshot.catalog_reader()?;
        prepare_with_catalog(sql, &self.sql_limits, &catalog, self.database_uuid)
    }

    /// Starts a pull-based query against this transaction's fixed snapshot.
    pub fn query<'rows>(
        &'rows mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<Rows<'rows>> {
        let catalog = self.snapshot.catalog_reader()?;
        let execution = prepare_query(
            statement,
            parameters,
            &catalog,
            self.database_uuid,
            self.max_query_memory_bytes,
        )?;
        Ok(Rows::borrowed_read(&mut self.snapshot, execution))
    }
}

impl fmt::Debug for ReadTransaction<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadTransaction")
            .finish_non_exhaustive()
    }
}

/// An exclusive database write that rolls back unless explicitly committed.
pub struct WriteTransaction<'connection> {
    batch: Option<WriteBatch>,
    sql_limits: SqlLimits,
    database_uuid: [u8; 16],
    max_query_memory_bytes: u64,
    _connection: PhantomData<&'connection mut Connection>,
}

impl<'connection> WriteTransaction<'connection> {
    pub(super) const fn new(
        batch: WriteBatch,
        sql_limits: SqlLimits,
        database_uuid: [u8; 16],
        max_query_memory_bytes: u64,
    ) -> Self {
        Self {
            batch: Some(batch),
            sql_limits,
            database_uuid,
            max_query_memory_bytes,
            _connection: PhantomData,
        }
    }

    /// Executes a non-row-producing statement in this transaction's write batch.
    pub fn execute(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        let memory_limit = self.max_query_memory_bytes;
        let batch = self.batch_mut()?;
        let validated = match validate_write_execution(batch, statement, parameters, memory_limit) {
            Ok(validated) => validated,
            Err(error) => return Err(classify_explicit_transaction_error(batch, error)),
        };
        let checkpoint = match batch.statement_checkpoint() {
            Ok(checkpoint) => checkpoint,
            Err(error) => return Err(classify_explicit_transaction_error(batch, error)),
        };
        match execute_validated_write_plan(batch, validated) {
            Ok(result) => Ok(result),
            Err(error) => {
                batch.restore_statement(checkpoint);
                Err(classify_explicit_transaction_error(batch, error))
            }
        }
    }

    /// Prepares one SQL statement against this write batch's catalog view.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let catalog = self.batch_mut()?.catalog_reader()?;
        prepare_with_catalog(sql, &self.sql_limits, &catalog, self.database_uuid)
    }

    /// Starts a pull-based query against this transaction's current write batch.
    pub fn query<'rows>(
        &'rows mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<Rows<'rows>> {
        let database_uuid = self.database_uuid;
        let max_query_memory_bytes = self.max_query_memory_bytes;
        let batch = self.batch_mut()?;
        let execution = (|| {
            let catalog = batch.catalog_reader()?;
            prepare_query(
                statement,
                parameters,
                &catalog,
                database_uuid,
                max_query_memory_bytes,
            )
        })();
        match execution {
            Ok(execution) => Ok(Rows::borrowed_write(batch, execution)),
            Err(error) => {
                if error.is_fatal() {
                    batch.mark_rollback_only();
                }
                Err(error)
            }
        }
    }

    /// Commits this transaction.
    pub fn commit(mut self) -> Result<()> {
        self.take_batch()?.commit()
    }

    /// Rolls this transaction back without publishing changes.
    pub fn rollback(mut self) -> Result<()> {
        self.take_batch()?.rollback()
    }

    fn take_batch(&mut self) -> Result<WriteBatch> {
        self.batch.take().ok_or_else(|| {
            DbError::new(
                ErrorCode::Transaction,
                "write transaction has already finished",
            )
        })
    }

    fn batch_mut(&mut self) -> Result<&mut WriteBatch> {
        self.batch.as_mut().ok_or_else(|| {
            DbError::new(
                ErrorCode::Transaction,
                "write transaction has already finished",
            )
        })
    }
}

fn classify_explicit_transaction_error(batch: &mut WriteBatch, error: DbError) -> DbError {
    if error.is_fatal() {
        batch.mark_rollback_only();
    }
    error
}

impl fmt::Debug for WriteTransaction<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WriteTransaction")
            .finish_non_exhaustive()
    }
}

impl Drop for WriteTransaction<'_> {
    fn drop(&mut self) {
        if let Some(mut batch) = self.batch.take() {
            let _ = batch.rollback();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::classify_explicit_transaction_error;
    use crate::{
        Database, DbError, ErrorCode, Options, Parameters, Value,
        storage::{
            catalog::types::ColumnType,
            test_support::{TempDatabase, phase_4b_add_table, phase_4b_catalog_fixture},
        },
    };

    #[test]
    fn create_and_drop_are_visible_in_transaction_and_rollbackable() {
        // Production break caught: DDL that bypasses the active WriteBatch is
        // either invisible to later prepare/query or survives transaction rollback.
        let temp = TempDatabase::new("transaction-create-rollback");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        {
            let mut transaction = connection.begin_write().unwrap();
            let create = transaction
                .prepare("CREATE TABLE T (ID INT64, NAME TEXT)")
                .unwrap();
            assert_eq!(
                transaction
                    .execute(&create, Parameters::none())
                    .unwrap()
                    .rows_affected(),
                0
            );
            let select = transaction.prepare("SELECT ID FROM T").unwrap();
            let mut rows = transaction.query(&select, Parameters::none()).unwrap();
            assert!(rows.next().unwrap().is_none());
            drop(rows);
            transaction.rollback().unwrap();
        }
        assert_eq!(
            connection.prepare("SELECT ID FROM T").unwrap_err().code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn constrained_ddl_is_unsupported_before_mutation() {
        // Production break caught: treating a deferred constraint as a partial
        // CREATE would publish catalog/page changes despite returning Unsupported.
        let temp = TempDatabase::new("transaction-constrained-create");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        drop(db);
        let before = std::fs::read(temp.path()).unwrap();
        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let constrained = transaction
            .prepare("CREATE TABLE T (ID INT64 PRIMARY KEY)")
            .unwrap();
        assert_eq!(
            transaction
                .execute(&constrained, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );
        transaction.commit().unwrap();
        assert_eq!(
            connection.prepare("SELECT ID FROM T").unwrap_err().code(),
            ErrorCode::Bind
        );
        drop(connection);
        drop(db);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);
    }

    #[test]
    fn drop_table_commit_rollback_and_old_statement_invalidation_are_transactional() {
        // Production break caught: a DROP that commits implicitly, escapes the
        // batch catalog, or skips retained-schema validation breaks these three states.
        let temp = TempDatabase::new("transaction-drop-commit-rollback");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let create = connection.prepare("CREATE TABLE T (ID INT64)").unwrap();
        let mut create_transaction = connection.begin_write().unwrap();
        create_transaction
            .execute(&create, Parameters::none())
            .unwrap();
        create_transaction.commit().unwrap();

        let old_select = connection.prepare("SELECT ID FROM T").unwrap();
        let mut rollback_transaction = connection.begin_write().unwrap();
        let drop_table = rollback_transaction.prepare("DROP TABLE T").unwrap();
        assert_eq!(
            rollback_transaction
                .execute(&drop_table, Parameters::none())
                .unwrap()
                .rows_affected(),
            0
        );
        assert_eq!(
            rollback_transaction
                .prepare("SELECT ID FROM T")
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert_eq!(
            rollback_transaction
                .query(&old_select, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
        rollback_transaction.rollback().unwrap();
        assert!(connection.prepare("SELECT ID FROM T").is_ok());

        let mut commit_transaction = connection.begin_write().unwrap();
        let drop_table = commit_transaction.prepare("DROP TABLE T").unwrap();
        commit_transaction
            .execute(&drop_table, Parameters::none())
            .unwrap();
        commit_transaction.commit().unwrap();
        assert_eq!(
            connection.prepare("SELECT ID FROM T").unwrap_err().code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn retained_drop_id_never_rebinds_to_a_recreated_name() {
        // Production break caught: resolving DROP by name during execution lets
        // an old Statement delete a newly created table with the same name.
        let temp = TempDatabase::new("transaction-retained-drop-id");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create = transaction.prepare("CREATE TABLE T (ID INT64)").unwrap();
        transaction.execute(&create, Parameters::none()).unwrap();
        let old_drop = transaction.prepare("DROP TABLE T").unwrap();
        transaction.execute(&old_drop, Parameters::none()).unwrap();
        let recreate = transaction.prepare("CREATE TABLE T (ID INT64)").unwrap();
        transaction.execute(&recreate, Parameters::none()).unwrap();
        assert_eq!(
            transaction
                .execute(&old_drop, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
        assert!(transaction.prepare("SELECT ID FROM T").is_ok());
        transaction.commit().unwrap();
        assert!(connection.prepare("SELECT ID FROM T").is_ok());
    }

    #[test]
    fn duplicate_create_is_ordinary_and_the_transaction_remains_usable() {
        // Production break caught: failing to validate the retained CREATE name
        // or poisoning the transaction on SchemaChanged prevents the later commit.
        let temp = TempDatabase::new("transaction-duplicate-create");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let first = transaction.prepare("CREATE TABLE T (ID INT64)").unwrap();
        let stale_duplicate = transaction.prepare("CREATE TABLE T (ID INT64)").unwrap();
        transaction.execute(&first, Parameters::none()).unwrap();
        assert_eq!(
            transaction
                .execute(&stale_duplicate, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
        let second = transaction.prepare("CREATE TABLE U (ID INT64)").unwrap();
        transaction.execute(&second, Parameters::none()).unwrap();
        transaction.commit().unwrap();
        assert!(connection.prepare("SELECT ID FROM T").is_ok());
        assert!(connection.prepare("SELECT ID FROM U").is_ok());
    }

    #[test]
    fn ddl_parameter_and_database_identity_errors_precede_mutation() {
        // Production break caught: mutating before parameter/UUID validation can
        // leave a duplicate table behind or poison an otherwise usable transaction.
        let first_temp = TempDatabase::new("transaction-ddl-first-database");
        let first_db = Database::create(first_temp.path(), Options::default()).unwrap();
        let mut first_connection = first_db.connect().unwrap();
        let foreign_create = first_connection
            .prepare("CREATE TABLE T (ID INT64)")
            .unwrap();

        let second_temp = TempDatabase::new("transaction-ddl-second-database");
        let second_db = Database::create(second_temp.path(), Options::default()).unwrap();
        let mut second_connection = second_db.connect().unwrap();
        let mut transaction = second_connection.begin_write().unwrap();
        assert_eq!(
            transaction
                .execute(&foreign_create, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );

        let local_create = transaction.prepare("CREATE TABLE T (ID INT64)").unwrap();
        let extra = [Value::Int64(1).into()];
        assert_eq!(
            transaction
                .execute(&local_create, Parameters::positional(&extra))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        transaction
            .execute(&local_create, Parameters::none())
            .unwrap();
        transaction.commit().unwrap();
        assert!(second_connection.prepare("SELECT ID FROM T").is_ok());
    }

    #[test]
    fn deferred_index_is_unsupported_without_any_committable_mutation() {
        // Production break caught: allocating pages or catalog objects before
        // DeferredUnsupported would change the database even though execute fails.
        let temp = TempDatabase::new("transaction-deferred-index");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut create_transaction = connection.begin_write().unwrap();
        let create = create_transaction
            .prepare("CREATE TABLE T (ID INT64)")
            .unwrap();
        create_transaction
            .execute(&create, Parameters::none())
            .unwrap();
        create_transaction.commit().unwrap();
        drop(connection);
        drop(db);
        let before = std::fs::read(temp.path()).unwrap();

        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let deferred = transaction.prepare("CREATE INDEX IX_T ON T (ID)").unwrap();
        assert_eq!(
            transaction
                .execute(&deferred, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );
        transaction.commit().unwrap();
        assert!(connection.prepare("CREATE INDEX IX_T ON T (ID)").is_ok());
        drop(connection);
        drop(db);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);
    }

    #[test]
    fn rust_write_transaction_rejects_wrong_execution_api_kinds() {
        // Production break caught: accepting a query or SQL transaction control
        // through this DDL sink bypasses Rows and Rust transaction ownership.
        let temp = TempDatabase::new("transaction-ddl-wrong-kinds");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let query = transaction.prepare("SELECT 1").unwrap();
        assert_eq!(
            transaction
                .execute(&query, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        let create = transaction.prepare("CREATE TABLE T (ID INT64)").unwrap();
        assert_eq!(
            transaction
                .query(&create, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        let begin = transaction.prepare("BEGIN").unwrap();
        assert_eq!(
            transaction
                .execute(&begin, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
        transaction.execute(&create, Parameters::none()).unwrap();
        transaction.rollback().unwrap();
    }

    #[test]
    fn fatal_ddl_validation_error_makes_the_batch_rollback_only() {
        // Production break caught: returning a fatal catalog error without
        // rollback-only state would permit further DDL or commit on bad state.
        let fixture = phase_4b_catalog_fixture(4096);
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let create = connection
            .prepare("CREATE TABLE NEW_TABLE (ID INT64)")
            .unwrap();
        let mut transaction = connection.begin_write().unwrap();
        crate::storage::test_support::phase_4c1_corrupt_batch_catalog(
            transaction.batch.as_mut().unwrap(),
        );
        assert_eq!(
            transaction
                .execute(&create, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            transaction
                .prepare("CREATE TABLE OTHER_TABLE (ID INT64)")
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
        transaction.rollback().unwrap();
    }

    #[test]
    fn explicit_transaction_classification_marks_exactly_the_fatal_codes() {
        // Production break caught: omitting one fatal class permits bad state
        // to continue, while marking an ordinary class rejects valid recovery.
        for code in [ErrorCode::Io, ErrorCode::Corruption, ErrorCode::Internal] {
            let temp = TempDatabase::new("transaction-fatal-classification");
            let db = Database::create(temp.path(), Options::default()).unwrap();
            let mut batch = db.pager.try_begin_write().unwrap();
            assert_eq!(
                classify_explicit_transaction_error(
                    &mut batch,
                    DbError::new(code, "injected fatal classification"),
                )
                .code(),
                code
            );
            assert_eq!(
                batch.catalog_reader().unwrap_err().code(),
                ErrorCode::Transaction
            );
            batch.rollback().unwrap();
        }

        for code in [
            ErrorCode::Bind,
            ErrorCode::SchemaChanged,
            ErrorCode::Type,
            ErrorCode::Unsupported,
            ErrorCode::ResourceLimit,
        ] {
            let temp = TempDatabase::new("transaction-ordinary-classification");
            let db = Database::create(temp.path(), Options::default()).unwrap();
            let mut batch = db.pager.try_begin_write().unwrap();
            classify_explicit_transaction_error(
                &mut batch,
                DbError::new(code, "injected ordinary classification"),
            );
            assert!(batch.catalog_reader().is_ok(), "{code:?}");
            batch.rollback().unwrap();
        }
    }

    #[test]
    fn read_transaction_prepare_reuses_original_catalog_snapshot() {
        // Production break caught: opening a fresh snapshot inside prepare would
        // let an already-open read transaction observe a later catalog commit.
        let fixture = phase_4b_catalog_fixture(4096);
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let writer_pager = db.pager.clone();
        let mut connection = db.connect().unwrap();
        let mut old_read = connection.begin_read().unwrap();

        phase_4b_add_table(
            &writer_pager,
            "NEW_TABLE",
            &[("ID", ColumnType::Int64, false)],
        )
        .unwrap();

        assert_eq!(
            old_read
                .prepare("SELECT ID FROM NEW_TABLE")
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        drop(old_read);
        assert!(connection.prepare("SELECT ID FROM NEW_TABLE").is_ok());
    }

    #[test]
    fn rust_transactions_stream_queries_from_their_own_storage_context() {
        // Production break caught: reopening storage instead of borrowing the
        // transaction would violate the fixed-snapshot/current-batch contract.
        let fixture = crate::storage::test_support::phase_4c1_query_relation_fixture(&[(7, "N")]);
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut read = connection.begin_read().unwrap();
        let statement = read.prepare("SELECT ID, NAME FROM USERS").unwrap();
        let mut rows = read.query(&statement, crate::Parameters::none()).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[crate::Value::Int64(7), crate::Value::Text("N".into())]
        );
        drop(rows);
        drop(read);

        let mut write = connection.begin_write().unwrap();
        let statement = write.prepare("SELECT ID FROM USERS").unwrap();
        let mut rows = write.query(&statement, crate::Parameters::none()).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[crate::Value::Int64(7)]
        );
        drop(rows);
        write.rollback().unwrap();
    }

    #[test]
    fn fatal_query_error_requires_rollback_but_ordinary_error_keeps_write_transaction_usable() {
        // Production break caught: returning a fatal streaming error without
        // transaction-owned rollback-only state allows a later commit, while
        // treating every query error as fatal rejects valid continuation.
        let ordinary = crate::storage::test_support::phase_4c1_query_relation_fixture(&[(7, "N")]);
        let ordinary_db = Database::open(ordinary.main_path(), Options::default()).unwrap();
        let mut ordinary_connection = ordinary_db.connect().unwrap();
        let ddl = ordinary_connection
            .prepare("CREATE TABLE UNUSED (ID INT64)")
            .unwrap();
        let mut ordinary_write = ordinary_connection.begin_write().unwrap();
        assert_eq!(
            ordinary_write
                .query(&ddl, crate::Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        let select = ordinary_write.prepare("SELECT ID FROM USERS").unwrap();
        let mut rows = ordinary_write
            .query(&select, crate::Parameters::none())
            .unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[crate::Value::Int64(7)]
        );
        drop(rows);
        ordinary_write.commit().unwrap();

        let corrupt = crate::storage::test_support::phase_4c1_corrupt_query_relation_fixture();
        let corrupt_db = Database::open(corrupt.main_path(), Options::default()).unwrap();
        let mut corrupt_connection = corrupt_db.connect().unwrap();
        let mut fatal_write = corrupt_connection.begin_write().unwrap();
        let select = fatal_write.prepare("SELECT ID FROM USERS").unwrap();
        let mut rows = fatal_write
            .query(&select, crate::Parameters::none())
            .unwrap();
        assert_eq!(rows.next().unwrap_err().code(), ErrorCode::Corruption);
        drop(rows);
        assert_eq!(
            fatal_write
                .prepare("SELECT ID FROM USERS")
                .unwrap_err()
                .code(),
            ErrorCode::Transaction
        );
        assert_eq!(
            fatal_write.commit().unwrap_err().code(),
            ErrorCode::Transaction
        );

        let mut rollback_write = corrupt_connection.begin_write().unwrap();
        let select = rollback_write.prepare("SELECT ID FROM USERS").unwrap();
        let mut rows = rollback_write
            .query(&select, crate::Parameters::none())
            .unwrap();
        assert_eq!(rows.next().unwrap_err().code(), ErrorCode::Corruption);
        drop(rows);
        rollback_write.rollback().unwrap();
    }

    #[test]
    fn fatal_query_construction_error_makes_write_transaction_rollback_only() {
        // Production break caught: handling fatal errors only in Rows::next
        // misses catalog/plan construction failures that happen before Rows
        // exists and would otherwise leave commit reachable.
        let fixture = crate::storage::test_support::phase_4c1_query_relation_fixture(&[(7, "N")]);
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let statement = connection.prepare("SELECT ID FROM USERS").unwrap();
        let mut write = connection.begin_write().unwrap();
        crate::storage::test_support::phase_4c1_corrupt_batch_catalog(
            write.batch.as_mut().unwrap(),
        );
        assert_eq!(
            write
                .query(&statement, crate::Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Corruption
        );
        assert_eq!(
            write.prepare("SELECT ID FROM USERS").unwrap_err().code(),
            ErrorCode::Transaction
        );
        write.rollback().unwrap();
    }
}
