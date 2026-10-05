# Phase 6 Rust Developer Preview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a GitHub-ready `v0.1.0` Rust developer preview whose existing public API is documented, demonstrated, packaged, and validated without adding database features.

**Architecture:** Keep the engine and format-2 implementation unchanged. Build the developer-facing layer around the current synchronous public API: rustdoc contracts, two executable examples with strictly owned temporary files, evidence-bounded guides, an explicit Cargo package allowlist, and a manually dispatched read-only release-preview workflow.

**Tech Stack:** Rust 1.85+, Cargo/rustdoc, the existing `ora_sqllite` library and integration tests, GitHub Actions, PowerShell package-content audit, no new runtime dependency.

**Spec:** `docs/superpowers/specs/2026-10-05-phase-6-rust-developer-preview-design.md`

## Global Constraints

- Keep crate name `ora_sqllite`, version `0.1.0`, edition `2024`, and `rust-version = "1.85"`.
- Keep database format 2 and preserve V1 records inside format 2; Phase 6 adds no disk-format or SQL behavior.
- Keep every documented public API and stable numeric `ErrorCode` source compatible throughout `v0.1.x`.
- Use only the existing three runtime dependencies; examples use only `std` and the public `ora_sqllite` API.
- Use dual license `MIT OR Apache-2.0` with `Copyright (c) 2026 ORA-SQLLite contributors`.
- Publish neither to crates.io nor through a GitHub workflow.
- Do not create or push `v0.1.0`; tagging and GitHub pre-release creation require a later explicit user approval.
- Keep PR CI lightweight; Release Preview is `workflow_dispatch` only and has `contents: read` permission.
- Do not make Deep CI, the optional complete all-target test suite, or any ignored scale test a new blocking release gate.
- Add no CLI, async API, ORM, FFI, network service, Oracle wire protocol, optimizer, or excluded Oracle feature.

## Review Focus

- A failed example setup or early return must remove only the directory and database files that the example itself created; Task 2 tests collision refusal, sibling preservation, and exact cleanup.
- The quickstart must visibly distinguish commit, explicit rollback, drop rollback, and reopen persistence without relying on internal APIs; Task 2 runs the binary and asserts every state transition.
- The Oracle example must preserve decimal/date/timestamp values, CHAR padding, empty-Text-to-NULL, and authored TSTZ offset while handling a named duplicate-key error without parsing message text; Task 3 asserts these observations.
- README links and commands must remain usable, and compatibility claims must separate Supported, Partial, and Unsupported instead of saying unqualified “Oracle compatible”; Task 4 adds a repository-level documentation contract test.
- The package and workflow must exclude internal/scratch material and every write/publish path; Tasks 5 and 6 enforce an allowlist and workflow permission/event assertions.

---

## File Structure

- Modify `src/lib.rs`: crate-level developer overview, exported-item documentation gate, and links to the two examples.
- Modify `src/error.rs`, `src/value.rs`, `src/decimal.rs`, `src/datetime.rs`, `src/sql/limits.rs`, and `src/api/*.rs`: complete rustdoc for the existing public types, variants, methods, lifetimes, ownership, limits, and errors.
- Create `examples/support/mod.rs`: shared owned temporary-directory/database guard using only `std` and the public crate API.
- Create `examples/quickstart.rs`: executable CRUD, parameter, transaction, rollback, close/reopen example.
- Create `examples/oracle_types.rs`: executable Oracle type, normalization, reopen, and structured-error example.
- Replace `README.md`: concise Rust developer landing page with tagged Git dependency, minimal code, example commands, status, MSRV, format, and evidence boundary.
- Create `docs/getting-started.md`: detailed public-API usage guide.
- Create `docs/oracle-compatibility.md`: Supported/Partial/Unsupported compatibility matrix.
- Create then finalize `docs/releases/v0.1.0.md`: developer-preview scope, compatibility promise, validation evidence, limitations, and upgrade policy.
- Create `CHANGELOG.md`: `[Unreleased]` plus `0.1.0` developer-preview entry.
- Create `tests/release_docs.rs`: local-link, command, dependency-snippet, and claim-boundary assertions.
- Create `LICENSE-MIT` and `LICENSE-APACHE`: canonical license texts.
- Modify `Cargo.toml`: license, readme, keywords, category, and explicit package include list.
- Create `.github/scripts/verify-package.ps1`: package-file allowlist and required-file validation.
- Create `.github/workflows/release-preview.yml`: manual, read-only stable/MSRV preview checks.

### Task 1: Complete and lock the public Rust API documentation

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/error.rs`
- Modify: `src/value.rs`
- Modify: `src/decimal.rs`
- Modify: `src/datetime.rs`
- Modify: `src/sql/limits.rs`
- Modify: `src/api/connection.rs`
- Modify: `src/api/database.rs`
- Modify: `src/api/metadata.rs`
- Modify: `src/api/options.rs`
- Modify: `src/api/params.rs`
- Modify: `src/api/result.rs`
- Modify: `src/api/row.rs`
- Modify: `src/api/statement.rs`
- Modify: `src/api/transaction.rs`
- Test: existing unit doctests and `tests/public_api.rs`

**Interfaces:**
- Consumes: the exact public exports currently listed in `src/lib.rs` and `ora_sqllite::error::SqlLocation`.
- Produces: complete rustdoc contracts for the unchanged public API and a crate-level `#![deny(missing_docs)]` gate.

- [ ] **Step 1: Capture the existing public surface before changing documentation**

Run: `rg -n "^pub use|^pub mod" src/lib.rs`

Run: `rg -n "^pub (struct|enum|type)|^    pub (fn|const fn)" src/api src/error.rs src/value.rs src/decimal.rs src/datetime.rs src/sql/limits.rs`

Run: `rustup run stable cargo doc --no-deps`

Expected: the source inventory matches the exports named by the spec and baseline documentation builds, but it does not yet enforce complete docs. Save the inventory in the task review notes; do not install nightly.

- [ ] **Step 2: Add crate and item documentation without changing signatures**

Document the following contracts directly beside their existing declarations:

- `Database::create` is exclusive and never overwrites; `Database::open` never creates; `Database` is cloneable and owns shared in-process database state.
- `Connection` is a synchronous session, mutable while preparing/executing, and permits one active borrowing transaction.
- `Statement` is an owned prepared plan with stable parameter/column metadata and schema revalidation at execution.
- `Rows` borrows its connection/transaction until exhausted or dropped; `Row::get` is zero-based and returns `None` out of range.
- `ReadTransaction` owns a fixed snapshot; `WriteTransaction::commit`, explicit `rollback`, and drop rollback are distinct.
- `Options`, `SqlLimits`, and every builder describe defaults, accepted range, create/open relevance, and resulting error where validation is fallible.
- `Value`, `DataType`, `ParameterValue`, `Parameters`, metadata enums, and all variants document representation and NULL behavior.
- `Decimal`, `OracleDate`, and `OracleTimestamp` document valid ranges, precision, scale, fixed-offset semantics, and checked constructors.
- `DbError`, `ErrorCode`, `Result`, and `SqlLocation` document stable versus diagnostic fields. State that numeric `ErrorCode`, optional Oracle code, and optional SQL location are structured contracts, while display text is not.

Add crate documentation in `src/lib.rs` with a `no_run` create/insert/query example that uses only root exports. Do not add convenience methods or make an internal module public.

- [ ] **Step 3: Enable the documentation gate**

After every exported item is documented, add this crate attribute immediately below `#![forbid(unsafe_code)]`:

```rust
#![deny(missing_docs)]
```

Do not silence the lint on public items and do not reduce visibility merely to pass it.

- [ ] **Step 4: Build strict documentation and run doctests**

PowerShell:

```powershell
$env:RUSTDOCFLAGS = '-D warnings'
rustup run stable cargo doc --no-deps
Remove-Item Env:RUSTDOCFLAGS
rustup run stable cargo test --doc
rustup run stable cargo test --test public_api
```

Expected: all commands PASS; generated docs expose the same production signatures as before Task 1.

- [ ] **Step 5: Confirm this is a documentation-only API diff and commit**

Run: `git diff --word-diff=porcelain -- src`

Verify that non-comment tokens differ only by the `deny(missing_docs)` crate attribute. If an existing public contract cannot be documented truthfully, stop and report the exact API defect rather than inventing behavior.

```bash
git add src/lib.rs src/error.rs src/value.rs src/decimal.rs src/datetime.rs src/sql/limits.rs src/api
git commit -m "docs: document the public Rust API"
```

### Task 2: Add the self-cleaning CRUD quickstart

**Files:**
- Create: `examples/support/mod.rs`
- Create: `examples/quickstart.rs`
- Test: unit tests inside `examples/support/mod.rs`

**Interfaces:**
- Consumes: `Database`, `Options`, `Connection`, `Parameters`, `ParameterValue`, `Value`, `Decimal`, and `DbError` from the documented public API.
- Produces: `OwnedExampleDatabase::create(label) -> ora_sqllite::Result<(OwnedExampleDatabase, Database)>`, `main_path() -> &Path`, and exact-file cleanup on drop.

- [ ] **Step 1: Write the owned-path tests first**

In `examples/support/mod.rs`, add tests covering all three safety boundaries:

```rust
#[test]
fn owned_database_stays_below_the_system_temp_directory() {
    let (owned, database) = OwnedExampleDatabase::create("path-test").unwrap();
    assert!(owned.main_path().starts_with(std::env::temp_dir()));
    assert_eq!(owned.main_path().parent(), Some(owned.directory()));
    drop(database);
}

#[test]
fn cleanup_removes_owned_files_but_preserves_a_sibling() {
    let sibling = std::env::temp_dir().join(format!("ora-sqllite-sibling-{}", std::process::id()));
    std::fs::write(&sibling, b"keep").unwrap();
    {
        let (owned, database) = OwnedExampleDatabase::create("cleanup-test").unwrap();
        assert!(owned.main_path().exists());
        drop(database);
    }
    assert_eq!(std::fs::read(&sibling).unwrap(), b"keep");
    std::fs::remove_file(sibling).unwrap();
}

#[test]
fn nonempty_owned_directory_is_not_recursively_deleted() {
    let (owned, database) = OwnedExampleDatabase::create("nonempty-test").unwrap();
    let directory = owned.directory().to_path_buf();
    let witness = directory.join("unexpected.txt");
    std::fs::write(&witness, b"preserve").unwrap();
    drop(database);
    drop(owned);
    assert_eq!(std::fs::read(&witness).unwrap(), b"preserve");
    std::fs::remove_file(witness).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
```

Also create a minimal `examples/quickstart.rs` containing `mod support; fn main() {}` so Cargo has an example target while the helper tests drive the first failure.

- [ ] **Step 2: Run the example test target and observe the missing helper**

Run: `rustup run stable cargo test --example quickstart`

Expected: compilation fails because `examples/quickstart.rs` and `OwnedExampleDatabase` do not exist.

- [ ] **Step 3: Implement strictly owned temporary storage**

Use an atomic counter, process ID, and current UNIX-epoch nanoseconds to propose a directory below `std::env::temp_dir()`. Acquire ownership with `std::fs::create_dir`; retry only `AlreadyExists` up to 32 candidates. Put `example.db` inside that directory, then call `Database::create` so an existing database can never be overwritten.

`Drop` may remove only `example.db`, `example.db-wal`, and the now-empty owned directory. It must never call recursive deletion. Ignore `NotFound`; leave an unexpected/nonempty directory intact.

- [ ] **Step 4: Implement the complete quickstart flow**

`examples/quickstart.rs` must execute and assert this public flow:

```rust
let create = connection.prepare(
    "CREATE TABLE ACCOUNTS (ID NUMBER(10,0) PRIMARY KEY, NAME VARCHAR2(100) NOT NULL, AMOUNT NUMBER(12,2))",
)?;
connection.execute(&create, Parameters::none())?;

let insert = connection.prepare("INSERT INTO ACCOUNTS VALUES (:1, :2, :3)")?;
let first = [
    Value::Decimal(Decimal::new(false, 0, 1)?).into(),
    Value::Text("Ada".into()).into(),
    Value::Decimal(Decimal::new(false, 2, 12_50)?).into(),
];
assert_eq!(connection.execute(&insert, Parameters::positional(&first))?.rows_affected(), 1);
```

Then:

1. insert a second row using named parameters;
2. query and match typed `Value::Decimal`/`Value::Text` fields;
3. update one row and delete the other with parameters, asserting `rows_affected()`;
4. insert ID 3 in `begin_write()` and commit;
5. insert ID 4 in a second write transaction and drop it without commit;
6. start another write transaction, insert ID 5, call explicit `rollback()`, and verify it is absent;
7. drop connection and database, reopen with `Database::open`, and assert IDs 1 and 3 are present while 2, 4, and 5 are absent;
8. drop all handles before the owned guard performs cleanup.

Use names without the SQL colon in the named binding slice, matching the existing binder contract:

```rust
let named_insert = connection.prepare("INSERT INTO ACCOUNTS VALUES (:id, :name, :amount)")?;
let second = [
    ("id", Value::Decimal(Decimal::new(false, 0, 2)?).into()),
    ("name", Value::Text("Grace".into()).into()),
    ("amount", Value::Decimal(Decimal::new(false, 2, 20_00)?).into()),
];
connection.execute(&named_insert, Parameters::named(&second))?;

let mut committed = connection.begin_write()?;
let insert_three = committed.prepare("INSERT INTO ACCOUNTS VALUES (3, 'Linus', 30.00)")?;
committed.execute(&insert_three, Parameters::none())?;
committed.commit()?;

{
    let mut dropped = connection.begin_write()?;
    let insert_four = dropped.prepare("INSERT INTO ACCOUNTS VALUES (4, 'Drop', 40.00)")?;
    dropped.execute(&insert_four, Parameters::none())?;
}

let mut rolled_back = connection.begin_write()?;
let insert_five = rolled_back.prepare("INSERT INTO ACCOUNTS VALUES (5, 'Rollback', 50.00)")?;
rolled_back.execute(&insert_five, Parameters::none())?;
rolled_back.rollback()?;
```

Use `run() -> ora_sqllite::Result<()>` and a `main() -> ExitCode` that prints `ErrorCode`, optional Oracle code, optional SQL location, and the `Display` message without parsing the message. Do not use unexplained `unwrap()` in production example flow.

- [ ] **Step 5: Run the tests and executable on stable and MSRV**

Run: `rustup run stable cargo test --example quickstart`

Run: `rustup run stable cargo run --example quickstart`

Run: `rustup run 1.85.0 cargo run --example quickstart`

Expected: PASS; the binary prints a short success summary and leaves no owned example directory behind.

- [ ] **Step 6: Commit the quickstart**

```bash
git add examples/support/mod.rs examples/quickstart.rs
git commit -m "docs: add executable Rust quickstart"
```

### Task 3: Add the Oracle type and structured-error example

**Files:**
- Create: `examples/oracle_types.rs`
- Modify: `examples/support/mod.rs` only if a reviewed general helper is missing
- Test: assertions executed by `examples/oracle_types.rs`

**Interfaces:**
- Consumes: Task 2 owned database guard plus `Decimal`, `OracleDate`, `OracleTimestamp`, `ParameterValue`, `Parameters`, `Value`, `ErrorCode`, and `DbError`.
- Produces: a public-only demonstration of Oracle declarations, normalization, fixed-offset persistence, reopen, and expected constraint handling.

- [ ] **Step 1: Add the example with a deliberately failing observation**

Create the table and parameter shapes, but initially assert an impossible duplicate insert success so the first run proves the error path executes:

```rust
let create = connection.prepare(
    "CREATE TABLE ORACLE_VALUES (\
       ID NUMBER(10,0), N NUMBER(12,2), V VARCHAR2(20), C CHAR(4), R RAW(8), \
       D DATE, TS TIMESTAMP(9), TZ TIMESTAMP(9) WITH TIME ZONE, \
       CONSTRAINT PK_ORACLE_VALUES PRIMARY KEY (ID))",
)?;
connection.execute(&create, Parameters::none())?;
```

Run: `rustup run stable cargo run --example oracle_types`

Expected: FAIL at the temporary assertion after the duplicate key returns a structured error.

- [ ] **Step 2: Bind and read every approved Oracle type**

Insert values through positional parameters constructed in Rust:

```rust
let date = OracleDate::new(2026, 10, 5, 9, 30, 15)?;
let timestamp = OracleTimestamp::from_nanoseconds(date, 123_456_789, None)?;
let zoned = OracleTimestamp::from_nanoseconds(date, 987_654_321, Some(480))?;
let values = [
    Value::Decimal(Decimal::new(false, 0, 1)?).into(),
    Value::Decimal(Decimal::new(false, 2, 123_45)?).into(),
    Value::Text(String::new()).into(),
    Value::Text("A".into()).into(),
    Value::Blob(vec![0x00, 0x7f, 0xff]).into(),
    Value::Timestamp(OracleTimestamp::new(date, 0, None)?).into(),
    Value::Timestamp(timestamp).into(),
    Value::Timestamp(zoned).into(),
];
```

Read by zero-based index and assert:

- `NUMBER(12,2)` is `Decimal(false, 2, 12345)`;
- empty `VARCHAR2` Text is `Value::Null`;
- `CHAR(4)` is `Value::Text("A   ")`;
- RAW bytes are unchanged;
- DATE has zero fractional component;
- TIMESTAMP retains 123,456,789 nanoseconds;
- TSTZ retains `Some(480)`, not merely the UTC instant.

- [ ] **Step 3: Handle a named duplicate-key failure without parsing text**

Execute the same primary-key value again and require:

```rust
let error = connection.execute(&insert, Parameters::positional(&values)).unwrap_err();
assert_eq!(error.code(), ErrorCode::Constraint);
assert_eq!(error.oracle_code(), Some(1));
print_error("PK_ORACLE_VALUES", &error);
```

`print_error` prints the known constraint identity supplied by this example, the stable numeric/code enum, optional Oracle code, and optional SQL location. It may print `Display`, but it must not search or split the message to recover metadata.

- [ ] **Step 4: Close, reopen, and repeat the normalized-value assertions**

Drop rows, connection, and database in that order. Reopen the exact owned main path with `Database::open`, query ID 1, and repeat all type/offset assertions before dropping handles and allowing cleanup.

- [ ] **Step 5: Run stable/MSRV examples and commit**

Run: `rustup run stable cargo test --example oracle_types`

Run: `rustup run stable cargo run --example oracle_types`

Run: `rustup run 1.85.0 cargo run --example oracle_types`

Expected: PASS; the expected duplicate error is printed, the process exits successfully, and owned files are removed.

```bash
git add examples/oracle_types.rs examples/support/mod.rs
git commit -m "docs: demonstrate Oracle value handling"
```

### Task 4: Replace internal-history landing text with developer documentation

**Files:**
- Replace: `README.md`
- Create: `docs/getting-started.md`
- Create: `docs/oracle-compatibility.md`
- Create: `docs/releases/v0.1.0.md`
- Create: `CHANGELOG.md`
- Create: `tests/release_docs.rs`

**Interfaces:**
- Consumes: Tasks 1-3 documented API and exact example commands.
- Produces: the developer landing page, usage guide, compatibility matrix, changelog, and automated repository-link/claim checks.

- [ ] **Step 1: Add failing documentation contract tests**

Create `tests/release_docs.rs` using only `std`. Read files relative to `env!("CARGO_MANIFEST_DIR")` and assert:

```rust
#[test]
fn readme_has_the_release_dependency_and_runnable_examples() {
    let readme = read("README.md");
    assert!(readme.contains(
        "ora_sqllite = { git = \"https://github.com/liu2kaibin/ORA-SQLLite.git\", tag = \"v0.1.0\" }"
    ));
    assert!(readme.contains("cargo run --example quickstart"));
    assert!(readme.contains("cargo run --example oracle_types"));
    assert!(readme.contains("MSRV: Rust 1.85"));
    assert!(readme.contains("on-disk format: 2"));
}

#[test]
fn compatibility_uses_qualified_states_and_names_wire_incompatibility() {
    let matrix = read("docs/oracle-compatibility.md");
    for heading in ["Supported", "Partial", "Unsupported"] {
        assert!(matrix.contains(heading), "missing state {heading}");
    }
    assert!(matrix.contains("not Oracle wire compatible"));
    assert!(matrix.contains("single process"));
    assert!(matrix.contains("fixed offsets"));
}
```

Add a small Markdown link extractor that checks every repository-relative `](path)` target in README and the three public docs. Skip `http://`, `https://`, `mailto:`, and `#anchor`; strip a trailing fragment before checking `Path::exists()`.

Run: `rustup run stable cargo test --test release_docs`

Expected: FAIL because the guides and new README contract do not exist.

- [ ] **Step 2: Rewrite README as a ten-minute landing page**

Use these sections in this order:

1. product sentence: pure-Rust, in-process, single-file-plus-WAL database with an Oracle-inspired CRUD subset;
2. “Developer preview” warning: `v0.1.0` GitHub pre-release, no crates.io publication, no Oracle wire compatibility;
3. exact tagged Git dependency;
4. minimal create/prepare/parameterized insert/query sample using the current API;
5. commands for both examples;
6. supported operating model and top limitations;
7. links to getting started, compatibility, API docs command, changelog, and release notes;
8. status line containing `MSRV: Rust 1.85`, `on-disk format: 2`, and the lightweight MVP evidence boundary.

Remove “planned”, the promised CLI, and the phase-by-phase internal implementation chronicle from the landing page. Preserve links to historical specs only under a short “Project records” section.

- [ ] **Step 3: Write the public API usage guide**

`docs/getting-started.md` must contain runnable snippets and explicit sections for:

- create versus open and file ownership;
- shared `Database` versus mutable `Connection`;
- statement preparation/reuse and schema-change errors;
- positional and named parameter arrays with `ParameterValue`;
- pull-based `Rows` iteration and indexed `Value` matching;
- read snapshot, write commit, explicit rollback, and drop rollback;
- `Options::read_only()` and resource-limit builders;
- stable `ErrorCode`, optional Oracle code/location, and unstable display text;
- main file/WAL lifecycle, one process, multiple readers/one writer.

Every snippet must use a currently exported symbol and must not imply `open` creates a missing file.

- [ ] **Step 4: Write the three-state Oracle compatibility matrix**

Create one table with columns `Area`, `State`, `Supported contract`, and `Oracle difference`. Cover at least:

- NUMBER/VARCHAR2/CHAR/RAW/DATE/TIMESTAMP/TSTZ;
- INSERT/SELECT/UPDATE/DELETE, predicates, joins, grouping, ordering, indexes;
- primary/unique/not-null/check/foreign-key constraints;
- explicit transactions, auto-commit, NULL and empty Text;
- timestamp fixed offsets and lack of named zones/NLS;
- sequences, pagination, Oracle scalar functions, DUAL, PL/SQL;
- CLI/network/Oracle protocol, users/roles/tablespaces;
- single-process ownership and multiple readers/one writer.

Label implemented subsets as **Supported** or **Partial** with the exact restriction. Label excluded features **Unsupported**. Do not use an unqualified “Oracle compatible” sentence.

- [ ] **Step 5: Add the changelog and pass documentation tests**

Create Keep-a-Changelog-style headings:

```markdown
# Changelog

## [Unreleased]

## [0.1.0] - 2026-10-05

### Added
- Rust developer preview for the documented embedded CRUD API.
- Oracle-oriented scalar types, constraints, transactions, and format-2 persistence described in the compatibility matrix.

### Known limitations
- No Oracle wire protocol, PL/SQL, network server, async API, or multi-process access.
```

Create `docs/releases/v0.1.0.md` with the seven headings specified in Task 7 Step 3. Under validation evidence, state exactly that Phase 6 branch validation and the hosted Release Preview have not yet executed at this documentation commit; list no PASS result. This is an honest intermediate evidence state, not a release-readiness claim. Task 7 replaces it with observed command results.

Run: `rustup run stable cargo test --test release_docs`

Run each README command exactly as written.

Expected: PASS and every local link resolves.

- [ ] **Step 6: Commit developer documentation**

```bash
git add README.md docs/getting-started.md docs/oracle-compatibility.md docs/releases/v0.1.0.md CHANGELOG.md tests/release_docs.rs
git commit -m "docs: add Rust developer preview guides"
```

### Task 5: Add dual licensing and an explicit package boundary

**Files:**
- Create: `LICENSE-MIT`
- Create: `LICENSE-APACHE`
- Modify: `Cargo.toml`
- Create: `.github/scripts/verify-package.ps1`
- Test: Cargo metadata/package commands and the package audit script

**Interfaces:**
- Consumes: Tasks 2-4 examples and public documentation.
- Produces: package metadata, canonical license grants, a source-package allowlist, and a repeatable file-list audit.

- [ ] **Step 1: Add the canonical license texts**

Add the standard MIT License text with this exact copyright line:

```text
Copyright (c) 2026 ORA-SQLLite contributors
```

Add the unmodified Apache License, Version 2.0 text dated January 2004. Do not add a custom NOTICE file or contributor agreement.

- [ ] **Step 2: Complete package metadata and define the allowlist**

Add to `[package]`:

```toml
license = "MIT OR Apache-2.0"
readme = "README.md"
keywords = ["database", "embedded", "oracle", "sql", "rust"]
categories = ["database-implementations"]
include = [
    "/src/**",
    "/examples/**",
    "/docs/getting-started.md",
    "/docs/oracle-compatibility.md",
    "/docs/releases/v0.1.0.md",
    "/README.md",
    "/CHANGELOG.md",
    "/LICENSE-MIT",
    "/LICENSE-APACHE",
]
```

Do not add `publish = false`: the release is intentionally not published now, but the manifest should remain a valid source package. No task may call `cargo publish`.

- [ ] **Step 3: Implement the package-content audit**

`.github/scripts/verify-package.ps1` accepts an optional `-AllowDirty` switch, executes `cargo package --list` (adding `--allow-dirty` only for that switch), and fails unless all required files exist:

```powershell
$required = @(
  'README.md', 'CHANGELOG.md', 'LICENSE-MIT', 'LICENSE-APACHE',
  'examples/quickstart.rs', 'examples/oracle_types.rs',
  'docs/getting-started.md', 'docs/oracle-compatibility.md', 'docs/releases/v0.1.0.md',
  'src/lib.rs'
)
```

Implement the command and rejection core explicitly:

```powershell
param([switch]$AllowDirty)
$arguments = @('package', '--list')
if ($AllowDirty) { $arguments += '--allow-dirty' }
$files = @(& cargo @arguments)
if ($LASTEXITCODE -ne 0) { throw 'cargo package --list failed' }

foreach ($path in $required) {
  if ($path -notin $files) { throw "required package file is missing: $path" }
}

$allowedRoots = @('src/', 'examples/')
foreach ($path in $files) {
  $knownRoot = $allowedRoots | Where-Object { $path.StartsWith($_) }
  $knownExact = $path -in $required -or $path -in @('Cargo.toml', 'Cargo.toml.orig', '.cargo_vcs_info.json')
  if (-not $knownRoot -and -not $knownExact) { throw "unexpected package file: $path" }
  if ($path -match '(^|/)(target|\.worktrees|\.superpowers|tests|docs/superpowers)(/|$)|\.db(-wal)?$') {
    throw "forbidden package file: $path"
  }
}
$files | Sort-Object
```

Allow only `Cargo.toml`, `Cargo.toml.orig`, `.cargo_vcs_info.json`, the required root files, and paths below `src/`, `examples/`, plus the three named public docs. Reject `target/`, `.worktrees/`, `.superpowers/`, `tests/`, `docs/superpowers/`, `*.db`, `*.db-wal`, and every unrecognized path. Print the sorted accepted list on success.

- [ ] **Step 4: Validate metadata and package creation before committing**

Run: `rustup run stable cargo metadata --no-deps --format-version 1`

Inspect the JSON and require name `ora_sqllite`, version `0.1.0`, rust version `1.85`, and license `MIT OR Apache-2.0`.

Run: `pwsh -File .github/scripts/verify-package.ps1 -AllowDirty`

Run: `rustup run stable cargo package --allow-dirty`

Expected: PASS; Cargo builds the packaged crate and the list contains public source/docs/examples/licenses but no internal plans, tests, databases, or build output.

- [ ] **Step 5: Commit licensing and packaging**

```bash
git add Cargo.toml LICENSE-MIT LICENSE-APACHE .github/scripts/verify-package.ps1
git commit -m "build: define the v0.1.0 source package"
```

### Task 6: Add the manual read-only Release Preview workflow

**Files:**
- Create: `.github/workflows/release-preview.yml`
- Modify: `tests/release_docs.rs`
- Test: workflow contract assertions plus local command equivalents

**Interfaces:**
- Consumes: all documented/example/package targets from Tasks 1-5.
- Produces: a manually dispatched stable/MSRV Linux validation workflow with no write or publish capability.

- [ ] **Step 1: Add a failing workflow safety test**

Extend `tests/release_docs.rs`:

```rust
#[test]
fn release_preview_is_manual_read_only_and_never_publishes() {
    let workflow = read(".github/workflows/release-preview.yml");
    assert!(workflow.contains("workflow_dispatch:"));
    assert!(workflow.contains("contents: read"));
    for forbidden in ["push:", "pull_request:", "contents: write", "cargo publish", "softprops/action-gh-release", "gh release", "git tag"] {
        assert!(!workflow.contains(forbidden), "forbidden release capability: {forbidden}");
    }
}
```

Run: `rustup run stable cargo test --test release_docs release_preview_is_manual_read_only_and_never_publishes -- --exact`

Expected: FAIL because the workflow does not exist.

- [ ] **Step 2: Create the exact manual workflow**

Use this job structure:

```yaml
name: Release Preview

on:
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: release-preview-${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUSTDOCFLAGS: -D warnings

jobs:
  stable:
    name: Linux stable release preview
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
      - name: Install stable Rust
        run: rustup toolchain install stable --profile minimal --component rustfmt
      - name: Check formatting
        run: rustup run stable cargo fmt --all -- --check
      - name: Check all targets
        run: rustup run stable cargo check --all-targets
      - name: Build strict documentation
        run: rustup run stable cargo doc --no-deps
      - name: Run doctests
        run: rustup run stable cargo test --doc
      - name: Run examples
        run: |
          rustup run stable cargo run --example quickstart
          rustup run stable cargo run --example oracle_types
      - name: Run focused public API tests
        run: >-
          rustup run stable cargo test
          --test public_api
          --test phase4c_api
          --test phase4c2_constraint_api
          --test phase5_oracle_types_api
          --test release_docs
      - name: Verify package contents
        shell: pwsh
        run: ./.github/scripts/verify-package.ps1
      - name: Build source package
        run: rustup run stable cargo package

  msrv:
    name: Linux Rust 1.85 release preview
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust 1.85
        run: rustup toolchain install 1.85.0 --profile minimal
      - name: Check all targets
        run: rustup run 1.85.0 cargo check --all-targets
      - name: Run examples
        run: |
          rustup run 1.85.0 cargo run --example quickstart
          rustup run 1.85.0 cargo run --example oracle_types
      - name: Run focused public API tests
        run: >-
          rustup run 1.85.0 cargo test
          --test public_api
          --test phase4c_api
          --test phase4c2_constraint_api
          --test phase5_oracle_types_api
          --test release_docs
```

Do not add a release job, secrets, artifact upload, crates.io token, Deep CI call, ignored test, push trigger, or pull-request trigger.

- [ ] **Step 3: Run the workflow contract and local stable equivalents**

Run: `rustup run stable cargo test --test release_docs`

Run every stable job command locally, using `pwsh -File .github/scripts/verify-package.ps1 -AllowDirty` before the workflow commit.

Expected: PASS. A complete local all-target test is informative, not required by this workflow.

- [ ] **Step 4: Commit Release Preview**

```bash
git add .github/workflows/release-preview.yml tests/release_docs.rs
git commit -m "ci: add read-only release preview"
```

### Task 7: Record release evidence and perform whole-branch review

**Files:**
- Modify: `docs/releases/v0.1.0.md`
- Modify only files required to correct demonstrated review findings
- Verify: every file changed from `8051038` through the Phase 6 branch tip

**Interfaces:**
- Consumes: Tasks 1-6 completed commits and their command output.
- Produces: truthful `v0.1.0` preview notes, final test evidence, and an independently reviewed implementation branch ready for a pull request.

- [ ] **Step 1: Run the mandatory local release matrix and retain exact results**

Run:

```powershell
rustup run stable cargo fmt --all -- --check
rustup run stable cargo check --all-targets
$env:RUSTDOCFLAGS = '-D warnings'
rustup run stable cargo doc --no-deps
Remove-Item Env:RUSTDOCFLAGS
rustup run stable cargo test --doc
rustup run stable cargo run --example quickstart
rustup run stable cargo run --example oracle_types
rustup run stable cargo test --test public_api --test phase4c_api --test phase4c2_constraint_api --test phase5_oracle_types_api --test release_docs
rustup run 1.85.0 cargo check --all-targets
rustup run 1.85.0 cargo run --example quickstart
rustup run 1.85.0 cargo run --example oracle_types
rustup run 1.85.0 cargo test --test public_api --test phase4c_api --test phase4c2_constraint_api --test phase5_oracle_types_api --test release_docs
pwsh -File .github/scripts/verify-package.ps1 -AllowDirty
rustup run stable cargo package --allow-dirty
```

Expected: every mandatory command PASS. If one does not pass, record the exact non-PASS result and fix only a demonstrated Phase 6 defect before claiming readiness.

- [ ] **Step 2: Run optional retained regression evidence without changing the gate**

Run when it completes within the approved MVP budget:

`rustup run stable cargo test --all-targets`

Do not run ignored scale tests or Deep CI locally. If the all-target run is skipped, interrupted, or partial, label it exactly that way; mandatory focused gates may still pass.

- [ ] **Step 3: Write truthful `v0.1.0` pre-release notes**

Use these sections:

1. **Release identity:** GitHub developer pre-release, crate version `0.1.0`, format 2, MSRV 1.85, not on crates.io.
2. **What developers can do:** current public CRUD, prepared parameters, rows, explicit transactions, constraints, supported Oracle values, reopen.
3. **`v0.1.x` compatibility:** documented source API, numeric `ErrorCode`, format-2 read/write, and V1-record promise; messages/plans are not stable.
4. **Validation evidence:** one row per command from Step 1 with environment and observed PASS/non-PASS status; hosted workflow remains “not executed in this local checkout” until GitHub reports it.
5. **Known limitations:** point to the compatibility matrix and name no network/wire protocol, PL/SQL, async, ORM, multi-process access, named timezones, sequences, pagination, or excluded SQL.
6. **Scale evidence boundary:** 100,000 rows is the approved maximum; preserve the prior 4.7-million-row observation as partial evidence only and make no 10-million PASS claim.
7. **Upgrade expectations:** patch-compatible `v0.1.x`; breaking API/format changes require at least `v0.2.0` plus migration notes.

Do not claim the Release Preview workflow passed until its GitHub run is green. Do not instruct the reader to publish, tag, or create a release.

- [ ] **Step 4: Rerun package and docs checks with the release notes present**

Run: `rustup run stable cargo test --test release_docs`

Run: `pwsh -File .github/scripts/verify-package.ps1 -AllowDirty`

Run: `rustup run stable cargo package --allow-dirty`

Expected: PASS and `docs/releases/v0.1.0.md` appears in the package list.

- [ ] **Step 5: Commit release notes and any evidence-only correction**

```bash
git add docs/releases/v0.1.0.md
git commit -m "docs: record v0.1.0 preview evidence"
```

- [ ] **Step 6: Perform independent task and whole-branch review**

For Subagent-driven execution, each Task 1-6 receives a fresh spec-compliance review and code-quality review before the next task. After Task 7, dispatch a fresh whole-branch reviewer against the approved spec.

Reviewer commands:

```bash
git diff --stat 8051038..HEAD
git diff --check 8051038..HEAD
git diff 8051038..HEAD -- Cargo.toml src examples tests README.md CHANGELOG.md docs/getting-started.md docs/oracle-compatibility.md docs/releases/v0.1.0.md .github
```

Reviewer must explicitly check:

- no SQL, format, dependency, or broad public-API expansion;
- examples use only public API and never recursively delete;
- every exported item passes `missing_docs` and strict rustdoc;
- README/compatibility claims match tests and known boundaries;
- package allowlist excludes internal/scratch data;
- workflow is manual/read-only and contains no tag/release/publish action;
- release notes distinguish local PASS, hosted-not-yet-run, partial, and skipped evidence.

- [ ] **Step 7: Correct findings test-first and rerun affected gates**

For each finding, add or tighten the smallest owning test, observe it fail where applicable, make the minimal correction, rerun the focused gate, then rerun Task 7 Step 1. Commit only actual corrections:

```bash
git add <reviewed-files>
git commit -m "fix: address Phase 6 release review"
```

If review produces no changes, create no empty commit.

- [ ] **Step 8: Stop at the release-authorization boundary**

Report the final commit, local verification matrix, and GitHub Release Preview status. The implementation may proceed to a pull request/merge workflow, but do not create/push `v0.1.0` and do not create a GitHub release until the implementation PR is merged, Release Preview is green, and the user separately approves both operations.
