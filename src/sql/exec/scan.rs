use crate::sql::bound::SourceId;
use crate::storage::relation::RelationScan;
use crate::{Result, Value};

use super::memory::owned_values_bytes;
use super::operator::{EvalRow, ExecutionContext, Operator};

pub(crate) struct OneRow {
    emitted: bool,
}

impl OneRow {
    pub(crate) const fn new() -> Self {
        Self { emitted: false }
    }
}

impl Operator for OneRow {
    fn next(&mut self, _context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        if self.emitted {
            Ok(None)
        } else {
            self.emitted = true;
            Ok(Some(EvalRow::default()))
        }
    }
}

pub(crate) struct TableScan {
    source: SourceId,
    scan: RelationScan,
}

impl TableScan {
    pub(crate) const fn new(source: SourceId, scan: RelationScan) -> Self {
        Self { source, scan }
    }
}

impl Operator for TableScan {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        let Some(row) = context.next_relation(&mut self.scan)? else {
            return Ok(None);
        };
        let values: Box<[Value]> = row.values().into();
        let memory_bytes = owned_values_bytes(values.iter())?;
        let row = EvalRow::with_source(self.source, values, memory_bytes)?;
        context.memory().reserve(memory_bytes)?;
        Ok(Some(row))
    }
}
