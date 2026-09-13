use crate::sql::{
    bound::SourceId,
    plan::{ExecExpr, QueryPlan},
};
use crate::storage::catalog::{CatalogReader, types::TableId};
use crate::storage::relation::{Relation, RelationScan, StoredRow};
use crate::storage::{ReadSnapshot, WriteBatch};
use crate::{DbError, ErrorCode, Result, Row, Value};

use super::memory::{owned_values_bytes, resource_limit};
use super::scan::{OneRow, TableScan};
use super::{ParameterFrame, WorkMemory, eval};

pub(crate) trait Operator {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>>;
}

pub(crate) trait OutputOperator {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<ProjectedRow>>;
}

#[derive(Clone)]
pub(crate) enum OperatorFactory {
    OneRow,
    TableScan {
        source: SourceId,
        table: TableId,
    },
    Filter {
        input: Box<OperatorFactory>,
        predicate: ExecExpr,
    },
}

impl OperatorFactory {
    pub(crate) fn build(
        self,
        catalog: &CatalogReader,
        max_row_bytes: usize,
    ) -> Result<Box<dyn Operator>> {
        Ok(match self {
            Self::OneRow => Box::new(OneRow::new()),
            Self::TableScan { source, table } => Box::new(TableScan::new(
                source,
                Relation::open(catalog, table)?.scan(max_row_bytes),
            )),
            Self::Filter { input, predicate } => Box::new(Filter {
                input: input.build(catalog, max_row_bytes)?,
                predicate,
            }),
        })
    }
}

pub(crate) struct ProjectedRow {
    output: Row,
    order_keys: Box<[Value]>,
    input_ordinal: u64,
    memory_bytes: u64,
}

impl ProjectedRow {
    pub(crate) fn into_output(self) -> (Row, u64) {
        (self.output, self.memory_bytes)
    }

    #[cfg(test)]
    pub(crate) fn from_output_for_test(output: Row, memory_bytes: u64) -> Self {
        Self {
            output,
            order_keys: Box::new([]),
            input_ordinal: 0,
            memory_bytes,
        }
    }

    #[allow(dead_code, reason = "consumed by DISTINCT and ORDER BY in Task 8")]
    pub(crate) fn output(&self) -> &Row {
        &self.output
    }

    #[allow(dead_code, reason = "consumed by ORDER BY in Task 8")]
    pub(crate) fn order_keys(&self) -> &[Value] {
        &self.order_keys
    }

    #[allow(
        dead_code,
        reason = "consumed as the stable ORDER BY tie-breaker in Task 8"
    )]
    pub(crate) const fn input_ordinal(&self) -> u64 {
        self.input_ordinal
    }
}

pub(crate) enum ExecutionStorage<'a> {
    Read(&'a mut ReadSnapshot),
    Write(&'a mut WriteBatch),
}

pub(crate) struct ExecutionContext<'a> {
    storage: ExecutionStorage<'a>,
    params: &'a ParameterFrame,
    #[allow(
        dead_code,
        reason = "shared by materializing operators added in Task 8"
    )]
    memory: &'a mut WorkMemory,
}

impl<'a> ExecutionContext<'a> {
    pub(crate) const fn new(
        storage: ExecutionStorage<'a>,
        params: &'a ParameterFrame,
        memory: &'a mut WorkMemory,
    ) -> Self {
        Self {
            storage,
            params,
            memory,
        }
    }

    pub(crate) const fn params(&self) -> &ParameterFrame {
        self.params
    }

    pub(crate) fn next_relation(&mut self, scan: &mut RelationScan) -> Result<Option<StoredRow>> {
        let result = match &mut self.storage {
            ExecutionStorage::Read(snapshot) => scan.next_snapshot(snapshot),
            ExecutionStorage::Write(batch) => scan.next_batch(batch),
        };
        result.map_err(|error| {
            if error.code() == ErrorCode::Type {
                resource_limit()
            } else {
                error
            }
        })
    }

    pub(crate) fn memory(&mut self) -> &mut WorkMemory {
        self.memory
    }
}

#[derive(Default)]
pub(crate) struct EvalRow {
    sources: Box<[Option<Box<[Value]>>]>,
    aggregates: Box<[Value]>,
    memory_bytes: u64,
}

impl EvalRow {
    pub(crate) fn with_source(
        source: SourceId,
        values: Box<[Value]>,
        memory_bytes: u64,
    ) -> Result<Self> {
        let count = source.0.checked_add(1).ok_or_else(resource_limit)?;
        let mut sources = vec![None; count].into_boxed_slice();
        sources[source.0] = Some(values);
        Ok(Self {
            sources,
            aggregates: Box::new([]),
            memory_bytes,
        })
    }

    pub(crate) const fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    pub(crate) fn source_value(&self, source: SourceId, ordinal: u16) -> Option<&Value> {
        self.sources
            .get(source.0)
            .and_then(Option::as_deref)
            .and_then(|row| row.get(usize::from(ordinal)))
    }

    pub(crate) fn aggregate_value(&self, slot: u32) -> Result<Option<&Value>> {
        let slot = usize::try_from(slot).map_err(|_| {
            DbError::new(
                ErrorCode::Internal,
                "aggregate slot is outside the platform range",
            )
        })?;
        Ok(self.aggregates.get(slot))
    }

    #[cfg(test)]
    pub(crate) fn from_sources_for_test(sources: &[&[Value]]) -> Self {
        Self {
            sources: sources
                .iter()
                .map(|source| Some(source.to_vec().into_boxed_slice()))
                .collect(),
            aggregates: Box::new([]),
            memory_bytes: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_aggregates_for_test(aggregates: &[Value]) -> Self {
        Self {
            sources: Box::new([]),
            aggregates: aggregates.into(),
            memory_bytes: 0,
        }
    }
}

pub(crate) struct Filter {
    input: Box<dyn Operator>,
    predicate: ExecExpr,
}

impl Operator for Filter {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        loop {
            let Some(row) = self.input.next(context)? else {
                return Ok(None);
            };
            let predicate = eval(&self.predicate, &row, context.params());
            match predicate {
                Err(error) => {
                    context.memory().release(row.memory_bytes())?;
                    return Err(error);
                }
                Ok(Value::Bool(true)) => return Ok(Some(row)),
                Ok(Value::Bool(false) | Value::Null) => {
                    context.memory().release(row.memory_bytes())?;
                }
                Ok(_) => {
                    context.memory().release(row.memory_bytes())?;
                    return Err(DbError::new(
                        ErrorCode::Type,
                        "query filter requires a boolean value",
                    ));
                }
            }
        }
    }
}

pub(crate) struct Project {
    input: Box<dyn Operator>,
    projection: Box<[ExecExpr]>,
    next_input_ordinal: u64,
}

impl Project {
    pub(crate) fn new(input: Box<dyn Operator>, query: &QueryPlan) -> Self {
        Self {
            input,
            projection: query.projection().into(),
            next_input_ordinal: 0,
        }
    }
}

impl OutputOperator for Project {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<ProjectedRow>> {
        let Some(row) = self.input.next(context)? else {
            return Ok(None);
        };
        let output = match self
            .projection
            .iter()
            .map(|expression| eval(expression, &row, context.params()))
            .collect::<Result<Vec<_>>>()
        {
            Ok(output) => output,
            Err(error) => {
                context.memory().release(row.memory_bytes())?;
                return Err(error);
            }
        };
        let output_bytes = match owned_values_bytes(output.iter()) {
            Ok(bytes) => bytes,
            Err(error) => {
                context.memory().release(row.memory_bytes())?;
                return Err(error);
            }
        };
        if let Err(error) = context.memory().reserve(output_bytes) {
            context.memory().release(row.memory_bytes())?;
            return Err(error);
        }
        context.memory().release(row.memory_bytes())?;
        let input_ordinal = self.next_input_ordinal;
        let Some(next_input_ordinal) = self.next_input_ordinal.checked_add(1) else {
            context.memory().release(output_bytes)?;
            return Err(resource_limit());
        };
        self.next_input_ordinal = next_input_ordinal;
        Ok(Some(ProjectedRow {
            output: Row::new(output),
            order_keys: Box::new([]),
            input_ordinal,
            memory_bytes: output_bytes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::phase_4b_catalog_fixture;

    #[test]
    fn ordinal_overflow_releases_reserved_projection_memory() {
        // Production break caught: returning ordinal overflow after reserving
        // the output leaves memory charged for a row that was never returned.
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut read = pager.begin_read().unwrap();
        let params = crate::sql::exec::tests::empty_frame();
        let mut memory = WorkMemory::new(64).unwrap();
        let mut context =
            ExecutionContext::new(ExecutionStorage::Read(&mut read), &params, &mut memory);
        let mut project = Project {
            input: Box::new(OneRow::new()),
            projection: Box::new([]),
            next_input_ordinal: u64::MAX,
        };

        let error = match project.next(&mut context) {
            Err(error) => error,
            Ok(_) => panic!("ordinal overflow must reject the row"),
        };
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(context.memory().used(), 0);
    }
}
