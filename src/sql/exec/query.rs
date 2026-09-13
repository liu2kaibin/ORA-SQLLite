use crate::sql::plan::{PlanKind, QueryPlan};
use crate::storage::catalog::CatalogReader;
use crate::{DbError, ErrorCode, Parameters, Result, Statement};

use super::{OperatorFactory, OutputOperator, ParameterFrame, Project, WorkMemory};

pub(crate) struct QueryExecution {
    root: Box<dyn OutputOperator>,
    params: ParameterFrame,
    memory: WorkMemory,
}

impl QueryExecution {
    pub(crate) fn into_parts(self) -> (Box<dyn OutputOperator>, ParameterFrame, WorkMemory) {
        (self.root, self.params, self.memory)
    }
}

pub(crate) fn prepare_query(
    statement: &Statement,
    parameters: Parameters<'_>,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    max_query_memory_bytes: u64,
) -> Result<QueryExecution> {
    let PlanKind::Query(query) = statement.plan().kind() else {
        return Err(DbError::new(
            ErrorCode::Type,
            "query requires a row-producing statement",
        ));
    };
    let factory = simple_factory(query)?;
    let mut memory = WorkMemory::new(max_query_memory_bytes)?;
    let params = ParameterFrame::bind(statement.parameters(), parameters, &mut memory)?;
    statement.validate_schema(database_uuid, catalog)?;
    let max_row_bytes = usize::try_from(memory.remaining()).unwrap_or(usize::MAX);
    let input = factory.build(catalog, max_row_bytes)?;
    Ok(QueryExecution {
        root: Box::new(Project::new(input, query)),
        params,
        memory,
    })
}

fn simple_factory(query: &QueryPlan) -> Result<OperatorFactory> {
    if !query.joins().is_empty()
        || !query.group_by().is_empty()
        || !query.aggregates().is_empty()
        || query.having().is_some()
        || query.distinct()
        || !query.order_by().is_empty()
    {
        return Err(DbError::new(
            ErrorCode::Unsupported,
            "query requires an operator added by a later Phase 4C1 task",
        ));
    }
    let input = match query.sources() {
        [] => OperatorFactory::OneRow,
        [source] => OperatorFactory::TableScan {
            source: source.source_id,
            table: source.table_id,
        },
        _ => {
            return Err(DbError::new(
                ErrorCode::Unsupported,
                "multiple query sources require JOIN execution",
            ));
        }
    };
    Ok(match query.selection() {
        Some(predicate) => OperatorFactory::Filter {
            input: Box::new(input),
            predicate: predicate.clone(),
        },
        None => input,
    })
}

#[cfg(test)]
mod tests {
    use crate::storage::test_support::{Phase4bCatalogFixture, phase_4c1_query_relation_fixture};
    use crate::{Database, Options, Parameters, Value};

    struct QueryRelationFixture {
        _storage: Phase4bCatalogFixture,
        database: Database,
    }

    impl QueryRelationFixture {
        fn database(&self) -> &Database {
            &self.database
        }
    }

    fn query_relation_fixture(rows: &[(i64, &str)]) -> QueryRelationFixture {
        let storage = phase_4c1_query_relation_fixture(rows);
        let database = Database::open(storage.main_path(), Options::default()).unwrap();
        QueryRelationFixture {
            _storage: storage,
            database,
        }
    }

    #[test]
    fn simple_query_streams_owned_rows() {
        let fixture = query_relation_fixture(&[(1, "A"), (2, "B")]);
        let mut connection = fixture.database().connect().unwrap();
        let statement = connection
            .prepare("SELECT ID, NAME FROM USERS WHERE ID > :1")
            .unwrap();
        let params = [Value::Int64(0).into()];
        let mut rows = connection
            .query(&statement, Parameters::positional(&params))
            .unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Int64(1), Value::Text("A".into())]
        );
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Int64(2), Value::Text("B".into())]
        );
        assert!(rows.next().unwrap().is_none());
    }

    #[test]
    fn dropping_rows_early_releases_the_snapshot_and_context_borrow() {
        let fixture = query_relation_fixture(&[(1, "A"), (2, "B")]);
        let mut connection = fixture.database().connect().unwrap();
        let statement = connection.prepare("SELECT ID FROM USERS").unwrap();
        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        assert!(rows.next().unwrap().is_some());
        drop(rows);
        let again = connection.query(&statement, Parameters::none()).unwrap();
        drop(again);
    }

    #[test]
    fn simple_query_filters_null_and_projects_owned_expressions() {
        // Production break caught: retaining FALSE/NULL predicates or returning
        // borrowed scan values would expose rows outside SQL WHERE semantics.
        let fixture = query_relation_fixture(&[(1, "A"), (2, "B")]);
        let mut connection = fixture.database().connect().unwrap();
        let filtered = connection
            .prepare("SELECT ID, NAME, ID + 10 FROM USERS WHERE ID = 2")
            .unwrap();
        let mut rows = connection.query(&filtered, Parameters::none()).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Int64(2), Value::Text("B".into()), Value::Int64(12),]
        );
        assert!(rows.next().unwrap().is_none());
        drop(rows);

        let unknown = connection
            .prepare("SELECT ID FROM USERS WHERE NULL")
            .unwrap();
        let mut rows = connection.query(&unknown, Parameters::none()).unwrap();
        assert!(rows.next().unwrap().is_none());
    }

    #[test]
    fn simple_query_rejects_operator_families_owned_by_later_tasks() {
        // Production break caught: silently partially executing a later-stage
        // node would return semantically incomplete query results.
        let fixture = query_relation_fixture(&[(1, "A"), (2, "B")]);
        let mut connection = fixture.database().connect().unwrap();
        for sql in [
            "SELECT U.ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID",
            "SELECT COUNT(*) FROM USERS",
            "SELECT DISTINCT ID FROM USERS",
            "SELECT ID FROM USERS ORDER BY ID",
        ] {
            let statement = connection.prepare(sql).unwrap();
            assert_eq!(
                connection
                    .query(&statement, Parameters::none())
                    .unwrap_err()
                    .code(),
                crate::ErrorCode::Unsupported,
                "{sql}"
            );
        }
    }

    #[test]
    fn query_validates_statement_identity_before_scanning() {
        // Production break caught: opening a relation without retained schema
        // validation could run a statement against a different database.
        let first = query_relation_fixture(&[(1, "A")]);
        let second = query_relation_fixture(&[(2, "B")]);
        let mut first_connection = first.database().connect().unwrap();
        let statement = first_connection.prepare("SELECT ID FROM USERS").unwrap();
        let mut second_connection = second.database().connect().unwrap();
        assert_eq!(
            second_connection
                .query(&statement, Parameters::none())
                .unwrap_err()
                .code(),
            crate::ErrorCode::SchemaChanged
        );
    }

    #[test]
    fn configured_query_memory_is_the_relation_row_decode_limit() {
        // Production break caught: using the storage test model's 16 KiB limit
        // would reject this row even when the caller configured enough shared
        // memory for the parameter frame, decoded row, and projected output.
        let long_name = "x".repeat(20_000);
        let storage = phase_4c1_query_relation_fixture(&[(1, &long_name)]);
        let database = Database::open(
            storage.main_path(),
            Options::default().with_max_query_memory_bytes(48 * 1024),
        )
        .unwrap();
        let mut connection = database.connect().unwrap();
        let statement = connection.prepare("SELECT NAME FROM USERS").unwrap();
        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Text(long_name)]
        );
    }

    #[test]
    fn parameters_and_decoded_row_share_one_query_memory_ceiling() {
        // Production break caught: giving the scan the original full limit
        // lets the parameter frame and decoded row each fit independently
        // while their combined retained storage exceeds the query ceiling.
        let name = "x".repeat(1_024);
        let storage = phase_4c1_query_relation_fixture(&[(1, &name)]);
        let database = Database::open(
            storage.main_path(),
            Options::default().with_max_query_memory_bytes(1_500),
        )
        .unwrap();
        let mut connection = database.connect().unwrap();
        let statement = connection
            .prepare("SELECT ID FROM USERS WHERE NAME = :1")
            .unwrap();
        let params = [Value::Text(name).into()];
        let mut rows = connection
            .query(&statement, Parameters::positional(&params))
            .unwrap();
        assert_eq!(
            rows.next().unwrap_err().code(),
            crate::ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn repeated_projection_reserves_transient_output_and_releases_each_row() {
        // Production break caught: cloning one large source value into three
        // output slots without a reservation bypasses amplification limits;
        // failing to release a returned row makes the second exact-boundary
        // row fail despite the caller owning the first output.
        let name = "y".repeat(1_024);
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let input_bytes = 32 + 4 * value_bytes + 1_024;
        let output_bytes = 32 + 3 * value_bytes + 3 * 1_024;
        let empty_parameter_frame_bytes = 32;
        let exact = empty_parameter_frame_bytes + input_bytes + output_bytes;

        for (limit, accepted) in [(exact - 1, false), (exact, true)] {
            let storage = phase_4c1_query_relation_fixture(&[(1, &name), (2, &name)]);
            let database = Database::open(
                storage.main_path(),
                Options::default().with_max_query_memory_bytes(limit),
            )
            .unwrap();
            let mut connection = database.connect().unwrap();
            let statement = connection
                .prepare("SELECT NAME, NAME, NAME FROM USERS")
                .unwrap();
            let mut rows = connection.query(&statement, Parameters::none()).unwrap();
            if accepted {
                for _ in 0..2 {
                    assert_eq!(
                        rows.next().unwrap().unwrap().values(),
                        &[
                            Value::Text(name.clone()),
                            Value::Text(name.clone()),
                            Value::Text(name.clone()),
                        ]
                    );
                }
                assert!(rows.next().unwrap().is_none());
            } else {
                assert_eq!(
                    rows.next().unwrap_err().code(),
                    crate::ErrorCode::ResourceLimit
                );
            }
        }
    }

    #[test]
    fn source_free_one_row_query_prepares_and_executes_without_dual() {
        // Production break caught: requiring a synthetic table source would
        // reject the approved OneRow query shape or introduce Phase 5 DUAL.
        let fixture = query_relation_fixture(&[]);
        let mut connection = fixture.database().connect().unwrap();
        let statement = connection.prepare("SELECT 1").unwrap();
        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(1)]);
        assert!(rows.next().unwrap().is_none());
    }
}
