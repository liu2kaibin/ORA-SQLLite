use crate::{
    Result,
    sql::{
        SqlLimits,
        ast::AstStatement,
        bind::{ParameterCollector, query::bind_query},
        bound::{BoundStatement, BoundStatementKind},
        parser::parse_statement,
    },
    storage::test_support::phase_4b_catalog_fixture,
};

/// Prepares one full SQL query against the Task 9 catalog fixture.
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
    let AstStatement::Query(query) = statement else {
        unreachable!("query test helper receives only query SQL")
    };
    let mut parameters = ParameterCollector::default();
    let bound = bind_query(&query, &catalog, database_uuid, &mut parameters)?;
    Ok(BoundStatement {
        kind: BoundStatementKind::Query {
            projection: bound.projection,
        },
        database_uuid,
        dependencies: bound.dependencies,
        created_names: Vec::new(),
        parameters: parameters.finish(),
        columns: bound.columns,
        span: query.span(),
    })
}
