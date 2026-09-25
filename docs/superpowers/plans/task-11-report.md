# Task 11 Validation Report

Date: 2026-09-25

## Delivered scope

- Added the ignored 100,000-row Phase 4C2-B constrained-table acceptance and a
  normal 100-row regression for a valid persisted freelist after constrained
  `UPDATE`.
- Corrected the independent format-2 test witness to recognize freelist page
  type `7`; it remains independent of production page decoders and enums.
- Added the dedicated `Linux Phase 4C2-B local constraints` GitHub Actions job
  with the fixed-seed model, crash matrix, and 100,000-row smoke commands.
- Documented the verified local-constraint scope and its Phase 4C2-C, 4C2-D,
  and Phase 5 boundaries in the README.

## Local verification

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | PASS |
| strict stable Clippy | PASS |
| stable `cargo test --all-targets` | PASS: 869 unit passed, 16 ignored; all integration targets passed |
| 100-row persisted-freelist regression | PASS |
| exhaustive 100,000-row constrained smoke | PASS in 1,721.54 s |
| fixed-seed constraint model | PASS: 3 seeds, 6,000 total steps, 1,102.26 s |
| constrained crash recovery matrix | PASS: 8 scenarios, 491 targets, 2,129.51 s |

The fixed-seed model used `0x4c2b0001`, `0x4c2b0002`, and `0x4c2b0003`.
The crash matrix reported every observed target recovered to exactly the old or
new state.

## Scope audit

`git diff --check` passed. The required keyword scan found existing
ForeignKey/ReferencedBy parser, binder, and rejection coverage plus explicit
deferred-boundary documentation. This task adds no foreign-key,
`IndexScan`, `ALTER TABLE`, cascade, deferrable, or Phase 5 behavior.

## Outstanding external evidence

- Rust 1.85 full-suite validation is **BLOCKED**, not passed: it could not
  start `rustc -vV` because Windows returned `拒绝访问` (os error 5).
- Remote GitHub Actions is **PENDING**. It was not triggered locally, so no
  remote CI URL exists yet.
