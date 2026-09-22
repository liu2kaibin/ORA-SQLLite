# Phase 4C2-A Index Storage and DDL Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add durable explicit ordinary and unique indexes, transactional `CREATE INDEX`/`DROP INDEX`, and atomic INSERT/UPDATE/DELETE index maintenance without adding constraints or query-planner index scans.

**Architecture:** Extend the format-2 catalog with versioned index records and one monotonic feature bit, while continuing to publish catalog, table, and index roots through the existing `WriteBatch` and WAL commit state. Put physical key encoding and B+Tree access behind `IndexStorage`, and route SQL mutations through `ConstrainedTableWrite`; in this phase that facade loads only explicit indexes, leaving stable extension points for constraints in 4C2-B/C. Keep SELECT, UPDATE target discovery, and DELETE target discovery on the existing table scan until 4C2-D.

**Tech Stack:** Rust 2024, MSRV Rust 1.85, existing standard-library collections and synchronization, existing Pager/B+Tree/WAL/catalog/row/SQL layers, existing `fs4`, `sqlparser`, and `getrandom` dependencies, GitHub Actions on Linux and Windows.

**Spec:** `docs/superpowers/specs/2026-09-18-phase-4c2-index-constraints-design.md`

## Global Constraints

- Read the approved specification above and the Phase 4C1 handoff in `docs/superpowers/specs/2026-09-13-phase-4c1-sql-execution-design.md` before changing code.
- This plan implements only Phase 4C2-A. PrimaryKey, Unique table constraints, NotNull constraint records, CheckProgram, ForeignKey, ReferencedBy, and IndexScan remain deferred to 4C2-B/C/D.
- Keep main-file format 2 and add `INDEX_CONSTRAINT_V1 = 1 << 1`; do not add startup migration, full-database rewrite, or a new WAL format.
- Keep `#![forbid(unsafe_code)]`; do not add unsafe Rust or a production dependency.
- Keep Cargo edition 2024 and `rust-version = "1.85"`; every public and internal signature must compile on Rust 1.85.
- Keep the existing public `Connection`, `Statement`, `Rows`, `ExecutionResult`, and transaction contracts unchanged.
- All index and table mutations must use the existing `WriteBatch`, statement checkpoint, WAL, snapshot, checkpoint, recovery, single-writer, and fatal-error rules.
- A read-only open must never enable a feature bit, create catalog records, rebuild an index, or change a file.
- Once committed, `INDEX_CONSTRAINT_V1` must never clear, including after the last explicit index is dropped.
- `INDEX_CONSTRAINT_V1` without `CATALOG_V1`, or any 4C2 catalog record without `INDEX_CONSTRAINT_V1`, is `Corruption`.
- Existing programs that do not know `INDEX_CONSTRAINT_V1` must refuse to write, recover, or truncate such a database; Phase 4C2-A must keep current unknown-bit rejection tests as explicit compatibility evidence.
- An unrecognized future feature bit returns `Unsupported`; contradictory combinations of recognized bits return `Corruption`.
- Index names are globally unique inside the index namespace and use the SQL identifier limit of 1–128 UTF-8 bytes; do not reuse the older storage-internal 1013-byte catalog-key ceiling as the SQL identifier limit.
- Every physical index key is `component-0 || ... || component-N || rowid-be`, every row is indexed, and the complete key must be at most 1024 bytes.
- Ordinary unique indexes permit multiple keys containing NULL, but reject a duplicate logical key when all indexed columns are non-NULL.
- Blob columns support equality semantics only; Phase 4C2-A does not add Blob range planning or any other IndexScan planning.
- DML must encode every affected index key and validate all size and uniqueness conditions before changing the table or any index.
- `CREATE INDEX` streams rows in ROWID order and uses no full key set, external sort, temporary file, background builder, or multi-commit publication.
- Index DDL increments the owning table's `SchemaVersion`; ordinary DML and table/index root splits do not.
- Do not optimize JOIN, ORDER BY, GROUP BY, DISTINCT, SELECT access paths, or UPDATE/DELETE target discovery in this phase.
- Use checked arithmetic for IDs, lengths, offsets, ordinals, key sizes, row counts, and memory estimates.
- Use TDD for every task: observe the named test fail, make the smallest implementation pass, run the focused suite, and commit.
- Do not weaken, ignore, or delete an existing assertion merely to make a new test pass.

## File and Responsibility Map

| Path | Responsibility |
|---|---|
| `src/storage/features.rs` | Declare and validate the monotonic 4C2 feature bit. |
| `src/storage/catalog/key.rs` | Stable 0x20–0x23 index keys and reserved 0x30–0x34 constraint key families. |
| `src/storage/catalog/record.rs` | Strict V1 index record envelopes and mapping payloads. |
| `src/storage/catalog/types.rs` | `IndexDef`, `IndexOwnership`, IDs, and schema-version helpers. |
| `src/storage/catalog/mod.rs` | Decode/cross-check index catalog image; expose reader/writer index operations. |
| `src/storage/index/key.rs` | Order-preserving IndexKey V1 encoding, decoding, and logical-prefix bounds. |
| `src/storage/index/mod.rs` | `IndexStorage` read/write facade, uniqueness probe, and root lifecycle. |
| `src/storage/relation.rs` | `ConstrainedTableWrite` atomic table-plus-explicit-index mutation facade. |
| `src/storage/validate.rs` | Deep reachability and non-overlap validation for every index tree. |
| `src/storage/pager.rs` | Track, refresh, and release current Index roots under mutation checkpoints. |
| `src/sql/bind/statement.rs` | Bind DROP INDEX to a stable `IndexId` and owning-table dependency. |
| `src/sql/bound/statement.rs` | Revalidate retained index identity and owning schema version. |
| `src/sql/plan/mutation.rs` | Immutable `CreateIndexPlan` and `DropIndexPlan`; retain IDs, never roots. |
| `src/sql/plan/mod.rs` | Replace deferred explicit-index plans with executable plan variants. |
| `src/sql/exec/mutation.rs` | Execute index DDL and route DML through `ConstrainedTableWrite`. |
| `src/sql/exec/test_support.rs` | Fixed-seed index model and SQL crash/recovery scenarios. |
| `src/storage/test_support.rs` | Real-file feature/catalog/index corruption and recovery fixtures. |
| `tests/phase4c2_index_api.rs` | Public SQL/API index DDL, DML, transaction, reopen, and snapshot tests. |
| `tests/phase4c2_index_scale.rs` | Ignored 100,000-row explicit-index acceptance smoke. |
| `.github/workflows/ci.yml` | Dedicated Linux Phase 4C2-A model, recovery, and scale job. |
| `README.md` | Verified Phase 4C2-A capabilities and explicit 4C2-B/C/D exclusions. |

---

### Task 1: Feature Bit and Complete 4C2 Catalog Key Space

**Files:**
- Modify: `src/storage/features.rs`
- Modify: `src/storage/header.rs`
- Modify: `src/storage/wal/format.rs`
- Modify: `src/storage/catalog/key.rs`
- Modify: `src/storage/catalog/types.rs`
- Test: inline tests in the five files above

**Interfaces:**
- Consumes: existing `CATALOG_V1`, `KNOWN_FEATURE_BITS`, `CatalogName`, `TableId`, `ColumnId`, `IndexId`, and `ConstraintId`.
- Produces: `INDEX_CONSTRAINT_V1`, stable `RecordKind` values 0x20–0x34, exact key builders, and `DecodedCatalogKey` variants used by Tasks 2–3 and reserved for 4C2-B/C.

- [ ] **Step 1: Add failing feature-bit and hand-computed key tests**

```rust
#[test]
fn phase_4c2_feature_bit_is_known_and_requires_catalog_v1() {
    assert_eq!(INDEX_CONSTRAINT_V1, 1 << 1);
    assert_eq!(KNOWN_FEATURE_BITS, CATALOG_V1 | INDEX_CONSTRAINT_V1);
    assert!(validate_feature_bits(CATALOG_V1 | INDEX_CONSTRAINT_V1).is_ok());
    assert_eq!(
        validate_feature_bits(INDEX_CONSTRAINT_V1).unwrap_err().code(),
        ErrorCode::Corruption
    );
    assert_eq!(
        validate_feature_bits(1 << 63).unwrap_err().code(),
        ErrorCode::Unsupported
    );
}

#[test]
fn phase_4c2_catalog_keys_have_stable_big_endian_bytes() {
    let table = TableId::new(0x0102_0304_0506_0708).unwrap();
    let index = IndexId::new(0x1112_1314_1516_1718).unwrap();
    let constraint = ConstraintId::new(0x2122_2324_2526_2728).unwrap();
    let name = CatalogName::new("IX_A").unwrap();
    assert_eq!(index_name_key(&name).unwrap(), b"\x20\x00\x04IX_A");
    assert_eq!(index_definition_key(index), [vec![0x21], index.get().to_be_bytes().to_vec()].concat());
    assert_eq!(table_index_key(table, index), [vec![0x22], table.get().to_be_bytes().to_vec(), index.get().to_be_bytes().to_vec()].concat());
    assert_eq!(index_column_key(index, 2), [vec![0x23], index.get().to_be_bytes().to_vec(), vec![0, 2]].concat());
    assert_eq!(constraint_name_key(&name).unwrap()[0], 0x30);
    assert_eq!(constraint_definition_key(constraint)[0], 0x31);
    assert_eq!(table_constraint_key(table, constraint)[0], 0x32);
    assert_eq!(constraint_column_key(constraint, 2)[0], 0x33);
    assert_eq!(referenced_by_key(constraint, ConstraintId::new(41).unwrap())[0], 0x34);
}
```

- [ ] **Step 2: Run the focused tests and confirm the red state**

Run: `rustup run stable cargo test phase_4c2_feature_bit_is_known_and_requires_catalog_v1 -- --exact`

Run: `rustup run stable cargo test phase_4c2_catalog_keys_have_stable_big_endian_bytes -- --exact`

Expected: compilation fails because the feature bit and 4C2 key builders do not exist.

- [ ] **Step 3: Add the bit and enforce its dependency**

```rust
pub(super) const CATALOG_V1: u64 = 1 << 0;
pub(super) const INDEX_CONSTRAINT_V1: u64 = 1 << 1;
pub(super) const KNOWN_FEATURE_BITS: u64 = CATALOG_V1 | INDEX_CONSTRAINT_V1;

pub(super) fn validate_feature_bits(bits: u64) -> Result<()> {
    if bits & !KNOWN_FEATURE_BITS != 0 {
        return Err(DbError::new(ErrorCode::Unsupported, "database uses unsupported feature bits"));
    }
    if bits & INDEX_CONSTRAINT_V1 != 0 && bits & CATALOG_V1 == 0 {
        return Err(DbError::new(ErrorCode::Corruption, "index/constraint feature requires catalog V1"));
    }
    Ok(())
}
```

- [ ] **Step 4: Add every approved key family and strict decoder variant**

```rust
#[repr(u8)]
pub(super) enum RecordKind {
    CatalogMeta = 0x00,
    TableName = 0x10,
    TableDefinition = 0x11,
    ColumnName = 0x12,
    ColumnOrdinal = 0x13,
    ColumnDefinition = 0x14,
    IndexName = 0x20,
    IndexDefinition = 0x21,
    TableIndex = 0x22,
    IndexColumn = 0x23,
    ConstraintName = 0x30,
    ConstraintDefinition = 0x31,
    TableConstraint = 0x32,
    ConstraintColumn = 0x33,
    ReferencedBy = 0x34,
}

pub(super) enum DecodedCatalogKey<'a> {
    // retain existing variants
    IndexName(&'a str),
    IndexDefinition(IndexId),
    TableIndex { table_id: TableId, index_id: IndexId },
    IndexColumn { index_id: IndexId, ordinal: u16 },
    ConstraintName(&'a str),
    ConstraintDefinition(ConstraintId),
    TableConstraint { table_id: TableId, constraint_id: ConstraintId },
    ConstraintColumn { constraint_id: ConstraintId, ordinal: u16 },
    ReferencedBy { parent_id: ConstraintId, child_id: ConstraintId },
}
```

Implement `index_name_key`, `index_definition_key`, `table_index_key`, `index_column_key`, `constraint_name_key`, `constraint_definition_key`, `table_constraint_key`, `constraint_column_key`, and `referenced_by_key` with checked lengths and big-endian IDs. Decode each family with exact-length checks, nonzero typed IDs, 1–128-byte names, and strict EOF.

- [ ] **Step 5: Add truncation, oversized-name, zero-ID, and ordering assertions**

```rust
#[test]
fn phase_4c2_key_decoder_rejects_every_truncation_and_zero_id() {
    let complete = table_index_key(TableId::new(7).unwrap(), IndexId::new(9).unwrap());
    for end in 0..complete.len() {
        assert_eq!(decode_key(&complete[..end]).unwrap_err().code(), ErrorCode::Corruption);
    }
    let mut zero = complete;
    zero[9..17].fill(0);
    assert_eq!(decode_key(&zero).unwrap_err().code(), ErrorCode::Corruption);
}

#[test]
fn table_index_and_index_column_keys_sort_by_owner_then_child() {
    let table = TableId::new(7).unwrap();
    assert!(table_index_key(table, IndexId::new(8).unwrap()) < table_index_key(table, IndexId::new(9).unwrap()));
    let index = IndexId::new(11).unwrap();
    assert!(index_column_key(index, 255) < index_column_key(index, 256));
}
```

- [ ] **Step 6: Run feature, key, header, and WAL format tests**

Run: `rustup run stable cargo test storage::features:: --lib`

Run: `rustup run stable cargo test storage::catalog::key:: --lib`

Run: `rustup run stable cargo test storage::header:: --lib`

Run: `rustup run stable cargo test storage::wal::format:: --lib`

Expected: all pass; unknown future bits are rejected as `Unsupported`, invalid known-bit dependencies are `Corruption`, and the new known bit is accepted only with `CATALOG_V1`.

- [ ] **Step 7: Commit Task 1**

```bash
git add src/storage/features.rs src/storage/header.rs src/storage/wal/format.rs src/storage/catalog/key.rs src/storage/catalog/types.rs
git commit -m "feat: reserve phase 4c2 catalog key space"
```

---

### Task 2: Strict Index Catalog Record V1 Codecs

**Files:**
- Modify: `src/storage/catalog/types.rs`
- Modify: `src/storage/catalog/record.rs`
- Test: inline tests in both files

**Interfaces:**
- Consumes: `IndexId`, `ConstraintId`, `TableId`, `ColumnId`, `PageRef`, `CatalogName`, `RecordKind`, and the existing catalog envelope.
- Produces: `IndexOwnership`, `IndexDef`, index mapping `CatalogRecord` variants, and strict V1 encode/decode functions for Task 3.

- [ ] **Step 1: Add failing type-invariant and exact-byte tests**

```rust
#[test]
fn explicit_index_definition_enforces_nonzero_columns_and_explicit_owner() {
    let index = IndexDef::new(
        IndexId::new(5).unwrap(), TableId::new(2).unwrap(), page_ref(7, 1), 2, true,
        IndexOwnership::Explicit, CatalogName::new("UX_T_AB").unwrap(),
    ).unwrap();
    assert_eq!(index.column_count(), 2);
    assert_eq!(index.ownership(), IndexOwnership::Explicit);
    assert_eq!(IndexDef::new(index.id(), index.table_id(), index.root(), 0, false, IndexOwnership::Explicit, index.name().clone()).unwrap_err().code(), ErrorCode::Type);
}

#[test]
fn index_record_v1_has_hand_computed_envelope_and_strict_round_trip() {
    let index = index_definition_fixture();
    let encoded = encode_index_definition(&index);
    assert_eq!(&encoded[0..4], &[0x21, 1, 0, 0]);
    assert_eq!(u32::from_le_bytes(encoded[4..8].try_into().unwrap()) as usize, encoded.len() - 8);
    assert_eq!(decode_index_definition(index.id(), &encoded).unwrap(), index);
    let mut reserved = encoded.clone();
    reserved[2] = 1;
    assert_eq!(decode_index_definition(index.id(), &reserved).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Run the focused record tests and confirm the red state**

Run: `rustup run stable cargo test index_record_v1_has_hand_computed_envelope_and_strict_round_trip -- --exact`

Expected: compilation fails because `IndexDef` and its record functions do not exist.

- [ ] **Step 3: Define the stable index record types**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexOwnership {
    Explicit,
    Constraint(ConstraintId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexDef {
    id: IndexId,
    table_id: TableId,
    root: PageRef,
    column_count: u16,
    unique: bool,
    ownership: IndexOwnership,
    name: CatalogName,
}
```

Add checked `IndexDef::new` and getters. Require `column_count` in `1..=MAX_COLUMNS`; require a valid `PageRef`; preserve `Constraint(ConstraintId)` on disk for 4C2-B while Task 7 allows only `Explicit` user DDL.

- [ ] **Step 4: Add exact V1 encoders and strict decoders**

```rust
pub(super) fn encode_index_name_mapping(id: IndexId) -> Vec<u8>;
pub(super) fn encode_table_index_mapping(id: IndexId) -> Vec<u8>;
pub(super) fn encode_index_column_mapping(id: ColumnId) -> Vec<u8>;
pub(super) fn encode_index_definition(index: &IndexDef) -> Vec<u8>;

pub(super) fn decode_index_name_mapping(bytes: &[u8]) -> Result<IndexId>;
pub(super) fn decode_table_index_mapping(bytes: &[u8]) -> Result<IndexId>;
pub(super) fn decode_index_column_mapping(bytes: &[u8]) -> Result<ColumnId>;
pub(super) fn decode_index_definition(expected: IndexId, bytes: &[u8]) -> Result<IndexDef>;
```

Encode the definition payload in this order: IndexId, TableId, root page ID, root generation, column count, unique byte, owner-kind byte (`0` explicit, `1` constraint), reserved `u16 = 0`, owner ConstraintId or zero, name length, name bytes. Reject unknown flags, nonzero reserved bytes, explicit owner with nonzero ConstraintId, constraint owner with zero ConstraintId, mismatched key/payload ID, invalid UTF-8, invalid name length, and trailing bytes.

- [ ] **Step 5: Add every-byte truncation and contradictory-owner tests**

```rust
#[test]
fn index_definition_rejects_every_truncation_and_trailing_byte() {
    let index = index_definition_fixture();
    let encoded = encode_index_definition(&index);
    for end in 0..encoded.len() {
        assert_eq!(decode_index_definition(index.id(), &encoded[..end]).unwrap_err().code(), ErrorCode::Corruption);
    }
    let mut trailing = encoded;
    trailing.push(0);
    assert_eq!(decode_index_definition(index.id(), &trailing).unwrap_err().code(), ErrorCode::Corruption);
}
```

Mutate owner kind, owner ID, unique byte, root page ID/generation, column count, record version, payload length, and name length one field at a time and assert `Corruption`, except a future record version which must remain `Unsupported`.

- [ ] **Step 6: Run all catalog record and type tests under stable and MSRV**

Run: `rustup run stable cargo test storage::catalog::record:: --lib`

Run: `rustup run stable cargo test storage::catalog::types:: --lib`

Run: `rustup run 1.85.0 cargo test storage::catalog::record:: --lib`

Expected: all pass with unchanged table and column golden bytes.

- [ ] **Step 7: Commit Task 2**

```bash
git add src/storage/catalog/types.rs src/storage/catalog/record.rs
git commit -m "feat: encode phase 4c2 index records"
```

---

### Task 3: Validated Index Catalog Image and Mutation API

**Files:**
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/catalog/types.rs`
- Modify: `src/storage/validate.rs`
- Modify: `src/storage/pager.rs`
- Test: inline catalog, pager, and validation tests

**Interfaces:**
- Consumes: Task 1 keys, Task 2 records, existing `CatalogImage`, `CatalogReader`, `CatalogWriter`, `BTree`, and `WriteBatch` tree tracking.
- Produces: index lookup/enumeration, atomic catalog insertion/removal/root update, table schema bump, and deep index-tree reachability validation for Tasks 5, 7, and 8.

- [ ] **Step 1: Add failing catalog cross-reference tests**

```rust
#[test]
fn catalog_requires_bidirectional_index_records_and_contiguous_columns() {
    let fixture = raw_index_catalog_fixture();
    assert!(fixture.complete_image().open().is_ok());
    for broken in [
        fixture.without_index_name(), fixture.without_table_index(),
        fixture.without_index_column(0), fixture.with_duplicate_index_column(0),
        fixture.with_wrong_table_index(), fixture.with_foreign_column(),
    ] {
        assert_eq!(broken.open().unwrap_err().code(), ErrorCode::Corruption);
    }
}

#[test]
fn catalog_rejects_4c2_records_without_the_feature_bit() {
    let fixture = raw_index_catalog_fixture().with_feature_bits(CATALOG_V1);
    assert_eq!(fixture.open().unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Run the catalog tests and confirm the red state**

Run: `rustup run stable cargo test catalog_requires_bidirectional_index_records_and_contiguous_columns -- --exact`

Expected: the complete fixture cannot be decoded because `CatalogImage` has no index maps.

- [ ] **Step 3: Decode and cross-check the index image**

```rust
pub(crate) struct CatalogImage {
    // retain existing fields
    indexes: BTreeMap<IndexId, IndexDef>,
    index_names: BTreeMap<String, IndexId>,
    table_indexes: BTreeMap<TableId, BTreeSet<IndexId>>,
    index_columns: BTreeMap<IndexId, BTreeMap<u16, ColumnId>>,
}

fn decode(source: &mut impl PageSource, tree: &BTree, feature_bits: u64) -> Result<Self>;
```

Pass the snapshot or batch feature bits into every `CatalogImage::decode` call. During decode, note whether any key is in 0x20–0x34. Reject all such records when `INDEX_CONSTRAINT_V1` is absent. In 4C2-A decode 0x20–0x23 fully; return `Unsupported` for well-formed reserved 0x30–0x34 records because their value programs are implemented by 4C2-B/C, while malformed keys still return `Corruption`. Both known feature bits with no index or constraint records decode as a legal empty 4C2 image.

Validate global object-ID uniqueness across tables, columns, and indexes; name-definition and table-definition links in both directions; exact column count; ordinals contiguous from zero; no repeated ColumnId; every indexed column owned by the index table; Index roots of `TreeKind::Index`; and explicit owner ID zero. A decoded `IndexOwnership::Constraint(_)` is structurally representable by the V1 codec but Phase 4C2-A catalog open returns `Unsupported` until 4C2-B can validate the owning constraint and reverse link.

- [ ] **Step 4: Add stable reader and writer interfaces**

```rust
impl CatalogReader {
    pub(crate) fn find_index_by_name(&self, name: &CatalogName) -> Option<&IndexDef>;
    pub(crate) fn find_index_by_id(&self, id: IndexId) -> Option<&IndexDef>;
    pub(crate) fn indexes_for_table(&self, table: TableId) -> impl Iterator<Item = &IndexDef>;
    pub(crate) fn columns_for_index(&self, index: IndexId) -> impl Iterator<Item = &ColumnDef>;
}

impl CatalogWriter {
    pub(crate) fn insert_index_definition(&mut self, batch: &mut WriteBatch, index: &IndexDef, columns: &[ColumnId]) -> Result<()>;
    pub(crate) fn update_index_root(&mut self, batch: &mut WriteBatch, index: IndexId, root: PageRef) -> Result<()>;
    pub(crate) fn remove_index(&mut self, batch: &mut WriteBatch, index: &IndexDef, columns: &[ColumnId]) -> Result<()>;
    pub(crate) fn bump_table_schema_version(&mut self, batch: &mut WriteBatch, table: TableId) -> Result<SchemaVersion>;
}

impl TableDef {
    pub(super) fn with_schema_version(self, schema_version: SchemaVersion) -> Self;
}
```

`insert_index_definition` writes IndexDefinition, IndexName, TableIndex, and ordered IndexColumn records in one nested mutation scope. `remove_index` verifies exact current records before deleting them and stages `release_tree_later(TreeKind::Index, root)`. `bump_table_schema_version` uses checked `u64 + 1` and replaces only the owning TableDefinition.

- [ ] **Step 5: Teach object/name validation and root tracking about indexes**

```rust
match kind {
    ObjectKind::Table => /* existing */,
    ObjectKind::Column => /* existing */,
    ObjectKind::Index => IndexId::from_object(object_id, kind)
        .ok().is_some_and(|id| self.image.indexes.contains_key(&id)),
    ObjectKind::Constraint => false,
}
```

Update `require_available_object` to include IndexDefinition keys. Add `WriteBatch::require_tracked_index_root`, use `track_tree(TreeKind::Index, root)` and `refresh_tree(TreeKind::Index, old, new)`, and preserve every added tracked root through nested checkpoint restore.

- [ ] **Step 6: Extend deep validation across catalog, table, index, and freelist pages**

```rust
fn validate_relation_trees(
    source: &mut impl PageSource,
    tables: impl Iterator<Item = PageRef>,
    indexes: impl Iterator<Item = PageRef>,
    live: &mut BTreeSet<PageId>,
) -> Result<()>;
```

For each table call `validate_tree(... TreeKind::Table ...)`; for each index call `validate_tree(... TreeKind::Index ...)`; reject overlap with catalog pages, any earlier tree, overflow pages, or freelist pages. Ordinary catalog open validates each root page identity but does not walk every descendant; `validate_deep` and destructive DDL perform the full walk.

- [ ] **Step 7: Test insert, root replacement, schema bump, removal, and rollback**

```rust
#[test]
fn index_catalog_mutations_restore_records_roots_and_schema_version() {
    let fixture = index_catalog_writer_fixture();
    let mut batch = fixture.pager().begin_write().unwrap();
    let checkpoint = batch.statement_checkpoint().unwrap();
    let index = fixture.insert_explicit_index(&mut batch, "IX_USERS_ID", &[fixture.id_column()]);
    fixture.split_and_update_index_root(&mut batch, index.id());
    fixture.bump_users_schema(&mut batch);
    batch.restore_statement(checkpoint);
    let catalog = batch.catalog_reader().unwrap();
    assert!(catalog.find_index_by_id(index.id()).is_none());
    assert_eq!(catalog.find_table_by_id(fixture.users()).unwrap().schema_version(), SchemaVersion::new(1).unwrap());
}
```

- [ ] **Step 8: Run catalog, pager, validation, and recovery-index tests**

Run: `rustup run stable cargo test storage::catalog:: --lib`

Run: `rustup run stable cargo test storage::pager:: --lib`

Run: `rustup run stable cargo test storage::validate:: --lib`

Expected: all pass; old format-2 catalog fixtures still open unchanged.

- [ ] **Step 9: Commit Task 3**

```bash
git add src/storage/catalog src/storage/validate.rs src/storage/pager.rs
git commit -m "feat: validate and mutate index catalog entries"
```

---

### Task 4: Order-Preserving IndexKey V1 Codec

**Files:**
- Create: `src/storage/index/key.rs`
- Create: `src/storage/index/mod.rs`
- Modify: `src/storage/mod.rs`
- Test: inline tests in `src/storage/index/key.rs`

**Interfaces:**
- Consumes: `Value`, `Decimal`, `OracleDate`, `OracleTimestamp`, `ColumnType`, `ColumnDef`, and `RowId`.
- Produces: `IndexKeyCodec`, `EncodedIndexKey`, and logical-prefix bounds for Tasks 5 and 8.

- [ ] **Step 1: Add failing scalar-order and normalization tests**

```rust
#[test]
fn index_key_byte_order_matches_sql_total_order() {
    let cases = ordered_scalar_cases();
    for pair in cases.windows(2) {
        let left = encode_one(&pair[0], RowId::new(1).unwrap()).unwrap();
        let right = encode_one(&pair[1], RowId::new(1).unwrap()).unwrap();
        assert!(left.physical() < right.physical());
    }
}

#[test]
fn decimal_equal_values_and_signed_zero_have_one_encoding() {
    assert_eq!(decimal_component(Decimal::new(false, 1, 10).unwrap()), decimal_component(Decimal::new(false, 0, 1).unwrap()));
    assert_eq!(float_component(-0.0).unwrap(), float_component(0.0).unwrap());
    assert_eq!(float_component(f64::NAN).unwrap_err().code(), ErrorCode::Type);
    assert_eq!(float_component(f64::INFINITY).unwrap_err().code(), ErrorCode::Type);
}
```

`ordered_scalar_cases()` must explicitly cover NULL placement; Int64 min/-1/0/1/max; Decimal negative/zero/positive values at scale -38 and 38; Float64 negative/zero/positive finite boundaries; Unicode text; Blob; Date; Timestamp without and with time zone; and false/true.

- [ ] **Step 2: Run the codec tests and confirm the red state**

Run: `rustup run stable cargo test storage::index::key:: --lib`

Expected: compilation fails because the index module and key codec do not exist.

- [ ] **Step 3: Define the exact codec result and entry points**

```rust
pub(crate) const MAX_INDEX_KEY_BYTES: usize = 1024;

pub(crate) struct EncodedIndexKey {
    physical: Vec<u8>,
    logical_len: usize,
    contains_null: bool,
}

pub(crate) struct DecodedIndexKey {
    values: Box<[Value]>,
    rowid: RowId,
}

impl EncodedIndexKey {
    pub(crate) fn physical(&self) -> &[u8];
    pub(crate) fn logical(&self) -> &[u8];
    pub(crate) const fn contains_null(&self) -> bool;
    pub(crate) const fn rowid(&self) -> RowId;
}

impl DecodedIndexKey {
    pub(crate) fn values(&self) -> &[Value];
    pub(crate) const fn rowid(&self) -> RowId;
}

impl IndexKeyCodec {
    pub(crate) fn encode(columns: &[ColumnDef], values: &[Value], rowid: RowId) -> Result<EncodedIndexKey>;
    pub(crate) fn decode(columns: &[ColumnDef], bytes: &[u8]) -> Result<DecodedIndexKey>;
    pub(crate) fn prefix_bounds(logical: &[u8]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>);
}
```

Require one value per indexed column and exact type agreement. Prefix each component with `0x00` non-NULL or `0x01` NULL. Append ROWID as eight big-endian bytes and reject zero during decode.

- [ ] **Step 4: Implement every approved scalar encoding**

```rust
fn encode_i64(value: i64, out: &mut Vec<u8>) {
    out.extend_from_slice(&(value as u64 ^ (1_u64 << 63)).to_be_bytes());
}

fn encode_f64(value: f64, out: &mut Vec<u8>) -> Result<()> {
    if !value.is_finite() { return Err(DbError::new(ErrorCode::Type, "indexed Float64 must be finite")); }
    let bits = if value == 0.0 { 0.0_f64.to_bits() } else { value.to_bits() };
    let ordered = if bits >> 63 == 1 { !bits } else { bits ^ (1_u64 << 63) };
    out.extend_from_slice(&ordered.to_be_bytes());
    Ok(())
}

fn encode_escaped(bytes: &[u8], out: &mut Vec<u8>) {
    for byte in bytes {
        if *byte == 0 { out.extend_from_slice(&[0, 0xff]); } else { out.push(*byte); }
    }
    out.extend_from_slice(&[0, 0]);
}
```

Implement Decimal canonicalization exactly as section 8 of the spec: canonical zero; strip magnitude trailing decimal zeros while reducing scale; checked `adjusted_exponent = digits.len() - scale` in `-37..=76`; sign prefix 0x00/0x01/0x02; sign-flipped i16 exponent; digit+1 bytes and 0x00 terminator; bytewise complement of the complete positive tail for negatives. Encode Date as year `u16` big-endian followed by month, day, hour, minute, and second bytes. Encode Timestamp as that Date tuple followed by microseconds `u32` big-endian; TimestampWithTimeZone then appends sign-flipped offset-minutes `i16` big-endian. Encode Bool as 0 for false and 1 for true. Escape Text/Blob zero bytes as shown.

- [ ] **Step 5: Add strict decode, composite-boundary, and 1024-byte tests**

```rust
#[test]
fn composite_text_boundaries_and_key_limit_are_unambiguous() {
    let a = encode_values(&[text("a"), text("b")], 1).unwrap();
    let b = encode_values(&[text("a\0"), text("b")], 1).unwrap();
    assert_ne!(a.physical(), b.physical());
    assert_eq!(IndexKeyCodec::decode(test_columns(), a.physical()).unwrap().rowid().get(), 1);
    assert!(encode_text_of_complete_key_size(MAX_INDEX_KEY_BYTES).is_ok());
    assert_eq!(encode_text_of_complete_key_size(MAX_INDEX_KEY_BYTES + 1).unwrap_err().code(), ErrorCode::ResourceLimit);
}
```

Decoder checks exact component count, NULL flag, fixed-width payload size, escape terminators, canonical Decimal, finite Float64, nonzero ROWID, and strict EOF. Add every-byte truncation loops for each scalar and composite key.

- [ ] **Step 6: Add deterministic property tests without a new dependency**

```rust
#[test]
fn fixed_seed_encodings_preserve_comparison_and_round_trip() {
    let mut rng = DeterministicRng::new(0x4c2a_2026_0918);
    for _ in 0..20_000 {
        let (column, left, right) = rng.comparable_values();
        let expected = sql_total_cmp(&left, &right);
        let encoded = (encode_for(&column, &left), encode_for(&column, &right));
        assert_eq!(encoded.0.cmp(&encoded.1), expected);
        assert_eq!(decode_for(&column, &encoded.0).unwrap(), left.canonical_for_index());
    }
}
```

- [ ] **Step 7: Run codec tests, formatting, and MSRV check**

Run: `rustup run stable cargo test storage::index::key:: --lib`

Run: `rustup run stable cargo fmt --check`

Run: `rustup run 1.85.0 cargo check --all-targets`

Expected: all pass and no dependency changes appear in `Cargo.toml`.

- [ ] **Step 8: Commit Task 4**

```bash
git add src/storage/index src/storage/mod.rs
git commit -m "feat: add canonical index key encoding"
```

---

### Task 5: IndexStorage and Unique-Prefix Enforcement

**Files:**
- Modify: `src/storage/index/mod.rs`
- Modify: `src/storage/btree/mod.rs`
- Test: inline tests in `src/storage/index/mod.rs`

**Interfaces:**
- Consumes: Tasks 3–4 catalog and key APIs, existing `BTree`, `BTreeScanState`, `TreeKind::Index`, and `WriteBatch`.
- Produces: `IndexSchema`, `PreparedIndexEntry`, and `IndexStorage` used by index DDL and `ConstrainedTableWrite`.

- [ ] **Step 1: Add failing ordinary, NULL-unique, and conflict tests**

```rust
#[test]
fn index_storage_records_every_row_and_enforces_only_complete_unique_keys() {
    let mut fixture = writable_index_fixture(true, &[ColumnType::Int64, ColumnType::Text]);
    fixture.insert(&[Value::Int64(1), Value::Text("A".into())], 1).unwrap();
    assert_eq!(fixture.insert(&[Value::Int64(1), Value::Text("A".into())], 2).unwrap_err().code(), ErrorCode::Constraint);
    fixture.insert(&[Value::Int64(1), Value::Null], 3).unwrap();
    fixture.insert(&[Value::Int64(1), Value::Null], 4).unwrap();
    assert_eq!(fixture.rowids(), vec![1, 3, 4]);
}

#[test]
fn update_unique_probe_excludes_only_its_own_rowid() {
    let mut fixture = writable_index_fixture(true, &[ColumnType::Int64]);
    fixture.insert(&[Value::Int64(7)], 1).unwrap();
    assert!(fixture.probe(&[Value::Int64(7)], Some(RowId::new(1).unwrap())).is_ok());
    assert_eq!(fixture.probe(&[Value::Int64(7)], Some(RowId::new(2).unwrap())).unwrap_err().code(), ErrorCode::Constraint);
}
```

The first test's helper must inspect physical entries directly so the attempted duplicate is absent while both NULL-bearing entries are present.

- [ ] **Step 2: Run focused tests and confirm the red state**

Run: `rustup run stable cargo test index_storage_records_every_row_and_enforces_only_complete_unique_keys -- --exact`

Expected: compilation fails because `IndexStorage` does not exist.

- [ ] **Step 3: Define the storage facade without retaining a batch borrow**

```rust
#[derive(Clone)]
pub(crate) struct IndexSchema {
    definition: IndexDef,
    columns: Box<[ColumnDef]>,
}

pub(crate) struct PreparedIndexEntry {
    physical: Vec<u8>,
    logical_len: usize,
    contains_null: bool,
    rowid: RowId,
}

pub(crate) struct IndexStorage {
    schema: IndexSchema,
    tree: BTree,
    original_root: PageRef,
}

impl IndexStorage {
    pub(crate) fn open(catalog: &CatalogReader, index: IndexId) -> Result<Self>;
    pub(crate) fn from_new(schema: IndexSchema, tree: BTree) -> Self;
    pub(crate) fn prepare(&self, values: &[Value], rowid: RowId) -> Result<PreparedIndexEntry>;
    pub(crate) fn check_unique(&self, batch: &mut WriteBatch, entry: &PreparedIndexEntry, exclude: Option<RowId>) -> Result<()>;
    pub(crate) fn insert(&mut self, batch: &mut WriteBatch, entry: &PreparedIndexEntry) -> Result<()>;
    pub(crate) fn delete(&mut self, batch: &mut WriteBatch, entry: &PreparedIndexEntry) -> Result<()>;
    pub(crate) const fn root(&self) -> PageRef;
}
```

`prepare` selects indexed values by stable ColumnId/ordinal, invokes `IndexKeyCodec`, and maps key length overflow to `ResourceLimit` before mutation. Index leaf values are `&[]`.

- [ ] **Step 4: Implement the bounded uniqueness probe and exact deletion**

```rust
pub(crate) fn check_unique(
    &self,
    batch: &mut WriteBatch,
    entry: &PreparedIndexEntry,
    exclude: Option<RowId>,
) -> Result<()> {
    if !self.schema.definition.unique() || entry.contains_null { return Ok(()); }
    let logical = &entry.physical[..entry.logical_len];
    let (start, end) = IndexKeyCodec::prefix_bounds(logical);
    let mut scan = self.tree.scan_state(start, end)?;
    while let Some((physical, value)) = scan.next(batch)? {
        if !value.is_empty() { return Err(corruption("index leaf value must be empty")); }
        let found = IndexKeyCodec::decode(&self.schema.columns, &physical)?.rowid();
        if Some(found) != exclude {
            return Err(DbError::new(ErrorCode::Constraint, format!("unique index {} is violated", self.schema.definition.name().as_str())));
        }
    }
    Ok(())
}
```

`delete` must require exactly the expected physical key and an empty value; a missing key or nonempty value is `Corruption`. `insert` must translate a duplicate physical key into `Corruption`, because ROWID makes a valid physical key unique and logical uniqueness is checked separately.

- [ ] **Step 5: Add root split/collapse and checkpoint-restore tests**

```rust
#[test]
fn index_storage_root_changes_are_restorable_and_track_index_kind() {
    let mut fixture = tiny_page_index_fixture();
    let before = fixture.storage.root();
    let checkpoint = fixture.batch.statement_checkpoint().unwrap();
    for rowid in 1..=200 { fixture.insert_int(rowid as i64, rowid).unwrap(); }
    assert_ne!(fixture.storage.root(), before);
    fixture.batch.restore_statement(checkpoint);
    assert_eq!(fixture.reopen_storage().root(), before);
    fixture.deep_validate().unwrap();
}
```

- [ ] **Step 6: Run index, B+Tree, and pager tests**

Run: `rustup run stable cargo test storage::index:: --lib`

Run: `rustup run stable cargo test storage::btree:: --lib`

Run: `rustup run stable cargo test storage::pager:: --lib`

Expected: all pass with both ordinary and unique index fixtures.

- [ ] **Step 7: Commit Task 5**

```bash
git add src/storage/index src/storage/btree/mod.rs
git commit -m "feat: add durable index storage facade"
```

---

### Task 6: Executable CREATE INDEX and DROP INDEX Plans

**Files:**
- Modify: `src/sql/bind/statement.rs`
- Modify: `src/sql/bound/statement.rs`
- Modify: `src/sql/plan/mutation.rs`
- Modify: `src/sql/plan/mod.rs`
- Modify: `src/api/connection.rs`
- Test: inline binder, bound-statement, plan, and connection tests

**Interfaces:**
- Consumes: `CatalogReader::find_index_by_name`, current `BoundStatementKind::{CreateIndex,DropIndex}`, table dependencies, and retained SQL locations.
- Produces: `CreateIndexPlan`, `DropIndexPlan`, `PlanKind::{CreateIndex,DropIndex}` with stable IDs and no `PageRef`.

- [ ] **Step 1: Add failing bind and immutable-plan tests**

```rust
#[test]
fn drop_index_binds_stable_index_and_owner_schema_identity() {
    let fixture = catalog_with_explicit_index("IX_USERS_ID");
    let bound = bind_fixture_sql(&fixture, "DROP INDEX IX_USERS_ID").unwrap();
    let BoundStatementKind::DropIndex { index_id, table } = bound.kind else { panic!("wrong kind") };
    assert_eq!(index_id, fixture.index_id());
    assert_eq!(table.table_id, fixture.users_id());
    assert_eq!(bound.dependencies.len(), 1);
}

#[test]
fn index_plans_retain_logical_ids_but_never_roots() {
    let create = plan_fixture_sql("CREATE UNIQUE INDEX UX_USERS_ID ON USERS (ID)").unwrap();
    let PlanKind::CreateIndex(plan) = create.kind() else { panic!("wrong kind") };
    assert!(plan.unique());
    assert_eq!(plan.columns()[0].column_id(), fixture_id_column());
    let drop = plan_fixture_sql("DROP INDEX IX_USERS_ID").unwrap();
    assert!(matches!(drop.kind(), PlanKind::DropIndex(_)));
}
```

- [ ] **Step 2: Run binder and plan tests and confirm the red state**

Run: `rustup run stable cargo test drop_index_binds_stable_index_and_owner_schema_identity -- --exact`

Run: `rustup run stable cargo test index_plans_retain_logical_ids_but_never_roots -- --exact`

Expected: DROP INDEX returns Bind and both index statements still plan as deferred.

- [ ] **Step 3: Bind DROP INDEX to its owning table**

```rust
DropIndex {
    index_id: IndexId,
    table: BoundTable,
}
```

Resolve the name through `find_index_by_name`, obtain the owning TableDef, create `BoundTable { table_id, schema_version, span }`, and add its exact database UUID/schema dependency. Update bound-schema validation to require the same IndexId still exists, has the same owner table, and the owner table's SchemaVersion still matches.

- [ ] **Step 4: Add immutable plan types and variants**

```rust
pub(crate) struct CreateIndexPlan {
    name: CatalogName,
    table: TableTarget,
    columns: Box<[ColumnTarget]>,
    unique: bool,
}

pub(crate) struct DropIndexPlan {
    index_id: IndexId,
    table: TableTarget,
}

pub(crate) enum PlanKind {
    // retain existing variants
    CreateIndex(CreateIndexPlan),
    DropIndex(DropIndexPlan),
}
```

Compile from the existing bound variants with `Locations`; retain the prepared name, table ID/version, ordered stable ColumnIds/ordinals/types, unique flag, and SQL locations. Do not retain an index root or reopen by name during execution.

- [ ] **Step 5: Update connection dispatch and stale-plan tests**

```rust
assert!(matches!(PlanKind::CreateIndex(create_plan()).is_write_or_query(), true));
assert!(matches!(PlanKind::DropIndex(drop_plan()).is_write_or_query(), true));
```

Route both variants through auto-commit and explicit SQL-write execution. Keep constrained CREATE TABLE deferred. Add tests showing an index DDL change invalidates a prepared statement on that table with `SchemaChanged`, while DML root changes do not.

- [ ] **Step 6: Run the SQL front-end and plan suites**

Run: `rustup run stable cargo test sql::bind:: --lib`

Run: `rustup run stable cargo test sql::plan:: --lib`

Run: `rustup run stable cargo test api::connection:: --lib`

Expected: all pass; no constraint plan becomes executable.

- [ ] **Step 7: Commit Task 6**

```bash
git add src/sql/bind/statement.rs src/sql/bound/statement.rs src/sql/plan src/api/connection.rs
git commit -m "feat: plan explicit index ddl"
```

---

### Task 7: Transactional CREATE INDEX Lifecycle

**Files:**
- Modify: `src/storage/index/mod.rs`
- Modify: `src/sql/exec/mutation.rs`
- Test: inline storage and executor tests
- Test: `tests/phase4c2_index_api.rs`

**Interfaces:**
- Consumes: Tasks 3, 5, and 6; `RelationScan`; outer `StatementCheckpoint`; `CatalogWriter`.
- Produces: `create_index` and executable `PlanKind::CreateIndex` with streaming backfill and atomic feature publication.

- [ ] **Step 1: Add failing public CREATE INDEX tests**

```rust
#[test]
fn create_index_backfills_commits_reopens_and_invalidates_old_table_plans() {
    let (_files, database) = support::new_database("phase4c2-create-index");
    let mut connection = database.connect().unwrap();
    support::execute(&mut connection, "CREATE TABLE T (A INT64, B TEXT)");
    support::execute(&mut connection, "INSERT INTO T VALUES (2, 'B')");
    support::execute(&mut connection, "INSERT INTO T VALUES (1, 'A')");
    let stale = connection.prepare("SELECT A FROM T").unwrap();
    support::execute(&mut connection, "CREATE INDEX IX_T_A ON T (A)");
    assert_eq!(connection.query(&stale, Parameters::none()).unwrap_err().code(), ErrorCode::SchemaChanged);
    drop(connection);
    let mut reopened = support::reopen_database(&_files).connect().unwrap();
    assert_eq!(support::query_i64(&mut reopened, "SELECT A FROM T ORDER BY A"), vec![1, 2]);
}

#[test]
fn unique_index_build_failure_restores_every_visible_state() {
    let mut fixture = duplicate_rows_fixture();
    let before = fixture.snapshot_state();
    let error = fixture.execute("CREATE UNIQUE INDEX UX_T_A ON T (A)").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert_eq!(fixture.snapshot_state(), before);
    fixture.execute("INSERT INTO T VALUES (9)").unwrap();
}
```

- [ ] **Step 2: Run the CREATE tests and confirm the red state**

Run: `rustup run stable cargo test --test phase4c2_index_api create_index_backfills_commits_reopens_and_invalidates_old_table_plans -- --exact`

Expected: execution returns `Unsupported` because the plan has no executor arm.

- [ ] **Step 3: Implement the storage lifecycle in the approved order**

```rust
pub(crate) struct IndexSpec {
    pub(crate) name: CatalogName,
    pub(crate) table_id: TableId,
    pub(crate) columns: Box<[ColumnId]>,
    pub(crate) unique: bool,
}

impl IndexSpec {
    pub(crate) fn new(name: CatalogName, table_id: TableId, columns: Vec<ColumnId>, unique: bool) -> Result<Self>;
}

pub(crate) fn create_index(batch: &mut WriteBatch, spec: &IndexSpec) -> Result<IndexId>;
```

Before opening `CatalogWriter`, revalidate database/table schema/name/column ownership from the current batch catalog and clone the `Relation`. Then establish a nested statement checkpoint, allocate IndexId, create an empty Index B+Tree, construct explicit `IndexDef`, stream `RelationScan::next_batch`, prepare/probe/insert each entry, insert all catalog records, bump table schema version, enable `INDEX_CONSTRAINT_V1`, finish `CatalogWriter`, and finish the checkpoint. On any error restore the checkpoint, writer state, tree root state, object counter, page allocations, catalog gate, feature bits, and schema version.

- [ ] **Step 4: Execute `CreateIndexPlan` without exposing storage details**

```rust
PlanKind::CreateIndex(plan) => {
    let spec = IndexSpec::new(
        plan.name().clone(), plan.table().table_id(),
        plan.columns().iter().map(ColumnTarget::column_id).collect(), plan.unique(),
    )?;
    create_index(batch, &spec)?;
    Ok(ExecutionResult::new(0))
}
```

Immediately before mutation, recheck the created-name dependency and table version. Map duplicate complete logical keys to `Constraint` containing the stable index name; map a physical key over 1024 bytes to `ResourceLimit`; preserve Type/Bind/SchemaChanged classifications for pre-mutation failures.

- [ ] **Step 5: Add feature-bit, NULL, composite, and resource rollback tests**

```rust
#[test]
fn first_successful_index_enables_bit_but_failed_build_does_not() {
    let mut fixture = legacy_format2_table_fixture();
    assert_eq!(fixture.feature_bits(), CATALOG_V1);
    assert_eq!(fixture.execute("CREATE UNIQUE INDEX UX_T_A ON T (A)").unwrap_err().code(), ErrorCode::Constraint);
    assert_eq!(fixture.feature_bits(), CATALOG_V1);
    fixture.execute("CREATE INDEX IX_T_A ON T (A)").unwrap();
    assert_eq!(fixture.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
}
```

Also prove: composite column order is persisted; all-NULL and partially-NULL rows are present; unique build accepts repeated NULL keys; exact 1024-byte complete keys pass; 1025-byte keys fail before catalog/table/index mutation; read-only prepare/execute changes neither feature bits nor file lengths.

- [ ] **Step 6: Run CREATE INDEX storage, SQL, transaction, and reopen tests**

Run: `rustup run stable cargo test create_index --lib`

Run: `rustup run stable cargo test --test phase4c2_index_api create_index -- --nocapture`

Expected: all pass, including auto-commit and explicit-transaction rollback/commit.

- [ ] **Step 7: Commit Task 7**

```bash
git add src/storage/index/mod.rs src/sql/exec/mutation.rs tests/phase4c2_index_api.rs
git commit -m "feat: create and backfill explicit indexes"
```

---

### Task 8: DROP INDEX and Indexed DROP TABLE Lifecycle

**Files:**
- Modify: `src/storage/index/mod.rs`
- Modify: `src/storage/relation.rs`
- Modify: `src/sql/exec/mutation.rs`
- Test: inline storage/executor tests
- Test: `tests/phase4c2_index_api.rs`

**Interfaces:**
- Consumes: stable `DropIndexPlan`, catalog removal/schema bump, `release_tree_later`, and existing `drop_relation`.
- Produces: `drop_index` and table drop that atomically frees all owned explicit indexes while preserving the feature bit.

- [ ] **Step 1: Add failing DROP INDEX and DROP TABLE tests**

```rust
#[test]
fn drop_index_frees_tree_bumps_schema_and_never_clears_feature_bit() {
    let mut fixture = committed_index_fixture();
    let stale = fixture.prepare("SELECT A FROM T");
    let index_pages = fixture.index_pages();
    fixture.execute("DROP INDEX IX_T_A").unwrap();
    assert_eq!(fixture.execute_prepared(&stale).unwrap_err().code(), ErrorCode::SchemaChanged);
    assert!(fixture.freelist_pages().is_superset(&index_pages));
    assert_eq!(fixture.feature_bits(), CATALOG_V1 | INDEX_CONSTRAINT_V1);
}

#[test]
fn drop_table_removes_all_owned_explicit_indexes_atomically() {
    let mut fixture = table_with_two_indexes();
    fixture.execute("DROP TABLE T").unwrap();
    assert!(fixture.catalog().find_table_by_name(&name("T")).is_none());
    assert!(fixture.catalog().find_index_by_name(&name("IX_T_A")).is_none());
    assert!(fixture.catalog().find_index_by_name(&name("IX_T_B")).is_none());
    fixture.deep_validate().unwrap();
}
```

- [ ] **Step 2: Run DROP tests and confirm the red state**

Run: `rustup run stable cargo test --test phase4c2_index_api drop_index_frees_tree_bumps_schema_and_never_clears_feature_bit -- --exact`

Expected: DROP INDEX has no executor arm and DROP TABLE leaves index records/pages reachable.

- [ ] **Step 3: Implement explicit DROP INDEX with ownership enforcement**

```rust
pub(crate) fn drop_index(batch: &mut WriteBatch, index_id: IndexId) -> Result<()> {
    let checkpoint = batch.statement_checkpoint()?;
    let result = drop_index_inner(batch, index_id);
    match result {
        Ok(()) => batch.finish_statement(checkpoint),
        Err(error) => { batch.restore_statement(checkpoint); Err(error) }
    }
}
```

`drop_index_inner` reopens by IndexId, verifies owner table/version and `IndexOwnership::Explicit`, deep-validates before release, removes all four index record families, stages the whole `TreeKind::Index` for release, bumps the table schema version, and finishes the catalog. A future `Constraint(_)` owner returns `Constraint` with no modification. Never disable `INDEX_CONSTRAINT_V1`.

- [ ] **Step 4: Extend DROP TABLE to indexes without adding FK behavior**

Before any release, load and deep-validate the relation plus every owned explicit index. Delete IndexColumn, TableIndex, IndexName, and IndexDefinition records, release every index tree, then remove table columns/table records and release the table tree. Phase 4C2-A has no ReferencedBy records; Task 1's reserved families remain unmodified.

- [ ] **Step 5: Add rollback, stale plan, and reopen assertions**

```rust
#[test]
fn failed_drop_restores_catalog_pages_and_prepared_statement_usability() {
    let mut fixture = drop_fault_fixture();
    let before = fixture.snapshot_state();
    fixture.fail_next_release(ErrorCode::Io);
    assert_eq!(fixture.execute("DROP INDEX IX_T_A").unwrap_err().code(), ErrorCode::Io);
    assert_eq!(fixture.reopen_state(), before);
}
```

Also test missing/stale IndexId returns `SchemaChanged` before mutation; explicit transaction can continue after an ordinary `Constraint`; fatal I/O makes it rollback-only; dropping the last index followed by reopen retains the feature bit and an empty valid 4C2 index catalog.

- [ ] **Step 6: Run DROP, freelist, catalog, and public API tests**

Run: `rustup run stable cargo test drop_index --lib`

Run: `rustup run stable cargo test drop_relation --lib`

Run: `rustup run stable cargo test --test phase4c2_index_api drop_ -- --nocapture`

Expected: all pass and deep validation reports no live/free overlap.

- [ ] **Step 7: Commit Task 8**

```bash
git add src/storage/index/mod.rs src/storage/relation.rs src/sql/exec/mutation.rs tests/phase4c2_index_api.rs
git commit -m "feat: drop indexes with owning relations"
```

---

### Task 9: Atomic Explicit-Index Maintenance for INSERT, UPDATE, and DELETE

**Files:**
- Modify: `src/storage/relation.rs`
- Modify: `src/storage/index/mod.rs`
- Modify: `src/sql/exec/mutation.rs`
- Test: inline relation/index/executor tests
- Test: `tests/phase4c2_index_api.rs`

**Interfaces:**
- Consumes: `Relation`, `IndexStorage`, `TableWrite` behavior, and existing SQL DML row evaluation.
- Produces: `ConstrainedTableWrite`; SQL execution no longer directly opens the lower-level `TableWrite`.

- [ ] **Step 1: Add failing atomic-maintenance tests**

```rust
#[test]
fn every_dml_keeps_one_index_entry_per_visible_row() {
    let mut fixture = two_index_table_fixture();
    fixture.execute("INSERT INTO T VALUES (1, 'A')").unwrap();
    fixture.execute("UPDATE T SET A = 2, B = 'B' WHERE A = 1").unwrap();
    fixture.execute("DELETE FROM T WHERE A = 2").unwrap();
    fixture.assert_each_visible_row_has_exactly_one_entry_per_index();
    fixture.assert_no_entry_references_a_missing_row();
}

#[test]
fn late_multirow_unique_failure_restores_table_and_all_indexes() {
    let mut fixture = rows_1_2_with_unique_index();
    let before = fixture.snapshot_state();
    let error = fixture.execute("UPDATE T SET A = 3").unwrap_err();
    assert_eq!(error.code(), ErrorCode::Constraint);
    assert_eq!(fixture.snapshot_state(), before);
    fixture.assert_catalog_and_trees_deep_valid();
}
```

- [ ] **Step 2: Run focused DML tests and confirm the red state**

Run: `rustup run stable cargo test --test phase4c2_index_api every_dml_keeps_one_index_entry_per_visible_row -- --exact`

Expected: table rows change but index entries remain at their CREATE-time state.

- [ ] **Step 3: Introduce the single SQL mutation boundary**

```rust
pub(crate) struct ConstrainedTableWrite {
    schema: Relation,
    rows: BTree,
    indexes: Box<[IndexStorage]>,
    catalog: CatalogWriter,
    original_table_root: PageRef,
}

impl ConstrainedTableWrite {
    pub(crate) fn open(batch: &mut WriteBatch, table: TableId) -> Result<Self>;
    pub(crate) fn next_after(&mut self, batch: &mut WriteBatch, rowid: Option<RowId>, max_row_bytes: usize) -> Result<Option<StoredRow>>;
    pub(crate) fn insert(&mut self, batch: &mut WriteBatch, values: &[Value]) -> Result<RowId>;
    pub(crate) fn replace(&mut self, batch: &mut WriteBatch, rowid: RowId, old: &[Value], new: &[Value]) -> Result<()>;
    pub(crate) fn delete(&mut self, batch: &mut WriteBatch, rowid: RowId, old: &[Value]) -> Result<bool>;
    pub(crate) fn finish(self, batch: &mut WriteBatch) -> Result<()>;
}
```

Open one validated catalog image, one table tree, all explicit index trees in IndexId order, and one `CatalogWriter`. Keep the lower-level `TableWrite` crate-private for focused storage fixtures only; production SQL DML imports only `ConstrainedTableWrite`.

- [ ] **Step 4: Precompute and precheck before each mutation**

For INSERT, inspect the current candidate `next_rowid`, RowCodec-encode the full row, prepare every index entry, and run every unique probe before allocating ROWID or touching a tree. For UPDATE, prepare all old and new entries for the known ROWID and run all unique probes with `exclude = Some(rowid)` before deletion. For DELETE, prepare every old entry before deletion.

```rust
let prepared = self.indexes.iter()
    .map(|index| index.prepare(values, rowid))
    .collect::<Result<Vec<_>>>()?;
for (index, entry) in self.indexes.iter().zip(&prepared) {
    index.check_unique(batch, entry, exclude)?;
}
```

- [ ] **Step 5: Mutate table and indexes inside a restorable nested scope**

```rust
let scope = batch.begin_mutation_scope()?;
let checkpoint = self.checkpoint();
let result = self.replace_inner(batch, rowid, old_entries, new_entries, encoded_row);
match result {
    Ok(value) => { batch.finish_mutation_scope(scope)?; Ok(value) }
    Err(error) => {
        batch.rollback_mutation_scope(scope)?;
        self.restore(checkpoint);
        Err(error)
    }
}
```

INSERT mutates table then each index; UPDATE deletes every old index entry, replaces the table row, then inserts every new entry; DELETE deletes every index entry before the table row. The nested scope and in-memory root checkpoint make the order unobservable on error. `finish` updates changed table and index roots in the one CatalogWriter and performs candidate validation once.

- [ ] **Step 6: Route all SQL DML through the facade**

Change `execute_insert`, `execute_update`, and `execute_delete` to use `ConstrainedTableWrite`. UPDATE must pass the same saved old row used for expression evaluation; DELETE must pass the decoded old row. Preserve the existing ROWID `next_after` progression and WorkMemory accounting, so neither UPDATE nor DELETE target discovery uses an index.

- [ ] **Step 7: Add NULL, key-size, split, and explicit-transaction rollback tests**

```rust
#[test]
fn dml_prevalidation_failure_consumes_no_rowid_and_changes_no_root() {
    let mut fixture = indexed_table_fixture();
    let before = fixture.catalog_roots_and_next_rowid();
    assert_eq!(fixture.execute_oversized_index_key().unwrap_err().code(), ErrorCode::ResourceLimit);
    assert_eq!(fixture.catalog_roots_and_next_rowid(), before);
    assert_eq!(fixture.execute("INSERT INTO T VALUES (1, 'ok')").unwrap().rows_affected(), 1);
    assert_eq!(fixture.only_rowid(), 1);
}
```

Also test: unique NULL duplicates; composite uniqueness; UPDATE self-key succeeds; UPDATE conflicts with another ROWID; multi-index root splits; DELETE removes exact old entries; a later-row error restores earlier-row changes; an ordinary violation leaves an explicit transaction usable; DML does not change SchemaVersion.

- [ ] **Step 8: Run relation, DML, public API, and MSRV tests**

Run: `rustup run stable cargo test storage::relation:: --lib`

Run: `rustup run stable cargo test sql::exec::mutation:: --lib`

Run: `rustup run stable cargo test --test phase4c2_index_api dml_ -- --nocapture`

Run: `rustup run 1.85.0 cargo test --test phase4c2_index_api`

Expected: all pass; query output remains table-scan based.

- [ ] **Step 9: Commit Task 9**

```bash
git add src/storage/relation.rs src/storage/index/mod.rs src/sql/exec/mutation.rs tests/phase4c2_index_api.rs
git commit -m "feat: maintain indexes with table mutations"
```

---

### Task 10: Snapshot, Real-File Compatibility, and Deep-Corruption Coverage

**Files:**
- Modify: `src/storage/test_support.rs`
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/validate.rs`
- Test: inline storage tests
- Test: `tests/phase4c2_index_api.rs`

**Interfaces:**
- Consumes: committed index catalog/tree support from Tasks 3–9 and existing real-file fixture utilities.
- Produces: acceptance evidence for lazy feature enablement, old/new snapshots, read-only behavior, and structural corruption detection.

- [ ] **Step 1: Add failing snapshot and format-2 real-file tests**

```rust
#[test]
fn old_snapshot_keeps_old_catalog_and_roots_across_index_ddl_and_dml() {
    let fixture = snapshot_index_fixture();
    let mut old = fixture.pager().begin_read().unwrap();
    fixture.commit_create_and_update_index();
    assert!(old.catalog_reader().unwrap().find_index_by_name(&name("IX_T_A")).is_none());
    let mut fresh = fixture.pager().begin_read().unwrap();
    assert!(fresh.catalog_reader().unwrap().find_index_by_name(&name("IX_T_A")).is_some());
    assert_eq!(fixture.read_old_rows(&mut old), vec![1]);
    assert_eq!(fixture.read_fresh_rows(&mut fresh), vec![2]);
}

#[test]
fn format2_without_4c2_opens_without_rewrite_and_read_only_never_upgrades() {
    let fixture = phase4c1_main_only_fixture();
    let before = fixture.file_bytes();
    fixture.open_read_only_and_catalog().unwrap();
    assert_eq!(fixture.file_bytes(), before);
}
```

- [ ] **Step 2: Run the snapshot/compatibility tests and confirm the red state**

Run: `rustup run stable cargo test old_snapshot_keeps_old_catalog_and_roots_across_index_ddl_and_dml -- --exact`

Expected: the new fixture exposes mixed or unvalidated index state until snapshot/catalog validation is complete.

- [ ] **Step 3: Add exact compatibility fixtures**

Keep checked-in or generated real files for: format 2 with no catalog; format 2 with `CATALOG_V1` but no 4C2 bit; format 2 with both bits and no index records; format 2 with one committed index; 4C2 records with the bit cleared; bit 1 set while bit 0 is clear; and an unknown high bit. Compare file length and bytes before/after read-only open. Assert `Corruption` for the two known-bit consistency violations and `Unsupported` for the unknown high bit.

- [ ] **Step 4: Add index-structure corruption matrix**

```rust
for corruption in [
    CorruptionCase::WrongTreeKind,
    CorruptionCase::RootGeneration,
    CorruptionCase::MissingNameBacklink,
    CorruptionCase::MissingTableBacklink,
    CorruptionCase::ColumnGap,
    CorruptionCase::ForeignColumn,
    CorruptionCase::MalformedKey,
    CorruptionCase::NonEmptyLeafValue,
    CorruptionCase::MissingRowid,
    CorruptionCase::LiveTreeOverlap,
    CorruptionCase::FreelistOverlap,
] {
    assert_eq!(open_and_deep_validate(corruption).unwrap_err().code(), ErrorCode::Corruption);
}
```

Catalog open checks record graph and root identity. Explicit deep validation decodes every physical index key using its column definitions, requires empty leaf values and strict ordering, and verifies all catalog/table/index/freelist page sets are disjoint. Do not compare every table row with index entries during normal open.

- [ ] **Step 5: Run storage compatibility and corruption tests**

Run: `rustup run stable cargo test phase_4c2_format2 --lib`

Run: `rustup run stable cargo test phase_4c2_index_corruption --lib`

Run: `rustup run stable cargo test --test phase4c2_index_api snapshot_ -- --nocapture`

Expected: all pass, and ordinary open remains bounded to catalog/root validation.

- [ ] **Step 6: Commit Task 10**

```bash
git add src/storage/test_support.rs src/storage/catalog/mod.rs src/storage/validate.rs tests/phase4c2_index_api.rs
git commit -m "test: cover index snapshots and format compatibility"
```

---

### Task 11: Fixed-Seed Index Model and Crash-Recovery Matrix

**Files:**
- Modify: `src/sql/exec/test_support.rs`
- Modify: `src/storage/test_support.rs`
- Test: inline ignored model/recovery tests

**Interfaces:**
- Consumes: existing Phase 4C1 reference model, fault injection points, child-process crash harness, and new index introspection fixtures.
- Produces: deterministic logical-consistency and WAL atomicity gates for CI in Task 12.

- [ ] **Step 1: Add failing fixed-seed table/index reference model**

```rust
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
        if step % 251 == 0 { model.reopen_and_recheck(); }
    }
}
```

Generate CREATE ordinary/unique/composite index, DROP INDEX, INSERT, UPDATE, DELETE, BEGIN/COMMIT/ROLLBACK, NULLs, duplicate complete keys, key-limit boundaries, and forced table/index root splits. Compare exact rows, physical logical-key/ROWID pairs, rows affected, error code, schema version, feature bits, and transaction usability.

- [ ] **Step 2: Run the model and confirm the red state**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2a_fixed_seed_index_model -- --exact --ignored --nocapture`

Expected: the new model catches at least one missing index-maintenance or recovery assertion before the support implementation is complete.

- [ ] **Step 3: Add CREATE/DML/DROP crash scenarios to the child harness**

```rust
enum Phase4c2aCrashScenario {
    CreateOrdinary,
    CreateUnique,
    InsertTwoIndexes,
    UpdateSplitTableAndIndexRoots,
    DeleteAndCollapseIndexRoot,
    DropIndex,
    DropTableWithIndexes,
}
```

For every scenario capture a complete pre-state and post-state containing feature bits, object counter, catalog records, table roots/rows, index roots/entries, freelist, main-file length, and WAL length. Abort at every observed occurrence of catalog page frame, table page frame, index page frame, Commit record write, WAL sync, memory publication, and catalog/main sync fault points.

- [ ] **Step 4: Require old-or-new atomic recovery and stable repeat open**

```rust
#[test]
#[ignore = "run by the dedicated Phase 4C2-A CI job"]
fn phase_4c2a_index_crash_recovery_matrix() {
    for outcome in every_index_crash_outcome() {
        let recovered = recover_and_capture(&outcome.path);
        assert!(recovered == outcome.pre || recovered == outcome.post);
        assert_eq!(recover_and_capture(&outcome.path), recovered);
        assert_deep_valid(&outcome.path);
        assert_file_handles_released(&outcome.path);
    }
}
```

Use the production recovery acceptance scan to decide whether a complete Commit exists. Do not infer durability from child exit status or the injected-fault position. A corrupt committed 4C2 candidate must return `Corruption`; recovery must not silently publish an older complete commit to hide it.

- [ ] **Step 5: Run the model and crash matrix locally**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2a_fixed_seed_index_model -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2a_index_crash_recovery_matrix -- --exact --ignored --nocapture`

Expected: both pass; every named fault family has at least one observed occurrence and every recovered state is exactly old or new.

- [ ] **Step 6: Commit Task 11**

```bash
git add src/sql/exec/test_support.rs src/storage/test_support.rs
git commit -m "test: model and recover indexed mutations"
```

---

### Task 12: 100,000-Row Acceptance, CI, Documentation, and Final Gates

**Files:**
- Create: `tests/phase4c2_index_scale.rs`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`
- Test: full repository

**Interfaces:**
- Consumes: the complete Phase 4C2-A implementation and Task 11 ignored tests.
- Produces: repeatable Linux CI acceptance, Windows/MSRV coverage through existing jobs, and an evidence-bounded Phase 4C2-A handoff.

- [ ] **Step 1: Add the ignored 100,000-row acceptance test**

```rust
#[test]
#[ignore = "run by the dedicated Phase 4C2-A CI job"]
fn phase_4c2a_hundred_thousand_row_index_smoke() {
    let (_files, database) = support::new_database("phase4c2-index-scale");
    let mut connection = database.connect().unwrap();
    support::execute(&mut connection, "CREATE TABLE T (ID INT64, G INT64, PAYLOAD TEXT)");
    for id in 0..100_000_i64 { support::insert_scale_row(&mut connection, id, id % 101); }
    support::execute(&mut connection, "CREATE UNIQUE INDEX UX_T_ID ON T (ID)");
    support::execute(&mut connection, "CREATE INDEX IX_T_G_ID ON T (G, ID)");
    support::execute(&mut connection, "UPDATE T SET G = G + 1 WHERE ID >= 99_000");
    support::execute(&mut connection, "DELETE FROM T WHERE ID < 1_000");
    support::assert_index_model(&mut connection, 99_000, &["UX_T_ID", "IX_T_G_ID"]);
    drop(connection);
    support::reopen_and_assert_index_model(&_files, 99_000, &["UX_T_ID", "IX_T_G_ID"]);
}
```

This is a correctness/time-limit gate, not a shared-runner millisecond benchmark. It must verify row and physical-index counts, selected logical key/ROWID pairs, unique rejection, rollback, reopen, and deep validation.

- [ ] **Step 2: Run the scale test and confirm it fails before CI wiring**

Run: `rustup run stable cargo test --profile exhaustive --test phase4c2_index_scale phase_4c2a_hundred_thousand_row_index_smoke -- --exact --ignored --nocapture`

Expected: compilation fails until the scale support functions and test target are present; after their minimal implementation the test passes inside the existing exhaustive profile.

- [ ] **Step 3: Add the dedicated Linux Phase 4C2-A job**

```yaml
phase4c2a-index-storage-ddl:
  name: Linux Phase 4C2-A index storage and DDL
  runs-on: ubuntu-latest
  timeout-minutes: 120
  steps:
    - uses: actions/checkout@v4
    - name: Install stable Rust
      run: rustup toolchain install stable --profile minimal
    - name: Run deterministic index model
      run: rustup run stable cargo test sql::exec::test_support::phase_4c2a_fixed_seed_index_model -- --exact --ignored --nocapture
    - name: Run index crash recovery matrix
      run: rustup run stable cargo test sql::exec::test_support::phase_4c2a_index_crash_recovery_matrix -- --exact --ignored --nocapture
    - name: Run 100,000-row index smoke
      run: rustup run stable cargo test --profile exhaustive --test phase4c2_index_scale phase_4c2a_hundred_thousand_row_index_smoke -- --exact --ignored --nocapture
```

Do not remove or weaken Linux stable, Rust 1.85 MSRV, Windows, Phase 4B, Phase 4C1, exhaustive storage, WAL recovery, or catalog recovery jobs.

- [ ] **Step 4: Update README with verified scope and exclusions**

Document that explicit ordinary/unique index DDL, reopen/recovery, and DML maintenance are supported. State plainly that SELECT still uses TableScan; UPDATE/DELETE target discovery still uses ROWID TableScan; table constraints, foreign keys, and optimizer IndexScan arrive in 4C2-B/C/D; and 10-million-row/10-GB acceptance is run only after all 4C2 phases.

- [ ] **Step 5: Run formatting, lint, full stable, MSRV, and public tests**

Run: `rustup run stable cargo fmt --check`

Run: `rustup run stable cargo clippy --all-targets --all-features -- -D warnings`

Run: `rustup run stable cargo test --all-targets`

Run: `rustup run 1.85.0 cargo test --all-targets`

Expected: all commands exit 0; the normal suite still skips only explicitly ignored exhaustive/model/crash/scale tests.

- [ ] **Step 6: Run every Phase 4C2-A specialized gate**

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2a_fixed_seed_index_model -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test sql::exec::test_support::phase_4c2a_index_crash_recovery_matrix -- --exact --ignored --nocapture`

Run: `rustup run stable cargo test --profile exhaustive --test phase4c2_index_scale phase_4c2a_hundred_thousand_row_index_smoke -- --exact --ignored --nocapture`

Expected: all pass with deterministic seeds and no unobserved required fault family.

- [ ] **Step 7: Inspect scope and commit the acceptance gate**

Run: `git diff --check`

Run: `git status --short`

Expected: no whitespace errors, only Phase 4C2-A files are changed, and no constraint enforcement or IndexScan implementation is present.

```bash
git add tests/phase4c2_index_scale.rs .github/workflows/ci.yml README.md
git commit -m "ci: gate phase 4c2a index storage"
```

---

## Phase 4C2-A Completion Evidence

Before requesting review, record the exact commit IDs and command results for:

- stable formatting and Clippy;
- all stable targets;
- all Rust 1.85 targets;
- Windows CI;
- fixed-seed index model;
- index crash/recovery matrix;
- 100,000-row index smoke;
- real-file old format-2 open and read-only no-rewrite tests;
- committed-feature, catalog/index deep-validation, and unknown-feature rejection tests.

The Phase 4C2-A branch is complete only when explicit indexes can be created, maintained, dropped, reopened, recovered, and rolled back atomically, while constraints and IndexScan remain absent from executable source paths.
