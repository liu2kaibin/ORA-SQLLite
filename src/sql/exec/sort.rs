use std::cmp::Ordering;

use crate::sql::ast::{NullPlacement, OrderDirection};
use crate::sql::plan::QueryPlan;
use crate::{DbError, ErrorCode, Result, Value};

use super::memory::{
    MATERIALIZED_CONTAINER_BYTES, MATERIALIZED_ENTRY_OVERHEAD_BYTES, checked_add_bytes,
};
use super::{ExecutionContext, OutputOperator, ProjectedRow, sql_total_cmp};

#[derive(Clone, Copy)]
struct OrderSpec {
    direction: OrderDirection,
    nulls: NullPlacement,
}

pub(crate) struct Sort {
    input: Box<dyn OutputOperator>,
    order: Box<[OrderSpec]>,
    rows: Vec<ProjectedRow>,
    initialized: bool,
    terminal: bool,
    storage_bytes: u64,
}

impl Sort {
    pub(crate) fn new(input: Box<dyn OutputOperator>, query: &QueryPlan) -> Self {
        Self {
            input,
            order: query
                .order_by()
                .iter()
                .map(|item| OrderSpec {
                    direction: item.direction(),
                    nulls: item.nulls(),
                })
                .collect(),
            rows: Vec::new(),
            initialized: false,
            terminal: false,
            storage_bytes: 0,
        }
    }

    fn initialize(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        context.memory().reserve(MATERIALIZED_CONTAINER_BYTES)?;
        self.storage_bytes = MATERIALIZED_CONTAINER_BYTES;
        loop {
            let candidate = match self.input.next(context) {
                Ok(Some(candidate)) => candidate,
                Ok(None) => break,
                Err(error) => return Err(error),
            };
            let entry_bytes = match checked_add_bytes(
                MATERIALIZED_ENTRY_OVERHEAD_BYTES,
                std::mem::size_of::<ProjectedRow>() as u64,
            ) {
                Ok(bytes) => bytes,
                Err(error) => {
                    context.memory().release(candidate.memory_bytes())?;
                    return Err(error);
                }
            };
            let next_storage = match checked_add_bytes(self.storage_bytes, entry_bytes) {
                Ok(bytes) => bytes,
                Err(error) => {
                    context.memory().release(candidate.memory_bytes())?;
                    return Err(error);
                }
            };
            if let Err(error) = context.memory().reserve(entry_bytes) {
                context.memory().release(candidate.memory_bytes())?;
                return Err(error);
            }
            self.storage_bytes = next_storage;
            self.rows.push(candidate);
        }

        let order = &self.order;
        let mut comparison_error = None;
        self.rows.sort_by(|left, right| {
            if comparison_error.is_some() {
                return Ordering::Equal;
            }
            match compare_rows(left, right, order) {
                Ok(ordering) => ordering,
                Err(error) => {
                    comparison_error = Some(error);
                    Ordering::Equal
                }
            }
        });
        if let Some(error) = comparison_error {
            return Err(error);
        }
        self.rows.reverse();
        self.initialized = true;
        Ok(())
    }

    fn clear(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        for row in self.rows.drain(..) {
            context.memory().release(row.memory_bytes())?;
        }
        if self.storage_bytes != 0 {
            context.memory().release(self.storage_bytes)?;
            self.storage_bytes = 0;
        }
        Ok(())
    }

    fn fail<T>(&mut self, context: &mut ExecutionContext<'_>, error: DbError) -> Result<T> {
        self.terminal = true;
        self.initialized = true;
        self.clear(context)?;
        Err(error)
    }
}

impl OutputOperator for Sort {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<ProjectedRow>> {
        if self.terminal {
            return Ok(None);
        }
        if !self.initialized {
            if let Err(error) = self.initialize(context) {
                return self.fail(context, error);
            }
        }
        if let Some(row) = self.rows.pop() {
            return Ok(Some(row));
        }
        self.terminal = true;
        self.clear(context)?;
        Ok(None)
    }
}

fn compare_rows(
    left: &ProjectedRow,
    right: &ProjectedRow,
    order: &[OrderSpec],
) -> Result<Ordering> {
    if left.order_keys().len() != order.len() || right.order_keys().len() != order.len() {
        return Err(DbError::new(
            ErrorCode::Internal,
            "ORDER BY key count does not match the planned ordering",
        ));
    }
    for ((left, right), spec) in left.order_keys().iter().zip(right.order_keys()).zip(order) {
        let ordering = compare_key(left, right, *spec)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left.input_ordinal().cmp(&right.input_ordinal()))
}

fn compare_key(left: &Value, right: &Value, spec: OrderSpec) -> Result<Ordering> {
    match (left, right) {
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        (Value::Null, _) => Ok(if nulls_first(spec) {
            Ordering::Less
        } else {
            Ordering::Greater
        }),
        (_, Value::Null) => Ok(if nulls_first(spec) {
            Ordering::Greater
        } else {
            Ordering::Less
        }),
        _ => {
            let ordering = sql_total_cmp(left, right)?;
            Ok(if spec.direction == OrderDirection::Descending {
                ordering.reverse()
            } else {
                ordering
            })
        }
    }
}

fn nulls_first(spec: OrderSpec) -> bool {
    match spec.nulls {
        NullPlacement::First => true,
        NullPlacement::Last => false,
        NullPlacement::Default => spec.direction == OrderDirection::Descending,
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::test_support::{Phase4bCatalogFixture, phase_4c1_query_relation_fixture};
    use crate::{Database, ErrorCode, Options, Parameters, Result, Value};

    struct SortFixture {
        _storage: Phase4bCatalogFixture,
        database: Database,
    }

    fn fixture(rows: &[(i64, &str)], options: Options) -> SortFixture {
        let storage = phase_4c1_query_relation_fixture(rows);
        let database = Database::open(storage.main_path(), options).unwrap();
        SortFixture {
            _storage: storage,
            database,
        }
    }

    fn values(fixture: &SortFixture, sql: &str) -> Result<Vec<Vec<Value>>> {
        let mut connection = fixture.database.connect()?;
        let statement = connection.prepare(sql)?;
        let mut rows = connection.query(&statement, Parameters::none())?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(row.values().to_vec());
        }
        Ok(result)
    }

    #[test]
    fn hidden_keys_null_defaults_explicit_placement_and_stable_ties_sort_rows() {
        // Production breaks caught: exposing a hidden key, applying direction
        // to NULL placement, or omitting input ordinal changes these literals.
        let null_fixture = fixture(&[(1, "A"), (2, "B"), (3, "C")], Options::default());
        let cases = [
            (
                "SELECT L.NAME FROM USERS L LEFT JOIN USERS R ON R.ID = L.ID AND R.NAME = 'B' ORDER BY R.ID",
                vec!["B", "A", "C"],
            ),
            (
                "SELECT L.NAME FROM USERS L LEFT JOIN USERS R ON R.ID = L.ID AND R.NAME = 'B' ORDER BY R.ID DESC",
                vec!["A", "C", "B"],
            ),
            (
                "SELECT L.NAME FROM USERS L LEFT JOIN USERS R ON R.ID = L.ID AND R.NAME = 'B' ORDER BY R.ID ASC NULLS FIRST",
                vec!["A", "C", "B"],
            ),
            (
                "SELECT L.NAME FROM USERS L LEFT JOIN USERS R ON R.ID = L.ID AND R.NAME = 'B' ORDER BY R.ID DESC NULLS LAST",
                vec!["B", "A", "C"],
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(
                values(&null_fixture, sql).unwrap(),
                expected
                    .into_iter()
                    .map(|name| vec![Value::Text(name.into())])
                    .collect::<Vec<_>>(),
                "{sql}",
            );
        }

        let ties = fixture(
            &[(2, "FIRST"), (1, "LOW"), (2, "SECOND"), (3, "HIGH")],
            Options::default(),
        );
        assert_eq!(
            values(&ties, "SELECT NAME FROM USERS ORDER BY ID DESC").unwrap(),
            ["HIGH", "FIRST", "SECOND", "LOW"]
                .map(|name| vec![Value::Text(name.into())])
                .to_vec()
        );
    }

    #[test]
    fn first_sort_row_that_exceeds_memory_is_not_retained() {
        // A source-free row has no decoded input reservation. The limit admits
        // the parameter frame, sort container, visible output, and cloned
        // order key exactly; the first sort entry itself does not fit.
        let name = "s".repeat(1_024);
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let output_bytes = 32 + value_bytes + 1_024;
        let order_key_bytes = 32 + value_bytes + 1_024;
        let entry_bytes = 64 + std::mem::size_of::<super::ProjectedRow>() as u64;
        let exact = 32 + 32 + output_bytes + order_key_bytes + entry_bytes;
        for (limit, accepted) in [(exact - 1, false), (exact, true)] {
            let fixture = fixture(&[], Options::default().with_max_query_memory_bytes(limit));
            let mut connection = fixture.database.connect().unwrap();
            let sql = format!("SELECT '{name}' ORDER BY 1");
            let statement = connection.prepare(&sql).unwrap();
            let mut rows = connection.query(&statement, Parameters::none()).unwrap();
            if accepted {
                assert_eq!(
                    rows.next().unwrap().unwrap().values(),
                    &[Value::Text(name.clone())]
                );
                assert!(rows.next().unwrap().is_none());
            } else {
                assert_eq!(rows.next().unwrap_err().code(), ErrorCode::ResourceLimit);
            }
        }
    }
}
