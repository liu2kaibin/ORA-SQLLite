use std::cmp::Ordering;

use crate::error::SqlLocation;
use crate::sql::{
    SqlType,
    bound::{BoundAggregateFunction, BoundBinaryOp},
    plan::{AggregateDefinition, ExecExpr, QueryPlan},
};
use crate::{DbError, Decimal, ErrorCode, Result, Value};

use super::expr::{decimal_arithmetic, int_decimal};
use super::memory::{
    MATERIALIZED_CONTAINER_BYTES, MATERIALIZED_ENTRY_OVERHEAD_BYTES, checked_add_bytes,
    checked_array_bytes, owned_value_payload_bytes, owned_values_bytes, resource_limit,
};
use super::{EvalRow, ExecutionContext, Operator, ParameterFrame, WorkMemory, eval, sql_total_cmp};

fn checked_count_increment(count: u64, location: SqlLocation) -> Result<u64> {
    count
        .checked_add(1)
        .filter(|next| *next <= i64::MAX as u64)
        .ok_or_else(|| resource_limit().with_sql_location(location))
}

struct AggregateSpec {
    function: BoundAggregateFunction,
    argument: Option<ExecExpr>,
    data_type: SqlType,
    location: SqlLocation,
}

impl AggregateSpec {
    fn from_definition(definition: &AggregateDefinition) -> Self {
        Self {
            function: definition.function(),
            argument: definition.argument().cloned(),
            data_type: definition.data_type(),
            location: definition.location(),
        }
    }
}

enum AggregateState {
    Count(u64),
    DecimalSum { sum: Decimal, count: u64 },
    FloatSum { sum: f64, count: u64 },
    DynamicSum { sum: Option<Value>, count: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl AggregateState {
    fn new(spec: &AggregateSpec) -> Result<Self> {
        Ok(match spec.function {
            BoundAggregateFunction::Count => Self::Count(0),
            BoundAggregateFunction::Sum | BoundAggregateFunction::Avg => match spec.data_type {
                SqlType::Decimal => Self::DecimalSum {
                    sum: Decimal::new(false, 0, 0)?,
                    count: 0,
                },
                SqlType::Float64 => Self::FloatSum { sum: 0.0, count: 0 },
                SqlType::Unknown => Self::DynamicSum {
                    sum: None,
                    count: 0,
                },
                _ => {
                    return Err(invalid_aggregate(
                        "SUM/AVG plan has a nonnumeric result type",
                    ));
                }
            },
            BoundAggregateFunction::Min => Self::Min(None),
            BoundAggregateFunction::Max => Self::Max(None),
        })
    }

    fn update(
        &mut self,
        spec: &AggregateSpec,
        row: &EvalRow,
        params: &ParameterFrame,
        memory: &mut WorkMemory,
        group_bytes: &mut u64,
    ) -> Result<()> {
        let argument = spec
            .argument
            .as_ref()
            .map(|expression| eval(expression, row, params))
            .transpose()?;
        match self {
            Self::Count(count) => {
                if argument
                    .as_ref()
                    .is_none_or(|value| !matches!(value, Value::Null))
                {
                    *count = checked_count_increment(*count, spec.location)?;
                }
                Ok(())
            }
            Self::DecimalSum { sum, count } => {
                let Some(argument) = non_null(argument) else {
                    return Ok(());
                };
                let value = match argument {
                    Value::Int64(value) => int_decimal(value),
                    Value::Decimal(value) => Ok(value),
                    _ => Err(aggregate_type_error(
                        "SUM/AVG received a non-Decimal runtime value",
                        spec.location,
                    )),
                }?;
                let next_count = checked_count_increment(*count, spec.location)?;
                let next_sum = decimal_arithmetic(BoundBinaryOp::Add, *sum, value)
                    .map_err(|error| error.with_sql_location(spec.location))?;
                *sum = next_sum;
                *count = next_count;
                Ok(())
            }
            Self::FloatSum { sum, count } => {
                let Some(argument) = non_null(argument) else {
                    return Ok(());
                };
                let Value::Float64(value) = argument else {
                    return Err(aggregate_type_error(
                        "SUM/AVG received a non-Float64 runtime value",
                        spec.location,
                    ));
                };
                let next_count = checked_count_increment(*count, spec.location)?;
                let next_sum = *sum + value;
                super::validate_sql_float(next_sum)
                    .map_err(|error| error.with_sql_location(spec.location))?;
                *sum = next_sum;
                *count = next_count;
                Ok(())
            }
            Self::DynamicSum { sum, count } => {
                let Some(argument) = non_null(argument) else {
                    return Ok(());
                };
                let next_count = checked_count_increment(*count, spec.location)?;
                let next_sum = match (sum.as_ref(), argument) {
                    (None, Value::Int64(value)) => Value::Decimal(int_decimal(value)?),
                    (None, Value::Decimal(value)) => Value::Decimal(value),
                    (None, Value::Float64(value)) => {
                        super::validate_sql_float(value)
                            .map_err(|error| error.with_sql_location(spec.location))?;
                        Value::Float64(value)
                    }
                    (Some(Value::Decimal(left)), Value::Int64(right)) => Value::Decimal(
                        decimal_arithmetic(BoundBinaryOp::Add, *left, int_decimal(right)?)
                            .map_err(|error| error.with_sql_location(spec.location))?,
                    ),
                    (Some(Value::Decimal(left)), Value::Decimal(right)) => Value::Decimal(
                        decimal_arithmetic(BoundBinaryOp::Add, *left, right)
                            .map_err(|error| error.with_sql_location(spec.location))?,
                    ),
                    (Some(Value::Float64(left)), Value::Float64(right)) => {
                        let value = *left + right;
                        super::validate_sql_float(value)
                            .map_err(|error| error.with_sql_location(spec.location))?;
                        Value::Float64(value)
                    }
                    _ => {
                        return Err(aggregate_type_error(
                            "SUM/AVG runtime values have incompatible numeric types",
                            spec.location,
                        ));
                    }
                };
                *sum = Some(next_sum);
                *count = next_count;
                Ok(())
            }
            Self::Min(current) => update_extreme(
                current,
                argument,
                Ordering::Less,
                spec.location,
                memory,
                group_bytes,
            ),
            Self::Max(current) => update_extreme(
                current,
                argument,
                Ordering::Greater,
                spec.location,
                memory,
                group_bytes,
            ),
        }
    }

    fn finish(self, spec: &AggregateSpec) -> Result<Value> {
        match self {
            Self::Count(count) => i64::try_from(count)
                .map(Value::Int64)
                .map_err(|_| resource_limit().with_sql_location(spec.location)),
            Self::DecimalSum { sum, count } => finish_decimal(sum, count, spec),
            Self::FloatSum { sum, count } => finish_float(sum, count, spec),
            Self::DynamicSum { sum, count } => match sum {
                None => Ok(Value::Null),
                Some(Value::Decimal(sum)) => finish_decimal(sum, count, spec),
                Some(Value::Float64(sum)) => finish_float(sum, count, spec),
                Some(_) => Err(invalid_aggregate(
                    "dynamic SUM/AVG retained a nonnumeric value",
                )),
            },
            Self::Min(value) | Self::Max(value) => Ok(value.unwrap_or(Value::Null)),
        }
    }
}

fn non_null(value: Option<Value>) -> Option<Value> {
    value.filter(|value| !matches!(value, Value::Null))
}

fn update_extreme(
    current: &mut Option<Value>,
    candidate: Option<Value>,
    wanted: Ordering,
    location: SqlLocation,
    memory: &mut WorkMemory,
    group_bytes: &mut u64,
) -> Result<()> {
    let Some(candidate) = non_null(candidate) else {
        return Ok(());
    };
    let replace = match current.as_ref() {
        Some(existing) => {
            sql_total_cmp(&candidate, existing)
                .map_err(|error| error.with_sql_location(location))?
                == wanted
        }
        None => true,
    };
    if !replace {
        return Ok(());
    }
    let old_payload = current
        .as_ref()
        .map(owned_value_payload_bytes)
        .transpose()?
        .unwrap_or(0);
    let new_payload = owned_value_payload_bytes(&candidate)?;
    if new_payload > old_payload {
        let extra = new_payload - old_payload;
        let next_group_bytes = checked_add_bytes(*group_bytes, extra)?;
        memory.reserve(extra)?;
        *group_bytes = next_group_bytes;
    }
    *current = Some(candidate);
    if old_payload > new_payload {
        let released = old_payload - new_payload;
        memory.release(released)?;
        *group_bytes = group_bytes.checked_sub(released).ok_or_else(|| {
            DbError::new(
                ErrorCode::Internal,
                "aggregate retained-value accounting underflow",
            )
        })?;
    }
    Ok(())
}

fn finish_decimal(sum: Decimal, count: u64, spec: &AggregateSpec) -> Result<Value> {
    if count == 0 {
        return Ok(Value::Null);
    }
    if spec.function == BoundAggregateFunction::Avg {
        let divisor = int_decimal(
            i64::try_from(count).map_err(|_| resource_limit().with_sql_location(spec.location))?,
        )?;
        Ok(Value::Decimal(
            decimal_arithmetic(BoundBinaryOp::Divide, sum, divisor)
                .map_err(|error| error.with_sql_location(spec.location))?,
        ))
    } else {
        Ok(Value::Decimal(sum))
    }
}

fn finish_float(sum: f64, count: u64, spec: &AggregateSpec) -> Result<Value> {
    if count == 0 {
        return Ok(Value::Null);
    }
    let value = if spec.function == BoundAggregateFunction::Avg {
        sum / count as f64
    } else {
        sum
    };
    super::validate_sql_float(value).map_err(|error| error.with_sql_location(spec.location))?;
    Ok(Value::Float64(value))
}

struct GroupState {
    key: Box<[Value]>,
    representative: EvalRow,
    aggregates: Box<[AggregateState]>,
    owned_bytes: u64,
}

pub(crate) struct Aggregate {
    input: Box<dyn Operator>,
    group_by: Box<[ExecExpr]>,
    specs: Box<[AggregateSpec]>,
    groups: Vec<GroupState>,
    initialized: bool,
    terminal: bool,
    container_bytes: u64,
}

impl Aggregate {
    pub(crate) fn new(input: Box<dyn Operator>, query: &QueryPlan) -> Self {
        Self {
            input,
            group_by: query.group_by().into(),
            specs: query
                .aggregates()
                .iter()
                .map(AggregateSpec::from_definition)
                .collect(),
            groups: Vec::new(),
            initialized: false,
            terminal: false,
            container_bytes: 0,
        }
    }

    fn initialize(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        context.memory().reserve(MATERIALIZED_CONTAINER_BYTES)?;
        self.container_bytes = MATERIALIZED_CONTAINER_BYTES;
        if self.group_by.is_empty() {
            self.push_group(Box::new([]), EvalRow::default(), context)?;
        }

        loop {
            let Some(row) = self.input.next(context)? else {
                self.initialized = true;
                return Ok(());
            };
            if self.group_by.is_empty() {
                let row_bytes = row.memory_bytes();
                let Some(group) = self.groups.first_mut() else {
                    context.memory().release(row_bytes)?;
                    return Err(invalid_aggregate(
                        "scalar aggregate state is unexpectedly absent",
                    ));
                };
                let (params, memory) = context.params_and_memory();
                let result = update_group(group, &self.specs, &row, params, memory);
                context.memory().release(row_bytes)?;
                result?;
                continue;
            }

            let key = match self
                .group_by
                .iter()
                .map(|expression| eval(expression, &row, context.params()))
                .collect::<Result<Vec<_>>>()
            {
                Ok(key) => key.into_boxed_slice(),
                Err(error) => {
                    context.memory().release(row.memory_bytes())?;
                    return Err(error);
                }
            };
            let index = match find_group(&self.groups, &key) {
                Ok(index) => index,
                Err(error) => {
                    context.memory().release(row.memory_bytes())?;
                    return Err(error);
                }
            };
            if let Some(index) = index {
                let row_bytes = row.memory_bytes();
                let Some(group) = self.groups.get_mut(index) else {
                    context.memory().release(row_bytes)?;
                    return Err(invalid_aggregate(
                        "located aggregate group is unexpectedly absent",
                    ));
                };
                let (params, memory) = context.params_and_memory();
                let result = update_group(group, &self.specs, &row, params, memory);
                context.memory().release(row_bytes)?;
                result?;
            } else {
                self.push_group(key, row, context)?;
                let (params, memory) = context.params_and_memory();
                let group = self.groups.last_mut().ok_or_else(|| {
                    invalid_aggregate("new aggregate group is unexpectedly absent")
                })?;
                update_group_from_representative(group, &self.specs, params, memory)?;
            }
        }
    }

    fn push_group(
        &mut self,
        key: Box<[Value]>,
        representative: EvalRow,
        context: &mut ExecutionContext<'_>,
    ) -> Result<()> {
        let setup = (|| {
            let states = self
                .specs
                .iter()
                .map(AggregateState::new)
                .collect::<Result<Vec<_>>>()?
                .into_boxed_slice();
            let key_bytes = owned_values_bytes(key.iter())?;
            let state_bytes = checked_add_bytes(
                MATERIALIZED_CONTAINER_BYTES,
                checked_array_bytes(
                    u64::try_from(states.len()).map_err(|_| resource_limit())?,
                    std::mem::size_of::<AggregateState>() as u64,
                )?,
            )?;
            let owned_bytes = checked_add_bytes(
                representative.memory_bytes(),
                checked_add_bytes(
                    MATERIALIZED_ENTRY_OVERHEAD_BYTES,
                    checked_add_bytes(key_bytes, state_bytes)?,
                )?,
            )?;
            let entry_bytes = checked_add_bytes(
                MATERIALIZED_ENTRY_OVERHEAD_BYTES,
                std::mem::size_of::<GroupState>() as u64,
            )?;
            let next_container = checked_add_bytes(self.container_bytes, entry_bytes)?;
            let newly_reserved = checked_add_bytes(
                entry_bytes,
                owned_bytes
                    .checked_sub(representative.memory_bytes())
                    .ok_or_else(|| invalid_aggregate("group ownership accounting underflow"))?,
            )?;
            Ok((
                states,
                owned_bytes,
                entry_bytes,
                next_container,
                newly_reserved,
            ))
        })();
        let (states, owned_bytes, _entry_bytes, next_container, newly_reserved) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                context.memory().release(representative.memory_bytes())?;
                return Err(error);
            }
        };
        if let Err(error) = context.memory().reserve(newly_reserved) {
            context.memory().release(representative.memory_bytes())?;
            return Err(error);
        }
        self.container_bytes = next_container;
        self.groups.push(GroupState {
            key,
            representative,
            aggregates: states,
            owned_bytes,
        });
        Ok(())
    }

    fn clear(&mut self, context: &mut ExecutionContext<'_>) -> Result<()> {
        for group in self.groups.drain(..) {
            context.memory().release(group.owned_bytes)?;
        }
        if self.container_bytes != 0 {
            context.memory().release(self.container_bytes)?;
            self.container_bytes = 0;
        }
        Ok(())
    }

    fn fail<T>(&mut self, context: &mut ExecutionContext<'_>, error: DbError) -> Result<T> {
        self.terminal = true;
        self.initialized = true;
        self.clear(context)?;
        Err(error)
    }

    fn emit_group(
        &mut self,
        context: &mut ExecutionContext<'_>,
        group: GroupState,
    ) -> Result<EvalRow> {
        let GroupState {
            key: _,
            representative,
            aggregates,
            owned_bytes,
        } = group;
        let mut values = Vec::with_capacity(aggregates.len());
        if aggregates.len() != self.specs.len() {
            context.memory().release(owned_bytes)?;
            return self.fail(
                context,
                invalid_aggregate("aggregate state and definition counts differ"),
            );
        }
        for (state, spec) in aggregates.into_vec().into_iter().zip(self.specs.iter()) {
            match state.finish(spec) {
                Ok(value) => values.push(value),
                Err(error) => {
                    context.memory().release(owned_bytes)?;
                    return self.fail(context, error);
                }
            }
        }
        let aggregate_bytes = match owned_values_bytes(values.iter()) {
            Ok(bytes) => bytes,
            Err(error) => {
                context.memory().release(owned_bytes)?;
                return self.fail(context, error);
            }
        };
        let target_bytes = match checked_add_bytes(representative.memory_bytes(), aggregate_bytes) {
            Ok(bytes) => bytes,
            Err(error) => {
                context.memory().release(owned_bytes)?;
                return self.fail(context, error);
            }
        };
        if target_bytes > owned_bytes {
            if let Err(error) = context.memory().reserve(target_bytes - owned_bytes) {
                context.memory().release(owned_bytes)?;
                return self.fail(context, error);
            }
        } else {
            context.memory().release(owned_bytes - target_bytes)?;
        }
        match representative.with_aggregates(values.into_boxed_slice(), target_bytes) {
            Ok(row) => Ok(row),
            Err(error) => {
                context.memory().release(target_bytes)?;
                self.fail(context, error)
            }
        }
    }
}

impl Operator for Aggregate {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        if self.terminal {
            return Ok(None);
        }
        if !self.initialized {
            if let Err(error) = self.initialize(context) {
                return self.fail(context, error);
            }
        }
        if let Some(group) = self.groups.pop() {
            return self.emit_group(context, group).map(Some);
        }
        self.terminal = true;
        self.clear(context)?;
        Ok(None)
    }
}

fn update_group(
    group: &mut GroupState,
    specs: &[AggregateSpec],
    row: &EvalRow,
    params: &ParameterFrame,
    memory: &mut WorkMemory,
) -> Result<()> {
    if group.aggregates.len() != specs.len() {
        return Err(invalid_aggregate(
            "aggregate state and definition counts differ",
        ));
    }
    for (state, spec) in group.aggregates.iter_mut().zip(specs) {
        state.update(spec, row, params, memory, &mut group.owned_bytes)?;
    }
    Ok(())
}

fn update_group_from_representative(
    group: &mut GroupState,
    specs: &[AggregateSpec],
    params: &ParameterFrame,
    memory: &mut WorkMemory,
) -> Result<()> {
    if group.aggregates.len() != specs.len() {
        return Err(invalid_aggregate(
            "aggregate state and definition counts differ",
        ));
    }
    let GroupState {
        representative,
        aggregates,
        owned_bytes,
        ..
    } = group;
    for (state, spec) in aggregates.iter_mut().zip(specs) {
        state.update(spec, representative, params, memory, owned_bytes)?;
    }
    Ok(())
}

fn find_group(groups: &[GroupState], key: &[Value]) -> Result<Option<usize>> {
    for (index, group) in groups.iter().enumerate() {
        if group.key.len() != key.len() {
            continue;
        }
        let mut equal = true;
        for (left, right) in group.key.iter().zip(key) {
            if !super::sql_group_equal(left, right)? {
                equal = false;
                break;
            }
        }
        if equal {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn invalid_aggregate(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Internal, message)
}

fn aggregate_type_error(message: &'static str, location: SqlLocation) -> DbError {
    DbError::new(ErrorCode::Type, message).with_sql_location(location)
}

#[cfg(test)]
mod tests {
    use crate::storage::test_support::{Phase4bCatalogFixture, phase_4c1_query_relation_fixture};
    use crate::{Database, Decimal, ErrorCode, Options, Parameters, Result, Value};

    struct AggregateFixture {
        _storage: Phase4bCatalogFixture,
        database: Database,
    }

    fn aggregate_fixture() -> AggregateFixture {
        aggregate_fixture_with(
            &[(1, "EAST"), (2, "EAST"), (2, "WEST"), (3, "NORTH")],
            Options::default(),
        )
    }

    fn aggregate_fixture_with(rows: &[(i64, &str)], options: Options) -> AggregateFixture {
        let storage = phase_4c1_query_relation_fixture(rows);
        let database = Database::open(storage.main_path(), options).unwrap();
        AggregateFixture {
            _storage: storage,
            database,
        }
    }

    fn values(fixture: &AggregateFixture, sql: &str) -> Result<Vec<Vec<Value>>> {
        let mut connection = fixture.database.connect()?;
        let statement = connection.prepare(sql)?;
        let mut rows = connection.query(&statement, Parameters::none())?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(row.values().to_vec());
        }
        Ok(result)
    }

    fn decimal(negative: bool, scale: i8, magnitude: u128) -> Value {
        Value::Decimal(Decimal::new(negative, scale, magnitude).unwrap())
    }

    #[test]
    fn aggregate_distinct_and_sort_match_approved_semantics() {
        // Production breaks caught: skipping any logical pipeline stage,
        // reordering JOIN/group/projection/DISTINCT/sort, or mis-finalizing an
        // aggregate changes one of these hand-derived literal result sets.
        let fixture = aggregate_fixture();
        assert_eq!(
            values(
                &fixture,
                "SELECT COUNT(*), SUM(ID), AVG(ID), MIN(ID), MAX(ID) FROM USERS",
            )
            .unwrap(),
            vec![vec![
                Value::Int64(4),
                decimal(false, 0, 8),
                decimal(false, 0, 2),
                Value::Int64(1),
                Value::Int64(3),
            ]]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT NAME, COUNT(*) FROM USERS GROUP BY NAME HAVING COUNT(*) > 1 ORDER BY NAME",
            )
            .unwrap(),
            vec![vec![Value::Text("EAST".into()), Value::Int64(2)]]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT DISTINCT NAME FROM USERS ORDER BY NAME DESC NULLS LAST",
            )
            .unwrap(),
            vec![
                vec![Value::Text("WEST".into())],
                vec![Value::Text("NORTH".into())],
                vec![Value::Text("EAST".into())],
            ]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT DISTINCT L.ID, COUNT(R.ID) AS MATCHES FROM USERS L LEFT JOIN USERS R ON R.ID = L.ID GROUP BY L.ID HAVING COUNT(R.ID) > 0 ORDER BY MATCHES DESC, L.ID ASC",
            )
            .unwrap(),
            vec![
                vec![Value::Int64(2), Value::Int64(4)],
                vec![Value::Int64(1), Value::Int64(1)],
                vec![Value::Int64(3), Value::Int64(1)],
            ]
        );
    }

    #[test]
    fn scalar_and_grouped_aggregates_cover_empty_all_null_and_having() {
        // Production breaks caught: inventing an empty input row, counting a
        // NULL argument, treating NULL group keys as distinct, or applying
        // HAVING before aggregation changes these literal results.
        let fixture = aggregate_fixture();
        assert_eq!(
            values(
                &fixture,
                "SELECT COUNT(*), COUNT(AMOUNT), SUM(ID), AVG(ID), MIN(ID), MAX(ID) FROM USERS WHERE FALSE",
            )
            .unwrap(),
            vec![vec![
                Value::Int64(0),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT COUNT(AMOUNT), SUM(AMOUNT), AVG(AMOUNT), MIN(AMOUNT), MAX(AMOUNT), MIN(NAME), MAX(NAME) FROM USERS",
            )
            .unwrap(),
            vec![vec![
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("EAST".into()),
                Value::Text("WEST".into()),
            ]]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT AMOUNT, COUNT(*) FROM USERS GROUP BY AMOUNT HAVING COUNT(*) = 4 ORDER BY AMOUNT NULLS FIRST",
            )
            .unwrap(),
            vec![vec![Value::Null, Value::Int64(4)]]
        );
        assert_eq!(
            values(
                &fixture,
                "SELECT NAME FROM USERS GROUP BY NAME HAVING NAME <> 'WEST' ORDER BY NAME",
            )
            .unwrap(),
            vec![
                vec![Value::Text("EAST".into())],
                vec![Value::Text("NORTH".into())],
            ]
        );
        assert!(
            values(&fixture, "SELECT COUNT(*) FROM USERS HAVING NULL")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn aggregate_numeric_overflow_is_checked_and_located() {
        // Production break caught: updating a Decimal/Float accumulator before
        // checking its representable bound loses the original aggregate span.
        let fixture = aggregate_fixture();
        for sql in [
            "SELECT SUM(99999999999999999999999999999999999999) FROM USERS",
            "SELECT SUM(1E308) FROM USERS",
        ] {
            let mut connection = fixture.database.connect().unwrap();
            let statement = connection.prepare(sql).unwrap();
            let mut rows = connection.query(&statement, Parameters::none()).unwrap();
            let error = rows.next().unwrap_err();
            assert_eq!(error.code(), ErrorCode::Type, "{sql}");
            assert_eq!(error.sql_location().unwrap().column(), 8, "{sql}");
        }
    }

    #[test]
    fn count_checks_the_int64_result_bound_before_updating_state() {
        // Production break caught: a u64-only increment can create a COUNT
        // state that cannot be represented by the declared Int64 result.
        let location = crate::error::SqlLocation::new(7, 1, 8).unwrap();
        let error = super::checked_count_increment(i64::MAX as u64, location).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(error.sql_location(), Some(location));
        assert_eq!(
            super::checked_count_increment(i64::MAX as u64 - 1, location).unwrap(),
            i64::MAX as u64
        );
    }

    #[test]
    fn extreme_growth_checks_group_accounting_before_reserving_memory() {
        // Production break caught: reserving a larger MIN/MAX payload before
        // validating the group's byte total leaves an unowned reservation
        // when that total overflows.
        let location = crate::error::SqlLocation::new(7, 1, 8).unwrap();
        let mut current = None;
        let mut group_bytes = u64::MAX;
        let mut memory = super::WorkMemory::new(u64::MAX).unwrap();

        let error = super::update_extreme(
            &mut current,
            Some(Value::Text("x".into())),
            std::cmp::Ordering::Less,
            location,
            &mut memory,
            &mut group_bytes,
        )
        .unwrap_err();

        assert_eq!(error.code(), ErrorCode::ResourceLimit);
        assert_eq!(memory.used(), 0);
        assert!(current.is_none());
        assert_eq!(group_bytes, u64::MAX);
    }

    #[test]
    fn first_group_that_exceeds_memory_is_not_retained() {
        // One byte below the hand-derived group retention charge rejects the
        // first group; the exact charge accepts it and completes projection.
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let key_bytes = 32 + value_bytes;
        let state_bytes = 32 + std::mem::size_of::<super::AggregateState>() as u64;
        let group_owned_bytes = 64 + key_bytes + state_bytes;
        let group_entry_bytes = 64 + std::mem::size_of::<super::GroupState>() as u64;
        let exact = 32 + 32 + group_entry_bytes + group_owned_bytes;
        for (limit, accepted) in [(exact - 1, false), (exact, true)] {
            let fixture =
                aggregate_fixture_with(&[], Options::default().with_max_query_memory_bytes(limit));
            let mut connection = fixture.database.connect().unwrap();
            let statement = connection.prepare("SELECT 1, COUNT(*) GROUP BY 1").unwrap();
            let mut rows = connection.query(&statement, Parameters::none()).unwrap();
            if accepted {
                assert_eq!(
                    rows.next().unwrap().unwrap().values(),
                    &[Value::Int64(1), Value::Int64(1)]
                );
                assert!(rows.next().unwrap().is_none());
            } else {
                assert_eq!(rows.next().unwrap_err().code(), ErrorCode::ResourceLimit);
            }
        }
    }

    #[test]
    fn dropping_a_partially_read_full_pipeline_releases_all_operator_state() {
        // Production break caught: retaining group, DISTINCT, sort, or snapshot
        // state after Rows Drop prevents the same connection from executing it.
        let fixture = aggregate_fixture();
        let mut connection = fixture.database.connect().unwrap();
        let statement = connection
            .prepare(
                "SELECT DISTINCT NAME, COUNT(*) FROM USERS GROUP BY NAME ORDER BY COUNT(*) DESC, NAME",
            )
            .unwrap();
        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        assert!(rows.next().unwrap().is_some());
        drop(rows);

        let mut rows = connection.query(&statement, Parameters::none()).unwrap();
        let mut count = 0;
        while rows.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn context_typed_numeric_aggregate_parameters_use_the_bound_accumulator() {
        // Production break caught: choosing accumulator state from the runtime
        // Value instead of the bound numeric context misroutes one of these.
        let fixture = aggregate_fixture();
        let mut connection = fixture.database.connect().unwrap();
        for (sql, parameter, expected) in [
            (
                "SELECT SUM(:1 + 0), AVG(:1 + 0), MIN(:1 + 0), MAX(:1 + 0) FROM USERS",
                Value::Int64(2),
                vec![
                    decimal(false, 0, 8),
                    decimal(false, 0, 2),
                    Value::Int64(2),
                    Value::Int64(2),
                ],
            ),
            (
                "SELECT SUM(:1 + 0E0), AVG(:1 + 0E0), MIN(:1 + 0E0), MAX(:1 + 0E0) FROM USERS",
                Value::Float64(1.5),
                vec![
                    Value::Float64(6.0),
                    Value::Float64(1.5),
                    Value::Float64(1.5),
                    Value::Float64(1.5),
                ],
            ),
        ] {
            let statement = connection.prepare(sql).unwrap();
            let params = [parameter.into()];
            let mut rows = connection
                .query(&statement, Parameters::positional(&params))
                .unwrap();
            assert_eq!(rows.next().unwrap().unwrap().values(), expected);
            assert!(rows.next().unwrap().is_none());
        }
    }

    #[test]
    fn resource_limit_cleanup_keeps_a_write_transaction_usable() {
        // Production break caught: retaining failed group state or treating a
        // query budget error as fatal prevents a later query and commit.
        let value_bytes = std::mem::size_of::<Value>() as u64;
        let key_bytes = 32 + value_bytes;
        let state_bytes = 32 + std::mem::size_of::<super::AggregateState>() as u64;
        let group_owned_bytes = 64 + key_bytes + state_bytes;
        let group_entry_bytes = 64 + std::mem::size_of::<super::GroupState>() as u64;
        let limit = 32 + 32 + group_entry_bytes + group_owned_bytes - 1;
        let fixture =
            aggregate_fixture_with(&[], Options::default().with_max_query_memory_bytes(limit));
        let mut connection = fixture.database.connect().unwrap();
        let mut transaction = connection.begin_write().unwrap();
        let failing = transaction
            .prepare("SELECT 1, COUNT(*) GROUP BY 1")
            .unwrap();
        let mut rows = transaction.query(&failing, Parameters::none()).unwrap();
        assert_eq!(rows.next().unwrap_err().code(), ErrorCode::ResourceLimit);
        drop(rows);

        let valid = transaction.prepare("SELECT 1").unwrap();
        let mut rows = transaction.query(&valid, Parameters::none()).unwrap();
        assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(1)]);
        drop(rows);
        transaction.commit().unwrap();
    }
}
