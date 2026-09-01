# ORA-SQLLite Phase 1 Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and verify the stable Rust value types, error model, binary disk primitives, versioned database/page headers, checksums, and fault-injection contract required by later storage work.

**Architecture:** Public Oracle-compatible values and errors live at the crate boundary; disk encoding and fault hooks remain internal. All persisted integers use fixed little-endian encoding, no Rust struct is written to disk by memory layout, and every decoder rejects truncated or invalid data without panic.

**Tech Stack:** Rust stable, edition 2024, standard library only, Cargo test/clippy/rustfmt.

**Spec:** `docs/superpowers/specs/2026-09-01-rust-oracle-compatible-embedded-db-design.md`

## Global Constraints

- Rust edition is 2024 and MSRV is 1.85.
- Keep `#![forbid(unsafe_code)]` enabled.
- Add no runtime or development dependencies in phase 1.
- Support decimal precision 1–38 and scale -38–38.
- Support database page sizes 4096, 8192, and 16384 bytes only.
- Use fixed little-endian disk encoding and 64-bit page identifiers.
- Use CRC32C polynomial `0x82F63B78` for corruption detection; it is not a security hash.
- Do not implement SQL, Pager, B+Tree mutation, WAL, file I/O, or application database APIs in this phase.
- Each task follows red-green-refactor and ends with a clean commit.

## File Map

- `src/lib.rs`: crate policy and public exports.
- `src/error.rs`: stable error categories, codes, and diagnostic structure.
- `src/value.rs`: public scalar value enum.
- `src/decimal.rs`: validated 38-digit decimal representation.
- `src/datetime.rs`: validated Oracle DATE and TIMESTAMP representations.
- `src/storage/mod.rs`: private storage primitive routing.
- `src/storage/checksum.rs`: dependency-free CRC32C.
- `src/storage/codec.rs`: bounds-checked little-endian encoder and decoder.
- `src/storage/header.rs`: 128-byte database header format.
- `src/storage/page.rs`: page identifiers, page types, and 48-byte page header format.
- `src/fault.rs`: internal fault points and injectable failure interface.

---

### Task 1: Install and verify the Rust toolchain

**Files:**
- Existing: `Cargo.toml`
- Existing: `rust-toolchain.toml`
- Existing: `src/lib.rs`

**Interfaces:**
- Consumes: Windows host with `winget` or an existing rustup installation.
- Produces: `rustc`, `cargo`, `rustfmt`, and `clippy` available for the repository.

- [ ] **Step 1: Confirm the current failure**

Run:

```powershell
cargo --version
rustc --version
```

Expected before installation: both commands report that the executable is not found.

- [ ] **Step 2: Install rustup when absent**

Run only after explicit approval for the machine-level installation:

```powershell
winget install --id Rustlang.Rustup --exact --accept-package-agreements --accept-source-agreements
```

Expected: winget reports a successful Rustup installation.

- [ ] **Step 3: Load the installed tools and install required components**

Run in a new PowerShell process, or address the binaries directly through `%USERPROFILE%\.cargo\bin`:

```powershell
rustup toolchain install stable --profile minimal --component rustfmt --component clippy
rustup default stable
cargo --version
rustc --version
```

Expected: Cargo and rustc print versions at or above 1.85.

- [ ] **Step 4: Verify the bootstrap crate**

Run:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Expected: all commands exit 0.

### Task 2: Add the stable error model

**Files:**
- Modify: `src/lib.rs`
- Create: `src/error.rs`
- Test: unit tests in `src/error.rs`

**Interfaces:**
- Produces: `pub type Result<T> = std::result::Result<T, DbError>`.
- Produces: `ErrorCode::{Syntax, Bind, Type, Constraint, Busy, Transaction, Unsupported, Corruption, Io, Internal}` with stable numeric values 1000–1900.
- Produces: `DbError::new(code: ErrorCode, message: impl Into<String>)`, `DbError::with_oracle_code(u16)`, `DbError::code()`, and `DbError::oracle_code()`.

- [ ] **Step 1: Write failing error tests**

Declare `pub mod error;` in `src/lib.rs`, then add to `src/error.rs` before the implementation:

```rust
#[cfg(test)]
mod tests {
    use super::{DbError, ErrorCode};

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(ErrorCode::Type.as_u32(), 1200);
        assert_eq!(ErrorCode::Corruption.as_u32(), 1700);
    }

    #[test]
    fn oracle_code_is_optional_metadata() {
        let error = DbError::new(ErrorCode::Constraint, "duplicate key")
            .with_oracle_code(1);
        assert_eq!(error.code(), ErrorCode::Constraint);
        assert_eq!(error.oracle_code(), Some(1));
        assert_eq!(error.to_string(), "[1300] duplicate key");
    }
}
```

- [ ] **Step 2: Run the tests and observe the compile failure**

Run: `cargo test --lib error::tests`

Expected: compilation fails because `DbError` and `ErrorCode` are undefined.

- [ ] **Step 3: Implement the exact error surface**

Implement `ErrorCode` as `#[repr(u32)]`, derive `Debug, Clone, Copy, PartialEq, Eq`, and return `*self as u32` from `as_u32`. Implement `DbError` with private fields `code: ErrorCode`, `message: String`, and `oracle_code: Option<u16>`. Implement `Display` with `write!(f, "[{}] {}", self.code.as_u32(), self.message)` and implement `std::error::Error`.

Export from `src/lib.rs`:

```rust
pub mod error;

pub use error::{DbError, ErrorCode, Result};
```

- [ ] **Step 4: Verify the error model**

Run:

```powershell
cargo test --lib error::tests
cargo clippy --all-targets -- -D warnings
```

Expected: tests and clippy pass.

- [ ] **Step 5: Commit**

```powershell
git add src/lib.rs src/error.rs
git commit -m "feat: define stable database errors"
```

### Task 3: Add validated Oracle-compatible scalar values

**Files:**
- Modify: `src/lib.rs`
- Create: `src/decimal.rs`
- Create: `src/datetime.rs`
- Create: `src/value.rs`
- Test: unit tests in the three new modules

**Interfaces:**
- Produces: `Decimal::new(negative: bool, scale: i8, magnitude: u128) -> Result<Decimal>` and read-only accessors.
- Produces: `OracleDate::new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Result<OracleDate>`.
- Produces: `OracleTimestamp::new(date: OracleDate, micros: u32, offset_minutes: Option<i16>) -> Result<OracleTimestamp>`.
- Produces: `Value::{Null, Int64, Decimal, Float64, Text, Blob, Date, Timestamp, Bool}`.

- [ ] **Step 1: Write failing decimal tests**

Declare `mod decimal;` in `src/lib.rs`, then create `src/decimal.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::Decimal;

    #[test]
    fn accepts_38_digit_magnitude() {
        let value = Decimal::new(false, 2, 99_999_999_999_999_999_999_999_999_999_999_999_999);
        assert!(value.is_ok());
    }

    #[test]
    fn rejects_more_than_38_digits_and_invalid_scale() {
        assert!(Decimal::new(false, 0, 100_000_000_000_000_000_000_000_000_000_000_000_000).is_err());
        assert!(Decimal::new(false, 39, 1).is_err());
    }
}
```

- [ ] **Step 2: Write failing date and timestamp tests**

Declare `mod datetime;` in `src/lib.rs`, then create `src/datetime.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::{OracleDate, OracleTimestamp};

    #[test]
    fn validates_calendar_and_time_fields() {
        assert!(OracleDate::new(2024, 2, 29, 23, 59, 59).is_ok());
        assert!(OracleDate::new(2023, 2, 29, 0, 0, 0).is_err());
        assert!(OracleDate::new(2026, 1, 1, 24, 0, 0).is_err());
    }

    #[test]
    fn validates_fraction_and_fixed_offset() {
        let date = OracleDate::new(2026, 9, 1, 12, 0, 0).expect("valid date");
        assert!(OracleTimestamp::new(date, 999_999, Some(840)).is_ok());
        assert!(OracleTimestamp::new(date, 1_000_000, None).is_err());
        assert!(OracleTimestamp::new(date, 0, Some(841)).is_err());
    }
}
```

- [ ] **Step 3: Run the tests and observe missing-type failures**

Run: `cargo test --lib`

Expected: compilation fails because the value types are undefined.

- [ ] **Step 4: Implement validation**

`Decimal` stores `negative: bool`, `scale: i8`, and `magnitude: u128`. Reject scales outside -38..=38 and magnitude values greater than `10^38 - 1`. Normalize negative zero to positive zero.

`OracleDate` stores the six constructor fields. Validate leap years using `(year % 4 == 0 && year % 100 != 0) || year % 400 == 0`, the month-specific day count, year 1–9999, hour 0–23, and minute/second 0–59.

`OracleTimestamp` stores `date`, `micros`, and `offset_minutes`. Reject microseconds above 999999 and offsets outside -840..=840 minutes.

All invalid constructors return `DbError` with `ErrorCode::Type`.

- [ ] **Step 5: Define and export `Value`**

Implement:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Decimal(crate::Decimal),
    Float64(f64),
    Text(String),
    Blob(Vec<u8>),
    Date(crate::OracleDate),
    Timestamp(crate::OracleTimestamp),
    Bool(bool),
}
```

Export `Decimal`, `OracleDate`, `OracleTimestamp`, and `Value` from `src/lib.rs`.

- [ ] **Step 6: Verify values and commit**

Run:

```powershell
cargo fmt --check
cargo test --lib
cargo clippy --all-targets -- -D warnings
```

Expected: all pass.

Commit:

```powershell
git add src/lib.rs src/decimal.rs src/datetime.rs src/value.rs
git commit -m "feat: add validated scalar values"
```

### Task 4: Add checksummed little-endian disk primitives

**Files:**
- Create: `src/storage/mod.rs`
- Create: `src/storage/checksum.rs`
- Create: `src/storage/codec.rs`
- Modify: `src/lib.rs`
- Test: unit tests in `checksum.rs` and `codec.rs`

**Interfaces:**
- Produces internally: `crc32c(bytes: &[u8]) -> u32`.
- Produces internally: `Encoder::{new, put_u8, put_u16, put_i16, put_u32, put_u64, put_u128, put_bytes, into_bytes}`.
- Produces internally: `Decoder::new(input)`, matching numeric getters, `get_bytes(len)`, `position()`, and `remaining()`.

- [ ] **Step 1: Write the CRC32C failing test**

Create `src/storage/mod.rs` with `mod checksum;`, then create `src/storage/checksum.rs` with:

```rust
#[test]
fn crc32c_matches_standard_vector() {
    assert_eq!(super::crc32c(b"123456789"), 0xe306_9283);
}
```

- [ ] **Step 2: Implement dependency-free CRC32C**

Run `cargo test --lib storage::checksum` first. Expected: compilation fails because `crc32c` is undefined.

Use initial value `!0u32`. For each byte, XOR it into the low byte of the CRC, perform eight iterations using reflected polynomial `0x82f6_3b78`, then return the bitwise complement. Do not use lookup tables or unsafe optimization in phase 1.

- [ ] **Step 3: Write codec failing tests**

Add `mod codec;` to `src/storage/mod.rs`, then create `src/storage/codec.rs` with:

```rust
#[test]
fn numeric_round_trip_is_little_endian() {
    let mut encoder = Encoder::new();
    encoder.put_u16(0x1234);
    encoder.put_u64(0x0102_0304_0506_0708);
    let bytes = encoder.into_bytes();
    assert_eq!(&bytes[..2], &[0x34, 0x12]);
    let mut decoder = Decoder::new(&bytes);
    assert_eq!(decoder.get_u16().expect("u16"), 0x1234);
    assert_eq!(decoder.get_u64().expect("u64"), 0x0102_0304_0506_0708);
    assert_eq!(decoder.remaining(), 0);
}

#[test]
fn truncated_input_returns_corruption() {
    let mut decoder = Decoder::new(&[1, 2, 3]);
    let error = decoder.get_u64().expect_err("truncated data must fail");
    assert_eq!(error.code(), crate::ErrorCode::Corruption);
}
```

- [ ] **Step 4: Implement the encoder and decoder**

Run `cargo test --lib storage::codec` first. Expected: compilation fails because `Encoder` and `Decoder` are undefined.

Use only each integer type's `to_le_bytes` and `from_le_bytes`. Decoder reads call `checked_add` before slicing and return `DbError::new(ErrorCode::Corruption, "truncated disk value")` when the range exceeds the input. `get_bytes` returns a borrowed slice and never allocates.

The completed private module routing is:

```rust
mod checksum;
mod codec;
```

Declare `mod storage;` in `src/lib.rs`; do not publicly re-export storage internals.

- [ ] **Step 5: Verify and commit**

Run:

```powershell
cargo test storage:: --lib
cargo clippy --all-targets -- -D warnings
```

Expected: all pass.

Commit:

```powershell
git add src/lib.rs src/storage/mod.rs src/storage/checksum.rs src/storage/codec.rs
git commit -m "feat: add disk encoding primitives"
```

### Task 5: Add versioned database and page headers

**Files:**
- Modify: `src/storage/mod.rs`
- Create: `src/storage/header.rs`
- Create: `src/storage/page.rs`
- Test: unit tests in both files

**Interfaces:**
- Produces internally: `PageId(u64)` and `Lsn(u64)`.
- Produces internally: `DatabaseHeader::new(page_size: u32, uuid: [u8; 16]) -> Result<Self>`, `encode() -> [u8; 128]`, and `decode(&[u8]) -> Result<Self>`.
- Produces internally: `PageType::{Catalog, TableInternal, TableLeaf, IndexInternal, IndexLeaf, Overflow, Freelist}`.
- Produces internally: `PageHeader::encode() -> [u8; 48]` and `PageHeader::decode(&[u8]) -> Result<Self>`.

- [ ] **Step 1: Write database header failing tests**

Add `mod header;` to `src/storage/mod.rs`, then create `src/storage/header.rs` with:

```rust
#[test]
fn database_header_round_trips() {
    let header = DatabaseHeader::new(4096, [7; 16]).expect("valid header");
    let bytes = header.encode();
    assert_eq!(&bytes[..16], b"ORA-SQLLITE\0\0\0\0\0");
    assert_eq!(DatabaseHeader::decode(&bytes).expect("decode"), header);
}

#[test]
fn database_header_rejects_corruption_and_page_size() {
    assert!(DatabaseHeader::new(2048, [1; 16]).is_err());
    let mut bytes = DatabaseHeader::new(8192, [1; 16]).expect("header").encode();
    bytes[32] ^= 1;
    assert_eq!(DatabaseHeader::decode(&bytes).expect_err("checksum").code(), crate::ErrorCode::Corruption);
}
```

- [ ] **Step 2: Implement the 128-byte database header**

Run `cargo test --lib storage::header` first. Expected: compilation fails because `DatabaseHeader` is undefined.

Encode fields at fixed offsets: magic 0–15, major 16–17, minor 18–19, page size 20–23, UUID 24–39, catalog root 40–47, freelist root 48–55, checkpoint LSN 56–63, features 64–71, reserved zeros 72–123, CRC32C 124–127. Version is 1.0. Compute CRC32C over bytes 0–123.

Reject wrong magic, wrong CRC, major other than 1, nonzero unknown feature bits, nonzero reserved bytes, or page sizes outside 4096/8192/16384 with `ErrorCode::Corruption`; constructor input errors use `ErrorCode::Type`.

Run `cargo test --lib storage::header` after implementation. Expected: both header tests pass.

- [ ] **Step 3: Write page header failing tests**

Add `mod page;` to `src/storage/mod.rs`, then create `src/storage/page.rs` with:

```rust
#[test]
fn page_header_round_trips() {
    let header = PageHeader::new(PageType::TableLeaf, PageId(42), 3, 4096).expect("page header");
    let bytes = header.encode();
    assert_eq!(PageHeader::decode(&bytes, 4096).expect("decode"), header);
}

#[test]
fn page_header_rejects_invalid_free_space_bounds() {
    let error = PageHeader::new(PageType::TableLeaf, PageId(1), 1, 40).expect_err("small page");
    assert_eq!(error.code(), crate::ErrorCode::Corruption);
}
```

- [ ] **Step 4: Implement the 48-byte page header**

Run `cargo test --lib storage::page` first. Expected: compilation fails because `PageHeader`, `PageType`, and `PageId` are undefined.

Encode fields at fixed offsets: page type 0, flags 1, reserved 2–3, page ID 4–11, generation 12–19, cell count 20–21, slot end 22–23, payload start 24–25, reserved 26–27, right sibling page ID 28–35, checksum 36–39, reserved zeros 40–47.

`PageHeader::new` starts with zero cells, `slot_end = 48`, `payload_start = page_size`, no right sibling, and checksum zero. Reject unknown page types, nonzero reserved values, `slot_end < 48`, `payload_start > page_size`, or `slot_end > payload_start` as corruption.

Run `cargo test --lib storage::page` after implementation. Expected: both page tests pass.

- [ ] **Step 5: Verify and commit**

Run:

```powershell
cargo fmt --check
cargo test storage:: --lib
cargo clippy --all-targets -- -D warnings
```

Expected: all pass.

Commit:

```powershell
git add src/storage/header.rs src/storage/page.rs
git commit -m "feat: define versioned disk headers"
```

### Task 6: Add fault-injection boundaries and complete phase verification

**Files:**
- Create: `src/fault.rs`
- Modify: `src/lib.rs`
- Test: unit tests in `src/fault.rs`
- Modify: `README.md`

**Interfaces:**
- Produces internally: `FaultPoint::{BeforeFileWrite, AfterFileWrite, BeforeFileSync, AfterFileSync, BeforeCheckpointPublish, AfterCheckpointPublish}`.
- Produces internally: trait `FaultInjector: Send + Sync { fn check(&self, point: FaultPoint) -> Result<()>; }`.
- Produces internally: zero-sized `NoFaults` implementation that always succeeds.

- [ ] **Step 1: Write the failing fault-hook tests**

Add `mod fault;` to `src/lib.rs`, then create `src/fault.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::{FaultInjector, FaultPoint, NoFaults};

    #[test]
    fn production_default_never_injects() {
        let injector = NoFaults;
        for point in [
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileSync,
            FaultPoint::AfterFileSync,
            FaultPoint::BeforeCheckpointPublish,
            FaultPoint::AfterCheckpointPublish,
        ] {
            injector.check(point).expect("NoFaults must succeed");
        }
    }
}
```

- [ ] **Step 2: Implement the internal fault contract**

Run `cargo test --lib fault::tests` first. Expected: compilation fails because `FaultInjector`, `FaultPoint`, and `NoFaults` are undefined.

Derive `Debug, Clone, Copy, PartialEq, Eq, Hash` for `FaultPoint`. Implement the trait and `NoFaults`. Add `mod fault;` to `src/lib.rs` without public export. Later file and WAL code must receive an `Arc<dyn FaultInjector>` rather than checking environment variables directly.

- [ ] **Step 3: Update status documentation**

Change the README status to state that phase 1 provides validated values, stable errors, disk codecs, versioned headers, checksums, and fault boundaries, while Pager, B+Tree, WAL, SQL, and the public database API remain unimplemented.

- [ ] **Step 4: Run the full phase gate**

Run:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 5: Commit and push**

```powershell
git add src/lib.rs src/fault.rs README.md
git commit -m "test: add storage fault boundaries"
git push origin main
```

Expected: push succeeds and `git status --short --branch` reports `main...origin/main` with no changed files.
