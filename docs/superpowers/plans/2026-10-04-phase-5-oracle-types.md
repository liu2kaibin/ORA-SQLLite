# Phase 5 Oracle Types Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add persisted and enforced Oracle-compatible `NUMBER`, `VARCHAR2`, `CHAR`, `RAW`, `DATE`, `TIMESTAMP`, and `TIMESTAMP WITH TIME ZONE` column contracts without losing format-2 compatibility or ORA-SQLLite's lightweight operation.

**Architecture:** Add an immutable `OracleTypeDescriptor` beside the existing physical `ColumnType`, persist it in `ColumnDef V2`, and route every target-column value through one normalization module. Preserve all V1 bytes and semantics; V2 comparison, row, CHECK, and index paths carry the descriptor needed for CHAR padding, decimal quantization, nanoseconds, and UTC instant ordering.

**Tech Stack:** Rust 1.85+, existing `sqlparser` frontend, format-2 catalog/WAL/B+Tree storage, existing public `Value`/prepared-statement API, no new runtime dependency.

**Spec:** `docs/superpowers/specs/2026-10-04-phase-5-oracle-types-design.md`

## Global Constraints

- Keep `rust-version = "1.85"`; every new Rust API must compile on Rust 1.85.
- Add no runtime dependency and no NLS, timezone database, locale, or background service.
- Keep database format 2 and use `ORACLE_TYPES_V1 = 1 << 2`; feature bits are monotonic.
- Preserve every V1 ColumnDef, row, CheckProgram, and IndexKey byte and its existing behavior.
- Enforce NUMBER precision 1..=38 and scale -38..=38.
- Enforce VARCHAR2 1..=32767 Unicode scalar values, CHAR 1..=2000 Unicode scalar values, and RAW 1..=32767 bytes.
- Enforce TIMESTAMP precision 0..=9 with default 6 and fixed offsets only in -14:00..=+14:00.
- Convert only empty Text to NULL; an empty Blob and whitespace-only Text remain non-NULL.
- Use existing stable `ErrorCode`; ORA numbers remain optional error metadata and do not affect fatality.
- Keep normal CI lightweight; the only scale acceptance is an explicit ignored 100,000-row test.
- Do not add Oracle functions, sequences, DUAL, ROWNUM, pagination, ALTER TABLE, NLS, named timezones, or `WITH LOCAL TIME ZONE`.

## Review Focus

- A NUMBER rounding carry such as `9.99 -> NUMBER(2,1)` must return Type/ORA-01438 without consuming ROWID or changing any index; Task 3 pins scalar behavior and Task 5 pins statement rollback.
- TIMESTAMP rounding at `23:59:59.999999999`, p=0, and +/-14:00 must carry or fail deterministically while V1 microsecond bytes stay unchanged; Task 4 owns these cases.
- Two `TIMESTAMP WITH TIME ZONE` values with different offsets but the same UTC instant must compare equal and collide in a unique index while SELECT preserves each row's original offset; Tasks 6 and 7 pin both paths.
- Empty Text written to a NOT NULL Oracle column must become NULL and return Constraint/ORA-01400, while empty Blob and one-space Text remain values; Tasks 3 and 5 cover the distinction.
- A crash during the first V2 CREATE TABLE must recover to either no V2 records/no bit or complete V2 records/bit, and a V2 record without the bit must be Corruption; Task 8 adds the fault matrix and real-file witness.

---

## File Structure

- Create `src/oracle_type.rs`: immutable Oracle descriptors, declaration-independent normalization, strict Text parsers, contract-aware comparison, and ORA-bearing errors.
- Modify `src/lib.rs`: register the internal module and keep the public surface limited to existing values plus the new nanosecond timestamp constructor/accessor.
- Modify `src/datetime.rs`: nanosecond `OracleTimestamp`, checked calendar carry, fixed-offset validation, and UTC comparison keys.
- Modify `src/storage/features.rs`: `ORACLE_TYPES_V1` and its dependency on `CATALOG_V1`.
- Modify `src/storage/catalog/types.rs`: optional Oracle descriptor in `ColumnDef`, V1/V2 constructors, and immutable accessors.
- Modify `src/storage/catalog/record.rs`: exact ColumnDef V2 envelope/payload encoding and strict decoding.
- Modify `src/storage/relation.rs`: carry descriptors through `ColumnSpec`, atomically enable the feature bit, and validate exact V2 foreign-key contracts.
- Modify `src/storage/row.rs`: select V1 microseconds or V2 nanoseconds from ColumnDef without changing row widths.
- Modify `src/storage/constraint/program.rs`: CheckProgram V2 Oracle comparison instructions while retaining byte-identical V1 programs.
- Modify `src/storage/index/key.rs`: V2 timestamp/nanosecond/UTC key components and descriptor-aware decoding.
- Modify `src/storage/index/scan.rs`: accept already normalized V2 range values and validate them with ColumnDef.
- Modify `src/sql/bound/{expr,statement}.rs`, `src/sql/bind/{scope,statement,types}.rs`, and `src/sql/plan/{expr,index,mutation}.rs`: carry descriptors and comparison contracts from catalog through immutable plans.
- Modify `src/sql/exec/{expr,mutation,params,scan}.rs`: normalize assignments and search inputs once, then share the result across scan and filter.
- Modify `src/api/metadata.rs`: retain an internal parameter-source policy without changing public metadata methods.
- Modify `src/error.rs`: keep codes stable and verify ORA formatting/accessors for the new mappings.
- Create `tests/phase5_oracle_types_api.rs`: public CRUD, parameter, constraint, comparison, index, reopen, and error tests.
- Create `tests/phase5_oracle_types_scale.rs`: ignored 100,000-row acceptance test only.
- Modify `tests/support/format2.rs`: inspect ColumnDef record version, descriptor fields, feature bits, rows, and indexes without using production decoders.
- Modify `.github/workflows/ci.yml`: add only the fast Phase 5 public integration target.

### Task 1: Oracle descriptors, feature bit, and ColumnDef V2

**Files:**
- Create: `src/oracle_type.rs`
- Modify: `src/lib.rs`
- Modify: `src/storage/features.rs`
- Modify: `src/storage/catalog/types.rs`
- Modify: `src/storage/catalog/record.rs`
- Test: unit tests in the five files above

**Interfaces:**
- Consumes: existing `ColumnType`, `ColumnDef`, catalog envelope, and feature validation.
- Produces: `OracleTypeDescriptor::{number,varchar2,char_type,raw,date,timestamp,timestamp_with_time_zone}`, `physical_type() -> ColumnType`, `ColumnDef::new_oracle(...)`, `ColumnDef::oracle_type() -> Option<OracleTypeDescriptor>`, and `ORACLE_TYPES_V1`.

- [ ] **Step 1: Write failing descriptor and feature-bit tests**

```rust
#[test]
fn oracle_descriptor_limits_and_physical_types_are_exact() {
    let number = OracleTypeDescriptor::number(38, -38).unwrap();
    assert_eq!(number.physical_type(), ColumnType::Decimal);
    assert_eq!(number.precision(), Some(38));
    assert_eq!(number.scale(), Some(-38));
    assert_eq!(OracleTypeDescriptor::number(0, 0).unwrap_err().oracle_code(), Some(902));
    assert_eq!(OracleTypeDescriptor::varchar2(32_768).unwrap_err().oracle_code(), Some(902));
    assert_eq!(OracleTypeDescriptor::char_type(2_001).unwrap_err().oracle_code(), Some(902));
    assert_eq!(OracleTypeDescriptor::raw(32_767).unwrap().physical_type(), ColumnType::Blob);
    assert_eq!(OracleTypeDescriptor::timestamp(10).unwrap_err().oracle_code(), Some(902));
}

#[test]
fn oracle_type_feature_requires_catalog_v1() {
    assert_eq!(ORACLE_TYPES_V1, 1 << 2);
    assert!(validate_feature_bits(CATALOG_V1 | ORACLE_TYPES_V1).is_ok());
    assert_eq!(validate_feature_bits(ORACLE_TYPES_V1).unwrap_err().code(), ErrorCode::Corruption);
}
```

- [ ] **Step 2: Run the focused tests and confirm the new symbols are absent**

Run: `rustup run stable cargo test oracle_descriptor_limits_and_physical_types_are_exact -- --exact --nocapture`

Run: `rustup run stable cargo test oracle_type_feature_requires_catalog_v1 -- --exact --nocapture`

Expected: compilation fails because `OracleTypeDescriptor` and `ORACLE_TYPES_V1` do not exist.

- [ ] **Step 3: Add the checked descriptor and feature bit**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OracleTypeDescriptor {
    Number { precision: u8, scale: i8 },
    Varchar2 { length_chars: u32 },
    Char { length_chars: u32 },
    Raw { length_bytes: u32 },
    Date,
    Timestamp { fractional_precision: u8 },
    TimestampWithTimeZone { fractional_precision: u8 },
}

impl OracleTypeDescriptor {
    pub(crate) fn number(precision: u8, scale: i8) -> Result<Self> {
        if !(1..=38).contains(&precision) || !(-38..=38).contains(&scale) {
            return Err(DbError::new(ErrorCode::Type, "invalid NUMBER precision or scale")
                .with_oracle_code(902));
        }
        Ok(Self::Number { precision, scale })
    }

    pub(crate) const fn physical_type(self) -> ColumnType {
        match self {
            Self::Number { .. } => ColumnType::Decimal,
            Self::Varchar2 { .. } | Self::Char { .. } => ColumnType::Text,
            Self::Raw { .. } => ColumnType::Blob,
            Self::Date => ColumnType::Date,
            Self::Timestamp { .. } => ColumnType::Timestamp,
            Self::TimestampWithTimeZone { .. } => ColumnType::TimestampWithTimeZone,
        }
    }
}

pub(super) const ORACLE_TYPES_V1: u64 = 1 << 2;
pub(super) const KNOWN_FEATURE_BITS: u64 = CATALOG_V1 | INDEX_CONSTRAINT_V1 | ORACLE_TYPES_V1;
```

Implement all constructors and read-only accessors with the exact limits from Global Constraints. Extend `validate_feature_bits` so `ORACLE_TYPES_V1` without `CATALOG_V1` is Corruption.

- [ ] **Step 4: Write failing ColumnDef V2 golden-byte and corruption tests**

```rust
fn oracle_number_column() -> ColumnDef {
    ColumnDef::new_oracle(
        ColumnId::new(3).unwrap(),
        TableId::new(2).unwrap(),
        0,
        false,
        CatalogName::new("AMOUNT").unwrap(),
        OracleTypeDescriptor::number(12, 2).unwrap(),
    ).unwrap()
}

#[test]
fn column_definition_v2_has_the_stable_number_payload() {
    let column = oracle_number_column();
    let encoded = encode_column_definition(&column);
    assert_eq!(encoded[1], 2);
    assert_eq!(&encoded[8 + 20..8 + 28], &[1, 12, 2, 0, 0, 0, 0, 0]);
    assert_eq!(decode_column_definition(column.id(), &encoded).unwrap(), column);
}

#[test]
fn column_definition_v2_rejects_nonzero_reserved_and_trailing_bytes() {
    let mut reserved = encode_column_definition(&oracle_number_column());
    reserved[8 + 30] = 1;
    assert_eq!(decode_column_definition(ColumnId::new(3).unwrap(), &reserved).unwrap_err().code(), ErrorCode::Corruption);
    let mut trailing = encode_column_definition(&oracle_number_column());
    trailing.push(0);
    assert_eq!(decode_column_definition(ColumnId::new(3).unwrap(), &trailing).unwrap_err().code(), ErrorCode::Corruption);
}
```

Use the existing 8-byte catalog envelope; payload offsets match the approved spec. Build the test helper with fixed IDs so the expected bytes remain hand-checkable.

- [ ] **Step 5: Implement V1/V2 ColumnDef construction and record dispatch**

```rust
pub(crate) struct ColumnDef {
    id: ColumnId,
    table_id: TableId,
    ordinal: u16,
    column_type: ColumnType,
    nullable: bool,
    oracle_type: Option<OracleTypeDescriptor>,
    name: CatalogName,
}

pub(crate) fn encode_column_definition(column: &ColumnDef) -> Vec<u8> {
    match column.oracle_type() {
        None => encode_column_definition_v1(column),
        Some(descriptor) => encode_column_definition_v2(column, descriptor),
    }
}
```

Keep `ColumnDef::new(...)` as the byte-identical V1 constructor. Add `new_oracle(...)`, derive the physical type from the descriptor, encode record version 2 with the exact 32-byte fixed prefix, and dispatch decoding by envelope version. V1 decoding must construct `oracle_type: None`; V2 decoding must reject unknown kind, bad parameter fields, physical-type mismatch, bad UTF-8, reserved data, and trailing bytes as Corruption.

- [ ] **Step 6: Run catalog regressions and commit**

Run: `rustup run stable cargo test storage::features -- --nocapture`

Run: `rustup run stable cargo test storage::catalog::record -- --nocapture`

Run: `rustup run stable cargo test storage::catalog::types -- --nocapture`

Expected: PASS, including all existing V1 golden bytes.

```bash
git add src/lib.rs src/oracle_type.rs src/storage/features.rs src/storage/catalog/types.rs src/storage/catalog/record.rs
git commit -m "feat: persist Oracle column descriptors"
```

### Task 2: Bind Oracle declarations and create V2 schemas atomically

**Files:**
- Modify: `src/sql/bound/statement.rs`
- Modify: `src/sql/bind/statement.rs`
- Modify: `src/sql/bind/scope.rs`
- Modify: `src/sql/plan/mutation.rs`
- Modify: `src/sql/exec/mutation.rs`
- Modify: `src/storage/relation.rs`
- Modify: `tests/support/format2.rs`
- Create: `tests/phase5_oracle_types_api.rs`

**Interfaces:**
- Consumes: Task 1 descriptors, `ColumnDef::new_oracle`, and `ORACLE_TYPES_V1`.
- Produces: `BoundTypeDeclaration { column_type, oracle_type }`; descriptor-bearing bound/plan columns; `ColumnSpec::new_oracle`; CREATE TABLE that commits V2 records and the feature bit together; independent `OracleColumnCapture` fields used by later tests.

- [ ] **Step 1: Write declaration and first-feature integration tests**

```rust
#[test]
fn creates_every_oracle_type_and_persists_its_arguments() {
    let (files, database) = support::new_database("phase5-types-ddl");
    let mut connection = database.connect().unwrap();
    let create = connection.prepare(
        "CREATE TABLE T (N NUMBER(12,2), V VARCHAR2(20), C CHAR(4), R RAW(16), D DATE, TS TIMESTAMP(9), TZ TIMESTAMP(3) WITH TIME ZONE)"
    ).unwrap();
    connection.execute(&create, Parameters::none()).unwrap();
    drop(connection);
    let capture = support::format2::raw_catalog_capture(files.main()).unwrap();
    assert_eq!(capture.feature_bits & 4, 4);
    assert_eq!(capture.oracle_columns["T.N"].precision, 12);
    assert_eq!(capture.oracle_columns["T.N"].scale, 2);
    assert_eq!(capture.oracle_columns["T.V"].length, 20);
    assert_eq!(capture.oracle_columns["T.TS"].precision, 9);
}

#[test]
fn rejects_bare_number_and_local_timezone_before_catalog_mutation() {
    let (files, database) = support::new_database("phase5-types-reject");
    let mut connection = database.connect().unwrap();
    for sql in [
        "CREATE TABLE A (N NUMBER)",
        "CREATE TABLE B (N NUMBER(0,0))",
        "CREATE TABLE C (V VARCHAR2(0))",
        "CREATE TABLE D (T TIMESTAMP(6) WITH LOCAL TIME ZONE)",
    ] {
        assert_eq!(connection.prepare(sql).unwrap_err().oracle_code(), Some(902));
    }
    assert_eq!(support::format2::raw_catalog_capture(files.main()).unwrap().feature_bits & 4, 0);
}
```

At the top of `tests/phase5_oracle_types_api.rs`, reuse the established public helpers and define only the Phase 5 value builders:

```rust
mod support;

use ora_sqllite::{Connection, Decimal, ParameterValue, Parameters, Value};

fn decimal_value(magnitude: u128, scale: i8) -> Value {
    Value::Decimal(Decimal::new(false, scale, magnitude).unwrap())
}

fn collect_with_params(
    connection: &mut Connection,
    sql: &str,
    values: &[ParameterValue],
) -> Vec<Vec<Value>> {
    let statement = connection.prepare(sql).unwrap();
    support::collect_statement(connection, &statement, Parameters::positional(values))
}
```

Extend `tests/support/format2.rs` in this task with `OracleColumnCapture { kind, precision, scale, length }` and populate it from record version 2 using independent byte offsets. Task 8 later adds corruption utilities and broader assertions.

- [ ] **Step 2: Run the DDL tests and verify the current V1 binder rejects them**

Run: `rustup run stable cargo test --test phase5_oracle_types_api creates_every_oracle_type_and_persists_its_arguments -- --exact --nocapture`

Expected: FAIL because type arguments/modifiers are still rejected as catalog V1.

- [ ] **Step 3: Carry descriptors through binding and planning**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundTypeDeclaration {
    pub(crate) column_type: ColumnType,
    pub(crate) oracle_type: Option<OracleTypeDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundColumnDefinition {
    pub(crate) name: CatalogName,
    pub(crate) ordinal: u16,
    pub(crate) column_type: ColumnType,
    pub(crate) oracle_type: Option<OracleTypeDescriptor>,
    pub(crate) nullable: bool,
    pub(crate) span: SqlSpan,
}
```

Replace `bind_column_type` with `bind_type_declaration`. Parse only the approved forms: NUMBER requires one or two integer arguments; TIMESTAMP accepts zero or one; VARCHAR2/CHAR/RAW require one; DATE requires none; only TIMESTAMP accepts `WITH TIME ZONE`. Preserve the existing V1 mapping for core declarations with no arguments/modifiers. Attach SQL location and ORA-00902 to every invalid recognized declaration.

- [ ] **Step 4: Build ColumnSpec V1/V2 and enable the bit in the same WriteBatch**

```rust
impl ColumnSpec {
    pub(crate) fn new_oracle(
        name: CatalogName,
        descriptor: OracleTypeDescriptor,
        nullable: bool,
    ) -> Self {
        Self {
            name,
            column_type: descriptor.physical_type(),
            oracle_type: Some(descriptor),
            nullable,
        }
    }
}
```

At relation creation, allocate all objects first, emit V1 or V2 ColumnDef from each ColumnSpec, and call `batch.enable_feature_bits(CATALOG_V1 | ORACLE_TYPES_V1)` only when at least one V2 column exists. Keep the feature update inside the same statement checkpoint and CatalogWriter finish as the table records.

- [ ] **Step 5: Add exact V2 foreign-key binding rules**

```rust
fn foreign_key_columns_compatible(child: &ColumnDef, parent: &ColumnDef) -> bool {
    match (child.oracle_type(), parent.oracle_type()) {
        (Some(left), Some(right)) => left == right,
        (None, None) => foreign_key_types_compatible(child.column_type(), parent.column_type()),
        _ => false,
    }
}
```

Use equivalent logic for a not-yet-persisted child `BoundColumnDefinition`. Reject mixed V1/V2 and unequal V2 descriptors during CREATE TABLE binding, before any catalog write.

- [ ] **Step 6: Run the DDL, catalog, and constraint suites and commit**

Run: `rustup run stable cargo test --test phase5_oracle_types_api --no-default-features`

Run: `rustup run stable cargo test sql::bind::statement -- --nocapture`

Run: `rustup run stable cargo test storage::relation -- --nocapture`

Expected: PASS; old core declarations still generate V1 records and do not set bit 2.

```bash
git add src/sql/bound/statement.rs src/sql/bind/statement.rs src/sql/bind/scope.rs src/sql/plan/mutation.rs src/sql/exec/mutation.rs src/storage/relation.rs tests/support/format2.rs tests/phase5_oracle_types_api.rs
git commit -m "feat: bind Oracle type declarations"
```

### Task 3: Central NUMBER, character, RAW, and empty-string normalization

**Files:**
- Modify: `src/oracle_type.rs`
- Modify: `src/decimal.rs`
- Modify: `src/error.rs`
- Test: unit tests in those files

**Interfaces:**
- Consumes: Task 1 `OracleTypeDescriptor` and existing `Value`/`Decimal`.
- Produces: `normalize_for_type(descriptor, value) -> Result<Value>`, `normalize_for_column(column, value) -> Result<Value>`, and ORA-bearing helpers used by DML, comparison, CHECK, and index tasks.

- [ ] **Step 1: Write failing normalization boundary tests**

```rust
fn decimal_value(magnitude: u128, scale: i8) -> Value {
    Value::Decimal(Decimal::new(false, scale, magnitude).unwrap())
}

#[test]
fn number_rounds_before_precision_and_reports_oracle_errors() {
    let ty = OracleTypeDescriptor::number(5, 2).unwrap();
    assert_eq!(normalize_for_type(ty, Value::Text("12.345".into())).unwrap(), decimal_value(1235, 2));
    let carry = OracleTypeDescriptor::number(2, 1).unwrap();
    let error = normalize_for_type(carry, Value::Text("9.99".into())).unwrap_err();
    assert_eq!((error.code(), error.oracle_code()), (ErrorCode::Type, Some(1438)));
    assert_eq!(normalize_for_type(OracleTypeDescriptor::number(2, -1).unwrap(), Value::Int64(149)).unwrap(), decimal_value(15, -1));
    assert_eq!(normalize_for_type(OracleTypeDescriptor::number(4, 5).unwrap(), Value::Text("0.01234".into())).unwrap(), decimal_value(1234, 5));
}

#[test]
fn empty_text_is_null_but_empty_blob_and_space_are_values() {
    let varchar = OracleTypeDescriptor::varchar2(3).unwrap();
    assert_eq!(normalize_for_type(varchar, Value::Text(String::new())).unwrap(), Value::Null);
    assert_eq!(normalize_for_type(varchar, Value::Text(" ".into())).unwrap(), Value::Text(" ".into()));
    let raw = OracleTypeDescriptor::raw(3).unwrap();
    assert_eq!(normalize_for_type(raw, Value::Blob(Vec::new())).unwrap(), Value::Blob(Vec::new()));
}

#[test]
fn unicode_lengths_and_char_padding_use_scalar_values() {
    let varchar = OracleTypeDescriptor::varchar2(2).unwrap();
    assert_eq!(normalize_for_type(varchar, Value::Text("表A".into())).unwrap(), Value::Text("表A".into()));
    let too_long = normalize_for_type(varchar, Value::Text("表AB".into())).unwrap_err();
    assert_eq!(too_long.oracle_code(), Some(12899));
    let char_type = OracleTypeDescriptor::char_type(3).unwrap();
    assert_eq!(normalize_for_type(char_type, Value::Text("表".into())).unwrap(), Value::Text("表  ".into()));
}
```

- [ ] **Step 2: Run tests and confirm normalization is absent**

Run: `rustup run stable cargo test oracle_type::tests -- --nocapture`

Expected: compilation fails because the normalization functions do not exist.

- [ ] **Step 3: Implement exact decimal parsing, quantization, and precision checks**

```rust
pub(crate) fn normalize_for_type(
    descriptor: OracleTypeDescriptor,
    value: Value,
) -> Result<Value> {
    if matches!(&value, Value::Text(text) if text.is_empty()) {
        return Ok(Value::Null);
    }
    match descriptor {
        OracleTypeDescriptor::Number { precision, scale } => {
            normalize_number(value, precision, scale).map(Value::Decimal)
        }
        OracleTypeDescriptor::Varchar2 { length_chars } => normalize_varchar(value, length_chars),
        OracleTypeDescriptor::Char { length_chars } => normalize_char(value, length_chars),
        OracleTypeDescriptor::Raw { length_bytes } => normalize_raw(value, length_bytes),
        OracleTypeDescriptor::Date => normalize_date(value),
        OracleTypeDescriptor::Timestamp { fractional_precision } => normalize_timestamp(value, fractional_precision, false),
        OracleTypeDescriptor::TimestampWithTimeZone { fractional_precision } => normalize_timestamp(value, fractional_precision, true),
    }
}
```

Parse NUMBER Text with an explicit byte-state machine accepting only sign, ASCII digits, and one decimal point. Quantize with checked powers of ten and half-away-from-zero; if discarded remainder is at least half the divisor, increment magnitude. Count digits after quantization, with zero counted as one, and return Type/ORA-01438 when digits exceed p. Return Type/ORA-01722 for syntax or value-domain errors. Do not route through f64.

- [ ] **Step 4: Implement Text/Blob contracts and column-aware NULL errors**

```rust
pub(crate) fn normalize_for_column(column: &ColumnDef, value: Value) -> Result<Value> {
    let normalized = match column.oracle_type() {
        Some(descriptor) => normalize_for_type(descriptor, value)?,
        None => value,
    };
    if !column.nullable() && normalized == Value::Null {
        return Err(DbError::new(
            ErrorCode::Constraint,
            format!("column {} cannot be NULL", column.name().as_str()),
        ).with_oracle_code(1400));
    }
    Ok(normalized)
}
```

Use `chars().count()` for VARCHAR2/CHAR, append U+0020 until the declared count, and return Type/ORA-12899 rather than truncating. RAW accepts only Blob at this layer and checks decoded byte length. A mismatched source shape returns Type with the column declaration in its message.

- [ ] **Step 5: Verify stable errors and commit**

Run: `rustup run stable cargo test oracle_type -- --nocapture`

Run: `rustup run stable cargo test decimal -- --nocapture`

Run: `rustup run stable cargo test error -- --nocapture`

Expected: PASS, including ErrorCode numeric stability.

```bash
git add src/oracle_type.rs src/decimal.rs src/error.rs
git commit -m "feat: normalize Oracle scalar contracts"
```

### Task 4: Nanosecond timestamps and V1/V2 row encoding

**Files:**
- Modify: `src/datetime.rs`
- Modify: `src/oracle_type.rs`
- Modify: `src/storage/row.rs`
- Modify: `src/storage/constraint/program.rs`
- Modify: `tests/phase5_oracle_types_api.rs`
- Test: unit tests in those files

**Interfaces:**
- Consumes: Task 1 descriptors and Task 3 normalization dispatch.
- Produces: `OracleTimestamp::from_nanoseconds`, `nanoseconds()`, checked fractional rounding/calendar carry, `utc_day_and_nanos()`, and descriptor-aware row scalar encoding.

- [ ] **Step 1: Write nanosecond, carry, offset, and V1-byte tests**

```rust
fn timestamp(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanos: u32,
    offset: Option<i16>,
) -> OracleTimestamp {
    OracleTimestamp::from_nanoseconds(
        OracleDate::new(year, month, day, hour, minute, second).unwrap(),
        nanos,
        offset,
    ).unwrap()
}

fn timestamp_column(descriptor: Option<OracleTypeDescriptor>) -> ColumnDef {
    match descriptor {
        Some(descriptor) => ColumnDef::new_oracle(
            ColumnId::new(2).unwrap(),
            TableId::new(1).unwrap(),
            0,
            false,
            CatalogName::new("TS").unwrap(),
            descriptor,
        ).unwrap(),
        None => ColumnDef::new(
            ColumnId::new(2).unwrap(),
            TableId::new(1).unwrap(),
            0,
            ColumnType::Timestamp,
            false,
            CatalogName::new("TS").unwrap(),
        ).unwrap(),
    }
}

#[test]
fn nanoseconds_preserve_the_old_microsecond_api() {
    let date = OracleDate::new(2026, 1, 2, 3, 4, 5).unwrap();
    let old = OracleTimestamp::new(date, 123_456, Some(480)).unwrap();
    assert_eq!(old.nanoseconds(), 123_456_000);
    assert_eq!(old.micros(), 123_456);
    let precise = OracleTimestamp::from_nanoseconds(date, 123_456_789, None).unwrap();
    assert_eq!(precise.nanoseconds(), 123_456_789);
    assert_eq!(precise.micros(), 123_456);
    assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(840)).is_ok());
    assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(-840)).is_ok());
    assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(841)).is_err());
    assert!(OracleTimestamp::from_nanoseconds(date, 0, Some(-841)).is_err());
}

#[test]
fn timestamp_rounding_carries_across_the_calendar() {
    let value = timestamp(2024, 12, 31, 23, 59, 59, 999_999_999, None);
    let rounded = round_timestamp(value, 0).unwrap();
    assert_eq!(rounded.date(), OracleDate::new(2025, 1, 1, 0, 0, 0).unwrap());
    assert_eq!(rounded.nanoseconds(), 0);
    let maximum = timestamp(9999, 12, 31, 23, 59, 59, 999_999_999, None);
    assert_eq!(round_timestamp(maximum, 0).unwrap_err().oracle_code(), Some(1861));
}

#[test]
fn v1_timestamp_rows_keep_microsecond_bytes_and_v2_rows_store_nanoseconds() {
    let value = timestamp(2026, 1, 2, 3, 4, 5, 123_456_000, None);
    assert_eq!(&encode_column_value(&timestamp_column(None), &Value::Timestamp(value)).unwrap()[8..12], &123_456_u32.to_le_bytes());
    let precise = timestamp(2026, 1, 2, 3, 4, 5, 123_456_789, None);
    let descriptor = OracleTypeDescriptor::timestamp(9).unwrap();
    assert_eq!(&encode_column_value(&timestamp_column(Some(descriptor)), &Value::Timestamp(precise)).unwrap()[8..12], &123_456_789_u32.to_le_bytes());
}
```

- [ ] **Step 2: Run focused tests and observe missing nanosecond support**

Run: `rustup run stable cargo test datetime -- --nocapture`

Run: `rustup run stable cargo test v1_timestamp_rows_keep_microsecond_bytes_and_v2_rows_store_nanoseconds -- --exact --nocapture`

Expected: compilation fails on `from_nanoseconds`, `nanoseconds`, and descriptor-aware row encoding.

- [ ] **Step 3: Replace the stored fraction with nanoseconds while preserving old methods**

```rust
pub struct OracleTimestamp {
    date: OracleDate,
    nanos: u32,
    offset_minutes: Option<i16>,
}

impl OracleTimestamp {
    pub fn new(date: OracleDate, micros: u32, offset_minutes: Option<i16>) -> Result<Self> {
        let nanos = micros.checked_mul(1_000)
            .ok_or_else(|| type_error("microseconds exceed timestamp range"))?;
        Self::from_nanoseconds(date, nanos, offset_minutes)
    }

    pub fn from_nanoseconds(date: OracleDate, nanos: u32, offset_minutes: Option<i16>) -> Result<Self> {
        if nanos >= 1_000_000_000 { return Err(type_error("nanoseconds must be below one second")); }
        validate_fixed_offset(offset_minutes)?;
        Ok(Self { date, nanos, offset_minutes })
    }

    pub const fn micros(self) -> u32 { self.nanos / 1_000 }
    pub const fn nanoseconds(self) -> u32 { self.nanos }
}
```

Reject +/-14 offsets with nonzero minutes. Implement Gregorian next/previous-day helpers without a date/time dependency. Parse only the two strict DATE forms and two strict TIMESTAMP forms from the spec. Fraction input may contain 1 through 9 digits; pad to nanoseconds before target-p rounding.

- [ ] **Step 4: Make row and CheckProgram scalar encoding version-aware**

```rust
fn encode_column_value(column: &ColumnDef, value: &Value) -> Result<Vec<u8>> {
    match (column.oracle_type(), column.column_type(), value) {
        (None, ColumnType::Timestamp, Value::Timestamp(ts)) => {
            if ts.nanoseconds() % 1_000 != 0 { return Err(type_error("legacy Timestamp requires microsecond precision")); }
            encode_timestamp_fraction(ts, ts.nanoseconds() / 1_000)
        }
        (Some(OracleTypeDescriptor::Timestamp { .. }), ColumnType::Timestamp, Value::Timestamp(ts)) => {
            encode_timestamp_fraction(ts, ts.nanoseconds())
        }
        _ => encode_existing_column_value(column, value),
    }
}
```

Change RowCodec's internal calls to pass `&ColumnDef`; retain the V1 `encode_value(ColumnType, ...)` wrapper for existing golden-byte tests. Decode V1 fractions as `stored * 1000` and V2 fractions directly. Add a CheckProgram scalar version 2 only when a literal contains sub-microsecond Timestamp; retain literal version 1 bytes otherwise.

At the same commit, add this shared integration-test builder to `tests/phase5_oracle_types_api.rs`:

```rust
fn timestamp_value(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanos: u32,
    offset: Option<i16>,
) -> Value {
    let date = OracleDate::new(year, month, day, hour, minute, second).unwrap();
    Value::Timestamp(OracleTimestamp::from_nanoseconds(date, nanos, offset).unwrap())
}
```

- [ ] **Step 5: Run value, row, and CheckProgram regressions and commit**

Run: `rustup run stable cargo test datetime -- --nocapture`

Run: `rustup run stable cargo test storage::row -- --nocapture`

Run: `rustup run stable cargo test storage::constraint::program -- --nocapture`

Expected: PASS; all pre-Phase-5 golden bytes remain unchanged.

```bash
git add src/datetime.rs src/oracle_type.rs src/storage/row.rs src/storage/constraint/program.rs tests/phase5_oracle_types_api.rs
git commit -m "feat: preserve nanosecond Oracle timestamps"
```

### Task 5: Normalize INSERT, UPDATE, parameters, and constraints before mutation

**Files:**
- Modify: `src/api/metadata.rs`
- Modify: `src/sql/bind/types.rs`
- Modify: `src/sql/bound/statement.rs`
- Modify: `src/sql/plan/mutation.rs`
- Modify: `src/sql/exec/params.rs`
- Modify: `src/sql/exec/mutation.rs`
- Modify: `src/storage/constraint/mod.rs`
- Modify: `src/storage/relation.rs`
- Test: `tests/phase5_oracle_types_api.rs`

**Interfaces:**
- Consumes: Task 3 `normalize_for_column` and Task 4 date/time normalization.
- Produces: parameter metadata's internal `allow_oracle_text_source`, normalized complete rows before constraints/indexes, and ORA-00001/01400/12899 statement errors.

- [ ] **Step 1: Add failing CRUD, parameter, and rollback tests**

```rust
#[test]
fn crud_uses_one_normalized_row_for_literals_and_parameters() {
    let (_files, database) = support::new_database("phase5-crud");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (ID NUMBER(4,0) PRIMARY KEY, N NUMBER(5,2), C CHAR(3), V VARCHAR2(3), R RAW(2))");
    let insert = connection.prepare("INSERT INTO T VALUES (:1, :2, :3, :4, :5)").unwrap();
    connection.execute(&insert, Parameters::positional(&[
        Value::Text("1".into()).into(),
        Value::Text("12.345".into()).into(),
        Value::Text("A".into()).into(),
        Value::Text("表A".into()).into(),
        Value::Blob(vec![0, 255]).into(),
    ])).unwrap();
    assert_eq!(collect_with_params(&mut connection, "SELECT N,C,V,R FROM T WHERE ID = :1", &[Value::Text("1".into()).into()]), vec![
        vec![decimal_value(1235, 2), Value::Text("A  ".into()), Value::Text("表A".into()), Value::Blob(vec![0,255])]
    ]);
}

#[test]
fn failed_normalization_leaves_rowid_and_indexes_unchanged() {
    let (files, database) = support::new_database("phase5-rollback");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (ID NUMBER(2,0) PRIMARY KEY, N NUMBER(2,1), V VARCHAR2(1) NOT NULL)");
    let before = support::format2::raw_catalog_capture(files.main()).unwrap();
    let insert = connection.prepare("INSERT INTO T VALUES (1, 9.99, '')").unwrap();
    let error = connection.execute(&insert, Parameters::none()).unwrap_err();
    assert_eq!((error.code(), error.oracle_code()), (ErrorCode::Type, Some(1438)));
    let after = support::format2::raw_catalog_capture(files.main()).unwrap();
    assert_eq!(after.tables["T"].next_rowid, before.tables["T"].next_rowid);
    assert_eq!(after.indexes, before.indexes);
}

#[test]
fn empty_text_hits_not_null_but_empty_blob_and_space_survive() {
    let (_files, database) = support::new_database("phase5-empty");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (V VARCHAR2(2) NOT NULL, R RAW(2))");
    let insert = connection.prepare("INSERT INTO T VALUES (:1, :2)").unwrap();
    let empty = [Value::Text(String::new()).into(), Value::Blob(Vec::new()).into()];
    let error = connection.execute(&insert, Parameters::positional(&empty)).unwrap_err();
    assert_eq!((error.code(), error.oracle_code()), (ErrorCode::Constraint, Some(1400)));
    let space = [Value::Text(" ".into()).into(), Value::Blob(Vec::new()).into()];
    connection.execute(&insert, Parameters::positional(&space)).unwrap();
    assert_eq!(support::collect(&mut connection, "SELECT V,R FROM T"), vec![vec![Value::Text(" ".into()), Value::Blob(Vec::new())]]);
}
```

- [ ] **Step 2: Run the CRUD tests and confirm raw assignment values still reach storage**

Run: `rustup run stable cargo test --test phase5_oracle_types_api -- --nocapture`

Expected: FAIL with incompatible assignment/parameter types or unnormalized values.

- [ ] **Step 3: Preserve raw Text parameters only for Oracle target contexts**

```rust
pub(crate) struct ParameterMetadata {
    identifier: ParameterIdentifier,
    data_type: Option<DataType>,
    allow_oracle_text_source: bool,
}

impl ParameterMetadata {
    pub(crate) fn inferred(
        identifier: ParameterIdentifier,
        sql_type: SqlType,
        allow_oracle_text_source: bool,
    ) -> Self {
        let data_type = match sql_type {
            SqlType::Null | SqlType::Unknown => None,
            value => Some(DataType::from_sql_type(value)),
        };
        Self { identifier, data_type, allow_oracle_text_source }
    }
}
```

Extend ParameterCollector entries with the boolean, set it when a parameter is assigned to or directly compared with NUMBER/DATE/TIMESTAMP V2, and OR the flag across repeated occurrences. ParameterFrame validates Text as an accepted source only when the flag is true and keeps the owned Text unchanged; per-use target normalization remains responsible for conversion.

- [ ] **Step 4: Normalize complete INSERT/UPDATE rows before any constraint or storage write**

```rust
for (target, expression) in plan.targets().iter().zip(plan.values()) {
    let raw = eval(expression, &empty_row, params)?;
    let column = relation.column_by_ordinal(target.ordinal())?;
    values[usize::from(target.ordinal())] = normalize_for_column(column, raw)
        .map_err(|error| error.with_sql_location(target.location()))?;
}
```

For UPDATE, evaluate all assignments against the same old row, build the complete replacement, then normalize each assigned target before `writer.validate_row`, constraint checks, key encoding, or deletion of old index entries. Do not consume ROWID or open a table writer until all INSERT values are normalized and row size is validated.

- [ ] **Step 5: Attach required ORA metadata to existing constraint paths**

Map normalized NULL in a nonnullable column to Constraint/ORA-01400. Map primary/unique conflict to Constraint/ORA-00001 while retaining the stable constraint/index name. Keep CHECK/FK errors without invented Oracle numbers. Preserve existing statement checkpoint behavior.

- [ ] **Step 6: Run mutation, parameter, constraint, and public API suites and commit**

Run: `rustup run stable cargo test sql::exec::params -- --nocapture`

Run: `rustup run stable cargo test sql::exec::mutation -- --nocapture`

Run: `rustup run stable cargo test storage::constraint -- --nocapture`

Run: `rustup run stable cargo test --test phase5_oracle_types_api -- --nocapture`

Expected: PASS; legacy DECIMAL parameters still reject Text.

```bash
git add src/api/metadata.rs src/sql/bind/types.rs src/sql/bound/statement.rs src/sql/plan/mutation.rs src/sql/exec/params.rs src/sql/exec/mutation.rs src/storage/constraint/mod.rs src/storage/relation.rs tests/phase5_oracle_types_api.rs
git commit -m "feat: enforce Oracle contracts in DML"
```

### Task 6: Contract-aware expressions and CheckProgram V2

**Files:**
- Modify: `src/oracle_type.rs`
- Modify: `src/sql/bound/expr.rs`
- Modify: `src/sql/bind/types.rs`
- Modify: `src/sql/plan/expr.rs`
- Modify: `src/sql/exec/expr.rs`
- Modify: `src/storage/constraint/program.rs`
- Test: `tests/phase5_oracle_types_api.rs`

**Interfaces:**
- Consumes: normalized values, descriptors, and UTC helpers from Tasks 3-5.
- Produces: `OracleComparisonContract`, `compare_for_contract`, optional comparison contract on bound/exec binary expressions, and CheckProgram V2 opcodes 0x50..=0x55.

- [ ] **Step 1: Add failing CHAR, UTC, legacy, and CHECK tests**

```rust
fn tz_value(hour: u8, minute: u8, offset: i16) -> Value {
    let date = OracleDate::new(2026, 1, 1, hour, minute, 0).unwrap();
    Value::Timestamp(OracleTimestamp::from_nanoseconds(date, 0, Some(offset)).unwrap())
}

#[test]
fn oracle_comparisons_are_contract_aware_without_changing_legacy_timestamp() {
    let char2 = OracleTypeDescriptor::char_type(2).unwrap();
    let char3 = OracleTypeDescriptor::char_type(3).unwrap();
    let char_contract = OracleComparisonContract::new(Some(char2), Some(char3)).unwrap();
    assert_eq!(compare_for_contract(char_contract, &Value::Text("A ".into()), &Value::Text("A  ".into())).unwrap(), Ordering::Equal);

    let tz = OracleTypeDescriptor::timestamp_with_time_zone(9).unwrap();
    let utc_contract = OracleComparisonContract::new(Some(tz), Some(tz)).unwrap();
    assert_eq!(compare_for_contract(utc_contract, &tz_value(8, 0, 480), &tz_value(0, 0, 0)).unwrap(), Ordering::Equal);
    assert_ne!(sql_total_cmp(&tz_value(8, 0, 480), &tz_value(0, 0, 0)).unwrap(), Ordering::Equal);
}

#[test]
fn char_check_normalizes_its_persisted_constant() {
    let (_files, database) = support::new_database("phase5-check");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (C CHAR(3) CHECK (C = 'A'))");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES ('A')");
    assert_eq!(support::collect(&mut connection, "SELECT C FROM T"), vec![vec![Value::Text("A  ".into())]]);
}

#[test]
fn check_program_versions_only_oracle_comparisons() {
    let v1 = CheckProgramTemplate::new(vec![CheckTemplateInstruction::Literal {
        value: Value::Bool(true),
        result: CheckType::Bool,
    }]).unwrap().resolve(&[]).unwrap();
    assert_eq!(v1.as_bytes()[0], 1);

    let char_type = OracleTypeDescriptor::char_type(3).unwrap();
    let v2 = CheckProgramTemplate::new(vec![
        CheckTemplateInstruction::Literal { value: Value::Text("A  ".into()), result: CheckType::Text },
        CheckTemplateInstruction::Literal { value: Value::Text("A  ".into()), result: CheckType::Text },
        CheckTemplateInstruction::OracleCompare {
            op: CheckBinaryOp::Equal,
            contract: OracleComparisonContract::new(Some(char_type), Some(char_type)).unwrap(),
            result: CheckType::Bool,
        },
    ]).unwrap().resolve(&[]).unwrap();
    assert_eq!(v2.as_bytes()[0], 2);
}
```

- [ ] **Step 2: Run the focused comparison tests and observe V1 field-tuple behavior everywhere**

Run: `rustup run stable cargo test oracle_comparisons_are_contract_aware_without_changing_legacy_timestamp -- --nocapture`

Expected: compilation fails because comparison contracts do not exist.

- [ ] **Step 3: Add comparison contracts to bound and executable binary expressions**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OracleComparisonContract {
    left: Option<OracleTypeDescriptor>,
    right: Option<OracleTypeDescriptor>,
}

pub(crate) enum BoundExprKind {
    Binary {
        left: Box<BoundExpr>,
        op: BoundBinaryOp,
        right: Box<BoundExpr>,
        oracle_contract: Option<OracleComparisonContract>,
    },
}
```

Update every constructor and iterative tree walker explicitly. The binder derives contracts only for comparison operators. Reject V1/V2 column-to-column comparison and different Oracle families; allow NUMBER-to-NUMBER, VARCHAR2-to-VARCHAR2, CHAR-to-CHAR, CHAR-to-VARCHAR2, DATE-to-DATE, Timestamp-to-Timestamp, and TimestampWithTimeZone-to-same-family.

- [ ] **Step 4: Implement contract comparison without changing `sql_total_cmp`**

For a V2 column against a literal/parameter, normalize the noncolumn operand with that column's descriptor before comparing. NUMBER calls existing decimal comparison; CHAR/CHAR pads both to the larger declared length; CHAR/VARCHAR2 compares stored Text without extra padding; V2 timezone values compare `utc_day_and_nanos()`. Null still returns Unknown through the existing three-valued wrapper.

```rust
fn binary_with_contract(
    op: BoundBinaryOp,
    contract: OracleComparisonContract,
    left: Value,
    right: Value,
) -> Result<Value> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let order = compare_for_contract(contract, &left, &right)?;
    Ok(Value::Bool(match op {
        BoundBinaryOp::Equal => order.is_eq(),
        BoundBinaryOp::NotEqual => !order.is_eq(),
        BoundBinaryOp::LessThan => order.is_lt(),
        BoundBinaryOp::LessThanOrEqual => order.is_le(),
        BoundBinaryOp::GreaterThan => order.is_gt(),
        BoundBinaryOp::GreaterThanOrEqual => order.is_ge(),
        _ => return Err(DbError::new(ErrorCode::Internal, "Oracle contract attached to non-comparison")),
    }))
}
```

- [ ] **Step 5: Add CheckProgram V2 Oracle comparison instructions**

Reserve 0x50 Equal, 0x51 NotEqual, 0x52 Less, 0x53 LessEqual, 0x54 Greater, and 0x55 GreaterEqual. Each instruction has two operands, Bool result, and a 16-byte payload containing two 8-byte descriptor encodings `(kind, precision, scale, reserved=0, length u32 LE)`. A template containing any Oracle comparison emits program version 2; otherwise it emits byte-identical V1. Both versions reject unknown opcodes, nonzero reserved bytes, invalid descriptors, wrong operand types, and trailing payload.

- [ ] **Step 6: Run expression, CHECK, and API regressions and commit**

Run: `rustup run stable cargo test sql::bind::types -- --nocapture`

Run: `rustup run stable cargo test sql::plan::expr -- --nocapture`

Run: `rustup run stable cargo test sql::exec::expr -- --nocapture`

Run: `rustup run stable cargo test storage::constraint::program -- --nocapture`

Run: `rustup run stable cargo test --test phase5_oracle_types_api -- --nocapture`

Expected: PASS; legacy TimestampWithTimeZone comparison tests remain unchanged.

```bash
git add src/oracle_type.rs src/sql/bound/expr.rs src/sql/bind/types.rs src/sql/plan/expr.rs src/sql/exec/expr.rs src/storage/constraint/program.rs tests/phase5_oracle_types_api.rs
git commit -m "feat: compare Oracle values by column contract"
```

### Task 7: V2 index keys and normalized IndexScan bounds

**Files:**
- Modify: `src/storage/index/key.rs`
- Modify: `src/storage/index/scan.rs`
- Modify: `src/sql/plan/index.rs`
- Modify: `src/sql/exec/scan.rs`
- Modify: `src/sql/exec/mutation.rs`
- Test: `tests/phase5_oracle_types_api.rs`

**Interfaces:**
- Consumes: descriptor-bearing ColumnDef, Task 3 normalization, Task 4 UTC/day helpers, and Task 6 predicate contracts.
- Produces: descriptor-aware IndexKey components; `IndexAccessPlan` carrying ColumnDef identity/descriptor; scan bounds normalized exactly once before key encoding.

- [ ] **Step 1: Write failing key-order, uniqueness, and TableScan/IndexScan parity tests**

```rust
#[test]
fn timezone_index_uses_utc_prefix_but_rows_keep_offsets() {
    let (_files, database) = support::new_database("phase5-tz-index");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (TS TIMESTAMP(9) WITH TIME ZONE UNIQUE)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES ('2026-01-01 08:00:00.123456789 +08:00')");
    let duplicate = connection.prepare("INSERT INTO T VALUES ('2026-01-01 00:00:00.123456789 +00:00')").unwrap();
    let error = connection.execute(&duplicate, Parameters::none()).unwrap_err();
    assert_eq!((error.code(), error.oracle_code()), (ErrorCode::Constraint, Some(1)));
    assert_eq!(support::collect(&mut connection, "SELECT TS FROM T"), vec![vec![
        timestamp_value(2026, 1, 1, 8, 0, 0, 123_456_789, Some(480))
    ]]);
}

#[test]
fn scan_and_filter_share_number_char_and_timestamp_normalization() {
    let (_files, database) = support::new_database("phase5-index-parity");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (ID NUMBER(4,0), N NUMBER(5,2), C CHAR(3), TZ TIMESTAMP(9) WITH TIME ZONE)");
    support::execute_sql(&mut connection, "CREATE INDEX T_N ON T(N)");
    support::execute_sql(&mut connection, "CREATE INDEX T_C ON T(C)");
    support::execute_sql(&mut connection, "CREATE INDEX T_TZ ON T(TZ)");
    support::execute_sql(&mut connection, "INSERT INTO T VALUES (1, 1.2, 'A', '2026-01-01 08:00:00.1 +08:00')");

    for (indexed, scanned, value) in [
        ("SELECT ID FROM T WHERE N = :1", "SELECT ID FROM T WHERE N = :1 OR FALSE", Value::Text("1.20".into())),
        ("SELECT ID FROM T WHERE C = :1", "SELECT ID FROM T WHERE C = :1 OR FALSE", Value::Text("A".into())),
        ("SELECT ID FROM T WHERE TZ = :1", "SELECT ID FROM T WHERE TZ = :1 OR FALSE", Value::Text("2026-01-01 00:00:00.1 +00:00".into())),
    ] {
        let parameters = [value.into()];
        assert_eq!(
            collect_with_params(&mut connection, indexed, &parameters),
            collect_with_params(&mut connection, scanned, &parameters),
        );
    }
}
```

- [ ] **Step 2: Run the index tests and confirm current keys use microseconds/local fields**

Run: `rustup run stable cargo test --test phase5_oracle_types_api -- --nocapture`

Expected: FAIL through incorrect uniqueness, unrepresentable bound, or scan/filter mismatch.

- [ ] **Step 3: Branch IndexKey encoding by ColumnDef descriptor**

```rust
match (column.oracle_type(), column.column_type(), value) {
    (Some(OracleTypeDescriptor::Timestamp { .. }), ColumnType::Timestamp, Value::Timestamp(ts)) => {
        encode_date(ts.date(), out);
        out.extend_from_slice(&ts.nanoseconds().to_be_bytes());
    }
    (Some(OracleTypeDescriptor::TimestampWithTimeZone { .. }), ColumnType::TimestampWithTimeZone, Value::Timestamp(ts)) => {
        let (day, nanos) = ts.utc_day_and_nanos()?;
        out.extend_from_slice(&((day as u32) ^ 0x8000_0000).to_be_bytes());
        out.extend_from_slice(&nanos.to_be_bytes());
    }
    _ => encode_v1_scalar(column.column_type(), value, out)?,
}
```

Keep Decimal/Text/Blob/Date and V1 timestamp key bytes unchanged. V2 timezone decoding returns an internal validated `IndexScalar::UtcTimestamp { day, nanos }`; ROWID extraction and table-row re-encoding validation must not require constructing a public out-of-range date.

```rust
enum IndexScalar {
    Value(Value),
    UtcTimestamp { day: i32, nanos: u64 },
}

pub(crate) struct DecodedIndexKey {
    components: Box<[IndexScalar]>,
    rowid: RowId,
}
```

Use `IndexScalar::Value` for every V1 and non-timezone V2 component. Existing callers that need public values receive them only when every component is `Value`; index scans need only `rowid()` and physical-key reproduction.

- [ ] **Step 4: Carry descriptors into index choices and normalize bounds once**

Replace `(ColumnType, EqualityValuePlan)` with `IndexColumnContract { column_type, oracle_type }` plus the value plan. In `evaluate_bounds`, call `normalize_for_type` for V2 and the existing lossless Int64-to-Decimal logic for V1. Store the normalized `Value` in `IndexScanRange`; both representability checks and key building consume it without further conversion.

```rust
#[derive(Clone, Copy)]
pub(crate) struct IndexColumnContract {
    column_type: ColumnType,
    oracle_type: Option<OracleTypeDescriptor>,
}

impl IndexColumnContract {
    pub(crate) const fn column_type(self) -> ColumnType { self.column_type }
    pub(crate) const fn oracle_type(self) -> Option<OracleTypeDescriptor> { self.oracle_type }
}

fn normalize_bound(value: Value, column: IndexColumnContract) -> Result<Value> {
    match column.oracle_type() {
        Some(descriptor) => normalize_for_type(descriptor, value),
        None => normalize_legacy_bound(value, column.column_type()),
    }
}
```

- [ ] **Step 5: Add key corruption and edge-order tests**

Cover V2 nanoseconds 0 and 999,999,999; UTC day -1 and final+1; +/-14:00; truncation at every byte; noncanonical day/nanos; same-instant different-offset prefixes; and exact 1024-byte key acceptance/1025-byte ResourceLimit. Require decode/re-encode stability for every reconstructible component and table-row reproduction for UTC internal components.

- [ ] **Step 6: Run index, planner, mutation, and API suites and commit**

Run: `rustup run stable cargo test storage::index -- --nocapture`

Run: `rustup run stable cargo test sql::plan::index -- --nocapture`

Run: `rustup run stable cargo test sql::exec::scan -- --nocapture`

Run: `rustup run stable cargo test sql::exec::mutation -- --nocapture`

Run: `rustup run stable cargo test --test phase5_oracle_types_api -- --nocapture`

Expected: PASS with both IndexScan and TableScan observation counters nonzero in parity tests.

```bash
git add src/storage/index/key.rs src/storage/index/scan.rs src/sql/plan/index.rs src/sql/exec/scan.rs src/sql/exec/mutation.rs tests/phase5_oracle_types_api.rs
git commit -m "feat: index normalized Oracle values"
```

### Task 8: Compatibility, recovery, CI, and 100,000-row acceptance

**Files:**
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/test_support.rs`
- Modify: `tests/support/format2.rs`
- Modify: `tests/support/mod.rs`
- Modify: `tests/phase5_oracle_types_api.rs`
- Create: `tests/phase5_oracle_types_scale.rs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: all prior tasks' stable catalog, normalization, row, CHECK, and index contracts.
- Produces: real-file compatibility witnesses, first-feature crash matrix, public reopen coverage, ignored 100k acceptance test, and the minimal fast CI gate.

- [ ] **Step 1: Harden the independent format-2 inspector**

Use the `OracleColumnCapture` introduced in Task 2 and extend its tests so envelope version and the exact V2 payload are decoded without calling production catalog decoders:

```rust
pub struct OracleColumnCapture {
    pub physical_type: u8,
    pub kind: u8,
    pub precision: u8,
    pub scale: i8,
    pub length: u32,
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
    pub oracle_columns: BTreeMap<String, OracleColumnCapture>,
}
```

Reject malformed V2 capture input in the helper instead of copying production acceptance logic. Keep all existing Phase 4 capture fields and tests.

- [ ] **Step 2: Add real-file old/new compatibility and corruption tests**

```rust
#[test]
fn v1_database_opens_without_rewrite_and_v2_requires_the_feature_bit() {
    let (files, database) = support::new_database("phase5-format");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE LEGACY (ID INT64, TS TIMESTAMP)");
    let insert = connection.prepare("INSERT INTO LEGACY VALUES (1, :1)").unwrap();
    let date = OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap();
    let values = [Value::Timestamp(OracleTimestamp::new(date, 0, None).unwrap()).into()];
    connection.execute(&insert, Parameters::positional(&values)).unwrap();
    drop(connection);
    drop(database);
    let before = std::fs::read(files.main()).unwrap();
    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    assert_eq!(support::collect(&mut connection, "SELECT ID FROM LEGACY"), vec![vec![Value::Int64(1)]]);
    drop(connection);
    drop(reopened);
    assert_eq!(std::fs::read(files.main()).unwrap(), before);

    let reopened = support::reopen(&files);
    let mut connection = reopened.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE ORACLE_T (N NUMBER(5,2))");
    drop(connection);
    drop(reopened);
    assert_eq!(support::format2::raw_catalog_capture(files.main()).unwrap().feature_bits & 4, 4);
}
```

In `src/storage/catalog/mod.rs`, use the existing real-file mutation helpers to clear bit 2 from a complete V2 catalog and require Corruption on open. Also test bit set/no V2 records as legal after DROP TABLE, an unknown ColumnDef version as Unsupported, invalid descriptor fields as Corruption, and V1/V2 columns in one table across reopen/checkpoint. Every mutator must restamp the affected checksum and target only its owned temporary database.

- [ ] **Step 3: Add first-V2-create WAL fault injection**

Reuse the existing catalog/WAL fault harness. For every directory write, page frame, Commit, WAL sync, in-memory publish, and directory sync boundary: create the first Oracle table, inject one failure, reopen, and assert exactly one of:

```rust
assert!(
    capture.feature_bits & 4 == 0 && !capture.tables.contains_key("T")
    || capture.feature_bits & 4 == 4 && capture.tables.contains_key("T") && capture.oracle_columns.contains_key("T.N")
);
```

Never accept a feature bit without a complete table graph or a V2 record without the bit. Include V2 row plus unique-index mutation failures after the schema exists.

- [ ] **Step 4: Add the ignored 100,000-row public acceptance test**

```rust
const ROWS: i64 = 100_000;

fn scale_parameters(id: i64) -> [ParameterValue; 5] {
    let date = OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap();
    let timestamp = OracleTimestamp::from_nanoseconds(
        date,
        u32::try_from(id % 1_000_000_000).unwrap(),
        None,
    ).unwrap();
    [
        Value::Text(id.to_string()).into(),
        Value::Text(format!("{}.25", id % 10_000)).into(),
        Value::Text(format!("C{:07}", id % 10_000_000)).into(),
        Value::Timestamp(timestamp).into(),
        Value::Blob(id.to_be_bytes().repeat(2)).into(),
    ]
}

fn assert_count(connection: &mut Connection, expected: i64) {
    assert_eq!(support::collect(connection, "SELECT COUNT(*) FROM T"), vec![vec![Value::Int64(expected)]]);
}

#[test]
#[ignore = "manual Phase 5 MVP scale acceptance"]
fn phase5_hundred_thousand_row_oracle_type_smoke() {
    let (files, database) = support::new_database("phase5-100k");
    let mut connection = database.connect().unwrap();
    support::execute_sql(&mut connection, "CREATE TABLE T (ID NUMBER(10,0) PRIMARY KEY, N NUMBER(12,2), C CHAR(8), TS TIMESTAMP(9), R RAW(16))");
    let insert = connection.prepare("INSERT INTO T VALUES (:1,:2,:3,:4,:5)").unwrap();
    for id in 0..ROWS {
        let values = scale_parameters(id);
        connection.execute(&insert, Parameters::positional(&values)).unwrap();
    }
    assert_count(&mut connection, ROWS);
    support::execute_sql(&mut connection, "UPDATE T SET N = N + 1 WHERE ID BETWEEN 40000 AND 49999");
    support::execute_sql(&mut connection, "DELETE FROM T WHERE ID >= 90000");
    assert_count(&mut connection, 90_000);
    drop(connection);
    drop(database);
    let reopened = Database::open(files.main(), support::scale_options()).unwrap();
    let mut connection = reopened.connect().unwrap();
    assert_count(&mut connection, 90_000);
    assert_eq!(support::collect(&mut connection, "SELECT ID FROM T WHERE ID = 45000"), vec![vec![decimal_value(45_000, 0)]]);
    assert!(support::collect(&mut connection, "SELECT ID FROM T WHERE ID = 95000").is_empty());
}
```

Do not add a million-row or 10-million-row Phase 5 test. Do not put this ignored test in normal CI.

- [ ] **Step 5: Add only the fast Phase 5 API target to CI**

Change the existing fast test command to:

```yaml
- name: Public API integration tests
  run: rustup run stable cargo test --test public_api --test phase4c_api --test phase4c2_constraint_api --test phase5_oracle_types_api
```

Do not add a new matrix, deep workflow job, or scale invocation.

- [ ] **Step 6: Run the final verification matrix**

Run: `rustup run stable cargo fmt --all -- --check`

Run: `rustup run stable cargo test --all-targets`

Run: `rustup run 1.85.0 cargo test --all-targets`

Run once manually: `rustup run stable cargo test --profile exhaustive --test phase5_oracle_types_scale phase5_hundred_thousand_row_oracle_type_smoke -- --exact --ignored --nocapture`

Expected: all executed tests pass; the manual scale test processes exactly 100,000 rows and reopens successfully.

- [ ] **Step 7: Commit the compatibility and acceptance gate**

```bash
git add src/storage/catalog/mod.rs src/storage/test_support.rs tests/support/format2.rs tests/support/mod.rs tests/phase5_oracle_types_api.rs tests/phase5_oracle_types_scale.rs .github/workflows/ci.yml
git commit -m "test: verify Phase 5 Oracle type compatibility"
```

### Task 9: Whole-branch review and release evidence

**Files:**
- Modify only files required to correct findings from review
- Verify: all files changed since the Phase 5 implementation base

**Interfaces:**
- Consumes: Tasks 1-8 completed commits.
- Produces: a clean branch whose implementation, tests, and evidence match the approved spec without non-goal behavior.

- [ ] **Step 1: Review the branch against the spec**

Run: `git diff --stat 5f00ccc..HEAD`

Run: `git diff --check 5f00ccc..HEAD`

Check each spec exit condition against a named test. Specifically inspect that V1 encoding branches never consult V2 units, every DML/index path uses the central normalizer, ORA codes do not change ErrorCode/fatality, and no excluded SQL feature was added.

- [ ] **Step 2: Run final stable, MSRV, and Windows-equivalent local checks**

Run: `rustup run stable cargo fmt --all -- --check`

Run: `rustup run stable cargo test --all-targets`

Run: `rustup run 1.85.0 cargo test --all-targets`

On Windows PowerShell, run the same three commands from the worktree root. Linux stable and MSRV run in GitHub Actions after push.

- [ ] **Step 3: Correct review findings with focused tests first**

For each finding, add the smallest failing test to the task-owning module, run that exact test to see the failure, apply the minimal correction, rerun the focused test, then rerun `cargo test --all-targets`. Do not add refactors or features unrelated to a demonstrated finding.

- [ ] **Step 4: Commit only if review produced changes**

```bash
git add src tests .github/workflows/ci.yml
git commit -m "fix: address Phase 5 final review"
```

If review produces no file changes, record the clean commit ID and test results in the pull request instead of creating an empty commit.
