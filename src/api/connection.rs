use std::{cell::Cell, fmt, marker::PhantomData};

use crate::{Result, SqlLimits, storage::Pager};

use super::{ReadTransaction, WriteTransaction};

/// A single database session used to start transactions.
pub struct Connection {
    pager: Pager,
    #[allow(
        dead_code,
        reason = "connection SQL limits are consumed by later statement-binding tasks"
    )]
    sql_limits: SqlLimits,
    _not_sync: PhantomData<Cell<()>>,
}

impl Connection {
    pub(super) const fn new(pager: Pager, sql_limits: SqlLimits) -> Self {
        Self {
            pager,
            sql_limits,
            _not_sync: PhantomData,
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
        Ok(ReadTransaction::new(self.pager.begin_read()?))
    }

    /// Starts an exclusive write transaction.
    pub fn begin_write(&mut self) -> Result<WriteTransaction<'_>> {
        Ok(WriteTransaction::new(self.pager.begin_write()?))
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Connection").finish_non_exhaustive()
    }
}
