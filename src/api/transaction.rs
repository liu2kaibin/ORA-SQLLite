use std::{fmt, marker::PhantomData};

use crate::{
    DbError, ErrorCode, Result,
    storage::{ReadSnapshot, WriteBatch},
};

use super::Connection;

/// A stable, read-only view of committed database state.
pub struct ReadTransaction<'connection> {
    #[allow(
        dead_code,
        reason = "snapshot reads are exposed by later query API tasks"
    )]
    snapshot: ReadSnapshot,
    _connection: PhantomData<&'connection mut Connection>,
}

impl<'connection> ReadTransaction<'connection> {
    pub(super) const fn new(snapshot: ReadSnapshot) -> Self {
        Self {
            snapshot,
            _connection: PhantomData,
        }
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
    batch: Option<WriteBatch<'connection>>,
}

impl<'connection> WriteTransaction<'connection> {
    pub(super) const fn new(batch: WriteBatch<'connection>) -> Self {
        Self { batch: Some(batch) }
    }

    /// Commits this transaction.
    pub fn commit(mut self) -> Result<()> {
        self.take_batch()?.commit()
    }

    /// Rolls this transaction back without publishing changes.
    pub fn rollback(mut self) -> Result<()> {
        self.take_batch()?.rollback()
    }

    fn take_batch(&mut self) -> Result<WriteBatch<'connection>> {
        self.batch.take().ok_or_else(|| {
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
