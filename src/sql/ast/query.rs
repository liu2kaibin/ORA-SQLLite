use super::super::{Identifier, SqlSpan};
use super::AstExpr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AstQuery {
    pub(crate) distinct: bool,
    pub(crate) projection: Vec<SelectItem>,
    pub(crate) from: Option<TableSource>,
    pub(crate) joins: Vec<Join>,
    pub(crate) selection: Option<AstExpr>,
    pub(crate) group_by: Vec<AstExpr>,
    pub(crate) having: Option<AstExpr>,
    pub(crate) order_by: Vec<OrderItem>,
    pub(crate) span: SqlSpan,
}

impl AstQuery {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectItem {
    Expr {
        expr: AstExpr,
        alias: Option<Identifier>,
        span: SqlSpan,
    },
    Wildcard {
        qualifier: Option<Identifier>,
        span: SqlSpan,
    },
}

impl SelectItem {
    pub(crate) const fn span(&self) -> SqlSpan {
        match self {
            Self::Expr { span, .. } | Self::Wildcard { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableSource {
    pub(crate) table: Identifier,
    pub(crate) alias: Option<Identifier>,
    pub(crate) span: SqlSpan,
}

impl TableSource {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Join {
    pub(crate) kind: JoinKind,
    pub(crate) source: TableSource,
    pub(crate) condition: AstExpr,
    pub(crate) span: SqlSpan,
}

impl Join {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrderItem {
    pub(crate) expr: AstExpr,
    pub(crate) direction: OrderDirection,
    pub(crate) nulls: NullPlacement,
    pub(crate) span: SqlSpan,
}

impl OrderItem {
    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderDirection {
    Default,
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullPlacement {
    Default,
    First,
    Last,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize) -> SqlSpan {
        SqlSpan::new(start, start + 1).unwrap()
    }

    fn expression(start: usize) -> AstExpr {
        AstExpr {
            kind: super::super::AstExprKind::Null,
            span: span(start),
        }
    }

    fn select_item_tag(item: &SelectItem) -> u8 {
        match item {
            SelectItem::Expr { .. } => 0,
            SelectItem::Wildcard { .. } => 1,
        }
    }

    fn join_kind_tag(kind: JoinKind) -> u8 {
        match kind {
            JoinKind::Inner => 0,
            JoinKind::Left => 1,
        }
    }

    fn order_direction_tag(direction: OrderDirection) -> u8 {
        match direction {
            OrderDirection::Default => 0,
            OrderDirection::Ascending => 1,
            OrderDirection::Descending => 2,
        }
    }

    fn null_placement_tag(nulls: NullPlacement) -> u8 {
        match nulls {
            NullPlacement::Default => 0,
            NullPlacement::First => 1,
            NullPlacement::Last => 2,
        }
    }

    #[test]
    fn query_shapes_are_closed_owned_and_span_preserving() {
        let mut table_source_text = String::from("users");
        let mut alias_source_text = String::from("user_id");
        let table = Identifier::unquoted(&table_source_text, span(40)).unwrap();
        let alias = Identifier::unquoted(&alias_source_text, span(41)).unwrap();
        table_source_text.clear();
        alias_source_text.clear();

        let projection = vec![
            SelectItem::Expr {
                expr: expression(42),
                alias: Some(alias),
                span: span(43),
            },
            SelectItem::Wildcard {
                qualifier: Some(table.clone()),
                span: span(44),
            },
        ];
        assert_eq!(
            projection.iter().map(select_item_tag).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(
            projection.iter().map(SelectItem::span).collect::<Vec<_>>(),
            [span(43), span(44)]
        );

        let source = TableSource {
            table,
            alias: None,
            span: span(45),
        };
        assert_eq!(source.span(), span(45));
        assert_eq!(source.table.as_str(), "USERS");

        let join = Join {
            kind: JoinKind::Left,
            source: TableSource {
                table: Identifier::unquoted("orders", span(46)).unwrap(),
                alias: Some(Identifier::unquoted("o", span(47)).unwrap()),
                span: span(48),
            },
            condition: expression(49),
            span: span(50),
        };
        assert_eq!(join.span(), span(50));
        assert_eq!(join.source.span(), span(48));
        assert_eq!([JoinKind::Inner, JoinKind::Left].map(join_kind_tag), [0, 1]);

        let order_by = vec![
            OrderItem {
                expr: expression(51),
                direction: OrderDirection::Default,
                nulls: NullPlacement::Default,
                span: span(52),
            },
            OrderItem {
                expr: expression(53),
                direction: OrderDirection::Ascending,
                nulls: NullPlacement::First,
                span: span(54),
            },
            OrderItem {
                expr: expression(55),
                direction: OrderDirection::Descending,
                nulls: NullPlacement::Last,
                span: span(56),
            },
        ];
        assert_eq!(
            order_by.iter().map(OrderItem::span).collect::<Vec<_>>(),
            [span(52), span(54), span(56)]
        );
        assert_eq!(
            [
                OrderDirection::Default,
                OrderDirection::Ascending,
                OrderDirection::Descending,
            ]
            .map(order_direction_tag),
            [0, 1, 2]
        );
        assert_eq!(
            [
                NullPlacement::Default,
                NullPlacement::First,
                NullPlacement::Last,
            ]
            .map(null_placement_tag),
            [0, 1, 2]
        );

        let query = AstQuery {
            distinct: true,
            projection,
            from: Some(source),
            joins: vec![join],
            selection: Some(expression(57)),
            group_by: vec![expression(58)],
            having: Some(expression(59)),
            order_by,
            span: span(60),
        };
        assert_eq!(query.span(), span(60));
        assert!(query.distinct);
        assert_eq!(query.from.as_ref().unwrap().table.as_str(), "USERS");
        assert_eq!(query.projection.len(), 2);
        assert_eq!(query.joins.len(), 1);
        assert_eq!(query.order_by.len(), 3);
    }
}
