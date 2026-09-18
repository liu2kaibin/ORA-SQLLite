use std::fmt;

use crate::sql::exec::{
    ExecutionContext, ExecutionStorage, OutputOperator, ParameterFrame, QueryExecution, WorkMemory,
};
use crate::storage::{ReadSnapshot, WriteBatch};
use crate::{Result, Value};

/// One row returned by statement execution.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    values: Box<[Value]>,
}

impl Row {
    pub(crate) fn new(values: Vec<Value>) -> Self {
        Self {
            values: values.into_boxed_slice(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_values_for_test(values: Vec<Value>) -> Self {
        Self::new(values)
    }

    pub const fn len(&self) -> usize {
        self.values.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }
}

enum RowsStorage<'a> {
    OwnedRead(ReadSnapshot),
    BorrowedRead(&'a mut ReadSnapshot),
    BorrowedWrite(&'a mut WriteBatch),
}

/// A pull-based result stream that owns each returned row.
pub struct Rows<'a> {
    storage: RowsStorage<'a>,
    root: Box<dyn OutputOperator>,
    params: ParameterFrame,
    memory: WorkMemory,
}

impl fmt::Debug for Rows<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Rows").finish_non_exhaustive()
    }
}

impl<'a> Rows<'a> {
    pub(crate) fn owned_read(snapshot: ReadSnapshot, execution: QueryExecution) -> Self {
        Self::new(RowsStorage::OwnedRead(snapshot), execution)
    }

    pub(crate) fn borrowed_read(snapshot: &'a mut ReadSnapshot, execution: QueryExecution) -> Self {
        Self::new(RowsStorage::BorrowedRead(snapshot), execution)
    }

    pub(crate) fn borrowed_write(batch: &'a mut WriteBatch, execution: QueryExecution) -> Self {
        Self::new(RowsStorage::BorrowedWrite(batch), execution)
    }

    fn new(storage: RowsStorage<'a>, execution: QueryExecution) -> Self {
        let (root, params, memory) = execution.into_parts();
        Self {
            storage,
            root,
            params,
            memory,
        }
    }

    #[allow(
        clippy::should_implement_trait,
        reason = "the streaming API returns Result<Option<Row>>, which cannot implement Iterator::next"
    )]
    pub fn next(&mut self) -> Result<Option<Row>> {
        let projected = {
            let storage = match &mut self.storage {
                RowsStorage::OwnedRead(snapshot) => ExecutionStorage::Read(snapshot),
                RowsStorage::BorrowedRead(snapshot) => ExecutionStorage::Read(snapshot),
                RowsStorage::BorrowedWrite(batch) => ExecutionStorage::Write(batch),
            };
            let mut context = ExecutionContext::new(storage, &self.params, &mut self.memory);
            self.root.next(&mut context)
        };
        let result = match projected {
            Ok(Some(projected)) => {
                let (row, memory_bytes) = projected.into_output();
                self.memory.release(memory_bytes).map(|()| Some(row))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(error),
        };
        if let Err(error) = &result {
            if error.is_fatal() {
                if let RowsStorage::BorrowedWrite(batch) = &mut self.storage {
                    batch.mark_rollback_only();
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::exec::ProjectedRow;
    use crate::storage::test_support::phase_4b_catalog_fixture;
    use crate::{ErrorCode, Parameters};

    struct ReleaseUnderflow(bool);

    impl OutputOperator for ReleaseUnderflow {
        fn next(&mut self, _context: &mut ExecutionContext<'_>) -> Result<Option<ProjectedRow>> {
            if std::mem::replace(&mut self.0, false) {
                Ok(Some(ProjectedRow::from_output_for_test(
                    Row::new(Vec::new()),
                    1,
                )))
            } else {
                Ok(None)
            }
        }
    }

    #[test]
    fn fatal_output_release_error_makes_write_batch_rollback_only() {
        // Production break caught: propagating a release failure before it is
        // classified leaves a borrowed write batch able to continue or commit.
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut batch = pager.begin_write().unwrap();
        let params =
            ParameterFrame::bind(&[], Parameters::none(), &mut WorkMemory::new(64).unwrap())
                .unwrap();
        let mut rows = Rows {
            storage: RowsStorage::BorrowedWrite(&mut batch),
            root: Box::new(ReleaseUnderflow(true)),
            params,
            memory: WorkMemory::new(64).unwrap(),
        };

        assert_eq!(rows.next().unwrap_err().code(), ErrorCode::Internal);
        drop(rows);
        assert_eq!(
            batch.catalog_reader().err().unwrap().code(),
            ErrorCode::Transaction
        );
        batch.rollback().unwrap();
    }
}
