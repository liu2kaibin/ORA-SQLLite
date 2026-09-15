# Task 12 Report: Streaming UPDATE and DELETE

## Status

Complete and ready for the requested source commit.

- Binding base: `dbcfa0703d8d7965bb114ad224c76809aab8f264`
- Commit message: `feat: execute atomic update and delete`
- Task-owned source files: `src/sql/exec/mutation.rs`, `src/storage/relation.rs`, `src/api/transaction.rs`
- Dependencies and persisted database/WAL/catalog/row/page formats: unchanged
- Rust edition/MSRV/unsafe policy: Rust 2024, Rust 1.85, and `#![forbid(unsafe_code)]` retained

## Implementation

- Added `PlanKind::Update` and `PlanKind::Delete` execution through the existing validated explicit-write path and statement checkpoint wrapper.
- Streams target rows in increasing hidden ROWID order and always resumes strictly after the last immutable ROWID. Replacing visible values cannot revisit a row, and deleting the current row cannot skip the next row.
- Evaluates the predicate and every UPDATE right-hand side against one immutable old row. All right-hand sides are complete before any target value is changed.
- Reuses the existing safe `Int64` to `Decimal` promotion and validates the complete replacement row before calling the relational mutation facade.
- Treats only SQL TRUE as a match; FALSE and NULL do not mutate or increment the affected count.
- Uses checked `u64` affected-row accounting and reports overflow as `ResourceLimit`.
- Accounts decoded old rows, pending assignment values, and replacement rows against the shared statement `WorkMemory`, then releases per-row accounting before advancing.
- Passes the caller's remaining row-decode budget into `TableWrite::next_after`; a row that cannot fit the execution budget is returned as `ResourceLimit` rather than being decoded with an unbounded limit.
- Disambiguates row-encoding memory exhaustion from genuine codec/type failures. Genuine assignment type failures remain located `Type` errors, including an offset-bearing timestamp assigned to plain `TIMESTAMP` at SQL column 19.
- Preserves the existing fatal boundary: `Io`, `Corruption`, and `Internal` restore the statement checkpoint and make the explicit write transaction rollback-only; ordinary errors restore the checkpoint and leave it usable.
- Added a test-only, thread-local `TableWrite::next_after` failure seam to exercise late fatal scan errors without exposing or changing the production storage contract.

## Statement Atomicity and Traversal Proof

For each UPDATE/DELETE row, execution performs this sequence:

1. Reopen the relation cursor strictly after the prior hidden ROWID.
2. Decode one owned row under the caller's remaining byte limit.
3. Evaluate the WHERE predicate against that old row.
4. For UPDATE, evaluate all assignments against the same old row, build and validate one complete replacement, then replace the existing ROWID key.
5. For DELETE, delete only the current matching ROWID key.
6. Increment the checked affected count only after the row mutation succeeds, release per-row memory accounting, and continue.

The Task 10 outer statement checkpoint covers all dirty pages, allocations, releases, catalog roots, and relation counters. Tests inject ordinary expression/row-budget failures and late fatal scan failures after earlier row changes, then prove the exact before-image is restored. Ordinary failures permit later work and commit; fatal failures permit only explicit rollback.

## TDD RED Evidence

The fully qualified transaction regressions were introduced against the Task 11 dispatcher before UPDATE/DELETE production dispatch existed. The named UPDATE test reached the retained plan and failed with the existing unsupported-write error:

```text
DbError { code: Unsupported, message: "write statement execution is not implemented", ... }
test result: FAILED. 0 passed; 1 failed
```

The caller-limited mutable relation scan regression also required the new `TableWrite::next_after(..., max_row_bytes)` interface before it could pass. These failures established the missing execution and storage-facade behavior before the implementation was added.

## Focused Stable Verification

The final focused suites passed:

```text
rustup run stable cargo test sql::exec::mutation:: --lib
test result: ok. 4 passed; 0 failed

rustup run stable cargo test api::transaction:: --lib
test result: ok. 28 passed; 0 failed

rustup run stable cargo test storage::relation:: --lib
test result: ok. 13 passed; 0 failed
```

Focused exact regressions passed for per-row memory accounting, late row-decode limits, timestamp type classification/location, ordinary mid-statement restoration, late `Io`/`Corruption`/`Internal` rollback-only behavior, old-row multi-assignment semantics, TRUE-only predicates, ROWID preservation, split/collapse rollback, commit, and reopen.

The lower storage suites were also run after the mutation changes:

```text
storage::btree::   51 passed; 0 failed
storage::catalog:: 82 passed; 0 failed
storage::pager::   76 passed; 0 failed
```

## Full Stable Regression

```text
rustup run stable cargo test --lib
test result: ok. 698 passed; 0 failed; 8 ignored

rustup run stable cargo test --test public_api
test result: ok. 10 passed; 0 failed
```

The full library binary began before a later comment-only edit. No executable token or assertion changed. The current source tree was subsequently recompiled and checked by the stable all-target gate, strict Clippy, and the Rust 1.85 focused suites below, so the full behavioral result remains applicable without repeating the 942.67-second library run.

## Rust 1.85 MSRV

The first MSRV attempts exposed a host setup issue rather than a code failure: PATH selected the separately installed Rust 1.98 compiler when Rust 1.85 Cargo spawned `rustc`, and sandboxed child-process creation returned Windows error 5. Reusing that initial target directory then correctly produced incompatible compiler-metadata errors.

The retry explicitly selected the verified Rust 1.85 compiler and used a new isolated target directory, `target/task12-msrv-verified`:

```text
rustup run 1.85.0 cargo check --all-targets --all-features \
  --target-dir target/task12-msrv-verified
Finished `dev` profile; exit 0

rustup run 1.85.0 cargo test sql::exec::mutation:: --lib \
  --target-dir target/task12-msrv-verified
test result: ok. 4 passed; 0 failed

rustup run 1.85.0 cargo test api::transaction:: --lib \
  --target-dir target/task12-msrv-verified
test result: ok. 28 passed; 0 failed

rustup run 1.85.0 cargo test storage::relation:: --lib \
  --target-dir target/task12-msrv-verified
test result: ok. 13 passed; 0 failed

rustup run 1.85.0 cargo test --test public_api \
  --target-dir target/task12-msrv-verified
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
# exit 0; no whitespace errors
```

Git emitted only the repository's informational Windows LF-to-CRLF working-copy notices.

Final scope inspection found exactly three changed source files, 1,069 insertions and 23 deletions. `Cargo.toml` and `Cargo.lock` have no diff. Added visibility is crate-private/test-only, no raw page/B+Tree/catalog/row-codec representation escaped the storage layer, no `unsafe` or new `sqlparser` boundary was added, and no auto-commit, SQL transaction-control, index/constraint, optimizer, persistent-format, or Phase 5 behavior entered Task 12.

## Files Changed

- `src/sql/exec/mutation.rs` — UPDATE/DELETE dispatch, streaming ROWID loops, old-row evaluation, complete-row validation, memory/error classification, checked counts, and focused predicate/count tests.
- `src/storage/relation.rs` — caller-limited mutable row decode, owned stored-row transfer, test-only late-failure injection, and relation regression coverage.
- `src/api/transaction.rs` — public explicit-write integration coverage for assignment semantics, parameters, three-valued predicates, row budgets, statement restoration, fatal rollback-only state, split/collapse behavior, commit, rollback, and reopen.
- `.superpowers/sdd/2026-09-13-phase-4c1-sql-execution/task-12-report.md` — Task 12 evidence, scope audit, and process record.

## Self-Review

- Re-read the Task 12 brief and the approved Phase 4C1 mutation/transaction constraints.
- Confirmed the branch HEAD and merge base both equal the required Task 11 binding base before this commit.
- Reviewed the complete production diff and the added transaction/relation regressions.
- Confirmed continuation is exclusive by immutable ROWID and visible value changes cannot influence traversal.
- Confirmed every assignment observes only the old row and complete replacement validation precedes mutation.
- Confirmed FALSE/NULL predicates are excluded and only successful matching mutations increment the checked count.
- Confirmed row decode, old row, assignment values, and replacement storage are bounded per row rather than accumulated across the table.
- Confirmed ordinary and fatal errors both restore the statement checkpoint, with only the three required fatal codes making the transaction rollback-only.
- Confirmed genuine timestamp/type errors retain the assignment SQL location while budget failures remain `ResourceLimit`.
- Confirmed no assertion was ignored, weakened, or deleted to make the implementation pass.
- Confirmed no dependency, unsafe code, public raw-storage API, persistent format, or out-of-phase behavior was added.

## Concerns

No open code-correctness concern.

Process deviation: before the controller explicitly prohibited further agents, the implementer dispatched two read-only child reviews (code review and test inventory). They did not edit the worktree and their results do not count as the required controller review. Their unofficial feedback did expose the row-budget versus genuine type-error classification regressions that were corrected and independently verified here. No further agent was dispatched; the controller will perform the official post-commit review.
