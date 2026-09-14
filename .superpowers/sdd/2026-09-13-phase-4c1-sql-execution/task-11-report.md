# Task 11 Report: Parameterized INSERT

## Status

Complete and ready for the requested source commit.

- Binding base: `b51000e6a4ca8e9c38bc5549ae92fda73e497ca2`
- Commit message: `feat: execute parameterized inserts`
- Task-owned source files: `src/sql/exec/mutation.rs`, `src/api/transaction.rs`
- Approved minimal facade extension: `src/storage/relation.rs`
- Dependencies and persisted database/WAL/catalog/row/page formats: unchanged
- Rust edition/MSRV/unsafe policy: Rust 2024, Rust 1.85, and `#![forbid(unsafe_code)]` retained

## Implementation

- Added `PlanKind::Insert` execution through the existing validated explicit-write path and statement checkpoint wrapper.
- Evaluates every INSERT expression before mutation, using the exact `ParameterFrame` and retained SQL expression locations.
- Maps each evaluated value by the bound target ordinal rather than source order and fills every omitted column with SQL `NULL`.
- Applies the existing safe `Int64` to `Decimal` promotion before row validation; other wrong types, wrong typed NULLs, non-finite floats, nullability violations, and row-size violations remain `Type` errors.
- Prevalidates the complete row through a crate-private `Relation` facade before `TableWrite::open`, so ordinary validation failures cannot allocate a ROWID or open the catalog write gate.
- Inserts through the Task 9 `TableWrite` facade and returns `ExecutionResult::new(1)`.
- Kept `Relation::open_from_batch`, `column_count`, and `validate_values` crate-private. No `RowCodec`, B+Tree, catalog key, row bytes, page reference, or persistent format was exposed to SQL execution.

## TDD RED Evidence

The brief's literal short command was run first:

```text
rustup run stable cargo test insert_maps_targets_parameters_and_omitted_columns -- --exact
test result: ok. 0 passed; 0 failed
```

Because Rust's exact filter requires the fully qualified test path, that zero-test run is recorded but is not counted as RED or GREEN proof.

The fully qualified transaction regressions were then run against the pre-implementation write dispatcher:

```text
rustup run stable cargo test \
  api::transaction::tests::insert_maps_targets_parameters_and_omitted_columns \
  --lib -- --exact

rustup run stable cargo test \
  api::transaction::tests::failed_insert_does_not_consume_rowid_or_change_the_batch \
  --lib -- --exact

DbError { code: Unsupported, message: "write statement execution is not implemented", ... }
test result: FAILED. 0 passed; 1 failed
```

The relation prevalidation regression was also compiled before its facade existed:

```text
rustup run stable cargo test \
  storage::relation::tests::relation_prevalidates_complete_rows_without_opening_the_catalog_gate \
  --lib -- --exact

error[E0599]: no function or associated item named `open_from_batch` found for struct `Relation`
```

This was the intended missing-interface RED. The minimal facade was permitted by the Task 11 ruling because complete-row validation must precede `TableWrite::open`; raw storage types remain hidden.

## TDD GREEN Evidence

The same fully qualified transaction and relation tests passed after implementation:

```text
api::transaction::tests::insert_maps_targets_parameters_and_omitted_columns
test result: ok. 1 passed; 0 failed

api::transaction::tests::failed_insert_does_not_consume_rowid_or_change_the_batch
test result: ok. 1 passed; 0 failed

storage::relation::tests::relation_prevalidates_complete_rows_without_opening_the_catalog_gate
test result: ok. 1 passed; 0 failed
```

Focused stable suites passed:

```text
rustup run stable cargo test insert_ -- --nocapture
test result: ok. 13 passed; 0 failed

rustup run stable cargo test sql::exec::params:: --lib
test result: ok. 4 passed; 0 failed

rustup run stable cargo test storage::row:: --lib
test result: ok. 20 passed; 0 failed

rustup run stable cargo test sql::exec::mutation:: --lib
test result: ok. 2 passed; 0 failed

rustup run stable cargo test api::transaction:: --lib
test result: ok. 19 passed; 0 failed

rustup run stable cargo test storage::relation:: --lib
test result: ok. 12 passed; 0 failed
```

The focused coverage includes positional, named, repeated, missing, extra, duplicate, and wrong-kind parameters; typed and untyped NULL; nullability; wrong types; safe Decimal promotion; non-finite floats; row-size limits; retained SQL error locations; omitted columns; target reordering; read-own-write; affected-row count; ROWID reuse after failure; relation root split; rollback; and commit/reopen visibility.

The Task 9 catalog/pager ownership tests were rerun exactly and remained green:

```text
rustup run stable cargo test \
  storage::catalog::tests::row_counter_row_bytes_and_split_root_publish_or_rollback_together \
  --lib -- --exact
test result: ok. 1 passed; 0 failed

rustup run stable cargo test \
  storage::pager::tests::statement_checkpoint_restores_every_mutation_field_without_clearing_fatal_state \
  --lib -- --exact
test result: ok. 1 passed; 0 failed
```

## Full Stable Regression

```text
rustup run stable cargo test --lib
test result: ok. 686 passed; 0 failed; 8 ignored

rustup run stable cargo test --test public_api
test result: ok. 10 passed; 0 failed
```

## Rust 1.85 MSRV

```text
rustup run 1.85.0 cargo check --all-targets --all-features \
  --target-dir target/task11-msrv
Finished `dev` profile; exit 0

rustup run 1.85.0 cargo test --lib \
  --target-dir target/task11-msrv
test result: ok. 686 passed; 0 failed; 8 ignored

rustup run 1.85.0 cargo test --test public_api \
  --target-dir target/task11-msrv
test result: ok. 10 passed; 0 failed
```

## Final Static and Scope Gates

```text
rustup run stable cargo fmt --all -- --check
# exit 0; no output

rustup run stable cargo check --all-targets --all-features
Finished `dev` profile; exit 0

rustup run stable cargo clippy --all-targets --all-features -- -D warnings
Finished `dev` profile; exit 0

git diff --check
# exit 0
```

Git emitted only the repository's informational Windows LF-to-CRLF working-copy notices. There were no whitespace errors.

The 168-point crash matrix was not rerun. Task 11 adds SQL-level INSERT orchestration and a read-only, crate-private prevalidation facade over the existing row codec; it does not alter the Task 9 mutation implementation, page/WAL/recovery logic, or any persistent format. Root split, rollback, commit, catalog/ROWID coupling, and statement restoration are covered by focused real-pager tests plus the full stable and Rust 1.85 library suites.

## Pre-Mutation Validation and Atomicity Proof

The execution order is:

1. The Task 10 validated-write path checks API kind, exact parameters, statement identity/schema, and unsupported-plan state.
2. INSERT opens a read-only `Relation` view from the active `WriteBatch` catalog and allocates a full-width value vector initialized to `NULL`.
3. Every expression is evaluated and safely promoted as needed, then stored at its bound target ordinal.
4. `Relation::validate_values` invokes the existing row codec for complete-row column count, nullability, type, finite-float, and byte-limit validation.
5. Only after all checks succeed does `TableWrite::open` acquire the catalog writer and allocate a ROWID during `insert`.
6. `TableWrite` atomically couples row-tree changes, catalog root changes, and `next_rowid`; the outer explicit-statement checkpoint restores any ordinary execution failure.

`insert_prevalidates_nullability_and_row_size_before_any_write` proves nullability and byte-limit failures leave the catalog reader available and preserve both the relation root and `next_rowid`. `failed_insert_does_not_consume_rowid_or_change_the_batch` and `insert_row_size_failure_leaves_rowid_one_available` prove the next successful row still receives ROWID 1. `sql_insert_root_split_rolls_back_and_commits_as_one_relation` inserts 96 large rows, proves read-own-write visibility, restores the exact prior relation on rollback, then repeats and commits with a changed root, `next_rowid == 97`, and 96 reopened rows.

## Files Changed

- `src/sql/exec/mutation.rs` — INSERT dispatch, pre-mutation expression evaluation/mapping/validation, one-row result, and focused atomicity coverage.
- `src/api/transaction.rs` — public explicit-write integration coverage for parameters, NULL/type/size errors, ROWID preservation, read-own-write, rollback, and affected rows.
- `src/storage/relation.rs` — approved crate-private read/validation facade and real-pager validation/root-split rollback/commit coverage.
- `.superpowers/sdd/2026-09-13-phase-4c1-sql-execution/task-11-report.md` — Task 11 evidence and review record.

## Self-Review

- Re-read the Task 11 brief, Task 11 ruling, Task 10 handoff, and Task 9 mutation/checkpoint contract.
- Confirmed the exact binding base and reviewed the complete source diff.
- Confirmed source expressions are written by retained target ordinal and omitted columns remain `NULL`.
- Confirmed all expressions and full-row validation complete before `TableWrite::open` or ROWID allocation.
- Confirmed safe `Int64` to `Decimal` promotion uses the existing checked executor helper.
- Confirmed located arithmetic errors propagate their stored SQL location.
- Confirmed ordinary bind/type/size failures preserve transaction usability, root, visible rows, and next ROWID.
- Confirmed root-splitting INSERTs publish or roll back row pages, table root, and row counter together.
- Confirmed all new storage entrypoints are crate-private and expose no raw storage or format representation.
- Confirmed no assertion was ignored, weakened, or replaced with implementation-derived expected output.
- Confirmed no dependency, unsafe code, public storage API, optimizer, UPDATE/DELETE, auto-commit, SQL transaction-control, index/constraint, or Phase 5 behavior was added.

## Concerns

No open correctness concern.

`Relation::validate_values` currently validates by encoding the complete row and discarding the encoded bytes. A successful INSERT is therefore encoded once during prevalidation and again inside `TableWrite::insert`. This preserves the required pre-mutation correctness and boundary but adds a minor per-row CPU/allocation cost that can be consolidated later without changing the SQL contract.
