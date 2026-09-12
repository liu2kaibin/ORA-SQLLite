# ORA-SQLLite Phase 4B SQL Frontend and Public API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现可在应用中真实打开、连接、开始事务和准备 SQL 的公共 API，并把受支持的通用 SQL 编译为只含稳定目录身份和静态类型的项目自有 `BoundStatement`。

**Architecture:** `sqlparser` 只存在于私有解析适配层，解析结果立即转换为自有 AST；规范化和绑定层通过 Phase 4A 的快照目录解析对象、参数及类型。公共 API 包装 Phase 3 Pager/事务，`Statement` 保存数据库 UUID、稳定对象 ID 和模式版本，但本阶段不增加规划器、执行器或 CRUD 数据访问。

**Tech Stack:** Rust 2024 edition、Rust 1.85 MSRV、`sqlparser = 0.62.0` 的 `std`-only 配置、`getrandom = 0.4.3`、现有 Pager/WAL/快照/Phase 4A 目录、标准库测试和固定种子模型。

**Spec:** `docs/superpowers/specs/2026-09-09-phase-4b-sql-api-design.md`

## Global Constraints

- 公共 API 只提供真实可用的 `Database`、`Connection`、`ReadTransaction`、`WriteTransaction`、`Statement` 和元数据；不提供 `execute`、`query`、`Row` 或游标。
- `sqlparser` 类型只能出现在 `src/sql/parser/` 内；自有 AST、绑定结果、公共 API 和存储层不得引用第三方 AST、span 或错误。
- `sqlparser` 精确为 `=0.62.0`，`default-features = false`，只启用 `std`；解析器和自有 AST 默认递归上限都为 128。
- `getrandom` 精确为 `=0.4.3`，`default-features = false`，只启用 `std`；数据库 UUID 在任何文件 I/O 前生成并规范化为 RFC 4122 variant 的版本 4。
- 未引用标识符按 ASCII 转大写，引用标识符保留 UTF-8 内容；规范化后最大 128 字节。
- 一条 SQL 只能使用位置参数或命名参数之一；位置参数为连续 `:1..=:65535`，命名参数按首次出现稳定排序。
- SQL 默认上限为 1,048,576 字节、100,000 token、100,000 AST 节点、128 层、65,535 参数和每查询块 64 JOIN。
- 绑定只读取 Phase 4A 快照目录，不读取用户表页面；`prepare()` 不得修改主文件、WAL、目录或事务状态。
- 模式依赖使用数据库 UUID、`TableId`/`ColumnId` 和 `SchemaVersion`；失效返回 `ErrorCode::SchemaChanged = 1150`，不自动重编译。
- 本阶段解析和绑定 DDL、CRUD、事务、INNER/LEFT JOIN、分组、排序、聚合和基础约束；Phase 5 Oracle 函数、序列、`DUAL`、`ROWNUM`、分页和 Oracle 声明属性保持 `Unsupported`。
- 保持 `#![forbid(unsafe_code)]`；不提交数据库、WAL、临时探针、构建目录或 `Cargo.lock`。
- 每个任务先运行新测试观察预期失败，再写最小实现；提交前运行本任务焦点测试和 `cargo fmt --check`。
- 最终门禁为 Windows stable、Linux stable、Linux Rust 1.85、Clippy `-D warnings`、Phase 1–4A 回归及 Phase 4B 专用语料库。

## File Structure

- Modify `Cargo.toml`: 精确加入 `sqlparser` 与 `getrandom`。
- Modify `.gitignore`: 在当前仍为库 crate 时忽略根 `Cargo.lock`。
- Modify `src/lib.rs`: 注册 `api`/`sql`，导出公共 API、数据类型和 SQL 限制。
- Modify `src/error.rs`: 增加 `SchemaChanged`、`SqlLocation` 和位置访问器。
- Create `src/sql/mod.rs`: SQL 前端内部入口与层间导出。
- Create `src/sql/types.rs`: `SqlSpan`、内部 `SqlType` 和类型统一规则。
- Create `src/sql/limits.rs`: 公共 `SqlLimits`、默认值和验证。
- Create `src/sql/name.rs`: 标识符规范化、参数键及边界校验。
- Create `src/sql/ast/mod.rs`: 自有 AST 汇总。
- Create `src/sql/ast/expr.rs`: 未绑定表达式。
- Create `src/sql/ast/query.rs`: 查询、FROM、JOIN、分组和排序 AST。
- Create `src/sql/ast/statement.rs`: DDL、DML、事务和约束 AST。
- Create `src/sql/parser/mod.rs`: 单语句入口和第三方错误映射。
- Create `src/sql/parser/source.rs`: 字节偏移与行列转换。
- Create `src/sql/parser/tokens.rs`: Oracle 参数 token 改写和 token 上限。
- Create `src/sql/parser/expr.rs`: 第三方表达式到自有 AST 的允许列表转换。
- Create `src/sql/parser/query.rs`: SELECT/JOIN/GROUP/ORDER 转换。
- Create `src/sql/parser/statement.rs`: DDL、DML 和事务转换。
- Create `src/sql/bound/mod.rs`: 绑定语句和模式依赖汇总。
- Create `src/sql/bound/expr.rs`: 强类型绑定表达式。
- Create `src/sql/bound/statement.rs`: 查询、DDL、DML 和事务绑定结果。
- Create `src/sql/bind/mod.rs`: 绑定入口和参数收尾。
- Create `src/sql/bind/scope.rs`: 表别名、列和星号解析。
- Create `src/sql/bind/types.rs`: 表达式类型约束、聚合与参数推断。
- Create `src/sql/bind/query.rs`: 查询块、分组和结果列绑定。
- Create `src/sql/bind/statement.rs`: DDL/DML/事务绑定。
- Create `src/sql/test_support.rs`: 仅测试使用的真实目录、解析和绑定 fixture。
- Modify `src/storage/mod.rs`: 将最小 Pager、快照、写批次和目录类型开放至 crate 内。
- Modify `src/storage/pager.rs`: 提供公共层需要的配置、UUID、目录快照和事务桥接。
- Modify `src/storage/file.rs`: 将 `OpenMode` 开放至 crate 内。
- Modify `src/storage/catalog/mod.rs`: 提供快照/写批次目录读取入口及绑定查询。
- Modify `src/storage/catalog/types.rs`: 保持 ID/定义只读访问器，增加显式类型映射所需可见性。
- Create `src/api/mod.rs`: 公共 API 导出。
- Create `src/api/options.rs`: `Options`、`AccessMode` 和 Pager/SQL 限制映射。
- Create `src/api/database.rs`: create/open/connect、UUID 生成和 Pager 所有权。
- Create `src/api/connection.rs`: 会话句柄和短期快照准备。
- Create `src/api/transaction.rs`: 只读/写事务 RAII 包装。
- Create `src/api/metadata.rs`: 公共 `DataType`、参数和结果列元数据。
- Create `src/api/statement.rs`: 不可变公共 `Statement` 和模式验证桥接。
- Create `tests/common/mod.rs`: 公共 API 集成测试使用的唯一临时路径管理器。
- Create `tests/public_api.rs`: 只使用 crate 公共接口的生命周期、错误和只读证明。
- Modify `src/storage/test_support.rs`: 增加供内部 Phase 4B 目录绑定使用的确定性真实目录 fixture。
- Modify `.github/workflows/ci.yml`: 增加 Phase 4B 专用语料库/MSRV 门禁。
- Modify `README.md`: 记录真实可用 API 和仍未实现的执行边界。

---

### Task 1: Pin dependencies and extend the stable error model

**Files:**
- Modify: `Cargo.toml`
- Modify: `.gitignore`
- Modify: `src/error.rs`

**Interfaces:**
- Produces: `ErrorCode::SchemaChanged = 1150`.
- Produces: `SqlLocation::new(byte_offset, line, column) -> Result<SqlLocation>`.
- Produces: `DbError::with_sql_location(self, SqlLocation) -> Self` and `DbError::sql_location(&self) -> Option<SqlLocation>`.
- Adds: exact `sqlparser` and `getrandom` dependency declarations from the spec.

- [ ] **Step 1: Write failing error-contract tests**

Add to `src/error.rs`:

```rust
#[test]
fn schema_changed_and_sql_locations_are_stable() {
    assert_eq!(ErrorCode::SchemaChanged.as_u32(), 1150);
    let location = SqlLocation::new(9, 2, 4).unwrap();
    let error = DbError::new(ErrorCode::Syntax, "bad token")
        .with_sql_location(location);
    assert_eq!(error.sql_location(), Some(location));
    assert_eq!(location.byte_offset(), 9);
    assert_eq!(location.line(), 2);
    assert_eq!(location.column(), 4);
}

#[test]
fn sql_locations_reject_zero_line_or_column() {
    assert_eq!(SqlLocation::new(0, 0, 1).unwrap_err().code(), ErrorCode::Internal);
    assert_eq!(SqlLocation::new(0, 1, 0).unwrap_err().code(), ErrorCode::Internal);
}
```

- [ ] **Step 2: Run the test and verify it fails before dependency edits**

Run: `cargo test error::tests::schema_changed_and_sql_locations_are_stable -- --exact`

Expected: compile failure because `SchemaChanged` and `SqlLocation` do not exist.

- [ ] **Step 3: Implement the error fields and pin dependencies**

Add `SchemaChanged = 1150`; define a `Copy + Eq` location with checked one-based line/column; add `sql_location: Option<SqlLocation>` to `DbError`. Preserve the existing Display text and Oracle code behavior.

Add exactly:

```toml
sqlparser = { version = "=0.62.0", default-features = false, features = ["std"] }
getrandom = { version = "=0.4.3", default-features = false, features = ["std"] }
```

Add `/Cargo.lock` to `.gitignore`; do not stage any generated lockfile.

- [ ] **Step 4: Verify stable and MSRV builds**

Run:

```text
cargo test error::tests -- --nocapture
rustup run 1.85.0 cargo check --all-targets
cargo fmt --check
```

Expected: error tests pass, both dependencies compile on Rust 1.85, and formatting passes.

- [ ] **Step 5: Commit**

```text
git add Cargo.toml .gitignore src/error.rs
git commit -m "feat: establish phase 4b error and dependency contracts"
```

---

### Task 2: Define SQL spans, limits, types, names, and parameters

**Files:**
- Create: `src/sql/mod.rs`
- Create: `src/sql/types.rs`
- Create: `src/sql/limits.rs`
- Create: `src/sql/name.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: public `SqlLimits` with exact spec defaults and immutable getters/builder methods.
- Produces: `SqlSpan { start: usize, end: usize }`, using an exclusive end.
- Produces: `SqlType::{Null, Unknown, Int64, Decimal, Float64, Text, Blob, Date, Timestamp, Bool}`.
- Produces: `Identifier { normalized: String, quoted: bool, span: SqlSpan }`.
- Produces: `ParameterKey::{Position(NonZeroU16), Name(String)}` and `ParameterStyle`.

- [ ] **Step 1: Write failing primitive-boundary tests**

```rust
#[test]
fn sql_limits_have_the_approved_defaults() {
    let limits = SqlLimits::default();
    assert_eq!(limits.max_sql_bytes(), 1_048_576);
    assert_eq!(limits.max_tokens(), 100_000);
    assert_eq!(limits.max_ast_nodes(), 100_000);
    assert_eq!(limits.max_nesting(), 128);
    assert_eq!(limits.max_parameters(), 65_535);
    assert_eq!(limits.max_joins(), 64);
}

#[test]
fn oracle_identifier_normalization_is_byte_bounded() {
    fn span() -> SqlSpan { SqlSpan::new(0, 2).unwrap() }
    assert_eq!(Identifier::unquoted("users", span()).unwrap().as_str(), "USERS");
    assert_eq!(Identifier::quoted("Users", span()).unwrap().as_str(), "Users");
    assert!(Identifier::quoted("界".repeat(42), span()).is_ok());
    assert_eq!(Identifier::quoted("界".repeat(43), span()).unwrap_err().code(), ErrorCode::Bind);
}

#[test]
fn positional_parameter_keys_reject_zero_leading_zero_and_overflow() {
    fn span() -> SqlSpan { SqlSpan::new(0, 2).unwrap() }
    assert!(ParameterKey::parse(":1", span()).is_ok());
    for invalid in [":0", ":01", ":65536"] {
        assert_eq!(ParameterKey::parse(invalid, span()).unwrap_err().code(), ErrorCode::Bind);
    }
}
```

- [ ] **Step 2: Run tests and verify missing-module failures**

Run: `cargo test sql:: -- --nocapture`

Expected: compile failure because `sql`, `SqlLimits`, names and parameter keys are absent.

- [ ] **Step 3: Implement checked primitives**

Use checked span construction (`start <= end`), positive configuration validation and ASCII-only uppercasing for unquoted names. Parameter names use the same unquoted normalization; reject quoted names. `SqlLimits` builder methods return `Result<Self>` when the supplied value is zero or exceeds `u32`/`u16` implementation hard bounds.

- [ ] **Step 4: Verify boundaries**

Run: `cargo test sql::limits sql::name sql::types -- --nocapture`

Expected: default, zero, exact boundary, UTF-8 byte length and parameter tests pass.

- [ ] **Step 5: Commit**

```text
git add src/lib.rs src/sql
git commit -m "feat(sql): define checked frontend primitives"
```

---

### Task 3: Define the complete owned AST

**Files:**
- Create: `src/sql/ast/mod.rs`
- Create: `src/sql/ast/expr.rs`
- Create: `src/sql/ast/query.rs`
- Create: `src/sql/ast/statement.rs`
- Modify: `src/sql/mod.rs`

**Interfaces:**
- Produces: `AstStatement` for query, DDL, DML and transaction statements.
- Produces: `AstExpr` with only the expression variants approved by the spec.
- Produces: `AstQuery`, `SelectItem`, `TableSource`, `Join`, `OrderItem`, `CreateTable`, `CreateIndex`, `Constraint`.
- Every user-authored node carries an exclusive `SqlSpan`; all strings and vectors are owned.

- [ ] **Step 1: Write AST construction and ownership tests**

```rust
#[test]
fn ast_is_owned_and_preserves_spans() {
    let mut source = String::from("users");
    let table = Identifier::unquoted(&source, SqlSpan::new(14, 19).unwrap()).unwrap();
    source.clear();
    let statement = AstStatement::Delete(Delete {
        table,
        alias: None,
        selection: None,
        span: SqlSpan::new(0, 19).unwrap(),
    });
    assert_eq!(statement.span(), SqlSpan::new(0, 19).unwrap());
    assert_eq!(statement.delete().unwrap().table.as_str(), "USERS");
}
```

Add one compile-time helper asserting `AstStatement: Send + Sync + 'static`.

- [ ] **Step 2: Run and verify the AST types are missing**

Run: `cargo test sql::ast::tests::ast_is_owned_and_preserves_spans -- --exact`

Expected: compile failure for missing AST modules/types.

- [ ] **Step 3: Implement explicit AST enums**

Use these top-level shapes; do not include catch-all or third-party variants:

```rust
pub(crate) enum AstStatement {
    Query(AstQuery),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    CreateTable(CreateTable),
    DropTable(DropObject),
    CreateIndex(CreateIndex),
    DropIndex(DropObject),
    Begin(SqlSpan),
    Commit(SqlSpan),
    Rollback(SqlSpan),
}

pub(crate) enum AstExprKind {
    Null,
    Bool(bool),
    Integer(String),
    Decimal(String),
    Float(String),
    Text(String),
    Blob(Vec<u8>),
    Column(Vec<Identifier>),
    Parameter(ParameterKey),
    Unary { op: UnaryOp, expr: Box<AstExpr> },
    Binary { left: Box<AstExpr>, op: BinaryOp, right: Box<AstExpr> },
    IsNull { expr: Box<AstExpr>, negated: bool },
    InList { expr: Box<AstExpr>, values: Vec<AstExpr>, negated: bool },
    Between { expr: Box<AstExpr>, low: Box<AstExpr>, high: Box<AstExpr>, negated: bool },
    Like { expr: Box<AstExpr>, pattern: Box<AstExpr>, negated: bool },
    Aggregate { function: AggregateFunction, argument: AggregateArgument },
}
```

Represent INNER/LEFT joins only, ORDER direction/null placement explicitly, and constraints as primary key, unique, not null, check and immediate foreign key declarations.

- [ ] **Step 4: Verify AST invariants**

Run: `cargo test sql::ast -- --nocapture`

Expected: ownership, spans, explicit enum and Send/Sync tests pass.

- [ ] **Step 5: Commit**

```text
git add src/sql
git commit -m "feat(sql): define owned phase 4b AST"
```

---

### Task 4: Build source mapping, token limits, and Oracle bind rewriting

**Files:**
- Create: `src/sql/parser/mod.rs`
- Create: `src/sql/parser/source.rs`
- Create: `src/sql/parser/tokens.rs`
- Modify: `src/sql/mod.rs`

**Interfaces:**
- Produces: `SourceMap::location(byte_offset) -> Result<SqlLocation>` and third-party span to `SqlSpan` conversion.
- Produces: `tokens::tokenize(sql, limits) -> Result<Vec<TokenWithSpan>>`; this type remains private to `sql::parser`.
- Produces: placeholder tokens for adjacent `:1` and `:name` without changing source positions.

- [ ] **Step 1: Write failing source and token tests**

```rust
#[test]
fn source_map_treats_crlf_as_one_line_break() {
    let map = SourceMap::new("一\r\n:x").unwrap();
    assert_eq!(map.location(5).unwrap(), SqlLocation::new(5, 2, 1).unwrap());
}

#[test]
fn bind_rewrite_ignores_literals_comments_and_quoted_names() {
    let sql = "SELECT ':1', \"A:B\" FROM T -- :2\n WHERE ID = :1";
    let tokens = tokenize(sql, &SqlLimits::default()).unwrap();
    assert_eq!(placeholder_texts(&tokens), vec![":1"]);
}

#[test]
fn token_limit_fails_before_ast_conversion() {
    let limits = SqlLimits::default().with_max_tokens(3).unwrap();
    assert_eq!(tokenize("SELECT A FROM T", &limits).unwrap_err().code(), ErrorCode::Syntax);
}
```

- [ ] **Step 2: Run and verify the parser helpers are absent**

Run: `cargo test sql::parser::tokens sql::parser::source -- --nocapture`

Expected: compile failure for missing parser modules.

- [ ] **Step 3: Implement mapping and token rewrite**

Build line-start byte offsets in one checked pass. Tokenize with `OracleDialect`, discard whitespace for limit counting but retain spans for parsing. Rewrite only source-adjacent colon plus decimal/word sequences; use `Parser::with_tokens` later. Reject invalid bind forms through `ParameterKey::parse`, and reject mixed styles or positional gaps after the full token pass.

- [ ] **Step 4: Verify lexical boundaries**

Run: `cargo test sql::parser::tokens sql::parser::source -- --nocapture`

Expected: CRLF/LF, Unicode scalar columns, comments, literals, adjacency, mixed styles, gaps and exact token limits pass.

- [ ] **Step 5: Commit**

```text
git add src/sql
git commit -m "feat(sql): adapt Oracle bind tokens safely"
```

---

### Task 5: Convert supported literals and expressions

**Files:**
- Create: `src/sql/parser/expr.rs`
- Modify: `src/sql/parser/mod.rs`
- Modify: `src/sql/ast/expr.rs`

**Interfaces:**
- Produces: `convert_expr(sqlparser::ast::Expr, &SourceMap, &mut AstBudget) -> Result<AstExpr>` private to parser.
- Produces: checked integer/decimal/float text and `X'ABCD'` blob decoding.
- Maps every unsupported expression variant to project `Unsupported` with a project `SqlLocation`.

- [ ] **Step 1: Write failing expression-corpus tests**

```rust
#[test]
fn supported_expressions_convert_to_owned_variants() {
    for expr in [
        "A + :1",
        "A BETWEEN 1 AND 3",
        "A IN (1, 2)",
        "A IS NOT NULL",
        "NAME LIKE 'A%'",
        "COUNT(*)",
        "X'00FF'",
    ] {
        assert!(parse_expr_fragment(expr, &SqlLimits::default()).is_ok(), "{expr}");
    }
}

#[test]
fn unsupported_expression_families_are_classified() {
    for expr in ["CASE WHEN A=1 THEN 2 END", "CAST(A AS INT)"] {
        assert_eq!(parse_expr_fragment(expr, &SqlLimits::default()).unwrap_err().code(), ErrorCode::Unsupported);
    }
}
```

Define `parse_expr_fragment` under `#[cfg(test)]` in `parser/expr.rs`; it tokenizes `SELECT <expr> FROM T`, applies the configured recursion limit, extracts the single projection expression and calls `convert_expr`. It is not a production API and is removed from non-test builds.

- [ ] **Step 2: Run and verify expression conversion fails**

Run: `cargo test sql::parser::expr -- --nocapture`

Expected: supported cases fail because no converter exists.

- [ ] **Step 3: Implement the allow-listed conversion**

Use an `AstBudget` that decrements node and depth budgets before allocating children and restores only depth on return. Parse blob hex two characters at a time; reject odd digits and non-hex as `Syntax`. Preserve literal text for the later checked numeric constructors.

- [ ] **Step 4: Verify supported, unsupported and every-truncation cases**

Run: `cargo test sql::parser::expr -- --nocapture`

Expected: all expression cases return the exact project AST or stable error category without panic.

- [ ] **Step 5: Commit**

```text
git add src/sql/parser src/sql/ast
git commit -m "feat(sql): convert allow-listed expressions"
```

---

### Task 6: Convert SELECT, joins, grouping, and ordering

**Files:**
- Create: `src/sql/parser/query.rs`
- Modify: `src/sql/parser/mod.rs`
- Modify: `src/sql/ast/query.rs`

**Interfaces:**
- Produces: `convert_query(Query, source, budget, limits) -> Result<AstQuery>`.
- Accepts: explicit projection, DISTINCT, one base table, INNER/LEFT JOIN ON, WHERE, GROUP BY, HAVING and ORDER BY.
- Rejects: subqueries, CTE, set operations, unsupported joins, LIMIT/OFFSET/FETCH/ROWNUM and SELECT FOR UPDATE.

- [ ] **Step 1: Write failing query-shape tests**

```rust
#[test]
fn query_surface_converts_without_dependency_types() {
    let query = parse_query(
        "SELECT DISTINCT U.ID AS USER_ID, COUNT(O.ID) AS N \
         FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID \
         WHERE U.ACTIVE = TRUE GROUP BY U.ID HAVING COUNT(O.ID) > 0 \
         ORDER BY USER_ID DESC NULLS FIRST"
    ).unwrap();
    assert!(query.distinct);
    assert_eq!(query.joins.len(), 1);
    assert_eq!(query.group_by.len(), 1);
    assert_eq!(query.order_by.len(), 1);
}
```

Define `parse_query(sql) -> Result<AstQuery>` under `#[cfg(test)]` in `parser/query.rs`; it tokenizes and parses exactly one SELECT with the production limits, calls `convert_query`, and rejects any non-query statement. It must not be compiled into the production API.

Add a table of every rejected query family and assert `Unsupported`.

- [ ] **Step 2: Run and verify query conversion fails**

Run: `cargo test sql::parser::query -- --nocapture`

Expected: missing converter or unsupported query result.

- [ ] **Step 3: Implement query conversion and JOIN limit**

Accept aliases without `AS` for tables and with/without `AS` for select items. Require `ON` for every join. Count joins per query block before pushing. Convert ORDER BY positive integer literals as expressions; binder decides whether they are ordinals.

- [ ] **Step 4: Verify exact query surface**

Run: `cargo test sql::parser::query -- --nocapture`

Expected: the full accepted query passes and every excluded family returns `Unsupported` with a location.

- [ ] **Step 5: Commit**

```text
git add src/sql/parser src/sql/ast
git commit -m "feat(sql): convert relational query syntax"
```

---

### Task 7: Convert DDL, DML, transactions, and enforce one statement

**Files:**
- Create: `src/sql/parser/statement.rs`
- Modify: `src/sql/parser/mod.rs`
- Modify: `src/sql/ast/statement.rs`

**Interfaces:**
- Produces: `parse_statement(sql: &str, limits: &SqlLimits) -> Result<AstStatement>`.
- Accepts: the exact Phase 4B statement and constraint surface.
- Ensures: empty input, repeated trailing semicolons and multiple statements are `Syntax`.

- [ ] **Step 1: Write failing statement-corpus tests**

```rust
#[test]
fn ddl_dml_and_transactions_convert() {
    for sql in [
        "CREATE TABLE T (ID INT64 PRIMARY KEY, NAME TEXT NOT NULL)",
        "CREATE UNIQUE INDEX T_U1 ON T (ID)",
        "DROP INDEX T_U1",
        "INSERT INTO T (ID, NAME) VALUES (:1, :2)",
        "UPDATE T SET NAME = :name WHERE ID = :id",
        "DELETE FROM T WHERE ID = :1",
        "BEGIN", "COMMIT", "ROLLBACK",
    ] {
        assert!(parse_statement(sql, &SqlLimits::default()).is_ok(), "{sql}");
    }
}

#[test]
fn exactly_one_statement_is_required() {
    for sql in ["", ";", "SELECT A FROM T;;", "SELECT A FROM T; DELETE FROM T"] {
        assert_eq!(parse_statement(sql, &SqlLimits::default()).unwrap_err().code(), ErrorCode::Syntax);
    }
}
```

- [ ] **Step 2: Run and verify the top-level parser is incomplete**

Run: `cargo test sql::parser::statement -- --nocapture`

Expected: DDL/DML conversion tests fail.

- [ ] **Step 3: Implement exhaustive statement conversion**

Map table/column constraints explicitly. Allow one optional trailing semicolon only. Reject IF EXISTS, OR REPLACE, ALTER, MERGE, TRUNCATE, RETURNING, multi-row INSERT, INSERT SELECT and UPDATE FROM as `Unsupported`. INSERT accepts one VALUES row; UPDATE/DELETE accept one unqualified target table plus optional alias.

- [ ] **Step 4: Verify parser isolation**

Run:

```text
cargo test sql::parser -- --nocapture
rg -n "sqlparser::|use sqlparser" src --glob "!sql/parser/**"
```

Expected: parser tests pass; the search returns no production references outside `src/sql/parser/`.

- [ ] **Step 5: Commit**

```text
git add src/sql/parser src/sql/ast
git commit -m "feat(sql): parse phase 4b statements"
```

---

### Task 8: Define bound IR and public metadata

**Files:**
- Create: `src/sql/bound/mod.rs`
- Create: `src/sql/bound/expr.rs`
- Create: `src/sql/bound/statement.rs`
- Create: `src/api/mod.rs`
- Create: `src/api/metadata.rs`
- Create: `src/api/statement.rs`
- Modify: `src/sql/mod.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces public: `DataType`, `ParameterIdentifier`, `ParameterMetadata`, `ColumnMetadata`, `Statement`.
- Produces internal: `BoundStatement`, `BoundStatementKind`, `BoundExpr`, `SchemaDependency`, `CreatedNameDependency`.
- `BoundStatement` contains no parser, storage page or WAL types.

- [ ] **Step 1: Write failing IR and metadata tests**

```rust
#[test]
fn statement_exposes_owned_stable_metadata() {
    let bound = fixture_bound_select();
    let statement = Statement::from_bound(bound);
    assert_eq!(statement.parameters()[0].identifier(), &ParameterIdentifier::Position(1));
    assert_eq!(statement.parameters()[0].data_type(), Some(DataType::Int64));
    assert_eq!(statement.columns()[0].name(), "ID");
    assert_eq!(statement.columns()[0].data_type(), DataType::Int64);
}

#[test]
fn bound_types_are_owned_send_and_sync() {
    fn assert_owned<T: Send + Sync + 'static>() {}
    assert_owned::<BoundStatement>();
    assert_owned::<Statement>();
}
```

Define `fixture_bound_select` in the same test module from checked `TableId(1)`, `ColumnId(2)`, `SchemaVersion(1)`, database UUID `[7; 16]`, one Int64 positional parameter and one non-null Int64 output column named `ID`. Do not add a production test constructor.

- [ ] **Step 2: Run and verify bound/public types are absent**

Run: `cargo test sql::bound api::metadata api::statement -- --nocapture`

Expected: compile failure for missing modules.

- [ ] **Step 3: Implement exact IR contracts**

Use these dependency shapes:

```rust
pub(crate) struct SchemaDependency {
    pub database_uuid: [u8; 16],
    pub table_id: TableId,
    pub schema_version: SchemaVersion,
}

pub(crate) struct CreatedNameDependency {
    pub object_kind: ObjectKind,
    pub name: CatalogName,
}

pub(crate) struct BoundExpr {
    pub kind: BoundExprKind,
    pub sql_type: SqlType,
    pub nullable: bool,
    pub span: SqlSpan,
}
```

`Statement` stores an `Arc<BoundStatement>` and cloned public metadata slices. Expose only `parameters()` and `columns()` publicly; keep bound access and schema validation crate-private.

- [ ] **Step 4: Verify no forbidden type leakage**

Run: `cargo test sql::bound api::metadata api::statement -- --nocapture`

Then run: `rg -n "PageRef|BTree|Wal|sqlparser" src/sql/bound src/api`

Expected: tests pass; search finds no physical storage or parser dependency in public/bound types.

- [ ] **Step 5: Commit**

```text
git add src/lib.rs src/sql src/api
git commit -m "feat(sql): define stable bound statement contract"
```

---

### Task 9: Expose a narrow storage and catalog bridge

**Files:**
- Modify: `src/storage/mod.rs`
- Modify: `src/storage/pager.rs`
- Modify: `src/storage/file.rs`
- Modify: `src/storage/catalog/mod.rs`
- Modify: `src/storage/catalog/types.rs`
- Modify: `src/storage/test_support.rs`

**Interfaces:**
- Produces: crate-visible `Pager`, `PagerConfig`, `ReadSnapshot`, `WriteBatch<'_>`, `OpenMode`.
- Produces: `Pager::database_uuid() -> Result<[u8; 16]>`.
- Produces: `ReadSnapshot::catalog_reader(&mut self) -> Result<CatalogReader>`.
- Produces: `WriteBatch::catalog_reader(&mut self) -> Result<CatalogReader>` for its unchanged Phase 4B catalog view.
- Produces under `#[cfg(test)]`: `Phase4bCatalogFixture` and `phase_4b_catalog_fixture(page_size) -> Phase4bCatalogFixture`, containing USERS(ID Int64 non-null, NAME Text nullable, AMOUNT Decimal nullable, FLOAT_COL Float64 nullable) and ORDERS(ID Int64 non-null, USER_ID Int64 non-null. The fixture exposes `pager()`, `main_path()`, `wal_path()`, `users_id()` and `orders_id()`.
- Makes crate-visible: `begin_read`, `begin_write`, `commit`, `rollback` and existing immutable catalog lookups only.

- [ ] **Step 1: Write failing bridge tests**

```rust
#[test]
fn phase_4b_bridge_reads_uuid_and_real_catalog_from_both_transaction_kinds() {
    let fixture = phase_4b_catalog_fixture(4096);
    let pager = fixture.pager();
    assert_ne!(pager.database_uuid().unwrap(), [0; 16]);

    let mut read = pager.begin_read().unwrap();
    assert!(read.catalog_reader().unwrap().find_table_by_id(fixture.users_id()).is_some());
    drop(read);

    let mut write = pager.begin_write().unwrap();
    assert!(write.catalog_reader().unwrap().find_table_by_id(fixture.users_id()).is_some());
    write.rollback().unwrap();
}
```

- [ ] **Step 2: Run and verify visibility/bridge failures**

Run: `cargo test storage::test_support::phase_4b_bridge_reads_uuid_and_real_catalog_from_both_transaction_kinds -- --exact`

Expected: compile failure because the bridge methods do not exist.

- [ ] **Step 3: Implement minimal visibility changes**

Do not make page, B+Tree, WAL, header or mutation internals public. `WriteBatch::catalog_reader` must open the current feature/root image through existing validation and must reject a pending catalog writer. Build the named fixture only with existing `CatalogWriter`, checked IDs and real table B+Tree roots; make `TempDatabase` and the fixture crate-visible only under `cfg(test)`. Current V1 catalogs contain no persisted indexes; index-name lookup therefore returns no match until Phase 4C defines those records, while CREATE INDEX can still bind its table and columns.

- [ ] **Step 4: Run storage regression around the bridge**

Run:

```text
cargo test storage::test_support::phase_4b_bridge_reads_uuid_and_real_catalog_from_both_transaction_kinds -- --exact
cargo test storage::catalog::tests -- --nocapture
cargo test storage::pager::tests::old_snapshot_keeps_old_page_after_new_commit -- --exact
```

Expected: new bridge and existing catalog/snapshot behavior pass.

- [ ] **Step 5: Commit**

```text
git add src/storage
git commit -m "feat(storage): expose phase 4b catalog bridge"
```

---

### Task 10: Implement Options, database creation, opening, and UUID generation

**Files:**
- Create: `src/api/options.rs`
- Create: `src/api/database.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/lib.rs`
- Modify: `src/storage/pager.rs`
- Create: `tests/common/mod.rs`
- Create: `tests/public_api.rs`

**Interfaces:**
- Produces public: `AccessMode::{ReadWrite, ReadOnly}` and `Options` builder/getters.
- Produces public: `Database::{create, open}`; Task 11 adds `connect`.
- Produces internal: `UuidSource`, `OsUuidSource`, `generate_database_uuid` and injectable create path for tests.
- `Database` is `Clone + Send + Sync` and owns `Pager` plus `SqlLimits`.

- [ ] **Step 1: Write failing public create/open and UUID tests**

```rust
#[test]
fn create_is_exclusive_and_open_never_creates() {
    let paths = TempPaths::new("public-create-open");
    let db = Database::create(paths.main(), Options::default()).unwrap();
    drop(db);
    assert_eq!(Database::create(paths.main(), Options::default()).unwrap_err().code(), ErrorCode::Io);
    assert!(Database::open(paths.missing(), Options::default()).is_err());
    assert!(!paths.missing().exists());
}

#[test]
fn uuid_source_failure_happens_before_any_file_creation() {
    let paths = TempPaths::new("uuid-failure");
    let error = Database::create_with_uuid_source(paths.main(), Options::default(), &FailUuid)
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Io);
    assert!(!paths.main().exists());
    assert!(!paths.wal().exists());
}

#[test]
fn generated_uuid_has_v4_and_rfc4122_bits() {
    let uuid = generate_database_uuid(&FixedUuid([0xff; 16])).unwrap();
    assert_eq!(uuid[6] >> 4, 4);
    assert_eq!(uuid[8] >> 6, 2);
}
```

Define `tests/common/mod.rs::TempPaths` in this task. It creates one collision-resistant directory below `std::env::temp_dir()` from process ID plus an atomic counter, exposes `main()`, `wal()` and `missing()`, and Drop removes only those exact files and then the exact empty directory. It never recursively deletes a caller-supplied path. Define `FailUuid` and `FixedUuid([u8; 16])` in the private `api::database` test module because the injectable source is crate-private.

- [ ] **Step 2: Run and verify public API is missing**

Run: `cargo test --test public_api create_is_exclusive_and_open_never_creates -- --exact`

Expected: compile failure because `Database` and `Options` are absent.

- [ ] **Step 3: Implement options and database lifecycle**

`Options::default()` uses read-write, 4096-byte create page size, 1024 cache pages, 10 GiB database, 64 MiB checkpoint threshold, 1 GiB WAL and default `SqlLimits`. Opening ignores the create-only page size and reads the disk page size. `generate_database_uuid` calls the injected source, then applies:

```rust
uuid[6] = (uuid[6] & 0x0f) | 0x40;
uuid[8] = (uuid[8] & 0x3f) | 0x80;
```

Map getrandom failures to `Io` without exposing platform-specific sensitive details. Do not bootstrap or mutate the logical catalog during create.

- [ ] **Step 4: Verify lifecycle and MSRV**

Run:

```text
cargo test --test public_api create -- --nocapture
cargo test api::database -- --nocapture
rustup run 1.85.0 cargo check --all-targets
```

Expected: create/open/UUID/read-only configuration tests pass on Rust 1.85.

- [ ] **Step 5: Commit**

```text
git add src/api src/lib.rs src/storage/pager.rs tests/common/mod.rs tests/public_api.rs
git commit -m "feat(api): create and open embedded databases"
```

---

### Task 11: Implement connections and RAII transactions

**Files:**
- Create: `src/api/connection.rs`
- Create: `src/api/transaction.rs`
- Modify: `src/api/database.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/lib.rs`
- Modify: `tests/public_api.rs`

**Interfaces:**
- Produces: `Database::connect(&self) -> Result<Connection>`.
- Produces: `Connection::begin_read(&mut self) -> Result<ReadTransaction<'_>>`.
- Produces: `Connection::begin_write(&mut self) -> Result<WriteTransaction<'_>>`.
- Produces: `WriteTransaction::{commit(self), rollback(self)}`; Drop rolls back an unfinished batch.
- Holds: `ReadTransaction` owns `ReadSnapshot`; `WriteTransaction` owns `Option<WriteBatch<'a>>`.

- [ ] **Step 1: Write failing transaction-lifecycle tests**

```rust
#[test]
fn unfinished_write_transaction_rolls_back_and_read_only_rejects_writes() {
    let paths = TempPaths::new("transaction-raii");
    let db = Database::create(paths.main(), Options::default()).unwrap();
    let before = std::fs::read(paths.main()).unwrap();
    {
        let mut conn = db.connect().unwrap();
        let _write = conn.begin_write().unwrap();
    }
    assert_eq!(std::fs::read(paths.main()).unwrap(), before);

    drop(db);
    let read_only = Database::open(paths.main(), Options::default().read_only()).unwrap();
    let mut conn = read_only.connect().unwrap();
    assert_eq!(conn.begin_write().unwrap_err().code(), ErrorCode::Unsupported);
}
```

Add a `compile_fail` doc test showing that a second transaction cannot start while the first borrows the same `Connection`.

- [ ] **Step 2: Run and verify transaction types are missing**

Run: `cargo test --test public_api transaction -- --nocapture`

Expected: compile failure for missing connection/transaction methods.

- [ ] **Step 3: Implement thin RAII wrappers**

Do not add an independent active-transaction flag; use `&mut Connection` borrowing and Pager writer ownership. Give `Connection` a private `PhantomData<Cell<()>>`, which keeps it `Send` but makes it `!Sync` for future session state. `WriteTransaction::commit` takes the batch from `Option` and consumes it. Drop calls rollback only when the option remains and never panics.

- [ ] **Step 4: Verify transactions and compile-fail documentation**

Run:

```text
cargo test --test public_api transaction -- --nocapture
cargo test --doc
cargo test storage::test_support::unfinished_writer_is_busy_readers_progress_and_checkpoint_waits_for_rollback -- --exact
```

Expected: RAII, read-only, borrow and existing single-writer behavior pass.

- [ ] **Step 5: Commit**

```text
git add src/api src/lib.rs tests/public_api.rs
git commit -m "feat(api): expose snapshot and write transactions"
```

---

### Task 12: Resolve table scopes, aliases, columns, and stars

**Files:**
- Create: `src/sql/bind/mod.rs`
- Create: `src/sql/bind/scope.rs`
- Modify: `src/sql/mod.rs`
- Modify: `src/storage/catalog/mod.rs`

**Interfaces:**
- Produces: `BindingScope::from_query_sources(&CatalogReader, &AstQuery) -> Result<BindingScope>`.
- Produces: `resolve_column(&self, &[Identifier]) -> Result<ResolvedColumn>`.
- Produces: `expand_star(&self, Option<&Identifier>) -> Result<Vec<ResolvedColumn>>`.
- `ResolvedColumn` contains `TableId`, `SchemaVersion`, `ColumnId`, ordinal, type, nullable and normalized display name.

- [ ] **Step 1: Write failing real-catalog scope tests**

```rust
#[test]
fn aliases_hide_table_names_and_unqualified_columns_must_be_unique() {
    let fixture = real_catalog_with_users_and_orders();
    let scope = BindingScope::from_sources(
        fixture.reader(),
        &[source("USERS", Some("U")), source("ORDERS", Some("O"))],
        64,
    ).unwrap();
    assert!(scope.resolve_column(&idents(&["U", "ID"])).is_ok());
    assert_eq!(scope.resolve_column(&idents(&["USERS", "ID"])).unwrap_err().code(), ErrorCode::Bind);
    assert_eq!(scope.resolve_column(&idents(&["ID"])).unwrap_err().code(), ErrorCode::Bind);
}

#[test]
fn star_expansion_is_from_order_then_catalog_ordinal() {
    let scope = fixture_scope();
    let names = scope.expand_star(None).unwrap().into_iter().map(|c| c.name).collect::<Vec<_>>();
    assert_eq!(names, ["ID", "NAME", "ID", "USER_ID"]);
}
```

Define local `source(name, alias)` and `idents(parts)` helpers in this test module; both construct checked unquoted `Identifier` values with a fixed valid test span. `real_catalog_with_users_and_orders` and `fixture_scope` must call the Task 9 `phase_4b_catalog_fixture`, whose exact schema is USERS(ID Int64 non-null, NAME Text nullable, AMOUNT Decimal nullable, FLOAT_COL Float64 nullable) and ORDERS(ID Int64 non-null, USER_ID Int64 non-null).

- [ ] **Step 2: Run and verify scope resolution is absent**

Run: `cargo test sql::bind::scope -- --nocapture`

Expected: compile failure for missing binding scope.

- [ ] **Step 3: Implement exact scope rules**

Bind FROM items in order, reject duplicate aliases, make aliases hide base names, and reject more than two column-name parts. Use `CatalogName` only after the 128-byte SQL name check. Preserve FROM/ordinal order without hash-map iteration dependence.

- [ ] **Step 4: Verify real-directory scope behavior**

Run: `cargo test sql::bind::scope -- --nocapture`

Expected: unknown, ambiguous, qualified, alias-hidden and star-order tests pass against Phase 4A records.

- [ ] **Step 5: Commit**

```text
git add src/sql/bind src/sql/mod.rs src/storage/catalog/mod.rs
git commit -m "feat(sql): bind catalog scopes and columns"
```

---

### Task 13: Infer expression, aggregate, and parameter types

**Files:**
- Create: `src/sql/bind/types.rs`
- Modify: `src/sql/bind/mod.rs`
- Modify: `src/sql/bound/expr.rs`
- Modify: `src/api/metadata.rs`

**Interfaces:**
- Produces: `bind_expr(expr, scope, expected, context) -> Result<BoundExpr>`.
- Produces: `ParameterCollector::{constrain, finish}` with stable position/name ordering.
- Implements: exact conservative compatibility and aggregate result table from the spec.

- [ ] **Step 1: Write failing type and parameter tests**

```rust
#[test]
fn repeated_parameters_merge_compatible_constraints() {
    let bound = bind_where("ID = :1 AND AMOUNT > :1", numeric_fixture()).unwrap();
    assert_eq!(bound.parameters()[0].data_type(), Some(DataType::Decimal));
}

#[test]
fn incompatible_parameter_contexts_fail_at_prepare_time() {
    let error = bind_where("ID = :x AND NAME = :x", mixed_fixture()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Type);
}

#[test]
fn aggregate_result_types_follow_the_contract() {
    assert_type("COUNT(*)", DataType::Int64, false);
    assert_type("SUM(ID)", DataType::Decimal, true);
    assert_type("AVG(FLOAT_COL)", DataType::Float64, true);
}
```

Define `numeric_fixture` and `mixed_fixture` from the Task 12 real scope. `bind_where(sql, fixture)` parses `SELECT ID FROM USERS WHERE <sql>` and returns its bound WHERE expression plus parameter metadata. `assert_type(expr, expected, nullable)` binds `SELECT <expr> FROM USERS` against that same real scope and compares the first output column. Keep all helpers under `#[cfg(test)]`.

- [ ] **Step 2: Run and verify type binding fails**

Run: `cargo test sql::bind::types -- --nocapture`

Expected: missing type binder/parameter collector.

- [ ] **Step 3: Implement checked literal and type rules**

Parse integer literals to i64 or checked Decimal, decimal literals through existing `Decimal`, exponent literals to finite Float64, and reject invalid/overflow values as `Type`. Implement Int64-to-Decimal as the only numeric promotion across distinct known types. Require Bool conditions, Text LIKE and compatible IN/BETWEEN lists. Record Unknown only when no context supplies a type.

- [ ] **Step 4: Verify the full type matrix**

Run: `cargo test sql::bind::types -- --nocapture`

Expected: every same-type, Null/Unknown, Int64/Decimal, Float64 rejection, aggregate, nullable and parameter merge cell passes.

- [ ] **Step 5: Commit**

```text
git add src/sql/bind src/sql/bound src/api/metadata.rs
git commit -m "feat(sql): infer expression and parameter types"
```

---

### Task 14: Bind queries, grouping, output metadata, and ordering

**Files:**
- Create: `src/sql/bind/query.rs`
- Create: `src/sql/test_support.rs`
- Modify: `src/sql/bind/mod.rs`
- Modify: `src/sql/bound/statement.rs`
- Modify: `src/api/metadata.rs`

**Interfaces:**
- Produces: `bind_query(query, catalog, uuid, parameters) -> Result<BoundQuery>`.
- Produces: ordered output `ColumnMetadata` and deduplicated `SchemaDependency` values.
- Enforces: SELECT aliases only in ORDER BY, grouping structural equivalence and no nested aggregates.

- [ ] **Step 1: Write failing query-binding tests**

```rust
#[test]
fn join_group_alias_and_order_bind_to_stable_ids() {
    let statement = prepare_fixture(
        "SELECT U.ID AS USER_ID, COUNT(O.ID) AS N \
         FROM USERS U LEFT JOIN ORDERS O ON O.USER_ID = U.ID \
         GROUP BY U.ID ORDER BY USER_ID"
    ).unwrap();
    assert_eq!(statement.columns().iter().map(ColumnMetadata::name).collect::<Vec<_>>(), ["USER_ID", "N"]);
    assert_eq!(statement.dependencies().len(), 2);
}

#[test]
fn aliases_do_not_leak_into_where_or_group_by() {
    for sql in [
        "SELECT ID AS X FROM USERS WHERE X = 1",
        "SELECT ID AS X FROM USERS GROUP BY X",
    ] {
        assert_eq!(prepare_fixture(sql).unwrap_err().code(), ErrorCode::Bind);
    }
}
```

Create `src/sql/test_support.rs` under `#[cfg(test)]` in this task. `prepare_fixture(sql) -> Result<BoundStatement>` opens the Task 9 real catalog fixture, calls the production parser and binder, and retains the temporary database until binding has completed. It is the only full-SQL helper used by Tasks 14–16.

- [ ] **Step 2: Run and verify query binder is absent**

Run: `cargo test sql::bind::query -- --nocapture`

Expected: compile failure or unbound query errors.

- [ ] **Step 3: Implement the approved binding order**

Bind FROM/JOIN ON, WHERE, GROUP BY, aggregate/HAVING, projection, DISTINCT, then ORDER BY. Expand stars before assigning output positions. Resolve ORDER BY positive integer as one-based projection ordinal and select alias against the completed projection. Use structural bound-expression equality for GROUP BY membership; reject nested aggregates during context tracking.

- [ ] **Step 4: Verify query semantics**

Run: `cargo test sql::bind::query -- --nocapture`

Expected: joins, aliases, stars, grouping, aggregates, DISTINCT, ordinal/alias ORDER BY and invalid group references pass with deterministic metadata.

- [ ] **Step 5: Commit**

```text
git add src/sql/bind src/sql/bound src/api/metadata.rs
git commit -m "feat(sql): bind relational queries"
```

---

### Task 15: Bind DDL/DML and validate schema dependencies

**Files:**
- Create: `src/sql/bind/statement.rs`
- Modify: `src/sql/bind/mod.rs`
- Modify: `src/sql/bound/statement.rs`
- Modify: `src/api/statement.rs`
- Modify: `src/storage/catalog/mod.rs`

**Interfaces:**
- Produces: `bind_statement(ast, catalog, database_uuid) -> Result<BoundStatement>`.
- Produces: `BoundStatement::validate_schema(database_uuid, &CatalogReader) -> Result<()>`.
- Binds: INSERT/UPDATE/DELETE targets, core DDL types, table/column constraints, CREATE INDEX columns and DROP object identity where the catalog kind exists.

- [ ] **Step 1: Write failing DDL/DML and invalidation tests**

```rust
#[test]
fn dml_targets_use_catalog_ordinals_and_stable_ids() {
    let insert = prepare_fixture("INSERT INTO USERS (NAME, ID) VALUES (:name, :id)").unwrap();
    assert_eq!(insert.target_ordinals(), [1, 0]);
    assert_eq!(insert.parameters()[0].identifier(), &ParameterIdentifier::Name("NAME".into()));
}

#[test]
fn recreate_same_name_does_not_satisfy_old_statement() {
    let (statement, database_uuid, old_catalog) = prepared_users_select();
    assert!(statement.validate_schema(database_uuid, &old_catalog).is_ok());
    let new_catalog = drop_and_recreate_users();
    assert_eq!(statement.validate_schema(database_uuid, &new_catalog).unwrap_err().code(), ErrorCode::SchemaChanged);
}

#[test]
fn oracle_type_attributes_are_not_silently_discarded() {
    for sql in ["CREATE TABLE T (N NUMBER(10,2))", "CREATE TABLE T (V VARCHAR2(30))"] {
        assert_eq!(prepare_fixture(sql).unwrap_err().code(), ErrorCode::Unsupported);
    }
}
```

`prepared_users_select` prepares `SELECT ID FROM USERS` and returns its statement, UUID and the old real `CatalogReader`. Implement `drop_and_recreate_users` as a `#[cfg(test)]` helper inside `storage::catalog`: use raw catalog B+Tree key operations to remove the old table/column definition and reverse-mapping records, insert a fully cross-checked USERS definition with a different `TableId`, finish the catalog writer gate, and commit. Do not add a production table-drop mutation API in Phase 4B. The helper returns a real reader for the committed replacement; production binding receives only `CatalogReader`.

- [ ] **Step 2: Run and verify statement binding is incomplete**

Run: `cargo test sql::bind::statement -- --nocapture`

Expected: DDL/DML binding and schema validation tests fail.

- [ ] **Step 3: Implement statement binding**

Map only `INT64, DECIMAL, FLOAT64, TEXT, BLOB, DATE, TIMESTAMP, BOOL` to Phase 4A `ColumnType`. Validate new-table column names/ordinals in an isolated scope; bind CHECK expressions to the new row and FK targets to real catalog tables. CREATE INDEX binds its table and existing columns. Current V1 has no persisted index definitions, so DROP INDEX against the real current catalog returns normal unknown-object `Bind`; keep the bound `IndexId` variant ready for Phase 4C once its record family exists rather than storing a late string target.

Schema validation compares database UUID, stable table ID and schema version. CREATE name dependencies must still be absent. An unrelated table change must not invalidate the statement.

- [ ] **Step 4: Verify DDL/DML and schema invalidation**

Run: `cargo test sql::bind::statement -- --nocapture`

Expected: column mapping, target types, constraints, name conflicts, drop/recreate, wrong database, unrelated changes and unsupported Oracle attributes pass.

- [ ] **Step 5: Commit**

```text
git add src/sql/bind src/sql/bound src/api/statement.rs src/storage/catalog/mod.rs
git commit -m "feat(sql): bind statements and schema identities"
```

---

### Task 16: Integrate prepare APIs, harden limits, add CI, and document Phase 4B

**Files:**
- Modify: `src/api/connection.rs`
- Modify: `src/api/transaction.rs`
- Modify: `src/api/statement.rs`
- Modify: `src/sql/mod.rs`
- Modify: `src/sql/limits.rs`
- Modify: `src/sql/test_support.rs`
- Modify: `src/storage/test_support.rs`
- Modify: `tests/public_api.rs`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`

**Interfaces:**
- Produces public: `Connection::prepare`, `ReadTransaction::prepare`, `WriteTransaction::prepare`.
- Exact signatures: `Connection::prepare(&self, &str)`, `ReadTransaction::prepare(&mut self, &str)`, and `WriteTransaction::prepare(&mut self, &str)`, each returning `Result<Statement>`.
- Produces internal: one `prepare_with_catalog` pipeline shared by all three entry points.
- Adds: deterministic accepted/rejected corpus, random UTF-8/no-panic model and read-only byte proof.

- [ ] **Step 1: Write failing end-to-end public tests**

```rust
#[test]
fn prepare_returns_metadata_without_writing_database_or_wal() {
    let fixture = phase_4b_catalog_fixture(4096);
    let before_main = std::fs::read(fixture.main_path()).unwrap();
    let before_wal = std::fs::read(fixture.wal_path()).unwrap();

    let db = Database::open(fixture.main_path(), Options::default()).unwrap();
    let conn = db.connect().unwrap();
    let statement = conn.prepare("SELECT ID, NAME FROM USERS WHERE ID = :1").unwrap();
    assert_eq!(statement.parameters().len(), 1);
    assert_eq!(statement.columns().len(), 2);
    assert_eq!(std::fs::read(fixture.main_path()).unwrap(), before_main);
    assert_eq!(std::fs::read(fixture.wal_path()).unwrap(), before_wal);
}
```

Place this test in `src/sql/test_support.rs`, where it can create the real Phase 4A catalog and then exercise only public `Database`, `Connection` and `Statement` methods. `tests/public_api.rs` separately proves that preparing `CREATE TABLE T (ID INT64)` works on a newly created empty database using only exported types.

Add the same SQL through read and write transaction prepare, then rollback the write transaction and compare bytes.

- [ ] **Step 2: Run and verify prepare entry points are missing**

Run: `cargo test prepare_returns_metadata_without_writing_database_or_wal -- --exact`

Expected: compile failure for missing `prepare`.

- [ ] **Step 3: Implement the shared pipeline**

The shared function must perform exactly:

```text
SQL byte preflight
-> SourceMap/tokenize/bind rewrite/token count
-> sqlparser with recursion limit
-> exactly-one-statement check
-> owned AST conversion with node/depth budgets
-> identifier/parameter normalization
-> catalog binding/type inference
-> Statement construction
```

Connection prepare creates and drops a short read snapshot. Read transaction prepare reuses its snapshot. Write transaction prepare opens a reader from its batch without changing batch state. Preserve parser/bind/type/unsupported classifications and SQL locations.

- [ ] **Step 4: Add corpus, resource and no-panic tests**

In `src/sql/test_support.rs::phase_4b_sql_corpus`, include at least one accepted and rejected case for every spec bullet and bind accepted table/query cases against the real Task 9 catalog. Add `phase_4b_random_sql_never_panics` using a fixed-seed xorshift generator to produce 10,000 bounded byte/UTF-8 inputs; wrap only the test call in `catch_unwind` and fail on panic. For SQL bytes, tokens, nodes, nesting, parameters and JOINs test limit-minus-one, exact limit and limit-plus-one. Do not make production code catch panics.

Run:

```text
cargo test phase_4b_sql_corpus -- --exact --nocapture
cargo test --test public_api -- --nocapture
cargo test phase_4b_random_sql_never_panics -- --exact
```

Expected: corpus, API, byte proof and deterministic no-panic model pass.

- [ ] **Step 5: Add CI and README boundaries**

Add a Linux job named `Linux Phase 4B SQL frontend` with a 30-minute timeout that installs Rust 1.85.0 and runs:

```text
rustup run 1.85.0 cargo test phase_4b_sql_corpus -- --exact --nocapture
rustup run 1.85.0 cargo test phase_4b_random_sql_never_panics -- --exact
```

Update README Status to state that Phase 4B provides real create/open/connect/transaction/prepare APIs, owned SQL parsing, binding and metadata, while execution, CRUD, indexes and constraints remain Phase 4C and Oracle compatibility remains Phase 5.

- [ ] **Step 6: Run release-grade verification**

Run in this order:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
rustup run 1.85.0 cargo test --all-targets
cargo test --profile exhaustive fixed_seed_reference_model_covers_every_page_size_and_tree_kind -- --ignored --nocapture
cargo test storage::test_support::phase_3_crash_recovery_matrix -- --exact --ignored --nocapture
cargo test storage::test_support::phase_4a_catalog_crash_recovery_matrix -- --exact --ignored --nocapture
```

Expected: formatting and Clippy are clean; normal suites report zero failures; dedicated Phase 2/3/4A matrices remain green.

- [ ] **Step 7: Verify repository hygiene and scope**

Run:

```text
git status --short
git diff --check
rg -n "sqlparser::|use sqlparser" src --glob "!sql/parser/**"
rg -n "fn (execute|query)|struct Row|enum PhysicalPlan|trait Executor" src
git ls-files Cargo.lock "*.db" "*.db-wal" "target/**" ".tmp-*"
```

Expected: only intended Phase 4B files are changed; parser dependency does not leak; no execution/row/plan types or generated artifacts are tracked.

- [ ] **Step 8: Commit**

```text
git add src/api src/sql src/storage/test_support.rs tests .github/workflows/ci.yml README.md
git commit -m "feat: complete phase 4b SQL preparation"
```

---

## Final Acceptance Checklist

- [ ] Every public type and method in the spec exists and has a public-interface test.
- [ ] Every accepted SQL family has an owned-AST and public-prepare corpus case.
- [ ] Every excluded SQL family returns `Unsupported`; malformed SQL returns `Syntax`.
- [ ] Names, aliases, stars, parameters, types, aggregates, grouping and ordering use deterministic rules.
- [ ] Bound statements contain database UUID, stable logical IDs and schema versions, never physical roots.
- [ ] Schema validation distinguishes drop/recreate from unchanged objects and ignores unrelated changes.
- [ ] `prepare()` on connection/read transaction/write transaction performs zero database or WAL writes.
- [ ] SQL byte/token/node/depth/parameter/JOIN limits pass exact boundary tests.
- [ ] Third-party types remain confined to `src/sql/parser/`.
- [ ] `sqlparser 0.62.0` and `getrandom 0.4.3` pass Windows, Linux and Rust 1.85.
- [ ] Phase 1–4A normal, exhaustive and crash/recovery suites have zero failures.
- [ ] README and CI truthfully preserve Phase 4C and Phase 5 boundaries.
- [ ] No `Cargo.lock`, database, WAL, target, temporary probe or unrelated file is committed.
