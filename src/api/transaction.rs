use std::{fmt, marker::PhantomData};

use crate::{
    DbError, ErrorCode, ExecutionResult, Parameters, Result, Rows, SqlLimits,
    sql::exec::{execute_validated_write_plan, prepare_query, validate_write_execution},
    storage::{ReadSnapshot, WriteBatch},
};

use super::{Connection, Statement, statement::prepare_with_catalog};

/// A read-only transaction that owns a fixed snapshot of committed database state.
///
/// The snapshot does not advance when other connections commit. The transaction mutably borrows
/// its connection until dropped, and a [`Rows`] stream mutably borrows the transaction until that
/// stream is dropped.
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

    /// Prepares one owned SQL statement against this transaction's fixed snapshot.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let catalog = self.snapshot.catalog_reader()?;
        prepare_with_catalog(sql, &self.sql_limits, &catalog, self.database_uuid)
    }

    /// Starts a pull-based query against this transaction's fixed snapshot.
    ///
    /// The returned [`Rows`] keeps this transaction borrowed until dropped.
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
///
/// This transaction owns a private write batch and mutably borrows its connection. [`commit`](Self::commit)
/// publishes the batch, [`rollback`](Self::rollback) explicitly discards it and reports any rollback
/// error, and dropping an unfinished transaction attempts rollback without publishing changes.
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
    ///
    /// Statement identity and schema dependencies are revalidated before mutation. Ordinary
    /// statement errors roll back that statement's changes; fatal errors make the transaction
    /// rollback-only.
    pub fn execute(
        &mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<ExecutionResult> {
        let memory_limit = self.max_query_memory_bytes;
        let batch = self.batch_mut()?;
        execute_write_batch(batch, statement, parameters, memory_limit)
    }

    /// Prepares one owned SQL statement against this write batch's current catalog view.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let catalog = self.batch_mut()?.catalog_reader()?;
        prepare_with_catalog(sql, &self.sql_limits, &catalog, self.database_uuid)
    }

    /// Starts a pull-based query against this transaction's current write batch.
    ///
    /// The returned [`Rows`] keeps this transaction borrowed until dropped. A fatal stream error
    /// makes the transaction rollback-only.
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

    /// Consumes the transaction and publishes its complete write batch atomically.
    ///
    /// A failed commit returns its structured error. This is distinct from calling
    /// [`rollback`](Self::rollback), which explicitly requests that the batch be discarded.
    /// Validation or execution errors returned before durability publish no changes, but a
    /// commit/WAL I/O error can occur after durable sync, so the commit outcome is uncertain rather
    /// than known successful or rolled back. Drop the owning [`Database`](crate::Database) and all
    /// handles, then reopen it with [`Database::open`](crate::Database::open) so recovery runs;
    /// [`Database::connect`](crate::Database::connect) is not recovery. Reconcile the observed
    /// database state before retrying the business write.
    pub fn commit(mut self) -> Result<()> {
        self.take_batch()?.commit()
    }

    /// Consumes the transaction and explicitly rolls it back without publishing changes.
    ///
    /// Unlike the best-effort rollback performed by `Drop`, this method reports rollback failure.
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

pub(super) fn execute_write_batch(
    batch: &mut WriteBatch,
    statement: &Statement,
    parameters: Parameters<'_>,
    memory_limit: u64,
) -> Result<ExecutionResult> {
    let validated = match validate_write_execution(batch, statement, parameters, memory_limit) {
        Ok(validated) => validated,
        Err(error) => return Err(classify_explicit_transaction_error(batch, error)),
    };
    let checkpoint = match batch.statement_checkpoint() {
        Ok(checkpoint) => checkpoint,
        Err(error) => return Err(classify_explicit_transaction_error(batch, error)),
    };
    match execute_validated_write_plan(batch, validated) {
        Ok(result) => batch
            .finish_statement(checkpoint)
            .map(|()| result)
            .map_err(|error| classify_explicit_transaction_error(batch, error)),
        Err(error) => {
            batch.restore_statement(checkpoint);
            Err(classify_explicit_transaction_error(batch, error))
        }
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
        DataType, Database, DbError, Decimal, ErrorCode, Options, OracleDate, OracleTimestamp,
        ParameterValue, Parameters, Value,
        storage::{
            catalog::types::{CatalogName, ColumnType, TableDef},
            relation::{
                Relation, fail_table_write_next_after_for_test,
                take_value_validation_limits_for_test,
            },
            test_support::{TempDatabase, phase_4b_add_table, phase_4b_catalog_fixture},
        },
    };

    fn batch_table_contents(
        transaction: &mut super::WriteTransaction<'_>,
        name: &str,
    ) -> (TableDef, Vec<(u64, Vec<Value>)>) {
        let batch = transaction.batch.as_mut().unwrap();
        let catalog = batch.catalog_reader().unwrap();
        let table = catalog
            .find_table_by_name(&CatalogName::new(name).unwrap())
            .unwrap()
            .clone();
        let relation = Relation::open(&catalog, table.id()).unwrap();
        let mut scan = relation.scan(usize::MAX);
        let mut rows = Vec::new();
        while let Some(row) = scan.next_batch(batch).unwrap() {
            rows.push((row.rowid().get(), row.values().to_vec()));
        }
        (table, rows)
    }

    fn query_values(transaction: &mut super::WriteTransaction<'_>, sql: &str) -> Vec<Vec<Value>> {
        let statement = transaction.prepare(sql).unwrap();
        let mut rows = transaction.query(&statement, Parameters::none()).unwrap();
        let mut values = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            values.push(row.values().to_vec());
        }
        values
    }

    #[test]
    fn update_uses_one_old_row_for_every_assignment_and_delete_counts_only_matches() {
        // Production breaks caught: applying A before evaluating B observes a
        // partially new row, revisiting changed values causes Halloween work,
        // and counting scanned DELETE rows overstates the affected result.
        let temp = TempDatabase::new("transaction-update-old-row-delete-count");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create = transaction
            .prepare("CREATE TABLE T (A INT64, B INT64)")
            .unwrap();
        transaction.execute(&create, Parameters::none()).unwrap();
        let insert = transaction
            .prepare("INSERT INTO T VALUES (:1, :2)")
            .unwrap();
        for (a, b) in [(1, 10), (2, 20), (3, 30)] {
            let values = [Value::Int64(a).into(), Value::Int64(b).into()];
            transaction
                .execute(&insert, Parameters::positional(&values))
                .unwrap();
        }

        let update = transaction
            .prepare("UPDATE T SET A = A + 1, B = A WHERE A >= 2")
            .unwrap();
        assert_eq!(
            transaction
                .execute(&update, Parameters::none())
                .unwrap()
                .rows_affected(),
            2
        );
        assert_eq!(
            query_values(&mut transaction, "SELECT A, B FROM T ORDER BY A"),
            vec![
                vec![Value::Int64(1), Value::Int64(10)],
                vec![Value::Int64(3), Value::Int64(2)],
                vec![Value::Int64(4), Value::Int64(3)],
            ]
        );

        let delete = transaction.prepare("DELETE FROM T WHERE A > 2").unwrap();
        assert_eq!(
            transaction
                .execute(&delete, Parameters::none())
                .unwrap()
                .rows_affected(),
            2
        );
        let no_match = transaction.prepare("DELETE FROM T WHERE A > 100").unwrap();
        assert_eq!(
            transaction
                .execute(&no_match, Parameters::none())
                .unwrap()
                .rows_affected(),
            0
        );
        assert_eq!(
            query_values(&mut transaction, "SELECT A, B FROM T"),
            vec![vec![Value::Int64(1), Value::Int64(10)]]
        );
        transaction.rollback().unwrap();
    }

    #[test]
    fn later_row_error_restores_every_earlier_update_and_keeps_transaction_usable() {
        // Production breaks caught: retaining row 1's replacement after row 2
        // divides by zero, or poisoning the transaction for an ordinary Type
        // error, makes the before image or the later INSERT assertion fail.
        let temp = TempDatabase::new("transaction-update-late-error");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create = transaction
            .prepare("CREATE TABLE T (ID INT64, V INT64)")
            .unwrap();
        transaction.execute(&create, Parameters::none()).unwrap();
        let insert = transaction
            .prepare("INSERT INTO T VALUES (:1, :2)")
            .unwrap();
        for (id, value) in [(1, 2), (2, 0), (3, 4)] {
            let values = [Value::Int64(id).into(), Value::Int64(value).into()];
            transaction
                .execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        let before = query_values(&mut transaction, "SELECT ID, V FROM T ORDER BY ID");

        let update = transaction.prepare("UPDATE T SET V = 100 / V").unwrap();
        let error = transaction
            .execute(&update, Parameters::none())
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        assert_eq!(error.sql_location().unwrap().column(), 18);
        assert_eq!(
            query_values(&mut transaction, "SELECT ID, V FROM T ORDER BY ID"),
            before
        );

        let values = [Value::Int64(9).into(), Value::Int64(1).into()];
        transaction
            .execute(&insert, Parameters::positional(&values))
            .unwrap();
        assert_eq!(
            query_values(&mut transaction, "SELECT ID, V FROM T ORDER BY ID"),
            vec![
                vec![Value::Int64(1), Value::Int64(2)],
                vec![Value::Int64(2), Value::Int64(0)],
                vec![Value::Int64(3), Value::Int64(4)],
                vec![Value::Int64(9), Value::Int64(1)],
            ]
        );
        transaction.rollback().unwrap();
    }

    #[test]
    fn mutation_memory_is_bounded_per_row_instead_of_accumulating_the_table() {
        // Production breaks caught: failing to account the old row and pending
        // replacement accepts the first low-budget UPDATE, while retaining
        // every scanned row rejects the larger streaming case.
        let low_temp = TempDatabase::new("transaction-update-row-memory-limit");
        let db = Database::create(low_temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut seed = connection.begin_write().unwrap();
        let create = seed
            .prepare("CREATE TABLE T (ID INT64, PAYLOAD TEXT)")
            .unwrap();
        seed.execute(&create, Parameters::none()).unwrap();
        let insert = seed.prepare("INSERT INTO T VALUES (1, :1)").unwrap();
        let payload = [Value::Text("x".repeat(256)).into()];
        seed.execute(&insert, Parameters::positional(&payload))
            .unwrap();
        seed.commit().unwrap();
        drop(connection);
        drop(db);

        let options = Options::default().with_max_query_memory_bytes(600);
        let db = Database::open(low_temp.path(), options).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let before = batch_table_contents(&mut transaction, "T");
        let update = transaction
            .prepare("UPDATE T SET PAYLOAD = PAYLOAD")
            .unwrap();
        assert_eq!(
            transaction
                .execute(&update, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(batch_table_contents(&mut transaction, "T"), before);
        let create_u = transaction.prepare("CREATE TABLE U (ID INT64)").unwrap();
        transaction.execute(&create_u, Parameters::none()).unwrap();
        transaction.rollback().unwrap();
        drop(connection);
        drop(db);

        let options = Options::default().with_max_query_memory_bytes(900);
        let db = Database::open(low_temp.path(), options).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let before = batch_table_contents(&mut transaction, "T");
        let update = transaction.prepare("UPDATE T SET ID = ID + 1").unwrap();
        assert_eq!(
            transaction
                .execute(&update, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(batch_table_contents(&mut transaction, "T"), before);
        transaction.rollback().unwrap();
        drop(connection);
        drop(db);

        let streaming_temp = TempDatabase::new("transaction-streaming-mutation-memory");
        let db = Database::create(streaming_temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut seed = connection.begin_write().unwrap();
        let create = seed
            .prepare("CREATE TABLE T (ID INT64, PAYLOAD TEXT)")
            .unwrap();
        seed.execute(&create, Parameters::none()).unwrap();
        let insert = seed.prepare("INSERT INTO T VALUES (:1, :2)").unwrap();
        for id in 1_i64..=64 {
            let values = [Value::Int64(id).into(), Value::Text("y".repeat(256)).into()];
            seed.execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        seed.commit().unwrap();
        drop(connection);
        drop(db);

        let options = Options::default().with_max_query_memory_bytes(2_048);
        let db = Database::open(streaming_temp.path(), options).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let update = transaction.prepare("UPDATE T SET ID = ID + 1").unwrap();
        assert_eq!(
            transaction
                .execute(&update, Parameters::none())
                .unwrap()
                .rows_affected(),
            64
        );
        let delete = transaction.prepare("DELETE FROM T WHERE ID > 33").unwrap();
        assert_eq!(
            transaction
                .execute(&delete, Parameters::none())
                .unwrap()
                .rows_affected(),
            32
        );
        assert_eq!(batch_table_contents(&mut transaction, "T").1.len(), 32);
        transaction.rollback().unwrap();
    }

    #[test]
    fn update_preserves_genuine_timestamp_type_errors_under_a_memory_budget() {
        let temp = TempDatabase::new("transaction-update-timestamp-shape");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create = transaction
            .prepare("CREATE TABLE T (TS TIMESTAMP)")
            .unwrap();
        transaction.execute(&create, Parameters::none()).unwrap();
        let insert = transaction.prepare("INSERT INTO T VALUES (NULL)").unwrap();
        transaction.execute(&insert, Parameters::none()).unwrap();
        let before = batch_table_contents(&mut transaction, "T");

        let update = transaction.prepare("UPDATE T SET TS = :1").unwrap();
        let date = OracleDate::new(2026, 9, 15, 12, 0, 0).unwrap();
        let timestamp = OracleTimestamp::new(date, 0, Some(60)).unwrap();
        let values = [Value::Timestamp(timestamp).into()];
        let error = transaction
            .execute(&update, Parameters::positional(&values))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        assert_eq!(error.sql_location().unwrap().column(), 19);
        assert_eq!(batch_table_contents(&mut transaction, "T"), before);
        let create_u = transaction.prepare("CREATE TABLE U (ID INT64)").unwrap();
        transaction.execute(&create_u, Parameters::none()).unwrap();
        transaction.rollback().unwrap();
    }

    #[test]
    fn invalid_wide_update_normalizes_before_diagnostic_validation() {
        // Production break caught: retrying validation with usize::MAX and
        // cloning/encoding one whole candidate row per assignment bypasses the
        // statement budget while diagnosing the final invalid assignment.
        let temp = TempDatabase::new("transaction-update-bounded-diagnostics");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut seed = connection.begin_write().unwrap();
        let create = seed
            .prepare(
                "CREATE TABLE T (C1 TEXT, C2 TEXT, C3 TEXT, C4 TEXT, C5 TEXT, \
                 C6 TEXT, C7 TEXT, C8 TEXT, TS TIMESTAMP)",
            )
            .unwrap();
        seed.execute(&create, Parameters::none()).unwrap();
        let insert = seed
            .prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5, :6, :7, :8, :9)")
            .unwrap();
        let mut values = (0..8)
            .map(|index| Value::Text(char::from(b'a' + index).to_string().repeat(128)).into())
            .collect::<Vec<ParameterValue>>();
        values.push(Value::Null.into());
        seed.execute(&insert, Parameters::positional(&values))
            .unwrap();
        seed.commit().unwrap();
        drop(connection);
        drop(db);

        let options = Options::default().with_max_query_memory_bytes(5_000);
        let db = Database::open(temp.path(), options).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let before = batch_table_contents(&mut transaction, "T");
        let update = transaction
            .prepare(
                "UPDATE T SET C1 = C1, C2 = C2, C3 = C3, C4 = C4, C5 = C5, \
                 C6 = C6, C7 = C7, C8 = C8, TS = :1",
            )
            .unwrap();
        let date = OracleDate::new(2026, 9, 15, 12, 0, 0).unwrap();
        let timestamp = OracleTimestamp::new(date, 0, Some(60)).unwrap();
        let parameters = [Value::Timestamp(timestamp).into()];

        take_value_validation_limits_for_test();
        let error = transaction
            .execute(&update, Parameters::positional(&parameters))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        let validation_limits = take_value_validation_limits_for_test();
        assert!(validation_limits.is_empty());
        assert_eq!(batch_table_contents(&mut transaction, "T"), before);
        transaction.rollback().unwrap();
    }

    #[test]
    fn parameterized_mutations_keep_rowids_and_sql_three_valued_predicates() {
        // Production breaks caught: scanning by a changed visible value can
        // revisit rows, FALSE/NULL predicates can be miscounted, and DELETE
        // must not make a committed ROWID available to a later INSERT.
        let temp = TempDatabase::new("transaction-parameterized-update-delete-rowids");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create = transaction
            .prepare("CREATE TABLE T (ID INT64, V INT64, FLAG BOOL)")
            .unwrap();
        transaction.execute(&create, Parameters::none()).unwrap();
        let insert = transaction
            .prepare("INSERT INTO T VALUES (:1, :2, :3)")
            .unwrap();
        for (id, flag) in [
            (1, Value::Bool(true)),
            (2, Value::Bool(false)),
            (3, Value::Null),
        ] {
            let values = [
                Value::Int64(id).into(),
                Value::Int64(id * 10).into(),
                flag.into(),
            ];
            transaction
                .execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        let before_rowids: Vec<_> = batch_table_contents(&mut transaction, "T")
            .1
            .into_iter()
            .map(|row| row.0)
            .collect();

        let update = transaction
            .prepare("UPDATE T SET ID = ID + :inc, V = ID WHERE ID < :limit")
            .unwrap();
        let parameters = [
            ("inc", Value::Int64(1).into()),
            ("limit", Value::Int64(4).into()),
        ];
        assert_eq!(
            transaction
                .execute(&update, Parameters::named(&parameters))
                .unwrap()
                .rows_affected(),
            3
        );
        let after_update = batch_table_contents(&mut transaction, "T").1;
        assert_eq!(
            after_update.iter().map(|row| row.0).collect::<Vec<_>>(),
            before_rowids
        );
        assert_eq!(
            after_update.iter().map(|row| &row.1).collect::<Vec<_>>(),
            vec![
                &vec![Value::Int64(2), Value::Int64(1), Value::Bool(true)],
                &vec![Value::Int64(3), Value::Int64(2), Value::Bool(false)],
                &vec![Value::Int64(4), Value::Int64(3), Value::Null],
            ]
        );

        let null_update = transaction
            .prepare("UPDATE T SET V = 999 WHERE NULL")
            .unwrap();
        assert_eq!(
            transaction
                .execute(&null_update, Parameters::none())
                .unwrap()
                .rows_affected(),
            0
        );
        let delete_true = transaction.prepare("DELETE FROM T WHERE FLAG").unwrap();
        assert_eq!(
            transaction
                .execute(&delete_true, Parameters::none())
                .unwrap()
                .rows_affected(),
            1
        );
        let delete_parameterized = transaction
            .prepare("DELETE FROM T WHERE ID >= :minimum")
            .unwrap();
        let minimum = [("minimum", Value::Int64(100).into())];
        assert_eq!(
            transaction
                .execute(&delete_parameterized, Parameters::named(&minimum))
                .unwrap()
                .rows_affected(),
            0
        );

        let insert_one = transaction
            .prepare("INSERT INTO T VALUES (9, 9, FALSE)")
            .unwrap();
        transaction
            .execute(&insert_one, Parameters::none())
            .unwrap();
        let (_, rows) = batch_table_contents(&mut transaction, "T");
        assert_eq!(rows.iter().map(|row| row.0).collect::<Vec<_>>(), [2, 3, 4]);
        assert_eq!(
            query_values(&mut transaction, "SELECT ID, V FROM T ORDER BY ID"),
            vec![
                vec![Value::Int64(3), Value::Int64(2)],
                vec![Value::Int64(4), Value::Int64(3)],
                vec![Value::Int64(9), Value::Int64(9)],
            ]
        );
        transaction.commit().unwrap();
        drop(connection);
        drop(db);

        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut reopened = connection.begin_write().unwrap();
        assert_eq!(
            query_values(&mut reopened, "SELECT ID, V FROM T ORDER BY ID"),
            vec![
                vec![Value::Int64(3), Value::Int64(2)],
                vec![Value::Int64(4), Value::Int64(3)],
                vec![Value::Int64(9), Value::Int64(9)],
            ]
        );
        assert_eq!(
            batch_table_contents(&mut reopened, "T")
                .1
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
        reopened.rollback().unwrap();
    }

    #[test]
    fn later_row_decode_limit_restores_prior_update_and_can_commit_later_work() {
        // Production breaks caught: decoding with an unbounded limit misses the
        // late size error, while restoring only the current row leaves row 1
        // changed or prevents a later ordinary statement from committing.
        let temp = TempDatabase::new("transaction-update-late-row-size");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut seed = connection.begin_write().unwrap();
        let create = seed
            .prepare("CREATE TABLE T (ID INT64, PAYLOAD TEXT)")
            .unwrap();
        seed.execute(&create, Parameters::none()).unwrap();
        let insert = seed.prepare("INSERT INTO T VALUES (:1, :2)").unwrap();
        for (id, payload) in [(1, "a".to_owned()), (2, "z".repeat(512))] {
            let values = [Value::Int64(id).into(), Value::Text(payload).into()];
            seed.execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        seed.commit().unwrap();
        drop(connection);
        drop(db);

        let options = Options::default().with_max_query_memory_bytes(400);
        let db = Database::open(temp.path(), options).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let before = batch_table_contents(&mut transaction, "T");
        let update = transaction.prepare("UPDATE T SET ID = ID + 1").unwrap();
        assert_eq!(
            transaction
                .execute(&update, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(batch_table_contents(&mut transaction, "T"), before);
        let delete = transaction.prepare("DELETE FROM T").unwrap();
        assert_eq!(
            transaction
                .execute(&delete, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(batch_table_contents(&mut transaction, "T"), before);
        let create_u = transaction.prepare("CREATE TABLE U (ID INT64)").unwrap();
        transaction.execute(&create_u, Parameters::none()).unwrap();
        transaction.commit().unwrap();
        drop(connection);
        drop(db);

        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut reopened = connection.begin_write().unwrap();
        assert_eq!(batch_table_contents(&mut reopened, "T"), before);
        assert!(reopened.prepare("SELECT ID FROM U").is_ok());
        reopened.rollback().unwrap();
    }

    #[test]
    fn update_across_split_tree_and_delete_collapse_rollback_commit_and_reopen_atomically() {
        // Production breaks caught: losing TableWrite's changed root on finish,
        // failing to restore split/collapse page state, changing ROWIDs during
        // replacement, or recycling a deleted ROWID breaks these snapshots.
        let temp = TempDatabase::new("transaction-update-split-delete-collapse");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut seed = connection.begin_write().unwrap();
        let create = seed
            .prepare("CREATE TABLE T (ID INT64, PAYLOAD TEXT)")
            .unwrap();
        seed.execute(&create, Parameters::none()).unwrap();
        let insert = seed.prepare("INSERT INTO T VALUES (:1, :2)").unwrap();
        for id in 1_i64..=72 {
            let values = [Value::Int64(id).into(), Value::Text("s".into()).into()];
            seed.execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        seed.commit().unwrap();

        let mut update_rollback = connection.begin_write().unwrap();
        let before = batch_table_contents(&mut update_rollback, "T");
        let original_rowids: Vec<_> = before.1.iter().map(|row| row.0).collect();
        let update = update_rollback
            .prepare("UPDATE T SET PAYLOAD = :payload")
            .unwrap();
        let payload = [("payload", Value::Text("x".repeat(128)).into())];
        assert_eq!(
            update_rollback
                .execute(&update, Parameters::named(&payload))
                .unwrap()
                .rows_affected(),
            72
        );
        let expanded = batch_table_contents(&mut update_rollback, "T");
        assert_eq!(expanded.0.next_rowid(), before.0.next_rowid());
        assert_eq!(
            expanded.1.iter().map(|row| row.0).collect::<Vec<_>>(),
            original_rowids
        );
        assert!(
            expanded
                .1
                .iter()
                .all(|row| { row.1[1] == Value::Text("x".repeat(128)) })
        );
        update_rollback.rollback().unwrap();

        let mut update_commit = connection.begin_write().unwrap();
        assert_eq!(batch_table_contents(&mut update_commit, "T"), before);
        let update = update_commit
            .prepare("UPDATE T SET PAYLOAD = :payload")
            .unwrap();
        update_commit
            .execute(&update, Parameters::named(&payload))
            .unwrap();
        update_commit.commit().unwrap();
        drop(connection);
        drop(db);

        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut delete_rollback = connection.begin_write().unwrap();
        let committed_expanded = batch_table_contents(&mut delete_rollback, "T");
        assert_eq!(
            committed_expanded
                .1
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            original_rowids
        );
        let delete = delete_rollback
            .prepare("DELETE FROM T WHERE ID < 72")
            .unwrap();
        assert_eq!(
            delete_rollback
                .execute(&delete, Parameters::none())
                .unwrap()
                .rows_affected(),
            71
        );
        let collapsed = batch_table_contents(&mut delete_rollback, "T");
        assert_ne!(collapsed.0.root(), committed_expanded.0.root());
        assert_eq!(
            collapsed.1,
            vec![(72, vec![Value::Int64(72), Value::Text("x".repeat(128))],)]
        );
        delete_rollback.rollback().unwrap();

        let mut delete_commit = connection.begin_write().unwrap();
        assert_eq!(
            batch_table_contents(&mut delete_commit, "T"),
            committed_expanded
        );
        let delete = delete_commit
            .prepare("DELETE FROM T WHERE ID < 72")
            .unwrap();
        delete_commit.execute(&delete, Parameters::none()).unwrap();
        delete_commit.commit().unwrap();
        drop(connection);
        drop(db);

        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut reopened = connection.begin_write().unwrap();
        let reopened_collapsed = batch_table_contents(&mut reopened, "T");
        assert_eq!(reopened_collapsed, collapsed);
        let insert = reopened
            .prepare("INSERT INTO T VALUES (100, 'new')")
            .unwrap();
        reopened.execute(&insert, Parameters::none()).unwrap();
        assert_eq!(
            batch_table_contents(&mut reopened, "T")
                .1
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            [72, 73]
        );
        reopened.rollback().unwrap();
    }

    #[test]
    fn later_delete_error_restores_collapsed_root_releases_and_all_rows() {
        // Production breaks caught: a late predicate error after earlier
        // deletes can retain a collapsed root, staged page releases, or a
        // subset of rows unless the outer statement checkpoint is restored.
        let temp = TempDatabase::new("transaction-delete-late-error-collapse-restore");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut seed = connection.begin_write().unwrap();
        let create = seed
            .prepare("CREATE TABLE T (ID INT64, V INT64, PAYLOAD TEXT)")
            .unwrap();
        seed.execute(&create, Parameters::none()).unwrap();
        let insert = seed.prepare("INSERT INTO T VALUES (:1, :2, :3)").unwrap();
        for id in 1_i64..=72 {
            let values = [
                Value::Int64(id).into(),
                Value::Int64(if id == 72 { 0 } else { 1 }).into(),
                Value::Text("x".repeat(128)).into(),
            ];
            seed.execute(&insert, Parameters::positional(&values))
                .unwrap();
        }
        seed.commit().unwrap();

        let mut transaction = connection.begin_write().unwrap();
        let before = batch_table_contents(&mut transaction, "T");
        let delete = transaction
            .prepare("DELETE FROM T WHERE 100 / V > 0")
            .unwrap();
        let error = transaction
            .execute(&delete, Parameters::none())
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        assert_eq!(error.sql_location().unwrap().column(), 21);
        assert_eq!(batch_table_contents(&mut transaction, "T"), before);

        let delete = transaction.prepare("DELETE FROM T WHERE ID < 72").unwrap();
        assert_eq!(
            transaction
                .execute(&delete, Parameters::none())
                .unwrap()
                .rows_affected(),
            71
        );
        let collapsed = batch_table_contents(&mut transaction, "T");
        assert_ne!(collapsed.0.root(), before.0.root());
        assert_eq!(collapsed.0.next_rowid(), before.0.next_rowid());
        assert_eq!(collapsed.1.len(), 1);
        assert_eq!(collapsed.1[0].0, 72);
        transaction.rollback().unwrap();

        let mut reopened = connection.begin_write().unwrap();
        assert_eq!(batch_table_contents(&mut reopened, "T"), before);
        reopened.rollback().unwrap();
    }

    #[test]
    fn fatal_later_row_errors_restore_then_require_explicit_rollback() {
        // Production break caught: omitting any fatal class leaves prepare and
        // commit reachable instead of making the transaction rollback-only.
        // Checkpoint state itself is covered by the lower storage-layer tests.
        for code in [ErrorCode::Io, ErrorCode::Corruption, ErrorCode::Internal] {
            let temp = TempDatabase::new("transaction-update-late-fatal");
            let db = Database::create(temp.path(), Options::default()).unwrap();
            let mut connection = db.connect().unwrap();
            let mut seed = connection.begin_write().unwrap();
            let create = seed.prepare("CREATE TABLE T (ID INT64)").unwrap();
            seed.execute(&create, Parameters::none()).unwrap();
            let insert = seed.prepare("INSERT INTO T VALUES (:1)").unwrap();
            for id in [1_i64, 2] {
                let values = [Value::Int64(id).into()];
                seed.execute(&insert, Parameters::positional(&values))
                    .unwrap();
            }
            seed.commit().unwrap();

            let mut transaction = connection.begin_write().unwrap();
            let before = batch_table_contents(&mut transaction, "T");
            let update = transaction.prepare("UPDATE T SET ID = ID + 10").unwrap();
            fail_table_write_next_after_for_test(1, code);
            assert_eq!(
                transaction
                    .execute(&update, Parameters::none())
                    .unwrap_err()
                    .code(),
                code
            );
            assert_eq!(
                transaction.prepare("SELECT ID FROM T").unwrap_err().code(),
                ErrorCode::Transaction
            );
            transaction.rollback().unwrap();

            let mut reopened = connection.begin_write().unwrap();
            assert_eq!(batch_table_contents(&mut reopened, "T"), before);
            reopened.rollback().unwrap();
        }
    }

    #[test]
    fn insert_maps_targets_parameters_and_omitted_columns() {
        // Production break caught: writing source-order values instead of target
        // ordinals swaps ID/NAME, and failing to fill omitted NOTE loses SQL NULL.
        let temp = TempDatabase::new("transaction-insert-targets");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create = transaction
            .prepare("CREATE TABLE T (ID INT64, NAME TEXT, NOTE TEXT)")
            .unwrap();
        transaction.execute(&create, Parameters::none()).unwrap();

        let positional = transaction
            .prepare("INSERT INTO T (ID, NAME) VALUES (:1, :2)")
            .unwrap();
        let positional_values = [Value::Int64(7).into(), Value::Text("A".into()).into()];
        assert_eq!(
            transaction
                .execute(&positional, Parameters::positional(&positional_values))
                .unwrap()
                .rows_affected(),
            1
        );

        let named = transaction
            .prepare("INSERT INTO T (NAME, ID) VALUES (:NAME, :ID)")
            .unwrap();
        let named_values = [
            ("id", Value::Int64(8).into()),
            ("name", Value::Text("B".into()).into()),
        ];
        assert_eq!(
            transaction
                .execute(&named, Parameters::named(&named_values))
                .unwrap()
                .rows_affected(),
            1
        );

        let select = transaction
            .prepare("SELECT ID, NAME, NOTE FROM T ORDER BY ID")
            .unwrap();
        let mut rows = transaction.query(&select, Parameters::none()).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Int64(7), Value::Text("A".into()), Value::Null]
        );
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Int64(8), Value::Text("B".into()), Value::Null]
        );
        assert!(rows.next().unwrap().is_none());
        drop(rows);
        transaction.rollback().unwrap();
    }

    #[test]
    fn failed_insert_does_not_consume_rowid_or_change_the_batch() {
        // Production break caught: allocating ROWID before full value validation
        // makes an ordinary failed INSERT leave the first successful row at 2.
        let temp = TempDatabase::new("transaction-insert-failed-rowid");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create = transaction.prepare("CREATE TABLE T (ID INT64)").unwrap();
        transaction.execute(&create, Parameters::none()).unwrap();
        let insert = transaction.prepare("INSERT INTO T VALUES (:1)").unwrap();

        let bad = [Value::Text("wrong".into()).into()];
        assert_eq!(
            transaction
                .execute(&insert, Parameters::positional(&bad))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        let good = [Value::Int64(1).into()];
        transaction
            .execute(&insert, Parameters::positional(&good))
            .unwrap();

        let batch = transaction.batch.as_mut().unwrap();
        let catalog = batch.catalog_reader().unwrap();
        let table = catalog
            .find_table_by_name(&CatalogName::new("T").unwrap())
            .unwrap();
        let relation = Relation::open(&catalog, table.id()).unwrap();
        let mut scan = relation.scan(usize::MAX);
        let row = scan.next_batch(batch).unwrap().unwrap();
        assert_eq!(row.rowid().get(), 1);
        assert_eq!(row.values(), &[Value::Int64(1)]);
        assert!(scan.next_batch(batch).unwrap().is_none());
        transaction.rollback().unwrap();
    }

    #[test]
    fn insert_enforces_exact_parameters_and_reuses_a_named_value() {
        // Production break caught: accepting an inexact frame mutates before
        // validation, while evaluating a repeated parameter only once can omit
        // its second target or fail the safe INT64-to-DECIMAL assignment.
        let fixture = phase_4b_catalog_fixture(4096);
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let named = transaction
            .prepare("INSERT INTO USERS (ID, AMOUNT) VALUES (:shared, :shared)")
            .unwrap();
        let before = batch_table_contents(&mut transaction, "USERS");

        assert_eq!(
            transaction
                .execute(&named, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        let extra = [
            ("shared", Value::Int64(9).into()),
            ("extra", Value::Int64(9).into()),
        ];
        assert_eq!(
            transaction
                .execute(&named, Parameters::named(&extra))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        let duplicate = [
            ("shared", Value::Int64(9).into()),
            (":SHARED", Value::Int64(9).into()),
        ];
        assert_eq!(
            transaction
                .execute(&named, Parameters::named(&duplicate))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        let positional = [Value::Int64(9).into()];
        assert_eq!(
            transaction
                .execute(&named, Parameters::positional(&positional))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        let wrong_type = [("shared", Value::Text("nine".into()).into())];
        assert_eq!(
            transaction
                .execute(&named, Parameters::named(&wrong_type))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        let wrong_typed_null = [(
            "shared",
            ParameterValue::typed_null(DataType::Text).unwrap(),
        )];
        assert_eq!(
            transaction
                .execute(&named, Parameters::named(&wrong_typed_null))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        let float_insert = transaction
            .prepare("INSERT INTO USERS (ID, FLOAT_COL) VALUES (1, :value)")
            .unwrap();
        let nonfinite = [("value", Value::Float64(f64::INFINITY).into())];
        assert_eq!(
            transaction
                .execute(&float_insert, Parameters::named(&nonfinite))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(batch_table_contents(&mut transaction, "USERS"), before);

        let shared = [("SHARED", Value::Int64(9).into())];
        assert_eq!(
            transaction
                .execute(&named, Parameters::named(&shared))
                .unwrap()
                .rows_affected(),
            1
        );
        let (_, rows) = batch_table_contents(&mut transaction, "USERS");
        assert_eq!(
            rows,
            vec![(
                1,
                vec![
                    Value::Int64(9),
                    Value::Null,
                    Value::Decimal(Decimal::new(false, 0, 9).unwrap()),
                    Value::Null,
                ],
            )]
        );
        transaction.rollback().unwrap();
    }

    #[test]
    fn insert_typed_null_and_expression_errors_are_atomic_and_located() {
        // Production break caught: skipping complete-row validation accepts a
        // NULL ID; losing expression locations makes arithmetic failures opaque.
        let fixture = phase_4b_catalog_fixture(4096);
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let nullable = transaction
            .prepare("INSERT INTO USERS (ID, NAME) VALUES (:1, :2)")
            .unwrap();
        let nullable_values = [
            Value::Int64(1).into(),
            ParameterValue::typed_null(DataType::Text).unwrap(),
        ];
        transaction
            .execute(&nullable, Parameters::positional(&nullable_values))
            .unwrap();

        let not_null = transaction
            .prepare("INSERT INTO USERS (ID) VALUES (:1)")
            .unwrap();
        let typed_null = [ParameterValue::typed_null(DataType::Int64).unwrap()];
        assert_eq!(
            transaction
                .execute(&not_null, Parameters::positional(&typed_null))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        let untyped_null = [Value::Null.into()];
        assert_eq!(
            transaction
                .execute(&not_null, Parameters::positional(&untyped_null))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );

        let arithmetic = transaction
            .prepare("INSERT INTO USERS (ID) VALUES (:1 / :2)")
            .unwrap();
        let divide_by_zero = [Value::Int64(1).into(), Value::Int64(0).into()];
        let error = transaction
            .execute(&arithmetic, Parameters::positional(&divide_by_zero))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Type);
        let location = error.sql_location().unwrap();
        assert_eq!((location.line(), location.column()), (1, 32));

        let good = [Value::Int64(2).into()];
        transaction
            .execute(&not_null, Parameters::positional(&good))
            .unwrap();
        let (_, rows) = batch_table_contents(&mut transaction, "USERS");
        assert_eq!(rows.len(), 2);
        assert_eq!([rows[0].0, rows[1].0], [1, 2]);
        assert_eq!(rows[0].1[1], Value::Null);
        assert_eq!(rows[1].1[0], Value::Int64(2));
        transaction.rollback().unwrap();
    }

    #[test]
    fn insert_row_size_failure_leaves_rowid_one_available() {
        // Production break caught: applying the row-size limit only after
        // TableWrite opens can consume ROWID or alter the row-tree root.
        let fixture = phase_4b_catalog_fixture(4096);
        let options = Options::default().with_max_query_memory_bytes(256);
        let db = Database::open(fixture.main_path(), options).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let oversized_sql = format!(
            "INSERT INTO USERS (ID, NAME) VALUES (1, '{}')",
            "x".repeat(220)
        );
        let oversized = transaction.prepare(&oversized_sql).unwrap();
        let before = batch_table_contents(&mut transaction, "USERS");
        assert_eq!(
            transaction
                .execute(&oversized, Parameters::none())
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(batch_table_contents(&mut transaction, "USERS"), before);

        let good = transaction
            .prepare("INSERT INTO USERS (ID) VALUES (1)")
            .unwrap();
        transaction.execute(&good, Parameters::none()).unwrap();
        let (_, rows) = batch_table_contents(&mut transaction, "USERS");
        assert_eq!(rows[0].0, 1);
        transaction.rollback().unwrap();
    }

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
    fn constrained_ddl_rolls_back_and_commits_with_explicit_transaction() {
        // Production break caught: the Task 4 temporary boundary rejected a
        // valid constrained CREATE, while a partial Task 5 write could escape
        // rollback or fail to make the committed schema visible.
        let temp = TempDatabase::new("transaction-constrained-create");
        let db = Database::create(temp.path(), Options::default()).unwrap();
        drop(db);
        let before = std::fs::read(temp.path()).unwrap();
        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut rollback = connection.begin_write().unwrap();
        let constrained = rollback
            .prepare("CREATE TABLE T (ID INT64 PRIMARY KEY)")
            .unwrap();
        assert_eq!(
            rollback
                .execute(&constrained, Parameters::none())
                .unwrap()
                .rows_affected(),
            0
        );
        rollback.rollback().unwrap();
        assert_eq!(
            connection.prepare("SELECT ID FROM T").unwrap_err().code(),
            ErrorCode::Bind
        );
        drop(connection);
        drop(db);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);

        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut commit = connection.begin_write().unwrap();
        let constrained = commit
            .prepare("CREATE TABLE T (ID INT64 PRIMARY KEY)")
            .unwrap();
        assert_eq!(
            commit
                .execute(&constrained, Parameters::none())
                .unwrap()
                .rows_affected(),
            0
        );
        commit.commit().unwrap();
        assert!(connection.prepare("SELECT ID FROM T").is_ok());
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
    fn create_index_rollback_leaves_files_unchanged_and_commit_survives_reopen() {
        // Publication before commit would change the database on rollback;
        // omitted publication would make the name available after committed reopen.
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
        let create_index = transaction.prepare("CREATE INDEX IX_T ON T (ID)").unwrap();
        assert_eq!(
            transaction
                .execute(&create_index, Parameters::none())
                .unwrap()
                .rows_affected(),
            0
        );
        transaction.rollback().unwrap();
        assert!(connection.prepare("CREATE INDEX IX_T ON T (ID)").is_ok());
        drop(connection);
        drop(db);
        assert_eq!(std::fs::read(temp.path()).unwrap(), before);

        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let create_index = transaction.prepare("CREATE INDEX IX_T ON T (ID)").unwrap();
        transaction
            .execute(&create_index, Parameters::none())
            .unwrap();
        transaction.commit().unwrap();
        drop(connection);
        drop(db);
        let db = Database::open(temp.path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        assert_eq!(
            connection
                .prepare("CREATE INDEX IX_T ON T (ID)")
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
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
