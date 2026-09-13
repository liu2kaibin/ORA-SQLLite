use std::{fmt, marker::PhantomData};

use crate::{
    DbError, ErrorCode, Result, SqlLimits,
    storage::{ReadSnapshot, WriteBatch},
};

use super::{Connection, Statement, statement::prepare_with_catalog};

/// A stable, read-only view of committed database state.
pub struct ReadTransaction<'connection> {
    snapshot: ReadSnapshot,
    sql_limits: SqlLimits,
    database_uuid: [u8; 16],
    _connection: PhantomData<&'connection mut Connection>,
}

impl<'connection> ReadTransaction<'connection> {
    pub(super) const fn new(
        snapshot: ReadSnapshot,
        sql_limits: SqlLimits,
        database_uuid: [u8; 16],
    ) -> Self {
        Self {
            snapshot,
            sql_limits,
            database_uuid,
            _connection: PhantomData,
        }
    }

    /// Prepares one SQL statement against this transaction's fixed snapshot.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let catalog = self.snapshot.catalog_reader()?;
        prepare_with_catalog(sql, &self.sql_limits, &catalog, self.database_uuid)
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
    _connection: PhantomData<&'connection mut Connection>,
}

impl<'connection> WriteTransaction<'connection> {
    pub(super) const fn new(
        batch: WriteBatch,
        sql_limits: SqlLimits,
        database_uuid: [u8; 16],
    ) -> Self {
        Self {
            batch: Some(batch),
            sql_limits,
            database_uuid,
            _connection: PhantomData,
        }
    }

    /// Prepares one SQL statement against this write batch's catalog view.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let catalog = self.batch_mut()?.catalog_reader()?;
        prepare_with_catalog(sql, &self.sql_limits, &catalog, self.database_uuid)
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
}
