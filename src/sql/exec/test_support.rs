use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};

use crate::{
    Connection, DataType, Database, Decimal, ErrorCode, Options, ParameterValue, Parameters,
    Statement, Value,
    fault::FaultPoint,
    sql::SqlLimits,
    storage::{
        Pager,
        test_support::{
            Phase4c1CrashPathKind, Phase4c1SqlStorageState, Phase4c2aIndexEntry,
            Phase4c2aStorageState, Phase4c2aWalRecord, TempDatabase, assert_commit_trace,
            phase_4c1_abort_wal_initialization, phase_4c1_aborting_crash_pager,
            phase_4c1_copy_crash_pair, phase_4c1_copy_main_only,
            phase_4c1_exact_crash_child_command, phase_4c1_main_only_crash_fixture,
            phase_4c1_recording_crash_pager, phase_4c1_recovered_lsn, phase_4c1_sql_storage_state,
            phase_4c1_validate_crash_path, phase_4c1_wal_initialization_trace,
            phase_4c2a_checkpoint, phase_4c2a_commit_malformed_index, phase_4c2a_create_pager,
            phase_4c2a_model_state, phase_4c2a_open_pager, phase_4c2a_pager_state,
            phase_4c2a_recover_and_capture, phase_4c2a_remove_recovered_files,
            phase_4c2a_wal_records,
        },
    },
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

#[test]
#[ignore = "cross-process recovery matrix"]
fn phase_4c1_sql_crash_recovery_matrix() {
    let mut crashes = run_sql_initialization_crashes();
    for fault in representative_sql_commit_faults() {
        for outcome in run_sql_child_at_fault(fault) {
            assert_sql_recovery_matches_durable_commit_record(&outcome);
            crashes += 1;
            eprintln!(
                "phase4c1 SQL {} {:?}/{}: durable_lsn={} recovered atomically",
                outcome.program, outcome.point, outcome.occurrence, outcome.recovered_lsn,
            );
        }
    }
    eprintln!("phase4c1 SQL crash matrix: {crashes} inspected child aborts");
}

fn run_sql_initialization_crashes() -> usize {
    let fixture = phase_4c1_main_only_crash_fixture();
    let trace_copy = phase_4c1_copy_main_only(fixture.path());
    let trace = phase_4c1_wal_initialization_trace(trace_copy.path());
    assert_eq!(
        trace,
        vec![
            FaultPoint::BeforeWalHeaderWrite,
            FaultPoint::AfterWalHeaderWrite,
            FaultPoint::BeforeWalSync,
            FaultPoint::AfterWalSync,
            FaultPoint::BeforeWalHeaderWrite,
            FaultPoint::AfterWalHeaderWrite,
            FaultPoint::BeforeWalSync,
            FaultPoint::AfterWalSync,
            FaultPoint::BeforeWalDirectorySync,
            FaultPoint::AfterWalDirectorySync,
        ],
        "WAL initialization must retain both header-slot and directory boundaries"
    );
    let expected = phase_4c1_sql_storage_state(trace_copy.path());
    assert_eq!(expected.lsn.0, 0);
    assert!(expected.catalog_records.is_empty());
    assert_eq!(expected.next_rowid, None);
    assert!(expected.rows.is_empty());

    for (point, occurrence) in [
        (FaultPoint::AfterWalHeaderWrite, 2),
        (FaultPoint::AfterWalDirectorySync, 1),
    ] {
        let crash_copy = phase_4c1_copy_main_only(fixture.path());
        let path = std::fs::canonicalize(crash_copy.path()).expect("canonical init crash path");
        let marker = "PHASE4C1_ABORT initialize";
        let mut command = phase_4c1_exact_crash_child_command(SQL_CRASH_CHILD);
        command
            .env(SQL_CRASH_PATH_ENV, &path)
            .env(SQL_CRASH_PROGRAM_ENV, "initialize")
            .env(SQL_CRASH_POINT_ENV, format!("{point:?}"))
            .env(SQL_CRASH_OCCURRENCE_ENV, occurrence.to_string())
            .env(
                SQL_CRASH_TRACE_ENV,
                trace
                    .iter()
                    .map(|point| format!("{point:?}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
        let output = command.output().expect("reap WAL initialization child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("running 1 test"),
            "exact child did not run: {stdout}"
        );
        assert!(
            stderr
                .lines()
                .any(|line| line == format!("{marker} {point:?} {occurrence}")),
            "wrong WAL initialization marker: {stderr}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                output.status.signal(),
                Some(6),
                "child must die from SIGABRT"
            );
        }
        #[cfg(windows)]
        assert_eq!(
            output.status.code(),
            Some(-1_073_740_791),
            "child must die from Windows abort fast-fail 0xC0000409"
        );
        assert_eq!(phase_4c1_recovered_lsn(&path).0, 0);
        assert_eq!(phase_4c1_sql_storage_state(&path), expected);
        eprintln!(
            "phase4c1 SQL initialization {point:?}/{occurrence}: empty pre-SQL state recovered"
        );
    }
    2
}

#[derive(Debug, Clone, Copy)]
enum SqlCommitFault {
    FirstPageFrame,
    LaterPageFrame,
    CommitRecord,
    BeforeWalSync,
    AfterWalSync,
    BeforePublication,
    AfterPublication,
}

fn representative_sql_commit_faults() -> [SqlCommitFault; 7] {
    [
        SqlCommitFault::FirstPageFrame,
        SqlCommitFault::LaterPageFrame,
        SqlCommitFault::CommitRecord,
        SqlCommitFault::BeforeWalSync,
        SqlCommitFault::AfterWalSync,
        SqlCommitFault::BeforePublication,
        SqlCommitFault::AfterPublication,
    ]
}

#[derive(Clone, Copy)]
struct SqlCrashProgram {
    name: &'static str,
    statements: &'static [&'static str],
}

const SQL_CRASH_PROGRAMS: [SqlCrashProgram; 4] = [
    SqlCrashProgram {
        name: "create-table",
        statements: &["CREATE TABLE T (ID INT64)"],
    },
    SqlCrashProgram {
        name: "insert-rows",
        statements: &[
            "BEGIN",
            "INSERT INTO T VALUES (1)",
            "INSERT INTO T VALUES (2)",
            "COMMIT",
        ],
    },
    SqlCrashProgram {
        name: "update-delete",
        statements: &[
            "BEGIN",
            "UPDATE T SET ID = ID + 10",
            "DELETE FROM T WHERE ID = 11",
            "COMMIT",
        ],
    },
    SqlCrashProgram {
        name: "drop-table",
        statements: &["DROP TABLE T"],
    },
];

const SQL_CRASH_CHILD: &str = "sql::exec::test_support::phase_4c1_sql_crash_child";
const SQL_CRASH_PATH_ENV: &str = "ORA_SQLLITE_PHASE4C1_PATH";
const SQL_CRASH_PROGRAM_ENV: &str = "ORA_SQLLITE_PHASE4C1_PROGRAM";
const SQL_CRASH_POINT_ENV: &str = "ORA_SQLLITE_PHASE4C1_POINT";
const SQL_CRASH_OCCURRENCE_ENV: &str = "ORA_SQLLITE_PHASE4C1_OCCURRENCE";
const SQL_CRASH_TRACE_ENV: &str = "ORA_SQLLITE_PHASE4C1_TRACE";

struct SqlCrashOutcome {
    program: &'static str,
    point: FaultPoint,
    occurrence: usize,
    recovered_lsn: u64,
    pre: Phase4c1SqlStorageState,
    post: Phase4c1SqlStorageState,
    recovered: Phase4c1SqlStorageState,
}

fn run_sql_child_at_fault(fault: SqlCommitFault) -> Vec<SqlCrashOutcome> {
    let outcomes = SQL_CRASH_PROGRAMS
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(program_index, program)| {
            let fixture = sql_crash_fixture(program_index);
            let pre = phase_4c1_sql_storage_state(fixture.path());
            let trace_copy = phase_4c1_copy_crash_pair(fixture.path());
            let (pager, faults) = phase_4c1_recording_crash_pager(trace_copy.path());
            run_sql_crash_program(pager, program.statements);
            let trace = faults.observed();
            assert_commit_trace(&trace);
            drop(faults);
            let post = phase_4c1_sql_storage_state(trace_copy.path());
            assert_eq!(post.lsn.0, pre.lsn.0 + 1, "{} commits once", program.name);
            assert_ne!(pre, post, "{} must change persistent state", program.name);

            let (point, occurrence) = sql_fault_occurrence(fault, &trace)?;
            let crash_copy = phase_4c1_copy_crash_pair(fixture.path());
            let path = std::fs::canonicalize(crash_copy.path()).expect("canonical SQL crash path");
            let marker = format!("PHASE4C1_ABORT {}", program.name);
            let mut command = phase_4c1_exact_crash_child_command(SQL_CRASH_CHILD);
            command
                .env(SQL_CRASH_PATH_ENV, &path)
                .env(SQL_CRASH_PROGRAM_ENV, program_index.to_string())
                .env(SQL_CRASH_POINT_ENV, format!("{point:?}"))
                .env(SQL_CRASH_OCCURRENCE_ENV, occurrence.to_string())
                .env(
                    SQL_CRASH_TRACE_ENV,
                    trace
                        .iter()
                        .map(|point| format!("{point:?}"))
                        .collect::<Vec<_>>()
                        .join(","),
                );
            let output = command
                .output()
                .expect("synchronously wait and reap SQL crash child");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("running 1 test"),
                "exact child did not run: {stdout}"
            );
            assert!(
                stderr
                    .lines()
                    .any(|line| { line == format!("{marker} {point:?} {occurrence}") }),
                "wrong SQL crash marker: {stderr}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(
                    output.status.signal(),
                    Some(6),
                    "child must die from SIGABRT"
                );
            }
            #[cfg(windows)]
            assert_eq!(
                output.status.code(),
                Some(-1_073_740_791),
                "child must die from Windows abort fast-fail 0xC0000409"
            );

            // This read-only production recovery scan, not child success or
            // fault position, decides whether a complete Commit Record exists.
            let recovered_lsn = phase_4c1_recovered_lsn(&path).0;
            let recovered = phase_4c1_sql_storage_state(&path);
            assert_eq!(
                phase_4c1_sql_storage_state(&path),
                recovered,
                "repeat recovery must be stable"
            );
            Some(SqlCrashOutcome {
                program: program.name,
                point,
                occurrence,
                recovered_lsn,
                pre,
                post,
                recovered,
            })
        })
        .collect::<Vec<_>>();
    assert!(
        !outcomes.is_empty(),
        "representative SQL fault is uncovered"
    );
    outcomes
}

fn sql_fault_occurrence(
    fault: SqlCommitFault,
    trace: &[FaultPoint],
) -> Option<(FaultPoint, usize)> {
    let count = |point| trace.iter().filter(|&&seen| seen == point).count();
    Some(match fault {
        SqlCommitFault::FirstPageFrame => (FaultPoint::AfterWalRecordPayloadPrefixWrite, 1),
        SqlCommitFault::LaterPageFrame => {
            if count(FaultPoint::AfterWalRecordPayloadPrefixWrite) < 3 {
                return None;
            }
            (FaultPoint::AfterWalRecordPayloadPrefixWrite, 2)
        }
        SqlCommitFault::CommitRecord => {
            let occurrence = count(FaultPoint::AfterWalRecordWrite);
            assert!(
                occurrence >= 2,
                "SQL commit trace lacks a page and Commit Record"
            );
            (FaultPoint::AfterWalRecordWrite, occurrence)
        }
        SqlCommitFault::BeforeWalSync => (FaultPoint::BeforeWalSync, 1),
        SqlCommitFault::AfterWalSync => (FaultPoint::AfterWalSync, 1),
        SqlCommitFault::BeforePublication => (FaultPoint::BeforeWalPublish, 1),
        SqlCommitFault::AfterPublication => (FaultPoint::AfterWalPublish, 1),
    })
}

fn assert_sql_recovery_matches_durable_commit_record(outcome: &SqlCrashOutcome) {
    let expected = if outcome.recovered_lsn == outcome.post.lsn.0 {
        &outcome.post
    } else {
        assert_eq!(
            outcome.recovered_lsn, outcome.pre.lsn.0,
            "recovery accepted neither the pre-state nor the complete SQL commit"
        );
        &outcome.pre
    };
    assert_eq!(
        &outcome.recovered, expected,
        "{} {:?}/{} tore catalog, next_rowid, or user rows",
        outcome.program, outcome.point, outcome.occurrence
    );
}

fn sql_crash_fixture(program_index: usize) -> TempDatabase {
    let temp = TempDatabase::new("phase4c1-sql-crash-fixture");
    let database = Database::create(temp.path(), Options::default()).expect("create SQL fixture");
    let mut connection = database.connect().expect("connect SQL fixture");
    for program in &SQL_CRASH_PROGRAMS[..program_index] {
        for &sql in program.statements {
            match actual_execute(&mut connection, sql, Parameters::none()) {
                Outcome::Affected(_) => {}
                outcome => panic!("fixture statement {sql:?} failed: {outcome:?}"),
            }
        }
    }
    drop(connection);
    drop(database);
    temp
}

fn run_sql_crash_program(pager: Pager, statements: &[&str]) {
    let mut connection = Connection::from_test_pager(pager);
    for &sql in statements {
        let statement = connection
            .prepare(sql)
            .unwrap_or_else(|error| panic!("prepare crash SQL {sql:?}: {error}"));
        connection
            .execute(&statement, Parameters::none())
            .unwrap_or_else(|error| panic!("execute crash SQL {sql:?}: {error}"));
    }
    drop(
        connection
            .begin_read()
            .expect("SQL crash program left a transaction open"),
    );
}

fn parse_sql_fault_point(name: &str) -> Option<FaultPoint> {
    [
        FaultPoint::BeforeMainExtend,
        FaultPoint::AfterMainExtend,
        FaultPoint::BeforeMainPageWrite,
        FaultPoint::AfterMainPageWrite,
        FaultPoint::BeforeMainSync,
        FaultPoint::AfterMainSync,
        FaultPoint::BeforeMainDirectorySync,
        FaultPoint::AfterMainDirectorySync,
        FaultPoint::BeforeSuperblockWrite,
        FaultPoint::AfterSuperblockWrite,
        FaultPoint::BeforeWalDirectorySync,
        FaultPoint::AfterWalDirectorySync,
        FaultPoint::BeforeWalHeaderWrite,
        FaultPoint::AfterWalHeaderWrite,
        FaultPoint::BeforeWalRecordWrite,
        FaultPoint::AfterWalRecordHeaderPrefixWrite,
        FaultPoint::AfterWalRecordPayloadPrefixWrite,
        FaultPoint::AfterWalRecordWrite,
        FaultPoint::BeforeWalSync,
        FaultPoint::AfterWalSync,
        FaultPoint::BeforeWalPublish,
        FaultPoint::AfterWalPublish,
        FaultPoint::BeforeWalTruncate,
        FaultPoint::AfterWalTruncate,
    ]
    .into_iter()
    .find(|point| format!("{point:?}") == name)
}

#[test]
#[ignore = "only launched with validated parameters by phase_4c1_sql_crash_recovery_matrix"]
fn phase_4c1_sql_crash_child() {
    let path = PathBuf::from(
        std::env::var_os(SQL_CRASH_PATH_ENV)
            .expect("invalid SQL crash configuration: missing path"),
    );
    let program_text = std::env::var(SQL_CRASH_PROGRAM_ENV).expect("invalid SQL crash program");
    let point = parse_sql_fault_point(
        &std::env::var(SQL_CRASH_POINT_ENV).expect("invalid SQL crash point"),
    )
    .expect("invalid SQL crash point");
    let occurrence_text =
        std::env::var(SQL_CRASH_OCCURRENCE_ENV).expect("invalid SQL crash occurrence");
    let occurrence: usize = occurrence_text
        .parse()
        .expect("invalid SQL crash occurrence");
    assert!(occurrence > 0 && occurrence_text == occurrence.to_string());
    let trace = std::env::var(SQL_CRASH_TRACE_ENV)
        .expect("invalid SQL crash trace")
        .split(',')
        .map(|name| parse_sql_fault_point(name).expect("invalid SQL crash trace"))
        .collect::<Vec<_>>();
    assert!(
        trace.iter().filter(|&&seen| seen == point).count() >= occurrence,
        "invalid SQL crash occurrence"
    );
    if program_text == "initialize" {
        assert_eq!(
            trace.last(),
            Some(&FaultPoint::AfterWalDirectorySync),
            "invalid WAL initialization trace"
        );
        let path = phase_4c1_validate_crash_path(path, Phase4c1CrashPathKind::Initialization);
        phase_4c1_abort_wal_initialization(
            &path,
            trace,
            point,
            occurrence,
            "PHASE4C1_ABORT initialize".to_owned(),
        );
        panic!("configured WAL initialization crash point was not reached");
    }
    let program_index: usize = program_text.parse().expect("invalid SQL crash program");
    let program = *SQL_CRASH_PROGRAMS
        .get(program_index)
        .expect("invalid SQL crash program");
    assert_commit_trace(&trace);
    let path = phase_4c1_validate_crash_path(path, Phase4c1CrashPathKind::Sql);
    let pager = phase_4c1_aborting_crash_pager(
        &path,
        trace,
        point,
        occurrence,
        format!("PHASE4C1_ABORT {}", program.name),
    );
    run_sql_crash_program(pager, program.statements);
    panic!("configured SQL crash point was not reached");
}

#[test]
fn phase_4c1_readers_never_observe_partial_sql_commit() {
    // Production breaks caught: query Rows refreshing their read snapshot, a
    // writer publishing page/catalog changes before COMMIT, or writer
    // acquisition waiting instead of immediately returning Busy.
    let temp = TempDatabase::new("phase4c1-sql-snapshot-stress");
    let options = Options::default()
        .with_checkpoint_wal_bytes(128 * 1024 * 1024)
        .with_max_wal_bytes(256 * 1024 * 1024);
    let database = Database::create(temp.path(), options).expect("create stress DB");
    let writer_database = database.clone();
    let first_reader_database = database.clone();
    let second_reader_database = database.clone();
    let contender_database = database.clone();
    let fresh_reader_database = database.clone();

    let mut setup = database.connect().expect("connect setup session");
    for sql in [
        "CREATE TABLE T (ID INT64, V INT64)",
        "BEGIN",
        "INSERT INTO T VALUES (1, 0)",
        "INSERT INTO T VALUES (2, 0)",
        "COMMIT",
    ] {
        assert_eq!(
            actual_execute(&mut setup, sql, Parameters::none()),
            Outcome::Affected(if sql.starts_with("INSERT") { 1 } else { 0 }),
            "setup statement: {sql}"
        );
    }
    drop(setup);

    for round in 0_i64..64 {
        let before = vec![
            vec![Value::Int64(1), Value::Int64(round)],
            vec![Value::Int64(2), Value::Int64(round)],
        ];
        let after = vec![
            vec![Value::Int64(1), Value::Int64(round + 1)],
            vec![Value::Int64(2), Value::Int64(round + 1)],
        ];

        let mut first_reader = first_reader_database
            .connect()
            .expect("connect first old reader");
        let first_select = first_reader
            .prepare("SELECT ID, V FROM T ORDER BY ID")
            .expect("prepare first old reader");
        let mut first_old_rows = first_reader
            .query(&first_select, Parameters::none())
            .expect("open first old Rows");

        let mut second_reader = second_reader_database
            .connect()
            .expect("connect second old reader");
        let second_select = second_reader
            .prepare("SELECT ID, V FROM T ORDER BY ID")
            .expect("prepare second old reader");
        let mut second_old_rows = second_reader
            .query(&second_select, Parameters::none())
            .expect("open second old Rows");

        let mut writer = writer_database.connect().expect("connect writer");
        for sql in ["BEGIN", "UPDATE T SET V = V + 1"] {
            assert_eq!(
                actual_execute(&mut writer, sql, Parameters::none()),
                Outcome::Affected(if sql.starts_with("UPDATE") { 2 } else { 0 }),
                "round {round} writer statement: {sql}"
            );
        }

        let mut contender = contender_database.connect().expect("connect contender");
        assert_eq!(
            contender
                .begin_write()
                .expect_err("second writer must be Busy immediately")
                .code(),
            ErrorCode::Busy,
            "round {round}"
        );

        assert_eq!(
            actual_execute(&mut writer, "COMMIT", Parameters::none()),
            Outcome::Affected(0),
            "round {round} commit"
        );

        assert_eq!(
            collect_public_rows(&mut first_old_rows),
            before,
            "round {round}"
        );
        assert_eq!(
            collect_public_rows(&mut second_old_rows),
            before,
            "round {round}"
        );
        drop(first_old_rows);
        drop(second_old_rows);

        let mut fresh_reader = fresh_reader_database
            .connect()
            .expect("connect fresh reader");
        assert_eq!(
            actual_query(
                &mut fresh_reader,
                "SELECT ID, V FROM T ORDER BY ID",
                Parameters::none(),
            ),
            Outcome::Rows(after),
            "round {round} fresh snapshot"
        );
    }
}

#[derive(Debug, Clone)]
struct IndexModelRow {
    a: Option<i64>,
    b: i64,
    c: String,
}

impl IndexModelRow {
    fn values(&self) -> Vec<Value> {
        vec![
            self.a.map_or(Value::Null, Value::Int64),
            Value::Int64(self.b),
            Value::Text(self.c.clone()),
        ]
    }
}

#[derive(Debug, Clone)]
struct IndexModelDef {
    columns: Vec<u16>,
    unique: bool,
}

fn phase4c2a_reference_index_key(values: &[Value], rowid: u64) -> Vec<u8> {
    assert_ne!(rowid, 0, "reference index ROWID must be nonzero");
    let mut physical = Vec::new();
    for value in values {
        match value {
            Value::Null => physical.push(1),
            Value::Int64(value) => {
                physical.push(0);
                physical.extend_from_slice(&(*value as u64 ^ (1_u64 << 63)).to_be_bytes());
            }
            Value::Text(value) => {
                physical.push(0);
                for byte in value.as_bytes() {
                    if *byte == 0 {
                        physical.extend_from_slice(&[0, 0xff]);
                    } else {
                        physical.push(*byte);
                    }
                }
                physical.extend_from_slice(&[0, 0]);
            }
            other => panic!("unsupported Phase 4C2-A reference index value {other:?}"),
        }
    }
    physical.extend_from_slice(&rowid.to_be_bytes());
    physical
}

fn phase4c2a_reference_index_entries(
    rows: &BTreeMap<u64, IndexModelRow>,
    definition: &IndexModelDef,
) -> Vec<Phase4c2aIndexEntry> {
    let mut expected = rows
        .iter()
        .map(|(&rowid, row)| {
            let values = row.values();
            let logical = definition
                .columns
                .iter()
                .map(|&ordinal| values[usize::from(ordinal)].clone())
                .collect::<Vec<_>>();
            Phase4c2aIndexEntry {
                physical: phase4c2a_reference_index_key(&logical, rowid),
                logical,
                rowid,
            }
        })
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.physical.cmp(&right.physical));
    expected
}

#[test]
fn phase_4c2a_reference_index_key_v1_oracle_rejects_physical_mutations() {
    let rows = BTreeMap::from([
        (
            2,
            IndexModelRow {
                a: Some(-1),
                b: 0,
                c: "a\0".to_owned(),
            },
        ),
        (
            256,
            IndexModelRow {
                a: Some(-1),
                b: 0,
                c: "a\0".to_owned(),
            },
        ),
        (
            3,
            IndexModelRow {
                a: None,
                b: 0,
                c: "a\0".to_owned(),
            },
        ),
    ]);
    let definition = IndexModelDef {
        columns: vec![0],
        unique: false,
    };
    let expected = phase4c2a_reference_index_entries(&rows, &definition);
    assert_eq!(
        expected
            .iter()
            .map(|entry| entry.physical.clone())
            .collect::<Vec<_>>(),
        vec![
            vec![
                0, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 2,
            ],
            vec![
                0, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 1, 0,
            ],
            vec![1, 0, 0, 0, 0, 0, 0, 0, 3],
        ]
    );
    assert_eq!(
        phase4c2a_reference_index_key(
            &[Value::Int64(0), Value::Text("a\0".to_owned())],
            0x0102_0304_0506_0708,
        ),
        vec![
            0, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, b'a', 0, 0xff, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8,
        ]
    );

    let mut wrong_marker = expected.clone();
    wrong_marker[0].physical[0] = 1;
    assert_ne!(wrong_marker, expected);
    let mut wrong_order = expected.clone();
    wrong_order.swap(0, 1);
    assert_ne!(wrong_order, expected);
    let mut wrong_rowid_bytes = expected.clone();
    *wrong_rowid_bytes[0].physical.last_mut().unwrap() = 3;
    assert_ne!(wrong_rowid_bytes, expected);
}

struct IndexReferenceModel {
    temp: TempDatabase,
    pager: Option<Pager>,
    connection: Option<Connection>,
    entropy: u64,
    step: usize,
    next_rowid: u64,
    next_a: i64,
    rows: BTreeMap<u64, IndexModelRow>,
    indexes: BTreeMap<String, IndexModelDef>,
    schema_version: u64,
    object_counter: u64,
    feature_bits: u64,
    last_expected: StepResult,
    last_actual: StepResult,
    last_state: Option<Phase4c2aStorageState>,
    coverage: BTreeMap<&'static str, usize>,
    previous_table_root: Option<u64>,
    previous_index_roots: BTreeMap<String, u64>,
    table_root_changes: usize,
    index_root_changes: usize,
    reopens: usize,
}

impl IndexReferenceModel {
    fn new(seed: u64) -> Self {
        let temp = TempDatabase::new("phase4c2a-fixed-seed");
        let pager = phase_4c2a_create_pager(&temp);
        let mut connection = Connection::from_test_pager(pager.clone());
        assert_eq!(
            actual_execute(
                &mut connection,
                "CREATE TABLE T (A INT64, B INT64, C TEXT)",
                Parameters::none(),
            ),
            Outcome::Affected(0)
        );
        let state = phase_4c2a_pager_state(&pager, temp.path()).expect("capture model baseline");
        let table = state.tables.get("T").expect("model table exists");
        assert_eq!(table.schema_version, 1);
        assert_eq!(state.object_counter, 5);
        assert_eq!(state.feature_bits, 1);
        Self {
            temp,
            pager: Some(pager),
            connection: Some(connection),
            entropy: seed,
            step: 0,
            next_rowid: 0,
            next_a: 1,
            rows: BTreeMap::new(),
            indexes: BTreeMap::new(),
            schema_version: 1,
            object_counter: 5,
            feature_bits: 1,
            last_expected: StepResult(Vec::new()),
            last_actual: StepResult(Vec::new()),
            last_state: None,
            coverage: BTreeMap::new(),
            previous_table_root: Some(table.root.page_id),
            previous_index_roots: BTreeMap::new(),
            table_root_changes: 0,
            index_root_changes: 0,
            reopens: 0,
        }
    }

    fn connection(&mut self) -> &mut Connection {
        self.connection.as_mut().expect("model connection is open")
    }

    fn next_entropy(&mut self) -> u64 {
        let mut value = self.entropy;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.entropy = value;
        value
    }

    fn record(&mut self, name: &'static str, expected: Vec<Outcome>, actual: Vec<Outcome>) {
        *self.coverage.entry(name).or_default() += 1;
        self.last_expected = StepResult(expected);
        self.last_actual = StepResult(actual);
    }

    fn execute(&mut self, sql: &str) -> Outcome {
        actual_execute(self.connection(), sql, Parameters::none())
    }

    fn usable_probe(&mut self) -> Outcome {
        actual_query(self.connection(), "SELECT 1", Parameters::none())
    }

    fn allocate_row(&mut self, a: Option<i64>, b: i64, c: String) -> (u64, IndexModelRow) {
        self.next_rowid += 1;
        (self.next_rowid, IndexModelRow { a, b, c })
    }

    fn insert_sql(row: &IndexModelRow) -> String {
        let a = row
            .a
            .map_or_else(|| "NULL".to_owned(), |value| value.to_string());
        format!("INSERT INTO T VALUES ({a}, {}, '{}')", row.b, row.c)
    }

    fn insert_normal(&mut self, label: &'static str, null_a: bool, text_len: usize) {
        if self.rows.len() >= 32 {
            self.delete_oldest("bounded-delete");
            return;
        }
        let a = if null_a {
            None
        } else {
            let value = self.next_a;
            self.next_a += 1;
            Some(value)
        };
        let b = i64::try_from(self.next_entropy() % 17).expect("small deterministic value");
        let text = if text_len == 0 {
            format!("v{:016x}", self.next_entropy())
        } else {
            "x".repeat(text_len)
        };
        let row = IndexModelRow { a, b, c: text };
        let sql = Self::insert_sql(&row);
        let actual = self.execute(&sql);
        let (rowid, row) = self.allocate_row(row.a, row.b, row.c);
        self.rows.insert(rowid, row);
        self.record(label, vec![Outcome::Affected(1)], vec![actual]);
    }

    fn delete_oldest(&mut self, label: &'static str) {
        let Some((rowid, row)) = self
            .rows
            .iter()
            .next()
            .map(|(&rowid, row)| (rowid, row.clone()))
        else {
            self.insert_normal("insert-empty", false, 0);
            return;
        };
        let predicate = row.a.map_or_else(
            || format!("A IS NULL AND B = {} AND C = '{}'", row.b, row.c),
            |a| format!("A = {a}"),
        );
        let sql = format!("DELETE FROM T WHERE {predicate}");
        let actual = self.execute(&sql);
        let expected_count = if row.a.is_some() {
            self.rows
                .values()
                .filter(|candidate| candidate.a == row.a)
                .count()
        } else {
            1
        };
        let remove = self
            .rows
            .iter()
            .filter_map(|(&id, candidate)| {
                ((row.a.is_some() && candidate.a == row.a) || id == rowid).then_some(id)
            })
            .collect::<Vec<_>>();
        for id in remove {
            self.rows.remove(&id);
        }
        self.record(
            label,
            vec![Outcome::Affected(expected_count as u64)],
            vec![actual],
        );
    }

    fn create_index(&mut self, name: &'static str, columns: &[u16], unique: bool) {
        if self.indexes.contains_key(name) {
            self.update_one();
            return;
        }
        let column_sql = match columns {
            [0] => "A",
            [1] => "B",
            [1, 2] => "B, C",
            _ => unreachable!(),
        };
        let sql = format!(
            "CREATE {}INDEX {name} ON T ({column_sql})",
            if unique { "UNIQUE " } else { "" }
        );
        let actual = self.execute(&sql);
        self.indexes.insert(
            name.to_owned(),
            IndexModelDef {
                columns: columns.to_vec(),
                unique,
            },
        );
        self.schema_version += 1;
        self.object_counter += 1;
        self.feature_bits = 3;
        self.record("create-index", vec![Outcome::Affected(0)], vec![actual]);
    }

    fn drop_index(&mut self, name: &'static str) {
        if self.indexes.remove(name).is_none() {
            self.insert_normal("insert-instead-of-drop", false, 0);
            return;
        }
        let actual = self.execute(&format!("DROP INDEX {name}"));
        self.schema_version += 1;
        self.record("drop-index", vec![Outcome::Affected(0)], vec![actual]);
    }

    fn update_one(&mut self) {
        let Some((rowid, before)) = self
            .rows
            .iter()
            .next()
            .map(|(&rowid, row)| (rowid, row.clone()))
        else {
            self.insert_normal("insert-before-update", false, 0);
            return;
        };
        let new_b = before.b + 37;
        let new_c = format!("u{:016x}", self.next_entropy());
        let predicate = before.a.map_or_else(
            || format!("A IS NULL AND B = {} AND C = '{}'", before.b, before.c),
            |a| format!("A = {a}"),
        );
        let actual = self.execute(&format!(
            "UPDATE T SET B = {new_b}, C = '{new_c}' WHERE {predicate}"
        ));
        let expected_count = if before.a.is_some() {
            self.rows
                .values()
                .filter(|candidate| candidate.a == before.a)
                .count()
        } else {
            1
        };
        let targets = self
            .rows
            .iter()
            .filter_map(|(&id, candidate)| {
                ((before.a.is_some() && candidate.a == before.a) || id == rowid).then_some(id)
            })
            .collect::<Vec<_>>();
        for id in targets {
            let row = self.rows.get_mut(&id).expect("selected model row");
            row.b = new_b;
            row.c = new_c.clone();
        }
        self.record(
            "update",
            vec![Outcome::Affected(expected_count as u64)],
            vec![actual],
        );
    }

    fn duplicate_error(&mut self) {
        let Some(existing) = self.rows.values().find_map(|row| row.a) else {
            self.insert_normal("insert-before-duplicate", false, 0);
            return;
        };
        let actual = self.execute(&format!(
            "INSERT INTO T VALUES ({existing}, 0, 'duplicate')"
        ));
        let probe = self.usable_probe();
        self.record(
            "duplicate-error",
            vec![
                Outcome::Error(ErrorCode::Constraint),
                Outcome::Rows(vec![vec![Value::Int64(1)]]),
            ],
            vec![actual, probe],
        );
    }

    fn oversized_error(&mut self) {
        let actual = self.execute(&format!(
            "INSERT INTO T VALUES ({}, 7, '{}')",
            self.next_a,
            "z".repeat(1014)
        ));
        let probe = self.usable_probe();
        self.record(
            "key-limit-error",
            vec![
                Outcome::Error(ErrorCode::ResourceLimit),
                Outcome::Rows(vec![vec![Value::Int64(1)]]),
            ],
            vec![actual, probe],
        );
    }

    fn update_conflict_error(&mut self) {
        let values = self
            .rows
            .values()
            .filter_map(|row| row.a)
            .take(2)
            .collect::<Vec<_>>();
        if values.len() != 2 {
            self.insert_normal("insert-before-conflict", false, 0);
            return;
        }
        let actual = self.execute(&format!(
            "UPDATE T SET A = {} WHERE A = {}",
            values[0], values[1]
        ));
        let probe = self.usable_probe();
        self.record(
            "update-conflict",
            vec![
                Outcome::Error(ErrorCode::Constraint),
                Outcome::Rows(vec![vec![Value::Int64(1)]]),
            ],
            vec![actual, probe],
        );
    }

    fn transaction_commit(&mut self) {
        if self.rows.len() >= 32 {
            self.delete_oldest("bounded-delete");
            return;
        }
        let a = self.next_a;
        self.next_a += 1;
        let row = IndexModelRow {
            a: Some(a),
            b: i64::try_from(self.next_entropy() % 17).unwrap(),
            c: format!("tx-{a}"),
        };
        let outcomes = vec![
            self.execute("BEGIN"),
            self.execute(&Self::insert_sql(&row)),
            self.usable_probe(),
            self.execute("COMMIT"),
        ];
        let (rowid, row) = self.allocate_row(row.a, row.b, row.c);
        self.rows.insert(rowid, row);
        self.record(
            "transaction-commit",
            vec![
                Outcome::Affected(0),
                Outcome::Affected(1),
                Outcome::Rows(vec![vec![Value::Int64(1)]]),
                Outcome::Affected(0),
            ],
            outcomes,
        );
    }

    fn transaction_rollback(&mut self) {
        let row = IndexModelRow {
            a: Some(self.next_a),
            b: 99,
            c: "rolled-back".to_owned(),
        };
        let outcomes = vec![
            self.execute("BEGIN"),
            self.execute(&Self::insert_sql(&row)),
            self.usable_probe(),
            self.execute("ROLLBACK"),
        ];
        self.record(
            "transaction-rollback",
            vec![
                Outcome::Affected(0),
                Outcome::Affected(1),
                Outcome::Rows(vec![vec![Value::Int64(1)]]),
                Outcome::Affected(0),
            ],
            outcomes,
        );
    }

    fn transaction_error_then_commit(&mut self) {
        let Some(existing) = self.rows.values().find_map(|row| row.a) else {
            self.insert_normal("insert-before-transaction-error", false, 0);
            return;
        };
        if self.rows.len() >= 32 {
            self.delete_oldest("bounded-delete");
            return;
        }
        let a = self.next_a;
        self.next_a += 1;
        let row = IndexModelRow {
            a: Some(a),
            b: 77,
            c: "after-error".to_owned(),
        };
        let outcomes = vec![
            self.execute("BEGIN"),
            self.execute(&format!(
                "INSERT INTO T VALUES ({existing}, 0, 'duplicate')"
            )),
            self.usable_probe(),
            self.execute(&Self::insert_sql(&row)),
            self.execute("COMMIT"),
        ];
        let (rowid, row) = self.allocate_row(row.a, row.b, row.c);
        self.rows.insert(rowid, row);
        self.record(
            "transaction-error-usable",
            vec![
                Outcome::Affected(0),
                Outcome::Error(ErrorCode::Constraint),
                Outcome::Rows(vec![vec![Value::Int64(1)]]),
                Outcome::Affected(1),
                Outcome::Affected(0),
            ],
            outcomes,
        );
    }

    fn read_probe(&mut self) {
        let actual = self.usable_probe();
        self.record(
            "read-probe",
            vec![Outcome::Rows(vec![vec![Value::Int64(1)]])],
            vec![actual],
        );
    }

    fn apply_next_operation(&mut self) {
        if self.step >= 512 && self.step % 32 != 0 {
            self.read_probe();
            self.step += 1;
            self.last_state = None;
            return;
        }
        match self.step % 21 {
            0 => self.create_index("IX_T_B", &[1], false),
            1 => self.create_index("UX_T_A", &[0], true),
            2 => self.create_index("IX_T_BC", &[1, 2], false),
            3 => self.insert_normal("insert", false, 0),
            4 => self.insert_normal("insert-null", true, 0),
            5 => self.update_one(),
            6 => self.delete_oldest("delete"),
            7 => self.transaction_commit(),
            8 => self.transaction_rollback(),
            9 => self.duplicate_error(),
            10 => self.oversized_error(),
            11 => self.insert_normal("key-limit-boundary", false, 980),
            12 => self.update_conflict_error(),
            13 => self.transaction_error_then_commit(),
            14 => self.drop_index("IX_T_B"),
            15 => self.create_index("IX_T_B", &[1], false),
            16 => self.drop_index("IX_T_BC"),
            17 => self.create_index("IX_T_BC", &[1, 2], false),
            18 => self.drop_index("UX_T_A"),
            19 => self.create_index("UX_T_A", &[0], true),
            20 => self.insert_normal("insert-split-pressure", false, 96),
            _ => unreachable!(),
        }
        self.step += 1;
        self.last_state = None;
    }

    fn assert_rows_affected_and_error_class(&self) {
        assert_eq!(
            self.last_actual, self.last_expected,
            "step={} seed-state={:#x}",
            self.step, self.entropy
        );
    }

    fn expected_rows(&self) -> Vec<Vec<Value>> {
        self.rows.values().map(IndexModelRow::values).collect()
    }

    fn assert_visible_rows_match_database(&mut self) {
        let actual = actual_query(
            self.connection(),
            "SELECT A, B, C FROM T",
            Parameters::none(),
        );
        assert_eq!(
            actual,
            Outcome::Rows(self.expected_rows()),
            "step={}",
            self.step
        );
        let state = self.capture();
        let table = state.tables.get("T").expect("model table remains visible");
        assert_eq!(
            table.schema_version, self.schema_version,
            "step={}",
            self.step
        );
        assert_eq!(
            state.object_counter, self.object_counter,
            "step={}",
            self.step
        );
        assert_eq!(state.feature_bits, self.feature_bits, "step={}", self.step);
        let physical_rows = table
            .rows
            .iter()
            .map(|(rowid, values)| (*rowid, values.clone()))
            .collect::<BTreeMap<_, _>>();
        let expected = self
            .rows
            .iter()
            .map(|(&rowid, row)| (rowid, row.values()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(physical_rows, expected, "step={}", self.step);
        if self.previous_table_root.replace(table.root.page_id) != Some(table.root.page_id) {
            self.table_root_changes += 1;
        }
        self.last_state = Some(state);
    }

    fn capture(&self) -> Phase4c2aStorageState {
        phase_4c2a_model_state(
            self.pager.as_ref().expect("model pager is open"),
            self.temp.path(),
        )
        .expect("capture fixed-seed indexed state")
    }

    fn assert_each_row_has_one_entry_per_index(&mut self) {
        let state = self
            .last_state
            .clone()
            .expect("visible-row assertion captures this step's physical state");
        assert_eq!(
            state.indexes.keys().collect::<Vec<_>>(),
            self.indexes.keys().collect::<Vec<_>>(),
            "step={}",
            self.step
        );
        for (name, expected_index) in &self.indexes {
            let actual = &state.indexes[name];
            assert_eq!(
                actual.unique, expected_index.unique,
                "{name} step={}",
                self.step
            );
            assert_eq!(
                actual.column_ordinals, expected_index.columns,
                "{name} step={}",
                self.step
            );
            assert_eq!(
                actual.entries.len(),
                self.rows.len(),
                "{name} step={}",
                self.step
            );
            if self
                .previous_index_roots
                .insert(name.clone(), actual.root.page_id)
                .is_some_and(|previous| previous != actual.root.page_id)
            {
                self.index_root_changes += 1;
            }
        }
        self.previous_index_roots
            .retain(|name, _| self.indexes.contains_key(name));
    }

    fn assert_each_entry_names_one_visible_row(&self) {
        let state = self
            .last_state
            .as_ref()
            .expect("visible-row assertion captures this step's physical state");
        for (name, definition) in &self.indexes {
            let expected = phase4c2a_reference_index_entries(&self.rows, definition);
            assert_eq!(
                state.indexes[name].entries, expected,
                "{name} raw physical sequence differs at step={}",
                self.step
            );
        }
    }

    fn reopen_and_recheck(&mut self) {
        phase_4c2a_checkpoint(self.pager.as_ref().expect("model pager is open"))
            .expect("checkpoint fixed-seed model before reopen");
        self.last_state = None;
        let before = phase_4c2a_pager_state(
            self.pager.as_ref().expect("model pager is open"),
            self.temp.path(),
        )
        .expect("deep-validate fixed-seed model before reopen");
        drop(self.connection.take());
        drop(self.pager.take());
        let pager = phase_4c2a_open_pager(self.temp.path()).expect("reopen fixed-seed model");
        let after =
            phase_4c2a_pager_state(&pager, self.temp.path()).expect("capture reopened model");
        assert_eq!(
            after, before,
            "reopen changed fixed-seed state at step {}",
            self.step
        );
        self.connection = Some(Connection::from_test_pager(pager.clone()));
        self.pager = Some(pager);
        self.last_state = None;
        self.reopens += 1;
        self.assert_visible_rows_match_database();
        self.assert_each_row_has_one_entry_per_index();
        self.assert_each_entry_names_one_visible_row();
    }

    fn finish(&self) {
        for required in [
            "create-index",
            "drop-index",
            "insert",
            "insert-null",
            "update",
            "delete",
            "transaction-commit",
            "transaction-rollback",
            "transaction-error-usable",
            "duplicate-error",
            "update-conflict",
            "key-limit-error",
            "key-limit-boundary",
            "insert-split-pressure",
            "read-probe",
        ] {
            assert!(
                self.coverage.get(required).copied().unwrap_or(0) > 0,
                "missing {required}"
            );
        }
        assert!(
            self.table_root_changes > 0,
            "table root never changed under split pressure"
        );
        assert!(
            self.index_root_changes > 0,
            "index root never changed under split pressure"
        );
        eprintln!(
            "phase4c2a fixed-seed model: steps={} rows={} coverage={:?} table_root_changes={} index_root_changes={} reopens={}",
            self.step,
            self.rows.len(),
            self.coverage,
            self.table_root_changes,
            self.index_root_changes,
            self.reopens,
        );
    }
}

#[test]
#[ignore = "run by the dedicated Phase 4C2-A CI job"]
fn phase_4c2a_fixed_seed_index_model() {
    let mut model = IndexReferenceModel::new(0x4c2a_2026_0918);
    for step in 0..25_000 {
        model.apply_next_operation();
        model.assert_rows_affected_and_error_class();
        model.assert_visible_rows_match_database();
        model.assert_each_row_has_one_entry_per_index();
        model.assert_each_entry_names_one_visible_row();
        if step % 251 == 0 {
            model.reopen_and_recheck();
        }
        if step > 0 && step % 250 == 0 {
            eprintln!("phase4c2a fixed-seed progress: {step}/25000");
        }
    }
    model.finish();
}

#[derive(Debug, Clone, Copy)]
enum Phase4c2aCrashScenario {
    CreateOrdinary,
    CreateUnique,
    InsertTwoIndexes,
    UpdateSplitTableAndIndexRoots,
    DeleteAndCollapseIndexRoot,
    DropIndex,
    DropTableWithIndexes,
}

impl Phase4c2aCrashScenario {
    const ALL: [Self; 7] = [
        Self::CreateOrdinary,
        Self::CreateUnique,
        Self::InsertTwoIndexes,
        Self::UpdateSplitTableAndIndexRoots,
        Self::DeleteAndCollapseIndexRoot,
        Self::DropIndex,
        Self::DropTableWithIndexes,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::CreateOrdinary => "create-ordinary",
            Self::CreateUnique => "create-unique",
            Self::InsertTwoIndexes => "insert-two-indexes",
            Self::UpdateSplitTableAndIndexRoots => "update-split-table-index-roots",
            Self::DeleteAndCollapseIndexRoot => "delete-collapse-index-root",
            Self::DropIndex => "drop-index",
            Self::DropTableWithIndexes => "drop-table-with-indexes",
        }
    }

    fn operation(self) -> Vec<String> {
        match self {
            Self::CreateOrdinary => vec!["CREATE INDEX IX_T_B ON T (B)".to_owned()],
            Self::CreateUnique => vec!["CREATE UNIQUE INDEX UX_T_A ON T (A)".to_owned()],
            Self::InsertTwoIndexes => {
                vec!["INSERT INTO T VALUES (200, 'two-indexes', 200)".to_owned()]
            }
            Self::UpdateSplitTableAndIndexRoots => vec![
                "BEGIN".to_owned(),
                phase4c2a_split_insert_sql(),
                phase4c2a_split_update_sql(),
                "COMMIT".to_owned(),
            ],
            Self::DeleteAndCollapseIndexRoot => {
                vec!["DELETE FROM T WHERE A < 95".to_owned()]
            }
            Self::DropIndex => vec!["DROP INDEX IX_T_B".to_owned()],
            Self::DropTableWithIndexes => vec!["DROP TABLE T".to_owned()],
        }
    }
}

fn phase4c2a_split_insert_sql() -> String {
    "INSERT INTO T VALUES (1000, 'xxxxxxxxxxxxxxxx', 0)".to_owned()
}

fn phase4c2a_split_update_sql() -> String {
    format!("UPDATE T SET B = '{}'", "changed".repeat(16))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase4c2aFaultFamily {
    CatalogPageFrame,
    TablePageFrame,
    IndexPageFrame,
    FreelistPageFrame,
    OtherPageFrame,
    CommitRecord,
    WalSync,
    Publication,
    MainSync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Phase4c2aFaultTarget {
    family: Phase4c2aFaultFamily,
    point: FaultPoint,
    occurrence: usize,
}

const PHASE4C2A_CRASH_CHILD: &str = "sql::exec::test_support::phase_4c2a_index_crash_child";
const PHASE4C2A_PATH_ENV: &str = "ORA_SQLLITE_PHASE4C2A_PATH";
const PHASE4C2A_SCENARIO_ENV: &str = "ORA_SQLLITE_PHASE4C2A_SCENARIO";
const PHASE4C2A_POINT_ENV: &str = "ORA_SQLLITE_PHASE4C2A_POINT";
const PHASE4C2A_OCCURRENCE_ENV: &str = "ORA_SQLLITE_PHASE4C2A_OCCURRENCE";
const PHASE4C2A_TRACE_ENV: &str = "ORA_SQLLITE_PHASE4C2A_TRACE";

fn phase4c2a_execute_statements(pager: Pager, statements: &[String]) {
    let borrowed = statements.iter().map(String::as_str).collect::<Vec<_>>();
    run_sql_crash_program(pager, &borrowed);
}

fn phase4c2a_fixture(scenario: Phase4c2aCrashScenario) -> TempDatabase {
    let temp = TempDatabase::new("phase4c2a-crash-fixture");
    let pager = phase_4c2a_create_pager(&temp);
    let mut connection = Connection::from_test_pager(pager.clone());
    for sql in ["CREATE TABLE T (A INT64, B TEXT, C INT64)"] {
        assert_eq!(
            actual_execute(&mut connection, sql, Parameters::none()),
            Outcome::Affected(0)
        );
    }
    let row_count = match scenario {
        Phase4c2aCrashScenario::UpdateSplitTableAndIndexRoots => 37,
        Phase4c2aCrashScenario::DeleteAndCollapseIndexRoot => 96,
        _ => 12,
    };
    let text_len = match scenario {
        Phase4c2aCrashScenario::UpdateSplitTableAndIndexRoots => 16,
        _ => 96,
    };
    assert_eq!(
        actual_execute(&mut connection, "BEGIN", Parameters::none()),
        Outcome::Affected(0)
    );
    for value in 0..row_count {
        let sql = format!(
            "INSERT INTO T VALUES ({value}, '{}', {})",
            "x".repeat(text_len),
            row_count - value
        );
        assert_eq!(
            actual_execute(&mut connection, &sql, Parameters::none()),
            Outcome::Affected(1)
        );
    }
    assert_eq!(
        actual_execute(&mut connection, "COMMIT", Parameters::none()),
        Outcome::Affected(0)
    );
    let setup_indexes: &[&str] = match scenario {
        Phase4c2aCrashScenario::CreateOrdinary | Phase4c2aCrashScenario::CreateUnique => &[],
        Phase4c2aCrashScenario::InsertTwoIndexes => &[
            "CREATE UNIQUE INDEX UX_T_A ON T (A)",
            "CREATE INDEX IX_T_B ON T (B)",
        ],
        Phase4c2aCrashScenario::UpdateSplitTableAndIndexRoots
        | Phase4c2aCrashScenario::DeleteAndCollapseIndexRoot
        | Phase4c2aCrashScenario::DropTableWithIndexes => &[
            "CREATE UNIQUE INDEX UX_T_A ON T (A)",
            "CREATE INDEX IX_T_B ON T (B)",
            "CREATE INDEX IX_T_C ON T (C)",
        ],
        Phase4c2aCrashScenario::DropIndex => &["CREATE INDEX IX_T_B ON T (B)"],
    };
    for &sql in setup_indexes {
        assert_eq!(
            actual_execute(&mut connection, sql, Parameters::none()),
            Outcome::Affected(0)
        );
    }
    drop(connection);
    phase_4c2a_checkpoint(&pager).expect("checkpoint crash fixture pre-state");
    phase_4c2a_pager_state(&pager, temp.path()).expect("deep-valid crash fixture pre-state");
    drop(pager);
    temp
}

fn phase4c2a_wal_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push("-wal");
    PathBuf::from(value)
}

fn phase4c2a_trace(
    fixture: &TempDatabase,
    scenario: Phase4c2aCrashScenario,
) -> (
    Vec<FaultPoint>,
    Vec<Phase4c2aWalRecord>,
    Phase4c2aStorageState,
    Phase4c2aStorageState,
) {
    let pre_pager = phase_4c2a_open_pager(fixture.path()).expect("open crash pre-state");
    let pre = phase_4c2a_pager_state(&pre_pager, fixture.path()).expect("capture crash pre-state");
    drop(pre_pager);
    let trace_copy = phase_4c1_copy_crash_pair(fixture.path());
    let wal_start = fs::metadata(phase4c2a_wal_path(trace_copy.path()))
        .expect("read trace WAL length")
        .len();
    let (pager, faults) = phase_4c1_recording_crash_pager(trace_copy.path());
    phase4c2a_execute_statements(pager.clone(), &scenario.operation());
    let records = phase_4c2a_wal_records(trace_copy.path(), wal_start)
        .expect("decode successful Phase 4C2-A transaction records");
    phase_4c2a_checkpoint(&pager).expect("checkpoint successful crash trace");
    let trace = faults.observed();
    let publish_end = trace
        .iter()
        .position(|&point| point == FaultPoint::AfterWalPublish)
        .expect("trace publishes SQL commit");
    assert_commit_trace(&trace[..=publish_end]);
    assert_eq!(
        records.len(),
        trace[..=publish_end]
            .iter()
            .filter(|&&point| point == FaultPoint::AfterWalRecordWrite)
            .count(),
        "one trace quartet per WAL record"
    );
    let post = phase_4c2a_pager_state(&pager, trace_copy.path()).expect("capture crash post-state");
    assert_eq!(post.lsn, pre.lsn + 1, "{} commits once", scenario.name());
    assert_ne!(post, pre, "{} changes captured state", scenario.name());
    phase4c2a_assert_overall_structure_transition(scenario, &pre, &post);
    drop(faults);
    drop(pager);
    (trace, records, pre, post)
}

fn phase4c2a_assert_overall_structure_transition(
    scenario: Phase4c2aCrashScenario,
    pre: &Phase4c2aStorageState,
    post: &Phase4c2aStorageState,
) {
    match scenario {
        Phase4c2aCrashScenario::UpdateSplitTableAndIndexRoots => {
            let pre_table = &pre.tables["T"];
            let post_table = &post.tables["T"];
            let pre_index = &pre.indexes["IX_T_B"];
            let post_index = &post.indexes["IX_T_B"];
            assert!(
                post_table.height > pre_table.height
                    || post_table.root.page_id != pre_table.root.page_id,
                "atomic INSERT pressure did not split table: pre={pre_table:?} post={post_table:?}"
            );
            assert!(
                post_index.height > pre_index.height
                    || post_index.root.page_id != pre_index.root.page_id,
                "atomic UPDATE did not split index: pre={pre_index:?} post={post_index:?}"
            );
        }
        Phase4c2aCrashScenario::DeleteAndCollapseIndexRoot => {
            let pre_index = &pre.indexes["IX_T_B"];
            let post_index = &post.indexes["IX_T_B"];
            assert!(
                post_index.height < pre_index.height,
                "DELETE index did not collapse: pre={pre_index:?} post={post_index:?}"
            );
            assert_ne!(post_index.root.page_id, pre_index.root.page_id);
        }
        _ => {}
    }
}

fn phase4c2a_page_family(
    page_id: u64,
    pre: &Phase4c2aStorageState,
    post: &Phase4c2aStorageState,
) -> Option<Phase4c2aFaultFamily> {
    if pre.catalog_pages.contains(&page_id) || post.catalog_pages.contains(&page_id) {
        Some(Phase4c2aFaultFamily::CatalogPageFrame)
    } else if pre.table_pages.contains(&page_id) || post.table_pages.contains(&page_id) {
        Some(Phase4c2aFaultFamily::TablePageFrame)
    } else if pre.index_pages.contains(&page_id) || post.index_pages.contains(&page_id) {
        Some(Phase4c2aFaultFamily::IndexPageFrame)
    } else if pre
        .freelist
        .iter()
        .chain(&post.freelist)
        .any(|trunk| trunk.reference.page_id == page_id || trunk.leaves.contains(&page_id))
    {
        Some(Phase4c2aFaultFamily::FreelistPageFrame)
    } else {
        Some(Phase4c2aFaultFamily::OtherPageFrame)
    }
}

fn phase4c2a_fault_targets(
    trace: &[FaultPoint],
    records: &[Phase4c2aWalRecord],
    pre: &Phase4c2aStorageState,
    post: &Phase4c2aStorageState,
) -> Vec<Phase4c2aFaultTarget> {
    trace
        .iter()
        .enumerate()
        .map(|(index, &point)| {
            let occurrence = trace[..=index]
                .iter()
                .filter(|&&seen| seen == point)
                .count();
            let family = match point {
                FaultPoint::BeforeWalRecordWrite
                | FaultPoint::AfterWalRecordHeaderPrefixWrite
                | FaultPoint::AfterWalRecordPayloadPrefixWrite
                | FaultPoint::AfterWalRecordWrite => match records
                    .get(occurrence - 1)
                    .expect("record hook must name a persisted WAL record")
                {
                    Phase4c2aWalRecord::PageFrame(page_id) => {
                        phase4c2a_page_family(*page_id, pre, post)
                            .expect("every WAL page frame has a crash family")
                    }
                    Phase4c2aWalRecord::Commit => Phase4c2aFaultFamily::CommitRecord,
                },
                FaultPoint::BeforeWalPublish | FaultPoint::AfterWalPublish => {
                    Phase4c2aFaultFamily::Publication
                }
                FaultPoint::BeforeMainExtend
                | FaultPoint::AfterMainExtend
                | FaultPoint::BeforeMainPageWrite
                | FaultPoint::AfterMainPageWrite
                | FaultPoint::BeforeMainSync
                | FaultPoint::AfterMainSync
                | FaultPoint::BeforeMainDirectorySync
                | FaultPoint::AfterMainDirectorySync
                | FaultPoint::BeforeSuperblockWrite
                | FaultPoint::AfterSuperblockWrite => Phase4c2aFaultFamily::MainSync,
                FaultPoint::BeforeWalDirectorySync
                | FaultPoint::AfterWalDirectorySync
                | FaultPoint::BeforeWalHeaderWrite
                | FaultPoint::AfterWalHeaderWrite
                | FaultPoint::BeforeWalSync
                | FaultPoint::AfterWalSync
                | FaultPoint::BeforeWalTruncate
                | FaultPoint::AfterWalTruncate => Phase4c2aFaultFamily::WalSync,
            };
            Phase4c2aFaultTarget {
                family,
                point,
                occurrence,
            }
        })
        .collect()
}

#[test]
fn phase_4c2a_fault_targets_cover_every_observed_hook_occurrence() {
    let mut total = 0usize;
    let mut hook_counts = BTreeMap::<String, usize>::new();
    let mut observed = Vec::new();
    for scenario in Phase4c2aCrashScenario::ALL {
        let fixture = phase4c2a_fixture(scenario);
        let (trace, records, pre, post) = phase4c2a_trace(&fixture, scenario);
        let targets = phase4c2a_fault_targets(&trace, &records, &pre, &post);
        observed.extend(trace.iter().copied());
        let expected = trace
            .iter()
            .enumerate()
            .map(|(index, &point)| {
                let occurrence = trace[..=index]
                    .iter()
                    .filter(|&&seen| seen == point)
                    .count();
                (point, occurrence)
            })
            .collect::<Vec<_>>();
        let actual = targets
            .iter()
            .map(|target| (target.point, target.occurrence))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            expected,
            "{} must target each observed hook exactly once",
            scenario.name()
        );
        for target in targets {
            *hook_counts
                .entry(format!("{:?}", target.point))
                .or_default() += 1;
            total += 1;
        }
    }
    assert_eq!(
        hook_counts.values().sum::<usize>(),
        total,
        "hook accounting must equal all generated targets"
    );
    for point in [
        FaultPoint::BeforeWalRecordWrite,
        FaultPoint::BeforeMainExtend,
        FaultPoint::AfterMainExtend,
        FaultPoint::BeforeWalTruncate,
        FaultPoint::AfterWalTruncate,
    ] {
        assert!(
            observed.contains(&point),
            "combined traces must observe {point:?}"
        );
    }
    eprintln!("phase4c2a trace accounting: targets={total} hook_counts={hook_counts:?}");
}

#[test]
fn phase_4c2a_named_crash_scenarios_prove_split_and_collapse() {
    let split_scenario = Phase4c2aCrashScenario::UpdateSplitTableAndIndexRoots;
    let split_fixture = phase4c2a_fixture(split_scenario);
    let pager = phase_4c2a_open_pager(split_fixture.path()).expect("open split proof fixture");
    let pre = phase_4c2a_pager_state(&pager, split_fixture.path()).expect("capture split pre");
    let mut connection = Connection::from_test_pager(pager.clone());
    for sql in [
        "BEGIN".to_owned(),
        phase4c2a_split_insert_sql(),
        "COMMIT".to_owned(),
    ] {
        assert!(matches!(
            actual_execute(&mut connection, &sql, Parameters::none()),
            Outcome::Affected(_)
        ));
    }
    let after_insert =
        phase_4c2a_pager_state(&pager, split_fixture.path()).expect("capture table split");
    assert!(
        after_insert.tables["T"].height > pre.tables["T"].height
            || after_insert.tables["T"].root.page_id != pre.tables["T"].root.page_id,
        "boundary INSERT did not split existing table: pre={:?} after={:?}",
        pre.tables["T"],
        after_insert.tables["T"]
    );
    assert_eq!(
        after_insert.indexes["IX_T_B"].height, pre.indexes["IX_T_B"].height,
        "boundary INSERT must leave IX_T_B unsplit for the UPDATE proof"
    );
    assert_eq!(
        actual_execute(
            &mut connection,
            &phase4c2a_split_update_sql(),
            Parameters::none(),
        ),
        Outcome::Affected(38)
    );
    let after_update =
        phase_4c2a_pager_state(&pager, split_fixture.path()).expect("capture index split");
    assert!(
        after_update.indexes["IX_T_B"].height > after_insert.indexes["IX_T_B"].height
            || after_update.indexes["IX_T_B"].root.page_id
                != after_insert.indexes["IX_T_B"].root.page_id,
        "UPDATE did not split existing IX_T_B: before={:?} after={:?}",
        after_insert.indexes["IX_T_B"],
        after_update.indexes["IX_T_B"]
    );
    eprintln!(
        "phase4c2a split proof: table height/root {:?}/{:?} -> {:?}/{:?}; IX_T_B {:?}/{:?} -> {:?}/{:?}",
        pre.tables["T"].height,
        pre.tables["T"].root,
        after_insert.tables["T"].height,
        after_insert.tables["T"].root,
        after_insert.indexes["IX_T_B"].height,
        after_insert.indexes["IX_T_B"].root,
        after_update.indexes["IX_T_B"].height,
        after_update.indexes["IX_T_B"].root,
    );
    drop(connection);
    drop(pager);

    let atomic_split_fixture = phase4c2a_fixture(split_scenario);
    phase4c2a_trace(&atomic_split_fixture, split_scenario);

    let collapse_scenario = Phase4c2aCrashScenario::DeleteAndCollapseIndexRoot;
    let collapse_fixture = phase4c2a_fixture(collapse_scenario);
    let (_, _, collapse_pre, collapse_post) = phase4c2a_trace(&collapse_fixture, collapse_scenario);
    eprintln!(
        "phase4c2a collapse proof: IX_T_B height/root {:?}/{:?} -> {:?}/{:?}",
        collapse_pre.indexes["IX_T_B"].height,
        collapse_pre.indexes["IX_T_B"].root,
        collapse_post.indexes["IX_T_B"].height,
        collapse_post.indexes["IX_T_B"].root,
    );
}

fn phase4c2a_run_child(
    fixture: &TempDatabase,
    scenario: Phase4c2aCrashScenario,
    trace: &[FaultPoint],
    target: Phase4c2aFaultTarget,
) -> TempDatabase {
    let crash_copy = phase_4c1_copy_crash_pair(fixture.path());
    let path = fs::canonicalize(crash_copy.path()).expect("canonical Phase 4C2-A crash path");
    let marker = format!("PHASE4C2A_ABORT {}", scenario.name());
    let mut command = phase_4c1_exact_crash_child_command(PHASE4C2A_CRASH_CHILD);
    command
        .env(PHASE4C2A_PATH_ENV, &path)
        .env(PHASE4C2A_SCENARIO_ENV, scenario.name())
        .env(PHASE4C2A_POINT_ENV, format!("{:?}", target.point))
        .env(PHASE4C2A_OCCURRENCE_ENV, target.occurrence.to_string())
        .env(
            PHASE4C2A_TRACE_ENV,
            trace
                .iter()
                .map(|point| format!("{point:?}"))
                .collect::<Vec<_>>()
                .join(","),
        );
    let output = command
        .output()
        .expect("synchronously reap Phase 4C2-A crash child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("running 1 test"),
        "exact child did not run: {stdout}"
    );
    assert!(
        stderr
            .lines()
            .any(|line| { line == format!("{marker} {:?} {}", target.point, target.occurrence) }),
        "wrong Phase 4C2-A child marker: {stderr}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(6),
            "child must die from SIGABRT"
        );
    }
    #[cfg(windows)]
    assert_eq!(
        output.status.code(),
        Some(-1_073_740_791),
        "child must die from Windows abort fast-fail 0xC0000409"
    );
    crash_copy
}

fn phase4c2a_recover_case(
    crash_copy: TempDatabase,
    scenario: Phase4c2aCrashScenario,
    target: Phase4c2aFaultTarget,
    pre: &Phase4c2aStorageState,
    post: &Phase4c2aStorageState,
) {
    let path = fs::canonicalize(crash_copy.path()).expect("canonical recovered crash path");
    let (accepted_lsn, recovered) = phase_4c2a_recover_and_capture(&path).unwrap_or_else(|error| {
        panic!("{} {:?}: recovery failed: {error}", scenario.name(), target)
    });
    let expected = if accepted_lsn == post.lsn {
        post
    } else {
        assert_eq!(
            accepted_lsn, pre.lsn,
            "accepted neither complete pre nor post Commit"
        );
        pre
    };
    assert_eq!(
        &recovered,
        expected,
        "{} {:?}/{}, family={:?} tore catalog/table/index/freelist/file state",
        scenario.name(),
        target.point,
        target.occurrence,
        target.family
    );
    let (repeat_lsn, repeat) =
        phase_4c2a_recover_and_capture(&path).expect("repeat Phase 4C2-A recovery succeeds");
    assert_eq!(repeat_lsn, accepted_lsn, "repeat acceptance changed LSN");
    assert_eq!(repeat, recovered, "repeat recovery changed state");
    phase_4c2a_remove_recovered_files(&path);
    drop(crash_copy);
}

fn phase4c2a_assert_corrupt_complete_candidate_is_not_hidden() {
    let temp = TempDatabase::new("phase4c2a-committed-corrupt");
    let pager = phase_4c2a_create_pager(&temp);
    let mut connection = Connection::from_test_pager(pager.clone());
    for sql in [
        "CREATE TABLE T (A INT64)",
        "INSERT INTO T VALUES (1)",
        "CREATE INDEX IX_CORRUPT ON T (A)",
    ] {
        assert!(matches!(
            actual_execute(&mut connection, sql, Parameters::none()),
            Outcome::Affected(_)
        ));
    }
    drop(connection);
    let before = phase_4c2a_pager_state(&pager, temp.path()).unwrap().lsn;
    phase_4c2a_commit_malformed_index(&pager, "IX_CORRUPT")
        .expect("commit complete malformed index candidate");
    assert_eq!(
        phase_4c2a_pager_state(&pager, temp.path())
            .expect_err("live deep validation must reject malformed index")
            .code(),
        ErrorCode::Corruption
    );
    drop(pager);
    let error = match phase_4c2a_open_pager(temp.path()) {
        Ok(_) => panic!("recovery hid committed malformed index behind LSN {before}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), ErrorCode::Corruption);
}

#[test]
fn phase_4c2a_committed_corrupt_index_candidate_is_not_hidden() {
    // Production break caught: recovery must reject the newest complete Commit
    // when its catalog-reachable index entry is malformed, never publish the
    // older valid indexed commit as a silent fallback.
    phase4c2a_assert_corrupt_complete_candidate_is_not_hidden();
}

fn phase4c2a_run_crash_matrix() {
    let mut family_counts = BTreeMap::<Phase4c2aFaultFamily, usize>::new();
    let mut hook_counts = BTreeMap::<String, usize>::new();
    let mut scenarios = 0usize;
    let mut crashes = 0usize;
    for scenario in Phase4c2aCrashScenario::ALL {
        let fixture = phase4c2a_fixture(scenario);
        let (trace, records, pre, post) = phase4c2a_trace(&fixture, scenario);
        let targets = phase4c2a_fault_targets(&trace, &records, &pre, &post);
        let expected_hooks = trace
            .iter()
            .enumerate()
            .map(|(index, &point)| {
                let occurrence = trace[..=index]
                    .iter()
                    .filter(|&&seen| seen == point)
                    .count();
                (point, occurrence)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.point, target.occurrence))
                .collect::<Vec<_>>(),
            expected_hooks,
            "{} must target every observed hook occurrence exactly once",
            scenario.name()
        );
        for target in targets {
            let crash_copy = phase4c2a_run_child(&fixture, scenario, &trace, target);
            phase4c2a_recover_case(crash_copy, scenario, target, &pre, &post);
            *family_counts.entry(target.family).or_default() += 1;
            *hook_counts
                .entry(format!("{:?}", target.point))
                .or_default() += 1;
            crashes += 1;
        }
        scenarios += 1;
        eprintln!(
            "phase4c2a crash progress: scenario={} scenarios={} crashes={} family_counts={family_counts:?} hook_counts={hook_counts:?}",
            scenario.name(),
            scenarios,
            crashes
        );
    }
    for family in [
        Phase4c2aFaultFamily::CatalogPageFrame,
        Phase4c2aFaultFamily::TablePageFrame,
        Phase4c2aFaultFamily::IndexPageFrame,
        Phase4c2aFaultFamily::CommitRecord,
        Phase4c2aFaultFamily::WalSync,
        Phase4c2aFaultFamily::Publication,
        Phase4c2aFaultFamily::MainSync,
    ] {
        assert!(
            family_counts.get(&family).copied().unwrap_or(0) > 0,
            "unobserved {family:?}"
        );
    }
    phase4c2a_assert_corrupt_complete_candidate_is_not_hidden();
    eprintln!(
        "phase4c2a index crash matrix: scenarios={scenarios} crashes={crashes} family_counts={family_counts:?} hook_counts={hook_counts:?}; every observed occurrence targeted; all recovered exactly old or new"
    );
}

#[test]
#[ignore = "only launched with validated parameters by phase_4c2a_index_crash_recovery_matrix"]
fn phase_4c2a_index_crash_child() {
    let path = PathBuf::from(
        std::env::var_os(PHASE4C2A_PATH_ENV)
            .expect("invalid Phase 4C2-A crash configuration: missing path"),
    );
    let scenario_name =
        std::env::var(PHASE4C2A_SCENARIO_ENV).expect("invalid Phase 4C2-A crash scenario");
    let scenario = Phase4c2aCrashScenario::ALL
        .into_iter()
        .find(|scenario| scenario.name() == scenario_name)
        .expect("invalid Phase 4C2-A crash scenario");
    let point = parse_sql_fault_point(
        &std::env::var(PHASE4C2A_POINT_ENV).expect("invalid Phase 4C2-A crash point"),
    )
    .expect("invalid Phase 4C2-A crash point");
    let occurrence_text =
        std::env::var(PHASE4C2A_OCCURRENCE_ENV).expect("invalid Phase 4C2-A crash occurrence");
    let occurrence: usize = occurrence_text
        .parse()
        .expect("invalid Phase 4C2-A crash occurrence");
    assert!(occurrence > 0 && occurrence_text == occurrence.to_string());
    let trace = std::env::var(PHASE4C2A_TRACE_ENV)
        .expect("invalid Phase 4C2-A crash trace")
        .split(',')
        .map(|name| parse_sql_fault_point(name).expect("invalid Phase 4C2-A trace point"))
        .collect::<Vec<_>>();
    assert!(trace.iter().filter(|&&seen| seen == point).count() >= occurrence);
    let path = phase_4c1_validate_crash_path(path, Phase4c1CrashPathKind::Sql);
    let pager = phase_4c1_aborting_crash_pager(
        &path,
        trace,
        point,
        occurrence,
        format!("PHASE4C2A_ABORT {}", scenario.name()),
    );
    phase4c2a_execute_statements(pager.clone(), &scenario.operation());
    phase_4c2a_checkpoint(&pager).expect("checkpoint Phase 4C2-A crash child");
    panic!("configured Phase 4C2-A crash point was not reached");
}

#[test]
#[ignore = "run by the dedicated Phase 4C2-A CI job"]
fn phase_4c2a_index_crash_recovery_matrix() {
    phase4c2a_run_crash_matrix();
}

fn collect_public_rows(rows: &mut crate::Rows<'_>) -> Vec<Vec<Value>> {
    let mut collected = Vec::new();
    loop {
        match rows.next().expect("read snapshot row") {
            Some(row) => collected.push(row.values().to_vec()),
            None => return collected,
        }
    }
}
