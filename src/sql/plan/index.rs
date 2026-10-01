use std::{cmp::Reverse, collections::BTreeMap};

use super::{ExecExpr, expr::ExprCompiler};
use crate::{
    Result,
    sql::{
        SqlType,
        bound::{BoundBinaryOp, BoundExpr, BoundExprKind, BoundIndex, BoundSource, SourceId},
    },
    storage::catalog::types::{ColumnId, ColumnType, IndexId},
};

#[derive(Clone)]
pub(crate) enum EqualityValuePlan {
    Expression(ExecExpr),
    IsNull,
}

impl EqualityValuePlan {
    pub(crate) fn expression(&self) -> Option<&ExecExpr> {
        match self {
            Self::Expression(expression) => Some(expression),
            Self::IsNull => None,
        }
    }

    pub(crate) const fn is_null(&self) -> bool {
        matches!(self, Self::IsNull)
    }
}

#[derive(Clone)]
pub(crate) struct RangeBoundPlan {
    expression: ExecExpr,
    inclusive: bool,
}

impl RangeBoundPlan {
    pub(crate) fn expression(&self) -> &ExecExpr {
        &self.expression
    }

    pub(crate) const fn inclusive(&self) -> bool {
        self.inclusive
    }
}

#[derive(Clone)]
pub(crate) struct IndexRangePlan {
    column_type: ColumnType,
    lower: Option<RangeBoundPlan>,
    upper: Option<RangeBoundPlan>,
}

impl IndexRangePlan {
    pub(crate) const fn column_type(&self) -> ColumnType {
        self.column_type
    }

    pub(crate) fn lower(&self) -> Option<&RangeBoundPlan> {
        self.lower.as_ref()
    }

    pub(crate) fn upper(&self) -> Option<&RangeBoundPlan> {
        self.upper.as_ref()
    }
}

#[derive(Clone)]
pub(crate) struct IndexAccessPlan {
    source: SourceId,
    index_id: IndexId,
    equality: Box<[(ColumnType, EqualityValuePlan)]>,
    range: Option<IndexRangePlan>,
}

impl IndexAccessPlan {
    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }

    pub(crate) const fn index_id(&self) -> IndexId {
        self.index_id
    }

    pub(crate) fn equality(&self) -> &[(ColumnType, EqualityValuePlan)] {
        &self.equality
    }

    pub(crate) fn range(&self) -> Option<&IndexRangePlan> {
        self.range.as_ref()
    }

    pub(super) fn from_choice(
        choice: IndexChoice<'_>,
        expressions: &mut ExprCompiler<'_, '_, '_>,
    ) -> Result<Self> {
        let equality = choice
            .equality
            .into_iter()
            .map(|(column_type, value)| {
                Ok((
                    column_type,
                    match value {
                        ChoiceEquality::Expression(expression) => {
                            EqualityValuePlan::Expression(expressions.compile(expression)?)
                        }
                        ChoiceEquality::IsNull => EqualityValuePlan::IsNull,
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .into_boxed_slice();
        let range = choice
            .range
            .map(|range| {
                Ok(IndexRangePlan {
                    column_type: range.column_type,
                    lower: range
                        .lower
                        .map(|bound| {
                            Ok(RangeBoundPlan {
                                expression: expressions.compile(bound.expression)?,
                                inclusive: bound.inclusive,
                            })
                        })
                        .transpose()?,
                    upper: range
                        .upper
                        .map(|bound| {
                            Ok(RangeBoundPlan {
                                expression: expressions.compile(bound.expression)?,
                                inclusive: bound.inclusive,
                            })
                        })
                        .transpose()?,
                })
            })
            .transpose()?;
        Ok(Self {
            source: choice.source,
            index_id: choice.index_id,
            equality,
            range,
        })
    }
}

#[derive(Clone, Copy)]
enum ChoiceEquality<'a> {
    Expression(&'a BoundExpr),
    IsNull,
}

struct ChoiceRange<'a> {
    column_type: ColumnType,
    lower: Option<ChoiceRangeBound<'a>>,
    upper: Option<ChoiceRangeBound<'a>>,
}

#[derive(Clone, Copy)]
struct ChoiceRangeBound<'a> {
    expression: &'a BoundExpr,
    inclusive: bool,
}

pub(super) struct IndexChoice<'a> {
    source: SourceId,
    index_id: IndexId,
    equality: Vec<(ColumnType, ChoiceEquality<'a>)>,
    range: Option<ChoiceRange<'a>>,
}

impl IndexChoice<'_> {
    fn index_id(&self) -> IndexId {
        self.index_id
    }

    fn equality(&self) -> &[(ColumnType, ChoiceEquality<'_>)] {
        &self.equality
    }

    fn range(&self) -> Option<&ChoiceRange<'_>> {
        self.range.as_ref()
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct CandidateScore {
    unique_point_form: bool,
    equality_columns: usize,
    has_range: bool,
    range_bound_count: u8,
    reverse_index_id: Reverse<IndexId>,
}

struct ColumnTerms<'a> {
    equality: Vec<ChoiceEquality<'a>>,
    lower: Vec<ChoiceRangeBound<'a>>,
    upper: Vec<ChoiceRangeBound<'a>>,
}

impl<'a> ColumnTerms<'a> {
    fn record_equality(&mut self, value: ChoiceEquality<'a>) {
        self.equality.push(value);
    }

    fn record_lower(&mut self, value: ChoiceRangeBound<'a>) {
        self.lower.push(value);
    }

    fn record_upper(&mut self, value: ChoiceRangeBound<'a>) {
        self.upper.push(value);
    }
}

pub(super) fn choose_index<'a>(
    source: &'a BoundSource,
    selection: &'a BoundExpr,
) -> Option<IndexChoice<'a>> {
    let terms = extract_terms(source.source_id, selection);
    source
        .indexes()
        .iter()
        .filter_map(|index| candidate(source.source_id, index, &terms))
        .max_by_key(|(score, _)| *score)
        .map(|(_, choice)| choice)
}

fn extract_terms<'a>(source: SourceId, root: &'a BoundExpr) -> BTreeMap<ColumnId, ColumnTerms<'a>> {
    let mut terms = BTreeMap::new();
    let mut work = vec![root];
    while let Some(expression) = work.pop() {
        match &expression.kind {
            BoundExprKind::Binary {
                left,
                op: BoundBinaryOp::And,
                right,
            } => {
                work.push(right);
                work.push(left);
            }
            BoundExprKind::Binary { left, op, right } => {
                record_binary(&mut terms, source, left, *op, right);
            }
            BoundExprKind::IsNull {
                expr,
                negated: false,
            } => {
                if let Some(column_id) = column_id(expr, source) {
                    terms
                        .entry(column_id)
                        .or_insert_with(default_terms)
                        .record_equality(ChoiceEquality::IsNull);
                }
            }
            BoundExprKind::Between {
                expr,
                low,
                high,
                negated: false,
            } => {
                if let Some(column_id) = column_id(expr, source)
                    && searchable_expression(low)
                    && searchable_expression(high)
                {
                    let entry = terms.entry(column_id).or_insert_with(default_terms);
                    entry.record_lower(ChoiceRangeBound {
                        expression: low,
                        inclusive: true,
                    });
                    entry.record_upper(ChoiceRangeBound {
                        expression: high,
                        inclusive: true,
                    });
                }
            }
            _ => {}
        }
    }
    terms
}

fn default_terms<'a>() -> ColumnTerms<'a> {
    ColumnTerms {
        equality: Vec::new(),
        lower: Vec::new(),
        upper: Vec::new(),
    }
}

fn record_binary<'a>(
    terms: &mut BTreeMap<ColumnId, ColumnTerms<'a>>,
    source: SourceId,
    left: &'a BoundExpr,
    op: BoundBinaryOp,
    right: &'a BoundExpr,
) {
    if op == BoundBinaryOp::Equal {
        if let Some(column_id) = column_id(left, source)
            && searchable_expression(right)
        {
            terms
                .entry(column_id)
                .or_insert_with(default_terms)
                .record_equality(ChoiceEquality::Expression(right));
            return;
        }
        if let Some(column_id) = column_id(right, source)
            && searchable_expression(left)
        {
            terms
                .entry(column_id)
                .or_insert_with(default_terms)
                .record_equality(ChoiceEquality::Expression(left));
        }
        return;
    }
    if let Some(column_id) = column_id(left, source)
        && searchable_expression(right)
    {
        record_comparison(terms, column_id, op, right);
        return;
    }
    if let Some(column_id) = column_id(right, source)
        && searchable_expression(left)
    {
        record_comparison(terms, column_id, reverse_comparison(op), left);
    }
}

fn record_comparison<'a>(
    terms: &mut BTreeMap<ColumnId, ColumnTerms<'a>>,
    column_id: ColumnId,
    op: BoundBinaryOp,
    value: &'a BoundExpr,
) {
    let Some((lower, inclusive)) = comparison_direction(op) else {
        return;
    };
    let bound = ChoiceRangeBound {
        expression: value,
        inclusive,
    };
    let entry = terms.entry(column_id).or_insert_with(default_terms);
    if lower {
        entry.record_lower(bound);
    } else {
        entry.record_upper(bound);
    }
}

fn comparison_direction(op: BoundBinaryOp) -> Option<(bool, bool)> {
    match op {
        BoundBinaryOp::GreaterThan => Some((true, false)),
        BoundBinaryOp::GreaterThanOrEqual => Some((true, true)),
        BoundBinaryOp::LessThan => Some((false, false)),
        BoundBinaryOp::LessThanOrEqual => Some((false, true)),
        _ => None,
    }
}

fn reverse_comparison(op: BoundBinaryOp) -> BoundBinaryOp {
    match op {
        BoundBinaryOp::LessThan => BoundBinaryOp::GreaterThan,
        BoundBinaryOp::LessThanOrEqual => BoundBinaryOp::GreaterThanOrEqual,
        BoundBinaryOp::GreaterThan => BoundBinaryOp::LessThan,
        BoundBinaryOp::GreaterThanOrEqual => BoundBinaryOp::LessThanOrEqual,
        value => value,
    }
}

fn column_id(expression: &BoundExpr, source: SourceId) -> Option<ColumnId> {
    match expression.kind {
        BoundExprKind::Column {
            source_id,
            column_id,
            ..
        } if source_id == source => Some(column_id),
        _ => None,
    }
}

fn searchable_expression(expression: &BoundExpr) -> bool {
    matches!(
        &expression.kind,
        BoundExprKind::Literal(_) | BoundExprKind::Parameter(_)
    )
}

fn searchable_value(expr: &BoundExpr, target: ColumnType) -> bool {
    searchable_expression(expr)
        && (sql_type_matches(target, expr.sql_type)
            || (target == ColumnType::Decimal && expr.sql_type == SqlType::Int64))
}

fn sql_type_matches(column_type: ColumnType, sql_type: SqlType) -> bool {
    matches!(
        (column_type, sql_type),
        (ColumnType::Int64, SqlType::Int64)
            | (ColumnType::Decimal, SqlType::Decimal)
            | (ColumnType::Float64, SqlType::Float64)
            | (ColumnType::Text, SqlType::Text)
            | (ColumnType::Blob, SqlType::Blob)
            | (ColumnType::Date, SqlType::Date)
            | (ColumnType::Timestamp, SqlType::Timestamp)
            | (ColumnType::TimestampWithTimeZone, SqlType::Timestamp)
            | (ColumnType::Bool, SqlType::Bool)
    )
}

fn candidate<'a>(
    source: SourceId,
    index: &'a BoundIndex,
    terms: &BTreeMap<ColumnId, ColumnTerms<'a>>,
) -> Option<(CandidateScore, IndexChoice<'a>)> {
    let mut equality = Vec::new();
    let mut unique_point_form = index.unique();
    for column in index.columns() {
        let Some(value) = terms
            .get(&column.column_id())
            .and_then(|terms| first_compatible_equality(terms, column.column_type()))
        else {
            break;
        };
        match value {
            ChoiceEquality::Expression(expression)
                if searchable_value(expression, column.column_type()) =>
            {
                equality.push((column.column_type(), value));
            }
            ChoiceEquality::IsNull => {
                unique_point_form = false;
                equality.push((column.column_type(), value));
            }
            ChoiceEquality::Expression(_) => break,
        }
    }
    unique_point_form &= equality.len() == index.columns().len();

    let range = index.columns().get(equality.len()).and_then(|column| {
        if column.column_type() == ColumnType::Blob {
            return None;
        }
        let terms = terms.get(&column.column_id())?;
        let lower = first_compatible_bound(&terms.lower, column.column_type());
        let upper = first_compatible_bound(&terms.upper, column.column_type());
        if lower.is_none() && upper.is_none() {
            None
        } else {
            Some(ChoiceRange {
                column_type: column.column_type(),
                lower,
                upper,
            })
        }
    });
    if equality.is_empty() && range.is_none() {
        return None;
    }
    let score = CandidateScore {
        unique_point_form,
        equality_columns: equality.len(),
        has_range: range.is_some(),
        range_bound_count: range
            .as_ref()
            .map(|range| u8::from(range.lower.is_some()) + u8::from(range.upper.is_some()))
            .unwrap_or(0),
        reverse_index_id: Reverse(index.id()),
    };
    Some((
        score,
        IndexChoice {
            source,
            index_id: index.id(),
            equality,
            range,
        },
    ))
}

fn first_compatible_equality<'a>(
    terms: &ColumnTerms<'a>,
    column_type: ColumnType,
) -> Option<ChoiceEquality<'a>> {
    terms.equality.iter().copied().find(|value| match value {
        ChoiceEquality::Expression(expression) => searchable_value(expression, column_type),
        ChoiceEquality::IsNull => true,
    })
}

fn first_compatible_bound<'a>(
    bounds: &[ChoiceRangeBound<'a>],
    column_type: ColumnType,
) -> Option<ChoiceRangeBound<'a>> {
    bounds
        .iter()
        .copied()
        .find(|bound| searchable_value(bound.expression, column_type))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ChoiceEquality, choose_index};
    use crate::{
        Value,
        api::ParameterIdentifier,
        sql::{
            SqlSpan, SqlType,
            bound::{
                BoundBinaryOp, BoundExpr, BoundExprKind, BoundIndex, BoundIndexColumn, BoundSource,
                BoundUnaryOp, SourceId,
            },
        },
        storage::catalog::types::{ColumnId, ColumnType, IndexId, SchemaVersion, TableId},
    };

    #[test]
    fn index_choice_obeys_unique_prefix_range_bounds_and_id_order() {
        // A regression in score order could choose the first range index rather
        // than the fully-equal unique index, or ignore the matching range key.
        let source = source(vec![
            index(
                20,
                false,
                &[(1, 4, ColumnType::Int64), (2, 9, ColumnType::Int64)],
            ),
            index(
                10,
                false,
                &[(1, 4, ColumnType::Int64), (3, 12, ColumnType::Int64)],
            ),
            index(30, true, &[(1, 4, ColumnType::Int64)]),
        ]);
        let where_expr = and(
            binary(
                column(1, 4, SqlType::Int64),
                BoundBinaryOp::Equal,
                parameter(1, SqlType::Int64),
            ),
            binary(
                column(2, 9, SqlType::Int64),
                BoundBinaryOp::GreaterThanOrEqual,
                literal(Value::Int64(5), SqlType::Int64),
            ),
        );
        assert_eq!(
            choose_index(&source, &where_expr).unwrap().index_id().get(),
            30
        );

        let range_only = and(
            binary(
                column(1, 4, SqlType::Int64),
                BoundBinaryOp::Equal,
                parameter(1, SqlType::Int64),
            ),
            between(
                column(2, 9, SqlType::Int64),
                literal(Value::Int64(5), SqlType::Int64),
                literal(Value::Int64(9), SqlType::Int64),
            ),
        );
        assert_eq!(
            choose_index(&source, &range_only).unwrap().index_id().get(),
            30
        );
    }

    #[test]
    fn range_scoring_prefers_presence_then_more_bounds_without_a_unique_point() {
        let presence_source = source(vec![
            index(
                10,
                false,
                &[(1, 4, ColumnType::Int64), (3, 12, ColumnType::Int64)],
            ),
            index(
                20,
                false,
                &[(1, 4, ColumnType::Int64), (2, 9, ColumnType::Int64)],
            ),
        ]);
        let presence_predicate = and(
            eq_col(1, 1),
            between(
                column(2, 9, SqlType::Int64),
                literal(Value::Int64(5), SqlType::Int64),
                literal(Value::Int64(9), SqlType::Int64),
            ),
        );
        assert_eq!(
            choose_index(&presence_source, &presence_predicate)
                .unwrap()
                .index_id()
                .get(),
            20
        );

        let bounds_source = source(vec![
            index(
                10,
                false,
                &[(1, 4, ColumnType::Int64), (3, 12, ColumnType::Int64)],
            ),
            index(
                20,
                false,
                &[(1, 4, ColumnType::Int64), (2, 9, ColumnType::Int64)],
            ),
        ]);
        let bounds_predicate = and(
            and(
                eq_col(1, 1),
                between(
                    column(2, 9, SqlType::Int64),
                    literal(Value::Int64(5), SqlType::Int64),
                    literal(Value::Int64(9), SqlType::Int64),
                ),
            ),
            binary(
                column(3, 12, SqlType::Int64),
                BoundBinaryOp::GreaterThan,
                literal(Value::Int64(5), SqlType::Int64),
            ),
        );
        assert_eq!(
            choose_index(&bounds_source, &bounds_predicate)
                .unwrap()
                .index_id()
                .get(),
            20
        );
    }

    #[test]
    fn incompatible_first_terms_do_not_mask_later_compatible_equality_or_bounds() {
        let equality_source = single_index_source();
        let equality = and(
            binary(
                column(1, 4, SqlType::Int64),
                BoundBinaryOp::Equal,
                parameter(1, SqlType::Decimal),
            ),
            eq_col(1, 7),
        );
        let choice = choose_index(&equality_source, &equality).unwrap();
        let ChoiceEquality::Expression(expression) = choice.equality()[0].1 else {
            panic!("the later compatible equality must be selected")
        };
        assert!(matches!(
            expression.kind,
            BoundExprKind::Literal(Value::Int64(7))
        ));

        let range_source = source(vec![index(
            11,
            false,
            &[(1, 4, ColumnType::Int64), (2, 9, ColumnType::Int64)],
        )]);
        let lower = and(
            and(
                eq_col(1, 1),
                binary(
                    column(2, 9, SqlType::Int64),
                    BoundBinaryOp::GreaterThan,
                    parameter(1, SqlType::Decimal),
                ),
            ),
            binary(
                column(2, 9, SqlType::Int64),
                BoundBinaryOp::GreaterThanOrEqual,
                literal(Value::Int64(5), SqlType::Int64),
            ),
        );
        let choice = choose_index(&range_source, &lower).unwrap();
        let lower = choice.range().unwrap().lower.as_ref().unwrap();
        assert!(lower.inclusive);
        assert!(matches!(
            lower.expression.kind,
            BoundExprKind::Literal(Value::Int64(5))
        ));

        let upper = and(
            and(
                eq_col(1, 1),
                binary(
                    column(2, 9, SqlType::Int64),
                    BoundBinaryOp::LessThan,
                    parameter(1, SqlType::Decimal),
                ),
            ),
            binary(
                column(2, 9, SqlType::Int64),
                BoundBinaryOp::LessThanOrEqual,
                literal(Value::Int64(9), SqlType::Int64),
            ),
        );
        let choice = choose_index(&range_source, &upper).unwrap();
        let upper = choice.range().unwrap().upper.as_ref().unwrap();
        assert!(upper.inclusive);
        assert!(matches!(
            upper.expression.kind,
            BoundExprKind::Literal(Value::Int64(9))
        ));
    }

    #[test]
    fn unsupported_terms_remain_residual_and_never_make_a_candidate() {
        // Accidental extraction from any of these forms would make an index
        // scan omit matching rows before the residual Filter can repair it.
        for predicate in [
            binary(eq_col(1, 7), BoundBinaryOp::Or, eq_col(1, 8)),
            unary_not(eq_col(1, 7)),
            binary(
                column(1, 4, SqlType::Int64),
                BoundBinaryOp::NotEqual,
                literal(Value::Int64(7), SqlType::Int64),
            ),
            in_list(column(1, 4, SqlType::Int64), vec![7, 8]),
            like(column(1, 4, SqlType::Int64), "7%"),
            binary(
                column(1, 4, SqlType::Int64),
                BoundBinaryOp::Equal,
                column(2, 9, SqlType::Int64),
            ),
            binary(
                binary(
                    column(1, 4, SqlType::Int64),
                    BoundBinaryOp::Add,
                    literal(Value::Int64(1), SqlType::Int64),
                ),
                BoundBinaryOp::Equal,
                literal(Value::Int64(8), SqlType::Int64),
            ),
        ] {
            assert!(choose_index(&single_index_source(), &predicate).is_none());
        }
    }

    #[test]
    fn reversed_comparisons_and_between_supply_the_next_key_range() {
        let source = source(vec![index(
            11,
            false,
            &[(1, 4, ColumnType::Int64), (2, 9, ColumnType::Int64)],
        )]);
        let predicate = and(
            eq_col(1, 3),
            binary(
                literal(Value::Int64(7), SqlType::Int64),
                BoundBinaryOp::LessThan,
                column(2, 9, SqlType::Int64),
            ),
        );
        let choice = choose_index(&source, &predicate).unwrap();
        assert_eq!(choice.index_id().get(), 11);
        assert!(choice.range().unwrap().lower.is_some());
        assert!(choice.range().unwrap().upper.is_none());
        assert!(!choice.range().unwrap().lower.as_ref().unwrap().inclusive);
    }

    #[test]
    fn blob_ranges_fall_back_but_blob_equality_remains_searchable() {
        let source = source(vec![index(11, false, &[(1, 4, ColumnType::Blob)])]);
        let range = binary(
            column(1, 4, SqlType::Blob),
            BoundBinaryOp::GreaterThan,
            literal(Value::Blob(vec![1]), SqlType::Blob),
        );
        assert!(choose_index(&source, &range).is_none());

        let equality = binary(
            column(1, 4, SqlType::Blob),
            BoundBinaryOp::Equal,
            literal(Value::Blob(vec![1]), SqlType::Blob),
        );
        assert_eq!(
            choose_index(&source, &equality).unwrap().index_id().get(),
            11
        );
    }

    #[test]
    fn duplicate_conjuncts_keep_the_first_usable_value() {
        let source = single_index_source();
        let predicate = and(eq_col(1, 7), eq_col(1, 8));
        let choice = choose_index(&source, &predicate).unwrap();
        let ChoiceEquality::Expression(expression) = choice.equality()[0].1 else {
            panic!("the equality must be an expression")
        };
        let BoundExprKind::Literal(Value::Int64(value)) = &expression.kind else {
            panic!("the first equality must be retained")
        };
        assert_eq!(*value, 7);
    }

    #[test]
    fn is_null_is_an_equality_prefix_but_not_a_unique_point() {
        let source = source(vec![
            index(20, true, &[(1, 4, ColumnType::Int64)]),
            index(
                30,
                false,
                &[(1, 4, ColumnType::Int64), (2, 9, ColumnType::Int64)],
            ),
        ]);
        let predicate = and(is_null(column(1, 4, SqlType::Int64)), eq_col(2, 9));
        assert_eq!(
            choose_index(&source, &predicate).unwrap().index_id().get(),
            30
        );
    }

    #[test]
    fn lower_index_id_breaks_equal_scores_even_when_list_order_differs() {
        let source = source(vec![
            index(20, false, &[(1, 4, ColumnType::Int64)]),
            index(10, false, &[(1, 4, ColumnType::Int64)]),
        ]);
        assert_eq!(
            choose_index(&source, &eq_col(1, 7))
                .unwrap()
                .index_id()
                .get(),
            10
        );
    }

    fn col(id: u64, ordinal: u16, ty: ColumnType) -> BoundIndexColumn {
        BoundIndexColumn::new(ColumnId::new(id).unwrap(), ordinal, ty)
    }

    fn index(id: u64, unique: bool, columns: &[(u64, u16, ColumnType)]) -> BoundIndex {
        BoundIndex::new(
            IndexId::new(id).unwrap(),
            unique,
            columns
                .iter()
                .map(|(id, ordinal, ty)| col(*id, *ordinal, *ty))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
        .unwrap()
    }

    fn source(indexes: Vec<BoundIndex>) -> BoundSource {
        BoundSource::new(
            SourceId(0),
            TableId::new(1).unwrap(),
            SchemaVersion::new(1).unwrap(),
            indexes.into_boxed_slice(),
        )
    }

    fn column(id: u64, ordinal: u16, ty: SqlType) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Column {
                source_id: SourceId(0),
                table_id: TableId::new(1).unwrap(),
                column_id: ColumnId::new(id).unwrap(),
                ordinal,
            },
            ty,
            true,
            span(),
        )
    }

    fn literal(value: Value, ty: SqlType) -> BoundExpr {
        BoundExpr::new(BoundExprKind::Literal(value), ty, false, span())
    }

    fn parameter(position: u16, ty: SqlType) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Parameter(ParameterIdentifier::Position(position)),
            ty,
            true,
            span(),
        )
    }

    fn binary(left: BoundExpr, op: BoundBinaryOp, right: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Binary {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            SqlType::Bool,
            true,
            span(),
        )
    }

    fn and(left: BoundExpr, right: BoundExpr) -> BoundExpr {
        binary(left, BoundBinaryOp::And, right)
    }

    fn between(expr: BoundExpr, low: BoundExpr, high: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Between {
                expr: Box::new(expr),
                low: Box::new(low),
                high: Box::new(high),
                negated: false,
            },
            SqlType::Bool,
            true,
            span(),
        )
    }

    fn is_null(expr: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::IsNull {
                expr: Box::new(expr),
                negated: false,
            },
            SqlType::Bool,
            false,
            span(),
        )
    }

    fn unary_not(expr: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Unary {
                op: BoundUnaryOp::Not,
                expr: Box::new(expr),
            },
            SqlType::Bool,
            true,
            span(),
        )
    }

    fn in_list(expr: BoundExpr, values: Vec<i64>) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::InList {
                expr: Box::new(expr),
                values: values
                    .into_iter()
                    .map(|value| literal(Value::Int64(value), SqlType::Int64))
                    .collect(),
                negated: false,
            },
            SqlType::Bool,
            true,
            span(),
        )
    }

    fn like(expr: BoundExpr, pattern: &str) -> BoundExpr {
        BoundExpr::new(
            BoundExprKind::Like {
                expr: Box::new(expr),
                pattern: Box::new(literal(Value::Text(pattern.to_owned()), SqlType::Text)),
                negated: false,
            },
            SqlType::Bool,
            true,
            span(),
        )
    }

    fn eq_col(id: u64, value: i64) -> BoundExpr {
        binary(
            column(id, if id == 1 { 4 } else { 9 }, SqlType::Int64),
            BoundBinaryOp::Equal,
            literal(Value::Int64(value), SqlType::Int64),
        )
    }

    fn single_index_source() -> BoundSource {
        source(vec![index(11, false, &[(1, 4, ColumnType::Int64)])])
    }

    fn span() -> SqlSpan {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let start = NEXT.fetch_add(1, Ordering::Relaxed);
        SqlSpan::new(start, start + 1).unwrap()
    }
}
