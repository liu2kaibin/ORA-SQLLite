# Phase 5 final whole-branch fix report

## Root cause and single hypothesis

Phase 5 introduced V2 semantics at the value decoder and execution layers
without carrying the same capability and precision boundary through every
consumer. The resulting failures had one common shape: a later layer knew about
V2 Oracle values while an earlier boundary still applied a V1 assumption.

- Catalog open decoded CheckProgram V2 but did not require the durable
  `ORACLE_TYPES_V1` feature bit.
- Parameter normalization cloned the complete frame and reserved work memory
  only after allocating, so touched transient values could bypass the configured
  ceiling and untouched values inflated scratch use.
- NUMBER Text parsing allocated an input-sized digit buffer in addition to the
  already retained parameter value.
- Binder and aggregate planner structural hashes reduced timestamp literals to
  microseconds although equality and V2 persistence retain nanoseconds.
- The pre-scan implementation already traversed all query expression families,
  but the final review correctly identified that this contract lacked direct
  runtime coverage.

## Finding disposition

1. **CheckProgram V2 feature dependency — fixed.** Catalog reconstruction now
   rejects a V2 CHECK program unless `ORACLE_TYPES_V1` is present. The legacy
   future-version fixture now writes version 3, because version 2 is valid.
   Committed/recovery-style corruption coverage witnesses the missing-bit case.
2. **Parameter normalization memory ceiling — fixed.** Normalization validates
   and accounts only strictly ordered touched slots, reserves scratch before any
   clone or conversion, stages all converted values, preserves the frame on
   failure, then transfers transient accounting to retained accounting.
3. **NUMBER Text transient amplification — fixed.** Parsing is now a two-pass,
   allocation-free scan over the borrowed ASCII input. The existing bounded
   `Decimal::quantize` implementation remains unchanged.
4. **Sub-microsecond timestamp hashes — fixed.** Both structural hash sites use
   `nanoseconds()` and have focused p=7..9 regression tests.
5. **Query pre-scan runtime matrix — covered.** The Phase 5 API test exercises
   GROUP BY, aggregate argument, HAVING, projection, explicit ORDER BY, and
   successful compatible reuse across WHERE/projection/ORDER BY.

## RED evidence

The focused tests were introduced before their production fixes.

- `cargo test --lib phase_5_check_program_v2_requires_oracle_types_feature`
  failed because opening the V2-without-feature candidate returned
  `Ok(CatalogReader { ... })` instead of `ErrorCode::Corruption`.
- `cargo test --lib oracle_slot_normalization_enforces_touched_scratch_at_exact_boundary`
  failed at the below-limit boundary with `called Result::unwrap_err() on an Ok
  value: ()`, proving scratch allocation bypassed the memory ceiling.
- `cargo test --lib timestamp_structural_keys_include_submicrosecond_nanoseconds`
  and `cargo test --lib aggregate_plan_keys_include_submicrosecond_nanoseconds`
  each failed their `assert_ne!` because values differing only below one
  microsecond produced identical keys.
- The query pre-scan matrix passed when first added. It is intentionally a
  coverage-only disposition for a code-reviewed traversal, not a fabricated
  RED failure.

## GREEN evidence

Fresh final-tree evidence on Windows stable:

- `cargo test --lib oracle_type::tests` — 12 passed, 0 failed.
- `cargo test --lib phase_5_check_program_v2_requires_oracle_types_feature` —
  1 passed, 0 failed.
- `cargo test --lib oracle_slot_normalization_enforces_touched_scratch_at_exact_boundary`
  — 1 passed, 0 failed.
- `cargo test --lib submicrosecond_nanoseconds` — 2 passed, 0 failed.
- `cargo test --test phase5_oracle_types_api` — 27 passed, 0 failed.

`cargo test --lib` started all 1,029 library tests and emitted no failure before
two known stress tests crossed 60 seconds. It was intentionally interrupted
under the approved lightweight-MVP long-tail policy. Its exit status reflects
the interrupt, so this run is **partial evidence, not a full-library PASS**.

## Stable and MSRV verification

- `rustup run stable cargo check --all-targets` — exit 0.
- `rustup run 1.85.0 cargo test --test phase5_oracle_types_api` — 27 passed,
  0 failed.
- `rustup run 1.85.0 cargo test --lib oracle_slot_normalization_enforces_touched_scratch_at_exact_boundary`
  — 1 passed, 0 failed.
- `rustup run 1.85.0 cargo test --lib submicrosecond_nanoseconds` — 2 passed,
  0 failed.

`rustup run stable cargo clippy --all-targets -- -D warnings` remains a
**non-PASS** with 15 existing warnings promoted to errors in
`src/oracle_type.rs`, `src/sql/bind/types.rs`, `src/storage/index/key.rs`, and
`src/storage/row.rs`. None is in this final-fix diff; they are not widened into
this correctness patch.

## Format and diff checks

- `cargo fmt --all -- --check` — exit 0.
- `git diff --check` — exit 0. Git printed only the configured LF-to-CRLF
  working-copy notices.

## Changed files

- `src/storage/catalog/mod.rs` — CheckProgram V2 feature gate, future-version
  correction, and committed corruption regression.
- `src/sql/exec/params.rs` — touched-slot scratch accounting, atomic staging,
  and exact-boundary regression.
- `src/decimal.rs` — allocation-free two-pass NUMBER Text parser.
- `src/sql/bind/query.rs` — nanosecond structural hash and regression.
- `src/sql/plan/expr.rs` — nanosecond aggregate-plan hash and regression.
- `tests/phase5_oracle_types_api.rs` — complete query pre-scan runtime matrix.
- `.superpowers/sdd/2026-10-04-phase-5-oracle-types/final-fix-report.md` — this
  audit record (force-added because the SDD directory is ignored).

## Commit

Single commit subject: `fix: close Phase 5 final review findings`.
The resulting hash is reported by the final handoff; embedding it in the file
inside that same commit would make the hash self-referential.

## Remaining concerns

- Stable/MSRV complete `cargo test --all-targets` evidence remains partial under
  the approved long-tail policy. Linux CI is still a post-push gate.
- The prior 100,000-row acceptance run showed sustained activity without an
  observed failure but was intentionally stopped; it remains **partial/non-PASS**
  and was not rerun for this focused fix wave.
- Strict stable Clippy remains red only on pre-existing, out-of-diff warnings
  listed above.

## Self-review

- The diff is limited to the six expected source/test files plus this report;
  it adds no dependency, public API, unsafe code, or unrelated cleanup.
- Catalog validation runs while reconstructing both committed and recovery
  candidates, before a V2 program can reach evaluation.
- Slot order/existence and scratch size are validated before reservation;
  conversion errors release scratch and leave all parameters unchanged; only a
  fully staged set is installed.
- The two-pass decimal parser preserves sign, grammar, half-up magnitude
  rounding, negative scale, precision-overflow, and negative-zero behavior as
  witnessed by the existing Oracle type suite.
- Nanosecond hash inputs now match timestamp equality/persistence precision.
- No remaining final-review finding is silently deferred; every limitation is
  explicitly classified above.
