# Phase 4C2-D MVP Scale Acceptance

Date: 2026-10-04

## Decision

The user accepted the observed Phase 4C2-D scale behavior as sufficient for the MVP and capped future scale validation at 100,000 rows. The retained 10-million-row/10-GB harness is optional diagnostic tooling and is no longer a Phase 5 gate.

## Evidence boundary

- The established 100,000-row IndexScan acceptance remains the maximum future scale run.
- The partial large-scale run started on 2026-10-03 at 08:49:17 +08:00 and was manually stopped on 2026-10-04 at 11:51:30 +08:00 after the acceptance policy changed.
- It committed 4,700,000 of the planned 10,000,000 rows.
- Its owned temporary database and WAL totaled 19,634,050,944 bytes (18.286 GiB) when cleaned up.
- Before the manual stop, the process remained responsive and reported no database assertion, panic, capacity error, or corruption error.
- Exit code `-1` and Cargo's final failure line were caused by the deliberate process termination; they are not a product test failure.
- This evidence must not be described as a completed 10-million-row/10-GB acceptance run.

## Result

Phase 4C2-D is accepted for the MVP using the verified 100,000-row gate plus the bounded partial-scale observation above. Phase 5 is no longer blocked by the retired 10-million-row/10-GB requirement.
