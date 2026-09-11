use crate::{
    Result,
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
