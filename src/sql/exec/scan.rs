use crate::sql::bound::SourceId;
use crate::storage::relation::RelationScan;
use crate::{Result, Value};

use super::memory::owned_values_bytes;
use super::operator::{EvalRow, ExecutionContext, Operator};

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static PAYLOAD_TRANSFERS: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn first_payload_ptr(values: &[Value]) -> Option<usize> {
    values.iter().find_map(|value| match value {
        Value::Text(text) if !text.is_empty() => Some(text.as_ptr() as usize),
        Value::Blob(blob) if !blob.is_empty() => Some(blob.as_ptr() as usize),
        _ => None,
    })
}

#[cfg(test)]
fn record_payload_transfer(decoded: Option<usize>, admitted: &[Value]) {
    if let (Some(decoded), Some(admitted)) = (decoded, first_payload_ptr(admitted)) {
        PAYLOAD_TRANSFERS.with(|transfers| transfers.borrow_mut().push((decoded, admitted)));
    }
}

#[cfg(test)]
fn take_payload_transfers() -> Vec<(usize, usize)> {
    PAYLOAD_TRANSFERS.with(|transfers| std::mem::take(&mut *transfers.borrow_mut()))
}

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
        #[cfg(test)]
        let decoded_payload = first_payload_ptr(row.values());
        let values = row.into_values();
        #[cfg(test)]
        record_payload_transfer(decoded_payload, &values);
        let memory_bytes = owned_values_bytes(values.iter())?;
        let row = EvalRow::with_source(self.source, values, memory_bytes)?;
        context.memory().reserve(memory_bytes)?;
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::test_support::phase_4c1_query_relation_fixture;
    use crate::{Database, ErrorCode, Options, Parameters, Value};

    use super::take_payload_transfers;

    #[test]
    fn filtered_large_row_transfers_its_decoded_payload_at_the_exact_memory_boundary() {
        // Production break caught: cloning StoredRow values in TableScan
        // gives the admitted row a different Text allocation while the
        // original decoded allocation remains live outside WorkMemory.
        let name = "x".repeat(20_000);
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let input_bytes = 32 + 4 * value_bytes + name.len() as u64;
        let empty_parameter_frame_bytes = 32;
        let exact = empty_parameter_frame_bytes + input_bytes;

        for (limit, accepted) in [(exact - 1, false), (exact, true)] {
            let _ = take_payload_transfers();
            let storage = phase_4c1_query_relation_fixture(&[(1, &name)]);
            let database = Database::open(
                storage.main_path(),
                Options::default().with_max_query_memory_bytes(limit),
            )
            .unwrap();
            let mut connection = database.connect().unwrap();
            let statement = connection
                .prepare("SELECT NAME FROM USERS WHERE FALSE")
                .unwrap();
            let mut rows = connection.query(&statement, Parameters::none()).unwrap();

            if accepted {
                assert!(rows.next().unwrap().is_none());
                let transfers = take_payload_transfers();
                assert_eq!(transfers.len(), 1);
                assert_eq!(
                    transfers[0].0, transfers[0].1,
                    "TableScan must transfer, not clone, the decoded payload allocation"
                );
            } else {
                assert_eq!(rows.next().unwrap_err().code(), ErrorCode::ResourceLimit);
                let _ = take_payload_transfers();
            }
        }
    }
}
