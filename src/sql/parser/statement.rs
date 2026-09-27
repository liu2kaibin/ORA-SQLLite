use sqlparser::ast::{
    Assignment as ParserAssignment, AssignmentTarget, CheckConstraint as ParserCheckConstraint,
    ColumnDef as ParserColumnDefinition, ColumnOption, ColumnOptionDef,
    CreateIndex as ParserCreateIndex, CreateTable as ParserCreateTable, CreateTableOptions,
    DataType, Delete as ParserDelete, ForeignKeyConstraint as ParserForeignKeyConstraint,
    FromTable, HiveDistributionStyle, Ident, IndexColumn, Insert as ParserInsert,
    KeyOrIndexDisplay, NullsDistinctOption, ObjectName, ObjectNamePart, ObjectType,
    PrimaryKeyConstraint as ParserPrimaryKeyConstraint, SetExpr, Spanned, Statement, TableAlias,
    TableConstraint, TableFactor, TableObject, TableWithJoins,
    UniqueConstraint as ParserUniqueConstraint, Update as ParserUpdate,
};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Token, TokenWithSpan};

use super::expr::convert_expr;
use super::query::{convert_query, preflight_unparsed_query_syntax};
use super::source::SourceMap;
use super::tokens::tokenize;
use super::{AstBudget, unquoted_identifier};
use crate::sql::ast::{
    Assignment, AstStatement, ColumnDefinition, Constraint, CreateIndex, CreateTable, Delete,
    DropObject, ForeignKey, Insert, NamedColumns, NotNull, TypeArgument, TypeDeclaration,
    TypeModifier, Update,
};
use crate::sql::{Identifier, SqlLimits, SqlSpan};
use crate::{DbError, ErrorCode, Result};

#[cfg(test)]
std::thread_local! {
    static STATEMENT_SOURCE_MAP_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(in crate::sql) fn parse_statement(sql: &str, limits: &SqlLimits) -> Result<AstStatement> {
    if sql.len() > limits.max_sql_bytes() as usize {
        return Err(DbError::new(
            ErrorCode::Syntax,
            "SQL text exceeds the configured byte limit",
        ));
    }
    #[cfg(test)]
    STATEMENT_SOURCE_MAP_BUILDS.with(|builds| builds.set(builds.get() + 1));
    // The dependency and adapter use recursive frames. A bounded private stack
    // makes the documented 128 (hard maximum 256) independent of caller stack
    // size; no third-party tree ever leaves this worker, including error Drop.
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("sql-parser".to_owned())
            .stack_size(32 * 1024 * 1024)
            .spawn_scoped(scope, || parse_statement_inner(sql, limits))
            .map_err(|error| {
                DbError::new(
                    ErrorCode::Io,
                    format!("cannot create SQL parser worker: {error}"),
                )
            })?;
        match worker.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

fn parse_statement_inner(sql: &str, limits: &SqlLimits) -> Result<AstStatement> {
    let source_map = SourceMap::new(sql)?;
    let tokens = tokenize(sql, limits)?;
    let token_index = StatementTokenIndex::new(&tokens, &source_map)?;
    if let Some(span) = token_index.anonymous_block_span() {
        return Err(unsupported(&source_map, Some(span)));
    }
    let statement_span = token_index.require_exactly_one_statement(&source_map)?;
    let mut depth = 0usize;
    let mut previous_is_period = false;
    for token in &tokens {
        if matches!(token.token, Token::Whitespace(_)) {
            continue;
        }
        match &token.token {
            Token::LParen => {
                depth += 1;
                if depth > usize::from(limits.max_nesting()) {
                    return Err(super::ast_limit_error(
                        source_map.sqlparser_location(token.span.start)?,
                    ));
                }
            }
            Token::RParen => depth = depth.saturating_sub(1),
            Token::Word(word)
                if matches!(
                    word.keyword,
                    Keyword::UNION | Keyword::INTERSECT | Keyword::EXCEPT | Keyword::MINUS
                ) && word.quote_style.is_none()
                    && !previous_is_period =>
            {
                return Err(unsupported(&source_map, source_map.span(token.span).ok()));
            }
            _ => {}
        }
        previous_is_period = token.token == Token::Period;
    }
    if token_index.starts_with_keyword(Keyword::CREATE)
        && token_index.keyword(1) == Keyword::TABLESPACE
    {
        return Err(unsupported(&source_map, Some(token_index.span(1))));
    }
    if let Some(span) = token_index.drop_table_cascade_constraints_span(&source_map) {
        return Err(unsupported(&source_map, Some(span)));
    }

    if token_index.starts_with_keyword(Keyword::SELECT) {
        preflight_unparsed_query_syntax(&tokens, &source_map)?;
    }

    let parser_tokens = token_index.normalize_local_time_zone(&tokens, &source_map)?;
    let dialect = super::dialect::BoundedOracle {
        max_depth: usize::from(limits.max_nesting()),
    };
    let statements = Parser::new(&dialect)
        .with_recursion_limit(limits.max_nesting() as usize)
        .with_tokens_with_locations(parser_tokens)
        .parse_statements()
        .map_err(|error| parser_error(error, &source_map))?;
    let mut statements = statements.into_iter();
    let statement = match (statements.next(), statements.next()) {
        (Some(statement), None) => statement,
        _ => {
            return Err(syntax_error(
                "SQL text must contain exactly one statement",
                0,
                &source_map,
            ));
        }
    };

    let mut budget = AstBudget::new(limits);
    convert_statement(
        statement,
        statement_span,
        &source_map,
        &token_index,
        &mut budget,
        limits,
    )
}

fn parser_error(error: ParserError, source_map: &SourceMap<'_>) -> DbError {
    if matches!(&error, ParserError::ParserError(message) if message == "ORA-SQLLite unsupported infix input")
    {
        return unsupported(source_map, None);
    }
    let (message, upstream_location) = match error {
        ParserError::TokenizerError(message) | ParserError::ParserError(message) => {
            let location = parser_message_location(&message);
            (message, location)
        }
        ParserError::RecursionLimitExceeded => (
            "SQL statement exceeds the configured parser nesting limit".to_owned(),
            None,
        ),
    };
    let error = DbError::new(ErrorCode::Syntax, message);
    let location = upstream_location
        .and_then(|location| source_map.sqlparser_location(location).ok())
        .or_else(|| source_map.location(0).ok());
    match location {
        Some(location) => error.with_sql_location(location),
        None => error,
    }
}

fn parser_message_location(message: &str) -> Option<Location> {
    let (_, location) = message.rsplit_once(" at Line: ")?;
    let (line, column) = location.split_once(", Column: ")?;
    Some(Location::new(line.parse().ok()?, column.parse().ok()?))
}

fn convert_statement(
    statement: Statement,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
    limits: &SqlLimits,
) -> Result<AstStatement> {
    match statement {
        Statement::Query(query) => {
            convert_query(*query, source_map, budget, limits).map(AstStatement::Query)
        }
        Statement::Insert(insert) => with_ast_node(span, source_map, budget, |budget| {
            convert_insert(insert, span, source_map, tokens, budget).map(AstStatement::Insert)
        }),
        Statement::Update(update) => with_ast_node(span, source_map, budget, |budget| {
            convert_update(update, span, source_map, tokens, budget).map(AstStatement::Update)
        }),
        Statement::Delete(delete) => with_ast_node(span, source_map, budget, |budget| {
            convert_delete(delete, span, source_map, tokens, budget).map(AstStatement::Delete)
        }),
        Statement::CreateTable(create) => with_ast_node(span, source_map, budget, |budget| {
            convert_create_table(create, span, source_map, tokens, budget)
                .map(AstStatement::CreateTable)
        }),
        Statement::CreateIndex(create) => with_ast_node(span, source_map, budget, |budget| {
            convert_create_index(create, span, source_map, tokens, budget)
                .map(AstStatement::CreateIndex)
        }),
        Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            restrict,
            purge,
            temporary,
            table,
        } => with_ast_node(span, source_map, budget, |_| {
            convert_drop(
                object_type,
                if_exists,
                names,
                cascade,
                restrict,
                purge,
                temporary,
                table,
                span,
                source_map,
                tokens,
            )
        }),
        Statement::StartTransaction {
            modes,
            begin,
            transaction,
            modifier,
            statements,
            exception,
            has_end_keyword,
        } if begin
            && modes.is_empty()
            && transaction.is_none()
            && modifier.is_none()
            && statements.is_empty()
            && exception.is_none()
            && !has_end_keyword
            && tokens.is_bare_statement(Keyword::BEGIN) =>
        {
            with_ast_node(span, source_map, budget, |_| Ok(AstStatement::Begin(span)))
        }
        Statement::Commit {
            chain: false,
            end: false,
            modifier: None,
        } if tokens.is_bare_statement(Keyword::COMMIT) => {
            with_ast_node(span, source_map, budget, |_| Ok(AstStatement::Commit(span)))
        }
        Statement::Rollback {
            chain: false,
            savepoint: None,
        } if tokens.is_bare_statement(Keyword::ROLLBACK) => {
            with_ast_node(span, source_map, budget, |_| {
                Ok(AstStatement::Rollback(span))
            })
        }
        _ => Err(unsupported(source_map, Some(span))),
    }
}

fn convert_insert(
    insert: ParserInsert,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<Insert> {
    let ParserInsert {
        insert_token: _,
        optimizer_hints,
        or,
        ignore,
        into,
        table,
        table_alias,
        columns,
        overwrite,
        source,
        assignments,
        partitioned,
        after_columns,
        has_table_keyword,
        on,
        returning,
        output,
        replace_into,
        priority,
        insert_alias,
        settings,
        format_clause,
        multi_table_insert_type,
        multi_table_into_clauses,
        multi_table_when_clauses,
        multi_table_else_clause,
    } = insert;

    if !optimizer_hints.is_empty()
        || or.is_some()
        || ignore
        || !into
        || table_alias.is_some()
        || overwrite
        || !assignments.is_empty()
        || partitioned.is_some()
        || !after_columns.is_empty()
        || has_table_keyword
        || on.is_some()
        || returning.is_some()
        || output.is_some()
        || replace_into
        || priority.is_some()
        || insert_alias.is_some()
        || settings.is_some()
        || format_clause.is_some()
        || multi_table_insert_type.is_some()
        || !multi_table_into_clauses.is_empty()
        || !multi_table_when_clauses.is_empty()
        || multi_table_else_clause.is_some()
    {
        return Err(unsupported(
            source_map,
            tokens
                .first_keyword_span(&[Keyword::RETURNING])
                .or(Some(span)),
        ));
    }

    let table = match table {
        TableObject::TableName(name) => convert_single_name(name, source_map)?,
        _ => return Err(unsupported(source_map, Some(span))),
    };
    let mut converted_columns = Vec::with_capacity(columns.len());
    for column in columns {
        converted_columns.push(convert_single_name(column, source_map)?);
    }

    let Some(source) = source else {
        return Err(unsupported(source_map, Some(span)));
    };
    let sqlparser::ast::Query {
        with,
        body,
        order_by,
        limit_clause,
        fetch,
        locks,
        for_clause,
        settings,
        format_clause,
        pipe_operators,
    } = *source;
    if with.is_some()
        || order_by.is_some()
        || limit_clause.is_some()
        || fetch.is_some()
        || !locks.is_empty()
        || for_clause.is_some()
        || settings.is_some()
        || format_clause.is_some()
        || !pipe_operators.is_empty()
    {
        return Err(unsupported(source_map, Some(span)));
    }
    let SetExpr::Values(values) = *body else {
        return Err(unsupported(
            source_map,
            tokens
                .first_keyword_span(&[Keyword::SELECT, Keyword::WITH])
                .or(Some(span)),
        ));
    };
    let sqlparser::ast::Values {
        explicit_row,
        value_keyword,
        rows,
    } = values;
    if explicit_row || value_keyword || rows.len() != 1 {
        let location = rows
            .get(1)
            .and_then(|row| source_map.span(row.opening_token.0.span).ok())
            .or_else(|| tokens.first_keyword_span(&[Keyword::VALUES, Keyword::VALUE]));
        return Err(unsupported(source_map, location.or(Some(span))));
    }
    let mut rows = rows.into_iter();
    let row = rows
        .next()
        .ok_or_else(|| source_shape_error(span.start(), source_map))?;
    let sqlparser::ast::Parens {
        opening_token: _,
        content,
        closing_token: _,
    } = row;
    let mut converted_values = Vec::with_capacity(content.len());
    for value in content {
        converted_values.push(convert_expr(value, source_map, budget)?);
    }

    Ok(Insert {
        table,
        columns: converted_columns,
        values: converted_values,
        span,
    })
}

fn convert_update(
    update: ParserUpdate,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<Update> {
    let ParserUpdate {
        update_token: _,
        optimizer_hints,
        table,
        assignments,
        from,
        selection,
        returning,
        output,
        or,
        order_by,
        limit,
    } = update;
    if !optimizer_hints.is_empty()
        || from.is_some()
        || returning.is_some()
        || output.is_some()
        || or.is_some()
        || !order_by.is_empty()
        || limit.is_some()
    {
        return Err(unsupported(
            source_map,
            tokens
                .first_keyword_span(&[Keyword::FROM, Keyword::RETURNING, Keyword::ORDER])
                .or(Some(span)),
        ));
    }

    let (table, alias) = convert_target_table(table, source_map, tokens)?;
    let mut converted_assignments = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        converted_assignments.push(convert_assignment(assignment, source_map, budget)?);
    }
    let selection = selection
        .map(|expression| convert_expr(expression, source_map, budget))
        .transpose()?;
    Ok(Update {
        table,
        alias,
        assignments: converted_assignments,
        selection,
        span,
    })
}

fn convert_assignment(
    assignment: ParserAssignment,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
) -> Result<Assignment> {
    let ParserAssignment { target, value } = assignment;
    let column = match target {
        AssignmentTarget::ColumnName(name) => convert_single_name(name, source_map)?,
        AssignmentTarget::Tuple(names) => {
            let partial = names
                .first()
                .and_then(|name| source_map.span(name.span()).ok());
            return Err(unsupported(source_map, partial));
        }
    };
    let assignment_anchor = column.span();
    with_ast_node(assignment_anchor, source_map, budget, |budget| {
        let value = convert_expr(value, source_map, budget)?;
        let assignment_span = SqlSpan::new(column.span().start(), value.span().end())?;
        Ok(Assignment {
            column,
            value,
            span: assignment_span,
        })
    })
}

fn convert_delete(
    delete: ParserDelete,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<Delete> {
    let ParserDelete {
        delete_token: _,
        optimizer_hints,
        tables,
        from,
        using,
        selection,
        returning,
        output,
        order_by,
        limit,
    } = delete;
    if !optimizer_hints.is_empty()
        || !tables.is_empty()
        || using.is_some()
        || returning.is_some()
        || output.is_some()
        || !order_by.is_empty()
        || limit.is_some()
    {
        return Err(unsupported(
            source_map,
            tokens
                .first_keyword_span(&[Keyword::USING, Keyword::RETURNING, Keyword::ORDER])
                .or(Some(span)),
        ));
    }
    let FromTable::WithFromKeyword(mut targets) = from else {
        return Err(unsupported(source_map, Some(span)));
    };
    if targets.len() != 1 {
        return Err(unsupported(source_map, Some(span)));
    }
    let target = targets
        .pop()
        .ok_or_else(|| source_shape_error(span.start(), source_map))?;
    let (table, alias) = convert_target_table(target, source_map, tokens)?;
    let selection = selection
        .map(|expression| convert_expr(expression, source_map, budget))
        .transpose()?;
    Ok(Delete {
        table,
        alias,
        selection,
        span,
    })
}

fn convert_target_table(
    target: TableWithJoins,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
) -> Result<(Identifier, Option<Identifier>)> {
    let TableWithJoins { relation, joins } = target;
    let partial = source_map.span(relation.span()).ok();
    if !joins.is_empty() {
        return Err(unsupported(source_map, partial));
    }
    match relation {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_hints,
        } if args.is_none()
            && with_hints.is_empty()
            && version.is_none()
            && !with_ordinality
            && partitions.is_empty()
            && json_path.is_none()
            && sample.is_none()
            && index_hints.is_empty() =>
        {
            let table = convert_single_name(name, source_map)?;
            let alias = alias
                .map(|alias| convert_target_alias(alias, source_map, tokens))
                .transpose()?;
            Ok((table, alias))
        }
        _ => Err(unsupported(source_map, partial)),
    }
}

fn convert_target_alias(
    alias: TableAlias,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
) -> Result<Identifier> {
    let TableAlias {
        explicit,
        name,
        columns,
        at,
    } = alias;
    let name_span = source_map.span(name.span)?;
    if explicit || !columns.is_empty() || at.is_some() {
        return Err(unsupported(
            source_map,
            tokens
                .keyword_span_before(Keyword::AS, name_span.start())
                .or(Some(name_span)),
        ));
    }
    convert_identifier(name, source_map)
}

fn convert_create_table(
    create: ParserCreateTable,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<CreateTable> {
    if !plain_create_table(&create) {
        return Err(unsupported(
            source_map,
            tokens
                .first_keyword_span(&[Keyword::OR, Keyword::IF, Keyword::AS])
                .or(Some(span)),
        ));
    }
    let ParserCreateTable {
        or_replace: _,
        temporary: _,
        external: _,
        dynamic: _,
        global: _,
        if_not_exists: _,
        transient: _,
        volatile: _,
        iceberg: _,
        snapshot: _,
        name,
        columns,
        constraints,
        hive_distribution: _,
        hive_formats: _,
        table_options: _,
        file_format: _,
        location: _,
        query: _,
        without_rowid: _,
        like: _,
        clone: _,
        version: _,
        comment: _,
        on_commit: _,
        on_cluster: _,
        primary_key: _,
        order_by: _,
        partition_by: _,
        cluster_by: _,
        clustered_by: _,
        inherits: _,
        partition_of: _,
        for_values: _,
        strict: _,
        copy_grants: _,
        enable_schema_evolution: _,
        change_tracking: _,
        data_retention_time_in_days: _,
        max_data_extension_time_in_days: _,
        default_ddl_collation: _,
        with_aggregation_policy: _,
        with_row_access_policy: _,
        with_storage_lifecycle_policy: _,
        with_tags: _,
        external_volume: _,
        base_location: _,
        catalog: _,
        catalog_sync: _,
        storage_serialization_policy: _,
        target_lag: _,
        warehouse: _,
        refresh_mode: _,
        initialize: _,
        require_user: _,
        diststyle: _,
        distkey: _,
        sortkey: _,
        backup: _,
    } = create;
    let name = convert_single_name(name, source_map)?;
    let Some(opening) = tokens.first_kind_after(name.span().end(), IndexedTokenKind::LeftParen)
    else {
        return Err(syntax_error(
            "CREATE TABLE requires a parenthesized column list",
            name.span().end(),
            source_map,
        ));
    };
    let closing = tokens
        .matching_right(opening)
        .ok_or_else(|| source_shape_error(name.span().end(), source_map))?;
    if columns.is_empty() {
        return Err(syntax_error(
            "CREATE TABLE requires at least one column",
            tokens.span(closing).start(),
            source_map,
        ));
    }

    let mut converted_columns = Vec::with_capacity(columns.len());
    for column in columns {
        converted_columns.push(convert_column_definition(
            column, opening, closing, source_map, tokens, budget,
        )?);
    }
    let mut converted_constraints = Vec::with_capacity(constraints.len());
    for constraint in constraints {
        converted_constraints.push(convert_table_constraint(
            constraint, opening, closing, source_map, tokens, budget,
        )?);
    }
    Ok(CreateTable {
        name,
        columns: converted_columns,
        constraints: converted_constraints,
        span,
    })
}

fn plain_create_table(create: &ParserCreateTable) -> bool {
    !create.or_replace
        && !create.temporary
        && !create.external
        && !create.dynamic
        && create.global.is_none()
        && !create.if_not_exists
        && !create.transient
        && !create.volatile
        && !create.iceberg
        && !create.snapshot
        && matches!(create.hive_distribution, HiveDistributionStyle::NONE)
        && create.hive_formats.is_none()
        && matches!(create.table_options, CreateTableOptions::None)
        && create.file_format.is_none()
        && create.location.is_none()
        && create.query.is_none()
        && !create.without_rowid
        && create.like.is_none()
        && create.clone.is_none()
        && create.version.is_none()
        && create.comment.is_none()
        && create.on_commit.is_none()
        && create.on_cluster.is_none()
        && create.primary_key.is_none()
        && create.order_by.is_none()
        && create.partition_by.is_none()
        && create.cluster_by.is_none()
        && create.clustered_by.is_none()
        && create.inherits.is_none()
        && create.partition_of.is_none()
        && create.for_values.is_none()
        && !create.strict
        && !create.copy_grants
        && create.enable_schema_evolution.is_none()
        && create.change_tracking.is_none()
        && create.data_retention_time_in_days.is_none()
        && create.max_data_extension_time_in_days.is_none()
        && create.default_ddl_collation.is_none()
        && create.with_aggregation_policy.is_none()
        && create.with_row_access_policy.is_none()
        && create.with_storage_lifecycle_policy.is_none()
        && create.with_tags.is_none()
        && create.external_volume.is_none()
        && create.base_location.is_none()
        && create.catalog.is_none()
        && create.catalog_sync.is_none()
        && create.storage_serialization_policy.is_none()
        && create.target_lag.is_none()
        && create.warehouse.is_none()
        && create.refresh_mode.is_none()
        && create.initialize.is_none()
        && !create.require_user
        && create.diststyle.is_none()
        && create.distkey.is_none()
        && create.sortkey.is_none()
        && create.backup.is_none()
}

fn convert_column_definition(
    column: ParserColumnDefinition,
    table_opening: usize,
    table_closing: usize,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<ColumnDefinition> {
    let ParserColumnDefinition {
        name,
        data_type,
        options,
    } = column;
    let name_span = source_map.span(name.span)?;
    let (segment_start, segment_end) =
        tokens.table_item_bounds(table_opening, table_closing, name_span.start())?;
    let column_span = tokens.range_span(segment_start, segment_end)?;
    with_ast_node(column_span, source_map, budget, |budget| {
        let name = convert_identifier(name, source_map)?;
        if matches!(data_type, DataType::Unspecified) {
            return Err(syntax_error(
                "column data type is required",
                name.span().end(),
                source_map,
            ));
        }
        let type_start = tokens
            .first_index_at_or_after(name.span().end())
            .filter(|index| *index < segment_end)
            .ok_or_else(|| source_shape_error(name.span().end(), source_map))?;
        let (data_type, mut cursor) =
            convert_type_declaration(type_start, segment_end, source_map, tokens, budget)?;
        let mut constraints = Vec::with_capacity(options.len());
        for option in options {
            let (constraint, next) = convert_column_constraint(
                option,
                &name,
                cursor,
                segment_end,
                source_map,
                tokens,
                budget,
            )?;
            constraints.push(constraint);
            cursor = next;
        }
        if cursor != segment_end {
            return Err(unsupported(source_map, Some(tokens.span(cursor))));
        }
        Ok(ColumnDefinition {
            name,
            data_type,
            constraints,
            span: column_span,
        })
    })
}

fn convert_type_declaration(
    start: usize,
    boundary: usize,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<(TypeDeclaration, usize)> {
    let Some(name_token) = tokens.tokens.get(start) else {
        return Err(source_shape_error(source_map.sql().len(), source_map));
    };
    let Token::Word(word) = &name_token.token else {
        return Err(syntax_error(
            "column data type name is required",
            name_token.span.start(),
            source_map,
        ));
    };
    let depth = name_token.depth;
    let mut cursor = start + 1;
    let mut argument_bounds = None;
    let mut end = name_token.span.end();
    if cursor < boundary && tokens.kind(cursor) == IndexedTokenKind::LeftParen {
        let opening = cursor;
        let closing = tokens
            .matching_right(opening)
            .filter(|closing| *closing < boundary)
            .ok_or_else(|| source_shape_error(tokens.span(opening).start(), source_map))?;
        argument_bounds = Some((opening, closing));
        end = tokens.span(closing).end();
        cursor = closing + 1;
    }

    let mut modifier = None;
    if cursor < boundary && tokens.keyword(cursor) == Keyword::WITH {
        let modifier_start = cursor;
        let converted = if tokens.keyword(cursor + 1) == Keyword::LOCAL
            && tokens.keyword(cursor + 2) == Keyword::TIME
            && tokens.keyword(cursor + 3) == Keyword::ZONE
            && tokens.same_depth_range(cursor, cursor + 4, depth)
        {
            cursor += 4;
            TypeModifier::WithLocalTimeZone(SqlSpan::new(
                tokens.span(modifier_start).start(),
                tokens.span(cursor - 1).end(),
            )?)
        } else if tokens.keyword(cursor + 1) == Keyword::TIME
            && tokens.keyword(cursor + 2) == Keyword::ZONE
            && tokens.same_depth_range(cursor, cursor + 3, depth)
        {
            cursor += 3;
            TypeModifier::WithTimeZone(SqlSpan::new(
                tokens.span(modifier_start).start(),
                tokens.span(cursor - 1).end(),
            )?)
        } else {
            return Err(unsupported(source_map, Some(tokens.span(cursor))));
        };
        end = converted.span().end();
        modifier = Some(converted);
    }
    let type_span = SqlSpan::new(name_token.span.start(), end)?;
    with_ast_node(type_span, source_map, budget, |budget| {
        let name = identifier_from_word(word, name_token.span, source_map)?;
        let arguments = argument_bounds
            .map(|(opening, closing)| tokens.type_arguments(opening, closing, source_map, budget))
            .transpose()?
            .unwrap_or_default();
        let mut modifiers = Vec::new();
        if let Some(modifier) = modifier {
            let modifier = with_ast_node(modifier.span(), source_map, budget, |_| Ok(modifier))?;
            modifiers.push(modifier);
        }
        Ok((
            TypeDeclaration {
                name,
                arguments,
                modifiers,
                span: type_span,
            },
            cursor,
        ))
    })
}

fn convert_column_constraint(
    option: ColumnOptionDef,
    column: &Identifier,
    start: usize,
    boundary: usize,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<(Constraint, usize)> {
    if start >= boundary {
        return Err(source_shape_error(column.span().end(), source_map));
    }
    let ColumnOptionDef { name, option } = option;
    let mut cursor = start;
    let name = if let Some(name) = name {
        tokens.expect_keyword(cursor, Keyword::CONSTRAINT, source_map)?;
        cursor += 1;
        let name_span = source_map.span(name.span)?;
        if tokens.span(cursor) != name_span {
            return Err(source_shape_error(name_span.start(), source_map));
        }
        cursor += 1;
        Some(convert_identifier(name, source_map)?)
    } else {
        None
    };

    let (end, constraint) = match option {
        ColumnOption::PrimaryKey(constraint) if simple_column_primary_key(&constraint) => {
            tokens.expect_keyword(cursor, Keyword::PRIMARY, source_map)?;
            tokens.expect_keyword(cursor + 1, Keyword::KEY, source_map)?;
            let end = cursor + 2;
            let span = SqlSpan::new(tokens.span(start).start(), tokens.span(end - 1).end())?;
            (
                end,
                Constraint::PrimaryKey(NamedColumns {
                    name,
                    columns: vec![column.clone()],
                    span,
                }),
            )
        }
        ColumnOption::Unique(constraint) if simple_column_unique(&constraint) => {
            tokens.expect_keyword(cursor, Keyword::UNIQUE, source_map)?;
            let end = cursor + 1;
            let span = SqlSpan::new(tokens.span(start).start(), tokens.span(end - 1).end())?;
            (
                end,
                Constraint::Unique(NamedColumns {
                    name,
                    columns: vec![column.clone()],
                    span,
                }),
            )
        }
        ColumnOption::NotNull => {
            tokens.expect_keyword(cursor, Keyword::NOT, source_map)?;
            tokens.expect_keyword(cursor + 1, Keyword::NULL, source_map)?;
            let end = cursor + 2;
            let span = SqlSpan::new(tokens.span(start).start(), tokens.span(end - 1).end())?;
            (
                end,
                Constraint::NotNull(NotNull {
                    name,
                    column: column.clone(),
                    span,
                }),
            )
        }
        ColumnOption::Check(ParserCheckConstraint {
            name: inner_name,
            expr,
            enforced,
        }) if inner_name.is_none() && enforced.is_none() => {
            tokens.expect_keyword(cursor, Keyword::CHECK, source_map)?;
            tokens.expect_kind(cursor + 1, IndexedTokenKind::LeftParen, source_map)?;
            let closing = tokens
                .matching_right(cursor + 1)
                .filter(|closing| *closing < boundary)
                .ok_or_else(|| source_shape_error(tokens.span(cursor).start(), source_map))?;
            let end = closing + 1;
            let span = SqlSpan::new(tokens.span(start).start(), tokens.span(closing).end())?;
            return with_ast_node(span, source_map, budget, |budget| {
                Ok((
                    Constraint::Check {
                        name,
                        expression: convert_expr(*expr, source_map, budget)?,
                        span,
                    },
                    end,
                ))
            });
        }
        ColumnOption::ForeignKey(constraint) if simple_column_foreign_key(&constraint) => {
            tokens.expect_keyword(cursor, Keyword::REFERENCES, source_map)?;
            let ParserForeignKeyConstraint {
                name: _,
                index_name: _,
                columns: _,
                foreign_table,
                referred_columns,
                on_delete: _,
                on_update: _,
                match_kind: _,
                characteristics: _,
            } = constraint;
            let table_span = source_map.span(foreign_table.span())?;
            if tokens.span(cursor + 1).start() != table_span.start() {
                return Err(source_shape_error(table_span.start(), source_map));
            }
            let referenced_table = convert_single_name(foreign_table, source_map)?;
            if referred_columns.is_empty() {
                return Err(unsupported(source_map, Some(table_span)));
            }
            let opening = tokens
                .first_kind_after(referenced_table.span().end(), IndexedTokenKind::LeftParen)
                .filter(|opening| *opening < boundary)
                .ok_or_else(|| source_shape_error(referenced_table.span().end(), source_map))?;
            let closing = tokens
                .matching_right(opening)
                .filter(|closing| *closing < boundary)
                .ok_or_else(|| source_shape_error(tokens.span(opening).start(), source_map))?;
            let referenced_columns = convert_identifier_list(referred_columns, source_map)?;
            let end = closing + 1;
            let span = SqlSpan::new(tokens.span(start).start(), tokens.span(closing).end())?;
            (
                end,
                Constraint::ForeignKey(ForeignKey {
                    name,
                    columns: vec![column.clone()],
                    referenced_table,
                    referenced_columns,
                    span,
                }),
            )
        }
        _ => return Err(unsupported(source_map, Some(tokens.span(start)))),
    };
    with_ast_node(constraint.span(), source_map, budget, |_| {
        Ok((constraint, end))
    })
}

fn simple_column_primary_key(constraint: &ParserPrimaryKeyConstraint) -> bool {
    let ParserPrimaryKeyConstraint {
        name,
        index_name,
        index_type,
        columns,
        index_options,
        characteristics,
    } = constraint;
    name.is_none()
        && index_name.is_none()
        && index_type.is_none()
        && columns.is_empty()
        && index_options.is_empty()
        && characteristics.is_none()
}

fn simple_column_unique(constraint: &ParserUniqueConstraint) -> bool {
    let ParserUniqueConstraint {
        name,
        index_name,
        index_type_display,
        index_type,
        columns,
        index_options,
        characteristics,
        nulls_distinct,
    } = constraint;
    name.is_none()
        && index_name.is_none()
        && matches!(index_type_display, KeyOrIndexDisplay::None)
        && index_type.is_none()
        && columns.is_empty()
        && index_options.is_empty()
        && characteristics.is_none()
        && matches!(nulls_distinct, NullsDistinctOption::None)
}

fn simple_column_foreign_key(constraint: &ParserForeignKeyConstraint) -> bool {
    let ParserForeignKeyConstraint {
        name,
        index_name,
        columns,
        foreign_table: _,
        referred_columns: _,
        on_delete,
        on_update,
        match_kind,
        characteristics,
    } = constraint;
    name.is_none()
        && index_name.is_none()
        && columns.is_empty()
        && on_delete.is_none()
        && on_update.is_none()
        && match_kind.is_none()
        && characteristics.is_none()
}

fn convert_table_constraint(
    constraint: TableConstraint,
    table_opening: usize,
    table_closing: usize,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    budget: &mut AstBudget,
) -> Result<Constraint> {
    if !matches!(
        &constraint,
        TableConstraint::PrimaryKey(_)
            | TableConstraint::Unique(_)
            | TableConstraint::Check(_)
            | TableConstraint::ForeignKey(_)
    ) {
        return Err(unsupported(
            source_map,
            source_map.span(constraint.span()).ok(),
        ));
    }
    let anchor = table_constraint_anchor(&constraint, source_map)?;
    let (start, end) = tokens.table_item_bounds(table_opening, table_closing, anchor)?;
    let constraint_span = tokens.range_span(start, end)?;
    with_ast_node(
        constraint_span,
        source_map,
        budget,
        |budget| match constraint {
            TableConstraint::PrimaryKey(constraint) => {
                let ParserPrimaryKeyConstraint {
                    name,
                    index_name,
                    index_type,
                    columns,
                    index_options,
                    characteristics,
                } = constraint;
                if index_name.is_some()
                    || index_type.is_some()
                    || columns.is_empty()
                    || !index_options.is_empty()
                    || characteristics.is_some()
                {
                    return Err(unsupported(source_map, Some(constraint_span)));
                }
                let (name, mut cursor) =
                    convert_table_constraint_name(name, start, source_map, tokens)?;
                tokens.expect_keyword(cursor, Keyword::PRIMARY, source_map)?;
                tokens.expect_keyword(cursor + 1, Keyword::KEY, source_map)?;
                cursor += 2;
                let closing = require_parenthesized_tail(cursor, end, source_map, tokens)?;
                if closing + 1 != end {
                    return Err(unsupported(source_map, Some(tokens.span(closing + 1))));
                }
                Ok(Constraint::PrimaryKey(NamedColumns {
                    name,
                    columns: convert_index_columns(columns, source_map)?,
                    span: constraint_span,
                }))
            }
            TableConstraint::Unique(constraint) => {
                let ParserUniqueConstraint {
                    name,
                    index_name,
                    index_type_display,
                    index_type,
                    columns,
                    index_options,
                    characteristics,
                    nulls_distinct,
                } = constraint;
                if index_name.is_some()
                    || !matches!(index_type_display, KeyOrIndexDisplay::None)
                    || index_type.is_some()
                    || columns.is_empty()
                    || !index_options.is_empty()
                    || characteristics.is_some()
                    || !matches!(nulls_distinct, NullsDistinctOption::None)
                {
                    return Err(unsupported(source_map, Some(constraint_span)));
                }
                let (name, cursor) =
                    convert_table_constraint_name(name, start, source_map, tokens)?;
                tokens.expect_keyword(cursor, Keyword::UNIQUE, source_map)?;
                let closing = require_parenthesized_tail(cursor + 1, end, source_map, tokens)?;
                if closing + 1 != end {
                    return Err(unsupported(source_map, Some(tokens.span(closing + 1))));
                }
                Ok(Constraint::Unique(NamedColumns {
                    name,
                    columns: convert_index_columns(columns, source_map)?,
                    span: constraint_span,
                }))
            }
            TableConstraint::Check(ParserCheckConstraint {
                name,
                expr,
                enforced,
            }) if enforced.is_none() => {
                let (name, cursor) =
                    convert_table_constraint_name(name, start, source_map, tokens)?;
                tokens.expect_keyword(cursor, Keyword::CHECK, source_map)?;
                let closing = require_parenthesized_tail(cursor + 1, end, source_map, tokens)?;
                if closing + 1 != end {
                    return Err(unsupported(source_map, Some(tokens.span(closing + 1))));
                }
                Ok(Constraint::Check {
                    name,
                    expression: convert_expr(*expr, source_map, budget)?,
                    span: constraint_span,
                })
            }
            TableConstraint::ForeignKey(constraint) => {
                let ParserForeignKeyConstraint {
                    name,
                    index_name,
                    columns,
                    foreign_table,
                    referred_columns,
                    on_delete,
                    on_update,
                    match_kind,
                    characteristics,
                } = constraint;
                if index_name.is_some()
                    || columns.is_empty()
                    || referred_columns.is_empty()
                    || on_delete.is_some()
                    || on_update.is_some()
                    || match_kind.is_some()
                    || characteristics.is_some()
                {
                    return Err(unsupported(source_map, Some(constraint_span)));
                }
                let (name, mut cursor) =
                    convert_table_constraint_name(name, start, source_map, tokens)?;
                tokens.expect_keyword(cursor, Keyword::FOREIGN, source_map)?;
                tokens.expect_keyword(cursor + 1, Keyword::KEY, source_map)?;
                cursor += 2;
                let local_closing = require_parenthesized(cursor, end, source_map, tokens)?;
                cursor = local_closing + 1;
                tokens.expect_keyword(cursor, Keyword::REFERENCES, source_map)?;
                cursor += 1;
                let table_span = source_map.span(foreign_table.span())?;
                if tokens.span(cursor).start() != table_span.start() {
                    return Err(source_shape_error(table_span.start(), source_map));
                }
                let referenced_table = convert_single_name(foreign_table, source_map)?;
                cursor = tokens
                    .first_index_at_or_after(referenced_table.span().end())
                    .ok_or_else(|| source_shape_error(referenced_table.span().end(), source_map))?;
                let referenced_closing = require_parenthesized(cursor, end, source_map, tokens)?;
                if referenced_closing + 1 != end {
                    return Err(unsupported(
                        source_map,
                        Some(tokens.span(referenced_closing + 1)),
                    ));
                }
                Ok(Constraint::ForeignKey(ForeignKey {
                    name,
                    columns: convert_identifier_list(columns, source_map)?,
                    referenced_table,
                    referenced_columns: convert_identifier_list(referred_columns, source_map)?,
                    span: constraint_span,
                }))
            }
            _ => Err(unsupported(source_map, Some(constraint_span))),
        },
    )
}

fn table_constraint_anchor(
    constraint: &TableConstraint,
    source_map: &SourceMap<'_>,
) -> Result<usize> {
    let span = match constraint {
        TableConstraint::PrimaryKey(constraint) => constraint
            .name
            .as_ref()
            .map(|name| name.span)
            .or_else(|| constraint.columns.first().map(Spanned::span)),
        TableConstraint::Unique(constraint) => constraint
            .name
            .as_ref()
            .map(|name| name.span)
            .or_else(|| constraint.columns.first().map(Spanned::span)),
        TableConstraint::Check(constraint) => constraint
            .name
            .as_ref()
            .map(|name| name.span)
            .or_else(|| Some(constraint.expr.span())),
        TableConstraint::ForeignKey(constraint) => constraint
            .name
            .as_ref()
            .map(|name| name.span)
            .or_else(|| constraint.columns.first().map(|column| column.span)),
        _ => None,
    }
    .ok_or_else(|| source_shape_error(0, source_map))?;
    Ok(source_map.span(span)?.start())
}

fn convert_table_constraint_name(
    name: Option<Ident>,
    start: usize,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
) -> Result<(Option<Identifier>, usize)> {
    match name {
        Some(name) => {
            tokens.expect_keyword(start, Keyword::CONSTRAINT, source_map)?;
            let name_span = source_map.span(name.span)?;
            if tokens.span(start + 1) != name_span {
                return Err(source_shape_error(name_span.start(), source_map));
            }
            Ok((Some(convert_identifier(name, source_map)?), start + 2))
        }
        None => Ok((None, start)),
    }
}

fn require_parenthesized_tail(
    opening: usize,
    boundary: usize,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
) -> Result<usize> {
    require_parenthesized(opening, boundary, source_map, tokens)
}

fn require_parenthesized(
    opening: usize,
    boundary: usize,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
) -> Result<usize> {
    tokens.expect_kind(opening, IndexedTokenKind::LeftParen, source_map)?;
    tokens
        .matching_right(opening)
        .filter(|closing| *closing < boundary)
        .ok_or_else(|| source_shape_error(tokens.span(opening).start(), source_map))
}

fn convert_create_index(
    create: ParserCreateIndex,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
    _budget: &mut AstBudget,
) -> Result<CreateIndex> {
    let ParserCreateIndex {
        name,
        table_name,
        using,
        columns,
        unique,
        concurrently,
        if_not_exists,
        include,
        nulls_distinct,
        with,
        predicate,
        index_options,
        alter_options,
    } = create;
    if using.is_some()
        || concurrently
        || if_not_exists
        || !include.is_empty()
        || nulls_distinct.is_some()
        || !with.is_empty()
        || predicate.is_some()
        || !index_options.is_empty()
        || !alter_options.is_empty()
    {
        return Err(unsupported(
            source_map,
            tokens.first_keyword_span(&[Keyword::IF]).or(Some(span)),
        ));
    }
    let Some(name) = name else {
        let offset = tokens
            .first_keyword_span(&[Keyword::ON])
            .map_or(span.start(), |keyword| keyword.start());
        return Err(syntax_error(
            "CREATE INDEX requires an index name",
            offset,
            source_map,
        ));
    };
    if columns.is_empty() {
        return Err(syntax_error(
            "CREATE INDEX requires at least one column",
            span.end(),
            source_map,
        ));
    }
    Ok(CreateIndex {
        name: convert_single_name(name, source_map)?,
        table: convert_single_name(table_name, source_map)?,
        columns: convert_index_columns(columns, source_map)?,
        unique,
        span,
    })
}

fn convert_index_columns(
    columns: Vec<IndexColumn>,
    source_map: &SourceMap<'_>,
) -> Result<Vec<Identifier>> {
    let mut converted = Vec::with_capacity(columns.len());
    for column in columns {
        let IndexColumn {
            column,
            operator_class,
        } = column;
        let partial = source_map.span(column.expr.span()).ok();
        if operator_class.is_some()
            || column.options.asc.is_some()
            || column.options.nulls_first.is_some()
            || column.with_fill.is_some()
        {
            return Err(unsupported(source_map, partial));
        }
        let sqlparser::ast::Expr::Identifier(identifier) = column.expr else {
            return Err(unsupported(source_map, partial));
        };
        converted.push(convert_identifier(identifier, source_map)?);
    }
    Ok(converted)
}

#[allow(clippy::too_many_arguments)]
fn convert_drop(
    object_type: ObjectType,
    if_exists: bool,
    names: Vec<ObjectName>,
    cascade: bool,
    restrict: bool,
    purge: bool,
    temporary: bool,
    table: Option<ObjectName>,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    tokens: &StatementTokenIndex,
) -> Result<AstStatement> {
    if if_exists || names.len() != 1 || cascade || restrict || purge || temporary || table.is_some()
    {
        return Err(unsupported(
            source_map,
            tokens.first_keyword_span(&[Keyword::IF]).or(Some(span)),
        ));
    }
    let mut names = names.into_iter();
    let name = convert_single_name(
        names
            .next()
            .ok_or_else(|| source_shape_error(span.start(), source_map))?,
        source_map,
    )?;
    let drop = DropObject { name, span };
    match object_type {
        ObjectType::Table => Ok(AstStatement::DropTable(drop)),
        ObjectType::Index => Ok(AstStatement::DropIndex(drop)),
        _ => Err(unsupported(source_map, Some(span))),
    }
}

fn convert_identifier_list(
    identifiers: Vec<Ident>,
    source_map: &SourceMap<'_>,
) -> Result<Vec<Identifier>> {
    let mut converted = Vec::with_capacity(identifiers.len());
    for identifier in identifiers {
        converted.push(convert_identifier(identifier, source_map)?);
    }
    Ok(converted)
}

fn convert_single_name(name: ObjectName, source_map: &SourceMap<'_>) -> Result<Identifier> {
    let partial = source_map.span(name.span()).ok();
    let mut parts = name.0.into_iter();
    match (parts.next(), parts.next()) {
        (Some(ObjectNamePart::Identifier(identifier)), None) => {
            convert_identifier(identifier, source_map)
        }
        _ => Err(unsupported(source_map, partial)),
    }
}

fn convert_identifier(identifier: Ident, source_map: &SourceMap<'_>) -> Result<Identifier> {
    let span = source_map.span(identifier.span)?;
    match identifier.quote_style {
        None => unquoted_identifier(identifier.value, span, source_map),
        Some('"') => Identifier::quoted(identifier.value, span),
        _ => Err(unsupported(source_map, Some(span))),
    }
}

fn identifier_from_word(
    word: &sqlparser::tokenizer::Word,
    span: SqlSpan,
    source_map: &SourceMap<'_>,
) -> Result<Identifier> {
    match word.quote_style {
        None => unquoted_identifier(&word.value, span, source_map),
        Some('"') => Identifier::quoted(word.value.clone(), span),
        _ => Err(unsupported(source_map, Some(span))),
    }
}

fn with_ast_node<T>(
    span: SqlSpan,
    source_map: &SourceMap<'_>,
    budget: &mut AstBudget,
    convert: impl FnOnce(&mut AstBudget) -> Result<T>,
) -> Result<T> {
    let location = source_map
        .location(span.start())
        .or_else(|_| source_map.location(0))?;
    budget.enter(location)?;
    let result = convert(budget);
    budget.leave();
    result
}

fn syntax_error(message: &str, offset: usize, source_map: &SourceMap<'_>) -> DbError {
    let error = DbError::new(ErrorCode::Syntax, message);
    match source_map
        .location(offset)
        .or_else(|_| source_map.location(0))
    {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

fn unsupported(source_map: &SourceMap<'_>, span: Option<SqlSpan>) -> DbError {
    let offset = span.map_or(0, |span| span.start());
    let error = DbError::new(ErrorCode::Unsupported, "SQL statement is not supported");
    match source_map
        .location(offset)
        .or_else(|_| source_map.location(0))
    {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

fn source_shape_error(offset: usize, source_map: &SourceMap<'_>) -> DbError {
    let error = DbError::new(
        ErrorCode::Internal,
        "parsed SQL statement does not match its source tokens",
    );
    match source_map
        .location(offset)
        .or_else(|_| source_map.location(0))
    {
        Ok(location) => error.with_sql_location(location),
        Err(_) => error,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexedTokenKind {
    LeftParen,
    RightParen,
    Comma,
    Semicolon,
    Other,
}

#[derive(Debug)]
struct IndexedToken {
    token: Token,
    keyword: Keyword,
    kind: IndexedTokenKind,
    depth: usize,
    span: SqlSpan,
    matching_right: Option<usize>,
}

#[derive(Debug)]
struct StatementTokenIndex {
    tokens: Vec<IndexedToken>,
}

impl StatementTokenIndex {
    fn new(tokens: &[TokenWithSpan], source_map: &SourceMap<'_>) -> Result<Self> {
        let mut indexed = Vec::<IndexedToken>::new();
        let mut depth = 0usize;
        let mut openings = Vec::<usize>::new();
        for token in tokens {
            if matches!(token.token, Token::Whitespace(_)) {
                continue;
            }
            if matches!(token.token, Token::RParen) {
                depth = depth.saturating_sub(1);
            }
            let keyword = match &token.token {
                Token::Word(word) => word.keyword,
                _ => Keyword::NoKeyword,
            };
            let kind = match token.token {
                Token::LParen => IndexedTokenKind::LeftParen,
                Token::RParen => IndexedTokenKind::RightParen,
                Token::Comma => IndexedTokenKind::Comma,
                Token::SemiColon => IndexedTokenKind::Semicolon,
                _ => IndexedTokenKind::Other,
            };
            let index = indexed.len();
            indexed.push(IndexedToken {
                token: token.token.clone(),
                keyword,
                kind,
                depth,
                span: source_map.span(token.span)?,
                matching_right: None,
            });
            if matches!(token.token, Token::RParen) {
                if let Some(opening) = openings.pop() {
                    indexed[opening].matching_right = Some(index);
                }
            }
            if matches!(token.token, Token::LParen) {
                openings.push(index);
                depth = depth.saturating_add(1);
            }
        }
        Ok(Self { tokens: indexed })
    }

    fn require_exactly_one_statement(&self, source_map: &SourceMap<'_>) -> Result<SqlSpan> {
        if self.tokens.is_empty() {
            return Err(syntax_error(
                "SQL text must contain exactly one statement",
                0,
                source_map,
            ));
        }
        let semicolons = self
            .tokens
            .iter()
            .enumerate()
            .filter_map(|(index, token)| {
                (token.kind == IndexedTokenKind::Semicolon).then_some(index)
            })
            .collect::<Vec<_>>();
        let body_end = match semicolons.as_slice() {
            [] => self.tokens.len(),
            [semicolon] if *semicolon == self.tokens.len() - 1 && *semicolon != 0 => *semicolon,
            [semicolon] => {
                return Err(syntax_error(
                    "SQL text must contain exactly one statement",
                    self.span(*semicolon).start(),
                    source_map,
                ));
            }
            [_, repeated, ..] => {
                return Err(syntax_error(
                    "SQL text may contain only one trailing semicolon",
                    self.span(*repeated).start(),
                    source_map,
                ));
            }
        };
        SqlSpan::new(
            self.tokens[0].span.start(),
            self.tokens[body_end - 1].span.end(),
        )
    }

    fn anonymous_block_span(&self) -> Option<SqlSpan> {
        let [begin, body @ .., end, trailing_semicolon] = self.tokens.as_slice() else {
            return None;
        };
        let first_body = body.first()?;
        if begin.keyword != Keyword::BEGIN
            || end.keyword != Keyword::END
            || trailing_semicolon.kind != IndexedTokenKind::Semicolon
            || first_body.kind == IndexedTokenKind::Semicolon
            || body.last()?.kind != IndexedTokenKind::Semicolon
            || matches!(
                first_body.keyword,
                Keyword::TRANSACTION
                    | Keyword::WORK
                    | Keyword::TRAN
                    | Keyword::DEFERRED
                    | Keyword::IMMEDIATE
                    | Keyword::EXCLUSIVE
                    | Keyword::TRY
                    | Keyword::CATCH
                    | Keyword::ISOLATION
                    | Keyword::READ
            )
        {
            return None;
        }
        SqlSpan::new(begin.span.start(), trailing_semicolon.span.end()).ok()
    }

    fn drop_table_cascade_constraints_span(&self, source_map: &SourceMap<'_>) -> Option<SqlSpan> {
        let body_len = self.tokens.len().checked_sub(usize::from(
            self.tokens
                .last()
                .is_some_and(|token| token.kind == IndexedTokenKind::Semicolon),
        ))?;
        let [drop, table, name, cascade, constraints] = self.tokens.get(..body_len)? else {
            return None;
        };
        let Token::Word(name_word) = &name.token else {
            return None;
        };
        let Token::Word(constraints_word) = &constraints.token else {
            return None;
        };
        (drop.keyword == Keyword::DROP
            && table.keyword == Keyword::TABLE
            && identifier_from_word(name_word, name.span, source_map).is_ok()
            && cascade.keyword == Keyword::CASCADE
            && constraints_word.quote_style.is_none()
            && constraints_word.value.eq_ignore_ascii_case("CONSTRAINTS"))
        .then_some(cascade.span)
    }

    fn normalize_local_time_zone(
        &self,
        tokens: &[TokenWithSpan],
        source_map: &SourceMap<'_>,
    ) -> Result<Vec<TokenWithSpan>> {
        let mut local_offsets = self
            .tokens
            .windows(4)
            .filter_map(|window| {
                (window[0].keyword == Keyword::WITH
                    && window[1].keyword == Keyword::LOCAL
                    && window[2].keyword == Keyword::TIME
                    && window[3].keyword == Keyword::ZONE
                    && window.iter().all(|token| token.depth == window[0].depth))
                .then_some(window[1].span.start())
            })
            .collect::<Vec<_>>();
        local_offsets.sort_unstable();
        let mut normalized = Vec::with_capacity(tokens.len());
        for token in tokens {
            let span = source_map.span(token.span)?;
            if local_offsets.binary_search(&span.start()).is_err() {
                normalized.push(token.clone());
            }
        }
        Ok(normalized)
    }

    fn starts_with_keyword(&self, keyword: Keyword) -> bool {
        self.tokens
            .first()
            .is_some_and(|token| token.keyword == keyword)
    }

    fn is_bare_statement(&self, keyword: Keyword) -> bool {
        let body_len = self.tokens.len()
            - usize::from(
                self.tokens
                    .last()
                    .is_some_and(|token| token.kind == IndexedTokenKind::Semicolon),
            );
        body_len == 1 && self.tokens[0].keyword == keyword
    }

    fn span(&self, index: usize) -> SqlSpan {
        self.tokens.get(index).map_or_else(
            || SqlSpan::new(0, 0).expect("zero span is valid"),
            |token| token.span,
        )
    }

    fn kind(&self, index: usize) -> IndexedTokenKind {
        self.tokens
            .get(index)
            .map_or(IndexedTokenKind::Other, |token| token.kind)
    }

    fn keyword(&self, index: usize) -> Keyword {
        self.tokens
            .get(index)
            .map_or(Keyword::NoKeyword, |token| token.keyword)
    }

    fn matching_right(&self, opening: usize) -> Option<usize> {
        self.tokens.get(opening)?.matching_right
    }

    fn first_index_at_or_after(&self, offset: usize) -> Option<usize> {
        let index = self
            .tokens
            .partition_point(|token| token.span.start() < offset);
        (index < self.tokens.len()).then_some(index)
    }

    fn first_kind_after(&self, offset: usize, kind: IndexedTokenKind) -> Option<usize> {
        let start = self.first_index_at_or_after(offset)?;
        self.tokens[start..]
            .iter()
            .position(|token| token.kind == kind)
            .map(|position| start + position)
    }

    fn first_keyword_span(&self, keywords: &[Keyword]) -> Option<SqlSpan> {
        self.tokens
            .iter()
            .find(|token| keywords.contains(&token.keyword))
            .map(|token| token.span)
    }

    fn keyword_span_before(&self, keyword: Keyword, boundary: usize) -> Option<SqlSpan> {
        self.tokens
            .iter()
            .rev()
            .find(|token| token.keyword == keyword && token.span.end() <= boundary)
            .map(|token| token.span)
    }

    fn same_depth_range(&self, start: usize, end: usize, depth: usize) -> bool {
        self.tokens
            .get(start..end)
            .is_some_and(|tokens| tokens.iter().all(|token| token.depth == depth))
    }

    fn table_item_bounds(
        &self,
        table_opening: usize,
        table_closing: usize,
        anchor: usize,
    ) -> Result<(usize, usize)> {
        let content_depth = self
            .tokens
            .get(table_opening)
            .ok_or_else(|| DbError::new(ErrorCode::Internal, "missing table opening token"))?
            .depth
            .saturating_add(1);
        let anchor_index = self
            .first_index_at_or_after(anchor)
            .ok_or_else(|| DbError::new(ErrorCode::Internal, "missing table item anchor"))?;
        let start = (table_opening + 1..anchor_index)
            .rev()
            .find(|index| {
                self.tokens[*index].kind == IndexedTokenKind::Comma
                    && self.tokens[*index].depth == content_depth
            })
            .map_or(table_opening + 1, |comma| comma + 1);
        let end = (anchor_index..table_closing)
            .find(|index| {
                self.tokens[*index].kind == IndexedTokenKind::Comma
                    && self.tokens[*index].depth == content_depth
            })
            .unwrap_or(table_closing);
        if start >= end {
            return Err(DbError::new(
                ErrorCode::Internal,
                "empty table item token range",
            ));
        }
        Ok((start, end))
    }

    fn range_span(&self, start: usize, end: usize) -> Result<SqlSpan> {
        let first = self
            .tokens
            .get(start)
            .ok_or_else(|| DbError::new(ErrorCode::Internal, "missing range start token"))?;
        let last = self
            .tokens
            .get(end.saturating_sub(1))
            .ok_or_else(|| DbError::new(ErrorCode::Internal, "missing range end token"))?;
        SqlSpan::new(first.span.start(), last.span.end())
    }

    fn type_arguments(
        &self,
        opening: usize,
        closing: usize,
        source_map: &SourceMap<'_>,
        budget: &mut AstBudget,
    ) -> Result<Vec<TypeArgument>> {
        let argument_depth = self.tokens[opening].depth.saturating_add(1);
        let mut arguments = Vec::new();
        let mut start = opening + 1;
        for boundary in (opening + 1..=closing).filter(|index| {
            *index == closing
                || (self.tokens[*index].kind == IndexedTokenKind::Comma
                    && self.tokens[*index].depth == argument_depth)
        }) {
            if start >= boundary {
                return Err(syntax_error(
                    "type arguments must not be empty",
                    self.span(boundary).start(),
                    source_map,
                ));
            }
            let span = self.range_span(start, boundary)?;
            let argument = with_ast_node(span, source_map, budget, |_| {
                Ok(TypeArgument {
                    text: source_map.sql()[span.start()..span.end()].trim().to_owned(),
                    span,
                })
            })?;
            arguments.push(argument);
            start = boundary + 1;
        }
        Ok(arguments)
    }

    fn expect_keyword(
        &self,
        index: usize,
        keyword: Keyword,
        source_map: &SourceMap<'_>,
    ) -> Result<()> {
        if self.keyword(index) == keyword {
            Ok(())
        } else {
            Err(source_shape_error(self.span(index).start(), source_map))
        }
    }

    fn expect_kind(
        &self,
        index: usize,
        kind: IndexedTokenKind,
        source_map: &SourceMap<'_>,
    ) -> Result<()> {
        if self.kind(index) == kind {
            Ok(())
        } else {
            Err(source_shape_error(self.span(index).start(), source_map))
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::ast::{Statement as ParserStatement, TableConstraint as ParserTableConstraint};
    use sqlparser::dialect::MySqlDialect;
    use sqlparser::parser::Parser;

    use super::{
        AstBudget, IndexedTokenKind, STATEMENT_SOURCE_MAP_BUILDS, SourceMap, StatementTokenIndex,
        convert_table_constraint, parse_statement, tokenize,
    };
    use crate::ErrorCode;
    use crate::error::SqlLocation;
    use crate::sql::ast::{AstExprKind, AstStatement, Constraint, TypeModifier};
    use crate::sql::{ParameterKey, SqlLimits, SqlSpan};

    fn authored_span(sql: &str, authored: &str) -> SqlSpan {
        let start = sql.find(authored).expect("authored fragment must exist");
        SqlSpan::new(start, start + authored.len()).unwrap()
    }

    #[test]
    fn sql_byte_limit_precedes_statement_source_mapping() {
        STATEMENT_SOURCE_MAP_BUILDS.with(|builds| builds.set(0));
        let limits = SqlLimits::default().with_max_sql_bytes(5).unwrap();

        let error = parse_statement("SELECT", &limits).unwrap_err();

        assert_eq!(error.code(), ErrorCode::Syntax);
        assert_eq!(error.sql_location(), None);
        STATEMENT_SOURCE_MAP_BUILDS.with(|builds| assert_eq!(builds.get(), 0));
    }

    #[test]
    fn ddl_dml_and_transactions_convert() {
        for sql in [
            "CREATE TABLE T (ID INT64 PRIMARY KEY, NAME TEXT NOT NULL)",
            "CREATE UNIQUE INDEX T_U1 ON T (ID)",
            "DROP TABLE T",
            "DROP INDEX T_U1",
            "INSERT INTO T (ID, NAME) VALUES (:1, :2)",
            "UPDATE T SET NAME = :name WHERE ID = :id",
            "DELETE FROM T WHERE ID = :1",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
        ] {
            assert!(parse_statement(sql, &SqlLimits::default()).is_ok(), "{sql}");
        }
    }

    #[test]
    fn dml_shapes_and_spans_are_preserved() {
        let insert_sql = "INSERT INTO T (ID, NAME) VALUES (:1, :2)";
        let AstStatement::Insert(insert) =
            parse_statement(insert_sql, &SqlLimits::default()).unwrap()
        else {
            panic!("expected INSERT");
        };
        assert_eq!(insert.span, SqlSpan::new(0, insert_sql.len()).unwrap());
        assert_eq!(insert.table.as_str(), "T");
        assert_eq!(insert.table.span(), SqlSpan::new(12, 13).unwrap());
        assert_eq!(
            insert
                .columns
                .iter()
                .map(|column| column.as_str())
                .collect::<Vec<_>>(),
            ["ID", "NAME"]
        );
        assert!(matches!(
            &insert.values[0].kind,
            AstExprKind::Parameter(ParameterKey::Position(position)) if position.get() == 1
        ));
        assert_eq!(insert.values[0].span(), authored_span(insert_sql, ":1"));
        assert_eq!(insert.values[1].span(), authored_span(insert_sql, ":2"));

        let update_sql = "UPDATE T X SET NAME = :name WHERE ID = :id";
        let AstStatement::Update(update) =
            parse_statement(update_sql, &SqlLimits::default()).unwrap()
        else {
            panic!("expected UPDATE");
        };
        assert_eq!(update.span, SqlSpan::new(0, update_sql.len()).unwrap());
        assert_eq!(update.table.as_str(), "T");
        assert_eq!(update.alias.as_ref().map(|alias| alias.as_str()), Some("X"));
        assert_eq!(update.assignments[0].column.as_str(), "NAME");
        assert_eq!(
            update.assignments[0].span,
            authored_span(update_sql, "NAME = :name")
        );
        assert!(matches!(
            &update.assignments[0].value.kind,
            AstExprKind::Parameter(ParameterKey::Name(name)) if name == "NAME"
        ));
        assert_eq!(
            update
                .selection
                .as_ref()
                .map(|expression| expression.span()),
            Some(authored_span(update_sql, "ID = :id"))
        );

        let delete_sql = "DELETE FROM T X WHERE ID = :1";
        let AstStatement::Delete(delete) =
            parse_statement(delete_sql, &SqlLimits::default()).unwrap()
        else {
            panic!("expected DELETE");
        };
        assert_eq!(delete.span, SqlSpan::new(0, delete_sql.len()).unwrap());
        assert_eq!(delete.table.as_str(), "T");
        assert_eq!(delete.alias.as_ref().map(|alias| alias.as_str()), Some("X"));
        assert_eq!(
            delete
                .selection
                .as_ref()
                .map(|expression| expression.span()),
            Some(authored_span(delete_sql, "ID = :1"))
        );
    }

    #[test]
    fn update_assignment_spans_end_at_the_owned_rhs_boundary() {
        for (sql, assignment_text, rhs_text) in [
            ("UPDATE T SET A = B IS NULL", "A = B IS NULL", "B IS NULL"),
            (
                "UPDATE T SET A = B IN (1, 2)",
                "A = B IN (1, 2)",
                "B IN (1, 2)",
            ),
            (
                "UPDATE T SET A = (B + 1) * (C - 2)",
                "A = (B + 1) * (C - 2)",
                "(B + 1) * (C - 2)",
            ),
        ] {
            let AstStatement::Update(update) = parse_statement(sql, &SqlLimits::default()).unwrap()
            else {
                panic!("expected UPDATE");
            };
            assert_eq!(update.assignments.len(), 1, "{sql}");
            assert_eq!(
                update.assignments[0].span,
                authored_span(sql, assignment_text),
                "{sql}"
            );
            assert_eq!(
                update.assignments[0].value.span(),
                authored_span(sql, rhs_text),
                "{sql}"
            );
        }
    }

    #[test]
    fn exactly_one_statement_is_required() {
        for sql in [
            "",
            " -- only a comment\n ",
            ";",
            "SELECT A FROM T;;",
            "SELECT A FROM T; DELETE FROM T",
        ] {
            assert_eq!(
                parse_statement(sql, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Syntax
            );
        }
    }

    #[test]
    fn anonymous_blocks_are_unsupported_without_weakening_statement_counting() {
        assert_eq!(
            parse_statement("BEGIN NULL; END;", &SqlLimits::default())
                .unwrap_err()
                .code(),
            ErrorCode::Unsupported
        );

        for sql in [
            "BEGIN; END;",
            "BEGIN TRANSACTION; END;",
            "BEGIN IMMEDIATE; END;",
            "BEGIN READ ONLY; END;",
            "BEGIN NULL; END; SELECT A FROM T;",
        ] {
            assert_eq!(
                parse_statement(sql, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Syntax,
                "{sql}"
            );
        }
    }

    #[test]
    fn optional_single_trailing_semicolon_and_query_identifiers_convert() {
        for sql in [
            "SELECT A FROM T;",
            "SELECT A FROM T; -- trailing comment",
            "SELECT MODEL, OVER FROM T",
        ] {
            assert!(parse_statement(sql, &SqlLimits::default()).is_ok(), "{sql}");
        }
    }

    #[test]
    fn type_declarations_preserve_names_arguments_modifiers_and_spans() {
        let number =
            parse_statement("CREATE TABLE T (N NUMBER(10,2))", &SqlLimits::default()).unwrap();
        let AstStatement::CreateTable(number) = number else {
            panic!("expected CREATE TABLE");
        };
        assert_eq!(number.span, SqlSpan::new(0, 31).unwrap());
        assert_eq!(number.columns[0].span, SqlSpan::new(16, 30).unwrap());
        let declaration = &number.columns[0].data_type;
        assert_eq!(declaration.name.as_str(), "NUMBER");
        assert_eq!(declaration.name.span(), SqlSpan::new(18, 24).unwrap());
        assert_eq!(declaration.span, SqlSpan::new(18, 30).unwrap());
        assert_eq!(declaration.arguments[0].text, "10");
        assert_eq!(declaration.arguments[0].span, SqlSpan::new(25, 27).unwrap());
        assert_eq!(declaration.arguments[1].text, "2");
        assert_eq!(declaration.arguments[1].span, SqlSpan::new(28, 29).unwrap());

        let timestamp = parse_statement(
            "CREATE TABLE T (TS TIMESTAMP WITH LOCAL TIME ZONE)",
            &SqlLimits::default(),
        )
        .unwrap();
        let AstStatement::CreateTable(timestamp) = timestamp else {
            panic!("expected CREATE TABLE");
        };
        let declaration = &timestamp.columns[0].data_type;
        assert_eq!(declaration.name.as_str(), "TIMESTAMP");
        assert_eq!(declaration.span, SqlSpan::new(19, 49).unwrap());
        assert_eq!(
            declaration.modifiers,
            vec![TypeModifier::WithLocalTimeZone(
                SqlSpan::new(29, 49).unwrap()
            )]
        );
    }

    #[test]
    fn type_arguments_and_modifiers_are_budgeted_beneath_the_declaration() {
        for (sql, exact_nodes) in [
            ("CREATE TABLE T (N NUMBER(10,2))", 5),
            ("CREATE TABLE T (TS TIMESTAMP WITH LOCAL TIME ZONE)", 4),
        ] {
            let insufficient_nodes = SqlLimits::default()
                .with_max_ast_nodes(exact_nodes - 1)
                .unwrap();
            assert_eq!(
                parse_statement(sql, &insufficient_nodes)
                    .unwrap_err()
                    .code(),
                ErrorCode::Syntax,
                "node boundary for {sql}"
            );

            let exact_nodes = SqlLimits::default()
                .with_max_ast_nodes(exact_nodes)
                .unwrap();
            assert!(
                parse_statement(sql, &exact_nodes).is_ok(),
                "exact node boundary for {sql}"
            );

            let insufficient_depth = SqlLimits::default().with_max_nesting(3).unwrap();
            assert_eq!(
                parse_statement(sql, &insufficient_depth)
                    .unwrap_err()
                    .code(),
                ErrorCode::Syntax,
                "depth boundary for {sql}"
            );

            let exact_depth = SqlLimits::default().with_max_nesting(4).unwrap();
            assert!(
                parse_statement(sql, &exact_depth).is_ok(),
                "exact depth boundary for {sql}"
            );
        }
    }

    #[test]
    fn column_and_table_constraints_convert_explicitly() {
        let sql = "CREATE TABLE T (ID INT64 CONSTRAINT T_PK PRIMARY KEY, CODE TEXT UNIQUE, NAME TEXT CONSTRAINT T_NN NOT NULL, SCORE INT64 CONSTRAINT T_CK CHECK (SCORE > 0), P_ID INT64 CONSTRAINT T_FK REFERENCES P (ID), CONSTRAINT T_U UNIQUE (CODE), CONSTRAINT T_F FOREIGN KEY (P_ID) REFERENCES P (ID))";
        let statement = parse_statement(sql, &SqlLimits::default()).unwrap();
        let AstStatement::CreateTable(create) = statement else {
            panic!("expected CREATE TABLE");
        };

        assert!(matches!(
            &create.columns[0].constraints[0],
            Constraint::PrimaryKey(columns)
                if columns.name.as_ref().is_some_and(|name| name.as_str() == "T_PK")
                    && columns.columns[0].as_str() == "ID"
        ));
        assert!(matches!(
            &create.columns[1].constraints[0],
            Constraint::Unique(columns)
                if columns.name.is_none() && columns.columns[0].as_str() == "CODE"
        ));
        assert!(matches!(
            &create.columns[2].constraints[0],
            Constraint::NotNull(constraint)
                if constraint.name.as_ref().is_some_and(|name| name.as_str() == "T_NN")
                    && constraint.column.as_str() == "NAME"
        ));
        assert!(matches!(
            &create.columns[3].constraints[0],
            Constraint::Check { name: Some(name), .. } if name.as_str() == "T_CK"
        ));
        assert!(matches!(
            &create.columns[4].constraints[0],
            Constraint::ForeignKey(constraint)
                if constraint.name.as_ref().is_some_and(|name| name.as_str() == "T_FK")
                    && constraint.columns[0].as_str() == "P_ID"
                    && constraint.referenced_table.as_str() == "P"
                    && constraint.referenced_columns[0].as_str() == "ID"
        ));
        assert!(matches!(&create.constraints[0], Constraint::Unique(_)));
        assert!(matches!(&create.constraints[1], Constraint::ForeignKey(_)));
    }

    #[test]
    fn column_check_constraint_consumes_one_node_and_nests_its_expression() {
        let sql = "CREATE TABLE T (ID INT64 CHECK (ID > 0))";
        let insufficient = SqlLimits::default().with_max_ast_nodes(6).unwrap();
        assert_eq!(
            parse_statement(sql, &insufficient).unwrap_err().code(),
            ErrorCode::Syntax
        );

        let exact = SqlLimits::default().with_max_ast_nodes(7).unwrap();
        assert!(parse_statement(sql, &exact).is_ok());

        let insufficient_depth = SqlLimits::default().with_max_nesting(4).unwrap();
        assert_eq!(
            parse_statement(sql, &insufficient_depth)
                .unwrap_err()
                .code(),
            ErrorCode::Syntax
        );

        let exact_depth = SqlLimits::default().with_max_nesting(5).unwrap();
        assert!(parse_statement(sql, &exact_depth).is_ok());
    }

    #[test]
    fn unsupported_statement_families_and_extensions_are_project_unsupported() {
        for sql in [
            "CREATE OR REPLACE TABLE T (ID INT64)",
            "CREATE TABLE IF NOT EXISTS T (ID INT64)",
            "CREATE TABLE T AS SELECT ID FROM U",
            "DROP TABLE IF EXISTS T",
            "DROP TABLE T CASCADE CONSTRAINTS",
            "ALTER TABLE T ADD C INT64",
            "MERGE INTO T USING U ON (T.ID = U.ID) WHEN MATCHED THEN UPDATE SET T.C = U.C",
            "TRUNCATE TABLE T",
            "INSERT INTO T (ID) VALUES (1) RETURNING ID",
            "INSERT INTO T (ID) VALUES (1), (2)",
            "INSERT INTO T (ID) SELECT ID FROM U",
            "UPDATE T SET ID = U.ID FROM U",
            "DELETE FROM T RETURNING ID",
            "BEGIN TRANSACTION",
            "COMMIT WORK",
            "ROLLBACK TO SAVEPOINT S",
        ] {
            assert_eq!(
                parse_statement(sql, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Unsupported,
                "{sql}"
            );
        }
    }

    #[test]
    fn drop_table_cascade_constraints_recognition_is_exact_and_non_executable() {
        for sql in [
            "DROP TABLE P CASCADE CONSTRAINTS",
            "DROP TABLE \"Quoted P\" CASCADE CONSTRAINTS;",
        ] {
            assert_eq!(
                parse_statement(sql, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Unsupported,
                "{sql}"
            );
        }

        for sql in [
            "DROP TABLE P CASCADE CONSTRAINT",
            "DROP TABLE P, Q CASCADE CONSTRAINTS",
        ] {
            assert_eq!(
                parse_statement(sql, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Syntax,
                "{sql}"
            );
        }
    }

    #[test]
    fn unsupported_constraint_shapes_never_leak_internal_errors() {
        for sql in [
            "CREATE TABLE T (P_ID INT64 REFERENCES P)",
            "CREATE TABLE T (ID INT64, PRIMARY KEY USING INDEX T_I)",
            "CREATE TABLE T (ID INT64, UNIQUE USING INDEX T_I)",
        ] {
            assert_eq!(
                parse_statement(sql, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Unsupported,
                "{sql}"
            );
        }
    }

    #[test]
    fn dependency_only_table_constraint_variants_are_project_unsupported() {
        for sql in [
            "CREATE TABLE T (ID INT, INDEX T_I (ID))",
            "CREATE TABLE T (ID INT, FULLTEXT INDEX T_I (ID))",
        ] {
            let mut statements = Parser::parse_sql(&MySqlDialect {}, sql).unwrap();
            let ParserStatement::CreateTable(create) = statements.remove(0) else {
                panic!("expected dependency CREATE TABLE");
            };
            assert_eq!(create.constraints.len(), 1, "{sql}");
            let constraint = create.constraints.into_iter().next().unwrap();
            assert!(
                matches!(
                    &constraint,
                    ParserTableConstraint::Index(_) | ParserTableConstraint::FulltextOrSpatial(_)
                ),
                "{sql}"
            );

            let limits = SqlLimits::default();
            let source_map = SourceMap::new(sql).unwrap();
            let source_tokens = tokenize(sql, &limits).unwrap();
            let token_index = StatementTokenIndex::new(&source_tokens, &source_map).unwrap();
            let opening = token_index
                .tokens
                .iter()
                .position(|token| token.kind == IndexedTokenKind::LeftParen)
                .unwrap();
            let closing = token_index.matching_right(opening).unwrap();
            let mut budget = AstBudget::new(&limits);

            assert_eq!(
                convert_table_constraint(
                    constraint,
                    opening,
                    closing,
                    &source_map,
                    &token_index,
                    &mut budget,
                )
                .unwrap_err()
                .code(),
                ErrorCode::Unsupported,
                "{sql}"
            );
        }
    }

    #[test]
    fn malformed_supported_families_are_syntax_errors() {
        for sql in [
            "SELECT",
            "CREATE TABLE T (",
            "CREATE INDEX ON T (ID)",
            "INSERT INTO T VALUES (",
            "INSERT INTO T VALUES ()",
            "UPDATE T SET",
            "DELETE FROM",
        ] {
            assert_eq!(
                parse_statement(sql, &SqlLimits::default())
                    .unwrap_err()
                    .code(),
                ErrorCode::Syntax,
                "{sql}"
            );
        }
    }

    #[test]
    fn malformed_parser_errors_map_the_upstream_location() {
        let sql = "UPDATE T\nSET 名 = 1 ID";

        let error = parse_statement(sql, &SqlLimits::default()).unwrap_err();

        assert_eq!(error.code(), ErrorCode::Syntax);
        assert_eq!(
            error.sql_location(),
            Some(SqlLocation::new(21, 2, 11).unwrap()),
            "{error:?}"
        );
    }

    #[test]
    fn lexical_nesting_limit_reports_first_excess_parenthesis() {
        let sql = "SELECT (((((((A))))))) FROM T";
        let limits = SqlLimits::default().with_max_nesting(1).unwrap();

        let error = parse_statement(sql, &limits).unwrap_err();

        assert_eq!(error.code(), ErrorCode::Syntax);
        assert_eq!(
            error.sql_location(),
            Some(SqlLocation::new(8, 1, 9).unwrap())
        );
    }

    #[test]
    fn unparsed_query_exclusions_are_unsupported_at_the_authored_clause() {
        for (sql, offset) in [
            ("SELECT SUM(A) OVER () FROM T", 14),
            ("SELECT A FROM T MODEL RETURN UPDATED ROWS", 16),
        ] {
            let error = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unsupported, "{sql}");
            assert_eq!(
                error.sql_location(),
                Some(SqlLocation::new(offset, 1, offset + 1).unwrap()),
                "{sql}"
            );
        }
    }
}
