use crate::sql::{
    bound::{BoundSource, SourceId},
    plan::IndexAccessPlan,
};
use crate::storage::{
    catalog::types::ColumnType,
    index::scan::{IndexRowIdScan, IndexScanBound, IndexScanRange},
    relation::{Relation, RelationScan},
};
use crate::{DbError, Decimal, ErrorCode, Result, Value};

use super::memory::owned_values_bytes;
use super::operator::{EvalRow, ExecutionContext, Operator};
use super::{ParameterFrame, eval};

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static PAYLOAD_TRANSFERS: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
    static ACCESS_PATHS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(super) fn take_access_paths() -> Vec<&'static str> {
    ACCESS_PATHS.with(|paths| std::mem::take(&mut *paths.borrow_mut()))
}

#[cfg(test)]
fn first_payload_ptr(values: &[Value]) -> Option<usize> {
    values.iter().find_map(|value| match value {
        Value::Text(text) if !text.is_empty() => Some(text.as_ptr() as usize),
        Value::Blob(blob) if !blob.is_empty() => Some(blob.as_ptr() as usize),
        _ => None,
    })
}

#[cfg(test)]
fn record_payload_transfer(decoded: Option<usize>, admitted: &[Value]) {
    if let (Some(decoded), Some(admitted)) = (decoded, first_payload_ptr(admitted)) {
        PAYLOAD_TRANSFERS.with(|transfers| transfers.borrow_mut().push((decoded, admitted)));
    }
}

#[cfg(test)]
fn take_payload_transfers() -> Vec<(usize, usize)> {
    PAYLOAD_TRANSFERS.with(|transfers| std::mem::take(&mut *transfers.borrow_mut()))
}

pub(crate) struct OneRow {
    emitted: bool,
}

impl OneRow {
    pub(crate) const fn new() -> Self {
        Self { emitted: false }
    }
}

impl Operator for OneRow {
    fn next(&mut self, _context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        if self.emitted {
            Ok(None)
        } else {
            self.emitted = true;
            Ok(Some(EvalRow::default()))
        }
    }
}

pub(crate) struct TableScan {
    source: SourceId,
    scan: RelationScan,
}

impl TableScan {
    pub(crate) const fn new(source: SourceId, scan: RelationScan) -> Self {
        Self { source, scan }
    }
}

impl Operator for TableScan {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        #[cfg(test)]
        ACCESS_PATHS.with(|paths| paths.borrow_mut().push("TableScan"));
        let Some(row) = context.next_relation(&mut self.scan)? else {
            return Ok(None);
        };
        #[cfg(test)]
        let decoded_payload = first_payload_ptr(row.values());
        let values = row.into_values();
        #[cfg(test)]
        record_payload_transfer(decoded_payload, &values);
        let memory_bytes = owned_values_bytes(values.iter())?;
        let row = EvalRow::with_source(self.source, values, memory_bytes)?;
        context.memory().reserve(memory_bytes)?;
        Ok(Some(row))
    }
}

pub(crate) struct IndexScan {
    source: BoundSource,
    access: IndexAccessPlan,
    max_row_bytes: usize,
    state: IndexScanState,
}

enum IndexScanState {
    Uninitialized,
    Empty,
    TableScan(TableScan),
    Scanning {
        relation: Relation,
        scan: IndexRowIdScan,
    },
}

impl IndexScan {
    pub(crate) fn new(source: BoundSource, access: IndexAccessPlan, max_row_bytes: usize) -> Self {
        Self {
            source,
            access,
            max_row_bytes,
            state: IndexScanState::Uninitialized,
        }
    }

    fn initialize_bounds(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        if !matches!(self.state, IndexScanState::Uninitialized) {
            return Ok(());
        }
        let (relation, index) = context.open_index(&self.source, &self.access)?;
        self.state = match evaluate_bounds(&self.access, context.params())? {
            Some(range) if !index.range_is_representable(&range)? => {
                // Legal SQL operands need not fit a persisted index key. Keep
                // the current view, decode bound, and original outer Filter.
                IndexScanState::TableScan(TableScan::new(
                    self.source.source_id,
                    relation.scan(self.max_row_bytes),
                ))
            }
            Some(range) => IndexScanState::Scanning {
                relation,
                scan: index.scan_rowids(&range)?,
            },
            None => IndexScanState::Empty,
        };
        Ok(())
    }
}

impl Operator for IndexScan {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        #[cfg(test)]
        ACCESS_PATHS.with(|paths| paths.borrow_mut().push("IndexScan"));
        self.initialize_bounds(context)?;
        if let IndexScanState::TableScan(scan) = &mut self.state {
            return scan.next(context);
        }
        let IndexScanState::Scanning { relation, scan } = &mut self.state else {
            return Ok(None);
        };
        let Some(entry) = context.next_index(scan)? else {
            return Ok(None);
        };
        let row = context
            .get_relation(relation, entry.rowid(), self.max_row_bytes)?
            .ok_or_else(|| {
                DbError::new(
                    ErrorCode::Corruption,
                    "index entry references a missing table row",
                )
            })?;
        scan.validate_row(&entry, row.values())?;
        #[cfg(test)]
        let decoded_payload = first_payload_ptr(row.values());
        let values = row.into_values();
        #[cfg(test)]
        record_payload_transfer(decoded_payload, &values);
        let memory_bytes = owned_values_bytes(values.iter())?;
        let row = EvalRow::with_source(self.source.source_id, values, memory_bytes)?;
        context.memory().reserve(memory_bytes)?;
        Ok(Some(row))
    }
}

fn evaluate_bounds(
    access: &IndexAccessPlan,
    params: &ParameterFrame,
) -> Result<Option<IndexScanRange>> {
    let row = EvalRow::default();
    let mut equality = Vec::with_capacity(access.equality().len());
    for (column_type, value) in access.equality() {
        if value.is_null() {
            equality.push(Value::Null);
        } else {
            let expression = value.expression().ok_or_else(invalid_bound)?;
            let value = eval(expression, &row, params)?;
            if matches!(value, Value::Null) {
                return Ok(None);
            }
            equality.push(normalize_bound(value, *column_type)?);
        }
    }
    let mut range = IndexScanRange {
        equality: equality.into_boxed_slice(),
        lower: None,
        upper: None,
    };
    if let Some(template) = access.range() {
        for (bound, output) in [
            (template.lower(), &mut range.lower),
            (template.upper(), &mut range.upper),
        ] {
            if let Some(bound) = bound {
                let value = eval(bound.expression(), &row, params)?;
                if matches!(value, Value::Null) {
                    return Ok(None);
                }
                *output = Some(IndexScanBound {
                    value: normalize_bound(value, template.column_type())?,
                    inclusive: bound.inclusive(),
                });
            }
        }
    }
    Ok(Some(range))
}

fn normalize_bound(value: Value, column_type: ColumnType) -> Result<Value> {
    let valid = match (column_type, &value) {
        (ColumnType::Decimal, Value::Int64(value)) => {
            return Decimal::new(*value < 0, 0, u128::from(value.unsigned_abs()))
                .map(Value::Decimal);
        }
        (ColumnType::Int64, Value::Int64(_))
        | (ColumnType::Decimal, Value::Decimal(_))
        | (ColumnType::Text, Value::Text(_))
        | (ColumnType::Blob, Value::Blob(_))
        | (ColumnType::Date, Value::Date(_))
        | (ColumnType::Bool, Value::Bool(_)) => true,
        (ColumnType::Float64, Value::Float64(value)) => value.is_finite(),
        (ColumnType::Timestamp, Value::Timestamp(value)) => value.offset_minutes().is_none(),
        (ColumnType::TimestampWithTimeZone, Value::Timestamp(value)) => {
            value.offset_minutes().is_some()
        }
        _ => false,
    };
    if valid {
        Ok(value)
    } else {
        Err(invalid_bound())
    }
}

fn invalid_bound() -> DbError {
    DbError::new(
        ErrorCode::Internal,
        "index bound does not match its planned column type",
    )
}

#[cfg(test)]
mod tests {
    use crate::storage::test_support::{TempDatabase, phase_4c1_query_relation_fixture};
    use crate::{Connection, Database, ErrorCode, Options, Parameters, Value};

    use super::{take_access_paths, take_payload_transfers};

    fn execute(connection: &mut Connection, sql: &str) {
        let statement = connection.prepare(sql).unwrap();
        connection.execute(&statement, Parameters::none()).unwrap();
    }

    fn indexed_query_fixture() -> (TempDatabase, Database) {
        let temp = TempDatabase::new("index-query");
        let database = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = database.connect().unwrap();
        for sql in [
            "CREATE TABLE T (ID INT64 PRIMARY KEY, A INT64, B TEXT)",
            "CREATE INDEX IX_T_A ON T (A)",
            "INSERT INTO T VALUES (1, NULL, 'null')",
            "INSERT INTO T VALUES (2, 5, 'keep')",
            "INSERT INTO T VALUES (3, 5, 'reject')",
            "INSERT INTO T VALUES (7, NULL, 'null-2')",
        ] {
            execute(&mut connection, sql);
        }
        (temp, database)
    }

    fn assert_index_path() {
        let paths = take_access_paths();
        assert!(!paths.is_empty(), "query did not pull an access operator");
        assert!(
            paths.iter().all(|path| *path == "IndexScan"),
            "wrong access path: {paths:?}"
        );
    }

    fn arithmetic_query_fixture() -> (TempDatabase, Database) {
        let temp = TempDatabase::new("index-arithmetic-equivalence");
        let database = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = database.connect().unwrap();
        execute(
            &mut connection,
            "CREATE TABLE T (ID INT64, A INT64, D INT64, F FLOAT64)",
        );
        execute(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
        let insert = connection
            .prepare("INSERT INTO T VALUES (:1, :2, :3, :4)")
            .unwrap();
        // ROWID order is 1,2,3; index order is 3,2,1. The excluded/error row
        // follows a visible row, and Float64 addition overflows in only one order.
        for (id, a, divisor, value) in [
            (1, 3, 1, f64::MAX),
            (2, 2, 0, f64::MAX),
            (3, 1, 1, -f64::MAX),
        ] {
            connection
                .execute(
                    &insert,
                    Parameters::positional(&[
                        Value::Int64(id).into(),
                        Value::Int64(a).into(),
                        Value::Int64(divisor).into(),
                        Value::Float64(value).into(),
                    ]),
                )
                .unwrap();
        }
        (temp, database)
    }

    fn dynamic_query_outcome(
        connection: &mut Connection,
        predicate: &str,
        parameters: &[crate::ParameterValue],
        force_table_scan: bool,
    ) -> (Vec<Vec<Value>>, Option<ErrorCode>) {
        let predicate = if force_table_scan {
            format!("({predicate}) OR FALSE")
        } else {
            predicate.to_owned()
        };
        let statement = connection
            .prepare(&format!("SELECT ID FROM T WHERE {predicate}"))
            .unwrap();
        if force_table_scan {
            let crate::sql::plan::PlanKind::Query(query) = statement.plan().kind() else {
                panic!("query")
            };
            assert!(query.index_access().is_none(), "TableScan reference");
        }
        let _ = take_access_paths();
        let mut rows = connection
            .query(&statement, Parameters::positional(parameters))
            .unwrap();
        let mut output = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => output.push(row.values().to_vec()),
                Ok(None) => return (output, None),
                Err(error) => return (output, Some(error.code())),
            }
        }
    }

    fn assert_dynamic_error_equivalence(predicate: &str, parameters: &[crate::ParameterValue]) {
        let (_temp, database) = indexed_query_fixture();
        let mut connection = database.connect().unwrap();
        let reference = dynamic_query_outcome(&mut connection, predicate, parameters, true);
        assert_eq!(reference, (vec![], Some(ErrorCode::Type)));
        let actual = dynamic_query_outcome(&mut connection, predicate, parameters, false);
        let paths = take_access_paths();
        assert_eq!(actual, reference, "{predicate}: actual paths {paths:?}");
        assert!(!paths.is_empty() && paths.iter().all(|path| *path == "TableScan"));
    }

    #[test]
    fn dynamic_type_equivalence_unary_positive_preserves_type_error() {
        // Mutation caught: accepting Unknown unary + skips its Text type error
        // when the nonempty table has no A=999 index candidate.
        assert_dynamic_error_equivalence(
            "A = 999 AND (+:1) IS NULL",
            &[Value::Text("x".into()).into()],
        );
    }

    #[test]
    fn dynamic_type_equivalence_binary_comparisons_preserve_type_error() {
        // Mutation caught: accepting either Unknown comparison operand hides
        // incompatible Text/Int64 evaluation for every comparison operator.
        for operator in ["=", "<>", "<", "<=", ">", ">="] {
            assert_dynamic_error_equivalence(
                &format!("A = 999 AND :1 {operator} :2"),
                &[Value::Text("x".into()).into(), Value::Int64(1).into()],
            );
        }
    }

    #[test]
    fn dynamic_type_equivalence_in_list_preserves_type_error() {
        // Mutation caught: guarding binary comparison but not IN's comparison
        // allows the same incompatible Unknown values to bypass evaluation.
        assert_dynamic_error_equivalence(
            "A = 999 AND :1 IN (:2)",
            &[Value::Text("x".into()).into(), Value::Int64(1).into()],
        );
    }

    #[test]
    fn dynamic_type_equivalence_between_preserves_type_error() {
        // Mutation caught: leaving BETWEEN's Unknown bounds unguarded hides
        // its incompatible ordered comparisons when no index key matches.
        assert_dynamic_error_equivalence(
            "A = 999 AND :1 BETWEEN :2 AND :3",
            &[
                Value::Text("x".into()).into(),
                Value::Int64(1).into(),
                Value::Int64(2).into(),
            ],
        );
    }

    #[test]
    fn dynamic_type_equivalence_empty_table_does_not_evaluate_unknown_operations() {
        // Mutation caught: evaluating expressions during planning/open rather
        // than choosing TableScan would introduce errors on an empty table.
        let (_temp, database) = indexed_query_fixture();
        let mut connection = database.connect().unwrap();
        execute(&mut connection, "DELETE FROM T");
        for (predicate, parameters) in [
            (
                "A = 999 AND (+:1) IS NULL",
                vec![Value::Text("x".into()).into()],
            ),
            (
                "A = 999 AND :1 = :2",
                vec![Value::Text("x".into()).into(), Value::Int64(1).into()],
            ),
            (
                "A = 999 AND :1 IN (:2)",
                vec![Value::Text("x".into()).into(), Value::Int64(1).into()],
            ),
            (
                "A = 999 AND :1 BETWEEN :2 AND :3",
                vec![
                    Value::Text("x".into()).into(),
                    Value::Int64(1).into(),
                    Value::Int64(2).into(),
                ],
            ),
        ] {
            for forced in [false, true] {
                assert_eq!(
                    dynamic_query_outcome(&mut connection, predicate, &parameters, forced),
                    (vec![], None),
                    "{predicate}, forced={forced}",
                );
            }
        }
    }

    #[test]
    fn dynamic_type_equivalence_retains_typed_and_inert_parameter_index_access() {
        // Mutation caught: banning every parameter/Unknown leaf would discard
        // valid typed index bounds and inert projection/IS NULL/COUNT uses.
        let (_temp, database) = indexed_query_fixture();
        let mut connection = database.connect().unwrap();
        for (sql, parameters, expected) in [
            (
                "SELECT ID FROM T WHERE ID = :1 AND +:1 = 2 AND :1 = :2",
                vec![Value::Int64(2).into(), Value::Int64(2).into()],
                vec![Value::Int64(2)],
            ),
            (
                "SELECT :1 FROM T WHERE ID = 2 AND :1 IS NOT NULL",
                vec![Value::Text("x".into()).into()],
                vec![Value::Text("x".into())],
            ),
            (
                "SELECT COUNT(:1) FROM T WHERE ID = 2",
                vec![Value::Text("x".into()).into()],
                vec![Value::Int64(1)],
            ),
        ] {
            let statement = connection.prepare(sql).unwrap();
            let _ = take_access_paths();
            let mut rows = connection
                .query(&statement, Parameters::positional(&parameters))
                .unwrap();
            assert_eq!(rows.next().unwrap().unwrap().values(), expected);
            assert!(rows.next().unwrap().is_none());
            assert_index_path();
        }
    }

    fn arithmetic_query_outcome(
        connection: &mut Connection,
        projection: &str,
        predicate: &str,
        force_table_scan: bool,
    ) -> (Vec<Vec<Value>>, Option<ErrorCode>) {
        let predicate = if force_table_scan {
            format!("({predicate}) OR FALSE")
        } else {
            predicate.to_owned()
        };
        let statement = connection
            .prepare(&format!("SELECT {projection} FROM T WHERE {predicate}"))
            .unwrap();
        if force_table_scan {
            let crate::sql::plan::PlanKind::Query(query) = statement.plan().kind() else {
                panic!("query plan")
            };
            assert!(
                query.index_access().is_none(),
                "reference must be TableScan"
            );
        }
        let _ = take_access_paths();
        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        let mut values = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => values.push(row.values().to_vec()),
                Ok(None) => return (values, None),
                Err(error) => return (values, Some(error.code())),
            }
        }
    }

    fn assert_arithmetic_query_error_equivalence(
        projection: &str,
        predicate: &str,
        emitted: Vec<Vec<Value>>,
    ) {
        let (_temp, database) = arithmetic_query_fixture();
        let mut connection = database.connect().unwrap();
        let reference = arithmetic_query_outcome(&mut connection, projection, predicate, true);
        assert_eq!(reference, (emitted, Some(ErrorCode::Type)));
        let actual = arithmetic_query_outcome(&mut connection, projection, predicate, false);
        assert_eq!(actual, reference, "{projection} WHERE {predicate}");
        let paths = take_access_paths();
        assert!(!paths.is_empty() && paths.iter().all(|path| *path == "TableScan"));
    }

    #[test]
    fn arithmetic_equivalence_preserves_constant_filter_errors_without_candidates() {
        // Break caught: an empty index range must not hide eager WHERE errors.
        assert_arithmetic_query_error_equivalence("ID", "A = 999 AND 1 / 0 = 0", vec![]);
    }

    #[test]
    fn arithmetic_equivalence_preserves_excluded_row_errors_after_partial_output() {
        // Break caught: candidate-only Filter must not skip the second row's
        // division by zero after returning the first, matching row.
        assert_arithmetic_query_error_equivalence(
            "ID",
            "A = 3 AND 1 / D = 1",
            vec![vec![Value::Int64(1)]],
        );
    }

    #[test]
    fn arithmetic_equivalence_preserves_projection_rows_before_error() {
        // Break caught: index order emits ID 3 before error, whereas ROWID order
        // emits ID 1. Comparing only the eventual Type code misses the defect.
        assert_arithmetic_query_error_equivalence(
            "ID, 1 / D",
            "A >= 1",
            vec![vec![Value::Int64(1), Value::Int64(1)]],
        );
    }

    #[test]
    fn arithmetic_equivalence_preserves_sum_overflow() {
        // MAX + MAX overflows before cancellation only in ROWID input order.
        assert_arithmetic_query_error_equivalence("SUM(F)", "A >= 1", vec![]);
    }

    #[test]
    fn arithmetic_equivalence_preserves_avg_overflow() {
        // AVG uses the same fallible running sum before dividing by count.
        assert_arithmetic_query_error_equivalence("AVG(F)", "A >= 1", vec![]);
    }

    #[test]
    fn arithmetic_equivalence_retains_safe_aggregate_index_access() {
        // A blanket ban on aggregation would lose safe COUNT/MIN/MAX access.
        let (_temp, database) = arithmetic_query_fixture();
        let mut connection = database.connect().unwrap();
        assert_eq!(
            arithmetic_query_outcome(&mut connection, "COUNT(*), MIN(A), MAX(A)", "A >= 1", false,),
            (
                vec![vec![Value::Int64(3), Value::Int64(1), Value::Int64(3)]],
                None,
            )
        );
        assert_index_path();
    }

    fn query_ids(connection: &mut Connection, sql: &str, parameters: &[Value]) -> Vec<i64> {
        let statement = connection.prepare(sql).unwrap();
        let crate::sql::plan::PlanKind::Query(query) = statement.plan().kind() else {
            panic!("query plan");
        };
        assert!(
            query.index_access().is_some(),
            "fixture must be index-eligible: {sql}"
        );
        let parameters = parameters
            .iter()
            .cloned()
            .map(Into::into)
            .collect::<Vec<_>>();
        let _ = take_access_paths();
        let mut rows = connection
            .query(&statement, Parameters::positional(&parameters))
            .unwrap();
        let mut actual = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            let Value::Int64(id) = row.values()[0] else {
                panic!("integer ID")
            };
            actual.push(id);
        }
        assert_index_path();
        actual
    }

    #[test]
    fn index_scan_distinguishes_null_parameter_from_is_null_and_rechecks_where() {
        // Breaks caught: falling back to TableScan, treating NULL comparisons as
        // IS NULL, or discarding any part of the original WHERE predicate.
        let (_temp, database) = indexed_query_fixture();
        let mut connection = database.connect().unwrap();
        assert_eq!(
            query_ids(
                &mut connection,
                "SELECT ID FROM T WHERE A = :1",
                &[Value::Null]
            ),
            Vec::<i64>::new()
        );
        assert_eq!(
            query_ids(&mut connection, "SELECT ID FROM T WHERE A IS NULL", &[]),
            [1, 7]
        );
        assert_eq!(
            query_ids(
                &mut connection,
                "SELECT ID FROM T WHERE A = 5 AND B <> 'reject'",
                &[]
            ),
            [2]
        );
        assert_eq!(
            query_ids(
                &mut connection,
                "SELECT ID FROM T WHERE A >= :1 AND A < 6",
                &[Value::Int64(5)]
            ),
            [2, 3]
        );
        assert_eq!(
            query_ids(
                &mut connection,
                "SELECT ID FROM T WHERE A < :1",
                &[Value::Null]
            ),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn index_scan_reads_current_write_batch_after_an_indexed_insert() {
        // Break caught: resolving a plan's index/table root before a write,
        // or reading the committed snapshot instead of the private overlay.
        let (_temp, database) = indexed_query_fixture();
        let mut connection = database.connect().unwrap();
        let statement = connection.prepare("SELECT ID FROM T WHERE A = 9").unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let insert = transaction
            .prepare("INSERT INTO T VALUES (8, 9, 'private')")
            .unwrap();
        transaction.execute(&insert, Parameters::none()).unwrap();
        let _ = take_access_paths();
        let mut rows = transaction.query(&statement, Parameters::none()).unwrap();
        assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(8)]);
        assert!(rows.next().unwrap().is_none());
        assert_index_path();
    }

    #[test]
    fn index_scan_promotes_integer_bounds_for_decimal_keys() {
        // Break caught: encoding an Int64 bound for a Decimal key without the
        // exact promotion already permitted by SQL comparison semantics.
        let temp = TempDatabase::new("index-decimal-query");
        let database = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = database.connect().unwrap();
        for sql in [
            "CREATE TABLE T (ID INT64 PRIMARY KEY, A DECIMAL)",
            "CREATE INDEX IX_T_A ON T (A)",
            "INSERT INTO T VALUES (1, 4)",
            "INSERT INTO T VALUES (2, 5)",
            "INSERT INTO T VALUES (3, 6)",
        ] {
            execute(&mut connection, sql);
        }
        assert_eq!(
            query_ids(&mut connection, "SELECT ID FROM T WHERE A = 5", &[]),
            [2]
        );
        assert_eq!(
            query_ids(
                &mut connection,
                "SELECT ID FROM T WHERE A >= 5 AND A < 6",
                &[]
            ),
            [2]
        );
    }

    fn long_bound_fixture(blob: bool) -> (TempDatabase, Database) {
        let temp = TempDatabase::new("index-long-bound");
        let database = Database::create(temp.path(), Options::default()).unwrap();
        let mut connection = database.connect().unwrap();
        let column_type = if blob { "BLOB" } else { "TEXT" };
        execute(
            &mut connection,
            &format!("CREATE TABLE T (ID INT64 PRIMARY KEY, A {column_type}, B TEXT)"),
        );
        execute(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
        let insert = connection
            .prepare("INSERT INTO T VALUES (:1, :2, :3)")
            .unwrap();
        for (id, value, note) in [
            (1, Some("a"), "keep"),
            (2, Some("a"), "reject"),
            (3, Some("b"), "keep"),
            (4, None, "keep"),
        ] {
            let value = match value {
                Some(value) if blob => Value::Blob(value.as_bytes().to_vec()),
                Some(value) => Value::Text(value.into()),
                None => Value::Null,
            };
            connection
                .execute(
                    &insert,
                    Parameters::positional(&[
                        Value::Int64(id).into(),
                        value.into(),
                        Value::Text(note.into()).into(),
                    ]),
                )
                .unwrap();
        }
        (temp, database)
    }

    fn long_bound_result(
        connection: &mut Connection,
        comparison: &str,
        parameter: &Value,
        force_table_scan: bool,
    ) -> std::result::Result<Vec<i64>, ErrorCode> {
        // OR FALSE is equivalent under SQL three-valued logic, but not a
        // supported access term. Verify both plans instead of assuming this.
        let predicate = format!("A {comparison} :1");
        let predicate = if force_table_scan {
            format!("({predicate} OR FALSE)")
        } else {
            predicate
        };
        let statement = connection
            .prepare(&format!(
                "SELECT ID FROM T WHERE {predicate} AND B <> 'reject'"
            ))
            .unwrap();
        let crate::sql::plan::PlanKind::Query(query) = statement.plan().kind() else {
            panic!("query plan")
        };
        assert_eq!(query.index_access().is_some(), !force_table_scan);
        let parameters = [parameter.clone().into()];
        let result = (|| {
            let mut rows = connection.query(&statement, Parameters::positional(&parameters))?;
            let mut result = Vec::new();
            while let Some(row) = rows.next()? {
                let Value::Int64(id) = row.values()[0] else {
                    panic!("integer ID")
                };
                result.push(id);
            }
            result.sort_unstable();
            crate::Result::Ok(result)
        })();
        result.map_err(|error| error.code())
    }

    fn assert_long_bound_matches_table_scan(
        blob: bool,
        comparisons: &[&str],
        parameter: Value,
        expected: &[i64],
    ) {
        let (_temp, database) = long_bound_fixture(blob);
        let mut connection = database.connect().unwrap();
        for comparison in comparisons {
            let table = long_bound_result(&mut connection, comparison, &parameter, true);
            assert_eq!(
                table,
                Ok(expected.to_vec()),
                "full-filter reference {comparison}"
            );
            let indexed = long_bound_result(&mut connection, comparison, &parameter, false);
            assert_eq!(
                indexed, table,
                "indexed {comparison} must accept a legal long comparison operand"
            );
        }
    }

    #[test]
    fn index_scan_long_text_equality_matches_full_filter() {
        // Break caught: applying the persisted key byte limit to a legal
        // no-match equality operand, including escape-expanded short text.
        for parameter in ["z".repeat(1014), "\0".repeat(507)] {
            assert_long_bound_matches_table_scan(false, &["="], Value::Text(parameter), &[]);
        }
    }

    #[test]
    fn index_scan_long_text_upper_bounds_match_full_filter() {
        // Break caught: rejecting valid upper fences or losing the full
        // residual predicate when handling an oversized physical bound.
        assert_long_bound_matches_table_scan(
            false,
            &["<", "<="],
            Value::Text("z".repeat(1014)),
            &[1, 3],
        );
    }

    #[test]
    fn index_scan_long_text_lower_bounds_match_full_filter() {
        // Break caught: treating an unrepresentable lower bound as empty,
        // even though the stored nonzero-byte strings compare above it.
        assert_long_bound_matches_table_scan(
            false,
            &[">", ">="],
            Value::Text("\0".repeat(507)),
            &[1, 3],
        );
    }

    #[test]
    fn index_scan_long_blob_equality_matches_full_filter() {
        // Break caught: confusing the stored Blob key limit with the public
        // parameter/value domain; escaped zero bytes count toward key size.
        for parameter in [vec![0x5a; 1014], vec![0; 507]] {
            assert_long_bound_matches_table_scan(true, &["="], Value::Blob(parameter), &[]);
        }
    }

    #[test]
    fn index_scan_long_bound_preserves_parameter_and_row_memory_limits() {
        // Break caught: suppressing genuine query ResourceLimit errors, or
        // failing to charge the long parameter alongside input/output rows.
        let (temp, database) = long_bound_fixture(false);
        drop(database);
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let parameter_bytes = 32 + value_bytes + 1014;
        let exact = parameter_bytes + (32 + 3 * value_bytes + 5) + (32 + value_bytes);
        for (limit, accepted) in [
            (parameter_bytes - 1, false),
            (exact - 1, false),
            (exact, true),
        ] {
            let database = Database::open(
                temp.path(),
                Options::default().with_max_query_memory_bytes(limit),
            )
            .unwrap();
            let mut connection = database.connect().unwrap();
            let parameter = Value::Text("z".repeat(1014));
            let table = long_bound_result(&mut connection, "<", &parameter, true);
            assert_eq!(
                table,
                if accepted {
                    Ok(vec![1, 3])
                } else {
                    Err(ErrorCode::ResourceLimit)
                }
            );
            assert_eq!(
                long_bound_result(&mut connection, "<", &parameter, false),
                table
            );
        }
    }

    #[test]
    fn index_scan_transfers_text_and_blob_at_exact_row_plus_parameter_memory_boundary() {
        // Breaks caught: cloning admitted decoded payloads, failing to charge
        // the parameter frame, or requiring an extra full-row buffer.
        for (column_type, payload) in [
            ("TEXT", Value::Text("x".repeat(20_000))),
            ("BLOB", Value::Blob(vec![0x5a; 20_000])),
        ] {
            let temp = TempDatabase::new("index-query-memory");
            let database = Database::create(temp.path(), Options::default()).unwrap();
            let mut connection = database.connect().unwrap();
            execute(
                &mut connection,
                &format!("CREATE TABLE T (ID INT64 PRIMARY KEY, A INT64, B {column_type})"),
            );
            execute(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
            let insert = connection
                .prepare("INSERT INTO T VALUES (1, 5, :1)")
                .unwrap();
            connection
                .execute(&insert, Parameters::positional(&[payload.into()]))
                .unwrap();
            drop(connection);
            drop(database);
            let value_bytes = std::mem::size_of::<Value>() as u64;
            let exact = (32 + value_bytes) + (32 + 3 * value_bytes + 20_000);
            for (limit, accepted) in [(exact - 1, false), (exact, true)] {
                let database = Database::open(
                    temp.path(),
                    Options::default().with_max_query_memory_bytes(limit),
                )
                .unwrap();
                let mut connection = database.connect().unwrap();
                let statement = connection
                    .prepare("SELECT B FROM T WHERE A = :1 AND FALSE")
                    .unwrap();
                let _ = take_access_paths();
                let _ = take_payload_transfers();
                let mut rows = connection
                    .query(
                        &statement,
                        Parameters::positional(&[Value::Int64(5).into()]),
                    )
                    .unwrap();
                if accepted {
                    assert!(rows.next().unwrap().is_none());
                    let transfers = take_payload_transfers();
                    assert_eq!(transfers.len(), 1);
                    assert_eq!(
                        transfers[0].0, transfers[0].1,
                        "IndexScan must transfer decoded payload ownership"
                    );
                } else {
                    assert_eq!(rows.next().unwrap_err().code(), ErrorCode::ResourceLimit);
                }
                assert_index_path();
            }
        }
    }

    #[test]
    fn index_scan_reports_missing_or_mismatched_rows_before_the_full_filter() {
        // Breaks caught: ignoring dangling ROWIDs or applying WHERE before
        // verifying that the complete fetched row reproduces the index key.
        use crate::sql::exec::{ExecutionContext, ExecutionStorage, prepare_query};
        use crate::storage::{
            catalog::types::{CatalogName, RowId},
            relation::TableWrite,
            test_support::phase_4c2a_open_pager,
        };
        for missing in [true, false] {
            let (temp, database) = indexed_query_fixture();
            let mut connection = database.connect().unwrap();
            let statement = connection
                .prepare("SELECT ID FROM T WHERE A = 5 AND FALSE")
                .unwrap();
            drop(connection);
            drop(database);
            let pager = phase_4c2a_open_pager(temp.path()).unwrap();
            let mut batch = pager.begin_write().unwrap();
            let table = batch
                .catalog_reader()
                .unwrap()
                .find_table_by_name(&CatalogName::new("T").unwrap())
                .unwrap()
                .id();
            let mut writer = TableWrite::open(&mut batch, table).unwrap();
            if missing {
                assert!(writer.delete(&mut batch, RowId::new(2).unwrap()).unwrap());
            } else {
                writer
                    .replace(
                        &mut batch,
                        RowId::new(2).unwrap(),
                        &[
                            Value::Int64(2),
                            Value::Int64(99),
                            Value::Text("keep".into()),
                        ],
                    )
                    .unwrap();
            }
            writer.finish(&mut batch).unwrap();
            let execution = prepare_query(
                &statement,
                Parameters::none(),
                &batch.catalog_reader().unwrap(),
                batch.database_uuid(),
                1_000_000,
            )
            .unwrap();
            let (mut root, params, mut memory) = execution.into_parts();
            let mut context =
                ExecutionContext::new(ExecutionStorage::Write(&mut batch), &params, &mut memory);
            match root.next(&mut context) {
                Err(error) => assert_eq!(error.code(), ErrorCode::Corruption),
                Ok(_) => panic!(
                    "missing/mismatched index row must fail before FALSE filter; missing={missing}"
                ),
            }
        }
    }

    #[test]
    fn filtered_large_row_transfers_its_decoded_payload_at_the_exact_memory_boundary() {
        // Production break caught: cloning StoredRow values in TableScan
        // gives the admitted row a different Text allocation while the
        // original decoded allocation remains live outside WorkMemory.
        let name = "x".repeat(20_000);
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let input_bytes = 32 + 4 * value_bytes + name.len() as u64;
        let empty_parameter_frame_bytes = 32;
        let exact = empty_parameter_frame_bytes + input_bytes;

        for (limit, accepted) in [(exact - 1, false), (exact, true)] {
            let _ = take_payload_transfers();
            let storage = phase_4c1_query_relation_fixture(&[(1, &name)]);
            let database = Database::open(
                storage.main_path(),
                Options::default().with_max_query_memory_bytes(limit),
            )
            .unwrap();
            let mut connection = database.connect().unwrap();
            let statement = connection
                .prepare("SELECT NAME FROM USERS WHERE FALSE")
                .unwrap();
            let mut rows = connection.query(&statement, Parameters::none()).unwrap();

            if accepted {
                assert!(rows.next().unwrap().is_none());
                let transfers = take_payload_transfers();
                assert_eq!(transfers.len(), 1);
                assert_eq!(
                    transfers[0].0, transfers[0].1,
                    "TableScan must transfer, not clone, the decoded payload allocation"
                );
            } else {
                assert_eq!(rows.next().unwrap_err().code(), ErrorCode::ResourceLimit);
                let _ = take_payload_transfers();
            }
        }
    }
}
