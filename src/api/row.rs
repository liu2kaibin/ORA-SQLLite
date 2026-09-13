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
        let storage = match &mut self.storage {
            RowsStorage::OwnedRead(snapshot) => ExecutionStorage::Read(snapshot),
            RowsStorage::BorrowedRead(snapshot) => ExecutionStorage::Read(snapshot),
            RowsStorage::BorrowedWrite(batch) => ExecutionStorage::Write(batch),
        };
        let mut context = ExecutionContext::new(storage, &self.params, &mut self.memory);
        self.root
            .next(&mut context)
            .map(|projected| projected.map(|row| row.into_output()))
    }
}
