mod expr;
mod query;
mod statement;

pub(crate) use query::{
    BoundIndex, BoundIndexColumn, BoundJoin, BoundOrderExpression, BoundOrderItem, BoundQuery,
    BoundSource, SourceId,
};

pub(crate) use expr::{
    BoundAggregateFunction, BoundBinaryOp, BoundExpr, BoundExprKind, BoundUnaryOp,
};
pub(crate) use statement::{
    BoundAssignment, BoundColumn, BoundColumnDefinition, BoundConstraint, BoundStatement,
    BoundStatementKind, BoundTable, CreatedNameDependency, SchemaDependency,
};

#[cfg(test)]
mod tests {
    use crate::{Statement, sql::bound::BoundStatement};

    #[test]
    fn bound_types_are_owned_send_and_sync() {
        fn assert_owned<T: Send + Sync + 'static>() {}
        assert_owned::<BoundStatement>();
        assert_owned::<Statement>();
    }
}
