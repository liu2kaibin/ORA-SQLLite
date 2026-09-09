mod expr;
mod query;
mod statement;

#[allow(
    unused_imports,
    reason = "the parser and binder tasks consume the complete owned AST"
)]
pub(crate) use expr::{
    AggregateArgument, AggregateFunction, AstExpr, AstExprKind, BinaryOp, UnaryOp,
};
#[allow(
    unused_imports,
    reason = "the parser and binder tasks consume the complete owned AST"
)]
pub(crate) use query::{
    AstQuery, Join, JoinKind, NullPlacement, OrderDirection, OrderItem, SelectItem, TableSource,
};
#[allow(
    unused_imports,
    reason = "the parser and binder tasks consume the complete owned AST"
)]
pub(crate) use statement::{
    Assignment, AstStatement, ColumnDefinition, Constraint, CreateIndex, CreateTable, Delete,
    DropObject, ForeignKey, Insert, NamedColumns, NotNull, TypeArgument, TypeDeclaration,
    TypeModifier, Update,
};

#[cfg(test)]
mod tests {
    use super::{AstStatement, Delete};
    use crate::sql::{Identifier, SqlSpan};

    #[test]
    fn ast_is_owned_and_preserves_spans() {
        let mut source = String::from("users");
        let table = Identifier::unquoted(&source, SqlSpan::new(14, 19).unwrap()).unwrap();
        source.clear();
        let statement = AstStatement::Delete(Delete {
            table,
            alias: None,
            selection: None,
            span: SqlSpan::new(0, 19).unwrap(),
        });
        assert_eq!(statement.span(), SqlSpan::new(0, 19).unwrap());
        assert_eq!(statement.delete().unwrap().table.as_str(), "USERS");
    }

    #[test]
    fn ast_is_send_sync_and_static() {
        fn assert_send_sync_static<T: Send + Sync + 'static>() {}

        assert_send_sync_static::<AstStatement>();
    }
}
