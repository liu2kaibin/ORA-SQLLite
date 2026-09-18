use crate::sql::{ast::JoinKind, bound::SourceId, plan::ExecExpr};
use crate::{DbError, ErrorCode, Result, Value};

use super::operator::Operator;
use super::{EvalRow, ExecutionContext, OperatorFactory, eval};

pub(crate) struct NestedLoopJoin {
    left: Box<dyn Operator>,
    right_factory: OperatorFactory,
    right: Option<Box<dyn Operator>>,
    on: ExecExpr,
    kind: JoinKind,
    right_source: SourceId,
    current_left: Option<EvalRow>,
    matched_current_left: bool,
}

impl NestedLoopJoin {
    pub(crate) fn new(
        left: Box<dyn Operator>,
        right_factory: OperatorFactory,
        on: ExecExpr,
        kind: JoinKind,
    ) -> Result<Self> {
        let right_source = right_factory
            .single_source()
            .ok_or_else(|| invalid_join_state("JOIN right factory is not a single table source"))?;
        Ok(Self {
            left,
            right_factory,
            right: None,
            on,
            kind,
            right_source,
            current_left: None,
            matched_current_left: false,
        })
    }

    fn clear_current_left(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        self.right = None;
        self.matched_current_left = false;
        if let Some(left) = self.current_left.take() {
            context.memory().release(left.memory_bytes())?;
        }
        Ok(())
    }

    fn fail<T>(&mut self, context: &mut ExecutionContext<'_>, error: DbError) -> Result<T> {
        self.clear_current_left(context)?;
        Err(error)
    }

    fn discard_candidate<T>(
        &mut self,
        context: &mut ExecutionContext<'_>,
        bytes: u64,
        error: DbError,
    ) -> Result<T> {
        context.memory().release(bytes)?;
        self.fail(context, error)
    }
}

impl Operator for NestedLoopJoin {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        loop {
            if self.current_left.is_none() {
                let Some(left) = self.left.next(context)? else {
                    return Ok(None);
                };
                self.current_left = Some(left);
                self.matched_current_left = false;
                self.right = match self.right_factory.build() {
                    Ok(right) => Some(right),
                    Err(error) => return self.fail(context, error),
                };
            }

            let next_right = match self.right.as_mut() {
                Some(right) => right.next(context),
                None => {
                    return self.fail(
                        context,
                        invalid_join_state("current JOIN left row has no right operator"),
                    );
                }
            };
            let right = match next_right {
                Ok(Some(right)) => right,
                Ok(None) => {
                    self.right = None;
                    if self.kind == JoinKind::Left && !self.matched_current_left {
                        let Some(left) = self.current_left.take() else {
                            return self.fail(
                                context,
                                invalid_join_state("right JOIN exhaustion has no current left row"),
                            );
                        };
                        let left_bytes = left.memory_bytes();
                        self.matched_current_left = false;
                        let null_right = match EvalRow::null_extended(self.right_source) {
                            Ok(row) => row,
                            Err(error) => {
                                context.memory().release(left_bytes)?;
                                return Err(error);
                            }
                        };
                        return match left.join_owned(null_right) {
                            Ok(joined) => Ok(Some(joined)),
                            Err(error) => {
                                context.memory().release(left_bytes)?;
                                Err(error)
                            }
                        };
                    }
                    self.clear_current_left(context)?;
                    continue;
                }
                Err(error) => return self.fail(context, error),
            };

            let right_bytes = right.memory_bytes();
            let Some(current_left) = self.current_left.as_ref() else {
                context.memory().release(right_bytes)?;
                return self.fail(
                    context,
                    invalid_join_state("right JOIN row has no current left row"),
                );
            };
            let left_bytes = current_left.memory_bytes();
            if let Err(error) = context.memory().reserve(left_bytes) {
                context.memory().release(right_bytes)?;
                return self.fail(context, error);
            }
            let Some(candidate_bytes) = left_bytes.checked_add(right_bytes) else {
                context.memory().release(left_bytes)?;
                context.memory().release(right_bytes)?;
                return self.fail(
                    context,
                    DbError::new(ErrorCode::ResourceLimit, "query work memory limit exceeded"),
                );
            };
            let candidate = match self.current_left.as_ref() {
                Some(left) => match left.join_clone(right) {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        return self.discard_candidate(context, candidate_bytes, error);
                    }
                },
                None => {
                    return self.discard_candidate(
                        context,
                        candidate_bytes,
                        invalid_join_state("right JOIN row has no current left row"),
                    );
                }
            };
            match eval(&self.on, &candidate, context.params()) {
                Ok(Value::Bool(true)) => {
                    self.matched_current_left = true;
                    return Ok(Some(candidate));
                }
                Ok(Value::Bool(false) | Value::Null) => {
                    context.memory().release(candidate.memory_bytes())?;
                }
                Ok(_) => {
                    return self.discard_candidate(
                        context,
                        candidate.memory_bytes(),
                        DbError::new(ErrorCode::Type, "JOIN ON requires a boolean value"),
                    );
                }
                Err(error) => {
                    return self.discard_candidate(context, candidate.memory_bytes(), error);
                }
            }
        }
    }
}

fn invalid_join_state(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Internal, message)
}

impl OperatorFactory {
    fn single_source(&self) -> Option<SourceId> {
        match self {
            Self::TableScan { source, .. } => Some(*source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::test_support::{Phase4bCatalogFixture, phase_4c1_query_relation_fixture};
    use crate::{Database, ErrorCode, Options, Parameters, Result, Value};

    struct JoinFixture {
        _storage: Phase4bCatalogFixture,
        database: Database,
    }

    impl JoinFixture {
        fn database(&self) -> &Database {
            &self.database
        }
    }

    fn join_fixture() -> JoinFixture {
        join_fixture_with_rows(
            &[(1, "A"), (2, "B"), (2, "C"), (3, "D")],
            Options::default(),
        )
    }

    fn join_fixture_with_rows(rows: &[(i64, &str)], options: Options) -> JoinFixture {
        let storage = phase_4c1_query_relation_fixture(rows);
        let database = Database::open(storage.main_path(), options).unwrap();
        JoinFixture {
            _storage: storage,
            database,
        }
    }

    fn run_query(fixture: &JoinFixture, sql: &str) -> Result<Vec<Vec<Value>>> {
        let mut connection = fixture.database().connect()?;
        let statement = connection.prepare(sql)?;
        let mut rows = connection.query(&statement, Parameters::none())?;
        let mut output = Vec::new();
        while let Some(row) = rows.next()? {
            output.push(row.values().to_vec());
        }
        Ok(output)
    }

    #[test]
    fn nested_loop_join_covers_inner_left_null_and_duplicate_matches() {
        // Production breaks caught: choosing the wrong join kind, treating
        // UNKNOWN as TRUE, dropping duplicate matches, or changing SQL source
        // order changes one of these literal nested-loop row sequences.
        let fixture = join_fixture();
        let cases = [
            (
                "SELECT L.NAME, R.NAME FROM USERS L INNER JOIN USERS R ON R.ID = L.ID",
                vec![
                    vec![Value::Text("A".into()), Value::Text("A".into())],
                    vec![Value::Text("B".into()), Value::Text("B".into())],
                    vec![Value::Text("B".into()), Value::Text("C".into())],
                    vec![Value::Text("C".into()), Value::Text("B".into())],
                    vec![Value::Text("C".into()), Value::Text("C".into())],
                    vec![Value::Text("D".into()), Value::Text("D".into())],
                ],
            ),
            (
                "SELECT L.NAME, R.NAME FROM USERS L LEFT JOIN USERS R ON R.ID = L.ID AND R.NAME = 'B'",
                vec![
                    vec![Value::Text("A".into()), Value::Null],
                    vec![Value::Text("B".into()), Value::Text("B".into())],
                    vec![Value::Text("C".into()), Value::Text("B".into())],
                    vec![Value::Text("D".into()), Value::Null],
                ],
            ),
            (
                "SELECT L.ID, R.ID FROM USERS L LEFT JOIN USERS R ON NULL",
                vec![
                    vec![Value::Int64(1), Value::Null],
                    vec![Value::Int64(2), Value::Null],
                    vec![Value::Int64(2), Value::Null],
                    vec![Value::Int64(3), Value::Null],
                ],
            ),
        ];

        for (sql, expected) in cases {
            assert_eq!(run_query(&fixture, sql).unwrap(), expected, "{sql}");
        }
    }

    #[test]
    fn multiple_joins_compose_in_sql_source_order() {
        // Production break caught: reordering either join, overwriting a
        // SourceId slot, or failing to rewind C for each A/B pair changes the
        // literal output order or values.
        let fixture = join_fixture();
        assert_eq!(
            run_query(
                &fixture,
                "SELECT A.NAME, B.NAME, C.NAME FROM USERS A INNER JOIN USERS B ON A.ID = B.ID LEFT JOIN USERS C ON C.ID = B.ID AND C.NAME = 'C'",
            )
            .unwrap(),
            vec![
                vec![Value::Text("A".into()), Value::Text("A".into()), Value::Null],
                vec![Value::Text("B".into()), Value::Text("B".into()), Value::Text("C".into())],
                vec![Value::Text("B".into()), Value::Text("C".into()), Value::Text("C".into())],
                vec![Value::Text("C".into()), Value::Text("B".into()), Value::Text("C".into())],
                vec![Value::Text("C".into()), Value::Text("C".into()), Value::Text("C".into())],
                vec![Value::Text("D".into()), Value::Text("D".into()), Value::Null],
            ]
        );
    }

    #[test]
    fn join_does_not_retain_simultaneous_snapshot_borrows() {
        // Production break caught: retaining a storage borrow from the left
        // pull while advancing the right scan prevents this real snapshot
        // query from compiling or completing through one ExecutionContext.
        let fixture = join_fixture();
        let rows = run_query(
            &fixture,
            "SELECT A.ID, B.ID FROM USERS A INNER JOIN USERS B ON A.ID = B.ID",
        )
        .unwrap();
        assert_eq!(rows[0], [Value::Int64(1), Value::Int64(1)]);
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn join_rows_drop_early_and_run_on_a_borrowed_fixed_snapshot() {
        // Production breaks caught: retaining a transaction borrow after Rows
        // Drop or building right scans against a different view makes either
        // the repeated connection query or fixed-snapshot query fail.
        let fixture = join_fixture();
        let mut connection = fixture.database().connect().unwrap();
        let statement = connection
            .prepare("SELECT A.NAME, B.NAME FROM USERS A INNER JOIN USERS B ON A.ID = B.ID")
            .unwrap();
        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Text("A".into()), Value::Text("A".into())]
        );
        drop(rows);
        let again = connection.query(&statement, Parameters::none()).unwrap();
        drop(again);

        let mut transaction = connection.begin_read().unwrap();
        let statement = transaction
            .prepare("SELECT A.ID, B.ID FROM USERS A INNER JOIN USERS B ON A.ID = B.ID")
            .unwrap();
        let mut rows = transaction.query(&statement, Parameters::none()).unwrap();
        let mut actual = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            actual.push(row.values().to_vec());
        }
        assert_eq!(
            actual,
            vec![
                vec![Value::Int64(1), Value::Int64(1)],
                vec![Value::Int64(2), Value::Int64(2)],
                vec![Value::Int64(2), Value::Int64(2)],
                vec![Value::Int64(2), Value::Int64(2)],
                vec![Value::Int64(2), Value::Int64(2)],
                vec![Value::Int64(3), Value::Int64(3)],
            ]
        );
    }

    #[test]
    fn join_releases_rejected_candidates_and_completed_left_rows() {
        // Production break caught: retaining a rejected candidate or an
        // exhausted current-left row makes a later pull exceed the same exact
        // memory ceiling even though each nested-loop step fits independently.
        let name = "x".repeat(1_024);
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let input_bytes = 32 + 4 * value_bytes + 1_024;
        let output_bytes = 32 + 2 * value_bytes;
        let empty_parameter_frame_bytes = 32;
        let exact = empty_parameter_frame_bytes + 3 * input_bytes + output_bytes;
        let fixture = join_fixture_with_rows(
            &[(1, &name), (2, &name)],
            Options::default().with_max_query_memory_bytes(exact),
        );
        assert_eq!(
            run_query(
                &fixture,
                "SELECT A.ID, B.ID FROM USERS A INNER JOIN USERS B ON A.ID = B.ID",
            )
            .unwrap(),
            vec![
                vec![Value::Int64(1), Value::Int64(1)],
                vec![Value::Int64(2), Value::Int64(2)],
            ]
        );
    }

    #[test]
    fn join_on_error_releases_state_without_poisoning_a_write_transaction() {
        // Production break caught: leaking JOIN state on an ordinary ON error
        // or classifying it as fatal prevents the same write transaction from
        // running a later valid query and committing.
        let fixture = join_fixture();
        let mut connection = fixture.database().connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let failing = transaction
            .prepare("SELECT A.ID, B.ID FROM USERS A INNER JOIN USERS B ON A.ID / :1 = B.ID")
            .unwrap();
        let params = [Value::Int64(0).into()];
        let mut rows = transaction
            .query(&failing, Parameters::positional(&params))
            .unwrap();
        assert_eq!(rows.next().unwrap_err().code(), ErrorCode::Type);
        drop(rows);

        let valid = transaction
            .prepare("SELECT A.ID, B.ID FROM USERS A INNER JOIN USERS B ON A.ID = B.ID")
            .unwrap();
        let mut rows = transaction.query(&valid, Parameters::none()).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().values(),
            &[Value::Int64(1), Value::Int64(1)]
        );
        drop(rows);
        transaction.commit().unwrap();
    }
}
