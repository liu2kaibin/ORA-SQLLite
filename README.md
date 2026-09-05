# ORA-SQLLite

ORA-SQLLite is a planned lightweight embedded database written in pure Rust. It combines a SQLite-style local storage model with an Oracle-compatible CRUD dialect.

## Selected boundaries

- In-process Rust library with a CLI added alongside the library.
- One main database file plus a temporary write-ahead log.
- Fixed-size pages, a pager, B+Tree tables and indexes.
- ACID transactions, crash recovery, multiple readers and one writer.
- Common Oracle data types, functions, sequences, pagination and NULL semantics.
- Initial capacity target: databases up to 10 GB and tables up to 10 million rows.

The first release will not implement PL/SQL, packages, a network server, Oracle wire protocols, users and roles, tablespaces, RAC or distributed transactions.

## Status

Phase 1 foundations are complete: validated Oracle-compatible values, stable errors,
disk codecs, versioned disk headers, checksums, and internal fault-injection
boundaries.

Phase 2 implements an internal single-file Pager and full-mutation B+Tree with a
bounded cache, checksums, generations, overflow storage, freelist reuse, validation,
and normal-close persistence.

Phase 2 has no WAL. A process or machine failure during a multi-page commit can leave a partially updated file; only successful commit followed by normal close/reopen is guaranteed.

WAL, SQL, and the public database CRUD API remain unimplemented.

See:

- [Architecture design](docs/superpowers/specs/2026-09-01-rust-oracle-compatible-embedded-db-design.md)
- [Bootstrap plan](docs/superpowers/plans/2026-09-01-project-bootstrap.md)
