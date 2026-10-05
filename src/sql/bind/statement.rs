use std::collections::HashSet;

use crate::{
    DbError, ErrorCode, Result,
    api::ColumnMetadata,
    error::SqlLocation,
    oracle_type::OracleTypeDescriptor,
    sql::{
        Identifier, SqlSpan, SqlType,
        ast::{
            AstStatement, Constraint, CreateIndex, CreateTable, Delete, ForeignKey, Insert,
            TypeDeclaration, TypeModifier, Update,
        },
        bound::{
            BoundAssignment, BoundColumn, BoundColumnDefinition, BoundConstraint, BoundExpr,
            BoundExprKind, BoundStatement, BoundStatementKind, BoundTable, BoundTypeDeclaration,
            CreatedNameDependency, SchemaDependency,
        },
    },
    storage::catalog::{
        CatalogReader,
        types::{
            CatalogName, ColumnId, ColumnType, ConstraintId, MAX_COLUMNS, ObjectKind,
            SchemaVersion, TableId,
        },
    },
};

use super::{
    BindingScope, ColumnResolver, ParameterCollector, ResolvedColumn,
    bind_assignment_expr_for_column, bind_expr, bind_query,
};

/// Binds one parsed statement to an immutable catalog image.
pub(crate) fn bind_statement(
    statement: &AstStatement,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
) -> Result<BoundStatement> {
    bind_statement_with_limits(
        statement,
        catalog,
        database_uuid,
        &crate::SqlLimits::default(),
    )
}

pub(crate) fn bind_statement_with_limits(
    statement: &AstStatement,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    limits: &crate::SqlLimits,
) -> Result<BoundStatement> {
    let mut parameters = ParameterCollector::with_node_limit(limits.max_ast_nodes() as usize);
    let mut parts = bind_parts(statement, catalog, database_uuid, &mut parameters)?;
    if parameters.solve()? {
        drop(parts);
        parts = bind_parts(statement, catalog, database_uuid, &mut parameters)?;
    }
    require_persistence_safe_checks(&parts.kind)?;
    Ok(BoundStatement {
        kind: parts.kind,
        database_uuid,
        dependencies: parts.dependencies,
        created_names: parts.created_names,
        parameters: parameters.finish(),
        columns: parts.columns,
        span: statement.span(),
    })
}

fn bind_parts(
    statement: &AstStatement,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    parameters: &mut ParameterCollector,
) -> Result<BoundParts> {
    let parts = match statement {
        AstStatement::Query(query) => {
            let bound = bind_query(query, catalog, database_uuid, parameters)?;
            BoundParts {
                dependencies: bound.dependencies.clone(),
                created_names: Vec::new(),
                columns: bound.columns.clone(),
                kind: BoundStatementKind::Query { query: bound },
            }
        }
        AstStatement::Insert(insert) => bind_insert(insert, catalog, database_uuid, parameters)?,
        AstStatement::Update(update) => bind_update(update, catalog, database_uuid, parameters)?,
        AstStatement::Delete(delete) => bind_delete(delete, catalog, database_uuid, parameters)?,
        AstStatement::CreateTable(create) => {
            bind_create_table(create, catalog, database_uuid, parameters)?
        }
        AstStatement::DropTable(drop) => bind_drop_table(&drop.name, catalog, database_uuid)?,
        AstStatement::CreateIndex(create) => bind_create_index(create, catalog, database_uuid)?,
        AstStatement::DropIndex(drop) => bind_drop_index(&drop.name, catalog, database_uuid)?,
        AstStatement::Begin(_) => BoundParts::empty(BoundStatementKind::Begin),
        AstStatement::Commit(_) => BoundParts::empty(BoundStatementKind::Commit),
        AstStatement::Rollback(_) => BoundParts::empty(BoundStatementKind::Rollback),
    };

    Ok(parts)
}

struct BoundParts {
    kind: BoundStatementKind,
    dependencies: Vec<SchemaDependency>,
    created_names: Vec<CreatedNameDependency>,
    columns: Vec<ColumnMetadata>,
}

impl BoundParts {
    fn empty(kind: BoundStatementKind) -> Self {
        Self {
            kind,
            dependencies: Vec::new(),
            created_names: Vec::new(),
            columns: Vec::new(),
        }
    }
}

/// INSERT VALUES has no old target row from which a column may be read.
struct NoColumnScope;

impl ColumnResolver for NoColumnScope {
    fn resolve_column(&self, _parts: &[Identifier]) -> Result<ResolvedColumn> {
        Err(bind_error(
            "INSERT VALUES cannot reference a target-row column",
        ))
    }
}

fn bind_insert(
    insert: &Insert,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    parameters: &mut ParameterCollector,
) -> Result<BoundParts> {
    let scope = BindingScope::for_target(catalog, &insert.table, None)?;
    let table = bound_table_from_scope(&scope, insert.table.span())?;
    let targets = if insert.columns.is_empty() {
        scope
            .expand_star(None)?
            .iter()
            .map(|column| bound_column(column, insert.table.span()))
            .collect()
    } else {
        bind_target_columns(&scope, &insert.columns)?
    };
    if targets.len() != insert.values.len() {
        return Err(bind_error(
            "INSERT target column count does not match VALUES count",
        ));
    }

    let value_scope = NoColumnScope;
    let mut values = Vec::with_capacity(insert.values.len());
    for (value, target) in insert.values.iter().zip(&targets) {
        let value = bind_assignment_expr_for_column(
            value,
            &value_scope,
            target.sql_type,
            oracle_text_source_allowed(target.oracle_type),
            parameters,
        )?;
        require_scalar(&value, "INSERT VALUES")?;
        values.push(value);
    }
    Ok(BoundParts {
        kind: BoundStatementKind::Insert {
            table,
            targets,
            values,
        },
        dependencies: vec![table_dependency(table, database_uuid)],
        created_names: Vec::new(),
        columns: Vec::new(),
    })
}

fn bind_update(
    update: &Update,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    parameters: &mut ParameterCollector,
) -> Result<BoundParts> {
    let scope = BindingScope::for_target(catalog, &update.table, update.alias.as_ref())?;
    let table = bound_table_from_scope(&scope, update.table.span())?;
    let mut assigned = HashSet::new();
    let mut assignments = Vec::with_capacity(update.assignments.len());
    for assignment in &update.assignments {
        let resolved = scope.resolve_column(std::slice::from_ref(&assignment.column))?;
        if !assigned.insert(resolved.column_id) {
            return Err(bind_error("UPDATE target column is repeated"));
        }
        let target = bound_column(&resolved, assignment.column.span());
        let value = bind_assignment_expr_for_column(
            &assignment.value,
            &scope,
            target.sql_type,
            oracle_text_source_allowed(target.oracle_type),
            parameters,
        )?;
        require_scalar(&value, "UPDATE assignment")?;
        assignments.push(BoundAssignment {
            target,
            value,
            span: assignment.span,
        });
    }
    let selection = update
        .selection
        .as_ref()
        .map(|selection| bind_condition(selection, &scope, parameters))
        .transpose()?;
    if let Some(selection) = &selection {
        require_scalar(selection, "UPDATE WHERE")?;
    }

    Ok(BoundParts {
        kind: BoundStatementKind::Update {
            table,
            assignments,
            selection,
        },
        dependencies: vec![table_dependency(table, database_uuid)],
        created_names: Vec::new(),
        columns: Vec::new(),
    })
}

fn bind_delete(
    delete: &Delete,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    parameters: &mut ParameterCollector,
) -> Result<BoundParts> {
    let scope = BindingScope::for_target(catalog, &delete.table, delete.alias.as_ref())?;
    let table = bound_table_from_scope(&scope, delete.table.span())?;
    let selection = delete
        .selection
        .as_ref()
        .map(|selection| bind_condition(selection, &scope, parameters))
        .transpose()?;
    if let Some(selection) = &selection {
        require_scalar(selection, "DELETE WHERE")?;
    }
    Ok(BoundParts {
        kind: BoundStatementKind::Delete { table, selection },
        dependencies: vec![table_dependency(table, database_uuid)],
        created_names: Vec::new(),
        columns: Vec::new(),
    })
}

fn bind_create_table(
    create: &CreateTable,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    parameters: &mut ParameterCollector,
) -> Result<BoundParts> {
    let name = checked_catalog_name(&create.name)?;
    if catalog.object_name_exists(ObjectKind::Table, &name) {
        return Err(bind_error("table name is already present in the catalog"));
    }

    let mut columns = bind_column_definitions(create)?;
    let mut primary_key_seen = false;
    for constraint in create_constraints(create) {
        match constraint {
            Constraint::PrimaryKey(named) => {
                if primary_key_seen {
                    return Err(bind_error(
                        "CREATE TABLE contains multiple PRIMARY KEY constraints",
                    ));
                }
                primary_key_seen = true;
                for ordinal in resolve_local_columns(&columns, &named.columns)? {
                    columns[usize::from(ordinal)].nullable = false;
                }
            }
            Constraint::NotNull(not_null) => {
                let ordinal = resolve_local_column(&columns, &not_null.column)?;
                columns[usize::from(ordinal)].nullable = false;
            }
            Constraint::Unique(_) | Constraint::Check { .. } | Constraint::ForeignKey(_) => {}
        }
    }

    let scope = NewTableScope::new(&columns)?;
    let mut dependencies = Vec::new();
    let mut constraint_names = HashSet::new();
    let mut created_names = vec![CreatedNameDependency {
        object_kind: ObjectKind::Table,
        name: name.clone(),
    }];
    let mut constraints = Vec::new();
    for constraint in create_constraints(create) {
        if let Constraint::ForeignKey(foreign_key) = constraint {
            let referenced_name = checked_catalog_name(&foreign_key.referenced_table)?;
            if referenced_name == name {
                return Err(DbError::new(
                    ErrorCode::Unsupported,
                    "self-referential foreign keys require Phase 4C2-C",
                ));
            }
        }
        let name = checked_constraint_name(constraint, &mut constraint_names)?;
        if let Some(constraint_name) = &name {
            if catalog.object_name_exists(ObjectKind::Constraint, constraint_name) {
                return Err(bind_error(
                    "constraint name is already present in the catalog",
                ));
            }
            created_names.push(CreatedNameDependency {
                object_kind: ObjectKind::Constraint,
                name: constraint_name.clone(),
            });
        }
        constraints.push(bind_constraint(
            constraint,
            name,
            &columns,
            &scope,
            catalog,
            database_uuid,
            parameters,
            &mut dependencies,
        )?);
    }

    Ok(BoundParts {
        kind: BoundStatementKind::CreateTable {
            name: name.clone(),
            columns,
            constraints,
        },
        dependencies,
        created_names,
        columns: Vec::new(),
    })
}

fn bind_column_definitions(create: &CreateTable) -> Result<Vec<BoundColumnDefinition>> {
    let mut names = HashSet::new();
    let mut columns = Vec::with_capacity(create.columns.len());
    for (index, column) in create.columns.iter().enumerate() {
        let ordinal = u16::try_from(index)
            .map_err(|_| bind_error("CREATE TABLE column ordinal exceeds u16"))?;
        if ordinal >= MAX_COLUMNS {
            return Err(bind_error("CREATE TABLE exceeds the catalog column limit"));
        }
        let name = checked_catalog_name(&column.name)?;
        if !names.insert(name.as_str().to_owned()) {
            return Err(bind_error("CREATE TABLE contains a duplicate column name"));
        }
        let declaration = bind_type_declaration(&column.data_type)?;
        columns.push(BoundColumnDefinition {
            name,
            ordinal,
            column_type: declaration.column_type,
            oracle_type: declaration.oracle_type,
            nullable: true,
            span: column.span,
        });
    }
    Ok(columns)
}

fn bind_type_declaration(declaration: &TypeDeclaration) -> Result<BoundTypeDeclaration> {
    if declaration.name.is_quoted() {
        return Err(DbError::new(
            ErrorCode::Unsupported,
            "quoted SQL type declarations are not supported by catalog V1",
        ));
    }
    let oracle_type = match declaration.name.as_str() {
        "NUMBER" => {
            require_no_modifiers(declaration)?;
            if !(1..=2).contains(&declaration.arguments.len()) {
                return Err(invalid_type_declaration(
                    declaration,
                    "NUMBER requires precision and optional scale",
                ));
            }
            let precision = parse_type_argument::<u8>(declaration, 0)?;
            let scale = if declaration.arguments.len() == 2 {
                parse_type_argument::<i8>(declaration, 1)?
            } else {
                0
            };
            Some(
                OracleTypeDescriptor::number(precision, scale)
                    .map_err(|error| locate_type_error(error, declaration.span))?,
            )
        }
        "VARCHAR2" => {
            require_exact_shape(declaration, 1)?;
            Some(
                OracleTypeDescriptor::varchar2(parse_type_argument(declaration, 0)?)
                    .map_err(|error| locate_type_error(error, declaration.span))?,
            )
        }
        "CHAR" => {
            require_exact_shape(declaration, 1)?;
            Some(
                OracleTypeDescriptor::char_type(parse_type_argument(declaration, 0)?)
                    .map_err(|error| locate_type_error(error, declaration.span))?,
            )
        }
        "RAW" => {
            require_exact_shape(declaration, 1)?;
            Some(
                OracleTypeDescriptor::raw(parse_type_argument(declaration, 0)?)
                    .map_err(|error| locate_type_error(error, declaration.span))?,
            )
        }
        "DATE" => {
            require_exact_shape(declaration, 0)?;
            Some(OracleTypeDescriptor::date())
        }
        "TIMESTAMP" => {
            if declaration.arguments.len() > 1 {
                return Err(invalid_type_declaration(
                    declaration,
                    "TIMESTAMP accepts at most one fractional precision",
                ));
            }
            let precision = if declaration.arguments.is_empty() {
                6
            } else {
                parse_type_argument::<u8>(declaration, 0)?
            };
            match declaration.modifiers.as_slice() {
                [] => Some(
                    OracleTypeDescriptor::timestamp(precision)
                        .map_err(|error| locate_type_error(error, declaration.span))?,
                ),
                [TypeModifier::WithTimeZone(_)] => Some(
                    OracleTypeDescriptor::timestamp_with_time_zone(precision)
                        .map_err(|error| locate_type_error(error, declaration.span))?,
                ),
                _ => {
                    return Err(invalid_type_declaration(
                        declaration,
                        "invalid TIMESTAMP time-zone modifier",
                    ));
                }
            }
        }
        "INT64" | "DECIMAL" | "FLOAT64" | "TEXT" | "BLOB" | "BOOL" => {
            require_exact_shape(declaration, 0)?;
            None
        }
        _ => Err(DbError::new(
            ErrorCode::Unsupported,
            "SQL type is not representable by catalog V1",
        ))?,
    };
    let column_type = match oracle_type {
        Some(descriptor) => descriptor.physical_type(),
        None => match declaration.name.as_str() {
            "INT64" => ColumnType::Int64,
            "DECIMAL" => ColumnType::Decimal,
            "FLOAT64" => ColumnType::Float64,
            "TEXT" => ColumnType::Text,
            "BLOB" => ColumnType::Blob,
            "BOOL" => ColumnType::Bool,
            _ => unreachable!("non-Oracle declaration was matched above"),
        },
    };
    Ok(BoundTypeDeclaration {
        column_type,
        oracle_type,
    })
}

fn require_exact_shape(declaration: &TypeDeclaration, arguments: usize) -> Result<()> {
    if declaration.arguments.len() != arguments || !declaration.modifiers.is_empty() {
        return Err(invalid_type_declaration(
            declaration,
            "invalid SQL type arguments or modifiers",
        ));
    }
    Ok(())
}

fn require_no_modifiers(declaration: &TypeDeclaration) -> Result<()> {
    if !declaration.modifiers.is_empty() {
        return Err(invalid_type_declaration(
            declaration,
            "invalid SQL type modifier",
        ));
    }
    Ok(())
}

fn parse_type_argument<T>(declaration: &TypeDeclaration, index: usize) -> Result<T>
where
    T: std::str::FromStr,
{
    declaration
        .arguments
        .get(index)
        .and_then(|argument| argument.text.parse().ok())
        .ok_or_else(|| {
            invalid_type_declaration(declaration, "SQL type argument must be an integer")
        })
}

fn invalid_type_declaration(declaration: &TypeDeclaration, message: &'static str) -> DbError {
    locate_type_error(
        DbError::new(ErrorCode::Type, message).with_oracle_code(902),
        declaration.span,
    )
}

fn locate_type_error(error: DbError, span: SqlSpan) -> DbError {
    let location = SqlLocation::new(span.start(), 1, span.start().saturating_add(1))
        .expect("SQL span offsets produce a valid fallback location");
    error.with_sql_location(location)
}

fn create_constraints(create: &CreateTable) -> Vec<&Constraint> {
    let mut constraints = create
        .columns
        .iter()
        .flat_map(|column| column.constraints.iter())
        .chain(create.constraints.iter())
        .collect::<Vec<_>>();
    constraints.sort_by_key(|constraint| constraint.span().start());
    constraints
}

#[allow(clippy::too_many_arguments)]
fn bind_constraint(
    constraint: &Constraint,
    name: Option<CatalogName>,
    columns: &[BoundColumnDefinition],
    scope: &NewTableScope,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    parameters: &mut ParameterCollector,
    dependencies: &mut Vec<SchemaDependency>,
) -> Result<BoundConstraint> {
    match constraint {
        Constraint::PrimaryKey(named) => Ok(BoundConstraint::PrimaryKey {
            name,
            columns: resolve_local_columns(columns, &named.columns)?,
            span: named.span,
        }),
        Constraint::Unique(named) => Ok(BoundConstraint::Unique {
            name,
            columns: resolve_local_columns(columns, &named.columns)?,
            span: named.span,
        }),
        Constraint::NotNull(not_null) => Ok(BoundConstraint::NotNull {
            name,
            column: resolve_local_column(columns, &not_null.column)?,
            span: not_null.span,
        }),
        Constraint::Check {
            expression, span, ..
        } => {
            let mut expression = bind_condition(expression, scope, parameters)?;
            mark_new_table_columns(&mut expression, scope.table_id)?;
            require_scalar(&expression, "CHECK")?;
            Ok(BoundConstraint::Check {
                name,
                expression,
                span: *span,
            })
        }
        Constraint::ForeignKey(foreign_key) => bind_foreign_key(
            foreign_key,
            name,
            columns,
            catalog,
            database_uuid,
            dependencies,
        ),
    }
}

/// Rejects expression nodes that cannot become a persistent, table-local CHECK
/// program. This is deliberately iterative because statement limits permit
/// deeply nested expressions.
fn require_persistence_safe_checks(kind: &BoundStatementKind) -> Result<()> {
    let BoundStatementKind::CreateTable { constraints, .. } = kind else {
        return Ok(());
    };
    for constraint in constraints {
        if let BoundConstraint::Check { expression, .. } = constraint {
            require_persistence_safe_check(expression)?;
        }
    }
    Ok(())
}

fn require_persistence_safe_check(expression: &BoundExpr) -> Result<()> {
    let mut work = vec![expression];
    while let Some(expression) = work.pop() {
        match &expression.kind {
            BoundExprKind::Parameter(_) => {
                return Err(bind_error("CHECK cannot contain parameters"));
            }
            BoundExprKind::Aggregate { .. } => {
                return Err(bind_error("CHECK cannot contain aggregates"));
            }
            BoundExprKind::Column { .. } => {
                return Err(bind_error("CHECK cannot reference another table"));
            }
            BoundExprKind::Binary {
                op: crate::sql::bound::BoundBinaryOp::Modulo,
                ..
            } => {
                return Err(bind_error("CHECK cannot contain unsupported operators"));
            }
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                work.push(expr);
            }
            BoundExprKind::Binary { left, right, .. } => {
                work.push(right);
                work.push(left);
            }
            BoundExprKind::InList { expr, values, .. } => {
                work.extend(values.iter());
                work.push(expr);
            }
            BoundExprKind::Between {
                expr, low, high, ..
            } => {
                work.push(high);
                work.push(low);
                work.push(expr);
            }
            BoundExprKind::Like { expr, pattern, .. } => {
                work.push(pattern);
                work.push(expr);
            }
            BoundExprKind::Literal(_) | BoundExprKind::NewTableColumn { .. } => {}
        }
    }
    Ok(())
}

fn bind_foreign_key(
    foreign_key: &ForeignKey,
    name: Option<CatalogName>,
    columns: &[BoundColumnDefinition],
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
    dependencies: &mut Vec<SchemaDependency>,
) -> Result<BoundConstraint> {
    let local_columns = resolve_local_columns(columns, &foreign_key.columns)?;
    if local_columns.len() != foreign_key.referenced_columns.len() {
        return Err(bind_error("FOREIGN KEY column counts do not match"));
    }
    let referenced_name = checked_catalog_name(&foreign_key.referenced_table)?;
    let referenced = catalog
        .find_table_by_name(&referenced_name)
        .ok_or_else(|| bind_error("referenced table is not present in the catalog"))?;
    let referenced_table = BoundTable {
        table_id: referenced.id(),
        schema_version: referenced.schema_version(),
        span: foreign_key.referenced_table.span(),
    };
    let mut seen = HashSet::new();
    let mut referenced_columns = Vec::with_capacity(foreign_key.referenced_columns.len());
    let mut referenced_column_ids = Vec::with_capacity(foreign_key.referenced_columns.len());
    for (ordinal, referenced_column_name) in local_columns
        .iter()
        .copied()
        .zip(&foreign_key.referenced_columns)
    {
        let span = referenced_column_name.span();
        let referenced_column_name = checked_catalog_name(referenced_column_name)?;
        let referenced_column = catalog
            .find_column_by_name(referenced.id(), &referenced_column_name)
            .ok_or_else(|| {
                bind_error("referenced column is not present on the referenced table")
            })?;
        if !seen.insert(referenced_column.id()) {
            return Err(bind_error(
                "FOREIGN KEY contains a duplicate referenced column",
            ));
        }
        let local_column = &columns[usize::from(ordinal)];
        if !bound_foreign_key_columns_compatible(local_column, referenced_column) {
            return Err(DbError::new(
                ErrorCode::Type,
                "FOREIGN KEY child and parent column types are incompatible",
            ));
        }
        let referenced_type = column_type_to_sql_type(referenced_column.column_type());
        referenced_column_ids.push(referenced_column.id());
        referenced_columns.push(BoundColumn {
            table_id: referenced_column.table_id(),
            column_id: referenced_column.id(),
            ordinal: referenced_column.ordinal(),
            sql_type: referenced_type,
            oracle_type: referenced_column.oracle_type(),
            nullable: referenced_column.nullable(),
            span,
        });
    }
    let parent_constraint_id = catalog
        .find_parent_key_constraint(referenced.id(), &referenced_column_ids)
        .map(|constraint| constraint.id())
        .ok_or_else(|| {
            bind_error(
                "FOREIGN KEY parent columns must exactly match a declared PRIMARY KEY or UNIQUE constraint",
            )
        })?;
    push_dependency(
        dependencies,
        table_dependency(referenced_table, database_uuid),
    );
    Ok(BoundConstraint::ForeignKey {
        name,
        columns: local_columns,
        referenced_table,
        referenced_columns,
        parent_constraint_id,
        span: foreign_key.span,
    })
}

fn foreign_key_types_compatible(child: ColumnType, parent: ColumnType) -> bool {
    child == parent || matches!((child, parent), (ColumnType::Int64, ColumnType::Decimal))
}

fn bound_foreign_key_columns_compatible(
    child: &BoundColumnDefinition,
    parent: &crate::storage::catalog::types::ColumnDef,
) -> bool {
    match (child.oracle_type, parent.oracle_type()) {
        (Some(left), Some(right)) => left == right,
        (None, None) => foreign_key_types_compatible(child.column_type, parent.column_type()),
        _ => false,
    }
}

fn bind_create_index(
    create: &CreateIndex,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
) -> Result<BoundParts> {
    let name = checked_catalog_name(&create.name)?;
    if catalog.object_name_exists(ObjectKind::Index, &name) {
        return Err(bind_error("index name is already present in the catalog"));
    }
    let scope = BindingScope::for_target(catalog, &create.table, None)?;
    let table = bound_table_from_scope(&scope, create.table.span())?;
    let columns = bind_target_columns(&scope, &create.columns)?;
    Ok(BoundParts {
        kind: BoundStatementKind::CreateIndex {
            name: name.clone(),
            table,
            columns,
            unique: create.unique,
        },
        dependencies: vec![table_dependency(table, database_uuid)],
        created_names: vec![CreatedNameDependency {
            object_kind: ObjectKind::Index,
            name,
        }],
        columns: Vec::new(),
    })
}

fn bind_drop_table(
    name: &Identifier,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
) -> Result<BoundParts> {
    let span = name.span();
    let name = checked_catalog_name(name)?;
    let table = catalog
        .find_table_by_name(&name)
        .ok_or_else(|| bind_error("table is not present in the catalog"))?;
    let table = BoundTable {
        table_id: table.id(),
        schema_version: table.schema_version(),
        span,
    };
    Ok(BoundParts {
        kind: BoundStatementKind::DropTable {
            table_id: table.table_id,
        },
        dependencies: vec![table_dependency(table, database_uuid)],
        created_names: Vec::new(),
        columns: Vec::new(),
    })
}

fn bind_drop_index(
    name: &Identifier,
    catalog: &CatalogReader,
    database_uuid: [u8; 16],
) -> Result<BoundParts> {
    let span = name.span();
    let name = checked_catalog_name(name)?;
    let index = catalog
        .find_index_by_name(&name)
        .ok_or_else(|| bind_error("index is not present in the catalog"))?;
    let index_id = index.id();
    let owner_table_id = index.table_id();
    let table = catalog
        .find_table_by_id(owner_table_id)
        .ok_or_else(|| bind_error("index owner table is not present in the catalog"))?;
    let table = BoundTable {
        table_id: table.id(),
        schema_version: table.schema_version(),
        span,
    };
    Ok(BoundParts {
        kind: BoundStatementKind::DropIndex { index_id, table },
        dependencies: vec![table_dependency(table, database_uuid)],
        created_names: Vec::new(),
        columns: Vec::new(),
    })
}

fn bind_target_columns(
    scope: &BindingScope,
    identifiers: &[Identifier],
) -> Result<Vec<BoundColumn>> {
    let mut seen = HashSet::new();
    let mut columns = Vec::with_capacity(identifiers.len());
    for identifier in identifiers {
        let column = scope.resolve_column(std::slice::from_ref(identifier))?;
        if !seen.insert(column.column_id) {
            return Err(bind_error("target column is repeated"));
        }
        columns.push(bound_column(&column, identifier.span()));
    }
    Ok(columns)
}

fn bound_table_from_scope(scope: &BindingScope, span: SqlSpan) -> Result<BoundTable> {
    let mut identities = scope.source_identities();
    let identity = identities
        .next()
        .ok_or_else(|| bind_error("target scope has no table identity"))?;
    if identities.next().is_some() {
        return Err(bind_error("target scope has more than one table identity"));
    }
    Ok(BoundTable {
        table_id: identity.table_id,
        schema_version: identity.schema_version,
        span,
    })
}

fn bound_column(column: &ResolvedColumn, span: SqlSpan) -> BoundColumn {
    BoundColumn {
        table_id: column.table_id,
        column_id: column.column_id,
        ordinal: column.ordinal,
        sql_type: column.sql_type,
        oracle_type: column.oracle_type,
        nullable: column.nullable,
        span,
    }
}

fn oracle_text_source_allowed(descriptor: Option<OracleTypeDescriptor>) -> bool {
    matches!(
        descriptor,
        Some(
            OracleTypeDescriptor::Number { .. }
                | OracleTypeDescriptor::Date
                | OracleTypeDescriptor::Timestamp { .. }
                | OracleTypeDescriptor::TimestampWithTimeZone { .. }
        )
    )
}

fn table_dependency(table: BoundTable, database_uuid: [u8; 16]) -> SchemaDependency {
    SchemaDependency {
        database_uuid,
        table_id: table.table_id,
        schema_version: table.schema_version,
    }
}

fn push_dependency(dependencies: &mut Vec<SchemaDependency>, dependency: SchemaDependency) {
    if dependencies.iter().any(|existing| {
        existing.table_id == dependency.table_id
            && existing.schema_version == dependency.schema_version
    }) {
        return;
    }
    dependencies.push(dependency);
}

fn bind_condition(
    expression: &crate::sql::ast::AstExpr,
    scope: &impl ColumnResolver,
    parameters: &mut ParameterCollector,
) -> Result<BoundExpr> {
    bind_expr(expression, scope, Some(SqlType::Bool), parameters)
}

fn require_scalar(expression: &BoundExpr, clause: &'static str) -> Result<()> {
    let mut work = vec![expression];
    while let Some(expression) = work.pop() {
        match &expression.kind {
            BoundExprKind::Aggregate { .. } => {
                return Err(bind_error(match clause {
                    "INSERT VALUES" => "INSERT VALUES cannot contain aggregates",
                    "UPDATE assignment" => "UPDATE assignments cannot contain aggregates",
                    "UPDATE WHERE" => "UPDATE WHERE cannot contain aggregates",
                    "DELETE WHERE" => "DELETE WHERE cannot contain aggregates",
                    "CHECK" => "CHECK cannot contain aggregates",
                    _ => "this clause cannot contain aggregates",
                }));
            }
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                work.push(expr);
            }
            BoundExprKind::Binary { left, right, .. } => {
                work.push(right);
                work.push(left);
            }
            BoundExprKind::InList { expr, values, .. } => {
                work.extend(values.iter());
                work.push(expr);
            }
            BoundExprKind::Between {
                expr, low, high, ..
            } => {
                work.push(high);
                work.push(low);
                work.push(expr);
            }
            BoundExprKind::Like { expr, pattern, .. } => {
                work.push(pattern);
                work.push(expr);
            }
            BoundExprKind::Literal(_)
            | BoundExprKind::Column { .. }
            | BoundExprKind::NewTableColumn { .. }
            | BoundExprKind::Parameter(_) => {}
        }
    }
    Ok(())
}

fn resolve_local_columns(
    columns: &[BoundColumnDefinition],
    identifiers: &[Identifier],
) -> Result<Vec<u16>> {
    let mut seen = HashSet::new();
    let mut resolved = Vec::with_capacity(identifiers.len());
    for identifier in identifiers {
        let ordinal = resolve_local_column(columns, identifier)?;
        if !seen.insert(ordinal) {
            return Err(bind_error("constraint contains a duplicate local column"));
        }
        resolved.push(ordinal);
    }
    Ok(resolved)
}

fn resolve_local_column(columns: &[BoundColumnDefinition], identifier: &Identifier) -> Result<u16> {
    columns
        .iter()
        .find(|column| column.name.as_str() == identifier.as_str())
        .map(|column| column.ordinal)
        .ok_or_else(|| bind_error("constraint column is not present on the new table"))
}

fn checked_constraint_name(
    constraint: &Constraint,
    names: &mut HashSet<String>,
) -> Result<Option<CatalogName>> {
    let name = match constraint {
        Constraint::PrimaryKey(named) | Constraint::Unique(named) => named.name.as_ref(),
        Constraint::NotNull(not_null) => not_null.name.as_ref(),
        Constraint::Check { name, .. } => name.as_ref(),
        Constraint::ForeignKey(foreign_key) => foreign_key.name.as_ref(),
    }
    .map(checked_catalog_name)
    .transpose()?;
    if let Some(name) = &name {
        if !names.insert(name.as_str().to_owned()) {
            return Err(bind_error(
                "CREATE TABLE contains a duplicate constraint name",
            ));
        }
    }
    Ok(name)
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

const fn column_type_to_sql_type(column_type: ColumnType) -> SqlType {
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

struct NewTableScope {
    table_id: TableId,
    columns: Vec<ResolvedColumn>,
}

impl NewTableScope {
    fn new(columns: &[BoundColumnDefinition]) -> Result<Self> {
        let table_id = TableId::new(u64::MAX)
            .map_err(|_| bind_error("new table binding marker is invalid"))?;
        let schema_version =
            SchemaVersion::new(1).map_err(|_| bind_error("new table binding marker is invalid"))?;
        let mut resolved = Vec::with_capacity(columns.len());
        for column in columns {
            let column_id = ColumnId::new(u64::MAX - u64::from(column.ordinal))
                .map_err(|_| bind_error("new table column binding marker is invalid"))?;
            resolved.push(ResolvedColumn {
                source_id: crate::sql::bound::SourceId(0),
                table_id,
                schema_version,
                column_id,
                ordinal: column.ordinal,
                sql_type: column_type_to_sql_type(column.column_type),
                oracle_type: column.oracle_type,
                nullable: column.nullable,
                name: column.name.as_str().to_owned(),
            });
        }
        Ok(Self {
            table_id,
            columns: resolved,
        })
    }
}

fn mark_new_table_columns(expression: &mut BoundExpr, table_id: TableId) -> Result<()> {
    let mut work = vec![expression];
    while let Some(expression) = work.pop() {
        let local_ordinal = match &expression.kind {
            BoundExprKind::Column {
                table_id: column_table_id,
                ordinal,
                ..
            } if *column_table_id == table_id => Some(*ordinal),
            BoundExprKind::Column { .. } => {
                return Err(DbError::new(
                    ErrorCode::Internal,
                    "new-table CHECK expression escaped its isolated scope",
                ));
            }
            _ => None,
        };
        if let Some(ordinal) = local_ordinal {
            expression.kind = BoundExprKind::NewTableColumn { ordinal };
            continue;
        }
        match &mut expression.kind {
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                work.push(expr);
            }
            BoundExprKind::Binary { left, right, .. } => {
                work.push(right);
                work.push(left);
            }
            BoundExprKind::InList { expr, values, .. } => {
                work.extend(values.iter_mut());
                work.push(expr);
            }
            BoundExprKind::Between {
                expr, low, high, ..
            } => {
                work.push(high);
                work.push(low);
                work.push(expr);
            }
            BoundExprKind::Like { expr, pattern, .. } => {
                work.push(pattern);
                work.push(expr);
            }
            BoundExprKind::Aggregate {
                argument: Some(argument),
                ..
            } => work.push(argument),
            BoundExprKind::Literal(_)
            | BoundExprKind::NewTableColumn { .. }
            | BoundExprKind::Parameter(_)
            | BoundExprKind::Aggregate { argument: None, .. }
            | BoundExprKind::Column { .. } => {}
        }
    }
    Ok(())
}

impl ColumnResolver for NewTableScope {
    fn resolve_column(&self, parts: &[Identifier]) -> Result<ResolvedColumn> {
        let [identifier] = parts else {
            return Err(bind_error(
                "CHECK columns must be unqualified new-table columns",
            ));
        };
        self.columns
            .iter()
            .find(|column| column.name == identifier.as_str())
            .cloned()
            .ok_or_else(|| bind_error("CHECK column is not present on the new table"))
    }
}

fn bind_error(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Bind, message)
}

#[cfg(test)]
mod tests {
    use crate::{
        ErrorCode,
        api::{DataType, ParameterIdentifier},
        sql::{
            SqlLimits,
            ast::AstStatement,
            bind::bind_statement,
            bound::{BoundConstraint, BoundStatement, BoundStatementKind},
            parser::parse_statement,
        },
        storage::{
            catalog::types::{CatalogName, ObjectKind},
            catalog::{
                CatalogReader, add_unrelated_table, bump_users_schema_version,
                drop_and_recreate_users, replace_users_id_column_id,
            },
            index::{IndexSpec, create_index},
            relation::{
                ColumnSpec, ConstrainedTableSpec, TableConstraintSpec, TableSpec,
                create_constrained_relation,
            },
            test_support::{
                Phase4bCatalogFixture, phase_4b_catalog_fixture_with_constraint,
                phase_4b_catalog_fixture_with_explicit_index,
            },
        },
    };

    use super::super::test_support::prepare_fixture;

    fn foreign_key_catalog_fixture() -> Phase4bCatalogFixture {
        use crate::storage::catalog::types::ColumnType;

        fn column(name: &str, column_type: ColumnType) -> ColumnSpec {
            ColumnSpec::new(CatalogName::new(name).unwrap(), column_type, false)
        }

        fn table(name: &str, columns: Vec<ColumnSpec>) -> TableSpec {
            TableSpec::new(CatalogName::new(name).unwrap(), columns).unwrap()
        }

        let fixture = crate::storage::test_support::phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let mut batch = pager.begin_write().unwrap();
        for (name, column_type) in [
            ("PARENT", ColumnType::Int64),
            ("DEC_PARENT", ColumnType::Decimal),
        ] {
            let spec = ConstrainedTableSpec::new(
                table(name, vec![column("ID", column_type)]),
                vec![TableConstraintSpec::PrimaryKey {
                    name: Some(CatalogName::new(format!("PK_{name}")).unwrap()),
                    columns: vec![0].into_boxed_slice(),
                }],
            )
            .unwrap();
            create_constrained_relation(&mut batch, &spec).unwrap();
        }
        let composite = ConstrainedTableSpec::new(
            table(
                "COMPOSITE",
                vec![
                    column("A", ColumnType::Int64),
                    column("B", ColumnType::Int64),
                ],
            ),
            vec![TableConstraintSpec::Unique {
                name: Some(CatalogName::new("UQ_COMPOSITE").unwrap()),
                columns: vec![0, 1].into_boxed_slice(),
            }],
        )
        .unwrap();
        create_constrained_relation(&mut batch, &composite).unwrap();

        let explicit = ConstrainedTableSpec::new(
            table("EXPLICIT_UQ", vec![column("A", ColumnType::Int64)]),
            Vec::new(),
        )
        .unwrap();
        let explicit_table = create_constrained_relation(&mut batch, &explicit).unwrap();
        let explicit_columns = batch
            .catalog_reader()
            .unwrap()
            .columns_for_table(explicit_table)
            .cloned()
            .collect();
        create_index(
            &mut batch,
            &IndexSpec::new(
                CatalogName::new("UX_EXPLICIT_UQ").unwrap(),
                explicit_table,
                explicit_columns,
                true,
            )
            .unwrap(),
            u64::MAX,
        )
        .unwrap();
        batch.commit().unwrap();
        fixture
    }

    #[test]
    fn foreign_key_binding_requires_exact_declared_parent_key_and_supported_types() {
        // Production break caught: broad type unification or index-shaped lookup
        // can accept a parent that is not the exact declared PK/UNIQUE target.
        let fixture = foreign_key_catalog_fixture();
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let bind = |sql: &str| {
            let statement = parse_statement(sql, &SqlLimits::default())?;
            bind_statement(&statement, &catalog, database_uuid)
        };

        let parent =
            bind("CREATE TABLE C (P INT64, CONSTRAINT FK FOREIGN KEY (P) REFERENCES PARENT (ID))")
                .unwrap();
        let expected_parent_constraint_id = catalog
            .find_constraint_by_name(&CatalogName::new("PK_PARENT").unwrap())
            .unwrap()
            .id();
        let BoundStatementKind::CreateTable { constraints, .. } = parent.kind else {
            panic!("expected CREATE TABLE")
        };
        assert!(matches!(
            &constraints[0],
            BoundConstraint::ForeignKey {
                parent_constraint_id,
                ..
            } if *parent_constraint_id == expected_parent_constraint_id
        ));
        assert!(
            bind(
                "CREATE TABLE C (P INT64, CONSTRAINT FK FOREIGN KEY (P) REFERENCES DEC_PARENT (ID))"
            )
            .is_ok()
        );
        assert!(
            bind(
                "CREATE TABLE C (A INT64, B INT64, FOREIGN KEY (A, B) REFERENCES COMPOSITE (A, B))"
            )
            .is_ok()
        );
        for sql in [
            "CREATE TABLE C (A INT64, FOREIGN KEY (A) REFERENCES EXPLICIT_UQ (A))",
            "CREATE TABLE C (A INT64, FOREIGN KEY (A) REFERENCES COMPOSITE (A))",
            "CREATE TABLE C (A INT64, B INT64, FOREIGN KEY (A, B) REFERENCES COMPOSITE (B, A))",
        ] {
            let error = bind(sql).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Bind, "{sql}");
            assert_eq!(
                error.to_string(),
                "[1100] FOREIGN KEY parent columns must exactly match a declared PRIMARY KEY or UNIQUE constraint",
                "{sql}"
            );
        }
        assert_eq!(
            bind("CREATE TABLE C (A TEXT, FOREIGN KEY (A) REFERENCES PARENT (ID))")
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn foreign_key_schema_dependency_is_checked_before_retained_parent_identities() {
        // Production break caught: execution must report the parent table's
        // schema-version change before inspecting retained FK column/constraint IDs.
        let fixture = foreign_key_catalog_fixture();
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let (bound, parent_table_id) = {
            let mut snapshot = pager.begin_read().unwrap();
            let catalog = snapshot.catalog_reader().unwrap();
            let sql = "CREATE TABLE C (P INT64 REFERENCES PARENT (ID))";
            let statement = parse_statement(sql, &SqlLimits::default()).unwrap();
            let bound = bind_statement(&statement, &catalog, database_uuid).unwrap();
            let parent_table_id = catalog
                .find_table_by_name(&CatalogName::new("PARENT").unwrap())
                .unwrap()
                .id();
            (bound, parent_table_id)
        };
        let mut batch = pager.begin_write().unwrap();
        let parent_columns = batch
            .catalog_reader()
            .unwrap()
            .columns_for_table(parent_table_id)
            .cloned()
            .collect();
        create_index(
            &mut batch,
            &IndexSpec::new(
                CatalogName::new("IX_PARENT_ID").unwrap(),
                parent_table_id,
                parent_columns,
                false,
            )
            .unwrap(),
            u64::MAX,
        )
        .unwrap();
        batch.commit().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let error = bound.validate_schema(database_uuid, &catalog).unwrap_err();
        assert_eq!(error.code(), ErrorCode::SchemaChanged);
        assert_eq!(
            error.to_string(),
            "[1150] prepared table schema version has changed"
        );
    }

    #[test]
    fn create_table_rejects_check_parameters_before_planning() {
        let error = prepare_fixture("CREATE TABLE T (N INT64, CHECK (N > :1))").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Bind);
    }

    #[test]
    fn create_table_tracks_explicit_constraint_names_and_rejects_duplicates() {
        let statement = prepare_fixture(
            "CREATE TABLE NAME_DEPS (A INT64 CONSTRAINT NN_A NOT NULL, CONSTRAINT CK_A CHECK (A > 0))",
        )
        .unwrap();
        assert_eq!(
            statement
                .created_names
                .iter()
                .map(|dependency| (dependency.object_kind, dependency.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ObjectKind::Table, "NAME_DEPS"),
                (ObjectKind::Constraint, "NN_A"),
                (ObjectKind::Constraint, "CK_A"),
            ]
        );
        let error = prepare_fixture(
            "CREATE TABLE DUP_DEPS (A INT64 CONSTRAINT DUP_NAME NOT NULL, CONSTRAINT DUP_NAME CHECK (A > 0))",
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Bind);
    }

    #[test]
    fn create_table_rejects_an_existing_global_constraint_name() {
        let fixture = phase_4b_catalog_fixture_with_constraint(4096, "CK_SHARED");
        let pager = fixture.pager();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let statement = parse_statement(
            "CREATE TABLE T (N INT64, CONSTRAINT CK_SHARED CHECK (N > 0))",
            &SqlLimits::default(),
        )
        .unwrap();

        let error =
            bind_statement(&statement, &catalog, pager.database_uuid().unwrap()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Bind);
    }

    fn prepared_users_select() -> (
        BoundStatement,
        [u8; 16],
        Phase4bCatalogFixture,
        CatalogReader,
    ) {
        let fixture = crate::storage::test_support::phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let statement = parse_statement("SELECT ID FROM USERS", &SqlLimits::default()).unwrap();
        let bound = bind_statement(&statement, &catalog, database_uuid).unwrap();
        (bound, database_uuid, fixture, catalog)
    }

    #[test]
    fn dml_targets_use_catalog_ordinals_stable_ids_and_parameter_contexts() {
        // Production break caught: resolving INSERT targets by source order or
        // leaving their parameters Unknown would write values into the wrong
        // catalog columns at execution.
        let insert = prepare_fixture("INSERT INTO USERS (NAME, ID) VALUES (:name, :id)").unwrap();
        assert_eq!(insert.target_ordinals(), vec![1, 0]);
        let BoundStatementKind::Insert {
            table,
            targets,
            values: _,
        } = &insert.kind
        else {
            panic!("expected bound INSERT")
        };
        assert_eq!((table.table_id.get(), table.schema_version.get()), (1, 1));
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.column_id.get(), target.ordinal, target.sql_type))
                .collect::<Vec<_>>(),
            vec![
                (3, 1, crate::sql::SqlType::Text),
                (2, 0, crate::sql::SqlType::Int64),
            ]
        );
        assert_eq!(
            insert
                .parameters()
                .iter()
                .map(|parameter| (parameter.identifier(), parameter.data_type()))
                .collect::<Vec<_>>(),
            vec![
                (
                    &ParameterIdentifier::Name("NAME".into()),
                    Some(DataType::Text)
                ),
                (
                    &ParameterIdentifier::Name("ID".into()),
                    Some(DataType::Int64)
                ),
            ]
        );

        let default_order =
            prepare_fixture("INSERT INTO USERS VALUES (:id, :name, :amount, :float_value)")
                .unwrap();
        assert_eq!(default_order.target_ordinals(), vec![0, 1, 2, 3]);
        assert_eq!(
            default_order
                .parameters()
                .iter()
                .map(|parameter| parameter.data_type())
                .collect::<Vec<_>>(),
            vec![
                Some(DataType::Int64),
                Some(DataType::Text),
                Some(DataType::Decimal),
                Some(DataType::Float64),
            ]
        );
    }

    #[test]
    fn insert_values_do_not_see_the_target_row() {
        // Production break caught: INSERT has no old target row, so resolving
        // these references would read an arbitrary pre-insert value.
        for sql in [
            "INSERT INTO USERS (ID) VALUES (ID)",
            "INSERT INTO USERS (ID) VALUES (USERS.ID)",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
        assert!(prepare_fixture("INSERT INTO USERS (ID) VALUES (:id)").is_ok());
        assert!(prepare_fixture("INSERT INTO USERS (ID) VALUES (1)").is_ok());
    }

    #[test]
    fn dml_assignments_reject_narrowing_and_keep_safe_target_contexts() {
        // Production break caught: symmetric type unification lets a Decimal
        // assignment silently narrow into an INT64 storage column.
        for sql in [
            "INSERT INTO USERS (ID) VALUES (1.5)",
            "UPDATE USERS SET ID = 1.5",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Type,
                "{sql}"
            );
        }

        for sql in [
            "INSERT INTO USERS (AMOUNT) VALUES (1)",
            "UPDATE USERS SET AMOUNT = 1",
            "INSERT INTO USERS (ID) VALUES (NULL)",
            "UPDATE USERS SET ID = NULL",
        ] {
            assert!(prepare_fixture(sql).is_ok(), "{sql}");
        }

        let insert_parameter =
            prepare_fixture("INSERT INTO USERS (AMOUNT) VALUES (:amount)").unwrap();
        assert_eq!(
            insert_parameter.parameters()[0].data_type(),
            Some(DataType::Decimal)
        );
        let update_parameter = prepare_fixture("UPDATE USERS SET ID = :id").unwrap();
        assert_eq!(
            update_parameter.parameters()[0].data_type(),
            Some(DataType::Int64)
        );
    }

    #[test]
    fn repeated_dml_parameter_uses_the_narrow_safe_assignment_type() {
        // Production break caught: a shared parameter between an INT64 and a
        // DECIMAL target must remain INT64 (which can widen for DECIMAL),
        // regardless of target-list order. Broadening it to DECIMAL would
        // make its INT64 assignment a silent narrowing conversion.
        for sql in [
            "INSERT INTO USERS (ID, AMOUNT) VALUES (:shared, :shared)",
            "INSERT INTO USERS (AMOUNT, ID) VALUES (:shared, :shared)",
            "UPDATE USERS SET ID = :shared, AMOUNT = :shared",
            "UPDATE USERS SET AMOUNT = :shared, ID = :shared",
            "UPDATE USERS SET ID = +:shared, AMOUNT = +:shared",
            "UPDATE USERS SET AMOUNT = +:shared, ID = +:shared",
            "UPDATE USERS SET ID = :shared WHERE AMOUNT = :shared",
        ] {
            let statement = prepare_fixture(sql).unwrap();
            assert_eq!(
                statement.parameters()[0].data_type(),
                Some(DataType::Int64),
                "{sql}"
            );
        }
    }

    #[test]
    fn dml_rejects_duplicate_unknown_and_miscounted_targets() {
        // Production break caught: accepting duplicate, unknown, or mismatched
        // target lists lets an executor perform an ambiguous row mutation.
        for sql in [
            "INSERT INTO USERS (ID, ID) VALUES (1, 2)",
            "INSERT INTO USERS (MISSING) VALUES (1)",
            "INSERT INTO USERS (ID) VALUES (1, 2)",
            "UPDATE USERS SET NAME = 'A', NAME = 'B'",
            "UPDATE USERS SET MISSING = 1",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
    }

    #[test]
    fn update_and_delete_bind_old_target_row_scope() {
        // Production break caught: evaluating UPDATE RHS or DELETE/UPDATE WHERE
        // outside the target's pre-update row scope changes SQL semantics.
        let update =
            prepare_fixture("UPDATE USERS U SET NAME = U.NAME, ID = ID + :delta WHERE U.ID = :id")
                .unwrap();
        assert_eq!(update.target_ordinals(), vec![1, 0]);
        assert_eq!(
            update
                .parameters()
                .iter()
                .map(|parameter| (parameter.identifier(), parameter.data_type()))
                .collect::<Vec<_>>(),
            vec![
                (
                    &ParameterIdentifier::Name("DELTA".into()),
                    Some(DataType::Int64)
                ),
                (
                    &ParameterIdentifier::Name("ID".into()),
                    Some(DataType::Int64)
                ),
            ]
        );
        assert!(prepare_fixture("DELETE FROM USERS U WHERE U.ID = :id").is_ok());
        for sql in [
            "UPDATE USERS U SET NAME = :name WHERE ORDERS.ID = 1",
            "DELETE FROM USERS U WHERE ORDERS.ID = 1",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
        assert_eq!(
            prepare_fixture("UPDATE USERS SET ID = NAME")
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn create_table_maps_only_core_declarations_and_binds_own_constraints() {
        // Production break caught: collapsing declarations with attributes or
        // resolving CHECK columns through the ambient catalog loses DDL meaning.
        let fixture = foreign_key_catalog_fixture();
        let pager = fixture.pager();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let sql = "CREATE TABLE NEW_TYPES (\
                 I INT64 NOT NULL, D DECIMAL, F FLOAT64, T TEXT, B BLOB,\
                 DA DATE, TS TIMESTAMP, OK BOOL,\
                 CONSTRAINT NEW_TYPES_CK CHECK (OK),\
                 CONSTRAINT NEW_TYPES_FK FOREIGN KEY (I) REFERENCES PARENT (ID),\
                 CONSTRAINT NEW_TYPES_U UNIQUE (I, D))";
        let statement = parse_statement(sql, &SqlLimits::default()).unwrap();
        let create = bind_statement(&statement, &catalog, pager.database_uuid().unwrap()).unwrap();
        let BoundStatementKind::CreateTable {
            name,
            columns,
            constraints,
        } = &create.kind
        else {
            panic!("expected bound CREATE TABLE")
        };
        assert_eq!(name.as_str(), "NEW_TYPES");
        assert_eq!(
            columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.ordinal,
                    column.column_type,
                    column.nullable
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "I",
                    0,
                    crate::storage::catalog::types::ColumnType::Int64,
                    false
                ),
                (
                    "D",
                    1,
                    crate::storage::catalog::types::ColumnType::Decimal,
                    true
                ),
                (
                    "F",
                    2,
                    crate::storage::catalog::types::ColumnType::Float64,
                    true
                ),
                (
                    "T",
                    3,
                    crate::storage::catalog::types::ColumnType::Text,
                    true
                ),
                (
                    "B",
                    4,
                    crate::storage::catalog::types::ColumnType::Blob,
                    true
                ),
                (
                    "DA",
                    5,
                    crate::storage::catalog::types::ColumnType::Date,
                    true
                ),
                (
                    "TS",
                    6,
                    crate::storage::catalog::types::ColumnType::Timestamp,
                    true
                ),
                (
                    "OK",
                    7,
                    crate::storage::catalog::types::ColumnType::Bool,
                    true
                ),
            ]
        );
        assert!(
            constraints
                .iter()
                .any(|constraint| matches!(constraint, BoundConstraint::Check { .. }))
        );
        assert!(constraints.iter().any(|constraint| {
            matches!(
                constraint,
                BoundConstraint::Check {
                    expression: crate::sql::bound::BoundExpr {
                        kind: crate::sql::bound::BoundExprKind::NewTableColumn { ordinal: 7 },
                        ..
                    },
                    ..
                }
            )
        }));
        assert!(
            constraints
                .iter()
                .any(|constraint| matches!(constraint, BoundConstraint::ForeignKey { .. }))
        );
        assert!(
            constraints
                .iter()
                .any(|constraint| matches!(constraint, BoundConstraint::Unique { .. }))
        );
        let parent_id = catalog
            .find_table_by_name(
                &crate::storage::catalog::types::CatalogName::new("PARENT").unwrap(),
            )
            .unwrap()
            .id();
        assert_eq!(
            create
                .dependencies()
                .iter()
                .map(|dependency| dependency.table_id.get())
                .collect::<Vec<_>>(),
            vec![parent_id.get()]
        );

        for sql in [
            "CREATE TABLE NEW_DUP (ID INT64, ID TEXT)",
            "CREATE TABLE NEW_CHECK_UNKNOWN (ID INT64, CHECK (MISSING > 0))",
            "CREATE TABLE NEW_CHECK_EXTERNAL (ID INT64, CHECK (USERS.ID > 0))",
            "CREATE TABLE NEW_UNIQUE_DUP (ID INT64, UNIQUE (ID, ID))",
            "CREATE TABLE NEW_FK_UNKNOWN (USER_ID INT64 REFERENCES MISSING (ID))",
            "CREATE TABLE NEW_FK_COLUMN (USER_ID INT64 REFERENCES USERS (MISSING))",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
        for sql in [
            "CREATE TABLE NEW_CHECK_TYPE (NAME TEXT, CHECK (NAME))",
            "CREATE TABLE NEW_FK_TYPE (NAME TEXT REFERENCES USERS (ID))",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Type,
                "{sql}"
            );
        }
    }

    #[test]
    fn create_table_accepts_oracle_attributes_and_rejects_invalid_recognized_forms() {
        // Production break caught: rejecting supported Oracle declarations or
        // silently dropping invalid recognized attributes changes the schema contract.
        for sql in [
            "CREATE TABLE T_NUM (N NUMBER(10,2))",
            "CREATE TABLE T_VARCHAR (V VARCHAR2(30))",
            "CREATE TABLE T_TIMESTAMP_ZONE (TS TIMESTAMP WITH TIME ZONE)",
        ] {
            assert!(prepare_fixture(sql).is_ok(), "{sql}");
        }
        for sql in [
            "CREATE TABLE T_CORE_LENGTH (V TEXT(30))",
            "CREATE TABLE T_BARE_NUMBER (N NUMBER)",
            "CREATE TABLE T_BAD_NUMBER (N NUMBER(39))",
            "CREATE TABLE T_BAD_TIMESTAMP (TS TIMESTAMP(10))",
            "CREATE TABLE T_LOCAL_TIMESTAMP (TS TIMESTAMP WITH LOCAL TIME ZONE)",
        ] {
            let error = prepare_fixture(sql).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Type, "{sql}");
            assert_eq!(error.oracle_code(), Some(902), "{sql}");
            assert!(error.sql_location().is_some(), "{sql}");
        }
    }

    #[test]
    fn create_table_rejects_multiple_primary_key_constraints() {
        // Production break caught: accepting two primary keys creates an
        // unexecutable DDL contract before Phase 4C maintains constraints.
        for sql in [
            "CREATE TABLE INLINE_AND_TABLE (ID INT64 PRIMARY KEY, OTHER INT64, PRIMARY KEY (OTHER))",
            "CREATE TABLE TWO_TABLE (ID INT64, OTHER INT64, PRIMARY KEY (ID), PRIMARY KEY (OTHER))",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }
        assert!(prepare_fixture("CREATE TABLE ONE_PRIMARY (ID INT64 PRIMARY KEY)").is_ok());
    }

    #[test]
    fn create_table_rejects_quoted_core_type_names() {
        // Production break caught: quoted text is a custom type name, not an
        // alternate spelling of one of the eight Phase 4B core declarations.
        for data_type in [
            "INT64",
            "DECIMAL",
            "FLOAT64",
            "TEXT",
            "BLOB",
            "DATE",
            "TIMESTAMP",
            "BOOL",
            "int64",
        ] {
            let quoted = format!("CREATE TABLE QUOTED_{data_type} (VALUE \"{data_type}\")");
            assert_eq!(
                prepare_fixture(&quoted).unwrap_err().code(),
                ErrorCode::Unsupported,
                "{quoted}"
            );
        }
        for data_type in [
            "INT64",
            "DECIMAL",
            "FLOAT64",
            "TEXT",
            "BLOB",
            "DATE",
            "TIMESTAMP",
            "BOOL",
        ] {
            let unquoted = format!("CREATE TABLE UNQUOTED_{data_type} (VALUE {data_type})");
            assert!(prepare_fixture(&unquoted).is_ok(), "{unquoted}");
        }
    }

    #[test]
    fn create_and_drop_bind_real_catalog_object_identities() {
        // Production break caught: DDL that carries only a name can silently
        // target a same-named replacement object after prepare.
        assert_eq!(
            prepare_fixture("CREATE TABLE USERS (ID INT64)")
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );

        let index =
            prepare_fixture("CREATE UNIQUE INDEX USERS_NAME_IDX ON USERS (NAME, ID)").unwrap();
        let BoundStatementKind::CreateIndex {
            table,
            columns,
            unique,
            ..
        } = &index.kind
        else {
            panic!("expected bound CREATE INDEX")
        };
        assert_eq!(table.table_id.get(), 1);
        assert!(*unique);
        assert_eq!(
            columns
                .iter()
                .map(|column| (column.column_id.get(), column.ordinal))
                .collect::<Vec<_>>(),
            vec![(3, 1), (2, 0)]
        );
        for sql in [
            "CREATE INDEX USERS_BAD_COLUMN ON USERS (MISSING)",
            "CREATE INDEX USERS_DUP_COLUMN ON USERS (ID, ID)",
            "CREATE INDEX MISSING_INDEX ON MISSING (ID)",
            "DROP TABLE MISSING",
            "DROP INDEX USERS_NAME_IDX",
        ] {
            assert_eq!(
                prepare_fixture(sql).unwrap_err().code(),
                ErrorCode::Bind,
                "{sql}"
            );
        }

        let drop = prepare_fixture("DROP TABLE USERS").unwrap();
        assert!(matches!(
            drop.kind,
            BoundStatementKind::DropTable { table_id } if table_id.get() == 1
        ));
    }

    #[test]
    fn drop_index_binds_stable_index_and_owner_schema_identity() {
        // Production break caught: retaining the parsed name instead of the
        // catalog IndexId and owner table can drop a replacement index.
        let fixture = phase_4b_catalog_fixture_with_explicit_index(4096, "IX_USERS_ID");
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let statement = parse_statement("DROP INDEX IX_USERS_ID", &SqlLimits::default()).unwrap();
        let bound = bind_statement(&statement, &catalog, database_uuid).unwrap();

        let BoundStatementKind::DropIndex { index_id, table } = &bound.kind else {
            panic!("expected bound DROP INDEX")
        };
        assert_eq!(*index_id, fixture.index_id());
        assert_eq!(table.table_id, fixture.users_id());
        assert_eq!(bound.dependencies.len(), 1);
        assert_eq!(bound.dependencies[0].database_uuid, database_uuid);
        assert_eq!(bound.dependencies[0].table_id, fixture.users_id());
        assert_eq!(bound.dependencies[0].schema_version, table.schema_version);
        assert!(bound.validate_schema(database_uuid, &catalog).is_ok());
    }

    #[test]
    fn schema_validation_rejects_wrong_uuid_version_recreation_and_created_name_collision() {
        // Production break caught: validating only object names or skipping the
        // database/version checks can execute a prepared statement on a different schema.
        let (statement, database_uuid, fixture, old_catalog) = prepared_users_select();
        assert!(
            statement
                .validate_schema(database_uuid, &old_catalog)
                .is_ok()
        );
        let mut wrong_uuid = database_uuid;
        wrong_uuid[0] ^= 0xff;
        assert_eq!(
            statement
                .validate_schema(wrong_uuid, &old_catalog)
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
        let version_changed = bump_users_schema_version(&fixture.pager()).unwrap();
        assert_eq!(
            statement
                .validate_schema(database_uuid, &version_changed)
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );

        let (statement, database_uuid, fixture, old_catalog) = prepared_users_select();
        assert!(
            statement
                .validate_schema(database_uuid, &old_catalog)
                .is_ok()
        );
        let recreated = drop_and_recreate_users(&fixture.pager()).unwrap();
        assert_eq!(
            statement
                .validate_schema(database_uuid, &recreated)
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );

        let fixture = crate::storage::test_support::phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let create =
            parse_statement("CREATE TABLE OTHER (ID INT64)", &SqlLimits::default()).unwrap();
        let create = bind_statement(&create, &catalog, database_uuid).unwrap();
        let with_other = add_unrelated_table(&pager).unwrap();
        assert_eq!(
            create
                .validate_schema(database_uuid, &with_other)
                .unwrap_err()
                .code(),
            ErrorCode::SchemaChanged
        );
    }

    #[test]
    fn schema_validation_ignores_an_unrelated_real_catalog_change() {
        // Production break caught: treating any catalog generation change as a
        // dependency invalidates unaffected prepared statements unnecessarily.
        let (statement, database_uuid, fixture, old_catalog) = prepared_users_select();
        assert!(
            statement
                .validate_schema(database_uuid, &old_catalog)
                .is_ok()
        );
        let changed = add_unrelated_table(&fixture.pager()).unwrap();
        assert!(statement.validate_schema(database_uuid, &changed).is_ok());
    }

    #[test]
    fn schema_validation_ignores_new_table_check_column_markers() {
        // Production break caught: treating a CREATE TABLE CHECK's local,
        // not-yet-persisted column marker as a catalog identity makes a valid
        // prepared CREATE fail validation before its table can exist.
        let fixture = crate::storage::test_support::phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let ast = parse_statement(
            "CREATE TABLE CHECK_CHILD (ID INT64, CHECK (ID > 0))",
            &SqlLimits::default(),
        )
        .unwrap();
        let statement = bind_statement(&ast, &catalog, database_uuid).unwrap();

        assert!(statement.validate_schema(database_uuid, &catalog).is_ok());
        let changed = add_unrelated_table(&pager).unwrap();
        assert!(statement.validate_schema(database_uuid, &changed).is_ok());
    }

    #[test]
    fn schema_validation_rejects_a_replaced_same_table_column_identity() {
        // Production break caught: retaining only a table version lets a raw
        // same-table column-ID replacement satisfy old query/DML/DDL bindings.
        let fixture = crate::storage::test_support::phase_4b_catalog_fixture(4096);
        let pager = fixture.pager();
        let database_uuid = pager.database_uuid().unwrap();
        let mut snapshot = pager.begin_read().unwrap();
        let catalog = snapshot.catalog_reader().unwrap();
        let sqls = [
            "SELECT ID FROM USERS",
            "INSERT INTO USERS (ID) VALUES (:id)",
            "UPDATE USERS SET NAME = NAME WHERE ID = 1",
            "DELETE FROM USERS WHERE ID = 1",
            "CREATE INDEX USERS_ID_IDX ON USERS (ID)",
        ];
        let statements = sqls
            .iter()
            .map(|sql| {
                let statement = parse_statement(sql, &SqlLimits::default())?;
                bind_statement(&statement, &catalog, database_uuid)
            })
            .collect::<crate::Result<Vec<_>>>()
            .unwrap();
        for statement in &statements {
            assert!(statement.validate_schema(database_uuid, &catalog).is_ok());
        }

        let replaced = replace_users_id_column_id(&pager).unwrap();
        for (sql, statement) in sqls.iter().zip(&statements) {
            assert_eq!(
                statement
                    .validate_schema(database_uuid, &replaced)
                    .unwrap_err()
                    .code(),
                ErrorCode::SchemaChanged,
                "{sql}"
            );
        }
    }
}
