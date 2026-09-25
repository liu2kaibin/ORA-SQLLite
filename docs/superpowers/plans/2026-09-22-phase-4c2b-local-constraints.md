# Phase 4C2-B Local Constraints Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add persistent PRIMARY KEY, UNIQUE, NOT NULL, and CHECK constraints with constraint-owned indexes, stable names, atomic constrained CREATE TABLE, and named Constraint errors.

**Architecture:** Extend catalog V1 with a strictly decoded local-constraint graph and keep CheckProgram V1 as an independent, stable postorder bytecode under storage. SQL planning retains only logical column ordinals and a transient check template; constrained relation creation allocates stable IDs, resolves templates to ColumnIds, creates table and system-index roots, and publishes the complete graph in one WriteBatch. ConstrainedTableWrite opens explicit and system indexes together, performs named local checks before row/index mutation, and charges CHECK evaluation scratch to the existing WorkMemory budget.

**Tech Stack:** Rust 2024, MSRV Rust 1.85, existing Pager/B+Tree/WAL/Catalog/SQL pipeline, no unsafe, no new production dependencies.

**Spec:** `docs/superpowers/specs/2026-09-18-phase-4c2-index-constraints-design.md` section 17.2 only.

## Global Constraints

- Scope is exactly Phase 4C2-B: PrimaryKey, Unique, NotNull, Check, system-index ownership, CheckProgram V1, constrained CREATE TABLE, stable names, and named Constraint errors.
- ForeignKey and ReferencedBy remain Phase 4C2-C; IndexScan remains Phase 4C2-D.
- ALTER TABLE, deferred constraints, referential actions, and Phase 5 Oracle behavior remain unsupported.
- Preserve main-file format 2 and the already-shipped INDEX_CONSTRAINT_V1 feature bit; do not add a startup migration or full table/index comparison.
- Keep the public Connection, Statement, Rows, ExecutionResult, and transaction contracts unchanged.
- Keep SELECT, UPDATE target discovery, and DELETE target discovery on ROWID TableScan.
- Rust edition stays 2024 and rust-version stays 1.85; add no production dependency and no unsafe code.
- CheckProgram bytes are at most 1,048,576 bytes; all lengths, counts, offsets, IDs, ordinals, stack sizes, and memory estimates use checked arithmetic.
- Constraint violations use ErrorCode::Constraint and include the persisted constraint name; explicit unique-index violations continue to include the index name.
- Every persistent mutation remains inside the existing statement checkpoint and one WriteBatch.

---

## File and Responsibility Map

- Create src/storage/constraint/mod.rs: local-constraint runtime facade, named violation helper, and CheckMemory trait.
- Create src/storage/constraint/program.rs: stable CheckProgram V1 opcodes, scalar codec, strict validator, template resolution, and iterative evaluator.
- Create src/scalar.rs: storage-independent scalar operators shared by SQL expression execution and CheckProgram.
- Modify src/lib.rs: register the internal scalar module.
- Modify src/storage/mod.rs: register the constraint module.
- Modify src/storage/catalog/types.rs: ConstraintKind, ConstraintDef, CatalogRecord variants, and checked invariants.
- Modify src/storage/catalog/record.rs: exact ConstraintDef/mapping V1 codecs and strict error classification.
- Modify src/storage/catalog/mod.rs: decoded constraint graph, bidirectional validation, reader/writer APIs, object/name identity, system-index ownership, and drop cleanup.
- Modify src/storage/relation.rs: constrained relation specification/creation, all-index opening, named prechecks, CheckProgram execution, and local-constraint cleanup.
- Modify src/storage/index/mod.rs: distinguish explicit-index and constraint-owner uniqueness diagnostics without changing physical key behavior.
- Modify src/sql/bind/statement.rs: reject CHECK parameters, enforce persisted constraint-name availability, and keep ForeignKey deferred.
- Modify src/sql/plan/check.rs: compile BoundExpr into an owned postorder CheckProgram template without persistent IDs.
- Modify src/sql/plan/mod.rs and src/sql/plan/mutation.rs: executable local-constraint CreateTablePlan and retained locations.
- Modify src/sql/exec/memory.rs and src/sql/exec/mutation.rs: implement CheckMemory for WorkMemory and route constrained DDL/DML through storage.
- Modify src/storage/test_support.rs and src/sql/exec/test_support.rs: format/recovery capture, fixed-seed model, and crash matrix.
- Create tests/phase4c2_constraint_api.rs: public local-constraint, rollback, reopen, snapshot, and compatibility coverage.
- Create tests/phase4c2_constraint_scale.rs: ignored 100,000-row local-constraint acceptance.
- Modify .github/workflows/ci.yml: dedicated Linux Phase 4C2-B job.
- Modify README.md: verified 4C2-B scope and explicit 4C2-C/D exclusions.

### Task 1: Checked Constraint Types and Exact ConstraintDef V1 Codec

**Files:**
- Modify: src/storage/catalog/types.rs
- Modify: src/storage/catalog/record.rs

**Interfaces:**
- Consumes: existing ConstraintId, IndexId, TableId, CatalogName, CatalogRecord envelope V1, and mapping record helpers.
- Produces: ConstraintKind, ConstraintDef::new, constraint mapping codecs, and raw CheckProgram bytes capped at 1 MiB.

- [ ] **Step 1: Add failing constructor and hand-computed golden-byte tests**

~~~rust
#[test]
fn constraint_def_v1_has_exact_payload_and_round_trips() {
    let definition = ConstraintDef::new(
        ConstraintId::new(0x0102_0304_0506_0708).unwrap(),
        TableId::new(0x1112_1314_1516_1718).unwrap(),
        ConstraintKind::PrimaryKey,
        2,
        Some(IndexId::new(0x2122_2324_2526_2728).unwrap()),
        Box::new([]),
        CatalogName::new("PK_T".into()).unwrap(),
    ).unwrap();
    let encoded = encode_constraint_definition(&definition);
    assert_eq!(encoded[0], RecordKind::ConstraintDefinition.byte());
    assert_eq!(encoded[1], 1);
    assert_eq!(decode_constraint_definition(definition.id(), &encoded).unwrap(), definition);
}

#[test]
fn constraint_mappings_are_exact_eight_byte_v1_records() {
    let id = ConstraintId::new(9).unwrap();
    assert_eq!(decode_constraint_name_mapping(&encode_constraint_name_mapping(id)).unwrap(), id);
    assert_eq!(decode_table_constraint_mapping(&encode_table_constraint_mapping(id)).unwrap(), id);
    let column = ColumnId::new(10).unwrap();
    assert_eq!(decode_constraint_column_mapping(&encode_constraint_column_mapping(column)).unwrap(), column);
}
~~~

- [ ] **Step 2: Run focused tests and confirm the red state**

Run: rustup run stable cargo test constraint_def_v1_has_exact_payload_and_round_trips -- --exact

Run: rustup run stable cargo test constraint_mappings_are_exact_eight_byte_v1_records -- --exact

Expected: compilation fails because ConstraintKind, ConstraintDef, and the constraint record codecs do not exist.

- [ ] **Step 3: Define the stable checked type surface**

~~~rust
pub(crate) const MAX_CHECK_PROGRAM_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintKind {
    PrimaryKey,
    Unique,
    NotNull,
    Check,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConstraintDef {
    id: ConstraintId,
    table_id: TableId,
    kind: ConstraintKind,
    column_count: u16,
    index_id: Option<IndexId>,
    check_program: Box<[u8]>,
    name: CatalogName,
}

impl ConstraintDef {
    pub(crate) const fn id(&self) -> ConstraintId;
    pub(crate) const fn table_id(&self) -> TableId;
    pub(crate) const fn kind(&self) -> ConstraintKind;
    pub(crate) const fn column_count(&self) -> u16;
    pub(crate) const fn index_id(&self) -> Option<IndexId>;
    pub(crate) fn check_program(&self) -> &[u8];
    pub(crate) const fn name(&self) -> &CatalogName;
}
~~~

ConstraintDef::new enforces these exact shapes:

- PrimaryKey and Unique: column_count is 1..=MAX_COLUMNS, index_id is Some, and check_program is empty.
- NotNull: column_count is 1, index_id is None, and check_program is empty.
- Check: column_count is 0, index_id is None, and check_program is nonempty and no larger than MAX_CHECK_PROGRAM_BYTES.
- The V1 payload reserves parent IDs and action bytes in their final positions, but 4C2-B always writes them as zero. Kind code 5 (ForeignKey), a ReferencedBy record, or nonzero parent fields returns Unsupported with "foreign-key catalogs require Phase 4C2-C"; no ForeignKey constructor or catalog mutation API exists in this plan.

Add ConstraintName, ConstraintDefinition, TableConstraint, and ConstraintColumn variants to CatalogRecord. ReferencedBy remains outside the decoded 4C2-B record surface.

- [ ] **Step 4: Encode the exact V1 field layout**

Use this payload order so the format does not depend on Rust enum layout:

~~~text
u64 constraint_id
u64 table_id
u8  kind                 # 1 PK, 2 UNIQUE, 3 NOT NULL, 4 CHECK, 5 FOREIGN KEY
u8  flags                # must be zero in V1
u16 local_column_count
u64 system_index_id      # zero means None
u64 parent_table_id      # zero means None
u64 parent_constraint_id # zero means None
u8  deferred_mode        # zero only
u8  on_update_action     # zero only
u8  on_delete_action     # zero only
u8  reserved             # zero only
u32 check_program_len
u16 name_len
u16 reserved             # zero only
check_program bytes
name UTF-8 bytes
~~~

Implement encode_constraint_definition, decode_constraint_definition, encode/decode_constraint_name_mapping, encode/decode_table_constraint_mapping, and encode/decode_constraint_column_mapping through the existing V1 envelope. Use checked conversions for both lengths, require strict EOF, and map invalid persisted field combinations to Corruption.

- [ ] **Step 5: Add exhaustive malformed-record coverage**

~~~rust
#[test]
fn constraint_definition_rejects_every_truncation_reserved_byte_and_shape_conflict() {
    let bytes = encoded_check_constraint();
    for end in 0..bytes.len() {
        assert_eq!(
            decode_constraint_definition(ConstraintId::new(7).unwrap(), &bytes[..end])
                .unwrap_err().code(),
            ErrorCode::Corruption
        );
    }
    assert_constraint_corruption(mutate(&bytes, FLAGS_OFFSET, 1));
    assert_constraint_corruption(mutate(&bytes, DEFERRED_OFFSET, 1));
    assert_constraint_corruption(with_trailing_byte(&bytes));
}

#[test]
fn constraint_definition_enforces_kind_specific_zero_fields_and_program_limit() {
    assert_eq!(primary_key_without_index().unwrap_err().code(), ErrorCode::Type);
    assert_eq!(not_null_with_two_columns().unwrap_err().code(), ErrorCode::Type);
    assert_eq!(check_with_index().unwrap_err().code(), ErrorCode::Type);
    assert_eq!(check_with_bytes(MAX_CHECK_PROGRAM_BYTES + 1).unwrap_err().code(), ErrorCode::ResourceLimit);
}
~~~

Include wrong key/payload ID, zero ID, unknown kind, future record version, invalid UTF-8, 129-byte SQL name, check length overflow, short payload, and trailing-byte cases. Preserve existing IndexDef and table/column golden bytes.

- [ ] **Step 6: Run record and type suites on stable and MSRV**

Run: rustup run stable cargo test storage::catalog::record:: --lib

Run: rustup run stable cargo test storage::catalog::types:: --lib

Run: rustup run 1.85.0 cargo test storage::catalog::record:: --lib

Expected: all pass; the V1 field positions are stable, while ForeignKey bytes are recognized and rejected as Unsupported for Phase 4C2-C.

- [ ] **Step 7: Commit Task 1**

~~~text
git add src/storage/catalog/types.rs src/storage/catalog/record.rs
git commit -m "feat: add local constraint catalog records"
~~~

### Task 2: Bidirectional Local-Constraint Catalog Graph

**Files:**
- Modify: src/storage/catalog/mod.rs
- Modify: src/storage/catalog/types.rs

**Interfaces:**
- Consumes: ConstraintDef/mapping codecs from Task 1 and existing IndexDef ownership/root validation.
- Produces: CatalogReader constraint lookup/iteration APIs and CatalogWriter atomic constraint-bundle insertion/removal.

- [ ] **Step 1: Add failing complete-graph and corruption tests**

~~~rust
#[test]
fn catalog_requires_complete_bidirectional_local_constraint_graph() {
    let complete = local_constraint_catalog_fixture();
    assert!(open_catalog_image(&complete).is_ok());
    for family in [
        MissingFamily::ConstraintName,
        MissingFamily::ConstraintDefinition,
        MissingFamily::TableConstraint,
        MissingFamily::ConstraintColumn,
        MissingFamily::OwnedIndex,
    ] {
        assert_eq!(
            open_catalog_image(&complete.without(family)).unwrap_err().code(),
            ErrorCode::Corruption
        );
    }
}

#[test]
fn catalog_rejects_two_primary_keys_and_foreign_keys_in_phase_4c2b() {
    assert_eq!(open_catalog_image(&two_primary_keys()).unwrap_err().code(), ErrorCode::Corruption);
    assert_eq!(open_catalog_image(&foreign_key_constraint()).unwrap_err().code(), ErrorCode::Unsupported);
}
~~~

- [ ] **Step 2: Run focused catalog tests and confirm the red state**

Run: rustup run stable cargo test catalog_requires_complete_bidirectional_local_constraint_graph -- --exact

Expected: the constraint key families still return Unsupported and constraint-owned IndexDef records cannot open.

- [ ] **Step 3: Extend CatalogImage and decode all 4C2-B records**

Add these maps:

~~~rust
constraints: BTreeMap<ConstraintId, ConstraintDef>,
constraint_names: BTreeMap<String, ConstraintId>,
table_constraints: BTreeMap<TableId, BTreeSet<ConstraintId>>,
constraint_columns: BTreeMap<ConstraintId, BTreeMap<u16, ColumnId>>,
~~~

Decode ConstraintName, local ConstraintDefinition, TableConstraint, and ConstraintColumn when INDEX_CONSTRAINT_V1 is present. Decode ReferencedBy, ConstraintDef kind code 5, or nonzero parent/action fields as Unsupported with the exact message "foreign-key catalogs require Phase 4C2-C" before accepting the record into a 4C2-B image.

- [ ] **Step 4: Validate every forward and reverse edge**

CatalogImage::validate_mappings must establish all of the following:

- Constraint IDs participate in the global object-ID uniqueness set and are below next_object_id.
- Every definition has exactly one matching global name and TableConstraint mapping; every reverse mapping points back to that same definition.
- ConstraintColumn ordinals are contiguous from zero and match column_count; each mapped column exists and belongs to the owning table.
- A table has at most one PrimaryKey.
- PrimaryKey and Unique each reference exactly one unique IndexDef owned by Constraint(the same ConstraintId), on the same table, with the same ordered columns; every constraint-owned index has that reverse owner.
- PrimaryKey columns and the one NotNull column have ColumnDef::nullable() == false.
- NotNull has exactly one ConstraintColumn and no index.
- Check has no ConstraintColumn and no index; its raw program is nonempty and no larger than 1 MiB. Task 3 replaces this length-only gate with the strict bytecode/type-stack validator before any SQL path can create a constrained table.
- Explicit indexes still have no constraint owner and keep their existing behavior.

Use ErrorCode::Corruption for incomplete, contradictory, dangling, duplicate, wrong-owner, wrong-table, nonunique-system-index, wrong-column-order, and nullable-primary-key persisted images.

- [ ] **Step 5: Add stable reader and identity APIs**

~~~rust
pub(crate) fn find_constraint_by_id(&self, id: ConstraintId) -> Option<&ConstraintDef>;
pub(crate) fn find_constraint_by_name(&self, name: &CatalogName) -> Option<&ConstraintDef>;
pub(crate) fn constraints_for_table(&self, table: TableId)
    -> impl Iterator<Item = &ConstraintDef>;
pub(crate) fn columns_for_constraint(&self, constraint: ConstraintId)
    -> impl Iterator<Item = &ColumnDef>;
~~~

Return constraints and columns in ascending stable ID/ordinal order. Teach object_id_exists(ObjectKind::Constraint, ...) and object_name_exists(ObjectKind::Constraint, ...) the real namespaces.

- [ ] **Step 6: Add one atomic writer bundle and general root update**

~~~rust
pub(crate) struct ConstraintBundle {
    pub(crate) definition: ConstraintDef,
    pub(crate) columns: Box<[ColumnId]>,
    pub(crate) system_index: Option<(IndexDef, Box<[ColumnId]>)>,
}

pub(crate) fn insert_constraint_bundle(
    &mut self,
    batch: &mut WriteBatch,
    bundle: &ConstraintBundle,
) -> Result<()>;

pub(crate) fn remove_constraint(
    &mut self,
    batch: &mut WriteBatch,
    constraint: &ConstraintDef,
    columns: &[ColumnId],
) -> Result<()>;
~~~

insert_constraint_bundle prechecks all keys, IDs, roots, owners, counts, and columns before inserting any record in its nested mutation scope. It writes ConstraintDefinition, ConstraintName, TableConstraint, contiguous ConstraintColumn records, and the optional owned IndexDefinition/IndexName/TableIndex/IndexColumn records, tracks the system index tree, and enables INDEX_CONSTRAINT_V1.

Allow update_index_root for both Explicit and Constraint ownership. Keep public drop_index guarded by require_explicit_owner; only relation teardown may remove a system index and its owner constraint together.

- [ ] **Step 7: Test writer rollback, generated namespace, and object allocation**

~~~rust
#[test]
fn constraint_bundle_insert_remove_and_root_update_restore_exact_candidate() {
    let before = capture_batch_and_catalog(&batch);
    fail_after_each_constraint_record(|point| {
        let error = insert_bundle_with_failure(&mut batch, point).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Io);
        assert_eq!(capture_batch_and_catalog(&batch), before);
    });
}

#[test]
fn constraint_names_are_global_but_separate_from_index_names() {
    insert_constraint_named("PK_T");
    assert!(catalog.object_name_exists(ObjectKind::Constraint, &name("PK_T")));
    assert!(!catalog.object_name_exists(ObjectKind::Index, &name("PK_T")));
}
~~~

- [ ] **Step 8: Run catalog, pager, validation, and index suites**

Run: rustup run stable cargo test storage::catalog:: --lib

Run: rustup run stable cargo test storage::pager:: --lib

Run: rustup run stable cargo test storage::validate:: --lib

Run: rustup run stable cargo test storage::index:: --lib

Expected: all pass; valid explicit-index catalogs remain byte-compatible, and valid local-constraint catalogs open with complete reverse ownership.

- [ ] **Step 9: Commit Task 2**

~~~text
git add src/storage/catalog/mod.rs src/storage/catalog/types.rs
git commit -m "feat: validate local constraint catalog graph"
~~~

### Task 3: Stable CheckProgram V1 Codec, Validator, and Evaluator

**Files:**
- Create: src/storage/constraint/mod.rs
- Create: src/storage/constraint/program.rs
- Modify: src/storage/mod.rs
- Create: src/scalar.rs
- Modify: src/lib.rs
- Modify: src/storage/catalog/mod.rs
- Modify: src/sql/exec/expr.rs

**Interfaces:**
- Consumes: Value, ColumnDef/ColumnId/ColumnType, existing SQL three-valued scalar semantics, and MAX_CHECK_PROGRAM_BYTES.
- Produces: CheckType, CheckProgramTemplate, CheckProgram::resolve, validate, and evaluate plus CheckMemory.

- [ ] **Step 1: Add failing opcode, scalar, validation, and truth-result tests**

~~~rust
#[test]
fn check_program_v1_has_stable_postorder_bytes_and_column_ids() {
    let template = CheckProgramTemplate::new(vec![
        CheckTemplateInstruction::Column { ordinal: 0, result: CheckType::Int64 },
        CheckTemplateInstruction::Literal {
            value: Value::Int64(0),
            result: CheckType::Int64,
        },
        CheckTemplateInstruction::Binary {
            op: CheckBinaryOp::GreaterThan,
            result: CheckType::Bool,
        },
    ]).unwrap();
    let program = template.resolve(&[ColumnId::new(42).unwrap()]).unwrap();
    assert_eq!(&program.as_bytes()[..8], &[1, 0, 3, 0, 0, 0, 0, 0]);
    assert_eq!(program.validate(&check_columns()).unwrap(), CheckType::Bool);
}

#[test]
fn check_false_fails_but_true_and_null_pass() {
    assert_eq!(run_check("N > 0", Value::Int64(-1)).unwrap_err().code(), ErrorCode::Constraint);
    assert!(run_check("N > 0", Value::Int64(1)).is_ok());
    assert!(run_check("N > 0", Value::Null).is_ok());
}
~~~

- [ ] **Step 2: Run focused tests and confirm the red state**

Run: rustup run stable cargo test storage::constraint::program::tests --lib

Expected: compilation fails because the constraint module and CheckProgram types do not exist.

- [ ] **Step 3: Define a Rust-layout-independent program format**

Program header:

~~~text
u8  version = 1
u8  reserved = 0
u32 instruction_count
u16 reserved = 0
~~~

Each postorder instruction:

~~~text
u8  opcode
u8  result_type       # 1 Int64, 2 Decimal, 3 Float64, 4 Text, 5 Blob,
                      # 6 Date, 7 Timestamp, 8 TimestampWithTimeZone, 9 Bool
u16 operand_count
u32 payload_len
payload bytes
~~~

Use stable opcode groups:

- 0x01 Column, 0x02 Literal.
- 0x10 Plus, 0x11 Minus, 0x12 Not.
- 0x20 Add, 0x21 Subtract, 0x22 Multiply, 0x23 Divide, 0x24 Concat, 0x25 And, 0x26 Or.
- 0x28 Equal, 0x29 NotEqual, 0x2a LessThan, 0x2b LessThanOrEqual, 0x2c GreaterThan, 0x2d GreaterThanOrEqual.
- 0x30 IsNull, 0x31 IsNotNull.
- 0x40 In, 0x41 NotIn, 0x42 Between, 0x43 NotBetween, 0x44 Like, 0x45 NotLike.

Column payload is one big-endian u64 ColumnId. Literal payload begins with scalar tag 0 Null, 1 Int64, 2 Decimal, 3 Float64, 4 Text, 5 Blob, 6 Date, 7 Timestamp, 8 TimestampWithTimeZone, or 9 Bool, followed by a u32 payload length and exact V1 scalar bytes. Use fixed-width little-endian numeric/temporal fields matching their logical values, require finite Float64, validate Decimal fields, validate UTF-8 Text, and require strict scalar EOF. This is a CheckProgram-specific codec even where its bytes match RowCodec.

- [ ] **Step 4: Build templates iteratively and resolve ordinals only after ID allocation**

~~~rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckType {
    Int64,
    Decimal,
    Float64,
    Text,
    Blob,
    Date,
    Timestamp,
    TimestampWithTimeZone,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckUnaryOp { Plus, Minus, Not }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckBinaryOp {
    Add, Subtract, Multiply, Divide, Concat, And, Or,
    Equal, NotEqual, LessThan, LessThanOrEqual, GreaterThan, GreaterThanOrEqual,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CheckTemplateInstruction {
    Column { ordinal: u16, result: CheckType },
    Literal { value: Value, result: CheckType },
    Unary { op: CheckUnaryOp, result: CheckType },
    Binary { op: CheckBinaryOp, result: CheckType },
    IsNull { negated: bool, result: CheckType },
    InList { value_count: u16, negated: bool, result: CheckType },
    Between { negated: bool, result: CheckType },
    Like { negated: bool, result: CheckType },
}

pub(crate) struct CheckProgramTemplate {
    instructions: Box<[CheckTemplateInstruction]>,
}

impl CheckProgramTemplate {
    pub(crate) fn new(instructions: Vec<CheckTemplateInstruction>) -> Result<Self>;
    pub(crate) fn resolve(&self, columns: &[ColumnId]) -> Result<CheckProgram>;
}

pub(crate) struct CheckProgram {
    bytes: Box<[u8]>,
}
~~~

new validates operand counts and one final Bool result with an explicit type stack. resolve replaces each transient column ordinal with the corresponding stable ColumnId, encodes with checked length arithmetic, rejects a first byte over 1 MiB as ResourceLimit, and immediately validates the encoded result.

- [ ] **Step 5: Implement strict streaming validation and evaluation**

~~~rust
pub(crate) trait CheckMemory {
    fn reserve_check_bytes(&mut self, bytes: u64) -> Result<()>;
    fn release_check_bytes(&mut self, bytes: u64) -> Result<()>;
}

impl CheckProgram {
    pub(crate) fn from_bytes(bytes: Box<[u8]>) -> Result<Self>;
    pub(crate) fn as_bytes(&self) -> &[u8];
    pub(crate) fn validate(&self, columns: &[ColumnDef]) -> Result<CheckType>;
    pub(crate) fn evaluate(
        &self,
        columns: &[ColumnDef],
        row: &[Value],
        memory: &mut impl CheckMemory,
    ) -> Result<crate::scalar::TruthValue>;
}
~~~

Both validator and evaluator walk bytes and stacks iteratively. Validation rejects unknown opcodes/types, wrong payload sizes, wrong operand counts, stack underflow/extra values, foreign/missing columns, mismatched column types, invalid operator type combinations, non-Bool final result, truncation, and trailing bytes.

Evaluation reserves the value-stack container, each Value body, and owned Text/Blob payload before retaining them. It releases all reservations on success and every error path. Move TruthValue plus unary, binary, comparison, LIKE, finite-float, and value-validation helpers from sql/exec/expr.rs into src/scalar.rs. Define ScalarUnaryOp and ScalarBinaryOp there with eval_unary, eval_binary, sql_equal, sql_total_cmp, like, validate_sql_float, and validate_value entry points; SQL execution maps Bound operators to them, while CheckProgram maps stable V1 opcodes to them. Both evaluators call the same scalar functions, so arithmetic, comparison, and three-valued semantics have one implementation. FALSE maps to a caller-supplied named Constraint error, TRUE and NULL pass, and Type/ResourceLimit errors keep their original code.

~~~rust
pub(crate) enum ScalarUnaryOp { Plus, Minus, Not }
pub(crate) enum ScalarBinaryOp {
    Add, Subtract, Multiply, Divide, Concat, And, Or,
    Equal, NotEqual, LessThan, LessThanOrEqual, GreaterThan, GreaterThanOrEqual,
}

pub(crate) fn eval_unary(op: ScalarUnaryOp, value: Value) -> Result<Value>;
pub(crate) fn eval_binary(op: ScalarBinaryOp, left: &Value, right: &Value) -> Result<Value>;
pub(crate) fn sql_equal(left: &Value, right: &Value) -> Result<TruthValue>;
pub(crate) fn sql_total_cmp(left: &Value, right: &Value) -> Result<Ordering>;
pub(crate) fn like(text: &str, pattern: &str) -> bool;
pub(crate) fn validate_sql_float(value: f64) -> Result<()>;
pub(crate) fn validate_value(value: &Value) -> Result<()>;
~~~

- [ ] **Step 6: Add every-byte, stack, size, and memory-boundary tests**

~~~rust
#[test]
fn every_check_program_byte_truncation_is_corruption() {
    let bytes = complete_program().as_bytes().to_vec();
    for end in 0..bytes.len() {
        assert_eq!(CheckProgram::from_bytes(bytes[..end].into()).unwrap_err().code(), ErrorCode::Corruption);
    }
}

#[test]
fn check_evaluation_reserves_before_owned_literal_and_releases_on_error() {
    let required = check_required_bytes_for_text_literal("界");
    assert_eq!(evaluate_with_limit(required - 1).unwrap_err().code(), ErrorCode::ResourceLimit);
    let mut memory = TestCheckMemory::new(required);
    evaluate_program(&mut memory).unwrap();
    assert_eq!(memory.used(), 0);
}
~~~

Cover zero/exact/one-over 1 MiB, u32 count overflow, stack underflow, two final values, invalid result type, invalid Float64, Decimal boundaries, all boolean truth-table cells, IN/BETWEEN/LIKE negation, deep programs without recursion, invalid row length, and a ColumnId from another table.

- [ ] **Step 7: Wire catalog validation to the strict program validator**

For each Check ConstraintDef, construct CheckProgram::from_bytes from its persisted bytes and call validate with the owning table's ColumnDefs. Do not retain a decoded instruction vector in CatalogImage; keep only the checked raw program bytes so DML cannot use an uncharged cached program.

- [ ] **Step 8: Run constraint, scalar-expression, catalog, and MSRV suites**

Run: rustup run stable cargo test storage::constraint:: --lib

Run: rustup run stable cargo test sql::exec::expr:: --lib

Run: rustup run stable cargo test storage::catalog:: --lib

Run: rustup run 1.85.0 cargo check --all-targets

Expected: all pass; existing scalar SQL behavior remains unchanged and no recursive evaluator path is introduced.

- [ ] **Step 9: Commit Task 3**

~~~text
git add src/storage/constraint src/storage/mod.rs src/scalar.rs src/lib.rs src/storage/catalog/mod.rs src/sql/exec/expr.rs
git commit -m "feat: add persistent check program v1"
~~~

### Task 4: Bind and Plan Executable Local Constraints

**Files:**
- Modify: src/sql/bind/statement.rs
- Modify: src/sql/bound/statement.rs
- Create: src/sql/plan/check.rs
- Modify: src/sql/plan/mod.rs
- Modify: src/sql/plan/mutation.rs

**Interfaces:**
- Consumes: BoundConstraint, CheckProgramTemplate from Task 3, CatalogReader constraint names, and existing Locations.
- Produces: TableConstraintDefinition retained by CreateTablePlan; ForeignKey remains DeferredUnsupported with required_phase "4C2-C".

- [ ] **Step 1: Add failing binder tests for persistence-safe CHECK and names**

~~~rust
#[test]
fn create_table_rejects_check_parameters_before_planning() {
    let error = bind("CREATE TABLE T (N INT64, CHECK (N > :1))").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Bind);
}

#[test]
fn create_table_rejects_an_existing_global_constraint_name() {
    let fixture = catalog_with_constraint("CK_SHARED");
    let error = bind_with(&fixture, "CREATE TABLE T (N INT64, CONSTRAINT CK_SHARED CHECK (N > 0))")
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Bind);
}
~~~

- [ ] **Step 2: Add failing immutable-plan and phase-boundary tests**

~~~rust
#[test]
fn local_constraint_plans_are_executable_but_foreign_keys_stay_deferred() {
    let local = plan_sql(
        "CREATE TABLE T (ID INT64 PRIMARY KEY, U INT64 UNIQUE, N INT64 NOT NULL, CHECK (N > 0))"
    ).unwrap();
    assert!(matches!(local.kind(), PlanKind::CreateTable(_)));

    let foreign = plan_sql(
        "CREATE TABLE C (ID INT64, CONSTRAINT FK_C FOREIGN KEY (ID) REFERENCES P (ID))"
    ).unwrap();
    assert!(matches!(
        foreign.kind(),
        PlanKind::DeferredUnsupported { required_phase: "4C2-C" }
    ));
}
~~~

- [ ] **Step 3: Run binder and planner tests and confirm the red state**

Run: rustup run stable cargo test create_table_rejects_check_parameters_before_planning -- --exact

Run: rustup run stable cargo test local_constraint_plans_are_executable_but_foreign_keys_stay_deferred -- --exact

Expected: CHECK parameters bind successfully today and every constrained table still becomes DeferredUnsupported for Phase 4C2.

- [ ] **Step 4: Tighten binding without allocating persistent identities**

After bind_condition and require_scalar, walk the BoundExpr iteratively and return Bind if any BoundExprKind::Parameter, Aggregate, ordinary Column, or other-table reference appears. Only Literal, NewTableColumn, Unary, Binary, IsNull, InList, Between, and Like may reach a local CHECK plan.

For every explicit constraint name:

- reject duplicates within the statement;
- reject catalog.object_name_exists(ObjectKind::Constraint, name);
- add CreatedNameDependency { object_kind: ObjectKind::Constraint, name } so execution revalidates prepared names.

Do not invent generated names in the binder; their IDs do not exist until the storage create operation.

- [ ] **Step 5: Compile CHECK to an owned transient postorder template**

~~~rust
pub(super) fn compile_check(
    expression: &BoundExpr,
    locations: &Locations<'_>,
) -> Result<(CheckProgramTemplate, SqlLocation)> {
    // explicit (node, ready) stack; map NewTableColumn to its u16 ordinal
}
~~~

Map BoundUnaryOp and BoundBinaryOp explicitly to the stable Check operators. Reject Parameter, Aggregate, and ordinary Column as Internal because the binder must already have excluded them. Preserve the root SqlLocation for create-time diagnostics, but do not persist SQL text, spans, parser nodes, locations, or Rust enum discriminants.

- [ ] **Step 6: Retain exact local-constraint plan definitions**

~~~rust
pub(crate) enum TableConstraintDefinition {
    PrimaryKey {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
        location: SqlLocation,
    },
    Unique {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
        location: SqlLocation,
    },
    NotNull {
        name: Option<CatalogName>,
        column: u16,
        location: SqlLocation,
    },
    Check {
        name: Option<CatalogName>,
        template: CheckProgramTemplate,
        location: SqlLocation,
    },
}
~~~

Add constraints: Box<[TableConstraintDefinition]> to CreateTablePlan and update from_bound to compile PrimaryKey, Unique, NotNull, and Check. If any BoundConstraint::ForeignKey is present, Plan::from_bound returns DeferredUnsupported { required_phase: "4C2-C" }; otherwise it always returns CreateTable, including columns made nonnullable by constraints.

- [ ] **Step 7: Update exhaustive plan-shape tests**

Assert that:

- column and table constraint declaration order is retained;
- composite key ordinal order is retained;
- Check templates contain postorder operations and transient ordinals only;
- plan Debug output contains no SQL text, literal values, IDs, program bytes, or names beyond existing redaction rules;
- planner coverage still matches every BoundStatementKind;
- no local constraint plan contains a PageRef, TableId, ColumnId, ConstraintId, or IndexId.

- [ ] **Step 8: Run SQL front-end and plan suites**

Run: rustup run stable cargo test sql::bind:: --lib

Run: rustup run stable cargo test sql::plan:: --lib

Run: rustup run 1.85.0 cargo test sql::plan:: --lib

Expected: all pass; local constraints plan successfully, CHECK parameters fail before writer acquisition, and ForeignKey remains deferred.

- [ ] **Step 9: Commit Task 4**

~~~text
git add src/sql/bind/statement.rs src/sql/bound/statement.rs src/sql/plan/check.rs src/sql/plan/mod.rs src/sql/plan/mutation.rs
git commit -m "feat: plan executable local constraints"
~~~

### Task 5: Atomic Constrained CREATE TABLE and Stable Generated Names

**Files:**
- Modify: src/storage/relation.rs
- Modify: src/storage/catalog/mod.rs
- Modify: src/sql/exec/mutation.rs
- Modify: src/sql/plan/mutation.rs

**Interfaces:**
- Consumes: TableConstraintDefinition, CheckProgramTemplate, ConstraintBundle, and CatalogWriter.
- Produces: TableConstraintSpec, create_constrained_relation, persisted SYS_C names, system unique-index roots, and complete one-batch publication.

- [ ] **Step 1: Add failing storage tests for ID/name/root allocation**

~~~rust
#[test]
fn constrained_create_allocates_complete_graph_in_one_deterministic_order() {
    let table = create_constrained_relation(&mut batch, &constrained_spec()).unwrap();
    let catalog = batch.catalog_reader().unwrap();
    let constraints = catalog.constraints_for_table(table).collect::<Vec<_>>();
    assert_eq!(constraints.len(), 4);
    assert_eq!(constraints[0].name().as_str(), format!("SYS_C{}", constraints[0].id().get()));
    for constraint in constraints {
        if matches!(constraint.kind(), ConstraintKind::PrimaryKey | ConstraintKind::Unique) {
            let index = catalog.find_index_by_id(constraint.index_id().unwrap()).unwrap();
            assert_eq!(index.ownership(), IndexOwnership::Constraint(constraint.id()));
            assert!(index.unique());
            assert_eq!(index.name(), constraint.name());
        }
    }
}

#[test]
fn generated_constraint_names_use_checked_collision_suffixes() {
    occupy_constraint_name("SYS_C7");
    occupy_constraint_name("SYS_C7_1");
    assert_eq!(generate_constraint_name(ConstraintId::new(7).unwrap()).unwrap().as_str(), "SYS_C7_2");
}
~~~

- [ ] **Step 2: Run focused create tests and confirm the red state**

Run: rustup run stable cargo test constrained_create_allocates_complete_graph_in_one_deterministic_order -- --exact

Expected: constrained creation has no storage specification or executable path.

- [ ] **Step 3: Define a checked storage-level create specification**

~~~rust
pub(crate) struct ConstrainedTableSpec {
    table: TableSpec,
    constraints: Box<[TableConstraintSpec]>,
}

pub(crate) enum TableConstraintSpec {
    PrimaryKey { name: Option<CatalogName>, columns: Box<[u16]> },
    Unique { name: Option<CatalogName>, columns: Box<[u16]> },
    NotNull { name: Option<CatalogName>, column: u16 },
    Check { name: Option<CatalogName>, template: CheckProgramTemplate },
}

impl ConstrainedTableSpec {
    pub(crate) fn new(table: TableSpec, constraints: Vec<TableConstraintSpec>) -> Result<Self>;
}

pub(crate) fn create_constrained_relation(
    batch: &mut WriteBatch,
    spec: &ConstrainedTableSpec,
) -> Result<TableId>;
~~~

ConstrainedTableSpec::new validates one primary key maximum, nonempty/nonrepeated key columns, in-range ordinals, one-column NotNull, CHECK final Bool shape, duplicate explicit names, and nullable flags. It derives nullable=false for PrimaryKey and NotNull columns and rejects a caller whose ColumnSpec flags contradict that derived schema.

- [ ] **Step 4: Prevalidate the live catalog before the first mutation**

create_constrained_relation first opens CatalogReader and verifies:

- table name is still free;
- every explicit constraint name is still free;
- all statement-local explicit names are distinct;
- required object count can be added to next_object_id without overflow;
- every CheckProgram template can be structurally validated before resolution;
- no ForeignKey, ReferencedBy, ALTER, or Phase 5 shape is accepted.

Only after this read-only pass may it create a statement checkpoint and bootstrap CatalogWriter.

- [ ] **Step 5: Allocate stable IDs and generated names deterministically**

Inside the checkpoint allocate in this order:

1. TableId.
2. ColumnIds in table ordinal order.
3. For each constraint in declaration order, its ConstraintId followed immediately by its system IndexId when the kind is PrimaryKey or Unique.

For an unnamed constraint try SYS_C<constraint_id>, then suffixes _1, _2, and onward. Check suffix increment, decimal formatting length, and the 128-byte SQL identifier limit before each CatalogName construction. Test both committed catalog collisions and earlier names generated in the same CREATE. Persist the selected name and use the same name for its system index; index and constraint namespaces remain separate.

- [ ] **Step 6: Create roots and resolve CHECK programs before catalog publication**

Create one empty Table B+Tree and one empty Index B+Tree for each PrimaryKey/Unique. Resolve every CheckProgramTemplate through the allocated ordinal-to-ColumnId vector and enforce the 1 MiB cap. Build all TableDef, ColumnDef, ConstraintDef, IndexDef, and ConstraintBundle values in memory before inserting the first definition record.

If root allocation or program resolution fails, restore the statement checkpoint so no page, object ID, feature bit, or pending catalog gate remains consumed.

- [ ] **Step 7: Publish the complete candidate through one CatalogWriter**

Insert the table, all columns, and each ConstraintBundle; enable INDEX_CONSTRAINT_V1 through bundle insertion; finish CatalogWriter once. A plain table with no constraints continues through create_relation and does not enable the feature bit. A constrained table starts at SchemaVersion 1; no extra version bump is needed because no prior prepared statement can reference it.

Map CreateTablePlan to ConstrainedTableSpec in execute_create_table. Revalidate the table name and all explicit constraint names immediately before calling storage. Map storage preflight errors to the retained constraint/check location where a location exists.

- [ ] **Step 8: Add allocation, failure, and feature-bit rollback tests**

~~~rust
#[test]
fn constrained_create_failure_restores_ids_pages_feature_and_catalog() {
    for fault in constrained_create_fault_points() {
        let before = capture_full_batch(&batch);
        inject(fault);
        assert!(create_constrained_relation(&mut batch, &spec).is_err());
        assert_eq!(capture_full_batch(&batch), before);
    }
}

#[test]
fn plain_create_does_not_enable_index_constraint_feature() {
    create_relation(&mut batch, &plain_spec()).unwrap();
    assert_eq!(batch.feature_bits() & INDEX_CONSTRAINT_V1, 0);
}
~~~

Cover failure after table root, after each system-index root, after object allocation, after CheckProgram resolution, after every catalog record family, after feature enable, and during catalog finish.

- [ ] **Step 9: Run relation, catalog, mutation, and transaction suites**

Run: rustup run stable cargo test constrained_create --lib

Run: rustup run stable cargo test storage::relation:: --lib

Run: rustup run stable cargo test storage::catalog:: --lib

Run: rustup run stable cargo test sql::exec::mutation:: --lib

Expected: all pass; a failed CREATE leaves the exact pre-statement batch image and a successful CREATE reopens with complete table/constraint/index ownership.

- [ ] **Step 10: Commit Task 5**

~~~text
git add src/storage/relation.rs src/storage/catalog/mod.rs src/sql/exec/mutation.rs src/sql/plan/mutation.rs
git commit -m "feat: create constrained tables atomically"
~~~

### Task 6: Named Local-Constraint Enforcement in INSERT and UPDATE

**Files:**
- Modify: src/storage/constraint/mod.rs
- Modify: src/storage/relation.rs
- Modify: src/storage/index/mod.rs
- Modify: src/sql/exec/memory.rs
- Modify: src/sql/exec/mutation.rs

**Interfaces:**
- Consumes: CatalogReader local constraints, all owned indexes, CheckProgram evaluator, and WorkMemory.
- Produces: ConstraintCatalog, named prechecks, constraint-aware unique diagnostics, and atomic table/system-index maintenance.

- [ ] **Step 1: Add failing named-error and NULL-semantics tests**

~~~rust
#[test]
fn primary_unique_not_null_and_check_report_the_persisted_constraint_name() {
    assert_named_constraint_error(insert("PK_T", null_pk()), "PK_T");
    assert_named_constraint_error(insert("UQ_T", duplicate_unique()), "UQ_T");
    assert_named_constraint_error(insert("NN_T", null_not_null()), "NN_T");
    assert_named_constraint_error(insert("CK_T", failing_check()), "CK_T");
}

#[test]
fn ordinary_unique_allows_multiple_null_keys_but_primary_key_does_not() {
    insert_row(vec![Value::Null, Value::Null]).unwrap();
    insert_row(vec![Value::Null, Value::Null]).unwrap();
    assert_named_constraint_error(insert_primary(vec![Value::Null]), "PK_T");
}
~~~

- [ ] **Step 2: Run focused DML tests and confirm the red state**

Run: rustup run stable cargo test primary_unique_not_null_and_check_report_the_persisted_constraint_name -- --exact

Expected: constrained tables cannot yet execute, and ConstrainedTableWrite opens only explicit indexes.

- [ ] **Step 3: Open one validated local-constraint set**

~~~rust
pub(crate) struct ConstraintCatalog {
    constraints: Box<[LocalConstraint]>,
    index_owners: BTreeMap<IndexId, CatalogName>,
}

pub(crate) enum LocalConstraint {
    PrimaryKey { name: CatalogName, ordinals: Box<[u16]> },
    Unique { name: CatalogName, index_id: IndexId },
    NotNull { name: CatalogName, ordinal: u16 },
    Check { name: CatalogName, program: CheckProgram },
}

impl ConstraintCatalog {
    pub(crate) fn open(catalog: &CatalogReader, table: TableId) -> Result<Self>;
    pub(crate) fn validate_row(
        &self,
        columns: &[ColumnDef],
        values: &[Value],
        memory: &mut impl CheckMemory,
    ) -> Result<()>;
    pub(crate) fn violation_name_for_index(&self, index: IndexId) -> Option<&CatalogName>;
}
~~~

open clones the already validated definitions/program bytes and ordered constraint columns, sorted by ConstraintId. validate_row evaluates constraints in that order:

- PrimaryKey fails if any owned column is NULL.
- Unique has no separate row predicate; its system index performs uniqueness.
- NotNull fails if its one column is NULL.
- Check streams the program and fails only on FALSE.

Use one helper that creates ErrorCode::Constraint with "constraint <persisted-name> is violated".

- [ ] **Step 4: Teach IndexStorage the owner-specific diagnostic**

Store a UniqueViolationName enum in IndexSchema:

~~~rust
enum UniqueViolationName {
    ExplicitIndex(CatalogName),
    Constraint(CatalogName),
}
~~~

IndexStorage::open resolves Constraint(owner_id) through CatalogReader, verifies the reverse IndexId, and uses Constraint(name) for conflicts. Explicit indexes retain the exact "unique index <name> is violated" message. Do not change NULL handling, prefix probing, ROWID exclusion, key bytes, or tree mutation.

- [ ] **Step 5: Open and maintain every table index**

Remove the Explicit-only filter from ConstrainedTableWrite::open. Open all indexes_for_table in stable IndexId order, plus ConstraintCatalog for the table. Root checkpoint/restore and finish update every changed explicit or system root through CatalogWriter::update_index_root.

- [ ] **Step 6: Enforce all preconditions before persistent mutation**

Change the internal write signatures to accept the existing memory budget:

~~~rust
pub(crate) fn insert(
    &mut self,
    batch: &mut WriteBatch,
    values: &[Value],
    max_row_bytes: usize,
    memory: &mut impl CheckMemory,
) -> Result<RowId>;

pub(crate) fn replace(
    &mut self,
    batch: &mut WriteBatch,
    rowid: RowId,
    old: &[Value],
    new: &[Value],
    max_row_bytes: usize,
    memory: &mut impl CheckMemory,
) -> Result<()>;
~~~

For INSERT and replacement rows, perform this exact pre-mutation order:

1. Validate PrimaryKey, NotNull, and Check with named errors.
2. Validate RowCodec value shapes/nullability/row-size bounds.
3. Encode every explicit and system index entry and enforce 1024-byte keys.
4. Probe every unique index, excluding the current ROWID only for UPDATE.
5. Enter the nested mutation scope and change table/index/catalog state.

DELETE needs no local predicate, but deletes every explicit and system index entry before the table row inside the same restorable scope.

- [ ] **Step 7: Charge CHECK work to WorkMemory**

Implement CheckMemory for WorkMemory by forwarding reserve/release. Change execute_insert to borrow mutable WorkMemory and pass usize::try_from(memory.remaining()).unwrap_or(usize::MAX) as max_row_bytes. Before the existing generic update replacement validation, run named local checks so RowCodec cannot replace a named NotNull/PrimaryKey diagnostic with a generic one; derive replacement max_row_bytes from the same remaining-budget calculation already used by validate_update_replacement, and make ConstrainedTableWrite recheck at its mutation boundary. Release all CHECK reservations per row before scanning the next ROWID.

- [ ] **Step 8: Add UPDATE conflict, immediate semantics, and rollback tests**

~~~rust
#[test]
fn update_checks_old_and_unprocessed_unique_keys_immediately() {
    execute("INSERT INTO T VALUES (1)").unwrap();
    execute("INSERT INTO T VALUES (2)").unwrap();
    let error = execute("UPDATE T SET ID = 3 - ID").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert_eq!(query_ids(), vec![1, 2]);
    assert_system_index_matches_table();
}

#[test]
fn later_check_failure_restores_prior_rows_and_every_index() {
    begin();
    let before = visible_table_and_indexes();
    assert_named_constraint_error(execute("UPDATE T SET N = N - 2"), "CK_POSITIVE");
    assert_eq!(visible_table_and_indexes(), before);
    execute("INSERT INTO T VALUES (9, 9)").unwrap();
}
~~~

Cover composite keys, multiple NULL unique keys, primary NULL, duplicate INSERT, self-ROWID UPDATE, key swaps failing immediately, Check TRUE/FALSE/NULL, arithmetic Type errors, low WorkMemory, 1024-byte key boundary, root split/collapse, auto-commit, explicit transaction continuation, rollback, commit, and reopen.

- [ ] **Step 9: Run relation, index, DML, public API, and MSRV suites**

Run: rustup run stable cargo test storage::relation:: --lib

Run: rustup run stable cargo test storage::index:: --lib

Run: rustup run stable cargo test sql::exec::mutation:: --lib

Run: rustup run stable cargo test --test phase4c2_constraint_api dml_ -- --nocapture

Run: rustup run 1.85.0 cargo test --test phase4c2_constraint_api

Expected: all pass; one visible row has exactly one entry in every owned index and ordinary errors leave explicit transactions usable.

- [ ] **Step 10: Commit Task 6**

~~~text
git add src/storage/constraint/mod.rs src/storage/relation.rs src/storage/index/mod.rs src/sql/exec/memory.rs src/sql/exec/mutation.rs
git commit -m "feat: enforce local constraints in dml"
~~~

### Task 7: Constraint-Aware DROP TABLE and Protected System Indexes

**Files:**
- Modify: src/storage/relation.rs
- Modify: src/storage/catalog/mod.rs
- Modify: src/storage/index/mod.rs
- Modify: src/sql/exec/mutation.rs

**Interfaces:**
- Consumes: complete table constraint/index graph and existing deep validation/release staging.
- Produces: atomic local-constraint teardown and stable rejection of independent system-index drops.

- [ ] **Step 1: Add failing DROP lifecycle tests**

~~~rust
#[test]
fn drop_system_index_by_name_is_rejected_without_mutation() {
    let before = capture_database_state();
    let error = execute("DROP INDEX PK_T").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert!(error.to_string().contains("PK_T"));
    assert_eq!(capture_database_state(), before);
}

#[test]
fn drop_constrained_table_releases_all_local_roots_and_records() {
    let roots = owned_table_and_index_roots("T");
    execute("DROP TABLE T").unwrap();
    assert_no_table_constraint_or_index_records("T");
    assert_all_roots_released_after_safe_reuse(roots);
    assert_index_constraint_feature_still_enabled();
}
~~~

- [ ] **Step 2: Run focused DROP tests and confirm the red state**

Run: rustup run stable cargo test drop_constrained_table_releases_all_local_roots_and_records -- --exact

Expected: current drop_relation rejects or leaves constraint-owned records and roots.

- [ ] **Step 3: Preflight the full teardown graph**

Before releasing any page, drop_relation opens CatalogReader, clones the relation, every local constraint with ordered columns, and every index with ordered columns, then calls validate_deep_batch. Assert in memory that each system index is paired with exactly one PrimaryKey/Unique owner. ReferencedBy is impossible in 4C2-B; if any such key is decoded, return Unsupported for Phase 4C2-C without mutation.

- [ ] **Step 4: Remove system indexes and constraints in dependency order**

Inside one statement checkpoint and one CatalogWriter:

1. Remove explicit indexes through their exact records.
2. For each PrimaryKey/Unique owner, remove its system-index records and stage its whole tree for release.
3. Remove ConstraintColumn, TableConstraint, ConstraintName, and ConstraintDefinition records for every local constraint.
4. Remove column/table records and stage the table tree for release.
5. Finish the catalog once.

Do not clear INDEX_CONSTRAINT_V1. Keep public drop_index guarded before CatalogWriter mutation; its system-index error must include the persisted owner constraint name.

- [ ] **Step 5: Add failure restoration and stale-plan tests**

~~~rust
#[test]
fn every_constrained_drop_failure_restores_records_roots_and_release_queue() {
    for fault in constrained_drop_fault_points() {
        let before = capture_full_batch(&batch);
        inject(fault);
        assert!(drop_relation(&mut batch, table_id).is_err());
        assert_eq!(capture_full_batch(&batch), before);
    }
}
~~~

Cover failure after each explicit/system index release, after each constraint family deletion, after table release, and at finish. Verify a prepared DROP INDEX cannot be rebound to a recreated object, and a dropped/recreated table invalidates old DML through database UUID/table SchemaVersion identity as already contracted.

- [ ] **Step 6: Run DROP, catalog, freelist, recovery, and API tests**

Run: rustup run stable cargo test drop_relation --lib

Run: rustup run stable cargo test drop_index --lib

Run: rustup run stable cargo test storage::catalog:: --lib

Run: rustup run stable cargo test --test phase4c2_constraint_api drop_ -- --nocapture

Expected: all pass; no released page remains reachable and no system index can be dropped independently.

- [ ] **Step 7: Commit Task 7**

~~~text
git add src/storage/relation.rs src/storage/catalog/mod.rs src/storage/index/mod.rs src/sql/exec/mutation.rs
git commit -m "feat: tear down local constraints atomically"
~~~

### Task 8: Public SQL/API Contract and Prepared-Statement Revalidation

**Files:**
- Create: tests/phase4c2_constraint_api.rs
- Modify: tests/common/mod.rs
- Modify: src/api/connection.rs
- Modify: src/api/transaction.rs
- Modify: src/sql/bound/statement.rs
- Modify: src/sql/exec/mutation.rs

**Interfaces:**
- Consumes: executable CreateTablePlan, existing Statement schema/name dependencies, and local-constraint DDL/DML.
- Produces: end-to-end public coverage without changing public types or method signatures.

- [ ] **Step 1: Add failing public CRUD scenarios**

~~~rust
#[test]
fn local_constraints_create_crud_commit_rollback_and_reopen() {
    let (files, database) = support::new_database("phase4c2-local-constraints");
    let mut connection = database.connect().unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE T (
            ID INT64 CONSTRAINT PK_T PRIMARY KEY,
            EMAIL TEXT CONSTRAINT UQ_T UNIQUE,
            SCORE INT64 CONSTRAINT NN_T NOT NULL,
            CONSTRAINT CK_T CHECK (SCORE BETWEEN 0 AND 100)
        )",
    );
    let insert = connection.prepare(
        "INSERT INTO T (ID, EMAIL, SCORE) VALUES (:1, :2, :3)"
    ).unwrap();
    let values = [
        Value::Int64(1).into(),
        Value::Text("a@example".into()).into(),
        Value::Int64(50).into(),
    ];
    connection.execute(&insert, Parameters::positional(&values)).unwrap();
    assert_constraint(&connection, "INSERT INTO T VALUES (1, 'b@example', 20)", "PK_T");
    assert_constraint(&connection, "INSERT INTO T VALUES (2, 'a@example', 20)", "UQ_T");
    assert_constraint(&connection, "INSERT INTO T VALUES (2, 'b@example', NULL)", "NN_T");
    assert_constraint(&connection, "INSERT INTO T VALUES (2, 'b@example', 101)", "CK_T");
    drop(connection);
    let reopened = support::reopen(&files);
    assert_eq!(support::query_i64(&reopened, "SELECT ID FROM T"), vec![1]);
}
~~~

- [ ] **Step 2: Add failing prepared-name race tests**

~~~rust
#[test]
fn prepared_constrained_create_rechecks_table_and_explicit_constraint_names() {
    let prepared = connection.prepare(
        "CREATE TABLE A (ID INT64 CONSTRAINT PK_SHARED PRIMARY KEY)"
    ).unwrap();
    support::execute_sql(
        &mut connection,
        "CREATE TABLE B (ID INT64 CONSTRAINT PK_SHARED PRIMARY KEY)",
    );
    let error = connection.execute(&prepared, Parameters::none()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::SchemaChanged);
    assert_no_table(&connection, "A");
}
~~~

- [ ] **Step 3: Run the public target and confirm the red state**

Run: rustup run stable cargo test --test phase4c2_constraint_api -- --nocapture

Expected: the new target fails until Tasks 4 through 7 connect the full public path.

- [ ] **Step 4: Revalidate every retained created name before writer acquisition**

Extend BoundStatement/Statement validation so the table and every explicit constraint CreatedNameDependency must still be free in the execution catalog. Preserve the current ordering:

1. validate execute API kind and parameters;
2. validate database UUID and table SchemaVersions;
3. validate created table/constraint names;
4. acquire the writer and establish the statement checkpoint;
5. storage revalidates the same names adjacent to mutation.

A race returns SchemaChanged with no catalog/page/ID change. Generated names are selected only under the writer and are not statement dependencies.

- [ ] **Step 5: Verify public results and unchanged transaction semantics**

Tests must assert:

- constrained CREATE TABLE returns affected_rows() == 0;
- each successful INSERT/UPDATE/DELETE returns the existing affected-row count;
- violation error.code() is Constraint and its display text contains the stable persisted name;
- the same prepared DML statement remains usable after ordinary table/system-index root splits;
- an ordinary constraint error restores only the statement and leaves an explicit transaction usable;
- rollback removes constrained DDL/DML together; commit and auto-commit survive reopen;
- Rows, column metadata, parameter metadata, and Rust transaction entry points are unchanged.

- [ ] **Step 6: Cover all supported declaration forms**

Add table-level composite PrimaryKey/Unique, column-level named and unnamed PrimaryKey/Unique/NotNull/Check, quoted identifiers, empty Text versus NULL, multiple NULL unique keys, explicit NotNull plus PrimaryKey on one column, and CHECK expressions using arithmetic, boolean, comparison, IS NULL, IN, BETWEEN, and LIKE.

Assert these stay Unsupported without mutation: ForeignKey, self-reference, ALTER TABLE, DEFERRABLE, ON DELETE/UPDATE actions, Oracle functions, and all Phase 5 type/function syntax.

- [ ] **Step 7: Run public, connection, transaction, and full Phase 4C targets**

Run: rustup run stable cargo test --test phase4c2_constraint_api -- --nocapture

Run: rustup run stable cargo test api::connection:: --lib

Run: rustup run stable cargo test api::transaction:: --lib

Run: rustup run stable cargo test --test phase4c_api

Run: rustup run stable cargo test --test phase4c2_index_api

Expected: all pass with no public API signature changes and no explicit-index regression.

- [ ] **Step 8: Commit Task 8**

~~~text
git add tests/phase4c2_constraint_api.rs tests/common/mod.rs src/api/connection.rs src/api/transaction.rs src/sql/bound/statement.rs src/sql/exec/mutation.rs
git commit -m "test: prove public local constraint contracts"
~~~

### Task 9: Snapshot, Format-2 Compatibility, and Corruption Coverage

**Files:**
- Modify: src/storage/test_support.rs
- Modify: src/storage/catalog/mod.rs
- Modify: src/storage/pager.rs
- Modify: tests/support/format2.rs
- Modify: tests/phase4c2_constraint_api.rs

**Interfaces:**
- Consumes: raw format-2 catalog/WAL helpers, CatalogReader candidate validation, and CheckProgram strict decoder.
- Produces: real-file compatibility fixtures, old/new snapshot assertions, and controlled corruption classifications.

- [ ] **Step 1: Add failing old-snapshot and real-file tests**

~~~rust
#[test]
fn old_snapshot_keeps_old_catalog_roots_and_rows_across_constraint_dml() {
    let old = connection.begin_read().unwrap();
    update_constrained_rows(&connection);
    assert_eq!(query_snapshot(&old), old_rows());
    assert_eq!(query_connection(&connection), new_rows());
    assert_catalog_roots_match_each_snapshot();
}

#[test]
fn legacy_format2_without_feature_opens_and_plain_crud_remains_byte_compatible() {
    let files = support::format2::legacy_catalog_fixture();
    let database = Database::open(files.path(), Options::default()).unwrap();
    plain_crud(&database);
    assert_eq!(support::format2::catalog_record_families(files.path()), vec![0x00, 0x10, 0x11, 0x12, 0x13, 0x14]);
}
~~~

- [ ] **Step 2: Run compatibility tests and confirm the red state**

Run: rustup run stable cargo test legacy_format2_without_feature_opens_and_plain_crud_remains_byte_compatible -- --exact

Expected: legacy files pass, while local-constraint fixture/capture helpers are absent.

- [ ] **Step 3: Extend raw capture without using production decoders as the oracle**

Add independent test-only decoding for:

- constraint names, definitions, table mappings, and ordered columns;
- system-index ownership, roots, and ordered columns;
- raw CheckProgram byte length/version;
- feature bits, catalog root, table root, index roots, next_object_id, freelist, and accepted LSN.

The helper must parse bytes directly from tests/support/format2.rs or storage test support rather than calling ConstraintDef or CheckProgram production decoders for expected values.

- [ ] **Step 4: Add the local-constraint corruption matrix**

For committed real files, mutate one case at a time and assert controlled open/validation failure:

- missing or aliased ConstraintName/TableConstraint/ConstraintColumn;
- noncontiguous or duplicate constraint ordinals;
- wrong table/column owner;
- two PrimaryKeys;
- PrimaryKey/Unique without an index or with a nonunique/wrong-table/wrong-column index;
- IndexOwnership::Constraint without reverse ConstraintDef or mismatched IDs;
- nullable PrimaryKey/NotNull ColumnDef;
- Check with columns/index, empty/oversized program, unknown opcode/type, stack underflow, non-Bool final value, invalid ColumnId, truncation, or trailing bytes;
- constraint records without INDEX_CONSTRAINT_V1;
- INDEX_CONSTRAINT_V1 without CATALOG_V1;
- unknown future program/record versions as Unsupported;
- ForeignKey ConstraintDef or ReferencedBy in this phase as Unsupported.

A damaged committed candidate must not fall back to an older complete commit. Only the already-proven unfinished WAL tail rule may discard data.

- [ ] **Step 5: Prove shallow-open and explicit deep validation boundaries**

Ordinary open must scan and cross-check the catalog, ConstraintDef, CheckProgram bytes, and root page identities, but must not walk table/index descendants or compare logical table rows to index entries. Explicit validation walks every table/system/explicit index tree, decodes every index key, proves live/free disjointness, and rejects wrong tree kind or overlapping descendants.

- [ ] **Step 6: Cover checkpoint and repeated recovery**

Create constrained state, commit DML that splits table and system-index roots, hold an old reader, checkpoint a target LSN, close, reopen twice, and compare raw capture each time. Every snapshot must pair its own table root, index roots, constraints, and rows; no mixed root set or regenerated constraint name is accepted.

- [ ] **Step 7: Run format, snapshot, catalog, pager, and WAL suites**

Run: rustup run stable cargo test phase_4c2b_format2 --lib

Run: rustup run stable cargo test phase_4c2b_constraint_corruption --lib

Run: rustup run stable cargo test --test phase4c2_constraint_api snapshot_ -- --nocapture

Run: rustup run stable cargo test storage::pager:: --lib

Run: rustup run stable cargo test storage::wal:: --lib

Expected: all pass; old format-2 remains writable without migration and damaged constraint candidates fail with the specified code.

- [ ] **Step 8: Commit Task 9**

~~~text
git add src/storage/test_support.rs src/storage/catalog/mod.rs src/storage/pager.rs tests/support/format2.rs tests/phase4c2_constraint_api.rs
git commit -m "test: cover constraint snapshots and corruption"
~~~

### Task 10: Fixed-Seed Constraint Model and Crash-Recovery Matrix

**Files:**
- Modify: src/sql/exec/test_support.rs
- Modify: src/storage/test_support.rs
- Modify: tests/phase4c2_constraint_api.rs

**Interfaces:**
- Consumes: public SQL execution, independent raw state capture, existing deterministic RNG/fault hooks, and recovery child harness.
- Produces: phase_4c2b_fixed_seed_constraint_model and phase_4c2b_constraint_crash_recovery_matrix.

- [ ] **Step 1: Add a failing independent reference model**

~~~rust
#[test]
#[ignore = "run by the dedicated Phase 4C2-B CI job"]
fn phase_4c2b_fixed_seed_constraint_model() {
    for seed in [0x4c2b_0001_u64, 0x4c2b_0002, 0x4c2b_0003] {
        let mut model = ConstraintModel::new(seed);
        let mut database = ModelDatabase::new(seed);
        for _step in 0..2_000 {
            let operation = model.next_operation();
            assert_same_outcome(model.apply(&operation), database.apply(&operation));
            assert_eq!(model.rows(), database.rows());
            assert_one_entry_per_visible_row_per_index(&database);
            assert_catalog_constraint_graph(&database);
            assert_transaction_state_matches(&model, &database);
        }
    }
}
~~~

The independent model uses ordinary Rust maps/sets and direct predicate functions, not production index keys, CheckProgram, or catalog decoders. Generate constrained CREATE, INSERT, UPDATE, DELETE, DROP, BEGIN, COMMIT, and ROLLBACK over PrimaryKey, composite Unique, NotNull, and Check with NULLs and duplicates. Compare success/error code, stable violation name, affected rows, visible rows, index logical entries, constraint graph, and transaction usability after every step.

- [ ] **Step 2: Run the model and confirm the red state**

Run: rustup run stable cargo test sql::exec::test_support::phase_4c2b_fixed_seed_constraint_model -- --exact --ignored --nocapture

Expected: compilation fails until the model and raw local-constraint capture exist.

- [ ] **Step 3: Add named constrained crash scenarios**

The child harness must execute and identify these scenarios:

- constrained-create-empty;
- constrained-create-many-records;
- insert-splits-table-and-two-system-indexes;
- update-changes-primary-unique-and-check;
- delete-collapses-table-and-system-index;
- drop-constrained-table-releases-all-roots;
- explicit-transaction-multiple-rows-then-commit;
- failing-check-restores-statement-then-later-commit.

For each scenario, first run without a crash to enumerate real occurrences for catalog-page frame, table-page frame, each system-index frame, WAL record/frame write, WAL sync, memory publication, main-file checkpoint write/sync, and directory sync hooks. Require every named hook family to have at least one observed target before accepting the matrix.

- [ ] **Step 4: Require exact old-or-new recovery**

~~~rust
#[test]
#[ignore = "run by the dedicated Phase 4C2-B CI job"]
fn phase_4c2b_constraint_crash_recovery_matrix() {
    for target in observed_constraint_fault_targets() {
        let recovered = crash_and_reopen(target);
        assert!(recovered == target.old_state || recovered == target.new_state);
        assert_complete_constraint_graph(&recovered);
        assert_table_and_all_indexes_match(&recovered);
        assert_eq!(reopen_again(), recovered);
    }
}
~~~

State equality includes accepted LSN, feature bits, next_object_id, catalog root, table/index roots, constraint names/definitions/columns, rows, logical index entries, and freelist. A recovered state containing half a constraint bundle, a regenerated SYS_C name, or mismatched table/system-index rows fails.

- [ ] **Step 5: Run model and crash matrix locally**

Run: rustup run stable cargo test sql::exec::test_support::phase_4c2b_fixed_seed_constraint_model -- --exact --ignored --nocapture

Run: rustup run stable cargo test sql::exec::test_support::phase_4c2b_constraint_crash_recovery_matrix -- --exact --ignored --nocapture

Expected: both pass with deterministic seeds, every required fault family observed, and each recovery exactly old or new.

- [ ] **Step 6: Commit Task 10**

~~~text
git add src/sql/exec/test_support.rs src/storage/test_support.rs tests/phase4c2_constraint_api.rs
git commit -m "test: model and crash local constraints"
~~~

### Task 11: 100,000-Row Acceptance, CI, Documentation, and Final Gates

**Files:**
- Create: tests/phase4c2_constraint_scale.rs
- Modify: .github/workflows/ci.yml
- Modify: README.md
- Modify: docs/superpowers/plans/2026-09-22-phase-4c2b-local-constraints.md

**Interfaces:**
- Consumes: all prior 4C2-B tasks and existing exhaustive profile.
- Produces: dedicated Linux 4C2-B gate, updated project status, and recorded completion evidence.

- [ ] **Step 1: Add the ignored 100,000-row constraint acceptance**

~~~rust
#[test]
#[ignore = "run by the dedicated Phase 4C2-B CI job"]
fn phase_4c2b_hundred_thousand_row_constraint_smoke() {
    let (_files, database) = support::new_database("phase4c2-constraint-scale");
    let connection = database.connect().unwrap();
    support::create_scale_constraint_table(&connection);
    support::insert_constraint_rows(&connection, 100_000);
    support::update_constraint_rows(&connection);
    support::delete_constraint_rows(&connection);
    support::assert_constraint_rows_and_indexes(&connection, 100_000);
    support::assert_duplicate_null_and_check_boundaries(&connection);
    support::reopen_and_assert_constraint_state(database);
}
~~~

The table has a PrimaryKey, a composite Unique with deterministic NULL rows, a NotNull column, a Check expression, and one explicit nonunique index. Exercise batched explicit transactions, updates that change all indexed columns, deletes that collapse trees, rollback, checkpoint, and reopen. Assert correctness only; do not add a shared-runner millisecond threshold.

- [ ] **Step 2: Run the scale target**

Run: rustup run stable cargo test --profile exhaustive --test phase4c2_constraint_scale phase_4c2b_hundred_thousand_row_constraint_smoke -- --exact --ignored --nocapture

Expected: pass with 100,000 inserted rows and a complete final table/explicit-index/system-index/constraint graph.

- [ ] **Step 3: Add the dedicated Linux Phase 4C2-B CI job**

Add a 120-minute ubuntu-latest job named "Linux Phase 4C2-B local constraints" that installs stable Rust and runs:

~~~yaml
- name: Run deterministic local-constraint model
  run: rustup run stable cargo test sql::exec::test_support::phase_4c2b_fixed_seed_constraint_model -- --exact --ignored --nocapture
- name: Run local-constraint crash recovery matrix
  run: rustup run stable cargo test sql::exec::test_support::phase_4c2b_constraint_crash_recovery_matrix -- --exact --ignored --nocapture
- name: Run 100,000-row local-constraint smoke
  run: rustup run stable cargo test --profile exhaustive --test phase4c2_constraint_scale phase_4c2b_hundred_thousand_row_constraint_smoke -- --exact --ignored --nocapture
~~~

Keep the existing Linux stable, Rust 1.85 MSRV, Windows, Phase 4B, Phase 4C1, Phase 4C2-A, storage-model, WAL, and catalog jobs unchanged.

- [ ] **Step 4: Update README with verified scope**

State that Phase 4C2-B supports persistent PrimaryKey, Unique, NotNull, and Check constraints; system-owned unique indexes; atomic constrained table lifecycle; named Constraint errors; rollback/reopen/recovery; and existing bounded WorkMemory. State explicitly that ForeignKey is Phase 4C2-C, IndexScan is Phase 4C2-D, SELECT and DML target discovery still use TableScan, and Phase 5 Oracle extensions remain deferred.

- [ ] **Step 5: Run formatting, lint, stable, MSRV, and public tests**

Run: rustup run stable cargo fmt --check

Run: rustup run stable cargo clippy --all-targets --all-features -- -D warnings

Run: rustup run stable cargo test --all-targets

Run: rustup run 1.85.0 cargo test --all-targets

Expected: all commands exit 0; only explicitly ignored model/crash/scale tests are skipped.

- [ ] **Step 6: Run every 4C2-B specialized gate**

Run: rustup run stable cargo test sql::exec::test_support::phase_4c2b_fixed_seed_constraint_model -- --exact --ignored --nocapture

Run: rustup run stable cargo test sql::exec::test_support::phase_4c2b_constraint_crash_recovery_matrix -- --exact --ignored --nocapture

Run: rustup run stable cargo test --profile exhaustive --test phase4c2_constraint_scale phase_4c2b_hundred_thousand_row_constraint_smoke -- --exact --ignored --nocapture

Expected: all pass with deterministic seeds, complete fault-family coverage, and no logical table/index mismatch.

- [ ] **Step 7: Inspect exact scope**

Run: git diff --check

Run: git status --short

Run: rg -n "ForeignKey|ReferencedBy|IndexScan|ALTER TABLE|CASCADE|DEFERRABLE" src tests README.md

Expected: no whitespace errors; changes are limited to 4C2-B; ForeignKey/ReferencedBy occur only in parse/bind/deferred/rejection tests; no IndexScan, ALTER, cascade, deferred-constraint, or Phase 5 behavior was implemented.

- [ ] **Step 8: Record completion evidence and commit**

Append the verified command results, test counts, specialized-gate seeds/scenario counts, and CI run URL under the Completion Evidence heading below.

~~~text
git add tests/phase4c2_constraint_scale.rs .github/workflows/ci.yml README.md docs/superpowers/plans/2026-09-22-phase-4c2b-local-constraints.md
git commit -m "test: gate phase 4c2b local constraints"
~~~

## Phase 4C2-B Completion Evidence

Record local and remote evidence here as each gate completes. Task 11 remains
incomplete until required local gates and remote CI have verified results.

### Task 11 local evidence — 2026-09-25

- `rustup run stable cargo fmt --check` — PASS after formatting the new scale
  regression.
- `rustup run stable cargo clippy --all-targets --all-features -- -D warnings`
  — PASS.
- `rustup run stable cargo test --all-targets` — PASS: unit suite 869 passed,
  16 ignored; public and integration targets also passed. The new
  `phase4c2_constraint_scale` target reported 1 passed and 1 ignored; the
  ignored 100,000-row target was run separately below.
- `rustup run 1.85.0 cargo test --all-targets` — the initial attempt was
  BLOCKED before compilation when Windows returned `could not execute process
  rustc -vV` / `拒绝访问` (os error 5). A later local run appeared to pass, but
  GitHub's clean Linux build proved that it had reused stable artifacts; it is
  not accepted as MSRV evidence.
- `rustup run stable cargo test --test phase4c2_constraint_scale
  phase_4c2b_stale_format2_oracle_does_not_reject_persisted_freelist_after_constrained_update
  -- --exact --nocapture` — PASS. This normal 100-row regression proves the
  independent format-2 witness accepts the nonempty persisted freelist created
  by a constrained update, and validates the persisted constraint graph.
- `rustup run stable cargo test --profile exhaustive --test
  phase4c2_constraint_scale phase_4c2b_hundred_thousand_row_constraint_smoke
  -- --exact --ignored --nocapture` — PASS: 100,000 inserted rows, constrained
  updates/deletes/rollback/checkpoint/reopen, and raw final table/index/constraint
  graph; 1,721.54 seconds.
- `rustup run stable cargo test
  sql::exec::test_support::phase_4c2b_fixed_seed_constraint_model -- --exact
  --ignored --nocapture` — PASS: 6,000 total steps across seeds
  `0x4c2b0001`, `0x4c2b0002`, and `0x4c2b0003`; 1,102.26 seconds.
- `rustup run stable cargo test
  sql::exec::test_support::phase_4c2b_constraint_crash_recovery_matrix --
  --exact --ignored --nocapture` — PASS: all 8 named scenarios and 491 observed
  crash targets recovered to exactly old or new state; 2,129.51 seconds.
- Scope checks: `git diff --check` passed. The required keyword scan found only
  pre-existing parser/binder/catalog rejection coverage and explicit deferred
  documentation for ForeignKey/ReferencedBy/IndexScan/ALTER/CASCADE/DEFERRABLE;
  Task 11 adds none of those behaviors.
- Remote CI — the first run exposed Rust 1.85-incompatible let-chain syntax;
  the compatibility fix has clean local compile evidence and requires a remote
  rerun before the MSRV gate can pass.

### Task 11 Fix Round 1 local evidence — 2026-09-25

- Added an independent raw physical witness for the explicit nonunique index,
  `PK_T` system index, and `UQ_T` system index. It verifies full hand-derived
  post-delete physical key sequences and counts, raw constraint/index
  bidirectional ownership mappings, and checkpoint/reopen persistence.
- Added deterministic collapse coverage: at 300 rows and at the ignored 100,000
  row gate, table/PK/UQ roots are internal with positive height before delete;
  after retaining three deterministic rows they are leaf roots of height zero,
  and each root reference changes.
- Added a table-overflow witness. It validates a legitimate overflow value
  independently (checksum, header, references, chunk lengths, and full chain)
  while index leaves still require empty inline values and reject overflow.
  The observed deterministic witness was `storage_code=1`, `inline_len=0`,
  `total_value_len=9121`, chain pages `6/1 -> 7/1 -> 8/1`.
- RED evidence: the initial raw-index witness failed because it accepted only
  explicit-index catalog records; the initial collapse assertion failed when a
  half delete left table root type 2. The first exact 100,000-row run failed
  after 1,177.48 seconds only because the test witness rejected a valid table
  overflow value (`leaf cell uses unsupported storage or an excessive value`).
- GREEN evidence: after fixing the witness, `rustup run stable cargo test
  --profile exhaustive --test phase4c2_constraint_scale
  phase_4c2b_hundred_thousand_row_constraint_smoke -- --exact --ignored
  --nocapture` passed in 1,479.77 seconds. Its final output was
  `phase 4C2-B 100k: final_rows=3; table pre=type2/height2 post=type3/height0;
  PK pre=type4/height2 post=type5/height0; UQ pre=type4/height2
  post=type5/height0; physical_entries explicit=3 pk=3 uq=3;
  elapsed=1479.65s`.
- Fresh local regression evidence after the fix: `cargo fmt --check` and strict
  stable Clippy passed; `cargo test --test phase4c2_constraint_scale` passed
  3 tests with the 100k gate explicitly ignored; index API passed 18/18;
  constraint API passed 10/10; `cargo test --all-targets` passed (core: 869
  passed, 16 ignored; all integration targets passed, including the scale
  target at 3 passed and 1 ignored). `git diff --check` passed.
- Remote CI initially failed the Rust 1.85 and Phase 4B jobs on unsupported
  let-chain syntax. The compatibility fix also replaced the one later-stable
  `usize::is_multiple_of` use found by a forced clean 1.85 compile; the remote
  rerun remains required.
- Fix Round 2 worktree evidence: `git status --short` was run before this
  evidence-only update. Exact result: no output (clean worktree).

### Final local revalidation — 2026-09-25

- `rustup run stable cargo test --all-targets` — PASS: 869 unit tests passed,
  16 explicitly ignored, and every integration target passed; the constraint
  scale target reported 3 passed and 1 ignored.
- `rustup run 1.85.0 rustc -vV` — PASS: rustc 1.85.0
  (`4d91de4e4`, x86_64-pc-windows-msvc).
- Forced clean Rust 1.85 compile with the explicit toolchain `rustc` and an
  isolated target directory — RED on the two unsupported let-chains and then
  on `usize::is_multiple_of`; GREEN after the compatibility fix with
  `cargo check --all-targets`.
- Remote CI rerun remains required; the first run URL is
  `https://github.com/liu2kaibin/ORA-SQLLite/actions/runs/36136039372`.
