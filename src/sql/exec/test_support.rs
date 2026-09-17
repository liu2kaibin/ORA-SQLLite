use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    panic::{AssertUnwindSafe, catch_unwind},
};

use crate::{
    Connection, DataType, Database, Decimal, ErrorCode, Options, ParameterValue, Parameters, Value,
    storage::test_support::TempDatabase,
};

const EDGE_INTS: &[i64] = &[i64::MIN, -1, 0, 1, i64::MAX];
const EDGE_TEXT: &[&str] = &["", "A", "a", "中", "😀", "%", "_"];

#[derive(Debug, Clone, Copy)]
enum ReferenceType {
    Int64,
    Decimal,
    Float64,
    Text,
    Blob,
}

impl ReferenceType {
    const fn public(self) -> DataType {
        match self {
            Self::Int64 => DataType::Int64,
            Self::Decimal => DataType::Decimal,
            Self::Float64 => DataType::Float64,
            Self::Text => DataType::Text,
            Self::Blob => DataType::Blob,
        }
    }

    fn accepts(self, value: &Value) -> bool {
        match (self, value) {
            (Self::Int64, Value::Int64(_))
            | (Self::Decimal, Value::Decimal(_))
            | (Self::Text, Value::Text(_))
            | (Self::Blob, Value::Blob(_)) => true,
            (Self::Float64, Value::Float64(value)) => value.is_finite(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct ReferenceColumn {
    data_type: ReferenceType,
    nullable: bool,
}

#[derive(Debug, Clone)]
struct TableSpec {
    name: &'static str,
    create_sql: &'static str,
    columns: Vec<ReferenceColumn>,
}

#[derive(Debug, Clone)]
struct SeedRow {
    table: &'static str,
    values: Vec<Value>,
}

#[derive(Debug, Clone)]
enum Operation {
    Predicate { minimum: i64, excluded_id: i64 },
    LeftJoin,
    Group,
    Distinct,
    Sort,
    QueryEdges,
    Insert { values: Vec<Value> },
    Update { group_id: i64, delta: i64 },
    Delete { id: i64 },
    Begin,
    Rollback,
    Commit,
    SchemaInvalidation,
    RecreateScratch,
    ErrorCorpus { non_finite: f64 },
    MemoryBoundary,
}

#[derive(Debug)]
struct GeneratedProgram {
    tables: Vec<TableSpec>,
    seed_rows: Vec<SeedRow>,
    operations: Vec<Operation>,
}

impl GeneratedProgram {
    fn from_seed(seed: u64, table_count: usize, operation_count: usize) -> Self {
        let mut generator = Generator::new(seed);
        let tables = table_specs()
            .into_iter()
            .take(table_count.min(4))
            .collect::<Vec<_>>();
        let table_exists = |name| tables.iter().any(|table| table.name == name);
        let mut seed_rows = Vec::new();

        if table_exists("PARENTS") {
            for (id, label) in [(0, "A"), (1, "a"), (2, "中")] {
                seed_rows.push(SeedRow {
                    table: "PARENTS",
                    values: vec![Value::Int64(id), Value::Text(label.into())],
                });
            }
        }
        if table_exists("ITEM") {
            let texts = [
                Value::Null,
                Value::Text(String::new()),
                Value::Text(EDGE_TEXT[generator.index(EDGE_TEXT.len())].into()),
                Value::Text("😀".into()),
            ];
            for (index, (id, parent_id, group_id, number)) in [
                (10, 0, 0, -1),
                (11, 0, 0, 0),
                (12, 1, 1, 1),
                (13, 99, 1, generator.small_i64()),
            ]
            .into_iter()
            .enumerate()
            {
                let scores = [-0.0, 0.0, 1.5, -2.25];
                let amounts = [
                    decimal_value(false, 38, 1),
                    decimal_value(false, 0, 0),
                    decimal_value(true, 2, 123),
                    decimal_value(false, -38, 1),
                ];
                seed_rows.push(SeedRow {
                    table: "ITEM",
                    values: vec![
                        Value::Int64(id),
                        Value::Int64(parent_id),
                        Value::Int64(group_id),
                        texts[index].clone(),
                        Value::Int64(number),
                        Value::Float64(scores[index]),
                        amounts[index].clone(),
                        Value::Blob(vec![index as u8, generator.next() as u8]),
                    ],
                });
            }
        }
        if table_exists("EDGE") {
            for (index, text) in EDGE_TEXT.iter().enumerate() {
                let floats = [-0.0, 0.0, f64::MIN_POSITIVE, f64::MAX, -1.5, 2.5, 42.0];
                let decimals = [
                    decimal_value(false, 38, 1),
                    decimal_value(false, -38, 1),
                    decimal_value(false, 0, 0),
                    decimal_value(false, 0, 99_999_999_999_999_999_999_999_999_999_999_999_999),
                    decimal_value(true, 0, 9_223_372_036_854_775_808),
                    decimal_value(false, 2, 123),
                    decimal_value(true, -2, 7),
                ];
                seed_rows.push(SeedRow {
                    table: "EDGE",
                    values: vec![
                        Value::Int64(EDGE_INTS[index % EDGE_INTS.len()]),
                        Value::Text((*text).into()),
                        Value::Float64(floats[index]),
                        decimals[index].clone(),
                        Value::Blob(if index == 0 {
                            Vec::new()
                        } else {
                            vec![0, index as u8, 0xff]
                        }),
                    ],
                });
            }
        }
        if table_exists("SCRATCH") {
            seed_rows.push(SeedRow {
                table: "SCRATCH",
                values: vec![Value::Int64(generator.small_i64())],
            });
        }

        let inserted_id = 1_000 + i64::try_from(seed % 1_000_000).unwrap_or(0) * 4;
        let inserted = item_values(
            inserted_id,
            99,
            generator.small_i64().rem_euclid(2),
            EDGE_TEXT[generator.index(EDGE_TEXT.len())],
            generator.small_i64(),
        );
        let rolled_back = item_values(inserted_id + 1, 2, 0, "rollback", 7);
        let committed = item_values(inserted_id + 2, 2, 1, "commit", 8);
        let non_finite = match seed % 3 {
            0 => f64::NAN,
            1 => f64::INFINITY,
            _ => f64::NEG_INFINITY,
        };
        let base = vec![
            Operation::Predicate {
                minimum: generator.small_i64().rem_euclid(3) - 1,
                excluded_id: 10 + generator.small_i64().rem_euclid(4),
            },
            Operation::LeftJoin,
            Operation::Group,
            Operation::Distinct,
            Operation::Sort,
            Operation::QueryEdges,
            Operation::Insert { values: inserted },
            Operation::Update {
                group_id: generator.small_i64().rem_euclid(2),
                delta: generator.small_i64().rem_euclid(5) - 2,
            },
            Operation::Delete {
                id: 10 + generator.small_i64().rem_euclid(4),
            },
            Operation::Begin,
            Operation::Insert {
                values: rolled_back,
            },
            Operation::Predicate {
                minimum: -10,
                excluded_id: -1,
            },
            Operation::Rollback,
            Operation::Predicate {
                minimum: -10,
                excluded_id: -1,
            },
            Operation::Begin,
            Operation::Insert { values: committed },
            Operation::Commit,
            Operation::Predicate {
                minimum: -10,
                excluded_id: -1,
            },
            Operation::SchemaInvalidation,
            Operation::RecreateScratch,
            Operation::ErrorCorpus { non_finite },
            Operation::MemoryBoundary,
            Operation::Group,
            Operation::LeftJoin,
        ];
        let operations = base.iter().cycle().take(operation_count).cloned().collect();
        Self {
            tables,
            seed_rows,
            operations,
        }
    }
}

fn decimal_value(negative: bool, scale: i8, magnitude: u128) -> Value {
    Decimal::new(negative, scale, magnitude).map_or(Value::Null, Value::Decimal)
}

fn item_values(id: i64, parent_id: i64, group_id: i64, text: &str, number: i64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::Int64(parent_id),
        Value::Int64(group_id),
        Value::Text(text.into()),
        Value::Int64(number),
        Value::Float64(if id % 2 == 0 { -0.0 } else { 0.0 }),
        decimal_value(id < 0, 2, u128::from(id.unsigned_abs() % 10_000)),
        Value::Blob(id.to_le_bytes().to_vec()),
    ]
}

fn table_specs() -> Vec<TableSpec> {
    vec![
        TableSpec {
            name: "PARENTS",
            create_sql: "CREATE TABLE PARENTS (ID INT64, LABEL TEXT)",
            columns: columns(&[(ReferenceType::Int64, true), (ReferenceType::Text, true)]),
        },
        TableSpec {
            name: "ITEM",
            create_sql: "CREATE TABLE ITEM (ID INT64, PARENT_ID INT64, GROUP_ID INT64, TXT TEXT, NUM INT64, SCORE FLOAT64, AMOUNT DECIMAL, RAW BLOB)",
            columns: columns(&[
                (ReferenceType::Int64, true),
                (ReferenceType::Int64, true),
                (ReferenceType::Int64, true),
                (ReferenceType::Text, true),
                (ReferenceType::Int64, true),
                (ReferenceType::Float64, true),
                (ReferenceType::Decimal, true),
                (ReferenceType::Blob, true),
            ]),
        },
        TableSpec {
            name: "EDGE",
            create_sql: "CREATE TABLE EDGE (I INT64, T TEXT, F FLOAT64, D DECIMAL, B BLOB)",
            columns: columns(&[
                (ReferenceType::Int64, true),
                (ReferenceType::Text, true),
                (ReferenceType::Float64, true),
                (ReferenceType::Decimal, true),
                (ReferenceType::Blob, true),
            ]),
        },
        TableSpec {
            name: "SCRATCH",
            create_sql: "CREATE TABLE SCRATCH (K INT64)",
            columns: columns(&[(ReferenceType::Int64, true)]),
        },
    ]
}

fn columns(specs: &[(ReferenceType, bool)]) -> Vec<ReferenceColumn> {
    specs
        .iter()
        .map(|&(data_type, nullable)| ReferenceColumn {
            data_type,
            nullable,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Rows(Vec<Vec<Value>>),
    Affected(u64),
    Error(ErrorCode),
}

#[derive(Debug, Clone, PartialEq)]
struct StepResult(Vec<Outcome>);

impl StepResult {
    fn one(outcome: Outcome) -> Self {
        Self(vec![outcome])
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ProgramResult {
    setup_error: Option<ErrorCode>,
    steps: Vec<StepResult>,
    final_items: Outcome,
}

#[derive(Debug, Clone)]
struct ReferenceTable {
    columns: Vec<ReferenceColumn>,
    rows: BTreeMap<u64, Vec<Value>>,
    next_rowid: u64,
}

#[derive(Debug, Default)]
struct ReferenceDatabase {
    tables: BTreeMap<String, ReferenceTable>,
    transaction_before: Option<BTreeMap<String, ReferenceTable>>,
}

impl ReferenceDatabase {
    fn run(program: &GeneratedProgram) -> ProgramResult {
        let mut database = Self::default();
        for table in &program.tables {
            if let Err(code) = database.create_table(table) {
                return failed_program(code);
            }
        }
        for row in &program.seed_rows {
            if let Err(code) = database.insert(row.table, row.values.clone()) {
                return failed_program(code);
            }
        }
        let steps = program
            .operations
            .iter()
            .map(|operation| database.apply(operation))
            .collect();
        let final_items = database
            .item_rows()
            .map_or_else(Outcome::Error, Outcome::Rows);
        ProgramResult {
            setup_error: None,
            steps,
            final_items,
        }
    }

    fn create_table(&mut self, spec: &TableSpec) -> Result<(), ErrorCode> {
        if self.tables.contains_key(spec.name) {
            return Err(ErrorCode::Bind);
        }
        self.tables.insert(
            spec.name.into(),
            ReferenceTable {
                columns: spec.columns.clone(),
                rows: BTreeMap::new(),
                next_rowid: 0,
            },
        );
        Ok(())
    }

    fn insert(&mut self, table_name: &str, values: Vec<Value>) -> Result<u64, ErrorCode> {
        let table = self.tables.get_mut(table_name).ok_or(ErrorCode::Bind)?;
        if values.len() != table.columns.len() {
            return Err(ErrorCode::Bind);
        }
        for (column, value) in table.columns.iter().zip(&values) {
            if matches!(value, Value::Null) {
                if !column.nullable {
                    return Err(ErrorCode::Constraint);
                }
            } else if !column.data_type.accepts(value) {
                return Err(ErrorCode::Type);
            }
        }
        let rowid = table
            .next_rowid
            .checked_add(1)
            .ok_or(ErrorCode::ResourceLimit)?;
        table.next_rowid = rowid;
        table.rows.insert(rowid, values);
        Ok(1)
    }

    fn apply(&mut self, operation: &Operation) -> StepResult {
        match operation {
            Operation::Predicate {
                minimum,
                excluded_id,
            } => StepResult::one(self.predicate(*minimum, *excluded_id).into_outcome()),
            Operation::LeftJoin => StepResult::one(self.left_join().into_outcome()),
            Operation::Group => StepResult::one(self.grouped().into_outcome()),
            Operation::Distinct => StepResult::one(self.distinct().into_outcome()),
            Operation::Sort => StepResult::one(self.sorted().into_outcome()),
            Operation::QueryEdges => StepResult::one(self.edge_rows().into_outcome()),
            Operation::Insert { values } => StepResult::one(
                self.insert("ITEM", values.clone())
                    .map_or_else(Outcome::Error, Outcome::Affected),
            ),
            Operation::Update { group_id, delta } => StepResult::one(
                self.update(*group_id, *delta)
                    .map_or_else(Outcome::Error, Outcome::Affected),
            ),
            Operation::Delete { id } => StepResult::one(
                self.delete(*id)
                    .map_or_else(Outcome::Error, Outcome::Affected),
            ),
            Operation::Begin => {
                if self.transaction_before.is_some() {
                    StepResult::one(Outcome::Error(ErrorCode::Transaction))
                } else {
                    self.transaction_before = Some(self.tables.clone());
                    StepResult::one(Outcome::Affected(0))
                }
            }
            Operation::Rollback => match self.transaction_before.take() {
                Some(before) => {
                    self.tables = before;
                    StepResult::one(Outcome::Affected(0))
                }
                None => StepResult::one(Outcome::Error(ErrorCode::Transaction)),
            },
            Operation::Commit => {
                if self.transaction_before.take().is_some() {
                    StepResult::one(Outcome::Affected(0))
                } else {
                    StepResult::one(Outcome::Error(ErrorCode::Transaction))
                }
            }
            Operation::SchemaInvalidation => {
                if self.tables.remove("SCRATCH").is_some() {
                    StepResult(vec![
                        Outcome::Affected(0),
                        Outcome::Error(ErrorCode::SchemaChanged),
                    ])
                } else {
                    StepResult::one(Outcome::Error(ErrorCode::Bind))
                }
            }
            Operation::RecreateScratch => {
                let scratch = table_specs()
                    .into_iter()
                    .find(|table| table.name == "SCRATCH");
                StepResult::one(match scratch {
                    Some(spec) => self
                        .create_table(&spec)
                        .map_or_else(Outcome::Error, |()| Outcome::Affected(0)),
                    None => Outcome::Error(ErrorCode::Internal),
                })
            }
            Operation::ErrorCorpus { non_finite } => {
                let invalid_type = self
                    .insert(
                        "PARENTS",
                        vec![
                            Value::Text("wrong type".into()),
                            Value::Text("ordinary".into()),
                        ],
                    )
                    .map_or_else(Outcome::Error, Outcome::Affected);
                StepResult(vec![
                    Outcome::Error(ErrorCode::Bind),
                    Outcome::Error(ErrorCode::Type),
                    if non_finite.is_finite() {
                        Outcome::Rows(vec![vec![Value::Float64(*non_finite)]])
                    } else {
                        Outcome::Error(ErrorCode::Type)
                    },
                    invalid_type,
                ])
            }
            Operation::MemoryBoundary => StepResult(vec![
                Outcome::Error(ErrorCode::ResourceLimit),
                Outcome::Rows(vec![vec![Value::Int64(1)]]),
            ]),
        }
    }

    fn rows(&self, name: &str) -> Result<&ReferenceTable, ErrorCode> {
        self.tables.get(name).ok_or(ErrorCode::Bind)
    }

    fn predicate(&self, minimum: i64, excluded_id: i64) -> ModelRows {
        let table = self.rows("ITEM")?;
        let mut rows = table
            .rows
            .values()
            .filter_map(|row| {
                let id = int_at(row, 0)?;
                let number = int_at(row, 4)?;
                (number >= minimum && id != excluded_id)
                    .then(|| vec![Value::Int64(id), Value::Int64(number)])
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| int_at(row, 0));
        Ok(rows)
    }

    fn left_join(&self) -> ModelRows {
        let parents = self.rows("PARENTS")?;
        let items = self.rows("ITEM")?;
        let mut rows = Vec::new();
        for parent in parents.rows.values() {
            let Some(parent_id) = int_at(parent, 0) else {
                return Err(ErrorCode::Internal);
            };
            let mut matched = items
                .rows
                .values()
                .filter_map(|item| {
                    (int_at(item, 1) == Some(parent_id))
                        .then(|| int_at(item, 0))
                        .flatten()
                })
                .collect::<Vec<_>>();
            matched.sort_unstable();
            if matched.is_empty() {
                rows.push(vec![Value::Int64(parent_id), Value::Null]);
            } else {
                rows.extend(
                    matched
                        .into_iter()
                        .map(|item_id| vec![Value::Int64(parent_id), Value::Int64(item_id)]),
                );
            }
        }
        rows.sort_by(|left, right| {
            int_at(left, 0)
                .cmp(&int_at(right, 0))
                .then_with(|| nullable_int_cmp(left.get(1), right.get(1), true))
        });
        Ok(rows)
    }

    fn grouped(&self) -> ModelRows {
        let table = self.rows("ITEM")?;
        let mut groups = BTreeMap::<i64, u64>::new();
        for row in table.rows.values() {
            let group = int_at(row, 2).ok_or(ErrorCode::Internal)?;
            let count = groups.entry(group).or_default();
            *count = count.checked_add(1).ok_or(ErrorCode::ResourceLimit)?;
        }
        groups
            .into_iter()
            .map(|(group, count)| {
                i64::try_from(count)
                    .map(|count| vec![Value::Int64(group), Value::Int64(count)])
                    .map_err(|_| ErrorCode::ResourceLimit)
            })
            .collect()
    }

    fn distinct(&self) -> ModelRows {
        let table = self.rows("ITEM")?;
        let mut values = Vec::<Value>::new();
        for row in table.rows.values() {
            let value = row.get(3).cloned().ok_or(ErrorCode::Internal)?;
            if !values.contains(&value) {
                values.push(value);
            }
        }
        values.sort_by(text_nulls_first);
        Ok(values.into_iter().map(|value| vec![value]).collect())
    }

    fn sorted(&self) -> ModelRows {
        let table = self.rows("ITEM")?;
        let mut rows = table
            .rows
            .values()
            .map(|row| {
                Ok(vec![
                    row.first().cloned().ok_or(ErrorCode::Internal)?,
                    row.get(3).cloned().ok_or(ErrorCode::Internal)?,
                ])
            })
            .collect::<ModelRows>()?;
        rows.sort_by(|left, right| {
            text_nulls_last(
                left.get(1).unwrap_or(&Value::Null),
                right.get(1).unwrap_or(&Value::Null),
            )
            .then_with(|| int_at(right, 0).cmp(&int_at(left, 0)))
        });
        Ok(rows)
    }

    fn edge_rows(&self) -> ModelRows {
        let table = self.rows("EDGE")?;
        let mut rows = table.rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            int_at(left, 0)
                .cmp(&int_at(right, 0))
                .then_with(|| text_at(left, 1).cmp(&text_at(right, 1)))
        });
        Ok(rows)
    }

    fn item_rows(&self) -> ModelRows {
        let table = self.rows("ITEM")?;
        let mut rows = table.rows.values().cloned().collect::<Vec<_>>();
        rows.sort_by_key(|row| int_at(row, 0));
        Ok(rows)
    }

    fn update(&mut self, group_id: i64, delta: i64) -> Result<u64, ErrorCode> {
        let table = self.tables.get_mut("ITEM").ok_or(ErrorCode::Bind)?;
        let before = table.rows.clone();
        let mut affected = 0_u64;
        for row in table.rows.values_mut() {
            if int_at(row, 2) == Some(group_id) {
                let Some(number) = int_at(row, 4) else {
                    table.rows = before;
                    return Err(ErrorCode::Internal);
                };
                let Some(updated) = number.checked_add(delta) else {
                    table.rows = before;
                    return Err(ErrorCode::Type);
                };
                let Some(slot) = row.get_mut(4) else {
                    table.rows = before;
                    return Err(ErrorCode::Internal);
                };
                *slot = Value::Int64(updated);
                affected = affected.checked_add(1).ok_or(ErrorCode::ResourceLimit)?;
            }
        }
        Ok(affected)
    }

    fn delete(&mut self, id: i64) -> Result<u64, ErrorCode> {
        let table = self.tables.get_mut("ITEM").ok_or(ErrorCode::Bind)?;
        let before = table.rows.len();
        table.rows.retain(|_, row| int_at(row, 0) != Some(id));
        u64::try_from(before - table.rows.len()).map_err(|_| ErrorCode::ResourceLimit)
    }
}

type ModelRows = Result<Vec<Vec<Value>>, ErrorCode>;

trait IntoOutcome {
    fn into_outcome(self) -> Outcome;
}

impl IntoOutcome for ModelRows {
    fn into_outcome(self) -> Outcome {
        self.map_or_else(Outcome::Error, Outcome::Rows)
    }
}

fn int_at(row: &[Value], index: usize) -> Option<i64> {
    match row.get(index) {
        Some(Value::Int64(value)) => Some(*value),
        _ => None,
    }
}

fn text_at(row: &[Value], index: usize) -> Option<&str> {
    match row.get(index) {
        Some(Value::Text(value)) => Some(value),
        _ => None,
    }
}

fn nullable_int_cmp(left: Option<&Value>, right: Option<&Value>, nulls_last: bool) -> Ordering {
    match (left, right) {
        (Some(Value::Int64(left)), Some(Value::Int64(right))) => left.cmp(right),
        (Some(Value::Null) | None, Some(Value::Null) | None) => Ordering::Equal,
        (Some(Value::Null) | None, _) => {
            if nulls_last {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (_, Some(Value::Null) | None) => {
            if nulls_last {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        _ => Ordering::Equal,
    }
}

fn text_nulls_first(left: &Value, right: &Value) -> Ordering {
    text_cmp(left, right, false)
}

fn text_nulls_last(left: &Value, right: &Value) -> Ordering {
    text_cmp(left, right, true)
}

fn text_cmp(left: &Value, right: &Value, nulls_last: bool) -> Ordering {
    match (left, right) {
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if nulls_last {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (_, Value::Null) => {
            if nulls_last {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        _ => Ordering::Equal,
    }
}

struct OraSqlLiteHarness;

impl OraSqlLiteHarness {
    fn run(program: &GeneratedProgram) -> ProgramResult {
        let temp = TempDatabase::new("phase4c1-reference-model");
        let database = match Database::create(temp.path(), Options::default()) {
            Ok(database) => database,
            Err(error) => return failed_program(error.code()),
        };
        let mut connection = match database.connect() {
            Ok(connection) => connection,
            Err(error) => return failed_program(error.code()),
        };
        match actual_execute(&mut connection, "BEGIN", Parameters::none()) {
            Outcome::Affected(0) => {}
            Outcome::Error(code) => return failed_program(code),
            _ => return failed_program(ErrorCode::Internal),
        }
        for table in &program.tables {
            match actual_execute(&mut connection, table.create_sql, Parameters::none()) {
                Outcome::Affected(0) => {}
                Outcome::Error(code) => return failed_program(code),
                _ => return failed_program(ErrorCode::Internal),
            }
        }
        for row in &program.seed_rows {
            let Some(table) = program.tables.iter().find(|table| table.name == row.table) else {
                return failed_program(ErrorCode::Internal);
            };
            let parameters = match typed_parameters(&row.values, &table.columns) {
                Ok(parameters) => parameters,
                Err(code) => return failed_program(code),
            };
            let sql = insert_sql(row.table, row.values.len());
            match actual_execute(&mut connection, &sql, Parameters::positional(&parameters)) {
                Outcome::Affected(1) => {}
                Outcome::Error(code) => return failed_program(code),
                _ => return failed_program(ErrorCode::Internal),
            }
        }
        match actual_execute(&mut connection, "COMMIT", Parameters::none()) {
            Outcome::Affected(0) => {}
            Outcome::Error(code) => return failed_program(code),
            _ => return failed_program(ErrorCode::Internal),
        }
        let steps = program
            .operations
            .iter()
            .map(|operation| actual_operation(&mut connection, program, operation))
            .collect();
        let final_items = actual_query(
            &mut connection,
            "SELECT ID, PARENT_ID, GROUP_ID, TXT, NUM, SCORE, AMOUNT, RAW FROM ITEM ORDER BY ID",
            Parameters::none(),
        );
        ProgramResult {
            setup_error: None,
            steps,
            final_items,
        }
    }
}

fn actual_operation(
    connection: &mut Connection,
    program: &GeneratedProgram,
    operation: &Operation,
) -> StepResult {
    match operation {
        Operation::Predicate {
            minimum,
            excluded_id,
        } => {
            let parameters = [
                Value::Int64(*minimum).into(),
                Value::Int64(*excluded_id).into(),
            ];
            StepResult::one(actual_query(
                connection,
                "SELECT ID, NUM FROM ITEM WHERE NUM >= :1 AND ID <> :2 ORDER BY ID",
                Parameters::positional(&parameters),
            ))
        }
        Operation::LeftJoin => StepResult::one(actual_query(
            connection,
            "SELECT P.ID, I.ID FROM PARENTS P LEFT JOIN ITEM I ON I.PARENT_ID = P.ID ORDER BY P.ID, I.ID NULLS LAST",
            Parameters::none(),
        )),
        Operation::Group => StepResult::one(actual_query(
            connection,
            "SELECT GROUP_ID, COUNT(*) FROM ITEM GROUP BY GROUP_ID ORDER BY GROUP_ID",
            Parameters::none(),
        )),
        Operation::Distinct => StepResult::one(actual_query(
            connection,
            "SELECT DISTINCT TXT FROM ITEM ORDER BY TXT NULLS FIRST",
            Parameters::none(),
        )),
        Operation::Sort => StepResult::one(actual_query(
            connection,
            "SELECT ID, TXT FROM ITEM ORDER BY TXT ASC NULLS LAST, ID DESC",
            Parameters::none(),
        )),
        Operation::QueryEdges => StepResult::one(actual_query(
            connection,
            "SELECT I, T, F, D, B FROM EDGE ORDER BY I, T",
            Parameters::none(),
        )),
        Operation::Insert { values } => {
            let Some(table) = program.tables.iter().find(|table| table.name == "ITEM") else {
                return StepResult::one(Outcome::Error(ErrorCode::Bind));
            };
            let parameters = match typed_parameters(values, &table.columns) {
                Ok(parameters) => parameters,
                Err(code) => return StepResult::one(Outcome::Error(code)),
            };
            StepResult::one(actual_execute(
                connection,
                &insert_sql("ITEM", values.len()),
                Parameters::positional(&parameters),
            ))
        }
        Operation::Update { group_id, delta } => {
            let parameters = [
                ("delta", Value::Int64(*delta).into()),
                ("group_id", Value::Int64(*group_id).into()),
            ];
            StepResult::one(actual_execute(
                connection,
                "UPDATE ITEM SET NUM = NUM + :delta WHERE GROUP_ID = :group_id",
                Parameters::named(&parameters),
            ))
        }
        Operation::Delete { id } => {
            let parameters = [Value::Int64(*id).into()];
            StepResult::one(actual_execute(
                connection,
                "DELETE FROM ITEM WHERE ID = :1",
                Parameters::positional(&parameters),
            ))
        }
        Operation::Begin => {
            StepResult::one(actual_execute(connection, "BEGIN", Parameters::none()))
        }
        Operation::Rollback => {
            StepResult::one(actual_execute(connection, "ROLLBACK", Parameters::none()))
        }
        Operation::Commit => {
            StepResult::one(actual_execute(connection, "COMMIT", Parameters::none()))
        }
        Operation::SchemaInvalidation => {
            let stale = match connection.prepare("SELECT K FROM SCRATCH") {
                Ok(statement) => statement,
                Err(error) => return StepResult::one(Outcome::Error(error.code())),
            };
            let drop_result = actual_execute(connection, "DROP TABLE SCRATCH", Parameters::none());
            let stale_result = actual_query_statement(connection, &stale, Parameters::none());
            StepResult(vec![drop_result, stale_result])
        }
        Operation::RecreateScratch => StepResult::one(actual_execute(
            connection,
            "CREATE TABLE SCRATCH (K INT64)",
            Parameters::none(),
        )),
        Operation::ErrorCorpus { non_finite } => {
            let non_finite_parameter = [Value::Float64(*non_finite).into()];
            let wrong_type_parameter = [Value::Text("wrong type".into()).into()];
            StepResult(vec![
                actual_query(connection, "SELECT :1", Parameters::none()),
                actual_query(connection, "SELECT 1 / 0", Parameters::none()),
                actual_query(
                    connection,
                    "SELECT :1",
                    Parameters::positional(&non_finite_parameter),
                ),
                actual_execute(
                    connection,
                    "INSERT INTO PARENTS VALUES (:1, 'ordinary')",
                    Parameters::positional(&wrong_type_parameter),
                ),
            ])
        }
        Operation::MemoryBoundary => StepResult(vec![
            low_memory_query(),
            actual_query(connection, "SELECT 1 ORDER BY 1", Parameters::none()),
        ]),
    }
}

fn actual_execute(connection: &mut Connection, sql: &str, parameters: Parameters<'_>) -> Outcome {
    let statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(error) => return Outcome::Error(error.code()),
    };
    connection.execute(&statement, parameters).map_or_else(
        |error| Outcome::Error(error.code()),
        |result| Outcome::Affected(result.rows_affected()),
    )
}

fn actual_query(connection: &mut Connection, sql: &str, parameters: Parameters<'_>) -> Outcome {
    let statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(error) => return Outcome::Error(error.code()),
    };
    actual_query_statement(connection, &statement, parameters)
}

fn actual_query_statement(
    connection: &mut Connection,
    statement: &crate::Statement,
    parameters: Parameters<'_>,
) -> Outcome {
    let mut rows = match connection.query(statement, parameters) {
        Ok(rows) => rows,
        Err(error) => return Outcome::Error(error.code()),
    };
    let mut values = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => values.push(row.values().to_vec()),
            Ok(None) => return Outcome::Rows(values),
            Err(error) => return Outcome::Error(error.code()),
        }
    }
}

fn low_memory_query() -> Outcome {
    let temp = TempDatabase::new("phase4c1-memory-boundary");
    let options = Options::default().with_max_query_memory_bytes(1);
    let database = match Database::create(temp.path(), options) {
        Ok(database) => database,
        Err(error) => return Outcome::Error(error.code()),
    };
    let mut connection = match database.connect() {
        Ok(connection) => connection,
        Err(error) => return Outcome::Error(error.code()),
    };
    actual_query(&mut connection, "SELECT 1 ORDER BY 1", Parameters::none())
}

fn typed_parameters(
    values: &[Value],
    columns: &[ReferenceColumn],
) -> Result<Vec<ParameterValue>, ErrorCode> {
    if values.len() != columns.len() {
        return Err(ErrorCode::Internal);
    }
    values
        .iter()
        .zip(columns)
        .map(|(value, column)| match value {
            Value::Null => {
                ParameterValue::typed_null(column.data_type.public()).map_err(|error| error.code())
            }
            value => Ok(value.clone().into()),
        })
        .collect()
}

fn insert_sql(table: &str, value_count: usize) -> String {
    let placeholders = (1..=value_count)
        .map(|index| format!(":{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {table} VALUES ({placeholders})")
}

fn failed_program(code: ErrorCode) -> ProgramResult {
    ProgramResult {
        setup_error: Some(code),
        steps: Vec::new(),
        final_items: Outcome::Error(code),
    }
}

#[derive(Debug)]
struct Generator {
    state: u64,
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x6a09_e667_f3bc_c909,
        }
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn index(&mut self, length: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(length).unwrap_or(1)).unwrap_or(0)
    }

    fn small_i64(&mut self) -> i64 {
        i64::try_from(self.next() % 17).unwrap_or(0) - 8
    }
}

struct NoPanicHarness {
    _temp: TempDatabase,
    connection: Connection,
}

impl NoPanicHarness {
    fn new() -> Result<Self, ErrorCode> {
        let temp = TempDatabase::new("phase4c1-no-panic");
        let database =
            Database::create(temp.path(), Options::default()).map_err(|error| error.code())?;
        let connection = database.connect().map_err(|error| error.code())?;
        Ok(Self {
            _temp: temp,
            connection,
        })
    }

    fn execute(&mut self, seed: u64) {
        let mut generator = Generator::new(seed);
        let text = Value::Text(EDGE_TEXT[generator.index(EDGE_TEXT.len())].into());
        let integer = Value::Int64(EDGE_INTS[generator.index(EDGE_INTS.len())]);
        let outcome = match seed % 8 {
            0 => {
                let parameters = [text.into()];
                actual_query(
                    &mut self.connection,
                    "SELECT :1",
                    Parameters::positional(&parameters),
                )
            }
            1 => {
                let parameters = [integer.into()];
                actual_query(
                    &mut self.connection,
                    "SELECT :1 = :1",
                    Parameters::positional(&parameters),
                )
            }
            2 => actual_query(&mut self.connection, "SELECT 1 / 0", Parameters::none()),
            3 => {
                let parameters = [Value::Blob(vec![0, generator.next() as u8, 0xff]).into()];
                actual_query(
                    &mut self.connection,
                    "SELECT DISTINCT :1 ORDER BY 1",
                    Parameters::positional(&parameters),
                )
            }
            4 => {
                let (negative, scale) = no_panic_decimal_variant(seed);
                let parameters = [decimal_value(negative, scale, 1).into()];
                actual_query(
                    &mut self.connection,
                    "SELECT :1 + 0",
                    Parameters::positional(&parameters),
                )
            }
            5 => {
                let parameters = [Value::Float64(match seed % 3 {
                    0 => f64::NAN,
                    1 => f64::INFINITY,
                    _ => f64::NEG_INFINITY,
                })
                .into()];
                actual_query(
                    &mut self.connection,
                    "SELECT :1",
                    Parameters::positional(&parameters),
                )
            }
            6 => actual_query(&mut self.connection, "SELECT :1", Parameters::none()),
            _ => {
                let parameters = [Value::Float64(no_panic_signed_zero_variant(seed)).into()];
                actual_query(
                    &mut self.connection,
                    "SELECT :1, COUNT(*) GROUP BY :1 ORDER BY :1",
                    Parameters::positional(&parameters),
                )
            }
        };
        drop(outcome);
    }
}

fn no_panic_decimal_variant(seed: u64) -> (bool, i8) {
    let variant = seed / 8;
    (variant % 2 != 0, if variant % 4 < 2 { -38 } else { 38 })
}

fn no_panic_signed_zero_variant(seed: u64) -> f64 {
    if (seed / 8) % 2 == 0 { -0.0 } else { 0.0 }
}

fn execute_generated_case(harness: &mut NoPanicHarness, seed: u64) {
    harness.execute(seed);
}

#[test]
fn phase_4c1_fixed_seed_execution_model() {
    // Production breaks caught: divergence in any supported CRUD, predicate,
    // JOIN, grouping, DISTINCT, sort, bind, transaction, or DDL path changes a
    // hand-modeled observation or the final committed rows for its fixed seed.
    for seed in 0u64..128 {
        let program = GeneratedProgram::from_seed(seed, 4, 24);
        let expected = ReferenceDatabase::run(&program);
        let actual = OraSqlLiteHarness::run(&program);
        assert_eq!(actual, expected, "seed={seed} program={program:?}");
    }
    eprintln!("phase 4c1 reference model: 128 fixed seeds x 24 operations");
}

#[test]
fn phase_4c1_random_runtime_inputs_never_panic() {
    // Production break caught: an unchecked generated runtime path unwinds
    // instead of returning its stable ErrorCode through the public API.
    let mut harness = NoPanicHarness::new()
        .unwrap_or_else(|code| panic!("no-panic harness initialization failed: {code:?}"));
    for seed in 0u64..512 {
        let result = catch_unwind(AssertUnwindSafe(|| {
            execute_generated_case(&mut harness, seed);
        }));
        assert!(result.is_ok(), "panic for seed {seed}");
    }
    eprintln!("phase 4c1 no-panic corpus: 512 deterministic runtime inputs");
}

#[test]
fn no_panic_numeric_generator_covers_both_boundary_sides() {
    let decimals = (0u64..512)
        .filter(|seed| seed % 8 == 4)
        .map(no_panic_decimal_variant)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        decimals,
        BTreeSet::from([(false, -38), (false, 38), (true, -38), (true, 38)])
    );

    let signed_zero_bits = (0u64..512)
        .filter(|seed| seed % 8 == 7)
        .map(|seed| no_panic_signed_zero_variant(seed).to_bits())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        signed_zero_bits,
        BTreeSet::from([0.0f64.to_bits(), (-0.0f64).to_bits()])
    );
}
