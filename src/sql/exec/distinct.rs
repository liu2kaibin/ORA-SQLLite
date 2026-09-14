use crate::{DbError, Result, Value};

use super::memory::{
    MATERIALIZED_CONTAINER_BYTES, MATERIALIZED_ENTRY_OVERHEAD_BYTES, checked_add_bytes,
    owned_values_bytes,
};
use super::{ExecutionContext, OutputOperator, ProjectedRow, sql_group_equal};

struct DistinctKey {
    values: Box<[Value]>,
    memory_bytes: u64,
}

pub(crate) struct Distinct {
    input: Box<dyn OutputOperator>,
    seen: Vec<DistinctKey>,
    initialized: bool,
    terminal: bool,
}

impl Distinct {
    pub(crate) fn new(input: Box<dyn OutputOperator>) -> Self {
        Self {
            input,
            seen: Vec::new(),
            initialized: false,
            terminal: false,
        }
    }

    fn initialize(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        context.memory().reserve(MATERIALIZED_CONTAINER_BYTES)?;
        self.initialized = true;
        Ok(())
    }

    fn clear(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        for key in self.seen.drain(..) {
            context.memory().release(key.memory_bytes)?;
        }
        if self.initialized {
            context.memory().release(MATERIALIZED_CONTAINER_BYTES)?;
            self.initialized = false;
        }
        Ok(())
    }

    fn fail<T>(&mut self, context: &mut ExecutionContext<'_>, error: DbError) -> Result<T> {
        self.terminal = true;
        self.clear(context)?;
        Err(error)
    }
}

impl OutputOperator for Distinct {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<ProjectedRow>> {
        if self.terminal {
            return Ok(None);
        }
        if !self.initialized {
            if let Err(error) = self.initialize(context) {
                self.terminal = true;
                return Err(error);
            }
        }
        loop {
            let candidate = match self.input.next(context) {
                Ok(Some(candidate)) => candidate,
                Ok(None) => {
                    self.terminal = true;
                    self.clear(context)?;
                    return Ok(None);
                }
                Err(error) => return self.fail(context, error),
            };
            let mut duplicate = false;
            for key in &self.seen {
                if key.values.len() != candidate.output().values().len() {
                    continue;
                }
                let mut equal = true;
                for (left, right) in key.values.iter().zip(candidate.output().values()) {
                    match sql_group_equal(left, right) {
                        Ok(true) => {}
                        Ok(false) => {
                            equal = false;
                            break;
                        }
                        Err(error) => {
                            context.memory().release(candidate.memory_bytes())?;
                            return self.fail(context, error);
                        }
                    }
                }
                if equal {
                    duplicate = true;
                    break;
                }
            }
            if duplicate {
                context.memory().release(candidate.memory_bytes())?;
                continue;
            }
            let key_bytes = match owned_values_bytes(candidate.output().values())
                .and_then(|bytes| checked_add_bytes(MATERIALIZED_ENTRY_OVERHEAD_BYTES, bytes))
            {
                Ok(bytes) => bytes,
                Err(error) => {
                    context.memory().release(candidate.memory_bytes())?;
                    return self.fail(context, error);
                }
            };
            if let Err(error) = context.memory().reserve(key_bytes) {
                context.memory().release(candidate.memory_bytes())?;
                return self.fail(context, error);
            }
            self.seen.push(DistinctKey {
                values: candidate.output().values().into(),
                memory_bytes: key_bytes,
            });
            return Ok(Some(candidate));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::test_support::{Phase4bCatalogFixture, phase_4c1_query_relation_fixture};
    use crate::{Database, ErrorCode, Options, Parameters, Result, Value};

    struct DistinctFixture {
        _storage: Phase4bCatalogFixture,
        database: Database,
    }

    fn fixture(rows: &[(i64, &str)], options: Options) -> DistinctFixture {
        let storage = phase_4c1_query_relation_fixture(rows);
        let database = Database::open(storage.main_path(), options).unwrap();
        DistinctFixture {
            _storage: storage,
            database,
        }
    }

    fn values(fixture: &DistinctFixture, sql: &str) -> Result<Vec<Vec<Value>>> {
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
    fn distinct_uses_the_complete_visible_row_and_not_hidden_order_keys() {
        // Production breaks caught: comparing a prefix, including hidden sort
        // keys in identity, or sorting before DISTINCT changes these literals.
        let fixture = fixture(
            &[(1, "Z"), (1, "A"), (2, "M"), (3, "N"), (3, "N")],
            Options::default(),
        );
        assert_eq!(
            values(&fixture, "SELECT DISTINCT ID FROM USERS ORDER BY NAME").unwrap(),
            vec![
                vec![Value::Int64(2)],
                vec![Value::Int64(3)],
                vec![Value::Int64(1)],
            ]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT DISTINCT ID, NAME FROM USERS ORDER BY ID, NAME",
            )
            .unwrap(),
            vec![
                vec![Value::Int64(1), Value::Text("A".into())],
                vec![Value::Int64(1), Value::Text("Z".into())],
                vec![Value::Int64(2), Value::Text("M".into())],
                vec![Value::Int64(3), Value::Text("N".into())],
            ]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT DISTINCT AMOUNT FROM USERS ORDER BY AMOUNT"
            )
            .unwrap(),
            vec![vec![Value::Null]]
        );
    }

    #[test]
    fn first_distinct_key_that_exceeds_memory_is_not_retained() {
        // A source-free row has no decoded input reservation. The limit admits
        // the parameter frame, DISTINCT container, and projection exactly;
        // the first retained visible key does not fit.
        let name = "d".repeat(1_024);
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let output_bytes = 32 + value_bytes + 1_024;
        let key_bytes = 64 + output_bytes;
        let exact = 32 + 32 + output_bytes + key_bytes;
        for (limit, accepted) in [(exact - 1, false), (exact, true)] {
            let fixture = fixture(&[], Options::default().with_max_query_memory_bytes(limit));
            let mut connection = fixture.database.connect().unwrap();
            let sql = format!("SELECT DISTINCT '{name}'");
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
