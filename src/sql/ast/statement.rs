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

#[cfg(test)]
mod tests {
    use super::super::{AstExprKind, SelectItem, TableSource};
    use super::*;

    fn span(start: usize) -> SqlSpan {
        SqlSpan::new(start, start + 1).unwrap()
    }

    fn identifier(value: &str, start: usize) -> Identifier {
        Identifier::unquoted(value, span(start)).unwrap()
    }

    fn expression(start: usize) -> AstExpr {
        AstExpr {
            kind: AstExprKind::Null,
            span: span(start),
        }
    }

    fn query(start: usize) -> AstQuery {
        AstQuery {
            distinct: false,
            projection: vec![SelectItem::Wildcard {
                qualifier: None,
                span: span(start + 1),
            }],
            from: Some(TableSource {
                table: identifier("users", start + 2),
                alias: None,
                span: span(start + 3),
            }),
            joins: Vec::new(),
            selection: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            span: span(start),
        }
    }

    fn statement_tag(statement: &AstStatement) -> u8 {
        match statement {
            AstStatement::Query(_) => 0,
            AstStatement::Insert(_) => 1,
            AstStatement::Update(_) => 2,
            AstStatement::Delete(_) => 3,
            AstStatement::CreateTable(_) => 4,
            AstStatement::DropTable(_) => 5,
            AstStatement::CreateIndex(_) => 6,
            AstStatement::DropIndex(_) => 7,
            AstStatement::Begin(_) => 8,
            AstStatement::Commit(_) => 9,
            AstStatement::Rollback(_) => 10,
        }
    }

    fn constraint_tag(constraint: &Constraint) -> u8 {
        match constraint {
            Constraint::PrimaryKey(_) => 0,
            Constraint::Unique(_) => 1,
            Constraint::NotNull(_) => 2,
            Constraint::Check { .. } => 3,
            Constraint::ForeignKey(_) => 4,
        }
    }

    fn type_modifier_tag(modifier: TypeModifier) -> u8 {
        match modifier {
            TypeModifier::WithTimeZone(_) => 0,
            TypeModifier::WithLocalTimeZone(_) => 1,
        }
    }

    #[test]
    fn statement_ddl_dml_transaction_and_constraint_shapes_are_closed_owned_and_span_preserving() {
        let mut table_source = String::from("users");
        let mut type_argument_source = String::from("38");
        let table = Identifier::unquoted(&table_source, span(0)).unwrap();
        let type_argument = TypeArgument {
            text: type_argument_source.clone(),
            span: span(1),
        };
        table_source.clear();
        type_argument_source.clear();

        let modifiers = [
            TypeModifier::WithTimeZone(span(2)),
            TypeModifier::WithLocalTimeZone(span(3)),
        ];
        assert_eq!(modifiers.map(type_modifier_tag), [0, 1]);
        assert_eq!(
            modifiers.map(|modifier| modifier.span()),
            [span(2), span(3)]
        );

        let declaration = TypeDeclaration {
            name: identifier("number", 4),
            arguments: vec![type_argument],
            modifiers: modifiers.to_vec(),
            span: span(5),
        };
        assert_eq!(declaration.span, span(5));
        assert_eq!(declaration.arguments[0].span, span(1));
        assert_eq!(declaration.arguments[0].text, "38");

        let primary_key = NamedColumns {
            name: Some(identifier("users_pk", 6)),
            columns: vec![identifier("id", 7)],
            span: span(8),
        };
        let unique = NamedColumns {
            name: Some(identifier("users_u1", 9)),
            columns: vec![identifier("name", 10)],
            span: span(11),
        };
        let not_null = NotNull {
            name: None,
            column: identifier("id", 12),
            span: span(13),
        };
        let foreign_key = ForeignKey {
            name: Some(identifier("users_fk1", 14)),
            columns: vec![identifier("manager_id", 15)],
            referenced_table: table.clone(),
            referenced_columns: vec![identifier("id", 16)],
            span: span(17),
        };
        let constraints = vec![
            Constraint::PrimaryKey(primary_key),
            Constraint::Unique(unique),
            Constraint::NotNull(not_null),
            Constraint::Check {
                name: Some(identifier("users_ck1", 18)),
                expression: expression(19),
                span: span(20),
            },
            Constraint::ForeignKey(foreign_key),
        ];
        assert_eq!(
            constraints.iter().map(constraint_tag).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            constraints.iter().map(Constraint::span).collect::<Vec<_>>(),
            [span(8), span(11), span(13), span(20), span(17)]
        );
        assert!(matches!(
            &constraints[0],
            Constraint::PrimaryKey(columns)
                if columns.span == span(8)
                    && columns.columns[0].as_str() == "ID"
        ));
        assert!(matches!(
            &constraints[2],
            Constraint::NotNull(constraint)
                if constraint.span == span(13) && constraint.column.as_str() == "ID"
        ));
        assert!(matches!(
            &constraints[4],
            Constraint::ForeignKey(constraint)
                if constraint.span == span(17)
                    && constraint.referenced_table.as_str() == "USERS"
        ));

        let column = ColumnDefinition {
            name: identifier("id", 21),
            data_type: declaration,
            constraints: vec![constraints[2].clone()],
            span: span(22),
        };
        assert_eq!(column.span, span(22));
        assert_eq!(column.data_type.span, span(5));

        let create_table = CreateTable {
            name: table.clone(),
            columns: vec![column],
            constraints: constraints.clone(),
            span: span(23),
        };
        assert_eq!(create_table.span, span(23));
        assert_eq!(create_table.name.as_str(), "USERS");
        assert_eq!(create_table.columns.len(), 1);
        assert_eq!(create_table.constraints.len(), 5);

        let assignment = Assignment {
            column: identifier("name", 24),
            value: expression(25),
            span: span(26),
        };
        assert_eq!(assignment.span, span(26));

        let insert = Insert {
            table: table.clone(),
            columns: vec![identifier("id", 27)],
            values: vec![expression(28)],
            span: span(29),
        };
        assert_eq!(insert.span, span(29));
        assert_eq!(insert.columns.len(), 1);
        assert_eq!(insert.values[0].span(), span(28));

        let update = Update {
            table: table.clone(),
            alias: Some(identifier("u", 30)),
            assignments: vec![assignment],
            selection: Some(expression(31)),
            span: span(32),
        };
        assert_eq!(update.span, span(32));
        assert_eq!(update.assignments[0].span, span(26));

        let delete = Delete {
            table: table.clone(),
            alias: Some(identifier("u", 33)),
            selection: Some(expression(34)),
            span: span(35),
        };
        assert_eq!(delete.span, span(35));

        let create_index = CreateIndex {
            name: identifier("users_i1", 36),
            table: table.clone(),
            columns: vec![identifier("name", 37)],
            unique: true,
            span: span(38),
        };
        assert_eq!(create_index.span, span(38));
        assert!(create_index.unique);

        let drop_table = DropObject {
            name: table,
            span: span(39),
        };
        let drop_index = DropObject {
            name: identifier("users_i1", 40),
            span: span(41),
        };
        assert_eq!(drop_table.span, span(39));
        assert_eq!(drop_index.span, span(41));

        let statements = vec![
            AstStatement::Query(query(50)),
            AstStatement::Insert(insert),
            AstStatement::Update(update),
            AstStatement::Delete(delete),
            AstStatement::CreateTable(create_table),
            AstStatement::DropTable(drop_table),
            AstStatement::CreateIndex(create_index),
            AstStatement::DropIndex(drop_index),
            AstStatement::Begin(span(42)),
            AstStatement::Commit(span(43)),
            AstStatement::Rollback(span(44)),
        ];
        assert_eq!(
            statements.iter().map(statement_tag).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
        assert_eq!(
            statements
                .iter()
                .map(AstStatement::span)
                .collect::<Vec<_>>(),
            [
                span(50),
                span(29),
                span(32),
                span(35),
                span(23),
                span(39),
                span(38),
                span(41),
                span(42),
                span(43),
                span(44),
            ]
        );
        assert!(matches!(
            &statements[1],
            AstStatement::Insert(insert) if insert.table.as_str() == "USERS"
        ));
    }
}
