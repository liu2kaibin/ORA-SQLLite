use super::super::{Identifier, SqlSpan};
use super::{AstExpr, AstQuery};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AstStatement {
    Query(AstQuery),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    CreateTable(CreateTable),
    DropTable(DropObject),
    CreateIndex(CreateIndex),
    DropIndex(DropObject),
    Begin(SqlSpan),
    Commit(SqlSpan),
    Rollback(SqlSpan),
}

impl AstStatement {
    pub(crate) const fn span(&self) -> SqlSpan {
        match self {
            Self::Query(query) => query.span(),
            Self::Insert(insert) => insert.span,
            Self::Update(update) => update.span,
            Self::Delete(delete) => delete.span,
            Self::CreateTable(create) => create.span,
            Self::DropTable(drop) => drop.span,
            Self::CreateIndex(create) => create.span,
            Self::DropIndex(drop) => drop.span,
            Self::Begin(span) | Self::Commit(span) | Self::Rollback(span) => *span,
        }
    }

    pub(crate) const fn delete(&self) -> Option<&Delete> {
        match self {
            Self::Delete(delete) => Some(delete),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Insert {
    pub(crate) table: Identifier,
    pub(crate) columns: Vec<Identifier>,
    pub(crate) values: Vec<AstExpr>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Update {
    pub(crate) table: Identifier,
    pub(crate) alias: Option<Identifier>,
    pub(crate) assignments: Vec<Assignment>,
    pub(crate) selection: Option<AstExpr>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assignment {
    pub(crate) column: Identifier,
    pub(crate) value: AstExpr,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Delete {
    pub(crate) table: Identifier,
    pub(crate) alias: Option<Identifier>,
    pub(crate) selection: Option<AstExpr>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateTable {
    pub(crate) name: Identifier,
    pub(crate) columns: Vec<ColumnDefinition>,
    pub(crate) constraints: Vec<Constraint>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDefinition {
    pub(crate) name: Identifier,
    pub(crate) data_type: TypeDeclaration,
    pub(crate) constraints: Vec<Constraint>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeDeclaration {
    pub(crate) name: Identifier,
    pub(crate) arguments: Vec<TypeArgument>,
    pub(crate) modifiers: Vec<TypeModifier>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeArgument {
    pub(crate) text: String,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeModifier {
    WithTimeZone(SqlSpan),
    WithLocalTimeZone(SqlSpan),
}

impl TypeModifier {
    pub(crate) const fn span(&self) -> SqlSpan {
        match self {
            Self::WithTimeZone(span) | Self::WithLocalTimeZone(span) => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Constraint {
    PrimaryKey(NamedColumns),
    Unique(NamedColumns),
    NotNull(NotNull),
    Check {
        name: Option<Identifier>,
        expression: AstExpr,
        span: SqlSpan,
    },
    ForeignKey(ForeignKey),
}

impl Constraint {
    pub(crate) const fn span(&self) -> SqlSpan {
        match self {
            Self::PrimaryKey(constraint) | Self::Unique(constraint) => constraint.span,
            Self::NotNull(constraint) => constraint.span,
            Self::Check { span, .. } => *span,
            Self::ForeignKey(constraint) => constraint.span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedColumns {
    pub(crate) name: Option<Identifier>,
    pub(crate) columns: Vec<Identifier>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotNull {
    pub(crate) name: Option<Identifier>,
    pub(crate) column: Identifier,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForeignKey {
    pub(crate) name: Option<Identifier>,
    pub(crate) columns: Vec<Identifier>,
    pub(crate) referenced_table: Identifier,
    pub(crate) referenced_columns: Vec<Identifier>,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateIndex {
    pub(crate) name: Identifier,
    pub(crate) table: Identifier,
    pub(crate) columns: Vec<Identifier>,
    pub(crate) unique: bool,
    pub(crate) span: SqlSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DropObject {
    pub(crate) name: Identifier,
    pub(crate) span: SqlSpan,
}
