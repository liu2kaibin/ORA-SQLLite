# Phase 4C2-C Foreign Keys Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add persistent immediate foreign keys, cross-table INSERT/UPDATE/DELETE enforcement, reverse dependencies, safe DROP TABLE behavior, and cross-table recovery proof without expanding beyond Phase 4C2-C.

**Architecture:** Extend the existing ConstraintDef V1 payload and reserved ReferencedBy key family into a strictly validated bidirectional foreign-key graph. Binding resolves each parent to a declared PrimaryKey or Unique constraint and retains stable IDs plus the parent SchemaVersion; atomic CREATE/DROP updates both sides and bumps every affected parent version. A focused foreign-key runtime probes the parent system index for child writes and either a matching child-index prefix or a bounded ROWID scan for parent restrict checks, always against the current WriteBatch before any persistent mutation.

**Tech Stack:** Rust 2024, MSRV Rust 1.85, existing Pager/B+Tree/WAL/Catalog/SQL pipeline, no unsafe, no new production dependencies.

**Spec:** `docs/superpowers/specs/2026-09-18-phase-4c2-index-constraints-design.md` sections 7.2, 9.3, 10.4, 11, 12, 14-18; implementation scope is section 17.3 only.

## Global Constraints

- Scope is exactly Phase 4C2-C: ForeignKey, ReferencedBy, child-write checks, parent UPDATE/DELETE restrict checks, optional child-index prefix probes, ROWID fallback scans, DROP TABLE dependencies, and cross-table recovery proof.
- Self-referential and cyclic foreign keys, ALTER TABLE, deferred constraints, ON DELETE/UPDATE actions, CASCADE CONSTRAINTS, and Phase 5 Oracle behavior remain unsupported.
- A parent key must be one declared PrimaryKey or Unique constraint whose ordered columns match exactly; an explicit unique index is never a valid parent key.
- Foreign keys never auto-create a child index. A child index is usable only when its leading ordered columns exactly equal the foreign-key local columns; otherwise use a streaming ROWID scan.
- Child and parent column types must be identical except for lossless Int64-to-Decimal normalization; child values are encoded in the parent key type.
- Any NULL child component makes the foreign-key check pass. Every non-NULL check reads the current WriteBatch, including earlier statements in the same transaction.
- Reverse dependency creation or removal bumps each distinct affected parent table SchemaVersion exactly once in the same statement, so stale prepared parent statements return SchemaChanged.
- Preserve main-file format 2 and the shipped INDEX_CONSTRAINT_V1 feature bit; do not add a startup migration, background validation, or full table/index comparison on ordinary open.
- Keep the public Connection, Statement, Rows, ExecutionResult, and transaction contracts unchanged.
- Keep SELECT, UPDATE target discovery, and DELETE target discovery on ROWID TableScan; internal foreign-key probes do not implement Phase 4C2-D IndexScan.
- Constraint violations use ErrorCode::Constraint and include the persisted child foreign-key name. Corrupt graph, row, or index data remains ErrorCode::Corruption.
- Every DDL/DML persistent mutation remains inside the existing statement checkpoint and one WriteBatch; all key encodings and foreign-key checks finish before the affected row/index pages are changed.
- All counts, lengths, IDs, ordinals, memory estimates, version increments, and prefix bounds use checked arithmetic.
- Rust edition stays 2024 and rust-version stays 1.85; add no production dependency and no unsafe code.

## Review Focus

1. Composite NULL and Int64-to-Decimal normalization: `foreign_key_null_components_and_int64_decimal_normalization_are_exact` in Task 5 proves NULL bypass and parent-type encoding without admitting any other cross-type pair.
2. Read-your-own-writes in one transaction: `foreign_keys_observe_current_write_batch_for_parent_and_child_changes` in Task 8 proves parent insert then child insert, child delete then parent delete, rollback, and retry behavior.
3. Prepared parent statements after reverse-graph changes: `foreign_key_create_invalidates_prepared_parent_dml` in Task 4 and `drop_child_invalidates_prepared_parent_dml` in Task 7 prove parent SchemaVersion bumps on both creation and removal.
4. Indexed and unindexed restrict equivalence: `parent_restrict_uses_valid_child_prefix_index_or_rowid_scan_with_identical_results` in Task 6 proves deterministic prefix selection, fallback, NULL handling, and corruption visibility.
5. Torn reverse mappings and crash boundaries: `foreign_key_graph_rejects_every_one_sided_or_misdirected_reference` in Task 2 and the Phase 4C2-C crash matrix in Task 10 prove that neither ordinary open nor recovery accepts a partial bidirectional graph.

---

## File and Responsibility Map

- Modify `src/storage/catalog/types.rs`: ForeignKeyTarget, ConstraintKind::ForeignKey, ConstraintDef checked shape, and ReferencedBy record surface.
- Modify `src/storage/catalog/record.rs`: V1 parent-field codec and ReferencedBy mapping codec with strict malformed classification.
- Modify `src/storage/catalog/key.rs`: retain the shipped 0x34 key format and add focused golden/malformed tests only.
- Modify `src/storage/catalog/mod.rs`: decoded reverse graph, complete candidate validation, reader queries, atomic bundle insert/removal, and parent SchemaVersion mutation.
- Modify `src/storage/constraint/mod.rs`: include outgoing/incoming foreign-key descriptors beside local constraints and keep named violation construction centralized.
- Create `src/storage/constraint/foreign_key.rs`: value normalization, parent system-index probe, child-index prefix probe, and bounded ROWID fallback restrict scan.
- Modify `src/storage/index/key.rs`: checked logical-prefix encoding for internal foreign-key probes.
- Modify `src/storage/index/mod.rs`: read-only exact/prefix existence probes that decode every visited key and require empty leaf values.
- Modify `src/storage/relation.rs`: foreign-key table specs, atomic constrained CREATE/DROP, reverse version bumps, and DML precheck ordering.
- Modify `src/sql/bind/statement.rs`: exact parent-key resolution, approved type compatibility, self-reference rejection, and parent dependency retention.
- Modify `src/sql/bound/statement.rs`: retain the resolved parent ConstraintId with the existing stable parent table/column identities.
- Modify `src/sql/plan/mutation.rs` and `src/sql/plan/mod.rs`: executable ForeignKey CreateTable plan instead of DeferredUnsupported.
- Modify `src/sql/exec/mutation.rs`: route child and parent checks through ConstrainedTableWrite with bounded work memory and stable locations.
- Modify `src/storage/test_support.rs`, `src/sql/exec/test_support.rs`, and `tests/support/format2.rs`: cross-table capture, raw graph inspection, fixed-seed model, corruption fixtures, and crash matrix.
- Modify `tests/phase4c2_constraint_api.rs`: public SQL, prepared statement, transaction, snapshot, reopen, and unsupported-boundary coverage.
- Modify `tests/phase4c2_constraint_scale.rs`: indexed and unindexed foreign-key scale witnesses.
- Modify `.github/workflows/deep-ci.yml`: one Phase 4C2-C deep job; keep PR CI at the existing three jobs.
- Modify `README.md`: verified Phase 4C2-C capability and retained Phase 4C2-D exclusions.

### Task 1: Stable Foreign-Key Types and Exact V1 Codecs

**Files:**
- Modify: `src/storage/catalog/types.rs`
- Modify: `src/storage/catalog/record.rs`
- Modify: `src/storage/catalog/key.rs`

**Interfaces:**
- Consumes: existing ConstraintId, TableId, ConstraintDef V1 envelope, mapping codecs, and ReferencedBy key `(parent_constraint_id, child_constraint_id)`.
- Produces: `ForeignKeyTarget`, `ConstraintKind::ForeignKey`, parent accessors on ConstraintDef, `CatalogRecord::ReferencedBy(ConstraintId)`, and `encode/decode_referenced_by_mapping`.

- [ ] **Step 1: Write failing exact-layout and strict-decoder tests**

```rust
#[test]
fn foreign_key_constraint_v1_uses_reserved_parent_fields_and_round_trips() {
    let target = ForeignKeyTarget::new(
        TableId::new(0x3132_3334_3536_3738).unwrap(),
        ConstraintId::new(0x4142_4344_4546_4748).unwrap(),
    );
    let definition = ConstraintDef::new(
        ConstraintId::new(0x0102_0304_0506_0708).unwrap(),
        TableId::new(0x1112_1314_1516_1718).unwrap(),
        ConstraintKind::ForeignKey,
        2,
        None,
        Some(target),
        Box::new([]),
        CatalogName::new("FK_CHILD_PARENT".into()).unwrap(),
    ).unwrap();
    let encoded = encode_constraint_definition(&definition);
    assert_eq!(encoded[0], RecordKind::ConstraintDefinition.byte());
    assert_eq!(encoded[1], 1);
    assert_eq!(&encoded[36..44], &target.table_id().get().to_be_bytes());
    assert_eq!(&encoded[44..52], &target.constraint_id().get().to_be_bytes());
    assert_eq!(decode_constraint_definition(definition.id(), &encoded).unwrap(), definition);
}

#[test]
fn referenced_by_mapping_is_exact_and_rejects_wrong_kind_or_trailing_bytes() {
    let child = ConstraintId::new(9).unwrap();
    let bytes = encode_referenced_by_mapping(child);
    assert_eq!(bytes, [0x34, 1, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 9]);
    assert_eq!(decode_referenced_by_mapping(&bytes).unwrap(), child);
    let mut trailing = bytes;
    trailing.push(0);
    assert_eq!(decode_referenced_by_mapping(&trailing).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Run the focused tests and confirm the red state**

Run: `rustup run stable cargo test foreign_key_constraint_v1_uses_reserved_parent_fields_and_round_trips -- --exact`

Run: `rustup run stable cargo test referenced_by_mapping_is_exact_and_rejects_wrong_kind_or_trailing_bytes -- --exact`

Expected: compilation fails because ForeignKeyTarget, ConstraintKind::ForeignKey, the expanded constructor, and ReferencedBy record codec do not exist.

- [ ] **Step 3: Define the checked in-memory shapes**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForeignKeyTarget {
    table_id: TableId,
    constraint_id: ConstraintId,
}

impl ForeignKeyTarget {
    pub(crate) const fn new(table_id: TableId, constraint_id: ConstraintId) -> Self;
    pub(crate) const fn table_id(self) -> TableId;
    pub(crate) const fn constraint_id(self) -> ConstraintId;
}

pub(crate) enum ConstraintKind {
    PrimaryKey,
    Unique,
    NotNull,
    Check,
    ForeignKey,
}

pub(crate) fn ConstraintDef::new(
    id: ConstraintId,
    table_id: TableId,
    kind: ConstraintKind,
    column_count: u16,
    index_id: Option<IndexId>,
    foreign_key_target: Option<ForeignKeyTarget>,
    check_program: Box<[u8]>,
    name: CatalogName,
) -> Result<ConstraintDef>;
```

ForeignKey requires `1..=MAX_COLUMNS` local columns, no system index, one target, and an empty CheckProgram. All four existing kinds require `foreign_key_target == None`; their current invariants remain byte-for-byte unchanged. Add `foreign_key_target()`, `parent_table_id()`, and `parent_constraint_id()` accessors, with the last two returning Option values derived from the single checked target.

Update every existing constructor call in production and unit tests to pass `None` between `index_id` and `check_program`; do not add an overload that can silently manufacture an invalid target pair.

- [ ] **Step 4: Encode and decode the already-reserved V1 fields**

```text
u64 constraint_id
u64 table_id
u8  kind                 # 1 PK, 2 UNIQUE, 3 NOT NULL, 4 CHECK, 5 FOREIGN KEY
u8  flags                # zero
u16 local_column_count
u64 system_index_id      # zero for ForeignKey
u64 parent_table_id      # nonzero only for ForeignKey
u64 parent_constraint_id # nonzero only for ForeignKey
u8  deferred_mode        # zero
u8  on_update_action     # zero
u8  on_delete_action     # zero
u8  reserved             # zero
u32 check_program_len    # zero for ForeignKey
u16 name_len
u16 reserved             # zero
check_program bytes
name UTF-8 bytes
```

Change kind code 5 and nonzero parent IDs from Unsupported to normal V1 decoding only when the full ForeignKey shape is valid. A zero/nonzero parent pair, parent fields on another kind, a system index on ForeignKey, nonempty CheckProgram, action bytes, deferred mode, reserved bytes, or strict-EOF failure returns Corruption. Future record versions remain Unsupported.

- [ ] **Step 5: Add exhaustive truncation and shape-conflict tests**

```rust
#[test]
fn foreign_key_definition_rejects_every_truncation_and_invalid_shape() {
    let bytes = encoded_foreign_key_constraint();
    for end in 0..bytes.len() {
        assert_eq!(
            decode_constraint_definition(ConstraintId::new(7).unwrap(), &bytes[..end])
                .unwrap_err().code(),
            ErrorCode::Corruption
        );
    }
    for mutation in foreign_key_shape_conflicts(bytes) {
        assert_eq!(
            decode_constraint_definition(ConstraintId::new(7).unwrap(), &mutation)
                .unwrap_err().code(),
            ErrorCode::Corruption
        );
    }
}
```

The mutation table covers: wrong key ID, zero parent table only, zero parent constraint only, both parents zero, nonzero system index, nonempty program, all three nonzero mode/action bytes, both reserved locations, zero column count, and count above MAX_COLUMNS.

- [ ] **Step 6: Run Task 1 tests and the local catalog codec suite**

Run: `rustup run stable cargo test storage::catalog::record::tests -- --nocapture`

Run: `rustup run stable cargo test storage::catalog::key::tests -- --nocapture`

Expected: all tests pass; existing local-constraint golden bytes remain unchanged.

- [ ] **Step 7: Commit the stable format surface**

```bash
git add src/storage/catalog/types.rs src/storage/catalog/record.rs src/storage/catalog/key.rs
git commit -m "feat: decode foreign key catalog records"
```

### Task 2: Bidirectional Catalog Graph and Candidate Validation

**Files:**
- Modify: `src/storage/catalog/mod.rs`

**Interfaces:**
- Consumes: Task 1 ConstraintDef target accessors and ReferencedBy mapping codec.
- Produces: validated `referenced_by: BTreeMap<ConstraintId, BTreeSet<ConstraintId>>`, `foreign_keys_referencing`, `find_parent_key_constraint`, and catalog writer reverse-record operations.

- [ ] **Step 1: Write failing complete-graph and corruption tests**

```rust
#[test]
fn foreign_key_graph_rejects_every_one_sided_or_misdirected_reference() {
    for candidate in [
        candidate_without_referenced_by(),
        candidate_with_orphan_referenced_by(),
        candidate_with_wrong_parent_key(),
        candidate_with_wrong_child_value(),
        candidate_with_explicit_unique_parent_index(),
        candidate_with_parent_column_order_mismatch(),
    ] {
        assert_eq!(open_candidate(candidate).unwrap_err().code(), ErrorCode::Corruption);
    }
}

#[test]
fn valid_foreign_key_graph_exposes_both_directions_in_constraint_id_order() {
    let catalog = open_valid_parent_and_two_children();
    assert_eq!(
        catalog.foreign_keys_referencing(parent_constraint_id())
            .map(ConstraintDef::id).collect::<Vec<_>>(),
        vec![child_constraint_id(1), child_constraint_id(2)]
    );
}
```

- [ ] **Step 2: Run the graph tests and confirm the red state**

Run: `rustup run stable cargo test foreign_key_graph_rejects_every_one_sided_or_misdirected_reference -- --exact`

Run: `rustup run stable cargo test valid_foreign_key_graph_exposes_both_directions_in_constraint_id_order -- --exact`

Expected: tests fail because CatalogImage still rejects ReferencedBy and has no reverse map.

- [ ] **Step 3: Decode ReferencedBy into the catalog image**

```rust
referenced_by: BTreeMap<ConstraintId, BTreeSet<ConstraintId>>,

DecodedCatalogKey::ReferencedBy { parent, child } => {
    require_index_feature(feature_bits)?;
    if record::decode_referenced_by_mapping(&value)? != child
        || !image.referenced_by.entry(parent).or_default().insert(child)
    {
        return Err(corruption("catalog contains an invalid referenced-by mapping"));
    }
}
```

- [ ] **Step 4: Validate every forward and reverse edge plus parent eligibility**

```rust
fn validate_foreign_key(
    &self,
    child: &ConstraintDef,
    target: ForeignKeyTarget,
    local_columns: &BTreeMap<u16, ColumnId>,
) -> Result<()>;
```

The validator requires: child and parent tables differ; parent constraint exists and belongs to target.table_id; parent kind is PrimaryKey or Unique; parent has a constraint-owned unique system index; local and parent counts match; both ordered column maps are contiguous; each pair has the same ColumnType except Int64/Decimal in the child-to-parent direction; and exactly one reverse edge `(target.constraint_id, child.id)` exists. Every reverse edge must point back to a ForeignKey whose target is that exact parent. An explicit unique index is not consulted.

- [ ] **Step 5: Add ordered reader queries and exact-match parent lookup**

```rust
pub(crate) fn find_parent_key_constraint(
    &self,
    table_id: TableId,
    ordered_columns: &[ColumnId],
) -> Option<&ConstraintDef>;

pub(crate) fn foreign_keys_referencing(
    &self,
    parent_constraint: ConstraintId,
) -> impl Iterator<Item = &ConstraintDef>;

pub(crate) fn referenced_by_exists(&self, parent_constraint: ConstraintId) -> bool;
```

`find_parent_key_constraint` examines only declared PrimaryKey/Unique definitions and requires exact ordered equality, not a prefix. Both iterators inherit BTreeMap/BTreeSet ConstraintId order.

- [ ] **Step 6: Add atomic writer helpers for the reverse record**

```rust
fn insert_referenced_by(
    tree: &mut BTree,
    batch: &mut WriteBatch,
    parent: ConstraintId,
    child: ConstraintId,
) -> Result<()>;

fn remove_referenced_by(
    tree: &mut BTree,
    batch: &mut WriteBatch,
    parent: ConstraintId,
    child: ConstraintId,
) -> Result<()>;
```

Insert requires the exact key to be absent and writes the child mapping value. Removal reads and decodes the exact value before deletion. `insert_constraint_bundle` invokes insert for ForeignKey after validating the parent candidate; `remove_constraint` removes it before deleting the child definition. Existing test hooks count ReferencedBy as its own record-family boundary.

- [ ] **Step 7: Run catalog unit and deep-validation tests**

Run: `rustup run stable cargo test storage::catalog::tests -- --nocapture`

Expected: all catalog tests pass, including missing/orphan/duplicate/misdirected reverse edges, wrong parent type/table/columns, and valid multiple-child ordering.

- [ ] **Step 8: Commit the validated graph**

```bash
git add src/storage/catalog/mod.rs
git commit -m "feat: validate foreign key catalog graph"
```

### Task 3: Bind and Plan Only Declared Parent Keys

**Files:**
- Modify: `src/sql/bind/statement.rs`
- Modify: `src/sql/bound/statement.rs`
- Modify: `src/sql/plan/mutation.rs`
- Modify: `src/sql/plan/mod.rs`

**Interfaces:**
- Consumes: Task 2 `find_parent_key_constraint`, stable parent constraint identity, existing parent SchemaDependency, and BoundTable/BoundColumn identity validation.
- Produces: executable `BoundConstraint::ForeignKey { parent_constraint_id }`, `TableConstraintDefinition::ForeignKey`, and `TableConstraintSpec::ForeignKey` conversion.

- [ ] **Step 1: Write failing binder and planner tests**

```rust
#[test]
fn foreign_key_binding_requires_exact_declared_parent_key_and_supported_types() {
    assert_bind_ok("CREATE TABLE C (P INT64, CONSTRAINT FK FOREIGN KEY (P) REFERENCES PARENT (ID))");
    assert_bind_ok("CREATE TABLE C (P INT64, CONSTRAINT FK FOREIGN KEY (P) REFERENCES DEC_PARENT (ID))");
    assert_bind_error("CREATE TABLE C (A INT64, FOREIGN KEY (A) REFERENCES EXPLICIT_UQ (A))");
    assert_bind_error("CREATE TABLE C (A INT64, FOREIGN KEY (A) REFERENCES COMPOSITE (A))");
    assert_bind_error("CREATE TABLE C (A TEXT, FOREIGN KEY (A) REFERENCES PARENT (ID))");
}

#[test]
fn planner_emits_stable_foreign_key_target_instead_of_deferred_unsupported() {
    let plan = plan_create_child_with_foreign_key();
    let TableConstraintDefinition::ForeignKey { parent_constraint_id, .. } = &plan.constraints()[0] else {
        panic!("expected executable foreign key plan");
    };
    assert_eq!(*parent_constraint_id, expected_parent_constraint_id());
}
```

- [ ] **Step 2: Run the tests and confirm the red state**

Run: `rustup run stable cargo test foreign_key_binding_requires_exact_declared_parent_key_and_supported_types -- --exact`

Run: `rustup run stable cargo test planner_emits_stable_foreign_key_target_instead_of_deferred_unsupported -- --exact`

Expected: binder accepts overly broad `SqlType::unify` pairs or lacks parent-key identity, and planner still emits DeferredUnsupported.

- [ ] **Step 3: Resolve the exact parent constraint during binding**

```rust
ForeignKey {
    name: Option<CatalogName>,
    columns: Vec<u16>,
    referenced_table: BoundTable,
    referenced_columns: Vec<BoundColumn>,
    parent_constraint_id: ConstraintId,
    span: SqlSpan,
}

fn foreign_key_types_compatible(child: ColumnType, parent: ColumnType) -> bool {
    child == parent || matches!((child, parent), (ColumnType::Int64, ColumnType::Decimal))
}
```

After resolving ordered parent columns, call `find_parent_key_constraint(parent_table_id, &column_ids)`. If absent, return Bind with `FOREIGN KEY parent columns must exactly match a declared PRIMARY KEY or UNIQUE constraint`. Keep the existing parent table SchemaDependency. Self-reference returns Unsupported before lookup; with CREATE-only foreign keys, no other cycle can be formed because the child table does not yet exist as a parent.

- [ ] **Step 4: Retain stable parent identity in the immutable plan**

```rust
pub(crate) enum TableConstraintDefinition {
    // existing variants
    ForeignKey {
        name: Option<CatalogName>,
        columns: Box<[u16]>,
        parent_table: BoundTable,
        parent_constraint_id: ConstraintId,
        parent_columns: Box<[BoundColumn]>,
        location: SqlLocation,
    },
}
```

`storage_spec()` emits `TableConstraintSpec::ForeignKey` with local ordinals and stable parent IDs. `explicit_name()` and `location()` include the new variant. Remove the CreateTable DeferredUnsupported branch; other unsupported FK syntax remains rejected by parser/binder before planning.

- [ ] **Step 5: Validate retained parent column and constraint identities at execution**

```rust
fn validate_foreign_key_plan(
    definition: &TableConstraintDefinition,
    catalog: &CatalogReader,
) -> Result<()>;
```

The validation requires the parent table/columns still match the retained IDs, ordinals, types, and table ownership; the exact parent ConstraintId still exists, is PK/Unique on those columns, and belongs to the retained table. SchemaDependency validation runs first, so a normal reverse-graph DDL change yields SchemaChanged rather than rebinding by name.

- [ ] **Step 6: Run the SQL binder/planner corpus**

Run: `rustup run stable cargo test sql::bind -- --nocapture`

Run: `rustup run stable cargo test sql::plan -- --nocapture`

Expected: all existing SQL tests pass; ordinary foreign keys now plan, while self-reference, actions, deferred options, type mismatch, prefix keys, reordered keys, and explicit-index-only parents remain rejected with the specified class.

- [ ] **Step 7: Commit binding and planning**

```bash
git add src/sql/bind/statement.rs src/sql/bound/statement.rs src/sql/plan/mutation.rs src/sql/plan/mod.rs
git commit -m "feat: bind declared foreign key parents"
```

### Task 4: Atomic Foreign-Key CREATE and Parent Version Invalidation

**Files:**
- Modify: `src/storage/relation.rs`
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/sql/exec/mutation.rs`

**Interfaces:**
- Consumes: Task 3 `TableConstraintSpec::ForeignKey`, Task 2 reverse graph writer, CatalogWriter::bump_table_schema_version, and existing statement checkpoint.
- Produces: ForeignKey ConstraintBundle creation, ReferencedBy insertion, and one parent version bump per distinct parent table.

- [ ] **Step 1: Write failing atomic-create and prepared-parent tests**

```rust
#[test]
fn constrained_create_publishes_foreign_key_and_reverse_edge_atomically() {
    let before = capture_allocator_pages_and_catalog();
    inject_each_foreign_key_create_boundary(|error| {
        assert_eq!(error.code(), ErrorCode::Io);
        assert_eq!(capture_allocator_pages_and_catalog(), before);
    });
    create_child_with_foreign_key().unwrap();
    assert_complete_forward_and_reverse_graph();
}

#[test]
fn foreign_key_create_invalidates_prepared_parent_dml() {
    let update = parent_connection.prepare("UPDATE P SET ID = ID + 1 WHERE ID = 1").unwrap();
    let delete = parent_connection.prepare("DELETE FROM P WHERE ID = 1").unwrap();
    create_child_with_foreign_key().unwrap();
    assert_schema_changed(parent_connection.execute(&update, Parameters::empty()));
    assert_schema_changed(parent_connection.execute(&delete, Parameters::empty()));
}
```

- [ ] **Step 2: Run the tests and confirm the red state**

Run: `rustup run stable cargo test constrained_create_publishes_foreign_key_and_reverse_edge_atomically -- --exact`

Run: `rustup run stable cargo test foreign_key_create_invalidates_prepared_parent_dml -- --exact`

Expected: CREATE cannot build the ForeignKey bundle and parent versions do not change.

- [ ] **Step 3: Extend storage specs and prevalidation**

```rust
TableConstraintSpec::ForeignKey {
    name: Option<CatalogName>,
    columns: Box<[u16]>,
    parent_table_id: TableId,
    parent_constraint_id: ConstraintId,
}
```

`ConstrainedTableSpec::new` validates nonempty, nonrepeating local ordinals. `prevalidate_constrained_create` reopens the current catalog and requires the exact parent table/constraint/ordered columns/type relation selected by the binder. It also checks every affected parent version can increment before allocating an ID or page.

- [ ] **Step 4: Build ForeignKey bundles without a system index**

```rust
ConstraintBundle {
    definition: ConstraintDef::new(
        allocated.id,
        child_table_id,
        ConstraintKind::ForeignKey,
        local_columns.len() as u16,
        None,
        Some(ForeignKeyTarget::new(parent_table_id, parent_constraint_id)),
        Box::new([]),
        allocated.name,
    )?,
    columns: local_columns.into_boxed_slice(),
    system_index: None,
}
```

`required_object_count` counts one ConstraintId and zero system IndexIds for each ForeignKey. The child table root and existing local system indexes retain current allocation order.

- [ ] **Step 5: Publish the whole graph and bump distinct parents once**

```rust
let parent_tables = bundles.iter()
    .filter_map(|bundle| bundle.definition.parent_table_id())
    .collect::<BTreeSet<_>>();
for bundle in &bundles {
    catalog.insert_constraint_bundle(batch, bundle)?;
}
for parent_table in parent_tables {
    catalog.bump_table_schema_version(batch, parent_table)?;
}
catalog.finish(batch)?;
```

The bump is after all record insertions and before finish, inside the existing statement checkpoint. A table with two foreign keys to the same parent bumps that parent once. Different parents each bump once.

- [ ] **Step 6: Map execution errors to the foreign-key SQL location**

```rust
fn validate_foreign_key_plans(
    plan: &CreateTablePlan,
    catalog: &CatalogReader,
) -> Result<()> {
    for constraint in plan.constraints() {
        if matches!(constraint, TableConstraintDefinition::ForeignKey { .. }) {
            validate_foreign_key_plan(constraint, catalog)
                .map_err(|error| error.with_sql_location(constraint.location()))?;
        }
    }
    Ok(())
}
```

Call this after the normal statement SchemaDependency check and immediately before constructing `ConstrainedTableSpec`. It attaches parent identity/type/key errors to the exact ForeignKey location; the existing CheckProgram error mapping remains unchanged.

- [ ] **Step 7: Run relation/catalog/create execution tests**

Run: `rustup run stable cargo test constrained_create -- --nocapture`

Run: `rustup run stable cargo test foreign_key_create_invalidates_prepared_parent_dml -- --exact`

Expected: atomic create, exact ID/page restoration, one bump per parent, stale parent plan rejection, and existing local-constraint CREATE behavior all pass.

- [ ] **Step 8: Commit atomic CREATE support**

```bash
git add src/storage/relation.rs src/storage/catalog/mod.rs src/sql/exec/mutation.rs
git commit -m "feat: create foreign key graphs atomically"
```

### Task 5: Immediate Child INSERT and UPDATE Checks

**Files:**
- Create: `src/storage/constraint/foreign_key.rs`
- Modify: `src/storage/constraint/mod.rs`
- Modify: `src/storage/index/key.rs`
- Modify: `src/storage/index/mod.rs`
- Modify: `src/storage/relation.rs`
- Modify: `src/sql/exec/mutation.rs`

**Interfaces:**
- Consumes: validated outgoing definitions, parent constraint-owned system IndexId, IndexKeyCodec, current WriteBatch, and scalar::int_decimal.
- Produces: `ForeignKeyCatalog::validate_child_row`, read-only exact parent-key probe, and named immediate violations before mutation.

- [ ] **Step 1: Write failing child-check tests**

```rust
#[test]
fn foreign_key_null_components_and_int64_decimal_normalization_are_exact() {
    insert_child([Value::Null, Value::Text("missing".into())]).unwrap();
    insert_child([Value::Int64(7), Value::Null]).unwrap();
    insert_parent_decimal(7, "A").unwrap();
    insert_child([Value::Int64(7), Value::Text("A".into())]).unwrap();
    assert_named_constraint(insert_child([Value::Int64(8), Value::Text("A".into())]), "FK_CHILD_PARENT");
    assert_type_error(create_float_to_decimal_foreign_key());
}

#[test]
fn child_insert_and_update_probe_parent_system_index_before_mutation() {
    assert_named_constraint(insert_missing_child(), "FK_CHILD_PARENT");
    assert_child_table_and_indexes_unchanged();
    insert_parent_key(1).unwrap();
    insert_child_key(1).unwrap();
    assert_named_constraint(update_child_key(1, 2), "FK_CHILD_PARENT");
    assert_child_key(1);
}
```

- [ ] **Step 2: Run the tests and confirm the red state**

Run: `rustup run stable cargo test foreign_key_null_components_and_int64_decimal_normalization_are_exact -- --exact`

Run: `rustup run stable cargo test child_insert_and_update_probe_parent_system_index_before_mutation -- --exact`

Expected: child writes currently perform only local constraints and uniqueness.

- [ ] **Step 3: Build outgoing runtime descriptors from the validated catalog**

```rust
pub(crate) struct ForeignKeySpec {
    name: CatalogName,
    child_table_id: TableId,
    child_columns: Box<[ColumnDef]>,
    parent_table_id: TableId,
    parent_constraint_id: ConstraintId,
    parent_columns: Box<[ColumnDef]>,
    parent_index_id: IndexId,
    child_probe_index_id: Option<IndexId>,
}

pub(crate) struct ForeignKeyCatalog {
    outgoing: Box<[ForeignKeySpec]>,
    incoming: Box<[ForeignKeySpec]>,
}

// src/storage/constraint/mod.rs
mod foreign_key;
pub(crate) use foreign_key::ForeignKeyCatalog;

impl ForeignKeyCatalog {
    pub(crate) fn open(catalog: &CatalogReader, table_id: TableId) -> Result<Self>;
    pub(crate) fn validate_child_row(
        &self,
        batch: &mut WriteBatch,
        child_values: &[Value],
    ) -> Result<()>;
}
```

Descriptors are sorted by child ConstraintId. Opening rechecks the parent ownership/index relationship defensively even though CatalogReader already validated it.

- [ ] **Step 4: Normalize child components to parent key types**

```rust
fn parent_key_values(spec: &ForeignKeySpec, child_row: &[Value]) -> Result<Option<Box<[Value]>>> {
    let mut values = Vec::with_capacity(spec.child_columns.len());
    for (child, parent) in spec.child_columns.iter().zip(&spec.parent_columns) {
        let value = child_row.get(usize::from(child.ordinal()))
            .ok_or_else(|| DbError::new(ErrorCode::Type, "foreign-key row is missing a local column"))?;
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        values.push(match (child.column_type(), parent.column_type(), value) {
            (ColumnType::Int64, ColumnType::Decimal, Value::Int64(value)) =>
                Value::Decimal(crate::scalar::int_decimal(*value)?),
            (left, right, value) if left == right => value.clone(),
            _ => return Err(DbError::new(ErrorCode::Corruption, "validated foreign-key column types diverged")),
        });
    }
    Ok(Some(values.into_boxed_slice()))
}
```

- [ ] **Step 5: Add an exact logical-key existence probe**

```rust
pub(crate) fn contains_key(
    &self,
    source: &mut impl PageSource,
    key_values: &[Value],
) -> Result<bool>;
```

Encode with the index's full ordered parent columns and a checked nonzero sentinel ROWID only to obtain the logical prefix. Scan IndexKeyCodec::prefix_bounds; for every result require an empty leaf value and decode the complete physical key. Return true only after one valid matching entry. This is an internal constraint probe, not a query IndexScan.

- [ ] **Step 6: Enforce outgoing keys before any row/index mutation**

```rust
pub(crate) fn insert(
    &mut self,
    batch: &mut WriteBatch,
    values: &[Value],
    max_row_bytes: usize,
    memory: &mut impl CheckMemory,
) -> Result<RowId> {
    self.validate_row(values, memory)?;
    self.foreign_keys.validate_child_row(batch, values)?;
    // row encoding, all index-key encoding, uniqueness, then mutation scope
}

pub(crate) fn replace(
    &mut self,
    batch: &mut WriteBatch,
    rowid: RowId,
    old: &[Value],
    new: &[Value],
    max_row_bytes: usize,
    memory: &mut impl CheckMemory,
    foreign_key_work_bytes: u64,
) -> Result<()> {
    self.validate_row(new, memory)?;
    self.foreign_keys.validate_child_row(batch, new)?;
    // parent restrict checks are added in Task 6, then mutation scope
}
```

On absence return `Constraint` using the persisted child FK name. The checks use roots resolved from the current batch catalog and read pages through the current WriteBatch.

Add `foreign_keys: ForeignKeyCatalog` to `ConstrainedTableWrite`; `open` builds it from the same validated CatalogReader as the local ConstraintCatalog and owned indexes. After statement restoration, callers continue to discard and reopen the facade exactly as they do today.

- [ ] **Step 7: Run child DML and regression tests**

Run: `rustup run stable cargo test foreign_key -- --nocapture`

Run: `rustup run stable cargo test storage::index -- --nocapture`

Run: `rustup run stable cargo test sql::exec::mutation -- --nocapture`

Expected: child INSERT/UPDATE, NULL, composite order, Decimal normalization, rollback, root split, and all local constraint/index regressions pass.

- [ ] **Step 8: Commit child enforcement**

```bash
git add src/storage/constraint/foreign_key.rs src/storage/constraint/mod.rs src/storage/index/key.rs src/storage/index/mod.rs src/storage/relation.rs src/sql/exec/mutation.rs
git commit -m "feat: enforce child foreign keys"
```

### Task 6: Parent UPDATE and DELETE Restrict with Indexed/Fallback Probes

**Files:**
- Modify: `src/storage/constraint/foreign_key.rs`
- Modify: `src/storage/index/key.rs`
- Modify: `src/storage/index/mod.rs`
- Modify: `src/storage/relation.rs`
- Modify: `src/sql/exec/mutation.rs`

**Interfaces:**
- Consumes: Task 5 incoming descriptors, WorkMemory remaining bytes, Relation streaming scan, and child index metadata.
- Produces: `validate_parent_update`, `validate_parent_delete`, checked index-prefix probe, and deterministic ROWID fallback.

- [ ] **Step 1: Write failing indexed/fallback and current-batch tests**

```rust
#[test]
fn parent_restrict_uses_valid_child_prefix_index_or_rowid_scan_with_identical_results() {
    for child_index in [NoIndex, ExactIndex, PrefixIndexWithTrailingColumn, WrongOrderIndex] {
        let db = foreign_key_fixture(child_index);
        db.insert_referencing_child(7).unwrap();
        assert_named_constraint(db.update_parent_key(7, 8), "FK_CHILD_PARENT");
        assert_named_constraint(db.delete_parent_key(7), "FK_CHILD_PARENT");
        db.delete_child_key(7).unwrap();
        db.delete_parent_key(7).unwrap();
    }
}

#[test]
fn parent_restrict_reads_current_batch_and_skips_unchanged_keys() {
    let mut tx = begin_foreign_key_transaction();
    tx.delete_child_key(7).unwrap();
    tx.delete_parent_key(7).unwrap();
    tx.rollback().unwrap();
    assert_parent_and_child_key(7);
    update_non_key_parent_column(7).unwrap();
}
```

- [ ] **Step 2: Run the tests and confirm the red state**

Run: `rustup run stable cargo test parent_restrict_uses_valid_child_prefix_index_or_rowid_scan_with_identical_results -- --exact`

Run: `rustup run stable cargo test parent_restrict_reads_current_batch_and_skips_unchanged_keys -- --exact`

Expected: parent UPDATE/DELETE currently ignores incoming references.

- [ ] **Step 3: Select a deterministic usable child index**

```rust
fn child_probe_index(catalog: &CatalogReader, foreign_key: &ConstraintDef) -> Option<IndexId> {
    let local = catalog.columns_for_constraint(foreign_key.id()).map(ColumnDef::id).collect::<Vec<_>>();
    catalog.indexes_for_table(foreign_key.table_id())
        .filter(|index| catalog.columns_for_index(index.id())
            .map(ColumnDef::id).take(local.len()).eq(local.iter().copied()))
        .min_by_key(|index| index.id())
        .map(IndexDef::id)
}
```

An index with fewer columns is unusable. Extra trailing columns are allowed. Explicit and constraint-owned child indexes are both eligible. Lowest IndexId wins; the choice is internal and does not change a query plan.

- [ ] **Step 4: Add checked logical-prefix encoding and probing**

```rust
pub(crate) fn encode_prefix(
    columns: &[ColumnDef],
    values: &[Value],
) -> Result<Vec<u8>>;

pub(crate) fn contains_prefix(
    &self,
    source: &mut impl PageSource,
    prefix_columns: usize,
    values: &[Value],
) -> Result<bool>;
```

Require `1 <= prefix_columns <= index.column_count`, one value per prefix column, the 1024-byte physical-key bound including an eventual ROWID, checked prefix bounds, empty leaf values, and full key decode for every visited record. A malformed matching entry returns Corruption rather than being treated as absent.

- [ ] **Step 5: Add bounded ROWID fallback scanning**

```rust
fn child_table_contains_key(
    spec: &ForeignKeySpec,
    batch: &mut WriteBatch,
    parent_key: &[Value],
    work_bytes: u64,
) -> Result<bool> {
    let Some(child_key) = child_probe_values(spec, parent_key)? else {
        return Ok(false);
    };
    if let Some(index_id) = spec.child_probe_index_id {
        let catalog = batch.catalog_reader()?;
        let storage = IndexStorage::open(&catalog, index_id)?;
        return storage.contains_prefix(batch, spec.child_columns.len(), &child_key);
    }
    let relation = Relation::open_from_batch(batch, spec.child_table_id)?;
    let mut scan = relation.scan_with_work_budget(work_bytes)?;
    while let Some(row) = scan.next_batch(batch)? {
        if child_row_references_parent(spec, row.values(), parent_key)? {
            return Ok(true);
        }
    }
    Ok(false)
}
```

Fallback holds one decoded child row and normalized key at a time, never a row/key collection. Zero or insufficient work bytes returns ResourceLimit before mutation.

For identical types, `child_probe_values` clones the parent components. For a child Int64/parent Decimal pair, it converts the Decimal to Int64 only when scale removal or multiplication is exact and the signed result fits i64; a fractional or out-of-range parent value returns `Ok(None)` because no valid child Int64 can reference it. The helper uses checked powers of ten, multiplication, division remainder, and the explicit i64::MIN magnitude boundary. The fallback path instead normalizes each child Int64 to Decimal with `scalar::int_decimal` and compares in parent-key representation.

- [ ] **Step 6: Enforce incoming references before parent mutation**

```rust
pub(crate) fn validate_parent_update(
    &self,
    batch: &mut WriteBatch,
    old_parent_row: &[Value],
    new_parent_row: &[Value],
    work_bytes: u64,
) -> Result<()>;

pub(crate) fn validate_parent_delete(
    &self,
    batch: &mut WriteBatch,
    old_parent_row: &[Value],
    work_bytes: u64,
) -> Result<()>;
```

For UPDATE, extract each incoming parent's ordered old/new key, normalize Decimal zero/scales through existing index encoding, and skip the probe when logical key bytes are unchanged. Otherwise reject if any current child row references the old key. DELETE always checks the old key. Probe constraints in child ConstraintId order so the first stable name is deterministic.

- [ ] **Step 7: Thread the remaining work budget through SQL DML**

```rust
let foreign_key_work_bytes = memory.remaining();
writer.replace(batch, rowid, old, new, max_row_bytes, memory, foreign_key_work_bytes)?;

let foreign_key_work_bytes = memory.remaining();
writer.delete(batch, rowid, old, foreign_key_work_bytes)?;
```

Checks happen after row/local/key prevalidation and before deleting an old index entry or table row. Statement checkpoint rollback remains the outer guarantee for later-row failures.

- [ ] **Step 8: Run parent restrict and memory/error tests**

Run: `rustup run stable cargo test parent_restrict -- --nocapture`

Run: `rustup run stable cargo test foreign_key_work_memory -- --nocapture`

Expected: exact index, longer prefix index, wrong-order fallback, no-index fallback, NULL child rows, same-batch changes, unchanged parent keys, ResourceLimit, and corrupted matching index/row cases pass identically.

- [ ] **Step 9: Commit parent enforcement**

```bash
git add src/storage/constraint/foreign_key.rs src/storage/index/key.rs src/storage/index/mod.rs src/storage/relation.rs src/sql/exec/mutation.rs
git commit -m "feat: restrict referenced parent keys"
```

### Task 7: DROP TABLE Dependency Rejection and Outgoing Cleanup

**Files:**
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/relation.rs`

**Interfaces:**
- Consumes: Task 2 reverse queries/removal, Task 4 parent version rule, existing deep validation and statement checkpoint.
- Produces: incoming dependency rejection before release and atomic outgoing edge cleanup with distinct parent bumps.

- [ ] **Step 1: Write failing DROP lifecycle tests**

```rust
#[test]
fn drop_parent_is_rejected_before_any_page_or_catalog_change() {
    let before = capture_pages_catalog_and_allocator();
    let error = drop_parent().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert!(error.to_string().contains("FK_CHILD_PARENT"));
    assert_eq!(capture_pages_catalog_and_allocator(), before);
}

#[test]
fn drop_child_removes_outgoing_edges_bumps_parents_and_rolls_back_every_fault() {
    for point in foreign_key_drop_fault_points() {
        let before = capture_pages_catalog_and_versions();
        inject_drop_fault(point);
        assert_eq!(drop_child().unwrap_err().code(), ErrorCode::Io);
        assert_eq!(capture_pages_catalog_and_versions(), before);
    }
    drop_child().unwrap();
    assert_no_child_or_reverse_edges();
    drop_parent().unwrap();
}

#[test]
fn drop_child_invalidates_prepared_parent_dml() {
    let update = parent_connection.prepare("UPDATE P SET ID = ID + 1 WHERE ID = 1").unwrap();
    drop_child().unwrap();
    assert_schema_changed(parent_connection.execute(&update, Parameters::empty()));
}
```

- [ ] **Step 2: Run the tests and confirm the red state**

Run: `rustup run stable cargo test drop_parent_is_rejected_before_any_page_or_catalog_change -- --exact`

Run: `rustup run stable cargo test drop_child_removes_outgoing_edges_bumps_parents_and_rolls_back_every_fault -- --exact`

Run: `rustup run stable cargo test drop_child_invalidates_prepared_parent_dml -- --exact`

Expected: DROP either removes the parent despite references or cannot remove outgoing ForeignKey records.

- [ ] **Step 3: Reject incoming dependencies before destructive validation or release**

```rust
fn first_incoming_foreign_key(catalog: &CatalogReader, table_id: TableId) -> Option<&ConstraintDef>;
```

Inspect every PK/Unique on the target table and its reverse edges. If present, return Constraint naming the lowest child ConstraintId. This check occurs before CatalogWriter bootstrap, tree release staging, or catalog deletion.

- [ ] **Step 4: Remove outgoing foreign keys and bump parents once**

```rust
let parent_tables = constraints.iter()
    .filter_map(|(constraint, _)| constraint.parent_table_id())
    .collect::<BTreeSet<_>>();
for (constraint, columns) in constraints {
    catalog.remove_constraint(batch, &constraint, &columns)?;
}
for parent in parent_tables {
    catalog.bump_table_schema_version(batch, parent)?;
}
```

`remove_constraint` deletes the exact ReferencedBy edge as part of the same mutation scope as child ConstraintColumn/TableConstraint/ConstraintName/ConstraintDefinition records. It never releases a system index for ForeignKey.

- [ ] **Step 5: Extend fault points across reverse deletion and version bump**

```rust
pub(crate) enum ConstrainedDropFaultPoint {
    ExplicitIndexRelease,
    SystemIndexRelease,
    ReferencedByRemoval,
    ConstraintRemoval,
    ParentVersionBump,
    TableRelease,
    CatalogFinished,
}
```

Each injected error restores exact table/index roots, catalog root, freelist, next object ID, parent versions, feature bits, and staged releases through the existing statement checkpoint.

- [ ] **Step 6: Run DROP and deep-validation regressions**

Run: `rustup run stable cargo test constrained_drop -- --nocapture`

Run: `rustup run stable cargo test drop_parent -- --nocapture`

Run: `rustup run stable cargo test drop_child -- --nocapture`

Expected: parent rejection, child cleanup, multiple-parent one-bump behavior, fault restoration, and existing explicit/system-index cleanup all pass.

- [ ] **Step 7: Commit DROP behavior**

```bash
git add src/storage/catalog/mod.rs src/storage/relation.rs
git commit -m "feat: enforce foreign key table dependencies"
```

### Task 8: Public SQL, Prepared Statements, Transactions, and Boundaries

**Files:**
- Modify: `tests/phase4c2_constraint_api.rs`
- Modify: `src/sql/bind/statement.rs`
- Modify: `src/sql/plan/mod.rs`
- Modify: `src/sql/exec/mutation.rs`

**Interfaces:**
- Consumes: Tasks 3-7 end-to-end SQL path and existing public test helpers.
- Produces: public acceptance proof for immediate semantics, stable errors, transaction recovery, prepared dependencies, and retained Unsupported scope.

- [ ] **Step 1: Add the public current-WriteBatch transaction test**

```rust
#[test]
fn foreign_keys_observe_current_write_batch_for_parent_and_child_changes() {
    let (files, database) = common::new_database("phase4c2c-current-batch");
    let mut connection = database.connect().unwrap();
    create_parent_and_child(&mut connection);
    execute_sql(&mut connection, "BEGIN").unwrap();
    execute_sql(&mut connection, "INSERT INTO P VALUES (1)").unwrap();
    execute_sql(&mut connection, "INSERT INTO C VALUES (10, 1)").unwrap();
    assert_constraint(&mut connection, "DELETE FROM P WHERE ID = 1", "FK_C_P");
    execute_sql(&mut connection, "DELETE FROM C WHERE ID = 10").unwrap();
    execute_sql(&mut connection, "DELETE FROM P WHERE ID = 1").unwrap();
    execute_sql(&mut connection, "ROLLBACK").unwrap();
    assert_eq!(collect(&mut connection, "SELECT ID FROM P"), rows([[1]]));
    assert_eq!(collect(&mut connection, "SELECT ID, P_ID FROM C"), rows([[10, 1]]));
    drop(files);
}
```

- [ ] **Step 2: Add prepared identity/version tests**

```rust
#[test]
fn prepared_foreign_key_create_and_parent_dml_recheck_stable_identities() {
    // prepare child CREATE, replace/reversion parent graph in another connection,
    // require SchemaChanged rather than name rebinding;
    // prepare parent UPDATE/DELETE, add or remove child FK, require SchemaChanged;
    // ordinary child/parent DML root splits do not invalidate either statement.
}
```

- [ ] **Step 3: Add stable error and statement-rollback tests**

```rust
#[test]
fn named_foreign_key_errors_restore_only_the_failing_statement() {
    let mut connection = foreign_key_connection();
    execute_sql(&mut connection, "BEGIN").unwrap();
    execute_sql(&mut connection, "INSERT INTO P VALUES (1)").unwrap();
    assert_constraint(&mut connection, "INSERT INTO C VALUES (10, 99)", "FK_C_P");
    execute_sql(&mut connection, "INSERT INTO C VALUES (10, 1)").unwrap();
    assert_constraint(&mut connection, "UPDATE P SET ID = 2 WHERE ID = 1", "FK_C_P");
    execute_sql(&mut connection, "COMMIT").unwrap();
    assert_parent_child_consistent(&mut connection, 1, 10);
}
```

- [ ] **Step 4: Pin every retained unsupported form before mutation**

```rust
#[test]
fn unsupported_foreign_key_forms_leave_catalog_and_allocator_unchanged() {
    for sql in [
        "CREATE TABLE S (ID INT64 PRIMARY KEY, P INT64 REFERENCES S(ID))",
        "CREATE TABLE C (P INT64 REFERENCES P(ID) ON DELETE CASCADE)",
        "CREATE TABLE C (P INT64 REFERENCES P(ID) ON UPDATE SET NULL)",
        "CREATE TABLE C (P INT64 REFERENCES P(ID) DEFERRABLE)",
        "DROP TABLE P CASCADE CONSTRAINTS",
    ] {
        assert_unsupported_without_file_change(sql);
    }
}
```

- [ ] **Step 5: Run the public integration target**

Run: `rustup run stable cargo test --test phase4c2_constraint_api -- --nocapture`

Expected: all Phase 4C2-B tests and the new Phase 4C2-C public tests pass with stable ErrorCode and names.

- [ ] **Step 6: Commit the public contract**

```bash
git add tests/phase4c2_constraint_api.rs src/sql/bind/statement.rs src/sql/plan/mod.rs src/sql/exec/mutation.rs
git commit -m "test: prove foreign key SQL contracts"
```

### Task 9: Snapshots, Real Files, Reopen, and Corruption Matrix

**Files:**
- Modify: `tests/support/format2.rs`
- Modify: `tests/phase4c2_constraint_api.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Consumes: complete V1 graph and existing independent raw format-2 parser/capture.
- Produces: raw parent fields/reverse edges, old/new snapshot proof, shallow/deep corruption behavior, checkpoint/reopen proof, and legacy compatibility.

- [ ] **Step 1: Extend the independent raw capture model**

```rust
pub struct RawConstraint {
    pub id: u64,
    pub table_id: u64,
    pub kind: u8,
    pub index_id: Option<u64>,
    pub parent_table_id: Option<u64>,
    pub parent_constraint_id: Option<u64>,
    pub columns: Vec<u64>,
    pub check_program_len: u32,
    pub check_program_version: Option<u8>,
}

pub struct RawCatalogCapture {
    pub feature_bits: u64,
    pub catalog_root: RawPageRef,
    pub accepted_lsn: u64,
    pub next_object_id: u64,
    pub page_count: u64,
    pub freelist: Vec<RawFreelistTrunk>,
    pub tables: BTreeMap<String, RawTable>,
    pub indexes: BTreeMap<String, RawIndex>,
    pub constraints: BTreeMap<String, RawConstraint>,
    pub table_constraints: BTreeMap<u64, Vec<u64>>,
    pub referenced_by: BTreeMap<u64, Vec<u64>>,
}
```

Parse 0x34 keys and their V1 mapping values without calling production catalog decoders. Validate strict lengths, key/value child equality, duplicate edges, and sorted captures.

- [ ] **Step 2: Run raw parser tests and confirm/update expected failures**

Run: `rustup run stable cargo test --test phase4c2_constraint_api raw_foreign_key -- --nocapture`

Expected before implementation: raw parser rejects family 0x34 or omits parent fields. Expected after implementation: old local-constraint files still parse with empty reverse maps and None parents.

- [ ] **Step 3: Add snapshot and checkpoint/reopen tests**

```rust
#[test]
fn old_snapshot_keeps_old_cross_table_graph_and_roots() {
    // old snapshot begins before child FK CREATE;
    // writer creates child and commits;
    // old snapshot sees no child/reverse edge and the old parent version;
    // new snapshot sees the complete child/reverse edge and bumped version.
}

#[test]
fn foreign_key_graph_survives_checkpoint_reopen_and_read_only_open_without_writes() {
    // checkpoint, reopen, enforce both child and parent directions;
    // hash database/WAL before and after read-only open and require equality.
}
```

- [ ] **Step 4: Add byte-level graph corruption fixtures**

```rust
#[test]
fn phase_4c2c_foreign_key_catalog_corruption_matrix() {
    for mutation in [
        RemoveReferencedBy,
        AddOrphanReferencedBy,
        ChangeReferencedByChildValue,
        ChangeParentTableId,
        ChangeParentConstraintId,
        ChangeParentKind,
        ReorderParentConstraintColumn,
        RemoveParentSystemIndex,
    ] {
        assert_fatal_corruption_on_open_and_recovery(mutation);
    }
}
```

Ordinary open validates the catalog graph and relation/index root disjointness but does not compare every logical table/index entry. Explicit deep validation additionally walks all table and index trees. Both reject structural graph corruption; logical missing-entry tests belong to model/fault tests.

- [ ] **Step 5: Prove legacy format-2 and feature-bit behavior**

```rust
#[test]
fn pre_constraint_format2_file_remains_writable_and_first_fk_uses_existing_feature_bit() {
    let file = create_pre_index_constraint_format2_file();
    open_and_create_parent_child_fk(&file).unwrap();
    let capture = capture_format2(&file).unwrap();
    assert!(capture.feature_bits & INDEX_CONSTRAINT_V1 != 0);
    assert_eq!(capture.referenced_by.len(), 1);
}
```

No new bit or migration is introduced. Records without INDEX_CONSTRAINT_V1 remain Corruption; unknown bits remain Unsupported before write/recovery/truncate.

- [ ] **Step 6: Run real-file, snapshot, recovery, and raw-capture tests**

Run: `rustup run stable cargo test --test phase4c2_constraint_api snapshot -- --nocapture`

Run: `rustup run stable cargo test --test phase4c2_constraint_api reopen -- --nocapture`

Run: `rustup run stable cargo test storage::test_support::phase_4c2c_foreign_key_catalog_corruption_matrix -- --exact --nocapture`

Run: `rustup run stable cargo test --test phase4c2_constraint_api raw_foreign_key -- --nocapture`

Expected: all new and old format-2 witnesses pass; each corruption is deterministic Corruption and no candidate falls back to an older complete commit.

- [ ] **Step 7: Commit persistence and corruption proof**

```bash
git add tests/support/format2.rs tests/phase4c2_constraint_api.rs src/storage/test_support.rs
git commit -m "test: cover foreign key persistence corruption"
```

### Task 10: Fixed-Seed Cross-Table Model and Crash-Recovery Matrix

**Files:**
- Modify: `src/sql/exec/test_support.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Consumes: public SQL path, raw capture from Task 9, existing SQL fault hooks/subprocess recovery harness, and deterministic RNG.
- Produces: ignored `phase_4c2c_fixed_seed_foreign_key_model` and `phase_4c2c_foreign_key_crash_recovery_matrix` tests.

- [ ] **Step 1: Add a deterministic reference model**

```rust
struct ForeignKeyModel {
    parents: BTreeMap<i64, ParentRow>,
    indexed_children: BTreeMap<i64, ChildRow>,
    scanned_children: BTreeMap<i64, ChildRow>,
    in_transaction: Option<Box<ForeignKeyModel>>,
}

enum ForeignKeyOperation {
    InsertParent(i64),
    UpdateParent { old: i64, new: i64 },
    DeleteParent(i64),
    InsertChild { indexed: bool, id: i64, parent: Option<i64> },
    UpdateChild { indexed: bool, id: i64, parent: Option<i64> },
    DeleteChild { indexed: bool, id: i64 },
    Begin,
    Commit,
    Rollback,
    Reopen,
    Checkpoint,
}
```

Use a fixed documented seed and at least 2,000 operations. After every operation compare ErrorCode, persisted constraint name, affected rows, transaction state, both table row sets, both child indexes, both parent system indexes, reverse graph, and parent SchemaVersions.

- [ ] **Step 2: Run the model and confirm the first divergence before completing the harness**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2c_fixed_seed_foreign_key_model -- --exact --ignored --nocapture`

Expected during RED: test is absent or the incomplete engine diverges on a generated child/parent operation. Expected after implementation: PASS with the seed and operation count printed.

- [ ] **Step 3: Add complete CREATE/DML/DROP fault scenarios**

```rust
enum Phase4c2cScenario {
    CreateForeignKey,
    InsertChild,
    UpdateChild,
    UpdateParentRestricted,
    DeleteParentRestricted,
    DropChild,
}
```

For successful mutations, crash at every observed catalog record, table page frame, parent/child index page frame, allocator/freelist frame, WAL commit, WAL sync, memory publish, checkpoint, and directory-sync occurrence. Restricted operations must prove no page/catalog/WAL mutation before their Constraint error.

- [ ] **Step 4: Define exact old-or-new recovery assertions**

```rust
fn assert_phase4c2c_recovery(actual: &RawCatalogCapture, old: &ExpectedState, new: &ExpectedState) {
    assert!(actual == &old.capture || actual == &new.capture);
    assert_complete_foreign_key_graph(actual);
    assert_table_index_constraint_entry_consistency(actual);
    assert_parent_versions_match_reverse_graph(actual);
}
```

No mixed state may contain only the child definition, only ReferencedBy, a bumped parent without the edge, an edge without the bump, a row without all owned index entries, or roots from different commits.

- [ ] **Step 5: Run model and crash matrix**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2c_fixed_seed_foreign_key_model -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2c_foreign_key_crash_recovery_matrix -- --exact --ignored --nocapture`

Expected: both pass and print nonzero scenario, crash, record-family, and hook occurrence counts; every observed occurrence is targeted.

- [ ] **Step 6: Commit model and crash proof**

```bash
git add src/sql/exec/test_support.rs src/storage/test_support.rs
git commit -m "test: model foreign key crash recovery"
```

### Task 11: Scale Witness, Documentation, Deep CI, and Full Verification

**Files:**
- Modify: `tests/phase4c2_constraint_scale.rs`
- Modify: `.github/workflows/deep-ci.yml`
- Modify: `README.md`

**Interfaces:**
- Consumes: all prior tasks and existing exhaustive profile/workflow split.
- Produces: indexed/unindexed scale acceptance, accurate README scope, one deep Phase 4C2-C job, and final stable/MSRV/Windows evidence.

- [ ] **Step 1: Add the ignored indexed/unindexed scale witness**

```rust
#[test]
#[ignore = "run by the dedicated Phase 4C2-C Deep CI job"]
fn phase_4c2c_hundred_thousand_row_foreign_key_smoke() {
    const ROWS: i64 = 100_000;
    let mut connection = scale_foreign_key_connection();
    insert_parent_rows(&mut connection, ROWS);
    insert_indexed_and_unindexed_children(&mut connection, ROWS);
    update_child_keys_with_existing_parents(&mut connection, ROWS);
    assert_parent_updates_restricted_for_both_paths(&mut connection);
    delete_all_children(&mut connection);
    delete_all_parents(&mut connection);
    checkpoint_reopen_and_assert_empty_complete_graph(&mut connection);
}
```

The test records completion and correctness only; it has no shared-runner millisecond threshold.

- [ ] **Step 2: Run the scale witness locally**

Run: `rustup run stable cargo test --profile exhaustive --test phase4c2_constraint_scale phase_4c2c_hundred_thousand_row_foreign_key_smoke -- --exact --ignored --nocapture`

Expected: PASS with 100,000 parents and both 100,000-row child paths; no leaked rows, indexes, reverse records, or pages after cleanup/reopen.

- [ ] **Step 3: Add one Deep CI job without expanding PR CI**

```yaml
  phase4c2c-foreign-keys:
    name: Linux Phase 4C2-C foreign keys
    runs-on: ubuntu-latest
    timeout-minutes: 120
    steps:
      - uses: actions/checkout@v4
      - name: Install stable Rust
        run: rustup toolchain install stable --profile minimal
      - name: Run deterministic foreign-key model
        run: rustup run stable cargo test sql::exec::test_support::phase_4c2c_fixed_seed_foreign_key_model -- --exact --ignored --nocapture
      - name: Run foreign-key crash recovery matrix
        run: rustup run stable cargo test sql::exec::test_support::phase_4c2c_foreign_key_crash_recovery_matrix -- --exact --ignored --nocapture
      - name: Run 100,000-row foreign-key smoke
        run: rustup run stable cargo test --profile exhaustive --test phase4c2_constraint_scale phase_4c2c_hundred_thousand_row_foreign_key_smoke -- --exact --ignored --nocapture
```

Do not alter `.github/workflows/ci.yml`: pull requests keep Linux quality/tests, Rust 1.85 check, and Windows check only. Pure documentation path filters remain unchanged.

- [ ] **Step 4: Update the verified scope in README**

```markdown
Phase 4C2-C supports immediate, named foreign keys that reference declared
PRIMARY KEY or UNIQUE constraints; child INSERT/UPDATE and parent UPDATE/DELETE
checks; optional child-index prefix probes with ROWID-scan fallback; atomic
reverse dependencies; and safe DROP TABLE rejection/cleanup. Self-reference,
cycles, ALTER TABLE, deferred/referential actions, and optimizer IndexScan remain
unsupported; IndexScan is Phase 4C2-D.
```

- [ ] **Step 5: Run formatting, lint, stable, integration, and MSRV gates**

Run: `rustup run stable cargo fmt --check`

Run: `rustup run stable cargo clippy --all-targets --all-features -- -D warnings`

Run: `rustup run stable cargo test --all-targets`

Run: `rustup run stable cargo test --test phase4c2_constraint_api -- --nocapture`

Run: `rustup run 1.85.0 cargo check --all-targets`

Expected: all commands exit 0 with no warnings; ignored deep tests remain explicitly listed but are run by Steps 2 and 6.

- [ ] **Step 6: Run all three Phase 4C2-C deep tests**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2c_fixed_seed_foreign_key_model -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2c_foreign_key_crash_recovery_matrix -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test --profile exhaustive --test phase4c2_constraint_scale phase_4c2c_hundred_thousand_row_foreign_key_smoke -- --exact --ignored --nocapture`

Expected: all pass with their evidence summaries.

- [ ] **Step 7: Verify Windows compatibility on a Windows host**

Run: `rustup run stable cargo test --all-targets`

Run: `rustup run 1.85.0 cargo check --all-targets`

Expected: both exit 0. The GitHub Windows stable job supplies independent runner evidence when Actions billing permits jobs to start.

- [ ] **Step 8: Perform the final scope audit**

Run: `rg -n "ForeignKey|ReferencedBy|phase_4c2c|4C2-C" src tests README.md .github/workflows/deep-ci.yml`

Run: `rg -n "Cascade|SetNull|SetDefault|Deferrable|ALTER TABLE|IndexScan" src`

Expected: the first command shows the planned catalog/runtime/tests/docs; the second shows only parser rejection, explicit Unsupported boundaries, or pre-existing Phase 4C2-D placeholders—no cascade/deferred/ALTER implementation and no DML/query optimizer IndexScan.

- [ ] **Step 9: Commit final acceptance assets**

```bash
git add tests/phase4c2_constraint_scale.rs .github/workflows/deep-ci.yml README.md
git commit -m "test: gate Phase 4C2-C foreign keys"
```

---

## Final Review Gate

- [ ] Every ForeignKey has one local ordered column list, one valid declared parent key, and exactly one ReferencedBy edge; every edge points back exactly.
- [ ] Child checks and parent restrict checks read current WriteBatch roots and finish before affected persistent mutations.
- [ ] NULL and Int64-to-Decimal behavior is exact; all other type differences fail before mutation.
- [ ] Indexed and ROWID fallback restrict paths return identical results and error classes, and neither hides corruption.
- [ ] CREATE/DROP changes each affected parent SchemaVersion once and stale parent statements return SchemaChanged.
- [ ] DROP parent rejection and DROP child cleanup are atomic and preserve the stable child constraint name in diagnostics.
- [ ] Snapshot, reopen, checkpoint, raw format-2, corruption, fixed-seed, crash, scale, stable, MSRV, and Windows evidence is recorded.
- [ ] The implementation contains no self/cyclic FK, ALTER TABLE, actions, deferral, cascade, query IndexScan, or Phase 5 behavior.
