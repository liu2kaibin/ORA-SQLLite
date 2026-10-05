# Phase 6 Rust Developer Preview Design

**Date:** 2026-10-05  
**Status:** Approved design, pending implementation plan  
**Release target:** GitHub pre-release `v0.1.0`

## 1. Purpose

Phase 6 turns the completed embedded database engine into a release candidate
that a Rust application developer can adopt from GitHub without learning the
internal storage architecture. It does not add SQL features. It documents,
exercises, packages, and publishes the behavior already implemented through
Phase 5.

The primary success case is a new Rust developer who can add the tagged Git
dependency, run a real CRUD example within ten minutes, understand the Oracle
compatibility boundary, and use the documented public API without reading
ORA-SQLLite's source.

## 2. Approved boundaries

### 2.1 In scope

- Audit and document the existing public Rust API for database creation/open,
  connections, prepared statements, parameters, result rows, explicit
  transactions, Oracle values, resource options, and structured errors.
- Add real executable examples for the common CRUD lifecycle and Oracle type
  handling.
- Add a developer quickstart, public API guide, Oracle compatibility matrix,
  changelog, licenses, and `v0.1.0` release notes.
- Complete Cargo package metadata and verify the source package without
  publishing it to crates.io.
- Add a manually dispatched, read-only GitHub Release Preview validation
  workflow.
- Establish the `v0.1.x` API and format-2 compatibility promise defined below.

### 2.2 Out of scope

- New SQL syntax, scalar functions, `DUAL`, sequences, `ROWNUM`, pagination,
  `ALTER TABLE`, views, triggers, subqueries, CTEs, or JSON.
- A CLI, network protocol, async API, ORM integration, C ABI, or bindings for
  another language.
- A new disk format, query optimizer, storage redesign, or performance
  refactor.
- crates.io publication.
- Automatically creating a Git tag or GitHub release.
- Making Deep CI, a complete 100,000-row run, or previously waived long-tail
  suites a new blocking requirement.

## 3. Release and compatibility contract

### 3.1 Release identity

The crate remains named `ora_sqllite` and keeps version `0.1.0`. The first
developer release is an explicitly marked GitHub pre-release tagged `v0.1.0`.
The README dependency example is:

```toml
[dependencies]
ora_sqllite = { git = "https://github.com/liu2kaibin/ORA-SQLLite.git", tag = "v0.1.0" }
```

The Phase 6 implementation must not create or push that tag. Tagging and
pre-release creation occur only after the implementation PR is merged, the
Release Preview workflow is green, and the user gives a separate explicit
approval.

### 3.2 `v0.1.x` stability promise

Within patch releases in the `v0.1.x` line:

- documented public types, methods, parameter conventions, and stable numeric
  `ErrorCode` values remain source compatible;
- format-2 databases created by `v0.1.0` remain readable and writable;
- existing V1 records inside format 2 remain supported;
- optional Oracle error metadata may be added where absent, but it may not
  change the stable `ErrorCode`, fatality, or successful behavior;
- diagnostic message text and execution plans are not stable contracts.

A breaking Rust API or format change requires at least `v0.2.0` and a written
migration note.

## 4. Public API documentation

### 4.1 Documented surface

Documentation covers every item exported by `src/lib.rs`, including:

- `Database`, `Options`, and `AccessMode`;
- `Connection`, `Statement`, parameter and result metadata;
- `Parameters`, `ParameterValue`, `Rows`, `Row`, and `ExecutionResult`;
- `ReadTransaction` and `WriteTransaction`;
- `Value`, `Decimal`, `OracleDate`, and `OracleTimestamp`;
- `DbError`, `ErrorCode`, `Result`, and `SqlLimits`.

Each public type describes ownership, lifecycle, important failure modes, and
the behavior that callers may rely on. Each public method documents arguments,
return values, relevant resource limits, and error behavior. Enum variants and
public modules receive meaningful documentation.

The crate enables the `missing_docs` lint only after the exported surface is
documented. Release validation treats documentation warnings as errors. Public
items must not be hidden or made private merely to satisfy the lint.

### 4.2 API change policy during Phase 6

Examples and documentation must use the current API directly. If they expose a
genuine inability to complete an approved use case, that is a design defect to
report, not permission for a broad convenience layer. Any production API
change must be the smallest source-compatible correction, have a failing public
test first, and receive an independent task review.

## 5. Executable examples

Examples use only the standard library and the public `ora_sqllite` API. They
must run on Windows and Linux, create only uniquely owned temporary files, close
and reopen the database where required, and remove only those owned files on
normal or error exit. They must not silently overwrite a caller's file.

### 5.1 `examples/quickstart.rs`

The quickstart is one end-to-end application flow:

1. create a database with `Options::default()`;
2. create a table with a primary key, required text, and numeric amount;
3. prepare and execute parameterized INSERT operations;
4. query rows and read typed `Value` instances;
5. update and delete with parameters;
6. demonstrate an explicit write transaction commit;
7. demonstrate that dropping an uncommitted write transaction rolls back;
8. close, reopen, and verify the committed result;
9. clean up its owned main and WAL files.

The example returns structured errors with `?` and a small top-level diagnostic
formatter. It does not use unexplained `unwrap()` calls.

### 5.2 `examples/oracle_types.rs`

The Oracle example creates and uses columns covering `NUMBER`, `VARCHAR2`,
`CHAR`, `RAW`, `DATE`, `TIMESTAMP`, and `TIMESTAMP WITH TIME ZONE`. It shows:

- construction and binding of `Decimal`, `OracleDate`, and `OracleTimestamp`;
- parameterized inserts and indexed reads;
- CHAR padding and empty-Text-to-NULL behavior;
- preservation of a TSTZ authored offset;
- a deliberately handled constraint or type failure that prints stable
  `ErrorCode`, optional Oracle code, SQL location when present, and constraint
  identity when present;
- close and reopen without losing normalized values.

Expected failures are asserted and handled; they do not cause the example
process to fail.

## 6. Developer documentation

### 6.1 README

The README becomes the landing page and contains:

- a concise product description and supported operating model;
- the tagged Git dependency snippet;
- a minimal create/insert/query code sample;
- commands to run both examples;
- links to the detailed guides and compatibility matrix;
- the current phase/status, MSRV 1.85, format 2, and the lightweight MVP
  verification boundary.

Historical implementation detail stays in the phase specifications rather
than dominating the landing page.

### 6.2 Detailed guides

`docs/getting-started.md` explains database ownership, create versus open,
connections, statement reuse, positional and named parameters, row iteration,
read/write transactions, rollback, read-only mode, resource options, and error
handling.

`docs/oracle-compatibility.md` is a three-state matrix:

- **Supported:** syntax and behavior implemented and covered by public tests;
- **Partial:** the exact supported subset and deviation are stated;
- **Unsupported:** the feature is rejected or intentionally absent.

The matrix covers data types, CRUD, predicates, joins, grouping, ordering,
indexes, constraints, transactions, NULL/empty Text, timestamps, sequences,
pagination, functions, PL/SQL, networking, and multi-process access. It never
uses an unqualified "Oracle compatible" claim.

`docs/releases/v0.1.0.md` records the release scope, compatibility promise,
validation commands and observed results, known limitations, partial/non-PASS
evidence, and upgrade expectations.

`CHANGELOG.md` follows a small Keep-a-Changelog-style structure with an
`[Unreleased]` section and the `0.1.0` developer preview entry.

## 7. Licensing and package metadata

The project uses the dual license `MIT OR Apache-2.0` and adds the canonical
`LICENSE-MIT` and `LICENSE-APACHE` texts. The MIT copyright line is:

`Copyright (c) 2026 ORA-SQLLite contributors`

`Cargo.toml` adds at least:

```toml
license = "MIT OR Apache-2.0"
readme = "README.md"
keywords = ["database", "embedded", "oracle", "sql", "rust"]
categories = ["database-implementations"]
```

The package includes the library source, examples, README, changelog, licenses,
and user guides needed to understand it. Scratch worktrees, test databases,
WAL files, target output, Superpowers scratch artifacts, and internal execution
reports are excluded.

`cargo package` is a validation step only. No task invokes `cargo publish`.

## 8. Release Preview workflow

Add `.github/workflows/release-preview.yml` with `workflow_dispatch` only and
`contents: read` permissions. It has no release, tag, package-publish, or write
permission.

The stable Linux job runs:

1. formatting check;
2. `cargo check --all-targets`;
3. documentation build with warnings denied;
4. doctests;
5. both executable examples;
6. the existing fast public API integration targets;
7. `cargo package` and inspection of the resulting package file list.

The Rust 1.85 Linux job checks all targets and runs both examples plus the
focused public API targets. Jobs use bounded timeouts. The workflow does not run
Deep CI or ignored scale tests.

The existing PR CI continues compiling all targets, so examples remain build
checked on every code PR. Running examples and packaging are release-preview
gates, not extra normal-PR workload.

## 9. Errors and safety

- Documentation distinguishes `Database::create` from `Database::open` and
  never suggests that open creates a missing database.
- Examples never overwrite a path they do not own.
- Error examples preserve the stable `ErrorCode`/optional Oracle metadata
  boundary and do not parse diagnostic strings.
- Transaction examples make commit, explicit rollback, and drop rollback
  visible.
- The compatibility matrix states single-process file ownership, multiple
  readers/one writer, transaction-level read snapshots, fixed-offset-only
  TSTZ, and the lack of Oracle wire compatibility.
- No documentation exposes internal pager, WAL, catalog, object ID, index key,
  or fault-injection APIs as supported interfaces.

## 10. Verification strategy

### 10.1 Documentation and examples

- `cargo doc --no-deps` with documentation warnings denied;
- `cargo test --doc`;
- `cargo run --example quickstart`;
- `cargo run --example oracle_types`;
- tests that ensure example cleanup targets are owned and cannot escape their
  temporary directory where such helper logic is non-trivial;
- a link/command audit so every README command and local relative link resolves.

### 10.2 Packaging

- `cargo metadata --no-deps --format-version 1` validates declared metadata;
- `cargo package` verifies the packaged source builds;
- an allowlist assertion verifies required guides, examples, changelog, and
  licenses are present and build output/scratch artifacts are absent;
- no crates.io credentials are used.

### 10.3 Engine regression

Phase 6 does not replace existing engine gates. At minimum it reruns formatting,
all-target compilation, the public API, Phase 4C, Phase 4C2 constraint, and
Phase 5 Oracle type integration targets on stable. Rust 1.85 runs the release
examples and focused API targets.

Known long-running all-target suites, Deep CI, and the manual 100,000-row tests
retain their previously approved evidence status. Release notes must label a
partial or unexecuted gate accurately; they may not imply a complete PASS.

## 11. Deliverables

- fully documented exported Rust API;
- two executable, self-cleaning examples;
- developer quickstart and Oracle compatibility matrix;
- `v0.1.0` pre-release notes and changelog;
- MIT and Apache-2.0 license files;
- complete Cargo metadata and verified package contents;
- manual read-only Release Preview workflow;
- updated README and accurate project status.

## 12. Exit criteria

Phase 6 implementation is complete when:

1. a clean clone can build documentation without warnings;
2. a new developer can follow the README and run both examples within ten
   minutes without editing ORA-SQLLite internals;
3. the examples prove CRUD, parameters, transactions, rollback, structured
   errors, Oracle types, and close/reopen behavior through public APIs only;
4. every exported public API item is documented;
5. the compatibility matrix is evidence-bounded and names unsupported Oracle
   features explicitly;
6. `cargo package` succeeds and contains only intended release material;
7. stable and Rust 1.85 release-preview gates pass;
8. the workflow has no write or publish permissions;
9. no SQL, disk-format, dependency, or public-API feature expansion was added;
10. the implementation PR is independently reviewed and merged.

Tag `v0.1.0` and creation of the GitHub pre-release are separate post-Phase-6
operations requiring explicit user approval. They are not implementation exit
criteria.

