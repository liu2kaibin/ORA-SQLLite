# Phase 4C1 SQL Execution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first real ORA-SQLLite SQL execution path for transactional DDL, complete Phase 4B SELECT, and parameterized INSERT/UPDATE/DELETE.

**Architecture:** Convert the existing project-owned `BoundStatement` into an immutable rule plan, then execute it through pull-based operators over a crate-private relational storage facade. Keep statements immutable, make read cursors and writer leases self-owning, and route every mutation through the existing catalog, row codec, B+Tree, mutation checkpoint, and WAL transaction boundaries.

**Tech Stack:** Rust 2024, MSRV Rust 1.85, standard library synchronization, existing `fs4`, `sqlparser`, and `getrandom` dependencies, existing Pager/B+Tree/WAL/catalog/row layers, GitHub Actions on Linux and Windows.

**Spec:** `docs/superpowers/specs/2026-09-13-phase-4c1-sql-execution-design.md`

## Global Constraints

- Read the spec above and the Phase 4B handoff in `docs/superpowers/specs/2026-09-09-phase-4b-sql-api-design.md` before changing code.
- Keep `#![forbid(unsafe_code)]`; do not add unsafe Rust or a production dependency.
- Keep Cargo edition 2024 and `rust-version = "1.85"`; every public and internal signature must compile on Rust 1.85.
- Do not change the main database format, WAL format, catalog V1 record encoding, row encoding, page size rules, or recovery acceptance rules.
- Never reparse SQL during planning or execution and never expose or retain a third-party AST outside `src/sql/parser/`.
- Every execution must reuse Phase 4B UUID, object ID, schema version, and created-name validation.
- Preserve multiple readers and one writer; public writer acquisition is nonblocking and returns `Busy` immediately.
- DDL is transactional; no implicit Oracle DDL commit is permitted.
- Default query working memory is exactly 64 MiB and is configurable through `Options`.
- Phase 4C1 executes every SELECT shape accepted by Phase 4B, but does not add indexes, constraints, a cost optimizer, temp-file spill, or Phase 5 Oracle behavior.
- Index or constraint statements already accepted by Phase 4B return `Unsupported` before storage mutation; auto-commit rejects them before acquiring a writer.
- Ordinary statement errors restore the statement checkpoint; `Io`, `Corruption`, and `Internal` poison an explicit write transaction until rollback.
- Use TDD for every task: observe the named test fail, make the smallest implementation pass, run the focused module, then commit.
- Do not weaken, ignore, or delete an existing assertion merely to make a new test pass.

## File and Responsibility Map

| Path | Responsibility |
|---|---|
| `src/api/params.rs` | Public positional/named parameter input and typed NULL |
| `src/api/row.rs` | Public owned `Row` and pull-based `Rows` facade |
| `src/api/result.rs` | Public non-query `ExecutionResult` |
| `src/api/options.rs` | 64 MiB default query-memory option |
| `src/api/connection.rs` | Auto-commit and SQL transaction session state |
| `src/api/transaction.rs` | Read/write transaction query and execute entry points |
| `src/api/statement.rs` | Immutable BoundStatement plus immutable Phase 4C1 Plan |
| `src/storage/pager.rs` | Owned writer lease and statement mutation checkpoints |
| `src/storage/btree/search.rs` | Scan state whose `next` only briefly borrows a page source |
| `src/storage/relation.rs` | Crate-private catalog/row/B+Tree facade for SQL execution |
| `src/sql/plan/` | BoundStatement-to-Plan conversion and executable expressions |
| `src/sql/exec/` | Parameters, expression evaluation, operators, DDL, and DML execution |
| `src/sql/exec/test_support.rs` | Generated reference-model and fault-injection coverage |
| `tests/phase4c_api.rs` | Public API CRUD, query, parameter, and transaction coverage |
| `tests/phase4c_scale.rs` | Ignored 100,000-row bounded smoke test |
| `.github/workflows/ci.yml` | Dedicated Linux Phase 4C1 execution job |
| `README.md` | Verified Phase 4C1 capability and remaining Phase 4C2/5 boundary |

---

### Task 1: Public Values, Result Types, and Execution Memory Limit

**Files:**
- Create: `src/api/params.rs`
- Create: `src/api/row.rs`
- Create: `src/api/result.rs`
- Modify: `src/error.rs`
- Modify: `src/api/options.rs`
- Modify: `src/api/database.rs`
- Modify: `src/api/connection.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/lib.rs`
- Test: inline tests in the files above

**Interfaces:**
- Consumes: existing `Value`, `DataType`, `Options`, `Database`, and `Connection`.
- Produces: `ParameterValue`, `Parameters<'a>`, `Row`, `ExecutionResult`, `ErrorCode::ResourceLimit`, and connection-level `max_query_memory_bytes: u64` for Tasks 5-16.

- [ ] **Step 1: Add failing public-contract tests**

```rust
#[test]
fn phase_4c1_public_primitives_have_stable_contracts() {
    assert_eq!(ErrorCode::ResourceLimit.as_u32(), 1650);
    let options = Options::default();
    assert_eq!(options.max_query_memory_bytes(), 64 * 1024 * 1024);
    assert_eq!(
        options.with_max_query_memory_bytes(4096).max_query_memory_bytes(),
        4096
    );

    let null = ParameterValue::typed_null(DataType::Text).unwrap();
    assert_eq!(null.data_type(), Some(DataType::Text));
    assert_eq!(null.value(), &Value::Null);
    assert_eq!(
        ParameterValue::typed_null(DataType::Unknown)
            .unwrap_err()
            .code(),
        ErrorCode::Type
    );

    let row = Row::from_values_for_test(vec![Value::Int64(7), Value::Text("N".into())]);
    assert_eq!(row.len(), 2);
    assert_eq!(row.get(0), Some(&Value::Int64(7)));
    assert_eq!(ExecutionResult::new_for_test(3).rows_affected(), 3);
}
```

- [ ] **Step 2: Run the focused tests and confirm the red state**

Run: `rustup run stable cargo test phase_4c1_public_primitives_have_stable_contracts -- --exact`

Expected: compilation fails because `ResourceLimit`, the option, and the public types do not exist.

- [ ] **Step 3: Add the minimal public types and option plumbing**

```rust
const DEFAULT_MAX_QUERY_MEMORY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum ParameterValue {
    Value(Value),
    TypedNull(DataType),
}

impl ParameterValue {
    pub fn typed_null(data_type: DataType) -> Result<Self> {
        match data_type {
            DataType::Null | DataType::Unknown => Err(DbError::new(
                ErrorCode::Type,
                "typed NULL requires a concrete data type",
            )),
            concrete => Ok(Self::TypedNull(concrete)),
        }
    }

    pub fn value(&self) -> &Value {
        match self {
            Self::Value(value) => value,
            Self::TypedNull(_) => &Value::Null,
        }
    }

    pub const fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Value(_) => None,
            Self::TypedNull(data_type) => Some(*data_type),
        }
    }
}

pub enum Parameters<'a> {
    None,
    Positional(&'a [ParameterValue]),
    Named(&'a [(&'a str, ParameterValue)]),
}

pub struct Row {
    values: Box<[Value]>,
}

pub struct ExecutionResult {
    rows_affected: u64,
}
```

Add `From<Value> for ParameterValue`, `Parameters::none/positional/named`, public `Row::len/is_empty/get/values`, crate-private constructors for `Row` and `ExecutionResult`, and the new option getter/builder. Copy the option into `Database` and every new `Connection` so runtime behavior is fixed at open/create time.

- [ ] **Step 4: Run API and error tests**

Run: `rustup run stable cargo test api:: --lib`

Run: `rustup run stable cargo test error:: --lib`

Expected: all API and error tests pass; `ResourceLimit` is 1650 and the default is 67,108,864 bytes.

- [ ] **Step 5: Run formatting and MSRV checks**

Run: `rustup run stable cargo fmt --check`

Run: `rustup run 1.85.0 cargo check --all-targets`

Expected: both commands exit 0.

- [ ] **Step 6: Commit Task 1**

```bash
git add src/api src/error.rs src/lib.rs
git commit -m "feat: add phase 4c1 execution api primitives"
```

---

### Task 2: Self-Owned Writer Lease

**Files:**
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/checkpoint.rs`
- Modify: `src/storage/catalog/mod.rs`
- Modify: every storage signature that still names `WriteBatch<'_>`
- Modify: `src/api/transaction.rs`
- Test: `src/storage/pager.rs`
- Test: `src/storage/test_support.rs`

**Interfaces:**
- Consumes: existing writer mutex, `Pager`, `WriteBatch`, checkpoint serialization, and WAL commit path.
- Produces: lifetime-free `WriteBatch`, `WriterLease`, blocking internal `Pager::begin_write()`, and nonblocking `Pager::try_begin_write()`; public `Connection::begin_write()` switches to the nonblocking entry.

- [ ] **Step 1: Add compile-red and runtime writer-ownership tests**

```rust
fn detached_batch(pager: Pager) -> Result<WriteBatch> {
    pager.try_begin_write()
}

#[test]
fn write_batch_owns_its_writer_lease() {
    let temp = TempDatabase::new("owned-writer-lease");
    let pager = create_pager(&temp);
    let mut batch = detached_batch(pager.clone()).unwrap();
    assert_eq!(pager.try_begin_write().unwrap_err().code(), ErrorCode::Busy);
    batch.rollback().unwrap();
    assert!(pager.try_begin_write().is_ok());
}

#[test]
fn dropping_unfinished_owned_batch_releases_writer_without_wal_io() {
    let temp = TempDatabase::new("owned-writer-drop");
    let pager = create_pager(&temp);
    let before = durable_lengths(&temp);
    drop(detached_batch(pager.clone()).unwrap());
    assert_eq!(durable_lengths(&temp), before);
    assert!(pager.try_begin_write().is_ok());
}
```

- [ ] **Step 2: Run the owned-writer test and confirm the red state**

Run: `rustup run stable cargo test write_batch_owns_its_writer_lease -- --exact`

Expected: compilation fails because `WriteBatch` still borrows the local Pager.

- [ ] **Step 3: Replace the long-lived MutexGuard with an owned coordinator lease**

```rust
struct WriterCoordinator {
    active: Mutex<bool>,
    available: Condvar,
}

struct WriterLease {
    coordinator: Arc<WriterCoordinator>,
}

impl WriterCoordinator {
    fn try_acquire(self: &Arc<Self>) -> Result<WriterLease> {
        let mut active = self.active.try_lock().map_err(map_writer_try_lock)?;
        if *active {
            return Err(DbError::new(ErrorCode::Busy, "another write batch is active"));
        }
        *active = true;
        Ok(WriterLease { coordinator: self.clone() })
    }

    fn acquire(self: &Arc<Self>) -> Result<WriterLease> {
        let mut active = self.active.lock().map_err(map_writer_poison)?;
        while *active {
            active = self.available.wait(active).map_err(map_writer_poison)?;
        }
        *active = true;
        Ok(WriterLease { coordinator: self.clone() })
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let mut active = match self.coordinator.active.lock() {
            Ok(active) => active,
            Err(poisoned) => poisoned.into_inner(),
        };
        *active = false;
        self.coordinator.available.notify_one();
    }
}

pub(crate) struct WriteBatch {
    pager: Pager,
    _writer: WriterLease,
    // retain every existing batch field unchanged
}
```

Use the blocking lease only for existing storage-internal flows that deliberately serialize. Use `try_begin_write()` for public Rust and SQL writers. Preserve the writer-before-published-state/cache/WAL lock order and keep checkpoint on the same coordinator.

- [ ] **Step 4: Mechanically remove WriteBatch lifetimes and run focused tests**

Run: `rustup run stable cargo test storage::pager:: --lib`

Run: `rustup run stable cargo test storage::checkpoint:: --lib`

Expected: writer, Drop, checkpoint, commit, and Busy tests pass.

- [ ] **Step 5: Run the full storage suite under stable and MSRV**

Run: `rustup run stable cargo test storage:: --lib`

Run: `rustup run 1.85.0 cargo test storage:: --lib`

Expected: both suites pass with no disk-format fixture changes.

- [ ] **Step 6: Commit Task 2**

```bash
git add src/storage src/api/transaction.rs
git commit -m "refactor: make write batches own writer leases"
```

---

### Task 3: Resumable B+Tree Scan and Read-Only Relational Facade

**Files:**
- Modify: `src/storage/btree/search.rs`
- Modify: `src/storage/btree/mod.rs`
- Create: `src/storage/relation.rs`
- Modify: `src/storage/mod.rs`
- Test: `src/storage/btree/search.rs`
- Test: `src/storage/relation.rs`

**Interfaces:**
- Consumes: `BTree`, `PageSource`, `CatalogReader`, `TableDef`, `ColumnDef`, `RowCodec`, and ROWID key helpers.
- Produces: `BTreeScanState`, `Relation`, `RelationScan`, and `StoredRow`; no produced type retains a `&mut ReadSnapshot` or `&mut WriteBatch`.

- [ ] **Step 1: Add failing alternating-scan and row-decode tests**

```rust
#[test]
fn two_scan_states_alternate_on_one_snapshot() {
    let (tree_a, tree_b, mut snapshot) = two_tree_fixture();
    let mut a = tree_a.scan_state(Bound::Unbounded, Bound::Unbounded).unwrap();
    let mut b = tree_b.scan_state(Bound::Unbounded, Bound::Unbounded).unwrap();
    assert_eq!(a.next(&mut snapshot).unwrap().unwrap().0, vec![1]);
    assert_eq!(b.next(&mut snapshot).unwrap().unwrap().0, vec![9]);
    assert_eq!(a.next(&mut snapshot).unwrap().unwrap().0, vec![2]);
}

#[test]
fn relation_scan_decodes_rowid_and_values_without_borrowing_snapshot() {
    let fixture = phase_4a_relation_fixture();
    let mut snapshot = fixture.pager().begin_read().unwrap();
    let catalog = snapshot.catalog_reader().unwrap();
    let relation = Relation::open(&catalog, fixture.table_id()).unwrap();
    let mut scan = relation.scan();
    let first = scan.next_snapshot(&mut snapshot).unwrap().unwrap();
    assert_eq!(first.rowid().get(), 1);
    assert_eq!(first.values(), &[Value::Int64(10)]);
}
```

- [ ] **Step 2: Run the new tests and confirm the red state**

Run: `rustup run stable cargo test two_scan_states_alternate_on_one_snapshot -- --exact`

Expected: compilation fails because `scan_state` and `Relation` do not exist.

- [ ] **Step 3: Add the state-only B+Tree scan contract**

```rust
pub(in crate::storage) struct BTreeScanState {
    kind: TreeKind,
    root: PageRef,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    current: Option<PageRef>,
    leaf: Option<LeafNode>,
    next_slot: usize,
    visited: BTreeSet<PageId>,
    max_leaf_hops: u64,
    previous_leaf_last_key: Option<Vec<u8>>,
}

impl BTreeScanState {
    pub(in crate::storage) fn next(
        &mut self,
        source: &mut impl PageSource,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.initialize_if_needed(source)?;
        self.next_validated_cell(source)
    }
}
```

Move the current cursor's corruption, sibling-generation, bound, decreasing-key, cycle, and overflow-value checks into the state methods. Retain `BTree::scan` as a compatibility wrapper over `BTreeScanState` until all existing tests use the new path.

- [ ] **Step 4: Add the crate-private relation reader**

```rust
pub(crate) struct Relation {
    table: TableDef,
    columns: Box<[ColumnDef]>,
}

pub(crate) struct RelationScan {
    relation: Relation,
    state: BTreeScanState,
}

pub(crate) struct StoredRow {
    rowid: RowId,
    values: Box<[Value]>,
}

impl RelationScan {
    pub(crate) fn next_snapshot(
        &mut self,
        snapshot: &mut ReadSnapshot,
    ) -> Result<Option<StoredRow>>;

    pub(crate) fn next_batch(
        &mut self,
        batch: &mut WriteBatch,
    ) -> Result<Option<StoredRow>>;
}
```

`Relation::open` clones only the validated TableDef and ordered ColumnDefs from a supplied CatalogReader. Each next call decodes an 8-byte ROWID key and one row with the existing RowCodec limits.

- [ ] **Step 5: Run scan, relation, catalog, and corruption tests**

Run: `rustup run stable cargo test storage:: --lib`

Expected: all tests pass, including every pre-existing malformed leaf-chain case.

- [ ] **Step 6: Commit Task 3**

```bash
git add src/storage/btree src/storage/relation.rs src/storage/mod.rs
git commit -m "refactor: add resumable relation scans"
```

---

### Task 4: Immutable Rule Plan

**Files:**
- Create: `src/sql/plan/mod.rs`
- Create: `src/sql/plan/expr.rs`
- Create: `src/sql/plan/query.rs`
- Create: `src/sql/plan/mutation.rs`
- Modify: `src/sql/mod.rs`
- Modify: `src/api/statement.rs`
- Test: `src/sql/plan/mod.rs`

**Interfaces:**
- Consumes: every `BoundStatementKind`, `BoundExpr`, schema dependency, parameter metadata, column metadata, and source SQL needed to resolve `SqlLocation`.
- Produces: `Plan`, `PlanKind`, `QueryPlan`, `ExecExpr`, stable parameter slots, aggregate slots, and `PlanKind::DeferredUnsupported`.

- [ ] **Step 1: Add failing planner-coverage tests**

```rust
#[test]
fn planner_covers_every_bound_statement_without_reparsing() {
    for bound in every_bound_statement_fixture() {
        let plan = Plan::from_bound(&bound, fixture_source_for(&bound)).unwrap();
        assert!(plan.kind().matches_bound_kind(bound.kind()));
    }
}

#[test]
fn index_and_constrained_table_plans_are_deferred() {
    for bound in [bound_create_index(), bound_constrained_create_table()] {
        assert!(matches!(
            Plan::from_bound(&bound, bound.source()).unwrap().kind(),
            PlanKind::DeferredUnsupported { required_phase: "4C2" }
        ));
    }
}

#[test]
fn executable_expressions_retain_project_sql_locations() {
    let plan = plan_sql("SELECT 1 / :1 FROM USERS").unwrap();
    assert_eq!(plan.first_divide_location(), SqlLocation::new(7, 1, 8).unwrap());
}
```

- [ ] **Step 2: Run planner tests and confirm the red state**

Run: `rustup run stable cargo test sql::plan:: --lib`

Expected: compilation fails because the `sql::plan` module and `Plan` do not exist.

- [ ] **Step 3: Define the immutable plan IR**

```rust
pub(crate) struct Plan {
    kind: PlanKind,
}

pub(crate) enum PlanKind {
    Query(QueryPlan),
    Insert(InsertPlan),
    Update(UpdatePlan),
    Delete(DeletePlan),
    CreateTable(CreateTablePlan),
    DropTable { table_id: TableId },
    Begin,
    Commit,
    Rollback,
    DeferredUnsupported { required_phase: &'static str },
}

pub(crate) struct ExecExpr {
    kind: ExecExprKind,
    data_type: SqlType,
    nullable: bool,
    location: SqlLocation,
}

pub(crate) enum ExecExprKind {
    Literal(Value),
    Column { source: SourceId, ordinal: u16 },
    Parameter { slot: u16 },
    Unary { op: BoundUnaryOp, expr: Box<ExecExpr> },
    Binary { left: Box<ExecExpr>, op: BoundBinaryOp, right: Box<ExecExpr> },
    IsNull { expr: Box<ExecExpr>, negated: bool },
    InList { expr: Box<ExecExpr>, values: Box<[ExecExpr]>, negated: bool },
    Between { expr: Box<ExecExpr>, low: Box<ExecExpr>, high: Box<ExecExpr>, negated: bool },
    Like { expr: Box<ExecExpr>, pattern: Box<ExecExpr>, negated: bool },
    Aggregate { slot: u32 },
}

impl PlanKind {
    pub(crate) const fn is_write_or_query(&self) -> bool {
        matches!(
            self,
            Self::Query(_) | Self::Insert(_) | Self::Update(_) | Self::Delete(_)
                | Self::CreateTable(_) | Self::DropTable { .. }
        )
    }
}
```

`QueryPlan` retains ordered sources, joins, WHERE, group expressions, deduplicated aggregate definitions, HAVING, projection, DISTINCT, and ORDER BY. Assign parameter and aggregate slots deterministically. Convert every BoundStatement match arm explicitly; never add a wildcard that silently routes a future variant.

- [ ] **Step 4: Store the plan in Statement and retain schema validation**

```rust
pub struct Statement {
    bound: Arc<BoundStatement>,
    plan: Arc<Plan>,
    parameters: Box<[ParameterMetadata]>,
    columns: Box<[ColumnMetadata]>,
}
```

Change `prepare_with_catalog` to pass the original SQL only to the project-owned span-to-location converter and planner. Keep Debug output free of BoundStatement, Plan, literals, IDs, UUIDs, and SQL text.

- [ ] **Step 5: Run SQL front-end, planner, and upgrade-sentinel tests**

Run: `rustup run stable cargo test sql:: --lib`

Run: `rustup run stable cargo test api::statement:: --lib`

Expected: all Phase 4B corpus and no-third-party-type tests still pass, plus every PlanKind test.

- [ ] **Step 6: Commit Task 4**

```bash
git add src/sql/plan src/sql/mod.rs src/api/statement.rs
git commit -m "feat: compile bound statements into rule plans"
```

---

### Task 5: Runtime Parameter Frame, Work Memory, and Scalar Evaluation

**Files:**
- Create: `src/sql/exec/mod.rs`
- Create: `src/sql/exec/params.rs`
- Create: `src/sql/exec/memory.rs`
- Create: `src/sql/exec/expr.rs`
- Modify: `src/sql/mod.rs`
- Test: the new modules

**Interfaces:**
- Consumes: `Parameters`, `ParameterValue`, Statement parameter metadata, `ExecExpr`, `Value`, `Decimal`, Date, and Timestamp.
- Produces: `ParameterFrame::bind`, `WorkMemory`, `EvalRow`, `eval`, `sql_equal`, `sql_total_cmp`, and finite-Float64 validation.

- [ ] **Step 1: Add failing parameter and expression tables**

```rust
#[test]
fn runtime_parameter_contract_is_exact_and_typed() {
    let metadata = positional_metadata(&[Some(DataType::Decimal), None]);
    let values = [Value::Int64(7).into(), ParameterValue::typed_null(DataType::Text).unwrap()];
    let mut memory = WorkMemory::new(4096).unwrap();
    let frame = ParameterFrame::bind(&metadata, Parameters::positional(&values), &mut memory).unwrap();
    assert!(matches!(frame.get(0), Value::Decimal(_)));
    assert_eq!(frame.get(1), &Value::Null);
    assert_eq!(
        ParameterFrame::bind(&metadata, Parameters::none(), &mut memory)
            .unwrap_err()
            .code(),
        ErrorCode::Bind
    );
}

#[test]
fn scalar_evaluator_uses_three_valued_logic_and_locations() {
    assert_eq!(eval_sql_expr("TRUE AND NULL").unwrap(), Value::Null);
    assert_eq!(eval_sql_expr("FALSE AND NULL").unwrap(), Value::Bool(false));
    assert_eq!(eval_sql_expr("NULL = NULL").unwrap(), Value::Null);
    let error = eval_sql_expr("1 / 0").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Type);
    assert_eq!(error.sql_location().unwrap().column(), 1);
}

#[test]
fn non_finite_float_is_rejected_and_signed_zero_is_one_key() {
    assert_eq!(validate_sql_float(f64::NAN).unwrap_err().code(), ErrorCode::Type);
    assert_eq!(sql_total_cmp(&Value::Float64(-0.0), &Value::Float64(0.0)).unwrap(), Ordering::Equal);
}
```

- [ ] **Step 2: Run execution-expression tests and confirm the red state**

Run: `rustup run stable cargo test sql::exec:: --lib`

Expected: compilation fails because the execution modules do not exist.

- [ ] **Step 3: Add exact parameter normalization and owned accounting**

```rust
pub(crate) struct ParameterFrame {
    values: Box<[Value]>,
}

pub(crate) struct WorkMemory {
    limit: u64,
    used: u64,
}

impl WorkMemory {
    pub(crate) fn reserve(&mut self, bytes: u64) -> Result<()> {
        let next = self.used.checked_add(bytes).ok_or_else(resource_limit)?;
        if next > self.limit {
            return Err(resource_limit());
        }
        self.used = next;
        Ok(())
    }
}
```

Normalize named identifiers with the existing Phase 4B helper, reject duplicates/missing/extras as `Bind`, copy values into the frame, account for Text/Blob bytes, promote only Int64 to Decimal, and require a concrete typed NULL when metadata is Unknown.

- [ ] **Step 4: Add total SQL value semantics and iterative/bounded evaluation**

```rust
pub(crate) enum TruthValue {
    True,
    False,
    Unknown,
}

pub(crate) fn eval(
    expr: &ExecExpr,
    row: &EvalRow,
    params: &ParameterFrame,
) -> Result<Value>;

pub(crate) fn sql_total_cmp(left: &Value, right: &Value) -> Result<Ordering>;

pub(crate) fn sql_group_equal(left: &Value, right: &Value) -> Result<bool>;
```

Implement every ExecExprKind explicitly. Use checked integer and Decimal operations, reject zero divisors and non-finite Float64 input/results, treat `%` and `_` in LIKE exactly as the spec states, compare Timestamp field tuples without timezone normalization, and attach `expr.location()` to runtime Type errors.

- [ ] **Step 5: Run evaluator, planner, and property tests**

Run: `rustup run stable cargo test sql:: --lib`

Expected: all parameter, scalar, NULL, comparison, LIKE, numeric-boundary, and location tests pass.

- [ ] **Step 6: Commit Task 5**

```bash
git add src/sql/exec src/sql/mod.rs
git commit -m "feat: evaluate parameters and scalar expressions"
```

---

### Task 6: Core Pull Executor and Streaming Read API

**Files:**
- Create: `src/sql/exec/operator.rs`
- Create: `src/sql/exec/scan.rs`
- Create: `src/sql/exec/query.rs`
- Modify: `src/api/row.rs`
- Modify: `src/api/connection.rs`
- Modify: `src/api/transaction.rs`
- Modify: `src/api/statement.rs`
- Test: `src/sql/exec/query.rs`
- Test: `src/api/transaction.rs`

**Interfaces:**
- Consumes: `PlanKind::Query`, `RelationScan`, `ParameterFrame`, `WorkMemory`, schema validation, ReadSnapshot, and WriteBatch.
- Produces: `Operator::next`, `OutputOperator::next`, `OperatorFactory`, `ExecutionContext`, `OneRow`, `TableScan`, `Filter`, `Project`, `ProjectedRow`, owned/borrowed `Rows` storage, and `query` on Connection/read/write transactions for queries without JOIN, aggregate, DISTINCT, or ORDER BY.

- [ ] **Step 1: Add failing streaming and early-Drop tests**

```rust
#[test]
fn simple_query_streams_owned_rows() {
    let fixture = query_relation_fixture(&[(1, "A"), (2, "B")]);
    let mut connection = fixture.database().connect().unwrap();
    let statement = connection.prepare("SELECT ID, NAME FROM USERS WHERE ID > :1").unwrap();
    let params = [Value::Int64(0).into()];
    let mut rows = connection.query(&statement, Parameters::positional(&params)).unwrap();
    assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(1), Value::Text("A".into())]);
    assert_eq!(rows.next().unwrap().unwrap().values(), &[Value::Int64(2), Value::Text("B".into())]);
    assert!(rows.next().unwrap().is_none());
}

#[test]
fn dropping_rows_early_releases_the_snapshot_and_context_borrow() {
    let fixture = query_relation_fixture(&[(1, "A"), (2, "B")]);
    let mut connection = fixture.database().connect().unwrap();
    let statement = connection.prepare("SELECT ID FROM USERS").unwrap();
    let mut rows = connection.query(&statement, Parameters::none()).unwrap();
    assert!(rows.next().unwrap().is_some());
    drop(rows);
    let again = connection.query(&statement, Parameters::none()).unwrap();
    drop(again);
}
```

- [ ] **Step 2: Run the query tests and confirm the red state**

Run: `rustup run stable cargo test simple_query_streams_owned_rows -- --exact`

Expected: compilation fails because `query` and executable `Rows` do not exist.

- [ ] **Step 3: Define storage-independent pull operators**

```rust
pub(crate) trait Operator {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>>;
}

pub(crate) trait OutputOperator {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<ProjectedRow>>;
}

#[derive(Clone)]
pub(crate) enum OperatorFactory {
    OneRow,
    TableScan { source: SourceId, table: TableId },
    Filter { input: Box<OperatorFactory>, predicate: ExecExpr },
}

pub(crate) struct ProjectedRow {
    output: Row,
    order_keys: Box<[Value]>,
    input_ordinal: u64,
}

pub(crate) enum ExecutionStorage<'a> {
    Read(&'a mut ReadSnapshot),
    Write(&'a mut WriteBatch),
}

pub(crate) struct ExecutionContext<'a> {
    storage: ExecutionStorage<'a>,
    params: &'a ParameterFrame,
    memory: &'a mut WorkMemory,
}

pub(crate) struct EvalRow {
    sources: Box<[Option<Box<[Value]>>]>,
    aggregates: Box<[Value]>,
}
```

OneRow emits exactly one empty source row. TableScan owns RelationScan and fills the bound SourceId slot. Filter keeps only `Value::Bool(true)`. Project evaluates projection expressions into an owned public Row.

- [ ] **Step 4: Make Rows own operators and only briefly borrow storage**

```rust
enum RowsStorage<'a> {
    OwnedRead(ReadSnapshot),
    BorrowedRead(&'a mut ReadSnapshot),
    BorrowedWrite(&'a mut WriteBatch),
}

pub struct Rows<'a> {
    storage: RowsStorage<'a>,
    root: Box<dyn OutputOperator>,
    params: ParameterFrame,
    memory: WorkMemory,
}

impl Rows<'_> {
    pub fn next(&mut self) -> Result<Option<Row>> {
        self.next_projected_row()
    }
}
```

Change `Connection::prepare` to `&mut self`. At query construction, validate the parameter frame and schema before creating any RelationScan. Owned Connection reads keep the ReadSnapshot in Rows; transaction methods return Rows borrowing only their snapshot or batch field.

- [ ] **Step 5: Run core query, API borrow, and Phase 4B Statement tests**

Run: `rustup run stable cargo test simple_query_streams_owned_rows -- --exact --nocapture`

Run: `rustup run stable cargo test dropping_rows_early -- --exact --nocapture`

Run: `rustup run stable cargo test api:: --lib`

Run: `rustup run stable cargo test sql::exec::query:: --lib`

Expected: simple scans, filters, projections, constants, NULL predicates, borrow rules, and early Drop pass.

- [ ] **Step 6: Commit Task 6**

```bash
git add src/sql/exec src/api
git commit -m "feat: stream simple select queries"
```

---

### Task 7: INNER and LEFT Nested-Loop JOIN

**Files:**
- Create: `src/sql/exec/join.rs`
- Modify: `src/sql/exec/query.rs`
- Modify: `src/sql/exec/operator.rs`
- Test: `src/sql/exec/join.rs`

**Interfaces:**
- Consumes: `Operator`, `ExecutionContext`, ordered QueryPlan sources/joins, and ON ExecExpr.
- Produces: rewindable `NestedLoopJoin` that preserves source order and implements INNER and LEFT semantics without holding two storage borrows.

- [ ] **Step 1: Add failing table-driven JOIN tests**

```rust
#[test]
fn nested_loop_join_covers_inner_left_null_and_duplicate_matches() {
    let cases = [
        ("SELECT U.ID, O.ID FROM USERS U INNER JOIN ORDERS O ON O.USER_ID = U.ID ORDER BY O.ID", 2),
        ("SELECT U.ID, O.ID FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID ORDER BY U.ID, O.ID", 3),
        ("SELECT U.ID FROM USERS U LEFT JOIN ORDERS O ON NULL ORDER BY U.ID", 2),
    ];
    for (sql, expected_rows) in cases {
        assert_eq!(run_query(join_fixture(), sql).unwrap().len(), expected_rows);
    }
}

#[test]
fn join_does_not_retain_simultaneous_snapshot_borrows() {
    let rows = run_query(join_fixture(), "SELECT A.ID, B.ID FROM A INNER JOIN B ON A.ID = B.ID").unwrap();
    assert_eq!(rows[0].values(), &[Value::Int64(1), Value::Int64(1)]);
}
```

- [ ] **Step 2: Run JOIN tests and confirm the red state**

Run: `rustup run stable cargo test nested_loop_join_covers_inner_left_null_and_duplicate_matches -- --exact`

Expected: query returns `Unsupported` for the JOIN plan or the Join operator is absent.

- [ ] **Step 3: Add rewindable right-side factories and nested-loop state**

```rust
pub(crate) struct NestedLoopJoin {
    left: Box<dyn Operator>,
    right_factory: OperatorFactory,
    right: Option<Box<dyn Operator>>,
    on: ExecExpr,
    kind: JoinKind,
    current_left: Option<EvalRow>,
    matched_current_left: bool,
}
```

For each left row, rebuild/reset the right subtree from its immutable factory and advance left/right by separate short calls to `next(context)`. INNER emits only TRUE matches. LEFT emits one null-extended right source after exhausting an unmatched right side. FALSE and UNKNOWN ON results are nonmatches.

- [ ] **Step 4: Compose multiple joins in SQL order**

Build one NestedLoopJoin per bound join from left to right. Preserve SourceId slots, validate null extension against ColumnMetadata nullability only at the result layer, and never reorder sources.

```rust
let root = plan.joins().iter().fold(base_source, |left, join| {
    Box::new(NestedLoopJoin::new(left, source_factory(join.source()), join.clone()))
});
```

- [ ] **Step 5: Run JOIN, query, and snapshot tests**

Run: `rustup run stable cargo test sql::exec:: --lib`

Expected: INNER, LEFT, duplicate matches, NULL ON, multiple joins, early Drop, and fixed snapshots pass.

- [ ] **Step 6: Commit Task 7**

```bash
git add src/sql/exec/join.rs src/sql/exec/query.rs src/sql/exec/operator.rs
git commit -m "feat: execute inner and left joins"
```

---

### Task 8: Aggregation, HAVING, DISTINCT, and ORDER BY

**Files:**
- Create: `src/sql/exec/aggregate.rs`
- Create: `src/sql/exec/distinct.rs`
- Create: `src/sql/exec/sort.rs`
- Modify: `src/sql/exec/query.rs`
- Modify: `src/sql/exec/operator.rs`
- Modify: `src/sql/exec/memory.rs`
- Test: the new modules

**Interfaces:**
- Consumes: full QueryPlan, EvalRow, aggregate slots, SQL equality/total order, and shared WorkMemory.
- Produces: scalar/group aggregates, post-aggregate HAVING/projection, DISTINCT over visible output, hidden ORDER BY keys, Oracle-style default NULL placement, and `ResourceLimit` before over-budget retention.

- [ ] **Step 1: Add failing full-query matrix tests**

```rust
#[test]
fn aggregate_distinct_and_sort_match_approved_semantics() {
    let fixture = aggregate_fixture();
    assert_eq!(
        values(fixture, "SELECT COUNT(*), SUM(AMOUNT), AVG(AMOUNT), MIN(AMOUNT), MAX(AMOUNT) FROM SALES"),
        expected_scalar_aggregates()
    );
    assert_eq!(
        values(fixture, "SELECT REGION, COUNT(*) FROM SALES GROUP BY REGION HAVING COUNT(*) > 1 ORDER BY REGION"),
        expected_grouped_rows()
    );
    assert_eq!(
        values(fixture, "SELECT DISTINCT REGION FROM SALES ORDER BY REGION DESC NULLS LAST"),
        expected_distinct_regions()
    );
}

#[test]
fn materialization_rejects_the_first_over_budget_item() {
    let error = run_with_memory(
        aggregate_fixture(),
        "SELECT DISTINCT PAYLOAD FROM SALES ORDER BY PAYLOAD",
        64,
    ).unwrap_err();
    assert_eq!(error.code(), ErrorCode::ResourceLimit);
}
```

- [ ] **Step 2: Run materialization tests and confirm the red state**

Run: `rustup run stable cargo test aggregate_distinct_and_sort_match_approved_semantics -- --exact`

Expected: query returns Unsupported for aggregate/DISTINCT/ORDER BY plans.

- [ ] **Step 3: Add aggregate state with checked finalization**

```rust
enum AggregateState {
    Count(u64),
    DecimalSum { sum: Decimal, count: u64 },
    FloatSum { sum: f64, count: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

struct GroupState {
    key: Box<[Value]>,
    representative: EvalRow,
    aggregates: Box<[AggregateState]>,
}
```

Use SQL group equality, make all NULL group keys equal, ignore NULL aggregate arguments, return COUNT 0 for empty scalar input, and return NULL for empty SUM/AVG/MIN/MAX. Check count, Decimal, and Float64 bounds before updating state.

- [ ] **Step 4: Add visible-row DISTINCT and stable ORDER BY**

```rust
pub(crate) struct Distinct {
    input: Box<dyn OutputOperator>,
    seen: Vec<Box<[Value]>>,
}

pub(crate) struct Sort {
    input: Box<dyn OutputOperator>,
    rows: Vec<ProjectedRow>,
    initialized: bool,
}
```

Use the `ProjectedRow` produced by Task 6. Compute hidden order keys before discarding EvalRow, compare the full visible output for DISTINCT, and use `input_ordinal` as the final stable-sort key. Apply explicit NULLS FIRST/LAST; otherwise ASC uses LAST and DESC uses FIRST. Without ORDER BY, expose no order guarantee in tests.

- [ ] **Step 5: Account memory and release it with operator state**

Reserve parameter, key, Value, Text/Blob, row, and conservative container bytes before insertion. On failure, do not insert the candidate. Keep the reservation until the materializing operator is dropped.

```rust
let bytes = estimated_projected_row_bytes(&candidate)?;
context.memory.reserve(bytes)?;
rows.push(candidate);
```

- [ ] **Step 6: Run the complete SELECT suite under stable and MSRV**

Run: `rustup run stable cargo test sql::exec:: --lib`

Run: `rustup run 1.85.0 cargo test sql::exec:: --lib`

Expected: the entire Phase 4B SELECT surface now executes, including memory boundaries.

- [ ] **Step 7: Commit Task 8**

```bash
git add src/sql/exec
git commit -m "feat: execute aggregate distinct and ordered queries"
```

---

### Task 9: Relational Mutation Facade and Statement Checkpoints

**Files:**
- Modify: `src/storage/relation.rs`
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/pager.rs`
- Test: `src/storage/relation.rs`
- Test: `src/storage/catalog/mod.rs`

**Interfaces:**
- Consumes: CatalogWriter, BTree mutation, RowCodec, ROWID allocation, tree-root tracking, WriteBatch mutation checkpoint/restore.
- Produces: `StatementCheckpoint`, `TableWrite`, `create_relation`, and `drop_relation`; SQL execution never manipulates raw catalog keys or row bytes.

- [ ] **Step 1: Add failing atomic relation-mutation tests**

```rust
#[test]
fn relation_write_updates_catalog_root_rowid_and_rows_together() {
    let fixture = empty_catalog_fixture();
    let mut batch = fixture.pager().try_begin_write().unwrap();
    create_relation(&mut batch, table_spec("T", &[nullable_int("ID")])).unwrap();
    let table_id = table_id(&mut batch, "T");
    let id = TableWrite::open(&mut batch, table_id).unwrap()
        .insert(&mut batch, &[Value::Int64(7)]).unwrap();
    assert_eq!(id.get(), 1);
    batch.commit().unwrap();
    assert_eq!(read_values(fixture.pager(), "T"), vec![vec![Value::Int64(7)]]);
}

#[test]
fn restoring_statement_checkpoint_removes_all_relation_mutations() {
    let fixture = one_table_fixture();
    let mut batch = fixture.pager().try_begin_write().unwrap();
    let checkpoint = batch.statement_checkpoint().unwrap();
    TableWrite::open(&mut batch, fixture.table_id()).unwrap()
        .insert(&mut batch, &[Value::Int64(99)]).unwrap();
    batch.restore_statement(checkpoint);
    batch.commit().unwrap();
    assert_eq!(read_values(fixture.pager(), "T"), fixture.original_values());
}
```

- [ ] **Step 2: Run relation-mutation tests and confirm the red state**

Run: `rustup run stable cargo test relation_write_updates_catalog_root_rowid_and_rows_together -- --exact`

Expected: compilation fails because the mutation facade and statement checkpoint API do not exist.

- [ ] **Step 3: Expose an opaque statement checkpoint**

```rust
pub(crate) struct StatementCheckpoint(WriteBatchCheckpoint);

impl WriteBatch {
    pub(crate) fn statement_checkpoint(&mut self) -> Result<StatementCheckpoint> {
        self.checkpoint_mutation().map(StatementCheckpoint)
    }

    pub(crate) fn restore_statement(&mut self, checkpoint: StatementCheckpoint) {
        self.restore_mutation(checkpoint.0);
    }
}
```

The opaque checkpoint must cover every existing mutable WriteBatch field already handled by checkpoint_mutation, including catalog gates and root tracking.

- [ ] **Step 4: Add the mutation facade**

```rust
pub(crate) struct TableWrite {
    schema: Relation,
    rows: BTree,
    catalog: CatalogWriter,
    original_root: PageRef,
}

impl TableWrite {
    pub(crate) fn open(batch: &mut WriteBatch, table_id: TableId) -> Result<Self>;
    pub(crate) fn insert(&mut self, batch: &mut WriteBatch, values: &[Value]) -> Result<RowId>;
    pub(crate) fn replace(&mut self, batch: &mut WriteBatch, rowid: RowId, values: &[Value]) -> Result<()>;
    pub(crate) fn delete(&mut self, batch: &mut WriteBatch, rowid: RowId) -> Result<bool>;
    pub(crate) fn next_after(&mut self, batch: &mut WriteBatch, rowid: Option<RowId>) -> Result<Option<StoredRow>>;
    pub(crate) fn finish(self, batch: &mut WriteBatch) -> Result<()>;
}
```

Insert allocates ROWID through CatalogWriter. Finish updates TableDef when the row-tree root changed, validates the candidate catalog, and closes the catalog update gate. Add a CatalogWriter method that removes exact table/column records and releases the bound table tree for `drop_relation`.

- [ ] **Step 5: Run relation, catalog, B+Tree, and WAL tests**

Run: `rustup run stable cargo test storage:: --lib`

Run: `rustup run stable cargo test storage::test_support::phase_4a_catalog_crash_recovery_matrix -- --exact --ignored --nocapture`

Expected: relation atomicity passes and the existing Phase 4A recovery matrix remains green.

- [ ] **Step 6: Commit Task 9**

```bash
git add src/storage/relation.rs src/storage/catalog/mod.rs src/storage/pager.rs
git commit -m "feat: add atomic relational mutations"
```

---

### Task 10: Transactional CREATE TABLE and DROP TABLE

**Files:**
- Create: `src/sql/exec/mutation.rs`
- Modify: `src/sql/exec/mod.rs`
- Modify: `src/api/transaction.rs`
- Test: `src/sql/exec/mutation.rs`
- Test: `src/api/transaction.rs`

**Interfaces:**
- Consumes: CreateTablePlan, DropTable Plan, DeferredUnsupported, create/drop relation facade, WriteBatch, and StatementCheckpoint.
- Produces: internal `execute_write_plan`, `WriteTransaction::execute` for DDL, transactional same-batch catalog visibility, and 0-row ExecutionResult.

- [ ] **Step 1: Add failing DDL transaction tests**

```rust
#[test]
fn create_and_drop_are_visible_in_transaction_and_rollbackable() {
    let db = empty_database();
    let mut connection = db.connect().unwrap();
    {
        let mut tx = connection.begin_write().unwrap();
        let create = tx.prepare("CREATE TABLE T (ID INT64, NAME TEXT)").unwrap();
        assert_eq!(tx.execute(&create, Parameters::none()).unwrap().rows_affected(), 0);
        assert!(tx.prepare("SELECT ID FROM T").is_ok());
        tx.rollback().unwrap();
    }
    assert!(connection.prepare("SELECT ID FROM T").is_err());
}

#[test]
fn constrained_ddl_is_unsupported_before_mutation() {
    let db = empty_database();
    let mut connection = db.connect().unwrap();
    let mut tx = connection.begin_write().unwrap();
    let statement = tx.prepare("CREATE TABLE T (ID INT64 PRIMARY KEY)").unwrap();
    let before = tx.batch_fingerprint_for_test();
    assert_eq!(tx.execute(&statement, Parameters::none()).unwrap_err().code(), ErrorCode::Unsupported);
    assert_eq!(tx.batch_fingerprint_for_test(), before);
}
```

- [ ] **Step 2: Run DDL tests and confirm the red state**

Run: `rustup run stable cargo test create_and_drop_are_visible_in_transaction_and_rollbackable -- --exact`

Expected: compilation fails because `WriteTransaction::execute` does not exist.

- [ ] **Step 3: Add write-plan dispatch and DDL execution**

```rust
pub(crate) struct ValidatedWrite<'a> {
    plan: &'a Plan,
    params: ParameterFrame,
    memory: WorkMemory,
}

pub(crate) fn validate_write_execution<'a>(
    batch: &mut WriteBatch,
    statement: &'a Statement,
    params: Parameters<'_>,
    memory_limit: u64,
) -> Result<ValidatedWrite<'a>>;

pub(crate) fn execute_validated_write_plan(
    batch: &mut WriteBatch,
    validated: ValidatedWrite<'_>,
) -> Result<ExecutionResult> {
    match validated.plan.kind() {
        PlanKind::CreateTable(plan) if plan.constraints().is_empty() => {
            create_relation(batch, plan.table_spec())?;
            Ok(ExecutionResult::new(0))
        }
        PlanKind::DropTable { table_id } => {
            drop_relation(batch, *table_id)?;
            Ok(ExecutionResult::new(0))
        }
        kind => Err(wrong_execute_kind(kind)),
    }
}
```

Use the batch catalog view for prepare and validation. Recheck a CREATE name immediately before create_relation. DROP uses only the bound TableId and never rebinds a name.

- [ ] **Step 4: Wrap explicit write statements in statement checkpoints**

```rust
let validated = validate_write_execution(batch, statement, params, memory_limit)?;
if matches!(validated.plan.kind(), PlanKind::DeferredUnsupported { .. }) {
    return Err(unsupported_4c2());
}
let checkpoint = batch.statement_checkpoint()?;
match execute_validated_write_plan(batch, validated) {
    Ok(result) => Ok(result),
    Err(error) => {
        batch.restore_statement(checkpoint);
        classify_explicit_transaction_error(error)
    }
}
```

Parameter, schema, and DeferredUnsupported checks run before this checkpoint. Ordinary DDL errors restore and keep the transaction active; fatal errors set the WriteTransaction failure flag.

- [ ] **Step 5: Run DDL, schema validation, rollback, and recovery tests**

Run: `rustup run stable cargo test sql::exec::mutation:: --lib`

Run: `rustup run stable cargo test api::transaction:: --lib`

Expected: create/drop commit and rollback, own-DDL prepare, old Statement invalidation, and constrained DDL rejection pass.

- [ ] **Step 6: Commit Task 10**

```bash
git add src/sql/exec/mutation.rs src/sql/exec/mod.rs src/api/transaction.rs
git commit -m "feat: execute transactional table ddl"
```

---

### Task 11: Parameterized INSERT

**Files:**
- Modify: `src/sql/exec/mutation.rs`
- Modify: `src/api/transaction.rs`
- Test: `src/sql/exec/mutation.rs`
- Test: `src/api/transaction.rs`

**Interfaces:**
- Consumes: InsertPlan, ParameterFrame, scalar evaluation, Relation/TableWrite, RowCodec validation, and explicit statement checkpoints.
- Produces: single-row INSERT through `WriteTransaction::execute`, omitted-column NULL filling, finite-Float validation, atomic ROWID allocation, and rows_affected = 1.

- [ ] **Step 1: Add failing positional/named INSERT tests**

```rust
#[test]
fn insert_maps_targets_parameters_and_omitted_columns() {
    let (mut tx, table) = writable_table("CREATE TABLE T (ID INT64, NAME TEXT, NOTE TEXT)");
    let positional = tx.prepare("INSERT INTO T (ID, NAME) VALUES (:1, :2)").unwrap();
    let values = [Value::Int64(7).into(), Value::Text("A".into()).into()];
    assert_eq!(tx.execute(&positional, Parameters::positional(&values)).unwrap().rows_affected(), 1);

    let named = tx.prepare("INSERT INTO T (NAME, ID) VALUES (:NAME, :ID)").unwrap();
    let values = [("id", Value::Int64(8).into()), ("name", Value::Text("B".into()).into())];
    assert_eq!(tx.execute(&named, Parameters::named(&values)).unwrap().rows_affected(), 1);
    assert_eq!(read_table(&mut tx, table), rows_with_null_notes());
}

#[test]
fn failed_insert_does_not_consume_rowid_or_change_the_batch() {
    let (mut tx, _) = writable_table("CREATE TABLE T (ID INT64)");
    let bad = tx.prepare("INSERT INTO T VALUES (:1)").unwrap();
    let values = [Value::Text("wrong".into()).into()];
    assert_eq!(tx.execute(&bad, Parameters::positional(&values)).unwrap_err().code(), ErrorCode::Type);
    let good = [Value::Int64(1).into()];
    tx.execute(&bad, Parameters::positional(&good)).unwrap();
    assert_eq!(first_internal_rowid(&mut tx, "T"), 1);
}
```

- [ ] **Step 2: Run INSERT tests and confirm the red state**

Run: `rustup run stable cargo test insert_maps_targets_parameters_and_omitted_columns -- --exact`

Expected: execute rejects InsertPlan as an unsupported write kind.

- [ ] **Step 3: Add INSERT evaluation before mutation**

```rust
fn execute_insert(
    batch: &mut WriteBatch,
    plan: &InsertPlan,
    frame: &ParameterFrame,
) -> Result<ExecutionResult> {
    let relation = Relation::open_from_batch(batch, plan.table_id())?;
    let mut values = vec![Value::Null; relation.columns().len()];
    for (target, expression) in plan.targets().iter().zip(plan.values()) {
        values[usize::from(target.ordinal())] = eval(expression, &EvalRow::empty(), frame)?;
    }
    relation.validate_values(&values)?;
    let mut writer = TableWrite::open(batch, plan.table_id())?;
    writer.insert(batch, &values)?;
    writer.finish(batch)?;
    Ok(ExecutionResult::new(1))
}
```

Reject non-finite Float64 and wrong value types before `TableWrite::open` allocates ROWID. Let the existing row codec enforce row byte limits, column count, and nullability.

- [ ] **Step 4: Run INSERT, parameter, row codec, and rollback tests**

Run: `rustup run stable cargo test insert_ -- --nocapture`

Run: `rustup run stable cargo test sql::exec::params:: --lib`

Run: `rustup run stable cargo test storage::row:: --lib`

Expected: target reordering, omitted NULL, named/position values, type errors, row limits, row-root splits, ROWID, rollback, and read-own-write pass.

- [ ] **Step 5: Commit Task 11**

```bash
git add src/sql/exec/mutation.rs src/api/transaction.rs
git commit -m "feat: execute parameterized inserts"
```

---

### Task 12: Streaming UPDATE and DELETE with Statement Atomicity

**Files:**
- Modify: `src/sql/exec/mutation.rs`
- Modify: `src/storage/relation.rs`
- Modify: `src/api/transaction.rs`
- Test: `src/sql/exec/mutation.rs`

**Interfaces:**
- Consumes: UpdatePlan/DeletePlan, TableWrite::next_after, scalar evaluation against old rows, statement checkpoints, and checked affected-row counts.
- Produces: bounded-memory UPDATE/DELETE in ROWID order, old-row assignment semantics, no Halloween behavior, and ordinary-error recovery that leaves the transaction usable.

- [ ] **Step 1: Add failing UPDATE/DELETE and mid-statement rollback tests**

```rust
#[test]
fn update_uses_old_row_and_delete_counts_only_matches() {
    let mut tx = three_row_transaction();
    let update = tx.prepare("UPDATE T SET A = A + 1, B = A WHERE A >= 2").unwrap();
    assert_eq!(tx.execute(&update, Parameters::none()).unwrap().rows_affected(), 2);
    assert_eq!(query_values(&mut tx, "SELECT A, B FROM T ORDER BY A"), old_row_assignment_result());
    let delete = tx.prepare("DELETE FROM T WHERE A > 2").unwrap();
    assert_eq!(tx.execute(&delete, Parameters::none()).unwrap().rows_affected(), 2);
}

#[test]
fn later_row_error_restores_every_earlier_update() {
    let mut tx = rows_with_zero_divisor_transaction();
    let before = query_values(&mut tx, "SELECT ID, V FROM T ORDER BY ID");
    let update = tx.prepare("UPDATE T SET V = 100 / V").unwrap();
    assert_eq!(tx.execute(&update, Parameters::none()).unwrap_err().code(), ErrorCode::Type);
    assert_eq!(query_values(&mut tx, "SELECT ID, V FROM T ORDER BY ID"), before);
    let insert = tx.prepare("INSERT INTO T VALUES (9, 1)").unwrap();
    tx.execute(&insert, Parameters::none()).unwrap();
}
```

- [ ] **Step 2: Run UPDATE tests and confirm the red state**

Run: `rustup run stable cargo test later_row_error_restores_every_earlier_update -- --exact`

Expected: execute rejects UpdatePlan or leaves an earlier row changed.

- [ ] **Step 3: Implement resume-after-ROWID mutation loops**

```rust
let mut last = None;
let mut affected = 0u64;
while let Some(stored) = writer.next_after(batch, last)? {
    last = Some(stored.rowid());
    let old = EvalRow::from_relation(plan.source_id(), stored.values());
    if predicate_is_true(plan.selection(), &old, frame)? {
        let replacement = evaluate_assignments(plan, &old, frame)?;
        writer.replace(batch, stored.rowid(), &replacement)?;
        affected = affected.checked_add(1).ok_or_else(resource_limit)?;
    }
}
writer.finish(batch)?;
Ok(ExecutionResult::new(affected))
```

DELETE uses the same loop and calls `writer.delete`. Update computes every right-hand side from one immutable old EvalRow before changing any target. The continuation key is always the immutable ROWID.

- [ ] **Step 4: Restore the outer checkpoint for every ordinary execution error**

Route predicate, expression, row codec, memory, and count errors through the WriteTransaction statement wrapper from Task 10. Confirm the wrapper restores catalog roots, next_rowid, dirty pages, allocated pages, and released pages before returning the error.

```rust
match execute_update_or_delete(batch, statement, frame) {
    Ok(result) => Ok(result),
    Err(error) if !is_fatal(&error) => {
        batch.restore_statement(checkpoint);
        Err(error)
    }
    Err(error) => {
        batch.restore_statement(checkpoint);
        self.failed = true;
        Err(error)
    }
}
```

- [ ] **Step 5: Run mutation, B+Tree split/collapse, and transaction tests**

Run: `rustup run stable cargo test sql::exec::mutation:: --lib -- --nocapture`

Run: `rustup run stable cargo test storage::btree:: --lib`

Run: `rustup run stable cargo test api::transaction:: --lib`

Expected: all matching, no matching, all rows, old-row values, mid-row errors, split/collapse, no ROWID reuse, and continued transaction use pass.

- [ ] **Step 6: Commit Task 12**

```bash
git add src/sql/exec/mutation.rs src/storage/relation.rs src/api/transaction.rs
git commit -m "feat: execute atomic update and delete"
```

---

### Task 13: Connection Auto-Commit and SQL Transaction State

**Files:**
- Modify: `src/api/connection.rs`
- Modify: `src/api/transaction.rs`
- Modify: `src/sql/exec/mod.rs`
- Test: `src/api/connection.rs`
- Create: `tests/phase4c_api.rs`
- Create: `tests/support/mod.rs`

**Interfaces:**
- Consumes: complete query/write execution, owned WriteBatch, nonblocking writer acquisition, and ExecutionResult.
- Produces: Connection `Idle/SqlWrite/SqlWriteFailed/NeedsReopen` states, auto-commit DDL/DML, SQL BEGIN/COMMIT/ROLLBACK, read-own-write, Drop rollback, and mutual exclusion with Rust transaction objects.

- [ ] **Step 1: Add failing public transaction-flow tests**

```rust
#[test]
fn sql_transaction_commits_rolls_back_and_reads_own_writes() {
    let (_files, db) = support::new_database("sql-transaction");
    let mut connection = db.connect().unwrap();
    execute_sql(&mut connection, "BEGIN");
    execute_sql(&mut connection, "CREATE TABLE T (ID INT64)");
    execute_sql(&mut connection, "INSERT INTO T VALUES (1)");
    assert_eq!(collect(&mut connection, "SELECT ID FROM T"), vec![vec![Value::Int64(1)]]);
    execute_sql(&mut connection, "ROLLBACK");
    assert!(connection.prepare("SELECT ID FROM T").is_err());
}

#[test]
fn auto_commit_survives_reopen_and_writer_conflict_is_busy() {
    let (files, db) = support::new_database("autocommit");
    let mut first = db.connect().unwrap();
    execute_sql(&mut first, "CREATE TABLE T (ID INT64)");
    let mut tx = first.begin_write().unwrap();
    let mut second = db.connect().unwrap();
    assert_eq!(second.begin_write().unwrap_err().code(), ErrorCode::Busy);
    tx.rollback().unwrap();
    drop((first, second, db));
    assert!(support::reopen(&files).connect().unwrap().prepare("SELECT ID FROM T").is_ok());
}
```

- [ ] **Step 2: Run public transaction tests and confirm the red state**

Run: `rustup run stable cargo test --test phase4c_api sql_transaction_commits_rolls_back_and_reads_own_writes -- --exact`

Expected: BEGIN is not executable and Connection has no SQL transaction state.

- [ ] **Step 3: Add explicit Connection states and active-catalog prepare**

```rust
enum ConnectionState {
    Idle,
    SqlWrite(WriteBatch),
    SqlWriteFailed(WriteBatch),
    NeedsReopen,
}

pub struct Connection {
    pager: Pager,
    sql_limits: SqlLimits,
    max_query_memory_bytes: u64,
    state: ConnectionState,
    _not_sync: PhantomData<Cell<()>>,
}
```

`prepare(&mut self)` uses the active batch catalog for SqlWrite, returns Transaction for SqlWriteFailed/NeedsReopen, and uses a short current snapshot for Idle. `begin_read` and `begin_write` reject every non-Idle state.

- [ ] **Step 4: Dispatch auto-commit and SQL control statements**

```rust
match (self.state_kind(), statement.plan().kind()) {
    (Idle, PlanKind::Begin) => self.start_sql_write(),
    (SqlWrite, PlanKind::Commit) => self.commit_sql_write(),
    (SqlWrite, PlanKind::Rollback) | (SqlWriteFailed, PlanKind::Rollback) => {
        self.rollback_sql_write()
    }
    (Idle, PlanKind::CreateTable(_) | PlanKind::DropTable { .. }
        | PlanKind::Insert(_) | PlanKind::Update(_) | PlanKind::Delete(_)) => {
        self.execute_autocommit(statement, parameters)
    }
    (SqlWrite, kind) if kind.is_write_or_query() => {
        self.execute_in_sql_write(statement, parameters)
    }
    _ => Err(transaction_state_error()),
}
```

Auto-commit creates one try-acquired WriteBatch and commits only after successful execution. BEGIN also uses try acquisition. Drop rolls back SqlWrite/SqlWriteFailed. A commit I/O error consumes the batch, transitions to NeedsReopen, and rejects all later operations until Connection is dropped.

- [ ] **Step 5: Add fatal-error poisoning and ordinary-error continuation tests**

```rust
#[test]
fn ordinary_error_keeps_sql_transaction_but_fatal_error_requires_reopen() {
    let mut connection = fault_capable_connection();
    execute_sql(&mut connection, "BEGIN");
    assert_eq!(execute_error(&mut connection, "INSERT INTO T VALUES ('bad')"), ErrorCode::Type);
    execute_sql(&mut connection, "INSERT INTO T VALUES (1)");
    inject_next_commit_io_error(&connection);
    assert_eq!(execute_error(&mut connection, "COMMIT"), ErrorCode::Io);
    assert_eq!(connection.prepare("SELECT ID FROM T").unwrap_err().code(), ErrorCode::Transaction);
}
```

- [ ] **Step 6: Run connection, public API, Drop, Busy, and MSRV tests**

Run: `rustup run stable cargo test api::connection:: --lib`

Run: `rustup run stable cargo test --test phase4c_api`

Run: `rustup run 1.85.0 cargo test --test phase4c_api`

Expected: auto-commit, SQL/Rust transactions, mutual exclusion, Drop rollback, ordinary/fatal errors, Busy, and reopen pass.

- [ ] **Step 7: Commit Task 13**

```bash
git add src/api src/sql/exec tests/phase4c_api.rs tests/support/mod.rs
git commit -m "feat: add autocommit and sql transactions"
```

---

### Task 14: Deterministic Reference Model and No-Panic Execution Corpus

**Files:**
- Create: `src/sql/exec/test_support.rs`
- Modify: `src/sql/exec/mod.rs`
- Test: `src/sql/exec/test_support.rs`

**Interfaces:**
- Consumes: complete public/internal execution path and deterministic fixture generator.
- Produces: fixed-seed independent row/transaction model for CRUD, predicates, JOIN, grouping, DISTINCT, sorting, parameters, and schema changes; no generated case may panic.

- [ ] **Step 1: Add a failing fixed-seed model entry point**

```rust
#[test]
fn phase_4c1_fixed_seed_execution_model() {
    for seed in 0u64..128 {
        let program = GeneratedProgram::from_seed(seed, 4, 24);
        let expected = ReferenceDatabase::run(&program);
        let actual = OraSqlLiteHarness::run(&program);
        assert_eq!(actual, expected, "seed={seed} program={program:?}");
    }
}

#[test]
fn phase_4c1_random_runtime_inputs_never_panic() {
    for seed in 0u64..512 {
        let result = catch_unwind(AssertUnwindSafe(|| execute_generated_case(seed)));
        assert!(result.is_ok(), "panic for seed {seed}");
    }
}
```

- [ ] **Step 2: Run the model test and confirm the red state**

Run: `rustup run stable cargo test phase_4c1_fixed_seed_execution_model -- --exact --nocapture`

Expected: compilation fails because GeneratedProgram and the two harnesses do not exist.

- [ ] **Step 3: Build a deliberately simple independent reference model**

```rust
struct ReferenceTable {
    columns: Vec<ReferenceColumn>,
    rows: BTreeMap<u64, Vec<Value>>,
    next_rowid: u64,
}

struct ReferenceDatabase {
    tables: BTreeMap<String, ReferenceTable>,
    transaction_before: Option<BTreeMap<String, ReferenceTable>>,
}
```

Implement model operations directly over vectors/maps, not Plan, Operator, Relation, RowCodec, or Pager. Generate only Phase 4C1-supported typed programs. Compare visible rows, errors by ErrorCode, affected counts, commit/rollback state, and schema invalidation.

- [ ] **Step 4: Add boundary-biased generated inputs**

Use deterministic generation for NULL, signed zero, finite Float64, Decimal extremes, empty/Unicode Text, Blob, zero divisors, duplicate group keys, unmatched LEFT rows, memory boundaries, wrong parameters, and ordinary statement errors. Exclude NaN/Infinity from stored-success cases and include them as expected Type failures.

```rust
const EDGE_INTS: &[i64] = &[i64::MIN, -1, 0, 1, i64::MAX];
const EDGE_TEXT: &[&str] = &["", "A", "a", "中", "😀", "%", "_"];
```

- [ ] **Step 5: Run model, corpus, and Clippy checks**

Run: `rustup run stable cargo test phase_4c1_fixed_seed_execution_model -- --exact --nocapture`

Run: `rustup run stable cargo test phase_4c1_random_runtime_inputs_never_panic -- --exact --nocapture`

Run: `rustup run stable cargo clippy --all-targets --all-features -- -D warnings`

Expected: all 128 model programs and 512 no-panic cases pass; Clippy emits no warnings.

- [ ] **Step 6: Commit Task 14**

```bash
git add src/sql/exec/test_support.rs src/sql/exec/mod.rs
git commit -m "test: add phase 4c1 execution reference model"
```

---

### Task 15: SQL-Level Crash Recovery and Snapshot Concurrency

**Files:**
- Modify: `src/sql/exec/test_support.rs`
- Modify: `src/storage/test_support.rs`
- Test: the two files above

**Interfaces:**
- Consumes: existing Phase 3 fault points/child-process harness, Phase 4A recovery checks, public SQL execution, SQL/Rust transactions, and fixed read snapshots.
- Produces: representative cross-layer crash matrix proving SQL-visible atomicity and a concurrency test proving readers never observe future or partial commits.

- [ ] **Step 1: Add failing SQL recovery and concurrency entry points**

```rust
#[test]
#[ignore = "cross-process recovery matrix"]
fn phase_4c1_sql_crash_recovery_matrix() {
    for point in representative_sql_commit_faults() {
        let outcome = run_sql_child_at_fault(point);
        assert_sql_recovery_matches_durable_commit_record(outcome);
    }
}

#[test]
fn phase_4c1_readers_never_observe_partial_sql_commit() {
    let fixture = concurrent_sql_fixture();
    let old = fixture.begin_old_reader();
    fixture.commit_writer_program("UPDATE T SET V = V + 1");
    assert_eq!(old.collect(), fixture.before_rows());
    assert_eq!(fixture.begin_reader().collect(), fixture.after_rows());
}
```

- [ ] **Step 2: Run concurrency first and confirm the recovery red state**

Run: `rustup run stable cargo test phase_4c1_readers_never_observe_partial_sql_commit -- --exact`

Run: `rustup run stable cargo test phase_4c1_sql_crash_recovery_matrix -- --exact --ignored --nocapture`

Expected: helpers are absent or SQL operations are not connected to the existing fault harness.

- [ ] **Step 3: Route representative fault points through real SQL**

Cover at least WAL header initialization, first page frame, later page frame, commit record, WAL sync, in-memory publication, and directory sync. Use programs that jointly change catalog records, next_rowid, and user rows.

```rust
let programs: &[&[&str]] = &[
    &["CREATE TABLE T (ID INT64)"],
    &["BEGIN", "INSERT INTO T VALUES (1)", "INSERT INTO T VALUES (2)", "COMMIT"],
    &["BEGIN", "UPDATE T SET ID = ID + 10", "DELETE FROM T WHERE ID = 11", "COMMIT"],
    &["DROP TABLE T"],
];
```

The harness executes each statement separately because prepare accepts one statement. Expected state is derived only from whether a complete durable Commit Record exists, never from whether the child returned success.

- [ ] **Step 4: Add multi-reader/single-writer stress with bounded iterations**

Use several Database clones and independent Connections. Readers hold Rows across a writer commit, verify their original snapshot, drop Rows, then verify the new snapshot. A second public writer must receive Busy immediately; no test may depend on timing sleeps.

```rust
for round in 0..64 {
    let old_rows = open_reader(&database);
    commit_round(&database, round);
    assert_eq!(collect_reader(old_rows), expected_before(round));
    assert_eq!(collect_fresh(&database), expected_after(round));
}
```

- [ ] **Step 5: Run SQL recovery plus existing recovery matrices**

Run: `rustup run stable cargo test phase_4c1_sql_crash_recovery_matrix -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test storage::test_support::phase_3_crash_recovery_matrix -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test storage::test_support::phase_4a_catalog_crash_recovery_matrix -- --exact --ignored --nocapture`

Expected: all three matrices pass and no committed catalog/row combination is torn.

- [ ] **Step 6: Commit Task 15**

```bash
git add src/sql/exec/test_support.rs src/storage/test_support.rs
git commit -m "test: verify sql crash recovery and snapshots"
```

---

### Task 16: 100,000-Row Smoke Test, CI Gate, and Phase Documentation

**Files:**
- Create: `tests/phase4c_scale.rs`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`
- Test: `tests/phase4c_scale.rs`

**Interfaces:**
- Consumes: complete Phase 4C1 public API and all preceding tests.
- Produces: ignored deterministic 100,000-row test, dedicated 60-minute Linux job, current README capability statement, and final release evidence.

- [ ] **Step 1: Add the ignored scale smoke test**

```rust
#[test]
#[ignore = "100,000-row Phase 4C1 smoke"]
fn phase_4c1_hundred_thousand_row_smoke() {
    let (_files, db) = support::new_database("phase4c-scale");
    let mut connection = db.connect().unwrap();
    execute_sql(&mut connection, "BEGIN");
    execute_sql(&mut connection, "CREATE TABLE T (ID INT64, G INT64, PAYLOAD TEXT)");
    let insert = connection.prepare("INSERT INTO T VALUES (:1, :2, :3)").unwrap();
    for id in 0i64..100_000 {
        let values = [
            Value::Int64(id).into(),
            Value::Int64(id % 100).into(),
            Value::Text(format!("row-{id}" )).into(),
        ];
        connection.execute(&insert, Parameters::positional(&values)).unwrap();
    }
    execute_sql(&mut connection, "COMMIT");
    assert_eq!(scalar_i64(&mut connection, "SELECT COUNT(*) FROM T"), 100_000);
    assert_eq!(collect_count(&mut connection, "SELECT G, COUNT(*) FROM T GROUP BY G ORDER BY G"), 100);
    assert_eq!(execute_sql_result(&mut connection, "UPDATE T SET G = G + 1 WHERE ID < 1000").rows_affected(), 1000);
    assert_eq!(execute_sql_result(&mut connection, "DELETE FROM T WHERE ID >= 99000").rows_affected(), 1000);
    drop(connection);
    assert_eq!(scalar_i64(&mut db.connect().unwrap(), "SELECT COUNT(*) FROM T"), 99_000);
}
```

- [ ] **Step 2: Run the scale test locally in the optimized profile**

Run: `rustup run stable cargo test --profile exhaustive --test phase4c_scale phase_4c1_hundred_thousand_row_smoke -- --exact --ignored --nocapture`

Expected: PASS within 60 minutes, with 99,000 rows after reopen.

- [ ] **Step 3: Add the dedicated Linux Phase 4C1 job**

```yaml
  phase4c-sql-execution:
    name: Linux Phase 4C1 SQL execution
    runs-on: ubuntu-latest
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4
      - name: Install stable Rust
        run: rustup toolchain install stable --profile minimal
      - name: Run deterministic execution model
        run: rustup run stable cargo test phase_4c1_fixed_seed_execution_model -- --exact --nocapture
      - name: Run SQL crash recovery matrix
        run: rustup run stable cargo test phase_4c1_sql_crash_recovery_matrix -- --exact --ignored --nocapture
      - name: Run 100,000-row smoke
        run: >-
          rustup run stable cargo test --profile exhaustive --test phase4c_scale
          phase_4c1_hundred_thousand_row_smoke -- --exact --ignored --nocapture
```

Do not increase the existing 30-minute quality, MSRV, SQL front-end, or Windows job limits to hide a regression. Keep the current 240-minute exhaustive recovery jobs unchanged.

- [ ] **Step 4: Update README with only verified capability**

State that Phase 4C1 now provides transactional plain-table DDL, the complete documented SELECT subset, parameterized CRUD, streaming Rows, auto-commit, SQL/Rust transactions, and bounded memory. State that indexes/constraints remain Phase 4C2 and Oracle-specific types/functions/DUAL/ROWNUM/pagination/empty-string behavior remain Phase 5.

```markdown
Phase 4C1 executes the complete Phase 4B SELECT subset and provides
transactional plain-table DDL plus parameterized INSERT, UPDATE, and DELETE.
Indexes and constraints remain Phase 4C2 work. Oracle-specific type and
compatibility semantics remain Phase 5 work.
```

- [ ] **Step 5: Run the complete local release gate**

Run: `rustup run stable cargo fmt --check`

Run: `rustup run stable cargo clippy --all-targets --all-features -- -D warnings`

Run: `rustup run stable cargo test --all-targets`

Run: `rustup run 1.85.0 cargo test --all-targets`

Run: `rustup run stable cargo test phase_4c1_sql_crash_recovery_matrix -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test --profile exhaustive --test phase4c_scale phase_4c1_hundred_thousand_row_smoke -- --exact --ignored --nocapture`

Expected: every command exits 0; no test is skipped except unrelated explicitly ignored exhaustive suites.

- [ ] **Step 6: Inspect the final diff against the specification**

Run: `git diff --check main...HEAD`

Run: `git diff --stat main...HEAD`

Verify the diff contains no index/constraint persistence, cost optimizer, temp-file spill, Phase 5 behavior, disk-format change, unsafe block, or unapproved production dependency.

- [ ] **Step 7: Commit Task 16**

```bash
git add tests/phase4c_scale.rs .github/workflows/ci.yml README.md
git commit -m "test: gate phase 4c1 sql execution"
```

- [ ] **Step 8: Request final review before merge**

Use `superpowers:requesting-code-review` against `main`, address findings through `superpowers:receiving-code-review`, rerun the complete release gate, then use `superpowers:finishing-a-development-branch` to present merge/PR choices. Do not merge without the user's explicit direction.

---

## Execution Order

Execute tasks strictly by number: 1 through 16. Tasks 1-5 establish public types, storage ownership, resumable scans, immutable plans, and expression semantics. Tasks 6-8 complete SELECT. Tasks 9-13 add atomic mutations and transaction entry points. Tasks 14-16 provide independent correctness, recovery, scale, CI, and documentation gates.
