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
    storage::catalog::types::{
        CatalogName, ColumnDef, ColumnId, ColumnType, IndexId, SchemaVersion, TableId,
    },
    storage::constraint::program::CheckProgramTemplate,
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

/// A persistence-safe, identity-free constraint definition for a new table.
/// Column references remain logical ordinals until the catalog writer assigns
/// durable `ColumnId` values.
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
            BoundConstraint::ForeignKey { .. } => Err(DbError::new(
                ErrorCode::Internal,
                "foreign keys must be deferred before local constraint planning",
            )),
        }
    }
}
