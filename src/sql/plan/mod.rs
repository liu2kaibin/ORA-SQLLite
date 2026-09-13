//! Immutable, storage-independent compilation of the project's bound SQL IR.
mod expr;
mod mutation;
mod query;

use std::fmt;

use super::{
    SqlSpan,
    bound::{BoundStatement, BoundStatementKind, SchemaDependency},
};
use crate::{
    DbError, ErrorCode, Result,
    api::{ColumnMetadata, ParameterMetadata},
    error::SqlLocation,
    storage::catalog::types::TableId,
};
pub(crate) use expr::{AggregateDefinition, ExecExpr, ExecExprKind};
pub(crate) use mutation::{CreateTablePlan, DeletePlan, InsertPlan, UpdatePlan};
pub(crate) use query::{OrderExpression, QueryPlan};

pub(crate) struct Plan {
    kind: PlanKind,
    parameters: Box<[ParameterMetadata]>,
    columns: Box<[ColumnMetadata]>,
    dependencies: Box<[SchemaDependency]>,
}

impl fmt::Debug for Plan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Plan").finish_non_exhaustive()
    }
}

pub(crate) enum PlanKind {
    Query(QueryPlan),
    Insert(InsertPlan),
    Update(UpdatePlan),
    Delete(DeletePlan),
    CreateTable(CreateTablePlan),
    DropTable { table_id: TableId },
    Begin,
    Commit,
    Rollback,
    DeferredUnsupported { required_phase: &'static str },
}

impl PlanKind {
    pub(crate) const fn is_write_or_query(&self) -> bool {
        matches!(
            self,
            Self::Query(_)
                | Self::Insert(_)
                | Self::Update(_)
                | Self::Delete(_)
                | Self::CreateTable(_)
                | Self::DropTable { .. }
        )
    }
}

impl Plan {
    /// SQL is borrowed solely to resolve project spans; it is never parsed or retained.
    pub(crate) fn from_bound(bound: &BoundStatement, sql: &str) -> Result<Self> {
        let locations = Locations::new(sql);
        locations.resolve(bound.span)?;
        let mut expressions = expr::ExprCompiler::new(bound.parameters(), &locations)?;
        let kind = match &bound.kind {
            BoundStatementKind::Query { query } => {
                PlanKind::Query(QueryPlan::from_bound(query, &mut expressions)?)
            }
            BoundStatementKind::Insert {
                table,
                targets,
                values,
            } => PlanKind::Insert(InsertPlan::from_bound(
                table,
                targets,
                values,
                &mut expressions,
            )?),
            BoundStatementKind::Update {
                table,
                assignments,
                selection,
            } => PlanKind::Update(UpdatePlan::from_bound(
                table,
                assignments,
                selection.as_ref(),
                &mut expressions,
            )?),
            BoundStatementKind::Delete { table, selection } => PlanKind::Delete(
                DeletePlan::from_bound(table, selection.as_ref(), &mut expressions)?,
            ),
            BoundStatementKind::CreateTable {
                name,
                columns,
                constraints,
            } => {
                if constraints.is_empty() && columns.iter().all(|column| column.nullable) {
                    PlanKind::CreateTable(CreateTablePlan::from_bound(name, columns, &locations)?)
                } else {
                    PlanKind::DeferredUnsupported {
                        required_phase: "4C2",
                    }
                }
            }
            BoundStatementKind::DropTable { table_id } => PlanKind::DropTable {
                table_id: *table_id,
            },
            BoundStatementKind::CreateIndex { .. } => PlanKind::DeferredUnsupported {
                required_phase: "4C2",
            },
            BoundStatementKind::DropIndex { .. } => PlanKind::DeferredUnsupported {
                required_phase: "4C2",
            },
            BoundStatementKind::Begin => PlanKind::Begin,
            BoundStatementKind::Commit => PlanKind::Commit,
            BoundStatementKind::Rollback => PlanKind::Rollback,
        };
        Ok(Self {
            kind,
            parameters: bound.parameters().into(),
            columns: bound.columns().into(),
            dependencies: bound.dependencies().into(),
        })
    }

    pub(crate) fn kind(&self) -> &PlanKind {
        &self.kind
    }
    pub(crate) fn parameters(&self) -> &[ParameterMetadata] {
        &self.parameters
    }
    pub(crate) fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }
    pub(crate) fn dependencies(&self) -> &[SchemaDependency] {
        &self.dependencies
    }
}

/// Transient project-only span converter. Index once, then resolve in O(log n).
/// Columns count Unicode scalars; LF starts a line, including the LF in CRLF.
struct Locations<'sql> {
    sql: &'sql str,
    boundaries: Vec<usize>,
    line_starts: Vec<usize>,
}

impl<'sql> Locations<'sql> {
    fn new(sql: &'sql str) -> Self {
        let mut boundaries = vec![0];
        let mut line_starts = vec![0];
        for (offset, character) in sql.char_indices() {
            if character == '\n' {
                line_starts.push(boundaries.len());
            }
            boundaries.push(offset + character.len_utf8());
        }
        Self {
            sql,
            boundaries,
            line_starts,
        }
    }

    fn resolve(&self, span: SqlSpan) -> Result<SqlLocation> {
        if span.end() > self.sql.len() || !self.sql.is_char_boundary(span.end()) {
            return Err(invalid_plan("SQL span is outside the original text"));
        }
        let scalar = self
            .boundaries
            .binary_search(&span.start())
            .map_err(|_| invalid_plan("SQL span is outside the original text"))?;
        let line = self.line_starts.partition_point(|start| *start <= scalar) - 1;
        SqlLocation::new(span.start(), line + 1, scalar - self.line_starts[line] + 1)
    }
}

fn invalid_plan(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use std::ops::Deref;

    use super::{ExecExprKind, Plan, PlanKind};
    use crate::{
        ErrorCode, ParameterIdentifier, Result, SqlLimits, Value,
        error::SqlLocation,
        sql::{
            SqlSpan, SqlType, bind_sql,
            bound::{BoundExpr, BoundExprKind, BoundStatement, BoundStatementKind, SourceId},
        },
        storage::{catalog::types::IndexId, test_support::phase_4b_catalog_fixture},
    };

    struct Fixture {
        bound: BoundStatement,
        source: &'static str,
    }

    impl Deref for Fixture {
        type Target = BoundStatement;
        fn deref(&self) -> &Self::Target {
            &self.bound
        }
    }

    impl Fixture {
        fn kind(&self) -> &BoundStatementKind {
            &self.bound.kind
        }
        fn source(&self) -> &str {
            self.source
        }
    }

    fn bind_fixture(sql: &str) -> BoundStatement {
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        bind_sql(
            sql,
            &SqlLimits::default(),
            &catalog,
            pager.database_uuid().unwrap(),
        )
        .unwrap()
    }

    fn fixture(source: &'static str) -> Fixture {
        Fixture {
            bound: bind_fixture(source),
            source,
        }
    }

    fn every_bound_statement_fixture() -> Vec<Fixture> {
        let mut cases = vec![
            fixture("SELECT ID FROM USERS"),
            fixture("INSERT INTO USERS (NAME, ID) VALUES ('x', :1)"),
            fixture("UPDATE USERS SET NAME = 'x', ID = :1 WHERE ID = 2"),
            fixture("DELETE FROM USERS WHERE ID = :1"),
            fixture("CREATE TABLE NEW_TABLE (ID INT64, NAME TEXT)"),
            fixture("DROP TABLE USERS"),
            bound_create_index(),
            fixture("BEGIN"),
            fixture("COMMIT"),
            fixture("ROLLBACK"),
            bound_constrained_create_table(),
        ];
        let mut drop_index = fixture("BEGIN");
        drop_index.bound.kind = BoundStatementKind::DropIndex {
            index_id: IndexId::new(123).unwrap(),
        };
        cases.push(drop_index);
        cases
    }

    fn fixture_source_for(bound: &Fixture) -> &str {
        bound.source()
    }
    fn bound_create_index() -> Fixture {
        fixture("CREATE INDEX IX_USERS ON USERS (ID)")
    }
    fn bound_constrained_create_table() -> Fixture {
        fixture("CREATE TABLE NEW_TABLE (ID INT64 CHECK (ID > 0))")
    }
    fn plan_sql(sql: &str) -> Result<Plan> {
        Plan::from_bound(&bind_fixture(sql), sql)
    }

    impl PlanKind {
        fn matches_bound_kind(&self, bound: &BoundStatementKind) -> bool {
            match bound {
                BoundStatementKind::Query { .. } => matches!(self, Self::Query(_)),
                BoundStatementKind::Insert { .. } => matches!(self, Self::Insert(_)),
                BoundStatementKind::Update { .. } => matches!(self, Self::Update(_)),
                BoundStatementKind::Delete { .. } => matches!(self, Self::Delete(_)),
                BoundStatementKind::CreateTable { constraints, .. } if !constraints.is_empty() => {
                    matches!(
                        self,
                        Self::DeferredUnsupported {
                            required_phase: "4C2"
                        }
                    )
                }
                BoundStatementKind::CreateTable { .. } => matches!(self, Self::CreateTable(_)),
                BoundStatementKind::DropTable { table_id } => {
                    matches!(self, Self::DropTable { table_id: actual } if actual == table_id)
                }
                BoundStatementKind::CreateIndex { .. } | BoundStatementKind::DropIndex { .. } => {
                    matches!(
                        self,
                        Self::DeferredUnsupported {
                            required_phase: "4C2"
                        }
                    )
                }
                BoundStatementKind::Begin => matches!(self, Self::Begin),
                BoundStatementKind::Commit => matches!(self, Self::Commit),
                BoundStatementKind::Rollback => matches!(self, Self::Rollback),
            }
        }
    }

    impl Plan {
        fn first_divide_location(&self) -> SqlLocation {
            let PlanKind::Query(query) = self.kind() else {
                panic!("query")
            };
            assert!(matches!(
                query.projection()[0].kind(),
                ExecExprKind::Binary {
                    op: crate::sql::bound::BoundBinaryOp::Divide,
                    ..
                }
            ));
            query.projection()[0].location()
        }
    }

    // A missing/misrouted bound variant or a planner reparse breaks this contract.
    #[test]
    fn planner_covers_every_bound_statement_without_reparsing() {
        for bound in every_bound_statement_fixture() {
            let plan = Plan::from_bound(&bound, fixture_source_for(&bound)).unwrap();
            assert!(plan.kind().matches_bound_kind(bound.kind()));
        }
    }

    #[test]
    fn index_and_constrained_table_plans_are_deferred() {
        for bound in [bound_create_index(), bound_constrained_create_table()] {
            assert!(matches!(
                Plan::from_bound(&bound, bound.source()).unwrap().kind(),
                PlanKind::DeferredUnsupported {
                    required_phase: "4C2"
                }
            ));
        }
    }

    #[test]
    fn executable_expressions_retain_project_sql_locations() {
        let plan = plan_sql("SELECT 1 / :1 FROM USERS").unwrap();
        assert_eq!(
            plan.first_divide_location(),
            SqlLocation::new(7, 1, 8).unwrap()
        );
    }

    #[test]
    fn planning_uses_source_only_for_locations_and_owns_its_result() {
        // Blank source of the same length cannot be parsed, but maps every span.
        let sql = "SELECT :2, :1, :2 FROM USERS";
        let bound = bind_fixture(sql);
        let source = " ".repeat(sql.len());
        let plan = Plan::from_bound(&bound, &source).unwrap();
        assert_eq!(plan.parameters(), bound.parameters());
        assert_eq!(plan.columns(), bound.columns());
        assert_eq!(plan.dependencies(), bound.dependencies());
        drop(source);
        drop(bound);
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        for (expr, slot) in query.projection().iter().zip([1, 0, 1]) {
            assert!(
                matches!(expr.kind(), ExecExprKind::Parameter { slot: actual } if *actual == slot)
            );
        }
        fn assert_owned<T: Send + Sync + 'static>() {}
        assert_owned::<Plan>();
    }

    #[test]
    fn named_parameter_slots_follow_metadata_not_planner_traversal() {
        let plan = plan_sql("SELECT :z FROM USERS WHERE ID = :a ORDER BY :z + :b").unwrap();
        assert_eq!(
            plan.parameters()
                .iter()
                .map(|p| p.identifier().clone())
                .collect::<Vec<_>>(),
            ["Z", "A", "B"].map(|n| ParameterIdentifier::Name(n.to_owned()))
        );
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        assert!(matches!(
            query.projection()[0].kind(),
            ExecExprKind::Parameter { slot: 0 }
        ));
        let ExecExprKind::Binary { right, .. } = query.selection().unwrap().kind() else {
            panic!("selection")
        };
        assert!(matches!(right.kind(), ExecExprKind::Parameter { slot: 1 }));
        let super::OrderExpression::Expression(order) = query.order_by()[0].expression() else {
            panic!("order")
        };
        let ExecExprKind::Binary { left, right, .. } = order.kind() else {
            panic!("order binary")
        };
        assert!(matches!(left.kind(), ExecExprKind::Parameter { slot: 0 }));
        assert!(matches!(right.kind(), ExecExprKind::Parameter { slot: 2 }));
    }

    #[test]
    fn query_retains_pipeline_order_and_deduplicates_aggregate_identity() {
        let sql = "SELECT DISTINCT U.ID, SUM(O.ID) AS TOTAL, SUM(O.ID), SUM(U.ID), COUNT(*), COUNT(O.ID) FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID WHERE U.ID > :1 GROUP BY U.ID HAVING SUM(O.ID) > :2 ORDER BY TOTAL DESC NULLS FIRST, SUM(U.ID) ASC NULLS LAST";
        let bound = bind_fixture(sql);
        let plan = Plan::from_bound(&bound, sql).unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        assert!(query.distinct());
        assert_eq!(
            query
                .sources()
                .iter()
                .map(|s| s.source_id)
                .collect::<Vec<_>>(),
            [SourceId(0), SourceId(1)]
        );
        assert_eq!(query.joins()[0].kind(), crate::sql::ast::JoinKind::Left);
        assert_eq!(query.joins()[0].source_id(), SourceId(1));
        assert!(query.selection().is_some());
        assert_eq!(query.group_by().len(), 1);
        assert_eq!(query.aggregates().len(), 4);
        for (expr, slot) in query.projection()[1..].iter().zip([0, 0, 1, 2, 3]) {
            assert!(
                matches!(expr.kind(), ExecExprKind::Aggregate { slot: actual } if *actual == slot)
            );
        }
        let ExecExprKind::Binary { left, .. } = query.having().unwrap().kind() else {
            panic!("having")
        };
        assert!(matches!(left.kind(), ExecExprKind::Aggregate { slot: 0 }));
        assert!(matches!(
            query.order_by()[0].expression(),
            super::OrderExpression::Projection(1)
        ));
        assert_eq!(
            query.order_by()[0].direction(),
            crate::sql::ast::OrderDirection::Descending
        );
        assert_eq!(
            query.order_by()[0].nulls(),
            crate::sql::ast::NullPlacement::First
        );
        assert_eq!(
            query.order_by()[1].direction(),
            crate::sql::ast::OrderDirection::Ascending
        );
        assert_eq!(
            query.order_by()[1].nulls(),
            crate::sql::ast::NullPlacement::Last
        );
        assert_eq!(query.columns(), bound.columns());
        assert_eq!(query.dependencies(), bound.dependencies());
        assert_eq!(
            query.aggregates()[0].function(),
            crate::sql::bound::BoundAggregateFunction::Sum
        );
        assert!(matches!(
            query.aggregates()[0].argument().unwrap().kind(),
            ExecExprKind::Column {
                source: SourceId(1),
                ordinal: 0
            }
        ));
        assert!(query.aggregates()[2].argument().is_none());
        assert!(query.aggregates()[3].argument().is_some());
    }

    #[test]
    fn aggregate_slots_distinguish_self_joins_functions_and_literal_arguments() {
        // Merging by catalog column instead of source occurrence, ignoring a
        // literal/function, or skipping HAVING/ORDER-only aggregates breaks this.
        let plan = plan_sql("SELECT SUM(A.ID), SUM(B.ID), SUM(A.ID + 1), SUM(A.ID + 1), SUM(A.ID + 2), AVG(A.ID), MIN(A.ID), MAX(A.ID) FROM USERS A INNER JOIN USERS B ON A.ID = B.ID HAVING COUNT(*) > 0 ORDER BY COUNT(A.ID)").unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        assert_eq!(query.aggregates().len(), 9);
        for (expression, slot) in query.projection().iter().zip([0, 1, 2, 2, 3, 4, 5, 6]) {
            assert!(
                matches!(expression.kind(), ExecExprKind::Aggregate { slot: actual } if *actual == slot)
            );
        }
        assert_ne!(
            query.projection()[2].location(),
            query.projection()[3].location()
        );
        assert_eq!(
            query.aggregates()[2].location(),
            query.projection()[2].location()
        );
        let ExecExprKind::Binary { left, .. } = query.having().unwrap().kind() else {
            panic!("having")
        };
        assert!(matches!(left.kind(), ExecExprKind::Aggregate { slot: 7 }));
        let super::OrderExpression::Expression(order) = query.order_by()[0].expression() else {
            panic!("order")
        };
        assert!(matches!(order.kind(), ExecExprKind::Aggregate { slot: 8 }));
        assert_eq!(
            query.aggregates()[5].function(),
            crate::sql::bound::BoundAggregateFunction::Min
        );
        assert_eq!(
            query.aggregates()[6].function(),
            crate::sql::bound::BoundAggregateFunction::Max
        );
    }

    #[test]
    fn expression_families_keep_operands_flags_types_and_nullability() {
        let plan = plan_sql("SELECT -ID, ID + 2, ID IS NOT NULL, ID NOT IN (1, 2), ID NOT BETWEEN 3 AND 4, NAME NOT LIKE 'x%', TRUE FROM USERS").unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        let p = query.projection();
        assert!(matches!(
            p[0].kind(),
            ExecExprKind::Unary {
                op: crate::sql::bound::BoundUnaryOp::Negative,
                ..
            }
        ));
        assert!(matches!(
            p[1].kind(),
            ExecExprKind::Binary {
                op: crate::sql::bound::BoundBinaryOp::Add,
                ..
            }
        ));
        assert!(matches!(
            p[2].kind(),
            ExecExprKind::IsNull { negated: true, .. }
        ));
        let ExecExprKind::InList {
            values,
            negated: true,
            ..
        } = p[3].kind()
        else {
            panic!("in")
        };
        assert!(matches!(
            values[0].kind(),
            ExecExprKind::Literal(Value::Int64(1))
        ));
        assert!(matches!(
            values[1].kind(),
            ExecExprKind::Literal(Value::Int64(2))
        ));
        let ExecExprKind::Between {
            low,
            high,
            negated: true,
            ..
        } = p[4].kind()
        else {
            panic!("between")
        };
        assert!(matches!(low.kind(), ExecExprKind::Literal(Value::Int64(3))));
        assert!(matches!(
            high.kind(),
            ExecExprKind::Literal(Value::Int64(4))
        ));
        let ExecExprKind::Like {
            pattern,
            negated: true,
            ..
        } = p[5].kind()
        else {
            panic!("like")
        };
        assert!(matches!(pattern.kind(), ExecExprKind::Literal(Value::Text(s)) if s == "x%"));
        assert!(matches!(
            p[6].kind(),
            ExecExprKind::Literal(Value::Bool(true))
        ));
        assert_eq!(p[0].data_type(), SqlType::Int64);
        assert!(!p[0].nullable());
        assert_eq!(p[5].data_type(), SqlType::Bool);
        assert!(p[5].nullable());
    }

    #[test]
    fn location_conversion_checks_utf8_boundaries_and_multiline_source() {
        let sql = "SELECT '界',\r\n 1 / :1 FROM USERS";
        let mut bound = bind_fixture(sql);
        let plan = Plan::from_bound(&bound, sql).unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        assert_eq!(
            query.projection()[1].location(),
            SqlLocation::new(16, 2, 2).unwrap()
        );
        let BoundStatementKind::Query { query } = &mut bound.kind else {
            panic!("query")
        };
        query.projection[0].span = SqlSpan::new(9, 10).unwrap();
        assert_eq!(
            Plan::from_bound(&bound, sql).unwrap_err().code(),
            ErrorCode::Internal
        );
        assert_eq!(
            Plan::from_bound(&bound, "").unwrap_err().code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn planner_rejects_new_table_columns_outside_deferred_constraints() {
        let mut bound = bind_fixture("SELECT ID FROM USERS");
        let BoundStatementKind::Query { query } = &mut bound.kind else {
            panic!("query")
        };
        query.projection[0].kind = BoundExprKind::NewTableColumn { ordinal: 0 };
        assert_eq!(
            Plan::from_bound(&bound, "SELECT ID FROM USERS")
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn mutation_plans_preserve_target_order_values_and_table_definitions() {
        let plan = plan_sql("INSERT INTO USERS (NAME, ID) VALUES ('x', :1)").unwrap();
        let PlanKind::Insert(insert) = plan.kind() else {
            panic!("insert")
        };
        assert_eq!(
            insert
                .targets()
                .iter()
                .map(|c| c.ordinal())
                .collect::<Vec<_>>(),
            [1, 0]
        );
        assert!(
            matches!(insert.values()[0].kind(), ExecExprKind::Literal(Value::Text(v)) if v == "x")
        );
        assert!(matches!(
            insert.values()[1].kind(),
            ExecExprKind::Parameter { slot: 0 }
        ));
        assert_eq!(insert.targets()[0].table_id(), insert.table().table_id());
        assert_eq!(insert.targets()[1].data_type(), SqlType::Int64);
        let plan = plan_sql("UPDATE USERS SET NAME = 'y', ID = ID + :1 WHERE ID > 2").unwrap();
        let PlanKind::Update(update) = plan.kind() else {
            panic!("update")
        };
        assert_eq!(
            update
                .assignments()
                .iter()
                .map(|a| a.target().ordinal())
                .collect::<Vec<_>>(),
            [1, 0]
        );
        let ExecExprKind::Binary { left, right, .. } = update.assignments()[1].value().kind()
        else {
            panic!("value")
        };
        assert!(matches!(
            left.kind(),
            ExecExprKind::Column {
                source: SourceId(0),
                ordinal: 0
            }
        ));
        assert!(matches!(right.kind(), ExecExprKind::Parameter { slot: 0 }));
        assert!(update.selection().is_some());
        let plan = plan_sql("DELETE FROM USERS WHERE ID = :1").unwrap();
        let PlanKind::Delete(delete) = plan.kind() else {
            panic!("delete")
        };
        assert!(delete.selection().is_some());
        let plan = plan_sql("CREATE TABLE NEW_TABLE (LABEL TEXT, ID INT64)").unwrap();
        let PlanKind::CreateTable(create) = plan.kind() else {
            panic!("create")
        };
        assert_eq!(create.name().as_str(), "NEW_TABLE");
        assert_eq!(
            create
                .columns()
                .iter()
                .map(|c| c.ordinal())
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(create.columns()[0].name().as_str(), "LABEL");
        assert_eq!(
            create.columns()[1].column_type(),
            crate::storage::catalog::types::ColumnType::Int64
        );
        assert!(create.columns().iter().all(|c| c.nullable()));
        for case in every_bound_statement_fixture() {
            let plan = Plan::from_bound(&case, case.source()).unwrap();
            assert_eq!(
                plan.kind().is_write_or_query(),
                matches!(
                    case.kind(),
                    BoundStatementKind::Query { .. }
                        | BoundStatementKind::Insert { .. }
                        | BoundStatementKind::Update { .. }
                        | BoundStatementKind::Delete { .. }
                        | BoundStatementKind::DropTable { .. }
                ) || matches!(case.kind(), BoundStatementKind::CreateTable { constraints, .. } if constraints.is_empty())
            );
        }
    }

    #[test]
    fn every_constraint_family_and_index_variant_is_deferred() {
        for sql in [
            "CREATE TABLE N (ID INT64 NOT NULL)",
            "CREATE TABLE N (ID INT64 PRIMARY KEY)",
            "CREATE TABLE N (ID INT64 UNIQUE)",
            "CREATE TABLE N (ID INT64 CHECK (ID > 0))",
            "CREATE TABLE N (ID INT64 REFERENCES USERS(ID))",
            "CREATE UNIQUE INDEX IX ON USERS(ID)",
        ] {
            assert!(matches!(
                plan_sql(sql).unwrap().kind(),
                PlanKind::DeferredUnsupported {
                    required_phase: "4C2"
                }
            ));
        }
    }

    #[test]
    fn missing_parameter_metadata_is_an_internal_error_not_a_new_slot() {
        let mut bound = bind_fixture("SELECT :1 FROM USERS");
        bound.parameters.clear();
        assert_eq!(
            Plan::from_bound(&bound, "SELECT :1 FROM USERS")
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn debug_never_discloses_logical_ids_sql_or_values() {
        let sql = "SELECT 987654321 FROM USERS";
        let plan = plan_sql(sql).unwrap();
        assert_eq!(format!("{plan:?}"), "Plan { .. }");
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        assert_eq!(format!("{:?}", query.projection()[0]), "ExecExpr { .. }");
    }

    #[test]
    fn deep_owned_expression_compiles_and_drops_without_recursion() {
        // Reintroducing a recursive converter/destructor would overflow the stack.
        let mut bound = bind_fixture("SELECT ID FROM USERS");
        let mut expression = BoundExpr::new(
            BoundExprKind::Literal(Value::Int64(1)),
            SqlType::Int64,
            false,
            SqlSpan::new(0, 1).unwrap(),
        );
        for _ in 0..20_000 {
            expression = BoundExpr::new(
                BoundExprKind::Unary {
                    op: crate::sql::bound::BoundUnaryOp::Negative,
                    expr: Box::new(expression),
                },
                SqlType::Int64,
                false,
                SqlSpan::new(0, 1).unwrap(),
            );
        }
        let BoundStatementKind::Query { query } = &mut bound.kind else {
            panic!("query")
        };
        query.projection = vec![expression];
        let plan = Plan::from_bound(&bound, "SELECT ID FROM USERS").unwrap();
        let PlanKind::Query(query) = plan.kind() else {
            panic!("query")
        };
        let mut expression = &query.projection()[0];
        let mut count = 0;
        while let ExecExprKind::Unary { expr, .. } = expression.kind() {
            count += 1;
            expression = expr;
        }
        assert_eq!(count, 20_000);
        drop(plan);
    }
}
