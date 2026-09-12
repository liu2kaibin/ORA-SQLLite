use crate::{
    DataType, Database, ErrorCode, Options, ParameterIdentifier, Result,
    sql::{SqlLimits, bind::bind_statement, bound::BoundStatement, parser::parse_statement},
    storage::test_support::phase_4b_catalog_fixture,
};

/// Prepares one full SQL statement against the Task 9 catalog fixture.
///
/// The fixture remains alive until parsing and binding have both completed, so
/// catalog IDs and the database identity always originate from a real snapshot.
pub(crate) fn prepare_fixture(sql: &str) -> Result<BoundStatement> {
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    let database_uuid = pager.database_uuid()?;
    let mut snapshot = pager.begin_read()?;
    let catalog = snapshot.catalog_reader()?;
    let statement = parse_statement(sql, &SqlLimits::default())?;
    bind_statement(&statement, &catalog, database_uuid)
}

#[test]
fn final_fix_exception_linked_write_parameters_keep_frozen_types() {
    // A component-wide assignment cap plus mutable final binding leaves the
    // earlier Q leaf INT64 while its public metadata becomes DECIMAL.
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let sql = "UPDATE USERS SET ID = :p WHERE :p = :q AND :q = 1.5";
    let statement = db.connect().unwrap().prepare(sql).unwrap();
    assert_exception_parameter_types(
        &statement,
        &[
            ("P", crate::sql::SqlType::Int64, DataType::Int64, 2),
            ("Q", crate::sql::SqlType::Decimal, DataType::Decimal, 2),
        ],
    );
}

#[test]
fn final_fix_exception_assignment_caps_stay_per_parameter() {
    // Sharing a cap across linked identifiers changes Q's declared type; ignoring
    // P's own cap or rewriting a promoted leaf breaks the retained/public match.
    use crate::sql::SqlType::{Decimal, Int64};
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let connection = db.connect().unwrap();
    for (sql, expected) in [
        (
            "UPDATE USERS SET ID = :p, AMOUNT = :q WHERE :p = :q AND :q = 1.5",
            vec![
                ("P", Int64, DataType::Int64, 2),
                ("Q", Decimal, DataType::Decimal, 3),
            ],
        ),
        (
            "UPDATE USERS SET AMOUNT = :q, ID = :p WHERE :q = :p AND :q = 1.5",
            vec![
                ("Q", Decimal, DataType::Decimal, 3),
                ("P", Int64, DataType::Int64, 2),
            ],
        ),
        (
            "UPDATE USERS SET ID = :q WHERE :p = :q AND :p = 1.5",
            vec![
                ("Q", Int64, DataType::Int64, 2),
                ("P", Decimal, DataType::Decimal, 2),
            ],
        ),
        (
            "SELECT :p, :q FROM USERS WHERE :p = :q AND :q = 1.5",
            vec![
                ("P", Decimal, DataType::Decimal, 2),
                ("Q", Decimal, DataType::Decimal, 3),
            ],
        ),
        (
            "UPDATE USERS SET ID = :p, AMOUNT = :p WHERE :p = 1.5",
            vec![("P", Int64, DataType::Int64, 3)],
        ),
    ] {
        let statement = connection.prepare(sql).unwrap();
        assert_exception_parameter_types(&statement, &expected);
    }
    // Removing unification/domain or final assignment validation would accept
    // incompatible linked writes or a Decimal expression into INT64.
    for sql in [
        "UPDATE USERS SET ID = :p WHERE :p = :q AND :q = 'x'",
        "UPDATE USERS SET ID = :p, NAME = :q WHERE :p = :q",
        "UPDATE USERS SET ID = :p + 1.5 WHERE :p = :q AND :q = 1",
    ] {
        assert_eq!(
            connection.prepare(sql).unwrap_err().code(),
            ErrorCode::Type,
            "{sql}"
        );
    }
    for function in ["SUM", "AVG"] {
        let sql = format!("SELECT :p, {function}(:p) = :p, :p + 1 FROM USERS");
        let statement = connection.prepare(&sql).unwrap();
        assert_exception_parameter_types(&statement, &[("P", Decimal, DataType::Decimal, 4)]);
    }
}

#[test]
fn final_fix_exception_finalized_collector_cannot_widen_or_add_parameters() {
    // Re-enabling collection after solve would widen P or create an uncollected Q.
    use crate::sql::{ParameterKey, SqlSpan, SqlType, bind::ParameterCollector};
    let key = ParameterKey::parse(":p", SqlSpan::new(0, 2).unwrap()).unwrap();
    let missing = ParameterKey::parse(":q", SqlSpan::new(3, 5).unwrap()).unwrap();
    let mut collector = ParameterCollector::default();
    collector.constrain(&key, SqlType::Int64).unwrap();
    assert!(collector.solve().unwrap());
    assert_eq!(
        collector.constrain(&key, SqlType::Decimal).unwrap(),
        SqlType::Int64
    );
    assert_eq!(
        collector.constrain(&key, SqlType::Text).unwrap_err().code(),
        ErrorCode::Type
    );
    assert_eq!(
        collector
            .constrain(&missing, SqlType::Unknown)
            .unwrap_err()
            .code(),
        ErrorCode::Internal
    );
    let metadata = collector.finish();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].data_type(), Some(DataType::Int64));
}

#[test]
fn final_fix_exception_finalized_assignment_validates_without_retyping() {
    // Re-running assignment inference would silently narrow a solved Decimal P
    // into INT64, or widen a solved INT64 P for an otherwise legal Decimal write.
    use crate::sql::{
        ParameterKey, SqlSpan, SqlType,
        ast::{AstExpr, AstExprKind, AstStatement},
        bind::{BindingScope, ParameterCollector, bind_assignment_expr},
    };
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    let mut snapshot = pager.begin_read().unwrap();
    let catalog = snapshot.catalog_reader().unwrap();
    let AstStatement::Query(query) =
        parse_statement("SELECT ID FROM USERS", &SqlLimits::default()).unwrap()
    else {
        unreachable!()
    };
    let scope = BindingScope::from_query_sources(&catalog, &query).unwrap();
    let span = SqlSpan::new(0, 2).unwrap();
    let key = ParameterKey::parse(":p", span).unwrap();
    let expression = AstExpr {
        kind: AstExprKind::Parameter(key.clone()),
        span,
    };
    for (source, target, accepted) in [
        (SqlType::Decimal, SqlType::Int64, false),
        (SqlType::Int64, SqlType::Decimal, true),
    ] {
        let mut collector = ParameterCollector::default();
        collector.constrain(&key, source).unwrap();
        collector.solve().unwrap();
        let result = bind_assignment_expr(&expression, &scope, target, &mut collector);
        if accepted {
            assert_eq!(result.unwrap().sql_type, source);
        } else {
            assert_eq!(result.unwrap_err().code(), ErrorCode::Type);
        }
        assert_eq!(collector.constrain(&key, SqlType::Unknown).unwrap(), source);
        assert_eq!(collector.finish().len(), 1);
    }
    let mut empty = ParameterCollector::default();
    assert!(!empty.solve().unwrap());
    assert_eq!(
        bind_assignment_expr(&expression, &scope, SqlType::Int64, &mut empty)
            .unwrap_err()
            .code(),
        ErrorCode::Internal
    );
    assert!(empty.finish().is_empty());
}

fn assert_exception_parameter_types(
    statement: &crate::Statement,
    expected: &[(&str, crate::sql::SqlType, DataType, usize)],
) {
    use crate::sql::bound::{BoundExprKind, BoundStatementKind};
    assert_eq!(statement.parameters().len(), expected.len());
    for (metadata, &(name, _, public_type, _)) in statement.parameters().iter().zip(expected) {
        assert_eq!(
            metadata.identifier(),
            &ParameterIdentifier::Name(name.to_owned())
        );
        assert_eq!(metadata.data_type(), Some(public_type));
    }
    let mut work = Vec::new();
    match &statement.bound().kind {
        BoundStatementKind::Update {
            assignments,
            selection,
            ..
        } => {
            work.extend(assignments.iter().map(|assignment| &assignment.value));
            work.extend(selection.iter());
        }
        BoundStatementKind::Query { query } => {
            work.extend(query.projection.iter());
            work.extend(query.selection.iter());
        }
        _ => panic!("exception fixture must be UPDATE or SELECT"),
    }
    let mut counts = vec![0; expected.len()];
    while let Some(expression) = work.pop() {
        match &expression.kind {
            BoundExprKind::Parameter(ParameterIdentifier::Name(name)) => {
                let index = expected.iter().position(|entry| entry.0 == name).unwrap();
                assert_eq!(
                    expression.sql_type, expected[index].1,
                    "{name} at {:?}",
                    expression.span
                );
                counts[index] += 1;
            }
            BoundExprKind::Binary { left, right, .. } => {
                work.push(left);
                work.push(right);
            }
            BoundExprKind::Unary { expr, .. } => work.push(expr),
            BoundExprKind::Aggregate {
                argument: Some(argument),
                ..
            } => work.push(argument),
            BoundExprKind::Literal(_) | BoundExprKind::Column { .. } => {}
            _ => panic!("unexpected expression in exception fixture"),
        }
    }
    assert_eq!(
        counts,
        expected.iter().map(|entry| entry.3).collect::<Vec<_>>()
    );
}

#[test]
fn final_fix_retains_query_semantics_at_public_statement_boundary() {
    // Dropping WHERE/JOIN/DISTINCT/GROUP/ORDER at Statement must change this result.
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    for (left, right) in [
        (
            "SELECT ID FROM USERS WHERE ID = 1",
            "SELECT ID FROM USERS WHERE ID = 2",
        ),
        ("SELECT ID FROM USERS", "SELECT DISTINCT ID FROM USERS"),
        (
            "SELECT U.ID FROM USERS U INNER JOIN ORDERS O ON U.ID = O.USER_ID",
            "SELECT U.ID FROM USERS U LEFT JOIN ORDERS O ON U.ID = O.USER_ID",
        ),
        (
            "SELECT COUNT(*) FROM USERS GROUP BY ID HAVING COUNT(*) > 1",
            "SELECT COUNT(*) FROM USERS GROUP BY NAME HAVING COUNT(*) > 2",
        ),
        (
            "SELECT ID FROM USERS ORDER BY NAME ASC NULLS FIRST",
            "SELECT ID FROM USERS ORDER BY NAME DESC NULLS LAST",
        ),
    ] {
        assert_ne!(
            conn.prepare(left).unwrap().bound().kind,
            conn.prepare(right).unwrap().bound().kind,
            "{left} lost semantics"
        );
    }
}

#[test]
fn final_fix_self_join_grouping_distinguishes_source_occurrences() {
    assert_eq!(
        prepare_fixture("SELECT M.ID FROM USERS U INNER JOIN USERS M ON U.ID = M.ID GROUP BY U.ID")
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
    let bound = prepare_fixture(
        "SELECT U.ID, M.ID FROM USERS U INNER JOIN USERS M ON U.ID = M.ID GROUP BY U.ID, M.ID",
    )
    .unwrap();
    assert_eq!(bound.dependencies.len(), 1);
    let crate::sql::bound::BoundStatementKind::Query { query } = &bound.kind else {
        panic!("query");
    };
    assert_eq!(query.sources.len(), 2);
    assert_eq!(query.sources[0].table_id, query.sources[1].table_id);
    assert_ne!(query.sources[0].source_id, query.sources[1].source_id);
    let ids = query
        .projection
        .iter()
        .map(|expr| match expr.kind {
            crate::sql::bound::BoundExprKind::Column { source_id, .. } => source_id,
            _ => panic!("column"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            crate::sql::bound::SourceId(0),
            crate::sql::bound::SourceId(1)
        ]
    );
}

#[test]
fn final_fix_schema_validation_visits_every_retained_query_clause() {
    use crate::sql::{
        ast::{JoinKind, NullPlacement, OrderDirection},
        bound::{BoundExpr, BoundExprKind, BoundOrderExpression, BoundStatementKind},
    };
    fn replace_column(expr: &mut BoundExpr) {
        let mut work = vec![expr];
        while let Some(expr) = work.pop() {
            match &mut expr.kind {
                BoundExprKind::Column { column_id, .. } => {
                    *column_id = crate::storage::catalog::types::ColumnId::new(1_000_000).unwrap();
                    return;
                }
                BoundExprKind::Binary { left, right, .. } => {
                    work.push(left);
                    work.push(right);
                }
                _ => {}
            }
        }
        panic!("fixture clause has no column");
    }
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let statement = db.connect().unwrap().prepare("SELECT DISTINCT U.ID, COUNT(O.ID) C FROM USERS U LEFT JOIN ORDERS O ON U.ID = O.USER_ID WHERE U.ID > 0 GROUP BY U.ID HAVING U.ID > 0 ORDER BY U.ID DESC NULLS FIRST, C").unwrap();
    drop(db);
    let pager = fixture.pager();
    let mut read = pager.begin_read().unwrap();
    let catalog = read.catalog_reader().unwrap();
    let uuid = pager.database_uuid().unwrap();
    let BoundStatementKind::Query { query } = &statement.bound().kind else {
        panic!("query");
    };
    assert!(query.distinct);
    assert_eq!(query.sources.len(), 2);
    assert_eq!(query.joins.len(), 1);
    assert_eq!(query.joins[0].kind, JoinKind::Left);
    assert_eq!(query.joins[0].source_id, query.sources[1].source_id);
    assert_eq!(query.order_by[0].direction, OrderDirection::Descending);
    assert_eq!(query.order_by[0].nulls, NullPlacement::First);
    assert_eq!(
        query.order_by[1].expression,
        BoundOrderExpression::Projection(1)
    );
    statement.validate_schema(uuid, &catalog).unwrap();
    for clause in 0..6 {
        let mut invalid = statement.bound().clone();
        let BoundStatementKind::Query { query } = &mut invalid.kind else {
            panic!("query");
        };
        let expression = match clause {
            0 => &mut query.projection[0],
            1 => &mut query.joins[0].condition,
            2 => query.selection.as_mut().unwrap(),
            3 => &mut query.group_by[0],
            4 => query.having.as_mut().unwrap(),
            5 => match &mut query.order_by[0].expression {
                BoundOrderExpression::Expression(expression) => expression,
                _ => panic!("expression order"),
            },
            _ => unreachable!(),
        };
        replace_column(expression);
        assert_eq!(
            invalid.validate_schema(uuid, &catalog).unwrap_err().code(),
            ErrorCode::SchemaChanged,
            "clause {clause}"
        );
    }
}

#[test]
fn final_fix_parameters_finalize_earlier_outputs() {
    let statement = prepare_fixture("SELECT :p, :p + 1 FROM USERS").unwrap();
    assert_eq!(statement.columns[0].data_type(), DataType::Int64);
    assert_eq!(statement.columns[1].data_type(), DataType::Int64);
}

#[test]
fn final_fix_parameter_finalization_precedes_grouping_validation() {
    let bound =
        prepare_fixture("SELECT ID + :p FROM USERS GROUP BY ID + :p HAVING :p = 1.5").unwrap();
    assert_eq!(bound.columns[0].data_type(), DataType::Decimal);
}

#[test]
fn final_fix_parameter_solver_propagates_derived_aggregate_output_constraints() {
    let bound =
        prepare_fixture("SELECT :q, :r, SUM(:p) = :q, SUM(:q) = :r, :p + 1 FROM USERS").unwrap();
    assert_eq!(bound.columns[0].data_type(), DataType::Decimal);
    assert_eq!(bound.columns[1].data_type(), DataType::Decimal);
    assert_eq!(
        bound
            .parameters
            .iter()
            .map(|p| p.data_type())
            .collect::<Vec<_>>(),
        [
            Some(DataType::Decimal),
            Some(DataType::Decimal),
            Some(DataType::Int64)
        ]
    );
}

#[test]
fn final_fix_parameter_solver_keeps_single_parameter_aggregate_self_links() {
    // Skipping constraint collection for one distinct parameter leaves the first
    // output INT64 while a later SUM/AVG comparison widens its metadata.
    for function in ["SUM", "AVG"] {
        let sql = format!("SELECT :p, {function}(:p) = :p, :p + 1 FROM USERS");
        let bound = prepare_fixture(&sql).unwrap();
        assert_eq!(bound.columns[0].data_type(), DataType::Decimal, "{sql}");
        assert_eq!(bound.columns[2].data_type(), DataType::Decimal, "{sql}");
        assert_eq!(bound.parameters[0].data_type(), Some(DataType::Decimal));
    }
}

#[test]
fn final_fix_depth_preflight_preserves_qualified_keyword_spans() {
    let sql = "SELECT T . UNION FROM T UNION SELECT U . UNION FROM U";
    let error = parse_statement(sql, &SqlLimits::default()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unsupported);
    assert_eq!(error.sql_location().unwrap().byte_offset(), 24);
}

#[test]
fn final_fix_parameter_solution_reaches_every_retained_expression_family() {
    for (sql, expected) in [
        (
            "SELECT :a, :b, :a = :b, :b = :c, :c + 1 FROM USERS",
            vec![
                DataType::Int64,
                DataType::Int64,
                DataType::Bool,
                DataType::Bool,
                DataType::Int64,
            ],
        ),
        (
            "SELECT SUM(:a), :a + 1 FROM USERS",
            vec![DataType::Decimal, DataType::Int64],
        ),
        (
            "SELECT MIN(:p), :p + 1 FROM USERS",
            vec![DataType::Int64, DataType::Int64],
        ),
        (
            "SELECT :a IN (:b, :c), :c + 1, :b FROM USERS",
            vec![DataType::Bool, DataType::Int64, DataType::Int64],
        ),
    ] {
        let bound = prepare_fixture(sql).unwrap();
        assert_eq!(
            bound
                .columns
                .iter()
                .map(|column| column.data_type())
                .collect::<Vec<_>>(),
            expected,
            "{sql}"
        );
        assert!(
            bound
                .parameters
                .iter()
                .all(|parameter| parameter.data_type() == Some(DataType::Int64)),
            "{sql}"
        );
    }
    for sql in [
        "SELECT SUM(:p), :p = 'x' FROM USERS",
        "SELECT :a BETWEEN :b AND :c, :c = X'AB' FROM USERS",
        "SELECT :a IN (:b), :a = 1, :b = 'x' FROM USERS",
        "UPDATE USERS SET ID = :p WHERE :p = :q AND :q = 'x'",
        "CREATE TABLE FLAGS (B BOOL CHECK (:a = :b AND :a = TRUE AND :b = 1))",
    ] {
        assert_eq!(
            prepare_fixture(sql).unwrap_err().code(),
            ErrorCode::Type,
            "{sql}"
        );
    }
}

#[test]
fn final_fix_parameters_revalidate_linked_types_and_domains() {
    for sql in [
        "SELECT ID FROM USERS WHERE :a = :b AND :a = 1 AND :b = 'x'",
        "SELECT :a + :b, :a = 'x' FROM USERS",
        "SELECT :a < :b, :a = X'AB' FROM USERS",
    ] {
        assert_eq!(
            prepare_fixture(sql).unwrap_err().code(),
            ErrorCode::Type,
            "{sql}"
        );
    }
}

#[test]
fn final_fix_parameter_metadata_is_lexical_through_public_prepare() {
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    let statement = conn.prepare("SELECT :projection FROM USERS U INNER JOIN ORDERS O ON O.ID = :joined WHERE U.ID = :filter ORDER BY :ordered, :PROJECTION").unwrap();
    assert_eq!(
        statement
            .parameters()
            .iter()
            .map(|p| p.identifier().clone())
            .collect::<Vec<_>>(),
        ["PROJECTION", "JOINED", "FILTER", "ORDERED"]
            .map(|name| ParameterIdentifier::Name(name.to_owned()))
    );
}

#[test]
fn final_fix_boolean_literals_use_common_expression_conversion() {
    for sql in [
        "INSERT INTO USERS (ID) VALUES (TRUE)",
        "UPDATE USERS SET ID = FALSE",
    ] {
        assert_eq!(
            prepare_fixture(sql).unwrap_err().code(),
            ErrorCode::Type,
            "Bool versus INT64: {sql}"
        );
    }
    for sql in [
        "SELECT TRUE, FALSE FROM USERS WHERE TRUE",
        "UPDATE USERS SET ID = 1 WHERE FALSE",
        "DELETE FROM USERS WHERE TRUE",
        "CREATE TABLE FLAGS (B BOOL CHECK (B = TRUE), CHECK (FALSE OR B))",
    ] {
        prepare_fixture(sql).unwrap();
    }
    for sql in [
        "SELECT \"TRUE\" FROM USERS",
        "SELECT U.TRUE FROM USERS U",
        "DELETE FROM USERS WHERE \"FALSE\"",
    ] {
        assert_eq!(
            prepare_fixture(sql).unwrap_err().code(),
            ErrorCode::Bind,
            "{sql}"
        );
    }
}

#[test]
fn final_fix_wildcard_expansion_obeys_bound_node_budget() {
    let fixture = phase_4b_catalog_fixture(4096);
    for (limit, accepted) in [(11, false), (12, true), (13, true)] {
        let limits = SqlLimits::default().with_max_ast_nodes(limit).unwrap();
        let result = public_prepare_with_limits(&fixture, limits, "SELECT *, U.*, * FROM USERS U");
        assert_eq!(
            result.is_ok(),
            accepted,
            "twelve output nodes with limit {limit}"
        );
        if let Ok(statement) = result {
            assert_eq!(statement.columns().len(), 12);
        }
    }
}

#[test]
fn final_fix_wide_catalog_expansion_stops_at_default_budget_without_writes() {
    use crate::storage::{catalog::types::ColumnType, test_support::phase_4b_add_table};
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    let names = (0..1000).map(|i| format!("C{i}")).collect::<Vec<_>>();
    let definitions = names
        .iter()
        .map(|name| (name.as_str(), ColumnType::Int64, true))
        .collect::<Vec<_>>();
    phase_4b_add_table(&pager, "WIDE", &definitions).unwrap();
    drop(pager);
    let before_main = std::fs::read(fixture.main_path()).unwrap();
    let before_wal = std::fs::read(fixture.wal_path()).unwrap();
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    for (stars, columns) in [
        (99, Some(99_000)),
        (100, Some(100_000)),
        (101, None),
        (5000, None),
    ] {
        let sql = format!("SELECT {} FROM WIDE", vec!["*"; stars].join(","));
        let result = conn.prepare(&sql);
        if let Some(columns) = columns {
            assert_eq!(result.unwrap().columns().len(), columns);
        } else {
            assert_eq!(result.unwrap_err().code(), ErrorCode::Bind);
        }
    }
    drop(conn);
    drop(db);
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);
}

#[test]
fn final_fix_boolean_dml_and_quoted_columns_bind_against_real_catalog() {
    use crate::storage::{catalog::types::ColumnType, test_support::phase_4b_add_table};
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    phase_4b_add_table(
        &pager,
        "FLAGS",
        &[
            ("B", ColumnType::Bool, true),
            ("TRUE", ColumnType::Int64, true),
        ],
    )
    .unwrap();
    drop(pager);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    for sql in [
        "INSERT INTO FLAGS (B) VALUES (TRUE)",
        "UPDATE FLAGS SET B = FALSE WHERE TRUE",
        "DELETE FROM FLAGS WHERE B = FALSE",
    ] {
        conn.prepare(sql).unwrap();
    }
    let statement = conn
        .prepare("SELECT \"TRUE\", F.TRUE, TRUE FROM FLAGS F")
        .unwrap();
    assert_eq!(
        statement
            .columns()
            .iter()
            .map(|column| column.data_type())
            .collect::<Vec<_>>(),
        [DataType::Int64, DataType::Int64, DataType::Bool]
    );
    assert!(statement.columns()[0].source_column_id().is_some());
    assert!(statement.columns()[1].source_column_id().is_some());
    assert!(statement.columns()[2].source_column_id().is_none());
}

#[test]
fn final_fix_safety_subprocess() {
    const CHILD: &str = "ORA_SQL_FINAL_FIX_SAFETY_CASE";
    if let Ok(case) = std::env::var(CHILD) {
        match case.as_str() {
            "flat" | "rejected" => {
                let mut sql = format!("SELECT {} FROM USERS", vec!["1"; 10_000].join("+"));
                if case == "rejected" {
                    sql.push_str(" WHERE (");
                }
                assert!(parse_statement(&sql, &SqlLimits::default()).is_err());
            }
            "nested" => {
                let sql = format!(
                    "SELECT {}1{} FROM USERS",
                    "(".repeat(10_000),
                    ")".repeat(10_000)
                );
                assert!(parse_statement(&sql, &SqlLimits::default()).is_err());
            }
            "owned_drop" => {
                use crate::sql::{
                    SqlSpan,
                    ast::{AstExpr, AstExprKind, UnaryOp},
                    bind::{BindingScope, ParameterCollector, bind_expr},
                };
                let span = SqlSpan::new(0, 1).unwrap();
                let mut expr = AstExpr {
                    kind: AstExprKind::Integer("1".to_owned()),
                    span,
                };
                for _ in 0..30_000 {
                    expr = AstExpr {
                        kind: AstExprKind::Unary {
                            op: UnaryOp::Positive,
                            expr: Box::new(expr),
                        },
                        span,
                    };
                }
                let fixture = phase_4b_catalog_fixture(4096);
                let pager = fixture.pager();
                let mut read = pager.begin_read().unwrap();
                let catalog = read.catalog_reader().unwrap();
                let parsed =
                    parse_statement("SELECT ID FROM USERS", &SqlLimits::default()).unwrap();
                let crate::sql::ast::AstStatement::Query(query) = parsed else {
                    panic!("query");
                };
                let scope = BindingScope::from_query_sources(&catalog, &query).unwrap();
                let bound =
                    bind_expr(&expr, &scope, None, &mut ParameterCollector::default()).unwrap();
                drop(bound);
                drop(expr);
            }
            "normal_drop" => {
                let sql = format!("SELECT {} FROM USERS", vec!["1"; 120].join("+"));
                drop(prepare_fixture(&sql).unwrap());
                assert!(SqlLimits::default().with_max_nesting(65_535).is_err());
            }
            "configured_boundaries" => {
                for limit in [128, 255, 256] {
                    let limits = SqlLimits::default().with_max_nesting(limit).unwrap();
                    for (leaves, accepted) in
                        [(limit - 3, true), (limit - 2, true), (limit - 1, false)]
                    {
                        let sql = format!("SELECT {} FROM USERS", vec!["1"; leaves].join("+"));
                        assert_eq!(
                            parse_statement(&sql, &limits).is_ok(),
                            accepted,
                            "depth {limit}, leaves {leaves}"
                        );
                    }
                    let sql = format!("SELECT {} + FROM USERS", vec!["1"; limit - 2].join("+"));
                    assert!(parse_statement(&sql, &limits).is_err());
                }
                assert!(SqlLimits::default().with_max_nesting(257).is_err());
            }
            "unsupported_chains" => {
                for sql in [
                    format!("SELECT 1{} FROM USERS", "::INT".repeat(10_000)),
                    format!("SELECT {}TRUE FROM USERS", "NOT ".repeat(10_000)),
                    vec!["SELECT 1 FROM USERS"; 10_000].join(" UNION "),
                ] {
                    assert!(parse_statement(&sql, &SqlLimits::default()).is_err());
                }
            }
            _ => panic!("unknown child case"),
        }
        return;
    }
    let mut failures = Vec::new();
    for case in [
        "flat",
        "rejected",
        "nested",
        "owned_drop",
        "normal_drop",
        "configured_boundaries",
        "unsupported_chains",
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sql::bind::test_support::final_fix_safety_subprocess",
                "--nocapture",
            ])
            .env(CHILD, case)
            .output()
            .unwrap();
        if !output.status.success() || !String::from_utf8_lossy(&output.stdout).contains("1 passed")
        {
            failures.push(format!(
                "case {case}: status {:?}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn prepare_returns_metadata_without_writing_database_or_wal() {
    // Production break caught: omitting any public prepare entry point, using a
    // different catalog view, or publishing prepare-time storage changes would
    // fail this real database/WAL and metadata contract.
    let fixture = phase_4b_catalog_fixture(4096);
    let before_main = std::fs::read(fixture.main_path()).unwrap();
    let before_wal = std::fs::read(fixture.wal_path()).unwrap();
    let sql = "SELECT ID, NAME FROM USERS WHERE ID = :1";

    let statement = {
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let conn = db.connect().unwrap();
        conn.prepare(sql).unwrap()
    };
    assert_eq!(statement.parameters().len(), 1);
    assert_eq!(
        statement.parameters()[0].identifier(),
        &ParameterIdentifier::Position(1)
    );
    assert_eq!(statement.parameters()[0].data_type(), Some(DataType::Int64));
    assert_eq!(statement.columns().len(), 2);
    assert_eq!(statement.columns()[0].name(), "ID");
    assert_eq!(statement.columns()[0].data_type(), DataType::Int64);
    assert!(!statement.columns()[0].nullable());
    assert!(statement.columns()[0].source_table_id().is_some());
    assert!(statement.columns()[0].source_column_id().is_some());
    assert_eq!(statement.columns()[1].name(), "NAME");
    assert_eq!(statement.columns()[1].data_type(), DataType::Text);
    assert!(statement.columns()[1].nullable());
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);

    {
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut conn = db.connect().unwrap();
        let mut read = conn.begin_read().unwrap();
        let statement = read.prepare(sql).unwrap();
        assert_eq!(statement.parameters().len(), 1);
        assert_eq!(statement.columns().len(), 2);
    }
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);

    {
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut conn = db.connect().unwrap();
        let mut write = conn.begin_write().unwrap();
        let statement = write.prepare(sql).unwrap();
        assert_eq!(statement.parameters().len(), 1);
        assert_eq!(statement.columns().len(), 2);
        write.rollback().unwrap();
    }
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);

    {
        let db = Database::open(fixture.main_path(), Options::default().read_only()).unwrap();
        let conn = db.connect().unwrap();
        let statement = conn.prepare(sql).unwrap();
        assert_eq!(statement.parameters().len(), 1);
        assert_eq!(statement.columns().len(), 2);
    }
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);
}

#[test]
fn write_transaction_prepare_leaves_an_empty_batch_unmodified() {
    // Production break caught: any prepare-time batch mutation would make an
    // otherwise empty commit publish different database or WAL bytes.
    let fixture = phase_4b_catalog_fixture(4096);
    let before_main = std::fs::read(fixture.main_path()).unwrap();
    let before_wal = std::fs::read(fixture.wal_path()).unwrap();

    {
        let db = Database::open(fixture.main_path(), Options::default()).unwrap();
        let mut connection = db.connect().unwrap();
        let mut write = connection.begin_write().unwrap();
        write
            .prepare("SELECT ID, NAME FROM USERS WHERE ID = :1")
            .unwrap();
        write.commit().unwrap();
    }

    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);
}

#[test]
fn prepare_rejects_first_logical_parameter_over_limit() {
    // Production break caught: checking the limit after catalog binding both
    // allocates all logical parameters and lets an unrelated catalog error hide
    // the first excess normalized parameter and its source location.
    let fixture = phase_4b_catalog_fixture(4096);
    let limits = SqlLimits::default().with_max_parameters(2).unwrap();
    let db = Database::open(
        fixture.main_path(),
        Options::default().with_sql_limits(limits),
    )
    .unwrap();
    let conn = db.connect().unwrap();

    let below = conn.prepare("SELECT :first, :FIRST FROM USERS").unwrap();
    assert_eq!(below.parameters().len(), 1);

    let exact = conn
        .prepare("SELECT :first, :FIRST, :second FROM USERS")
        .unwrap();
    assert_eq!(exact.parameters().len(), 2);

    let above = "SELECT :first, :FIRST, :second,\n  :third FROM MISSING";
    let error = conn.prepare(above).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Bind);
    let location = error
        .sql_location()
        .expect("the first excess logical parameter has a source location");
    assert_eq!(location.byte_offset(), above.find(":third").unwrap());
    assert_eq!((location.line(), location.column()), (2, 3));
}

fn public_prepare_with_limits(
    fixture: &crate::storage::test_support::Phase4bCatalogFixture,
    limits: SqlLimits,
    sql: &str,
) -> Result<crate::Statement> {
    let db = Database::open(
        fixture.main_path(),
        Options::default().with_sql_limits(limits),
    )?;
    db.connect()?.prepare(sql)
}

#[test]
fn phase_4b_sql_corpus() {
    // Production break caught: bypassing any parser, normalization, catalog
    // binding, static typing, or metadata stage changes an approved statement's
    // result or an excluded statement's stable error classification.
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();

    let accepted = [
        "SELECT ID, NAME FROM USERS WHERE ID = :1",
        " -- lead\n SELECT /* projection */ \"ID\" FROM \"USERS\"; -- tail",
        "SELECT * FROM USERS",
        "SELECT U.*, O.USER_ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID",
        "SELECT U.ID, O.ID FROM USERS U LEFT OUTER JOIN ORDERS O ON O.USER_ID = U.ID",
        "SELECT DISTINCT NAME FROM USERS WHERE NAME IS NOT NULL ORDER BY NAME",
        "SELECT NAME AS DISPLAY_NAME FROM USERS ORDER BY DISPLAY_NAME",
        "SELECT USER_ID, COUNT(*), SUM(ID), AVG(ID), MIN(ID), MAX(ID) FROM ORDERS GROUP BY USER_ID HAVING COUNT(*) > 0 ORDER BY 1",
        "SELECT NULL, TRUE, 1, 1.5, 1E2, 'A''B', X'ABCD' FROM USERS",
        "SELECT ID FROM USERS WHERE NOT (ID BETWEEN 1 AND 10) OR (NAME LIKE 'A%' AND ID IN (1, 2) AND NAME IS NOT NULL)",
        "SELECT +ID, -ID, ID + 1, ID - 1, ID * 2, ID / 2, ID % 2 FROM USERS WHERE ID <> 0 AND ID != 1 AND ID < 10 AND ID <= 10 AND ID > -1 AND ID >= 0 AND ID IS NOT NULL AND ID NOT IN (3) AND ID NOT BETWEEN 4 AND 5 AND NAME NOT LIKE 'Z%'",
        "SELECT :1, :1 FROM USERS",
        "SELECT :name, :NAME, :NaMe FROM USERS",
        "SELECT :2, :1 FROM USERS",
        "INSERT INTO USERS (ID, NAME) VALUES (:id, :name)",
        "INSERT INTO USERS VALUES (:id, :name, :amount, :float_col)",
        "UPDATE USERS U SET NAME = U.NAME, ID = ID + :delta WHERE U.ID = :id",
        "DELETE FROM USERS U WHERE U.ID = :id",
        "CREATE TABLE CHILD (ID INT64 PRIMARY KEY, USER_ID INT64 REFERENCES USERS (ID), NAME TEXT NOT NULL, AMOUNT DECIMAL, SCORE FLOAT64, RAW_VALUE BLOB, CREATED DATE, UPDATED TIMESTAMP, ENABLED BOOL, CONSTRAINT CHILD_CK CHECK (ID > 0), UNIQUE (NAME))",
        "CREATE INDEX USERS_NAME_IDX ON USERS (NAME)",
        "CREATE UNIQUE INDEX USERS_NAME_UK ON USERS (NAME)",
        "DROP TABLE ORDERS",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
    ];
    for sql in accepted {
        conn.prepare(sql)
            .unwrap_or_else(|error| panic!("accepted corpus rejected {sql:?}: {error}"));
    }

    for (sql, logical_parameters) in [
        ("SELECT :1, :1 FROM USERS", 1),
        ("SELECT :name, :NAME, :NaMe FROM USERS", 1),
        ("SELECT :2, :1 FROM USERS", 2),
    ] {
        assert_eq!(
            conn.prepare(sql).unwrap().parameters().len(),
            logical_parameters,
            "wrong normalized logical-parameter count for {sql:?}"
        );
    }

    for (bytes, accepted) in [(127, true), (128, true), (129, false)] {
        let identifier = "A".repeat(bytes);
        let sql = format!("CREATE TABLE {identifier} (ID INT64)");
        let result = conn.prepare(&sql);
        assert_eq!(
            result.is_ok(),
            accepted,
            "wrong public-prepare result for a {bytes}-byte identifier: {result:?}"
        );
        if !accepted {
            assert_eq!(result.unwrap_err().code(), ErrorCode::Bind);
        }
    }

    let utf8_identifier_cases = [
        (format!("{}A", "名".repeat(42)), 127, true),
        (format!("{}AA", "名".repeat(42)), 128, true),
        ("名".repeat(43), 129, false),
    ];
    for (identifier, expected_bytes, accepted) in utf8_identifier_cases {
        assert_eq!(
            identifier.len(),
            expected_bytes,
            "test construction must hit the intended UTF-8 byte boundary"
        );
        let sql = format!("CREATE TABLE {identifier} (ID INT64)");
        let result = conn.prepare(&sql);
        assert_eq!(
            result.is_ok(),
            accepted,
            "wrong public-prepare result for a {expected_bytes}-byte non-ASCII identifier: {result:?}"
        );
        if !accepted {
            assert_eq!(result.unwrap_err().code(), ErrorCode::Bind);
        }
    }

    let rejected = [
        ("", ErrorCode::Syntax),
        ("SELECT ID FROM USERS; DELETE FROM USERS", ErrorCode::Syntax),
        ("SELECT FROM USERS", ErrorCode::Syntax),
        (
            "SELECT /* outer /* nested */ still outer */ ID FROM USERS",
            ErrorCode::Syntax,
        ),
        ("SELECT ID FROM [USERS]", ErrorCode::Syntax),
        ("SELECT ID FROM `USERS`", ErrorCode::Syntax),
        ("CREATE TABLE \"\" (ID INT64)", ErrorCode::Bind),
        ("CREATE TABLE EXISTS (ID INT64)", ErrorCode::Bind),
        ("SELECT ID FROM USERS U WHERE USERS.ID = 1", ErrorCode::Bind),
        (
            "SELECT X.ID FROM USERS X INNER JOIN ORDERS X ON X.ID = X.USER_ID",
            ErrorCode::Bind,
        ),
        (
            "SELECT ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID",
            ErrorCode::Bind,
        ),
        (
            "SELECT ID FROM USERS U INNER JOIN ORDERS O ON O.ID = U.ID",
            ErrorCode::Bind,
        ),
        ("SELECT MISSING FROM USERS", ErrorCode::Bind),
        ("SELECT USERS.ID.VALUE FROM USERS", ErrorCode::Unsupported),
        ("SELECT ID FROM APP.USERS", ErrorCode::Unsupported),
        ("SELECT ID FROM USERS@REMOTE", ErrorCode::Unsupported),
        ("SELECT :1, :name FROM USERS", ErrorCode::Bind),
        ("SELECT :0 FROM USERS", ErrorCode::Bind),
        ("SELECT :01 FROM USERS", ErrorCode::Bind),
        ("SELECT :65536 FROM USERS", ErrorCode::Bind),
        ("SELECT :\"name\" FROM USERS", ErrorCode::Bind),
        ("SELECT :1, :3 FROM USERS", ErrorCode::Bind),
        (
            "SELECT ID FROM (SELECT ID FROM USERS) X",
            ErrorCode::Unsupported,
        ),
        (
            "WITH X AS (SELECT ID FROM USERS) SELECT ID FROM X",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT ID FROM USERS UNION SELECT ID FROM ORDERS",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT ID FROM USERS INTERSECT SELECT ID FROM ORDERS",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT ID FROM USERS MINUS SELECT ID FROM ORDERS",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT U.ID FROM USERS U RIGHT JOIN ORDERS O ON O.USER_ID = U.ID",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT U.ID FROM USERS U CROSS JOIN ORDERS O",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT U.ID FROM USERS U FULL JOIN ORDERS O ON O.USER_ID = U.ID",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT U.ID FROM USERS U NATURAL JOIN ORDERS O",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT U.ID FROM USERS U INNER JOIN ORDERS O USING (ID)",
            ErrorCode::Unsupported,
        ),
        ("SELECT ID FROM USERS LIMIT 1", ErrorCode::Unsupported),
        ("SELECT ID FROM USERS OFFSET 1 ROWS", ErrorCode::Unsupported),
        (
            "SELECT ID FROM USERS FETCH FIRST 1 ROWS ONLY",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT ID FROM USERS WHERE ROWNUM <= 1",
            ErrorCode::Unsupported,
        ),
        ("SELECT SUM(ID) OVER () FROM USERS", ErrorCode::Unsupported),
        (
            "SELECT ID FROM USERS QUALIFY ID = 1",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT * FROM USERS PIVOT (SUM(ID) FOR NAME IN ('A'))",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT ID FROM USERS MODEL RETURN UPDATED ROWS",
            ErrorCode::Unsupported,
        ),
        (
            "SELECT ID FROM USERS CONNECT BY PRIOR ID = ID",
            ErrorCode::Unsupported,
        ),
        ("SELECT ID FROM USERS FOR UPDATE", ErrorCode::Unsupported),
        ("SELECT ID FROM DUAL", ErrorCode::Bind),
        (
            "SELECT CASE WHEN ID = 1 THEN 1 ELSE 0 END FROM USERS",
            ErrorCode::Unsupported,
        ),
        ("SELECT CAST(ID AS TEXT) FROM USERS", ErrorCode::Unsupported),
        ("SELECT ABS(ID) FROM USERS", ErrorCode::Unsupported),
        (
            "SELECT ID FROM USERS WHERE ID IN (SELECT ID FROM ORDERS)",
            ErrorCode::Unsupported,
        ),
        ("SELECT NAME || NAME FROM USERS", ErrorCode::Unsupported),
        ("SELECT ID + NAME FROM USERS", ErrorCode::Type),
        ("CREATE TABLE USERS (ID INT64)", ErrorCode::Bind),
        (
            "CREATE TABLE ORACLE_TYPES (N NUMBER(10,2))",
            ErrorCode::Unsupported,
        ),
        (
            "CREATE TABLE ORACLE_VARCHAR (V VARCHAR2(10))",
            ErrorCode::Unsupported,
        ),
        (
            "CREATE TABLE ORACLE_CHAR (V CHAR(10))",
            ErrorCode::Unsupported,
        ),
        (
            "CREATE TABLE ORACLE_RAW (V RAW(10))",
            ErrorCode::Unsupported,
        ),
        (
            "CREATE TABLE ORACLE_TIME (V TIMESTAMP WITH TIME ZONE)",
            ErrorCode::Unsupported,
        ),
        (
            "CREATE TABLE BAD_CHECK (NAME TEXT, CHECK (NAME))",
            ErrorCode::Type,
        ),
        ("DROP TABLE MISSING", ErrorCode::Bind),
        ("DROP INDEX MISSING", ErrorCode::Bind),
        (
            "MERGE INTO USERS U USING ORDERS O ON (U.ID = O.USER_ID) WHEN MATCHED THEN UPDATE SET U.NAME = 'X'",
            ErrorCode::Unsupported,
        ),
        ("TRUNCATE TABLE USERS", ErrorCode::Unsupported),
        ("GRANT SELECT ON USERS TO APP", ErrorCode::Unsupported),
        ("CREATE USER APP PASSWORD = 'x'", ErrorCode::Unsupported),
        ("CREATE SCHEMA APP", ErrorCode::Unsupported),
        ("CREATE TABLESPACE APP DATAFILE 'x'", ErrorCode::Unsupported),
        ("BEGIN NULL; END;", ErrorCode::Unsupported),
        ("BEGIN TRANSACTION", ErrorCode::Unsupported),
    ];
    for (sql, expected) in rejected {
        let error = match conn.prepare(sql) {
            Err(error) => error,
            Ok(statement) => panic!("rejected corpus accepted {sql:?}: {statement:?}"),
        };
        assert_eq!(
            error.code(),
            expected,
            "wrong classification for {sql:?}: {error}"
        );
    }
}

#[test]
fn phase_4b_prepare_limits_cover_below_exact_and_above_boundaries() {
    // Production break caught: off-by-one comparisons at any public preparation
    // budget would reject the exact boundary or accept the first excess unit.
    let fixture = phase_4b_catalog_fixture(4096);
    let simple = "SELECT ID FROM USERS";

    for (limit, accepted) in [
        (simple.len() - 1, false),
        (simple.len(), true),
        (simple.len() + 1, true),
    ] {
        let limits = SqlLimits::default().with_max_sql_bytes(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "SQL byte limit {limit}"
        );
    }
    for (limit, accepted) in [(3, false), (4, true), (5, true)] {
        let limits = SqlLimits::default().with_max_tokens(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "token limit {limit}"
        );
    }
    for (limit, accepted) in [(3, false), (4, true), (5, true)] {
        let limits = SqlLimits::default().with_max_ast_nodes(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "AST node limit {limit}"
        );
    }
    for (limit, accepted) in [(2, false), (3, true), (4, true)] {
        let limits = SqlLimits::default().with_max_nesting(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, simple).is_ok(),
            accepted,
            "nesting limit {limit}"
        );
    }

    let two_parameters = "SELECT :first, :second FROM USERS";
    for (limit, accepted) in [(1, false), (2, true), (3, true)] {
        let limits = SqlLimits::default().with_max_parameters(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, two_parameters).is_ok(),
            accepted,
            "parameter limit {limit}"
        );
    }

    let two_joins = "SELECT U.ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID LEFT JOIN USERS U2 ON U2.ID = O.USER_ID";
    for (limit, accepted) in [(1, false), (2, true), (3, true)] {
        let limits = SqlLimits::default().with_max_joins(limit).unwrap();
        assert_eq!(
            public_prepare_with_limits(&fixture, limits, two_joins).is_ok(),
            accepted,
            "JOIN limit {limit}"
        );
    }
}

#[test]
fn phase_4b_random_sql_never_panics() {
    // Production break caught: an unchecked tokenizer/parser/owned-AST branch
    // that panics for bounded arbitrary bytes or valid Unicode fails this model.
    let fixture = phase_4b_catalog_fixture(4096);
    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    let mut state = 0x6a09_e667_f3bc_c909_u64;

    for case in 0..10_000 {
        let length = (xorshift64(&mut state) as usize) % 65;
        let sql = if case % 2 == 0 {
            let bytes = (0..length)
                .map(|_| xorshift64(&mut state) as u8)
                .collect::<Vec<_>>();
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            let mut value = String::new();
            for _ in 0..length {
                let mut scalar = (xorshift64(&mut state) % 0x11_0000) as u32;
                if (0xd800..=0xdfff).contains(&scalar) {
                    scalar = 0xfffd;
                }
                value.push(char::from_u32(scalar).unwrap());
            }
            value
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| conn.prepare(&sql)));
        assert!(
            result.is_ok(),
            "prepare panicked for deterministic case {case}: {sql:?}"
        );
    }
}

fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
