# Phase 4C2-D IndexScan Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add deterministic single-table `IndexScan` planning and execution, prove it is result/error-equivalent to `TableScan`, and provide opt-in Phase 4C2 scale gates without expanding MVP pull-request CI.

**Architecture:** Binding captures immutable index identities and ordered column metadata from the validated catalog. The rule planner extracts only safe AND-connected search terms, stores an `IndexId` plus executable bound templates, and never stores a `PageRef`. At execution, the current snapshot catalog supplies the index root, a storage range scan yields validated ROWIDs, the table is point-read, and the unchanged full WHERE filter rechecks every row.

**Tech Stack:** Rust 1.85+, existing SQL parser/binder/plan/operator pipeline, format-2 catalog, B+Tree `IndexStorage`, `Relation`, WAL snapshots, and GitHub Actions manual Deep CI.

**Spec:** `docs/superpowers/specs/2026-09-18-phase-4c2-index-constraints-design.md`

## Global Constraints

- Optimize only a single-table `SELECT` WHERE clause. JOIN sources and UPDATE/DELETE target discovery remain ROWID `TableScan` paths.
- Searchable terms are AND-connected leading-column equality/`IS NULL`, followed by at most one column with lower/upper bounds. OR, NOT, `!=`, IN, LIKE, arithmetic, functions, column-to-column comparison, and non-leading-only terms remain residual filters.
- Candidate order is exact: complete non-NULL unique equality, most equality-prefix columns, presence of a next-column range, more range bounds, then lower `IndexId`.
- Every `IndexScan` row is rechecked by the complete WHERE expression. A missing or mismatched table row is `Corruption` before Filter can hide it.
- Prepared plans retain only catalog identities and expressions. Execution opens the current snapshot's index/table roots after UUID and `SchemaVersion` validation.
- Runtime NULL supplied to `column = :parameter` or a range parameter produces an empty scan; it is never reinterpreted as `IS NULL`.
- Blob supports equality only. Range planning on Blob always falls back to `TableScan`.
- Search values must match the indexed column type, except lossless Int64-to-Decimal normalization. Unsafe Decimal-to-Int64 terms fall back to `TableScan`.
- No persistent format, feature bit, catalog record, B+Tree page, WAL, or public API format changes are permitted.
- User ruling dated 2026-09-27 overrides the older automatic-CI wording in the spec: `.github/workflows/ci.yml` remains the single Linux MVP validation job. Phase 4C2-D differential/scale gates are manual `workflow_dispatch` jobs and local acceptance commands.
- The 10-million-row/10-GB run is an opt-in pre-Phase-5 acceptance gate. Its harness lands in this phase, but the MVP PR is not blocked when the required host capacity is unavailable; Phase 5 remains blocked until the evidence report exists.

## Review Focus

- Parameter NULL versus `IS NULL`: Task 5 tests that equality/range NULL parameters return no rows while explicit `IS NULL` finds indexed NULL rows.
- Range fenceposts and prefix spill: Task 3 hand-checks `<`, `<=`, `>`, `>=`, BETWEEN, composite-prefix ceilings, minimum/maximum ROWID, and adjacent encoded values.
- Deterministic competing indexes: Task 2 permutes conjunction order and index declaration order while pinning the five-stage score and lower-`IndexId` tie-break.
- Prepared identity across root and schema changes: Task 6 proves ordinary DML/root splits keep a plan usable while index DROP/recreate or table schema change returns `SchemaChanged`.
- Logical index corruption: Task 7 proves missing rows, wrong-row mappings, non-empty leaf values, malformed keys, and duplicate logical entries return `Corruption` before residual filtering.

---

## File Structure

- `src/sql/bound/query.rs`: immutable bound index metadata attached to each query source.
- `src/sql/bind/query.rs`: catalog-to-bound-index capture in stable ID/column order.
- `src/sql/plan/index.rs`: searchable-term extraction, candidate scoring, and immutable index access templates.
- `src/sql/plan/query.rs`: selected access path retained by `QueryPlan`.
- `src/storage/index/scan.rs`: checked physical range construction and validated ROWID streaming.
- `src/storage/relation.rs`: bounded point lookup by ROWID for index-driven reads.
- `src/sql/exec/scan.rs`: `IndexScan` operator, runtime parameter bounds, row/index cross-check, and work-memory admission.
- `src/sql/exec/operator.rs`: index factory variant and snapshot/write-batch read facades.
- `src/sql/exec/query.rs`: use the selected path only for the eligible single source and keep the full Filter.
- `tests/phase4c2_index_scan_api.rs`: public SQL, prepared statement, transaction, snapshot, and fallback acceptance.
- `src/sql/exec/test_support.rs`: deterministic indexed-versus-unindexed differential model and test-only access observations.
- `src/storage/test_support.rs`: controlled logical index corruption witnesses.
- `tests/phase4c2_index_scan_scale.rs`: ignored 100k and opt-in 10-million-row/10-GB acceptance harnesses.
- `.github/workflows/deep-ci.yml`: manual Phase 4C2-D model and 100k job only.
- `README.md`: verified IndexScan scope and retained exclusions.

### Task 1: Capture Stable Index Metadata in Bound Query Sources

**Files:**
- Modify: `src/sql/bound/query.rs`
- Modify: `src/sql/bound/mod.rs`
- Modify: `src/sql/bind/query.rs`

**Interfaces:**
- Consumes: validated `CatalogReader::indexes_for_table` and `columns_for_index` order.
- Produces: `BoundIndex`, `BoundIndexColumn`, and `BoundSource::indexes()` for Task 2.

- [ ] **Step 1: Write the failing binder test**

```rust
#[test]
fn query_sources_capture_indexes_in_stable_id_and_column_order() {
    let fixture = phase_4c2b_constraint_fixture();
    let mut snapshot = fixture.pager().begin_read().unwrap();
    let catalog = snapshot.catalog_reader().unwrap();
    let bound = bind_sql(
        "SELECT ID FROM T WHERE ID = 7",
        &SqlLimits::default(),
        &catalog,
        fixture.pager().database_uuid().unwrap(),
    )
    .unwrap();
    let BoundStatementKind::Query { query } = bound.kind else { panic!() };
    assert_eq!(query.sources[0].indexes().len(), 1);
    assert_eq!(query.sources[0].indexes()[0].id(), fixture.index_id());
    assert_eq!(query.sources[0].indexes()[0].columns()[0].ordinal(), 0);
    assert_eq!(query.sources[0].indexes()[0].columns()[0].column_type(), ColumnType::Int64);
}
```

- [ ] **Step 2: Run the binder test and verify RED**

Run: `rustup run stable cargo test sql::bind::query::tests::query_sources_capture_indexes_in_stable_id_and_column_order -- --exact --nocapture`

Expected: compile failure because `BoundSource::indexes`, `BoundIndex`, and `BoundIndexColumn` do not exist.

- [ ] **Step 3: Add immutable bound index types**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundIndexColumn {
    column_id: ColumnId,
    ordinal: u16,
    column_type: ColumnType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundIndex {
    id: IndexId,
    unique: bool,
    columns: Box<[BoundIndexColumn]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundSource {
    pub(crate) source_id: SourceId,
    pub(crate) table_id: TableId,
    pub(crate) schema_version: SchemaVersion,
    indexes: Box<[BoundIndex]>,
}
```

Add read-only getters for every private field used by the planner. Do not retain index root pages, names, or ownership.

Provide checked constructors used by binder and plan tests:

```rust
impl BoundIndexColumn {
    pub(crate) const fn new(column_id: ColumnId, ordinal: u16, column_type: ColumnType) -> Self;
    fn from_column(column: &ColumnDef) -> Self;
}

impl BoundIndex {
    pub(crate) fn new(
        id: IndexId,
        unique: bool,
        columns: Box<[BoundIndexColumn]>,
    ) -> Result<Self>;
}
```

`BoundIndex::new` rejects an empty column list and non-contiguous ordinals as `Internal`; catalog validation should make either state unreachable in production.

- [ ] **Step 4: Populate metadata from the validated catalog**

```rust
fn indexes_for_source(catalog: &CatalogReader, table_id: TableId) -> Result<Box<[BoundIndex]>> {
    catalog
        .indexes_for_table(table_id)
        .map(|index| {
            let columns = catalog
                .columns_for_index(index.id())
                .map(BoundIndexColumn::from_column)
                .collect::<Vec<_>>()
                .into_boxed_slice();
            BoundIndex::new(index.id(), index.unique(), columns)
        })
        .collect::<Result<Vec<_>>>()
        .map(Vec::into_boxed_slice)
}
```

Change source construction to return `Result<Vec<BoundSource>>` and preserve catalog `BTreeSet<IndexId>` order.

- [ ] **Step 5: Run binder and planning regressions**

Run: `rustup run stable cargo test sql::bind::query -- --nocapture`

Run: `rustup run stable cargo test sql::plan -- --nocapture`

Expected: stable metadata test and all existing query-binding/plan tests pass.

- [ ] **Step 6: Commit bound metadata**

```bash
git add src/sql/bound/query.rs src/sql/bound/mod.rs src/sql/bind/query.rs
git commit -m "feat: bind stable query index metadata"
```

### Task 2: Extract Search Terms and Select One Deterministic Index

**Files:**
- Create: `src/sql/plan/index.rs`
- Modify: `src/sql/plan/mod.rs`
- Modify: `src/sql/plan/query.rs`

**Interfaces:**
- Consumes: Task 1 `BoundSource::indexes()` plus `BoundExpr` WHERE tree.
- Produces: `IndexAccessPlan`, equality/range templates, and `QueryPlan::index_access()` for Task 5.

- [ ] **Step 1: Add failing score and extraction tests**

```rust
#[test]
fn index_choice_obeys_unique_prefix_range_bounds_and_id_order() {
    let source = source_with_indexes([
        index(20, false, [col(1), col(2)]),
        index(10, false, [col(1), col(3)]),
        index(30, true, [col(1)]),
    ]);
    let where_expr = and(eq(column(1), parameter(1)), ge(column(2), literal(5)));
    assert_eq!(choose_index(&source, &where_expr).unwrap().index_id().get(), 30);

    let range_only = and(eq(column(1), parameter(1)), between(column(2), 5, 9));
    assert_eq!(choose_index(&source, &range_only).unwrap().index_id().get(), 20);
}

#[test]
fn unsupported_terms_remain_residual_and_never_make_a_candidate() {
    for predicate in [or(eq_col(1, 7), eq_col(1, 8)), not(eq_col(1, 7)),
        not_equal_col(1, 7), in_col(1, [7, 8]), like_col(1, "7%"),
        equal_columns(1, 2), add_equals(1, 1, 8)] {
        assert!(choose_index(&single_index_source(), &predicate).is_none());
    }
}
```

Add separate assertions for reversed comparisons, Blob range fallback, duplicate conjuncts, `IS NULL`, and lower `IndexId` ties.

Define the test-local builders in `index.rs` with these exact contracts so the cases do not depend on catalog I/O:

```rust
fn col(id: u64, ordinal: u16, ty: ColumnType) -> BoundIndexColumn;
fn index(id: u64, unique: bool, columns: &[(u64, u16, ColumnType)]) -> BoundIndex;
fn source(indexes: Vec<BoundIndex>) -> BoundSource;
fn column(id: u64, ordinal: u16, ty: SqlType) -> BoundExpr;
fn literal(value: Value, ty: SqlType) -> BoundExpr;
fn parameter(position: u16, ty: SqlType) -> BoundExpr;
fn binary(left: BoundExpr, op: BoundBinaryOp, right: BoundExpr) -> BoundExpr;
fn and(left: BoundExpr, right: BoundExpr) -> BoundExpr;
```

Every helper uses `SourceId(0)`, `TableId(1)`, `SchemaVersion(1)`, and distinct one-byte spans; it performs no parsing or storage access.

- [ ] **Step 2: Run planner tests and verify RED**

Run: `rustup run stable cargo test sql::plan::index::tests -- --nocapture`

Expected: compile failure because `sql::plan::index` and `choose_index` do not exist.

- [ ] **Step 3: Define the immutable access plan**

```rust
#[derive(Clone)]
pub(crate) enum EqualityValuePlan {
    Expression(ExecExpr),
    IsNull,
}

#[derive(Clone)]
pub(crate) struct RangeBoundPlan {
    expression: ExecExpr,
    inclusive: bool,
}

#[derive(Clone)]
pub(crate) struct IndexRangePlan {
    column_type: ColumnType,
    lower: Option<RangeBoundPlan>,
    upper: Option<RangeBoundPlan>,
}

#[derive(Clone)]
pub(crate) struct IndexAccessPlan {
    source: SourceId,
    index_id: IndexId,
    equality: Box<[(ColumnType, EqualityValuePlan)]>,
    range: Option<IndexRangePlan>,
}
```

Expose read-only getters; keep expressions immutable and retain no catalog root.

- [ ] **Step 4: Implement iterative AND extraction and safe value recognition**

Flatten only `BoundBinaryOp::And`. Accept a column paired with a literal or parameter, plus explicit non-negated `IS NULL` and non-negated `BETWEEN`. Reverse `<`, `<=`, `>`, `>=` when the column is on the right. Treat unary/arithmetic expressions as residuals. Record the first left-to-right usable equality, lower bound, and upper bound for each `ColumnId`; the full Filter preserves semantics for duplicates and contradictions.

```rust
fn searchable_value(expr: &BoundExpr, target: ColumnType) -> bool {
    matches!(expr.kind, BoundExprKind::Literal(_) | BoundExprKind::Parameter(_))
        && (sql_type_matches(target, expr.sql_type)
            || (target == ColumnType::Decimal && expr.sql_type == SqlType::Int64))
}
```

- [ ] **Step 5: Implement exact candidate scoring**

```rust
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct CandidateScore {
    unique_point_form: bool,
    equality_columns: usize,
    has_range: bool,
    range_bound_count: u8,
    reverse_index_id: Reverse<IndexId>,
}
```

`unique_point_form` requires every index column to have `=` rather than `IS NULL`; runtime NULL parameters still yield an empty scan. Stop equality-prefix construction at the first missing leading column, and consider a range only on the immediately following column. Reject Blob ranges.

- [ ] **Step 6: Attach the selected path to single-table QueryPlan only**

In `QueryPlan::from_bound`, call the selector only when `sources.len() == 1`, `joins.is_empty()`, and `selection.is_some()`. Compile selected value expressions with the existing `ExprCompiler`; continue compiling the entire selection independently for residual Filter evaluation.

- [ ] **Step 7: Run deterministic planner tests**

Run: `rustup run stable cargo test sql::plan::index::tests -- --nocapture`

Run: `rustup run stable cargo test sql::plan -- --nocapture`

Expected: all extraction, tie-break, fallback, and existing immutable-plan tests pass.

- [ ] **Step 8: Commit rule planning**

```bash
git add src/sql/plan/index.rs src/sql/plan/mod.rs src/sql/plan/query.rs
git commit -m "feat: plan deterministic index scans"
```

### Task 3: Build Checked Physical Index Ranges and Stream ROWIDs

**Files:**
- Create: `src/storage/index/scan.rs`
- Modify: `src/storage/index/mod.rs`
- Modify: `src/storage/index/key.rs`

**Interfaces:**
- Consumes: normalized equality values and optional next-column bounds.
- Produces: `IndexScanRange`, `IndexRowIdScan`, `ScannedIndexEntry`, and `IndexStorage::scan_rowids` for Task 5.

- [ ] **Step 1: Add failing golden range tests**

```rust
#[test]
fn composite_range_fences_do_not_spill_into_adjacent_prefixes() {
    let mut fixture = range_fixture(&[
        ([1, 4], 1), ([1, 5], 2), ([1, 5], u64::MAX),
        ([1, 6], 3), ([2, 5], 4),
    ]);
    assert_eq!(fixture.scan(eq([1]), gt(5), le(6)), [3]);
    assert_eq!(fixture.scan(eq([1]), ge(5), lt(6)), [2, u64::MAX]);
}

#[test]
fn null_equality_and_unbounded_sides_have_exact_physical_bounds() {
    let mut fixture = nullable_range_fixture();
    assert_eq!(fixture.scan(eq([Value::Null]), none(), none()), [1, 9]);
    assert_eq!(fixture.scan(no_eq(), none(), lt(Value::Int64(0))), [2]);
}
```

Also assert non-empty leaf values, malformed keys, zero ROWID, excessive arity, and Blob range requests return controlled errors.

- [ ] **Step 2: Run storage range tests and verify RED**

Run: `rustup run stable cargo test storage::index::scan::tests -- --nocapture`

Expected: compile failure because the range scan types do not exist.

- [ ] **Step 3: Define owned scan inputs and outputs**

```rust
pub(crate) struct IndexScanBound {
    pub(crate) value: Value,
    pub(crate) inclusive: bool,
}

pub(crate) struct IndexScanRange {
    pub(crate) equality: Box<[Value]>,
    pub(crate) lower: Option<IndexScanBound>,
    pub(crate) upper: Option<IndexScanBound>,
}

pub(crate) struct ScannedIndexEntry {
    physical: Vec<u8>,
    rowid: RowId,
}

pub(crate) struct IndexRowIdScan {
    storage: IndexStorage,
    state: BTreeScanState,
}
```

- [ ] **Step 4: Implement checked lower and upper physical bounds**

Use `IndexKeyCodec::encode_prefix` and a checked prefix successor. Return an explicit `BuiltRange::Empty` or `BuiltRange::Bounds { start, end }`; never encode an empty range as two unbounded sides. The exact mapping is:

- equality prefix only: `[prefix, prefix_successor)`;
- `>= value`: lower `Included(encoded_value_prefix)`;
- `> value`: lower `Included(encoded_value_prefix_successor)`;
- `< value`: upper `Excluded(encoded_value_prefix)`;
- `<= value`: upper `Excluded(encoded_value_prefix_successor)`.

Cap every range by the equality-prefix successor so it cannot enter the next composite prefix. For a first-column range, the equality ceiling is unbounded. Equality-prefix successor overflow and inclusive-upper successor overflow correctly mean an unbounded upper side. Strict-lower successor overflow means `BuiltRange::Empty`, because no key can be greater than the maximum encoded value. If the computed lower bound is not strictly before the upper bound, return `BuiltRange::Empty`.

The tests define a local `RangeFixture` that creates a two-column Int64 index with `IndexStorage::from_new`, inserts the exact `(values, rowid)` tuples shown in Step 1 into one `WriteBatch`, and exposes:

```rust
fn scan(
    &mut self,
    equality: Vec<Value>,
    lower: Option<IndexScanBound>,
    upper: Option<IndexScanBound>,
) -> Result<Vec<u64>>;
```

Add explicit cases for strict-lower successor overflow, contradictory `> 9 AND < 1`, and inclusive upper overflow.

- [ ] **Step 5: Stream and validate every physical entry**

```rust
impl IndexRowIdScan {
    pub(crate) fn next(&mut self, source: &mut impl PageSource)
        -> Result<Option<ScannedIndexEntry>>
    {
        let Some((physical, value)) = self.state.next(source)? else { return Ok(None) };
        if !value.is_empty() { return Err(corruption("index leaf value must be empty")); }
        let decoded = IndexKeyCodec::decode(&self.storage.schema.columns, &physical)?;
        Ok(Some(ScannedIndexEntry { physical, rowid: decoded.rowid() }))
    }
}
```

Add `validate_row(&self, entry, values)` by re-encoding the row with the entry ROWID and requiring byte-for-byte equality with `entry.physical`.

- [ ] **Step 6: Run storage and key-codec regressions**

Run: `rustup run stable cargo test storage::index::scan::tests -- --nocapture`

Run: `rustup run stable cargo test storage::index::key::tests -- --nocapture`

Expected: fenceposts, corruption, existing golden bytes, and prefix probes pass.

- [ ] **Step 7: Commit range storage**

```bash
git add src/storage/index/scan.rs src/storage/index/mod.rs src/storage/index/key.rs
git commit -m "feat: stream checked index rowid ranges"
```

### Task 4: Add Bounded Relation Point Lookup

**Files:**
- Modify: `src/storage/relation.rs`

**Interfaces:**
- Consumes: `RowId`, current `ReadSnapshot` or `WriteBatch`, and caller row-byte limit.
- Produces: `Relation::get_snapshot` and `Relation::get_batch` for Task 5.

- [ ] **Step 1: Write failing point-read tests**

```rust
#[test]
fn relation_point_lookup_distinguishes_present_missing_limit_and_corruption() {
    let fixture = point_read_fixture();
    assert_eq!(fixture.get(1, LARGE_ROW_BYTES).unwrap().unwrap().rowid().get(), 1);
    assert!(fixture.get(999, LARGE_ROW_BYTES).unwrap().is_none());
    assert_eq!(fixture.get(1, LARGE_ROW_BYTES - 1).unwrap_err().code(), ErrorCode::Type);
    fixture.corrupt_row(1);
    assert_eq!(fixture.get(1, LARGE_ROW_BYTES).unwrap_err().code(), ErrorCode::Corruption);
}
```

The test-local `PointReadFixture` uses `phase_4c1_query_relation_fixture` with row `(1, "x" repeated LARGE_ROW_BYTES payload)` and exposes `get(rowid, limit)` by starting a fresh read snapshot. `corrupt_row` uses the existing storage-only raw row corruption hook and commits before the next read.

- [ ] **Step 2: Run the relation test and verify RED**

Run: `rustup run stable cargo test storage::relation::tests::relation_point_lookup_distinguishes_present_missing_limit_and_corruption -- --exact --nocapture`

Expected: compile failure because point lookup is absent.

- [ ] **Step 3: Factor one strict row decoder and add snapshot/batch entry points**

```rust
impl Relation {
    pub(crate) fn get_snapshot(
        &self, snapshot: &mut ReadSnapshot, rowid: RowId, max_row_bytes: usize,
    ) -> Result<Option<StoredRow>> {
        self.get_from(snapshot, rowid, max_row_bytes)
    }

    pub(crate) fn get_batch(
        &self, batch: &mut WriteBatch, rowid: RowId, max_row_bytes: usize,
    ) -> Result<Option<StoredRow>> {
        self.get_from(batch, rowid, max_row_bytes)
    }
}
```

Use `BTree::get(encode_rowid_key(rowid))`, return `None` only for an absent key, and decode present bytes with the same `RowCodec` and byte limit as `RelationScan`.

- [ ] **Step 4: Reuse the strict decoder in RelationScan**

Factor the existing scan decode into a private helper so scan and point lookup cannot drift in type, strict-EOF, or resource-limit behavior.

- [ ] **Step 5: Run relation regressions**

Run: `rustup run stable cargo test storage::relation::tests -- --nocapture`

Expected: new point reads and existing scans/mutations pass.

- [ ] **Step 6: Commit point lookup**

```bash
git add src/storage/relation.rs
git commit -m "feat: add bounded relation rowid lookup"
```

### Task 5: Execute IndexScan and Preserve Full Filter Semantics

**Files:**
- Modify: `src/sql/exec/scan.rs`
- Modify: `src/sql/exec/operator.rs`
- Modify: `src/sql/exec/query.rs`
- Modify: `src/sql/exec/mod.rs`

**Interfaces:**
- Consumes: Tasks 2-4 access plan, storage range stream, and point read.
- Produces: `IndexScan` operator and factory path used by all public read APIs.

- [ ] **Step 1: Add failing operator tests for NULL and full-filter behavior**

```rust
#[test]
fn index_scan_distinguishes_null_parameter_from_is_null_and_rechecks_where() {
    let fixture = indexed_query_fixture();
    assert_eq!(fixture.query("SELECT ID FROM T WHERE A = :1", [Value::Null]), rows([]));
    assert_eq!(fixture.query("SELECT ID FROM T WHERE A IS NULL", []), rows([[3], [7]]));
    assert_eq!(
        fixture.query("SELECT ID FROM T WHERE A = 5 AND B <> 'reject'", []),
        rows([[2]])
    );
}
```

Add one test that queries inside a write transaction after inserting an indexed row, proving the scan reads the current `WriteBatch`.

Add an exact query-memory boundary test parallel to the existing TableScan payload-transfer test: one byte below the decoded row plus parameter frame returns `ResourceLimit`; the exact boundary succeeds; the admitted Text/Blob allocation is transferred rather than cloned.

The test-local `indexed_query_fixture()` creates `T(ID INT64 PRIMARY KEY, A INT64, B TEXT)` and `IX_T_A`, then commits rows `(1, NULL, "null")`, `(2, 5, "keep")`, `(3, 5, "reject")`, and `(7, NULL, "null-2")`. It returns the owned temporary database guard plus `Database`, so no test leaks or reuses a path.

- [ ] **Step 2: Run the operator tests and verify RED**

Run: `rustup run stable cargo test sql::exec::scan::tests::index_scan -- --nocapture`

Expected: plan selection exists but execution still builds `TableScan`, so the test-only access observation reports the wrong path.

- [ ] **Step 3: Extend factory and execution storage facades**

```rust
pub(crate) enum OperatorFactory {
    OneRow,
    TableScan { source: SourceId, relation: Relation, max_row_bytes: usize },
    IndexScan {
        source: SourceId,
        relation: Relation,
        index: IndexStorage,
        access: IndexAccessPlan,
        max_row_bytes: usize,
    },
    Filter { input: Box<OperatorFactory>, predicate: ExecExpr },
    Join { /* existing fields unchanged */ },
}
```

Add `ExecutionContext::next_index` and `get_relation` switches over `ReadSnapshot` and `WriteBatch`. Map only row-size `Type` from point decode to the existing query `ResourceLimit`; preserve `Corruption`, I/O, and transaction-fatal errors.

- [ ] **Step 4: Evaluate bounds once and normalize values safely**

On the first `IndexScan::next`, evaluate only the retained literal/parameter expressions against `EvalRow::default()`. Return an empty operator when any ordinary equality/range expression evaluates to NULL. Explicit `EqualityValuePlan::IsNull` emits `Value::Null`. Promote `Value::Int64` to Decimal only when the target is `ColumnType::Decimal`; reject every other shape mismatch as `Internal` because binding/planning admitted an impossible template.

- [ ] **Step 5: Implement the pull operator**

```rust
impl Operator for IndexScan {
    fn next(&mut self, context: &mut ExecutionContext<'_>) -> Result<Option<EvalRow>> {
        self.initialize_bounds(context)?;
        loop {
            let Some(entry) = context.next_index(self.scan_mut()?)? else { return Ok(None) };
            let row = context
                .get_relation(&self.relation, entry.rowid(), self.max_row_bytes)?
                .ok_or_else(|| corruption("index entry references a missing table row"))?;
            self.scan_ref()?.validate_row(&entry, row.values())?;
            let values = row.into_values();
            let memory_bytes = owned_values_bytes(values.iter())?;
            context.memory().reserve(memory_bytes)?;
            return Ok(Some(EvalRow::with_source(self.source, values, memory_bytes)?));
        }
    }
}
```

- [ ] **Step 6: Select IndexScan without removing Filter**

`source_factory` uses `QueryPlan::index_access()` only for the matching sole source. `query_factory` continues to wrap the source in `OperatorFactory::Filter` whenever `query.selection()` exists. JOIN right factories and all mutation planning remain unchanged.

- [ ] **Step 7: Run execution and public MVP regressions**

Run: `rustup run stable cargo test sql::exec::scan::tests -- --nocapture`

Run: `rustup run stable cargo test --test public_api --test phase4c_api --test phase4c2_constraint_api`

Expected: IndexScan-specific tests and the existing 40 public/API tests pass.

- [ ] **Step 8: Commit execution**

```bash
git add src/sql/exec/scan.rs src/sql/exec/operator.rs src/sql/exec/query.rs src/sql/exec/mod.rs
git commit -m "feat: execute snapshot-safe index scans"
```

### Task 6: Prove Public SQL, Prepared Identity, Transactions, and Snapshots

**Files:**
- Create: `tests/phase4c2_index_scan_api.rs`
- Modify: `tests/common/mod.rs`

**Interfaces:**
- Consumes: complete public prepare/query/transaction path.
- Produces: black-box Phase 4C2-D contract independent of internal test hooks.

- [ ] **Step 1: Add public point/range/composite result tests**

```rust
#[test]
fn indexed_point_range_null_and_residual_queries_match_expected_rows() {
    let mut connection = indexed_connection();
    for (sql, parameters, expected) in query_cases() {
        let statement = connection.prepare(sql).unwrap();
        let actual = collect_params(&mut connection, &statement, parameters);
        assert_eq!(sorted(actual), sorted(expected), "{sql}");
    }
}
```

Cases must cover every scalar index type, composite equality-plus-range, inclusive/exclusive bounds, `BETWEEN`, `IS NULL`, runtime parameter NULL, reversed comparison, and an ORDER BY query whose Sort makes output deterministic.

`indexed_connection()` creates exactly:

```sql
CREATE TABLE T (
  ID INT64 PRIMARY KEY,
  A INT64,
  B TEXT,
  C DECIMAL,
  F FLOAT64,
  RAW BLOB,
  D DATE,
  TS TIMESTAMP,
  FLAG BOOL,
  U INT64 UNIQUE
)
CREATE INDEX IX_T_A_B ON T(A, B)
CREATE INDEX IX_T_C ON T(C)
CREATE INDEX IX_T_RAW ON T(RAW)
```

Rows include NULL in every nullable indexed column, negative/zero/positive numeric boundaries, escaped zero bytes in Text/Blob, and duplicate NULL unique keys. `collect_params` binds public positional `ParameterValue` values and returns owned row vectors.

- [ ] **Step 2: Add explicit fallback result tests**

Use OR, NOT, `!=`, IN, LIKE, arithmetic, a non-leading column, Blob range, JOIN, and two-column comparison. Compare each result and error code with the same table after dropping its explicit index in a separate database.

Also pin stable errors: missing/wrong parameter type remains `Bind`/`Type`, row decode memory exhaustion remains `ResourceLimit`, stale identity remains `SchemaChanged`, and selected index/table corruption remains `Corruption`.

- [ ] **Step 3: Add prepared identity and root-split tests**

```rust
#[test]
fn prepared_index_scan_reopens_current_roots_but_rejects_index_ddl() {
    let statement = connection.prepare("SELECT ID FROM T WHERE A = :1").unwrap();
    insert_enough_rows_to_split_table_and_index(&mut connection);
    assert_eq!(query_one(&mut connection, &statement, 9000), Some(9000));
    execute_sql(&mut connection, "DROP INDEX IX_T_A").unwrap();
    assert_eq!(connection.query(&statement, params![9000]).unwrap_err().code(), ErrorCode::SchemaChanged);
}
```

Also drop/recreate the same index name with a new identity and require `SchemaChanged`, never rebinding.

- [ ] **Step 4: Add current-WriteBatch and fixed-snapshot tests**

One write transaction inserts/updates/deletes indexed rows and queries them before commit/rollback. One read transaction starts before a writer changes roots and proves it retains its old complete table/index view while a new reader sees the new view.

- [ ] **Step 5: Run the public target and verify GREEN**

Run: `rustup run stable cargo test --test phase4c2_index_scan_api -- --nocapture`

Expected: all result, fallback, prepared, transaction, and snapshot cases pass.

- [ ] **Step 6: Commit the public contract**

```bash
git add tests/phase4c2_index_scan_api.rs tests/common/mod.rs
git commit -m "test: prove public index scan contracts"
```

### Task 7: Reject Logical Index Corruption Before Residual Filtering

**Files:**
- Modify: `src/storage/test_support.rs`
- Modify: `src/sql/exec/test_support.rs`

**Interfaces:**
- Consumes: Task 5 row/index byte cross-check.
- Produces: controlled corruption evidence that normal Filter evaluation cannot mask.

- [ ] **Step 1: Add corruption fixtures and failing tests**

```rust
#[test]
fn phase_4c2d_index_scan_corruption_matrix() {
    for mutation in [
        RemoveReferencedTableRow,
        PointKeyAtWrongExistingRow,
        ReplaceLeafValueWithNonEmpty,
        TruncatePhysicalKey,
        ZeroPhysicalRowId,
        DuplicateLogicalEntryForExistingRow,
    ] {
        assert_index_scan_corruption(mutation, "SELECT ID FROM T WHERE A = 7 AND FALSE");
    }
}
```

The predicate deliberately contains `FALSE`; the scan must detect corruption before the residual Filter can discard the candidate.

`assert_index_scan_corruption` creates `T(ID PRIMARY KEY, A INT64)` plus `IX_T_A`, commits rows `(1, 7)` and `(2, 99)`, applies exactly one raw B+Tree mutation in a new committed `WriteBatch`, reopens the database, executes the supplied SQL, and asserts `ErrorCode::Corruption`. Raw mutations may bypass `ConstrainedTableWrite` only inside `src/storage/test_support.rs`.

- [ ] **Step 2: Run the exact matrix and verify RED where coverage is missing**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2d_index_scan_corruption_matrix -- --exact --nocapture`

Expected before completing Task 5 cross-checks: at least the wrong-existing-row or duplicate logical entry is silently filtered/emitted instead of returning `Corruption`.

- [ ] **Step 3: Complete the minimum production correction if RED found a gap**

Restrict changes to byte-for-byte re-encoding in `IndexRowIdScan::validate_row` or missing-row classification in `IndexScan`. Do not add open-time table/index comparison or persistent repair.

- [ ] **Step 4: Prove corruption outside the selected range is not touched**

Corrupt a key under prefix `A = 99`, run `A = 7`, and require the valid result. Then run `A = 99` and require `Corruption`. This pins bounded I/O rather than whole-index validation on every query.

- [ ] **Step 5: Run corruption and storage validation regressions**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2d_index_scan_corruption_matrix -- --exact --nocapture`

Run: `rustup run stable cargo test storage::test_support::phase_4c2c_foreign_key_catalog_corruption_matrix -- --exact --nocapture`

Expected: selected corruption fails deterministically; unrelated existing structural validation remains green.

- [ ] **Step 6: Commit corruption proof**

```bash
git add src/storage/test_support.rs src/sql/exec/test_support.rs src/storage/index/scan.rs src/sql/exec/scan.rs
git commit -m "test: reject corrupt index scan mappings"
```

### Task 8: Add a Fixed-Seed IndexScan/TableScan Differential Model

**Files:**
- Modify: `src/sql/exec/test_support.rs`

**Interfaces:**
- Consumes: public SQL execution plus test-only scan-path observations.
- Produces: ignored `phase_4c2d_fixed_seed_index_scan_model` with at least 4,000 deterministic query operations.

- [ ] **Step 1: Define twin tables and operation grammar**

```rust
enum QueryShape {
    UniquePoint, CompositePoint, CompositeRange, IsNull,
    ReversedRange, ResidualAnd, OrFallback, NonLeadingFallback,
    InFallback, LikeFallback, BlobEquality, BlobRangeFallback,
}

struct IndexScanModel {
    rows: BTreeMap<i64, ModelRow>,
    seed: u64,
    operations: usize,
}
```

Maintain identical `T_INDEXED` and `T_SCAN` tables; only the first has explicit/system indexes. Mutations apply to both inside equivalent transactions.

- [ ] **Step 2: Add the ignored model and verify RED observation**

```rust
#[test]
#[ignore = "run by the manual Phase 4C2-D Deep CI job"]
fn phase_4c2d_fixed_seed_index_scan_model() {
    run_index_scan_model(0x4c2d_2026_0927, 4_000).unwrap();
}
```

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2d_fixed_seed_index_scan_model -- --exact --ignored --nocapture`

Expected during RED: indexed and scan results may match, but the test-only observation proves no `IndexScan` operation occurred.

- [ ] **Step 3: Compare complete outcomes after every query**

Compare error code, row multiset, ordered output when ORDER BY exists, transaction state, and parameter NULL behavior. Record `IndexScan` and `TableScan` observation counts and require both nonzero. Generate Int64/Decimal mixed comparisons and require unsafe Decimal-to-Int64 cases to use fallback with identical results.

- [ ] **Step 4: Include schema and snapshot transitions**

At fixed intervals commit, rollback, reopen, checkpoint, split roots with DML, and rebuild an explicit index. Reprepare after intentional schema changes; separately retain one stale statement and require `SchemaChanged`.

- [ ] **Step 5: Run the model twice with the documented seed**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2d_fixed_seed_index_scan_model -- --exact --ignored --nocapture`

Run the identical command again.

Expected: both runs print the same 4,000-operation digest and nonzero path counts, with no divergence.

- [ ] **Step 6: Commit the differential model**

```bash
git add src/sql/exec/test_support.rs
git commit -m "test: model index scan equivalence"
```

### Task 9: Add 100k and Opt-In 10-Million-Row/10-GB Acceptance Harnesses

**Files:**
- Create: `tests/phase4c2_index_scan_scale.rs`
- Modify: `tests/support/mod.rs`

**Interfaces:**
- Consumes: public DDL/DML/query APIs and persisted database/WAL files.
- Produces: ignored 100k regular witness and explicitly gated large-host acceptance harness.

- [ ] **Step 1: Add the ignored 100k acceptance test**

```rust
#[test]
#[ignore = "run by the manual Phase 4C2-D Deep CI job"]
fn phase_4c2d_hundred_thousand_row_index_scan_smoke() {
    const ROWS: i64 = 100_000;
    let mut db = scale_database("phase4c2d-100k");
    load_rows_and_indexes(&mut db, ROWS, 64);
    assert_point_queries(&mut db, [1, 50_000, 100_000]);
    assert_composite_ranges(&mut db, ROWS);
    mutate_indexed_rows(&mut db, ROWS);
    checkpoint_reopen_and_repeat(&mut db);
}
```

- [ ] **Step 2: Run the 100k test and verify it exercises both point and range scans**

Run: `rustup run stable cargo test --profile exhaustive --test phase4c2_index_scan_scale phase_4c2d_hundred_thousand_row_index_scan_smoke -- --exact --ignored --nocapture`

Expected: PASS with 100,000 rows, point/range counts, reopen result digest, database bytes, WAL peak bytes, and elapsed durations printed. No shared-runner millisecond threshold is asserted.

- [ ] **Step 3: Add the opt-in large acceptance test**

```rust
#[test]
#[ignore = "requires explicit ORA_SQLLITE_LARGE_SCALE=1 and at least 30 GB free"]
fn phase_4c2d_ten_million_row_ten_gb_acceptance() {
    assert_eq!(std::env::var("ORA_SQLLITE_LARGE_SCALE").as_deref(), Ok("1"));
    const ROWS: i64 = 10_000_000;
    const PAYLOAD_BYTES: usize = 1_024;
    let mut db = scale_database("phase4c2d-10m-10gb");
    load_rows_and_indexes(&mut db, ROWS, PAYLOAD_BYTES);
    assert!(db.main_file_bytes() >= 10_000_000_000);
    run_large_acceptance_queries_and_mutations(&mut db, ROWS);
    checkpoint_reopen_and_repeat(&mut db);
    db.print_machine_readable_metrics();
}
```

The helper prints import, point query, range query, update, delete, checkpoint/reopen time, main-file size, and WAL peak in one stable key/value block.

Define the shared test helpers with these exact signatures:

```rust
fn scale_database(label: &str) -> ScaleDatabase;
fn load_rows_and_indexes(db: &mut ScaleDatabase, rows: i64, payload_bytes: usize);
fn assert_point_queries(db: &mut ScaleDatabase, ids: impl IntoIterator<Item = i64>);
fn assert_composite_ranges(db: &mut ScaleDatabase, rows: i64);
fn mutate_indexed_rows(db: &mut ScaleDatabase, rows: i64);
fn checkpoint_reopen_and_repeat(db: &mut ScaleDatabase);
fn run_large_acceptance_queries_and_mutations(db: &mut ScaleDatabase, rows: i64);
```

`ScaleDatabase` owns the temporary files and current `Database`; its file-size and WAL-peak methods use checked `u64` arithmetic and never delete paths it did not create.

- [ ] **Step 4: Compile but do not accidentally run the large gate**

Run: `rustup run stable cargo test --test phase4c2_index_scan_scale --no-run`

Run: `rustup run stable cargo test --test phase4c2_index_scan_scale -- --list`

Expected: both ignored tests are listed; no database is created by compile/list.

- [ ] **Step 5: Document the exact pre-Phase-5 large-host command**

PowerShell:

```powershell
$env:ORA_SQLLITE_LARGE_SCALE='1'
rustup run stable cargo test --profile exhaustive --test phase4c2_index_scan_scale phase_4c2d_ten_million_row_ten_gb_acceptance -- --exact --ignored --nocapture
Remove-Item Env:ORA_SQLLITE_LARGE_SCALE
```

The implementation report must state whether this command ran. If capacity prevents it, record the unresolved Phase-5 gate without claiming acceptance.

- [ ] **Step 6: Commit scale harnesses**

```bash
git add tests/phase4c2_index_scan_scale.rs tests/support/mod.rs
git commit -m "test: add Phase 4C2-D scale gates"
```

### Task 10: Documentation, Manual Deep CI, and Final Verification

**Files:**
- Modify: `.github/workflows/deep-ci.yml`
- Modify: `README.md`

**Interfaces:**
- Consumes: all prior Phase 4C2-D tasks.
- Produces: accurate supported-scope documentation, one manual deep job, and final local evidence.

- [ ] **Step 1: Add one manual Deep CI job without touching MVP PR CI**

```yaml
  phase4c2d-index-scan:
    name: Linux Phase 4C2-D IndexScan
    runs-on: ubuntu-latest
    timeout-minutes: 120
    steps:
      - uses: actions/checkout@v4
      - name: Install stable Rust
        run: rustup toolchain install stable --profile minimal
      - name: Run deterministic IndexScan differential model
        run: rustup run stable cargo test sql::exec::test_support::phase_4c2d_fixed_seed_index_scan_model -- --exact --ignored --nocapture
      - name: Run 100,000-row IndexScan smoke
        run: rustup run stable cargo test --profile exhaustive --test phase4c2_index_scan_scale phase_4c2d_hundred_thousand_row_index_scan_smoke -- --exact --ignored --nocapture
```

Do not add `push` or `pull_request` to `deep-ci.yml`. Do not modify `.github/workflows/ci.yml`.

- [ ] **Step 2: Update README verified scope**

State that single-table SELECT can use deterministic leading equality/NULL plus one next-column range; every candidate is table-read and full-filtered; joins and UPDATE/DELETE discovery remain `TableScan`; no statistics, cost optimizer, covering scan, ordering elimination, or Phase 5 behavior exists.

- [ ] **Step 3: Run format, strict lint, compilation, and public targets**

Run: `rustup run stable cargo fmt --check`

Run: `rustup run stable cargo clippy --all-targets --all-features -- -D warnings`

Run: `rustup run stable cargo check --all-targets`

Run: `rustup run 1.85.0 cargo check --all-targets`

Run: `rustup run stable cargo test --test public_api --test phase4c_api --test phase4c2_constraint_api --test phase4c2_index_scan_api`

Expected: all commands exit 0 with no warnings or failures.

- [ ] **Step 4: Run full and Phase 4C2-D local evidence**

Run: `rustup run stable cargo test --all-targets`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2d_fixed_seed_index_scan_model -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test --profile exhaustive --test phase4c2_index_scan_scale phase_4c2d_hundred_thousand_row_index_scan_smoke -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2a_index_crash_recovery_matrix -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2b_constraint_crash_recovery_matrix -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2c_foreign_key_crash_recovery_matrix -- --exact --ignored --nocapture`

Expected: the complete suite, 4,000-operation differential, 100k witness, and all three existing Phase 4C2 write/crash matrices pass. The 10-million-row/10-GB gate is reported separately and is never inferred from the 100k result.

- [ ] **Step 5: Run scope and MVP-CI audits**

Run: `rg -n "IndexScan|IndexAccessPlan|phase_4c2d" src tests README.md .github/workflows/deep-ci.yml`

Run: `rg -n "IndexScan" src/sql/plan/mutation.rs src/sql/exec/mutation.rs`

Run: `git diff --exit-code "$(git merge-base main HEAD)" -- .github/workflows/ci.yml`

Expected: IndexScan is confined to SELECT planning/execution and tests; mutation target discovery contains no IndexScan; MVP PR CI is byte-identical to `main`.

- [ ] **Step 6: Commit acceptance assets**

```bash
git add .github/workflows/deep-ci.yml README.md
git commit -m "docs: complete Phase 4C2-D acceptance"
```

---

## Final Review Gate

- [ ] Only eligible single-table SELECT plans retain an `IndexId`; no plan retains `PageRef`, mutable catalog state, or an index name for rebinding.
- [ ] Candidate scoring exactly follows unique point, equality-prefix length, range presence, bound count, and lower `IndexId`.
- [ ] Runtime NULL equality/range parameters produce an empty scan; explicit `IS NULL` remains searchable.
- [ ] Range bounds are fencepost-correct for inclusive/exclusive and composite prefix ceilings, including maximum ROWID.
- [ ] Every index entry has an empty value, decodes strictly, maps to an existing row, and re-encodes byte-for-byte before Filter sees it.
- [ ] Query work memory rejects one byte below the exact row/parameter boundary and transfers admitted variable payloads without an unmetered clone.
- [ ] Full WHERE Filter remains present for every IndexScan and preserves result/error equivalence.
- [ ] Prepared plans survive ordinary table/index root changes and reject schema/index identity changes as `SchemaChanged`.
- [ ] Read snapshots and current WriteBatch queries each use their own complete catalog/table/index roots.
- [ ] JOIN and UPDATE/DELETE discovery remain TableScan-only; Blob ranges and unsupported predicates fall back safely.
- [ ] No format, catalog, WAL, feature-bit, public API, automatic PR CI, or Phase 5 behavior is added.
- [ ] Local full tests, fixed-seed differential, and 100k acceptance are recorded. The 10-million-row/10-GB evidence is either recorded or explicitly left as a Phase-5 blocker.
- [ ] Existing Phase 4C2-A index, 4C2-B constraint, and 4C2-C foreign-key crash matrices remain green on the final tree.
