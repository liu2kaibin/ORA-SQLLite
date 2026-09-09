# ORA-SQLLite Phase 4A Catalog and Row Storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Phase 3 存储引擎上实现可恢复、可快照读取的 V1 系统目录、稳定对象身份、隐藏 ROWID 和模式有序用户行编码。

**Architecture:** 单一 `TreeKind::Table` B+Tree 由现有 `catalog_root` 引导；目录键和值采用手写版本化二进制格式，表数据根以完整 `PageRef` 持久化。目录变更、用户树根、对象计数器、ROWID 计数器和 `CATALOG_V1` 特性位通过同一 `WriteBatch`/WAL 提交原子发布；行编解码保持独立且不依赖 SQL 层。

**Tech Stack:** Rust 2024 edition、Rust 1.85 MSRV、现有 Pager/B+Tree/WAL/CRC32C/故障注入基础设施、标准库；不增加第三方依赖。

**Spec:** `docs/superpowers/specs/2026-09-09-phase-4a-catalog-row-design.md`

## Global Constraints

- 所有新目录和行接口保持 `pub(crate)` 或更窄，不新增公共 SQL、DDL、DML 或 CRUD API。
- `CATALOG_V1 = 1 << 0`，数据库头和 WAL 必须引用同一份 `KNOWN_FEATURE_BITS`。
- 目录继续使用 `TreeKind::Table`；`PageType::Catalog` 保持保留，不改变 Phase 3 B+Tree 页格式。
- 稳定 ID、模式版本和 ROWID 都是非零 `u64`；已提交值不复用，`u64::MAX` 不分配、不回绕。
- 目录键不超过 1024 字节；单表列数为 `0..=1000`，列序从 0 连续递增。
- 行格式 V1 固定 32 字节行头，保留 NULL、空 Text、空 Blob 的区别，并覆盖当前全部 `Value` 变体。
- 普通打开完整扫描目录但只浅验证用户表根；专用深验证才遍历全部用户树。
- 磁盘格式错误使用 `Corruption`，未来合法版本使用 `Unsupported`，调用参数错误使用 `Type`，命名/身份冲突使用 `Constraint`，未完成目录 writer 使用 `Transaction`。
- 错误文本保留目录对象、表 ID、列序或行上下文，但不直接回显未经验证的磁盘字节。
- Phase 4A 对 `CatalogName` 只做非空、UTF-8 和物理键长校验，不执行 Oracle 大小写折叠或标识符语义。
- 所有长度和偏移使用检查运算；磁盘输入不得造成 panic、越界或部分解码结果。
- 保持 `#![forbid(unsafe_code)]`，不提交 `Cargo.lock`、数据库、WAL、临时进程或生成目录。
- 验收平台为 Windows stable、Linux stable 和 Linux Rust 1.85；Clippy 必须以 `-D warnings` 通过。

## File Structure

- Create `src/storage/features.rs`: 共享磁盘特性位及验证。
- Create `src/storage/catalog/mod.rs`: 目录镜像、读取器、写入器、交叉引用验证和深验证入口。
- Create `src/storage/catalog/types.rs`: 稳定 ID、名称、列类型、`CatalogMeta`、`TableDef`、`ColumnDef`。
- Create `src/storage/catalog/key.rs`: 目录键族、精确键、前缀范围和 ROWID 之外的键解码。
- Create `src/storage/catalog/record.rs`: 8 字节信封、映射值和定义记录的 V1 编解码。
- Create `src/storage/row.rs`: ROWID 键、标量载荷和完整用户行 V1 编解码。
- Modify `src/storage/mod.rs`: 注册新模块。
- Modify `src/storage/header.rs`: 使用共享特性位并提供受控候选特性更新。
- Modify `src/storage/wal/format.rs`: 使用共享特性位验证提交状态。
- Modify `src/storage/pager.rs`: 暴露快照特性、目录候选状态、writer 收尾门禁和恢复验证钩子。
- Modify `src/datetime.rs`: 为持久化编码增加只读字段访问器。
- Modify `src/storage/test_support.rs`: 目录/行参考模型、快照、故障和子进程恢复矩阵。
- Modify `.github/workflows/ci.yml`: 增加 Phase 4A 专用深验证与崩溃矩阵。
- Modify `README.md`: 记录 Phase 4A 已实现边界和仍未实现的 SQL/API。

---

### Task 1: Share catalog feature bits through headers, WAL, snapshots, and write batches

**Files:**
- Create: `src/storage/features.rs`
- Modify: `src/storage/mod.rs:1-14`
- Modify: `src/storage/header.rs:1-16,110-135,260-280`
- Modify: `src/storage/wal/format.rs:1-18,288-352`
- Modify: `src/storage/pager.rs:28-43,300-335,578-614,683-715,1030-1090`

**Interfaces:**
- Produces: `features::{CATALOG_V1, KNOWN_FEATURE_BITS, validate_feature_bits}`.
- Produces: `ReadSnapshot::feature_bits(&self) -> u64`.
- Produces: `WriteBatch::feature_bits(&self) -> u64` and `WriteBatch::enable_feature_bits(&mut self, bits: u64) -> Result<()>`.
- Changes: WAL `CommitState.feature_bits` is taken from the candidate header, not copied unconditionally from the committed state.

- [ ] **Step 1: Write failing shared-feature tests**

Add tests proving bit 0 round-trips through a database header and WAL commit, an unknown bit returns `Corruption`, and a committed write exposes bit 0 only to snapshots opened after the commit:

```rust
#[test]
fn catalog_v1_feature_round_trips_through_header_and_wal() {
    let mut header = DatabaseHeader::new(4096, [7; 16]).unwrap();
    header.set_features(CATALOG_V1).unwrap();
    assert_eq!(DatabaseHeader::decode_slot(&header.encode_slot()).unwrap().features(), CATALOG_V1);

    let state = CommitState {
        page_count: 2,
        catalog_root: PageId(1),
        freelist_root: PageId(0),
        feature_bits: CATALOG_V1,
    };
    let encoded = CommitPayload {
        previous_lsn: Lsn(0),
        state,
        frame_count: 0,
        transaction_crc32c: 0,
    }.encode().unwrap();
    assert_eq!(CommitPayload::decode(&encoded).unwrap().state.feature_bits, CATALOG_V1);
}
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `cargo test catalog_v1_feature_round_trips_through_header_and_wal -- --nocapture`

Expected: compilation fails because `CATALOG_V1` and `set_features` do not exist.

- [ ] **Step 3: Add the shared feature module and header validation**

Implement the shared constants and reject unknown bits through one function:

```rust
pub(super) const CATALOG_V1: u64 = 1 << 0;
pub(super) const KNOWN_FEATURE_BITS: u64 = CATALOG_V1;

pub(super) fn validate_feature_bits(bits: u64) -> Result<()> {
    if bits & !KNOWN_FEATURE_BITS != 0 {
        return Err(DbError::new(ErrorCode::Corruption, "database state has unknown feature bits"));
    }
    Ok(())
}
```

Replace the private zero-valued constants in `header.rs` and `wal/format.rs`. Add `DatabaseHeader::set_features`, which calls `validate_feature_bits` before changing the field.

- [ ] **Step 4: Carry candidate features through `WriteBatch`**

When a write begins, copy `committed_state.feature_bits` into `base_header`. Implement:

```rust
pub(super) const fn feature_bits(&self) -> u64 {
    self.current_header().features()
}

pub(super) fn enable_feature_bits(&mut self, bits: u64) -> Result<()> {
    self.require_active()?;
    let candidate = self.feature_bits() | bits;
    validate_feature_bits(candidate)?;
    let mut header = self.current_header();
    header.set_features(candidate)?;
    self.candidate_header = Some(header);
    Ok(())
}
```

Build `CommitState.feature_bits` from `roots.features()`. Include candidate feature state in mutation checkpoints through the existing `candidate_header` copy.

- [ ] **Step 5: Run focused and complete fast tests**

Run: `cargo test feature_bits -- --nocapture`

Run: `cargo test --all-targets --all-features`

Expected: all selected and non-ignored tests pass; existing zero-feature databases remain readable.

- [ ] **Step 6: Commit**

```bash
git add src/storage/features.rs src/storage/mod.rs src/storage/header.rs src/storage/wal/format.rs src/storage/pager.rs
git commit -m "feat(storage): share catalog feature state"
```

---

### Task 2: Define stable catalog identities and in-memory definitions

**Files:**
- Create: `src/storage/catalog/mod.rs`
- Create: `src/storage/catalog/types.rs`
- Modify: `src/storage/mod.rs:1-14`

**Interfaces:**
- Produces: `ObjectId`, `TableId`, `ColumnId`, `IndexId`, `ConstraintId`, `SchemaVersion`, `RowId`.
- Produces: `CatalogName`, `ColumnType`, `CatalogMeta`, `TableDef`, `ColumnDef`.
- Produces: explicit typed-object conversion methods; no implicit conversion between unrelated ID types.

- [ ] **Step 1: Write failing type-boundary tests**

Cover zero rejection, stable getters, explicit object conversion, the 1013-byte conservative name limit, all nine column type codes, and 1000-column boundaries:

```rust
#[test]
fn stable_ids_reject_zero_and_require_explicit_kinds() {
    assert_eq!(TableId::new(0).unwrap_err().code(), ErrorCode::Type);
    let table = TableId::new(9).unwrap();
    let object: ObjectId = table.into();
    assert_eq!(TableId::from_object(object, ObjectKind::Table).unwrap(), table);
    assert_eq!(ColumnId::from_object(object, ObjectKind::Table).unwrap_err().code(), ErrorCode::Type);
}
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `cargo test storage::catalog::types::tests::stable_ids_reject_zero_and_require_explicit_kinds -- --exact`

Expected: compilation fails because the catalog types module is absent.

- [ ] **Step 3: Implement nonzero wrappers and explicit conversions**

Use `NonZeroU64` internally and expose only checked construction and `get()`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TableId(NonZeroU64);

impl TableId {
    pub(crate) fn new(value: u64) -> Result<Self> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or_else(|| DbError::new(ErrorCode::Type, "table id must be nonzero"))
    }

    pub(crate) const fn get(self) -> u64 { self.0.get() }
}
```

Apply the same checked pattern with type-specific messages. Define `ObjectKind::{Table, Column, Index, Constraint}` and require the matching kind in each `from_object` method.

- [ ] **Step 4: Implement names, column codes, and definition structs**

Freeze these V1 column codes:

```rust
#[repr(u8)]
pub(crate) enum ColumnType {
    Int64 = 1,
    Decimal = 2,
    Float64 = 3,
    Text = 4,
    Blob = 5,
    Date = 6,
    Timestamp = 7,
    TimestampWithTimeZone = 8,
    Bool = 9,
}
```

`CatalogName::new` rejects empty strings and names above 1013 UTF-8 bytes, ensuring both table-name and column-name physical keys can fit the 1024-byte B+Tree limit. Define immutable structs with checked constructors and read-only getters:

```rust
pub(crate) struct TableDef {
    id: TableId,
    schema_version: SchemaVersion,
    root: PageRef,
    next_rowid: RowId,
    column_count: u16,
    name: CatalogName,
}

pub(crate) struct ColumnDef {
    id: ColumnId,
    table_id: TableId,
    ordinal: u16,
    column_type: ColumnType,
    nullable: bool,
    name: CatalogName,
}
```

- [ ] **Step 5: Run type tests and commit**

Run: `cargo test storage::catalog::types -- --nocapture`

Expected: all catalog type tests pass.

```bash
git add src/storage/mod.rs src/storage/catalog/mod.rs src/storage/catalog/types.rs
git commit -m "feat(storage): define catalog identities"
```

---

### Task 3: Encode and decode the catalog key space

**Files:**
- Create: `src/storage/catalog/key.rs`
- Modify: `src/storage/catalog/mod.rs`

**Interfaces:**
- Produces: `RecordKind` with codes `0x00`, `0x10..=0x14`.
- Produces: exact-key builders for meta, names, ordinals, and definitions.
- Produces: `decode_key(&[u8]) -> Result<DecodedCatalogKey<'_>>`.
- Produces: `prefix_bounds(&[u8]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>)` for table-scoped scans.

- [ ] **Step 1: Write byte-golden and ordering tests**

Use hand-computed bytes, including multibyte UTF-8 and numeric sort order:

```rust
#[test]
fn column_ordinal_key_is_big_endian_and_order_preserving() {
    let table = TableId::new(0x0102_0304_0506_0708).unwrap();
    assert_eq!(column_ordinal_key(table, 9), vec![
        0x13, 1, 2, 3, 4, 5, 6, 7, 8, 0, 9,
    ]);
    assert!(column_ordinal_key(table, 255) < column_ordinal_key(table, 256));
}
```

Also test malformed lengths, trailing bytes, zero IDs, unknown record families, and 1025-byte keys.

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `cargo test column_ordinal_key_is_big_endian_and_order_preserving -- --nocapture`

Expected: compilation fails because `key.rs` is absent.

- [ ] **Step 3: Implement exact builders and strict decoding**

Expose these functions with checked key length:

```rust
pub(super) fn catalog_meta_key() -> Vec<u8>;
pub(super) fn table_name_key(name: &CatalogName) -> Result<Vec<u8>>;
pub(super) fn table_definition_key(id: TableId) -> Vec<u8>;
pub(super) fn column_name_key(table: TableId, name: &CatalogName) -> Result<Vec<u8>>;
pub(super) fn column_ordinal_key(table: TableId, ordinal: u16) -> Vec<u8>;
pub(super) fn column_definition_key(id: ColumnId) -> Vec<u8>;
pub(super) fn decode_key(bytes: &[u8]) -> Result<DecodedCatalogKey<'_>>;
```

Use big-endian ID/ordinal fields and big-endian `u16` name lengths. Reserved families `0x20..=0x3f` and unknown higher families return `Unsupported`; malformed known-family keys return `Corruption`.

- [ ] **Step 4: Implement prefix ranges without sentinel ambiguity**

Build the exclusive upper bound by incrementing the rightmost byte that is not `0xff`; `[0x13, ff..ff]` therefore ends at `[0x14]`. An all-`0xff` prefix uses an unbounded end rather than panicking:

```rust
fn prefix_bounds(prefix: &[u8]) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let start = Bound::Included(prefix.to_vec());
    let mut end = prefix.to_vec();
    let upper = match end.iter().rposition(|byte| *byte != 0xff) {
        Some(index) => {
            end[index] += 1;
            end.truncate(index + 1);
            Bound::Excluded(end)
        }
        None => Bound::Unbounded,
    };
    (start, upper)
}
```

- [ ] **Step 5: Run key tests and commit**

Run: `cargo test storage::catalog::key -- --nocapture`

Expected: every golden byte and malformed-key case passes.

```bash
git add src/storage/catalog/mod.rs src/storage/catalog/key.rs
git commit -m "feat(storage): encode catalog keys"
```

---

### Task 4: Encode versioned catalog records and mappings

**Files:**
- Create: `src/storage/catalog/record.rs`
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/catalog/types.rs`

**Interfaces:**
- Produces: V1 encoders/decoders for `CatalogMeta`, `TableDef`, `ColumnDef`.
- Produces: exact 8-byte mapping payload encoders/decoders inside the common envelope.
- Produces: `decode_record(expected: RecordKind, bytes: &[u8]) -> Result<CatalogRecord>`.

- [ ] **Step 1: Write exact record-golden tests**

Assert all envelope fields and payload bytes rather than round-trip alone:

```rust
#[test]
fn table_name_mapping_has_envelope_and_exact_eight_byte_payload() {
    let bytes = encode_table_name_mapping(TableId::new(0x0102_0304_0506_0708).unwrap());
    assert_eq!(bytes, vec![
        0x10, 1, 0, 0, 8, 0, 0, 0,
        8, 7, 6, 5, 4, 3, 2, 1,
    ]);
}
```

Add one golden vector for each record kind and corruption cases for truncation at every fixed field, nonzero reserved bytes, wrong kind, wrong payload length, invalid enum, invalid UTF-8, zero ID, invalid `PageRef`, and trailing bytes.

- [ ] **Step 2: Run the first golden test and verify it fails**

Run: `cargo test table_name_mapping_has_envelope_and_exact_eight_byte_payload -- --nocapture`

Expected: compilation fails because record encoders do not exist.

- [ ] **Step 3: Implement the common 8-byte envelope**

Use strict total-length equality and distinguish known/future versions:

```rust
fn decode_envelope(expected: RecordKind, bytes: &[u8]) -> Result<&[u8]> {
    let kind = RecordKind::from_byte(*bytes.first().ok_or_else(truncated_record)?)?;
    if kind != expected { return Err(corruption("catalog key and record kind disagree")); }
    let version = *bytes.get(1).ok_or_else(truncated_record)?;
    if version > 1 { return Err(DbError::new(ErrorCode::Unsupported, "catalog record version is newer than V1")); }
    if version != 1 { return Err(corruption("catalog record version zero is invalid")); }
    // Decode reserved and payload length with checked slicing, then require exact EOF.
    strict_payload(bytes)
}
```

Add `strict_payload(bytes: &[u8]) -> Result<&[u8]>` in the same file; it decodes bytes 2..8, verifies the reserved field is zero, converts the little-endian payload length with checked arithmetic, and returns a slice only when `bytes.len() == 8 + payload_len`.

- [ ] **Step 4: Implement all V1 payloads and constructor revalidation**

Encode integer payload fields little-endian. Decoders must reconstruct IDs, `SchemaVersion`, `RowId`, `PageRef`, `CatalogName`, and `ColumnType` through checked constructors, remapping constructor rejection of untrusted disk fields to `Corruption`. Mapping decoders require exactly eight payload bytes. Definition decoders require the key ID to match the payload ID.

- [ ] **Step 5: Run record tests and commit**

Run: `cargo test storage::catalog::record -- --nocapture`

Expected: all golden vectors, every truncation point, and all future-version classifications pass.

```bash
git add src/storage/catalog/mod.rs src/storage/catalog/types.rs src/storage/catalog/record.rs
git commit -m "feat(storage): encode catalog records"
```

---

### Task 5: Encode every scalar value payload through validated constructors

**Files:**
- Create: `src/storage/row.rs`
- Modify: `src/storage/mod.rs`
- Modify: `src/datetime.rs:4-88`

**Interfaces:**
- Produces: read-only `OracleDate` and `OracleTimestamp` field accessors.
- Produces: internal `encode_value(ColumnType, &Value) -> Result<Vec<u8>>`.
- Produces: internal `decode_value(ColumnType, &[u8]) -> Result<Value>`.

- [ ] **Step 1: Write scalar golden tests**

Add exact vectors for all types, including decimal negative zero normalization, NaN bit preservation, leap-day validation, timestamp offset boundaries, empty text/blob, and invalid fixed lengths:

```rust
#[test]
fn timestamp_with_time_zone_payload_is_exact() {
    let date = OracleDate::new(2026, 9, 9, 12, 34, 56).unwrap();
    let value = Value::Timestamp(OracleTimestamp::new(date, 123_456, Some(480)).unwrap());
    assert_eq!(encode_value(ColumnType::TimestampWithTimeZone, &value).unwrap(), vec![
        0xea, 0x07, 9, 9, 12, 34, 56, 0,
        0x40, 0xe2, 0x01, 0x00, 0xe0, 0x01, 0, 0,
    ]);
}
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `cargo test timestamp_with_time_zone_payload_is_exact -- --nocapture`

Expected: compilation fails because row encoding and datetime accessors are absent.

- [ ] **Step 3: Add read-only datetime accessors**

Expose only immutable values:

```rust
impl OracleDate {
    pub const fn year(self) -> u16 { self.year }
    pub const fn month(self) -> u8 { self.month }
    pub const fn day(self) -> u8 { self.day }
    pub const fn hour(self) -> u8 { self.hour }
    pub const fn minute(self) -> u8 { self.minute }
    pub const fn second(self) -> u8 { self.second }
}

impl OracleTimestamp {
    pub const fn date(self) -> OracleDate { self.date }
    pub const fn micros(self) -> u32 { self.micros }
    pub const fn offset_minutes(self) -> Option<i16> { self.offset_minutes }
}
```

- [ ] **Step 4: Implement strict scalar encoding and decoding**

Use the spec's exact widths. Require `Timestamp` to have no offset and `TimestampWithTimeZone` to have one. Decode dates, decimals, and timestamps by calling `OracleDate::new`, `Decimal::new`, and `OracleTimestamp::new`; map invalid disk components to `Corruption`, while caller/value type mismatches remain `Type`. Preserve `f64::to_bits()` exactly.

- [ ] **Step 5: Run scalar tests and commit**

Run: `cargo test storage::row -- --nocapture`

Expected: all scalar golden and invalid-payload tests pass.

```bash
git add src/datetime.rs src/storage/mod.rs src/storage/row.rs
git commit -m "feat(storage): encode row scalar values"
```

---

### Task 6: Implement the canonical row layout and ROWID keys

**Files:**
- Modify: `src/storage/row.rs`
- Modify: `src/storage/catalog/types.rs`

**Interfaces:**
- Produces: `RowCodec::encode(&TableDef, &[ColumnDef], &[Value], usize) -> Result<Vec<u8>>`.
- Produces: `RowCodec::decode(&TableDef, &[ColumnDef], &[u8], usize) -> Result<Vec<Value>>`.
- Produces: `encode_rowid_key(RowId) -> [u8; 8]` and `decode_rowid_key(&[u8]) -> Result<RowId>`.

- [ ] **Step 1: Write full-row golden and boundary tests**

Cover 0, 1, 8, 9, 999, and 1000 columns; NULL/empty distinctions; mixed fixed/variable values; zero-length non-NULL offsets; resource limits; a future row format version returning `Unsupported`; and a complete hand-computed row:

```rust
#[test]
fn rowid_key_is_big_endian_and_numeric_ordered() {
    assert_eq!(encode_rowid_key(RowId::new(0x0102_0304_0506_0708).unwrap()), [1,2,3,4,5,6,7,8]);
    assert!(encode_rowid_key(RowId::new(255).unwrap()) < encode_rowid_key(RowId::new(256).unwrap()));
    assert_eq!(decode_rowid_key(&[0; 8]).unwrap_err().code(), ErrorCode::Corruption);
}
```

For corruption tests, mutate every header field, bitmap high bit, directory offset/length, UTF-8 byte, reserved byte, payload boundary, and append a trailing byte.

- [ ] **Step 2: Run the first layout test and verify it fails**

Run: `cargo test rowid_key_is_big_endian_and_numeric_ordered -- --nocapture`

Expected: compilation fails because ROWID key helpers are absent.

- [ ] **Step 3: Implement ROWID keys and checked row sizing**

ROWID keys are exactly eight big-endian bytes. Before allocating the output, compute all sizes through `checked_add`/`checked_mul`, require the total to fit `u32` and `max_row_bytes`, and reject schema/column mismatches before encoding any bytes.

- [ ] **Step 4: Implement the canonical 32-byte header, bitmap, and directory**

Define:

```rust
pub(crate) struct RowCodec;

impl RowCodec {
    pub(crate) fn encode(
        table: &TableDef,
        columns: &[ColumnDef],
        values: &[Value],
        max_row_bytes: usize,
    ) -> Result<Vec<u8>>;

    pub(crate) fn decode(
        table: &TableDef,
        columns: &[ColumnDef],
        bytes: &[u8],
        max_row_bytes: usize,
    ) -> Result<Vec<Value>>;
}
```

NULL entries must have `(offset, length) == (0, 0)`. Every non-NULL range starts at the previous non-NULL range end, including zero-length values. Decode only after header, bitmap, directory, total length, and schema version are fully validated.

- [ ] **Step 5: Add a deterministic mutation/fuzz-style decoder test**

For each byte of a valid mixed row, apply `byte ^ 0xff`, run the decoder under `catch_unwind`, and require either a complete valid value vector or a controlled `DbError`; truncating at every byte must always return `Corruption` and never panic.

- [ ] **Step 6: Run row tests and commit**

Run: `cargo test storage::row -- --nocapture`

Expected: all row golden, boundary, mutation, and truncation cases pass.

```bash
git add src/storage/catalog/types.rs src/storage/row.rs
git commit -m "feat(storage): encode canonical user rows"
```

---

### Task 7: Build a read-only catalog image with complete cross-reference validation

**Files:**
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/pager.rs:578-614`
- Modify: `src/storage/validate.rs:18-30,67-105`

**Interfaces:**
- Produces: `CatalogReader::open_empty_or_v1(&mut ReadSnapshot) -> Result<CatalogReader>`.
- Produces: internal `CatalogReader::open_candidate(&mut WriteBatch, &BTree, u64) -> Result<CatalogReader>`.
- Produces: storage-private `catalog_tree_with_report` access returning the tree and its validated live-page set.
- Produces: `find_table_by_name`, `find_table_by_id`, `columns_for_table`, and `find_column_by_name` returning references into an immutable decoded image.
- Produces: `CatalogImage` containing one meta record, definitions, reverse mappings, and ordered columns.

- [ ] **Step 1: Write reader and corruption tests**

Build raw catalog B+Trees through the existing test pager and cover empty Phase 3 state, a valid V1 image, every missing/duplicate reverse mapping, dangling IDs, wrong object kinds, non-contiguous ordinals, wrong table ownership, mismatched names/counts, duplicate global object IDs, reserved families, and shallow wrong-root type/generation.

```rust
#[test]
fn uninitialized_snapshot_is_an_immutable_empty_catalog() {
    let (temp, pager) = test_pager(4096).unwrap();
    let before = std::fs::read(temp.path()).unwrap();
    let mut read = pager.begin_read().unwrap();
    let catalog = CatalogReader::open_empty_or_v1(&mut read).unwrap();
    let name = CatalogName::new("T".to_owned()).unwrap();
    assert!(catalog.find_table_by_name(&name).is_none());
    drop(read);
    assert_eq!(std::fs::read(temp.path()).unwrap(), before);
}
```

- [ ] **Step 2: Run the empty-catalog test and verify it fails**

Run: `cargo test uninitialized_snapshot_is_an_immutable_empty_catalog -- --nocapture`

Expected: compilation fails because `CatalogReader` is absent.

- [ ] **Step 3: Expose snapshot state without exposing Pager internals**

Add `ReadSnapshot::feature_bits()` and a `catalog_tree_with_report(TreeKind::Table)` helper; keep the existing `catalog_tree` as a delegating compatibility method. For candidate writes, add an equivalent storage-private helper that resolves the current catalog root through the batch-visible header and reads its actual generation before constructing and validating a `BTree`.

- [ ] **Step 4: Scan and decode one immutable catalog image**

When `(feature_bits & CATALOG_V1) == 0` and root is zero, return `CatalogImage::empty()`. When the feature is absent but root is nonzero, return `Unsupported` so Phase 4A neither overwrites nor interprets the legacy internal tree. Feature-without-root is `Corruption`.

For V1, scan the complete catalog B+Tree once, decode each key/value pair, populate `BTreeMap` structures, then verify all forward/reverse mappings, global IDs, column continuity, counts, ownership, names, and the single meta record before returning the reader.

- [ ] **Step 5: Shallow-validate every user root**

For each `TableDef`, read exactly the root `PageRef`; require matching generation and `TableLeaf` or `TableInternal`. Reject duplicate user roots and roots already present in the catalog live-page set. Add all root page IDs to the shallow live set and call `validate_freelist` with that set, proving roots are not free without traversing descendants. Do not call `validate_tree` for user tables in ordinary open.

- [ ] **Step 6: Run reader tests and commit**

Run: `cargo test storage::catalog -- --nocapture`

Expected: valid images open, malformed mappings fail with the specified code, and large user trees cause only one root read per table beyond the catalog scan.

```bash
git add src/storage/catalog/mod.rs src/storage/pager.rs src/storage/validate.rs
git commit -m "feat(storage): read and validate catalog images"
```

---

### Task 8: Bootstrap catalogs and enforce writer finish semantics

**Files:**
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/pager.rs:683-715,724-805,1030-1170`

**Interfaces:**
- Produces: `CatalogWriter::bootstrap(&mut WriteBatch) -> Result<CatalogWriter>`.
- Produces: `CatalogWriter::finish(self, &mut WriteBatch) -> Result<()>`.
- Produces: `WriteBatch::{begin_catalog_update, finish_catalog_update}` and commit-time unfinished-writer rejection.
- Changes: `WriteBatchCheckpoint` preserves the catalog-update-pending flag.

- [ ] **Step 1: Write bootstrap atomicity and finish-gate tests**

Cover first bootstrap, repeated open, legacy-root refusal, feature/root/meta inconsistent states, rollback, omitted finish, and a directory-root split:

```rust
#[test]
fn unfinished_catalog_writer_cannot_commit() {
    let (_temp, pager) = test_pager(4096).unwrap();
    let mut batch = pager.begin_write().unwrap();
    let _writer = CatalogWriter::bootstrap(&mut batch).unwrap();
    let error = batch.commit().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Transaction);
}
```

- [ ] **Step 2: Run the finish-gate test and verify it fails**

Run: `cargo test unfinished_catalog_writer_cannot_commit -- --nocapture`

Expected: compilation fails because `CatalogWriter` and the finish gate are absent.

- [ ] **Step 3: Add a batch-local catalog update gate**

Store `catalog_update_pending: bool` in `WriteBatch` and its mutation checkpoint. `begin_catalog_update` sets it after requiring an active batch. `finish_catalog_update` clears it only after the candidate root and feature state are validated. `validate_for_commit` returns `Transaction` while it is set; rollback may discard it without publishing.

- [ ] **Step 4: Implement atomic bootstrap**

Define:

```rust
pub(crate) struct CatalogWriter {
    tree: BTree,
    dirty: bool,
}

impl CatalogWriter {
    pub(crate) fn bootstrap(batch: &mut WriteBatch<'_>) -> Result<Self>;
    pub(crate) fn finish(mut self, batch: &mut WriteBatch<'_>) -> Result<()>;
}
```

For an uninitialized batch, checkpoint mutation state, create a Table B+Tree, insert one `CatalogMeta { next_object_id: 1 }`, enable `CATALOG_V1`, and mark the catalog update pending. Restore the checkpoint and original root on every error. Existing V1 state opens the current tree without changing bytes.

- [ ] **Step 5: Implement `finish` as the only publication route**

When dirty, reopen the candidate through `CatalogReader::open_candidate`, then call `track_tree(TreeKind::Table, root)`, `set_catalog_root(root)`, and `finish_catalog_update` in that order. If any operation fails, keep the pending flag set so commit remains impossible.

- [ ] **Step 6: Run bootstrap tests and commit**

Run: `cargo test storage::catalog -- --nocapture`

Expected: only complete uninitialized or complete V1 states are observable after rollback/reopen.

```bash
git add src/storage/catalog/mod.rs src/storage/pager.rs
git commit -m "feat(storage): bootstrap catalogs atomically"
```

---

### Task 9: Add object allocation, table/column mutations, root replacement, and ROWID allocation

**Files:**
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/catalog/record.rs`
- Modify: `src/storage/catalog/types.rs`

**Interfaces:**
- Produces: `CatalogWriter::allocate_object_id(&mut self, &mut WriteBatch) -> Result<ObjectId>`.
- Produces: `insert_table_definition`, `insert_column_definition`, `update_table_root`, and `allocate_rowid`.
- Guarantees: every logical mutation restores both B+Tree root and `WriteBatchCheckpoint` on error.

- [ ] **Step 1: Write mutation and counter tests**

Cover IDs 1..N, global table/column collision detection, duplicate names/ordinals, non-contiguous columns, root splits, `u64::MAX`, rollback reuse, committed non-reuse, table-root replacement, and ROWID+row insertion in one batch. Add `committed_single_column_catalog() -> (TempDatabase, Pager, TableId)` as a test-only fixture that bootstraps a table named `T`, one nullable `Int64` column, finishes the writer, and commits. Add these exact counter helpers:

```rust
fn allocate_then_rollback(pager: &Pager, table_id: TableId) -> RowId {
    let mut batch = pager.begin_write().unwrap();
    let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
    let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
    batch.rollback().unwrap();
    rowid
}

fn allocate_then_commit(pager: &Pager, table_id: TableId) -> RowId {
    let mut batch = pager.begin_write().unwrap();
    let mut writer = CatalogWriter::bootstrap(&mut batch).unwrap();
    let rowid = writer.allocate_rowid(&mut batch, table_id).unwrap();
    writer.finish(&mut batch).unwrap();
    batch.commit().unwrap();
    rowid
}
```

```rust
#[test]
fn rolled_back_rowid_is_reused_but_committed_rowid_is_not() {
    let (_temp, pager, table_id) = committed_single_column_catalog();
    let rolled_back = allocate_then_rollback(&pager, table_id);
    let committed = allocate_then_commit(&pager, table_id);
    assert_eq!(rolled_back, RowId::new(1).unwrap());
    assert_eq!(committed, RowId::new(1).unwrap());
    assert_eq!(allocate_then_commit(&pager, table_id), RowId::new(2).unwrap());
}
```

- [ ] **Step 2: Run the counter test and verify it fails**

Run: `cargo test rolled_back_rowid_is_reused_but_committed_rowid_is_not -- --nocapture`

Expected: compilation fails because writer mutation methods are absent.

- [ ] **Step 3: Implement an all-or-nothing logical mutation helper**

Before a delete+insert replacement or multi-key insertion, capture `batch.checkpoint_mutation()` and `self.tree.root()`. On error, restore the checkpoint and reconstruct the tree from the old root:

```rust
fn restore(&mut self, batch: &mut WriteBatch<'_>, checkpoint: WriteBatchCheckpoint, root: PageRef) {
    batch.restore_mutation(checkpoint);
    self.tree = BTree::from_root(TreeKind::Table, root);
}
```

Mark `dirty` and call `begin_catalog_update` only for a successful logical mutation.

- [ ] **Step 4: Implement object allocation and definition insertion**

`allocate_object_id` reads current `CatalogMeta`, rejects 0/`u64::MAX`, checks all V1 definition namespaces for the candidate, replaces the meta record with `candidate + 1`, and returns the typed `ObjectId`.

`insert_table_definition` inserts definition and name mapping and tracks the user root. `insert_column_definition` inserts definition, table-scoped name mapping, and ordinal mapping. It requires an existing table, ordinal below 1000, and a unique global object identity. Final count/continuity validation occurs in `finish`.

- [ ] **Step 5: Implement table-root replacement and ROWID allocation**

Use exact signatures:

```rust
pub(crate) fn update_table_root(
    &mut self,
    batch: &mut WriteBatch<'_>,
    table_id: TableId,
    root: PageRef,
) -> Result<()>;

pub(crate) fn allocate_rowid(
    &mut self,
    batch: &mut WriteBatch<'_>,
    table_id: TableId,
) -> Result<RowId>;
```

Both replace the complete `TableDef` record. Root replacement requires a tracked Table B+Tree root. ROWID allocation returns the current candidate, stores checked `candidate + 1`, and rejects `u64::MAX` before modifying records.

- [ ] **Step 6: Prove row/root/catalog atomicity**

In one batch, allocate ROWID, encode a row, insert it into the table B+Tree, call `update_table_root` if the B+Tree split, finish the catalog writer, and commit. Inject an error before finish and prove rollback leaves the row, root, and counter absent together.

- [ ] **Step 7: Run mutation tests and commit**

Run: `cargo test storage::catalog -- --nocapture`

Expected: all identity, root, column, ROWID, rollback, and split cases pass.

```bash
git add src/storage/catalog/mod.rs src/storage/catalog/record.rs src/storage/catalog/types.rs
git commit -m "feat(storage): mutate catalog definitions"
```

---

### Task 10: Integrate recovery validation and add explicit deep validation

**Files:**
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/pager.rs:117-210,520-614`
- Modify: `src/storage/validate.rs:18-30,67-105,361-430`

**Interfaces:**
- Produces: `CatalogReader::validate_deep(&self, &mut ReadSnapshot) -> Result<()>`.
- Changes: `Pager::validate_recovered` validates feature/root/meta state and catalog cross-references before publishing an opened database.
- Guarantees: ordinary open does not traverse user-table descendants; deep validation proves catalog/user/freelist disjointness.

- [ ] **Step 1: Write normal-open cost and deep-overlap tests**

Create a table tree large enough to have internal and overflow pages. Count page-source reads during ordinary catalog open and require only catalog pages plus one root read per table. Then run deep validation and require all user pages to be visited. Add corrupt fixtures where two tables share a page, a user tree overlaps the catalog, or any live page appears in the freelist.

- [ ] **Step 2: Run the overlap test and verify it fails**

Run: `cargo test deep_catalog_validation_rejects_cross_tree_overlap -- --nocapture`

Expected: compilation fails because deep catalog validation is absent.

- [ ] **Step 3: Return reusable page sets from tree validation**

Reuse `TreeValidation::all_live_pages()`. Deep validation starts with the catalog tree's page set, then validates every table root with `validate_tree`; reject any nonempty intersection before adding that tree's pages to the combined live set. Call `validate_freelist` once with the combined set.

- [ ] **Step 4: Integrate V1 catalog checks into open/recovery**

After WAL recovery constructs the snapshot but before `Pager::open` returns, call `CatalogReader::open_empty_or_v1`. Preserve Phase 3 behavior for featureless root-zero databases. A durable V1 state with missing root/meta, invalid mapping, wrong root generation/type, or invalid schema version returns `Corruption`; it is never replaced by an older complete commit.

- [ ] **Step 5: Add checkpoint/reopen snapshot tests**

Hold an old snapshot, commit a new table or ROWID, checkpoint, and assert the old reader's decoded catalog image remains unchanged while a new snapshot sees the new state. Reopen after checkpoint and require identical definitions, roots, and counters.

- [ ] **Step 6: Run recovery/deep tests and commit**

Run: `cargo test storage::catalog -- --nocapture`

Run: `cargo test reopen -- --nocapture`

Expected: all recovery, shallow-open, deep-overlap, checkpoint, and old-snapshot cases pass.

```bash
git add src/storage/catalog/mod.rs src/storage/pager.rs src/storage/validate.rs
git commit -m "feat(storage): validate catalog recovery"
```

---

### Task 11: Add deterministic reference models, corruption matrices, and crash recovery

**Files:**
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Produces: fixed-seed catalog/row model test against `BTreeMap`.
- Produces: ignored `phase_4a_catalog_crash_recovery_matrix` and its child helper.
- Verifies: bootstrap, split, bulk metadata, and ROWID+row commits at every existing WAL fault boundary.

- [ ] **Step 1: Build a deterministic in-memory reference model**

Use a local linear-congruential generator with a fixed seed; do not add `rand`. Model table names, ordered columns, object counter, per-table ROWID counter, and encoded row bytes in `BTreeMap`. Generate at least 500 operations containing table creation, column creation, row allocation/insertion, rollback windows, reopen, and snapshot reads.

```rust
struct CatalogModel {
    next_object_id: u64,
    tables: BTreeMap<u64, ModelTable>,
}

struct ModelTable {
    name: String,
    next_rowid: u64,
    columns: Vec<(String, ColumnType, bool)>,
    rows: BTreeMap<u64, Vec<Value>>,
}
```

After each committed window, compare every lookup, column order, counter, ROWID key order, and decoded row to the model.

- [ ] **Step 2: Add every-byte corruption matrices**

Add tests named `catalog_record_corruption_matrix_never_panics` and `row_corruption_matrix_never_panics`. For each valid catalog record and representative mixed row, truncate at every byte and flip every fixed/reserved/length byte. Run each decode/open through `catch_unwind`; require a complete valid value or a controlled `DbError`, never panic. Assert exact error codes for future version versus known-format corruption.

- [ ] **Step 3: Add the Phase 4A crash child and parent matrix**

Follow the existing Phase 3 child-process protocol with distinct environment names:

```rust
const PHASE_4A_CRASH_CHILD: &str = "storage::test_support::phase_4a_catalog_crash_child";
const PHASE_4A_SCENARIO_ENV: &str = "ORA_SQLLITE_PHASE4A_SCENARIO";
const PHASE_4A_FAULT_ENV: &str = "ORA_SQLLITE_PHASE4A_FAULT";
const PHASE_4A_OCCURRENCE_ENV: &str = "ORA_SQLLITE_PHASE4A_OCCURRENCE";
```

Scenarios are bootstrap, catalog-root split, bulk table/column definitions, and ROWID+row insertion. For every observed WAL write/sync/publication occurrence, abort the child, reopen in the parent, and require exactly the complete pre-state or complete post-state. At or after durable WAL sync, require the post-state. Reopen twice and deep-validate to prove idempotence.

- [ ] **Step 4: Run fast model and corruption tests**

Run: `cargo test phase_4a_reference_model -- --nocapture`

Run: `cargo test catalog_record_corruption_matrix_never_panics -- --nocapture`

Run: `cargo test row_corruption_matrix_never_panics -- --nocapture`

Expected: all deterministic non-ignored model and corruption tests pass.

- [ ] **Step 5: Run the ignored crash matrix once**

Run: `cargo test phase_4a_catalog_crash_recovery_matrix -- --ignored --nocapture`

Expected: every fault occurrence reopens to one complete atomic state and passes deep validation.

- [ ] **Step 6: Commit**

```bash
git add src/storage/test_support.rs
git commit -m "test(storage): prove catalog and row recovery"
```

---

### Task 12: Add Phase 4A CI gates, document the boundary, and run release-grade verification

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md:26-56`

**Interfaces:**
- Produces: dedicated Linux Phase 4A crash/deep-validation CI job.
- Produces: accurate README status retaining the Phase 4B/4C boundary.
- Verifies: every Phase 4A acceptance criterion and repository hygiene rule.

- [ ] **Step 1: Add the dedicated CI job**

Keep all five existing jobs unchanged. Add:

```yaml
  phase4a-catalog-recovery:
    name: Linux catalog and row recovery
    runs-on: ubuntu-latest
    timeout-minutes: 240
    steps:
      - uses: actions/checkout@v4
      - name: Install stable Rust
        run: rustup toolchain install stable --profile minimal
      - name: Run catalog crash recovery matrix
        run: >-
          rustup run stable cargo test
          phase_4a_catalog_crash_recovery_matrix
          -- --ignored --nocapture
```

- [ ] **Step 2: Update README status and limitations**

State that Phase 4A now provides V1 system catalog persistence, stable object IDs, snapshot-visible table/column definitions, hidden ROWID allocation, and canonical row encoding. Preserve this explicit limitation:

```text
SQL parsing and binding, the public database/connection/statement API, DDL/DML
execution, indexes, and constraint enforcement remain unimplemented. These are
Phase 4B and Phase 4C work; Phase 4A APIs are crate-internal storage boundaries.
```

- [ ] **Step 3: Run formatting and lint gates**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: both commands exit 0 with no warnings.

- [ ] **Step 4: Run stable and MSRV suites**

Run: `cargo test --all-targets --all-features`

Run: `rustup run 1.85.0 cargo test --all-targets --all-features`

Expected: all non-ignored tests pass on stable and Rust 1.85.

- [ ] **Step 5: Run exhaustive Phase 2/3/4A suites**

Run: `cargo test --profile exhaustive fixed_seed_reference_model_covers_every_page_size_and_tree_kind -- --ignored --nocapture`

Run: `cargo test phase_3_crash_recovery_matrix -- --ignored --nocapture`

Run: `cargo test phase_4a_catalog_crash_recovery_matrix -- --ignored --nocapture`

Expected: all storage configurations and all Phase 3/4A crash boundaries pass.

- [ ] **Step 6: Verify scope and repository hygiene**

Run: `rg -n "unsafe" src`

Expected: only `#![forbid(unsafe_code)]`; no unsafe block.

Run: `rg -n "pub struct (Database|Connection|Statement|Transaction)|sqlparser|CREATE TABLE|SELECT .* FROM" src`

Expected: no public SQL/CRUD surface or parser dependency.

Run: `git status --short --branch`

Expected: only intended README/workflow changes before the final commit; no `Cargo.lock`, `*.db`, `*.wal`, child process, or generated directory.

- [ ] **Step 7: Commit documentation and CI**

```bash
git add .github/workflows/ci.yml README.md
git commit -m "docs: record phase 4a storage guarantees"
```

- [ ] **Step 8: Perform the final diff review**

Run: `git diff --check origin/main...HEAD`

Run: `git log --oneline origin/main..HEAD`

Run: `git diff --stat origin/main...HEAD`

Confirm the branch contains only Phase 4A feature state, catalog, row encoding, recovery validation, tests, CI, and documentation. Confirm Phase 4B SQL/API and Phase 4C execution/index/constraint code are absent.
