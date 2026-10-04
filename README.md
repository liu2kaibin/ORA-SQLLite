# ORA-SQLLite

ORA-SQLLite is a planned lightweight embedded database written in pure Rust. It combines a SQLite-style local storage model with an Oracle-compatible CRUD dialect.

## Selected boundaries

- In-process Rust library with a CLI added alongside the library.
- One main database file plus a persistent write-ahead log sidecar.
- Fixed-size pages, a pager, B+Tree tables and indexes.
- ACID transactions, crash recovery, multiple readers and one writer.
- Common Oracle data types, functions, sequences, pagination and NULL semantics.
- MVP scale acceptance is capped at 100,000 rows by the 2026-10-04 user ruling.
  The retained 10-million-row/10-GB harness is optional and no longer blocks
  Phase 5; no complete 10-million-row pass is claimed.

The first release will not implement PL/SQL, packages, a network server, Oracle wire protocols, users and roles, tablespaces, RAC or distributed transactions.

## Status

Phase 1 foundations are complete: validated Oracle-compatible values, stable errors,
disk codecs, versioned disk headers, checksums, and internal fault-injection
boundaries.

Phase 2 implements an internal single-file Pager and full-mutation B+Tree with a
bounded cache, checksums, generations, overflow storage, freelist reuse, validation,
and normal-close persistence.

Phase 3 implements full-durability WAL commits, deterministic recovery,
transaction-level read snapshots, multiple readers with one writer, checkpointing,
and single-process file ownership. Successful commits remain recoverable after a
normal close or process or machine termination, while uncommitted transactions
remain invisible.

Phase 4A provides V1 system catalog persistence, stable object IDs,
snapshot-visible table and column definitions, hidden ROWID allocation, and
canonical row encoding. These physical catalog and row-storage APIs are internal.

Phase 4B provides real create, open, connect, transaction, and prepare APIs.
Preparation uses an owned SQL parser and AST, snapshot catalog binding, static
type checking, and stable parameter/result metadata.

Phase 4C1 executes the complete documented Phase 4B SELECT subset with streaming
`Rows` and bounded memory. It provides transactional plain-table DDL,
parameterized INSERT, UPDATE, and DELETE, auto-commit, and both SQL and Rust
transaction entry points. Phase 4C2 adds the indexes and constraints below.
Oracle-specific type declarations, functions, `DUAL`, `ROWNUM`, pagination, and
empty-string compatibility behavior remain Phase 5 work.

Phase 4C2-A supports explicit ordinary and unique `CREATE INDEX` and `DROP INDEX`
DDL, DML index maintenance, and atomic rollback, reopen, and recovery of those
indexes. Later Phase 4C2-B/C/D work adds the constraints and SELECT index access
described below; `UPDATE`/`DELETE` target discovery remains a ROWID `TableScan`.

Phase 4C2-B supports persistent `PrimaryKey`, `Unique`, `NotNull`, and `Check`
constraints; system-owned unique indexes; atomic constrained-table lifecycle;
named `Constraint` errors; rollback, reopen, and recovery. Constraint evaluation
uses the existing bounded `WorkMemory` budget. Phase 5 Oracle extensions remain
deferred.

Phase 4C2-C supports immediate, named foreign keys that reference declared
PRIMARY KEY or UNIQUE constraints; child INSERT/UPDATE and parent UPDATE/DELETE
checks; optional child-index prefix probes with ROWID-scan fallback; atomic
reverse dependencies; and safe DROP TABLE rejection/cleanup. Self-reference,
cycles, ALTER TABLE, and deferred/referential actions remain unsupported.

Phase 4C2-D adds deterministic `IndexScan` access for eligible single-table
SELECT queries. It uses a contiguous leading equality/`IS NULL` prefix and at
most one range on the next index column (including a first-column range with no
equality prefix). Search operands are compatible literals or parameters; supported
range forms are `<`, `<=`, `>`, `>=`, and non-negated `BETWEEN`. Candidate priority
is unique full-key equality, longer equality prefix, range presence, more range
bounds, then lower stable `IndexId`; this is not a statistics-based cost optimizer.
Runtime NULL equality/range parameters produce an empty scan, while explicit
`IS NULL` is searchable.

Every scanned index entry must have an empty value and a strictly decoded key.
Its ROWID must find a table row whose indexed values re-encode byte-for-byte to
that key, before the complete original WHERE `Filter` evaluates the candidate.
Missing rows, malformed entries, mismatched keys, and encountered damaged pages
return `Corruption`; this is validation of the selected access path, not a scan
of all database pages. There are no covering/index-only scans. Decoded rows and
parameters retain bounded query-memory accounting.

Prepared access plans retain stable identities, not physical page roots or index
names for rebinding. Each execution opens catalog, table, and index roots from
its own read snapshot or current write batch, including that batch's uncommitted
changes. Ordinary root changes do not invalidate a prepared statement; changes to
the bound table schema or selected index identity return `SchemaChanged`.

JOIN inputs and UPDATE/DELETE target discovery remain `TableScan`-only. Unsupported
search terms (including OR, NOT, IN, LIKE, expressions on indexed columns, and
column-to-column comparisons) stay in the full Filter; without another usable
prefix/range the plan uses `TableScan`. Blob equality is searchable, but Blob
ranges are not; public SQL still rejects ordered Blob comparisons with `Type`.
Queries containing binary arithmetic, unary negation, or `SUM`/`AVG` in WHERE,
projection, GROUP BY, HAVING, or ORDER BY conservatively retain ROWID `TableScan`,
even when a usable index predicate exists. Skipping rows must not hide eager
predicate errors, and reordering candidates must not change rows emitted before
an error or aggregate overflow/rounding. `COUNT`/`MIN`/`MAX` with otherwise safe
arguments remain index-eligible. This is a conservative eligibility rule, not
constant folding or a change to expression evaluation.
Unary positive and comparisons (including IN/BETWEEN) also retain `TableScan`
when operand types remain unknown. Typed index parameters and inert unknown
parameter uses in projection, `IS NULL`, and `COUNT` remain eligible.
Legal bounds that exceed the index key representation limit fall
back to the same-view `TableScan` and full Filter, without suppressing real query
resource or storage errors. Index access does not remove sorting, DISTINCT,
grouping, or aggregation, and does not promise row order without ORDER BY.
No statistics, cost optimizer, ordering elimination, new public API, disk/catalog/
WAL format or feature bits, or Phase 5 behavior is added.

Creation synchronizes the main/WAL files and their parent directory entries before
publishing the format-2 initialization marker. Unix uses a directory file sync;
Windows uses a writable directory handle with `FILE_FLAG_BACKUP_SEMANTICS` and
`sync_all`. Filesystems or permissions that reject these operations cause creation
to fail; no weaker fallback is used. Durability depends on the OS, filesystem and
storage device honoring successful flushes. An initialized database never recreates
a missing WAL, even before its first checkpoint.

Recovery checks the configured WAL hard limit (1 GiB by default) before scanning.
Index publication is a constant-size state swap after WAL synchronization; preparing
that swap currently copies the total in-memory index before each commit. This is an
explicit Phase 3 performance/memory tradeoff, not a persistent-index implementation.

Phase 4A APIs remain crate-internal storage boundaries.
Phase 3 supports multiple readers and one writer only within one process; a second
process opening the same database receives `Busy`.

The on-disk format is now format 2; format-1 Phase 2 development databases must
be recreated.

See:

- [Architecture design](docs/superpowers/specs/2026-09-01-rust-oracle-compatible-embedded-db-design.md)
- [Phase 3 WAL and transactions design](docs/superpowers/specs/2026-09-05-phase-3-wal-transactions-design.md)

## Continuous integration

Pull requests targeting `main` run the unchanged fast `PR CI` workflow: Linux
formatting, compilation of all targets, and the `public_api`, `phase4c_api`, and
`phase4c2_constraint_api` test targets. It is also manually dispatchable.

`Deep CI` is **manual only** (`workflow_dispatch`); neither pushes nor pull requests
start it. It includes strict Clippy, the full stable/MSRV/Windows suites,
deterministic models, crash-recovery matrices, and 100,000-row acceptance tests.
The `phase4c2d-index-scan` job runs the ignored 4,000-operation fixed-seed
IndexScan/TableScan differential model followed by the ignored 100,000-row
IndexScan smoke using the exhaustive profile. It does not run the 10-million-row
diagnostic harness. Under the current MVP policy, 100,000 rows is also the
maximum local scale run. Hosted execution is separate from local verification
evidence.

Documentation-only pull requests are excluded from automatic PR CI, and pushes
do not trigger either workflow. For a documentation-only follow-up commit on a
pull request that already contains code, `[skip ci]` may be added to that commit
message. Never use `[skip ci]` for source, Cargo, workflow, build, or test changes.

### MVP scale acceptance

The 100,000-row IndexScan smoke is the maximum future scale run for the MVP. A
larger observation reached 4,700,000 committed rows and 18.286 GiB of owned
temporary database/WAL data without a database assertion, panic, capacity, or
corruption error. It was then stopped manually when the acceptance policy
changed. This is bounded partial-scale evidence, not a completed 10-million-row
pass.

The ignored `phase_4c2d_ten_million_row_ten_gb_acceptance` harness remains in the
repository as optional diagnostic tooling, but it is not run under the current
MVP policy and is not a Phase 5 gate. See the
[MVP scale acceptance record](docs/superpowers/plans/2026-10-04-phase-4c2d-mvp-scale-acceptance.md)
for the exact evidence boundary.

- [Bootstrap plan](docs/superpowers/plans/2026-09-01-project-bootstrap.md)
