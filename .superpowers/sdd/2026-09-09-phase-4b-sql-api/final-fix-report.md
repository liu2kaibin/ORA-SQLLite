# Phase 4B final fix wave

Base: 303ee26d37ca9a35b67e0b5d98230a209c79611e. All eight final-review findings addressed; final Windows stable and Rust 1.85 verification completed successfully on 2026-09-11.

## Root-cause investigation (before implementation)

1. Query loss: parser creates all AstQuery clauses; bind/query.rs creates BoundQuery but omits sources/kinds. bind/statement.rs moves only projection into BoundStatementKind::Query; api/statement.rs faithfully owns that incomplete value. Neighboring UPDATE retains selection. Hypothesis: moving a complete query IR into bound/ and retaining that single value fixes the loss and enables complete schema traversal.
2. Stack safety: parse_statement passes OracleDialect to upstream. Its parse_subexpr loop grows left-deep infix trees without using additional recursion allowance; query.span/body.span and MoveOnlyParserExpr::span recurse before AstBudget.enter. An error drops the still-owned upstream tree recursively. Owned and bound Box trees also recursively drop; existing 30,000-node tests explicitly forget them. Hypothesis: a private Oracle dialect growth guard, bounded public nesting, and iterative owned/bound Drop address construction, rejection, spans and destruction separately. Subprocess tests must prove all paths.
3. MSRV: checked_constraint_name and StatementTokenIndex::new use let chains despite Cargo.toml rust-version=1.85. Neighboring let-else/nested-if code is stable. Hypothesis: equivalent nested conditionals restore compilation without behavior change.
4. Occurrence identity: BindingScope stores source aliases but ResolvedColumn discards source occurrence. BoundExpr::Column and structural_key/equality retain only stable table/column identity. Schema dependency dedup is appropriate but expression identity is not. Hypothesis: query-local SourceId carried from ordered scope through expressions and retained query sources separates self-joins without duplicating dependencies.
5. Parameters: constrain updates collector entries immediately; bind_expr snapshots types into nodes; apply_expected short-circuits unchanged Unknown and does not retain links. Thus later contexts cannot update earlier nodes or revisit earlier operators. Hypothesis: explicit linked type constraints, statement-wide solution, then complete rebinding/finalization using solved types will validate domains and refresh every node/metadata.
6. Parameter order: collector insertion follows JOIN/WHERE/GROUP/HAVING before SELECT; finish sorts only positional identifiers. Source spans already retain lexical locations. Hypothesis: seed/register lexical first occurrences independently of semantic bind traversal.
7. Booleans: common convert_expr converts identifiers into columns; only convert_query_expr runs normalize_query_boolean_literals. DML/CHECK use common conversion directly. Hypothesis: normalize only standalone unquoted TRUE/FALSE in common conversion, keeping quoted and qualified names intact.
8. Expansion: AstBudget counts a wildcard once; expand_star constructs a new Vec and clones every column for each star; query binder pushes unlimited outputs. No binding limits are passed from bind_sql. Hypothesis: an explicit checked bound-node budget charged before expansion and lazy star iteration prevent multiplicative allocations.

## Controller boundary

Retain catalog-root validation during prepare, unchanged. The ambiguity between no user-table-page reads and required existing root validation remains a specification clarification, not permission to weaken validation. No storage persistence/recovery production changes are intended.

## Design changes

- `bound/query.rs` owns the complete query: DISTINCT, ordered `BoundSource`s, ordered `BoundJoin`s (kind, source occurrence, ON), WHERE, GROUP BY, HAVING, projection and ORDER BY. `BoundStatementKind::Query` retains that value. ORDER BY aliases/ordinals still reference projection indices, not cloned expression trees. Output metadata/dependency copies remain immutable metadata, not alternate query semantics.
- `SourceId` is query-local and distinct from stable catalog IDs. Scope-derived column references, stars, structural hashing and equality all retain it. Schema dependencies remain first-source ordered and deduplicated by table/version. Tests prove two aliases of USERS have distinct expression identities but one dependency.
- Parameter collection records lexical minimum byte offsets independently of semantic traversal. Union-find collects type-preserving links; a bounded directed worklist propagates SUM/AVG result constraints (INT64/DECIMAL input -> DECIMAL result; FLOAT64 -> FLOAT64). Assignment caps remain directional. The solver completes before one final full bind, which revalidates operator domains, aggregate inputs, grouping and all assignment expressions and rebuilds output metadata. No earlier bound tree survives finalization. Unresolved SUM/AVG input still returns Type; generic unconstrained parameters remain Unknown.
- `BindingBudget` is carried in the binding context. The existing configured node limit independently bounds AST nodes and retained bound-expression nodes, charging expression leaves/operators and all wildcard-expanded columns. Star counts use checked addition and are charged before iteration/allocation; iteration yields columns lazily without intermediate per-star vectors. Derived-constraint edges also have an explicit node-limit-sized cap. Finalization resets per-pass retained-node accounting after the prior tree is dropped; it is not an unbounded retry loop.
- Common expression conversion normalizes only standalone unquoted TRUE/FALSE. The query-only normalization walk was removed. Quoted and qualified boolean-looking identifiers remain columns.
- The private Oracle dialect preserves the upstream Oracle TypeId and overrides its growth boundary: before each infix addition, an iterative check examines the already-owned left input. The Pratt loop therefore cannot construct unbounded flat chains before span calculation or syntax-error Drop. Unsupported infix subtrees are rejected rather than counted as opaque leaves. Lexical parentheses are bounded before parser descent; unsupported set-operation chains are rejected before their recursive tree can grow. This preflight preserves significant-token qualification and the single-statement boundary.
- Default nesting remains 128; the documented hard implementation maximum is now 256 (257 and 65,535 reject). Parser recursion and owned AST depth use the same requested limit. The dependency and current expression converter still use recursive frames, so all third-party construction/conversion/destruction runs on a scoped, joined 32 MiB parser stack, independent of the caller's stack. Thread creation failure is an ordinary Io error. Panics are not swallowed or converted into successful parses; a join panic resumes unwinding. No parser AST crosses that worker boundary. The worker is not a background service.
- Owned AST and bound expressions detach child nodes and Drop them iteratively, including partially constructed/error paths. Both pre-existing deliberate `mem::forget` calls were removed from each deep-tree regression. No unsafe code was added.
- Both let chains were replaced with equivalent nested conditionals. No dependency versions/features, storage persistence/recovery code, execution, physical plans, catalog formats or Phase 5 behavior changed.

## Meaningful RED evidence

All commands below ran in this worktree. Cargo test process exit is 1 on this host for failing tests/builds. The initial complete behavioral RED was `cargo test final_fix_ -- --nocapture`: **0 passed, 8 failed, 0 ignored, 560 filtered**, exit 1. These were real parse/bind/public-prepare tests against the real catalog fixture, not source-text checks or mocks.

| Finding | Test suffix (under `sql::bind::test_support::final_fix_`) | Observed RED |
|---|---|---|
| 1 | `retains_query_semantics_at_public_statement_boundary` | WHERE ID=1 and ID=2 produced equal final `Query { projection: ... }` values; assert-ne failed. |
| 2 | `safety_subprocess` | 10,000-term flat chain child terminated with Windows status **3221225725 / 0xC00000FD**, stack overflow; parent reported a normal test failure. Expanded diagnostic run also reproduced overflow for 10,000 nested parentheses, 30,000-node owned/bound normal Drop, and the normal prepared 120-term chain. |
| 4 | `self_join_grouping_distinguishes_source_occurrences` | `SELECT M.ID ... GROUP BY U.ID` returned Ok instead of Bind, with both aliases mapped only to table 1/column 2. |
| 5 | `parameters_finalize_earlier_outputs` | First `:p` output was Unknown instead of Int64 for `SELECT :p, :p + 1`. |
| 5 | `parameters_revalidate_linked_types_and_domains` | `:a=:b AND :a=1 AND :b='x'` incorrectly returned Ok with A Int64/B Text. |
| 6 | `parameter_metadata_is_lexical_through_public_prepare` | Actual order JOINED,FILTER,PROJECTION,ORDERED; expected PROJECTION,JOINED,FILTER,ORDERED. |
| 7 | `boolean_literals_use_common_expression_conversion` | INSERT TRUE into INT64 returned Bind (unknown column), not Type (Bool incompatible with INT64). |
| 8 | `wildcard_expansion_obeys_bound_node_budget` | Three four-column stars were accepted with an 11-node budget instead of rejecting 12 output nodes. |

MSRV RED (finding 3):

```powershell
$env:RUSTC='C:\Users\liukaibin\.rustup\toolchains\1.85.0-x86_64-pc-windows-msvc\bin\rustc.exe'
& 'C:\Users\liukaibin\.rustup\toolchains\1.85.0-x86_64-pc-windows-msvc\bin\cargo.exe' test --all-targets --target-dir target\final-fix-msrv185
```

Actual explicit compiler: **rustc 1.85.0 (4d91de4e4 2025-02-17)**. Clean isolated build failed with exactly two E0658 let-expression errors at the then-current bind/statement.rs:671 and parser/statement.rs:1591. Cargo reported `could not compile ... due to 2 previous errors`, exit 1.

Additional real REDs found during self-review, before their fixes:

- `cargo test final_fix_parameter_finalization -- --nocapture`: 0 passed, 1 failed; grouping returned Bind for `ID + :p GROUP BY ID + :p HAVING :p = 1.5`. Hypothesis confirmed: grouping structural/type checks must wait for parameter solution. After deferring this check during collection, the same test passed and output is Decimal.
- `cargo test final_fix_parameter_solver_propagates --target-dir target\final-fix-derived -- --nocapture`: 0 passed, 1 failed; an earlier `:q` output remained Unknown instead of Decimal after `SUM(:p)=:q`, and chained `SUM(:q)=:r`. Hypothesis confirmed: union links alone cannot model derived aggregate result types. Added directed worklist constraints, then the same regression passed.
- `cargo test final_fix_depth_preflight --target-dir target\final-fix-msrv185-probe -- --nocapture`: 0 passed, 1 failed; qualified `T . UNION` incorrectly stole a real UNION error location (11 instead of 24). Hypothesis confirmed: preflight needs previous significant token, not previous raw token. Same regression then passed with byte offset 24.
- `cargo test final_fix_parameter_solver_keeps_single --target-dir target\final-fix-self-link -- --nocapture`: 0 passed, 1 failed, 575 filtered; `SELECT :p, SUM(:p) = :p, :p + 1 FROM USERS` left the first output Int64 instead of Decimal. Self-review traced this to the optimization that skipped link collection with fewer than two distinct parameters: directed aggregate self-links are still necessary. Removed only that optimization and covered both SUM and AVG, including earlier/final columns and parameter metadata.

## GREEN evidence and boundary coverage

Focused executions, each exit 0 after its fix:

- `cargo test final_fix_boolean -- --nocapture`: 1 passed initially; later expanded with real BOOL-target INSERT/UPDATE/DELETE and quoted/qualified column metadata.
- `cargo test final_fix_retains -- --nocapture`: 1 passed. Final-public-statement comparisons cover WHERE, DISTINCT, INNER versus LEFT join, grouping/HAVING and direction/NULL placement in ordering.
- `cargo test final_fix_self_join -- --nocapture`: 1 passed. Extended assertions also inspect retained source IDs 0/1 and deduplicated dependency count 1.
- `cargo test final_fix_parameter --target-dir target\final-fix-derived -- --nocapture`: 6 passed, including transitive linked parameters, derived SUM/AVG result propagation, MIN result finalization, IN/BETWEEN/domain conflicts, DML/CHECK contexts and deferred grouping.
- After the single-parameter self-link fix, `cargo test final_fix_parameter --target-dir target\final-fix-self-link -- --nocapture`: **7 passed**, 0 failed, 569 filtered, exit 0; SUM and AVG self-links now finalize all outputs and metadata consistently.
- `cargo test final_fix_ -- --nocapture`: initial coherent GREEN 8/8, then 11/11 with complete schema traversal and added parameter cases. Later final full-suite coverage includes all 16 final-fix tests.
- `cargo test final_fix_depth_preflight -- --nocapture`: 1 passed, 574 filtered.
- `cargo test final_fix_safety_subprocess -- --nocapture`: 1 parent test passed, 574 filtered; the parent requires each child output to contain `1 passed`, not merely a successful zero-test exit.

The final safety test launches **seven isolated child cases**: long flat input; rejected malformed input; deeply nested input; 30,000-node owned/bound normal Drop; normal prepared-statement Drop; configured depth boundaries; unsupported cast/NOT/set-operation chains. The configured case checks limit-minus-one, exact and over-limit AST depths at **128, 255 and 256**, malformed expression cleanup, and rejection of hard maximum+1. Children use the real parser/binder; no leaks, mocks or source checks hide failure.

Schema validation coverage starts from a public prepared statement containing every clause. It replaces a real column identity in each of projection, ON, WHERE, GROUP BY, HAVING and expression ORDER BY with an absent ID; all six return SchemaChanged against the real catalog, while the unmodified statement validates. Alias ORDER BY remains a shared projection index.

Expansion coverage checks a 12-output query at budgets 11/12/13. The max-width fixture uses **1,000 real catalog columns**: 99 stars return 99,000 columns; 100 stars return 100,000; 101 stars reject; 5,000 stars (potentially 5 million outputs from small SQL) reject at the binding budget. Main/WAL bytes are compared after public handles release the Windows file lock.

## Harness/tooling corrections (not counted as valid REDs)

- Initial lexical-order test used bare JOIN, which the existing accepted surface excludes; changed to explicit INNER JOIN before recording meaningful RED.
- Initial subprocess used the wrong test module path and accidentally ran zero tests. Corrected to `sql::bind::test_support`, required a `1 passed` output, and reran before implementation; all reported stack-overflow evidence is from the corrected harness.
- `cargo +1.85.0` is not supported by this host's direct Cargo executable; `rustc +1.85.0 --version` printed the default compiler. `rustup run ... cargo` also resolved a default compiler through PATH. These are not MSRV evidence. Explicit compiler path and a clean isolated target directory were required. Initial sandbox execution was denied (os error 5); the approved escalation ran the genuine compiler. A first shared-target attempt showed E0514 stale artifacts; isolated MSRV RED removed that ambiguity.
- Windows prevented relinking a test executable while another suite was still running (LNK1104). Used isolated build targets; this is not a behavioral RED.
- A new schema-validation test initially held a public DB open while opening a second fixture pager (Busy). Corrected fixture handle lifetime.
- A max-width test initially compared bytes while the DB's Windows lock was held (os error 33), after its expansion assertions had completed. Corrected the test to release connection/database first. Fresh final gates rerun the corrected test; earlier runs are not accepted as final gates.
- One intermediate fmt check and Clippy run reported formatting/needless mutable-reference borrows; these were corrected and rerun. No warnings were suppressed.

## Final verification

Every acceptance command below ran after the last production change (the single-parameter aggregate self-link fix). No production/test source changed during or after these gates. Stable compiler: **rustc 1.98.0 (88d9e12ae 2026-08-18)**. MSRV compiler: **rustc 1.85.0 (4d91de4e4 2025-02-17)**, selected by explicit compiler path, not inferred from a wrapper name.

- `cargo fmt --check`: exit 0, no diff.
- `cargo clippy --all-targets --all-features --target-dir target\final-fix-self-link -- -D warnings`: exit 0, clean.
- `cargo test sql:: --target-dir target\final-fix-self-link -- --skip final_fix_wide_catalog_expansion_stops_at_default_budget_without_writes --skip phase_4b_random_sql_never_panics`: **153 passed**, 0 failed, 423 filtered, exit 0 (54.71 s). This focused run includes all SQL parser/binder/corpus/safety regressions except the two expensive tests, which are included in both full acceptance gates.
- `cargo test --test public_api --target-dir target\final-fix-self-link -- --nocapture`: **10 passed**, 0 failed, exit 0 (1.04 s).
- `cargo test --all-targets --all-features --target-dir target\final-fix-self-link`: **568 unit tests passed**, 0 failed, 8 ignored, 0 filtered (551.21 s); **10 public integration tests passed**, 0 failed (1.85 s); process exit **0**.
- Explicit Rust 1.85 full command below: **568 unit tests passed**, 0 failed, 8 ignored, 0 filtered (516.47 s); **10 public integration tests passed**, 0 failed (1.75 s); process exit **0**.

```powershell
$env:RUSTC='C:\Users\liukaibin\.rustup\toolchains\1.85.0-x86_64-pc-windows-msvc\bin\rustc.exe'
& $env:RUSTC --version
& 'C:\Users\liukaibin\.rustup\toolchains\1.85.0-x86_64-pc-windows-msvc\bin\cargo.exe' test --all-targets --target-dir target\final-fix-msrv185-acceptance
```

Both full gates include all **16** final-fix tests, the complete SQL corpus, all **10,000** fixed-seed random inputs, seven safety subprocesses, ordinary storage/catalog/recovery regressions, and the corrected real 1,000-column expansion/no-write fixture. The eight pre-existing ignored heavy tests remain ignored as explained below. No filtered/zero-test run is counted as acceptance.

Final gate logs remain local and ignored: `target/final-fix-stable-acceptance.log`, `target/final-fix-msrv185-acceptance.log`, and `target/final-fix-sql-focused-acceptance.log`. Commands piped through Tee preserve `$LASTEXITCODE` and explicitly exit with it. Only the verified results are committed, not generated artifacts. Earlier full runs passed 567+10 before the self-link addition, but are superseded by the final 568+10 gates above.

## Self-review and mutation checks

No extra agent or reviewer was dispatched. Self-review checked parser -> owned AST -> binding scopes/type constraints -> bound query -> final public Statement across all eight findings. Mutation-oriented checks are behavioral: restoring projection-only query storage, removing occurrence IDs, removing union or derived links, omitting final binding/grouping deferral, sorting by traversal order, restoring SELECT-only boolean handling, omitting wildcard charging, removing infix growth guards or iterative Drop, and restoring let chains each have corresponding RED/regression/compiler evidence above. Schema-ID mutation tests exercise all retained expression roots.

Isolation checks: production sqlparser references remain confined to `src/sql/parser/`; no SQL/API unsafe or `mem::forget` matches; no newly exposed execute/query/Row/cursor/planner/executor. Broad name searches only found existing storage RowCodec/RowLayout and private test/parser helper names. `src/storage` has zero diff; connection/transaction catalog-reader calls and their validation are unchanged. No Cargo.lock, DB/WAL, target or temporary probe artifacts are tracked. The report is intentionally force-added from the otherwise ignored review-work directory.

## Changed files

`src/api/statement.rs`; `src/sql/ast/expr.rs`; `src/sql/bind/{mod,query,scope,statement,types}.rs`; `src/sql/bound/{expr,mod,query,statement}.rs`; `src/sql/limits.rs`; `src/sql/mod.rs`; `src/sql/parser/{dialect,expr,mod,query,statement}.rs`; `src/sql/test_support.rs`; `tests/public_api.rs`; this report. New production files are only `bound/query.rs` and `parser/dialect.rs`.

## Scope, omissions and concerns

- Retained catalog-root validation exactly as ruled. No storage/persistence/recovery production file changed. Therefore the dedicated ignored Phase 3 crash, Phase 4A catalog crash and exhaustive page-size matrices were not rerun in this SQL-only wave; full normal suites still include their ordinary storage/catalog/recovery regressions. This is an evidence-based scope omission, not a claim those ignored matrices ran.
- Local verification is Windows MSVC. Linux execution remains the existing CI job's responsibility; no Linux run is claimed here.
- The scoped 32 MiB parser stack reserves bounded per-prepare address space and adds thread-start overhead. It is a deliberate safety boundary for the current recursive dependency/adapter, not a performance benchmark claim. Configurations above nesting 256 now fail validation instead of advertising unsafe depths. Public default 128 remains unchanged and exact boundaries are covered in subprocesses.
- The real 1,000-column fixture is intentionally expensive in debug mode because it uses production catalog writes/validation. It is not replaced with a fake catalog.

## Commits

Implementation and this verified report are committed together. The implementation commit SHA will be recorded here in a report-only follow-up commit, avoiding a self-referential hash.
