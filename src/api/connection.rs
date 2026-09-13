use std::{cell::Cell, fmt, marker::PhantomData};

use crate::sql::exec::prepare_query;
use crate::{Parameters, Result, Rows, SqlLimits, storage::Pager};

use super::{ReadTransaction, Statement, WriteTransaction, statement::prepare_with_catalog};

/// A single database session used to start transactions.
pub struct Connection {
    pager: Pager,
    sql_limits: SqlLimits,
    max_query_memory_bytes: u64,
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
            _not_sync: PhantomData,
        }
    }

    /// Prepares one SQL statement against a short-lived current catalog snapshot.
    pub fn prepare(&mut self, sql: &str) -> Result<Statement> {
        let database_uuid = self.pager.database_uuid()?;
        let mut snapshot = self.pager.begin_read()?;
        let catalog = snapshot.catalog_reader()?;
        prepare_with_catalog(sql, &self.sql_limits, &catalog, database_uuid)
    }

    /// Starts a pull-based query at the current committed snapshot.
    pub fn query<'connection>(
        &'connection mut self,
        statement: &Statement,
        parameters: Parameters<'_>,
    ) -> Result<Rows<'connection>> {
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
        let batch = self.pager.try_begin_write()?;
        let database_uuid = batch.database_uuid();
        Ok(WriteTransaction::new(
            batch,
            self.sql_limits,
            database_uuid,
            self.max_query_memory_bytes,
        ))
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Connection").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc::{self, RecvTimeoutError},
        time::Duration,
    };

    use crate::{Database, ErrorCode, Options, storage::test_support::TempDatabase};

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
