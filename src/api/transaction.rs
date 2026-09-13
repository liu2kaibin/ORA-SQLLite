use std::{fmt, marker::PhantomData};

use crate::{
    DbError, ErrorCode, Parameters, Result, Rows, SqlLimits,
    sql::exec::prepare_query,
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
        let catalog = batch.catalog_reader()?;
        let execution = prepare_query(
            statement,
            parameters,
            &catalog,
            database_uuid,
            max_query_memory_bytes,
        )?;
        Ok(Rows::borrowed_write(batch, execution))
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
    use crate::{
        Database, ErrorCode, Options,
        storage::{
            catalog::types::ColumnType,
            test_support::{phase_4b_add_table, phase_4b_catalog_fixture},
        },
    };

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
}
