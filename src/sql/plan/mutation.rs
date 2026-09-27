use super::{ExecExpr, Locations, check, expr::ExprCompiler};
use crate::{
    DbError, ErrorCode, Result,
    error::SqlLocation,
    sql::{
        SqlType,
        bound::{
            BoundAssignment, BoundColumn, BoundColumnDefinition, BoundConstraint, BoundExpr,
            BoundTable,
        },
    },
    storage::catalog::{
        CatalogReader,
        types::{
            CatalogName, ColumnDef, ColumnId, ColumnType, ConstraintId, ConstraintKind, IndexId,
            SchemaVersion, TableId,
        },
    },
    storage::constraint::program::CheckProgramTemplate,
    storage::relation::TableConstraintSpec,
};

/// Logical identities only: never a physical root or a retained catalog reader.
pub(crate) struct TableTarget {
    table_id: TableId,
    schema_version: SchemaVersion,
    location: SqlLocation,
}

impl TableTarget {
    fn from_bound(bound: &BoundTable, expressions: &ExprCompiler<'_, '_, '_>) -> Result<Self> {
        Ok(Self {
            table_id: bound.table_id,
            schema_version: bound.schema_version,
            location: expressions.location(bound.span)?,
        })
    }
    pub(crate) const fn table_id(&self) -> TableId {
        self.table_id
    }
    pub(crate) const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }
    pub(crate) const fn location(&self) -> SqlLocation {
        self.location
    }
}

pub(crate) struct ColumnTarget {
    table_id: TableId,
    column_id: ColumnId,
    ordinal: u16,
    data_type: SqlType,
    nullable: bool,
    location: SqlLocation,
}

impl ColumnTarget {
    fn from_bound(bound: &BoundColumn, expressions: &ExprCompiler<'_, '_, '_>) -> Result<Self> {
        Ok(Self {
            table_id: bound.table_id,
            column_id: bound.column_id,
            ordinal: bound.ordinal,
            data_type: bound.sql_type,
            nullable: bound.nullable,
            location: expressions.location(bound.span)?,
        })
    }
    pub(crate) const fn table_id(&self) -> TableId {
        self.table_id
    }
    pub(crate) const fn column_id(&self) -> ColumnId {
        self.column_id
    }
    pub(crate) const fn ordinal(&self) -> u16 {
        self.ordinal
    }
    pub(crate) const fn data_type(&self) -> SqlType {
        self.data_type
    }
    pub(crate) const fn nullable(&self) -> bool {
        self.nullable
    }
    pub(crate) fn validate_catalog_column(&self, column: &ColumnDef) -> Result<()> {
        let logical_type = match column.column_type() {
            ColumnType::Int64 => SqlType::Int64,
            ColumnType::Decimal => SqlType::Decimal,
            ColumnType::Float64 => SqlType::Float64,
            ColumnType::Text => SqlType::Text,
            ColumnType::Blob => SqlType::Blob,
            ColumnType::Date => SqlType::Date,
            ColumnType::Timestamp | ColumnType::TimestampWithTimeZone => SqlType::Timestamp,
            ColumnType::Bool => SqlType::Bool,
        };
        if self.column_id != column.id()
            || self.table_id != column.table_id()
            || self.ordinal != column.ordinal()
            || self.data_type != logical_type
            || self.nullable != column.nullable()
        {
            return Err(DbError::new(
                ErrorCode::SchemaChanged,
                "prepared indexed column definition changed",
            )
            .with_sql_location(self.location));
        }
        Ok(())
    }
    pub(crate) const fn location(&self) -> SqlLocation {
        self.location
    }
}

pub(crate) struct InsertPlan {
    table: TableTarget,
    targets: Box<[ColumnTarget]>,
    values: Box<[ExecExpr]>,
}

impl InsertPlan {
    pub(super) fn from_bound(
        table: &BoundTable,
        targets: &[BoundColumn],
        values: &[BoundExpr],
        expressions: &mut ExprCompiler<'_, '_, '_>,
    ) -> Result<Self> {
        Ok(Self {
            table: TableTarget::from_bound(table, expressions)?,
            targets: targets
                .iter()
                .map(|target| ColumnTarget::from_bound(target, expressions))
                .collect::<Result<_>>()?,
            values: values
                .iter()
                .map(|value| expressions.compile(value))
                .collect::<Result<_>>()?,
        })
    }
    pub(crate) fn table(&self) -> &TableTarget {
        &self.table
    }
    pub(crate) fn targets(&self) -> &[ColumnTarget] {
        &self.targets
    }
    pub(crate) fn values(&self) -> &[ExecExpr] {
        &self.values
    }
}

pub(crate) struct Assignment {
    target: ColumnTarget,
    value: ExecExpr,
    location: SqlLocation,
}

impl Assignment {
    pub(crate) fn target(&self) -> &ColumnTarget {
        &self.target
    }
    pub(crate) fn value(&self) -> &ExecExpr {
        &self.value
    }
    pub(crate) const fn location(&self) -> SqlLocation {
        self.location
    }
}

pub(crate) struct UpdatePlan {
    table: TableTarget,
    assignments: Box<[Assignment]>,
    selection: Option<ExecExpr>,
}

impl UpdatePlan {
    pub(super) fn from_bound(
        table: &BoundTable,
        assignments: &[BoundAssignment],
        selection: Option<&BoundExpr>,
        expressions: &mut ExprCompiler<'_, '_, '_>,
    ) -> Result<Self> {
        Ok(Self {
            table: TableTarget::from_bound(table, expressions)?,
            assignments: assignments
                .iter()
                .map(|assignment| {
                    Ok(Assignment {
                        target: ColumnTarget::from_bound(&assignment.target, expressions)?,
                        value: expressions.compile(&assignment.value)?,
                        location: expressions.location(assignment.span)?,
                    })
                })
                .collect::<Result<_>>()?,
            selection: selection
                .map(|expr| expressions.compile(expr))
                .transpose()?,
        })
    }
    pub(crate) fn table(&self) -> &TableTarget {
        &self.table
    }
    pub(crate) fn assignments(&self) -> &[Assignment] {
        &self.assignments
    }
    pub(crate) fn selection(&self) -> Option<&ExecExpr> {
        self.selection.as_ref()
    }
}

pub(crate) struct DeletePlan {
    table: TableTarget,
    selection: Option<ExecExpr>,
}

impl DeletePlan {
    pub(super) fn from_bound(
        table: &BoundTable,
        selection: Option<&BoundExpr>,
        expressions: &mut ExprCompiler<'_, '_, '_>,
    ) -> Result<Self> {
        Ok(Self {
            table: TableTarget::from_bound(table, expressions)?,
            selection: selection
                .map(|expr| expressions.compile(expr))
                .transpose()?,
        })
    }
    pub(crate) fn table(&self) -> &TableTarget {
        &self.table
    }
    pub(crate) fn selection(&self) -> Option<&ExecExpr> {
        self.selection.as_ref()
    }
}

/// An immutable CREATE INDEX request retaining only catalog identities.
pub(crate) struct CreateIndexPlan {
    name: CatalogName,
    table: TableTarget,
    columns: Box<[ColumnTarget]>,
    unique: bool,
}

impl CreateIndexPlan {
    pub(super) fn from_bound(
        name: &CatalogName,
        table: &BoundTable,
        columns: &[BoundColumn],
        unique: bool,
        expressions: &mut ExprCompiler<'_, '_, '_>,
    ) -> Result<Self> {
        Ok(Self {
            name: name.clone(),
            table: TableTarget::from_bound(table, expressions)?,
            columns: columns
                .iter()
                .map(|column| ColumnTarget::from_bound(column, expressions))
                .collect::<Result<_>>()?,
            unique,
        })
    }

    pub(crate) fn name(&self) -> &CatalogName {
        &self.name
    }
    pub(crate) fn table(&self) -> &TableTarget {
        &self.table
    }
    pub(crate) fn columns(&self) -> &[ColumnTarget] {
        &self.columns
    }
    pub(crate) const fn unique(&self) -> bool {
        self.unique
    }
}

/// An immutable DROP INDEX request retaining the bound index and owner table.
pub(crate) struct DropIndexPlan {
    index_id: IndexId,
    table: TableTarget,
}

impl DropIndexPlan {
    pub(super) fn from_bound(
        index_id: IndexId,
        table: &BoundTable,
        expressions: &ExprCompiler<'_, '_, '_>,
    ) -> Result<Self> {
        Ok(Self {
            index_id,
            table: TableTarget::from_bound(table, expressions)?,
        })
    }

    pub(crate) const fn index_id(&self) -> IndexId {
        self.index_id
    }
    pub(crate) fn table(&self) -> &TableTarget {
        &self.table
    }
}

pub(crate) struct ColumnDefinition {
    name: CatalogName,
    ordinal: u16,
    column_type: ColumnType,
    nullable: bool,
    location: SqlLocation,
}

impl ColumnDefinition {
    pub(crate) fn name(&self) -> &CatalogName {
        &self.name
    }
    pub(crate) const fn ordinal(&self) -> u16 {
        self.ordinal
    }
    pub(crate) const fn column_type(&self) -> ColumnType {
        self.column_type
    }
    pub(crate) const fn nullable(&self) -> bool {
        self.nullable
    }
    pub(crate) const fn location(&self) -> SqlLocation {
        self.location
    }
}

pub(crate) struct CreateTablePlan {
    name: CatalogName,
    columns: Box<[ColumnDefinition]>,
    constraints: Box<[TableConstraintDefinition]>,
}

/// A persistence-safe constraint definition for a new table. Local column
/// references remain logical ordinals until the catalog writer assigns durable
/// `ColumnId` values; a foreign key retains stable parent catalog identities.
pub(crate) enum TableConstraintDefinition {
    PrimaryKey {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
        location: SqlLocation,
    },
    Unique {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
        location: SqlLocation,
    },
    NotNull {
        name: Option<CatalogName>,
        column: u16,
        location: SqlLocation,
    },
    Check {
        name: Option<CatalogName>,
        template: CheckProgramTemplate,
        location: SqlLocation,
    },
    ForeignKey {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
        parent_table: BoundTable,
        parent_constraint_id: ConstraintId,
        parent_columns: Box<[BoundColumn]>,
        location: SqlLocation,
    },
}

impl CreateTablePlan {
    pub(super) fn from_bound(
        name: &CatalogName,
        columns: &[BoundColumnDefinition],
        constraints: &[BoundConstraint],
        locations: &Locations<'_>,
    ) -> Result<Self> {
        Ok(Self {
            name: name.clone(),
            columns: columns
                .iter()
                .map(|column| {
                    Ok(ColumnDefinition {
                        name: column.name.clone(),
                        ordinal: column.ordinal,
                        column_type: column.column_type,
                        nullable: column.nullable,
                        location: locations.resolve(column.span)?,
                    })
                })
                .collect::<Result<_>>()?,
            constraints: constraints
                .iter()
                .map(|constraint| TableConstraintDefinition::from_bound(constraint, locations))
                .collect::<Result<_>>()?,
        })
    }
    pub(crate) fn name(&self) -> &CatalogName {
        &self.name
    }
    pub(crate) fn columns(&self) -> &[ColumnDefinition] {
        &self.columns
    }
    pub(crate) fn constraints(&self) -> &[TableConstraintDefinition] {
        &self.constraints
    }
}

impl TableConstraintDefinition {
    pub(crate) fn storage_spec(&self) -> TableConstraintSpec<'_> {
        match self {
            Self::PrimaryKey { name, columns, .. } => TableConstraintSpec::PrimaryKey {
                name: name.clone(),
                columns: columns.clone(),
            },
            Self::Unique { name, columns, .. } => TableConstraintSpec::Unique {
                name: name.clone(),
                columns: columns.clone(),
            },
            Self::NotNull { name, column, .. } => TableConstraintSpec::NotNull {
                name: name.clone(),
                column: *column,
            },
            Self::Check { name, template, .. } => TableConstraintSpec::Check {
                name: name.clone(),
                template,
            },
            Self::ForeignKey {
                name,
                columns,
                parent_table,
                parent_constraint_id,
                ..
            } => TableConstraintSpec::ForeignKey {
                name: name.clone(),
                columns: columns.clone(),
                parent_table_id: parent_table.table_id,
                parent_constraint_id: *parent_constraint_id,
            },
        }
    }

    pub(crate) fn explicit_name(&self) -> Option<&CatalogName> {
        match self {
            Self::PrimaryKey { name, .. }
            | Self::Unique { name, .. }
            | Self::NotNull { name, .. }
            | Self::Check { name, .. }
            | Self::ForeignKey { name, .. } => name.as_ref(),
        }
    }

    pub(crate) const fn location(&self) -> SqlLocation {
        match self {
            Self::PrimaryKey { location, .. }
            | Self::Unique { location, .. }
            | Self::NotNull { location, .. }
            | Self::Check { location, .. }
            | Self::ForeignKey { location, .. } => *location,
        }
    }

    pub(crate) const fn is_check(&self) -> bool {
        matches!(self, Self::Check { .. })
    }

    fn from_bound(constraint: &BoundConstraint, locations: &Locations<'_>) -> Result<Self> {
        match constraint {
            BoundConstraint::PrimaryKey {
                name,
                columns,
                span,
                ..
            } => Ok(Self::PrimaryKey {
                name: name.clone(),
                columns: columns.clone().into_boxed_slice(),
                location: locations.resolve(*span)?,
            }),
            BoundConstraint::Unique {
                name,
                columns,
                span,
                ..
            } => Ok(Self::Unique {
                name: name.clone(),
                columns: columns.clone().into_boxed_slice(),
                location: locations.resolve(*span)?,
            }),
            BoundConstraint::NotNull {
                name, column, span, ..
            } => Ok(Self::NotNull {
                name: name.clone(),
                column: *column,
                location: locations.resolve(*span)?,
            }),
            BoundConstraint::Check {
                name, expression, ..
            } => {
                let (template, location) = check::compile_check(expression, locations)?;
                Ok(Self::Check {
                    name: name.clone(),
                    template,
                    location,
                })
            }
            BoundConstraint::ForeignKey {
                name,
                columns,
                referenced_table,
                referenced_columns,
                parent_constraint_id,
                span,
            } => Ok(Self::ForeignKey {
                name: name.clone(),
                columns: columns.clone().into_boxed_slice(),
                parent_table: *referenced_table,
                parent_constraint_id: *parent_constraint_id,
                parent_columns: referenced_columns.clone().into_boxed_slice(),
                location: locations.resolve(*span)?,
            }),
        }
    }
}

pub(crate) fn validate_foreign_key_plan(
    definition: &TableConstraintDefinition,
    catalog: &CatalogReader,
) -> Result<()> {
    let TableConstraintDefinition::ForeignKey {
        parent_table,
        parent_constraint_id,
        parent_columns,
        ..
    } = definition
    else {
        return Ok(());
    };
    let Some(table) = catalog.find_table_by_id(parent_table.table_id) else {
        return Err(foreign_key_schema_changed(
            "prepared foreign key parent table is no longer present",
        ));
    };
    if table.schema_version() != parent_table.schema_version {
        return Err(foreign_key_schema_changed(
            "prepared foreign key parent table schema version has changed",
        ));
    }
    for retained in parent_columns {
        let Some(column) = catalog.find_column_by_id(retained.column_id) else {
            return Err(foreign_key_schema_changed(
                "prepared foreign key parent column is no longer present",
            ));
        };
        if column.table_id() != parent_table.table_id
            || retained.table_id != parent_table.table_id
            || column.ordinal() != retained.ordinal
            || logical_column_type(column.column_type()) != retained.sql_type
        {
            return Err(foreign_key_schema_changed(
                "prepared foreign key parent column identity has changed",
            ));
        }
    }
    let Some(constraint) = catalog.find_constraint_by_id(*parent_constraint_id) else {
        return Err(foreign_key_schema_changed(
            "prepared foreign key parent constraint is no longer present",
        ));
    };
    if constraint.table_id() != parent_table.table_id
        || !matches!(
            constraint.kind(),
            ConstraintKind::PrimaryKey | ConstraintKind::Unique
        )
        || !catalog
            .columns_for_constraint(*parent_constraint_id)
            .map(|column| column.id())
            .eq(parent_columns.iter().map(|column| column.column_id))
    {
        return Err(foreign_key_schema_changed(
            "prepared foreign key parent constraint identity has changed",
        ));
    }
    Ok(())
}

fn logical_column_type(column_type: ColumnType) -> SqlType {
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

fn foreign_key_schema_changed(message: &'static str) -> DbError {
    DbError::new(ErrorCode::SchemaChanged, message)
}
