use crate::{
    DbError, ErrorCode, Result,
    sql::{
        Identifier, SqlType,
        ast::{AstQuery, Join, JoinKind, TableSource},
    },
    storage::catalog::{
        CatalogReader,
        types::{CatalogName, ColumnDef, ColumnId, ColumnType, SchemaVersion, TableId},
    },
};

const MAX_AST_JOINS: u16 = u16::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedColumn {
    pub(crate) source_id: crate::sql::bound::SourceId,
    pub(crate) table_id: TableId,
    pub(crate) schema_version: SchemaVersion,
    pub(crate) column_id: ColumnId,
    pub(crate) ordinal: u16,
    pub(crate) sql_type: SqlType,
    pub(crate) nullable: bool,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundTable {
    visible_name: String,
    table_id: TableId,
    schema_version: SchemaVersion,
    columns: Vec<ResolvedColumn>,
    null_extended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SourceIdentity {
    pub(crate) table_id: TableId,
    pub(crate) schema_version: SchemaVersion,
}

pub(crate) trait ColumnResolver {
    fn resolve_column(&self, parts: &[Identifier]) -> Result<ResolvedColumn>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingScope {
    tables: Vec<BoundTable>,
}

pub(crate) struct JoinConditionScope<'scope, 'query> {
    tables: &'scope [BoundTable],
    preceding_joins: &'query [Join],
}

impl BindingScope {
    /// Builds the single pre-mutation row scope used by DML target expressions.
    pub(crate) fn for_target(
        catalog: &CatalogReader,
        table: &Identifier,
        alias: Option<&Identifier>,
    ) -> Result<Self> {
        Self::from_sources(
            catalog,
            &[TableSource {
                table: table.clone(),
                alias: alias.cloned(),
                span: table.span(),
            }],
            0,
        )
    }

    pub(crate) fn from_query_sources(catalog: &CatalogReader, query: &AstQuery) -> Result<Self> {
        Ok(Self::join_condition_scope(catalog, query)?.with_left_join_null_extension(query))
    }

    pub(crate) fn join_condition_scope(catalog: &CatalogReader, query: &AstQuery) -> Result<Self> {
        let mut sources = Vec::with_capacity(query.joins.len().saturating_add(1));
        sources.push(query.from.clone());
        sources.extend(query.joins.iter().map(|join| join.source.clone()));
        Self::from_sources(catalog, &sources, MAX_AST_JOINS)
    }

    pub(crate) fn scope_for_join_condition<'scope, 'query>(
        &'scope self,
        query: &'query AstQuery,
        index: usize,
    ) -> Result<JoinConditionScope<'scope, 'query>> {
        if index >= query.joins.len() {
            return Err(bind_error("JOIN condition index is outside the query"));
        }
        Ok(JoinConditionScope {
            tables: &self.tables[..index.saturating_add(2)],
            preceding_joins: &query.joins[..index],
        })
    }

    pub(crate) fn with_left_join_null_extension(mut self, query: &AstQuery) -> Self {
        for (table, join) in self.tables.iter_mut().skip(1).zip(&query.joins) {
            table.null_extended = join.kind == JoinKind::Left;
        }
        self
    }

    fn from_sources(
        catalog: &CatalogReader,
        sources: &[TableSource],
        max_joins: u16,
    ) -> Result<Self> {
        if sources.is_empty() {
            return Err(bind_error("query scope must contain a FROM source"));
        }
        if sources.len().saturating_sub(1) > usize::from(max_joins) {
            return Err(bind_error("query exceeds the maximum join count"));
        }

        let mut tables = Vec::with_capacity(sources.len());
        for (index, source) in sources.iter().enumerate() {
            let table_name = checked_catalog_name(&source.table)?;
            let table = catalog
                .find_table_by_name(&table_name)
                .ok_or_else(|| bind_error("table is not present in the catalog"))?;
            let visible = source.alias.as_ref().unwrap_or(&source.table);
            if tables
                .iter()
                .any(|bound: &BoundTable| bound.visible_name == visible.as_str())
            {
                return Err(bind_error("duplicate table alias or visible table name"));
            }

            let columns = catalog
                .columns_for_table(table.id())
                .map(|column| {
                    resolved_column(
                        crate::sql::bound::SourceId(index),
                        table.schema_version(),
                        column,
                    )
                })
                .collect();
            tables.push(BoundTable {
                visible_name: visible.as_str().to_owned(),
                table_id: table.id(),
                schema_version: table.schema_version(),
                columns,
                null_extended: false,
            });
        }
        Ok(Self { tables })
    }

    pub(crate) fn resolve_column(&self, parts: &[Identifier]) -> Result<ResolvedColumn> {
        resolve_column_in_tables(&self.tables, parts, |index| {
            self.tables[index].null_extended
        })
    }

    pub(crate) fn expand_star(
        &self,
        qualifier: Option<&Identifier>,
    ) -> Result<Vec<ResolvedColumn>> {
        Ok(self.star_columns(qualifier)?.collect())
    }

    fn star_tables(&self, qualifier: Option<&Identifier>) -> Result<&[BoundTable]> {
        match qualifier {
            Some(qualifier) => {
                let index = self
                    .tables
                    .iter()
                    .position(|table| table.visible_name == qualifier.as_str())
                    .ok_or_else(|| {
                        bind_error("star qualifier is not present in the query scope")
                    })?;
                Ok(&self.tables[index..index + 1])
            }
            None => Ok(&self.tables),
        }
    }

    pub(crate) fn star_count(&self, qualifier: Option<&Identifier>) -> Result<usize> {
        self.star_tables(qualifier)?
            .iter()
            .try_fold(0usize, |count, table| {
                count
                    .checked_add(table.columns.len())
                    .ok_or_else(|| bind_error("star column count overflow"))
            })
    }

    pub(crate) fn star_columns(
        &self,
        qualifier: Option<&Identifier>,
    ) -> Result<impl Iterator<Item = ResolvedColumn> + '_> {
        Ok(self.star_tables(qualifier)?.iter().flat_map(|table| {
            table
                .columns
                .iter()
                .map(|column| resolve_for_table(column, table.null_extended))
        }))
    }

    pub(crate) fn source_identities(&self) -> impl Iterator<Item = SourceIdentity> + '_ {
        self.tables.iter().map(|table| SourceIdentity {
            table_id: table.table_id,
            schema_version: table.schema_version,
        })
    }
}

impl ColumnResolver for BindingScope {
    fn resolve_column(&self, parts: &[Identifier]) -> Result<ResolvedColumn> {
        self.resolve_column(parts)
    }
}

impl ColumnResolver for JoinConditionScope<'_, '_> {
    fn resolve_column(&self, parts: &[Identifier]) -> Result<ResolvedColumn> {
        resolve_column_in_tables(self.tables, parts, |index| {
            index > 0
                && self
                    .preceding_joins
                    .get(index - 1)
                    .is_some_and(|join| join.kind == JoinKind::Left)
        })
    }
}

fn resolve_column_in_tables(
    tables: &[BoundTable],
    parts: &[Identifier],
    null_extended: impl Fn(usize) -> bool,
) -> Result<ResolvedColumn> {
    match parts {
        [column] => {
            let mut matches = tables.iter().enumerate().flat_map(|(index, table)| {
                table
                    .columns
                    .iter()
                    .filter(move |candidate| candidate.name == column.as_str())
                    .map(move |candidate| (index, candidate))
            });
            let (index, resolved) = matches
                .next()
                .ok_or_else(|| bind_error("column is not present in the query scope"))?;
            if matches.next().is_some() {
                return Err(bind_error("unqualified column name is ambiguous"));
            }
            Ok(resolve_for_table(resolved, null_extended(index)))
        }
        [qualifier, column] => {
            let (index, table) = tables
                .iter()
                .enumerate()
                .find(|(_, table)| table.visible_name == qualifier.as_str())
                .ok_or_else(|| bind_error("table qualifier is not present in the query scope"))?;
            table
                .columns
                .iter()
                .find(|candidate| candidate.name == column.as_str())
                .map(|column| resolve_for_table(column, null_extended(index)))
                .ok_or_else(|| bind_error("column is not present on the qualified table"))
        }
        [] => Err(bind_error("column name must contain one or two parts")),
        _ => Err(bind_error(
            "column name must not contain more than two parts",
        )),
    }
}

fn resolve_for_table(column: &ResolvedColumn, null_extended: bool) -> ResolvedColumn {
    let mut resolved = column.clone();
    if null_extended {
        resolved.nullable = true;
    }
    resolved
}

fn checked_catalog_name(identifier: &Identifier) -> Result<CatalogName> {
    if identifier.as_str().is_empty() || identifier.as_str().len() > 128 {
        return Err(bind_error(
            "SQL identifier must contain 1 through 128 UTF-8 bytes",
        ));
    }
    CatalogName::new(identifier.as_str().to_owned())
        .map_err(|_| bind_error("SQL identifier cannot be represented as a catalog name"))
}

fn resolved_column(
    source_id: crate::sql::bound::SourceId,
    schema_version: SchemaVersion,
    column: &ColumnDef,
) -> ResolvedColumn {
    ResolvedColumn {
        source_id,
        table_id: column.table_id(),
        schema_version,
        column_id: column.id(),
        ordinal: column.ordinal(),
        sql_type: logical_type(column.column_type()),
        nullable: column.nullable(),
        name: column.name().as_str().to_owned(),
    }
}

const fn logical_type(column_type: ColumnType) -> SqlType {
    match column_type {
        ColumnType::Int64 => SqlType::Int64,
        ColumnType::Decimal => SqlType::Decimal,
        ColumnType::Float64 => SqlType::Float64,
        ColumnType::Text => SqlType::Text,
        ColumnType::Blob => SqlType::Blob,
        ColumnType::Date => SqlType::Date,
        ColumnType::Timestamp | ColumnType::TimestampWithTimeZone => SqlType::Timestamp,
        ColumnType::Bool => SqlType::Bool,
    }
}

fn bind_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Bind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ErrorCode,
        sql::{
            Identifier, SqlSpan,
            ast::{AstExpr, AstExprKind, AstQuery, Join, JoinKind, TableSource},
        },
        storage::{catalog::CatalogReader, test_support::phase_4b_catalog_fixture},
    };

    const TEST_MAX_JOINS: u16 = 64;

    fn span() -> SqlSpan {
        SqlSpan::new(0, 1).unwrap()
    }

    fn source(name: &str, alias: Option<&str>) -> TableSource {
        TableSource {
            table: Identifier::unquoted(name, span()).unwrap(),
            alias: alias.map(|value| Identifier::unquoted(value, span()).unwrap()),
            span: span(),
        }
    }

    fn idents(parts: &[&str]) -> Vec<Identifier> {
        parts
            .iter()
            .map(|part| Identifier::unquoted(part, span()).unwrap())
            .collect()
    }

    fn real_catalog_with_users_and_orders() -> CatalogReader {
        let fixture = phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut read = pager.begin_read().unwrap();
        read.catalog_reader().unwrap()
    }

    fn fixture_scope() -> BindingScope {
        let reader = real_catalog_with_users_and_orders();
        BindingScope::from_sources(
            &reader,
            &[source("USERS", None), source("ORDERS", None)],
            TEST_MAX_JOINS,
        )
        .unwrap()
    }

    fn query(from: TableSource, joins: Vec<TableSource>) -> AstQuery {
        AstQuery {
            distinct: false,
            projection: Vec::new(),
            from,
            joins: joins
                .into_iter()
                .map(|source| Join {
                    kind: JoinKind::Inner,
                    source,
                    condition: AstExpr {
                        kind: AstExprKind::Bool(true),
                        span: span(),
                    },
                    span: span(),
                })
                .collect(),
            selection: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            span: span(),
        }
    }

    #[test]
    fn aliases_hide_table_names_and_unqualified_columns_must_be_unique() {
        // Production break caught: accepting a hidden base name or choosing one
        // of two same-named columns would make binding depend on source order.
        let reader = real_catalog_with_users_and_orders();
        let scope = BindingScope::from_sources(
            &reader,
            &[source("USERS", Some("U")), source("ORDERS", Some("O"))],
            TEST_MAX_JOINS,
        )
        .unwrap();
        assert!(scope.resolve_column(&idents(&["U", "ID"])).is_ok());
        assert_eq!(
            scope
                .resolve_column(&idents(&["USERS", "ID"]))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert_eq!(
            scope.resolve_column(&idents(&["ID"])).unwrap_err().code(),
            ErrorCode::Bind
        );
        let reversed = BindingScope::from_sources(
            &reader,
            &[source("ORDERS", Some("O")), source("USERS", Some("U"))],
            TEST_MAX_JOINS,
        )
        .unwrap();
        assert_eq!(
            reversed
                .resolve_column(&idents(&["ID"]))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn star_expansion_is_from_order_then_catalog_ordinal() {
        // Production break caught: map iteration or name sorting would scramble
        // the user-visible SELECT * column order.
        let columns = fixture_scope().expand_star(None).unwrap();
        let names = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["ID", "NAME", "AMOUNT", "FLOAT_COL", "ID", "USER_ID"]
        );
        assert_eq!(
            columns
                .iter()
                .map(|column| column.sql_type)
                .collect::<Vec<_>>(),
            [
                SqlType::Int64,
                SqlType::Text,
                SqlType::Decimal,
                SqlType::Float64,
                SqlType::Int64,
                SqlType::Int64,
            ]
        );
        assert_eq!(
            columns
                .iter()
                .map(|column| column.nullable)
                .collect::<Vec<_>>(),
            [false, true, true, true, false, false]
        );
    }

    #[test]
    fn qualified_and_unqualified_columns_retain_real_catalog_identity() {
        // Production break caught: reconstructing IDs or losing schema/type/null
        // metadata would make schema-change checks and expression typing unsound.
        let reader = real_catalog_with_users_and_orders();
        let users = reader
            .find_table_by_name(&crate::storage::catalog::types::CatalogName::new("USERS").unwrap())
            .unwrap();
        let expected = reader.columns_for_table(users.id()).nth(1).unwrap().clone();
        let scope = BindingScope::from_sources(
            &reader,
            &[source("USERS", Some("U")), source("ORDERS", Some("O"))],
            TEST_MAX_JOINS,
        )
        .unwrap();

        let qualified = scope.resolve_column(&idents(&["U", "NAME"])).unwrap();
        let unqualified = scope.resolve_column(&idents(&["NAME"])).unwrap();
        assert_eq!(qualified, unqualified);
        assert_eq!(qualified.table_id, users.id());
        assert_eq!(qualified.schema_version, users.schema_version());
        assert_eq!(qualified.column_id, expected.id());
        assert_eq!(qualified.ordinal, 1);
        assert_eq!(qualified.sql_type, crate::sql::SqlType::Text);
        assert!(qualified.nullable);
        assert_eq!(qualified.name, "NAME");
    }

    #[test]
    fn unknown_duplicate_and_overqualified_names_are_bind_errors() {
        // Production break caught: silently accepting absent or structurally
        // invalid names delays a deterministic prepare-time failure.
        let reader = real_catalog_with_users_and_orders();
        assert_eq!(
            BindingScope::from_sources(&reader, &[source("MISSING", None)], TEST_MAX_JOINS,)
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert_eq!(
            BindingScope::from_sources(
                &reader,
                &[source("USERS", Some("X")), source("ORDERS", Some("X"))],
                TEST_MAX_JOINS,
            )
            .unwrap_err()
            .code(),
            ErrorCode::Bind
        );
        let scope = fixture_scope();
        for invalid in [
            idents(&["MISSING"]),
            idents(&["USERS", "MISSING"]),
            idents(&["A", "B", "C"]),
        ] {
            assert_eq!(
                scope.resolve_column(&invalid).unwrap_err().code(),
                ErrorCode::Bind
            );
        }
    }

    #[test]
    fn qualified_star_obeys_alias_visibility_and_query_source_order() {
        // Production break caught: bypassing the alias namespace or ignoring JOIN
        // order would expose the wrong source columns.
        let reader = real_catalog_with_users_and_orders();
        let query = query(
            source("ORDERS", Some("O")),
            vec![source("USERS", Some("U"))],
        );
        let scope = BindingScope::from_query_sources(&reader, &query).unwrap();
        let names = scope
            .expand_star(Some(&idents(&["U"])[0]))
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["ID", "NAME", "AMOUNT", "FLOAT_COL"]);
        assert_eq!(
            scope
                .expand_star(Some(&idents(&["USERS"])[0]))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert_eq!(
            scope
                .expand_star(Some(&idents(&["MISSING"])[0]))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        let all_names = scope
            .expand_star(None)
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        assert_eq!(
            all_names,
            ["ID", "USER_ID", "ID", "NAME", "AMOUNT", "FLOAT_COL"]
        );
    }

    #[test]
    fn join_condition_scope_borrows_its_visible_prefix() {
        // Production break caught: copying the complete source scope for each
        // JOIN ON copies later, not-yet-visible table columns. A condition view
        // must borrow only its visible prefix while retaining prior LEFT JOIN
        // null extension semantics.
        let reader = real_catalog_with_users_and_orders();
        let mut query = query(
            source("USERS", Some("U")),
            vec![
                source("ORDERS", Some("O")),
                source("USERS", Some("M")),
                source("ORDERS", Some("L")),
            ],
        );
        query.joins[0].kind = JoinKind::Left;
        let scope = BindingScope::join_condition_scope(&reader, &query).unwrap();
        let condition_scope = scope.scope_for_join_condition(&query, 1).unwrap();

        assert_eq!(condition_scope.tables.len(), 3);
        assert_eq!(condition_scope.tables.as_ptr(), scope.tables.as_ptr());
        assert!(
            condition_scope
                .resolve_column(&idents(&["O", "ID"]))
                .unwrap()
                .nullable
        );
        assert!(
            !condition_scope
                .resolve_column(&idents(&["M", "ID"]))
                .unwrap()
                .nullable
        );
        assert_eq!(
            condition_scope
                .resolve_column(&idents(&["L", "ID"]))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }
}
